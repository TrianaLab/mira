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
use crate::logs::{append_fixed, nanos};
use crate::schema::{
    EXEMPLARS, EXP_HIST_DP, HIST_BOUNDS, HIST_DP, METRICS, MetricKind, NUMBER_DP, SUMMARY_DP,
};
use crate::signal::{Sealed, Sidecars, SignalBuilder};

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
        // `nanos` reads an unrepresentable timestamp as the wire's own "unset",
        // so the null branch already here covers a broken clock too.
        let start = nanos(start);
        if start != 0 {
            self.start.append_value(start);
        } else {
            self.start.append_null();
        }
        self.time.append_value(nanos(time));
        self.flags.append_value(flags);
    }

    fn finish(&self) -> Vec<ArrayRef> {
        vec![
            Arc::new(self.id.finish_cloned()),
            Arc::new(self.metric_id.finish_cloned()),
            Arc::new(self.start.finish_cloned()),
            Arc::new(self.time.finish_cloned()),
            Arc::new(self.flags.finish_cloned()),
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

    fn finish(&self) -> Vec<ArrayRef> {
        vec![
            Arc::new(self.count.finish_cloned()),
            Arc::new(self.sum.finish_cloned()),
            Arc::new(self.min.finish_cloned()),
            Arc::new(self.max.finish_cloned()),
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
        // Same guard for a missing clock and an unrepresentable one: a point past
        // 2^63 would wrap negative, and a negative `min_ts` publishes a block
        // directory `block::parse_dir_name` refuses. See [`crate::logs::nanos`].
        let time = nanos(time);
        if time != 0 {
            self.min_ts = self.min_ts.min(time);
            self.max_ts = self.max_ts.max(time);
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
            self.ex_time.append_value(nanos(e.time_unix_nano));
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
        let out = self.seal(Sidecars::Build);
        *self = Self::new();
        out
    }

    fn seal(&self, sidecars: Sidecars) -> Result<Sealed> {
        let cols: Vec<ArrayRef> = vec![
            Arc::new(self.m_id.finish_cloned()),
            self.m_name.finish(),
            Arc::new(self.m_description.finish_cloned()),
            self.m_unit.finish(),
            Arc::new(self.m_kind.finish_cloned()),
            Arc::new(self.m_temporality.finish_cloned()),
            Arc::new(self.m_monotonic.finish_cloned()),
            Arc::new(self.m_resource_id.finish_cloned()),
            Arc::new(self.m_scope_id.finish_cloned()),
        ];
        let metrics = RecordBatch::try_new(METRICS.clone(), cols)?;

        let mut number = self.num.finish();
        number.push(Arc::new(self.num_int.finish_cloned()));
        number.push(Arc::new(self.num_double.finish_cloned()));

        let mut hist = self.hist.finish();
        hist.extend(self.hist_stats.finish());
        hist.push(Arc::new(self.hist_counts.finish_cloned()));
        hist.push(Arc::new(self.hist_bounds_id.finish_cloned()));

        let cols: Vec<ArrayRef> = vec![
            Arc::new(self.bounds_id.finish_cloned()),
            Arc::new(self.bounds_values.finish_cloned()),
        ];
        let bounds = RecordBatch::try_new(HIST_BOUNDS.clone(), cols)?;

        let mut exp = self.exp.finish();
        exp.extend(self.exp_stats.finish());
        exp.push(Arc::new(self.exp_scale.finish_cloned()));
        exp.push(Arc::new(self.exp_zero_count.finish_cloned()));
        exp.push(Arc::new(self.exp_zero_threshold.finish_cloned()));
        exp.push(Arc::new(self.exp_pos_offset.finish_cloned()));
        exp.push(Arc::new(self.exp_pos_counts.finish_cloned()));
        exp.push(Arc::new(self.exp_neg_offset.finish_cloned()));
        exp.push(Arc::new(self.exp_neg_counts.finish_cloned()));

        let mut summ = self.summ.finish();
        summ.push(Arc::new(self.summ_count.finish_cloned()));
        summ.push(Arc::new(self.summ_sum.finish_cloned()));
        summ.push(Arc::new(self.summ_quantile.finish_cloned()));
        summ.push(Arc::new(self.summ_value.finish_cloned()));

        let cols: Vec<ArrayRef> = vec![
            Arc::new(self.ex_id.finish_cloned()),
            Arc::new(self.ex_parent.finish_cloned()),
            Arc::new(self.ex_time.finish_cloned()),
            Arc::new(self.ex_int.finish_cloned()),
            Arc::new(self.ex_double.finish_cloned()),
            Arc::new(self.ex_trace_id.finish_cloned()),
            Arc::new(self.ex_span_id.finish_cloned()),
        ];
        let exemplars = RecordBatch::try_new(EXEMPLARS.clone(), cols)?;

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
        Ok(Sealed::with(
            sidecars,
            self.next_dp_id as usize,
            tables,
            self.min_ts,
            self.max_ts,
        ))
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
    fn snapshot(&self) -> Result<Sealed> {
        self.seal(Sidecars::Skip)
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

#[cfg(test)]
mod tests {
    use super::*;
    use arrow_array::cast::AsArray;
    use arrow_array::types::{Float64Type, Int64Type, UInt8Type, UInt32Type, UInt64Type};
    use arrow_array::{Array, RecordBatch, TimestampNanosecondArray};
    use mira_proto::metrics::v1::summary_data_point::ValueAtQuantile;
    use mira_proto::metrics::v1::{
        ExponentialHistogram, Gauge, Histogram, ResourceMetrics, ScopeMetrics, Sum, Summary,
    };

    /// One `ExportMetricsServiceRequest` carrying `metrics` under one resource
    /// and one scope. Every shape test below differs only in that list.
    fn request(metrics: Vec<Metric>) -> ExportMetricsServiceRequest {
        ExportMetricsServiceRequest {
            resource_metrics: vec![ResourceMetrics {
                scope_metrics: vec![ScopeMetrics {
                    metrics,
                    ..Default::default()
                }],
                ..Default::default()
            }],
        }
    }

    /// An instrument that was registered and never recorded into sends a
    /// descriptor with no `data`, and the collector forwards it. It has to
    /// survive as a descriptor row with no points: dropping it loses the name,
    /// unit and description that make the instrument discoverable before its
    /// first sample, which is exactly when someone is looking for it.
    #[test]
    fn a_metric_with_no_data_is_a_descriptor_and_no_points() {
        let req = ExportMetricsServiceRequest {
            resource_metrics: vec![ResourceMetrics {
                scope_metrics: vec![ScopeMetrics {
                    metrics: vec![
                        Metric {
                            name: "queue.depth".into(),
                            description: "items awaiting a worker".into(),
                            data: None,
                            ..Default::default()
                        },
                        Metric {
                            name: "http.server.duration".into(),
                            data: Some(Data::Gauge(Gauge {
                                data_points: vec![NumberDataPoint {
                                    time_unix_nano: 1_000,
                                    exemplars: vec![Exemplar {
                                        time_unix_nano: 1_000,
                                        value: Some(exemplar::Value::AsInt(7)),
                                        ..Default::default()
                                    }],
                                    ..Default::default()
                                }],
                            })),
                            ..Default::default()
                        },
                    ],
                    ..Default::default()
                }],
                ..Default::default()
            }],
        };

        let mut b = MetricsBuilder::new();
        // Through the trait, because that is how the flusher asks — and the
        // headroom walk has to survive the data-less metric too.
        assert!(SignalBuilder::has_headroom_for(&b, &req));
        assert_eq!(b.append_request(&req).unwrap(), 1, "one point, two metrics");
        assert_eq!(b.num_rows(), 1, "points, not descriptors");

        let sealed = b.finish().unwrap();
        let metrics = sealed.table("metrics").unwrap();
        assert_eq!(metrics.num_rows(), 2);
        let kind = metrics.column_by_name("kind").unwrap();
        assert_eq!(
            kind.as_primitive::<UInt8Type>().values(),
            &[MetricKind::Unset as u8, MetricKind::Gauge as u8]
        );
        let desc = metrics.column_by_name("description").unwrap();
        assert_eq!(desc.as_string::<i32>().value(0), "items awaiting a worker");
        assert!(desc.is_null(1), "an empty description is absent, not \"\"");

        // An integer exemplar lands in `int`, leaving `double` null: the two
        // columns are how the reader recovers which arm of the union it was.
        let ex = sealed.table("exemplars").unwrap();
        let int = ex.column_by_name("int").unwrap();
        assert_eq!(int.as_primitive::<Int64Type>().value(0), 7);
        assert!(ex.column_by_name("double").unwrap().is_null(0));
    }

    /// A point past 2^63 wrapped negative into `min_ts`, and a negative `min_ts`
    /// publishes a block directory `block::parse_dir_name` refuses — invisible
    /// to every query and to every retention sweep, after the export was acked.
    #[test]
    fn point_times_past_i64_do_not_wrap_the_block_range() {
        let point = |start: u64, time: u64| NumberDataPoint {
            start_time_unix_nano: start,
            time_unix_nano: time,
            exemplars: vec![Exemplar {
                time_unix_nano: u64::MAX,
                ..Default::default()
            }],
            ..Default::default()
        };

        let mut b = MetricsBuilder::new();
        b.append_request(&ExportMetricsServiceRequest {
            resource_metrics: vec![ResourceMetrics {
                scope_metrics: vec![ScopeMetrics {
                    metrics: vec![Metric {
                        name: "process.memory".into(),
                        data: Some(Data::Gauge(Gauge {
                            data_points: vec![point(u64::MAX, u64::MAX), point(u64::MAX, 2_000)],
                        })),
                        ..Default::default()
                    }],
                    ..Default::default()
                }],
                ..Default::default()
            }],
        })
        .unwrap();

        let sealed = b.finish().unwrap();
        assert_eq!(
            sealed.num_rows, 2,
            "malformed points are stored, not dropped"
        );
        assert_eq!((sealed.min_ts, sealed.max_ts), (2_000, 2_000));

        let ts = |t: &str, c: &str| {
            sealed
                .table(t)
                .unwrap()
                .column_by_name(c)
                .unwrap()
                .as_any()
                .downcast_ref::<TimestampNanosecondArray>()
                .unwrap()
                .clone()
        };
        // An unrepresentable process start is as absent as a zero one.
        assert!(ts("number_dp", "start_time_unix_nano").is_null(0));
        assert_eq!(ts("number_dp", "time_unix_nano").values(), &[0, 2_000]);
        assert_eq!(ts("exemplars", "time_unix_nano").values(), &[0, 0]);
    }

    /// Five metric types, four point tables, one id space.
    ///
    /// Two invariants ride on this and neither is visible in a single-shape
    /// test. A point routed to the wrong table is a point no chart ever finds
    /// again, because the read path resolves a value by *which* table the row
    /// sits in. And `dp_attrs` and `exemplars` carry no discriminant column —
    /// they key on a point id alone — so if two of the four tables ever issued
    /// the same id, one point's attributes and exemplars would silently attach
    /// to another metric's point. The shapes below are the ones a real
    /// collector sends: every value oneof including the unset one, a histogram
    /// with no `sum` and one with no bounds at all, an exponential histogram
    /// with only positive buckets, and a descriptor whose `data` is missing.
    #[test]
    fn every_otlp_metric_shape_lands_in_the_table_written_for_it() {
        let num = |t: u64, v: Option<number_data_point::Value>| NumberDataPoint {
            time_unix_nano: t,
            value: v,
            ..Default::default()
        };
        let bounds = vec![1.0, 2.0];
        let hist = |sum: Option<f64>, explicit_bounds: Vec<f64>| HistogramDataPoint {
            time_unix_nano: 3_000,
            count: 6,
            sum,
            bucket_counts: vec![1, 2, 3],
            explicit_bounds,
            ..Default::default()
        };
        let req = request(vec![
            // A descriptor an exporter registered and never wrote a point to.
            // It still names a metric, so the row is kept — and a description
            // is stored where an absent one is null rather than "".
            Metric {
                name: "declared.only".into(),
                description: "registered by an exporter that never fired".into(),
                data: None,
                ..Default::default()
            },
            Metric {
                name: "gauge".into(),
                data: Some(Data::Gauge(Gauge {
                    data_points: vec![
                        num(1_000, Some(number_data_point::Value::AsInt(7))),
                        num(1_001, Some(number_data_point::Value::AsDouble(0.5))),
                        // OTLP allows a point with neither: both columns null,
                        // and the row is still stored so the gap is visible.
                        num(1_002, None),
                    ],
                })),
                ..Default::default()
            },
            Metric {
                name: "counter".into(),
                unit: "1".into(),
                data: Some(Data::Sum(Sum {
                    is_monotonic: true,
                    // Out of range on the wire. Every OTLP enum defines zero as
                    // "unspecified", so a client sending nonsense gets that
                    // rather than a rejected export.
                    aggregation_temporality: -3,
                    data_points: vec![NumberDataPoint {
                        time_unix_nano: 2_000,
                        value: Some(number_data_point::Value::AsInt(11)),
                        exemplars: vec![
                            Exemplar {
                                time_unix_nano: 2_000,
                                value: Some(exemplar::Value::AsInt(11)),
                                trace_id: vec![1u8; 16].into(),
                                span_id: vec![2u8; 8].into(),
                                ..Default::default()
                            },
                            Exemplar {
                                time_unix_nano: 2_001,
                                value: Some(exemplar::Value::AsDouble(1.5)),
                                ..Default::default()
                            },
                            Exemplar {
                                time_unix_nano: 2_002,
                                value: None,
                                ..Default::default()
                            },
                        ],
                        ..Default::default()
                    }],
                })),
                ..Default::default()
            },
            Metric {
                name: "hist".into(),
                data: Some(Data::Histogram(Histogram {
                    aggregation_temporality: 99,
                    data_points: vec![
                        hist(Some(4.5), bounds.clone()),
                        // Same boundaries: one `hist_bounds` row, two points.
                        // That interning is what takes the table from 410 to
                        // 246 bytes a row.
                        hist(None, bounds),
                        // A histogram with no boundaries is a single implicit
                        // bucket, so there is nothing to intern and `bounds_id`
                        // is null rather than pointing at an empty list.
                        hist(Some(1.0), Vec::new()),
                    ],
                })),
                ..Default::default()
            },
            Metric {
                name: "exp".into(),
                data: Some(Data::ExponentialHistogram(ExponentialHistogram {
                    aggregation_temporality: 1,
                    data_points: vec![ExponentialHistogramDataPoint {
                        time_unix_nano: 4_000,
                        count: 3,
                        scale: -2,
                        zero_count: 1,
                        zero_threshold: 0.25,
                        positive: Some(Buckets {
                            offset: 5,
                            bucket_counts: vec![1, 1],
                        }),
                        // Nothing negative was observed, which is the common
                        // case and must not cost a bucket list.
                        negative: None,
                        ..Default::default()
                    }],
                })),
                ..Default::default()
            },
            Metric {
                name: "summ".into(),
                data: Some(Data::Summary(Summary {
                    data_points: vec![SummaryDataPoint {
                        time_unix_nano: 5_000,
                        count: 2,
                        sum: 3.0,
                        quantile_values: vec![
                            ValueAtQuantile {
                                quantile: 0.5,
                                value: 1.0,
                            },
                            ValueAtQuantile {
                                quantile: 0.99,
                                value: 2.0,
                            },
                        ],
                        ..Default::default()
                    }],
                })),
                ..Default::default()
            },
        ]);

        let mut b = MetricsBuilder::new();
        assert!(b.is_empty(), "a fresh builder holds no descriptors");
        // Headroom walks the same five shapes `append_metric` does, through a
        // second match that has drifted from it before. Asked of an empty
        // builder about a request this small the answer can only be yes; what
        // is being checked is that every arm of that walk survives the ask.
        assert!(SignalBuilder::has_headroom_for(&b, &req));
        assert_eq!(SignalBuilder::append_request(&mut b, &req).unwrap(), 9);
        // `num_rows` is points, not descriptors: it is what the flusher sizes a
        // block by, and seven descriptors would be a rounding error against it.
        assert_eq!(b.num_rows(), 9);
        assert!(!b.is_empty());
        assert!(b.approx_bytes() > 0);

        let sealed = SignalBuilder::finish(&mut b).unwrap();
        assert_eq!(sealed.num_rows, 9);
        let table = |n: &str| sealed.table(n).expect(n).clone();
        let rows = |n: &str| table(n).num_rows();
        assert_eq!(
            [
                rows("number_dp"),
                rows("hist_dp"),
                rows("exp_hist_dp"),
                rows("summary_dp")
            ],
            [4, 3, 1, 1],
            "each point type in the table written for its shape"
        );

        // The id space. Every point id appears exactly once across the four
        // tables and they are dense from zero, which is what lets `dp_attrs`
        // and `exemplars` key on the id with no table discriminant.
        let mut ids: Vec<u32> = Vec::new();
        for t in ["number_dp", "hist_dp", "exp_hist_dp", "summary_dp"] {
            let b = table(t);
            let col = b.column_by_name("id").unwrap();
            ids.extend(col.as_primitive::<UInt32Type>().values().iter().copied());
        }
        ids.sort_unstable();
        assert_eq!(ids, (0..9).collect::<Vec<u32>>());

        let m = table("metrics");
        let u8s = |b: &RecordBatch, c: &str| {
            b.column_by_name(c)
                .unwrap()
                .as_primitive::<UInt8Type>()
                .values()
                .to_vec()
        };
        assert_eq!(
            u8s(&m, "kind"),
            [
                MetricKind::Unset as u8,
                MetricKind::Gauge as u8,
                MetricKind::Sum as u8,
                MetricKind::Histogram as u8,
                MetricKind::ExponentialHistogram as u8,
                MetricKind::Summary as u8,
            ]
        );
        // Temporality lives on the wrapper, not the point, and the two out of
        // range values clamp in opposite directions: negative to the
        // "unspecified" zero, too-large down to the highest defined variant.
        assert_eq!(u8s(&m, "temporality"), [0, 0, 0, 2, 1, 0]);
        let mono = m.column_by_name("is_monotonic").unwrap().as_boolean();
        assert_eq!(
            (0..6).map(|i| mono.value(i)).collect::<Vec<bool>>(),
            [false, false, true, false, false, false],
            "only a Sum can be monotonic"
        );
        let desc = m.column_by_name("description").unwrap().as_string::<i32>();
        assert_eq!(desc.value(0), "registered by an exporter that never fired");
        assert!(
            (1..6).all(|i| desc.is_null(i)),
            "an empty description is absent, not an empty string"
        );

        // Numbers keep the type they arrived as: an sfixed64 counter past 2^53
        // read back through a double loses the low bits that made it worth
        // charting, so `int` and `double` are separate nullable columns and
        // exactly one is set per point.
        let n = table("number_dp");
        let ints = n.column_by_name("int").unwrap().as_primitive::<Int64Type>();
        let dbls = n
            .column_by_name("double")
            .unwrap()
            .as_primitive::<Float64Type>();
        assert_eq!(
            (0..4)
                .map(|i| (ints.is_null(i), dbls.is_null(i)))
                .collect::<Vec<_>>(),
            [(false, true), (true, false), (true, true), (false, true)]
        );

        // Identical boundaries intern to one row; a point with none at all
        // points at nothing rather than at an empty list.
        let h = table("hist_dp");
        assert_eq!(table("hist_bounds").num_rows(), 1);
        let bid = h
            .column_by_name("bounds_id")
            .unwrap()
            .as_primitive::<UInt32Type>();
        assert_eq!((bid.value(0), bid.value(1)), (0, 0));
        assert!(bid.is_null(2));
        let hsum = h
            .column_by_name("sum")
            .unwrap()
            .as_primitive::<Float64Type>();
        assert!(hsum.is_null(1), "a histogram may report count and no sum");

        // A missing bucket side is null, not an empty list: "we saw nothing
        // negative" and "we did not record the negative side" are different
        // answers and a reader has to be able to tell them apart.
        let e = table("exp_hist_dp");
        assert!(e.column_by_name("positive_counts").unwrap().is_valid(0));
        assert!(e.column_by_name("negative_counts").unwrap().is_null(0));
        assert_eq!(
            e.column_by_name("zero_count")
                .unwrap()
                .as_primitive::<UInt64Type>()
                .value(0),
            1
        );

        // Summary predates exemplars, so its three exemplars are the counter's.
        let ex = table("exemplars");
        assert_eq!(ex.num_rows(), 3);
        let exi = ex
            .column_by_name("int")
            .unwrap()
            .as_primitive::<Int64Type>();
        let exd = ex
            .column_by_name("double")
            .unwrap()
            .as_primitive::<Float64Type>();
        assert_eq!(
            (0..3)
                .map(|i| (exi.is_null(i), exd.is_null(i)))
                .collect::<Vec<_>>(),
            [(false, true), (true, false), (true, true)]
        );
        assert_eq!(exi.value(0), 11);
        // An exemplar with no ids is null in both, not sixteen zero bytes —
        // otherwise "no trace" would render as a trace id a caller can search.
        let tid = ex.column_by_name("trace_id").unwrap();
        assert!(tid.is_valid(0) && tid.is_null(1));

        // Quantiles are two parallel lists, one row per point.
        let s = table("summary_dp");
        assert_eq!(
            s.column_by_name("quantile")
                .unwrap()
                .as_list::<i32>()
                .value(0)
                .len(),
            2
        );
    }

    /// A failed append must not leave a builder that seals into a block whose
    /// columns disagree in length.
    ///
    /// `append_metric` writes the two dictionary columns before anything else,
    /// so an overflow on the second one leaves `name` a row ahead of every
    /// other column of the descriptor table. Arrow refuses to build that batch,
    /// which is the right answer — the wrong one would be a published block
    /// where row *n* of `name` describes row *n* of nothing. `finish` resets
    /// even on that path, so the next block is clean rather than permanently
    /// poisoned.
    #[test]
    fn a_torn_append_fails_the_seal_instead_of_publishing_a_ragged_block() {
        let mut b = MetricsBuilder::new();
        // 65,536 distinct units, which is the dictionary's whole key space.
        // Driven straight at the column rather than through 65,536 `Metric`
        // messages: the state under test is the full dictionary, and building
        // the protobufs to reach it would cost seconds for nothing.
        for i in 0..crate::schema::DICT_CAP {
            b.m_unit.append(&format!("u{i}")).unwrap();
        }
        assert!(!b.m_unit.has_headroom(1), "the unit dictionary is full");

        let one = |unit: &str| Metric {
            name: "requests".into(),
            unit: unit.into(),
            data: Some(Data::Gauge(Gauge {
                data_points: vec![NumberDataPoint {
                    time_unix_nano: 1_000,
                    value: Some(number_data_point::Value::AsInt(1)),
                    ..Default::default()
                }],
            })),
            ..Default::default()
        };
        let err = b.append_metric(&one("brand-new"), 0, 0).unwrap_err();
        assert!(
            matches!(err, crate::error::Error::DictionaryFull("metrics.unit")),
            "{err}"
        );
        // `name` took the extra row; nothing else did.
        let Err(err) = b.finish() else {
            panic!("a ragged descriptor table sealed");
        };
        assert!(matches!(err, crate::error::Error::Arrow(_)), "{err}");

        // And the reset happened anyway, so the next block is a clean one.
        assert!(b.is_empty());
        b.append_request(&request(vec![one("ms")])).unwrap();
        let sealed = b.finish().unwrap();
        assert_eq!(sealed.num_rows, 1);
        assert_eq!(sealed.table("metrics").unwrap().num_rows(), 1);
    }
}
