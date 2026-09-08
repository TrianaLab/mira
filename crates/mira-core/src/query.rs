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
}

/// The trace id this search pins down exactly, if it pins one down.
///
/// Only `trace_id = <32 hex chars>` on the traces signal qualifies. Terms are
/// AND-ed, so one such term is enough no matter what else is in the list: a
/// block that cannot hold the id cannot hold a row satisfying the conjunction.
fn trace_needle(q: &Search) -> Option<[u8; 16]> {
    if q.signal != Signal::Traces {
        return None;
    }
    q.terms.iter().find_map(|t| match (&t.target, t.op) {
        (Target::Field(f), Op::Eq) if f == "trace_id" => unhex(t.value.as_str()?)?.try_into().ok(),
        _ => None,
    })
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
    // Newest first, so `limit` can cut the scan short.
    refs.sort_by_key(|b| std::cmp::Reverse((b.max_ts, b.seq)));

    let needle = trace_needle(q);
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

        // The second pruning key, and the only one that helps a trace lookup:
        // "every span of trace X" has no time bound, so without this every
        // block on disk is opened and paged in. A 20 KB sidecar read is two
        // orders of magnitude cheaper than the block it skips.
        if let Some(id) = &needle
            && let Ok(f) = std::fs::read(bref.dir.join(crate::bloom::TRACE_IDX))
            && !crate::bloom::may_contain(&f, id)
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
        stats.rows_matched += sel.len();
        if let Some(time) = b.time() {
            hits.extend(sel.iter().map(|&row| Hit {
                ts: time[row as usize],
                block: i,
                row,
            }));
        }
        open.push(Some(b));

        // Trim as we go, so memory is bounded by `limit` rather than by the
        // match count, which is not bounded by anything.
        hits.sort_unstable_by_key(|h| std::cmp::Reverse(h.ts));
        hits.truncate(q.limit);
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
    })
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
        Ok(Some(Block {
            signal,
            attrs: load(signal.attrs())?,
            resource_attrs: load("resource_attrs")?,
            scope_attrs: load("scope_attrs")?,
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

    /// Write one root row as a JSON object, attributes merged in.
    fn emit_row(&self, j: &mut Json, row: u32) {
        j.obj(|j| {
            for (i, f) in self.root.schema().fields().iter().enumerate() {
                // Internal plumbing. The caller asked for a log line, not for
                // the block-local ids that found it.
                if matches!(f.name().as_str(), "id" | "resource_id" | "scope_id") {
                    continue;
                }
                let col = self.root.column(i);
                if col.is_null(row as usize) {
                    continue;
                }
                j.key(f.name());
                emit_value(j, col.as_ref(), row as usize);
            }

            j.key("attributes");
            j.obj(|j| {
                let levels = [
                    (&self.resource_attrs, self.fk(row, "resource_id")),
                    (&self.scope_attrs, self.fk(row, "scope_id")),
                    (&self.attrs, Some(row)),
                ];
                let mut merged: Vec<(&str, &RecordBatch, usize)> = Vec::new();
                for (table, parent) in levels {
                    let (Some(a), Some(parent)) = (table, parent) else {
                        continue;
                    };
                    let parents = a.column(0).as_primitive::<UInt32Type>().values();
                    for r in (0..a.num_rows()).filter(|&r| parents[r] == parent) {
                        merged.push((attr_key(a, r), a, r));
                    }
                }
                // Sorted so output is deterministic, and stably so that within
                // one key the last level pushed — the most specific one — is
                // the entry that survives the dedup below.
                merged.sort_by_key(|(k, _, _)| *k);
                for (i, &(k, a, r)) in merged.iter().enumerate() {
                    if merged.get(i + 1).is_some_and(|nxt| nxt.0 == k) {
                        continue;
                    }
                    j.key(k);
                    emit_attr(j, a, r);
                }
            });
        });
    }

    fn fk(&self, row: u32, name: &str) -> Option<u32> {
        self.root
            .column_by_name(name)
            .map(|c| c.as_primitive::<UInt16Type>().value(row as usize) as u32)
    }
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
                Op::Contains => v.as_str().is_some_and(|n| s.contains(n)),
                _ => v.as_str().is_some_and(|n| op.test_ord(s.cmp(n))),
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
