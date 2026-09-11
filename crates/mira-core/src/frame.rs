//! The frame algebra: correlation as a closed set of operations (section 7.3).
//!
//! A **frame** is a bounded region of telemetry — a time window, plus the
//! traces and entities found in it. Every operation here is `Frame -> Frame`,
//! which is the property the design turns on: an investigation is a walk over
//! frames, every intermediate state is a legal frame, and there is no way to
//! build one that is not executable.
//!
//! That closure is what makes this the agentic surface rather than SQL. An
//! agent handed a star schema with EAV attribute tables writes wrong joins, and
//! they are *silently* wrong — a missing `parent_id` predicate returns a cross
//! product that looks like data. An agent handed `anchor` and three expanders
//! cannot express a wrong join at all.
//!
//! There is no `fetch` here, and that is deliberate. Once a frame names a trace
//! or a service, reading its rows is `trace_id = ...` or `service.name = ...`
//! through the ordinary [`crate::query::search`] — both of which the block
//! sidecars already prune on (section 7.4). A second read path would be a second
//! predicate language for no new answer.
//!
//! Blocking: this mmaps and page-faults, the same as [`crate::query`]. Callers
//! on an async runtime must go through `spawn_blocking`.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use arrow_array::cast::AsArray;
use arrow_array::types::{TimestampNanosecondType, UInt8Type, UInt16Type, UInt32Type, UInt64Type};
use arrow_array::{Array, ArrayRef, FixedSizeBinaryArray};

use crate::block::{self, Src};
use crate::error::Result;
use crate::json::Json;
use crate::query::{self, Search};
use crate::signal::Open;

/// A bounded region of telemetry.
///
/// No `spans` member, unlike section 7.3's sketch: the one question it was for — "this
/// span and its children" — is what a `trace_id` query already answers, and a
/// set nothing reads is a set nothing keeps correct.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Frame {
    pub from: i64,
    pub to: i64,
    /// `resources.key` values (section 7.2). Never contains
    /// [`crate::identity::NO_IDENTITY`]: treating "no stable identity" as an
    /// identity would merge every resource an exporter failed to describe into
    /// one entity.
    pub entities: Vec<u64>,
    pub traces: Vec<[u8; 16]>,
    /// Something was dropped on the way here. Carried on the frame rather than
    /// returned beside it so it survives a walk: three expansions later the
    /// caller still knows the answer is a sample, which is the difference
    /// between a wide investigation and a wrong one.
    pub truncated: bool,
}

/// How wide a frame is allowed to get.
///
/// A frame with half a million traces in it is not a frame, it is a scan with
/// extra steps. These are deliberately small: an investigation narrows.
pub const MAX_TRACES: usize = 1000;
pub const MAX_ENTITIES: usize = 256;

impl Frame {
    fn add_trace(&mut self, id: &[u8]) {
        let Ok(id) = <[u8; 16]>::try_from(id) else {
            return;
        };
        if self.traces.contains(&id) {
            return;
        }
        if self.traces.len() >= MAX_TRACES {
            self.truncated = true;
            return;
        }
        self.traces.push(id);
    }

    fn add_entity(&mut self, key: u64) {
        // An entity set containing the sentinel means "every resource nobody
        // described", which is not an entity.
        if key == crate::identity::NO_IDENTITY || self.entities.contains(&key) {
            return;
        }
        if self.entities.len() >= MAX_ENTITIES {
            self.truncated = true;
            return;
        }
        self.entities.push(key);
    }

    /// The frame as a response body. `names` labels the entity keys — see
    /// [`names_of`]; an empty map renders every one as `unknown`, which is what
    /// a caller that did not ask for labels gets.
    pub fn write_json(&self, j: &mut Json, names: &HashMap<u64, String>) {
        j.obj(|j| {
            j.key("from");
            j.i64_str(self.from);
            j.key("to");
            j.i64_str(self.to);
            j.key("entities");
            j.arr(|j| {
                for e in &self.entities {
                    j.obj(|j| {
                        j.key("key");
                        j.u64_str(*e);
                        j.key("name");
                        j.str(names.get(e).map_or(UNKNOWN, String::as_str));
                    });
                }
            });
            j.key("traces");
            j.arr(|j| {
                for t in &self.traces {
                    j.hex(t);
                }
            });
            j.key("truncated");
            j.bool(self.truncated);
        });
    }
}

/// One step of a correlation walk.
///
/// Three, not section 7.3's seven. `by_trace` and `by_span` are what a `trace_id`
/// query already does, `by_link` and `by_exemplar` are edges the row itself
/// carries out to the caller, and an expander with no caller is an expander
/// with no test.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Expand {
    /// Widen the window to the full extent of the traces already in the frame.
    ///
    /// A trace id says nothing about when, so a window taken from the row that
    /// matched usually cuts the trace in half — the log line that started the
    /// investigation is at the end of a request whose first span is 800 ms
    /// earlier. This is the fix, and it is why `Around` is not a substitute:
    /// the extent is measured, not guessed at.
    Traces,
    /// Widen `entities` to everything that took part in the frame's traces.
    ///
    /// The service map, for one investigation rather than the whole store:
    /// *which other services were involved in the traces this one took part
    /// in*. Two hops over data already in the block, with no service map to
    /// maintain and no metrics-generator sidecar. Requires traces — with none,
    /// "everything that shared a trace with nothing" is every entity there is,
    /// and returning that would look like an answer.
    Peers,
    /// Widen the window by ±d nanoseconds, keeping everything else.
    Around(i64),
}

/// Where an investigation starts: the frame around what a search matched.
///
/// The predicate is the one [`crate::query::search_open`] runs, evaluated by the
/// same code — this harvests ids where a search renders rows. That matters for
/// a reason beyond saving a scan loop: "the frame around what I am looking at"
/// is only true if *what I am looking at* is decided identically, and two
/// predicate evaluators would drift.
///
/// `q.limit` and `q.after` are ignored. A frame is bounded by [`MAX_TRACES`] and
/// [`MAX_ENTITIES`]; a page size bounds what is *rendered*, which is a different
/// question and a different call.
pub fn anchor(root: &Path, q: &Search, open: &[Arc<Open>]) -> Result<(Frame, Stats)> {
    let disk = block::scan(root, q.signal.dir())?;
    let mut refs = block::sources(&disk, open);
    let mut st = Stats {
        blocks_total: refs.len(),
        ..Default::default()
    };
    refs.retain(|b| b.overlaps(q.from, q.to));
    // Newest first, so a store far larger than the caps is sampled from the end
    // someone is looking at rather than from wherever `readdir` started.
    refs.sort_by_key(|b| std::cmp::Reverse((b.max_ts, b.seq)));

    let mut f = Frame {
        from: q.from,
        to: q.to,
        ..Default::default()
    };
    let scan = query::Scan::new(q);
    let mut i = 0;
    // Full width from the first wave, unlike a search: there is no `limit` to
    // exit on, so there is no cheap case for `search_open`'s ramp to protect.
    // The wave takes only threads that are spare and answers short when there
    // are none, so this stays one line rather than becoming a mode.
    //
    // Stopping on `truncated` is the other bound, and the honest one: once a
    // cap has dropped something, reading further blocks only drops more.
    while i < refs.len() && !f.truncated {
        let answers = scan.wave(&refs, i, refs.len());
        i += answers.len();
        for done in answers {
            let (Some(b), hits) = done? else { continue };
            st.blocks_scanned += 1;
            st.rows_scanned += b.root.num_rows();
            st.rows_matched += hits.len();
            // Every hit in this answer came from this block, so one lookup of
            // the resource table covers all of them.
            let Some(h0) = hits.first() else { continue };
            let keys = entity_keys(&refs[h0.block])?;
            let traces = b.root.column_by_name("trace_id").and_then(binary);
            let rids = b.root.column_by_name("resource_id");
            for h in &hits {
                let row = h.row as usize;
                if let Some(t) = traces.filter(|t| t.is_valid(row)) {
                    f.add_trace(t.value(row));
                }
                if let Some(&k) =
                    rids.and_then(|c| keys.get(c.as_primitive::<UInt16Type>().value(row) as usize))
                {
                    f.add_entity(k);
                }
            }
        }
    }
    Ok((f, st))
}

/// Apply a walk to a frame, in order.
///
/// In order and not as a set, because they do not commute: `Around` after
/// `Traces` widens the measured extent of the traces, `Around` before it is
/// overwritten by the measurement.
pub fn expand(
    root: &Path,
    f: &Frame,
    ops: &[Expand],
    open: &[Arc<Open>],
) -> Result<(Frame, Stats)> {
    let mut f = f.clone();
    let mut st = Stats::default();
    for op in ops {
        match *op {
            Expand::Around(d) => {
                f.from = f.from.saturating_sub(d);
                f.to = f.to.saturating_add(d);
            }
            op => walk_spans(root, &mut f, op, open, &mut st)?,
        }
    }
    Ok((f, st))
}

/// The one pass both span-side expanders need: every span of the frame's
/// traces, with its start, its duration and its resource.
///
/// One function for two operations because they differ in three lines and share
/// the expensive part — opening trace blocks and matching 16-byte ids. A
/// `[Traces, Peers]` walk still reads them twice; that is one extra pass per
/// call, not per row, and splitting the shared shape to save it would cost more
/// than it returns.
fn walk_spans(
    root: &Path,
    f: &mut Frame,
    op: Expand,
    open: &[Arc<Open>],
    st: &mut Stats,
) -> Result<()> {
    if f.traces.is_empty() {
        return Ok(());
    }
    let disk = block::scan(root, "traces")?;
    let refs = block::sources(&disk, open);
    st.blocks_total += refs.len();
    let (mut lo, mut hi) = (i64::MAX, i64::MIN);
    for bref in &refs {
        // `Traces` deliberately does not filter on the window: a trace id says
        // nothing about when, which is the whole reason the expander exists.
        // The trace sidecar is what keeps that affordable — section 7.4 measures it
        // pruning most of the store on a miss.
        if op == Expand::Peers && !bref.overlaps(f.from, f.to) {
            continue;
        }
        if !may_hold(bref, &f.traces) {
            continue;
        }
        let Some(spans) = bref.load("spans")? else {
            continue;
        };
        let Some(ids) = spans.column_by_name("trace_id").and_then(binary) else {
            continue;
        };
        st.blocks_scanned += 1;
        st.rows_scanned += spans.num_rows();
        let keys = entity_keys(bref)?;
        let rids = spans.column_by_name("resource_id");
        let start = spans
            .column_by_name("start_time_unix_nano")
            .map(|c| c.as_primitive::<TimestampNanosecondType>());
        let dur = spans
            .column_by_name("duration_nano")
            .map(|c| c.as_primitive::<UInt64Type>());
        for row in 0..spans.num_rows() {
            if !ids.is_valid(row) || !f.traces.iter().any(|t| t[..] == *ids.value(row)) {
                continue;
            }
            st.rows_matched += 1;
            if op == Expand::Peers {
                if let Some(&k) =
                    rids.and_then(|c| keys.get(c.as_primitive::<UInt16Type>().value(row) as usize))
                {
                    f.add_entity(k);
                }
            } else {
                let s = start.map_or(0, |c| c.value(row));
                lo = lo.min(s);
                hi = hi.max(s.saturating_add(dur.map_or(0, |c| c.value(row)) as i64));
            }
        }
    }
    if op == Expand::Traces && lo <= hi {
        f.from = f.from.min(lo);
        f.to = f.to.max(hi);
    }
    Ok(())
}

/// A service map over a window: who calls whom, how often, and how badly.
///
/// The edge is the one a service map has always been — a span's resource to its
/// parent span's resource — and the join is `parent_span_id -> span_id`. What
/// makes it affordable here is that it is a *read*: Tempo answers the same
/// question with a metrics-generator writing into a separate Prometheus, which
/// is a second write path, a second store and a second thing to operate.
///
/// Bounded by `max_spans`, newest block first. A map is a shape, not a census:
/// five services do not become six because the window held twenty million spans
/// instead of one, and a page that waits two seconds for the same picture is a
/// page nobody leaves open. What was covered is reported in `stats`.
pub fn map(
    root: &Path,
    from: i64,
    to: i64,
    max_spans: usize,
    open: &[Arc<Open>],
) -> Result<query::Results> {
    let disk = block::scan(root, "traces")?;
    let mut refs = block::sources(&disk, open);
    let mut stats = query::Stats {
        blocks_total: refs.len(),
        ..Default::default()
    };
    refs.retain(|b| b.overlaps(from, to));
    refs.sort_by_key(|b| std::cmp::Reverse((b.max_ts, b.seq)));

    let mut nodes: HashMap<u64, Node> = HashMap::new();
    let mut edges: HashMap<(u64, u64), Edge> = HashMap::new();
    let mut names: HashMap<u64, String> = HashMap::new();
    // Spans whose parent was not in the sample. Reported rather than hidden:
    // "40% unresolved" is how a reader knows to widen `max_spans` before
    // believing a thin edge is a thin dependency.
    let mut unresolved = 0u64;

    for bref in &refs {
        if stats.rows_scanned >= max_spans {
            break;
        }
        let Some(spans) = bref.load("spans")? else {
            continue;
        };
        let (Some(ids), Some(parents), Some(rids)) = (
            spans.column_by_name("span_id").and_then(binary),
            spans.column_by_name("parent_span_id").and_then(binary),
            spans.column_by_name("resource_id"),
        ) else {
            continue;
        };
        stats.blocks_scanned += 1;
        stats.rows_scanned += spans.num_rows();
        let keys = entity_keys(bref)?;
        resource_names(bref, &keys, &mut names)?;
        let rids = rids.as_primitive::<UInt16Type>();
        let time = spans
            .column_by_name("start_time_unix_nano")
            .map(|c| c.as_primitive::<TimestampNanosecondType>());
        let dur = spans
            .column_by_name("duration_nano")
            .map(|c| c.as_primitive::<UInt64Type>());
        let status = spans
            .column_by_name("status_code")
            .map(|c| c.as_primitive::<UInt8Type>());
        let key_of = |row: usize| keys.get(rids.value(row) as usize).copied().unwrap_or(0);

        // Who owns each span id, and which rows are in the window. Built first
        // because the join below needs the *parent's* resource, and the parent
        // is an arbitrary row of this same table — one pass to index, one to
        // walk.
        //
        // Every span goes into `owner`, including ones outside the window: a
        // child inside it whose parent started just before would otherwise
        // count as unresolved, which is exactly the edge a window boundary
        // cuts.
        let mut owner: HashMap<&[u8], u64> = HashMap::with_capacity(spans.num_rows());
        let mut live: Vec<usize> = Vec::with_capacity(spans.num_rows());
        for row in 0..spans.num_rows() {
            if ids.is_valid(row) {
                owner.insert(ids.value(row), key_of(row));
            }
            if time.is_none_or(|t| t.value(row) >= from && t.value(row) <= to) {
                live.push(row);
            }
        }

        for row in live {
            let key = key_of(row);
            let bad = status.is_some_and(|s| s.value(row) == STATUS_ERROR);
            let d = dur.map_or(0, |c| c.value(row));
            let n = nodes.entry(key).or_default();
            n.spans += 1;
            n.errors += u64::from(bad);
            n.nanos += d;

            // A span with no parent is an entry point, not a missing edge.
            let caller = if parents.is_valid(row) {
                owner.get(parents.value(row)).copied()
            } else {
                Some(ENTRY)
            };
            let Some(caller) = caller else {
                unresolved += 1;
                continue;
            };
            // A span whose parent is in the same service is internal work, not
            // a call. Keeping those would make every node a self-loop weighted
            // by its own span count, which is what `nodes` already says.
            if caller == key {
                continue;
            }
            let e = edges.entry((caller, key)).or_default();
            e.calls += 1;
            e.errors += u64::from(bad);
            e.nanos += d;
            e.max_nanos = e.max_nanos.max(d);
        }
    }
    stats.rows_matched = edges.len();

    // Sorted, so two identical requests produce identical bytes. A diff, an
    // ETag and a graph that does not reshuffle its nodes on every poll all
    // depend on that, and `HashMap` iteration order provides none of it.
    let mut ns: Vec<_> = nodes.into_iter().collect();
    ns.sort_unstable_by_key(|(k, _)| *k);
    let mut es: Vec<_> = edges.into_iter().collect();
    es.sort_unstable_by_key(|(k, _)| *k);

    let mut j = Json::new();
    j.obj(|j| {
        j.key("nodes");
        j.arr(|j| {
            for (key, n) in &ns {
                j.obj(|j| {
                    j.key("key");
                    j.u64_str(*key);
                    j.key("name");
                    j.str(names.get(key).map_or(UNKNOWN, String::as_str));
                    j.key("spans");
                    j.u64(n.spans);
                    j.key("errors");
                    j.u64(n.errors);
                    j.key("avg_nano");
                    j.u64_str(n.nanos / n.spans.max(1));
                });
            }
        });
        j.key("edges");
        j.arr(|j| {
            for ((a, b), e) in &es {
                j.obj(|j| {
                    j.key("from");
                    // The synthetic caller every root span hangs off. Named
                    // rather than omitted: a map without its entry points does
                    // not say where the traffic arrives.
                    if *a == ENTRY {
                        j.str("entry");
                    } else {
                        j.u64_str(*a);
                    }
                    j.key("to");
                    j.u64_str(*b);
                    j.key("calls");
                    j.u64(e.calls);
                    j.key("errors");
                    j.u64(e.errors);
                    j.key("avg_nano");
                    j.u64_str(e.nanos / e.calls.max(1));
                    j.key("max_nano");
                    j.u64_str(e.max_nanos);
                });
            }
        });
        j.key("unresolved");
        j.u64(unresolved);
    });
    Ok(query::Results {
        json: j.into_string(),
        stats,
        next: None,
    })
}

/// Every entity present in a window, with its name and how many blocks hold it.
///
/// What a "filter by service" control is populated from, and the one place
/// section 7.2's `resources.key` becomes something a human can pick. Reads only the
/// `resources` and `resource_attrs` tables — tens of rows a block — so it is a
/// facet lookup rather than a scan, and it covers all three signals because a
/// service that only emits metrics still belongs in the list.
pub fn entities(
    root: &Path,
    from: i64,
    to: i64,
    open: &[Vec<Arc<Open>>],
) -> Result<query::Results> {
    let mut names: HashMap<u64, String> = HashMap::new();
    let mut seen: HashMap<u64, u64> = HashMap::new();
    let mut stats = query::Stats::default();
    for (i, signal) in SIGNALS.iter().enumerate() {
        let disk = block::scan(root, signal)?;
        let refs = block::sources(&disk, open_for(open, i));
        stats.blocks_total += refs.len();
        for bref in &refs {
            if !bref.overlaps(from, to) {
                continue;
            }
            stats.blocks_scanned += 1;
            let keys = entity_keys(bref)?;
            resource_names(bref, &keys, &mut names)?;
            for k in keys.iter().filter(|&&k| k != 0) {
                *seen.entry(*k).or_default() += 1;
            }
        }
    }
    let mut out: Vec<_> = seen.into_iter().collect();
    // By name, then key: a picker is read by a human, and the key is a hash.
    out.sort_unstable_by(|a, b| {
        let name = |k: &u64| names.get(k).map_or(UNKNOWN, String::as_str);
        name(&a.0).cmp(name(&b.0)).then(a.0.cmp(&b.0))
    });
    stats.rows_matched = out.len();
    let mut j = Json::new();
    j.arr(|j| {
        for (key, blocks) in &out {
            j.obj(|j| {
                j.key("key");
                j.u64_str(*key);
                j.key("name");
                j.str(names.get(key).map_or(UNKNOWN, String::as_str));
                j.key("blocks");
                j.u64(*blocks);
            });
        }
    });
    Ok(query::Results {
        json: j.into_string(),
        stats,
        next: None,
    })
}

/// `service.name` for the entities of a frame.
///
/// A frame carries keys, and a key is a hash. This is what turns one into a
/// label for the response — the same walk [`entities`] does, without the
/// counting, and skipping any block that holds none of them.
pub fn names_of(root: &Path, f: &Frame, open: &[Vec<Arc<Open>>]) -> Result<HashMap<u64, String>> {
    let mut names = HashMap::new();
    if f.entities.is_empty() {
        return Ok(names);
    }
    for (i, signal) in SIGNALS.iter().enumerate() {
        let disk = block::scan(root, signal)?;
        for bref in block::sources(&disk, open_for(open, i)) {
            if !bref.overlaps(f.from, f.to) {
                continue;
            }
            let keys = entity_keys(&bref)?;
            if keys.iter().any(|k| f.entities.contains(k)) {
                resource_names(&bref, &keys, &mut names)?;
            }
        }
    }
    names.retain(|k, _| f.entities.contains(k));
    Ok(names)
}

/// What a frame walk cost. The same four numbers a search reports, for the same
/// reason: "how much did that cost" is the first question when it is slow, and
/// the second is what an agent uses to decide its filter was too broad.
#[derive(Debug, Default, Clone)]
pub struct Stats {
    pub blocks_total: usize,
    pub blocks_scanned: usize,
    pub rows_scanned: usize,
    pub rows_matched: usize,
}

/// The block directories, which are also the signal names on the wire.
///
/// The two functions that walk all three take their open blocks as a slice
/// parallel to this, because an `Open` does not record which signal produced it
/// and sequence numbers are per-signal — hand the logs snapshot to the traces
/// directory and a `(node, seq)` collision silently swaps one block for
/// another. A short or empty slice means "no open blocks for the rest", which
/// is what a test and a cold process both want.
const SIGNALS: [&str; 3] = ["logs", "traces", "metrics"];

fn open_for(open: &[Vec<Arc<Open>>], i: usize) -> &[Arc<Open>] {
    open.get(i).map_or(&[], Vec::as_slice)
}

/// OTLP `STATUS_CODE_ERROR`.
const STATUS_ERROR: u8 = 2;

/// A resource with no `service.name` — which is a resource no SDK described.
const UNKNOWN: &str = "unknown";

/// The caller of a root span.
///
/// A synthetic key in the same space as the real ones, which is a collision at
/// 2^-64 per distinct resource and not worth a tagged enum on every edge. The
/// obvious free value is [`crate::identity::NO_IDENTITY`], and it is taken:
/// zero already means *undescribed resource*, and merging "traffic from
/// outside" into "resources nobody labelled" is the one confusion a service map
/// must not have.
const ENTRY: u64 = u64::MAX;

#[derive(Default)]
struct Node {
    spans: u64,
    errors: u64,
    nanos: u64,
}

#[derive(Default)]
struct Edge {
    calls: u64,
    errors: u64,
    nanos: u64,
    max_nanos: u64,
}

/// `resources.key` indexed by `resource_id`, for one block.
///
/// A `Vec` and not a map: `resource_id` is a dense `u16` dictionary index, so
/// the id *is* the slot. Tens of entries — one page, no hashing.
fn entity_keys(bref: &Src<'_>) -> Result<Vec<u64>> {
    let Some(r) = bref.load("resources")? else {
        return Ok(Vec::new());
    };
    let (Some(ids), Some(keys)) = (r.column_by_name("id"), r.column_by_name("key")) else {
        return Ok(Vec::new());
    };
    let ids = ids.as_primitive::<UInt16Type>();
    let keys = keys.as_primitive::<UInt64Type>();
    // Ids are dense from zero, so the slot count is the row count — but the
    // rows need not arrive in id order, so this places rather than pushes.
    let mut out = vec![0u64; r.num_rows()];
    for row in 0..r.num_rows() {
        let i = ids.value(row) as usize;
        if i < out.len() {
            out[i] = keys.value(row);
        }
    }
    Ok(out)
}

/// `service.name` per entity key, accumulated across blocks.
///
/// Every OTel SDK sets `service.name` and section 7.2's identity ladder is built on
/// it, so a key almost always has one. `or_insert` and not `insert`: the first
/// block wins, and callers walk newest first, so a renamed service shows the
/// name it has now rather than the one it booted with.
fn resource_names(bref: &Src<'_>, keys: &[u64], out: &mut HashMap<u64, String>) -> Result<()> {
    let Some(a) = bref.load("resource_attrs")? else {
        return Ok(());
    };
    let (Some(parents), Some(vals)) = (a.column_by_name("parent_id"), a.column_by_name("str"))
    else {
        return Ok(());
    };
    let parents = parents.as_primitive::<UInt32Type>();
    let vals = crate::attrs::str_values(vals);
    for row in 0..a.num_rows() {
        if query::attr_key(&a, row) != "service.name" || !vals.is_valid(row) {
            continue;
        }
        if let Some(&k) = keys.get(parents.value(row) as usize).filter(|&&k| k != 0) {
            out.entry(k).or_insert_with(|| vals.value(row).to_owned());
        }
    }
    Ok(())
}

/// Could this block hold any of these traces? Fails open, like every sidecar:
/// a snapshot has no directory and a damaged filter reads as "scan me".
fn may_hold(bref: &Src<'_>, traces: &[[u8; 16]]) -> bool {
    let Some(dir) = bref.dir else { return true };
    match std::fs::read(dir.join(crate::bloom::TRACE_IDX)) {
        Ok(f) => traces.iter().any(|t| crate::bloom::may_contain(&f, t)),
        Err(_) => true,
    }
}

fn binary(c: &ArrayRef) -> Option<&FixedSizeBinaryArray> {
    c.as_any().downcast_ref::<FixedSizeBinaryArray>()
}

#[cfg(test)]
mod tests {
    use super::*;
    use mira_proto::collector::trace::v1::ExportTraceServiceRequest;
    use mira_proto::common::v1::any_value::Value as AnyVal;
    use mira_proto::common::v1::{AnyValue, KeyValue};
    use mira_proto::resource::v1::Resource;
    use mira_proto::trace::v1::{ResourceSpans, ScopeSpans, Span};

    /// Spans start here. The damaged blocks below sit whole hours away, so a
    /// guard that fails to skip one moves the measured extent by an hour rather
    /// than by a nanosecond — visible in an assertion instead of plausible.
    const T0: u64 = 1_000_000_000;
    const HOUR: u64 = 3_600_000_000_000;

    fn kv(k: &str, v: &str) -> KeyValue {
        KeyValue {
            key: k.into(),
            value: Some(AnyValue {
                value: Some(AnyVal::StringValue(v.into())),
            }),
        }
    }

    fn span(trace: u8, id: [u8; 8], parent: Option<[u8; 8]>, start: u64, dur: u64) -> Span {
        Span {
            trace_id: [trace; 16].to_vec().into(),
            span_id: id.to_vec().into(),
            parent_span_id: parent.map(|p| p.to_vec()).unwrap_or_default().into(),
            name: "GET /checkout".into(),
            start_time_unix_nano: start,
            end_time_unix_nano: start + dur,
            ..Default::default()
        }
    }

    /// One traces block per call, and its directory back so a test can damage
    /// it the way retention or an older writer would.
    fn traces_block(root: &Path, seq: u64, services: Vec<(&str, Vec<Span>)>) -> std::path::PathBuf {
        let mut b = crate::traces::TracesBuilder::new();
        b.append_request(&ExportTraceServiceRequest {
            resource_spans: services
                .into_iter()
                .map(|(name, spans)| ResourceSpans {
                    resource: Some(Resource {
                        attributes: vec![kv("service.name", name)],
                        ..Default::default()
                    }),
                    scope_spans: vec![ScopeSpans {
                        spans,
                        ..Default::default()
                    }],
                    ..Default::default()
                })
                .collect(),
        })
        .unwrap();
        let sealed = b.finish().unwrap();
        block::publish(root, "traces", block::node_id("a"), seq, 0, &sealed)
            .unwrap()
            .dir
    }

    /// Rewrite `table` without `col`, the shape a writer that predates the
    /// column left behind. Staged and renamed rather than truncated in place,
    /// because a mapping over a file being shortened is a SIGBUS and not an
    /// error anything can catch.
    fn drop_column(dir: &Path, table: &str, col: &str) {
        let path = dir.join(format!("{table}.arrow"));
        let mut b = block::open_table_opt(&path).unwrap().unwrap().batches[0].clone();
        b.remove_column(b.schema().index_of(col).unwrap());
        let staged = dir.join(format!("{table}.staged"));
        block::write_table(&staged, &b).unwrap();
        std::fs::rename(&staged, &path).unwrap();
    }

    /// Three services, one trace, and three blocks that are each broken in a
    /// different way — the states a running store actually reaches.
    ///
    /// There is no coordinator and no schema version to check against: the
    /// block directory is the manifest, so a reader meets blocks being unlinked
    /// under it by retention and blocks written by another build of the binary.
    /// Every one of those has to contribute nothing and be *visibly* nothing,
    /// because the alternative is a service map missing an edge, or a frame
    /// whose window silently grew by an hour, with a 200 on both.
    #[test]
    fn a_block_the_reader_cannot_use_contributes_nothing_and_widens_nothing() {
        use crate::query::{Search, Signal};

        let root = std::env::temp_dir().join(format!("mira-frame-torn-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);

        // gateway -> api -> db, one trace, the only block that is intact.
        let sid = |svc: u8| [svc, 1, 0, 0, 0, 0, 0, 0];
        let chain = |start: u64| {
            vec![
                ("gateway", vec![span(1, sid(1), None, start, 900)]),
                ("api", vec![span(1, sid(2), Some(sid(1)), start, 500)]),
                ("db", vec![span(1, sid(3), Some(sid(2)), start, 100)]),
            ]
        };
        traces_block(&root, 0, chain(T0 + 1));

        // A block holding a different trace entirely, an hour later. The trace
        // sidecar answers "no" for it, which is the prune section 7.4 measures — and
        // its `resources` table has lost the column that carries the identity,
        // so nothing in it can be named either.
        let ghost = traces_block(
            &root,
            1,
            vec![(
                "ghost",
                // Its parent is a span id nothing in this block owns: the shape
                // a window boundary or a dropped block leaves behind.
                vec![span(
                    9,
                    [9, 1, 0, 0, 0, 0, 0, 0],
                    Some([8, 8, 0, 0, 0, 0, 0, 0]),
                    T0 + HOUR,
                    10,
                )],
            )],
        );
        drop_column(&ghost, "resources", "key");
        drop_column(&ghost, "resource_attrs", "str");

        // Retention got here mid-query: the span table is gone, and so is the
        // resource table, but the directory is still listed and the trace
        // sidecar still claims the trace.
        let unlinked = traces_block(&root, 2, chain(T0 + 2 * HOUR));
        std::fs::remove_file(unlinked.join("spans.arrow")).unwrap();
        std::fs::remove_file(unlinked.join("resources.arrow")).unwrap();

        // Another build's block: no trace sidecar at all, so the filter fails
        // open, and a span table with neither the id a frame joins on nor the
        // one a service map joins on.
        let foreign = traces_block(&root, 3, chain(T0 + 3 * HOUR));
        std::fs::remove_file(foreign.join(crate::bloom::TRACE_IDX)).unwrap();
        drop_column(&foreign, "spans", "trace_id");
        drop_column(&foreign, "spans", "parent_span_id");

        // The frame around the intact block, anchored on a window two
        // nanoseconds wide so that any widening at all is the expander's.
        let q = Search {
            signal: Signal::Traces,
            from: T0 as i64,
            to: (T0 + 2) as i64,
            terms: Vec::new(),
            limit: 10,
            after: None,
        };
        let (f, _) = anchor(&root, &q, &[]).unwrap();
        assert_eq!(f.traces, vec![[1u8; 16]], "one trace in the window");
        assert_eq!(f.entities.len(), 3, "gateway, api and db");

        // `Traces` reads every trace block regardless of the window — that is
        // the whole point of it — so all four are offered and three are
        // refused. The extent it measures is the intact block's alone; a guard
        // that let any of the others through would move `to` by hours.
        let (g, st) = expand(&root, &f, &[Expand::Traces], &[]).unwrap();
        assert_eq!(st.blocks_total, 4);
        assert_eq!(st.blocks_scanned, 1, "three unusable blocks were read");
        assert_eq!((g.from, g.to), (T0 as i64, (T0 + 901) as i64));
        assert_eq!(g.entities, f.entities, "`Traces` does not touch entities");

        // The entity facet over the whole store. The two blocks with no
        // readable `resources` table contribute no entities rather than an
        // entity called zero, and the intact pair report themselves twice —
        // same services, same identity, two blocks.
        let all = entities(&root, 0, i64::MAX, &[]).unwrap();
        assert_eq!(all.stats.blocks_scanned, 4);
        assert_eq!(all.json.matches(r#""name""#).count(), 3, "{}", all.json);
        assert_eq!(all.json.matches(r#""blocks":2"#).count(), 3, "{}", all.json);
        assert!(!all.json.contains("ghost"), "{}", all.json);
        for name in ["api", "db", "gateway"] {
            assert!(all.json.contains(&format!(r#""name":"{name}""#)), "{name}");
        }

        // Narrowed to the intact block's own window, the other three are never
        // opened: the directory name is the index.
        let near = entities(&root, T0 as i64, (T0 + 2_000) as i64, &[]).unwrap();
        assert_eq!(near.stats.blocks_scanned, 1);
        assert_eq!(
            near.json.matches(r#""blocks":1"#).count(),
            3,
            "{}",
            near.json
        );

        // Labels for the frame, over both windows. The damaged blocks add no
        // names and remove none, and no key ever comes back as `unknown`.
        let sorted = |f: &Frame| {
            let mut v: Vec<String> = names_of(&root, f, &[]).unwrap().into_values().collect();
            v.sort();
            v
        };
        assert_eq!(sorted(&f), ["api", "db", "gateway"]);
        let wide = Frame {
            from: 0,
            to: i64::MAX,
            ..f
        };
        assert_eq!(sorted(&wide), ["api", "db", "gateway"]);

        // The service map, reconstructed from `parent_span_id` alone. Only two
        // blocks have a usable span table, and only one of those has parents.
        let m = map(&root, 0, i64::MAX, 1_000, &[]).unwrap();
        assert_eq!(m.stats.blocks_scanned, 2, "{}", m.json);
        assert_eq!(m.stats.rows_matched, 3, "entry->gateway->api->db");
        assert!(m.json.contains(r#""from":"entry""#), "{}", m.json);
        // The orphan's parent is owned by nobody, and that is reported rather
        // than dropped: a thin edge and a missing parent look identical on a
        // graph, so the count is how a reader tells them apart.
        assert!(m.json.contains(r#""unresolved":1"#), "{}", m.json);
        // Its resource cannot be identified, so it is one `unknown` node — not
        // a fabricated key and not a panic.
        assert_eq!(
            m.json.matches(r#""name":"unknown""#).count(),
            1,
            "{}",
            m.json
        );

        // `max_spans` stops the walk. A map is a shape, not a census: the two
        // unusable blocks cost nothing to refuse, and the first block that does
        // scan exhausts the budget before the intact one is reached.
        let one = map(&root, 0, i64::MAX, 1, &[]).unwrap();
        assert_eq!(one.stats.blocks_scanned, 1, "{}", one.json);
        assert!(!one.json.contains(r#""from":"entry""#), "{}", one.json);

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn the_caps_are_what_makes_a_frame_a_frame() {
        let mut f = Frame::default();
        for i in 0..MAX_TRACES as u32 + 5 {
            f.add_trace(&i.to_be_bytes().repeat(4));
        }
        assert_eq!(f.traces.len(), MAX_TRACES);
        assert!(f.truncated, "a dropped trace has to be visible");
        // A short id is not a trace id, and is not silently zero-extended.
        f.add_trace(&[1, 2, 3]);
        assert_eq!(f.traces.len(), MAX_TRACES);

        let mut f = Frame::default();
        for i in 0..MAX_ENTITIES as u64 + 5 {
            // `+ 1`, because zero is the sentinel and never lands in the set.
            f.add_entity(i + 1);
        }
        assert_eq!(f.entities.len(), MAX_ENTITIES);
        f.add_entity(0);
        assert!(!f.entities.contains(&0), "the sentinel is not an entity");
    }

    #[test]
    fn expansions_are_ordered_because_they_do_not_commute() {
        let root = std::env::temp_dir().join("mira-frame-empty");
        let _ = std::fs::remove_dir_all(&root);
        let f = Frame {
            from: 1_000,
            to: 2_000,
            ..Default::default()
        };
        let (g, _) = expand(&root, &f, &[Expand::Around(500), Expand::Around(500)], &[]).unwrap();
        assert_eq!((g.from, g.to), (0, 3_000));
        // No traces, so the span-side expanders are a no-op rather than a scan
        // of everything — the case that would otherwise return every entity in
        // the store and look like an answer.
        let (h, st) = expand(&root, &f, &[Expand::Traces, Expand::Peers], &[]).unwrap();
        assert_eq!(h, f);
        assert_eq!(st.blocks_scanned, 0);
    }
}
