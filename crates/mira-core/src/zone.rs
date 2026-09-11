//! Block-level numeric ranges — the zone map.
//!
//! [`crate::bloom`] answers "could this block hold *this exact value*", which is
//! everything an equality needs and nothing an ordering can use. The queries
//! that actually open a tracing UI are orderings: *"spans slower than a
//! second"*, *"anything that returned 5xx"*. Both scan every block in retention
//! today, because `duration_nano > 1_000_000_000` has no value to hash.
//!
//! So a second sidecar, holding one `(min, max)` pair per numeric thing the
//! block contains — every numeric column of the root table, and every attribute
//! key with a numeric value at any level. A term whose range cannot reach the
//! block's is a block not opened. Reading it is a `read(2)` of a few kilobytes
//! and a binary search, against the tens of megabytes it decides not to fault
//! in.
//!
//! Three properties make it safe to skip a block on this file's word, and all
//! three are the reason the build side is fussier than "call min and max":
//!
//! * **Absent key means prune.** The map is complete over the block, so a key
//!   with no entry is a key with no comparable value — no row can satisfy an
//!   ordering against it. That is only true because [`index`] is driven off the
//!   schema, exactly as [`crate::attrs::index`] is: a signal that grows a fourth
//!   attribute level is covered without anyone remembering it here.
//! * **Strings that look like numbers are numbers.** `query::attr_matches`
//!   parses a `str`-typed attribute before an ordered comparison, because half
//!   the SDKs emit `http.response.status_code` as text. A range built only from
//!   the `int` and `double` columns would therefore prune away blocks holding
//!   `"503"`, so parseable text is folded into the double range.
//! * **Strings that do not look like numbers fall back to a lexicographic
//!   compare**, which no interval over the reals describes. A key with any of
//!   those gets [`Range::ANY`] — the entry exists, and it answers "maybe" to
//!   everything. It costs the pruning for that one key rather than the
//!   correctness of the file.
//!
//! Two ranges per key rather than one, `i64` beside `f64`, because the query
//! layer compares integers as integers: an `i64` past 2⁵³ rounds when it becomes
//! a double, and rounding a *max* down is exactly the false negative this whole
//! module is not allowed to produce. The pair costs 32 bytes on entries that
//! number in the dozens.
//!
//! Like every sidecar here it fails open: unreadable, short or unrecognized is
//! "scan the block".

use std::collections::HashMap;

use arrow_array::cast::AsArray;
use arrow_array::types::{
    Float64Type, Int32Type, Int64Type, TimestampNanosecondType, UInt8Type, UInt16Type, UInt32Type,
    UInt64Type,
};
use arrow_array::{Array, RecordBatch};
use arrow_schema::DataType;

use crate::query::Op;
use crate::schema::{ATTRS, AttrType};

/// Filename inside a published block. Part of the on-disk format.
pub const ZONE_IDX: &str = "zone.idx";

const MAGIC: [u8; 4] = *b"MZON";
const VERSION: u8 = 1;
/// magic 4 | version 1 | pad 3 | entries 4 | crc32 4
const HEADER: usize = 16;
/// key 8 | int_min 8 | int_max 8 | dbl_min 8 | dbl_max 8
const ENTRY: usize = 40;

/// Beyond this many distinct keys the map stops being a rounding error on the
/// block and a block that diverse prunes little anyway. Past it we write
/// nothing, which the reader reads as "scan me".
///
/// 4,096 entries is 160 KB, against `bloom`'s 1M-key ceiling: the two are sized
/// differently on purpose, because a Bloom filter degrades into false positives
/// as it fills and this degrades into a linear amount of disk.
const MAX_KEYS: usize = 4096;

/// What one key's values span, in whichever of the two number lines they live
/// on.
///
/// Empty is `min > max`, which every test below rejects without a special case:
/// an `Lt` against a `min` of `i64::MAX` is false, a `Gt` against a `max` of
/// `i64::MIN` is false, and `Eq` needs both.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Range {
    pub int_min: i64,
    pub int_max: i64,
    pub dbl_min: f64,
    pub dbl_max: f64,
}

impl Range {
    /// Nothing seen yet: both intervals empty.
    const EMPTY: Range = Range {
        int_min: i64::MAX,
        int_max: i64::MIN,
        dbl_min: f64::INFINITY,
        dbl_max: f64::NEG_INFINITY,
    };

    /// "I cannot describe this key" — both intervals unbounded, so every probe
    /// against it answers maybe. What a lexicographically-compared string
    /// value degrades a key to.
    pub const ANY: Range = Range {
        int_min: i64::MIN,
        int_max: i64::MAX,
        dbl_min: f64::NEG_INFINITY,
        dbl_max: f64::INFINITY,
    };

    fn int(&mut self, v: i64) {
        self.int_min = self.int_min.min(v);
        self.int_max = self.int_max.max(v);
    }

    /// NaN is dropped rather than folded in. `partial_cmp` returns `None`
    /// against it, so no operator matches a NaN row and no interval has to
    /// cover one — and `min`/`max` over a NaN would poison the interval that
    /// covers the rest.
    fn float(&mut self, v: f64) {
        if !v.is_nan() {
            self.dbl_min = self.dbl_min.min(v);
            self.dbl_max = self.dbl_max.max(v);
        }
    }
}

/// One ordered term, in the form a zone map answers.
///
/// `int` and `float` are the same query scalar read two ways, because the row
/// it will be compared against may be stored either way and the block holds
/// both intervals. Absent means "no row of that number line can match", which
/// is the query layer's own rule: `attr_matches`'s `INT` arm is
/// `v.as_i64().is_some_and(..)`, so a fractional scalar never matches an
/// integer column.
pub struct Probe {
    pub key: u64,
    pub op: Op,
    pub int: Option<i64>,
    pub float: Option<f64>,
}

impl Probe {
    /// Could a row in this block satisfy the term? A key the map does not hold
    /// is a key the block has no comparable value for, so the answer is no —
    /// see the module docs for why that is sound and not merely convenient.
    pub fn maybe(&self, m: &Map) -> bool {
        match m.get(self.key) {
            None => false,
            Some(r) => {
                self.int
                    .is_some_and(|t| reachable(self.op, r.int_min, r.int_max, t))
                    || self
                        .float
                        .is_some_and(|t| reachable(self.op, r.dbl_min, r.dbl_max, t))
            }
        }
    }
}

/// Could any value in `[min, max]` satisfy `x op target`?
///
/// `Ne` and `Contains` are deliberately absent from the callers rather than
/// answered here: a range only rules `Ne` out when it is a single point, which
/// is a rounding error's worth of pruning for a branch that is easy to get
/// backwards.
fn reachable<T: PartialOrd + Copy>(op: Op, min: T, max: T, target: T) -> bool {
    match op {
        Op::Eq => min <= target && target <= max,
        Op::Lt => min < target,
        Op::Lte => min <= target,
        Op::Gt => max > target,
        Op::Gte => max >= target,
        Op::Ne | Op::Contains => true,
    }
}

/// Accumulates the ranges of one block.
#[derive(Default)]
pub struct Builder {
    keys: HashMap<u64, Range>,
    full: bool,
}

impl Builder {
    fn at(&mut self, key: u64) -> Option<&mut Range> {
        if !self.keys.contains_key(&key) && self.keys.len() >= MAX_KEYS {
            self.full = true;
            return None;
        }
        Some(self.keys.entry(key).or_insert(Range::EMPTY))
    }

    pub fn int(&mut self, key: u64, v: i64) {
        if let Some(r) = self.at(key) {
            r.int(v);
        }
    }

    pub fn float(&mut self, key: u64, v: f64) {
        if let Some(r) = self.at(key) {
            r.float(v);
        }
    }

    /// Give up on one key: it holds something no interval describes.
    pub fn any(&mut self, key: u64) {
        if let Some(r) = self.at(key) {
            *r = Range::ANY;
        }
    }

    /// `None` when there is nothing to say, or too much of it. Both mean no
    /// file, which the reader reads as "scan me".
    pub fn build(&self) -> Option<Vec<u8>> {
        if self.full || self.keys.is_empty() {
            return None;
        }
        // Sorted, so the reader binary-searches instead of holding a hash map
        // it would have to allocate per block probed.
        let mut entries: Vec<(&u64, &Range)> = self.keys.iter().collect();
        entries.sort_unstable_by_key(|(k, _)| **k);

        let mut body = Vec::with_capacity(entries.len() * ENTRY);
        for (k, r) in entries {
            body.extend_from_slice(&k.to_le_bytes());
            body.extend_from_slice(&r.int_min.to_le_bytes());
            body.extend_from_slice(&r.int_max.to_le_bytes());
            body.extend_from_slice(&r.dbl_min.to_le_bytes());
            body.extend_from_slice(&r.dbl_max.to_le_bytes());
        }

        let mut out = Vec::with_capacity(HEADER + body.len());
        out.extend_from_slice(&MAGIC);
        out.push(VERSION);
        out.extend_from_slice(&[0, 0, 0]);
        out.extend_from_slice(&(self.keys.len() as u32).to_le_bytes());
        out.extend_from_slice(&crc32fast::hash(&body).to_le_bytes());
        out.extend_from_slice(&body);
        Some(out)
    }
}

/// A zone map checked out, with its header validated once.
pub struct Map<'a> {
    body: &'a [u8],
    n: usize,
}

impl<'a> Map<'a> {
    /// `None` for anything unreadable, which every caller must treat as "scan
    /// the block".
    pub fn open(file: &'a [u8]) -> Option<Map<'a>> {
        if file.len() < HEADER || file[..4] != MAGIC || file[4] != VERSION {
            return None;
        }
        let n = u32::from_le_bytes(file[8..12].try_into().expect("4 bytes")) as usize;
        let crc = u32::from_le_bytes(file[12..16].try_into().expect("4 bytes"));
        let body = &file[HEADER..];
        if n == 0 || body.len() != n * ENTRY || crc32fast::hash(body) != crc {
            return None;
        }
        Some(Map { body, n })
    }

    fn key_at(&self, i: usize) -> u64 {
        u64::from_le_bytes(self.body[i * ENTRY..][..8].try_into().expect("8 bytes"))
    }

    /// Binary search over the packed entries by hand: the body is bytes, not
    /// `Range`s, so `slice::binary_search` has nothing to search.
    fn get(&self, key: u64) -> Option<Range> {
        let (mut lo, mut hi) = (0usize, self.n);
        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            match self.key_at(mid).cmp(&key) {
                std::cmp::Ordering::Less => lo = mid + 1,
                std::cmp::Ordering::Greater => hi = mid,
                std::cmp::Ordering::Equal => {
                    lo = mid;
                    break;
                }
            }
        }
        if lo >= self.n || self.key_at(lo) != key {
            return None;
        }
        let i = lo;
        let f = |off: usize| {
            self.body[i * ENTRY + off..][..8]
                .try_into()
                .expect("8 bytes")
        };
        Some(Range {
            int_min: i64::from_le_bytes(f(8)),
            int_max: i64::from_le_bytes(f(16)),
            dbl_min: f64::from_le_bytes(f(24)),
            dbl_max: f64::from_le_bytes(f(32)),
        })
    }
}

/// Hash of an attribute key, in its own domain so that an attribute and a root
/// column of the same name do not share an entry.
///
/// A collision between two attribute keys merges their ranges, which widens one
/// and prunes less. Widening is the safe direction, which is why 64 bits is
/// enough here and why there is no need to store the key text.
pub fn attr_key(key: &str) -> u64 {
    mix(crate::identity::hash64(key.as_bytes()) ^ 0xa77b_a77b_a77b_a77b)
}

/// Hash of a root-table column name. See [`attr_key`].
pub fn field_key(name: &str) -> u64 {
    mix(crate::identity::hash64(name.as_bytes()) ^ 0xf1e1_f1e1_f1e1_f1e1)
}

/// splitmix64's finalizer, as in [`crate::bloom`]: the domain constant above
/// only separates the two spaces if every input bit reaches every output bit.
fn mix(mut x: u64) -> u64 {
    x ^= x >> 30;
    x = x.wrapping_mul(0xbf58_476d_1ce4_e5b9);
    x ^= x >> 27;
    x = x.wrapping_mul(0x94d0_49bb_1331_11eb);
    x ^ (x >> 31)
}

/// Build a block's [`ZONE_IDX`] over its root columns and every attribute table
/// in it.
///
/// `tables[0]` is the root by the convention every signal's `finish` follows —
/// it is also the table `query::Block` opens under the signal's own name, so
/// the two agree by construction.
pub fn index(tables: &[(&'static str, RecordBatch)]) -> Option<Vec<u8>> {
    let mut b = Builder::default();
    if let Some((_, root)) = tables.first() {
        index_root(&mut b, root);
    }
    for (_, t) in tables {
        if std::sync::Arc::ptr_eq(&t.schema(), &ATTRS) {
            index_attrs(&mut b, t);
        }
    }
    b.build()
}

/// Every numeric column of the root table, under [`field_key`].
///
/// The integer arms cast to `i64` exactly as `query::field_pred` does, wrapping
/// included: a range built any other way would describe a different comparison
/// than the one the scan performs, and disagreeing with the scan is the whole
/// failure mode.
fn index_root(b: &mut Builder, root: &RecordBatch) {
    macro_rules! ints {
        ($t:ty, $col:expr, $key:expr) => {{
            let a = $col.as_primitive::<$t>();
            for i in 0..a.len() {
                if !a.is_null(i) {
                    b.int($key, a.value(i) as i64);
                }
            }
        }};
    }

    for (f, col) in root.schema().fields().iter().zip(root.columns()) {
        let key = field_key(f.name());
        match col.data_type() {
            DataType::Timestamp(_, _) => ints!(TimestampNanosecondType, col, key),
            DataType::Int64 => ints!(Int64Type, col, key),
            DataType::Int32 => ints!(Int32Type, col, key),
            DataType::UInt64 => ints!(UInt64Type, col, key),
            DataType::UInt32 => ints!(UInt32Type, col, key),
            DataType::UInt16 => ints!(UInt16Type, col, key),
            DataType::UInt8 => ints!(UInt8Type, col, key),
            DataType::Float64 => {
                let a = col.as_primitive::<Float64Type>();
                for i in 0..a.len() {
                    if !a.is_null(i) {
                        b.float(key, a.value(i));
                    }
                }
            }
            // Not orderable as a number, and therefore not prunable by one: a
            // numeric term against a string, boolean or id column matches
            // nothing at all (`field_pred` returns `None` and the scan clears
            // its selection), so leaving the column out of the map is not just
            // safe, it is the same answer arrived at earlier.
            _ => {}
        }
    }
}

/// Every attribute key with a comparable value, under [`attr_key`]. Column
/// positions match `attrs::index_table`, which walks the same schema.
fn index_attrs(b: &mut Builder, t: &RecordBatch) {
    let dict = t.column(1).as_dictionary::<UInt16Type>();
    let names = dict.values().as_string::<i32>();
    let codes = dict.keys().values();
    let types = t.column(2).as_primitive::<UInt8Type>().values();
    let strs = crate::attrs::str_column(t);
    let ints = t.column(4).as_primitive::<Int64Type>();
    let doubles = t.column(5).as_primitive::<Float64Type>();

    const STR: u8 = AttrType::Str as u8;
    const INT: u8 = AttrType::Int as u8;
    const DOUBLE: u8 = AttrType::Double as u8;

    // Hashing the key text once per *row* would be most of the cost of this
    // pass, and the dictionary is a few dozen entries against a few hundred
    // thousand rows.
    let hashes: Vec<u64> = (0..names.len()).map(|i| attr_key(names.value(i))).collect();

    for row in 0..t.num_rows() {
        let key = hashes[codes[row] as usize];
        match types[row] {
            INT => b.int(key, ints.value(row)),
            DOUBLE => b.float(key, doubles.value(row)),
            STR => match strs.value(row).parse::<f64>() {
                // The same parse `attr_matches` performs before an ordered
                // comparison, so the interval covers exactly the rows the scan
                // would compare numerically.
                Ok(v) => b.float(key, v),
                // And the rows it would compare lexicographically, which no
                // interval covers.
                Err(_) => b.any(key),
            },
            // Bool, Bytes, Slice, Map and Empty match no numeric term at all —
            // `v.as_bool()` is `None` for a number and the rest are not
            // filterable — so they contribute nothing and rule nothing out.
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use arrow_array::{Float64Array, Int64Array, RecordBatch, StringArray, UInt64Array};
    use arrow_schema::{Field, Schema};

    use super::*;

    fn map_of(b: &Builder) -> Vec<u8> {
        b.build().expect("something to write")
    }

    fn probe(key: u64, op: Op, int: Option<i64>, float: Option<f64>) -> Probe {
        Probe {
            key,
            op,
            int,
            float,
        }
    }

    #[test]
    fn a_range_answers_the_five_ordered_operators_and_nothing_else() {
        let mut b = Builder::default();
        let k = attr_key("http.status_code");
        b.int(k, 200);
        b.int(k, 404);
        let bytes = map_of(&b);
        let m = Map::open(&bytes).expect("readable");

        let ask = |op, t: i64| probe(k, op, Some(t), Some(t as f64)).maybe(&m);
        assert!(ask(Op::Eq, 200) && ask(Op::Eq, 300) && !ask(Op::Eq, 500));
        assert!(ask(Op::Gte, 404) && !ask(Op::Gte, 405));
        assert!(ask(Op::Gt, 403) && !ask(Op::Gt, 404));
        assert!(ask(Op::Lte, 200) && !ask(Op::Lte, 199));
        assert!(ask(Op::Lt, 201) && !ask(Op::Lt, 200));
        // Ne and Contains are never pruned on, whatever the range says.
        assert!(ask(Op::Ne, 200) && ask(Op::Contains, 999));
    }

    #[test]
    fn a_key_the_block_never_saw_prunes_and_an_unreadable_file_does_not() {
        let mut b = Builder::default();
        b.int(attr_key("present"), 1);
        let bytes = map_of(&b);
        let m = Map::open(&bytes).expect("readable");
        assert!(probe(attr_key("present"), Op::Gte, Some(0), Some(0.0)).maybe(&m));
        assert!(!probe(attr_key("absent"), Op::Gte, Some(0), Some(0.0)).maybe(&m));

        // Every way the file can be wrong reads as "no map", never as "skip".
        assert!(Map::open(&[]).is_none());
        assert!(Map::open(&bytes[..HEADER]).is_none());
        let mut torn = bytes.clone();
        torn.pop();
        assert!(Map::open(&torn).is_none());
        let mut flipped = bytes.clone();
        *flipped.last_mut().expect("non-empty") ^= 0xff;
        assert!(Map::open(&flipped).is_none(), "the crc has to catch this");
        let mut version = bytes.clone();
        version[4] = 2;
        assert!(Map::open(&version).is_none());
    }

    #[test]
    fn an_integer_past_two_to_the_fifty_three_keeps_its_own_number_line() {
        // 2^53 + 1 is the first integer a double cannot hold. Folded into the
        // double range it would round *down*, and a `gte` on the exact value
        // would then prune the block that holds it.
        let v = (1i64 << 53) + 1;
        let mut b = Builder::default();
        let k = attr_key("bytes");
        b.int(k, v);
        let bytes = map_of(&b);
        let m = Map::open(&bytes).expect("readable");
        assert!(probe(k, Op::Gte, Some(v), Some(v as f64)).maybe(&m));
        assert!(!probe(k, Op::Gt, Some(v), Some(v as f64)).maybe(&m));
    }

    #[test]
    fn a_fractional_scalar_cannot_reach_an_integer_only_key() {
        let mut b = Builder::default();
        let k = attr_key("retries");
        b.int(k, 3);
        let bytes = map_of(&b);
        let m = Map::open(&bytes).expect("readable");
        // `Value::as_i64` returns None for 3.5, exactly as the scan's INT arm
        // does, so the int range is not consulted and the double range is empty.
        assert!(!probe(k, Op::Eq, None, Some(3.5)).maybe(&m));
        assert!(!probe(k, Op::Lt, None, Some(3.5)).maybe(&m));
    }

    #[test]
    fn text_that_parses_is_a_number_and_text_that_does_not_gives_up_the_key() {
        let attrs = |vals: Vec<&str>| {
            let mut a = crate::attrs::AttrsBuilder::new("t");
            for v in vals {
                a.append(
                    0,
                    "code",
                    Some(&mira_proto::common::v1::AnyValue {
                        value: Some(mira_proto::common::v1::any_value::Value::StringValue(
                            v.into(),
                        )),
                    }),
                )
                .expect("appends");
            }
            vec![("t", a.finish().expect("finishes"))]
        };

        let k = attr_key("code");
        let numeric = index(&attrs(vec!["200", "503"])).expect("a map");
        let m = Map::open(&numeric).expect("readable");
        assert!(probe(k, Op::Gte, Some(500), Some(500.0)).maybe(&m));
        assert!(!probe(k, Op::Gt, Some(503), Some(503.0)).maybe(&m));

        // One value the scan would compare as text, and the key stops pruning —
        // for that key only.
        let mixed = index(&attrs(vec!["200", "unset"])).expect("a map");
        let m = Map::open(&mixed).expect("readable");
        assert!(probe(k, Op::Gt, Some(9999), Some(9999.0)).maybe(&m));
    }

    #[test]
    fn the_root_tables_numeric_columns_are_in_it_and_the_others_are_not() {
        let schema = Arc::new(Schema::new(vec![
            Field::new("duration_nano", DataType::UInt64, false),
            Field::new("count", DataType::Int64, true),
            Field::new("ratio", DataType::Float64, false),
            Field::new("body", DataType::Utf8, false),
        ]));
        let root = RecordBatch::try_new(
            schema,
            vec![
                Arc::new(UInt64Array::from(vec![10u64, 2_000_000_000])),
                // All null: nothing can match, and the empty range says so.
                Arc::new(Int64Array::from(vec![None, None] as Vec<Option<i64>>)),
                Arc::new(Float64Array::from(vec![0.25, f64::NAN])),
                Arc::new(StringArray::from(vec!["a", "b"])),
            ],
        )
        .expect("a batch");

        let bytes = index(&[("root", root)]).expect("a map");
        let m = Map::open(&bytes).expect("readable");

        let d = field_key("duration_nano");
        assert!(probe(d, Op::Gt, Some(1_000_000_000), Some(1e9)).maybe(&m));
        assert!(!probe(d, Op::Gt, Some(2_000_000_000), Some(2e9)).maybe(&m));

        // Present but empty: an all-null column matches nothing, and a column
        // no numeric term can compare is not in the map at all. Both prune.
        assert!(!probe(field_key("count"), Op::Gte, Some(0), Some(0.0)).maybe(&m));
        assert!(!probe(field_key("body"), Op::Gte, Some(0), Some(0.0)).maybe(&m));

        // NaN never matches an operator, so it is not in the interval either.
        let r = field_key("ratio");
        assert!(probe(r, Op::Lte, None, Some(0.25)).maybe(&m));
        assert!(!probe(r, Op::Gt, None, Some(0.25)).maybe(&m));
    }

    #[test]
    fn too_many_keys_writes_nothing_rather_than_a_map_nobody_wants() {
        let mut b = Builder::default();
        for i in 0..=MAX_KEYS {
            b.int(attr_key(&format!("k{i}")), i as i64);
        }
        assert!(b.build().is_none(), "over the cap, so no file");
        assert!(Builder::default().build().is_none(), "nothing to say");
    }
}
