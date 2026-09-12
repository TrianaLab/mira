//! The read path: prune blocks, scan columns, materialize JSON.
//!
//! There is no query planner and no expression tree here, and that is a
//! decision rather than an omission. Mira answers a small, known set of
//! questions — "the last N records matching these filters", "every span of this
//! trace", "this metric bucketed over time" — and each is a hand-written scan
//! over a layout designed for it. A general planner spends its budget
//! rediscovering at runtime what this module knows at compile time: which
//! column holds the timestamp, that `parent_id` is a row index, that block
//! directory names already carry the pruning key. That is most of what the 151
//! transitive crates of a general engine buy.
//!
//! Three properties the layout hands the scan, in the order they matter:
//!
//! * **Blocks prune by name.** `<min_ts>-<max_ts>-<node>-<seq>` is the whole
//!   index. A time-bounded query opens no file it will not read, and with
//!   blocks visited newest-first a `limit` stops the scan early.
//! * **`parent_id` is a row index.** Ids are rebased dense per block at ingest,
//!   so attaching an attribute to its record is an array store, not a hash
//!   join.
//! * **Dictionary columns compare as `u16`.** `severity_text = "ERROR"` resolves
//!   the string once against the dictionary, then scans a buffer of 16-bit
//!   codes and never touches string data again.
//!
//! Everything runs against mmap'd buffers, so a cold block costs demand paging
//! and a warm one costs memory bandwidth. Nothing allocates per row until
//! materialization, which happens after `limit` has cut the result to size.
//!
//! With no write-ahead log, nothing here needs to read the open, unsealed
//! block: ingest acknowledges an export only after the block containing it has
//! been fsynced and renamed into place, so read-your-writes falls out of the
//! durability rule. Turn the log on and the acknowledgement moves ahead of the
//! seal, which breaks that — so [`search_open`] takes the open block's snapshot
//! alongside the directory scan and merges the two into one ordered page. See
//! [`crate::signal::Open`]; the argument is ARCHITECTURE section 4.

use std::path::Path;
use std::sync::Arc;
// `Relaxed` alone rather than `Ordering`, which is `std::cmp::Ordering` in this
// file.
use std::sync::atomic::{AtomicUsize, Ordering::Relaxed};

use arrow_array::cast::AsArray;
use arrow_array::types::{
    Float64Type, Int32Type, Int64Type, TimestampNanosecondType, UInt8Type, UInt16Type, UInt32Type,
    UInt64Type,
};
use arrow_array::{Array, BooleanArray, RecordBatch, StringArray};
use arrow_schema::DataType;
use mira_proto::common::v1::AnyValue;
use prost::Message;

use crate::block::{self, Src};
use crate::error::Result;
use crate::json::Json;
use crate::schema::AttrType;
use crate::signal::Open;

/// Comparison operator. `Contains` is substring matching on strings and matches
/// nothing on any other type: a filter that cannot apply returns no rows rather
/// than an error, because a query spanning signals with slightly different
/// columns is a normal thing for an agent to try.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Op {
    Eq,
    Ne,
    Lt,
    Lte,
    Gt,
    Gte,
    Contains,
}

impl Op {
    pub fn parse(s: &str) -> Option<Op> {
        Some(match s {
            "eq" | "=" | "==" => Op::Eq,
            "ne" | "!=" => Op::Ne,
            "lt" | "<" => Op::Lt,
            "lte" | "<=" => Op::Lte,
            "gt" | ">" => Op::Gt,
            "gte" | ">=" => Op::Gte,
            "contains" | "~" => Op::Contains,
            _ => return None,
        })
    }

    fn test_ord(self, ord: std::cmp::Ordering) -> bool {
        use std::cmp::Ordering::*;
        match self {
            Op::Eq => ord == Equal,
            Op::Ne => ord != Equal,
            Op::Lt => ord == Less,
            Op::Lte => ord != Greater,
            Op::Gt => ord == Greater,
            Op::Gte => ord != Less,
            // Never reached: every caller handles Contains before comparing.
            Op::Contains => false,
        }
    }
}

/// A scalar from a query document.
///
/// Deliberately loose about numbers. Queries arrive as JSON from a browser or
/// an LLM, and both write `"200"` for `http.response.status_code` about as often
/// as they write `200`. Rejecting the quoted form would be defensible and
/// useless, so coercion happens at comparison time, once the column's real type
/// is known.
#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    Str(String),
    Int(i64),
    Double(f64),
    Bool(bool),
}

impl Value {
    fn as_i64(&self) -> Option<i64> {
        match self {
            Value::Int(i) => Some(*i),
            // A whole-valued float is the same number; a fractional one is not,
            // and truncating it would make `duration > 0.5` mean `> 0`.
            Value::Double(d) if d.fract() == 0.0 => Some(*d as i64),
            Value::Str(s) => s.parse().ok(),
            Value::Double(_) | Value::Bool(_) => None,
        }
    }

    fn as_f64(&self) -> Option<f64> {
        match self {
            Value::Int(i) => Some(*i as f64),
            Value::Double(d) => Some(*d),
            Value::Str(s) => s.parse().ok(),
            Value::Bool(_) => None,
        }
    }

    fn as_bool(&self) -> Option<bool> {
        match self {
            Value::Bool(b) => Some(*b),
            Value::Str(s) if s == "true" => Some(true),
            Value::Str(s) if s == "false" => Some(false),
            Value::Str(_) | Value::Int(_) | Value::Double(_) => None,
        }
    }

    fn as_str(&self) -> Option<&str> {
        match self {
            Value::Str(s) => Some(s),
            _ => None,
        }
    }
}

/// What a term filters on.
#[derive(Debug, Clone)]
pub enum Target {
    /// A column of the signal's root table, by name.
    Field(String),
    /// An attribute key, looked up at every level the signal has — record,
    /// resource, scope, and on traces the span's own events and links — and
    /// unioned.
    ///
    /// Whether `service.name` is a resource attribute is a detail of whoever
    /// configured the SDK, and users do not know it. Searching all of them is
    /// what every usable tracing UI does; the star schema makes it a handful of
    /// scans of tables that are tiny next to the root.
    ///
    /// The child levels are not a nicety. `Span.recordException` — the one API
    /// call behind most of the spans anyone goes looking for — writes
    /// `exception.type` to a span *event*, so leaving events out made the
    /// commonest question in a tracing UI return an empty list while the value
    /// was visible in `events[].attributes` of the very same response. A match
    /// on a child selects the span it hangs off, which is the row the caller
    /// asked for.
    Attr(String),
}

/// One conjunct. Terms are AND-ed. There is no OR in V0: a disjunction over
/// attributes is rare enough that supporting it means building a planner for a
/// query nobody has typed yet.
#[derive(Debug, Clone)]
pub struct Term {
    pub target: Target,
    pub op: Op,
    pub value: Value,
}

/// Which signal, and therefore which tables and which time column.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Signal {
    Logs,
    Traces,
}

impl Signal {
    pub fn parse(s: &str) -> Option<Signal> {
        match s {
            "logs" => Some(Signal::Logs),
            "traces" | "spans" => Some(Signal::Traces),
            _ => None,
        }
    }

    pub fn dir(self) -> &'static str {
        match self {
            Signal::Logs => "logs",
            Signal::Traces => "traces",
        }
    }

    /// The root table's file stem.
    fn root(self) -> &'static str {
        match self {
            Signal::Logs => "logs",
            Signal::Traces => "spans",
        }
    }

    /// The record-level attribute table.
    fn attrs(self) -> &'static str {
        match self {
            Signal::Logs => "log_attrs",
            Signal::Traces => "span_attrs",
        }
    }

    /// The column that orders results and bounds the query.
    fn time_col(self) -> &'static str {
        match self {
            Signal::Logs => "time_unix_nano",
            Signal::Traces => "start_time_unix_nano",
        }
    }
}

/// A record search: filter, order by time descending, take `limit`.
#[derive(Debug, Clone)]
pub struct Search {
    pub signal: Signal,
    /// Inclusive nanosecond bounds.
    pub from: i64,
    pub to: i64,
    pub terms: Vec<Term>,
    pub limit: usize,
    /// Start after this row. See [`Cursor`].
    pub after: Option<Cursor>,
}

/// Where the previous page stopped.
///
/// Keyset, not offset, and not for tidiness: `offset: 20000` forces the engine
/// to find and discard twenty thousand rows on every page, which turns the
/// early exit below into a full scan and makes the last page the most expensive
/// one. It is also *wrong* on a store that is still being written to — a batch
/// arriving between two pages shifts every row down and the reader sees a row
/// twice or never.
///
/// This is the sort key of the last row returned, so "the next page" is
/// "everything that sorts after this", which is exact whatever else has landed
/// meanwhile. `(node, seq)` identifies the block globally with no coordination
/// (see [`block::node_id`]) and `row` is its offset inside it, so the key is
/// intrinsic to the record rather than to the query that found it.
///
/// Rendered as `ts.node.seq.row`, in decimal, on purpose: an agent reading a
/// response can tell what it is holding, and a human debugging a stuck reader
/// can tell where it stopped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cursor {
    pub ts: i64,
    pub node: u32,
    pub seq: u64,
    pub row: u32,
}

impl Cursor {
    /// Descending: newest first, and for rows sharing a nanosecond, the
    /// higher-numbered block and row first. Any total order would do; what
    /// matters is that it is total, so no row can hide in a tie.
    fn key(&self) -> std::cmp::Reverse<(i64, u32, u64, u32)> {
        std::cmp::Reverse((self.ts, self.node, self.seq, self.row))
    }
}

impl std::fmt::Display for Cursor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}.{}.{}.{}", self.ts, self.node, self.seq, self.row)
    }
}

impl std::str::FromStr for Cursor {
    type Err = String;

    fn from_str(s: &str) -> std::result::Result<Cursor, String> {
        let bad = || format!("{s:?} is not a cursor; pass back the `next` field verbatim");
        let mut p = s.split('.');
        let mut next = |f: &dyn Fn(&str) -> bool| p.next().filter(|v| f(v)).ok_or_else(bad);
        // `ts` may be negative; nothing else may. Parsing the pieces by hand
        // rather than trusting `parse` to reject `+1` or `1_000`.
        let ts = next(&|v: &str| !v.is_empty())?.parse().map_err(|_| bad())?;
        let digits = |v: &str| !v.is_empty() && v.bytes().all(|b| b.is_ascii_digit());
        let c = Cursor {
            ts,
            node: next(&digits)?.parse().map_err(|_| bad())?,
            seq: next(&digits)?.parse().map_err(|_| bad())?,
            row: next(&digits)?.parse().map_err(|_| bad())?,
        };
        match p.next() {
            Some(_) => Err(bad()),
            None => Ok(c),
        }
    }
}

/// One matching record, kept only long enough to sort and materialize.
///
/// Holds a block index and a row number, not the row's data: a query over a
/// wide window can match millions of rows and keep a hundred, and copying the
/// other 999,900 out of the mapping in order to discard them is the easiest way
/// to make a columnar engine slow.
pub(crate) struct Hit {
    ts: i64,
    pub(crate) block: usize,
    pub(crate) row: u32,
}

/// What a search cost, reported alongside the rows.
///
/// Not decoration. "How much did that cost" is the first question when a query
/// is slow, and it is also what tells an agent its filter was too broad.
#[derive(Debug, Default, Clone)]
pub struct Stats {
    pub blocks_total: usize,
    pub blocks_scanned: usize,
    pub rows_scanned: usize,
    /// Rows satisfying the whole query — window, terms and `after` — not rows
    /// on this page. `limit` cuts the page; this says how much there was to
    /// cut, so a caller can tell "my filter is right and I am reading page one
    /// of forty" from "my filter is wrong".
    pub rows_matched: usize,
    /// Series the metrics cap refused (`series::bound`); always zero on a
    /// record search, which has no such cap.
    ///
    /// The counterpart to the per-series `dropped_points`, and here rather than
    /// in the response body because the response body is a JSON array with
    /// nowhere to hang a number that belongs to all of it. A chart with a
    /// series missing and no way to know it is the failure mode both of these
    /// exist to prevent.
    pub dropped_series: usize,
}

pub struct Results {
    /// A JSON array of row objects.
    pub json: String,
    pub stats: Stats,
    /// Pass back as `after` for the next page. `None` means this was the last
    /// one — not "ask again and see", which is the ambiguity that makes readers
    /// poll forever.
    pub next: Option<Cursor>,
}

/// The trace id this search pins down exactly, if it pins one down.
///
/// Only `trace_id = <32 hex chars>` qualifies, on either signal — logs blocks
/// carry the same filter as span blocks, so "the logs for this trace" prunes
/// exactly as hard as "the spans for it". Terms are AND-ed, so one such term is
/// enough no matter what else is in the list: a block that cannot hold the id
/// cannot hold a row satisfying the conjunction.
fn trace_needle(q: &Search) -> Option<[u8; 16]> {
    q.terms.iter().find_map(|t| match (&t.target, t.op) {
        (Target::Field(f), Op::Eq) if f == "trace_id" => unhex(t.value.as_str()?)?.try_into().ok(),
        _ => None,
    })
}

/// The attribute equalities in a search, in the form a block filter answers.
///
/// Terms are AND-ed, so any single one the block cannot satisfy rules the whole
/// block out — which is why a list of independent probes is enough and no
/// expression tree is needed.
///
/// `numeric` records that the query scalar could be read as a number. Doubles
/// are not in the index (see [`crate::bloom::HAS_DOUBLE`]), so a block that
/// holds any must be scanned for a numeric query even when the text probe misses.
struct AttrProbe {
    hash: (u64, u64),
    numeric: bool,
}

impl AttrProbe {
    /// The block's own answer to "could a row here satisfy this term?".
    fn maybe(&self, f: &crate::bloom::Filter) -> bool {
        f.may_contain(self.hash) || (self.numeric && f.flags & crate::bloom::HAS_DOUBLE != 0)
    }
}

fn attr_probes(q: &Search) -> Vec<AttrProbe> {
    q.terms
        .iter()
        .filter(|t| t.op == Op::Eq)
        .filter_map(|t| match &t.target {
            Target::Attr(key) => Some(AttrProbe {
                hash: crate::bloom::attr_hash(key, canon(&t.value).as_bytes()),
                numeric: t.value.as_f64().is_some(),
            }),
            Target::Field(_) => None,
        })
        .collect()
}

/// The comparisons in a search that a zone map can answer.
///
/// Only an *unquoted* number qualifies, and that is a rule about the scan
/// rather than about the index. A quoted scalar means a text comparison on a
/// `str`-typed attribute (see [`attr_matches`]) and a numeric one on an `int`
/// column, so the same term is lexicographic against one row and arithmetic
/// against the next — two orderings, and [`crate::zone`] describes one. Since
/// an absent key is read as "prune", a probe that describes the wrong ordering
/// would not merely mislead, it would drop rows. `Ne` and `Contains` are out
/// for the reason in [`crate::zone`]: they prune nothing worth the branch.
fn range_probes(q: &Search) -> Vec<crate::zone::Probe> {
    q.terms
        .iter()
        .filter(|t| !matches!(t.op, Op::Ne | Op::Contains))
        .filter(|t| matches!(t.value, Value::Int(_) | Value::Double(_)))
        .map(|t| crate::zone::Probe {
            key: match &t.target {
                Target::Attr(k) => crate::zone::attr_key(k),
                Target::Field(f) => crate::zone::field_key(f),
            },
            op: t.op,
            // The same two readings `attr_matches` and `field_pred` take of the
            // scalar, so the interval consulted is the one the row would be
            // compared against.
            int: t.value.as_i64(),
            float: t.value.as_f64(),
        })
        .collect()
}

/// The text an attribute of this value would have been indexed under.
///
/// This is the contract between [`attr_matches`] and the block index, and both
/// sides are written against it: `Op::Eq` on a stored string is *defined* as
/// equality with this text, and [`crate::attrs::index`] writes exactly these
/// bytes for the str, int and bool types. Doubles are not in the index at all —
/// the `numeric` flag covers them — so a stored double is reached through
/// `HAS_DOUBLE` rather than through this string.
///
/// Every value has a text form, including a fractional double: nothing forces a
/// producer to send `0.5` as a double rather than as `"0.5"`, and returning
/// `None` here to mean "unindexable" made every fractional-double equality scan
/// every block.
fn canon(v: &Value) -> String {
    match v {
        Value::Str(s) => s.clone(),
        Value::Int(i) => i.to_string(),
        Value::Bool(b) => b.to_string(),
        // `1.0` is written `1` by the int arm of the indexer and would be
        // written `1` by anything rendering the number for a human, so a whole
        // float has to canonicalise the same way or `eq: 1.0` misses `1`.
        Value::Double(d) if d.fract() == 0.0 => (*d as i64).to_string(),
        Value::Double(d) => d.to_string(),
    }
}

/// Run a search against the blocks under `root`.
///
/// Blocking: this mmaps and page-faults. Callers on an async runtime must go
/// through `spawn_blocking` — a hard fault stalls the whole OS thread with no
/// yield point and no signal to the scheduler.
pub fn search(root: &Path, q: &Search) -> Result<Results> {
    search_open(root, q, &[])
}

/// As [`search`], but also reads `open` — snapshots of blocks that have been
/// acknowledged and not yet published.
///
/// This is what keeps read-your-writes true once the write-ahead log moves the
/// acknowledgement ahead of the seal (ARCHITECTURE section 4): without it a client that
/// got a `200` and queried immediately would get nothing back for up to
/// `max_block_age`. See [`crate::signal::Open`] for why a cursor stays valid
/// across the seal.
pub fn search_open(root: &Path, q: &Search, open_blocks: &[Arc<Open>]) -> Result<Results> {
    let disk = block::scan(root, q.signal.dir())?;
    let mut refs = block::sources(&disk, open_blocks);
    let mut stats = Stats {
        blocks_total: refs.len(),
        ..Default::default()
    };
    refs.retain(|b| b.overlaps(q.from, q.to));
    // A block whose oldest row is newer than the cursor is entirely on a page
    // already delivered. Pruning here rather than per row is what keeps deep
    // paging as cheap as the first page.
    if let Some(c) = &q.after {
        refs.retain(|b| b.min_ts <= c.ts);
    }
    // Newest first, so `limit` can cut the scan short.
    refs.sort_by_key(|b| std::cmp::Reverse((b.max_ts, b.seq)));
    let refs = &refs[..];

    let scan = Scan::new(q);
    let mut hits: Vec<Hit> = Vec::new();
    let mut open: Vec<Option<Block>> = Vec::with_capacity(refs.len());

    // Waves, widening. The first is one block wide because the commonest query
    // there is — "the last `limit` records" — is answered by the first block and
    // exits, and reading eleven more in parallel to throw them away would make
    // the cheap case eleven times dearer to make the dear case faster. Doubling
    // reaches full width after four waves and fifteen blocks, which is noise
    // against the scan this is for.
    let mut width = 1;
    let mut i = 0;
    while i < refs.len() {
        // The early exit. With `limit` hits held, a block whose newest row
        // predates the oldest hit cannot contribute — and since blocks are
        // ordered by max_ts descending, neither can any block after it.
        //
        // Checked at the head of the wave rather than per block, which is what
        // parallelism costs: a wave that begins before the limit is reached runs
        // to the end even if its first block would have satisfied it. So
        // `blocks_scanned` is now "what the scan read", not "the fewest blocks
        // that could have answered" — the two were the same number when the loop
        // was serial and are within one wave of each other now.
        if hits.len() >= q.limit && hits.last().is_some_and(|w| refs[i].max_ts < w.ts) {
            break;
        }
        let asked = (i + width).min(refs.len());
        let answers = scan.wave(refs, i, asked);
        let end = i + answers.len();
        for done in answers {
            let (block, found) = done?;
            if let Some(b) = &block {
                stats.blocks_scanned += 1;
                stats.rows_scanned += b.root.num_rows();
            }
            // Counted *after* the cursor, which is what makes it a property of
            // the query rather than of the scan. The block retain above already
            // drops every block that lies wholly ahead of the cursor, so
            // counting before the cursor made `rows_matched` shrink from page to
            // page by whatever that pruning happened to remove — a number that
            // moves when an optimisation fires is reporting the optimisation,
            // not the query. Counting behind the cursor instead gives one
            // meaning that holds on every page: how many matching rows are still
            // to be read from here.
            //
            // It stays a lower bound in exactly one case — the early exit above,
            // which cannot fire until `limit` hits are held and therefore cannot
            // fire on a response whose `next` is `None`. So the count is exact
            // whenever the answer is complete, and short only when the reader has
            // already been told to page.
            stats.rows_matched += found.len();
            hits.extend(found);
            open.push(block);
        }

        // Trim between waves, so memory is bounded by `limit` times the wave
        // width rather than by the match count, which is not bounded by
        // anything.
        //
        // Partition before sorting. `limit` is a hundred and a broad filter over
        // one block is hundreds of thousands, so sorting the match set to throw
        // away all but its head is the dominant cost of the commonest query
        // there is — "the last 100 records", which matches every row it reads.
        // `select_nth_unstable` is linear and leaves the head in the first
        // `limit` slots; only those get ordered.
        if hits.len() > q.limit {
            hits.select_nth_unstable_by_key(q.limit, |h| cursor(&refs[h.block], h).key());
            hits.truncate(q.limit);
        }
        hits.sort_unstable_by_key(|h| cursor(&refs[h.block], h).key());
        i = end;
        width = (width * 2).min(MAX_FANOUT);
    }

    let mut j = Json::new();
    j.arr(|j| {
        for h in &hits {
            let b = open[h.block].as_ref().expect("a hit implies an open block");
            b.emit_row(j, h.row);
        }
    });
    Ok(Results {
        json: j.into_string(),
        stats,
        // A short page is the last page. Saying so costs nothing here and saves
        // every reader one round trip that returns nothing.
        next: (hits.len() == q.limit)
            .then(|| hits.last().map(|h| cursor(&refs[h.block], h)))
            .flatten(),
    })
}

/// The widest a single search will fan out across blocks.
///
/// Past this the win is not CPU. A block scan is mostly page faults, the device
/// answers them at its own rate, and every extra thread is one more spawn and
/// one more shootdown for the same IO.
const MAX_FANOUT: usize = 16;

/// Threads a search may borrow beyond its own, for the whole process.
///
/// Fan-out is only worth anything when there is a core going spare, and this is
/// the measurement that says so. One search over 37 blocks and 19M rows: 1.81 s
/// serial, 0.20 s fanned out. Eight concurrent searches on the same twelve
/// cores: throughput unchanged to within noise, and the short classes' p99 an
/// order of magnitude worse — the cores were already busy, so every extra
/// thread was scheduler work and nothing else.
///
/// So the budget is shared rather than per-search. A search takes what is idle
/// and runs serially when nothing is, which makes the two cases above the same
/// code with no mode to pick and nothing to configure. Bounded by the core
/// count less the caller's own thread, because that is what "idle" means here.
///
/// Deliberately *not* a fair queue: a search never waits for the budget, it
/// only asks. Waiting would trade the thing being optimised — latency — for a
/// share of a resource the caller is about to finish with anyway.
static SPARE: std::sync::LazyLock<AtomicUsize> = std::sync::LazyLock::new(|| {
    AtomicUsize::new(
        std::thread::available_parallelism()
            .map_or(1, |n| n.get())
            .saturating_sub(1)
            .min(MAX_FANOUT),
    )
});

/// Threads borrowed from [`SPARE`], returned on drop — including the drop that
/// unwinds a panicking scan. Leaking one would degrade the process to serial
/// scans permanently, with nothing to point at.
pub(crate) struct Helpers(usize);

impl Helpers {
    /// `pub(crate)` for one caller that is not a scan: a test claiming the
    /// whole budget, so it can run the same search with the fan-out off and
    /// with it on and demand the same bytes back.
    pub(crate) fn claim(want: usize) -> Helpers {
        let mut got = 0;
        let _ = SPARE.fetch_update(Relaxed, Relaxed, |n| {
            got = n.min(want);
            (got > 0).then(|| n - got)
        });
        Helpers(got)
    }
}

impl Drop for Helpers {
    fn drop(&mut self) {
        SPARE.fetch_add(self.0, Relaxed);
    }
}

/// Everything the per-block half of a search reads, and nothing it writes.
///
/// One struct rather than five arguments because it crosses a thread boundary:
/// shared by reference, immutable for the whole scan, and therefore `Sync`
/// without a lock. What is left out is as deliberate — `hits` and `stats` are
/// the only state a block scan produces, and both are returned rather than
/// accumulated, which is what makes the blocks independent.
pub(crate) struct Scan<'a> {
    q: &'a Search,
    needle: Option<[u8; 16]>,
    probes: Vec<AttrProbe>,
    ranges: Vec<crate::zone::Probe>,
    after: std::cmp::Reverse<(i64, u32, u64, u32)>,
}

impl<'a> Scan<'a> {
    /// The per-block half of a query, prepared once.
    ///
    /// Everything here is derived from `q` alone, which is what lets one `Scan`
    /// serve every block of a search and every thread scanning them.
    pub(crate) fn new(q: &'a Search) -> Scan<'a> {
        Scan {
            q,
            needle: trace_needle(q),
            probes: attr_probes(q),
            ranges: range_probes(q),
            // With no cursor, every row is after the start — which is the key
            // that sorts ahead of all of them.
            after: q.after.map_or(
                std::cmp::Reverse((i64::MAX, u32::MAX, u64::MAX, u32::MAX)),
                |c| c.key(),
            ),
        }
    }

    /// Blocks `[from, to)` scanned at once, answered in block order.
    ///
    /// `std::thread::scope` and not a pool: the threads borrow the mapping, the
    /// query and each other's nothing, and a scope is the one construct that
    /// lets them do that without an `Arc` around every field. They are also the
    /// wrong threads to pool — a pool exists to amortise spawns against short
    /// tasks, and a block scan is milliseconds against a spawn's microseconds.
    ///
    /// The caller's own thread takes the first block, so a wave with no helpers
    /// — the first wave of every search, and every wave on a busy node — spawns
    /// nothing at all.
    ///
    /// `to` is the wave the ramp asked for and the answer may be shorter: what
    /// [`SPARE`] hands out decides how many blocks this wave actually covers,
    /// and the caller advances by the number of answers rather than by the
    /// width it asked for.
    pub(crate) fn wave(&self, refs: &[Src<'_>], from: usize, to: usize) -> Vec<Result<Scanned>> {
        let helpers = Helpers::claim(to - from - 1);
        if helpers.0 == 0 {
            return vec![self.block(from, &refs[from])];
        }
        let to = from + 1 + helpers.0;
        std::thread::scope(|s| {
            let rest: Vec<_> = (from + 1..to)
                .map(|k| s.spawn(move || self.block(k, &refs[k])))
                .collect();
            let mut out = Vec::with_capacity(to - from);
            out.push(self.block(from, &refs[from]));
            // A panic in a block scan is a bug in this file, not a bad query.
            // Re-raising it on the caller's thread puts it where the runtime
            // will turn it into a 500 with the original message attached;
            // swallowing it would return a silently short answer instead.
            out.extend(
                rest.into_iter()
                    .map(|h| h.join().unwrap_or_else(|e| std::panic::resume_unwind(e))),
            );
            out
        })
    }

    /// One block: prune it, or open it and find the rows that match.
    pub(crate) fn block(&self, i: usize, bref: &Src<'_>) -> Result<Scanned> {
        // The sidecar filters (section 7.4). Both cover the case the block name cannot:
        // a query with no useful time bound, either because it names a trace or
        // because it names an attribute value that is rare or absent. Without
        // them the scan runs to the end of retention to prove a negative. A
        // 20 KB read is two orders of magnitude cheaper than the block it skips,
        // and every damaged or missing filter reads as "scan me".
        //
        // `is_some_and`/`is_ok_and` rather than a `let` chain: those are stable
        // since 1.70 and the chains are stable since 1.88, which is three years
        // past the MSRV this workspace declares.
        let no_trace = bref.dir.zip(self.needle.as_ref()).is_some_and(|(dir, id)| {
            std::fs::read(dir.join(crate::bloom::TRACE_IDX))
                .is_ok_and(|f| !crate::bloom::may_contain(&f, id))
        });
        if no_trace {
            return Ok((None, Vec::new()));
        }
        let no_attr = !self.probes.is_empty()
            && bref.dir.is_some_and(|dir| {
                std::fs::read(dir.join(crate::bloom::ATTR_IDX)).is_ok_and(|bytes| {
                    crate::bloom::Filter::open(&bytes)
                        .is_some_and(|f| !self.probes.iter().all(|p| p.maybe(&f)))
                })
            });
        if no_attr {
            return Ok((None, Vec::new()));
        }
        // And the orderings, which the Bloom filter cannot help with at all:
        // "slower than a second", "returned 5xx". Same shape, same fail-open —
        // a block published before the zone map existed has no file here and is
        // scanned, exactly as it was before.
        let out_of_range = !self.ranges.is_empty()
            && bref.dir.is_some_and(|dir| {
                std::fs::read(dir.join(crate::zone::ZONE_IDX)).is_ok_and(|bytes| {
                    crate::zone::Map::open(&bytes)
                        .is_some_and(|m| !self.ranges.iter().all(|p| p.maybe(&m)))
                })
            });
        if out_of_range {
            return Ok((None, Vec::new()));
        }

        let Some(b) = Block::open(bref, self.q.signal)? else {
            return Ok((None, Vec::new()));
        };
        let sel = b.select(self.q, bref);
        let hits = b.time().map_or_else(Vec::new, |time| {
            sel.iter()
                .filter_map(|&row| {
                    let h = Hit {
                        ts: time[row as usize],
                        block: i,
                        row,
                    };
                    (cursor(bref, &h).key() > self.after).then_some(h)
                })
                .collect()
        });
        Ok((Some(b), hits))
    }
}

/// What one block contributed: the mapping, if it was opened, and its matches.
///
/// The mapping comes back because the hits are row numbers into it and the
/// rendering pass at the end of the search needs it still open. Dropping it and
/// re-opening later would page the block in twice.
pub(crate) type Scanned = (Option<Block>, Vec<Hit>);

/// The sort key of one hit, which is also the cursor a caller pages on.
fn cursor(bref: &Src, h: &Hit) -> Cursor {
    Cursor {
        ts: h.ts,
        node: bref.node,
        seq: bref.seq,
        row: h.row,
    }
}

/// The tables of one block, opened and ready to scan.
///
/// No `Mmap` handle is kept: every Arrow buffer read out of a block owns an
/// `Arc<Mmap>` of its own (see `block::open_table`), so holding the
/// `RecordBatch` is what holds the mapping. That is also why row data can be
/// borrowed straight out of these batches with ordinary lifetimes.
pub(crate) struct Block {
    signal: Signal,
    pub(crate) root: RecordBatch,
    /// Record-level attributes; `parent_id` indexes `root` directly.
    attrs: Option<Attrs>,
    resource_attrs: Option<Attrs>,
    scope_attrs: Option<Attrs>,
    /// Rows that hang off a root row rather than being one: a span's events and
    /// its links. Empty for logs.
    children: Vec<Child>,
}

/// A child table and the attribute table keyed by its `id`.
///
/// Both indexes are built once when the block is opened, because both are read
/// once per emitted span and once per attribute term: rebuilding either per
/// span made emission quadratic in the number of spans in the block, which is
/// the one place a `limit` does not bound the work.
struct Child {
    /// What the array is called in the emitted row.
    label: &'static str,
    rows: RecordBatch,
    attrs: Option<Attrs>,
    /// Child rows grouped by the root row they hang off, indexed by it.
    by_parent: Vec<Vec<u32>>,
    /// The root row each child `id` belongs to, indexed by the id. Child ids
    /// are dense from zero per block, so this is an array store like every
    /// other join here — but it is not the identity, because `id` numbers a
    /// child and the row number is where it happens to sit.
    parent_of_id: Vec<u32>,
}

/// An attribute table, plus the one fact about it that makes rendering a row
/// cheap: whether `parent_id` ascends.
///
/// Every builder with the [`crate::schema::ATTRS`] shape appends one parent's
/// attributes in one go, parents in ascending order, so a parent's rows are a
/// contiguous run and two binary searches find it. [`emit_attrs`] used to scan
/// the whole table per emitted row per level — on a 330 K-row logs block that
/// is 66 million comparisons to render a hundred records, and it measured as
/// *most* of an unfiltered `limit 100`, more than the scan and more than the
/// paging docs/architecture.md section 11 attributes it to. Same shape
/// [`Block::emit_children`] already fixed for the child tables; this is the
/// other half of it.
///
/// Checked at open rather than assumed, because a binary search over unsorted
/// parents does not fail — it silently drops attributes, which is the one
/// outcome nobody would notice.
struct Attrs {
    rows: RecordBatch,
    ordered: bool,
}

impl Attrs {
    fn new(rows: RecordBatch) -> Attrs {
        let ordered = Attrs::parents(&rows).is_some_and(|p| p.windows(2).all(|w| w[0] <= w[1]));
        Attrs { rows, ordered }
    }

    fn parents(rows: &RecordBatch) -> Option<&[u32]> {
        rows.column(0)
            .as_primitive_opt::<UInt32Type>()
            .map(|c| &**c.values())
    }

    /// The rows belonging to `parent`, as a range the caller still filters —
    /// exact when the table is ordered, the whole table when it is not.
    ///
    /// Takes the column the caller already downcast rather than repeating it,
    /// since this runs once per emitted row per level.
    ///
    /// ponytail: that fallback is the linear scan this replaced, kept for a
    /// table no builder in this tree produces. It is O(rows) per emitted row;
    /// if one ever turns up, the fix is to sort it once at open rather than to
    /// make this cleverer.
    fn run(&self, parents: &[u32], parent: u32) -> std::ops::Range<usize> {
        if !self.ordered {
            return 0..parents.len();
        }
        let lo = parents.partition_point(|&p| p < parent);
        lo..lo + parents[lo..].partition_point(|&p| p == parent)
    }
}

/// The child tables of each signal: emitted name, row table, attribute table.
///
/// Links are the *out*-edge of the correlation graph — a link's `trace_id`
/// points at another trace, usually one this node never saw — so returning them
/// is the whole point of storing them. Events are a span's log lines and are
/// what a waterfall shows when a row is expanded.
fn child_tables(signal: Signal) -> &'static [(&'static str, &'static str, &'static str)] {
    match signal {
        Signal::Logs => &[],
        Signal::Traces => &[
            ("events", "span_events", "span_event_attrs"),
            ("links", "span_links", "span_link_attrs"),
        ],
    }
}

impl Block {
    fn open(bref: &Src, signal: Signal) -> Result<Option<Block>> {
        let load = |name: &str| bref.load(name);

        // No root table means the directory is being torn down by retention
        // underneath us. Treat it as absent rather than as an error: racing a
        // deletion of an expired block is normal, not a query failure.
        let Some(root) = load(signal.root())? else {
            return Ok(None);
        };
        let mut children = Vec::new();
        for &(label, table, attrs) in child_tables(signal) {
            // A block whose spans carried no events writes no `span_events`
            // file at all (`publish` skips empty tables), which is absence, not
            // damage.
            if let Some(rows) = load(table)? {
                children.push(Child {
                    label,
                    by_parent: crate::series::index_by_parent(&rows),
                    parent_of_id: index_parent_of_id(&rows),
                    rows,
                    attrs: load(attrs)?.map(Attrs::new),
                });
            }
        }
        Ok(Some(Block {
            signal,
            attrs: load(signal.attrs())?.map(Attrs::new),
            resource_attrs: load("resource_attrs")?.map(Attrs::new),
            scope_attrs: load("scope_attrs")?.map(Attrs::new),
            children,
            root,
        }))
    }

    /// The root's time column as a raw slice, straight out of the mapping.
    fn time(&self) -> Option<&[i64]> {
        self.root
            .column_by_name(self.signal.time_col())
            .map(|c| &**c.as_primitive::<TimestampNanosecondType>().values())
    }

    /// Row numbers of `root` matching the query, ascending.
    fn select(&self, q: &Search, bref: &Src) -> Vec<u32> {
        let n = self.root.num_rows();
        let Some(time) = self.time() else {
            return Vec::new();
        };

        // Time first: cheapest filter, and on a block the query only partly
        // covers, usually the most selective. When the block sits wholly inside
        // the window the comparison is skipped entirely — the directory name
        // already proved it, which is the point of putting the range there.
        // ponytail: the selection is now sized for the whole block even when
        // the time filter throws most of it away, where the filtered collect
        // this replaced grew to the match count — 4 bytes per row of one block,
        // for as long as the block's hits take to build, against a fan-out of
        // sixteen. Worth it for a 2.6x on the one predicate every query has; if
        // the transient ever matters, `shrink_to_fit` after the time filter
        // buys it back for one realloc.
        let mut sel: Vec<u32> = (0..n as u32).collect();
        if !(q.from <= bref.min_ts && q.to >= bref.max_ts) {
            // The `&[i64]` loop docs/architecture.md section 10 names as what
            // replaces intrinsics on the scan, and the one column every query
            // has a predicate on. `None` for the validity because the time
            // column is non-nullable in every signal's schema — and the scan it
            // replaced read the raw slice too, so a nullable one would compare
            // the same bytes it always did.
            keep(&mut sel, None, time, |t| (q.from..=q.to).contains(&t));
        }

        for term in &q.terms {
            if sel.is_empty() {
                break;
            }
            match &term.target {
                Target::Field(name) => {
                    let ok = self
                        .root
                        .column_by_name(name)
                        .is_some_and(|c| field_filter(&mut sel, c.as_ref(), term.op, &term.value));
                    // Unknown column, or a value that cannot be compared
                    // against this column's type at all.
                    if !ok {
                        sel.clear();
                    }
                }
                Target::Attr(key) => {
                    let matched = self.attr_rows(key, term.op, &term.value, n);
                    sel.retain(|&i| matched[i as usize]);
                }
            }
        }
        sel
    }

    /// A bitmap over root rows: does this record carry `key op value` at any
    /// attribute level it has — record, resource, scope, or one of its own
    /// child rows?
    fn attr_rows(&self, key: &str, op: Op, value: &Value, n: usize) -> Vec<bool> {
        let mut out = vec![false; n];

        // Record level: parent_id *is* the root row number, so this is a store,
        // not a join. That is what rebasing ids at ingest bought.
        if let Some(a) = &self.attrs {
            for pid in attr_parents(&a.rows, key, op, value) {
                if let Some(slot) = out.get_mut(pid as usize) {
                    *slot = true;
                }
            }
        }

        // Resource and scope level: parent_id is an entity id, and a foreign key
        // column on the root says which rows point at it. Entity ids are dense
        // from zero and number in the tens, so the reverse lookup is a small
        // boolean array rather than a hash set.
        for (table, fk) in [
            (&self.resource_attrs, "resource_id"),
            (&self.scope_attrs, "scope_id"),
        ] {
            let (Some(a), Some(col)) = (table, self.root.column_by_name(fk)) else {
                continue;
            };
            let ids = attr_parents(&a.rows, key, op, value);
            let Some(&top) = ids.iter().max() else {
                continue;
            };
            let mut wanted = vec![false; top as usize + 1];
            for id in ids {
                wanted[id as usize] = true;
            }
            for (i, &id) in col.as_primitive::<UInt16Type>().values().iter().enumerate() {
                if wanted.get(id as usize).copied().unwrap_or(false) {
                    out[i] = true;
                }
            }
        }

        // Child level: a span's events and links carry attributes of their own,
        // and `recordException` is the reason this matters — the OTel API puts
        // `exception.type`, `exception.message` and `exception.stacktrace` on an
        // *event*, so "which spans threw NullPointerException" is a child-level
        // filter and nothing else. The row that matches is the span the event
        // hangs off: `attr_parents` gives the event's own id, and
        // `parent_of_id` turns that into the root row in one indexed load.
        //
        // The attribute Bloom sidecar has always covered these tables
        // (`attrs::index` walks every table with the ATTRS schema), so a block
        // holding the value was already being opened and scanned for it. What
        // was missing was the last hop.
        for c in &self.children {
            let Some(a) = &c.attrs else { continue };
            for id in attr_parents(&a.rows, key, op, value) {
                // Two `get`s and no `let` chain, for the MSRV reason given in
                // `search` above.
                if let Some(slot) = c
                    .parent_of_id
                    .get(id as usize)
                    .and_then(|&root| out.get_mut(root as usize))
                {
                    *slot = true;
                }
            }
        }
        out
    }

    /// Write one root row as a JSON object, attributes merged in and child rows
    /// nested under it.
    fn emit_row(&self, j: &mut Json, row: u32) {
        j.obj(|j| {
            emit_fields(j, &self.root, row);
            j.key("attributes");
            emit_attrs(
                j,
                &[
                    (&self.resource_attrs, self.fk(row, "resource_id")),
                    (&self.scope_attrs, self.fk(row, "scope_id")),
                    (&self.attrs, Some(row)),
                ],
            );
            for c in &self.children {
                self.emit_children(j, c, row);
            }
        });
    }

    /// The `events` / `links` array of one span.
    ///
    /// Reads the index built when the block was opened. It used to be a linear
    /// pass over the child table per emitted row, on the argument that `limit`
    /// bounds the row count — but the other factor is the block's event count,
    /// so the product is `limit` × events-in-block, and a block holds hundreds
    /// of thousands of events. One pass at open time replaces all of them.
    fn emit_children(&self, j: &mut Json, c: &Child, row: u32) {
        // No key at all rather than an empty array: most spans have neither
        // events nor links, and two empty arrays per span is most of the
        // response.
        let hits = match c.by_parent.get(row as usize) {
            Some(h) if !h.is_empty() => h,
            _ => return,
        };
        j.key(c.label);
        j.arr(|j| {
            for &r in hits {
                let r = r as usize;
                j.obj(|j| {
                    emit_fields(j, &c.rows, r as u32);
                    // The child's own `id`, which its attribute table keys on —
                    // not the span's row number.
                    let id = c
                        .rows
                        .column_by_name("id")
                        .map(|col| col.as_primitive::<UInt32Type>().value(r));
                    j.key("attributes");
                    emit_attrs(j, &[(&c.attrs, id)]);
                });
            }
        });
    }

    fn fk(&self, row: u32, name: &str) -> Option<u32> {
        self.root
            .column_by_name(name)
            .map(|c| c.as_primitive::<UInt16Type>().value(row as usize) as u32)
    }
}

/// The `parent_id` of each child row, indexed by that row's own `id`.
///
/// The inverse of [`crate::series::index_by_parent`] and the same trick: ids
/// are dense from zero per block, so the id is the slot. A table missing either
/// column indexes as empty, and every lookup then misses — which is the same
/// answer a scan of it would give.
fn index_parent_of_id(b: &RecordBatch) -> Vec<u32> {
    let (Some(ids), Some(parents)) = (b.column_by_name("id"), b.column_by_name("parent_id")) else {
        return Vec::new();
    };
    let ids = ids.as_primitive::<UInt32Type>().values();
    let parents = parents.as_primitive::<UInt32Type>().values();
    let mut out = vec![u32::MAX; ids.iter().copied().max().unwrap_or(0) as usize + 1];
    for (&id, &parent) in ids.iter().zip(parents) {
        out[id as usize] = parent;
    }
    out
}

/// Every non-null column of one row, by name.
///
/// The block-local ids are skipped: the caller asked for a log line or a span
/// event, not for the row numbers that found it.
pub(crate) fn emit_fields(j: &mut Json, b: &RecordBatch, row: u32) {
    for (i, f) in b.schema().fields().iter().enumerate() {
        if matches!(
            f.name().as_str(),
            "id" | "parent_id" | "resource_id" | "scope_id"
        ) {
            continue;
        }
        let col = b.column(i);
        if col.is_null(row as usize) {
            continue;
        }
        j.key(f.name());
        // `body_ser` is the one Binary column that is not opaque bytes: it holds
        // the protobuf encoding of a non-string log body, written precisely so
        // that nothing is lost. Sending it through the generic Binary arm below
        // renders a map-valued body as a hex dump, which loses it on the way out
        // instead of on the way in.
        if f.name() == "body_ser" {
            emit_any(j, col.as_binary::<i32>().value(row as usize));
        } else {
            emit_value(j, col.as_ref(), row as usize);
        }
    }
}

/// One `{...}` merging the attributes of several levels, most specific last.
fn emit_attrs(j: &mut Json, levels: &[(&Option<Attrs>, Option<u32>)]) {
    j.obj(|j| {
        let mut merged: Vec<(&str, &RecordBatch, usize)> = Vec::new();
        for &(table, parent) in levels {
            let (Some(a), Some(parent)) = (table, parent) else {
                continue;
            };
            // Empty for a table whose parent column is not a `u32` — which no
            // schema in this tree produces, and which then emits nothing rather
            // than panicking on a block someone else wrote.
            let parents = Attrs::parents(&a.rows).unwrap_or_default();
            for r in a.run(parents, parent).filter(|&r| parents[r] == parent) {
                merged.push((attr_key(&a.rows, r), &a.rows, r));
            }
        }
        // Sorted so output is deterministic, and stably so that within one key
        // the last level pushed — the most specific one — is the entry that
        // survives the dedup below.
        merged.sort_by_key(|(k, _, _)| *k);
        for (i, &(k, a, r)) in merged.iter().enumerate() {
            if merged.get(i + 1).is_some_and(|nxt| nxt.0 == k) {
                continue;
            }
            j.key(k);
            emit_attr(j, a, r);
        }
    });
}

/// `parent_id`s of the attribute rows whose key matches and whose value
/// satisfies `op value`.
pub(crate) fn attr_parents(a: &RecordBatch, key: &str, op: Op, value: &Value) -> Vec<u32> {
    let keys = a.column(1).as_dictionary::<UInt16Type>();
    // Resolve the key string once. Everything after this compares u16 codes.
    let Some(code) = dict_index(keys.values().as_string::<i32>(), key) else {
        return Vec::new();
    };
    let codes = keys.keys().values();
    let parents = &**a.column(0).as_primitive::<UInt32Type>().values();
    let types = &**a.column(2).as_primitive::<UInt8Type>().values();
    let pred = AttrPred::new(a, op, value);

    // Three raw buffers zipped, so the key test — which rejects most rows in a
    // table holding every key of every record — is a `u16` compare against a
    // slice the bounds check is already gone from.
    codes
        .iter()
        .zip(types)
        .zip(parents)
        .enumerate()
        .filter_map(|(i, ((&c, &ty), &p))| (c == code && pred.test(ty, i)).then_some(p))
        .collect()
}

/// One attribute term with everything that does not depend on the row resolved
/// once: the four value columns downcast, the query scalar canonicalized, and —
/// for the string column — the predicate already evaluated against the
/// dictionary.
///
/// This used to be [`attr_matches`], which did all of it *per row*: two
/// `downcast_ref`s, a `String` allocation for [`canon`], and a `parse::<f64>()`
/// on every ordered comparison. That measured 30.9 ns per root row against 6.1
/// for the allocation-free integer arm, which made an attribute filter fifteen
/// times dearer than a column one on the same block.
struct AttrPred<'a> {
    op: Op,
    /// The `str` column's dictionary codes, and which dictionary entries
    /// satisfy the predicate.
    ///
    /// The same trick as [`field_filter`]'s `Dictionary` arm: attribute values
    /// are where telemetry repeats (see [`crate::schema::ATTRS`]), so a few
    /// hundred string comparisons replace one per row and the row loop never
    /// touches string data.
    ///
    /// ponytail: evaluated over the whole dictionary, which every key in the
    /// table shares — so filtering on a rare key in a block whose values are
    /// nearly all distinct pays one comparison per distinct value to reject a
    /// handful of rows. The ceiling is a value column with no repetition in it,
    /// which is the case its dictionary encoding is already the wrong layout
    /// for; the upgrade path is a lazily filled memo over the same array.
    strs: Option<(&'a [u32], Vec<bool>)>,
    ints: Option<(&'a [i64], i64)>,
    doubles: Option<(&'a [f64], f64)>,
    bools: Option<(&'a BooleanArray, bool)>,
}

impl AttrPred<'_> {
    fn new<'a>(a: &'a RecordBatch, op: Op, v: &Value) -> AttrPred<'a> {
        let text = canon(v);
        // A quoted query scalar asked for a text comparison and gets one; so
        // does anything that is not a number. Decided once rather than per row.
        let numeric = !matches!(v, Value::Str(_));
        // `_opt` on all four: a table whose columns are not the ATTRS shape
        // matches nothing here instead of panicking inside a scan thread.
        let strs = a
            .column(3)
            .as_dictionary_opt::<UInt32Type>()
            .and_then(|d| Some((d, d.values().as_string_opt::<i32>()?)))
            .map(|(d, values)| {
                let ok = (0..values.len())
                    .map(|i| {
                        let s = values.value(i);
                        match op {
                            Op::Contains => s.contains(text.as_str()),
                            // Equality against a string column is *defined* as
                            // equality with `canon`, because that is the text
                            // the block index holds (see [`canon`]). Widening
                            // it any further — say, matching the stored string
                            // "200.0" against `eq: 200` because both parse to
                            // the same number — would make the filter prune
                            // away blocks that do contain a match, which is the
                            // one failure mode a sidecar is not allowed to
                            // have.
                            Op::Eq | Op::Ne => op.test_ord(s.cmp(text.as_str())),
                            // Ordering is not in the index — `attr_probes` only
                            // takes `Op::Eq` — so there is nothing here to
                            // disagree with, and a number written as a string
                            // can be ordered as the number it is. It has to be:
                            // half the SDKs that emit
                            // `http.response.status_code` emit it as text, and
                            // lexicographically "1000" sorts below "400", so
                            // `gte: 400` would otherwise mean something
                            // different on each of them.
                            _ => match (s.parse::<f64>(), v.as_f64()) {
                                (Ok(x), Some(y)) if numeric => {
                                    x.partial_cmp(&y).is_some_and(|o| op.test_ord(o))
                                }
                                _ => op.test_ord(s.cmp(text.as_str())),
                            },
                        }
                    })
                    .collect();
                (&**d.keys().values(), ok)
            });
        AttrPred {
            op,
            strs,
            ints: a
                .column(4)
                .as_primitive_opt::<Int64Type>()
                .zip(v.as_i64())
                .map(|(c, y)| (&**c.values(), y)),
            doubles: a
                .column(5)
                .as_primitive_opt::<Float64Type>()
                .zip(v.as_f64())
                .map(|(c, y)| (&**c.values(), y)),
            bools: a.column(6).as_boolean_opt().zip(v.as_bool()),
        }
    }

    /// Compare one attribute row against the query scalar, dispatching on the
    /// stored `type` rather than on the query's — the column decides what it
    /// is.
    ///
    /// Empty, Bytes, Slice and Map are returned in results but not filterable
    /// in V0, and a column whose type does not match the schema reads the same
    /// way: no match.
    fn test(&self, ty: u8, row: usize) -> bool {
        const STR: u8 = AttrType::Str as u8;
        const INT: u8 = AttrType::Int as u8;
        const DOUBLE: u8 = AttrType::Double as u8;
        const BOOL: u8 = AttrType::Bool as u8;
        match ty {
            STR => self.strs.as_ref().is_some_and(|(codes, ok)| {
                codes
                    .get(row)
                    .and_then(|&c| ok.get(c as usize))
                    .copied()
                    .unwrap_or(false)
            }),
            INT => self
                .ints
                .is_some_and(|(xs, y)| xs.get(row).is_some_and(|&x| self.op.test_ord(x.cmp(&y)))),
            DOUBLE => self.doubles.is_some_and(|(xs, y)| {
                xs.get(row)
                    .and_then(|x| x.partial_cmp(&y))
                    .is_some_and(|o| self.op.test_ord(o))
            }),
            BOOL => self
                .bools
                .is_some_and(|(xs, y)| row < xs.len() && self.op.test_ord(xs.value(row).cmp(&y))),
            _ => false,
        }
    }
}

pub(crate) fn attr_key(a: &RecordBatch, row: usize) -> &str {
    let d = a.column(1).as_dictionary::<UInt16Type>();
    d.values()
        .as_string::<i32>()
        .value(d.keys().value(row) as usize)
}

/// Position of `needle` in a dictionary's value array.
///
/// Linear over the dictionary, which is at most 65536 entries and in practice a
/// few dozen. Doing it once here is what keeps the row scan off the string data
/// entirely.
pub(crate) fn dict_index(values: &StringArray, needle: &str) -> Option<u16> {
    (0..values.len())
        .find(|&i| values.value(i) == needle)
        .map(|i| i as u16)
}

/// Narrow `sel` to the rows of `col` satisfying `op value`.
///
/// `false` means the query value cannot be compared against this column at all,
/// which the caller turns into an empty result. Nothing is written to `sel`
/// before that decision, so a refusal leaves it untouched.
///
/// One monomorphic loop per column type, where this used to build a
/// `Box<dyn Fn(u32) -> bool>` and pay an indirect call per row.
/// docs/architecture.md section 10 rejects hand-written intrinsics on the scan
/// and names what replaces them: "a tight loop over `&[i64]` with no bounds
/// checks and no branches, which LLVM turns into NEON unasked". That is what
/// [`keep`] is; this function's only job is to hand it a values slice and a
/// comparison that inlines into it.
fn field_filter(sel: &mut Vec<u32>, col: &dyn Array, op: Op, v: &Value) -> bool {
    macro_rules! ints {
        ($t:ty) => {{
            let Some(target) = v.as_i64() else {
                return false;
            };
            let vals = col.as_primitive::<$t>().values();
            keep(sel, col.nulls(), vals, |x| {
                op.test_ord((x as i64).cmp(&target))
            });
        }};
    }

    match col.data_type() {
        DataType::Timestamp(_, _) => ints!(TimestampNanosecondType),
        DataType::Int64 => ints!(Int64Type),
        DataType::Int32 => ints!(Int32Type),
        DataType::UInt64 => ints!(UInt64Type),
        DataType::UInt32 => ints!(UInt32Type),
        DataType::UInt16 => ints!(UInt16Type),
        DataType::UInt8 => ints!(UInt8Type),
        DataType::Float64 => {
            let Some(target) = v.as_f64() else {
                return false;
            };
            let vals = col.as_primitive::<Float64Type>().values();
            keep(sel, col.nulls(), vals, |x: f64| {
                x.partial_cmp(&target).is_some_and(|o| op.test_ord(o))
            });
        }
        DataType::Dictionary(_, _) => {
            let d = col.as_dictionary::<UInt16Type>();
            let values = d.values().as_string::<i32>();
            let Some(needle) = v.as_str() else {
                return false;
            };
            // Evaluate the predicate against the dictionary, not the rows: a
            // few dozen string comparisons replace one per row, and the scan
            // below is a lookup table indexed by a u16.
            let ok: Vec<bool> = (0..values.len())
                .map(|i| {
                    let s = values.value(i);
                    match op {
                        Op::Contains => s.contains(needle),
                        _ => op.test_ord(s.cmp(needle)),
                    }
                })
                .collect();
            keep(sel, col.nulls(), d.keys().values(), |c| {
                ok.get(c as usize).copied().unwrap_or(false)
            });
        }
        DataType::Boolean => {
            let a = col.as_boolean();
            let Some(target) = v.as_bool() else {
                return false;
            };
            keep_by(sel, a.len(), col.nulls(), |i| {
                op.test_ord(a.value(i).cmp(&target))
            });
        }
        DataType::Utf8 => {
            let a = col.as_string::<i32>();
            let Some(target) = v.as_str() else {
                return false;
            };
            keep_by(sel, a.len(), col.nulls(), |i| {
                let s = a.value(i);
                match op {
                    Op::Contains => s.contains(target),
                    _ => op.test_ord(s.cmp(target)),
                }
            });
        }
        DataType::FixedSizeBinary(_) => {
            // Trace and span ids: hex in the query, bytes on disk. Decoding the
            // needle once beats hex-encoding every row.
            let a = col.as_fixed_size_binary();
            let Some(target) = v.as_str().and_then(unhex) else {
                return false;
            };
            keep_by(sel, a.len(), col.nulls(), |i| {
                op.test_ord(a.value(i).cmp(target.as_slice()))
            });
        }
        _ => return false,
    }
    true
}

/// Narrow `sel` to the rows where `p(vals[row])` holds.
///
/// Three shapes, because the first one is the whole point. A column with no
/// nulls, not yet narrowed by an earlier term — which is every first term on a
/// block the time range covers whole — walks `vals` contiguously and writes the
/// surviving row number unconditionally, advancing the cursor by the boolean.
/// No indirect call, no bounds check on the load, and no branch in the body, so
/// the comparison itself stays in vector registers.
///
/// `sel.len() == vals.len()` is what proves the selection is still the identity
/// `0..n`: [`Block::select`] only ever removes from it, and it is built
/// ascending, so a full-length selection has nothing missing from it.
///
/// ponytail: the two narrowed shapes stay a gather under `retain` and do not
/// vectorise. Making them would mean the selection becoming a bitmap so every
/// term reads and writes contiguously — a different engine, and worth it only
/// once a query with several selective terms shows up in a profile. Terms are
/// applied cheapest-first-by-accident today, and most queries carry one.
fn keep<T: Copy>(
    sel: &mut Vec<u32>,
    nulls: Option<&arrow_buffer::NullBuffer>,
    vals: &[T],
    p: impl Fn(T) -> bool,
) {
    match nulls {
        None if sel.len() == vals.len() => {
            let mut k = 0;
            for (i, &x) in vals.iter().enumerate() {
                sel[k] = i as u32;
                k += p(x) as usize;
            }
            sel.truncate(k);
        }
        None => sel.retain(|&i| vals.get(i as usize).is_some_and(|&x| p(x))),
        // An `is_null` per row is what kept this loop scalar, which is why the
        // case above exists at all. Every column a sealed block filters on is
        // non-null in practice; this is the path that stays correct when one is
        // not.
        Some(n) => {
            sel.retain(|&i| n.is_valid(i as usize) && vals.get(i as usize).is_some_and(|&x| p(x)));
        }
    }
}

/// [`keep`] for a column with no values slice to walk: a bitmap, a variable
/// offset array or a fixed stride, none of which a vector register helps with.
/// The win here is only the closure being monomorphic rather than boxed.
fn keep_by(
    sel: &mut Vec<u32>,
    len: usize,
    nulls: Option<&arrow_buffer::NullBuffer>,
    p: impl Fn(usize) -> bool,
) {
    match nulls {
        None => sel.retain(|&i| (i as usize) < len && p(i as usize)),
        Some(n) => sel.retain(|&i| n.is_valid(i as usize) && p(i as usize)),
    }
}

/// Decode hex, either case. `None` on an odd length or any non-hex byte, so a
/// malformed trace id matches nothing rather than matching a truncated prefix.
pub fn unhex(s: &str) -> Option<Vec<u8>> {
    if s.len() % 2 != 0 {
        return None;
    }
    let b = s.as_bytes();
    (0..b.len() / 2)
        .map(|i| {
            let hi = (b[i * 2] as char).to_digit(16)?;
            let lo = (b[i * 2 + 1] as char).to_digit(16)?;
            Some((hi * 16 + lo) as u8)
        })
        .collect()
}

/// Emit one cell of a root table.
fn emit_value(j: &mut Json, col: &dyn Array, row: usize) {
    match col.data_type() {
        // Nanoseconds, never a formatted date: every other representation
        // either loses precision or picks a timezone on the user's behalf, and
        // the UI formats them, which is where that belongs. As a *string*,
        // because that is what OTLP/JSON says a 64-bit integer is and because
        // 1.7e18 is twenty times past what a JSON number survives — see
        // [`Json::i64_str`].
        DataType::Timestamp(_, _) => {
            j.i64_str(col.as_primitive::<TimestampNanosecondType>().value(row));
        }
        DataType::Int64 => j.i64_str(col.as_primitive::<Int64Type>().value(row)),
        // 32 bits and narrower stay bare: they fit a double exactly, and a
        // reader doing arithmetic on `severity_number` or `status_code` should
        // not have to parse it first.
        DataType::Int32 => j.i64(col.as_primitive::<Int32Type>().value(row) as i64),
        DataType::UInt64 => j.u64_str(col.as_primitive::<UInt64Type>().value(row)),
        DataType::UInt32 => j.u64(col.as_primitive::<UInt32Type>().value(row) as u64),
        DataType::UInt16 => j.u64(col.as_primitive::<UInt16Type>().value(row) as u64),
        DataType::UInt8 => j.u64(col.as_primitive::<UInt8Type>().value(row) as u64),
        DataType::Float64 => j.f64(col.as_primitive::<Float64Type>().value(row)),
        DataType::Boolean => j.bool(col.as_boolean().value(row)),
        DataType::Utf8 => j.str(col.as_string::<i32>().value(row)),
        DataType::Binary => j.hex(col.as_binary::<i32>().value(row)),
        // `as_fixed_size_binary` panics on a type mismatch, exactly like the
        // `as_primitive`, `as_string` and `as_binary` arms above it. The match
        // is on `data_type()`, so a mismatch would mean the array disagreeing
        // with its own type — a broken Arrow build, not a broken block.
        DataType::FixedSizeBinary(_) => j.hex(col.as_fixed_size_binary().value(row)),
        DataType::Dictionary(_, _) => {
            let d = col.as_dictionary::<UInt16Type>();
            j.str(
                d.values()
                    .as_string::<i32>()
                    .value(d.keys().value(row) as usize),
            );
        }
        DataType::List(_) => {
            let inner = col.as_list::<i32>().value(row);
            j.arr(|j| {
                for i in 0..inner.len() {
                    if inner.is_null(i) {
                        j.null();
                    } else {
                        emit_value(j, inner.as_ref(), i);
                    }
                }
            });
        }
        _ => j.null(),
    }
}

/// Emit one attribute's value from whichever column its `type` names.
pub(crate) fn emit_attr(j: &mut Json, a: &RecordBatch, row: usize) {
    const STR: u8 = AttrType::Str as u8;
    const INT: u8 = AttrType::Int as u8;
    const DOUBLE: u8 = AttrType::Double as u8;
    const BOOL: u8 = AttrType::Bool as u8;
    const BYTES: u8 = AttrType::Bytes as u8;
    const SLICE: u8 = AttrType::Slice as u8;
    const MAP: u8 = AttrType::Map as u8;
    match a.column(2).as_primitive::<UInt8Type>().value(row) {
        STR => j.str(crate::attrs::str_column(a).value(row)),
        // Int attributes go out as JSON strings, because that is the OTLP/JSON
        // encoding of an `int64` and principle 3 says OTLP decides the wire
        // form. The property is round-trip symmetry, not browser etiquette:
        // ingest reads these as strings, so what Mira emits Mira must accept —
        // and it is the same property in reverse that makes it safe, since a
        // reader that wants the number back has the exact digits rather than
        // whatever a double rounded them to.
        INT => j.i64_str(a.column(4).as_primitive::<Int64Type>().value(row)),
        DOUBLE => j.f64(a.column(5).as_primitive::<Float64Type>().value(row)),
        BOOL => j.bool(a.column(6).as_boolean().value(row)),
        BYTES => j.hex(a.column(7).as_binary::<i32>().value(row)),
        // `ser` holds the protobuf encoding of the whole `AnyValue`, so an array
        // or a map is a decode away rather than a reconstruction. They are not
        // exotic: `process.command_args` and `http.request.header.*` are arrays
        // by semantic convention, and `gen_ai.input.messages` — the first-class
        // case in ARCHITECTURE section 1 — is a kvlist.
        SLICE | MAP => emit_any(j, a.column(8).as_binary::<i32>().value(row)),
        // AttrType::Empty: the key arrived with no value at all, which is what
        // `null` means.
        _ => j.null(),
    }
}

/// Render a protobuf-encoded `AnyValue` — the attribute `ser` column, and a log
/// body that was not a string — as the JSON it describes.
///
/// A decode failure becomes `null`. These bytes were written by this process
/// from an already-decoded message, so a failure here means the block is
/// damaged, and one damaged value must not take the whole response with it.
pub(crate) fn emit_any(j: &mut Json, bytes: &[u8]) {
    match AnyValue::decode(bytes) {
        Ok(v) => emit_any_value(j, v.value.as_ref()),
        Err(_) => j.null(),
    }
}

/// Recursion is bounded by prost's own decode recursion limit (100), which
/// `AnyValue::decode` above has already enforced on these bytes — a hostile
/// client cannot nest deeply enough here to reach the stack.
fn emit_any_value(j: &mut Json, v: Option<&mira_proto::common::v1::any_value::Value>) {
    use mira_proto::common::v1::any_value::Value as Av;
    match v {
        None => j.null(),
        Some(Av::StringValue(s)) => j.str(s),
        // A string for the same reason [`emit_attr`]'s `INT` arm is one: these
        // bytes are an OTLP `AnyValue`, and OTLP/JSON writes its `int_value` as
        // a string.
        Some(Av::IntValue(i)) => j.i64_str(*i),
        Some(Av::DoubleValue(d)) => j.f64(*d),
        Some(Av::BoolValue(b)) => j.bool(*b),
        Some(Av::BytesValue(b)) => j.hex(b),
        Some(Av::ArrayValue(a)) => j.arr(|j| {
            for e in &a.values {
                emit_any_value(j, e.value.as_ref());
            }
        }),
        Some(Av::KvlistValue(m)) => j.obj(|j| {
            for e in &m.values {
                j.key(&e.key);
                emit_any_value(j, e.value.as_ref().and_then(|v| v.value.as_ref()));
            }
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use arrow_array::builder::StringDictionaryBuilder;
    use arrow_array::{
        BinaryArray, BooleanArray, FixedSizeBinaryArray, Float64Array, Int32Array, Int64Array,
        ListArray, TimestampNanosecondArray, UInt8Array, UInt16Array, UInt32Array, UInt64Array,
    };
    use std::sync::Arc;

    /// `u32::MAX` for "this column cannot be compared against this value",
    /// which the scan turns into an empty result rather than into every row.
    fn hits(col: &dyn Array, op: Op, v: &Value) -> Vec<u32> {
        let mut sel: Vec<u32> = (0..col.len() as u32).collect();
        if field_filter(&mut sel, col, op, v) {
            sel
        } else {
            vec![u32::MAX]
        }
    }

    /// The predicate builder is a type-dispatch table, and a column type missing
    /// from it does not error — it returns no rows. So the only way an arm can be
    /// wrong and stay quiet is if nothing exercises it.
    ///
    /// Every arm gets the same three rows — below, equal, above — so one
    /// expectation checks the arm, the ordering and the null handling at once.
    #[test]
    fn every_column_type_compares_the_same_way() {
        let cols: Vec<(&str, Arc<dyn Array>)> = vec![
            (
                "timestamp",
                Arc::new(TimestampNanosecondArray::from(vec![
                    Some(1),
                    Some(2),
                    Some(3),
                    None,
                ])),
            ),
            (
                "i64",
                Arc::new(Int64Array::from(vec![Some(1), Some(2), Some(3), None])),
            ),
            (
                "i32",
                Arc::new(Int32Array::from(vec![Some(1), Some(2), Some(3), None])),
            ),
            (
                "u64",
                Arc::new(UInt64Array::from(vec![Some(1), Some(2), Some(3), None])),
            ),
            (
                "u32",
                Arc::new(UInt32Array::from(vec![Some(1), Some(2), Some(3), None])),
            ),
            (
                "u16",
                Arc::new(UInt16Array::from(vec![Some(1), Some(2), Some(3), None])),
            ),
            (
                "u8",
                Arc::new(UInt8Array::from(vec![Some(1), Some(2), Some(3), None])),
            ),
            (
                "f64",
                Arc::new(Float64Array::from(vec![
                    Some(1.0),
                    Some(2.0),
                    Some(3.0),
                    None,
                ])),
            ),
        ];
        for (name, col) in &cols {
            let c = col.as_ref();
            assert_eq!(hits(c, Op::Eq, &Value::Int(2)), [1], "{name} eq");
            assert_eq!(hits(c, Op::Ne, &Value::Int(2)), [0, 2], "{name} ne");
            assert_eq!(hits(c, Op::Lt, &Value::Int(2)), [0], "{name} lt");
            assert_eq!(hits(c, Op::Lte, &Value::Int(2)), [0, 1], "{name} lte");
            assert_eq!(hits(c, Op::Gt, &Value::Int(2)), [2], "{name} gt");
            assert_eq!(hits(c, Op::Gte, &Value::Int(2)), [1, 2], "{name} gte");
            // A null is not less than anything, and `ne` is where that bites:
            // the naive reading would return it.
            assert!(!hits(c, Op::Ne, &Value::Int(9)).contains(&3), "{name} null");
            // Quoted, because a browser and an LLM both write "2" as often as 2.
            assert_eq!(hits(c, Op::Eq, &Value::Str("2".into())), [1], "{name} str");
            // Nothing to compare against: no rows, not an error.
            assert_eq!(
                hits(c, Op::Eq, &Value::Bool(true)),
                [u32::MAX],
                "{name} bool"
            );
        }

        // A fractional target against an integer column has no integer to be
        // equal to. Truncating would make `duration > 0.5` mean `duration > 0`.
        assert_eq!(
            hits(cols[1].1.as_ref(), Op::Gt, &Value::Double(1.5)),
            [u32::MAX]
        );
        assert_eq!(hits(cols[1].1.as_ref(), Op::Gt, &Value::Double(2.0)), [2]);
        // Floats compare as floats, and NaN is unordered rather than equal.
        let f = Float64Array::from(vec![Some(1.5), Some(f64::NAN)]);
        assert_eq!(hits(&f, Op::Gt, &Value::Double(1.0)), [0]);
        assert_eq!(
            hits(&f, Op::Eq, &Value::Double(f64::NAN)),
            Vec::<u32>::new()
        );

        let b = BooleanArray::from(vec![Some(true), Some(false), None]);
        assert_eq!(hits(&b, Op::Eq, &Value::Bool(true)), [0]);
        assert_eq!(hits(&b, Op::Eq, &Value::Str("false".into())), [1]);
        assert_eq!(hits(&b, Op::Eq, &Value::Int(1)), [u32::MAX]);

        let s = StringArray::from(vec![Some("alpha"), Some("beta"), None]);
        assert_eq!(hits(&s, Op::Eq, &Value::Str("beta".into())), [1]);
        assert_eq!(hits(&s, Op::Contains, &Value::Str("et".into())), [1]);
        assert_eq!(hits(&s, Op::Lt, &Value::Str("b".into())), [0]);
        assert_eq!(hits(&s, Op::Eq, &Value::Int(1)), [u32::MAX]);

        let mut d = StringDictionaryBuilder::<UInt16Type>::new();
        for v in ["ERROR", "INFO", "ERROR"] {
            d.append_value(v);
        }
        let d = d.finish();
        assert_eq!(hits(&d, Op::Eq, &Value::Str("ERROR".into())), [0, 2]);
        // Resolved against the dictionary once. A value that is not in it cannot
        // match any row, and saying so costs no row scan at all.
        assert_eq!(
            hits(&d, Op::Eq, &Value::Str("TRACE".into())),
            Vec::<u32>::new()
        );

        // Ids arrive as hex and live as bytes. The needle is decoded once, so a
        // needle that is not hex at all is no rows rather than every row.
        let ids = FixedSizeBinaryArray::try_from_iter([[1u8, 2], [3, 4]].into_iter()).unwrap();
        assert_eq!(hits(&ids, Op::Eq, &Value::Str("0102".into())), [0]);
        assert_eq!(hits(&ids, Op::Gt, &Value::Str("0102".into())), [1]);
        assert_eq!(hits(&ids, Op::Eq, &Value::Str("zz".into())), [u32::MAX]);
        assert_eq!(hits(&ids, Op::Eq, &Value::Str("010".into())), [u32::MAX]);

        // A type the table does not know is not a panic and not an error.
        let l = ListArray::from_iter_primitive::<Int64Type, _, _>(vec![Some(vec![Some(1)])]);
        assert_eq!(hits(&l, Op::Eq, &Value::Int(1)), [u32::MAX]);
    }

    /// Materialization is the same dispatch table read the other way, and its
    /// failure mode is worse: a column emitted under the wrong JSON type is a
    /// reader's bug, not ours.
    #[test]
    fn every_column_type_materializes_as_the_json_type_it_is() {
        let cell = |col: &dyn Array| {
            let mut j = Json::new();
            j.arr(|j| emit_value(j, col, 0));
            let s = j.into_string();
            s[1..s.len() - 1].to_owned()
        };
        let l = ListArray::from_iter_primitive::<Int64Type, _, _>(vec![Some(vec![
            Some(1),
            None,
            Some(3),
        ])]);
        let mut d = StringDictionaryBuilder::<UInt16Type>::new();
        d.append_value("ERROR");
        let cases: Vec<(Arc<dyn Array>, &str)> = vec![
            // The 64-bit trio is quoted and the narrower integers are not. The
            // timestamp is the case that would be silently wrong as a number:
            // as a double it reads back 1700000000000000000, one nanosecond off
            // and no parser anywhere complains.
            (
                Arc::new(TimestampNanosecondArray::from(vec![
                    1_700_000_000_000_000_001i64,
                ])),
                r#""1700000000000000001""#,
            ),
            (Arc::new(Int64Array::from(vec![-7i64])), r#""-7""#),
            (Arc::new(Int32Array::from(vec![-7i32])), "-7"),
            (
                Arc::new(UInt64Array::from(vec![u64::MAX])),
                r#""18446744073709551615""#,
            ),
            (Arc::new(UInt32Array::from(vec![7u32])), "7"),
            (Arc::new(UInt16Array::from(vec![7u16])), "7"),
            (Arc::new(UInt8Array::from(vec![7u8])), "7"),
            (Arc::new(Float64Array::from(vec![0.5f64])), "0.5"),
            (Arc::new(BooleanArray::from(vec![true])), "true"),
            (Arc::new(StringArray::from(vec!["a\"b"])), r#""a\"b""#),
            (
                Arc::new(BinaryArray::from(vec![&b"\xab\xcd"[..]])),
                r#""abcd""#,
            ),
            // A trace id: hex, and the same hex the query takes as a needle.
            (
                Arc::new(
                    FixedSizeBinaryArray::try_from_iter([[0xabu8, 0xcd]].into_iter()).unwrap(),
                ),
                r#""abcd""#,
            ),
            (Arc::new(d.finish()), r#""ERROR""#),
            // A null inside a list stays a null; the surrounding array does not
            // collapse to one. The elements are `Int64`, so they are quoted for
            // the same reason a scalar `Int64` is — the list arm recurses into
            // this same table rather than having a second encoding.
            (Arc::new(l), r#"["1",null,"3"]"#),
        ];
        for (col, want) in cases {
            assert_eq!(cell(col.as_ref()), want, "{:?}", col.data_type());
        }

        // A type with no representation is null rather than a guess.
        let m = arrow_array::Int8Array::from(vec![1i8]);
        assert_eq!(cell(&m), "null");
    }

    /// Everything the scalar helpers promise in their own doc comments, in one
    /// place, because each of them is a coercion rule a query author will hit
    /// and none of them is guessable from the type.
    #[test]
    fn a_scalar_coerces_to_what_the_column_needs_or_to_nothing() {
        for (s, want) in [
            ("eq", Op::Eq),
            ("=", Op::Eq),
            ("==", Op::Eq),
            ("ne", Op::Ne),
            ("!=", Op::Ne),
            ("lt", Op::Lt),
            ("<", Op::Lt),
            ("lte", Op::Lte),
            ("<=", Op::Lte),
            ("gt", Op::Gt),
            (">", Op::Gt),
            ("gte", Op::Gte),
            (">=", Op::Gte),
            ("contains", Op::Contains),
            ("~", Op::Contains),
        ] {
            assert_eq!(Op::parse(s), Some(want), "{s}");
        }
        assert_eq!(Op::parse("=~"), None);
        // Documented as unreachable — every caller handles Contains first — so
        // the guarantee is that it stays inert if one day a caller does not.
        use std::cmp::Ordering::*;
        for ord in [Less, Equal, Greater] {
            assert!(!Op::Contains.test_ord(ord));
        }

        assert_eq!(Value::Int(3).as_i64(), Some(3));
        assert_eq!(Value::Double(3.0).as_i64(), Some(3));
        assert_eq!(Value::Double(3.5).as_i64(), None);
        assert_eq!(Value::Str("3".into()).as_i64(), Some(3));
        assert_eq!(Value::Str("3.5".into()).as_i64(), None);
        assert_eq!(Value::Bool(true).as_i64(), None);

        assert_eq!(Value::Int(3).as_f64(), Some(3.0));
        assert_eq!(Value::Double(3.5).as_f64(), Some(3.5));
        assert_eq!(Value::Str("3.5".into()).as_f64(), Some(3.5));
        assert_eq!(Value::Str("x".into()).as_f64(), None);
        assert_eq!(Value::Bool(true).as_f64(), None);

        assert_eq!(Value::Bool(false).as_bool(), Some(false));
        assert_eq!(Value::Str("true".into()).as_bool(), Some(true));
        assert_eq!(Value::Str("false".into()).as_bool(), Some(false));
        assert_eq!(Value::Str("TRUE".into()).as_bool(), None);
        assert_eq!(Value::Int(1).as_bool(), None);
        assert_eq!(Value::Double(1.0).as_bool(), None);

        assert_eq!(Value::Str("x".into()).as_str(), Some("x"));
        assert_eq!(Value::Int(1).as_str(), None);

        // Ids arrive from a URL, a log line or a model, so both cases and
        // neither-of-them all have to land somewhere predictable.
        assert_eq!(unhex("0aFf"), Some(vec![0x0a, 0xff]));
        assert_eq!(unhex(""), Some(vec![]));
        assert_eq!(unhex("abc"), None);
        assert_eq!(unhex("0g"), None);
        assert_eq!(unhex("0 1"), None);

        let vals = StringArray::from(vec!["a", "b"]);
        assert_eq!(dict_index(&vals, "b"), Some(1));
        assert_eq!(dict_index(&vals, "c"), None);
    }

    /// Every `AnyValue` arm, including the ones no attribute in the scan below
    /// carries, and bytes that are not an `AnyValue` at all. The renderer is a
    /// recursive decoder over attacker-supplied structure, which is the one
    /// shape in this file where a missing arm is a silently wrong answer.
    #[test]
    fn every_any_value_arm_renders_and_damage_renders_as_null() {
        use mira_proto::common::v1::any_value::Value as Av;
        use mira_proto::common::v1::{ArrayValue, KeyValue, KeyValueList};

        let render = |v: Option<Av>| {
            let mut j = Json::new();
            emit_any(&mut j, &AnyValue { value: v }.encode_to_vec());
            j.into_string()
        };
        assert_eq!(render(None), "null");
        assert_eq!(render(Some(Av::StringValue("s".into()))), r#""s""#);
        // Quoted: an `AnyValue`'s `int_value` is an `int64`, and OTLP/JSON
        // writes those as strings in both directions.
        assert_eq!(render(Some(Av::IntValue(-1))), r#""-1""#);
        assert_eq!(render(Some(Av::DoubleValue(0.5))), "0.5");
        assert_eq!(render(Some(Av::BoolValue(true))), "true");
        assert_eq!(
            render(Some(Av::BytesValue(vec![0xbe, 0xef].into()))),
            r#""beef""#
        );
        // Nested both ways round, because the recursion is the only part of
        // this that can be wrong in a way a flat value would not show.
        assert_eq!(
            render(Some(Av::ArrayValue(ArrayValue {
                values: vec![
                    AnyValue { value: None },
                    AnyValue {
                        value: Some(Av::KvlistValue(KeyValueList {
                            values: vec![KeyValue {
                                key: "k".into(),
                                value: Some(AnyValue {
                                    value: Some(Av::IntValue(2)),
                                }),
                            }],
                        })),
                    },
                ],
            }))),
            r#"[null,{"k":"2"}]"#
        );

        // A field number and wire type no protobuf carries. The bytes only get
        // here by being on disk, so this is a damaged block, not a bad request.
        let mut j = Json::new();
        emit_any(&mut j, &[0xff, 0xff, 0xff]);
        assert_eq!(j.into_string(), "null");
    }

    /// The inverse index a child's attribute join reads: the child's own `id`
    /// is the slot and the value is the row it hangs off. A table missing
    /// either column indexes as empty and every lookup then misses, which is
    /// the same answer scanning it would give and not a panic.
    #[test]
    fn a_child_row_finds_its_parent_by_its_own_id() {
        let ids: Arc<dyn Array> = Arc::new(UInt32Array::from(vec![2u32, 0]));
        let parents: Arc<dyn Array> = Arc::new(UInt32Array::from(vec![7u32, 5]));
        let b = RecordBatch::try_from_iter([("id", ids.clone()), ("parent_id", parents)]).unwrap();
        let idx = index_parent_of_id(&b);
        assert_eq!(idx.len(), 3, "dense from zero to the largest id");
        assert_eq!(idx[0], 5);
        assert_eq!(idx[1], u32::MAX, "an id no row claims has no parent");
        assert_eq!(idx[2], 7);

        let orphan = RecordBatch::try_from_iter([("id", ids)]).unwrap();
        assert!(index_parent_of_id(&orphan).is_empty());
    }

    /// A block on disk, scanned. The predicate tables above are exercised in
    /// isolation; this is the path that reaches them — the time prefilter, the
    /// three attribute levels, and the merge that decides which of two levels
    /// setting the same key the caller actually sees.
    #[test]
    fn a_scan_narrows_by_time_then_by_terms_and_the_most_specific_level_wins() {
        use mira_proto::collector::logs::v1::ExportLogsServiceRequest;
        use mira_proto::common::v1::any_value::Value as Av;
        use mira_proto::common::v1::{
            AnyValue, ArrayValue, InstrumentationScope, KeyValue, KeyValueList,
        };
        use mira_proto::logs::v1::{LogRecord, ResourceLogs, ScopeLogs};
        use mira_proto::resource::v1::Resource;

        let kv = |k: &str, v: Av| KeyValue {
            key: k.into(),
            value: Some(AnyValue { value: Some(v) }),
        };
        let base = 1_700_000_000_000_000_000u64;
        let req = ExportLogsServiceRequest {
            resource_logs: vec![ResourceLogs {
                resource: Some(Resource {
                    attributes: vec![
                        kv("service.name", Av::StringValue("checkout".into())),
                        // Also set per-record below, which is the whole point:
                        // an SDK default that one record overrides.
                        kv("deploy.env", Av::StringValue("prod".into())),
                    ],
                    ..Default::default()
                }),
                scope_logs: vec![ScopeLogs {
                    scope: Some(InstrumentationScope {
                        name: "mira.test".into(),
                        attributes: vec![kv("scope.kind", Av::StringValue("lib".into()))],
                        ..Default::default()
                    }),
                    log_records: (0..3)
                        .map(|i| LogRecord {
                            time_unix_nano: base + i * 1_000_000_000,
                            severity_number: 9 + i as i32,
                            severity_text: "INFO".into(),
                            // OTel Events carry the identity of the event here
                            // and not in an attribute; empty on the others,
                            // which is how OTLP says "this is a plain log".
                            event_name: if i == 0 {
                                "user.login".into()
                            } else {
                                String::new()
                            },
                            // The last record's body is a map rather than a
                            // string, which is the case `body_ser` exists for.
                            body: Some(AnyValue {
                                value: Some(if i == 2 {
                                    Av::KvlistValue(KeyValueList {
                                        values: vec![kv(
                                            "msg",
                                            Av::StringValue("structured body".into()),
                                        )],
                                    })
                                } else {
                                    Av::StringValue(format!("line {i}"))
                                }),
                            }),
                            attributes: vec![
                                kv("deploy.env", Av::StringValue("canary".into())),
                                kv("attempt", Av::IntValue(i as i64)),
                                // None of these three is filterable in V0; all
                                // three still have to come back in the row.
                                kv("payload", Av::BytesValue(vec![0xde, 0xad].into())),
                                kv(
                                    "tags",
                                    Av::ArrayValue(ArrayValue {
                                        values: vec![
                                            AnyValue {
                                                value: Some(Av::StringValue("a".into())),
                                            },
                                            AnyValue {
                                                value: Some(Av::IntValue(7)),
                                            },
                                        ],
                                    }),
                                ),
                                kv(
                                    "gen_ai.input.messages",
                                    Av::KvlistValue(KeyValueList {
                                        values: vec![kv("role", Av::StringValue("user".into()))],
                                    }),
                                ),
                                // A key that arrived with no value at all.
                                // Legal OTLP, and `null` is what it means.
                                KeyValue {
                                    key: "trace.hint".into(),
                                    value: None,
                                },
                            ],
                            ..Default::default()
                        })
                        .collect(),
                    ..Default::default()
                }],
                ..Default::default()
            }],
        };

        let dir = std::env::temp_dir().join(format!("mira-scan-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let mut b = crate::logs::LogsBuilder::new();
        b.append_request(&req).unwrap();
        let sealed = b.finish().unwrap();
        let bref =
            crate::block::publish(&dir, "logs", crate::block::node_id("a"), 0, 0, &sealed).unwrap();

        let base = base as i64;
        let scan = |from: i64, to: i64, terms: Vec<Term>| {
            search(
                &dir,
                &Search {
                    signal: Signal::Logs,
                    from,
                    to,
                    terms,
                    limit: 100,
                    after: None,
                },
            )
            .unwrap()
        };
        let field = |n: &str, op: Op, v: Value| Term {
            target: Target::Field(n.into()),
            op,
            value: v,
        };
        let attr = |n: &str, op: Op, v: Value| Term {
            target: Target::Attr(n.into()),
            op,
            value: v,
        };

        // A window the block only partly covers: the directory name proved the
        // block is worth opening, and only then does the timestamp get read.
        let r = scan(base + 500_000_000, base + 1_500_000_000, vec![]);
        assert_eq!(r.stats.rows_matched, 1, "{}", r.json);
        assert!(r.json.contains("line 1"), "{}", r.json);

        let all = base + 10_000_000_000;
        assert_eq!(scan(base, all, vec![]).stats.rows_matched, 3);

        // A column this signal does not have is no rows, not an error and not
        // every row — a query spanning signals is a normal thing to try.
        assert_eq!(
            scan(
                base,
                all,
                vec![field("duration_nano", Op::Gt, Value::Int(0))]
            )
            .stats
            .rows_matched,
            0
        );
        // Same for a value that cannot be compared against the column's type.
        assert_eq!(
            scan(
                base,
                all,
                vec![field("severity_number", Op::Eq, Value::Bool(true))]
            )
            .stats
            .rows_matched,
            0
        );
        // Once nothing is selected the remaining terms are skipped, so a term
        // that would have been expensive costs nothing.
        assert_eq!(
            scan(
                base,
                all,
                vec![
                    field("severity_number", Op::Gt, Value::Int(99)),
                    attr("service.name", Op::Eq, Value::Str("checkout".into())),
                ]
            )
            .stats
            .rows_matched,
            0
        );
        // Attributes are found without being told which level they live at.
        for (k, v) in [("service.name", "checkout"), ("scope.kind", "lib")] {
            assert_eq!(
                scan(base, all, vec![attr(k, Op::Eq, Value::Str(v.into()))])
                    .stats
                    .rows_matched,
                3,
                "{k}"
            );
        }
        assert_eq!(
            scan(base, all, vec![attr("attempt", Op::Gte, Value::Int(1))])
                .stats
                .rows_matched,
            2
        );
        // A bytes attribute is not filterable in V0. No rows, and no panic on
        // the way to deciding that.
        assert_eq!(
            scan(
                base,
                all,
                vec![attr("payload", Op::Eq, Value::Str("dead".into()))]
            )
            .stats
            .rows_matched,
            0
        );
        // That one never opened the block — `attr_probes` answered it from the
        // filter. `contains` is not probed, so the same term on the same
        // attribute reaches the row scan, which is where the decision that a
        // bytes value is not comparable actually has to be made.
        let r = scan(
            base,
            all,
            vec![attr("payload", Op::Contains, Value::Str("dead".into()))],
        );
        assert_eq!(r.stats.blocks_scanned, 1, "no probe, so the block is read");
        assert_eq!(r.stats.rows_matched, 0);

        let row = scan(base, all, vec![]).json;
        // Record level beats resource level for the same key.
        assert!(row.contains(r#""deploy.env":"canary""#), "{row}");
        assert!(!row.contains("prod"), "{row}");
        assert!(row.contains(r#""service.name":"checkout""#), "{row}");
        assert!(row.contains(r#""payload":"dead""#), "{row}");
        // Slice and Map round-trip through `ser` as the JSON they were, because
        // a value nothing can read back is a value that was not stored.
        assert!(row.contains(r#""tags":["a","7"]"#), "{row}");
        assert!(
            row.contains(r#""gen_ai.input.messages":{"role":"user"}"#),
            "{row}"
        );
        // Same for a non-string body, and for the field that says the record is
        // an OTel Event rather than a log line.
        assert!(
            row.contains(r#""body_ser":{"msg":"structured body"}"#),
            "{row}"
        );
        assert!(row.contains(r#""event_name":"user.login""#), "{row}");
        // Empty on the wire is absent in the row, not an empty string.
        assert_eq!(row.matches("event_name").count(), 1, "{row}");
        // An attribute that arrived with no value is a key with a `null`, not a
        // key that vanished: the exporter sent it, so the record has it.
        assert!(row.contains(r#""trace.hint":null"#), "{row}");

        // A block written before `event_name` was added to the schema. Once a
        // block is on disk a schema change is not revertible, so the claim that
        // the reader is `column_by_name` all the way down has to be checked
        // rather than asserted: dropping the column reproduces the old writer
        // exactly, and the only difference in the answer must be the field.
        let table = bref.dir.join("logs.arrow");
        let mut old = block::open_table_opt(&table).unwrap().unwrap().batches[0].clone();
        old.remove_column(old.schema().index_of("event_name").unwrap());
        // Staged and renamed rather than truncated in place: a mapping over a
        // truncated file is a SIGBUS, which is why `write_table` says so.
        let staged = bref.dir.join("logs.arrow.new");
        crate::block::write_table(&staged, &old).unwrap();
        std::fs::rename(&staged, &table).unwrap();
        let r = scan(base, all, vec![]);
        assert_eq!(r.stats.rows_matched, 3, "{}", r.json);
        assert!(!r.json.contains("event_name"), "{}", r.json);
        assert!(
            r.json.contains(r#""body_ser":{"msg":"structured body"}"#),
            "{}",
            r.json
        );

        // One column deeper, and the one with no fallback: the time column is
        // what `select` filters on before it looks at anything else. A root
        // table without it selects nothing, so a table this build cannot make
        // sense of costs its own rows and no more.
        let mut old = block::open_table_opt(&table).unwrap().unwrap().batches[0].clone();
        old.remove_column(old.schema().index_of("time_unix_nano").unwrap());
        crate::block::write_table(&staged, &old).unwrap();
        std::fs::rename(&staged, &table).unwrap();
        let r = scan(base, all, vec![]);
        assert_eq!(r.json, "[]");
        assert_eq!(r.stats.rows_matched, 0);

        // Retention can delete a block between the directory listing and the
        // read. The block still counts as present and simply contributes
        // nothing, rather than failing the query.
        std::fs::remove_file(bref.dir.join("logs.arrow")).unwrap();
        let r = scan(base, all, vec![]);
        assert_eq!((r.stats.blocks_total, r.stats.blocks_scanned), (1, 0));
        assert_eq!(r.json, "[]");
    }

    /// The fourth attribute level: a span's own events and links.
    ///
    /// This is not a completeness exercise. The OTel API puts the fields anyone
    /// actually searches for on an *event* — `recordException` writes
    /// `exception.type`, `exception.message` and `exception.stacktrace` onto one
    /// — so "which spans threw a NullPointerException" is a child-level filter
    /// and there is no span-level equivalent to fall back on. The row came back
    /// with the value visible under `events[].attributes` while a filter for the
    /// same key returned nothing, which is the worst shape a search can have.
    #[test]
    fn an_attribute_on_a_span_event_or_link_selects_the_span_it_hangs_off() {
        use mira_proto::collector::trace::v1::ExportTraceServiceRequest;
        use mira_proto::common::v1::any_value::Value as Av;
        use mira_proto::common::v1::{AnyValue, InstrumentationScope, KeyValue};
        use mira_proto::resource::v1::Resource;
        use mira_proto::trace::v1::span::{Event, Link};
        use mira_proto::trace::v1::{ResourceSpans, ScopeSpans, Span};

        let kv = |k: &str, v: &str| KeyValue {
            key: k.into(),
            value: Some(AnyValue {
                value: Some(Av::StringValue(v.into())),
            }),
        };
        let event = |name: &str, attrs: Vec<KeyValue>| Event {
            time_unix_nano: 10_050,
            name: name.into(),
            attributes: attrs,
            ..Default::default()
        };
        let span = |i: u64, events: Vec<Event>, links: Vec<Link>| Span {
            trace_id: vec![1u8; 16].into(),
            span_id: vec![i as u8 + 1; 8].into(),
            name: format!("span {i}"),
            start_time_unix_nano: 10_000 + i,
            end_time_unix_nano: 10_100 + i,
            attributes: vec![kv("http.method", "GET")],
            events,
            links,
            ..Default::default()
        };

        // Span 0 carries two events and span 1 carries the throw, so the
        // throwing event's own id is 2 while the span it belongs to is row 1.
        // An implementation that treats a child id as a root row number — the
        // obvious mistake, since every other join in this file is exactly that —
        // answers "span 2" here and passes any test where each span has one
        // event.
        let req = ExportTraceServiceRequest {
            resource_spans: vec![ResourceSpans {
                resource: Some(Resource {
                    attributes: vec![kv("service.name", "checkout")],
                    ..Default::default()
                }),
                scope_spans: vec![ScopeSpans {
                    scope: Some(InstrumentationScope {
                        name: "mira.test".into(),
                        ..Default::default()
                    }),
                    spans: vec![
                        span(
                            0,
                            vec![
                                event("cache.miss", vec![kv("cache.key", "cart:7")]),
                                event("retrying", vec![]),
                            ],
                            vec![],
                        ),
                        span(
                            1,
                            vec![event(
                                "exception",
                                vec![kv("exception.type", "NullPointerException")],
                            )],
                            vec![],
                        ),
                        span(
                            2,
                            vec![],
                            vec![Link {
                                trace_id: vec![9u8; 16].into(),
                                span_id: vec![8u8; 8].into(),
                                attributes: vec![kv("link.kind", "follows_from")],
                                ..Default::default()
                            }],
                        ),
                    ],
                    ..Default::default()
                }],
                ..Default::default()
            }],
        };

        let dir = std::env::temp_dir().join(format!("mira-child-attr-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let mut b = crate::traces::TracesBuilder::new();
        b.append_request(&req).unwrap();
        let sealed = b.finish().unwrap();
        let bref = crate::block::publish(&dir, "traces", crate::block::node_id("a"), 0, 0, &sealed)
            .unwrap();

        let find = |key: &str, value: &str| {
            search(
                &dir,
                &Search {
                    signal: Signal::Traces,
                    from: 0,
                    to: i64::MAX,
                    terms: vec![Term {
                        target: Target::Attr(key.into()),
                        op: Op::Eq,
                        value: Value::Str(value.into()),
                    }],
                    limit: 10,
                    after: None,
                },
            )
            .unwrap()
        };

        // The case the bug report named, and the two either side of it: an
        // event attribute on a span that has siblings, an event attribute on a
        // span whose events are not the first in the block, and a *link*
        // attribute, which travels the same path through a different table.
        for (key, value, want) in [
            ("exception.type", "NullPointerException", "span 1"),
            ("cache.key", "cart:7", "span 0"),
            ("link.kind", "follows_from", "span 2"),
        ] {
            let r = find(key, value);
            assert_eq!(r.stats.rows_matched, 1, "{key}: {}", r.json);
            assert!(
                r.json.contains(&format!(r#""name":"{want}""#)),
                "{}",
                r.json
            );
        }

        // The other three levels still answer, and a value nothing carries at
        // any of the four still matches nothing — the union must widen the
        // search, not defeat it.
        assert_eq!(find("http.method", "GET").stats.rows_matched, 3);
        assert_eq!(find("service.name", "checkout").stats.rows_matched, 3);
        assert_eq!(find("exception.type", "IOError").stats.rows_matched, 0);

        // And the value is still rendered where it was found, so the filter and
        // the row now agree about where an event attribute lives.
        let r = find("exception.type", "NullPointerException");
        assert!(
            r.json
                .contains(r#""attributes":{"exception.type":"NullPointerException"}"#),
            "{}",
            r.json
        );

        // Rewrite one table without one column, the way a writer that predates
        // it left the block. Staged and renamed rather than edited in place: a
        // mapping over a file being shortened is a SIGBUS, not an error.
        let strip = |table: &str, col: &str| {
            let path = bref.dir.join(format!("{table}.arrow"));
            let mut b = block::open_table_opt(&path).unwrap().unwrap().batches[0].clone();
            b.remove_column(b.schema().index_of(col).unwrap());
            let staged = bref.dir.join(format!("{table}.staged"));
            crate::block::write_table(&staged, &b).unwrap();
            std::fs::rename(&staged, &path).unwrap();
        };

        // The last hop above is an index built from `span_events.parent_id`. Without
        // that column every lookup has to miss, which is the answer a scan of
        // the table would give — and emphatically not row zero, which is what
        // an index defaulting to the dense-id trick would hand back.
        strip("span_events", "parent_id");
        let r = find("exception.type", "NullPointerException");
        assert_eq!(r.stats.rows_matched, 0, "{}", r.json);
        assert!(
            !r.json.contains("span"),
            "an unjoinable event picked a span"
        );
        // The levels that do not go through that index are untouched, so this
        // is one broken join and not a broken block.
        assert_eq!(find("link.kind", "follows_from").stats.rows_matched, 1);
        assert_eq!(find("http.method", "GET").stats.rows_matched, 3);

        // The root table without the column every scan starts from. No rows —
        // not the whole block unfiltered, which is what a scan that treated a
        // missing time column as "no time filter" would return.
        strip("spans", "start_time_unix_nano");
        let r = find("http.method", "GET");
        assert_eq!(r.stats.blocks_scanned, 1, "the block was still opened");
        assert_eq!(r.json, "[]");
        assert_eq!(r.stats.rows_matched, 0);

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Empty, bytes, array and map attributes come back in the row and match no
    /// filter, and those two halves have to stay in step.
    ///
    /// `gen_ai.input.messages` is a kvlist and is the first-class case in
    /// ARCHITECTURE section 1, so these are not exotic types nobody stores. V0 cannot
    /// order or substring-match them, and the only safe answer to a filter on
    /// one is *no rows*: matching everything would silently widen a query, and
    /// the row still carries the value so a caller can see what is there.
    ///
    /// `Op::Contains` rather than `Op::Eq` on purpose — an `Eq` term builds an
    /// attribute Bloom probe that prunes the block before the comparison is
    /// reached, so it never proves what the comparison does.
    #[test]
    fn an_attribute_v0_cannot_filter_still_renders_and_still_matches_nothing() {
        use mira_proto::collector::logs::v1::ExportLogsServiceRequest;
        use mira_proto::common::v1::any_value::Value as Av;
        use mira_proto::common::v1::{AnyValue, ArrayValue, KeyValue, KeyValueList};
        use mira_proto::logs::v1::{LogRecord, ResourceLogs, ScopeLogs};

        let kv = |k: &str, v: Option<Av>| KeyValue {
            key: k.into(),
            value: v.map(|v| AnyValue { value: Some(v) }),
        };
        let req = ExportLogsServiceRequest {
            resource_logs: vec![ResourceLogs {
                scope_logs: vec![ScopeLogs {
                    log_records: vec![LogRecord {
                        time_unix_nano: 1_000,
                        attributes: vec![
                            // No value at all on the wire. Not an empty string,
                            // and not the key being absent.
                            kv("note", None),
                            kv("payload", Some(Av::BytesValue(vec![0xde, 0xad].into()))),
                            kv(
                                "tags",
                                Some(Av::ArrayValue(ArrayValue {
                                    values: vec![AnyValue {
                                        value: Some(Av::StringValue("a".into())),
                                    }],
                                })),
                            ),
                            kv(
                                "gen_ai.input.messages",
                                Some(Av::KvlistValue(KeyValueList {
                                    values: vec![kv("role", Some(Av::StringValue("user".into())))],
                                })),
                            ),
                            // The control: a type V0 *can* filter, carrying a
                            // value every probe below is a substring of.
                            kv("level", Some(Av::StringValue("a dead role".into()))),
                        ],
                        ..Default::default()
                    }],
                    ..Default::default()
                }],
                ..Default::default()
            }],
        };

        let dir = std::env::temp_dir().join(format!("mira-unfilterable-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let mut b = crate::logs::LogsBuilder::new();
        b.append_request(&req).unwrap();
        let sealed = b.finish().unwrap();
        crate::block::publish(&dir, "logs", crate::block::node_id("a"), 0, 0, &sealed).unwrap();

        let scan = |terms: Vec<Term>| {
            search(
                &dir,
                &Search {
                    signal: Signal::Logs,
                    from: 0,
                    to: i64::MAX,
                    terms,
                    limit: 10,
                    after: None,
                },
            )
            .unwrap()
        };
        let contains = |k: &str, v: &str| {
            vec![Term {
                target: Target::Attr(k.into()),
                op: Op::Contains,
                value: Value::Str(v.into()),
            }]
        };

        // The same operator and a probe that really is inside the stored value,
        // so a zero here is the attribute's *type* and not the operator or the
        // needle.
        assert_eq!(scan(contains("level", "dead")).stats.rows_matched, 1);
        for (key, needle) in [
            ("note", ""),
            ("payload", "dead"),
            ("tags", "a"),
            ("gen_ai.input.messages", "role"),
        ] {
            let r = scan(contains(key, needle));
            assert_eq!(r.stats.rows_matched, 0, "{key}: {}", r.json);
            // The block was opened and the comparison really ran: a zero that
            // came from the sidecar pruning the block would prove nothing about
            // what the comparison decides.
            assert_eq!(r.stats.blocks_scanned, 1, "{key}");
        }

        // And every one of them is still in the row. An empty attribute renders
        // as `null`, which is what "the key arrived with no value" means — the
        // key disappearing would be a different statement.
        let row = scan(Vec::new()).json;
        assert!(row.contains(r#""note":null"#), "{row}");
        assert!(row.contains(r#""payload":"dead""#), "{row}");
        assert!(row.contains(r#""tags":["a"]"#), "{row}");
        assert!(
            row.contains(r#""gen_ai.input.messages":{"role":"user"}"#),
            "{row}"
        );

        // The index that turns a child id into a root row, on a table that has
        // neither column: empty, so every lookup misses. The alternative — a
        // vector sized off whichever column *is* present — would join rows to
        // whatever happened to sit at that slot.
        let ids = Arc::new(UInt32Array::from(vec![0u32, 1])) as Arc<dyn Array>;
        let only_ids = RecordBatch::try_from_iter(vec![("id", ids.clone())]).unwrap();
        let only_parents = RecordBatch::try_from_iter(vec![("parent_id", ids)]).unwrap();
        assert!(index_parent_of_id(&only_ids).is_empty());
        assert!(index_parent_of_id(&only_parents).is_empty());

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `rows_matched` is a property of the query, not of the page.
    ///
    /// The UI prints it next to the page and the MCP tool hands it to a model as
    /// "how much there was", so a number that counts rows an earlier page
    /// already delivered reads as a result set that grows as you page through
    /// it.
    #[test]
    fn rows_matched_counts_what_is_left_behind_the_cursor() {
        use mira_proto::collector::logs::v1::ExportLogsServiceRequest;
        use mira_proto::common::v1::any_value::Value as Av;
        use mira_proto::common::v1::{AnyValue, InstrumentationScope};
        use mira_proto::logs::v1::{LogRecord, ResourceLogs, ScopeLogs};

        let base = 1_700_000_000_000_000_000u64;
        let req = ExportLogsServiceRequest {
            resource_logs: vec![ResourceLogs {
                scope_logs: vec![ScopeLogs {
                    scope: Some(InstrumentationScope {
                        name: "mira.test".into(),
                        ..Default::default()
                    }),
                    log_records: (0..5)
                        .map(|i| LogRecord {
                            time_unix_nano: base + i * 1_000_000_000,
                            body: Some(AnyValue {
                                value: Some(Av::StringValue(format!("line {i}"))),
                            }),
                            ..Default::default()
                        })
                        .collect(),
                    ..Default::default()
                }],
                ..Default::default()
            }],
        };

        let dir = std::env::temp_dir().join(format!("mira-paged-count-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let mut b = crate::logs::LogsBuilder::new();
        b.append_request(&req).unwrap();
        let sealed = b.finish().unwrap();
        crate::block::publish(&dir, "logs", crate::block::node_id("a"), 0, 0, &sealed).unwrap();

        let page = |after: Option<Cursor>| {
            search(
                &dir,
                &Search {
                    signal: Signal::Logs,
                    from: 0,
                    to: i64::MAX,
                    terms: Vec::new(),
                    limit: 2,
                    after,
                },
            )
            .unwrap()
        };

        // Five rows, two per page. Each page reports what is still ahead of the
        // reader including the rows it is holding, so the sequence falls by the
        // page size and the last page is exact rather than being the whole
        // block over again.
        let mut cursor = None;
        for want in [5, 3, 1] {
            let r = page(cursor);
            assert_eq!(r.stats.rows_matched, want, "{}", r.json);
            cursor = r.next;
        }
        // A short page is the last page, so there is nothing to ask for again.
        assert_eq!(cursor, None);

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// [`keep`] is three loops where there used to be one boxed closure, and
    /// only one of them is reachable from a full selection — so the thing to
    /// pin is that all three answer the same question.
    ///
    /// The contiguous branch-free loop is taken when the column has no nulls
    /// *and* nothing has narrowed the selection yet; a second term on the same
    /// block, or any null at all, gathers through `sel` instead. Every
    /// column-type case above enters through the first of those, which is
    /// exactly why the other two need their own test.
    #[test]
    fn narrowing_a_selection_agrees_with_scanning_one_whole() {
        let plain = Int64Array::from(vec![1, 2, 3, 4, 5, 6]);
        let holed = Int64Array::from(vec![Some(1), None, Some(3), Some(4), None, Some(6)]);
        let gt2 = |sel: &mut Vec<u32>, col: &dyn Array| {
            assert!(field_filter(sel, col, Op::Gt, &Value::Int(2)));
        };

        let mut sel: Vec<u32> = (0..6).collect();
        gt2(&mut sel, &plain);
        assert_eq!(sel, [2, 3, 4, 5], "whole block, no nulls");

        let mut sel = vec![1, 3, 5];
        gt2(&mut sel, &plain);
        assert_eq!(sel, [3, 5], "already narrowed by an earlier term");

        let mut sel: Vec<u32> = (0..6).collect();
        gt2(&mut sel, &holed);
        assert_eq!(sel, [2, 3, 5], "a null is not a match, whole block");

        let mut sel = vec![1, 3, 4];
        gt2(&mut sel, &holed);
        assert_eq!(sel, [3], "a null is not a match, narrowed");

        // Nothing selected stays nothing selected — the length test that picks
        // the fast path must not read an empty selection as a full one.
        let mut sel: Vec<u32> = Vec::new();
        gt2(&mut sel, &plain);
        assert!(sel.is_empty());

        // A refusal writes nothing: `select` clears the selection itself, and
        // a half-filtered one left behind would be an answer, not an empty
        // result.
        let mut sel: Vec<u32> = (0..6).collect();
        assert!(!field_filter(
            &mut sel,
            &plain,
            Op::Eq,
            &Value::Str("beta".into())
        ));
        assert_eq!(sel.len(), 6);

        // And the same both ways round: a number against a dictionary column
        // has no encoding to look up, so it is a refusal rather than a miss.
        // `severity_text: 2` must not quietly mean "no logs", which is what a
        // plain `false` per row would have made it.
        let mut d = StringDictionaryBuilder::<UInt16Type>::new();
        d.append_value("warn");
        d.append_value("info");
        let dict = d.finish();
        let mut sel: Vec<u32> = (0..2).collect();
        assert!(!field_filter(&mut sel, &dict, Op::Eq, &Value::Int(2)));
        assert_eq!(sel.len(), 2);
    }

    /// Rendering a row binary-searches its attributes, which is only right
    /// because every builder appends parents in ascending order.
    ///
    /// A table that is not in that order has to keep working, and the reason is
    /// the failure mode rather than the likelihood: a binary search over
    /// unsorted parents does not fail, it returns some of the rows, and an
    /// attribute quietly missing from a response is what nobody would notice.
    #[test]
    fn an_attribute_table_out_of_parent_order_falls_back_to_the_scan() {
        let table = |parents: &'static [u32]| {
            let mut b = crate::attrs::AttrsBuilder::new("t.key");
            for &p in parents {
                b.append(p, "k", None).unwrap();
            }
            let a = Attrs::new(b.finish().unwrap());
            // The slice `run` is handed below is the caller's own, so pin it to
            // the column the builder actually wrote before trusting it.
            assert_eq!(Attrs::parents(&a.rows), Some(parents));
            (a, parents)
        };

        let (ordered, p) = table(&[0, 0, 1, 3, 3]);
        assert!(ordered.ordered);
        assert_eq!(ordered.run(p, 0), 0..2);
        assert_eq!(ordered.run(p, 1), 2..3);
        // A parent with no attributes of its own, and one past the end: both
        // are empty runs rather than a panic or somebody else's rows.
        assert_eq!(ordered.run(p, 2), 3..3);
        assert_eq!(ordered.run(p, 9), 5..5);

        let (jumbled, p) = table(&[3, 0, 1, 0, 3]);
        assert!(!jumbled.ordered);
        // The whole table, which the caller still filters row by row — the
        // scan this replaced, reached only by a block nothing here writes.
        assert_eq!(jumbled.run(p, 0), 0..5);

        // An empty table is ordered by vacuous truth and has no rows for
        // anyone, which is the same answer either way.
        let (empty, p) = table(&[]);
        assert_eq!(empty.run(p, 0), 0..0);
    }

    /// One log block of `rows` records, through the real encoder, held in
    /// memory as an [`Open`] snapshot so a scan of it touches no file.
    fn bench_block(rows: usize) -> Open {
        use mira_proto::collector::logs::v1::ExportLogsServiceRequest;
        use mira_proto::common::v1::{InstrumentationScope, KeyValue, any_value};
        use mira_proto::logs::v1::{LogRecord, ResourceLogs, ScopeLogs};
        use mira_proto::resource::v1::Resource;

        let sv = |k: &str, v: &str| KeyValue {
            key: k.into(),
            value: Some(AnyValue {
                value: Some(any_value::Value::StringValue(v.into())),
            }),
        };
        const SEV: [(i32, &str); 4] = [(5, "DEBUG"), (9, "INFO"), (13, "WARN"), (17, "ERROR")];
        const ROUTES: [&str; 6] = [
            "/checkout",
            "/cart",
            "/search",
            "/api/v1/orders",
            "/healthz",
            "/metrics",
        ];

        let mut b = crate::logs::LogsBuilder::new();
        let mut done = 0usize;
        while done < rows {
            let take = (rows - done).min(8192);
            let log_records = (0..take)
                .map(|k| {
                    let i = done + k;
                    LogRecord {
                        time_unix_nano: 1_700_000_000_000_000_000 + i as u64 * 1_000,
                        severity_number: SEV[i % 4].0,
                        severity_text: SEV[i % 4].1.into(),
                        body: Some(AnyValue {
                            value: Some(any_value::Value::StringValue(format!(
                                "GET /api/v1/orders/{i} -> 200 in {}ms for tenant t{}",
                                i % 997,
                                i % 64
                            ))),
                        }),
                        attributes: vec![
                            sv("http.route", ROUTES[i % ROUTES.len()]),
                            KeyValue {
                                key: "http.status_code".into(),
                                value: Some(AnyValue {
                                    value: Some(any_value::Value::IntValue(
                                        200 + (i % 5) as i64 * 100,
                                    )),
                                }),
                            },
                        ],
                        ..Default::default()
                    }
                })
                .collect();
            b.append_request(&ExportLogsServiceRequest {
                resource_logs: vec![ResourceLogs {
                    resource: Some(Resource {
                        attributes: vec![sv("service.name", "checkout"), sv("env", "prod")],
                        ..Default::default()
                    }),
                    scope_logs: vec![ScopeLogs {
                        scope: Some(InstrumentationScope {
                            name: "http".into(),
                            attributes: vec![sv("tier", "gold")],
                            ..Default::default()
                        }),
                        log_records,
                        ..Default::default()
                    }],
                    ..Default::default()
                }],
            })
            .unwrap();
            done += take;
        }
        Open {
            node: 1,
            seq: 1,
            sealed: b.finish().unwrap(),
        }
    }

    /// What one row of one block costs to filter, per predicate kind, with no
    /// paging and no CRC in the number.
    ///
    /// Scaled rather than `#[ignore]`d: the default 4096 rows run in the normal
    /// suite, so every line here is a covered correctness check on the six
    /// predicate shapes, and the same code is the measurement at a real row
    /// count.
    ///
    /// ```sh
    /// MIRA_BENCH_ROWS=2000000 cargo test --release -p mira-core \
    ///     --lib scan_cost_per_row -- --nocapture
    /// ```
    #[test]
    fn scan_cost_per_row() {
        let rows: usize = std::env::var("MIRA_BENCH_ROWS")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(4_096);
        let open = bench_block(rows);
        let (lo, hi) = (open.sealed.min_ts, open.sealed.max_ts);
        let src = Src::open(&open);
        let b = Block::open(&src, Signal::Logs).unwrap().unwrap();
        let n = b.root.num_rows();
        assert_eq!(n, rows);

        let all = |terms: Vec<Term>| Search {
            signal: Signal::Logs,
            from: 0,
            to: i64::MAX,
            terms,
            limit: 100,
            after: None,
        };
        let field = |name: &str, op, value| {
            all(vec![Term {
                target: Target::Field(name.into()),
                op,
                value,
            }])
        };
        let attr = |key: &str, op, value| {
            all(vec![Term {
                target: Target::Attr(key.into()),
                op,
                value,
            }])
        };
        let cases: Vec<(&str, Search)> = vec![
            ("no term (whole block)", all(Vec::new())),
            (
                "time range (per-row)",
                Search {
                    from: lo + (hi - lo) / 4,
                    to: hi,
                    ..all(Vec::new())
                },
            ),
            (
                "int field  severity_number gte",
                field("severity_number", Op::Gte, Value::Int(9)),
            ),
            (
                "utf8 field body contains",
                field("body", Op::Contains, Value::Str("tenant t7".into())),
            ),
            (
                "dict field severity_text eq",
                field("severity_text", Op::Eq, Value::Str("ERROR".into())),
            ),
            (
                "attr record http.route eq",
                attr("http.route", Op::Eq, Value::Str("/checkout".into())),
            ),
            (
                "attr record http.status_code gte",
                attr("http.status_code", Op::Gte, Value::Int(500)),
            ),
            (
                "attr resource service.name eq",
                attr("service.name", Op::Eq, Value::Str("checkout".into())),
            ),
        ];

        let reps = if rows > 100_000 { 5 } else { 1 };
        for (name, s) in &cases {
            let hit = b.select(s, &src).len();
            let t = std::time::Instant::now();
            for _ in 0..reps {
                std::hint::black_box(b.select(s, &src));
            }
            let ns = t.elapsed().as_nanos() as f64 / (reps * n) as f64;
            println!("{name:34} {ns:8.3} ns/row  {hit} of {n}");
            assert!(hit > 0, "{name} matched nothing");
        }

        // And the same block through the whole read path, published and warm,
        // so `select` can be read as a fraction of what a query actually costs.
        // Everything outside it — the mmap's minor faults, the CRC32 of every
        // table body, the dictionary scan, `Block::open`'s two child indexes,
        // the cursor filter and the JSON of `limit` rows — is the "no term"
        // line, and that is the claim in docs/architecture.md section 11 that
        // the per-block cost is paging rather than scanning.
        let dir = std::env::temp_dir().join(format!("mira-scan-cost-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        crate::block::publish(&dir, "logs", 1, 1, 0, &open.sealed).unwrap();
        let one = (
            "no term, limit 1",
            Search {
                limit: 1,
                ..all(Vec::new())
            },
        );
        for (name, s) in [&cases[0], &one, &cases[3], &cases[5]] {
            let _ = search(&dir, s).unwrap();
            let t = std::time::Instant::now();
            for _ in 0..reps {
                std::hint::black_box(search(&dir, s).unwrap());
            }
            let ns = t.elapsed().as_nanos() as f64 / (reps * n) as f64;
            println!("  full search: {name:21} {ns:8.3} ns/row");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}
