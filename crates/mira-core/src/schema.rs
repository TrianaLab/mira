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

/// The tables a logs block is made of, in publish order.
pub const LOGS_BLOCK_TABLES: [&str; 5] = [
    "logs",
    "log_attrs",
    "resources",
    "resource_attrs",
    "scope_attrs",
];
