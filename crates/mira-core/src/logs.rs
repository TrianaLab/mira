//! OTLP logs -> Arrow, one allocation-lean pass over the decoded protobuf.
//!
//! The builder accumulates across many export requests and is drained once, at
//! flush. `RecordBatch` is immutable and has no append, so the accumulation
//! lives in Arrow's typed builders (which own growable Vecs) rather than in a
//! `Vec<RecordBatch>` that would need `concat_batches` at flush — that costs
//! roughly 2x peak memory for the duration of the concat.

use std::collections::HashMap;

use arrow_array::builder::{
    ArrayBuilder, BinaryBuilder, BooleanBuilder, FixedSizeBinaryBuilder, Float64Builder,
    Int32Builder, Int64Builder, StringBuilder, StringDictionaryBuilder, TimestampNanosecondBuilder,
    UInt8Builder, UInt16Builder, UInt32Builder, UInt64Builder,
};
use arrow_array::types::UInt16Type;
use arrow_array::{ArrayRef, RecordBatch};
use prost::Message;
use std::sync::Arc;

use mira_proto::collector::logs::v1::ExportLogsServiceRequest;
use mira_proto::common::v1::{AnyValue, KeyValue, any_value::Value};

use crate::error::{Error, Result};
use crate::identity::resource_key;
use crate::schema::{ATTRS, AttrType, LOGS, RESOURCES};

/// How many distinct values a `UInt16` dictionary can hold. Reaching it is a
/// signal to seal the block, never to fail an export — see
/// [`LogsBuilder::has_headroom_for`].
pub const DICT_CAP: usize = u16::MAX as usize + 1;

/// Builder for one of the three attribute tables.
struct AttrsBuilder {
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
    fn new() -> Self {
        Self {
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

    fn len(&self) -> usize {
        self.ty.len()
    }

    /// Bytes held in the variable-width value heaps. Fixed-width columns are
    /// estimated from the row count; these cannot be, because one row can be a
    /// 32 KB GenAI prompt.
    fn heap_bytes(&self) -> usize {
        self.str_.values_slice().len()
            + self.bytes.values_slice().len()
            + self.ser.values_slice().len()
    }

    /// Append every attribute of `kvs` as rows pointing at `parent_id`.
    fn append_all(&mut self, parent_id: u32, kvs: &[KeyValue]) -> Result<()> {
        for kv in kvs {
            self.append(parent_id, &kv.key, kv.value.as_ref())?;
        }
        Ok(())
    }

    fn append(&mut self, parent_id: u32, key: &str, value: Option<&AnyValue>) -> Result<()> {
        // The dictionary goes first because it is the only fallible step here.
        // Every append below it is infallible, so an overflow leaves all nine
        // columns the same length and the block is still sealable. A half-written
        // row would fail `RecordBatch::try_new` at flush and take the whole block
        // with it.
        let k = self
            .key
            .append(key)
            .map_err(|_| Error::DictionaryFull("attrs.key"))?;
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

    fn finish(&mut self) -> Result<RecordBatch> {
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

/// A sealed logs block: the Arrow batches plus the pruning keys that go into
/// the block's directory name.
pub struct LogsBlock {
    pub logs: RecordBatch,
    pub log_attrs: RecordBatch,
    pub resources: RecordBatch,
    pub resource_attrs: RecordBatch,
    pub scope_attrs: RecordBatch,
    pub min_ts: i64,
    pub max_ts: i64,
}

impl LogsBlock {
    pub fn tables(&self) -> [(&'static str, &RecordBatch); 5] {
        [
            ("logs", &self.logs),
            ("log_attrs", &self.log_attrs),
            ("resources", &self.resources),
            ("resource_attrs", &self.resource_attrs),
            ("scope_attrs", &self.scope_attrs),
        ]
    }

    pub fn num_rows(&self) -> usize {
        self.logs.num_rows()
    }
}

pub struct LogsBuilder {
    id: UInt32Builder,
    time: TimestampNanosecondBuilder,
    observed: TimestampNanosecondBuilder,
    sev_num: Int32Builder,
    sev_text: StringDictionaryBuilder<UInt16Type>,
    /// Distinct entries in `sev_text`. Twenty-four in practice, unbounded from a
    /// hostile client, so it is counted like any other dictionary.
    n_sev: usize,
    body: StringBuilder,
    body_ser: BinaryBuilder,
    trace_id: FixedSizeBinaryBuilder,
    span_id: FixedSizeBinaryBuilder,
    flags: UInt32Builder,
    dropped: UInt32Builder,
    resource_id: UInt16Builder,
    scope_id: UInt16Builder,

    log_attrs: AttrsBuilder,
    resource_attrs: AttrsBuilder,
    scope_attrs: AttrsBuilder,

    // The `resources` table. One row per interned resource, so these grow by
    // tens per block, not by millions.
    res_id: UInt16Builder,
    res_key: UInt64Builder,
    res_dropped: UInt32Builder,

    // Dedup is keyed on the canonical protobuf encoding of the Resource /
    // InstrumentationScope message. Storing the bytes rather than a hash means
    // no collision risk, and there are only a handful of distinct resources per
    // block so the memory is irrelevant. Deliberately block-local: a global
    // resource dictionary would be shared mutable state on the ingest hot path
    // and would break TTL-by-directory-drop.
    resources: HashMap<Vec<u8>, u16>,
    scopes: HashMap<Vec<u8>, u16>,

    next_id: u32,
    min_ts: i64,
    max_ts: i64,
}

impl Default for LogsBuilder {
    fn default() -> Self {
        Self::new()
    }
}

impl LogsBuilder {
    pub fn new() -> Self {
        Self {
            id: UInt32Builder::new(),
            time: TimestampNanosecondBuilder::new(),
            observed: TimestampNanosecondBuilder::new(),
            sev_num: Int32Builder::new(),
            sev_text: StringDictionaryBuilder::new(),
            n_sev: 0,
            body: StringBuilder::new(),
            body_ser: BinaryBuilder::new(),
            trace_id: FixedSizeBinaryBuilder::new(16),
            span_id: FixedSizeBinaryBuilder::new(8),
            flags: UInt32Builder::new(),
            dropped: UInt32Builder::new(),
            resource_id: UInt16Builder::new(),
            scope_id: UInt16Builder::new(),
            log_attrs: AttrsBuilder::new(),
            resource_attrs: AttrsBuilder::new(),
            scope_attrs: AttrsBuilder::new(),
            res_id: UInt16Builder::new(),
            res_key: UInt64Builder::new(),
            res_dropped: UInt32Builder::new(),
            resources: HashMap::new(),
            scopes: HashMap::new(),
            next_id: 0,
            min_ts: i64::MAX,
            max_ts: i64::MIN,
        }
    }

    pub fn num_rows(&self) -> usize {
        self.next_id as usize
    }

    pub fn is_empty(&self) -> bool {
        self.next_id == 0
    }

    /// Rough resident cost of the accumulated builders. Used by the flusher to
    /// decide when to seal, so that block size is bounded by bytes rather than
    /// by row count (rows vary by three orders of magnitude in width).
    ///
    /// The variable-width heaps are measured rather than estimated. Counting only
    /// fixed-width columns makes a 32 KB log body weigh the same as a 20-byte one,
    /// which is how a 32 MB target seals a multi-gigabyte block on a GenAI
    /// workload — and resident footprint is one of the four axes.
    pub fn approx_bytes(&self) -> usize {
        self.next_id as usize * 64
            + (self.log_attrs.len() + self.resource_attrs.len() + self.scope_attrs.len()) * 48
            + self.body.values_slice().len()
            + self.body_ser.values_slice().len()
            + self.log_attrs.heap_bytes()
            + self.resource_attrs.heap_bytes()
            + self.scope_attrs.heap_bytes()
    }

    /// Whether `req` is guaranteed to fit without overflowing a `UInt16`
    /// dictionary or id space.
    ///
    /// Deliberately conservative — it assumes every key in the request is new —
    /// because the alternative is discovering the overflow halfway through an
    /// append, and an Arrow builder cannot be rolled back. The flusher seals when
    /// this returns false and puts the request in the next block, so overflow
    /// costs a slightly small block and never costs the caller their data.
    pub fn has_headroom_for(&self, req: &ExportLogsServiceRequest) -> bool {
        let (mut resources, mut scopes, mut records) = (0usize, 0usize, 0usize);
        let (mut res_kv, mut scope_kv, mut log_kv) = (0usize, 0usize, 0usize);
        for rl in &req.resource_logs {
            resources += 1;
            res_kv += rl.resource.as_ref().map_or(0, |r| r.attributes.len());
            for sl in &rl.scope_logs {
                scopes += 1;
                // +2 for the synthesised otel.scope.name / .version rows.
                scope_kv += sl.scope.as_ref().map_or(0, |s| s.attributes.len()) + 2;
                records += sl.log_records.len();
                log_kv += sl
                    .log_records
                    .iter()
                    .map(|r| r.attributes.len())
                    .sum::<usize>();
            }
        }
        self.resources.len() + resources <= DICT_CAP
            && self.scopes.len() + scopes <= DICT_CAP
            && self.n_sev + records <= DICT_CAP
            && self.log_attrs.n_keys + log_kv <= DICT_CAP
            && self.resource_attrs.n_keys + res_kv <= DICT_CAP
            && self.scope_attrs.n_keys + scope_kv <= DICT_CAP
    }

    /// Absorb one OTLP export request. Returns the number of log records added.
    pub fn append_request(&mut self, req: &ExportLogsServiceRequest) -> Result<usize> {
        let mut added = 0;
        for rl in &req.resource_logs {
            let rid = self.intern_resource(rl.resource.as_ref())?;
            for sl in &rl.scope_logs {
                let sid = self.intern_scope(sl.scope.as_ref())?;
                for rec in &sl.log_records {
                    // Same rule as `AttrsBuilder::append`: the one fallible step
                    // in the row runs before anything is written, so a dictionary
                    // overflow cannot leave a half-row behind and poison the
                    // block. It also runs before `next_id` moves, so ids stay
                    // dense — the whole join story rests on that.
                    if !rec.severity_text.is_empty() {
                        let k = self
                            .sev_text
                            .append(&rec.severity_text)
                            .map_err(|_| Error::DictionaryFull("logs.severity_text"))?;
                        self.n_sev = self.n_sev.max(k as usize + 1);
                    } else {
                        self.sev_text.append_null();
                    }

                    let id = self.next_id;
                    self.next_id += 1;

                    // OTLP allows time_unix_nano == 0 ("unknown"); fall back to
                    // observed_time so the block's time range stays meaningful
                    // and the record is still reachable by a temporal query.
                    let t = if rec.time_unix_nano != 0 {
                        rec.time_unix_nano
                    } else {
                        rec.observed_time_unix_nano
                    } as i64;
                    self.min_ts = self.min_ts.min(t);
                    self.max_ts = self.max_ts.max(t);

                    self.id.append_value(id);
                    self.time.append_value(t);
                    if rec.observed_time_unix_nano != 0 {
                        self.observed
                            .append_value(rec.observed_time_unix_nano as i64);
                    } else {
                        self.observed.append_null();
                    }
                    self.sev_num.append_value(rec.severity_number);

                    match rec.body.as_ref().and_then(|b| b.value.as_ref()) {
                        Some(Value::StringValue(s)) => {
                            self.body.append_value(s);
                            self.body_ser.append_null();
                        }
                        Some(_) => {
                            self.body.append_null();
                            self.body_ser
                                .append_value(rec.body.as_ref().unwrap().encode_to_vec());
                        }
                        None => {
                            self.body.append_null();
                            self.body_ser.append_null();
                        }
                    }

                    append_fixed(&mut self.trace_id, &rec.trace_id, 16)?;
                    append_fixed(&mut self.span_id, &rec.span_id, 8)?;

                    self.flags.append_value(rec.flags);
                    self.dropped.append_value(rec.dropped_attributes_count);
                    self.resource_id.append_value(rid);
                    self.scope_id.append_value(sid);

                    self.log_attrs.append_all(id, &rec.attributes)?;
                    added += 1;
                }
            }
        }
        Ok(added)
    }

    fn intern_resource(&mut self, res: Option<&mira_proto::resource::v1::Resource>) -> Result<u16> {
        let key = res.map(|r| r.encode_to_vec()).unwrap_or_default();
        if let Some(&id) = self.resources.get(&key) {
            return Ok(id);
        }
        // resource_id is UInt16 to keep the root table narrow. Overflow means
        // "seal this block", never "drop data".
        let id = u16::try_from(self.resources.len())
            .map_err(|_| Error::DictionaryFull("logs.resource_id"))?;
        self.resources.insert(key, id);

        let attrs = res.map(|r| r.attributes.as_slice()).unwrap_or_default();
        self.res_id.append_value(id);
        self.res_key.append_value(resource_key(attrs));
        self.res_dropped
            .append_value(res.map(|r| r.dropped_attributes_count).unwrap_or(0));
        self.resource_attrs.append_all(id as u32, attrs)?;
        Ok(id)
    }

    fn intern_scope(
        &mut self,
        scope: Option<&mira_proto::common::v1::InstrumentationScope>,
    ) -> Result<u16> {
        let key = scope.map(|s| s.encode_to_vec()).unwrap_or_default();
        if let Some(&id) = self.scopes.get(&key) {
            return Ok(id);
        }
        let id =
            u16::try_from(self.scopes.len()).map_err(|_| Error::DictionaryFull("logs.scope_id"))?;
        self.scopes.insert(key, id);
        if let Some(s) = scope {
            self.scope_attrs.append_all(id as u32, &s.attributes)?;
            // Scope name/version are not attributes on the wire, but modelling
            // them as such means one table and one join path instead of two.
            if !s.name.is_empty() {
                self.scope_attrs.append(
                    id as u32,
                    "otel.scope.name",
                    Some(&AnyValue {
                        value: Some(Value::StringValue(s.name.clone())),
                    }),
                )?;
            }
            if !s.version.is_empty() {
                self.scope_attrs.append(
                    id as u32,
                    "otel.scope.version",
                    Some(&AnyValue {
                        value: Some(Value::StringValue(s.version.clone())),
                    }),
                )?;
            }
        }
        Ok(id)
    }

    /// Seal the accumulated rows into a block and reset for the next one.
    pub fn finish(&mut self) -> Result<LogsBlock> {
        let cols: Vec<ArrayRef> = vec![
            Arc::new(self.id.finish()),
            Arc::new(self.time.finish()),
            Arc::new(self.observed.finish()),
            Arc::new(self.sev_num.finish()),
            Arc::new(self.sev_text.finish()),
            Arc::new(self.body.finish()),
            Arc::new(self.body_ser.finish()),
            Arc::new(self.trace_id.finish()),
            Arc::new(self.span_id.finish()),
            Arc::new(self.flags.finish()),
            Arc::new(self.dropped.finish()),
            Arc::new(self.resource_id.finish()),
            Arc::new(self.scope_id.finish()),
        ];
        let resources = RecordBatch::try_new(
            RESOURCES.clone(),
            vec![
                Arc::new(self.res_id.finish()) as ArrayRef,
                Arc::new(self.res_key.finish()),
                Arc::new(self.res_dropped.finish()),
            ],
        )?;
        let block = LogsBlock {
            logs: RecordBatch::try_new(LOGS.clone(), cols)?,
            log_attrs: self.log_attrs.finish()?,
            resources,
            resource_attrs: self.resource_attrs.finish()?,
            scope_attrs: self.scope_attrs.finish()?,
            min_ts: if self.min_ts == i64::MAX {
                0
            } else {
                self.min_ts
            },
            max_ts: if self.max_ts == i64::MIN {
                0
            } else {
                self.max_ts
            },
        };
        *self = Self::new();
        Ok(block)
    }
}

/// OTLP leaves trace_id/span_id empty when unset; anything that is neither
/// empty nor the exact width is malformed and becomes null rather than an error
/// — a bad id must not cost the caller the whole export.
fn append_fixed(b: &mut FixedSizeBinaryBuilder, v: &[u8], width: usize) -> Result<()> {
    if v.len() == width {
        b.append_value(v)?;
    } else {
        b.append_null();
    }
    Ok(())
}
