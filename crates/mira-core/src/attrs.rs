//! The parts of the star schema that every signal has in common.
//!
//! OTLP's three signals disagree about almost everything, but they all hang off
//! the same Resource-Scope preamble and they all carry attributes in the same
//! key/type/value shape. [`schema::ATTRS`](crate::schema::ATTRS) is already one
//! schema for all five attribute tables; this is the matching builder, plus
//! [`ResourceScope`], which owns the `resources` / `resource_attrs` /
//! `scope_attrs` triple that is byte-identical whether it is fronting logs,
//! spans or data points.
//!
//! Extracted from `logs.rs` rather than designed up front: it is shared because
//! it turned out to be the same code three times, which is the only good reason.

use std::collections::HashMap;
use std::sync::Arc;

use arrow_array::builder::{
    ArrayBuilder, BinaryBuilder, BooleanBuilder, Float64Builder, Int64Builder,
    StringDictionaryBuilder, UInt8Builder, UInt16Builder, UInt32Builder, UInt64Builder,
};
use arrow_array::types::{UInt16Type, UInt32Type};
use arrow_array::{Array, ArrayRef, RecordBatch};
use prost::Message;

use mira_proto::common::v1::{AnyValue, InstrumentationScope, KeyValue, any_value::Value};
use mira_proto::resource::v1::Resource;

use crate::error::{Error, Result};
use crate::identity::resource_key;
use crate::schema::{ATTRS, AttrType, DICT_CAP, RESOURCES};

/// A `Dictionary<UInt16, Utf8>` column that knows how full it is.
///
/// `StringDictionaryBuilder` does not expose its cardinality and returns the
/// overflow as an error from `append`, which is too late: the seal decision has
/// to be made before the append, because a builder cannot be rolled back. Every
/// enumerable string column in the engine — severity text, span name, event
/// name, metric name and unit — needs exactly this, so it is one type.
pub struct DictColumn {
    label: &'static str,
    b: StringDictionaryBuilder<UInt16Type>,
    n: usize,
}

impl DictColumn {
    pub fn new(label: &'static str) -> Self {
        Self {
            label,
            b: StringDictionaryBuilder::new(),
            n: 0,
        }
    }

    /// Whether `n` more *distinct* values fit. Callers pass the total number of
    /// values they are about to append, because deduplicating them first would
    /// cost more than the occasional block sealed a little early.
    pub fn has_headroom(&self, n: usize) -> bool {
        self.n + n <= DICT_CAP
    }

    /// Empty becomes null rather than a dictionary entry. proto3 cannot
    /// distinguish an unset string from an empty one, so every span without a
    /// `trace_state` would otherwise burn a slot and a validity bit to say so.
    pub fn append(&mut self, v: &str) -> Result<()> {
        if v.is_empty() {
            self.b.append_null();
            return Ok(());
        }
        let k = self
            .b
            .append(v)
            .map_err(|_| Error::DictionaryFull(self.label))?;
        self.n = self.n.max(k as usize + 1);
        Ok(())
    }

    /// Materialise the column without consuming the builder.
    ///
    /// `finish_cloned`, not `finish`, throughout the seal path: the same code
    /// serves both the real seal — whose caller replaces the whole builder
    /// afterwards anyway — and the open-block snapshot the read path queries
    /// (section 4), which must leave the builder accumulating. The cost is one
    /// buffer copy per column instead of a move, which at the 32 MiB block
    /// target is a few milliseconds once per block.
    pub fn finish(&self) -> ArrayRef {
        Arc::new(self.b.finish_cloned())
    }
}

/// Builder for one attribute table — log, span, event, link, data point,
/// exemplar, resource or scope. They are all the same nine columns.
pub struct AttrsBuilder {
    /// Which table this is, for error messages. `"log_attrs.key"` beats
    /// `"attrs.key"` when three of them are open at once.
    label: &'static str,
    parent_id: UInt32Builder,
    key: StringDictionaryBuilder<UInt16Type>,
    /// Distinct entries in `key`. Tracked here because the dictionary builder
    /// does not expose it, and the seal decision has to be made *before* an
    /// append rather than after one has already failed.
    n_keys: usize,
    ty: UInt8Builder,
    /// See [`schema::ATTRS`](crate::schema::ATTRS) for why this one column is
    /// dictionary-encoded. The width is `u32`, so unlike `key` it has no cap and
    /// no seal-early check.
    str_: StringDictionaryBuilder<UInt32Type>,
    /// Distinct values in `str_`, and the bytes they hold. Same reason as
    /// `n_keys`: the builder exposes neither, and with a dictionary the heap is
    /// what survives deduplication rather than what was appended — counting the
    /// appends would seal a block of one repeated 32 KB prompt hundreds of times
    /// too early.
    n_str: usize,
    str_bytes: usize,
    int: Int64Builder,
    double: Float64Builder,
    bool_: BooleanBuilder,
    bytes: BinaryBuilder,
    ser: BinaryBuilder,
}

impl AttrsBuilder {
    pub fn new(label: &'static str) -> Self {
        Self {
            label,
            parent_id: UInt32Builder::new(),
            key: StringDictionaryBuilder::new(),
            n_keys: 0,
            ty: UInt8Builder::new(),
            str_: StringDictionaryBuilder::new(),
            n_str: 0,
            str_bytes: 0,
            int: Int64Builder::new(),
            double: Float64Builder::new(),
            bool_: BooleanBuilder::new(),
            bytes: BinaryBuilder::new(),
            ser: BinaryBuilder::new(),
        }
    }

    pub fn len(&self) -> usize {
        self.ty.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Whether `n` more attribute rows are guaranteed not to overflow the key
    /// dictionary. Assumes every one of them is a new key, because the cheap
    /// check has to be the conservative one.
    pub fn has_headroom(&self, n: usize) -> bool {
        self.n_keys + n <= DICT_CAP
    }

    /// Bytes held in the variable-width value heaps. Fixed-width columns are
    /// estimated from the row count; these cannot be, because one row can be a
    /// 32 KB GenAI prompt.
    pub fn heap_bytes(&self) -> usize {
        self.str_bytes + self.bytes.values_slice().len() + self.ser.values_slice().len()
    }

    /// Append every attribute of `kvs` as rows pointing at `parent_id`.
    pub fn append_all(&mut self, parent_id: u32, kvs: &[KeyValue]) -> Result<()> {
        for kv in kvs {
            self.append(parent_id, &kv.key, kv.value.as_ref())?;
        }
        Ok(())
    }

    pub fn append(&mut self, parent_id: u32, key: &str, value: Option<&AnyValue>) -> Result<()> {
        // The dictionary goes first because it is the only fallible step here.
        // Every append below it is infallible, so an overflow leaves all nine
        // columns the same length and the block is still sealable. A half-written
        // row would fail `RecordBatch::try_new` at flush and take the whole block
        // with it.
        let k = self
            .key
            .append(key)
            .map_err(|_| Error::DictionaryFull(self.label))?;
        self.n_keys = self.n_keys.max(k as usize + 1);
        self.parent_id.append_value(parent_id);

        // Exactly one of the six value columns is non-null per row; `type` says
        // which. Null-appending the other five costs one validity bit each.
        let mut set = [false; 6];
        let ty = match value.and_then(|v| v.value.as_ref()) {
            None => AttrType::Empty,
            Some(Value::StringValue(s)) => {
                // A `u32` dictionary cannot overflow inside a block this engine
                // would ever seal, so the error arm is unreachable rather than
                // load-bearing — mapped and not unwrapped because an unreachable
                // panic in the ingest path is still a panic.
                let k = self
                    .str_
                    .append(s)
                    .map_err(|_| Error::DictionaryFull(self.label))?;
                if k as usize >= self.n_str {
                    self.n_str = k as usize + 1;
                    self.str_bytes += s.len();
                }
                set[0] = true;
                AttrType::Str
            }
            Some(Value::IntValue(i)) => {
                self.int.append_value(*i);
                set[1] = true;
                AttrType::Int
            }
            Some(Value::DoubleValue(d)) => {
                self.double.append_value(*d);
                set[2] = true;
                AttrType::Double
            }
            Some(Value::BoolValue(b)) => {
                self.bool_.append_value(*b);
                set[3] = true;
                AttrType::Bool
            }
            Some(Value::BytesValue(b)) => {
                self.bytes.append_value(b);
                set[4] = true;
                AttrType::Bytes
            }
            Some(v @ Value::ArrayValue(_)) | Some(v @ Value::KvlistValue(_)) => {
                let owned = AnyValue {
                    value: Some(v.clone()),
                };
                self.ser.append_value(owned.encode_to_vec());
                set[5] = true;
                if matches!(v, Value::ArrayValue(_)) {
                    AttrType::Slice
                } else {
                    AttrType::Map
                }
            }
        };
        self.ty.append_value(ty as u8);

        if !set[0] {
            self.str_.append_null();
        }
        if !set[1] {
            self.int.append_null();
        }
        if !set[2] {
            self.double.append_null();
        }
        if !set[3] {
            self.bool_.append_null();
        }
        if !set[4] {
            self.bytes.append_null();
        }
        if !set[5] {
            self.ser.append_null();
        }
        Ok(())
    }

    /// See [`DictColumn::finish`] for why this does not consume the builder.
    pub fn finish(&self) -> Result<RecordBatch> {
        let cols: Vec<ArrayRef> = vec![
            Arc::new(self.parent_id.finish_cloned()),
            Arc::new(self.key.finish_cloned()),
            Arc::new(self.ty.finish_cloned()),
            Arc::new(self.str_.finish_cloned()),
            Arc::new(self.int.finish_cloned()),
            Arc::new(self.double.finish_cloned()),
            Arc::new(self.bool_.finish_cloned()),
            Arc::new(self.bytes.finish_cloned()),
            Arc::new(self.ser.finish_cloned()),
        ];
        Ok(RecordBatch::try_new(ATTRS.clone(), cols)?)
    }
}

/// The `str` column of an attribute table, read through its dictionary.
///
/// Five call sites — the block index, the zone index, the filter, the JSON
/// encoder and the `service.name` scan — all want the same thing: the text of
/// row *n*. The dictionary makes that two indirections instead of one, so it is
/// resolved here rather than five times, and the column position lives in one
/// place with it.
///
/// `value` on a null row reads whatever key byte is under it, exactly as
/// `StringArray::value` did before. Every caller reaches it through
/// `type == AttrType::Str`, which is only written alongside a value.
pub struct StrColumn<'a> {
    keys: &'a arrow_array::UInt32Array,
    values: &'a arrow_array::StringArray,
}

impl StrColumn<'_> {
    pub fn value(&self, row: usize) -> &str {
        self.values.value(self.keys.value(row) as usize)
    }

    pub fn is_valid(&self, row: usize) -> bool {
        self.keys.is_valid(row)
    }
}

/// Read column 3 of any table with the [`ATTRS`] shape.
pub fn str_column(b: &RecordBatch) -> StrColumn<'_> {
    str_values(b.column(3))
}

/// The same, for the one caller that reaches the column by name because the
/// table it holds may be missing it — see `frame::resource_names`.
pub fn str_values(col: &dyn Array) -> StrColumn<'_> {
    use arrow_array::cast::AsArray;
    let d = col.as_dictionary::<UInt32Type>();
    StrColumn {
        keys: d.keys(),
        values: d.values().as_string::<i32>(),
    }
}

/// Build a block's [`crate::bloom::ATTR_IDX`] over every attribute table in it.
///
/// Driven off the schema rather than off a list of table names, so a signal that
/// grows a fourth attribute level gets covered without anyone remembering to add
/// it here — and forgetting would not be a slow query, it would be a block
/// wrongly skipped.
///
/// `None` when the block has no attributes at all or too many distinct ones; the
/// reader treats a missing file as "scan me", so both are safe.
pub fn index(tables: &[(&'static str, RecordBatch)]) -> Option<Vec<u8>> {
    let mut keys = crate::bloom::Keys::default();
    for (_, b) in tables {
        if Arc::ptr_eq(&b.schema(), &ATTRS) {
            index_table(&mut keys, b);
        }
    }
    keys.build()
}

fn index_table(keys: &mut crate::bloom::Keys, b: &RecordBatch) {
    use arrow_array::cast::AsArray;
    use arrow_array::types::{Int64Type, UInt8Type};

    let dict = b.column(1).as_dictionary::<UInt16Type>();
    let names = dict.values().as_string::<i32>();
    let codes = dict.keys().values();
    let types = b.column(2).as_primitive::<UInt8Type>().values();
    let strs = str_column(b);
    let ints = b.column(4).as_primitive::<Int64Type>();
    let bools = b.column(6).as_boolean();

    const STR: u8 = AttrType::Str as u8;
    const INT: u8 = AttrType::Int as u8;
    const DOUBLE: u8 = AttrType::Double as u8;
    const BOOL: u8 = AttrType::Bool as u8;

    // Reused across rows so the common case — a value that is already text —
    // costs no allocation at all.
    let mut buf = String::new();
    for row in 0..b.num_rows() {
        let name = names.value(codes[row] as usize);
        let text: &str = match types[row] {
            STR => strs.value(row),
            INT => {
                buf.clear();
                use std::fmt::Write;
                let _ = write!(buf, "{}", ints.value(row));
                &buf
            }
            BOOL => {
                if bools.value(row) {
                    "true"
                } else {
                    "false"
                }
            }
            DOUBLE => {
                keys.flag(crate::bloom::HAS_DOUBLE);
                continue;
            }
            // Empty, Bytes, Slice and Map are not comparable by any operator the
            // query layer offers, so no query can be pruned wrongly by leaving
            // them out — and indexing them would only add false positives.
            _ => continue,
        };
        keys.insert(crate::bloom::attr_hash(name, text.as_bytes()));
    }
}

/// The `resources` + `resource_attrs` + `scope_attrs` arms of the star, shared
/// by every signal.
///
/// Interning is keyed on the canonical protobuf encoding of the `Resource` /
/// `InstrumentationScope` message. Storing the bytes rather than a hash means no
/// collision risk, and there are only a handful of distinct resources per block
/// so the memory is irrelevant.
///
/// Deliberately block-local. A process-wide resource dictionary would be shared
/// mutable state on the ingest hot path, and it would break TTL-by-directory-drop
/// — a block whose resource rows lived somewhere else could not be deleted by
/// unlinking it.
pub struct ResourceScope {
    resources: HashMap<Vec<u8>, u16>,
    scopes: HashMap<Vec<u8>, u16>,
    res_id: UInt16Builder,
    res_key: UInt64Builder,
    res_dropped: UInt32Builder,
    pub resource_attrs: AttrsBuilder,
    pub scope_attrs: AttrsBuilder,
}

impl Default for ResourceScope {
    fn default() -> Self {
        Self::new()
    }
}

impl ResourceScope {
    pub fn new() -> Self {
        Self {
            resources: HashMap::new(),
            scopes: HashMap::new(),
            res_id: UInt16Builder::new(),
            res_key: UInt64Builder::new(),
            res_dropped: UInt32Builder::new(),
            resource_attrs: AttrsBuilder::new("resource_attrs.key"),
            scope_attrs: AttrsBuilder::new("scope_attrs.key"),
        }
    }

    /// Whether this block can take `resources` more distinct resources,
    /// `scopes` more distinct scopes, and their attributes.
    pub fn has_headroom(
        &self,
        resources: usize,
        scopes: usize,
        res_kv: usize,
        scope_kv: usize,
    ) -> bool {
        self.resources.len() + resources <= DICT_CAP
            && self.scopes.len() + scopes <= DICT_CAP
            && self.resource_attrs.has_headroom(res_kv)
            && self.scope_attrs.has_headroom(scope_kv)
    }

    /// Rows across all three tables, for the seal-size estimate.
    pub fn len(&self) -> usize {
        self.resource_attrs.len() + self.scope_attrs.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn heap_bytes(&self) -> usize {
        self.resource_attrs.heap_bytes() + self.scope_attrs.heap_bytes()
    }

    pub fn resource(&mut self, res: Option<&Resource>) -> Result<u16> {
        let key = res.map(|r| r.encode_to_vec()).unwrap_or_default();
        if let Some(&id) = self.resources.get(&key) {
            return Ok(id);
        }
        // resource_id is UInt16 to keep the root table narrow. Overflow means
        // "seal this block", never "drop data".
        let id = u16::try_from(self.resources.len())
            .map_err(|_| Error::DictionaryFull("resource_id"))?;
        self.resources.insert(key, id);

        let attrs = res.map(|r| r.attributes.as_slice()).unwrap_or_default();
        self.res_id.append_value(id);
        self.res_key.append_value(resource_key(attrs));
        self.res_dropped
            .append_value(res.map(|r| r.dropped_attributes_count).unwrap_or(0));
        self.resource_attrs.append_all(id as u32, attrs)?;
        Ok(id)
    }

    pub fn scope(&mut self, scope: Option<&InstrumentationScope>) -> Result<u16> {
        let key = scope.map(|s| s.encode_to_vec()).unwrap_or_default();
        if let Some(&id) = self.scopes.get(&key) {
            return Ok(id);
        }
        let id = u16::try_from(self.scopes.len()).map_err(|_| Error::DictionaryFull("scope_id"))?;
        self.scopes.insert(key, id);
        if let Some(s) = scope {
            self.scope_attrs.append_all(id as u32, &s.attributes)?;
            // Scope name/version are not attributes on the wire, but modelling
            // them as such means one table and one join path instead of two.
            for (k, v) in [
                ("otel.scope.name", &s.name),
                ("otel.scope.version", &s.version),
            ] {
                if !v.is_empty() {
                    let value = AnyValue {
                        value: Some(Value::StringValue(v.clone())),
                    };
                    self.scope_attrs.append(id as u32, k, Some(&value))?;
                }
            }
        }
        Ok(id)
    }

    /// The three tables, in the order every signal's block lists them:
    /// `resources`, `resource_attrs`, `scope_attrs`.
    pub fn finish(&self) -> Result<[(&'static str, RecordBatch); 3]> {
        let cols: Vec<ArrayRef> = vec![
            Arc::new(self.res_id.finish_cloned()),
            Arc::new(self.res_key.finish_cloned()),
            Arc::new(self.res_dropped.finish_cloned()),
        ];
        let resources = RecordBatch::try_new(RESOURCES.clone(), cols)?;
        Ok([
            ("resources", resources),
            ("resource_attrs", self.resource_attrs.finish()?),
            ("scope_attrs", self.scope_attrs.finish()?),
        ])
    }
}

/// Count the attributes a request will contribute, for a headroom check.
///
/// Scope contributes `attributes.len() + 2` because `otel.scope.name` and
/// `.version` are synthesised into the attribute table.
pub fn scope_kv(scope: Option<&InstrumentationScope>) -> usize {
    scope.map_or(0, |s| s.attributes.len()) + 2
}

pub fn resource_kv(res: Option<&Resource>) -> usize {
    res.map_or(0, |r| r.attributes.len())
}

#[cfg(test)]
mod tests {
    use super::*;
    use arrow_array::Array;
    use arrow_array::cast::AsArray;
    use arrow_array::types::UInt8Type;
    use mira_proto::common::v1::{ArrayValue, KeyValueList};

    fn any(v: Value) -> AnyValue {
        AnyValue { value: Some(v) }
    }

    /// Every `AnyValue` OTLP can put on the wire, and the invariant the whole
    /// EAV table rests on: `type` names exactly one of the six value columns,
    /// that column is the only non-null one on the row, and all nine columns
    /// end the same length.
    ///
    /// Getting this wrong is not a crash — it is a reader that returns a
    /// neighbouring row's scalar for an attribute, or a `RecordBatch::try_new`
    /// at seal that takes the whole block with it.
    #[test]
    fn every_any_value_variant_fills_exactly_the_column_its_type_names() {
        let mut b = AttrsBuilder::new("log_attrs.key");
        assert!(b.is_empty(), "a fresh table has no rows");

        let nested = KeyValueList {
            values: vec![KeyValue {
                key: "inner".into(),
                value: Some(any(Value::IntValue(1))),
            }],
        };
        // (key, value, expected type, expected non-null value column)
        //
        // `unset` is the one proto3 cannot spell twice: a `KeyValue` with no
        // `value` at all and one whose `AnyValue` is empty are the same fact,
        // and both have to store as `Empty` rather than as an empty string.
        let rows: [(&str, Option<AnyValue>, AttrType, Option<usize>); 9] = [
            ("absent", None, AttrType::Empty, None),
            (
                "unset",
                Some(AnyValue { value: None }),
                AttrType::Empty,
                None,
            ),
            (
                "str",
                Some(any(Value::StringValue("s".into()))),
                AttrType::Str,
                Some(3),
            ),
            (
                "int",
                Some(any(Value::IntValue(-7))),
                AttrType::Int,
                Some(4),
            ),
            (
                "double",
                Some(any(Value::DoubleValue(0.5))),
                AttrType::Double,
                Some(5),
            ),
            (
                "bool",
                Some(any(Value::BoolValue(true))),
                AttrType::Bool,
                Some(6),
            ),
            (
                "bytes",
                Some(any(Value::BytesValue(vec![0xde, 0xad].into()))),
                AttrType::Bytes,
                Some(7),
            ),
            (
                "slice",
                Some(any(Value::ArrayValue(ArrayValue {
                    values: vec![any(Value::IntValue(1)), any(Value::StringValue("x".into()))],
                }))),
                AttrType::Slice,
                Some(8),
            ),
            (
                "map",
                Some(any(Value::KvlistValue(nested.clone()))),
                AttrType::Map,
                Some(8),
            ),
        ];
        for (i, (key, value, _, _)) in rows.iter().enumerate() {
            b.append(i as u32, key, value.as_ref()).expect("append");
        }
        assert_eq!(b.len(), rows.len());
        assert!(!b.is_empty());

        let batch = b.finish().expect("finish");
        assert_eq!(batch.num_rows(), rows.len());
        let types = batch.column(2).as_primitive::<UInt8Type>();
        for (row, (key, _, ty, col)) in rows.iter().enumerate() {
            assert_eq!(types.value(row), *ty as u8, "{key} stored the wrong type");
            for c in 3..9 {
                assert_eq!(
                    batch.column(c).is_null(row),
                    Some(c) != *col,
                    "{key}: column {c} nullness"
                );
            }
        }
        // A nested value round-trips through the `ser` column, which is what
        // makes an array or a map queryable at all later.
        let ser = batch.column(8).as_binary::<i32>();
        assert_eq!(
            AnyValue::decode(ser.value(8)).expect("ser decodes"),
            any(Value::KvlistValue(nested))
        );
        // The heap accounting the flusher seals on counts the wide columns and
        // nothing else: one string byte, two bytes-value bytes, and the two
        // serialized values.
        assert!(
            b.heap_bytes() > ser.value(8).len(),
            "the seal estimate must see every variable-width heap"
        );
    }

    /// The property the whole `str` dictionary exists for: a value that repeats
    /// is stored once. Asserted on `heap_bytes` and not on the compressed size,
    /// because `heap_bytes` is what decides when a block seals — a builder that
    /// charged every append would seal a block of one repeated GenAI prompt
    /// hundreds of times too early, which is the bug this replaces.
    #[test]
    fn a_repeated_attribute_value_is_stored_once() {
        let prompt = "summarise the incident in one paragraph".repeat(64);
        let mut b = AttrsBuilder::new("log_attrs.key");
        for i in 0..1_000 {
            b.append(
                i,
                "gen_ai.prompt",
                Some(&any(Value::StringValue(prompt.clone()))),
            )
            .unwrap();
        }
        assert_eq!(
            b.heap_bytes(),
            prompt.len(),
            "a thousand copies of one value are one value"
        );

        // And it still reads back as itself through the extra indirection.
        let batch = b.finish().unwrap();
        let strs = str_column(&batch);
        assert_eq!(strs.value(0), prompt);
        assert_eq!(strs.value(999), prompt);
        assert!(strs.is_valid(999));
        assert!(
            matches!(
                batch.column(3).data_type(),
                arrow_schema::DataType::Dictionary(k, _) if **k == arrow_schema::DataType::UInt32,
            ),
            "the key width is the one the reader downcasts to"
        );

        // A distinct value still costs its own bytes, or the count above would
        // pass on a builder that had simply stopped counting.
        b.append(
            0,
            "gen_ai.prompt",
            Some(&any(Value::StringValue("no".into()))),
        )
        .unwrap();
        assert_eq!(b.heap_bytes(), prompt.len() + 2);
    }

    /// The attribute bloom filter, which decides whether a block is opened at
    /// all. A value spelled one way at seal and another at query is a block
    /// wrongly skipped — a query that silently returns fewer rows, which is the
    /// worst failure mode this engine has.
    #[test]
    fn the_attribute_index_spells_every_comparable_value_the_way_a_query_will() {
        let mut b = AttrsBuilder::new("log_attrs.key");
        for (key, v) in [
            ("service.name", any(Value::StringValue("checkout".into()))),
            ("http.status", any(Value::IntValue(503))),
            ("canary", any(Value::BoolValue(true))),
            ("stable", any(Value::BoolValue(false))),
            ("ratio", any(Value::DoubleValue(0.25))),
            ("blob", any(Value::BytesValue(vec![1, 2].into()))),
        ] {
            b.append(0, key, Some(&v)).expect("append");
        }
        b.append(0, "missing", None).expect("append");
        let batch = b.finish().expect("finish");

        let bytes = index(&[("log_attrs", batch)]).expect("an index over seven rows");
        let f = crate::bloom::Filter::open(&bytes).expect("filter header");
        for (key, text) in [
            ("service.name", "checkout"),
            ("http.status", "503"),
            ("canary", "true"),
            ("stable", "false"),
        ] {
            assert!(
                f.may_contain(crate::bloom::attr_hash(key, text.as_bytes())),
                "{key}={text} was indexed as something else"
            );
        }
        // A double is not indexed by value — no textual spelling of one is
        // stable — so the flag is what tells the reader not to trust a miss.
        assert_eq!(f.flags & crate::bloom::HAS_DOUBLE, crate::bloom::HAS_DOUBLE);
        // And a table with nothing comparable in it writes no file at all,
        // which the reader reads as "scan me" rather than as "skip me".
        let mut only_bytes = AttrsBuilder::new("log_attrs.key");
        only_bytes
            .append(0, "blob", Some(&any(Value::BytesValue(vec![9].into()))))
            .expect("append");
        assert!(index(&[("log_attrs", only_bytes.finish().expect("finish"))]).is_none());
    }

    /// Scope name and version are synthesised into the attribute table, so the
    /// key dictionary can overflow on a row the caller never wrote. The
    /// contract when it does is the one the whole seal path depends on: an
    /// error naming the table, and nine columns still the same length, so the
    /// block that is already open is still sealable.
    #[test]
    fn a_full_key_dictionary_is_an_error_that_leaves_the_block_sealable() {
        let mut rs = ResourceScope::default();
        assert!(rs.is_empty(), "a fresh preamble contributes no rows");

        for i in 0..DICT_CAP {
            rs.scope_attrs
                .append(0, &format!("k{i}"), None)
                .expect("headroom");
        }
        assert!(
            !rs.has_headroom(1, 1, 0, 1),
            "the hint must see the ceiling"
        );
        assert!(!rs.is_empty());

        // No attributes of its own: the key that does not fit is the
        // `otel.scope.name` this builder synthesises.
        let scope = InstrumentationScope {
            name: "payments".into(),
            version: "1.2.3".into(),
            ..Default::default()
        };
        let e = rs.scope(Some(&scope)).expect_err("the dictionary is full");
        assert!(matches!(e, Error::DictionaryFull("scope_attrs.key")), "{e}");
        let tables = rs.finish().expect("a full table is still a sealable one");
        assert_eq!(tables[2].0, "scope_attrs");
        assert_eq!(tables[2].1.num_rows(), DICT_CAP);
    }

    /// Interning is on the canonical encoding of the message, so two exports
    /// that describe the same resource share one row and one id — and two that
    /// differ by one attribute do not. Both directions matter: collapsing them
    /// loses the attribute, splitting them makes the root table's
    /// `resource_id` useless as a join key.
    #[test]
    fn identical_resources_and_scopes_intern_to_one_row_and_different_ones_do_not() {
        let mut rs = ResourceScope::new();
        let res = |name: &str| Resource {
            attributes: vec![KeyValue {
                key: "service.name".into(),
                value: Some(any(Value::StringValue(name.into()))),
            }],
            dropped_attributes_count: 0,
            ..Default::default()
        };
        assert_eq!(rs.resource(Some(&res("checkout"))).expect("resource"), 0);
        assert_eq!(rs.resource(Some(&res("checkout"))).expect("resource"), 0);
        assert_eq!(rs.resource(Some(&res("payments"))).expect("resource"), 1);
        // No resource at all is its own interned entry rather than an error.
        assert_eq!(rs.resource(None).expect("resource"), 2);

        let scope = InstrumentationScope {
            name: "tracer".into(),
            ..Default::default()
        };
        assert_eq!(rs.scope(Some(&scope)).expect("scope"), 0);
        assert_eq!(rs.scope(Some(&scope)).expect("scope"), 0);
        assert_eq!(rs.scope(None).expect("scope"), 1);
        // A scope with no version synthesises one key, not two: an empty
        // string is proto3 for absent and would cost a dictionary slot.
        assert_eq!(rs.scope_attrs.len(), 1);
        assert_eq!(
            scope_kv(Some(&scope)),
            2,
            "the hint counts both, on purpose"
        );
        assert_eq!(resource_kv(Some(&res("checkout"))), 1);
        assert_eq!(resource_kv(None), 0);

        let tables = rs.finish().expect("finish");
        let names: Vec<&str> = tables.iter().map(|(n, _)| *n).collect();
        assert_eq!(names, ["resources", "resource_attrs", "scope_attrs"]);
        assert_eq!(tables[0].1.num_rows(), 3, "three distinct resources");
        assert_eq!(tables[1].1.num_rows(), 2, "and two of them carry one attr");
        assert_eq!(rs.len(), 3);
        assert!(rs.heap_bytes() > 0);
    }

    /// Empty is null in a dictionary column, and the ceiling is reported before
    /// the append rather than after it. A builder that let an overflow through
    /// would leave a column one row short of its siblings and fail the seal.
    #[test]
    fn a_dictionary_column_stores_the_unset_string_as_null_and_refuses_to_overflow() {
        let mut d = DictColumn::new("logs.severity_text");
        assert!(d.has_headroom(DICT_CAP));
        d.append("").expect("empty");
        d.append("ERROR").expect("value");
        d.append("ERROR").expect("repeat");
        let col = d.finish();
        assert_eq!(col.len(), 3);
        assert!(col.is_null(0), "proto3's unset string is not a slot");
        assert!(d.has_headroom(DICT_CAP - 1));
        assert!(
            !d.has_headroom(DICT_CAP),
            "one distinct value used, so one fewer fits"
        );
    }
}
