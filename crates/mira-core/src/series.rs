//! The metrics read path: match metrics, gather points, group into series.
//!
//! A different shape from [`crate::query::search`], and deliberately a separate
//! function rather than a mode of it. A log search filters one root table and
//! returns rows newest-first; a metrics query filters a *descriptor* table,
//! gathers points from four point tables that share one id space, and then
//! regroups them by a key that has to be stable across blocks. Folding both into
//! one code path would mean inventing the abstraction that both are special
//! cases of, which is a planner.
//!
//! The grouping key is the hard part and the reason this is not trivial. Ids are
//! rebased per block — that is what makes the attribute joins array stores — so
//! nothing block-local can identify a series across two blocks. The key is built
//! from values instead: the metric's name, unit and kind, plus the merged
//! attribute map from all four levels. That is one string built per point, which
//! is the honest cost of a layout optimized for writing and pruning rather than
//! for grouping.
//!
//! ## What V0 returns
//!
//! Gauges and sums come back as their own value. Histograms, exponential
//! histograms and summaries come back as two derived series each, `<name>.count`
//! and `<name>.sum` — the same convention Prometheus uses, and the same two
//! numbers that answer "how often" and "how much". Bucket and quantile maths is
//! not here: it is a heatmap feature, it needs the UI to exist first, and
//! shipping it wrong would be worse than shipping it later. Everything needed
//! for it is on disk already — `bucket_counts`, `bounds_id`, `scale`, `quantile`
//! — so this is a read-path gap, not a storage one.
//!
//! Blocking: mmaps and page-faults, same as `search`. Callers on an async
//! runtime must use `spawn_blocking`.

use std::collections::HashMap;
use std::path::Path;

use arrow_array::cast::AsArray;
use arrow_array::types::{
    Float64Type, Int64Type, TimestampNanosecondType, UInt8Type, UInt16Type, UInt32Type, UInt64Type,
};
use arrow_array::{Array, RecordBatch};

use crate::block::{self, BlockRef};
use crate::error::Result;
use crate::json::Json;
use crate::query::{Results, Stats, Term, attr_key, attr_parents, dict_index, emit_attr};
use crate::schema::MetricKind;

/// A metrics query.
///
/// Note what is missing: no `step`, no aggregation, no rate. Downsampling in the
/// engine would need a fill policy, an alignment rule and a choice of aggregator
/// per metric kind, all of which are presentation decisions the caller is better
/// placed to make and all of which are wrong for somebody. Raw points, bounded
/// by `max_points`, and the chart decides.
#[derive(Debug, Clone)]
pub struct SeriesQuery {
    /// Exact metric name. `None` matches every metric, which is what the name
    /// listing uses and what an exploratory query does before it knows better.
    pub name: Option<String>,
    /// Inclusive nanosecond bounds.
    pub from: i64,
    pub to: i64,
    /// Attribute filters. A term is satisfied if it matches at *any* level —
    /// resource, scope, metric or data point — because a caller filtering on
    /// `service.name` should not have to know which of the four carries it.
    pub terms: Vec<Term>,
    pub max_series: usize,
    /// Per series, not in total: truncating one busy series must not silently
    /// empty the others charted beside it.
    pub max_points: usize,
}

/// One point. Integers stay integers: an OTLP `as_int` is `sfixed64`, and a
/// counter past 2^53 pushed through an f64 loses its low bits — which is exactly
/// the moment a counter is interesting.
#[derive(Debug, Clone, Copy)]
enum Pt {
    Int(i64),
    Double(f64),
}

struct Series {
    /// Rendered `"name":...,"unit":...,` etc. Built once when the series is
    /// first seen; identical for every point in it by construction.
    desc: String,
    /// Rendered `{...}` attribute object.
    attrs: String,
    points: Vec<(i64, Pt)>,
    /// Points dropped by `max_points`, reported rather than hidden. A chart
    /// missing its spike because the engine quietly truncated is the failure
    /// mode this exists to prevent.
    dropped: usize,
    /// The trace ids behind the points: `(time, rendered object)`, rendered
    /// here because the block they were read from is unmapped before the
    /// response is written.
    exemplars: Vec<(i64, String)>,
}

impl Series {
    /// Discard all but the newest `max` points.
    ///
    /// Which points a cap keeps is a correctness question, not a tuning one.
    /// Points arrive in block-scan order, so refusing them once the buffer is
    /// full kept whichever ones the directory listing happened to reach first
    /// — an arbitrary subset that the sort at render time then dressed up as a
    /// contiguous series. Newest-wins is the same rule `limit` already uses on
    /// the record side, and it is a window a reader can reason about.
    ///
    /// Called at twice `max`, so every call throws away half of what it sorts
    /// and the amortised cost is constant per point. A bounded heap would hold
    /// exactly `max` at all times and pay `log max` on every point instead;
    /// at these sizes the occasional sort is cheaper and much less code.
    ///
    /// ponytail: truncation, not downsampling. A chart that needs the whole
    /// range at a lower resolution needs an aggregation window — until that
    /// exists the response says `dropped_points` rather than pretending.
    fn compact(&mut self, max: usize) {
        // Descending, so `truncate` keeps the newest.
        self.points.sort_unstable_by_key(|p| std::cmp::Reverse(p.0));
        self.dropped += self.points.len() - max;
        self.points.truncate(max);
    }
}

/// Exemplars kept per series.
///
/// An exemplar is one sampled measurement per collection interval per bucket,
/// so a well-behaved exporter sends few — but nothing in OTLP bounds it, and an
/// unbounded array here would make one misconfigured SDK able to inflate every
/// chart response. ponytail: a flat cap, not a reservoir sample; the first ones
/// in a window are as good a sample as any until someone proves otherwise.
const MAX_EXEMPLARS: usize = 64;

/// One attribute as `(key, rendered JSON value)`. Rendered once and carried as
/// a string because the same value is written into both the grouping key and
/// the response.
type Attr = (String, String);

/// A descriptor row's cached contribution: its rendered JSON members, and the
/// attributes every point beneath it inherits.
type Prefix = (String, Vec<Attr>);

/// The four point tables, and how many name suffixes each contributes.
///
/// One table per OTLP point type rather than one wide table: measured, the split
/// layout is 2.34x smaller (73.0 vs 170.6 bytes/point), because a wide table
/// pays for every column of every type on every row.
const DP_TABLES: [&str; 4] = ["number_dp", "hist_dp", "exp_hist_dp", "summary_dp"];

/// The suffixes a histogram's or summary's name is derived with. Named rather
/// than written twice, because the name filter has to accept back exactly the
/// names the renderer hands out and two literal lists would drift apart.
const COUNT: &str = ".count";
const SUM: &str = ".sum";
const DERIVED: [&str; 2] = [COUNT, SUM];

pub fn series(root: &Path, q: &SeriesQuery) -> Result<Results> {
    let refs = block::scan(root, "metrics")?;
    let mut stats = Stats {
        blocks_total: refs.len(),
        ..Default::default()
    };
    let mut out: HashMap<String, Series> = HashMap::new();

    for bref in &refs {
        // The directory name is the whole index; a block outside the window is
        // never opened.
        if bref.max_ts < q.from || bref.min_ts > q.to {
            continue;
        }
        stats.blocks_scanned += 1;
        collect_block(bref, q, &mut out, &mut stats)?;
    }

    // The last compaction of each series, and the only one for a series that
    // never reached the trigger. Sorting ascending here as well means the
    // render below reads `points` directly instead of cloning it.
    for s in out.values_mut() {
        if s.points.len() > q.max_points {
            s.compact(q.max_points);
        }
        s.points.sort_unstable_by_key(|p| p.0);
    }

    // Sorted by key so two identical queries produce byte-identical responses.
    // An ETag, a diff and a cache all depend on that, and a HashMap iteration
    // order does not provide it.
    let mut keys: Vec<&String> = out.keys().collect();
    keys.sort_unstable();
    keys.truncate(q.max_series);

    let mut j = Json::new();
    j.arr(|j| {
        for k in keys {
            let s = &out[k];
            let pts = &s.points;
            j.obj(|j| {
                j.raw(&s.desc);
                j.key("attributes");
                j.raw(&s.attrs);
                if s.dropped > 0 {
                    j.key("dropped_points");
                    j.u64(s.dropped as u64);
                }
                j.key("points");
                j.arr(|j| {
                    for &(ts, v) in pts {
                        j.arr(|j| {
                            j.i64(ts);
                            match v {
                                Pt::Int(i) => j.i64(i),
                                Pt::Double(d) => j.f64(d),
                            }
                        });
                    }
                });
                // The answer to "which trace made this spike". Omitted rather
                // than emitted empty, because most series have none and this is
                // the response a chart polls.
                if !s.exemplars.is_empty() {
                    let mut ex = s.exemplars.clone();
                    ex.sort_unstable_by_key(|e| e.0);
                    j.key("exemplars");
                    j.arr(|j| {
                        for (_, rendered) in &ex {
                            j.raw(rendered);
                        }
                    });
                }
            });
        }
    });
    Ok(Results {
        json: j.into_string(),
        stats,
        // Metrics are bounded by `max_series` and `max_points`, which cap what
        // a chart can render rather than cut a list short. Nothing to page.
        next: None,
    })
}

/// Every metric name present in the window, with its unit and kind.
///
/// This is what a UI puts in a dropdown and what an agent reads before writing
/// its first query, so it is a first-class endpoint rather than something to
/// derive from an unfiltered `series` call — the descriptor table is tens of
/// rows per block, while the points it describes are hundreds of thousands.
pub fn names(root: &Path, from: i64, to: i64) -> Result<Results> {
    let refs = block::scan(root, "metrics")?;
    let mut stats = Stats {
        blocks_total: refs.len(),
        ..Default::default()
    };
    let mut seen: HashMap<String, (String, u8)> = HashMap::new();

    for bref in &refs {
        if bref.max_ts < from || bref.min_ts > to {
            continue;
        }
        stats.blocks_scanned += 1;
        let Some(m) = load(bref, "metrics")? else {
            continue;
        };
        stats.rows_scanned += m.num_rows();
        for r in 0..m.num_rows() {
            let name = dict_str(&m, "name", r).unwrap_or("").to_owned();
            let unit = dict_str(&m, "unit", r).unwrap_or("").to_owned();
            let kind = u8_col(&m, "kind", r);
            seen.entry(name).or_insert((unit, kind));
        }
    }
    stats.rows_matched = seen.len();

    let mut names: Vec<&String> = seen.keys().collect();
    names.sort_unstable();
    let mut j = Json::new();
    j.arr(|j| {
        for n in names {
            let (unit, kind) = &seen[n];
            j.obj(|j| {
                j.key("name");
                j.str(n);
                j.key("unit");
                j.str(unit);
                j.key("kind");
                j.str(kind_name(*kind));
            });
        }
    });
    Ok(Results {
        json: j.into_string(),
        stats,
        // Metrics are bounded by `max_series` and `max_points`, which cap what
        // a chart can render rather than cut a list short. Nothing to page.
        next: None,
    })
}

fn kind_name(k: u8) -> &'static str {
    match k {
        x if x == MetricKind::Gauge as u8 => "gauge",
        x if x == MetricKind::Sum as u8 => "sum",
        x if x == MetricKind::Histogram as u8 => "histogram",
        x if x == MetricKind::ExponentialHistogram as u8 => "exponential_histogram",
        x if x == MetricKind::Summary as u8 => "summary",
        _ => "unset",
    }
}

fn load(bref: &BlockRef, name: &str) -> Result<Option<RecordBatch>> {
    let path = bref.dir.join(format!("{name}.arrow"));
    Ok(block::open_table_opt(&path)?.and_then(|t| t.batches.first().cloned()))
}

/// Child rows grouped by `parent_id`, indexed by it.
///
/// Ids are rebased dense from zero per block, so the parent id *is* the slot —
/// no hash map, and the whole thing is one pass over a `u32` buffer.
fn index_by_parent(b: &RecordBatch) -> Vec<Vec<u32>> {
    let Some(col) = b.column_by_name("parent_id") else {
        return Vec::new();
    };
    let parents = col.as_primitive::<UInt32Type>().values();
    let mut out: Vec<Vec<u32>> =
        vec![Vec::new(); parents.iter().copied().max().unwrap_or(0) as usize + 1];
    for (r, &p) in parents.iter().enumerate() {
        out[p as usize].push(r as u32);
    }
    out
}

fn exemplar_time(b: &RecordBatch, row: u32) -> i64 {
    b.column_by_name("time_unix_nano").map_or(0, |c| {
        c.as_primitive::<TimestampNanosecondType>()
            .value(row as usize)
    })
}

fn dict_str<'a>(b: &'a RecordBatch, col: &str, row: usize) -> Option<&'a str> {
    let c = b.column_by_name(col)?;
    let d = c.as_dictionary::<UInt16Type>();
    if d.is_null(row) {
        return None;
    }
    Some(
        d.values()
            .as_string::<i32>()
            .value(d.keys().value(row) as usize),
    )
}

fn u8_col(b: &RecordBatch, col: &str, row: usize) -> u8 {
    b.column_by_name(col)
        .map_or(0, |c| c.as_primitive::<UInt8Type>().value(row))
}

fn collect_block(
    bref: &BlockRef,
    q: &SeriesQuery,
    out: &mut HashMap<String, Series>,
    stats: &mut Stats,
) -> Result<()> {
    // A missing descriptor table means retention is unlinking this block under
    // us. Normal, not an error.
    let Some(metrics) = load(bref, "metrics")? else {
        return Ok(());
    };
    let n_metrics = metrics.num_rows();
    let metric_attrs = load(bref, "metric_attrs")?;
    let dp_attrs = load(bref, "dp_attrs")?;
    // Data point ids are one dense space across all four point tables, so one
    // list indexed by id serves every table below — the same property that lets
    // `dp_attrs` carry no discriminant.
    let exemplars = load(bref, "exemplars")?;
    let by_point = exemplars.as_ref().map(index_by_parent).unwrap_or_default();
    let resource_attrs = load(bref, "resource_attrs")?;
    let scope_attrs = load(bref, "scope_attrs")?;

    // Name filter: resolve the string against the dictionary once, then compare
    // u16 codes. A block whose dictionary lacks the name has no rows to check.
    let mut wanted = vec![true; n_metrics];
    // Set when the requested name was a derived one, so only that half of the
    // histogram is emitted rather than both.
    let mut only_suffix = "";
    if let Some(want) = &q.name {
        let d = metrics
            .column_by_name("name")
            .map(|c| c.as_dictionary::<UInt16Type>());
        let Some(d) = d else { return Ok(()) };
        let names = d.values().as_string::<i32>();
        // The descriptor dictionary holds `http.server.duration`; this function
        // hands back series called `http.server.duration.count`. A caller
        // pasting a name off its own previous answer — a chart legend, an agent
        // reading the result it just got — must not get a silent empty series
        // list, so a miss retries against the base name. A metric genuinely
        // called `foo.count` matches on the first try and keeps winning.
        let mut code = dict_index(names, want);
        if code.is_none() {
            for s in DERIVED {
                if let Some(base) = want.strip_suffix(s) {
                    code = dict_index(names, base);
                    only_suffix = s;
                    break;
                }
            }
        }
        let Some(code) = code else { return Ok(()) };
        let codes = d.keys().values();
        for (r, w) in wanted.iter_mut().enumerate() {
            *w = codes[r] == code;
        }
    }

    // For each term, which metric rows satisfy it *above* the point level. A
    // point still qualifies if its own attributes satisfy the term, so this is
    // one half of an OR evaluated per point below.
    let above: Vec<Vec<bool>> = q
        .terms
        .iter()
        .map(|t| {
            let key = match &t.target {
                crate::query::Target::Attr(k) | crate::query::Target::Field(k) => k,
            };
            let mut hit = vec![false; n_metrics];
            // Metric level: parent_id is the descriptor row number.
            if let Some(a) = &metric_attrs {
                for pid in attr_parents(a, key, t.op, &t.value) {
                    if let Some(s) = hit.get_mut(pid as usize) {
                        *s = true;
                    }
                }
            }
            // Resource and scope level: parent_id is an entity id, and the
            // descriptor row carries the foreign key.
            for (table, fk) in [(&resource_attrs, "resource_id"), (&scope_attrs, "scope_id")] {
                let (Some(a), Some(col)) = (table, metrics.column_by_name(fk)) else {
                    continue;
                };
                let ids = attr_parents(a, key, t.op, &t.value);
                let Some(&top) = ids.iter().max() else {
                    continue;
                };
                let mut want = vec![false; top as usize + 1];
                for id in ids {
                    want[id as usize] = true;
                }
                for (r, &id) in col.as_primitive::<UInt16Type>().values().iter().enumerate() {
                    if want.get(id as usize).copied().unwrap_or(false) {
                        hit[r] = true;
                    }
                }
            }
            hit
        })
        .collect();

    // Same, at the point level. Data point ids are one shared space across all
    // four point tables — that is why `dp_attrs` needs no table discriminant —
    // so one bitmap per term serves every table below.
    let dp_hit: Vec<Vec<bool>> = q
        .terms
        .iter()
        .map(|t| {
            let key = match &t.target {
                crate::query::Target::Attr(k) | crate::query::Target::Field(k) => k,
            };
            let mut hit = Vec::new();
            if let Some(a) = &dp_attrs {
                for pid in attr_parents(a, key, t.op, &t.value) {
                    if hit.len() <= pid as usize {
                        hit.resize(pid as usize + 1, false);
                    }
                    hit[pid as usize] = true;
                }
            }
            hit
        })
        .collect();

    // Attributes above the point level, rendered once per descriptor row rather
    // than once per point. A metric with 10k points would otherwise re-render
    // its resource attributes 10k times.
    let mut prefix: Vec<Option<Prefix>> = vec![None; n_metrics];

    for table in DP_TABLES {
        let Some(dp) = load(bref, table)? else {
            continue;
        };
        let n = dp.num_rows();
        stats.rows_scanned += n;
        let (Some(time), Some(mid), Some(did)) = (
            dp.column_by_name("time_unix_nano")
                .map(|c| &**c.as_primitive::<TimestampNanosecondType>().values()),
            dp.column_by_name("metric_id")
                .map(|c| &**c.as_primitive::<UInt32Type>().values()),
            dp.column_by_name("id")
                .map(|c| &**c.as_primitive::<UInt32Type>().values()),
        ) else {
            continue;
        };
        let vals = Values::for_table(table);

        for r in 0..n {
            let m = mid[r] as usize;
            if !(q.from..=q.to).contains(&time[r]) || !wanted.get(m).copied().unwrap_or(false) {
                continue;
            }
            let d = did[r] as usize;
            if !(0..q.terms.len()).all(|t| {
                above[t].get(m).copied().unwrap_or(false)
                    || dp_hit[t].get(d).copied().unwrap_or(false)
            }) {
                continue;
            }
            stats.rows_matched += 1;

            let (desc_prefix, upper) = prefix[m].get_or_insert_with(|| {
                (
                    describe(&metrics, m),
                    upper_attrs(&metrics, m, &metric_attrs, &resource_attrs, &scope_attrs),
                )
            });
            // The point's own attributes, merged over the inherited ones.
            let own = own_attrs(&dp_attrs, d as u32);
            let attrs = merge(upper, &own);

            for (suffix, v) in vals.at(&dp, r) {
                if !only_suffix.is_empty() && suffix != only_suffix {
                    continue;
                }
                let key = format!("{desc_prefix}\u{1}{suffix}\u{1}{attrs}");
                let s = out.entry(key).or_insert_with(|| Series {
                    desc: with_suffix(desc_prefix, suffix),
                    attrs: attrs.clone(),
                    points: Vec::new(),
                    dropped: 0,
                    exemplars: Vec::new(),
                });
                s.points.push((time[r], v));
                if s.points.len() >= 2 * q.max_points {
                    s.compact(q.max_points);
                }
                // A histogram yields two derived series from one point, and its
                // exemplars belong to both: whichever of `.count` and `.sum` is
                // charted, the spike in it points at the same traces.
                if let (Some(ex), Some(rows)) = (&exemplars, by_point.get(d)) {
                    for &er in rows {
                        if s.exemplars.len() >= MAX_EXEMPLARS {
                            break;
                        }
                        let mut j = Json::new();
                        j.obj(|j| crate::query::emit_fields(j, ex, er));
                        s.exemplars.push((exemplar_time(ex, er), j.into_string()));
                    }
                }
            }
        }
    }
    Ok(())
}

/// The descriptor's identity as rendered JSON members, without the trailing
/// comma. Doubles as the stable half of the series key.
fn describe(metrics: &RecordBatch, row: usize) -> String {
    let mut j = Json::new();
    j.key("name");
    j.str(dict_str(metrics, "name", row).unwrap_or(""));
    j.key("unit");
    j.str(dict_str(metrics, "unit", row).unwrap_or(""));
    j.key("kind");
    j.str(kind_name(u8_col(metrics, "kind", row)));
    j.key("temporality");
    j.u64(u8_col(metrics, "temporality", row) as u64);
    j.key("monotonic");
    j.bool(
        metrics
            .column_by_name("is_monotonic")
            .is_some_and(|c| c.as_boolean().value(row)),
    );
    j.into_string()
}

/// `describe` with the derived-series suffix folded into the name, so a
/// `.count` series reports the name a caller can query it back by.
///
/// That is only true because `collect_block` strips a [`DERIVED`] suffix when
/// the descriptor dictionary does not hold the requested name. Emitting a name
/// here that the filter there does not accept is the same bug as returning a
/// cursor nobody can page with.
fn with_suffix(desc: &str, suffix: &str) -> String {
    if suffix.is_empty() {
        return desc.to_owned();
    }
    // `describe` always emits `"name":"..."` first, so the closing quote of the
    // name is the second unescaped quote after the colon. Splitting on the known
    // prefix is cheaper and less fragile than re-rendering.
    match desc.find("\",\"unit\"") {
        Some(i) => format!("{}{suffix}{}", &desc[..i], &desc[i..]),
        None => desc.to_owned(),
    }
}

/// Attributes from the metric, resource and scope levels, sorted and deduped
/// with the most specific level winning.
fn upper_attrs(
    metrics: &RecordBatch,
    row: usize,
    metric_attrs: &Option<RecordBatch>,
    resource_attrs: &Option<RecordBatch>,
    scope_attrs: &Option<RecordBatch>,
) -> Vec<Attr> {
    let fk = |name: &str| {
        metrics
            .column_by_name(name)
            .map(|c| c.as_primitive::<UInt16Type>().value(row) as u32)
    };
    let mut v = Vec::new();
    // Least specific first: `merge_into` keeps the last write per key.
    for (table, parent) in [
        (resource_attrs, fk("resource_id")),
        (scope_attrs, fk("scope_id")),
        (metric_attrs, Some(row as u32)),
    ] {
        let (Some(a), Some(p)) = (table, parent) else {
            continue;
        };
        collect_attrs(a, p, &mut v);
    }
    dedup_last(&mut v);
    v
}

fn own_attrs(dp_attrs: &Option<RecordBatch>, dp_id: u32) -> Vec<Attr> {
    let mut v = Vec::new();
    if let Some(a) = dp_attrs {
        collect_attrs(a, dp_id, &mut v);
    }
    dedup_last(&mut v);
    v
}

fn collect_attrs(a: &RecordBatch, parent: u32, out: &mut Vec<Attr>) {
    let parents = a.column(0).as_primitive::<UInt32Type>().values();
    for r in 0..a.num_rows() {
        if parents[r] != parent {
            continue;
        }
        let mut j = Json::new();
        emit_attr(&mut j, a, r);
        out.push((attr_key(a, r).to_owned(), j.into_string()));
    }
}

fn dedup_last(v: &mut Vec<Attr>) {
    // Stable, so entries pushed later — from the more specific level — sort
    // after their earlier namesakes and win the dedup.
    v.sort_by(|a, b| a.0.cmp(&b.0));
    v.dedup_by(|a, b| {
        if a.0 == b.0 {
            std::mem::swap(a, b);
            true
        } else {
            false
        }
    });
}

/// Render the union of two sorted attribute lists as a JSON object, `own`
/// winning on collision. A merge, not a concatenation and a re-sort: `upper` is
/// already sorted and shared by every point of the metric.
fn merge(upper: &[Attr], own: &[Attr]) -> String {
    let mut j = Json::new();
    j.obj(|j| {
        let (mut i, mut k) = (0, 0);
        while i < upper.len() || k < own.len() {
            let take_own = match (upper.get(i), own.get(k)) {
                (Some(u), Some(o)) => {
                    if u.0 == o.0 {
                        i += 1;
                    }
                    o.0 <= u.0
                }
                (None, Some(_)) => true,
                _ => false,
            };
            let (key, val) = if take_own {
                k += 1;
                &own[k - 1]
            } else {
                i += 1;
                &upper[i - 1]
            };
            j.key(key);
            j.raw(val);
        }
    });
    j.into_string()
}

/// Which columns of a point table carry chartable values, resolved once per
/// table instead of per row.
enum Values {
    /// `int` and `double`, exactly one set per row.
    Number,
    /// `count` and `sum`, emitted as `<name>.count` and `<name>.sum`.
    CountSum,
    None,
}

impl Values {
    fn for_table(table: &str) -> Values {
        match table {
            "number_dp" => Values::Number,
            "hist_dp" | "exp_hist_dp" | "summary_dp" => Values::CountSum,
            _ => Values::None,
        }
    }

    fn at(&self, dp: &RecordBatch, r: usize) -> Vec<(&'static str, Pt)> {
        match self {
            Values::Number => {
                if let Some(c) = dp.column_by_name("int") {
                    let a = c.as_primitive::<Int64Type>();
                    if !a.is_null(r) {
                        return vec![("", Pt::Int(a.value(r)))];
                    }
                }
                if let Some(c) = dp.column_by_name("double") {
                    let a = c.as_primitive::<Float64Type>();
                    if !a.is_null(r) {
                        return vec![("", Pt::Double(a.value(r)))];
                    }
                }
                Vec::new()
            }
            Values::CountSum => {
                let mut v = Vec::with_capacity(2);
                if let Some(c) = dp.column_by_name("count") {
                    let a = c.as_primitive::<UInt64Type>();
                    if !a.is_null(r) {
                        v.push((COUNT, Pt::Int(a.value(r) as i64)));
                    }
                }
                if let Some(c) = dp.column_by_name("sum") {
                    let a = c.as_primitive::<Float64Type>();
                    if !a.is_null(r) {
                        v.push((SUM, Pt::Double(a.value(r))));
                    }
                }
                v
            }
            Values::None => Vec::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mira_proto::collector::metrics::v1::ExportMetricsServiceRequest;
    use mira_proto::metrics::v1::metric::Data;
    use mira_proto::metrics::v1::number_data_point::Value as NumValue;
    use mira_proto::metrics::v1::{
        Gauge, Histogram, HistogramDataPoint, Metric, NumberDataPoint, ResourceMetrics,
        ScopeMetrics,
    };

    /// A histogram comes back as two series called `<name>.count` and
    /// `<name>.sum`, and those are the names a chart legend shows and an agent
    /// reads off its own previous answer. Matching only the base name against
    /// the descriptor dictionary made that round trip return an empty list with
    /// a 200, which reads as "the metric stopped reporting".
    #[test]
    fn a_derived_series_name_queries_back_to_the_series_it_names() {
        let dir = std::env::temp_dir().join(format!("mira-derived-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);

        let req = ExportMetricsServiceRequest {
            resource_metrics: vec![ResourceMetrics {
                scope_metrics: vec![ScopeMetrics {
                    metrics: vec![
                        Metric {
                            name: "http.server.duration".into(),
                            data: Some(Data::Histogram(Histogram {
                                data_points: vec![HistogramDataPoint {
                                    time_unix_nano: 1_000,
                                    count: 3,
                                    sum: Some(1.5),
                                    ..Default::default()
                                }],
                                ..Default::default()
                            })),
                            ..Default::default()
                        },
                        // A gauge whose name already ends in `.count`. The base
                        // name has to win the lookup, or this metric becomes
                        // unreachable the moment the stripping is added.
                        Metric {
                            name: "queue.depth.count".into(),
                            data: Some(Data::Gauge(Gauge {
                                data_points: vec![NumberDataPoint {
                                    time_unix_nano: 1_000,
                                    value: Some(NumValue::AsInt(42)),
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

        let mut b = crate::metrics::MetricsBuilder::new();
        b.append_request(&req).unwrap();
        let sealed = b.finish().unwrap();
        crate::block::publish(&dir, "metrics", crate::block::node_id("a"), 0, &sealed).unwrap();

        let ask = |name: &str| {
            series(
                &dir,
                &SeriesQuery {
                    name: Some(name.into()),
                    from: 0,
                    to: 10_000,
                    terms: Vec::new(),
                    max_series: 100,
                    max_points: 100,
                },
            )
            .unwrap()
            .json
        };

        // The base name still returns both halves.
        let both = ask("http.server.duration");
        for half in [COUNT, SUM] {
            assert!(
                both.contains(&format!(r#""name":"http.server.duration{half}""#)),
                "{both}"
            );
        }

        // Each derived name returns exactly the series it names, and only it.
        let c = ask("http.server.duration.count");
        assert!(c.contains(r#""name":"http.server.duration.count""#), "{c}");
        assert!(!c.contains(SUM), "{c}");
        assert!(c.contains("[1000,3]"), "{c}");
        let s = ask("http.server.duration.sum");
        assert!(s.contains(r#""name":"http.server.duration.sum""#), "{s}");
        assert!(!s.contains(COUNT), "{s}");
        assert!(s.contains("[1000,1.5]"), "{s}");

        // A metric that really is called `x.count` matches before the suffix is
        // stripped.
        let q = ask("queue.depth.count");
        assert!(q.contains(r#""name":"queue.depth.count""#), "{q}");
        assert!(q.contains("[1000,42]"), "{q}");

        // The retry widens the lookup; it does not make it match anything. A
        // derived name on a metric that has no derived series is still empty,
        // and so is a base name nothing carries.
        assert_eq!(ask("queue.depth.count.sum"), "[]");
        assert_eq!(ask("nope.count"), "[]");

        let _ = std::fs::remove_dir_all(&dir);
    }
}
