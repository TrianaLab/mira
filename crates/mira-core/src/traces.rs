//! OTLP traces -> Arrow, following the same shape as [`crate::logs`].
//!
//! Nine tables, because a span is not flat. `spans` is the root; `span_events`
//! and `span_links` are child tables carrying their own dense block-local `id`
//! so that `span_event_attrs` and `span_link_attrs` can key on them the same way
//! `span_attrs` keys on a span. The alternative — `List<Struct>` columns for
//! events and links — would need a second, different mechanism to attach
//! attributes to list *elements*, and the whole point of the EAV table is that
//! there is only one.
//!
//! The id story, which is the part that is easy to get wrong: `id` and
//! `parent_id` are rebased to be dense within this block, because they name rows
//! in these files. `trace_id`, `span_id` and a link's target ids are **not**
//! rebased, because they name things outside it — usually on another node
//! entirely. That distinction is what makes a join inside a block
//! unconditionally correct without a partition discriminant.

use arrow_array::builder::{
    FixedSizeBinaryBuilder, StringBuilder, TimestampNanosecondBuilder, UInt8Builder, UInt16Builder,
    UInt32Builder, UInt64Builder,
};
use arrow_array::{ArrayRef, RecordBatch};
use std::sync::Arc;

use mira_proto::collector::trace::v1::ExportTraceServiceRequest;
use mira_proto::trace::v1::Span;

use crate::attrs::{AttrsBuilder, DictColumn, ResourceScope, resource_kv, scope_kv};
use crate::error::Result;
use crate::logs::append_fixed;
use crate::schema::{SPAN_EVENTS, SPAN_LINKS, SPANS};
use crate::signal::{Sealed, SignalBuilder};

pub struct TracesBuilder {
    id: UInt32Builder,
    trace_id: FixedSizeBinaryBuilder,
    span_id: FixedSizeBinaryBuilder,
    parent_span_id: FixedSizeBinaryBuilder,
    trace_state: StringBuilder,
    flags: UInt32Builder,
    name: DictColumn,
    kind: UInt8Builder,
    start: TimestampNanosecondBuilder,
    duration: UInt64Builder,
    status_code: UInt8Builder,
    status_message: StringBuilder,
    dropped_attrs: UInt32Builder,
    dropped_events: UInt32Builder,
    dropped_links: UInt32Builder,
    resource_id: UInt16Builder,
    scope_id: UInt16Builder,
    span_attrs: AttrsBuilder,

    ev_id: UInt32Builder,
    ev_parent: UInt32Builder,
    ev_time: TimestampNanosecondBuilder,
    ev_name: DictColumn,
    ev_dropped: UInt32Builder,
    event_attrs: AttrsBuilder,
    next_event_id: u32,

    ln_id: UInt32Builder,
    ln_parent: UInt32Builder,
    ln_trace_id: FixedSizeBinaryBuilder,
    ln_span_id: FixedSizeBinaryBuilder,
    ln_trace_state: StringBuilder,
    ln_flags: UInt32Builder,
    ln_dropped: UInt32Builder,
    link_attrs: AttrsBuilder,
    next_link_id: u32,

    rs: ResourceScope,
    next_id: u32,
    min_ts: i64,
    max_ts: i64,
}

impl Default for TracesBuilder {
    fn default() -> Self {
        Self::new()
    }
}

impl TracesBuilder {
    pub fn new() -> Self {
        Self {
            id: UInt32Builder::new(),
            trace_id: FixedSizeBinaryBuilder::new(16),
            span_id: FixedSizeBinaryBuilder::new(8),
            parent_span_id: FixedSizeBinaryBuilder::new(8),
            trace_state: StringBuilder::new(),
            flags: UInt32Builder::new(),
            name: DictColumn::new("spans.name"),
            kind: UInt8Builder::new(),
            start: TimestampNanosecondBuilder::new(),
            duration: UInt64Builder::new(),
            status_code: UInt8Builder::new(),
            status_message: StringBuilder::new(),
            dropped_attrs: UInt32Builder::new(),
            dropped_events: UInt32Builder::new(),
            dropped_links: UInt32Builder::new(),
            resource_id: UInt16Builder::new(),
            scope_id: UInt16Builder::new(),
            span_attrs: AttrsBuilder::new("span_attrs.key"),

            ev_id: UInt32Builder::new(),
            ev_parent: UInt32Builder::new(),
            ev_time: TimestampNanosecondBuilder::new(),
            ev_name: DictColumn::new("span_events.name"),
            ev_dropped: UInt32Builder::new(),
            event_attrs: AttrsBuilder::new("span_event_attrs.key"),
            next_event_id: 0,

            ln_id: UInt32Builder::new(),
            ln_parent: UInt32Builder::new(),
            ln_trace_id: FixedSizeBinaryBuilder::new(16),
            ln_span_id: FixedSizeBinaryBuilder::new(8),
            ln_trace_state: StringBuilder::new(),
            ln_flags: UInt32Builder::new(),
            ln_dropped: UInt32Builder::new(),
            link_attrs: AttrsBuilder::new("span_link_attrs.key"),
            next_link_id: 0,

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

    /// Same accounting as [`crate::logs::LogsBuilder::approx_bytes`]: fixed-width
    /// columns estimated from row counts, variable-width heaps measured. A span
    /// is wider than a log record, and events and links are rows of their own.
    pub fn approx_bytes(&self) -> usize {
        self.next_id as usize * 96
            + self.next_event_id as usize * 24
            + self.next_link_id as usize * 48
            + (self.span_attrs.len()
                + self.event_attrs.len()
                + self.link_attrs.len()
                + self.rs.len())
                * 48
            + self.trace_state.values_slice().len()
            + self.status_message.values_slice().len()
            + self.ln_trace_state.values_slice().len()
            + self.span_attrs.heap_bytes()
            + self.event_attrs.heap_bytes()
            + self.link_attrs.heap_bytes()
            + self.rs.heap_bytes()
    }

    pub fn has_headroom_for(&self, req: &ExportTraceServiceRequest) -> bool {
        let (mut resources, mut scopes) = (0usize, 0usize);
        let (mut res_kv, mut sc_kv) = (0usize, 0usize);
        // Links are not counted: they have no dictionary column of their own, so
        // the only ceiling they can hit is `link_attrs`.
        let (mut spans, mut events) = (0usize, 0usize);
        let (mut span_kv, mut ev_kv, mut ln_kv) = (0usize, 0usize, 0usize);
        for rs in &req.resource_spans {
            resources += 1;
            res_kv += resource_kv(rs.resource.as_ref());
            for ss in &rs.scope_spans {
                scopes += 1;
                sc_kv += scope_kv(ss.scope.as_ref());
                spans += ss.spans.len();
                for s in &ss.spans {
                    span_kv += s.attributes.len();
                    events += s.events.len();
                    ev_kv += s.events.iter().map(|e| e.attributes.len()).sum::<usize>();
                    ln_kv += s.links.iter().map(|l| l.attributes.len()).sum::<usize>();
                }
            }
        }
        self.rs.has_headroom(resources, scopes, res_kv, sc_kv)
            && self.span_attrs.has_headroom(span_kv)
            && self.event_attrs.has_headroom(ev_kv)
            && self.link_attrs.has_headroom(ln_kv)
            && self.name.has_headroom(spans)
            && self.ev_name.has_headroom(events)
    }

    pub fn append_request(&mut self, req: &ExportTraceServiceRequest) -> Result<usize> {
        let mut added = 0;
        for rs in &req.resource_spans {
            let rid = self.rs.resource(rs.resource.as_ref())?;
            for ss in &rs.scope_spans {
                let sid = self.rs.scope(ss.scope.as_ref())?;
                for span in &ss.spans {
                    self.append_span(span, rid, sid)?;
                    added += 1;
                }
            }
        }
        Ok(added)
    }

    fn append_span(&mut self, s: &Span, rid: u16, sid: u16) -> Result<()> {
        // Fallible first, before `next_id` moves: an overflow here must leave the
        // block sealable and the ids dense. See `AttrsBuilder::append`.
        self.name.append(&s.name)?;

        let id = self.next_id;
        self.next_id += 1;

        let start = s.start_time_unix_nano;
        // A malformed end before start would wrap; a zero end means "still
        // running" in some exporters. Both become a zero duration rather than a
        // 584-year one.
        let duration = s.end_time_unix_nano.saturating_sub(start);
        // Only real timestamps set the block's range. OTLP requires a start time,
        // so a zero is malformed — folding it in would make this block claim to
        // cover the epoch and match every temporal query ever asked.
        if start != 0 {
            self.min_ts = self.min_ts.min(start as i64);
            self.max_ts = self.max_ts.max(start.saturating_add(duration) as i64);
        }

        self.id.append_value(id);
        append_fixed(&mut self.trace_id, &s.trace_id, 16)?;
        append_fixed(&mut self.span_id, &s.span_id, 8)?;
        append_fixed(&mut self.parent_span_id, &s.parent_span_id, 8)?;
        opt_str(&mut self.trace_state, &s.trace_state);
        self.flags.append_value(s.flags);
        // Anything outside the enum is a client bug; recording it as
        // UNSPECIFIED keeps the column a UInt8 and loses nothing real.
        self.kind
            .append_value(u8::try_from(s.kind).unwrap_or(0).min(5));
        self.start.append_value(start as i64);
        self.duration.append_value(duration);
        let (code, msg) = s.status.as_ref().map_or((0, ""), |st| {
            (
                u8::try_from(st.code).unwrap_or(0).min(2),
                st.message.as_str(),
            )
        });
        self.status_code.append_value(code);
        opt_str(&mut self.status_message, msg);
        self.dropped_attrs.append_value(s.dropped_attributes_count);
        self.dropped_events.append_value(s.dropped_events_count);
        self.dropped_links.append_value(s.dropped_links_count);
        self.resource_id.append_value(rid);
        self.scope_id.append_value(sid);

        for e in &s.events {
            self.ev_name.append(&e.name)?;
            let eid = self.next_event_id;
            self.next_event_id += 1;
            self.ev_id.append_value(eid);
            self.ev_parent.append_value(id);
            self.ev_time.append_value(e.time_unix_nano as i64);
            self.ev_dropped.append_value(e.dropped_attributes_count);
            self.event_attrs.append_all(eid, &e.attributes)?;
        }

        for l in &s.links {
            let lid = self.next_link_id;
            self.next_link_id += 1;
            self.ln_id.append_value(lid);
            self.ln_parent.append_value(id);
            append_fixed(&mut self.ln_trace_id, &l.trace_id, 16)?;
            append_fixed(&mut self.ln_span_id, &l.span_id, 8)?;
            opt_str(&mut self.ln_trace_state, &l.trace_state);
            self.ln_flags.append_value(l.flags);
            self.ln_dropped.append_value(l.dropped_attributes_count);
            self.link_attrs.append_all(lid, &l.attributes)?;
        }

        self.span_attrs.append_all(id, &s.attributes)?;
        Ok(())
    }

    /// Seal and reset, including on the error path — see
    /// [`SignalBuilder::finish`].
    pub fn finish(&mut self) -> Result<Sealed> {
        let out = self.seal();
        *self = Self::new();
        out
    }

    fn seal(&mut self) -> Result<Sealed> {
        let trace_ids = self.trace_id.finish();
        // Built from the finished column rather than accumulated per append:
        // the ids are already contiguous here, and a filter built alongside the
        // rows would have to be discarded whenever `finish` fails part-way.
        let trace_idx = crate::bloom::build(&trace_ids);
        let spans: Vec<ArrayRef> = vec![
            Arc::new(self.id.finish()),
            Arc::new(trace_ids),
            Arc::new(self.span_id.finish()),
            Arc::new(self.parent_span_id.finish()),
            Arc::new(self.trace_state.finish()),
            Arc::new(self.flags.finish()),
            self.name.finish(),
            Arc::new(self.kind.finish()),
            Arc::new(self.start.finish()),
            Arc::new(self.duration.finish()),
            Arc::new(self.status_code.finish()),
            Arc::new(self.status_message.finish()),
            Arc::new(self.dropped_attrs.finish()),
            Arc::new(self.dropped_events.finish()),
            Arc::new(self.dropped_links.finish()),
            Arc::new(self.resource_id.finish()),
            Arc::new(self.scope_id.finish()),
        ];
        let events: Vec<ArrayRef> = vec![
            Arc::new(self.ev_id.finish()),
            Arc::new(self.ev_parent.finish()),
            Arc::new(self.ev_time.finish()),
            self.ev_name.finish(),
            Arc::new(self.ev_dropped.finish()),
        ];
        let links: Vec<ArrayRef> = vec![
            Arc::new(self.ln_id.finish()),
            Arc::new(self.ln_parent.finish()),
            Arc::new(self.ln_trace_id.finish()),
            Arc::new(self.ln_span_id.finish()),
            Arc::new(self.ln_trace_state.finish()),
            Arc::new(self.ln_flags.finish()),
            Arc::new(self.ln_dropped.finish()),
        ];

        // Order matches `schema::TRACES_BLOCK_TABLES`; the test pins the two
        // together so a new table cannot be added to one and forgotten in the
        // other.
        let mut tables = vec![
            ("spans", RecordBatch::try_new(SPANS.clone(), spans)?),
            ("span_attrs", self.span_attrs.finish()?),
            (
                "span_events",
                RecordBatch::try_new(SPAN_EVENTS.clone(), events)?,
            ),
            ("span_event_attrs", self.event_attrs.finish()?),
            (
                "span_links",
                RecordBatch::try_new(SPAN_LINKS.clone(), links)?,
            ),
            ("span_link_attrs", self.link_attrs.finish()?),
        ];
        tables.extend(self.rs.finish()?);
        Ok(Sealed {
            num_rows: self.next_id as usize,
            tables,
            sidecars: trace_idx
                .map(|b| vec![(crate::bloom::TRACE_IDX, b)])
                .unwrap_or_default(),
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

impl SignalBuilder for TracesBuilder {
    type Request = ExportTraceServiceRequest;
    const SIGNAL: &'static str = "traces";

    fn has_headroom_for(&self, req: &Self::Request) -> bool {
        TracesBuilder::has_headroom_for(self, req)
    }
    fn append_request(&mut self, req: &Self::Request) -> Result<usize> {
        TracesBuilder::append_request(self, req)
    }
    fn approx_bytes(&self) -> usize {
        TracesBuilder::approx_bytes(self)
    }
    fn is_empty(&self) -> bool {
        TracesBuilder::is_empty(self)
    }
    fn finish(&mut self) -> Result<Sealed> {
        TracesBuilder::finish(self)
    }
}

/// proto3 gives an unset string and an empty one the same representation, and
/// for every one of these columns the distinction does not exist. Null costs a
/// validity bit; an empty string costs an offset entry too.
fn opt_str(b: &mut StringBuilder, v: &str) {
    if v.is_empty() {
        b.append_null();
    } else {
        b.append_value(v);
    }
}
