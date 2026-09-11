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

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::Path;
use std::sync::Arc;

use arrow_array::cast::AsArray;
use arrow_array::types::{
    Float64Type, Int64Type, TimestampNanosecondType, UInt8Type, UInt16Type, UInt32Type, UInt64Type,
};
use arrow_array::{Array, RecordBatch};

use crate::block::{self, Src};
use crate::error::Result;
use crate::json::Json;
use crate::query::{Results, Stats, Term, attr_key, attr_parents, dict_index, emit_attr};
use crate::schema::MetricKind;
use crate::signal::Open;

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
    series_open(root, q, &[])
}

/// As [`series`], but also reads the open block's snapshot — see
/// [`crate::query::search_open`], which this mirrors exactly.
pub fn series_open(root: &Path, q: &SeriesQuery, open: &[Arc<Open>]) -> Result<Results> {
    let disk = block::scan(root, "metrics")?;
    let refs = block::sources(&disk, open);
    let mut stats = Stats {
        blocks_total: refs.len(),
        ..Default::default()
    };
    let mut out: BTreeMap<String, Series> = BTreeMap::new();
    let mut dropped: BTreeSet<String> = BTreeSet::new();

    for bref in &refs {
        // The directory name is the whole index; a block outside the window is
        // never opened.
        if bref.max_ts < q.from || bref.min_ts > q.to {
            continue;
        }
        stats.blocks_scanned += 1;
        collect_block(bref, q, &mut out, &mut dropped, &mut stats)?;
    }
    stats.dropped_series = dropped.len();

    // The last compaction of each series, and the only one for a series that
    // never reached the trigger. Sorting ascending here as well means the
    // render below reads `points` directly instead of cloning it.
    for s in out.values_mut() {
        if s.points.len() > q.max_points {
            s.compact(q.max_points);
        }
        s.points.sort_unstable_by_key(|p| p.0);
    }

    // A `BTreeMap`, so this is already in key order: two identical queries
    // produce byte-identical responses, which an ETag, a diff and a cache all
    // depend on and which a `HashMap` iteration order does not provide. It is
    // also already at most `max_series` long — see [`bound`].
    let mut j = Json::new();
    j.arr(|j| {
        for s in out.values() {
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
                            // Both 64-bit halves go out as strings, for the
                            // reason [`Json::i64_str`] gives: a nanosecond
                            // timestamp is ~1.7e18 and an OTLP `as_int` is an
                            // `sfixed64`, and a counter past 2^53 read back
                            // through a double loses exactly the low bits that
                            // made it worth charting. A double stays a number
                            // — it was never anything else.
                            j.i64_str(ts);
                            match v {
                                Pt::Int(i) => j.i64_str(i),
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
    names_open(root, from, to, &[])
}

/// As [`names`], but also reads the open block's snapshot. A metric name that
/// has only ever been written to the open block is exactly the one a dropdown
/// must not omit — it is the new one.
pub fn names_open(root: &Path, from: i64, to: i64, open: &[Arc<Open>]) -> Result<Results> {
    let disk = block::scan(root, "metrics")?;
    let refs = block::sources(&disk, open);
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

fn load(bref: &Src, name: &str) -> Result<Option<RecordBatch>> {
    bref.load(name)
}

/// Child rows grouped by `parent_id`, indexed by it.
///
/// Ids are rebased dense from zero per block, so the parent id *is* the slot —
/// no hash map, and the whole thing is one pass over a `u32` buffer.
pub(crate) fn index_by_parent(b: &RecordBatch) -> Vec<Vec<u32>> {
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

/// Hold `out` to `max_series` entries by evicting its largest key.
///
/// The result is the same set `sort(keys).truncate(max_series)` produced, and
/// the argument is short enough to check: the answer is the `max_series`
/// smallest keys, and a key that belongs in it can never be the largest key of
/// a map that is already one over — everything else in that map is smaller, so
/// there are already `max_series` keys ahead of it. Evicting the maximum
/// therefore only ever discards a key the sort would have truncated. It also
/// cannot come back: the evicted key is greater than every key in the map, and
/// the map's maximum only falls, so the same key arriving from a later block is
/// evicted again on sight.
///
/// The difference is *when*, and that is the whole point. The old map grew one
/// entry per distinct (name, unit, kind, temporality, monotonic, attributes)
/// tuple until the query finished, each carrying a rendered descriptor, a
/// rendered attribute object and a key concatenating both — about a kilobyte.
/// `max_points` bounded the points inside a series and nothing bounded the
/// series, so a `query_metric` with no `name` over a store with a request id in
/// a data-point attribute allocated until the process died.
///
/// The evicted key is kept, without its payload, so the response can say how
/// many series it is not showing. ponytail: that record is itself capped at
/// `max_series` keys, so `dropped_series` saturates rather than counting an
/// unbounded number of them exactly — remembering every key to count it is the
/// allocation this function exists to prevent. `dropped_series == max_series`
/// reads as "at least". Counting evictions instead of distinct keys was the
/// other option and it is worse: a series above the cap is re-inserted and
/// re-evicted once per point, so a store with 100 series and a cap of 64 would
/// report tens of thousands dropped.
fn bound(out: &mut BTreeMap<String, Series>, dropped: &mut BTreeSet<String>, max: usize) {
    while out.len() > max {
        let Some((k, _)) = out.pop_last() else { break };
        if dropped.len() < max {
            dropped.insert(k);
        }
    }
}

fn collect_block(
    bref: &Src,
    q: &SeriesQuery,
    out: &mut BTreeMap<String, Series>,
    dropped: &mut BTreeSet<String>,
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
                // Applied per point, not once at the end: the whole reason
                // `max_series` exists is that the map between here and the end
                // is what runs the process out of memory.
                bound(out, dropped, q.max_series);
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
    use arrow_array::{ArrayRef, Float64Array, Int64Array, UInt32Array, UInt64Array};
    use mira_proto::collector::metrics::v1::ExportMetricsServiceRequest;
    use mira_proto::common::v1::any_value::Value as AnyVal;
    use mira_proto::common::v1::{AnyValue, InstrumentationScope, KeyValue};
    use mira_proto::metrics::v1::metric::Data;
    use mira_proto::metrics::v1::number_data_point::Value as NumValue;
    use mira_proto::metrics::v1::{
        Exemplar, ExponentialHistogram, ExponentialHistogramDataPoint, Gauge, Histogram,
        HistogramDataPoint, Metric, NumberDataPoint, ResourceMetrics, ScopeMetrics, Sum, Summary,
        SummaryDataPoint, exemplar,
    };
    use mira_proto::resource::v1::Resource;

    /// A named temporary directory, emptied first so a crashed run does not
    /// hand the next one a half-written store.
    fn tmp(name: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("mira-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        d
    }

    fn kv(k: &str, v: &str) -> KeyValue {
        KeyValue {
            key: k.into(),
            value: Some(AnyValue {
                value: Some(AnyVal::StringValue(v.into())),
            }),
        }
    }

    /// Publish `metrics` as one block, and hand back its directory so a test
    /// can damage it the way retention or an older writer would.
    fn publish(
        dir: &std::path::Path,
        seq: u64,
        resource: Option<Resource>,
        scope: Option<InstrumentationScope>,
        metrics: Vec<Metric>,
    ) -> std::path::PathBuf {
        let req = ExportMetricsServiceRequest {
            resource_metrics: vec![ResourceMetrics {
                resource,
                scope_metrics: vec![ScopeMetrics {
                    scope,
                    metrics,
                    ..Default::default()
                }],
                ..Default::default()
            }],
        };
        let mut b = crate::metrics::MetricsBuilder::new();
        b.append_request(&req).unwrap();
        let sealed = b.finish().unwrap();
        crate::block::publish(dir, "metrics", crate::block::node_id("a"), seq, 0, &sealed)
            .unwrap()
            .dir
    }

    /// A gauge of one integer point per timestamp, the simplest thing that
    /// produces a series.
    fn gauge(name: &str, points: &[(u64, i64)]) -> Metric {
        Metric {
            name: name.into(),
            data: Some(Data::Gauge(Gauge {
                data_points: points
                    .iter()
                    .map(|&(t, v)| NumberDataPoint {
                        time_unix_nano: t,
                        value: Some(NumValue::AsInt(v)),
                        ..Default::default()
                    })
                    .collect(),
            })),
            ..Default::default()
        }
    }

    fn query(dir: &std::path::Path, q: &SeriesQuery) -> Results {
        series(dir, q).unwrap()
    }

    /// The query every test below varies one field of.
    fn wide() -> SeriesQuery {
        SeriesQuery {
            name: None,
            from: 0,
            to: i64::MAX,
            terms: Vec::new(),
            max_series: 100,
            max_points: 100,
        }
    }

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
        crate::block::publish(&dir, "metrics", crate::block::node_id("a"), 0, 0, &sealed).unwrap();

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
        // Both halves of a point are strings when the value is an integer: the
        // timestamp is nanoseconds and a `.count` is a `uint64`, so both are
        // 64-bit and both go out the OTLP/JSON way. `.sum` below is a double
        // and stays a bare number, which is what makes this pair worth having.
        assert!(c.contains(r#"["1000","3"]"#), "{c}");
        let s = ask("http.server.duration.sum");
        assert!(s.contains(r#""name":"http.server.duration.sum""#), "{s}");
        assert!(!s.contains(COUNT), "{s}");
        assert!(s.contains(r#"["1000",1.5]"#), "{s}");

        // A metric that really is called `x.count` matches before the suffix is
        // stripped.
        let q = ask("queue.depth.count");
        assert!(q.contains(r#""name":"queue.depth.count""#), "{q}");
        assert!(q.contains(r#"["1000","42"]"#), "{q}");

        // The retry widens the lookup; it does not make it match anything. A
        // derived name on a metric that has no derived series is still empty,
        // and so is a base name nothing carries.
        assert_eq!(ask("queue.depth.count.sum"), "[]");
        assert_eq!(ask("nope.count"), "[]");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `max_series` bounds the map, not just the render.
    ///
    /// A high-cardinality attribute — a request id, a customer id, a pod name in
    /// a cluster that recycles them — makes one metric name expand into a series
    /// per distinct value. Accumulating them all and truncating at the end meant
    /// the cap the caller set had no effect on the memory the query took, and
    /// the store is the one thing here with no bound on how many values it
    /// holds.
    #[test]
    fn the_series_cap_bounds_the_map_and_says_what_it_refused() {
        let dir = std::env::temp_dir().join(format!("mira-cap-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);

        // Ten series of one metric, distinguished only by an attribute, and
        // named so that key order is the order a reader would guess.
        let req = ExportMetricsServiceRequest {
            resource_metrics: vec![ResourceMetrics {
                scope_metrics: vec![ScopeMetrics {
                    metrics: vec![Metric {
                        name: "rpc.duration".into(),
                        data: Some(Data::Gauge(Gauge {
                            data_points: (0..10)
                                .map(|i| NumberDataPoint {
                                    time_unix_nano: 1_000,
                                    value: Some(NumValue::AsInt(i)),
                                    attributes: vec![mira_proto::common::v1::KeyValue {
                                        key: "peer".into(),
                                        value: Some(mira_proto::common::v1::AnyValue {
                                            value: Some(
                                                mira_proto::common::v1::any_value::Value::StringValue(
                                                    format!("p{i}"),
                                                ),
                                            ),
                                        }),
                                    }],
                                    ..Default::default()
                                })
                                .collect(),
                        })),
                        ..Default::default()
                    }],
                    ..Default::default()
                }],
                ..Default::default()
            }],
        };

        let mut b = crate::metrics::MetricsBuilder::new();
        b.append_request(&req).unwrap();
        let sealed = b.finish().unwrap();
        crate::block::publish(&dir, "metrics", crate::block::node_id("a"), 0, 0, &sealed).unwrap();

        let ask = |max_series: usize| {
            series(
                &dir,
                &SeriesQuery {
                    name: None,
                    from: 0,
                    to: 10_000,
                    terms: Vec::new(),
                    max_series,
                    max_points: 100,
                },
            )
            .unwrap()
        };

        // Under the cap: everything, and nothing refused.
        let r = ask(100);
        assert_eq!(r.json.matches(r#""peer""#).count(), 10, "{}", r.json);
        assert_eq!(r.stats.dropped_series, 0);

        // Over it: the smallest keys survive, which is the same set the old
        // sort-then-truncate produced — that equivalence is the whole safety
        // argument for evicting the maximum as we go, so it is what this checks
        // rather than the count alone.
        let r = ask(8);
        for i in 0..10 {
            assert_eq!(
                r.json.contains(&format!(r#""peer":"p{i}""#)),
                i < 8,
                "p{i}: {}",
                r.json
            );
        }
        assert_eq!(r.stats.dropped_series, 2);

        // The ponytail ceiling, asserted rather than left to be discovered: the
        // record of what was refused is itself capped at `max_series`, so with
        // six dropped and a cap of four the answer is four and reads as "at
        // least". Remembering every key exactly is the allocation the cap
        // exists to prevent.
        let r = ask(4);
        assert_eq!(r.json.matches(r#""peer""#).count(), 4, "{}", r.json);
        assert_eq!(r.stats.dropped_series, 4);

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A point's value lives in whichever column its table set, and a row that
    /// set none must yield no point rather than a zero. A gauge with no `value`
    /// on the wire means "not reported"; charting it as 0 invents a reading
    /// nobody took, and on a `.sum` it would drag an average down.
    ///
    /// The null cases are built by hand because today's writer never emits
    /// them — `count` is non-nullable and every gauge point has one of `int`
    /// and `double`. The reader is the half that outlives the writer that wrote
    /// the block, so it is the half that has to survive them.
    #[test]
    fn a_point_yields_the_value_of_whichever_column_its_table_actually_set() {
        for (table, want) in [
            ("number_dp", "number"),
            ("hist_dp", "countsum"),
            ("exp_hist_dp", "countsum"),
            ("summary_dp", "countsum"),
            // Not a point table. `collect_block` only ever asks about the four,
            // so this arm is what stops a fifth table added later from being
            // charted as whatever its first two columns happen to be called.
            ("logs", "none"),
        ] {
            let got = match Values::for_table(table) {
                Values::Number => "number",
                Values::CountSum => "countsum",
                Values::None => "none",
            };
            assert_eq!(got, want, "{table}");
        }

        /// `(suffix, value)` as text, because `Pt` is deliberately not `Eq` —
        /// comparing two charted doubles for equality is a bug everywhere else.
        fn shape(v: Vec<(&'static str, Pt)>) -> Vec<String> {
            v.into_iter()
                .map(|(s, p)| match p {
                    Pt::Int(i) => format!("{s}=i{i}"),
                    Pt::Double(d) => format!("{s}=d{d}"),
                })
                .collect()
        }

        let num = RecordBatch::try_from_iter(vec![
            (
                "int",
                Arc::new(Int64Array::from(vec![Some(7), None, None])) as ArrayRef,
            ),
            (
                "double",
                Arc::new(Float64Array::from(vec![None, Some(0.5), None])) as ArrayRef,
            ),
        ])
        .unwrap();
        assert_eq!(shape(Values::Number.at(&num, 0)), ["=i7"]);
        assert_eq!(shape(Values::Number.at(&num, 1)), ["=d0.5"]);
        assert!(shape(Values::Number.at(&num, 2)).is_empty());

        // A writer that never emitted the column at all, rather than emitting
        // it null: the reader must fall through to the other one, not index a
        // column that is not there.
        let only_double = RecordBatch::try_from_iter(vec![(
            "double",
            Arc::new(Float64Array::from(vec![1.25])) as ArrayRef,
        )])
        .unwrap();
        assert_eq!(shape(Values::Number.at(&only_double, 0)), ["=d1.25"]);

        let cs = RecordBatch::try_from_iter(vec![
            (
                "count",
                Arc::new(UInt64Array::from(vec![None, Some(4), Some(4), None])) as ArrayRef,
            ),
            (
                "sum",
                Arc::new(Float64Array::from(vec![Some(1.5), None, Some(2.5), None])) as ArrayRef,
            ),
        ])
        .unwrap();
        assert_eq!(shape(Values::CountSum.at(&cs, 0)), [".sum=d1.5"]);
        assert_eq!(shape(Values::CountSum.at(&cs, 1)), [".count=i4"]);
        assert_eq!(
            shape(Values::CountSum.at(&cs, 2)),
            [".count=i4", ".sum=d2.5"]
        );
        assert!(shape(Values::CountSum.at(&cs, 3)).is_empty());

        // The half of a histogram that is missing is missing, not zero: a
        // `sum`-less point charts a `.count` series and no `.sum` series at
        // all. `starts_with`, not `==`: `shape` renders the value into the
        // string, so an equality against the bare suffix could never match and
        // would hold however many `.sum` points came back.
        assert!(
            !shape(Values::CountSum.at(&cs, 1))
                .iter()
                .any(|s| s.starts_with(SUM))
        );
        assert!(shape(Values::None.at(&cs, 2)).is_empty());
    }

    /// Every kind OTLP can declare gets a name a caller can read, including the
    /// two that were added last and the descriptor with no `data` at all.
    ///
    /// This is the dropdown an agent reads before writing its first query. A
    /// kind rendered as the wrong word — or an exponential histogram rendered
    /// as "unset" because the match arm was never added — is a metric the
    /// caller cannot tell apart from one that is not reporting.
    #[test]
    fn every_metric_kind_reports_the_name_a_dropdown_shows() {
        let dir = tmp("series-kinds");
        let point = NumberDataPoint {
            time_unix_nano: 1_000,
            value: Some(NumValue::AsInt(1)),
            ..Default::default()
        };
        let named = |name: &str, unit: &str, data: Option<Data>| Metric {
            name: name.into(),
            unit: unit.into(),
            data,
            ..Default::default()
        };
        publish(
            &dir,
            0,
            None,
            None,
            vec![
                // No `data` oneof: a descriptor somebody's exporter believes in
                // that owns no points. It still belongs in the listing.
                named("a.declared", "1", None),
                named(
                    "b.gauge",
                    "By",
                    Some(Data::Gauge(Gauge {
                        data_points: vec![point.clone()],
                    })),
                ),
                named(
                    "c.sum",
                    "1",
                    Some(Data::Sum(Sum {
                        data_points: vec![point],
                        ..Default::default()
                    })),
                ),
                named(
                    "d.hist",
                    "s",
                    Some(Data::Histogram(Histogram {
                        data_points: vec![HistogramDataPoint {
                            time_unix_nano: 1_000,
                            count: 2,
                            sum: Some(3.0),
                            ..Default::default()
                        }],
                        ..Default::default()
                    })),
                ),
                named(
                    "e.exp",
                    "s",
                    Some(Data::ExponentialHistogram(ExponentialHistogram {
                        data_points: vec![ExponentialHistogramDataPoint {
                            time_unix_nano: 1_000,
                            count: 2,
                            sum: Some(3.0),
                            ..Default::default()
                        }],
                        ..Default::default()
                    })),
                ),
                named(
                    "f.summary",
                    "s",
                    Some(Data::Summary(Summary {
                        data_points: vec![SummaryDataPoint {
                            time_unix_nano: 1_000,
                            count: 2,
                            sum: 3.0,
                            ..Default::default()
                        }],
                    })),
                ),
            ],
        );

        let r = names(&dir, 0, 10_000).unwrap();
        assert_eq!(
            r.json,
            concat!(
                r#"[{"name":"a.declared","unit":"1","kind":"unset"},"#,
                r#"{"name":"b.gauge","unit":"By","kind":"gauge"},"#,
                r#"{"name":"c.sum","unit":"1","kind":"sum"},"#,
                r#"{"name":"d.hist","unit":"s","kind":"histogram"},"#,
                r#"{"name":"e.exp","unit":"s","kind":"exponential_histogram"},"#,
                r#"{"name":"f.summary","unit":"s","kind":"summary"}]"#,
            )
        );

        // The same words come back on the series themselves, and the three
        // aggregate kinds each split into the two derived halves.
        let json = query(&dir, &wide()).json;
        for (name, kind) in [
            ("b.gauge", "gauge"),
            ("c.sum", "sum"),
            ("d.hist.count", "histogram"),
            ("d.hist.sum", "histogram"),
            ("e.exp.count", "exponential_histogram"),
            ("e.exp.sum", "exponential_histogram"),
            ("f.summary.count", "summary"),
            ("f.summary.sum", "summary"),
        ] {
            assert!(
                json.contains(&format!(r#""name":"{name}","unit":"#))
                    && json.contains(&format!(r#""kind":"{kind}""#)),
                "{name}/{kind}: {json}"
            );
        }
        // A descriptor with no points charts nothing, however loudly it is
        // declared.
        assert!(!json.contains("a.declared"), "{json}");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A block outside the window is never opened, and a series over the point
    /// cap keeps its newest points and says how many it dropped.
    ///
    /// Both are the difference between a chart and a lie. Opening every block
    /// makes the directory name — the whole index — worthless; keeping an
    /// arbitrary subset of points and sorting it at render time draws a
    /// contiguous line through whichever rows the directory listing reached
    /// first, with no sign that anything is missing.
    #[test]
    fn a_series_is_bounded_by_the_window_asked_for_and_the_points_it_can_chart() {
        let dir = tmp("series-bounds");
        let pts: Vec<(u64, i64)> = (1..=6).map(|i| (i as u64 * 1_000, i)).collect();
        publish(&dir, 0, None, None, vec![gauge("m", &pts)]);
        // Hours later, so it lands in its own partition and its own directory
        // name. Nothing in the window below can reach it.
        publish(
            &dir,
            1,
            None,
            None,
            vec![gauge("m", &[(9_000_000_000_000, 99)])],
        );

        let mut q = wide();
        q.to = 10_000;
        q.max_points = 4;
        let r = query(&dir, &q);

        assert_eq!(r.stats.blocks_total, 2);
        assert_eq!(r.stats.blocks_scanned, 1, "the far block was opened");
        // Six points offered, four charted, and the two dropped are the two
        // oldest — not the two the scan happened to see last.
        assert!(r.json.contains(r#""dropped_points":2"#), "{}", r.json);
        assert!(
            r.json
                .contains(r#""points":[["3000","3"],["4000","4"],["5000","5"],["6000","6"]]"#),
            "{}",
            r.json
        );

        // Under the cap there is no `dropped_points` member at all: a chart
        // must be able to treat its presence as "something is missing".
        let mut q = wide();
        q.to = 10_000;
        let r = query(&dir, &q);
        assert!(!r.json.contains("dropped_points"), "{}", r.json);
        assert!(r.json.contains(r#"["1000","1"]"#), "{}", r.json);

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A term is satisfied at whichever of the four levels carries the
    /// attribute, and a block missing a level is filtered by the levels it has.
    ///
    /// Whether `service.name` is a resource attribute or a data point one is a
    /// detail of whoever configured the SDK. A filter that only looked at the
    /// point level would return nothing for the single most common metrics
    /// query there is, with a 200 and an empty list.
    #[test]
    fn a_term_is_satisfied_at_whichever_level_carries_the_attribute() {
        let dir = tmp("series-levels");
        publish(
            &dir,
            0,
            Some(Resource {
                attributes: vec![kv("service.name", "api")],
                ..Default::default()
            }),
            Some(InstrumentationScope {
                attributes: vec![kv("otel.lib", "sdk")],
                ..Default::default()
            }),
            vec![
                Metric {
                    metadata: vec![kv("tier", "gold")],
                    ..gauge("m1", &[(1_000, 1)])
                },
                Metric {
                    ..gauge("m2", &[(1_000, 2)])
                },
            ],
        );
        // No resource and no scope: both attribute tables are empty, so
        // `publish` never writes them. Filtering must fall back to the levels
        // this block does have rather than treating the absence as a match.
        publish(
            &dir,
            1,
            None,
            None,
            vec![Metric {
                metadata: vec![kv("tier", "gold")],
                ..gauge("m3", &[(1_000, 3)])
            }],
        );

        // Point-level attributes, added after the fact so each metric above
        // stays readable.
        let with_pod = |name: &str, v: i64, pod: &str| Metric {
            data: Some(Data::Gauge(Gauge {
                data_points: vec![NumberDataPoint {
                    time_unix_nano: 1_000,
                    value: Some(NumValue::AsInt(v)),
                    attributes: vec![kv("pod", pod)],
                    ..Default::default()
                }],
            })),
            ..Metric {
                name: name.into(),
                ..Default::default()
            }
        };
        publish(
            &dir,
            2,
            Some(Resource {
                attributes: vec![kv("service.name", "db")],
                ..Default::default()
            }),
            None,
            vec![with_pod("m4", 4, "p4")],
        );

        let ask = |key: &str, want: &str| {
            let mut q = wide();
            q.to = 10_000;
            q.terms = vec![Term {
                target: crate::query::Target::Attr(key.into()),
                op: crate::query::Op::Eq,
                value: crate::query::Value::Str(want.into()),
            }];
            let r = query(&dir, &q);
            let mut names: Vec<String> = Vec::new();
            for m in ["m1", "m2", "m3", "m4"] {
                if r.json.contains(&format!(r#""name":"{m}""#)) {
                    names.push(m.to_owned());
                }
            }
            names
        };

        // Resource level. The second block has no resource attributes at all
        // and must not match on the strength of not having them.
        assert_eq!(ask("service.name", "api"), ["m1", "m2"]);
        assert_eq!(ask("service.name", "db"), ["m4"]);
        // Scope level.
        assert_eq!(ask("otel.lib", "sdk"), ["m1", "m2"]);
        // Metric level — `Metric.metadata`, which is per descriptor row and so
        // reaches both blocks that declare it.
        assert_eq!(ask("tier", "gold"), ["m1", "m3"]);
        // Point level.
        assert_eq!(ask("pod", "p4"), ["m4"]);
        // A term nothing carries at any level matches nothing, rather than
        // falling back to "no opinion" and matching everything.
        assert!(ask("tier", "bronze").is_empty());
        assert!(ask("nope", "x").is_empty());

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The four attribute levels merge with the most specific winning, and the
    /// merged map is what identifies the series.
    ///
    /// Attributes are half the grouping key, so getting the precedence wrong
    /// does not just mislabel a line — it splits one series into two, or fuses
    /// two into one, and neither is visible in the response.
    #[test]
    fn attributes_from_every_level_merge_with_the_most_specific_winning() {
        let dir = tmp("series-merge");
        publish(
            &dir,
            0,
            Some(Resource {
                attributes: vec![kv("env", "prod"), kv("region", "eu")],
                ..Default::default()
            }),
            None,
            vec![Metric {
                // Same key as the resource carries, one level down.
                metadata: vec![kv("env", "staging")],
                data: Some(Data::Gauge(Gauge {
                    data_points: vec![NumberDataPoint {
                        time_unix_nano: 1_000,
                        value: Some(NumValue::AsInt(1)),
                        // And again, one level further down.
                        attributes: vec![kv("env", "canary"), kv("pod", "x")],
                        ..Default::default()
                    }],
                })),
                ..Metric {
                    name: "m".into(),
                    ..Default::default()
                }
            }],
        );

        let json = query(&dir, &wide()).json;
        // `env` resolves to the point's own value; the keys only an upper level
        // carries survive; and the losing values appear nowhere.
        assert!(
            json.contains(r#""attributes":{"env":"canary","pod":"x","region":"eu"}"#),
            "{json}"
        );
        assert!(
            !json.contains("prod") && !json.contains("staging"),
            "{json}"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Exemplars are capped per series, because nothing in OTLP bounds how many
    /// an exporter attaches to a point.
    ///
    /// Without the cap one misconfigured SDK inflates every chart response that
    /// touches its metric — and this is the response a chart polls on a timer.
    #[test]
    fn exemplars_are_capped_so_one_exporter_cannot_inflate_every_chart_response() {
        let dir = tmp("series-exemplars");
        publish(
            &dir,
            0,
            None,
            None,
            vec![Metric {
                data: Some(Data::Gauge(Gauge {
                    data_points: vec![NumberDataPoint {
                        time_unix_nano: 1_000,
                        value: Some(NumValue::AsInt(1)),
                        exemplars: (0..MAX_EXEMPLARS as i64 + 6)
                            .map(|i| Exemplar {
                                time_unix_nano: 1_000 + i as u64,
                                value: Some(exemplar::Value::AsInt(i)),
                                trace_id: vec![7u8; 16].into(),
                                ..Default::default()
                            })
                            .collect(),
                        ..Default::default()
                    }],
                })),
                ..Metric {
                    name: "m".into(),
                    ..Default::default()
                }
            }],
        );

        let json = query(&dir, &wide()).json;
        // Points carry their timestamp as a bare array element, so every
        // occurrence of the key is one exemplar object.
        assert_eq!(
            json.matches(r#""time_unix_nano""#).count(),
            MAX_EXEMPLARS,
            "{json}"
        );
        // Capped, not dropped: the ones that are there carry the trace id that
        // is the whole point of an exemplar.
        assert!(json.contains(r#""trace_id":"07070707"#), "{json}");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A table shaped differently from the one this reader writes contributes
    /// nothing, rather than panicking or charting garbage.
    ///
    /// Two things produce that shape and neither is an error: retention
    /// unlinking a block while a query walks it, and a block written by another
    /// version of the binary — the block directory is the manifest, so there is
    /// no schema version to check against and no coordinator to ask.
    #[test]
    fn a_table_the_reader_did_not_write_contributes_nothing_instead_of_panicking() {
        let dir = tmp("series-ragged");
        publish(&dir, 0, None, None, vec![gauge("ok", &[(1_000, 1)])]);
        let unlinked = publish(&dir, 1, None, None, vec![gauge("gone", &[(1_000, 2)])]);
        let ragged = publish(&dir, 2, None, None, vec![gauge("ragged", &[(1_000, 3)])]);

        // Retention got here first: the descriptor table is gone, but the
        // directory is still listed.
        std::fs::remove_file(unlinked.join("metrics.arrow")).unwrap();

        // An older writer's point table, with no `id` column to join
        // attributes and exemplars on. Staged and renamed, because a mapping
        // over a file being truncated is a SIGBUS, not an error.
        let table = ragged.join("number_dp.arrow");
        let mut b = crate::block::open_table_opt(&table)
            .unwrap()
            .unwrap()
            .batches[0]
            .clone();
        b.remove_column(b.schema().index_of("id").unwrap());
        let staged = ragged.join("number_dp.arrow.staged");
        crate::block::write_table(&staged, &b).unwrap();
        std::fs::rename(&staged, &table).unwrap();

        let r = query(&dir, &wide());
        assert_eq!(r.stats.blocks_scanned, 3, "all three were in the window");
        assert!(r.json.contains(r#""name":"ok""#), "{}", r.json);
        assert!(!r.json.contains("gone"), "{}", r.json);
        assert!(!r.json.contains("ragged"), "{}", r.json);

        // The name listing reads only the descriptor table, so the ragged block
        // still names its metric and the unlinked one names nothing.
        let n = names(&dir, 0, 10_000).unwrap();
        assert_eq!(
            n.json,
            concat!(
                r#"[{"name":"ok","unit":"","kind":"gauge"},"#,
                r#"{"name":"ragged","unit":"","kind":"gauge"}]"#,
            )
        );

        // The same tolerance one layer down, where the missing column is the
        // one the whole index is built on.
        let no_parent = RecordBatch::try_from_iter(vec![(
            "id",
            Arc::new(UInt32Array::from(vec![0u32, 1])) as ArrayRef,
        )])
        .unwrap();
        assert!(index_by_parent(&no_parent).is_empty());

        // And on a descriptor this renderer did not render: the suffix is
        // dropped rather than spliced into the middle of some other member.
        // Returning the input unchanged keeps the string valid JSON, which a
        // `find`-and-insert on a miss would not.
        assert_eq!(
            with_suffix(r#""kind":"histogram""#, COUNT),
            r#""kind":"histogram""#
        );
        assert_eq!(
            with_suffix(r#""name":"x","unit":"s""#, ""),
            r#""name":"x","unit":"s""#
        );
        assert_eq!(
            with_suffix(r#""name":"x","unit":"s""#, COUNT),
            r#""name":"x.count","unit":"s""#
        );

        let _ = std::fs::remove_dir_all(&dir);
    }
}
