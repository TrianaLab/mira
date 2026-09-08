//! OTLP metrics -> Arrow. The widest of the three signals, and the one where
//! the layout choice is worth the most.
//!
//! # Why four point tables instead of one
//!
//! OTLP has five metric types carrying four incompatible point shapes. The
//! obvious layout is one wide `data_points` table with every column any point
//! type might need and nulls everywhere else. Measured on 300,000 points in the
//! usual mix (90% number, 8% histogram, 1% exponential, 1% summary):
//!
//! ```text
//! one wide table   170.6 B/point
//! four split tables 73.0 B/point   2.34x
//! ```
//!
//! Nulls are not free in Arrow. A histogram's `bucket_counts` list column still
//! costs an offset entry on every one of the 270,000 number points that will
//! never have buckets, and the validity bitmaps stack up column by column. The
//! four tables also let a "graph this counter" query touch `number_dp` alone.
//!
//! Two more measurements shaped the histogram tables. Flattening `bucket_counts`
//! into a child table costs 1.47x what the `List<UInt64>` column costs, because a
//! child row pays a 4-byte parent id per bucket where the list pays one 4-byte
//! offset per point. And interning `explicit_bounds` into a side table takes
//! `hist_dp` from 410 to 246 B/row — every point of a histogram repeats the same
//! boundaries, which is what makes it the same histogram.
//!
//! # One id space for points
//!
//! `dp_attrs` and `exemplars` both key on a data point, and a point lives in one
//! of four tables. Rather than a discriminant column saying which, the four
//! tables draw `id` from a single counter, so a point id names exactly one row
//! in exactly one table. Attribute filtering — which is 68% of a metrics block
//! by size, measured — is then one semi-join instead of four.
//!
//! Ids stay ascending within each table, so the join is a binary search rather
//! than the direct index the logs and spans tables allow. Points of one metric
//! arrive together, so in practice the ids being searched are a contiguous run.

use std::collections::HashMap;
use std::sync::Arc;

use arrow_array::builder::{
    BooleanBuilder, FixedSizeBinaryBuilder, Float64Builder, Int32Builder, Int64Builder,
    ListBuilder, StringBuilder, TimestampNanosecondBuilder, UInt8Builder, UInt16Builder,
    UInt32Builder, UInt64Builder,
};
use arrow_array::{ArrayRef, RecordBatch};

use mira_proto::collector::metrics::v1::ExportMetricsServiceRequest;
use mira_proto::metrics::v1::exponential_histogram_data_point::Buckets;
use mira_proto::metrics::v1::metric::Data;
use mira_proto::metrics::v1::{
    Exemplar, ExponentialHistogramDataPoint, HistogramDataPoint, Metric, NumberDataPoint,
    SummaryDataPoint, exemplar, number_data_point,
};

use crate::attrs::{AttrsBuilder, DictColumn, ResourceScope, resource_kv, scope_kv};
use crate::error::Result;
use crate::logs::append_fixed;
use crate::schema::{
    EXEMPLARS, EXP_HIST_DP, HIST_BOUNDS, HIST_DP, METRICS, MetricKind, NUMBER_DP, SUMMARY_DP,
};
use crate::signal::{Sealed, SignalBuilder};

/// The five columns every point table starts with. Grouped so that the four
/// tables cannot drift apart, which would make a temporal filter four functions.
struct DpHead {
    id: UInt32Builder,
    metric_id: UInt32Builder,
    start: TimestampNanosecondBuilder,
    time: TimestampNanosecondBuilder,
    flags: UInt32Builder,
}

impl DpHead {
    fn new() -> Self {
        Self {
            id: UInt32Builder::new(),
            metric_id: UInt32Builder::new(),
            start: TimestampNanosecondBuilder::new(),
            time: TimestampNanosecondBuilder::new(),
            flags: UInt32Builder::new(),
        }
    }

    fn append(&mut self, id: u32, metric_id: u32, start: u64, time: u64, flags: u32) {
        self.id.append_value(id);
        self.metric_id.append_value(metric_id);
        if start != 0 {
            self.start.append_value(start as i64);
        } else {
            self.start.append_null();
        }
        self.time.append_value(time as i64);
        self.flags.append_value(flags);
    }

    fn finish(&mut self) -> Vec<ArrayRef> {
        vec![
            Arc::new(self.id.finish()),
            Arc::new(self.metric_id.finish()),
            Arc::new(self.start.finish()),
            Arc::new(self.time.finish()),
            Arc::new(self.flags.finish()),
        ]
    }
}

/// `count`/`sum`/`min`/`max`, shared by the three aggregating point types.
struct Stats {
    count: UInt64Builder,
    sum: Float64Builder,
    min: Float64Builder,
    max: Float64Builder,
}

impl Stats {
    fn new() -> Self {
        Self {
            count: UInt64Builder::new(),
            sum: Float64Builder::new(),
            min: Float64Builder::new(),
            max: Float64Builder::new(),
        }
    }

    fn append(&mut self, count: u64, sum: Option<f64>, min: Option<f64>, max: Option<f64>) {
        self.count.append_value(count);
        self.sum.append_option(sum);
        self.min.append_option(min);
        self.max.append_option(max);
    }

    fn finish(&mut self) -> Vec<ArrayRef> {
        vec![
            Arc::new(self.count.finish()),
            Arc::new(self.sum.finish()),
            Arc::new(self.min.finish()),
            Arc::new(self.max.finish()),
        ]
    }
}

pub struct MetricsBuilder {
    // metrics descriptors
    m_id: UInt32Builder,
    m_name: DictColumn,
    m_description: StringBuilder,
    m_unit: DictColumn,
    m_kind: UInt8Builder,
    m_temporality: UInt8Builder,
    m_monotonic: BooleanBuilder,
    m_resource_id: UInt16Builder,
    m_scope_id: UInt16Builder,
    metric_attrs: AttrsBuilder,
    next_metric_id: u32,

    num: DpHead,
    num_int: Int64Builder,
    num_double: Float64Builder,

    hist: DpHead,
    hist_stats: Stats,
    hist_counts: ListBuilder<UInt64Builder>,
    hist_bounds_id: UInt32Builder,

    /// `explicit_bounds` interned by their bit patterns. f64 has no `Hash` and
    /// `-0.0 == 0.0` while their bits differ, so the key is the raw bits: two
    /// bound arrays share a row only if they are byte-identical, which is the
    /// conservative direction (a missed intern costs space, a wrong one would
    /// mislabel every bucket).
    bounds_index: HashMap<Vec<u64>, u32>,
    bounds_id: UInt32Builder,
    bounds_values: ListBuilder<Float64Builder>,
    next_bounds_id: u32,

    exp: DpHead,
    exp_stats: Stats,
    exp_scale: Int32Builder,
    exp_zero_count: UInt64Builder,
    exp_zero_threshold: Float64Builder,
    exp_pos_offset: Int32Builder,
    exp_pos_counts: ListBuilder<UInt64Builder>,
    exp_neg_offset: Int32Builder,
    exp_neg_counts: ListBuilder<UInt64Builder>,

    summ: DpHead,
    summ_count: UInt64Builder,
    summ_sum: Float64Builder,
    summ_quantile: ListBuilder<Float64Builder>,
    summ_value: ListBuilder<Float64Builder>,

    dp_attrs: AttrsBuilder,
    /// One counter across all four point tables — see the module header.
    next_dp_id: u32,

    ex_id: UInt32Builder,
    ex_parent: UInt32Builder,
    ex_time: TimestampNanosecondBuilder,
    ex_int: Int64Builder,
    ex_double: Float64Builder,
    ex_trace_id: FixedSizeBinaryBuilder,
    ex_span_id: FixedSizeBinaryBuilder,
    exemplar_attrs: AttrsBuilder,
    next_exemplar_id: u32,

    rs: ResourceScope,
    /// List elements written so far, for `approx_bytes`. Tracked rather than
    /// measured because `ListBuilder::values` needs `&mut self`.
    list_values: usize,
    min_ts: i64,
    max_ts: i64,
}

impl Default for MetricsBuilder {
    fn default() -> Self {
        Self::new()
    }
}

impl MetricsBuilder {
    pub fn new() -> Self {
        Self {
            m_id: UInt32Builder::new(),
            m_name: DictColumn::new("metrics.name"),
            m_description: StringBuilder::new(),
            m_unit: DictColumn::new("metrics.unit"),
            m_kind: UInt8Builder::new(),
            m_temporality: UInt8Builder::new(),
            m_monotonic: BooleanBuilder::new(),
            m_resource_id: UInt16Builder::new(),
            m_scope_id: UInt16Builder::new(),
            metric_attrs: AttrsBuilder::new("metric_attrs.key"),
            next_metric_id: 0,

            num: DpHead::new(),
            num_int: Int64Builder::new(),
            num_double: Float64Builder::new(),

            hist: DpHead::new(),
            hist_stats: Stats::new(),
            hist_counts: ListBuilder::new(UInt64Builder::new()),
            hist_bounds_id: UInt32Builder::new(),

            bounds_index: HashMap::new(),
            bounds_id: UInt32Builder::new(),
            bounds_values: ListBuilder::new(Float64Builder::new()),
            next_bounds_id: 0,

            exp: DpHead::new(),
            exp_stats: Stats::new(),
            exp_scale: Int32Builder::new(),
            exp_zero_count: UInt64Builder::new(),
            exp_zero_threshold: Float64Builder::new(),
            exp_pos_offset: Int32Builder::new(),
            exp_pos_counts: ListBuilder::new(UInt64Builder::new()),
            exp_neg_offset: Int32Builder::new(),
            exp_neg_counts: ListBuilder::new(UInt64Builder::new()),

            summ: DpHead::new(),
            summ_count: UInt64Builder::new(),
            summ_sum: Float64Builder::new(),
            summ_quantile: ListBuilder::new(Float64Builder::new()),
            summ_value: ListBuilder::new(Float64Builder::new()),

            dp_attrs: AttrsBuilder::new("dp_attrs.key"),
            next_dp_id: 0,

            ex_id: UInt32Builder::new(),
            ex_parent: UInt32Builder::new(),
            ex_time: TimestampNanosecondBuilder::new(),
            ex_int: Int64Builder::new(),
            ex_double: Float64Builder::new(),
            ex_trace_id: FixedSizeBinaryBuilder::new(16),
            ex_span_id: FixedSizeBinaryBuilder::new(8),
            exemplar_attrs: AttrsBuilder::new("exemplar_attrs.key"),
            next_exemplar_id: 0,

            rs: ResourceScope::new(),
            list_values: 0,
            min_ts: i64::MAX,
            max_ts: i64::MIN,
        }
    }

    /// Data points, across all four tables. This is the row count the flusher
    /// reports and the number that matters — descriptors are a rounding error.
    pub fn num_rows(&self) -> usize {
        self.next_dp_id as usize
    }

    pub fn is_empty(&self) -> bool {
        self.next_metric_id == 0
    }

    pub fn approx_bytes(&self) -> usize {
        self.next_dp_id as usize * 48
            + self.next_metric_id as usize * 32
            + self.next_exemplar_id as usize * 56
            + self.list_values * 8
            + (self.dp_attrs.len()
                + self.metric_attrs.len()
                + self.exemplar_attrs.len()
                + self.rs.len())
                * 48
            + self.m_description.values_slice().len()
            + self.dp_attrs.heap_bytes()
            + self.metric_attrs.heap_bytes()
            + self.exemplar_attrs.heap_bytes()
            + self.rs.heap_bytes()
    }

    pub fn has_headroom_for(&self, req: &ExportMetricsServiceRequest) -> bool {
        let (mut resources, mut scopes) = (0usize, 0usize);
        let (mut res_kv, mut sc_kv) = (0usize, 0usize);
        let (mut names, mut meta_kv, mut dp_kv, mut ex_kv) = (0usize, 0usize, 0usize, 0usize);
        for rm in &req.resource_metrics {
            resources += 1;
            res_kv += resource_kv(rm.resource.as_ref());
            for sm in &rm.scope_metrics {
                scopes += 1;
                sc_kv += scope_kv(sm.scope.as_ref());
                names += sm.metrics.len();
                for m in &sm.metrics {
                    meta_kv += m.metadata.len();
                    for_each_point(m, |attrs, exemplars| {
                        dp_kv += attrs;
                        ex_kv += exemplars;
                    });
                }
            }
        }
        self.rs.has_headroom(resources, scopes, res_kv, sc_kv)
            && self.metric_attrs.has_headroom(meta_kv)
            && self.dp_attrs.has_headroom(dp_kv)
            && self.exemplar_attrs.has_headroom(ex_kv)
            // Name and unit both draw from `names` because a metric contributes
            // at most one new entry to each.
            && self.m_name.has_headroom(names)
            && self.m_unit.has_headroom(names)
    }

    pub fn append_request(&mut self, req: &ExportMetricsServiceRequest) -> Result<usize> {
        let mut added = 0;
        for rm in &req.resource_metrics {
            let rid = self.rs.resource(rm.resource.as_ref())?;
            for sm in &rm.scope_metrics {
                let sid = self.rs.scope(sm.scope.as_ref())?;
                for m in &sm.metrics {
                    added += self.append_metric(m, rid, sid)?;
                }
            }
        }
        Ok(added)
    }

    fn append_metric(&mut self, m: &Metric, rid: u16, sid: u16) -> Result<usize> {
        // Both dictionaries before anything else is written, for the reason in
        // `AttrsBuilder::append`.
        self.m_name.append(&m.name)?;
        self.m_unit.append(&m.unit)?;

        let mid = self.next_metric_id;
        self.next_metric_id += 1;

        // Temporality and monotonicity live on the wrapper message, not the
        // point, and only Sum has both. Flattening them onto the descriptor is
        // what lets the point tables be four columns narrower.
        let (kind, temporality, monotonic) = match &m.data {
            None => (MetricKind::Unset, 0, false),
            Some(Data::Gauge(_)) => (MetricKind::Gauge, 0, false),
            Some(Data::Sum(s)) => (
                MetricKind::Sum,
                clamp_u8(s.aggregation_temporality, 2),
                s.is_monotonic,
            ),
            Some(Data::Histogram(h)) => (
                MetricKind::Histogram,
                clamp_u8(h.aggregation_temporality, 2),
                false,
            ),
            Some(Data::ExponentialHistogram(h)) => (
                MetricKind::ExponentialHistogram,
                clamp_u8(h.aggregation_temporality, 2),
                false,
            ),
            Some(Data::Summary(_)) => (MetricKind::Summary, 0, false),
        };

        self.m_id.append_value(mid);
        if m.description.is_empty() {
            self.m_description.append_null();
        } else {
            self.m_description.append_value(&m.description);
        }
        self.m_kind.append_value(kind as u8);
        self.m_temporality.append_value(temporality);
        self.m_monotonic.append_value(monotonic);
        self.m_resource_id.append_value(rid);
        self.m_scope_id.append_value(sid);
        self.metric_attrs.append_all(mid, &m.metadata)?;

        let mut added = 0;
        match &m.data {
            None => {}
            Some(Data::Gauge(g)) => {
                for p in &g.data_points {
                    self.append_number(p, mid)?;
                    added += 1;
                }
            }
            Some(Data::Sum(s)) => {
                for p in &s.data_points {
                    self.append_number(p, mid)?;
                    added += 1;
                }
            }
            Some(Data::Histogram(h)) => {
                for p in &h.data_points {
                    self.append_hist(p, mid)?;
                    added += 1;
                }
            }
            Some(Data::ExponentialHistogram(h)) => {
                for p in &h.data_points {
                    self.append_exp_hist(p, mid)?;
                    added += 1;
                }
            }
            Some(Data::Summary(s)) => {
                for p in &s.data_points {
                    self.append_summary(p, mid)?;
                    added += 1;
                }
            }
        }
        Ok(added)
    }

    /// Claim the next point id and fold its timestamp into the block's range.
    ///
    /// Only `time_unix_nano` widens the range. See [`crate::schema::NUMBER_DP`]
    /// for why `start_time_unix_nano` must not.
    fn next_point(&mut self, time: u64) -> u32 {
        let id = self.next_dp_id;
        self.next_dp_id += 1;
        if time != 0 {
            self.min_ts = self.min_ts.min(time as i64);
            self.max_ts = self.max_ts.max(time as i64);
        }
        id
    }

    fn append_number(&mut self, p: &NumberDataPoint, mid: u32) -> Result<()> {
        let id = self.next_point(p.time_unix_nano);
        self.num
            .append(id, mid, p.start_time_unix_nano, p.time_unix_nano, p.flags);
        match p.value {
            // sfixed64 stays an Int64. Routing a counter through f64 would
            // silently drop its low bits past 2^53, which is a number real
            // request counters reach.
            Some(number_data_point::Value::AsInt(i)) => {
                self.num_int.append_value(i);
                self.num_double.append_null();
            }
            Some(number_data_point::Value::AsDouble(d)) => {
                self.num_int.append_null();
                self.num_double.append_value(d);
            }
            None => {
                self.num_int.append_null();
                self.num_double.append_null();
            }
        }
        self.dp_attrs.append_all(id, &p.attributes)?;
        self.append_exemplars(id, &p.exemplars)
    }

    fn append_hist(&mut self, p: &HistogramDataPoint, mid: u32) -> Result<()> {
        let id = self.next_point(p.time_unix_nano);
        self.hist
            .append(id, mid, p.start_time_unix_nano, p.time_unix_nano, p.flags);
        self.hist_stats.append(p.count, p.sum, p.min, p.max);
        self.hist_counts
            .append_value(p.bucket_counts.iter().copied().map(Some));
        self.list_values += p.bucket_counts.len();

        if p.explicit_bounds.is_empty() {
            self.hist_bounds_id.append_null();
        } else {
            let key: Vec<u64> = p.explicit_bounds.iter().map(|b| b.to_bits()).collect();
            let bid = match self.bounds_index.get(&key) {
                Some(&b) => b,
                None => {
                    let b = self.next_bounds_id;
                    self.next_bounds_id += 1;
                    self.bounds_id.append_value(b);
                    self.bounds_values
                        .append_value(p.explicit_bounds.iter().copied().map(Some));
                    self.list_values += p.explicit_bounds.len();
                    self.bounds_index.insert(key, b);
                    b
                }
            };
            self.hist_bounds_id.append_value(bid);
        }

        self.dp_attrs.append_all(id, &p.attributes)?;
        self.append_exemplars(id, &p.exemplars)
    }

    fn append_exp_hist(&mut self, p: &ExponentialHistogramDataPoint, mid: u32) -> Result<()> {
        let id = self.next_point(p.time_unix_nano);
        self.exp
            .append(id, mid, p.start_time_unix_nano, p.time_unix_nano, p.flags);
        self.exp_stats.append(p.count, p.sum, p.min, p.max);
        self.exp_scale.append_value(p.scale);
        self.exp_zero_count.append_value(p.zero_count);
        self.exp_zero_threshold.append_value(p.zero_threshold);
        let buckets = |b: &Option<Buckets>,
                       off: &mut Int32Builder,
                       counts: &mut ListBuilder<UInt64Builder>,
                       total: &mut usize| {
            match b {
                Some(b) => {
                    off.append_value(b.offset);
                    counts.append_value(b.bucket_counts.iter().copied().map(Some));
                    *total += b.bucket_counts.len();
                }
                None => {
                    off.append_value(0);
                    counts.append_null();
                }
            }
        };
        buckets(
            &p.positive,
            &mut self.exp_pos_offset,
            &mut self.exp_pos_counts,
            &mut self.list_values,
        );
        buckets(
            &p.negative,
            &mut self.exp_neg_offset,
            &mut self.exp_neg_counts,
            &mut self.list_values,
        );
        self.dp_attrs.append_all(id, &p.attributes)?;
        self.append_exemplars(id, &p.exemplars)
    }

    fn append_summary(&mut self, p: &SummaryDataPoint, mid: u32) -> Result<()> {
        let id = self.next_point(p.time_unix_nano);
        self.summ
            .append(id, mid, p.start_time_unix_nano, p.time_unix_nano, p.flags);
        self.summ_count.append_value(p.count);
        self.summ_sum.append_value(p.sum);
        self.summ_quantile
            .append_value(p.quantile_values.iter().map(|q| Some(q.quantile)));
        self.summ_value
            .append_value(p.quantile_values.iter().map(|q| Some(q.value)));
        self.list_values += p.quantile_values.len() * 2;
        // Summary has no exemplars on the wire — it predates them.
        self.dp_attrs.append_all(id, &p.attributes)
    }

    fn append_exemplars(&mut self, dp_id: u32, exemplars: &[Exemplar]) -> Result<()> {
        for e in exemplars {
            let eid = self.next_exemplar_id;
            self.next_exemplar_id += 1;
            self.ex_id.append_value(eid);
            self.ex_parent.append_value(dp_id);
            self.ex_time.append_value(e.time_unix_nano as i64);
            match e.value {
                Some(exemplar::Value::AsInt(i)) => {
                    self.ex_int.append_value(i);
                    self.ex_double.append_null();
                }
                Some(exemplar::Value::AsDouble(d)) => {
                    self.ex_int.append_null();
                    self.ex_double.append_value(d);
                }
                None => {
                    self.ex_int.append_null();
                    self.ex_double.append_null();
                }
            }
            append_fixed(&mut self.ex_trace_id, &e.trace_id, 16)?;
            append_fixed(&mut self.ex_span_id, &e.span_id, 8)?;
            self.exemplar_attrs
                .append_all(eid, &e.filtered_attributes)?;
        }
        Ok(())
    }

    /// Seal and reset, including on the error path — see
    /// [`SignalBuilder::finish`].
    pub fn finish(&mut self) -> Result<Sealed> {
        let out = self.seal();
        *self = Self::new();
        out
    }

    fn seal(&mut self) -> Result<Sealed> {
        let metrics = RecordBatch::try_new(
            METRICS.clone(),
            vec![
                Arc::new(self.m_id.finish()) as ArrayRef,
                self.m_name.finish(),
                Arc::new(self.m_description.finish()),
                self.m_unit.finish(),
                Arc::new(self.m_kind.finish()),
                Arc::new(self.m_temporality.finish()),
                Arc::new(self.m_monotonic.finish()),
                Arc::new(self.m_resource_id.finish()),
                Arc::new(self.m_scope_id.finish()),
            ],
        )?;

        let mut number = self.num.finish();
        number.push(Arc::new(self.num_int.finish()));
        number.push(Arc::new(self.num_double.finish()));

        let mut hist = self.hist.finish();
        hist.extend(self.hist_stats.finish());
        hist.push(Arc::new(self.hist_counts.finish()));
        hist.push(Arc::new(self.hist_bounds_id.finish()));

        let bounds = RecordBatch::try_new(
            HIST_BOUNDS.clone(),
            vec![
                Arc::new(self.bounds_id.finish()) as ArrayRef,
                Arc::new(self.bounds_values.finish()),
            ],
        )?;

        let mut exp = self.exp.finish();
        exp.extend(self.exp_stats.finish());
        exp.push(Arc::new(self.exp_scale.finish()));
        exp.push(Arc::new(self.exp_zero_count.finish()));
        exp.push(Arc::new(self.exp_zero_threshold.finish()));
        exp.push(Arc::new(self.exp_pos_offset.finish()));
        exp.push(Arc::new(self.exp_pos_counts.finish()));
        exp.push(Arc::new(self.exp_neg_offset.finish()));
        exp.push(Arc::new(self.exp_neg_counts.finish()));

        let mut summ = self.summ.finish();
        summ.push(Arc::new(self.summ_count.finish()));
        summ.push(Arc::new(self.summ_sum.finish()));
        summ.push(Arc::new(self.summ_quantile.finish()));
        summ.push(Arc::new(self.summ_value.finish()));

        let exemplars = RecordBatch::try_new(
            EXEMPLARS.clone(),
            vec![
                Arc::new(self.ex_id.finish()) as ArrayRef,
                Arc::new(self.ex_parent.finish()),
                Arc::new(self.ex_time.finish()),
                Arc::new(self.ex_int.finish()),
                Arc::new(self.ex_double.finish()),
                Arc::new(self.ex_trace_id.finish()),
                Arc::new(self.ex_span_id.finish()),
            ],
        )?;

        // Order matches `schema::METRICS_BLOCK_TABLES`, pinned by a test.
        let mut tables = vec![
            ("metrics", metrics),
            ("metric_attrs", self.metric_attrs.finish()?),
            (
                "number_dp",
                RecordBatch::try_new(NUMBER_DP.clone(), number)?,
            ),
            ("hist_dp", RecordBatch::try_new(HIST_DP.clone(), hist)?),
            ("hist_bounds", bounds),
            (
                "exp_hist_dp",
                RecordBatch::try_new(EXP_HIST_DP.clone(), exp)?,
            ),
            (
                "summary_dp",
                RecordBatch::try_new(SUMMARY_DP.clone(), summ)?,
            ),
            ("dp_attrs", self.dp_attrs.finish()?),
            ("exemplars", exemplars),
            ("exemplar_attrs", self.exemplar_attrs.finish()?),
        ];
        tables.extend(self.rs.finish()?);
        Ok(Sealed {
            num_rows: self.next_dp_id as usize,
            tables,
            sidecars: Vec::new(),
            min_ts: if self.min_ts == i64::MAX {
                0
            } else {
                self.min_ts
            },
            max_ts: if self.max_ts == i64::MIN {
                0
            } else {
                self.max_ts
            },
        })
    }
}

impl SignalBuilder for MetricsBuilder {
    type Request = ExportMetricsServiceRequest;
    const SIGNAL: &'static str = "metrics";

    fn has_headroom_for(&self, req: &Self::Request) -> bool {
        MetricsBuilder::has_headroom_for(self, req)
    }
    fn append_request(&mut self, req: &Self::Request) -> Result<usize> {
        MetricsBuilder::append_request(self, req)
    }
    fn approx_bytes(&self) -> usize {
        MetricsBuilder::approx_bytes(self)
    }
    fn is_empty(&self) -> bool {
        MetricsBuilder::is_empty(self)
    }
    fn finish(&mut self) -> Result<Sealed> {
        MetricsBuilder::finish(self)
    }
}

/// Visit `(attribute_count, exemplar_attribute_count)` for every point of `m`,
/// whichever of the five shapes it has. Exists so `has_headroom_for` does not
/// repeat the five-arm match that `append_metric` already has.
fn for_each_point(m: &Metric, mut f: impl FnMut(usize, usize)) {
    let ex = |e: &[Exemplar]| e.iter().map(|x| x.filtered_attributes.len()).sum::<usize>();
    match &m.data {
        None => {}
        Some(Data::Gauge(g)) => {
            for p in &g.data_points {
                f(p.attributes.len(), ex(&p.exemplars));
            }
        }
        Some(Data::Sum(s)) => {
            for p in &s.data_points {
                f(p.attributes.len(), ex(&p.exemplars));
            }
        }
        Some(Data::Histogram(h)) => {
            for p in &h.data_points {
                f(p.attributes.len(), ex(&p.exemplars));
            }
        }
        Some(Data::ExponentialHistogram(h)) => {
            for p in &h.data_points {
                f(p.attributes.len(), ex(&p.exemplars));
            }
        }
        Some(Data::Summary(s)) => {
            for p in &s.data_points {
                f(p.attributes.len(), 0);
            }
        }
    }
}

/// Enums arrive as `i32` and a client can send anything. Out of range becomes
/// the zero variant, which every OTLP enum defines as "unspecified".
fn clamp_u8(v: i32, max: u8) -> u8 {
    u8::try_from(v).unwrap_or(0).min(max)
}
