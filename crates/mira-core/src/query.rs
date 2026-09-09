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
//! Nothing here reads the open, unsealed block, and it does not need to:
//! ingest acknowledges an export only after the block containing it has been
//! fsynced and renamed into place. Read-your-writes therefore falls out of the
//! durability rule rather than needing a second, in-memory read path.

use std::path::Path;

use arrow_array::cast::AsArray;
use arrow_array::types::{
    Float64Type, Int32Type, Int64Type, TimestampNanosecondType, UInt8Type, UInt16Type, UInt32Type,
    UInt64Type,
};
use arrow_array::{Array, FixedSizeBinaryArray, RecordBatch, StringArray};
use arrow_schema::DataType;

use crate::block::{self, BlockRef};
use crate::error::Result;
use crate::json::Json;
use crate::schema::AttrType;

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
    /// An attribute key, looked up at all three levels — record, resource and
    /// scope — and unioned.
    ///
    /// Whether `service.name` is a resource attribute is a detail of whoever
    /// configured the SDK, and users do not know it. Searching all three is what
    /// every usable tracing UI does; the star schema makes it three scans of
    /// tables that are tiny next to the root.
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
struct Hit {
    ts: i64,
    block: usize,
    row: u32,
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
    pub rows_matched: usize,
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
    let mut refs = block::scan(root, q.signal.dir())?;
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

    let needle = trace_needle(q);
    let probes = attr_probes(q);
    // With no cursor, every row is after the start — which is the key that
    // sorts ahead of all of them.
    let after = q.after.map_or(
        std::cmp::Reverse((i64::MAX, u32::MAX, u64::MAX, u32::MAX)),
        |c| c.key(),
    );
    let mut hits: Vec<Hit> = Vec::new();
    let mut open: Vec<Option<Block>> = Vec::with_capacity(refs.len());

    for (i, bref) in refs.iter().enumerate() {
        // The early exit. With `limit` hits held, a block whose newest row
        // predates the oldest hit cannot contribute — and since blocks are
        // ordered by max_ts descending, neither can any block after it. For
        // "the last 100 records", which is what every session opens with, this
        // reads one block no matter how many are on disk.
        if hits.len() >= q.limit && hits.last().is_some_and(|w| bref.max_ts < w.ts) {
            break;
        }

        // The sidecar filters (§7.4). Both cover the case the block name cannot:
        // a query with no useful time bound, either because it names a trace or
        // because it names an attribute value that is rare or absent. Without
        // them the scan runs to the end of retention to prove a negative. A
        // 20 KB read is two orders of magnitude cheaper than the block it skips,
        // and every damaged or missing filter reads as "scan me".
        if let Some(id) = &needle
            && let Ok(f) = std::fs::read(bref.dir.join(crate::bloom::TRACE_IDX))
            && !crate::bloom::may_contain(&f, id)
        {
            open.push(None);
            continue;
        }
        if !probes.is_empty()
            && let Ok(bytes) = std::fs::read(bref.dir.join(crate::bloom::ATTR_IDX))
            && let Some(f) = crate::bloom::Filter::open(&bytes)
            && !probes.iter().all(|p| p.maybe(&f))
        {
            open.push(None);
            continue;
        }

        let Some(b) = Block::open(bref, q.signal)? else {
            open.push(None);
            continue;
        };
        stats.blocks_scanned += 1;
        stats.rows_scanned += b.root.num_rows();

        let sel = b.select(q, bref);
        // Counted before the cursor is applied: `rows_matched` answers "how
        // broad is my filter", which is a property of the query and not of
        // which page of it is being read.
        stats.rows_matched += sel.len();
        if let Some(time) = b.time() {
            hits.extend(sel.iter().filter_map(|&row| {
                let h = Hit {
                    ts: time[row as usize],
                    block: i,
                    row,
                };
                (cursor(bref, &h).key() > after).then_some(h)
            }));
        }
        open.push(Some(b));

        // Trim as we go, so memory is bounded by `limit` rather than by the
        // match count, which is not bounded by anything.
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

/// The sort key of one hit, which is also the cursor a caller pages on.
fn cursor(bref: &block::BlockRef, h: &Hit) -> Cursor {
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
struct Block {
    signal: Signal,
    root: RecordBatch,
    /// Record-level attributes; `parent_id` indexes `root` directly.
    attrs: Option<RecordBatch>,
    resource_attrs: Option<RecordBatch>,
    scope_attrs: Option<RecordBatch>,
    /// Rows that hang off a root row rather than being one: a span's events and
    /// its links. Empty for logs.
    children: Vec<Child>,
}

/// A child table and the attribute table keyed by its `id`.
struct Child {
    /// What the array is called in the emitted row.
    label: &'static str,
    rows: RecordBatch,
    attrs: Option<RecordBatch>,
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
    fn open(bref: &BlockRef, signal: Signal) -> Result<Option<Block>> {
        let load = |name: &str| -> Result<Option<RecordBatch>> {
            let path = bref.dir.join(format!("{name}.arrow"));
            // `write_table` emits exactly one record batch per file, so row
            // numbers are unambiguous and there is never a second batch to
            // stitch.
            Ok(block::open_table_opt(&path)?.and_then(|t| t.batches.first().cloned()))
        };

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
                    rows,
                    attrs: load(attrs)?,
                });
            }
        }
        Ok(Some(Block {
            signal,
            attrs: load(signal.attrs())?,
            resource_attrs: load("resource_attrs")?,
            scope_attrs: load("scope_attrs")?,
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
    fn select(&self, q: &Search, bref: &BlockRef) -> Vec<u32> {
        let n = self.root.num_rows();
        let Some(time) = self.time() else {
            return Vec::new();
        };

        // Time first: cheapest filter, and on a block the query only partly
        // covers, usually the most selective. When the block sits wholly inside
        // the window the comparison is skipped entirely — the directory name
        // already proved it, which is the point of putting the range there.
        let mut sel: Vec<u32> = if q.from <= bref.min_ts && q.to >= bref.max_ts {
            (0..n as u32).collect()
        } else {
            (0..n as u32)
                .filter(|&i| (q.from..=q.to).contains(&time[i as usize]))
                .collect()
        };

        for term in &q.terms {
            if sel.is_empty() {
                break;
            }
            match &term.target {
                Target::Field(name) => {
                    let pred = self
                        .root
                        .column_by_name(name)
                        .and_then(|c| field_pred(c.as_ref(), term.op, &term.value));
                    match pred {
                        Some(p) => sel.retain(|&i| p(i)),
                        // Unknown column, or a value that cannot be compared
                        // against this column's type at all.
                        None => sel.clear(),
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

    /// A bitmap over root rows: does this record carry `key op value` at any of
    /// the three attribute levels?
    fn attr_rows(&self, key: &str, op: Op, value: &Value, n: usize) -> Vec<bool> {
        let mut out = vec![false; n];

        // Record level: parent_id *is* the root row number, so this is a store,
        // not a join. That is what rebasing ids at ingest bought.
        if let Some(a) = &self.attrs {
            for pid in attr_parents(a, key, op, value) {
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
            let ids = attr_parents(a, key, op, value);
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
    /// ponytail: a linear pass over the child table per emitted row. Bounded by
    /// `limit` root rows, and the alternative — a parent_id→rows index built per
    /// block — costs more than it saves until `limit` is in the thousands.
    fn emit_children(&self, j: &mut Json, c: &Child, row: u32) {
        let Some(parents) = c.rows.column_by_name("parent_id") else {
            return;
        };
        let parents = parents.as_primitive::<UInt32Type>().values();
        let hits: Vec<usize> = (0..c.rows.num_rows())
            .filter(|&r| parents[r] == row)
            .collect();
        // No key at all rather than an empty array: most spans have neither
        // events nor links, and two empty arrays per span is most of the
        // response.
        if hits.is_empty() {
            return;
        }
        j.key(c.label);
        j.arr(|j| {
            for r in hits {
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
        emit_value(j, col.as_ref(), row as usize);
    }
}

/// One `{...}` merging the attributes of several levels, most specific last.
fn emit_attrs(j: &mut Json, levels: &[(&Option<RecordBatch>, Option<u32>)]) {
    j.obj(|j| {
        let mut merged: Vec<(&str, &RecordBatch, usize)> = Vec::new();
        for &(table, parent) in levels {
            let (Some(a), Some(parent)) = (table, parent) else {
                continue;
            };
            let parents = a.column(0).as_primitive::<UInt32Type>().values();
            for r in (0..a.num_rows()).filter(|&r| parents[r] == parent) {
                merged.push((attr_key(a, r), a, r));
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
    let parents = a.column(0).as_primitive::<UInt32Type>();
    let types = a.column(2).as_primitive::<UInt8Type>();

    (0..a.num_rows())
        .filter(|&i| codes[i] == code && attr_matches(a, types.value(i), i, op, value))
        .map(|i| parents.value(i))
        .collect()
}

pub(crate) fn attr_key(a: &RecordBatch, row: usize) -> &str {
    let d = a.column(1).as_dictionary::<UInt16Type>();
    d.values()
        .as_string::<i32>()
        .value(d.keys().value(row) as usize)
}

/// Compare one attribute row against a query scalar, dispatching on the stored
/// `type` rather than on the query's — the column decides what it is.
fn attr_matches(a: &RecordBatch, ty: u8, row: usize, op: Op, v: &Value) -> bool {
    const STR: u8 = AttrType::Str as u8;
    const INT: u8 = AttrType::Int as u8;
    const DOUBLE: u8 = AttrType::Double as u8;
    const BOOL: u8 = AttrType::Bool as u8;
    match ty {
        STR => {
            let s = a.column(3).as_string::<i32>().value(row);
            match op {
                // Equality against a string column is *defined* as equality
                // with `canon`, because that is the text the block index holds
                // (see [`canon`]). Widening it any further — say, matching the
                // stored string "200.0" against `eq: 200` because both parse to
                // the same number — would make the filter prune away blocks
                // that do contain a match, which is the one failure mode a
                // sidecar is not allowed to have.
                Op::Eq | Op::Ne => op.test_ord(s.cmp(canon(v).as_str())),
                Op::Contains => s.contains(canon(v).as_str()),
                // Ordering is not in the index — `attr_probes` only takes
                // `Op::Eq` — so there is nothing here to disagree with, and a
                // number written as a string can be ordered as the number it
                // is. It has to be: half the SDKs that emit
                // `http.response.status_code` emit it as text, and
                // lexicographically "1000" sorts below "400", so `gte: 400`
                // would otherwise mean something different on each of them.
                _ => match (s.parse::<f64>(), v.as_f64()) {
                    (Ok(x), Some(y)) if !matches!(v, Value::Str(_)) => {
                        x.partial_cmp(&y).is_some_and(|o| op.test_ord(o))
                    }
                    // A quoted query scalar asked for a text comparison and
                    // gets one; so does anything that is not a number.
                    _ => op.test_ord(s.cmp(canon(v).as_str())),
                },
            }
        }
        INT => {
            let x = a.column(4).as_primitive::<Int64Type>().value(row);
            v.as_i64().is_some_and(|y| op.test_ord(x.cmp(&y)))
        }
        DOUBLE => {
            let x = a.column(5).as_primitive::<Float64Type>().value(row);
            v.as_f64()
                .and_then(|y| x.partial_cmp(&y))
                .is_some_and(|o| op.test_ord(o))
        }
        BOOL => {
            let x = a.column(6).as_boolean().value(row);
            v.as_bool().is_some_and(|y| op.test_ord(x.cmp(&y)))
        }
        // Empty, Bytes, Slice and Map are returned in results but not
        // filterable in V0.
        _ => false,
    }
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

/// Build a row predicate for a root-table column, with type dispatch done once
/// instead of per row.
///
/// `None` means the query value cannot be compared against this column at all,
/// which the caller turns into an empty result.
fn field_pred<'a>(col: &'a dyn Array, op: Op, v: &Value) -> Option<Box<dyn Fn(u32) -> bool + 'a>> {
    macro_rules! int_col {
        ($t:ty) => {{
            let a = col.as_primitive::<$t>();
            let target = v.as_i64()?;
            Some(Box::new(move |i: u32| {
                !a.is_null(i as usize) && op.test_ord((a.value(i as usize) as i64).cmp(&target))
            }) as Box<dyn Fn(u32) -> bool + 'a>)
        }};
    }

    match col.data_type() {
        DataType::Timestamp(_, _) => int_col!(TimestampNanosecondType),
        DataType::Int64 => int_col!(Int64Type),
        DataType::Int32 => int_col!(Int32Type),
        DataType::UInt64 => int_col!(UInt64Type),
        DataType::UInt32 => int_col!(UInt32Type),
        DataType::UInt16 => int_col!(UInt16Type),
        DataType::UInt8 => int_col!(UInt8Type),
        DataType::Float64 => {
            let a = col.as_primitive::<Float64Type>();
            let target = v.as_f64()?;
            Some(Box::new(move |i: u32| {
                !a.is_null(i as usize)
                    && a.value(i as usize)
                        .partial_cmp(&target)
                        .is_some_and(|o| op.test_ord(o))
            }))
        }
        DataType::Boolean => {
            let a = col.as_boolean();
            let target = v.as_bool()?;
            Some(Box::new(move |i: u32| {
                !a.is_null(i as usize) && op.test_ord(a.value(i as usize).cmp(&target))
            }))
        }
        DataType::Utf8 => {
            let a = col.as_string::<i32>();
            let target = v.as_str()?.to_owned();
            Some(Box::new(move |i: u32| {
                if a.is_null(i as usize) {
                    return false;
                }
                let s = a.value(i as usize);
                match op {
                    Op::Contains => s.contains(&target),
                    _ => op.test_ord(s.cmp(target.as_str())),
                }
            }))
        }
        DataType::Dictionary(_, _) => {
            let d = col.as_dictionary::<UInt16Type>();
            let values = d.values().as_string::<i32>();
            let needle = v.as_str()?;
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
            let codes = d.keys();
            Some(Box::new(move |i: u32| {
                !codes.is_null(i as usize)
                    && ok
                        .get(codes.value(i as usize) as usize)
                        .copied()
                        .unwrap_or(false)
            }))
        }
        DataType::FixedSizeBinary(_) => {
            // Trace and span ids: hex in the query, bytes on disk. Decoding the
            // needle once beats hex-encoding every row.
            let a = col.as_any().downcast_ref::<FixedSizeBinaryArray>()?;
            let target = unhex(v.as_str()?)?;
            Some(Box::new(move |i: u32| {
                !a.is_null(i as usize) && op.test_ord(a.value(i as usize).cmp(target.as_slice()))
            }))
        }
        _ => None,
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
        // Nanoseconds as an integer. Every other representation either loses
        // precision or picks a timezone on the user's behalf; the UI formats
        // them, which is where that belongs.
        DataType::Timestamp(_, _) => {
            j.i64(col.as_primitive::<TimestampNanosecondType>().value(row))
        }
        DataType::Int64 => j.i64(col.as_primitive::<Int64Type>().value(row)),
        DataType::Int32 => j.i64(col.as_primitive::<Int32Type>().value(row) as i64),
        DataType::UInt64 => j.u64(col.as_primitive::<UInt64Type>().value(row)),
        DataType::UInt32 => j.u64(col.as_primitive::<UInt32Type>().value(row) as u64),
        DataType::UInt16 => j.u64(col.as_primitive::<UInt16Type>().value(row) as u64),
        DataType::UInt8 => j.u64(col.as_primitive::<UInt8Type>().value(row) as u64),
        DataType::Float64 => j.f64(col.as_primitive::<Float64Type>().value(row)),
        DataType::Boolean => j.bool(col.as_boolean().value(row)),
        DataType::Utf8 => j.str(col.as_string::<i32>().value(row)),
        DataType::Binary => j.hex(col.as_binary::<i32>().value(row)),
        DataType::FixedSizeBinary(_) => match col.as_any().downcast_ref::<FixedSizeBinaryArray>() {
            Some(a) => j.hex(a.value(row)),
            None => j.null(),
        },
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
    match a.column(2).as_primitive::<UInt8Type>().value(row) {
        STR => j.str(a.column(3).as_string::<i32>().value(row)),
        // Int attributes go out as JSON numbers even though OTLP allows the
        // full i64 range and a browser reads JSON numbers as f64. A value past
        // 2^53 is a counter, not an id; stringifying every int to protect that
        // case would break arithmetic on all the others.
        INT => j.i64(a.column(4).as_primitive::<Int64Type>().value(row)),
        DOUBLE => j.f64(a.column(5).as_primitive::<Float64Type>().value(row)),
        BOOL => j.bool(a.column(6).as_boolean().value(row)),
        BYTES => j.hex(a.column(7).as_binary::<i32>().value(row)),
        // Slice and Map are protobuf-encoded in `ser`. Decoding them for
        // display is a V0 cut, not a storage limitation.
        _ => j.null(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use arrow_array::builder::StringDictionaryBuilder;
    use arrow_array::{
        BinaryArray, BooleanArray, Float64Array, Int32Array, Int64Array, ListArray,
        TimestampNanosecondArray, UInt8Array, UInt16Array, UInt32Array, UInt64Array,
    };
    use std::sync::Arc;

    fn hits(col: &dyn Array, op: Op, v: Value) -> Vec<u32> {
        match field_pred(col, op, &v) {
            Some(p) => (0..col.len() as u32).filter(|&i| p(i)).collect(),
            None => vec![u32::MAX],
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
            assert_eq!(hits(c, Op::Eq, Value::Int(2)), [1], "{name} eq");
            assert_eq!(hits(c, Op::Ne, Value::Int(2)), [0, 2], "{name} ne");
            assert_eq!(hits(c, Op::Lt, Value::Int(2)), [0], "{name} lt");
            assert_eq!(hits(c, Op::Lte, Value::Int(2)), [0, 1], "{name} lte");
            assert_eq!(hits(c, Op::Gt, Value::Int(2)), [2], "{name} gt");
            assert_eq!(hits(c, Op::Gte, Value::Int(2)), [1, 2], "{name} gte");
            // A null is not less than anything, and `ne` is where that bites:
            // the naive reading would return it.
            assert!(!hits(c, Op::Ne, Value::Int(9)).contains(&3), "{name} null");
            // Quoted, because a browser and an LLM both write "2" as often as 2.
            assert_eq!(hits(c, Op::Eq, Value::Str("2".into())), [1], "{name} str");
            // Nothing to compare against: no rows, not an error.
            assert_eq!(
                hits(c, Op::Eq, Value::Bool(true)),
                [u32::MAX],
                "{name} bool"
            );
        }

        // A fractional target against an integer column has no integer to be
        // equal to. Truncating would make `duration > 0.5` mean `duration > 0`.
        assert_eq!(
            hits(cols[1].1.as_ref(), Op::Gt, Value::Double(1.5)),
            [u32::MAX]
        );
        assert_eq!(hits(cols[1].1.as_ref(), Op::Gt, Value::Double(2.0)), [2]);
        // Floats compare as floats, and NaN is unordered rather than equal.
        let f = Float64Array::from(vec![Some(1.5), Some(f64::NAN)]);
        assert_eq!(hits(&f, Op::Gt, Value::Double(1.0)), [0]);
        assert_eq!(hits(&f, Op::Eq, Value::Double(f64::NAN)), Vec::<u32>::new());

        let b = BooleanArray::from(vec![Some(true), Some(false), None]);
        assert_eq!(hits(&b, Op::Eq, Value::Bool(true)), [0]);
        assert_eq!(hits(&b, Op::Eq, Value::Str("false".into())), [1]);
        assert_eq!(hits(&b, Op::Eq, Value::Int(1)), [u32::MAX]);

        let s = StringArray::from(vec![Some("alpha"), Some("beta"), None]);
        assert_eq!(hits(&s, Op::Eq, Value::Str("beta".into())), [1]);
        assert_eq!(hits(&s, Op::Contains, Value::Str("et".into())), [1]);
        assert_eq!(hits(&s, Op::Lt, Value::Str("b".into())), [0]);
        assert_eq!(hits(&s, Op::Eq, Value::Int(1)), [u32::MAX]);

        let mut d = StringDictionaryBuilder::<UInt16Type>::new();
        for v in ["ERROR", "INFO", "ERROR"] {
            d.append_value(v);
        }
        let d = d.finish();
        assert_eq!(hits(&d, Op::Eq, Value::Str("ERROR".into())), [0, 2]);
        // Resolved against the dictionary once. A value that is not in it cannot
        // match any row, and saying so costs no row scan at all.
        assert_eq!(
            hits(&d, Op::Eq, Value::Str("TRACE".into())),
            Vec::<u32>::new()
        );

        // Ids arrive as hex and live as bytes. The needle is decoded once, so a
        // needle that is not hex at all is no rows rather than every row.
        let ids = FixedSizeBinaryArray::try_from_iter([[1u8, 2], [3, 4]].into_iter()).unwrap();
        assert_eq!(hits(&ids, Op::Eq, Value::Str("0102".into())), [0]);
        assert_eq!(hits(&ids, Op::Gt, Value::Str("0102".into())), [1]);
        assert_eq!(hits(&ids, Op::Eq, Value::Str("zz".into())), [u32::MAX]);
        assert_eq!(hits(&ids, Op::Eq, Value::Str("010".into())), [u32::MAX]);

        // A type the table does not know is not a panic and not an error.
        let l = ListArray::from_iter_primitive::<Int64Type, _, _>(vec![Some(vec![Some(1)])]);
        assert_eq!(hits(&l, Op::Eq, Value::Int(1)), [u32::MAX]);
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
            (
                Arc::new(TimestampNanosecondArray::from(vec![
                    1_700_000_000_000_000_001i64,
                ])),
                "1700000000000000001",
            ),
            (Arc::new(Int64Array::from(vec![-7i64])), "-7"),
            (Arc::new(Int32Array::from(vec![-7i32])), "-7"),
            (
                Arc::new(UInt64Array::from(vec![u64::MAX])),
                "18446744073709551615",
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
            (Arc::new(d.finish()), r#""ERROR""#),
            // A null inside a list stays a null; the surrounding array does not
            // collapse to one.
            (Arc::new(l), "[1,null,3]"),
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

    /// A block on disk, scanned. The predicate tables above are exercised in
    /// isolation; this is the path that reaches them — the time prefilter, the
    /// three attribute levels, and the merge that decides which of two levels
    /// setting the same key the caller actually sees.
    #[test]
    fn a_scan_narrows_by_time_then_by_terms_and_the_most_specific_level_wins() {
        use mira_proto::collector::logs::v1::ExportLogsServiceRequest;
        use mira_proto::common::v1::any_value::Value as Av;
        use mira_proto::common::v1::{AnyValue, ArrayValue, InstrumentationScope, KeyValue};
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
                            body: Some(AnyValue {
                                value: Some(Av::StringValue(format!("line {i}"))),
                            }),
                            attributes: vec![
                                kv("deploy.env", Av::StringValue("canary".into())),
                                kv("attempt", Av::IntValue(i as i64)),
                                // Neither of these is filterable in V0; both
                                // still have to come back in the row.
                                kv("payload", Av::BytesValue(vec![0xde, 0xad].into())),
                                kv(
                                    "tags",
                                    Av::ArrayValue(ArrayValue {
                                        values: vec![AnyValue {
                                            value: Some(Av::StringValue("a".into())),
                                        }],
                                    }),
                                ),
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
            crate::block::publish(&dir, "logs", crate::block::node_id("a"), 0, &sealed).unwrap();

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

        let row = scan(base, all, vec![]).json;
        // Record level beats resource level for the same key.
        assert!(row.contains(r#""deploy.env":"canary""#), "{row}");
        assert!(!row.contains("prod"), "{row}");
        assert!(row.contains(r#""service.name":"checkout""#), "{row}");
        assert!(row.contains(r#""payload":"dead""#), "{row}");
        // A slice is stored but not yet decoded for display, and null is an
        // honest answer where a guess would not be.
        assert!(row.contains(r#""tags":null"#), "{row}");

        // Retention can delete a block between the directory listing and the
        // read. The block still counts as present and simply contributes
        // nothing, rather than failing the query.
        std::fs::remove_file(bref.dir.join("logs.arrow")).unwrap();
        let r = scan(base, all, vec![]);
        assert_eq!((r.stats.blocks_total, r.stats.blocks_scanned), (1, 0));
        assert_eq!(r.json, "[]");
    }
}
