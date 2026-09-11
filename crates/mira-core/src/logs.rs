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
    BinaryBuilder, FixedSizeBinaryBuilder, Int32Builder, StringBuilder, TimestampNanosecondBuilder,
    UInt16Builder, UInt32Builder,
};
use arrow_array::{ArrayRef, RecordBatch};
use prost::Message;
use std::sync::Arc;

use mira_proto::collector::logs::v1::ExportLogsServiceRequest;
use mira_proto::common::v1::any_value::Value;

use crate::attrs::{AttrsBuilder, DictColumn, ResourceScope, resource_kv, scope_kv};
use crate::error::Result;
use crate::schema::LOGS;
use crate::signal::{Sealed, Sidecars, SignalBuilder};

pub struct LogsBuilder {
    id: UInt32Builder,
    time: TimestampNanosecondBuilder,
    observed: TimestampNanosecondBuilder,
    sev_num: Int32Builder,
    /// Twenty-four distinct values in practice, unbounded from a hostile
    /// client, so it is counted like any other dictionary.
    sev_text: DictColumn,
    /// Set only on OTel Events, and enumerable by definition — an event name
    /// names a schema, so a producer minting a new one per record is already
    /// wrong.
    event_name: DictColumn,
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
            sev_text: DictColumn::new("logs.severity_text"),
            event_name: DictColumn::new("logs.event_name"),
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
            && self.sev_text.has_headroom(records)
            && self.event_name.has_headroom(records)
    }

    /// Absorb one OTLP export request. Returns the number of log records added.
    pub fn append_request(&mut self, req: &ExportLogsServiceRequest) -> Result<usize> {
        let mut added = 0;
        for rl in &req.resource_logs {
            let rid = self.rs.resource(rl.resource.as_ref())?;
            for sl in &rl.scope_logs {
                let sid = self.rs.scope(sl.scope.as_ref())?;
                for rec in &sl.log_records {
                    // Same rule as `AttrsBuilder::append`: the fallible steps of
                    // the row run before anything else is written, so a
                    // dictionary overflow cannot leave a half-row behind and
                    // poison the block. They also run before `next_id` moves, so
                    // ids stay dense — the whole join story rests on that.
                    //
                    // Two dictionaries now, so an overflow of the second does
                    // leave the first one row long. `has_headroom_for` counts
                    // both, and the one caller that skips it — an oversized
                    // request against an empty block — discards the builder on
                    // any error, so neither path can publish an uneven block.
                    self.sev_text.append(&rec.severity_text)?;
                    self.event_name.append(&rec.event_name)?;

                    let id = self.next_id;
                    self.next_id += 1;

                    // Both timestamps are optional on the wire, and `nanos` folds
                    // an unrepresentable one into the same "unset", so this one
                    // chain covers a missing clock and a broken one.
                    // `observed_time_unix_nano` is defined by the spec as the
                    // receiver's own reading, which makes it the fallback OTLP
                    // already names; when even that is absent, the moment we took
                    // delivery is the only fact left. Storing the zero instead
                    // would put the record at the epoch and drag the block's
                    // `min_ts` down with it — and a block claiming to start in
                    // 1970 is opened by every query in retention, so one record
                    // would defeat the pruning the whole design rests on.
                    let observed = nanos(rec.observed_time_unix_nano);
                    let t = match nanos(rec.time_unix_nano) {
                        0 if observed != 0 => observed,
                        0 => now_nanos(),
                        t => t,
                    };
                    self.min_ts = self.min_ts.min(t);
                    self.max_ts = self.max_ts.max(t);

                    self.id.append_value(id);
                    self.time.append_value(t);
                    if observed != 0 {
                        self.observed.append_value(observed);
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
        let out = self.seal(Sidecars::Build);
        *self = Self::new();
        out
    }

    fn seal(&self, sidecars: Sidecars) -> Result<Sealed> {
        // The same trace filter a traces block carries. "The logs for this
        // trace" is the second half of every trace investigation, and it is the
        // half with no useful time bound — you look a trace up because you do
        // not know when it happened. Without this the spans come out of one
        // block and the logs cost a scan of all of retention.
        let trace_ids = self.trace_id.finish_cloned();
        let trace_idx = match sidecars {
            Sidecars::Build => crate::bloom::build(&trace_ids),
            Sidecars::Skip => None,
        };
        let cols: Vec<ArrayRef> = vec![
            Arc::new(self.id.finish_cloned()),
            Arc::new(self.time.finish_cloned()),
            Arc::new(self.observed.finish_cloned()),
            Arc::new(self.sev_num.finish_cloned()),
            self.sev_text.finish(),
            self.event_name.finish(),
            Arc::new(self.body.finish_cloned()),
            Arc::new(self.body_ser.finish_cloned()),
            Arc::new(trace_ids),
            Arc::new(self.span_id.finish_cloned()),
            Arc::new(self.flags.finish_cloned()),
            Arc::new(self.dropped.finish_cloned()),
            Arc::new(self.resource_id.finish_cloned()),
            Arc::new(self.scope_id.finish_cloned()),
        ];
        let mut tables = vec![
            ("logs", RecordBatch::try_new(LOGS.clone(), cols)?),
            ("log_attrs", self.log_attrs.finish()?),
        ];
        tables.extend(self.rs.finish()?);
        Ok(Sealed::with(
            sidecars,
            self.next_id as usize,
            tables,
            self.min_ts,
            self.max_ts,
        )
        .with_sidecar(crate::bloom::TRACE_IDX, trace_idx))
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
    fn snapshot(&self) -> Result<Sealed> {
        self.seal(Sidecars::Skip)
    }
}

/// One OTLP `fixed64` nanosecond timestamp, read into the `Int64` Arrow uses.
///
/// Everything at or above 2^63 is unrepresentable, and it arrives: a skewed
/// clock, an SDK that scales seconds to nanoseconds twice, or one line of curl.
/// Reading it as zero — the same value the wire uses for "unset" — is the only
/// clamp honest at both ends. A raw `as i64` wraps negative, and a negative
/// `min_ts` renders as a directory name starting with `-`, which
/// `block::parse_dir_name` splits into an empty first field and rejects: the
/// block is published, the export is acked as durable, and no query and no
/// retention sweep can ever see it again. Saturating to `i64::MAX` avoids the
/// wrap but pushes `max_ts` past every cutoff `block::expire` will ever
/// compute, which is the same disk leak by a longer route.
///
/// So this follows [`append_fixed`]: a malformed value reads as absent, and the
/// absent-value path each signal already has takes it from there.
pub(crate) fn nanos(t: u64) -> i64 {
    i64::try_from(t).unwrap_or(0)
}

/// The wall clock, read the way the rest of the tree reads it. Used for exactly
/// one thing — a log record that carries no timestamp of any kind — because the
/// receipt time is the only honest stamp left for one, and a stored zero is what
/// makes a block match every query ever asked.
fn now_nanos() -> i64 {
    let since_epoch = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    i64::try_from(since_epoch.as_nanos()).unwrap_or(i64::MAX)
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

#[cfg(test)]
mod tests {
    use super::*;
    use arrow_array::{Array, TimestampNanosecondArray};
    use mira_proto::logs::v1::{LogRecord, ResourceLogs, ScopeLogs};

    fn request(records: Vec<LogRecord>) -> ExportLogsServiceRequest {
        ExportLogsServiceRequest {
            resource_logs: vec![ResourceLogs {
                scope_logs: vec![ScopeLogs {
                    log_records: records,
                    ..Default::default()
                }],
                ..Default::default()
            }],
        }
    }

    fn col(s: &Sealed, name: &str) -> TimestampNanosecondArray {
        s.table("logs")
            .unwrap()
            .column_by_name(name)
            .unwrap()
            .as_any()
            .downcast_ref::<TimestampNanosecondArray>()
            .unwrap()
            .clone()
    }

    /// `time_unix_nano` is a `fixed64` and one line of curl can set it past
    /// 2^63. Cast straight to `i64` it wrapped negative, `min_ts` took it,
    /// `block::dir_name` wrote a directory starting with `-`, and
    /// `block::parse_dir_name` then refused to parse it back — so the block was
    /// acked as durable and never seen again by a query or by retention. This
    /// asserts the whole chain, not just the number.
    #[test]
    fn a_timestamp_past_i64_still_publishes_a_block_the_catalog_can_see() {
        let mut b = LogsBuilder::new();
        b.append_request(&request(vec![
            LogRecord {
                time_unix_nano: u64::MAX,
                observed_time_unix_nano: u64::MAX,
                ..Default::default()
            },
            LogRecord {
                time_unix_nano: 5_000,
                ..Default::default()
            },
        ]))
        .unwrap();
        let sealed = b.finish().unwrap();
        assert_eq!(sealed.min_ts, 5_000);
        assert!(sealed.min_ts >= 0 && sealed.max_ts >= sealed.min_ts);
        // Unrepresentable is unset, so the row keeps the receipt time and the
        // observed column stays null rather than recording a wrapped value.
        assert!(col(&sealed, "time_unix_nano").value(0) > 5_000);
        assert!(col(&sealed, "observed_time_unix_nano").is_null(0));

        let root = std::env::temp_dir().join(format!("mira-ts-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let published = crate::block::publish(&root, "logs", 7, 1, 0, &sealed).unwrap();
        assert_eq!(crate::block::scan(&root, "logs").unwrap(), vec![published]);
        // And retention can reclaim it, which the wrapped name also prevented.
        assert_eq!(crate::block::expire(&root, "logs", i64::MAX).unwrap(), 1);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// `time_unix_nano` is optional and plenty of SDKs leave it unset once
    /// `observed_time_unix_nano` is. Both of them zero used to store a zero,
    /// which put the block's `min_ts` at the epoch and made every query in
    /// retention overlap it — block pruning defeated by one record.
    #[test]
    fn a_record_with_no_clock_falls_back_to_observed_then_to_receipt_time() {
        let before = now_nanos();
        let mut b = LogsBuilder::new();
        assert!(b.is_empty() && b.num_rows() == 0, "nothing appended yet");
        b.append_request(&request(vec![
            LogRecord {
                time_unix_nano: 3_000,
                observed_time_unix_nano: 4_000,
                ..Default::default()
            },
            LogRecord {
                observed_time_unix_nano: 4_000,
                ..Default::default()
            },
            LogRecord::default(),
        ]))
        .unwrap();
        // The flusher sizes and logs a block by this counter, and it counts
        // records rather than requests: one export of three records is three.
        assert_eq!(b.num_rows(), 3);
        assert!(!b.is_empty());
        let sealed = b.finish().unwrap();
        assert_eq!(sealed.table("logs").unwrap().num_rows(), 3);

        let time = col(&sealed, "time_unix_nano");
        assert_eq!(time.value(0), 3_000, "a real clock wins");
        assert_eq!(time.value(1), 4_000, "then the receiver's own reading");
        assert!(time.value(2) >= before, "then the moment we took delivery");
        // The one that matters: no record dragged the range to 1970.
        assert_eq!(sealed.min_ts, 3_000);
        assert_eq!(sealed.max_ts, time.value(2));

        let observed = col(&sealed, "observed_time_unix_nano");
        assert_eq!(observed.value(0), 4_000);
        assert!(observed.is_null(2), "the fallback is not written back");
    }
}
