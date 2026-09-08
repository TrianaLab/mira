//! OTLP logs -> Arrow, one allocation-lean pass over the decoded protobuf.
//!
//! The builder accumulates across many export requests and is drained once, at
//! flush. `RecordBatch` is immutable and has no append, so the accumulation
//! lives in Arrow's typed builders (which own growable Vecs) rather than in a
//! `Vec<RecordBatch>` that would need `concat_batches` at flush — that costs
//! roughly 2x peak memory for the duration of the concat.
//!
//! Everything shared with traces and metrics — the attribute tables and the
//! Resource-Scope preamble — lives in [`crate::attrs`]. What is left here is the
//! `logs` root table and nothing else.

use arrow_array::builder::{
    BinaryBuilder, FixedSizeBinaryBuilder, Int32Builder, StringBuilder, StringDictionaryBuilder,
    TimestampNanosecondBuilder, UInt16Builder, UInt32Builder,
};
use arrow_array::types::UInt16Type;
use arrow_array::{ArrayRef, RecordBatch};
use prost::Message;
use std::sync::Arc;

use mira_proto::collector::logs::v1::ExportLogsServiceRequest;
use mira_proto::common::v1::any_value::Value;

use crate::attrs::{AttrsBuilder, ResourceScope, resource_kv, scope_kv};
use crate::error::{Error, Result};
use crate::schema::{DICT_CAP, LOGS};
use crate::signal::{Sealed, SignalBuilder};

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
    rs: ResourceScope,

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
            log_attrs: AttrsBuilder::new("log_attrs.key"),
            rs: ResourceScope::new(),
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
            + (self.log_attrs.len() + self.rs.len()) * 48
            + self.body.values_slice().len()
            + self.body_ser.values_slice().len()
            + self.log_attrs.heap_bytes()
            + self.rs.heap_bytes()
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
        let (mut res_kv, mut sc_kv, mut log_kv) = (0usize, 0usize, 0usize);
        for rl in &req.resource_logs {
            resources += 1;
            res_kv += resource_kv(rl.resource.as_ref());
            for sl in &rl.scope_logs {
                scopes += 1;
                sc_kv += scope_kv(sl.scope.as_ref());
                records += sl.log_records.len();
                log_kv += sl
                    .log_records
                    .iter()
                    .map(|r| r.attributes.len())
                    .sum::<usize>();
            }
        }
        self.rs.has_headroom(resources, scopes, res_kv, sc_kv)
            && self.log_attrs.has_headroom(log_kv)
            && self.n_sev + records <= DICT_CAP
    }

    /// Absorb one OTLP export request. Returns the number of log records added.
    pub fn append_request(&mut self, req: &ExportLogsServiceRequest) -> Result<usize> {
        let mut added = 0;
        for rl in &req.resource_logs {
            let rid = self.rs.resource(rl.resource.as_ref())?;
            for sl in &rl.scope_logs {
                let sid = self.rs.scope(sl.scope.as_ref())?;
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

    /// Seal the accumulated rows into a block and reset for the next one.
    ///
    /// The reset happens on the error path too. `seal` calls `finish` on each
    /// column builder as it goes, so a failure part way through leaves this one
    /// holding columns of unequal length; reusing it would make every subsequent
    /// seal fail identically and the node would reject exports until restarted.
    pub fn finish(&mut self) -> Result<Sealed> {
        let out = self.seal();
        *self = Self::new();
        out
    }

    fn seal(&mut self) -> Result<Sealed> {
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
        let mut tables = vec![
            ("logs", RecordBatch::try_new(LOGS.clone(), cols)?),
            ("log_attrs", self.log_attrs.finish()?),
        ];
        tables.extend(self.rs.finish()?);
        Ok(Sealed {
            num_rows: self.next_id as usize,
            tables,
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
        })
    }
}

impl SignalBuilder for LogsBuilder {
    type Request = ExportLogsServiceRequest;
    const SIGNAL: &'static str = "logs";

    fn has_headroom_for(&self, req: &Self::Request) -> bool {
        LogsBuilder::has_headroom_for(self, req)
    }
    fn append_request(&mut self, req: &Self::Request) -> Result<usize> {
        LogsBuilder::append_request(self, req)
    }
    fn approx_bytes(&self) -> usize {
        LogsBuilder::approx_bytes(self)
    }
    fn is_empty(&self) -> bool {
        LogsBuilder::is_empty(self)
    }
    fn finish(&mut self) -> Result<Sealed> {
        LogsBuilder::finish(self)
    }
}

/// OTLP leaves trace_id/span_id empty when unset; anything that is neither
/// empty nor the exact width is malformed and becomes null rather than an error
/// — a bad id must not cost the caller the whole export.
pub(crate) fn append_fixed(b: &mut FixedSizeBinaryBuilder, v: &[u8], width: usize) -> Result<()> {
    if v.len() == width {
        b.append_value(v)?;
    } else {
        b.append_null();
    }
    Ok(())
}
