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
    ArrayBuilder, BinaryBuilder, BooleanBuilder, Float64Builder, Int64Builder, StringBuilder,
    StringDictionaryBuilder, UInt8Builder, UInt16Builder, UInt32Builder, UInt64Builder,
};
use arrow_array::types::UInt16Type;
use arrow_array::{ArrayRef, RecordBatch};
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

    pub fn finish(&mut self) -> ArrayRef {
        self.n = 0;
        Arc::new(self.b.finish())
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
    str_: StringBuilder,
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
            str_: StringBuilder::new(),
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
        self.str_.values_slice().len()
            + self.bytes.values_slice().len()
            + self.ser.values_slice().len()
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
                self.str_.append_value(s);
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

    pub fn finish(&mut self) -> Result<RecordBatch> {
        let cols: Vec<ArrayRef> = vec![
            Arc::new(self.parent_id.finish()),
            Arc::new(self.key.finish()),
            Arc::new(self.ty.finish()),
            Arc::new(self.str_.finish()),
            Arc::new(self.int.finish()),
            Arc::new(self.double.finish()),
            Arc::new(self.bool_.finish()),
            Arc::new(self.bytes.finish()),
            Arc::new(self.ser.finish()),
        ];
        Ok(RecordBatch::try_new(ATTRS.clone(), cols)?)
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
                    self.scope_attrs.append(
                        id as u32,
                        k,
                        Some(&AnyValue {
                            value: Some(Value::StringValue(v.clone())),
                        }),
                    )?;
                }
            }
        }
        Ok(id)
    }

    /// The three tables, in the order every signal's block lists them:
    /// `resources`, `resource_attrs`, `scope_attrs`.
    pub fn finish(&mut self) -> Result<[(&'static str, RecordBatch); 3]> {
        let resources = RecordBatch::try_new(
            RESOURCES.clone(),
            vec![
                Arc::new(self.res_id.finish()) as ArrayRef,
                Arc::new(self.res_key.finish()),
                Arc::new(self.res_dropped.finish()),
            ],
        )?;
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
