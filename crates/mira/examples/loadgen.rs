//! A synthetic OTLP source: logs, spans and metrics from a fake shop.
//!
//! Two jobs. It fills a dev instance so the UI has something to draw, and it is
//! the front half of the ingest benchmark — it reports achieved rate, so the
//! number it prints is a floor on what the server sustained.
//!
//! The HTTP client is thirty lines of `TcpStream`. `reqwest` would pull in
//! hyper, rustls and the rest of the tree into a binary whose entire job is to
//! write one POST and read one status line, and the four-axes principle starts
//! with not paying for things we do not use.
//!
//!     cargo run --release --example loadgen -- --for 60s --conns 64 --batch 8192
//!
//! Everything is derived from a counter, not a random source: the same
//! arguments produce the same bytes, so two runs are comparable.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::time::{Duration, Instant, SystemTime};

use prost::Message;

use mira_proto::collector::logs::v1::ExportLogsServiceRequest;
use mira_proto::collector::metrics::v1::ExportMetricsServiceRequest;
use mira_proto::collector::trace::v1::ExportTraceServiceRequest;
use mira_proto::common::v1::{AnyValue, InstrumentationScope, KeyValue, any_value};
use mira_proto::logs::v1::{LogRecord, ResourceLogs, ScopeLogs};
use mira_proto::metrics::v1::{
    AggregationTemporality, Gauge, HistogramDataPoint, Metric, NumberDataPoint, ResourceMetrics,
    ScopeMetrics, Sum, metric, number_data_point,
};
use mira_proto::resource::v1::Resource;
use mira_proto::trace::v1::{ResourceSpans, ScopeSpans, Span, span, status};

const SERVICES: [&str; 4] = ["checkout", "payments", "inventory", "frontend"];
const ROUTES: [&str; 5] = [
    "GET /cart",
    "POST /checkout",
    "GET /items",
    "POST /pay",
    "GET /health",
];
/// Records per export.
///
/// The OpenTelemetry Collector's batch processor defaults to 8192, and the
/// server seals a block on size *or* age — so a batch far below the size
/// threshold measures the age timer rather than the engine. Default high enough
/// that it does not, overridable to find where the crossover is.
static BATCH: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(2_000);
fn batch() -> usize {
    BATCH.load(std::sync::atomic::Ordering::Relaxed)
}

fn main() {
    let mut addr = "127.0.0.1:4318".to_string();
    let mut secs = 30u64;
    let mut conns = 8usize;
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        let mut v = || args.next().expect("missing value");
        match a.as_str() {
            "--addr" => addr = v(),
            "--for" => secs = v().trim_end_matches('s').parse().expect("--for"),
            "--conns" => conns = v().parse().expect("--conns"),
            "--batch" => BATCH.store(
                v().parse().expect("--batch"),
                std::sync::atomic::Ordering::Relaxed,
            ),
            _ => {
                eprintln!(
                    "usage: loadgen [--addr host:port] [--for 30s] [--conns 8] [--batch 2000]"
                );
                std::process::exit(2);
            }
        }
    }

    let t0 = Instant::now();
    let deadline = t0 + Duration::from_secs(secs);
    let base = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap()
        .as_nanos() as u64;

    // Concurrency, not a rate limiter. The server acknowledges an export only
    // once the block holding it is fsynced, so a single connection measures the
    // flush interval and nothing else. N in flight is what an exporter fleet
    // actually looks like, and it is the only shape under which the sealer sees
    // enough data to seal on size rather than on the timer.
    let workers: Vec<_> = (0..conns)
        .map(|w| {
            let addr = addr.clone();
            std::thread::spawn(move || {
                let mut conn = Conn::connect(&addr);
                let mut s = Stats::default();
                let mut i = w as u64;
                while Instant::now() < deadline {
                    // Timestamps advance with the wall clock so the default
                    // "last hour" window in the UI contains the data.
                    let ts = base + t0.elapsed().as_nanos() as u64;
                    s.send(&mut conn, "/v1/logs", logs_batch(i, ts).encode_to_vec());
                    s.send(&mut conn, "/v1/traces", spans_batch(i, ts).encode_to_vec());
                    s.send(
                        &mut conn,
                        "/v1/metrics",
                        metrics_batch(i, ts).encode_to_vec(),
                    );
                    s.logs += batch() as u64;
                    s.spans += batch() as u64;
                    s.points += SERVICES.len() as u64 * 3;
                    i += conns as u64;
                }
                s
            })
        })
        .collect();

    let mut all = Stats::default();
    for w in workers {
        all.merge(w.join().unwrap());
    }

    let el = t0.elapsed().as_secs_f64();
    let n = all.logs + all.spans + all.points;
    all.acks.sort_unstable();
    let pct = |p: f64| all.acks[((all.acks.len() - 1) as f64 * p) as usize] as f64 / 1e6;
    println!(
        "{} logs, {} spans, {} points in {el:.1}s over {conns} connections \
         of {} records\n\
         {:.0} records/s, {:.1} MiB/s on the wire, {} exports shed\n\
         ack latency p50 {:.1}ms  p99 {:.1}ms  max {:.1}ms",
        all.logs,
        all.spans,
        all.points,
        batch(),
        n as f64 / el,
        all.bytes as f64 / el / (1 << 20) as f64,
        all.shed,
        pct(0.50),
        pct(0.99),
        pct(1.0),
    );
}

#[derive(Default)]
struct Stats {
    logs: u64,
    spans: u64,
    points: u64,
    bytes: u64,
    /// Exports the engine shed with a 503. Retried, and reported: a headline
    /// throughput number that quietly dropped a third of its offered load is
    /// the most common way an ingest benchmark lies.
    shed: u64,
    /// Nanoseconds from the first byte written to the 200. Under
    /// ack-after-durability this is the fsync, so it is the number that tells
    /// you whether the sealer is keeping up.
    acks: Vec<u64>,
}

impl Stats {
    /// Retries until accepted, so the record counts the caller keeps are true.
    fn send(&mut self, conn: &mut Conn, path: &str, body: Vec<u8>) {
        let t = Instant::now();
        loop {
            let (n, ok) = conn.post(path, &body);
            self.bytes += n;
            if ok {
                break;
            }
            self.shed += 1;
            std::thread::sleep(Duration::from_millis(20));
        }
        self.acks.push(t.elapsed().as_nanos() as u64);
    }

    fn merge(&mut self, o: Stats) {
        self.logs += o.logs;
        self.spans += o.spans;
        self.points += o.points;
        self.bytes += o.bytes;
        self.shed += o.shed;
        self.acks.extend(o.acks);
    }
}

/// One keep-alive connection, one POST at a time.
///
/// No pipelining within a connection: the point is to measure what the server
/// does with a request, and overlapping them here would measure the client's
/// queue instead. Concurrency comes from `--conns`.
struct Conn {
    addr: String,
    io: BufReader<TcpStream>,
}

impl Conn {
    fn connect(addr: &str) -> Self {
        let s = TcpStream::connect(addr).unwrap_or_else(|e| panic!("connect {addr}: {e}"));
        s.set_nodelay(true).unwrap();
        Conn {
            addr: addr.to_string(),
            io: BufReader::new(s),
        }
    }

    /// Returns the bytes written and whether the export was accepted, so the
    /// caller can report wire throughput and count backpressure separately.
    fn post(&mut self, path: &str, body: &[u8]) -> (u64, bool) {
        let head = format!(
            "POST {path} HTTP/1.1\r\nHost: {}\r\nContent-Type: application/x-protobuf\r\n\
             Content-Length: {}\r\n\r\n",
            self.addr,
            body.len()
        );
        let s = self.io.get_mut();
        push(s, head.as_bytes());
        push(s, body);

        let mut line = String::new();
        self.io.read_line(&mut line).unwrap();
        let status = line.trim().to_owned();
        // Read the response fully before judging it. Leaving a body in the
        // socket desynchronizes the next request on this keep-alive connection,
        // which then fails for a reason that has nothing to do with the bug.
        let mut len = 0usize;
        loop {
            line.clear();
            self.io.read_line(&mut line).unwrap();
            if let Some(v) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                len = v.trim().parse().unwrap();
            }
            if line == "\r\n" || line.is_empty() {
                break;
            }
        }
        let mut msg = vec![0u8; len];
        self.io.read_exact(&mut msg).unwrap();

        // 503 is the engine shedding load, which is a correct answer and not an
        // error: OTLP says retry. Anything else is a bug worth stopping on, and
        // the server's message is the whole reason to stop here.
        let ok = status.contains(" 200 ");
        assert!(
            ok || status.contains(" 503 "),
            "{path}: {status}: {}",
            String::from_utf8_lossy(&msg)
        );
        ((head.len() + body.len()) as u64, ok)
    }
}

/// `write_all`, but patient.
///
/// A hundred blocking sockets pushing megabyte bodies at loopback exhausts the
/// kernel's mbuf pool on macOS and the write comes back ENOBUFS. That is the
/// client running out of socket buffers, not the server refusing anything, so
/// backing off and continuing is the correct handling — failing here would
/// report a client limit as a server result.
fn push(s: &mut TcpStream, mut buf: &[u8]) {
    /// ENOBUFS: 55 on Darwin, 105 on Linux.
    const ENOBUFS: i32 = if cfg!(target_os = "macos") { 55 } else { 105 };
    /// Offer the kernel a socket buffer's worth at a time rather than a whole
    /// megabyte body. A blocking `send` that cannot allocate mbufs for the
    /// whole request fails outright, and a failed send that may have written
    /// part of the buffer leaves the connection unresynchronizable.
    const CHUNK: usize = 64 << 10;
    while !buf.is_empty() {
        match s.write(&buf[..buf.len().min(CHUNK)]) {
            Ok(0) => panic!("connection closed mid-write"),
            Ok(n) => buf = &buf[n..],
            Err(e)
                if e.kind() == std::io::ErrorKind::Interrupted
                    || e.kind() == std::io::ErrorKind::WouldBlock =>
            {
                std::thread::sleep(Duration::from_millis(1));
            }
            Err(e) if e.raw_os_error() == Some(ENOBUFS) => {
                std::thread::sleep(Duration::from_millis(1));
            }
            Err(e) => panic!("write: {e}"),
        }
    }
}

fn kv(k: &str, v: &str) -> KeyValue {
    KeyValue {
        key: k.into(),
        value: Some(AnyValue {
            value: Some(any_value::Value::StringValue(v.into())),
        }),
    }
}

fn resource(i: u64) -> Resource {
    Resource {
        attributes: vec![
            kv("service.name", SERVICES[(i % 4) as usize]),
            kv("service.version", "1.4.2"),
            kv("deployment.environment", "prod"),
            kv("k8s.pod.name", &format!("pod-{:04}", i % 16)),
        ],
        ..Default::default()
    }
}

fn scope() -> Option<InstrumentationScope> {
    Some(InstrumentationScope {
        name: "mira.loadgen".into(),
        version: "0.0.1".into(),
        ..Default::default()
    })
}

fn logs_batch(i: u64, ts: u64) -> ExportLogsServiceRequest {
    ExportLogsServiceRequest {
        resource_logs: vec![ResourceLogs {
            resource: Some(resource(i)),
            scope_logs: vec![ScopeLogs {
                scope: scope(),
                log_records: (0..batch())
                    .map(|n| {
                        let k = i.wrapping_mul(batch() as u64) + n as u64;
                        // One in fifty is an error: rare enough that a severity
                        // filter is worth typing, common enough to see.
                        let err = k % 50 == 0;
                        let route = ROUTES[(k % 5) as usize];
                        LogRecord {
                            time_unix_nano: ts + n as u64 * 1_000,
                            observed_time_unix_nano: ts + n as u64 * 1_000,
                            severity_number: if err { 17 } else { 9 },
                            severity_text: if err { "ERROR" } else { "INFO" }.into(),
                            body: Some(AnyValue {
                                value: Some(any_value::Value::StringValue(if err {
                                    format!("{route} failed: upstream timeout after 30s")
                                } else {
                                    format!("{route} 200 in {}ms", 3 + k % 90)
                                })),
                            }),
                            attributes: vec![
                                kv("http.route", route),
                                kv("http.status_code", if err { "504" } else { "200" }),
                            ],
                            trace_id: trace_id(k / 8),
                            span_id: span_id(k),
                            ..Default::default()
                        }
                    })
                    .collect(),
                ..Default::default()
            }],
            ..Default::default()
        }],
    }
}

/// Eight spans per trace, chained parent to child, so the waterfall has depth.
fn spans_batch(i: u64, ts: u64) -> ExportTraceServiceRequest {
    ExportTraceServiceRequest {
        resource_spans: vec![ResourceSpans {
            resource: Some(resource(i)),
            scope_spans: vec![ScopeSpans {
                scope: scope(),
                spans: (0..batch())
                    .map(|n| {
                        let k = i.wrapping_mul(batch() as u64) + n as u64;
                        let depth = k % 8;
                        let start = ts + n as u64 / 8 * 8_000 + depth * 400_000;
                        let dur = (8 - depth) * 900_000 + (k % 37) * 40_000;
                        Span {
                            trace_id: trace_id(k / 8),
                            span_id: span_id(k),
                            parent_span_id: if depth == 0 {
                                Vec::new().into()
                            } else {
                                span_id(k - 1)
                            },
                            name: ROUTES[(k % 5) as usize].into(),
                            kind: if depth == 0 {
                                span::SpanKind::Server
                            } else {
                                span::SpanKind::Client
                            } as i32,
                            start_time_unix_nano: start,
                            end_time_unix_nano: start + dur,
                            attributes: vec![
                                kv("http.route", ROUTES[(k % 5) as usize]),
                                kv("net.peer.name", SERVICES[((k + 1) % 4) as usize]),
                            ],
                            status: Some(mira_proto::trace::v1::Status {
                                code: if k % 50 == 0 {
                                    status::StatusCode::Error
                                } else {
                                    status::StatusCode::Ok
                                } as i32,
                                message: if k % 50 == 0 {
                                    "timeout".into()
                                } else {
                                    String::new()
                                },
                            }),
                            ..Default::default()
                        }
                    })
                    .collect(),
                ..Default::default()
            }],
            ..Default::default()
        }],
    }
}

/// A counter, a gauge and a histogram per service — one of each kind the read
/// path knows how to chart.
fn metrics_batch(i: u64, ts: u64) -> ExportMetricsServiceRequest {
    let dp = |v: f64, attrs: Vec<KeyValue>| NumberDataPoint {
        attributes: attrs,
        start_time_unix_nano: ts,
        time_unix_nano: ts,
        value: Some(number_data_point::Value::AsDouble(v)),
        ..Default::default()
    };
    // A sine over the batch counter: something with a shape, so a broken axis
    // or a dropped point is visible rather than plausible.
    let wave = |phase: f64| (i as f64 / 30.0 + phase).sin() * 0.5 + 0.5;

    ExportMetricsServiceRequest {
        resource_metrics: SERVICES
            .iter()
            .enumerate()
            .map(|(s, name)| ResourceMetrics {
                resource: Some(Resource {
                    attributes: vec![
                        kv("service.name", name),
                        kv("deployment.environment", "prod"),
                    ],
                    ..Default::default()
                }),
                scope_metrics: vec![ScopeMetrics {
                    scope: scope(),
                    metrics: vec![
                        Metric {
                            name: "http.server.requests".into(),
                            unit: "1".into(),
                            description: "Requests handled".into(),
                            data: Some(metric::Data::Sum(Sum {
                                data_points: vec![dp(
                                    (i * batch() as u64) as f64,
                                    vec![kv("http.method", "GET")],
                                )],
                                aggregation_temporality: AggregationTemporality::Cumulative as i32,
                                is_monotonic: true,
                            })),
                            ..Default::default()
                        },
                        Metric {
                            name: "process.memory.usage".into(),
                            unit: "By".into(),
                            data: Some(metric::Data::Gauge(Gauge {
                                data_points: vec![dp(2.0e8 + wave(s as f64) * 6.0e7, vec![])],
                            })),
                            ..Default::default()
                        },
                        Metric {
                            name: "http.server.duration".into(),
                            unit: "ms".into(),
                            data: Some(metric::Data::Histogram(
                                mira_proto::metrics::v1::Histogram {
                                    data_points: vec![HistogramDataPoint {
                                        attributes: vec![kv("http.method", "POST")],
                                        start_time_unix_nano: ts,
                                        time_unix_nano: ts,
                                        count: batch() as u64,
                                        sum: Some(batch() as f64 * (12.0 + wave(s as f64) * 40.0)),
                                        bucket_counts: vec![300, 150, 40, 9, 1],
                                        explicit_bounds: vec![5.0, 25.0, 100.0, 500.0],
                                        ..Default::default()
                                    }],
                                    aggregation_temporality: AggregationTemporality::Delta as i32,
                                },
                            )),
                            ..Default::default()
                        },
                    ],
                    ..Default::default()
                }],
                ..Default::default()
            })
            .collect(),
    }
}

fn trace_id(k: u64) -> bytes::Bytes {
    let mut b = [0u8; 16];
    b[..8].copy_from_slice(&k.to_be_bytes());
    b[8..].copy_from_slice(&(k ^ 0x5555_5555_5555_5555).to_be_bytes());
    b.to_vec().into()
}

fn span_id(k: u64) -> bytes::Bytes {
    (k.wrapping_mul(0x9e37_79b9_7f4a_7c15) | 1)
        .to_be_bytes()
        .to_vec()
        .into()
}
