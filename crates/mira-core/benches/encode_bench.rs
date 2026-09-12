//! What does one core cost to turn OTLP into Arrow, with no I/O anywhere?
//!
//! section 11 scores "ingest throughput per core" and now has a server-side number
//! for it: 891k records/s/core, from 604,166 records/s over the 0.68 cores of
//! CPU the load harness metered. That figure is the right one to compare against
//! another database, and the wrong one to optimise against — it has a WAL write,
//! three receivers, a Tokio runtime and a loopback socket inside it, so a change
//! to the encoder moves it by a fraction of what the change was worth.
//!
//! This bench is the other half. Nothing here opens a file, binds a socket or
//! spawns a thread. What it measures is the three pieces of CPU an export costs
//! between arriving as bytes and being sealed into `RecordBatch`es:
//!
//! | stage | what it is | who pays it |
//! |---|---|---|
//! | `decode` | `prost::Message::decode` of the request body | every export |
//! | `append` | [`SignalBuilder::append_request`] into the open block | every export |
//! | `seal`   | [`SignalBuilder::finish`] — the column-by-column `finish` | once per block |
//!
//! `seal` is amortized over a whole block and is reported that way: per record,
//! against a block filled to the flusher's real `target_block_bytes` of 32 MiB,
//! not against whatever the loop happened to accumulate.
//!
//! ```sh
//! cargo bench -p mira-core --bench encode_bench
//! ```
//!
//! Two things this deliberately does **not** do.
//!
//! It does not assert. `wal_bench` exits non-zero because the WAL has an SLA
//! from a brief; the encoder has a target (section 11: ≥ 1 M records/s/core) that it is
//! not currently expected to meet, and a bench that fails every run is a bench
//! nobody runs. The number is the deliverable.
//!
//! It does not model contention. One thread, one builder, one core — which is
//! the definition of the axis. A real node runs three flushers and N decoders,
//! and the aggregate figure that produces is `loadgen`'s job
//! (docs/internals/e2e.md section 3). Comparing the two is the point of having
//! both, and the two now exist: this bench blends to about 1.02 M
//! records/s/core over the server's half-logs-half-spans mix, against the 778k
//! records/s/core the server itself reaches. The 24% between them is the whole
//! network path, the Tokio runtime and the durability barrier — which is a much
//! smaller tax than it is usually assumed to be, and it means the encoder is
//! where ingest work still pays.
//!
//! **Run it on an idle machine**, for the reason `wal_bench` gives at more
//! length: these are microsecond timings and the tail belongs to whoever else is
//! on the CPU.

use std::time::{Duration, Instant};

use mira_core::SignalBuilder;
use mira_core::logs::LogsBuilder;
use mira_core::metrics::MetricsBuilder;
use mira_core::traces::TracesBuilder;
use mira_proto::collector::logs::v1::ExportLogsServiceRequest;
use mira_proto::collector::metrics::v1::ExportMetricsServiceRequest;
use mira_proto::collector::trace::v1::ExportTraceServiceRequest;
use mira_proto::common::v1::{AnyValue, InstrumentationScope, KeyValue, any_value};
use mira_proto::logs::v1::{LogRecord, ResourceLogs, ScopeLogs};
use mira_proto::metrics::v1::{
    Gauge, Metric, NumberDataPoint, ResourceMetrics, ScopeMetrics, metric, number_data_point,
};
use mira_proto::resource::v1::Resource;
use mira_proto::trace::v1::{ResourceSpans, ScopeSpans, Span, Status, span, status};
use prost::Message;
use prost::bytes::Bytes;

/// Records per export request.
///
/// 8,192 is what `loadgen --batch 8192` sends and therefore what section 11's aggregate
/// row was produced with, so the two numbers are about the same shape of work.
const BATCH: usize = 8_192;

/// The flusher's real seal trigger, from `pipeline::Config::default`. Amortizing
/// `finish` over anything smaller would flatter it.
const TARGET_BLOCK_BYTES: usize = 32 << 20;

/// How many blocks' worth to run per signal.
///
/// Enough that the per-append percentiles have thousands of samples and that
/// `seal` has more than one observation — a single seal is a measurement of one
/// allocator's mood.
const BLOCKS: usize = 4;

fn main() {
    println!(
        "otlp -> arrow, one core, no I/O   batch {BATCH} records, block target {} MiB",
        TARGET_BLOCK_BYTES >> 20
    );
    println!();

    let rows = vec![
        bench::<LogsBuilder>("logs", &logs_request()),
        bench::<TracesBuilder>("traces", &traces_request()),
        bench::<MetricsBuilder>("metrics", &metrics_request()),
    ];

    header();
    for r in &rows {
        r.print();
    }

    println!();
    println!("  decode  prost::Message::decode of the request body");
    println!("  append  SignalBuilder::append_request into the open block");
    println!("  seal    SignalBuilder::finish, amortized over one 32 MiB block");
    println!("  rate    records/s on one core for decode + append + amortized seal");
}

fn header() {
    println!(
        "  {:>8}  {:>7}  {:>9}  {:>9}  {:>9}  {:>9}  {:>13}  {:>10}",
        "signal", "wire B", "decode", "append", "seal/rec", "total/rec", "records/s", "MiB/s"
    );
}

struct Row {
    signal: &'static str,
    /// Encoded protobuf bytes per record, so the MiB/s column is comparable with
    /// section 11's 190.6 MiB/s and with `wal_bench`'s body sizes.
    wire_per_record: f64,
    decode: Duration,
    append: Duration,
    seal_per_record: Duration,
}

impl Row {
    fn total_per_record(&self) -> Duration {
        (self.decode + self.append) / BATCH as u32 + self.seal_per_record
    }

    fn print(&self) {
        let per = self.total_per_record().as_secs_f64();
        let rate = if per > 0.0 { 1.0 / per } else { 0.0 };
        println!(
            "  {:>8}  {:>7.0}  {:>7.3}ms  {:>7.3}ms  {:>7.3}µs  {:>7.3}µs  {:>13}  {:>7.1}",
            self.signal,
            self.wire_per_record,
            ms(self.decode),
            ms(self.append),
            us(self.seal_per_record),
            us(self.total_per_record()),
            thousands(rate as u64),
            rate * self.wire_per_record / (1024.0 * 1024.0),
        );
    }
}

/// Fill `BLOCKS` blocks with the same request over and over, timing each stage.
///
/// The same request every time is deliberate. Varying the payload per iteration
/// would fold a generator into a measurement of an encoder, and the encoder does
/// not branch on values — it branches on *shape*, which is fixed by the request
/// builders below and chosen to exercise every column that costs anything.
fn bench<B: SignalBuilder>(signal: &'static str, req: &B::Request) -> Row
where
    B::Request: Message + Default,
{
    let wire = req.encode_to_vec();
    let wire_per_record = wire.len() as f64 / BATCH as f64;

    // Warm up: the first append pays for every column builder's first
    // allocation and for the dictionary's first insert of each key.
    let mut b = B::default();
    b.append_request(req).expect("warmup append");
    let _ = b.finish().expect("warmup seal");

    let mut decodes = Vec::new();
    let mut appends = Vec::new();
    let mut seals = Vec::new();
    let mut sealed_records = 0usize;

    for _ in 0..BLOCKS {
        let mut b = B::default();
        let mut records = 0usize;
        loop {
            let t = Instant::now();
            let decoded = B::Request::decode(&wire[..]).expect("decode");
            decodes.push(t.elapsed());

            // The flusher's own order: ask before appending, because an Arrow
            // builder cannot be rolled back (section 5). A bench that skipped this
            // would be measuring a path the server never takes.
            if !b.has_headroom_for(&decoded) {
                break;
            }
            let t = Instant::now();
            records += b.append_request(&decoded).expect("append");
            appends.push(t.elapsed());

            if b.approx_bytes() >= TARGET_BLOCK_BYTES {
                break;
            }
        }
        let t = Instant::now();
        let _ = b.finish().expect("seal");
        seals.push(t.elapsed());
        sealed_records += records;
    }

    decodes.sort_unstable();
    appends.sort_unstable();
    let seal_total: Duration = seals.iter().sum();

    Row {
        signal,
        wire_per_record,
        decode: median(&decodes),
        append: median(&appends),
        // Per record over every record that went into a sealed block, which is
        // the only honest denominator: `finish` is O(rows), so quoting it
        // per-block would hide the block size it was measured at.
        seal_per_record: seal_total / sealed_records.max(1) as u32,
    }
}

/// Median rather than a mean, for the reason `wal_bench` picks nearest-rank
/// percentiles: one descheduled sample moves a mean of a few thousand
/// microsecond timings and moves nothing about the encoder.
fn median(sorted: &[Duration]) -> Duration {
    if sorted.is_empty() {
        return Duration::ZERO;
    }
    sorted[sorted.len() / 2]
}

fn ms(d: Duration) -> f64 {
    d.as_secs_f64() * 1000.0
}

fn us(d: Duration) -> f64 {
    d.as_secs_f64() * 1_000_000.0
}

fn thousands(n: u64) -> String {
    let s = n.to_string();
    let mut out = String::with_capacity(s.len() + s.len() / 3);
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(c);
    }
    out
}

// ---------------------------------------------------------------------------
// The payloads.
//
// Shaped to match what section 11's aggregate row was measured against — roughly 137
// bytes a record on the wire — and to touch every column that costs the encoder
// anything: a resource with attributes (the `resources` table and the semi-join
// key), a scope, per-record attributes of three different value types (the six
// typed columns in `ATTRS`), and for traces the two child tables.
// ---------------------------------------------------------------------------

fn str_kv(k: &str, v: &str) -> KeyValue {
    KeyValue {
        key: k.into(),
        value: Some(AnyValue {
            value: Some(any_value::Value::StringValue(v.into())),
        }),
    }
}

fn int_kv(k: &str, v: i64) -> KeyValue {
    KeyValue {
        key: k.into(),
        value: Some(AnyValue {
            value: Some(any_value::Value::IntValue(v)),
        }),
    }
}

fn bool_kv(k: &str, v: bool) -> KeyValue {
    KeyValue {
        key: k.into(),
        value: Some(AnyValue {
            value: Some(any_value::Value::BoolValue(v)),
        }),
    }
}

fn resource() -> Resource {
    Resource {
        attributes: vec![
            str_kv("service.name", "checkout"),
            str_kv("service.namespace", "shop"),
            str_kv("service.instance.id", "checkout-7d9f8b6c4d-h2xkq"),
            str_kv("deployment.environment.name", "production"),
            str_kv("k8s.pod.name", "checkout-7d9f8b6c4d-h2xkq"),
            str_kv("host.name", "ip-10-0-14-207"),
        ],
        ..Default::default()
    }
}

fn scope() -> InstrumentationScope {
    InstrumentationScope {
        name: "io.opentelemetry.instrumentation.http".into(),
        version: "2.11.0".into(),
        ..Default::default()
    }
}

/// A 16-byte trace id that varies per record, because trace ids are the highest
/// cardinality data in the system (section 0) and a constant one would let the
/// `FixedSizeBinaryBuilder` look better than it is.
///
/// `Bytes`, not `Vec<u8>`: `mira-proto` enables prost's `bytes = "bytes"`
/// codegen precisely so an id is not a heap allocation per field (section A3a), and a
/// bench that handed the encoder `Vec`s would not be measuring the type the
/// server actually receives.
fn trace_id(i: usize) -> Bytes {
    let mut id = [0u8; 16];
    id[..8].copy_from_slice(&(i as u64).to_be_bytes());
    id[8..].copy_from_slice(&(i as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15).to_be_bytes());
    Bytes::copy_from_slice(&id)
}

fn span_id(i: usize) -> Bytes {
    Bytes::copy_from_slice(&(i as u64).wrapping_mul(0xA24B_AED4_963E_E407).to_be_bytes())
}

fn logs_request() -> ExportLogsServiceRequest {
    let base = 1_757_241_600_000_000_000u64;
    let records = (0..BATCH)
        .map(|i| LogRecord {
            time_unix_nano: base + i as u64 * 1_000,
            observed_time_unix_nano: base + i as u64 * 1_000 + 500,
            severity_number: 9,
            severity_text: "INFO".into(),
            body: Some(AnyValue {
                value: Some(any_value::Value::StringValue(format!(
                    "GET /api/v1/cart/{i} 200"
                ))),
            }),
            attributes: vec![
                str_kv("http.request.method", "GET"),
                int_kv("http.response.status_code", 200),
                str_kv("url.path", "/api/v1/cart"),
                bool_kv("cache.hit", i % 3 == 0),
            ],
            trace_id: trace_id(i),
            span_id: span_id(i),
            ..Default::default()
        })
        .collect();

    ExportLogsServiceRequest {
        resource_logs: vec![ResourceLogs {
            resource: Some(resource()),
            scope_logs: vec![ScopeLogs {
                scope: Some(scope()),
                log_records: records,
                ..Default::default()
            }],
            ..Default::default()
        }],
    }
}

fn traces_request() -> ExportTraceServiceRequest {
    let base = 1_757_241_600_000_000_000u64;
    let spans = (0..BATCH)
        .map(|i| Span {
            trace_id: trace_id(i),
            span_id: span_id(i),
            parent_span_id: if i % 4 == 0 {
                Bytes::new()
            } else {
                span_id(i / 4)
            },
            name: "GET /api/v1/cart".into(),
            kind: span::SpanKind::Server as i32,
            start_time_unix_nano: base + i as u64 * 1_000,
            end_time_unix_nano: base + i as u64 * 1_000 + 4_200_000,
            attributes: vec![
                str_kv("http.request.method", "GET"),
                int_kv("http.response.status_code", 200),
                str_kv("url.path", "/api/v1/cart"),
                bool_kv("cache.hit", i % 3 == 0),
            ],
            // One event on one span in eight: `recordException` is not on the
            // happy path, and making it universal would make the child tables
            // as big as the root one, which is not a real trace corpus.
            events: if i % 8 == 0 {
                vec![mira_proto::trace::v1::span::Event {
                    time_unix_nano: base + i as u64 * 1_000 + 1_000_000,
                    name: "exception".into(),
                    attributes: vec![
                        str_kv("exception.type", "java.lang.NullPointerException"),
                        str_kv("exception.message", "cart is null"),
                    ],
                    ..Default::default()
                }]
            } else {
                vec![]
            },
            links: if i % 32 == 0 {
                vec![mira_proto::trace::v1::span::Link {
                    trace_id: trace_id(i + 1),
                    span_id: span_id(i + 1),
                    ..Default::default()
                }]
            } else {
                vec![]
            },
            status: Some(Status {
                code: status::StatusCode::Ok as i32,
                ..Default::default()
            }),
            ..Default::default()
        })
        .collect();

    ExportTraceServiceRequest {
        resource_spans: vec![ResourceSpans {
            resource: Some(resource()),
            scope_spans: vec![ScopeSpans {
                scope: Some(scope()),
                spans,
                ..Default::default()
            }],
            ..Default::default()
        }],
    }
}

/// Metrics are shaped as data points, not records, so `BATCH` here is data
/// points spread over a handful of metric names — which is what a real scrape
/// looks like and what the series map (`series.rs`) is bounded against.
fn metrics_request() -> ExportMetricsServiceRequest {
    let base = 1_757_241_600_000_000_000u64;
    const NAMES: &[&str] = &[
        "http.server.request.duration",
        "http.server.active_requests",
        "process.runtime.jvm.memory.used",
        "system.cpu.utilization",
    ];
    let per_name = BATCH / NAMES.len();

    let metrics = NAMES
        .iter()
        .map(|name| Metric {
            name: (*name).into(),
            unit: "1".into(),
            description: "one of the four a stock runtime emits".into(),
            data: Some(metric::Data::Gauge(Gauge {
                data_points: (0..per_name)
                    .map(|i| NumberDataPoint {
                        time_unix_nano: base + i as u64 * 1_000_000,
                        start_time_unix_nano: base,
                        value: Some(number_data_point::Value::AsDouble(i as f64 * 0.5)),
                        attributes: vec![
                            str_kv("http.request.method", "GET"),
                            int_kv("http.response.status_code", 200),
                        ],
                        ..Default::default()
                    })
                    .collect(),
            })),
            ..Default::default()
        })
        .collect();

    ExportMetricsServiceRequest {
        resource_metrics: vec![ResourceMetrics {
            resource: Some(resource()),
            scope_metrics: vec![ScopeMetrics {
                scope: Some(scope()),
                metrics,
                ..Default::default()
            }],
            ..Default::default()
        }],
    }
}
