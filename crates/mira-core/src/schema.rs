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

/// Which `Metric.data` variant a descriptor row carries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum MetricKind {
    /// No `data` oneof set. The descriptor is kept — it still names a metric
    /// somebody's exporter believes in — but it owns no points.
    Unset = 0,
    Gauge = 1,
    Sum = 2,
    Histogram = 3,
    ExponentialHistogram = 4,
    Summary = 5,
}

/// METRICS descriptor table — one row per `Metric` message, not per point.
///
/// Name, unit, kind, temporality and monotonicity are properties of the metric,
/// repeated on every single point by OTLP's nesting. Hoisting them into a
/// descriptor table that a hundred thousand points point at is most of why the
/// split layout below measures 2.34x smaller than one wide point table.
pub static METRICS: LazyLock<SchemaRef> = LazyLock::new(|| {
    Arc::new(Schema::new(vec![
        Field::new("id", DataType::UInt32, false),
        Field::new("name", dict_u16_utf8(), true),
        Field::new("description", DataType::Utf8, true),
        Field::new("unit", dict_u16_utf8(), true),
        Field::new("kind", DataType::UInt8, false),
        Field::new("temporality", DataType::UInt8, false),
        Field::new("is_monotonic", DataType::Boolean, false),
        Field::new("resource_id", DataType::UInt16, false),
        Field::new("scope_id", DataType::UInt16, false),
    ]))
});

fn list_of(t: DataType) -> DataType {
    DataType::List(Arc::new(Field::new_list_field(t, true)))
}

/// Columns every data point table has, in the same order, so that a temporal
/// filter is the same code against any of the four.
///
/// `start_time_unix_nano` is nullable and, importantly, does **not** contribute
/// to the block's time range. For a cumulative metric it is process start, which
/// can be hours or days before the point; folding it in would make every block
/// claim to cover that whole span and destroy time-based pruning for the one
/// signal that needs it most.
fn dp_head(parent: &str) -> Vec<Field> {
    vec![
        Field::new("id", DataType::UInt32, false),
        Field::new(parent, DataType::UInt32, false),
        Field::new("start_time_unix_nano", ts(), true),
        Field::new("time_unix_nano", ts(), false),
        Field::new("flags", DataType::UInt32, true),
    ]
}

/// NUMBER_DP — gauge and sum points.
///
/// `int` and `double` are separate nullable columns rather than one Float64,
/// because OTLP's `as_int` is `sfixed64` and a counter past 2^53 would silently
/// lose its low bits on the way through an f64. Exactly one is set per row.
pub static NUMBER_DP: LazyLock<SchemaRef> = LazyLock::new(|| {
    let mut f = dp_head("metric_id");
    f.push(Field::new("int", DataType::Int64, true));
    f.push(Field::new("double", DataType::Float64, true));
    Arc::new(Schema::new(f))
});

/// HIST_DP — explicit-bucket histogram points.
///
/// `bucket_counts` stays a `List<UInt64>` rather than being flattened into a
/// child table: measured, the flat child table is 1.47x the size of the list
/// column, because a child table pays a 4-byte parent id per bucket where the
/// list pays one 4-byte offset per point.
///
/// `bounds_id` points at [`HIST_BOUNDS`]. Every point of a histogram repeats the
/// same bucket boundaries — that is what makes it the same histogram — and
/// interning them measured 1.67x smaller on the point table (410 -> 246 B/row).
pub static HIST_DP: LazyLock<SchemaRef> = LazyLock::new(|| {
    let mut f = dp_head("metric_id");
    f.extend([
        Field::new("count", DataType::UInt64, false),
        Field::new("sum", DataType::Float64, true),
        Field::new("min", DataType::Float64, true),
        Field::new("max", DataType::Float64, true),
        Field::new("bucket_counts", list_of(DataType::UInt64), true),
        Field::new("bounds_id", DataType::UInt32, true),
    ]);
    Arc::new(Schema::new(f))
});

/// HIST_BOUNDS — the interned `explicit_bounds` arrays of this block.
///
/// Tens of rows against hundreds of thousands of points, and the reason
/// [`HIST_DP`] is 1.67x smaller than it would be inline.
pub static HIST_BOUNDS: LazyLock<SchemaRef> = LazyLock::new(|| {
    Arc::new(Schema::new(vec![
        Field::new("id", DataType::UInt32, false),
        Field::new("bounds", list_of(DataType::Float64), false),
    ]))
});

/// EXP_HIST_DP — exponential histogram points.
///
/// No bounds to intern: the buckets are defined by `scale` and `offset`, which
/// is the whole point of the representation.
pub static EXP_HIST_DP: LazyLock<SchemaRef> = LazyLock::new(|| {
    let mut f = dp_head("metric_id");
    f.extend([
        Field::new("count", DataType::UInt64, false),
        Field::new("sum", DataType::Float64, true),
        Field::new("min", DataType::Float64, true),
        Field::new("max", DataType::Float64, true),
        // Spec-bounded to [-10, 20], but stored as sent: silently clamping a
        // malformed scale would misplace every bucket in the point rather than
        // making the point visibly wrong.
        Field::new("scale", DataType::Int32, false),
        Field::new("zero_count", DataType::UInt64, false),
        Field::new("zero_threshold", DataType::Float64, true),
        Field::new("positive_offset", DataType::Int32, false),
        Field::new("positive_counts", list_of(DataType::UInt64), true),
        Field::new("negative_offset", DataType::Int32, false),
        Field::new("negative_counts", list_of(DataType::UInt64), true),
    ]);
    Arc::new(Schema::new(f))
});

/// SUMMARY_DP — the legacy quantile representation, kept because OTLP still
/// carries it out of Prometheus.
pub static SUMMARY_DP: LazyLock<SchemaRef> = LazyLock::new(|| {
    let mut f = dp_head("metric_id");
    f.extend([
        Field::new("count", DataType::UInt64, false),
        Field::new("sum", DataType::Float64, true),
        // Two parallel lists rather than a List<Struct>: every access is
        // "the 0.99 value", which is a lookup in one and an index into the other.
        Field::new("quantile", list_of(DataType::Float64), true),
        Field::new("value", list_of(DataType::Float64), true),
    ]);
    Arc::new(Schema::new(f))
});

/// EXEMPLARS — the bridge from a metric point to the trace that produced it.
///
/// `parent_id` is a data point id, and data point ids are one shared space
/// across all four point tables precisely so that this column needs no
/// discriminant saying which one to look in.
pub static EXEMPLARS: LazyLock<SchemaRef> = LazyLock::new(|| {
    Arc::new(Schema::new(vec![
        Field::new("id", DataType::UInt32, false),
        Field::new("parent_id", DataType::UInt32, false),
        Field::new("time_unix_nano", ts(), false),
        Field::new("int", DataType::Int64, true),
        Field::new("double", DataType::Float64, true),
        Field::new("trace_id", DataType::FixedSizeBinary(16), true),
        Field::new("span_id", DataType::FixedSizeBinary(8), true),
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

/// The tables a metrics block is made of, in publish order.
///
/// Thirteen, which sounds like a lot until you notice that `publish` skips the
/// empty ones: a service exporting only counters writes five of them.
pub const METRICS_BLOCK_TABLES: [&str; 13] = [
    "metrics",
    "metric_attrs",
    "number_dp",
    "hist_dp",
    "hist_bounds",
    "exp_hist_dp",
    "summary_dp",
    "dp_attrs",
    "exemplars",
    "exemplar_attrs",
    "resources",
    "resource_attrs",
    "scope_attrs",
];
