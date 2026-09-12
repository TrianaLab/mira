//! The load harness: a synthetic OTLP source, a query driver, and a report on
//! all four axes at once.
//!
//! Three jobs. It fills a dev instance so the UI has something to draw; it is
//! the ingest benchmark; and with `--readers` it is the query benchmark, run
//! *while* ingest is running, because a p99 measured on a quiet store is a
//! number no operator will ever see.
//!
//! ponytail: this ships in no installable artifact. It is a cargo example, so
//! `cargo install mira`, the release tarball and the Docker image all lack it —
//! only someone with the repo checked out can fill an instance. Moving the
//! generator into the binary as a `mira demo` subcommand is the upgrade path and
//! was deliberately deferred: it puts `prost` encode paths and eight hundred
//! lines of fake shop into the artifact whose size is one of the four axes, to
//! serve a first-run experience that `make demo` already covers for the people
//! who have the repo. Revisit the day someone who installed a binary asks how to
//! see it working.
//!
//! Two modes, because the two jobs want opposite things:
//!
//! * the default is the **benchmark** generator — one resource per export, a
//!   flat span chain, timestamps that track the wall clock. Every field derives
//!   from a counter, so two runs produce the same bytes and their numbers are
//!   comparable.
//! * `--demo` is the **realistic** generator — a four-service shop, traces that
//!   cross service boundaries, exception events, links, exemplars, structured
//!   bodies and nested attributes, backdated across a window so a chart has
//!   shape on first load. It is what `make demo` runs.
//!
//! The split is deliberate and it is a performance decision, not a taste one.
//! Realism costs allocations per record — several resources per export, an event
//! vector, a nested `AnyValue` — and section 11's ingest number has to stay a
//! measurement of the engine rather than of how baroque the generator got. So
//! `--demo` is off unless asked for, and the benchmark path below is untouched
//! by any of it.
//!
//! section 11 scores four axes and the principle is that they are scored together —
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
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime};

use prost::Message;

use mira_proto::collector::logs::v1::ExportLogsServiceRequest;
use mira_proto::collector::metrics::v1::ExportMetricsServiceRequest;
use mira_proto::collector::trace::v1::ExportTraceServiceRequest;
use mira_proto::common::v1::{
    AnyValue, ArrayValue, InstrumentationScope, KeyValue, KeyValueList, any_value,
};
use mira_proto::logs::v1::{LogRecord, ResourceLogs, ScopeLogs};
use mira_proto::metrics::v1::{
    AggregationTemporality, Exemplar, Gauge, Histogram, HistogramDataPoint, Metric,
    NumberDataPoint, ResourceMetrics, ScopeMetrics, Sum, exemplar, metric, number_data_point,
};
use mira_proto::resource::v1::Resource;
use mira_proto::trace::v1::span::SpanKind;
use mira_proto::trace::v1::{ResourceSpans, ScopeSpans, Span, Status, span, status};

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

/// Cleared when the writers are done, and only in `--demo`.
///
/// A benchmark run ends on a deadline, so the readers and the RSS sampler can
/// share it. A `--demo` run does not: `--for` is the width of the history being
/// laid down, not the length of the run, so `--demo --for 45m --pid N` would
/// keep `ps` running for forty-five minutes after ten seconds of work — the
/// process looks hung and the peak it eventually reports is of an idle server.
/// Only `--demo` clears this: `--conns 0 --readers 8` has no writers to clear
/// it, and its readers must still run to the deadline.
static WRITING: AtomicBool = AtomicBool::new(true);

fn main() {
    let mut addr = "127.0.0.1:4318".to_string();
    let mut secs = 30u64;
    let mut conns = 8usize;
    let mut readers = 0usize;
    let mut pid: Option<u32> = None;
    let mut data_dir: Option<std::path::PathBuf> = None;
    let mut demo = false;
    let mut batch_set = false;
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        let mut v = || args.next().expect("missing value");
        match a.as_str() {
            "--addr" => addr = v(),
            "--for" => secs = duration(&v()),
            "--conns" => conns = v().parse().expect("--conns"),
            "--readers" => readers = v().parse().expect("--readers"),
            "--pid" => pid = Some(v().parse().expect("--pid")),
            "--data-dir" => data_dir = Some(v().into()),
            "--batch" => {
                batch_set = true;
                BATCH.store(v().parse().expect("--batch"), Ordering::Relaxed);
            }
            "--demo" => demo = true,
            "--selftest" => return selftest(),
            _ => {
                eprintln!(
                    "usage: loadgen [--addr host:port] [--for 30s] [--conns 8] [--batch 2000]\n\
                     \x20              [--readers 0] [--pid N] [--data-dir PATH]\n\
                     \x20              [--demo] [--selftest]\n\
                     \n\
                     --demo generates a realistic four-service shop instead of the\n\
                     benchmark filler, and `--for` then means how much *history* to\n\
                     backdate rather than how long to run — `--demo --for 45m` finishes\n\
                     in seconds and leaves 45 minutes of telemetry on disk."
                );
                std::process::exit(2);
            }
        }
    }
    assert!(
        conns + readers > 0,
        "nothing to do: --conns and --readers are both 0"
    );
    // The benchmark wants batches big enough to seal a block on size (section 11); the
    // demo wants enough separate exports to give the timeline granularity, and a
    // demo batch is whole traces rather than records. Two different right
    // answers for one flag, so the default follows the mode and an explicit
    // `--batch` still wins.
    if demo && !batch_set {
        BATCH.store(DEMO_TRACES_PER_BATCH, Ordering::Relaxed);
    }

    // Cost per GB is a delta, not a total: run against a store that already has
    // blocks in it and the total divided by this run's records is meaningless.
    let disk0 = data_dir.as_deref().map(du).unwrap_or(0);

    // Same idea one axis over: the ingest number worth comparing is per core,
    // and the only honest denominator is CPU the server actually burned, not
    // the core count of the box. A delta for the same reason `disk0` is one —
    // the process may have been running for hours before the run.
    let cpu0 = pid.and_then(cpu_seconds);

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
    // In `--demo` the run is a fixed amount of work, not a deadline: `--for` is
    // the width of the history to lay down, every connection owns an equal share
    // of the slots in it, and the run is over when the last slot is acked.
    let plan = Demo {
        base,
        window: secs * 1_000_000_000,
        slots: conns as u64 * DEMO_BATCHES,
    };
    // And the three signals get a connection each rather than sharing one. They
    // have independent flushers and an export is acked only once its own block
    // seals, so a worker posting traces, then logs, then metrics down one socket
    // pays three block ages per round instead of one — measured, that is 25
    // seconds for the demo against 8. Nobody is reading a throughput number off
    // a `--demo` run, and the person watching this one is watching a terminal.
    let threads = conns * if demo { SIGNALS.len() } else { 1 };

    let workers: Vec<_> = (0..threads)
        .map(|k| {
            let (w, sig) = (k % conns.max(1), k / conns.max(1));
            let addr = addr.clone();
            std::thread::spawn(move || {
                let mut conn = Conn::connect(&addr);
                let mut s = Stats::default();
                // `n` counts this connection's own batches; `i` interleaves the
                // connections into one gapless sequence, which is what keeps the
                // content deterministic. Metrics need `n`: a cumulative counter
                // reports what its own producer has sent.
                let mut n = 0u64;
                loop {
                    let i = n * conns as u64 + w as u64;
                    if demo {
                        if n >= DEMO_BATCHES {
                            break;
                        }
                        plan.slot(&mut s, &mut conn, sig, w, i);
                        n += 1;
                        continue;
                    }
                    if Instant::now() >= deadline {
                        break;
                    }
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
    let rss = pid.map(|pid| std::thread::spawn(move || peak_memory(pid, deadline)));

    let mut all = Stats::default();
    for w in workers {
        all.merge(w.join().unwrap());
    }
    if demo {
        WRITING.store(false, Ordering::Relaxed);
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
    // Per core, and measured rather than assumed. An aggregate rate is a
    // property of the offered load as much as of the engine — raise the
    // connection count and it moves without a line of the server changing — so
    // the figure worth putting in a table is the one with CPU underneath it.
    if let (Some(a), Some(b)) = (cpu0, pid.and_then(cpu_seconds)) {
        let cores = (b - a) / el;
        print!("cpu      {cores:.2} cores busy");
        let n = all.logs + all.spans + all.points;
        if n > 0 && cores > 0.0 {
            print!("   {:.0} records/s/core", n as f64 / el / cores);
        }
        println!();
    }
    reads.report(el);
    if let Some(rss) = rss {
        let (rss, anon) = rss.join().unwrap();
        print!("memory   peak RSS {rss:.0} MiB");
        // Absent rather than 0 when the platform did not answer: a zero here
        // would read as "this engine allocates nothing", which is the one thing
        // it cannot mean.
        if anon > 0.0 {
            print!(
                "   peak anonymous {anon:.0} MiB  (the open blocks; RSS also counts mapped block pages)"
            );
        }
        println!();
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
    while Instant::now() < deadline && WRITING.load(Ordering::Relaxed) {
        for class in 0..CLASSES.len() {
            if Instant::now() >= deadline || !WRITING.load(Ordering::Relaxed) {
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

/// Peak memory of the server, in MiB, sampled from outside it.
///
/// From outside on purpose. Mira reads through `mmap`, so most of what it costs
/// a machine is page cache the process never allocated — a heap counter would
/// report a number that is flattering and wrong.
///
/// Two numbers, because RSS alone cannot answer the question section 11 actually asks.
/// The axis is "resident footprint ≤ 2 × the open block's target size", and RSS
/// counts every mapped block page a query touched, so on a read-heavy run it
/// measures the corpus rather than the engine. The second number is
/// **anonymous** memory — what the process allocated rather than mapped — and
/// that one *is* the open blocks, the in-flight decodes and the staging copies.
/// It is the figure section 11's footprint row was owed and never had.
///
/// Sampled at two cadences because they cost different amounts: `ps` is a fork
/// and an exec, and on macOS the anonymous figure needs `vmmap`, which takes
/// well over a second. A peak over a 30-second run does not need 4 Hz.
fn peak_memory(pid: u32, deadline: Instant) -> (f64, f64) {
    let mut peak_rss = 0u64;
    let mut peak_anon = 0u64;
    let mut next_anon = Instant::now();
    while Instant::now() < deadline && WRITING.load(Ordering::Relaxed) {
        // Not a `let` chain: those are stable since 1.88 and the workspace
        // MSRV is 1.85.
        let sample = std::process::Command::new("ps")
            .args(["-o", "rss=", "-p", &pid.to_string()])
            .output()
            .ok()
            .and_then(|out| {
                String::from_utf8_lossy(&out.stdout)
                    .trim()
                    .parse::<u64>()
                    .ok()
            });
        if let Some(kib) = sample {
            peak_rss = peak_rss.max(kib);
        }
        if Instant::now() >= next_anon {
            if let Some(kib) = anon_kib(pid) {
                peak_anon = peak_anon.max(kib);
            }
            next_anon = Instant::now() + Duration::from_secs(2);
        }
        std::thread::sleep(Duration::from_millis(250));
    }
    (peak_rss as f64 / 1024.0, peak_anon as f64 / 1024.0)
}

/// Total CPU seconds `pid` has consumed since it started.
///
/// `ps -o cputime=` prints `MM:SS.ss`, and grows a leading `HH:` once the
/// process passes an hour — which a dev instance does long before anyone
/// benchmarks against it. Folding left over the colon-separated parts is right
/// for either shape without having to know which one arrived.
///
/// `None` rather than zero when it cannot be read, for the reason `anon_kib`
/// gives: a confident zero here divides into an infinite records/s/core.
fn cpu_seconds(pid: u32) -> Option<f64> {
    let out = std::process::Command::new("ps")
        .args(["-o", "cputime=", "-p", &pid.to_string()])
        .output()
        .ok()?;
    let text = String::from_utf8_lossy(&out.stdout);
    let mut secs = 0.0;
    for part in text.trim().split(':') {
        secs = secs * 60.0 + part.parse::<f64>().ok()?;
    }
    Some(secs)
}

/// Anonymous (non-file-backed) memory of `pid`, in KiB.
///
/// `None` rather than zero when it cannot be read, so a platform that does not
/// answer prints nothing instead of printing a confident 0 MiB.
#[cfg(target_os = "linux")]
fn anon_kib(pid: u32) -> Option<u64> {
    // `RssAnon` is exactly the split this wants and the kernel maintains it, so
    // there is nothing to fork and nothing to parse beyond one line.
    std::fs::read_to_string(format!("/proc/{pid}/status"))
        .ok()?
        .lines()
        .find_map(|l| l.strip_prefix("RssAnon:"))
        .and_then(|v| v.split_whitespace().next()?.parse().ok())
}

/// macOS has no `/proc`, and `ps` reports no anonymous split. `vmmap -summary`
/// does: its `TOTAL` row's DIRTY and SWAPPED columns are the pages that are not
/// backed by a file, which is the same quantity `RssAnon` names on Linux.
/// Swapped is added rather than ignored because a page the kernel pushed out is
/// still memory the process asked for.
#[cfg(not(target_os = "linux"))]
fn anon_kib(pid: u32) -> Option<u64> {
    let out = std::process::Command::new("vmmap")
        .args(["-summary", &pid.to_string()])
        .output()
        .ok()?;
    let text = String::from_utf8_lossy(&out.stdout);
    let total = text.lines().find(|l| l.starts_with("TOTAL "))?;
    let mut f = total.split_whitespace().skip(1);
    // VIRTUAL, RESIDENT, DIRTY, SWAPPED.
    let dirty = vmmap_kib(f.nth(2)?)?;
    let swapped = vmmap_kib(f.next()?)?;
    Some(dirty + swapped)
}

/// `vmmap` prints sizes as `272K`, `55.4M` or `1.2G`. There is no flag to make
/// it print bytes.
#[cfg(not(target_os = "linux"))]
fn vmmap_kib(s: &str) -> Option<u64> {
    let (num, scale) = match s.as_bytes().last()? {
        b'K' => (&s[..s.len() - 1], 1.0),
        b'M' => (&s[..s.len() - 1], 1024.0),
        b'G' => (&s[..s.len() - 1], 1024.0 * 1024.0),
        _ => (s, 1.0 / 1024.0),
    };
    Some((num.parse::<f64>().ok()? * scale) as u64)
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

/// Long enough that no real acknowledgement reaches it — the slowest measured
/// p99 is the log-off block-seal path at 2.6 s and its max is under 4 — and
/// short enough that a peer which will never answer is caught in a minute.
const READ_TIMEOUT: Duration = Duration::from_secs(60);

/// Consecutive failed attempts at *one* body before `post` gives up. The reset
/// path exists for ENOBUFS, which clears on the next try; a body that cannot
/// get through twenty times running has a cause a retry will not fix.
const MAX_RESETS: u64 = 20;

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
                // One body failing this many times in a row is not the mbuf
                // exhaustion the reset path exists for — that clears on the
                // next attempt. It is structural, and the likeliest structure
                // is the wrong port.
                Err(e) if resets >= MAX_RESETS => {
                    eprintln!(
                        "loadgen: {} POSTs to {} in a row failed, last: {e}\n\
                         \x20 loadgen speaks OTLP/HTTP, so --addr wants the HTTP\n\
                         \x20 port (default 4318), not the gRPC one (4317). A gRPC\n\
                         \x20 listener accepts the connection and then never replies.",
                        resets, self.addr
                    );
                    std::process::exit(1);
                }
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

/// Connect, or say what to start.
///
/// Not a panic. Nothing is listening on 4318 the first time anyone runs this,
/// so an eighteen-line backtrace through `unwrap_or_else` is the *most likely*
/// first experience of the harness, and it names a Rust source location instead
/// of the one thing that would fix it. One line, the address, and the command.
///
/// Every path into a socket goes through here — the first connect and the
/// mid-run reconnect after an ENOBUFS reset both — so this is also the right
/// place for it: a server that dies half way through a run wants the same
/// sentence, not a different one.
fn dial(addr: &str) -> TcpStream {
    match TcpStream::connect(addr) {
        Ok(s) => {
            let _ = s.set_nodelay(true);
            // A read timeout, not because a slow ack is an error — the log-off
            // p99 is seconds — but because a peer that will *never* answer is
            // otherwise indistinguishable from one that is thinking. The
            // commonest way to get one is `--addr` on the gRPC port: the
            // listener accepts the connection, cannot parse HTTP/1.1 as HTTP/2,
            // and says nothing, so a run with no timeout hangs forever with no
            // output at all. `post` turns the timeout into the sentence below.
            let _ = s.set_read_timeout(Some(READ_TIMEOUT));
            s
        }
        Err(e) => {
            eprintln!(
                "loadgen: cannot reach {addr}: {e}\n\
                 \x20 nothing is listening there. start Mira first:\n\
                 \x20   mira --data-dir ./mira-data\n\
                 \x20 or let one command do the whole thing:\n\
                 \x20   make demo"
            );
            std::process::exit(1);
        }
    }
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
            // Not decoration, and unconditional rather than demo-only: the
            // README offers `{"attr":"service.instance.id","eq":"…"}` as the
            // answer to "everything this pod emitted", and until this line
            // existed that predicate matched nothing anywhere in the tree —
            // metrics set it, logs and spans did not. One `KeyValue` per export,
            // not per record, so the benchmark path can afford it.
            kv("service.instance.id", &format!("instance-{:04}", i % 16)),
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
                            data: Some(metric::Data::Histogram(Histogram {
                                data_points: vec![histogram_point(
                                    ts,
                                    ts,
                                    batch() as f64,
                                    vec![kv("http.method", "POST")],
                                    Vec::new(),
                                )],
                                aggregation_temporality: AggregationTemporality::Delta as i32,
                            })),
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

/// `30s`, `45m`, `2h`, or a bare count of seconds.
///
/// `--for` used to be `trim_end_matches('s')` and a `parse`, so `--for 45m`
/// panicked with `invalid digit found in string` and no mention of `--for`.
/// `--demo` makes minutes the natural unit for it, so it now understands them.
fn duration(s: &str) -> u64 {
    let (n, mul) = match s.as_bytes().last() {
        Some(b'h') => (&s[..s.len() - 1], 3_600),
        Some(b'm') => (&s[..s.len() - 1], 60),
        Some(b's') => (&s[..s.len() - 1], 1),
        _ => (s, 1),
    };
    n.parse::<u64>()
        .unwrap_or_else(|_| panic!("--for {s}: expected a duration like 30s, 45m or 2h"))
        * mul
}

/// One histogram point whose buckets, `count` and `sum` agree.
///
/// They did not. `count` was the batch size while `bucket_counts` was a
/// hard-coded `[300, 150, 40, 9, 1]`, so the two matched only at `--batch 500`
/// and were 4× apart at the default. A histogram whose buckets do not sum to its
/// count is not a histogram: the derived `.count` series and the bucket series
/// then tell two different stories about the same point, and nothing in the read
/// path can say which of them is the lie. Deriving all three from one number is
/// the only version that cannot drift.
fn histogram_point(
    start: u64,
    ts: u64,
    requests: f64,
    attributes: Vec<KeyValue>,
    exemplars: Vec<Exemplar>,
) -> HistogramDataPoint {
    /// The shape of a healthy HTTP latency histogram — most requests fast, a fat
    /// middle, a thin tail. Fractions rather than counts, so the shape survives
    /// any volume.
    const SHAPE: [f64; 5] = [0.55, 0.30, 0.10, 0.04, 0.01];
    /// A representative latency per bucket, for `sum`. The last bucket is
    /// unbounded, so its representative is a judgement call and not a mean.
    const MID: [f64; 5] = [2.5, 15.0, 62.5, 300.0, 900.0];

    let bucket_counts: Vec<u64> = SHAPE.iter().map(|f| (requests * f) as u64).collect();
    HistogramDataPoint {
        attributes,
        start_time_unix_nano: start,
        time_unix_nano: ts,
        count: bucket_counts.iter().sum(),
        sum: Some(
            bucket_counts
                .iter()
                .zip(MID)
                .map(|(&c, m)| c as f64 * m)
                .sum(),
        ),
        explicit_bounds: vec![5.0, 25.0, 100.0, 500.0],
        bucket_counts,
        exemplars,
        ..Default::default()
    }
}

// ---------------------------------------------------------------------------
// The demo generator: a four-service shop, backdated.
// ---------------------------------------------------------------------------

/// Time slots each connection lays down in `--demo`.
///
/// Deliberately small. Mira acks an export only once the block holding it is
/// fsynced, and a block seals on size *or* a 2 s age timer, so a demo nowhere
/// near the size threshold costs one block age per round of batches. Four rounds
/// is under ten seconds of wall clock for the whole thing, which is the budget
/// for something a human is sitting and watching.
const DEMO_BATCHES: u64 = 4;

/// Traces per demo export, and so the granularity of the timeline: every trace
/// gets its own instant inside its slot.
const DEMO_TRACES_PER_BATCH: usize = 120;

/// Metric points per series per slot. Two points are not a chart, and two points
/// is exactly what `--for 10s` used to produce.
const DEMO_POINTS: u64 = 12;

/// One trace in every this many fails, all the way up the call chain.
const DEMO_BROKEN: u64 = 13;

/// The shape of one demo trace.
///
/// Columns: service, span name, kind, parent row (`-1` is the root), start
/// microseconds after the root's start, duration in microseconds.
///
/// Eight spans over four services. `service.name` lives on the *Resource*, so a
/// trace crossing a service boundary is by construction several `ResourceSpans`
/// in one export — which is why the benchmark generator, with its single
/// resource per export, could not produce one however deep it chained the
/// parents, and why every correlation claim in the README was unobservable
/// locally. This table is the fix.
///
/// The offsets nest: each row's window lies inside its parent's. That is
/// checked, not asserted by eye — `--selftest` walks it.
const GRAPH: [(&str, &str, SpanKind, i8, u64, u64); 8] = [
    (
        "frontend",
        "POST /checkout",
        SpanKind::Server,
        -1,
        0,
        92_000,
    ),
    ("frontend", "GET /items", SpanKind::Client, 0, 2_000, 21_000),
    (
        "inventory",
        "GET /items",
        SpanKind::Server,
        1,
        3_000,
        18_000,
    ),
    ("frontend", "POST /pay", SpanKind::Client, 0, 26_000, 62_000),
    ("checkout", "POST /pay", SpanKind::Server, 3, 27_000, 60_000),
    (
        "checkout",
        "SELECT orders",
        SpanKind::Client,
        4,
        30_000,
        12_000,
    ),
    (
        "checkout",
        "POST /authorize",
        SpanKind::Client,
        4,
        44_000,
        40_000,
    ),
    (
        "payments",
        "POST /authorize",
        SpanKind::Server,
        6,
        45_000,
        38_000,
    ),
];

/// The database call. A client span with `db.*` attributes rather than a service
/// of its own, because that is what an instrumented application emits — the
/// database is not running an OTel SDK.
const DEMO_DB_HOP: usize = 5;

/// Where the failure is raised, and the callers that see it. A leaf that fails
/// while its callers stay green is a shape nobody debugging a real incident
/// meets, and it would make the error filter look better than it is.
const DEMO_ERROR_PATH: [usize; 5] = [0, 3, 4, 6, 7];

/// The services in [`GRAPH`], in first-appearance order.
const DEMO_SERVICES: [&str; 4] = ["frontend", "inventory", "checkout", "payments"];

/// The OTLP/HTTP paths, in the order a `--demo` thread's index selects them.
const SIGNALS: [&str; 3] = ["/v1/traces", "/v1/logs", "/v1/metrics"];

/// The run-wide shape of a `--demo` run.
///
/// A struct rather than five more positional parameters on the call below: the
/// three numbers are fixed for the whole run and only the slot moves.
#[derive(Clone, Copy)]
struct Demo {
    /// The instant the generated history ends — now.
    base: u64,
    /// How much history to lay down, in nanoseconds, ending at `base`.
    window: u64,
    /// How many slots the window is cut into.
    slots: u64,
}

impl Demo {
    /// One connection's share of one time slot, for one signal: the same shop,
    /// some minutes ago.
    fn slot(&self, s: &mut Stats, conn: &mut Conn, sig: usize, w: usize, i: u64) {
        // Slot `i` of `slots`, oldest first, ending now. Backdating is the whole
        // reason the first screen looks like an application that has been up for
        // a while: stamping everything with the wall clock, as the benchmark
        // path does, puts every log row on one millisecond and every chart on
        // two points. Keeping the window inside the hour the UI defaults to is
        // `make demo`'s job, not this function's.
        let slot = (self.window / self.slots.max(1)).max(1);
        let origin = self.base.saturating_sub(self.window);
        let t0 = origin + i * slot;
        let traces = batch() as u64;
        let first = i * traces;

        match SIGNALS[sig] {
            "/v1/traces" => {
                let r = demo_spans(first, traces, t0, slot);
                s.spans += count_spans(&r);
                s.send(conn, SIGNALS[sig], &r.encode_to_vec());
                // The newest trace id on the server, for `--readers`.
                TRACES.fetch_max(first + traces.saturating_sub(1), Ordering::Relaxed);
            }
            "/v1/logs" => {
                let r = demo_logs(first, traces, t0, slot, w);
                s.logs += count_logs(&r);
                s.send(conn, SIGNALS[sig], &r.encode_to_vec());
            }
            _ => {
                let r = demo_metrics(w, i, origin, t0, slot, first, traces);
                s.points += count_points(&r);
                s.send(conn, SIGNALS[sig], &r.encode_to_vec());
            }
        }
    }
}

/// A deterministic per-trace scale, 0.70× to 1.30×, in thousandths.
///
/// One factor for the whole trace rather than one per span: a child scaled
/// differently from its parent escapes it, and a waterfall with a child
/// outliving its parent reads as a bug in the viewer rather than as jitter.
fn jitter(trace: u64) -> u64 {
    700 + trace.wrapping_mul(2_654_435_761) % 601
}

/// Microseconds from [`GRAPH`], jittered and converted to nanoseconds.
fn scaled(us: u64, trace: u64) -> u64 {
    us * jitter(trace) / 1_000 * 1_000
}

/// A demo span's id, from the trace and the row rather than from a running
/// counter — so `parent_span_id` is computable without carrying the parent.
fn demo_span_id(trace: u64, hop: usize) -> bytes::Bytes {
    span_id(trace.wrapping_mul(16) + hop as u64)
}

/// The service on the far end of a client span: whoever this row's child runs
/// in. For a server span, its own service.
fn callee(hop: usize) -> &'static str {
    GRAPH
        .iter()
        .find(|g| g.3 == hop as i8)
        .map_or(GRAPH[hop].0, |g| g.0)
}

/// The row where a service handles the request, which is where its logs hang.
fn server_hop(svc: &str) -> usize {
    GRAPH
        .iter()
        .position(|g| g.0 == svc && matches!(g.2, SpanKind::Server))
        .unwrap_or(0)
}

/// The instant a trace starts: evenly spread across its slot, so a log list has
/// distinct timestamps and a chart has points to join.
fn trace_start(t0: u64, t: u64, traces: u64, slot: u64) -> u64 {
    t0 + t * slot / traces.max(1)
}

fn demo_spans(first: u64, traces: u64, t0: u64, slot: u64) -> ExportTraceServiceRequest {
    ExportTraceServiceRequest {
        resource_spans: DEMO_SERVICES
            .iter()
            .map(|&svc| ResourceSpans {
                // The connection index is not in the span resource: a span is
                // not a cumulative stream, so splitting the four services into
                // 4×conns resources would only cost dedup rows.
                resource: Some(demo_resource(svc, 0)),
                scope_spans: vec![ScopeSpans {
                    scope: scope(),
                    spans: (0..traces)
                        .flat_map(move |t| {
                            let trace = first + t;
                            let start = trace_start(t0, t, traces, slot);
                            (0..GRAPH.len())
                                .filter(move |&h| GRAPH[h].0 == svc)
                                .map(move |h| demo_span(trace, h, start))
                        })
                        .collect(),
                    ..Default::default()
                }],
                ..Default::default()
            })
            .collect(),
    }
}

fn demo_span(trace: u64, hop: usize, t0: u64) -> Span {
    let (_, name, kind, parent, off, dur) = GRAPH[hop];
    let start = t0 + scaled(off, trace);
    let broken = trace % DEMO_BROKEN == 0;
    let failed = broken && DEMO_ERROR_PATH.contains(&hop);

    let attributes = if hop == DEMO_DB_HOP {
        vec![
            kv("db.system.name", "postgresql"),
            kv("db.namespace", "orders"),
            kv("db.collection.name", "orders"),
            kv(
                "db.query.text",
                "SELECT id, total, state FROM orders WHERE user_id = $1",
            ),
            kv("server.address", "orders-db.shop.svc"),
        ]
    } else {
        let (method, path) = name.split_once(' ').unwrap_or(("GET", name));
        vec![
            kv("http.request.method", method),
            kv("http.route", path),
            kv("url.path", path),
            // An `int_value`, not the string the benchmark path sends: OTLP/JSON
            // writes 64-bit integers as strings in both directions and the demo
            // is the only place anyone will notice if that stops round-tripping.
            int("http.response.status_code", if failed { 503 } else { 200 }),
            kv("server.address", &format!("{}.shop.svc", callee(hop))),
            kv("user.id", &format!("u-{:05}", trace % 400)),
        ]
    };

    Span {
        trace_id: trace_id(trace),
        span_id: demo_span_id(trace, hop),
        parent_span_id: match parent {
            -1 => Vec::new().into(),
            p => demo_span_id(trace, p as usize),
        },
        name: name.into(),
        kind: kind as i32,
        start_time_unix_nano: start,
        end_time_unix_nano: start + scaled(dur, trace),
        attributes,
        events: demo_events(trace, hop, start, dur),
        links: demo_links(trace, hop),
        status: Some(Status {
            // Unset on the happy path, not Ok. An SDK records a status only when
            // the application asks for one, so a store where every span says OK
            // teaches a filter that matches nothing in production.
            code: match failed {
                true => status::StatusCode::Error,
                false => status::StatusCode::Unset,
            } as i32,
            message: match failed {
                true => "authorization upstream returned 503".into(),
                false => String::new(),
            },
        }),
        // Sampled. Zero flags on every span is the other thing that marks
        // synthetic data at a glance.
        flags: 1,
        ..Default::default()
    }
}

/// Span events, including the exception the README's correlation bullet is about.
///
/// The OTel convention is not an attribute on the span — it is an event named
/// `exception` carrying `exception.type`, `exception.message` and
/// `exception.stacktrace`. That distinction is the whole reason the span-event
/// attribute filter exists, and
/// `{"attr":"exception.type","eq":"payments.CardDeclined"}` against demo data is
/// the query that proves it works end to end — there is no existence operator,
/// so the value has to be one this file actually emits.
fn demo_events(trace: u64, hop: usize, start: u64, dur: u64) -> Vec<span::Event> {
    let broken = trace % DEMO_BROKEN == 0;
    let mut out = Vec::new();

    // Raised where it happens — the payments handler — not where it is reported.
    if broken && hop == 7 {
        out.push(span::Event {
            time_unix_nano: start + scaled(dur, trace) * 9 / 10,
            name: "exception".into(),
            attributes: vec![
                kv("exception.type", "payments.CardDeclined"),
                kv(
                    "exception.message",
                    "issuer declined authorization: insufficient_funds",
                ),
                kv(
                    "exception.stacktrace",
                    "payments/authorize.go:118 Authorize\n\
                     payments/handler.go:64  (*Server).Pay\n\
                     net/http/server.go:2166 HandlerFunc.ServeHTTP",
                ),
                b("exception.escaped", true),
            ],
            ..Default::default()
        });
    }
    // The caller retries once before giving up, which is why the client span is
    // longer than the server span it wraps.
    if broken && hop == 6 {
        out.push(span::Event {
            time_unix_nano: start + scaled(dur, trace) / 3,
            name: "retry".into(),
            attributes: vec![int("retry.attempt", 1), kv("retry.reason", "503")],
            ..Default::default()
        });
    }
    // Something ordinary, so the waterfall is not only decorated where it broke.
    if hop == 2 && trace % 3 == 0 {
        out.push(span::Event {
            time_unix_nano: start + scaled(dur, trace) / 5,
            name: "cache.miss".into(),
            attributes: vec![kv("cache.key", &format!("sku:{:04}", trace % 240))],
            ..Default::default()
        });
    }
    out
}

/// Span links. A retried checkout points at the attempt it is retrying, which is
/// the commonest real link there is and the one a waterfall can show.
fn demo_links(trace: u64, hop: usize) -> Vec<span::Link> {
    if hop != 0 || trace == 0 || trace % 5 != 0 {
        return Vec::new();
    }
    vec![span::Link {
        trace_id: trace_id(trace - 1),
        span_id: demo_span_id(trace - 1, 0),
        attributes: vec![kv("link.kind", "retry_of")],
        flags: 1,
        ..Default::default()
    }]
}

/// One service's Resource.
///
/// Two things the benchmark's does not carry. `service.instance.id`, because a
/// cumulative counter belongs to exactly one producer and the README offers that
/// attribute as the answer to "everything this pod emitted". And
/// `process.command_args`, an *array* attribute — one of the two names the
/// README uses to claim nested attributes come back as what they are.
fn demo_resource(svc: &str, w: usize) -> Resource {
    let inst = w % 3;
    let bin = format!("/usr/local/bin/{svc}");
    Resource {
        attributes: vec![
            kv("service.name", svc),
            kv("service.namespace", "shop"),
            kv("service.version", "2.7.0"),
            kv("service.instance.id", &format!("{svc}-{inst}")),
            kv("deployment.environment.name", "prod"),
            kv("k8s.pod.name", &format!("{svc}-5d9f7c-{inst}")),
            kv("k8s.namespace.name", "shop"),
            kv("telemetry.sdk.language", "go"),
            arr("process.command_args", &[&bin, "--port", "8080"]),
        ],
        ..Default::default()
    }
}

fn demo_logs(first: u64, traces: u64, t0: u64, slot: u64, w: usize) -> ExportLogsServiceRequest {
    ExportLogsServiceRequest {
        resource_logs: DEMO_SERVICES
            .iter()
            .map(|&svc| {
                let hop = server_hop(svc);
                let mut scope_logs = vec![ScopeLogs {
                    scope: scope(),
                    log_records: (0..traces)
                        .map(|t| demo_log(svc, hop, first + t, trace_start(t0, t, traces, slot)))
                        .collect(),
                    ..Default::default()
                }];
                if svc == "frontend" {
                    scope_logs.push(demo_assistant(first, traces, t0, slot));
                }
                ResourceLogs {
                    resource: Some(demo_resource(svc, w)),
                    scope_logs,
                    ..Default::default()
                }
            })
            .collect(),
    }
}

fn demo_log(svc: &str, hop: usize, trace: u64, start: u64) -> LogRecord {
    let (_, name, _, _, off, dur) = GRAPH[hop];
    // Logged when the handler returns, so a log line lands at the right end of
    // its span in the waterfall rather than at the left of every span at once.
    let ts = start + scaled(off + dur, trace);
    let ms = dur * jitter(trace) / 1_000_000;
    let failed = trace % DEMO_BROKEN == 0 && DEMO_ERROR_PATH.contains(&hop);
    let (severity, text) = if failed {
        (17, "ERROR")
    } else if trace % 7 == 0 {
        (13, "WARN")
    } else if trace % 3 == 0 {
        (5, "DEBUG")
    } else {
        (9, "INFO")
    };
    let (_, path) = name.split_once(' ').unwrap_or(("GET", name));

    LogRecord {
        time_unix_nano: ts,
        // A little after the event: that gap is what the column is for, and a
        // store where the two are always equal cannot show it.
        observed_time_unix_nano: ts + 1_500_000,
        severity_number: severity,
        severity_text: text.into(),
        // `LogRecord.event_name` has a column of its own and filters like a
        // field in both UIs. Nothing generated one until now.
        event_name: "http.server.request".into(),
        body: Some(match (svc, failed) {
            // A structured body, not a sentence with numbers baked into it.
            // "Nested attributes come back as what they are" is a README bullet
            // and a kvlist body is precisely the case it is about.
            ("inventory", _) => map_value(vec![
                kv("event", "inventory.lookup"),
                kv("sku", &format!("SKU-{:04}", trace % 240)),
                int("qty", (trace % 5) as i64),
                int("latency_ms", ms as i64),
                b("in_stock", trace % 11 != 0),
            ]),
            (_, true) => text_value(&format!(
                "{name} failed: upstream returned 503 after {ms}ms"
            )),
            (_, false) => text_value(&format!("{name} 200 in {ms}ms")),
        }),
        attributes: vec![
            kv("http.route", path),
            int("http.response.status_code", if failed { 503 } else { 200 }),
            arr(
                "http.request.header.accept",
                &["application/json", "text/html"],
            ),
            kv("thread.name", &format!("worker-{}", trace % 4)),
        ],
        trace_id: trace_id(trace),
        span_id: demo_span_id(trace, hop),
        flags: 1,
        ..Default::default()
    }
}

/// A second scope on the frontend, emitting GenAI records.
///
/// Not colour. Principle 2's second reading is telemetry *for* AI workloads, and
/// the concrete demand it puts on the store is that a prompt survives as a
/// nested `AnyValue` rather than as a dictionary key. `gen_ai.input.messages` is
/// an array of maps — two levels — which is the shape a decoder that
/// special-cases one level gets wrong, and it is the other attribute name the
/// README offers as proof.
fn demo_assistant(first: u64, traces: u64, t0: u64, slot: u64) -> ScopeLogs {
    ScopeLogs {
        scope: Some(InstrumentationScope {
            name: "shop.assistant".into(),
            version: "0.3.1".into(),
            ..Default::default()
        }),
        log_records: (0..traces)
            .filter(|t| (first + t) % 7 == 0)
            .map(|t| {
                let trace = first + t;
                let ts = trace_start(t0, t, traces, slot) + scaled(40_000, trace);
                LogRecord {
                    time_unix_nano: ts,
                    observed_time_unix_nano: ts,
                    severity_number: 9,
                    severity_text: "INFO".into(),
                    event_name: "gen_ai.client.inference.operation.details".into(),
                    body: Some(text_value(
                        "support assistant answered an order-status question",
                    )),
                    attributes: vec![
                        kv("gen_ai.provider.name", "anthropic"),
                        kv("gen_ai.operation.name", "chat"),
                        kv("gen_ai.request.model", "claude-sonnet-4-5"),
                        int("gen_ai.usage.input_tokens", 180 + (trace % 90) as i64),
                        int("gen_ai.usage.output_tokens", 40 + (trace % 30) as i64),
                        KeyValue {
                            key: "gen_ai.input.messages".into(),
                            value: Some(AnyValue {
                                value: Some(any_value::Value::ArrayValue(ArrayValue {
                                    values: vec![map_value(vec![
                                        kv("role", "user"),
                                        kv(
                                            "content",
                                            &format!("where is order {}?", 1_000 + trace % 400),
                                        ),
                                    ])],
                                })),
                            }),
                        },
                    ],
                    trace_id: trace_id(trace),
                    span_id: demo_span_id(trace, 0),
                    flags: 1,
                    ..Default::default()
                }
            })
            .collect(),
        ..Default::default()
    }
}

fn demo_metrics(
    w: usize,
    i: u64,
    origin: u64,
    t0: u64,
    slot: u64,
    first: u64,
    traces: u64,
) -> ExportMetricsServiceRequest {
    let step = (slot / DEMO_POINTS).max(1);
    ExportMetricsServiceRequest {
        resource_metrics: DEMO_SERVICES
            .iter()
            .enumerate()
            .map(|(s, &svc)| {
                let at = |p: u64| t0 + p * step;
                // Phase from absolute time, not from a per-connection counter:
                // each connection owns a different slice of the window, so a
                // counter-derived phase would make the instances of one service
                // disagree about when the traffic peak was and the chart would
                // come out as noise.
                let wave = |ts: u64| {
                    let mins = (ts / 60_000_000_000) as f64;
                    (mins / 7.0 + s as f64).sin() * 0.5 + 0.5
                };
                let load = |p: u64| 300.0 + wave(at(p)) * 420.0;

                ResourceMetrics {
                    resource: Some(demo_resource(svc, w)),
                    scope_metrics: vec![ScopeMetrics {
                        scope: scope(),
                        metrics: vec![
                            Metric {
                                name: "http.server.request.duration".into(),
                                unit: "ms".into(),
                                description: "Duration of inbound HTTP requests".into(),
                                data: Some(metric::Data::Histogram(Histogram {
                                    aggregation_temporality: AggregationTemporality::Delta as i32,
                                    data_points: (0..DEMO_POINTS)
                                        .map(|p| {
                                            histogram_point(
                                                at(p),
                                                at(p) + step,
                                                load(p),
                                                vec![
                                                    kv("http.request.method", "POST"),
                                                    kv("http.route", "/checkout"),
                                                ],
                                                demo_exemplars(first, traces, p, at(p) + step),
                                            )
                                        })
                                        .collect(),
                                })),
                                ..Default::default()
                            },
                            Metric {
                                name: "http.server.requests".into(),
                                unit: "{request}".into(),
                                description: "Requests handled".into(),
                                data: Some(metric::Data::Sum(Sum {
                                    is_monotonic: true,
                                    aggregation_temporality: AggregationTemporality::Cumulative
                                        as i32,
                                    data_points: (0..DEMO_POINTS)
                                        .map(|p| NumberDataPoint {
                                            attributes: vec![
                                                kv("http.request.method", "POST"),
                                                int("http.response.status_code", 200),
                                            ],
                                            // One start for the whole stream, as
                                            // a cumulative counter requires: a
                                            // start that moves with the point is
                                            // how a rate ends up divided by the
                                            // wrong interval.
                                            start_time_unix_nano: origin,
                                            time_unix_nano: at(p) + step,
                                            value: Some(number_data_point::Value::AsInt(
                                                ((i * DEMO_POINTS + p + 1) * 350) as i64,
                                            )),
                                            ..Default::default()
                                        })
                                        .collect(),
                                })),
                                ..Default::default()
                            },
                            Metric {
                                name: "process.runtime.memory".into(),
                                unit: "By".into(),
                                data: Some(metric::Data::Gauge(Gauge {
                                    data_points: (0..DEMO_POINTS)
                                        .map(|p| NumberDataPoint {
                                            attributes: vec![kv("process.memory.type", "rss")],
                                            start_time_unix_nano: at(p),
                                            time_unix_nano: at(p) + step,
                                            value: Some(number_data_point::Value::AsDouble(
                                                1.8e8 + wave(at(p)) * 9.0e7,
                                            )),
                                            ..Default::default()
                                        })
                                        .collect(),
                                })),
                                ..Default::default()
                            },
                        ],
                        ..Default::default()
                    }],
                    ..Default::default()
                }
            })
            .collect(),
    }
}

/// Exemplars naming traces that exist.
///
/// The point of an exemplar is "which trace made this spike", and one pointing
/// at an id nobody ever wrote answers that with an empty page. These are ids
/// `demo_spans` is emitting into the same slot, so the hop from the chart to the
/// waterfall resolves — which is the only way to tell a correlation edge that
/// works from one that is merely stored. Two per point: the slow one and a
/// broken one, which are the two anybody clicks.
fn demo_exemplars(first: u64, traces: u64, p: u64, ts: u64) -> Vec<Exemplar> {
    if traces == 0 {
        return Vec::new();
    }
    let slow = p * 7 % traces;
    let broken = (0..traces)
        .find(|t| (first + t) % DEMO_BROKEN == 0)
        .unwrap_or(slow);
    [(slow, 180.0), (broken, 820.0)]
        .into_iter()
        .map(|(t, v)| {
            let trace = first + t;
            Exemplar {
                filtered_attributes: vec![kv("http.route", "/checkout")],
                time_unix_nano: ts,
                value: Some(exemplar::Value::AsDouble(v)),
                span_id: demo_span_id(trace, 0),
                trace_id: trace_id(trace),
            }
        })
        .collect()
}

// --- small OTLP value constructors, so the generators above read as data ------

fn int(k: &str, v: i64) -> KeyValue {
    KeyValue {
        key: k.into(),
        value: Some(AnyValue {
            value: Some(any_value::Value::IntValue(v)),
        }),
    }
}

fn b(k: &str, v: bool) -> KeyValue {
    KeyValue {
        key: k.into(),
        value: Some(AnyValue {
            value: Some(any_value::Value::BoolValue(v)),
        }),
    }
}

fn arr(k: &str, vs: &[&str]) -> KeyValue {
    KeyValue {
        key: k.into(),
        value: Some(AnyValue {
            value: Some(any_value::Value::ArrayValue(ArrayValue {
                values: vs.iter().map(|v| text_value(v)).collect(),
            })),
        }),
    }
}

fn text_value(v: &str) -> AnyValue {
    AnyValue {
        value: Some(any_value::Value::StringValue(v.into())),
    }
}

fn map_value(vs: Vec<KeyValue>) -> AnyValue {
    AnyValue {
        value: Some(any_value::Value::KvlistValue(KeyValueList { values: vs })),
    }
}

// --- counts, taken off the built request rather than from a formula -----------

fn count_spans(r: &ExportTraceServiceRequest) -> u64 {
    r.resource_spans
        .iter()
        .flat_map(|rs| &rs.scope_spans)
        .map(|ss| ss.spans.len() as u64)
        .sum()
}

fn count_logs(r: &ExportLogsServiceRequest) -> u64 {
    r.resource_logs
        .iter()
        .flat_map(|rl| &rl.scope_logs)
        .map(|sl| sl.log_records.len() as u64)
        .sum()
}

fn count_points(r: &ExportMetricsServiceRequest) -> u64 {
    r.resource_metrics
        .iter()
        .flat_map(|rm| &rm.scope_metrics)
        .flat_map(|sm| &sm.metrics)
        .map(|m| match &m.data {
            Some(metric::Data::Gauge(g)) => g.data_points.len() as u64,
            Some(metric::Data::Sum(s)) => s.data_points.len() as u64,
            Some(metric::Data::Histogram(h)) => h.data_points.len() as u64,
            _ => 0,
        })
        .sum()
}

// ---------------------------------------------------------------------------
// The self-check.
// ---------------------------------------------------------------------------

/// Everything the demo generator promises, asserted against two of its slots.
///
/// It lives behind a flag rather than in a `#[cfg(test)] mod tests` because
/// cargo does not run an example's tests unless the target opts in with
/// `[[example]] test = true`, and the manifest is not this file's to change. So
/// `make test` runs `loadgen --selftest` after the suite. It is the smallest
/// thing that goes red if the generator stops producing what the README, the
/// quickstart and docs/internals/e2e.md all claim it produces.
fn selftest() {
    const TRACES: u64 = 64;
    const T0: u64 = 1_757_000_000_000_000_000;
    const SLOT: u64 = 90_000_000_000;

    // --- traces cross a service boundary, and the tree is well formed --------

    let req = demo_spans(0, TRACES, T0, SLOT);
    let services: Vec<&str> = req
        .resource_spans
        .iter()
        .map(|rs| attr(&rs.resource.as_ref().unwrap().attributes, "service.name"))
        .collect();
    assert_eq!(
        services, DEMO_SERVICES,
        "one Resource per service, or no trace crosses a boundary"
    );

    let spans: Vec<&Span> = req
        .resource_spans
        .iter()
        .flat_map(|rs| &rs.scope_spans)
        .flat_map(|ss| &ss.spans)
        .collect();
    assert_eq!(spans.len() as u64, TRACES * GRAPH.len() as u64);

    let mut roots = 0;
    let mut errors = 0;
    let mut exceptions = 0;
    let mut links = 0;
    for s in &spans {
        let window = |x: &Span| (x.start_time_unix_nano, x.end_time_unix_nano);
        assert!(
            s.start_time_unix_nano >= T0 && s.end_time_unix_nano <= T0 + SLOT + 200_000_000,
            "span outside its slot: {s:?}"
        );
        if s.parent_span_id.is_empty() {
            roots += 1;
        } else {
            let parent = spans
                .iter()
                .find(|p| p.span_id == s.parent_span_id && p.trace_id == s.trace_id)
                .unwrap_or_else(|| panic!("orphan span {}", s.name));
            let (ps, pe) = window(parent);
            let (cs, ce) = window(s);
            assert!(
                cs >= ps && ce <= pe,
                "{} escapes its parent {}: [{cs},{ce}] vs [{ps},{pe}]",
                s.name,
                parent.name
            );
        }
        if s.status.as_ref().map(|x| x.code) == Some(status::StatusCode::Error as i32) {
            errors += 1;
        }
        links += s.links.len();
        for e in &s.events {
            if e.name == "exception" {
                exceptions += 1;
                for k in [
                    "exception.type",
                    "exception.message",
                    "exception.stacktrace",
                ] {
                    assert!(!attr(&e.attributes, k).is_empty(), "event missing {k}");
                }
            }
        }
    }
    assert_eq!(roots as u64, TRACES, "exactly one root per trace");
    assert!(errors > 0, "no span failed; the error filter demos nothing");
    assert!(exceptions > 0, "no exception event; the README claims one");
    assert!(links > 0, "no span links; the README claims those too");

    // Two slots must not land on top of each other — the flat timeline is the
    // bug this whole mode exists to fix.
    let later = demo_spans(TRACES, TRACES, T0 + SLOT, SLOT);
    let first_end = spans.iter().map(|s| s.start_time_unix_nano).max().unwrap();
    let next_start = later
        .resource_spans
        .iter()
        .flat_map(|rs| &rs.scope_spans)
        .flat_map(|ss| &ss.spans)
        .map(|s| s.start_time_unix_nano)
        .min()
        .unwrap();
    assert!(
        next_start > first_end,
        "slots overlap; the timeline is flat"
    );
    let starts: std::collections::BTreeSet<u64> =
        spans.iter().map(|s| s.start_time_unix_nano).collect();
    assert!(
        starts.len() as u64 >= TRACES,
        "{} distinct instants for {TRACES} traces: every row shares a millisecond",
        starts.len()
    );

    // --- logs: instance id, structured body, nested attributes ---------------

    let logs = demo_logs(0, TRACES, T0, SLOT, 1);
    let mut structured = 0;
    let mut arrays = 0;
    // An array whose elements are maps: two levels, which is the shape a decoder
    // that special-cases one level gets wrong, and the shape the GenAI
    // convention puts a prompt in.
    let mut nested = 0;
    for rl in &logs.resource_logs {
        let attrs = &rl.resource.as_ref().unwrap().attributes;
        assert!(
            !attr(attrs, "service.instance.id").is_empty(),
            "no service.instance.id: \"everything this pod emitted\" matches nothing"
        );
        for r in rl.scope_logs.iter().flat_map(|sl| &sl.log_records) {
            assert!(
                !r.trace_id.is_empty(),
                "log record with no trace to join to"
            );
            match r.body.as_ref().and_then(|v| v.value.as_ref()) {
                Some(any_value::Value::KvlistValue(_)) => structured += 1,
                Some(any_value::Value::StringValue(_)) => {}
                other => panic!("unexpected log body {other:?}"),
            }
            for a in &r.attributes {
                if let Some(any_value::Value::ArrayValue(v)) =
                    a.value.as_ref().and_then(|v| v.value.as_ref())
                {
                    arrays += 1;
                    nested += v
                        .values
                        .iter()
                        .filter(|e| matches!(e.value, Some(any_value::Value::KvlistValue(_))))
                        .count();
                }
            }
        }
    }
    assert!(structured > 0, "no structured log body");
    assert!(arrays > 0, "no array-valued attribute");
    assert!(
        logs.resource_logs
            .iter()
            .flat_map(|rl| &rl.scope_logs)
            .any(|sl| sl
                .scope
                .as_ref()
                .is_some_and(|s| s.name == "shop.assistant")),
        "no GenAI scope: gen_ai.input.messages is a README bullet"
    );
    assert!(nested > 0, "nothing nests two levels deep");

    // --- metrics: honest histograms, exemplars that resolve, enough points ----

    let m = demo_metrics(1, 0, T0, T0, SLOT, 0, TRACES);
    let ids: std::collections::BTreeSet<&[u8]> =
        spans.iter().map(|s| s.trace_id.as_ref()).collect();
    let mut exemplars = 0;
    let mut points = 0;
    for metric in m
        .resource_metrics
        .iter()
        .flat_map(|rm| &rm.scope_metrics)
        .flat_map(|sm| &sm.metrics)
    {
        if let Some(metric::Data::Histogram(h)) = &metric.data {
            points = points.max(h.data_points.len());
            for p in &h.data_points {
                assert_eq!(
                    p.bucket_counts.iter().sum::<u64>(),
                    p.count,
                    "histogram buckets do not sum to its count"
                );
                assert_eq!(p.bucket_counts.len(), p.explicit_bounds.len() + 1);
                for e in &p.exemplars {
                    assert!(
                        ids.contains(e.trace_id.as_ref()),
                        "exemplar names a trace nobody wrote"
                    );
                    exemplars += 1;
                }
            }
        }
    }
    assert!(exemplars > 0, "no exemplars on the histogram");
    assert!(points > 2, "{points} points is not a chart");

    // The benchmark path shares the histogram helper, so it is checked too.
    for metric in metrics_batch(0, 0, T0)
        .resource_metrics
        .iter()
        .flat_map(|rm| &rm.scope_metrics)
        .flat_map(|sm| &sm.metrics)
    {
        if let Some(metric::Data::Histogram(h)) = &metric.data {
            for p in &h.data_points {
                assert_eq!(p.bucket_counts.iter().sum::<u64>(), p.count);
            }
        }
    }

    // The out-of-process samplers stop when the writers do, not when `--for`
    // says. In `--demo` that flag is the width of the history, so a sampler that
    // only watched the deadline would keep shelling out to `ps` for forty-five
    // minutes after ten seconds of work — the run looks hung, and the peak it
    // finally reports is of an idle server.
    WRITING.store(false, Ordering::Relaxed);
    let t = Instant::now();
    peak_memory(std::process::id(), t + Duration::from_secs(3_600));
    assert!(
        t.elapsed() < Duration::from_secs(5),
        "peak_memory ran past the writers; --demo --for 45m --pid N would hang"
    );
    WRITING.store(true, Ordering::Relaxed);

    // And `--for` understands the units the demo is documented with.
    assert_eq!(duration("45m"), 2_700);
    assert_eq!(duration("2h"), 7_200);
    assert_eq!(duration("30s"), 30);
    assert_eq!(duration("30"), 30);

    println!(
        "loadgen --selftest ok: {} spans over {} services, {errors} failed, \
         {exceptions} exceptions, {links} links, {exemplars} exemplars",
        spans.len(),
        DEMO_SERVICES.len()
    );
}

/// A string attribute by key, or empty. Only the self-check needs this.
fn attr<'a>(attrs: &'a [KeyValue], key: &str) -> &'a str {
    attrs
        .iter()
        .find(|a| a.key == key)
        .and_then(|a| match a.value.as_ref()?.value.as_ref()? {
            any_value::Value::StringValue(s) => Some(s.as_str()),
            _ => None,
        })
        .unwrap_or("")
}
