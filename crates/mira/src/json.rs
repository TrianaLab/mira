//! OTLP/HTTP with a JSON body.
//!
//! JSON is a normative encoding of OTLP, not an extra: the browser SDK emits it,
//! and it is what anyone reaches for with `curl`. The three endpoints in
//! [`crate::receiver`] dispatch on `content-type` and land here.
//!
//! **No new dependency.** The document is parsed by the same `yaml_rust2` loader
//! that reads queries and config (`api::parse`), because YAML 1.2 is a superset
//! of JSON — so this file is only the mapping from a parsed document onto the
//! prost structs. It also means a KYAML body works, which is principle 5 falling
//! out for free rather than being built.
//!
//! **Two places the OTLP JSON mapping is not the canonical protobuf one**, and
//! both are why an off-the-shelf reflective decoder is wrong here:
//!
//! - `trace_id`, `span_id` and `parent_span_id` are **hex**, not base64. A
//!   32-character hex string is also valid base64, so a canonical decoder does
//!   not fail on one — it silently produces 24 bytes of nonsense. Every other
//!   `bytes` field really is base64.
//! - 64-bit integers are strings. `time_unix_nano` arrives as `"1544712660300000000"`
//!   because a JSON number cannot hold it exactly. Numbers are accepted too;
//!   emitters disagree.
//!
//! Everything here is lenient in the directions the spec allows and strict where
//! being lenient would store the wrong bytes: a field name may be either
//! `lowerCamelCase` or the original proto name, an enum may be its name or its
//! number, but an id of the wrong length is an error rather than a truncation.

use bytes::Bytes;
use yaml_rust2::Yaml;

use mira_proto::collector::logs::v1::ExportLogsServiceRequest;
use mira_proto::collector::metrics::v1::ExportMetricsServiceRequest;
use mira_proto::collector::trace::v1::ExportTraceServiceRequest;
use mira_proto::common::v1::{AnyValue, ArrayValue, InstrumentationScope, KeyValue, KeyValueList};
use mira_proto::logs::v1::{LogRecord, ResourceLogs, ScopeLogs};
use mira_proto::metrics::v1::{
    Exemplar, ExponentialHistogram, ExponentialHistogramDataPoint, Gauge, Histogram,
    HistogramDataPoint, Metric, NumberDataPoint, ResourceMetrics, ScopeMetrics, Sum, Summary,
    SummaryDataPoint, exponential_histogram_data_point, metric, number_data_point,
    summary_data_point,
};
use mira_proto::resource::v1::Resource;
use mira_proto::trace::v1::{ResourceSpans, ScopeSpans, Span, Status, span};

type R<T> = Result<T, String>;

// ---------------------------------------------------------------- field access

/// Look up one field.
///
/// proto3 JSON says a decoder must accept both the `lowerCamelCase` name and the
/// original proto name, and real traffic contains both — SDKs send camel, the
/// Collector's `otlpjson` marshaler can send either. Two exact hash probes cost
/// less than one fuzzy scan of the mapping, so both are just tried.
fn f<'a>(y: &'a Yaml, camel: &'static str, snake: &'static str) -> &'a Yaml {
    match &y[camel] {
        Yaml::BadValue => &y[snake],
        v => v,
    }
}

/// Absent, null and "present but empty" are the same thing to every caller here:
/// proto3 has no way to tell them apart on the wire either.
fn list(y: &Yaml) -> &[Yaml] {
    y.as_vec().map_or(&[], Vec::as_slice)
}

fn missing(y: &Yaml) -> bool {
    matches!(y, Yaml::BadValue | Yaml::Null)
}

fn s(y: &Yaml) -> String {
    y.as_str().unwrap_or_default().to_owned()
}

fn boolean(y: &Yaml) -> bool {
    y.as_bool().unwrap_or_default()
}

/// A 64-bit integer, as a JSON number or — the proto3 JSON default, because a
/// double cannot hold one exactly — as a string.
fn int(y: &Yaml, what: &'static str) -> R<i64> {
    match y {
        Yaml::BadValue | Yaml::Null => Ok(0),
        Yaml::Integer(n) => Ok(*n),
        Yaml::String(t) if t.is_empty() => Ok(0),
        Yaml::String(t) => t
            .parse()
            .map_err(|_| format!("{what}: {t:?} is not an integer")),
        other => Err(format!("{what}: expected an integer, got {other:?}")),
    }
}

/// Unsigned 64-bit. Values above `i64::MAX` are legal for `fixed64`/`uint64` and
/// arrive as strings, so they are parsed as `u64` rather than routed through
/// `int`, which would reject them.
fn uint(y: &Yaml, what: &'static str) -> R<u64> {
    match y {
        Yaml::Integer(n) if *n >= 0 => Ok(*n as u64),
        Yaml::String(t) if !t.is_empty() => t
            .parse()
            .map_err(|_| format!("{what}: {t:?} is not an unsigned integer")),
        _ => Ok(int(y, what)?.try_into().unwrap_or_default()),
    }
}

fn u32f(y: &Yaml, what: &'static str) -> R<u32> {
    let n = uint(y, what)?;
    u32::try_from(n).map_err(|_| format!("{what}: {n} does not fit in 32 bits"))
}

fn i32f(y: &Yaml, what: &'static str) -> R<i32> {
    let n = int(y, what)?;
    i32::try_from(n).map_err(|_| format!("{what}: {n} does not fit in 32 bits"))
}

/// A double. proto3 JSON spells the three special values as strings, and lets
/// any number arrive as a string as well.
fn float(y: &Yaml, what: &'static str) -> R<f64> {
    match y {
        Yaml::BadValue | Yaml::Null => Ok(0.0),
        Yaml::Real(_) | Yaml::Integer(_) => Ok(y.as_f64().unwrap_or_default()),
        Yaml::String(t) => match t.as_str() {
            "NaN" => Ok(f64::NAN),
            "Infinity" => Ok(f64::INFINITY),
            "-Infinity" => Ok(f64::NEG_INFINITY),
            "" => Ok(0.0),
            _ => t
                .parse()
                .map_err(|_| format!("{what}: {t:?} is not a number")),
        },
        other => Err(format!("{what}: expected a number, got {other:?}")),
    }
}

/// A trace or span id: lowercase or uppercase hex, exactly `want` bytes.
///
/// Absent and all-zero are both "no id" in OTLP and both reach the encoder as an
/// empty `Bytes`. A *wrong length* is an error instead, because the alternative
/// is storing a truncated id that will never join to anything and never explain
/// why.
fn hex(y: &Yaml, want: usize, what: &'static str) -> R<Bytes> {
    let t = match y {
        Yaml::BadValue | Yaml::Null => return Ok(Bytes::new()),
        Yaml::String(t) if t.is_empty() => return Ok(Bytes::new()),
        Yaml::String(t) => t,
        other => return Err(format!("{what}: expected a hex string, got {other:?}")),
    };
    if t.len() != want * 2 {
        return Err(format!(
            "{what}: expected {} hex characters, got {}",
            want * 2,
            t.len()
        ));
    }
    let mut out = Vec::with_capacity(want);
    let b = t.as_bytes();
    for pair in b.chunks_exact(2) {
        let nib = |c: u8| match c {
            b'0'..=b'9' => Ok(c - b'0'),
            b'a'..=b'f' => Ok(c - b'a' + 10),
            b'A'..=b'F' => Ok(c - b'A' + 10),
            _ => Err(format!("{what}: {t:?} is not hex")),
        };
        out.push(nib(pair[0])? << 4 | nib(pair[1])?);
    }
    Ok(Bytes::from(out))
}

/// Standard base64 with padding, which is what proto3 JSON uses for every
/// `bytes` field that is not an id. URL-safe input is accepted because it costs
/// two match arms and a rejected attribute value is a lost attribute value.
fn base64(y: &Yaml, what: &'static str) -> R<Bytes> {
    let Some(t) = y.as_str() else {
        return Ok(Bytes::new());
    };
    let mut out = Vec::with_capacity(t.len() / 4 * 3);
    let (mut acc, mut bits) = (0u32, 0u32);
    for c in t.bytes() {
        let v = match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'+' | b'-' => 62,
            b'/' | b'_' => 63,
            b'=' | b'\r' | b'\n' => continue,
            _ => return Err(format!("{what}: not base64")),
        };
        acc = acc << 6 | v as u32;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
        }
    }
    Ok(Bytes::from(out))
}

/// An enum, as its proto name or as its number. `names[i]` is the name of value
/// `i`; every OTLP enum reached from here numbers from zero with no gaps.
fn enumerate(y: &Yaml, names: &[&str], what: &'static str) -> R<i32> {
    match y {
        Yaml::BadValue | Yaml::Null => Ok(0),
        Yaml::String(t) => names
            .iter()
            .position(|n| *n == t)
            .map(|i| i as i32)
            // A number in a string is still a number, and some emitters send it.
            .or_else(|| t.parse().ok())
            .ok_or_else(|| format!("{what}: {t:?} is not a known value")),
        other => i32f(other, what),
    }
}

// ------------------------------------------------------------------- common

fn any_value(y: &Yaml) -> R<Option<AnyValue>> {
    use mira_proto::common::v1::any_value::Value;
    if missing(y) {
        return Ok(None);
    }
    // Exactly one key is set, so the first one that is present wins. Checked in
    // declaration order for no reason other than that it reads like the proto.
    let v = if !missing(f(y, "stringValue", "string_value")) {
        Value::StringValue(s(f(y, "stringValue", "string_value")))
    } else if !missing(f(y, "boolValue", "bool_value")) {
        Value::BoolValue(boolean(f(y, "boolValue", "bool_value")))
    } else if !missing(f(y, "intValue", "int_value")) {
        Value::IntValue(int(f(y, "intValue", "int_value"), "intValue")?)
    } else if !missing(f(y, "doubleValue", "double_value")) {
        Value::DoubleValue(float(f(y, "doubleValue", "double_value"), "doubleValue")?)
    } else if !missing(f(y, "arrayValue", "array_value")) {
        let vs = &f(y, "arrayValue", "array_value")["values"];
        Value::ArrayValue(ArrayValue {
            values: list(vs)
                .iter()
                .map(|v| Ok(any_value(v)?.unwrap_or_default()))
                .collect::<R<_>>()?,
        })
    } else if !missing(f(y, "kvlistValue", "kvlist_value")) {
        let vs = &f(y, "kvlistValue", "kvlist_value")["values"];
        Value::KvlistValue(KeyValueList {
            values: key_values(vs)?,
        })
    } else if !missing(f(y, "bytesValue", "bytes_value")) {
        Value::BytesValue(base64(f(y, "bytesValue", "bytes_value"), "bytesValue")?)
    } else {
        // `{}` is a legal AnyValue with no variant set, and OTLP uses it for an
        // attribute whose value the SDK dropped.
        return Ok(Some(AnyValue::default()));
    };
    Ok(Some(AnyValue { value: Some(v) }))
}

fn key_values(y: &Yaml) -> R<Vec<KeyValue>> {
    list(y)
        .iter()
        .map(|kv| {
            Ok(KeyValue {
                key: s(&kv["key"]),
                value: any_value(&kv["value"])?,
            })
        })
        .collect()
}

fn resource(y: &Yaml) -> R<Option<Resource>> {
    if missing(y) {
        return Ok(None);
    }
    Ok(Some(Resource {
        attributes: key_values(&y["attributes"])?,
        dropped_attributes_count: u32f(
            f(y, "droppedAttributesCount", "dropped_attributes_count"),
            "droppedAttributesCount",
        )?,
        // entity_refs is not read by any encoder; decoding it would be storage
        // for something nothing can query.
        ..Default::default()
    }))
}

fn scope(y: &Yaml) -> R<Option<InstrumentationScope>> {
    if missing(y) {
        return Ok(None);
    }
    Ok(Some(InstrumentationScope {
        name: s(&y["name"]),
        version: s(&y["version"]),
        attributes: key_values(&y["attributes"])?,
        dropped_attributes_count: u32f(
            f(y, "droppedAttributesCount", "dropped_attributes_count"),
            "droppedAttributesCount",
        )?,
    }))
}

fn dropped(y: &Yaml, camel: &'static str, snake: &'static str) -> R<u32> {
    u32f(f(y, camel, snake), camel)
}

// --------------------------------------------------------------------- logs

const SEVERITY: [&str; 25] = [
    "SEVERITY_NUMBER_UNSPECIFIED",
    "SEVERITY_NUMBER_TRACE",
    "SEVERITY_NUMBER_TRACE2",
    "SEVERITY_NUMBER_TRACE3",
    "SEVERITY_NUMBER_TRACE4",
    "SEVERITY_NUMBER_DEBUG",
    "SEVERITY_NUMBER_DEBUG2",
    "SEVERITY_NUMBER_DEBUG3",
    "SEVERITY_NUMBER_DEBUG4",
    "SEVERITY_NUMBER_INFO",
    "SEVERITY_NUMBER_INFO2",
    "SEVERITY_NUMBER_INFO3",
    "SEVERITY_NUMBER_INFO4",
    "SEVERITY_NUMBER_WARN",
    "SEVERITY_NUMBER_WARN2",
    "SEVERITY_NUMBER_WARN3",
    "SEVERITY_NUMBER_WARN4",
    "SEVERITY_NUMBER_ERROR",
    "SEVERITY_NUMBER_ERROR2",
    "SEVERITY_NUMBER_ERROR3",
    "SEVERITY_NUMBER_ERROR4",
    "SEVERITY_NUMBER_FATAL",
    "SEVERITY_NUMBER_FATAL2",
    "SEVERITY_NUMBER_FATAL3",
    "SEVERITY_NUMBER_FATAL4",
];

pub fn logs(doc: &Yaml) -> R<ExportLogsServiceRequest> {
    Ok(ExportLogsServiceRequest {
        resource_logs: list(f(doc, "resourceLogs", "resource_logs"))
            .iter()
            .map(|rl| {
                Ok(ResourceLogs {
                    resource: resource(&rl["resource"])?,
                    scope_logs: list(f(rl, "scopeLogs", "scope_logs"))
                        .iter()
                        .map(scope_logs)
                        .collect::<R<_>>()?,
                    schema_url: s(f(rl, "schemaUrl", "schema_url")),
                })
            })
            .collect::<R<_>>()?,
    })
}

fn scope_logs(sl: &Yaml) -> R<ScopeLogs> {
    Ok(ScopeLogs {
        scope: scope(&sl["scope"])?,
        log_records: list(f(sl, "logRecords", "log_records"))
            .iter()
            .map(log_record)
            .collect::<R<_>>()?,
        schema_url: s(f(sl, "schemaUrl", "schema_url")),
    })
}

fn log_record(r: &Yaml) -> R<LogRecord> {
    Ok(LogRecord {
        time_unix_nano: uint(f(r, "timeUnixNano", "time_unix_nano"), "timeUnixNano")?,
        observed_time_unix_nano: uint(
            f(r, "observedTimeUnixNano", "observed_time_unix_nano"),
            "observedTimeUnixNano",
        )?,
        severity_number: enumerate(
            f(r, "severityNumber", "severity_number"),
            &SEVERITY,
            "severityNumber",
        )?,
        severity_text: s(f(r, "severityText", "severity_text")),
        body: any_value(&r["body"])?,
        attributes: key_values(&r["attributes"])?,
        dropped_attributes_count: dropped(r, "droppedAttributesCount", "dropped_attributes_count")?,
        flags: u32f(&r["flags"], "flags")?,
        trace_id: hex(f(r, "traceId", "trace_id"), 16, "traceId")?,
        span_id: hex(f(r, "spanId", "span_id"), 8, "spanId")?,
        event_name: s(f(r, "eventName", "event_name")),
    })
}

// -------------------------------------------------------------------- traces

const SPAN_KIND: [&str; 6] = [
    "SPAN_KIND_UNSPECIFIED",
    "SPAN_KIND_INTERNAL",
    "SPAN_KIND_SERVER",
    "SPAN_KIND_CLIENT",
    "SPAN_KIND_PRODUCER",
    "SPAN_KIND_CONSUMER",
];
const STATUS_CODE: [&str; 3] = ["STATUS_CODE_UNSET", "STATUS_CODE_OK", "STATUS_CODE_ERROR"];

pub fn traces(doc: &Yaml) -> R<ExportTraceServiceRequest> {
    Ok(ExportTraceServiceRequest {
        resource_spans: list(f(doc, "resourceSpans", "resource_spans"))
            .iter()
            .map(|rs| {
                Ok(ResourceSpans {
                    resource: resource(&rs["resource"])?,
                    scope_spans: list(f(rs, "scopeSpans", "scope_spans"))
                        .iter()
                        .map(scope_spans)
                        .collect::<R<_>>()?,
                    schema_url: s(f(rs, "schemaUrl", "schema_url")),
                })
            })
            .collect::<R<_>>()?,
    })
}

fn scope_spans(ss: &Yaml) -> R<ScopeSpans> {
    Ok(ScopeSpans {
        scope: scope(&ss["scope"])?,
        spans: list(&ss["spans"]).iter().map(span).collect::<R<_>>()?,
        schema_url: s(f(ss, "schemaUrl", "schema_url")),
    })
}

fn span(sp: &Yaml) -> R<Span> {
    Ok(Span {
        trace_id: hex(f(sp, "traceId", "trace_id"), 16, "traceId")?,
        span_id: hex(f(sp, "spanId", "span_id"), 8, "spanId")?,
        trace_state: s(f(sp, "traceState", "trace_state")),
        parent_span_id: hex(f(sp, "parentSpanId", "parent_span_id"), 8, "parentSpanId")?,
        flags: u32f(&sp["flags"], "flags")?,
        name: s(&sp["name"]),
        kind: enumerate(&sp["kind"], &SPAN_KIND, "kind")?,
        start_time_unix_nano: uint(
            f(sp, "startTimeUnixNano", "start_time_unix_nano"),
            "startTimeUnixNano",
        )?,
        end_time_unix_nano: uint(
            f(sp, "endTimeUnixNano", "end_time_unix_nano"),
            "endTimeUnixNano",
        )?,
        attributes: key_values(&sp["attributes"])?,
        dropped_attributes_count: dropped(
            sp,
            "droppedAttributesCount",
            "dropped_attributes_count",
        )?,
        events: list(&sp["events"])
            .iter()
            .map(|e| {
                Ok(span::Event {
                    time_unix_nano: uint(f(e, "timeUnixNano", "time_unix_nano"), "timeUnixNano")?,
                    name: s(&e["name"]),
                    attributes: key_values(&e["attributes"])?,
                    dropped_attributes_count: dropped(
                        e,
                        "droppedAttributesCount",
                        "dropped_attributes_count",
                    )?,
                })
            })
            .collect::<R<_>>()?,
        dropped_events_count: dropped(sp, "droppedEventsCount", "dropped_events_count")?,
        links: list(&sp["links"])
            .iter()
            .map(|l| {
                Ok(span::Link {
                    trace_id: hex(f(l, "traceId", "trace_id"), 16, "traceId")?,
                    span_id: hex(f(l, "spanId", "span_id"), 8, "spanId")?,
                    trace_state: s(f(l, "traceState", "trace_state")),
                    attributes: key_values(&l["attributes"])?,
                    dropped_attributes_count: dropped(
                        l,
                        "droppedAttributesCount",
                        "dropped_attributes_count",
                    )?,
                    flags: u32f(&l["flags"], "flags")?,
                })
            })
            .collect::<R<_>>()?,
        dropped_links_count: dropped(sp, "droppedLinksCount", "dropped_links_count")?,
        status: match &sp["status"] {
            y if missing(y) => None,
            y => Some(Status {
                message: s(&y["message"]),
                code: enumerate(&y["code"], &STATUS_CODE, "code")?,
            }),
        },
    })
}

// ------------------------------------------------------------------- metrics

const TEMPORALITY: [&str; 3] = [
    "AGGREGATION_TEMPORALITY_UNSPECIFIED",
    "AGGREGATION_TEMPORALITY_DELTA",
    "AGGREGATION_TEMPORALITY_CUMULATIVE",
];

pub fn metrics(doc: &Yaml) -> R<ExportMetricsServiceRequest> {
    Ok(ExportMetricsServiceRequest {
        resource_metrics: list(f(doc, "resourceMetrics", "resource_metrics"))
            .iter()
            .map(|rm| {
                Ok(ResourceMetrics {
                    resource: resource(&rm["resource"])?,
                    scope_metrics: list(f(rm, "scopeMetrics", "scope_metrics"))
                        .iter()
                        .map(scope_metrics)
                        .collect::<R<_>>()?,
                    schema_url: s(f(rm, "schemaUrl", "schema_url")),
                })
            })
            .collect::<R<_>>()?,
    })
}

fn scope_metrics(sm: &Yaml) -> R<ScopeMetrics> {
    Ok(ScopeMetrics {
        scope: scope(&sm["scope"])?,
        metrics: list(&sm["metrics"])
            .iter()
            .map(metric_of)
            .collect::<R<_>>()?,
        schema_url: s(f(sm, "schemaUrl", "schema_url")),
    })
}

fn temporality(y: &Yaml) -> R<i32> {
    enumerate(
        f(y, "aggregationTemporality", "aggregation_temporality"),
        &TEMPORALITY,
        "aggregationTemporality",
    )
}

fn metric_of(m: &Yaml) -> R<Metric> {
    let data = if !missing(&m["gauge"]) {
        Some(metric::Data::Gauge(Gauge {
            data_points: number_points(&m["gauge"])?,
        }))
    } else if !missing(&m["sum"]) {
        let g = &m["sum"];
        Some(metric::Data::Sum(Sum {
            data_points: number_points(g)?,
            aggregation_temporality: temporality(g)?,
            is_monotonic: boolean(f(g, "isMonotonic", "is_monotonic")),
        }))
    } else if !missing(&m["histogram"]) {
        let g = &m["histogram"];
        Some(metric::Data::Histogram(Histogram {
            data_points: histogram_points(g)?,
            aggregation_temporality: temporality(g)?,
        }))
    } else if !missing(f(m, "exponentialHistogram", "exponential_histogram")) {
        let g = f(m, "exponentialHistogram", "exponential_histogram");
        Some(metric::Data::ExponentialHistogram(ExponentialHistogram {
            data_points: exp_histogram_points(g)?,
            aggregation_temporality: temporality(g)?,
        }))
    } else if !missing(&m["summary"]) {
        Some(metric::Data::Summary(Summary {
            data_points: summary_points(&m["summary"])?,
        }))
    } else {
        // A metric with no data is legal on the wire and encodes to no rows.
        None
    };
    Ok(Metric {
        name: s(&m["name"]),
        description: s(&m["description"]),
        unit: s(&m["unit"]),
        metadata: key_values(&m["metadata"])?,
        data,
    })
}

fn data_points(g: &Yaml) -> &[Yaml] {
    list(f(g, "dataPoints", "data_points"))
}

/// `optional double` in the proto, so absent and `0` are different values and
/// the difference is visible in a histogram.
fn opt_float(y: &Yaml, what: &'static str) -> R<Option<f64>> {
    if missing(y) {
        Ok(None)
    } else {
        Ok(Some(float(y, what)?))
    }
}

fn exemplars(p: &Yaml) -> R<Vec<Exemplar>> {
    list(&p["exemplars"])
        .iter()
        .map(|e| {
            Ok(Exemplar {
                filtered_attributes: key_values(f(e, "filteredAttributes", "filtered_attributes"))?,
                time_unix_nano: uint(f(e, "timeUnixNano", "time_unix_nano"), "timeUnixNano")?,
                value: number_value(e, "exemplar")?.map(|v| match v {
                    number_data_point::Value::AsDouble(d) => {
                        mira_proto::metrics::v1::exemplar::Value::AsDouble(d)
                    }
                    number_data_point::Value::AsInt(i) => {
                        mira_proto::metrics::v1::exemplar::Value::AsInt(i)
                    }
                }),
                span_id: hex(f(e, "spanId", "span_id"), 8, "spanId")?,
                trace_id: hex(f(e, "traceId", "trace_id"), 16, "traceId")?,
            })
        })
        .collect()
}

/// The `asDouble` / `asInt` oneof, shared by `NumberDataPoint` and `Exemplar`.
fn number_value(p: &Yaml, what: &'static str) -> R<Option<number_data_point::Value>> {
    if !missing(f(p, "asDouble", "as_double")) {
        Ok(Some(number_data_point::Value::AsDouble(float(
            f(p, "asDouble", "as_double"),
            what,
        )?)))
    } else if !missing(f(p, "asInt", "as_int")) {
        Ok(Some(number_data_point::Value::AsInt(int(
            f(p, "asInt", "as_int"),
            what,
        )?)))
    } else {
        Ok(None)
    }
}

fn number_points(g: &Yaml) -> R<Vec<NumberDataPoint>> {
    data_points(g)
        .iter()
        .map(|p| {
            Ok(NumberDataPoint {
                attributes: key_values(&p["attributes"])?,
                start_time_unix_nano: uint(
                    f(p, "startTimeUnixNano", "start_time_unix_nano"),
                    "startTimeUnixNano",
                )?,
                time_unix_nano: uint(f(p, "timeUnixNano", "time_unix_nano"), "timeUnixNano")?,
                value: number_value(p, "dataPoint")?,
                exemplars: exemplars(p)?,
                flags: u32f(&p["flags"], "flags")?,
            })
        })
        .collect()
}

fn floats(y: &Yaml, what: &'static str) -> R<Vec<f64>> {
    list(y).iter().map(|v| float(v, what)).collect()
}

fn uints(y: &Yaml, what: &'static str) -> R<Vec<u64>> {
    list(y).iter().map(|v| uint(v, what)).collect()
}

fn histogram_points(g: &Yaml) -> R<Vec<HistogramDataPoint>> {
    data_points(g)
        .iter()
        .map(|p| {
            Ok(HistogramDataPoint {
                attributes: key_values(&p["attributes"])?,
                start_time_unix_nano: uint(
                    f(p, "startTimeUnixNano", "start_time_unix_nano"),
                    "startTimeUnixNano",
                )?,
                time_unix_nano: uint(f(p, "timeUnixNano", "time_unix_nano"), "timeUnixNano")?,
                count: uint(&p["count"], "count")?,
                sum: opt_float(&p["sum"], "sum")?,
                bucket_counts: uints(f(p, "bucketCounts", "bucket_counts"), "bucketCounts")?,
                explicit_bounds: floats(
                    f(p, "explicitBounds", "explicit_bounds"),
                    "explicitBounds",
                )?,
                exemplars: exemplars(p)?,
                flags: u32f(&p["flags"], "flags")?,
                min: opt_float(&p["min"], "min")?,
                max: opt_float(&p["max"], "max")?,
            })
        })
        .collect()
}

fn buckets(y: &Yaml) -> R<Option<exponential_histogram_data_point::Buckets>> {
    if missing(y) {
        return Ok(None);
    }
    Ok(Some(exponential_histogram_data_point::Buckets {
        offset: i32f(&y["offset"], "offset")?,
        bucket_counts: uints(f(y, "bucketCounts", "bucket_counts"), "bucketCounts")?,
    }))
}

fn exp_histogram_points(g: &Yaml) -> R<Vec<ExponentialHistogramDataPoint>> {
    data_points(g)
        .iter()
        .map(|p| {
            Ok(ExponentialHistogramDataPoint {
                attributes: key_values(&p["attributes"])?,
                start_time_unix_nano: uint(
                    f(p, "startTimeUnixNano", "start_time_unix_nano"),
                    "startTimeUnixNano",
                )?,
                time_unix_nano: uint(f(p, "timeUnixNano", "time_unix_nano"), "timeUnixNano")?,
                count: uint(&p["count"], "count")?,
                sum: opt_float(&p["sum"], "sum")?,
                scale: i32f(&p["scale"], "scale")?,
                zero_count: uint(f(p, "zeroCount", "zero_count"), "zeroCount")?,
                positive: buckets(&p["positive"])?,
                negative: buckets(&p["negative"])?,
                flags: u32f(&p["flags"], "flags")?,
                exemplars: exemplars(p)?,
                min: opt_float(&p["min"], "min")?,
                max: opt_float(&p["max"], "max")?,
                zero_threshold: float(f(p, "zeroThreshold", "zero_threshold"), "zeroThreshold")?,
            })
        })
        .collect()
}

fn summary_points(g: &Yaml) -> R<Vec<SummaryDataPoint>> {
    data_points(g)
        .iter()
        .map(|p| {
            Ok(SummaryDataPoint {
                attributes: key_values(&p["attributes"])?,
                start_time_unix_nano: uint(
                    f(p, "startTimeUnixNano", "start_time_unix_nano"),
                    "startTimeUnixNano",
                )?,
                time_unix_nano: uint(f(p, "timeUnixNano", "time_unix_nano"), "timeUnixNano")?,
                count: uint(&p["count"], "count")?,
                sum: float(&p["sum"], "sum")?,
                quantile_values: list(f(p, "quantileValues", "quantile_values"))
                    .iter()
                    .map(|q| {
                        Ok(summary_data_point::ValueAtQuantile {
                            quantile: float(&q["quantile"], "quantile")?,
                            value: float(&q["value"], "value")?,
                        })
                    })
                    .collect::<R<_>>()?,
                flags: u32f(&p["flags"], "flags")?,
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn doc(text: &str) -> Yaml {
        crate::api::parse(text).expect("document")
    }

    /// All five metric kinds in one document, because the five point decoders
    /// share only their scalar helpers and the read path stores them in five
    /// different tables — a mistake in one is invisible from the others.
    ///
    /// The dialects are mixed on purpose: `dataPoints` next to `data_points`,
    /// `"9"` next to `9`, an enum by name next to the same enum by number. The
    /// spec allows all of it within one document and the Collector's marshaler
    /// produces it.
    #[test]
    fn metrics_json_decodes_every_data_point_kind() {
        use mira_proto::metrics::v1::exemplar;

        let m = metrics(&doc(r#"{
          "resourceMetrics": [{
            "resource": {"attributes": [{"key":"service.name","value":{"stringValue":"m"}}],
                         "droppedAttributesCount": 2},
            "scopeMetrics": [{
              "scope": {"name":"s","version":"1",
                        "attributes":[{"key":"a","value":{"intValue":"1"}}]},
              "schemaUrl": "https://schemas/1",
              "metrics": [
                {"name":"g","unit":"By","description":"a gauge",
                 "gauge":{"dataPoints":[{"timeUnixNano":"7","asDouble":1.5,
                   "exemplars":[{"timeUnixNano":"7","asInt":"3",
                                 "traceId":"AABBCCDDEEFF00112233445566778899",
                                 "spanId":"1122334455667788",
                                 "filteredAttributes":[
                                   {"key":"k","value":{"stringValue":"v"}}]}]}]}},
                {"name":"c",
                 "sum":{"data_points":[{"time_unix_nano":8,"as_int":"9","flags":1}],
                        "aggregation_temporality":2,"is_monotonic":true}},
                {"name":"h",
                 "histogram":{"dataPoints":[{"timeUnixNano":"9","count":"3","sum":"6.5",
                   "bucketCounts":["1","2"],"explicitBounds":[2.5],
                   "min":"NaN","max":"Infinity"}],
                   "aggregationTemporality":"AGGREGATION_TEMPORALITY_DELTA"}},
                {"name":"e",
                 "exponentialHistogram":{"dataPoints":[{"timeUnixNano":"10","count":"4",
                   "scale":-1,"zeroCount":"1","zeroThreshold":"1e-9","min":"-Infinity",
                   "positive":{"offset":2,"bucketCounts":["1","3"]}}],
                   "aggregationTemporality":1}},
                {"name":"q",
                 "summary":{"dataPoints":[{"timeUnixNano":"11","count":"5","sum":12.5,
                   "quantileValues":[{"quantile":0.99,"value":42.0}]}]}},
                {"name":"nothing"}
              ]
            }]
          }]
        }"#))
        .unwrap();

        let rm = &m.resource_metrics[0];
        assert_eq!(rm.resource.as_ref().unwrap().dropped_attributes_count, 2);
        let sm = &rm.scope_metrics[0];
        assert_eq!(sm.schema_url, "https://schemas/1");
        assert_eq!(sm.scope.as_ref().unwrap().attributes.len(), 1);
        let ms = &sm.metrics;
        assert_eq!(ms.len(), 6);

        let Some(metric::Data::Gauge(g)) = &ms[0].data else {
            panic!("{:?}", ms[0])
        };
        assert_eq!(ms[0].unit, "By");
        let p = &g.data_points[0];
        assert_eq!(p.value, Some(number_data_point::Value::AsDouble(1.5)));
        let ex = &p.exemplars[0];
        // Hex, uppercase, sixteen bytes. A base64 reader does not fail on a
        // 32-character hex string — it returns twenty-four bytes of nonsense —
        // so this is checked as bytes and not as a length.
        assert_eq!(ex.trace_id[..4], [0xaa, 0xbb, 0xcc, 0xdd]);
        assert_eq!(ex.trace_id.len(), 16);
        assert_eq!(ex.span_id.len(), 8);
        assert_eq!(ex.value, Some(exemplar::Value::AsInt(3)));
        assert_eq!(ex.filtered_attributes[0].key, "k");

        let Some(metric::Data::Sum(sum)) = &ms[1].data else {
            panic!("{:?}", ms[1])
        };
        assert!(sum.is_monotonic);
        assert_eq!(sum.aggregation_temporality, 2);
        let p = &sum.data_points[0];
        assert_eq!(p.time_unix_nano, 8);
        assert_eq!(p.flags, 1);
        // `asInt` and `asDouble` are a oneof, and 9 stored as 9.0 is a silent
        // loss of the producer's choice — the read path has two columns.
        assert_eq!(p.value, Some(number_data_point::Value::AsInt(9)));

        let Some(metric::Data::Histogram(h)) = &ms[2].data else {
            panic!("{:?}", ms[2])
        };
        assert_eq!(h.aggregation_temporality, 1, "DELTA, spelled by name");
        let p = &h.data_points[0];
        assert_eq!(p.count, 3);
        assert_eq!(p.sum, Some(6.5));
        assert_eq!(p.bucket_counts, [1, 2]);
        assert_eq!(p.explicit_bounds, [2.5]);
        // The three specials are strings in proto3 JSON; a number reader that
        // did not know that would either error or store zero.
        assert!(p.min.unwrap().is_nan());
        assert_eq!(p.max, Some(f64::INFINITY));

        let Some(metric::Data::ExponentialHistogram(e)) = &ms[3].data else {
            panic!("{:?}", ms[3])
        };
        let p = &e.data_points[0];
        assert_eq!(p.scale, -1);
        assert_eq!(p.zero_count, 1);
        assert_eq!(p.zero_threshold, 1e-9);
        assert_eq!(p.min, Some(f64::NEG_INFINITY));
        let pos = p.positive.as_ref().unwrap();
        assert_eq!(pos.offset, 2);
        assert_eq!(pos.bucket_counts, [1, 3]);
        // Absent is `None`, not an empty bucket set: one means "no negative
        // side was reported" and the other means "it was, and it was empty".
        assert!(p.negative.is_none());

        let Some(metric::Data::Summary(q)) = &ms[4].data else {
            panic!("{:?}", ms[4])
        };
        let p = &q.data_points[0];
        assert_eq!(p.count, 5);
        assert_eq!(p.sum, 12.5);
        assert_eq!(p.quantile_values[0].quantile, 0.99);
        assert_eq!(p.quantile_values[0].value, 42.0);

        // A metric carrying no data is legal on the wire. It encodes to no rows,
        // which is not the same as being a bad request.
        assert!(ms[5].data.is_none());
    }

    /// The readers under all three signals: lenient in the directions proto3
    /// JSON is, and refusing exactly the inputs whose lenient reading would
    /// store the wrong bytes.
    #[test]
    fn the_scalar_readers_are_strict_only_where_leniency_would_lose_data() {
        let y = |t: &str| doc(&format!("{{\"v\":{t}}}"))["v"].clone();
        let none = Yaml::BadValue;

        // Ids: either case, and a wrong length is an error rather than a
        // truncation, because a truncated id joins to nothing and never says so.
        assert_eq!(
            hex(&y(r#""AaBbCcDd00112233""#), 8, "id").unwrap()[..],
            [0xaa, 0xbb, 0xcc, 0xdd, 0x00, 0x11, 0x22, 0x33]
        );
        assert!(hex(&none, 8, "id").unwrap().is_empty());
        assert!(hex(&y(r#""""#), 8, "id").unwrap().is_empty());
        assert!(
            hex(&y(r#""abcd""#), 8, "id")
                .unwrap_err()
                .contains("expected 16 hex characters, got 4")
        );
        assert!(
            hex(&y(r#""zzzzzzzzzzzzzzzz""#), 8, "id")
                .unwrap_err()
                .contains("is not hex")
        );
        assert!(hex(&y("17"), 8, "id").unwrap_err().contains("hex string"));

        // Every other `bytes` field really is base64, and the URL-safe alphabet
        // costs two match arms against a dropped attribute value.
        assert_eq!(base64(&y(r#""aGVsbG8=""#), "b").unwrap()[..], b"hello"[..]);
        assert_eq!(
            base64(&y(r#""-_8=""#), "b").unwrap()[..],
            [0xfb, 0xff],
            "URL-safe"
        );
        assert!(base64(&y("3"), "b").unwrap().is_empty());
        assert!(base64(&y(r#""!!""#), "b").unwrap_err().contains("base64"));

        // Integers: a JSON number or the proto3 default of a string, and above
        // `i64::MAX` only the unsigned reader is correct.
        assert_eq!(int(&y(r#""-5""#), "n").unwrap(), -5);
        assert_eq!(int(&none, "n").unwrap(), 0);
        assert_eq!(int(&y(r#""""#), "n").unwrap(), 0);
        assert_eq!(
            uint(&y(r#""18446744073709551615""#), "n").unwrap(),
            u64::MAX
        );
        assert_eq!(uint(&y("-1"), "n").unwrap(), 0, "clamped, not wrapped");
        assert!(int(&y("[1]"), "n").unwrap_err().contains("expected an int"));
        assert!(
            int(&y(r#""x""#), "n")
                .unwrap_err()
                .contains("not an integer")
        );
        assert!(u32f(&y(r#""4294967296""#), "n").unwrap_err().contains("32"));
        assert!(i32f(&y(r#""2147483648""#), "n").unwrap_err().contains("32"));

        // Doubles, including the three the spec spells as strings.
        assert_eq!(float(&y("1.5"), "d").unwrap(), 1.5);
        assert_eq!(float(&y(r#""1e-9""#), "d").unwrap(), 1e-9);
        assert_eq!(float(&none, "d").unwrap(), 0.0);
        assert_eq!(float(&y(r#""""#), "d").unwrap(), 0.0);
        assert!(float(&y(r#""NaN""#), "d").unwrap().is_nan());
        assert!(
            float(&y(r#""x""#), "d")
                .unwrap_err()
                .contains("not a number")
        );
        assert!(
            float(&y("[1]"), "d")
                .unwrap_err()
                .contains("expected a num")
        );

        // Enums by name, by number, and by number-in-a-string.
        assert_eq!(
            enumerate(&y(r#""AGGREGATION_TEMPORALITY_DELTA""#), &TEMPORALITY, "t").unwrap(),
            1
        );
        assert_eq!(enumerate(&y("2"), &TEMPORALITY, "t").unwrap(), 2);
        assert_eq!(enumerate(&y(r#""2""#), &TEMPORALITY, "t").unwrap(), 2);
        assert_eq!(enumerate(&none, &TEMPORALITY, "t").unwrap(), 0);
        assert!(
            enumerate(&y(r#""NOPE""#), &TEMPORALITY, "t")
                .unwrap_err()
                .contains("not a known")
        );

        // AnyValue: the two nested kinds, and the empty one OTLP uses for an
        // attribute whose value the SDK dropped.
        let av = any_value(&y(
            r#"{"arrayValue":{"values":[{"stringValue":"a"},{"intValue":"2"},{}]}}"#,
        ))
        .unwrap()
        .unwrap();
        let Some(mira_proto::common::v1::any_value::Value::ArrayValue(a)) = av.value else {
            panic!("{av:?}")
        };
        assert_eq!(a.values.len(), 3);
        assert!(a.values[2].value.is_none(), "`{{}}` is a valid AnyValue");
        let av = any_value(&y(
            r#"{"kvlist_value":{"values":[{"key":"k","value":{"bool_value":true}}]}}"#,
        ))
        .unwrap()
        .unwrap();
        let Some(mira_proto::common::v1::any_value::Value::KvlistValue(l)) = av.value else {
            panic!("{av:?}")
        };
        assert_eq!(l.values[0].key, "k");
        assert!(any_value(&none).unwrap().is_none());
    }
}
