//! On-disk Arrow schemas. These follow the OTAP star schema (otap-spec.md
//! sections 5.4 and 6.3) rather than a flattened one-row-per-span layout:
//! a root table per signal carrying `resource_id`/`scope_id` foreign keys, plus
//! entity-attribute-value side tables keyed by `parent_id`.
//!
//! Two deliberate deviations from the OTAP wire spec, both documented in
//! docs/ARCHITECTURE.md:
//!
//! * `parent_id` and the root `id` are UInt32 and **block-local**. On the wire
//!   OTAP ids are only unique within one `BatchArrowRecords`; persisting them
//!   verbatim and joining across batches silently produces a cross product.
//!   We rebase to a dense per-block id at ingest, so a join inside a block is
//!   unconditionally correct and needs no partition discriminant column.
//! * Attribute *values* are plain, not dictionary-encoded. Dictionary encoding
//!   is a low-cardinality technique with a hard ceiling (`DictionaryKeyOverflow`
//!   at 2^16 for UInt16 keys); attribute values are the highest-cardinality data
//!   in the system. Only enumerable columns (attribute keys, severity text) are
//!   dictionaries.

use std::sync::{Arc, LazyLock};

use arrow_schema::{DataType, Field, Schema, SchemaRef, TimeUnit};

/// OTAP attribute value discriminant (`type` column). Matches otap-spec.md 5.4.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum AttrType {
    Empty = 0,
    Str = 1,
    Int = 2,
    Double = 3,
    Bool = 4,
    Bytes = 5,
    Slice = 6,
    Map = 7,
}

fn dict_u16_utf8() -> DataType {
    DataType::Dictionary(Box::new(DataType::UInt16), Box::new(DataType::Utf8))
}

/// How many distinct values a `UInt16` dictionary can hold. Lives here because
/// it is a consequence of the key width chosen above, not of any one builder.
/// Reaching it is a signal to seal the block, never to fail an export.
pub const DICT_CAP: usize = u16::MAX as usize + 1;

fn ts() -> DataType {
    DataType::Timestamp(TimeUnit::Nanosecond, None)
}

/// Shared shape for LOG_ATTRS / RESOURCE_ATTRS / SCOPE_ATTRS.
///
/// One schema for all three so a single builder, a single reader and a single
/// semi-join helper cover every attribute level.
pub static ATTRS: LazyLock<SchemaRef> = LazyLock::new(|| {
    Arc::new(Schema::new(vec![
        Field::new("parent_id", DataType::UInt32, false),
        Field::new("key", dict_u16_utf8(), false),
        Field::new("type", DataType::UInt8, false),
        Field::new("str", DataType::Utf8, true),
        Field::new("int", DataType::Int64, true),
        Field::new("double", DataType::Float64, true),
        Field::new("bool", DataType::Boolean, true),
        // `bytes` holds AnyValue::BytesValue. `ser` holds Slice/Map values,
        // serialized. ponytail: OTAP specifies CBOR here; we write the protobuf
        // encoding of the AnyValue instead because we own both ends and it costs
        // zero dependencies. Switch to CBOR when a third party needs to read it.
        Field::new("bytes", DataType::Binary, true),
        Field::new("ser", DataType::Binary, true),
    ]))
});

/// LOGS root table.
///
/// `time_unix_nano` is a plain nanosecond timestamp. Delta-of-delta was in the
/// original brief and is dropped: Arrow IPC has no such encoding (the whole
/// surface is LZ4/ZSTD whole-buffer compression plus Dictionary and RunEndEncoded
/// layouts), and Gorilla's 12x rests on samples landing on exact interval
/// boundaries, which OTLP wall-clock reads do not. See docs/ARCHITECTURE.md.
pub static LOGS: LazyLock<SchemaRef> = LazyLock::new(|| {
    Arc::new(Schema::new(vec![
        Field::new("id", DataType::UInt32, false),
        Field::new("time_unix_nano", ts(), false),
        Field::new("observed_time_unix_nano", ts(), true),
        Field::new("severity_number", DataType::Int32, true),
        Field::new("severity_text", dict_u16_utf8(), true),
        // String bodies, the overwhelmingly common case, land in `body`.
        // Anything else is protobuf-encoded into `body_ser` so nothing is lost.
        Field::new("body", DataType::Utf8, true),
        Field::new("body_ser", DataType::Binary, true),
        Field::new("trace_id", DataType::FixedSizeBinary(16), true),
        Field::new("span_id", DataType::FixedSizeBinary(8), true),
        Field::new("flags", DataType::UInt32, true),
        Field::new("dropped_attributes_count", DataType::UInt32, false),
        Field::new("resource_id", DataType::UInt16, false),
        Field::new("scope_id", DataType::UInt16, false),
    ]))
});

/// RESOURCES table — one row per distinct resource in the block.
///
/// Tiny: tens of rows against hundreds of thousands in the root table. It exists
/// for `key`, the stable cross-block entity identity from [`crate::identity`],
/// which is the join key correlation is built on. `id` is block-local and
/// meaningless outside the block; `key` is neither.
///
/// Note that `id` and `key` are not one-to-one. Two resources whose attribute
/// sets differ only in a non-identifying attribute get two `id`s and one `key` —
/// which is the entire point.
pub static RESOURCES: LazyLock<SchemaRef> = LazyLock::new(|| {
    Arc::new(Schema::new(vec![
        Field::new("id", DataType::UInt16, false),
        Field::new("key", DataType::UInt64, false),
        Field::new("dropped_attributes_count", DataType::UInt32, false),
    ]))
});

/// SPANS root table.
///
/// One deviation from the wire format, and it is the only one: OTLP sends
/// `start_time_unix_nano` and `end_time_unix_nano`; we store the start and a
/// `duration_nano`. Two reasons, and end time is recoverable exactly from the
/// pair either way.
///
/// Duration is what trace search actually filters on — "spans slower than
/// 500ms" is the query every tracing UI opens with — so it deserves to be a
/// column rather than a subtraction across two others. And it compresses:
/// durations are small integers clustered near zero, absolute nanosecond
/// timestamps are 19-digit numbers that share only their high bytes.
///
/// `name` is a dictionary because the semantic conventions require span names
/// to be low-cardinality; if an instrumentation library violates that badly
/// enough to fill 65536 slots, the block seals early and nothing is lost.
pub static SPANS: LazyLock<SchemaRef> = LazyLock::new(|| {
    Arc::new(Schema::new(vec![
        Field::new("id", DataType::UInt32, false),
        Field::new("trace_id", DataType::FixedSizeBinary(16), true),
        Field::new("span_id", DataType::FixedSizeBinary(8), true),
        Field::new("parent_span_id", DataType::FixedSizeBinary(8), true),
        Field::new("trace_state", DataType::Utf8, true),
        Field::new("flags", DataType::UInt32, true),
        Field::new("name", dict_u16_utf8(), true),
        // SpanKind is 0..=5 on the wire in an Int32 field. UInt8 with the
        // out-of-range case clamped to UNSPECIFIED costs 3 bytes a span.
        Field::new("kind", DataType::UInt8, false),
        Field::new("start_time_unix_nano", ts(), false),
        Field::new("duration_nano", DataType::UInt64, false),
        // Status. Split out of the nested message because `code` is the second
        // most-filtered column in the table ("show me the errors") and burying
        // it in a struct costs a child-array indirection on every scan.
        Field::new("status_code", DataType::UInt8, false),
        Field::new("status_message", DataType::Utf8, true),
        Field::new("dropped_attributes_count", DataType::UInt32, false),
        Field::new("dropped_events_count", DataType::UInt32, false),
        Field::new("dropped_links_count", DataType::UInt32, false),
        Field::new("resource_id", DataType::UInt16, false),
        Field::new("scope_id", DataType::UInt16, false),
    ]))
});

/// SPAN_EVENTS — a child table, not a `List<Struct>` column.
///
/// Events carry attributes, and attributes already live in their own EAV table
/// keyed by `parent_id`. A list-of-struct column would need a second, different
/// mechanism to hang attributes off list *elements*; a child table with its own
/// dense `id` reuses the one that exists.
///
/// `id` is this table's own block-local id, distinct from `parent_id`, which
/// points at the span. `span_event_attrs.parent_id` refers to `id` here.
pub static SPAN_EVENTS: LazyLock<SchemaRef> = LazyLock::new(|| {
    Arc::new(Schema::new(vec![
        Field::new("id", DataType::UInt32, false),
        Field::new("parent_id", DataType::UInt32, false),
        Field::new("time_unix_nano", ts(), false),
        Field::new("name", dict_u16_utf8(), true),
        Field::new("dropped_attributes_count", DataType::UInt32, false),
    ]))
});

/// SPAN_LINKS — same child-table reasoning as [`SPAN_EVENTS`].
///
/// A link's `trace_id`/`span_id` point *out* of this block, usually out of this
/// node entirely, so they stay raw ids and are not rebased. That is the
/// distinction the whole id scheme rests on: `parent_id` is block-local because
/// it names a row here, `trace_id` is not because it names something elsewhere.
pub static SPAN_LINKS: LazyLock<SchemaRef> = LazyLock::new(|| {
    Arc::new(Schema::new(vec![
        Field::new("id", DataType::UInt32, false),
        Field::new("parent_id", DataType::UInt32, false),
        Field::new("trace_id", DataType::FixedSizeBinary(16), true),
        Field::new("span_id", DataType::FixedSizeBinary(8), true),
        Field::new("trace_state", DataType::Utf8, true),
        Field::new("flags", DataType::UInt32, true),
        Field::new("dropped_attributes_count", DataType::UInt32, false),
    ]))
});

/// The tables a logs block is made of, in publish order.
pub const LOGS_BLOCK_TABLES: [&str; 5] = [
    "logs",
    "log_attrs",
    "resources",
    "resource_attrs",
    "scope_attrs",
];

/// The tables a traces block is made of, in publish order.
pub const TRACES_BLOCK_TABLES: [&str; 9] = [
    "spans",
    "span_attrs",
    "span_events",
    "span_event_attrs",
    "span_links",
    "span_link_attrs",
    "resources",
    "resource_attrs",
    "scope_attrs",
];
