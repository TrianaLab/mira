//! The load harness: a synthetic OTLP source, a query driver, and a report on
//! all four axes at once.
//!
//! Three jobs. It fills a dev instance so the UI has something to draw; it is
//! the ingest benchmark; and with `--readers` it is the query benchmark, run
//! *while* ingest is running, because a p99 measured on a quiet store is a
//! number no operator will ever see.
//!
//! §11 scores four axes and the principle is that they are scored together —
//! ingest throughput per core, resident footprint, query p99, cost per GB. A
//! harness that reports one of them is how a storage engine ends up fast at
//! whichever one its authors were looking at. So this reports all four, in one
//! block, from one command:
//!
//!     cargo run --release --example loadgen -- \
//!       --for 60s --conns 64 --batch 8192 --readers 8 \
//!       --pid $(pgrep -n mira) --data-dir ./data
//!
//! `--conns 0` makes it read-only, which is the number to quote for a cold
//! store; `--readers 0` (the default) makes it write-only, which is the number
//! to quote for ingest with nothing competing for the page cache.
//!
//! The HTTP client is thirty lines of `TcpStream`. `reqwest` would pull in
//! hyper, rustls and the rest of the tree into a binary whose entire job is to
//! write one POST and read one status line, and the four-axes principle starts
//! with not paying for things we do not use.
//!
//! Everything is derived from a counter, not a random source: the same
//! arguments produce the same bytes, so two runs are comparable.

use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::sync::atomic::{AtomicU64, Ordering};
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
    BATCH.load(Ordering::Relaxed)
}

/// The highest trace index the writers have sent, so a reader asking for a
/// trace asks for one that exists. Zero means nothing has been written this run
/// and the readers fall back to a fixed range — a lookup that misses is a real
/// query too, and the hit rate is reported either way.
static TRACES: AtomicU64 = AtomicU64::new(0);

fn main() {
    let mut addr = "127.0.0.1:4318".to_string();
    let mut secs = 30u64;
    let mut conns = 8usize;
    let mut readers = 0usize;
    let mut pid: Option<u32> = None;
    let mut data_dir: Option<std::path::PathBuf> = None;
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        let mut v = || args.next().expect("missing value");
        match a.as_str() {
            "--addr" => addr = v(),
            "--for" => secs = v().trim_end_matches('s').parse().expect("--for"),
            "--conns" => conns = v().parse().expect("--conns"),
            "--readers" => readers = v().parse().expect("--readers"),
            "--pid" => pid = Some(v().parse().expect("--pid")),
            "--data-dir" => data_dir = Some(v().into()),
            "--batch" => BATCH.store(v().parse().expect("--batch"), Ordering::Relaxed),
            _ => {
                eprintln!(
                    "usage: loadgen [--addr host:port] [--for 30s] [--conns 8] [--batch 2000]\n\
                     \x20              [--readers 0] [--pid N] [--data-dir PATH]"
                );
                std::process::exit(2);
            }
        }
    }
    assert!(
        conns + readers > 0,
        "nothing to do: --conns and --readers are both 0"
    );

    // Cost per GB is a delta, not a total: run against a store that already has
    // blocks in it and the total divided by this run's records is meaningless.
    let disk0 = data_dir.as_deref().map(du).unwrap_or(0);

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
                // `n` counts this connection's own batches; `i` interleaves the
                // connections into one gapless sequence, which is what keeps the
                // content deterministic. Metrics need `n`: a cumulative counter
                // reports what its own producer has sent.
                let mut n = 0u64;
                while Instant::now() < deadline {
                    let i = n * conns as u64 + w as u64;
                    // Timestamps advance with the wall clock so the default
                    // "last hour" window in the UI contains the data.
                    let ts = base + t0.elapsed().as_nanos() as u64;
                    s.send(&mut conn, "/v1/logs", &logs_batch(i, ts).encode_to_vec());
                    s.send(&mut conn, "/v1/traces", &spans_batch(i, ts).encode_to_vec());
                    s.send(
                        &mut conn,
                        "/v1/metrics",
                        &metrics_batch(w, n, ts).encode_to_vec(),
                    );
                    s.logs += batch() as u64;
                    s.spans += batch() as u64;
                    s.points += SERVICES.len() as u64 * 3;
                    // The newest trace id now on the server, for the readers.
                    let top = (i * batch() as u64 + batch() as u64 - 1) / 8;
                    TRACES.fetch_max(top, Ordering::Relaxed);
                    n += 1;
                }
                s
            })
        })
        .collect();

    let readers: Vec<_> = (0..readers)
        .map(|r| {
            let addr = addr.clone();
            std::thread::spawn(move || read_loop(&addr, r, deadline))
        })
        .collect();

    // Resident footprint is an axis, and it is the one nothing inside the
    // process can report honestly — a heap total misses the page cache the mmap
    // reader lives on. So: the kernel's number, sampled from outside, peak kept.
    let rss = pid.map(|pid| std::thread::spawn(move || peak_rss(pid, deadline)));

    let mut all = Stats::default();
    for w in workers {
        all.merge(w.join().unwrap());
    }
    let mut reads = Reads::default();
    for r in readers {
        reads.merge(&r.join().unwrap());
    }
    let el = t0.elapsed().as_secs_f64();

    if conns > 0 {
        let n = all.logs + all.spans + all.points;
        println!(
            "ingest   {:.0} records/s   {:.1} MiB/s wire   {} shed   {} resets\n\
             \x20        {} logs + {} spans + {} points in {el:.1}s, \
             {conns} conns x {} records\n\
             \x20        ack p50 {:.1}ms  p99 {:.1}ms  max {:.1}ms",
            n as f64 / el,
            all.bytes as f64 / el / (1 << 20) as f64,
            all.shed,
            all.resets,
            all.logs,
            all.spans,
            all.points,
            batch(),
            all.acks.p(0.50),
            all.acks.p(0.99),
            all.acks.p(1.0),
        );
    }
    reads.report(el);
    if let Some(rss) = rss {
        println!("memory   peak RSS {:.0} MiB", rss.join().unwrap());
    }
    if let Some(dir) = &data_dir {
        // Everything acked is already durable — the server does not answer an
        // export until the block holding it is fsynced — so this is not a
        // partial figure. Compaction may still shrink it later; it never grows.
        let disk = du(dir);
        let records = all.logs + all.spans + all.points;
        print!(
            "storage  {:.2} GiB on disk",
            disk as f64 / (1u64 << 30) as f64
        );
        if records > 0 {
            // Against the uncompressed protobuf actually sent, so above 1.0 means
            // the store grew by more than the wire — which the sidecars and an
            // uncompacted block directory can genuinely make it, briefly.
            let grew = disk.saturating_sub(disk0);
            print!(
                "   +{:.2} GiB this run   {:.0} B/record   {:.2}x the wire bytes",
                grew as f64 / (1u64 << 30) as f64,
                grew as f64 / records as f64,
                grew as f64 / all.bytes.max(1) as f64
            );
        }
        println!();
    }
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
    /// Connections rebuilt after a write the kernel could not complete. A
    /// client-side artifact of loopback mbuf exhaustion, reported because it
    /// inflates ack latency and is otherwise invisible.
    resets: u64,
    /// Nanoseconds from the first byte written to the 200. Under
    /// ack-after-durability this is the fsync, so it is the number that tells
    /// you whether the sealer is keeping up.
    acks: Hist,
}

impl Stats {
    /// Retries until accepted, so the record counts the caller keeps are true.
    fn send(&mut self, conn: &mut Conn, path: &str, body: &[u8]) {
        let t = Instant::now();
        loop {
            let r = conn.post(path, body);
            self.bytes += r.wrote;
            self.resets += r.resets;
            if r.ok {
                break;
            }
            self.shed += 1;
            std::thread::sleep(Duration::from_millis(20));
        }
        self.acks.0.push(t.elapsed().as_nanos() as u64);
    }

    fn merge(&mut self, o: Stats) {
        self.logs += o.logs;
        self.spans += o.spans;
        self.points += o.points;
        self.bytes += o.bytes;
        self.shed += o.shed;
        self.resets += o.resets;
        self.acks.0.extend(o.acks.0);
    }
}

/// Latencies in nanoseconds. A full sample, not a sketch: at these counts the
/// memory is a few megabytes and an exact p999 is worth more than a t-digest
/// whose error is largest exactly where the interesting number is.
#[derive(Default)]
struct Hist(Vec<u64>);

impl Hist {
    /// Milliseconds at quantile `q`. Sorts on the way, which is free the second
    /// time and keeps the ordering invariant off the caller.
    fn p(&mut self, q: f64) -> f64 {
        if self.0.is_empty() {
            return 0.0;
        }
        self.0.sort_unstable();
        self.0[((self.0.len() - 1) as f64 * q) as usize] as f64 / 1e6
    }
}

// ---------------------------------------------------------------------------
// The read side.
// ---------------------------------------------------------------------------

/// The query mix, in the order a reader cycles it.
///
/// Not a microbenchmark of one shape. Each of these costs something different —
/// `tail` should read one block and stop, `errors` has no choice but to scan the
/// window, `trace` has no useful time bound at all and lives or dies on the
/// sidecar filter, `page` is the deep-paging case a keyset cursor exists for —
/// and an engine can be fast at any one of them while being unusable.
const CLASSES: [&str; 6] = ["tail", "attr", "errors", "trace", "page x10", "series"];
/// Index of the paging class in [`CLASSES`], which is the one that is more than
/// a single request.
const PAGING: usize = 4;

#[derive(Default)]
struct Reads {
    lat: [Hist; CLASSES.len()],
    /// `rows_matched` from the first response of each query: what the filter
    /// found. A query mix that matches nothing measures the empty path and
    /// reports beautiful numbers, so this column is not decoration.
    matched: [u64; CLASSES.len()],
    pages: u64,
}

impl Reads {
    fn merge(&mut self, o: &Reads) {
        for i in 0..CLASSES.len() {
            self.lat[i].0.extend(&o.lat[i].0);
            self.matched[i] += o.matched[i];
        }
        self.pages += o.pages;
    }

    fn report(&mut self, el: f64) {
        let total: usize = self.lat.iter().map(|h| h.0.len()).sum();
        if total == 0 {
            return;
        }
        println!(
            "query    {:.0} queries/s over {total} queries",
            total as f64 / el
        );
        for (i, label) in CLASSES.iter().enumerate() {
            let n = self.lat[i].0.len();
            if n == 0 {
                continue;
            }
            let matched = self.matched[i] as f64 / n as f64;
            let extra = if i == PAGING {
                format!("  {:.1} pages/walk", self.pages as f64 / n as f64)
            } else {
                String::new()
            };
            println!(
                "\x20        {label:<9} p50 {:>6.2}ms  p99 {:>6.2}ms  p999 {:>6.2}ms  \
                 max {:>6.2}ms  {matched:>7.0} matched{extra}",
                self.lat[i].p(0.50),
                self.lat[i].p(0.99),
                self.lat[i].p(0.999),
                self.lat[i].p(1.0),
            );
        }
    }
}

fn read_loop(addr: &str, worker: usize, deadline: Instant) -> Reads {
    let mut conn = Conn::connect(addr);
    let mut out = Reads::default();
    let mut n = worker as u64;
    while Instant::now() < deadline {
        for class in 0..CLASSES.len() {
            if Instant::now() >= deadline {
                break;
            }
            let t = Instant::now();
            let matched = ask(&mut conn, class, n, &mut out.pages);
            out.lat[class].0.push(t.elapsed().as_nanos() as u64);
            out.matched[class] += matched;
            n += 1;
        }
    }
    out
}

/// One query of one class. Returns `rows_matched` from the first response.
fn ask(conn: &mut Conn, class: usize, n: u64, pages: &mut u64) -> u64 {
    let (path, body) = document(class, n);
    let r = conn.post(path, body.as_bytes());
    assert!(r.ok, "{path} {body}: {}", r.body);
    let matched = field(&r.body, "\"rows_matched\":");
    if class != PAGING {
        return matched;
    }
    // Walk the cursor. Page ten costing what page one cost is the whole claim of
    // a keyset cursor, and it is only true if someone measures it.
    *pages += 1;
    let mut next = after(&r.body);
    for _ in 0..9 {
        let Some(c) = next else { break };
        let doc = format!(
            "{{\"signal\":\"logs\",\"from\":\"-1h\",\"to\":\"now\",\"limit\":100,\"after\":\"{c}\"}}"
        );
        let r = conn.post("/api/v1/query", doc.as_bytes());
        assert!(r.ok, "page: {}", r.body);
        *pages += 1;
        next = after(&r.body);
    }
    matched
}

fn document(class: usize, n: u64) -> (&'static str, String) {
    const Q: &str = "/api/v1/query";
    match class {
        // The session opener. Blocks are ordered newest first and `limit` cuts
        // the scan short, so this should touch one block whatever is on disk.
        0 => (Q, r#"{"signal":"logs","from":"-5m","to":"now","limit":100}"#.into()),
        // An attribute equality: the case the block filter is for.
        1 => (
            Q,
            format!(
                r#"{{"signal":"logs","from":"-1h","to":"now","limit":100,"where":[{{"attr":"http.route","eq":"{}"}}]}}"#,
                ROUTES[(n % 5) as usize]
            ),
        ),
        // One in fifty rows, on a column with no index: the honest scan.
        2 => (
            Q,
            r#"{"signal":"logs","from":"-1h","to":"now","limit":100,"where":[{"field":"severity_number","gte":17}]}"#.into(),
        ),
        // Correlation. No useful time bound — a trace id says nothing about
        // when — so every block is a candidate and only `trace.idx` prunes.
        3 => (
            Q,
            format!(
                r#"{{"signal":"traces","from":"-24h","to":"now","limit":200,"where":[{{"field":"trace_id","eq":"{}"}}]}}"#,
                trace_hex(n % TRACES.load(Ordering::Relaxed).max(100_000))
            ),
        ),
        PAGING => (Q, r#"{"signal":"logs","from":"-1h","to":"now","limit":100}"#.into()),
        _ => (
            "/api/v1/metrics/query",
            r#"{"name":"http.server.requests","from":"-15m","to":"now"}"#.into(),
        ),
    }
}

/// An unsigned field out of the response envelope, without a JSON parser. The
/// envelope is written by `api::envelope` as a fixed format string, so this is
/// reading a known shape rather than guessing at one.
fn field(body: &str, key: &str) -> u64 {
    body.split(key)
        .nth(1)
        .map(|s| {
            s.chars()
                .take_while(char::is_ascii_digit)
                .collect::<String>()
        })
        .and_then(|s| s.parse().ok())
        .unwrap_or(0)
}

fn after(body: &str) -> Option<String> {
    Some(
        body.split("\"next\":\"")
            .nth(1)?
            .split('"')
            .next()?
            .to_owned(),
    )
}

/// Peak resident set of the server, in MiB, sampled from outside it.
///
/// From outside on purpose. Mira reads through `mmap`, so most of what it costs
/// a machine is page cache the process never allocated — a heap counter would
/// report a number that is flattering and wrong.
fn peak_rss(pid: u32, deadline: Instant) -> f64 {
    let mut peak = 0u64;
    while Instant::now() < deadline {
        if let Ok(out) = std::process::Command::new("ps")
            .args(["-o", "rss=", "-p", &pid.to_string()])
            .output()
            && let Ok(kib) = String::from_utf8_lossy(&out.stdout).trim().parse::<u64>()
        {
            peak = peak.max(kib);
        }
        std::thread::sleep(Duration::from_millis(250));
    }
    peak as f64 / 1024.0
}

/// Bytes under a directory. `du -s` in eight lines, because parsing `du` across
/// two platforms is more code than walking it.
fn du(dir: &std::path::Path) -> u64 {
    std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| match e.file_type() {
            Ok(t) if t.is_dir() => du(&e.path()),
            _ => e.metadata().map(|m| m.len()).unwrap_or(0),
        })
        .sum()
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
        Conn {
            addr: addr.to_string(),
            io: BufReader::new(dial(addr)),
        }
    }

    /// The bytes written, whether it was accepted, how many times the
    /// connection had to be rebuilt, and the response — so the caller can report
    /// wire throughput, backpressure and client-side damage separately, and a
    /// reader can find its cursor.
    fn post(&mut self, path: &str, body: &[u8]) -> Reply {
        let mut resets = 0;
        loop {
            match self.attempt(path, body) {
                Ok(mut r) => {
                    r.resets = resets;
                    return r;
                }
                // The socket is in an unknown state — a `send` that failed may
                // have queued part of the request, and nothing on this side can
                // tell how much. The only way back to a known state is a new
                // connection; resuming would feed the server a body starting
                // mid-protobuf, which it correctly reports as garbage.
                Err(_) => {
                    resets += 1;
                    std::thread::sleep(Duration::from_millis(5));
                    self.io = BufReader::new(dial(&self.addr));
                }
            }
        }
    }

    fn attempt(&mut self, path: &str, body: &[u8]) -> io::Result<Reply> {
        // OTLP/HTTP is protobuf; everything else here is a KYAML query document.
        let ct = match path.starts_with("/v1/") {
            true => "application/x-protobuf",
            false => "application/yaml",
        };
        let head = format!(
            "POST {path} HTTP/1.1\r\nHost: {}\r\nContent-Type: {ct}\r\n\
             Content-Length: {}\r\n\r\n",
            self.addr,
            body.len()
        );
        let s = self.io.get_mut();
        push(s, head.as_bytes())?;
        push(s, body)?;

        let mut line = String::new();
        self.io.read_line(&mut line)?;
        let status = line.trim().to_owned();
        // Read the response fully before judging it. Leaving a body in the
        // socket desynchronizes the next request on this keep-alive connection,
        // which then fails for a reason that has nothing to do with the bug.
        let mut len = 0usize;
        loop {
            line.clear();
            self.io.read_line(&mut line)?;
            if let Some(v) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                len = v.trim().parse().unwrap_or(0);
            }
            if line == "\r\n" || line.is_empty() {
                break;
            }
        }
        let mut msg = vec![0u8; len];
        self.io.read_exact(&mut msg)?;

        // 503 is the engine shedding load, which is a correct answer and not an
        // error: OTLP says retry. Anything else is a bug worth stopping on, and
        // the server's message is the whole reason to stop here.
        let ok = status.contains(" 200 ");
        assert!(
            ok || status.contains(" 503 "),
            "{path}: {status}: {}",
            String::from_utf8_lossy(&msg)
        );
        Ok(Reply {
            wrote: (head.len() + body.len()) as u64,
            ok,
            resets: 0,
            body: String::from_utf8_lossy(&msg).into_owned(),
        })
    }
}

struct Reply {
    wrote: u64,
    ok: bool,
    resets: u64,
    body: String,
}

fn dial(addr: &str) -> TcpStream {
    let s = TcpStream::connect(addr).unwrap_or_else(|e| panic!("connect {addr}: {e}"));
    s.set_nodelay(true).unwrap();
    s
}

/// `write_all`, but it gives up rather than guessing.
///
/// A hundred blocking sockets pushing megabyte bodies at loopback exhausts the
/// kernel's mbuf pool on macOS and the write comes back ENOBUFS. POSIX leaves
/// the transferred count unspecified on a failed `send`, so the retry that
/// looks obvious — reissue from the same offset — is the thing that corrupts
/// the stream. Chunking makes it rare; only reconnecting makes it correct.
fn push(s: &mut TcpStream, mut buf: &[u8]) -> io::Result<()> {
    /// Offer the kernel a socket buffer's worth at a time rather than a whole
    /// megabyte body: a smaller request is far likelier to find mbufs.
    const CHUNK: usize = 64 << 10;
    while !buf.is_empty() {
        match s.write(&buf[..buf.len().min(CHUNK)]) {
            Ok(0) => return Err(io::ErrorKind::WriteZero.into()),
            Ok(n) => buf = &buf[n..],
            // The two kinds that are defined to have transferred nothing.
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(1));
            }
            Err(e) => return Err(e),
        }
    }
    Ok(())
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

/// A counter, a gauge and a histogram per service *per connection* — one of
/// each kind the read path knows how to chart.
///
/// The connection is in the series identity, and has to be. A cumulative
/// counter is owned by exactly one producer; point every connection at one
/// stream and its value goes backwards on nearly every sample, which a reader
/// is right to read as a process restart. The chart becomes a sawtooth of
/// resets and the rate under it is noise. `service.instance.id` is OTel's name
/// for that owner, so that is the attribute that carries it.
fn metrics_batch(w: usize, n: u64, ts: u64) -> ExportMetricsServiceRequest {
    let dp = |v: f64, attrs: Vec<KeyValue>| NumberDataPoint {
        attributes: attrs,
        start_time_unix_nano: ts,
        time_unix_nano: ts,
        value: Some(number_data_point::Value::AsDouble(v)),
        ..Default::default()
    };
    // A sine over the batch counter: something with a shape, so a broken axis
    // or a dropped point is visible rather than plausible.
    let wave = |phase: f64| (n as f64 / 30.0 + phase).sin() * 0.5 + 0.5;

    ExportMetricsServiceRequest {
        resource_metrics: SERVICES
            .iter()
            .enumerate()
            .map(|(s, name)| ResourceMetrics {
                resource: Some(Resource {
                    attributes: vec![
                        kv("service.name", name),
                        kv("service.instance.id", &format!("{name}-{w:03}")),
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
                                    (n * batch() as u64) as f64,
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

fn trace_hex(k: u64) -> String {
    trace_id(k).iter().map(|b| format!("{b:02x}")).collect()
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
