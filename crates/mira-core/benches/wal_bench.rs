//! Does the write-ahead log actually meet the < 5 ms p99 acknowledgement SLA,
//! and what would the alternative have cost?
//!
//! Two numbers, because one of them only means something next to the other.
//! The first is [`Wal::append`], which is what an acknowledgement waits on. The
//! second is an append followed by `sync_data`, which is what the
//! acknowledgement would have waited on had Mira chosen durable acks — and on
//! an Apple target that is `F_FULLFSYNC`, so it is expected to blow the SLA by
//! roughly an order of magnitude. That contrast is the entire argument in
//! `wal.rs`'s module docs, and an argument nobody can re-run is folklore.
//!
//! ```sh
//! cargo bench -p miradb-core --bench wal_bench
//! ```
//!
//! Exits non-zero if the p99 of a plain append is at or above 5 ms, so this is
//! a check and not just a report. It deliberately does *not* fail on the
//! fsync numbers: those are allowed to be terrible, that is the point.
//!
//! **Run it on an idle machine.** This measures a syscall whose tail is set by
//! whoever else is touching the page cache, and it is not defended against
//! that — it cannot be. Measured during a parallel `cargo build` with a
//! real-time antivirus scanner at 212% CPU, the same 1 MiB run reported a p99
//! of 2.3 ms and then 13.2 ms with a 426 ms maximum. Neither number is about
//! Mira. If the p99 moves between two back-to-back runs, check `uptime` before
//! believing either.

use std::path::Path;
use std::time::{Duration, Instant};

use mira_core::wal::{Signal, Wal};

/// The SLA, from the phase 2 brief.
const SLA: Duration = Duration::from_millis(5);

/// Body sizes, and how many of each to send.
///
/// 1 MiB is the batched case: section 11 measured 992 k records/s at 129.7 MiB/s,
/// about 137 bytes a record, so the 8,192-record batch the load generator sends is
/// roughly 1.1 MiB of protobuf. 4 KiB is a single-span export from a quiet
/// service — the case whose p99 was 2,647 ms before this log existed, and so
/// the one the SLA is really about.
///
/// The counts are chosen so a nearest-rank p99 has at least ten samples above
/// it. A p99 over 64 samples is the maximum wearing a different name, which is
/// how the first draft of this bench managed to report a 10 ms p99 that was one
/// unlucky sample.
const SIZES: &[(usize, usize)] = &[(4 << 10, 20_000), (64 << 10, 8_000), (1 << 20, 1_000)];

/// The rate the paced runs offer, in bytes per second.
///
/// section 11 measured this engine ingesting 204.6 MiB/s on this machine at the
/// top of its curve, which is what the WAL would be asked to absorb; this is
/// 2.5x that, so the SLA is being checked with headroom rather than at the
/// edge. It was 4x when the engine measured 71 MiB/s, and the multiple has been
/// held at 2.5x since: the point of it is that the log is never the thing that
/// gives. Raise the constant the day the engine's rate closes on this one.
///
/// Pacing is the whole reason the number below is meaningful. Unpaced, the
/// loop offers about 1 GiB/s of dirty pages — several times what the engine in
/// front of it can produce — and the kernel starts blocking the writer to
/// flush. That is a real effect and it is measured separately as the
/// saturation run, but it is not the acknowledgement latency of a Mira under
/// any load Mira can generate, and asserting the SLA against it would be
/// asserting against a strawman.
const PACED_BYTES_PER_SEC: f64 = 512.0 * 1024.0 * 1024.0;

/// Override the paced rate, in MiB/s, to find where a given machine's cliff
/// is: `MIRA_WAL_BENCH_MIBS=568 cargo bench -p miradb-core --bench wal_bench`.
/// The default above is a fact about the machine section 11 was measured on, and the
/// only way to keep it a fact is for the next person to be able to re-derive
/// it without editing this file.
fn paced_rate() -> f64 {
    match std::env::var("MIRA_WAL_BENCH_MIBS") {
        Ok(v) => {
            v.parse::<f64>()
                .expect("MIRA_WAL_BENCH_MIBS must be a number")
                * 1024.0
                * 1024.0
        }
        Err(_) => PACED_BYTES_PER_SEC,
    }
}

fn main() {
    let dir = std::env::temp_dir().join(format!("mira-wal-bench-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create bench dir");

    println!(
        "wal append latency  (ack path: page cache, no fsync)  paced at {:.0} MiB/s",
        paced_rate() / (1024.0 * 1024.0)
    );
    header();
    let mut worst_p99 = Duration::ZERO;
    for &(size, n) in SIZES {
        let s = run(&dir, size, n, Some(paced_rate()), false);
        worst_p99 = worst_p99.max(s.p99);
        s.print(size);
    }

    println!();
    println!("wal append latency  (unpaced — where writeback throttling starts)");
    header();
    for &(size, n) in SIZES {
        run(&dir, size, n, None, false).print(size);
    }

    println!();
    println!("wal append + fsync  (NOT the ack path — the road not taken)");
    header();
    for &(size, _) in SIZES {
        run(&dir, size, 200, None, true).print(size);
    }

    let _ = std::fs::remove_dir_all(&dir);

    println!();
    if worst_p99 >= SLA {
        println!(
            "FAIL: worst p99 {:.3} ms is at or above the {:.0} ms SLA",
            ms(worst_p99),
            ms(SLA)
        );
        std::process::exit(1);
    }
    println!(
        "ok: worst p99 {:.3} ms, {:.0}x under the {:.0} ms SLA",
        ms(worst_p99),
        SLA.as_secs_f64() / worst_p99.as_secs_f64().max(f64::MIN_POSITIVE),
        ms(SLA)
    );
}

fn header() {
    println!(
        "  {:>9}  {:>7}  {:>9}  {:>9}  {:>9}  {:>9}  {:>10}",
        "body", "n", "p50", "p90", "p99", "max", "throughput"
    );
}

struct Stats {
    n: usize,
    p50: Duration,
    p90: Duration,
    p99: Duration,
    max: Duration,
    bytes_per_sec: f64,
}

impl Stats {
    fn print(&self, size: usize) {
        println!(
            "  {:>9}  {:>7}  {:>7.3}ms  {:>7.3}ms  {:>7.3}ms  {:>7.3}ms  {:>7.1} MiB/s",
            human(size),
            self.n,
            ms(self.p50),
            ms(self.p90),
            ms(self.p99),
            ms(self.max),
            self.bytes_per_sec / (1024.0 * 1024.0),
        );
    }
}

/// `rate` paces the offered load; `None` runs as fast as the loop can, which
/// for the fsync variant is the only sensible thing (fsync is its own pacer)
/// and for the plain variant is the saturation measurement.
fn run(root: &Path, size: usize, n: usize, rate: Option<f64>, fsync_each: bool) -> Stats {
    let dir = root.join(format!("{size}-{}-{fsync_each}", rate.is_some()));
    std::fs::create_dir_all(&dir).expect("create run dir");
    let wal = Wal::open(&dir, 0).expect("open wal");

    // Not zeros: a body of zeros is unrepresentative of anything the CRC or
    // the filesystem will see, and on a compressing filesystem it is free.
    let body: Vec<u8> = (0..size)
        .map(|i| (i.wrapping_mul(31) & 0xff) as u8)
        .collect();

    // Warm up: the first append pays for the page-cache pages behind the file
    // and, on the fsync variant, for the first metadata flush.
    for _ in 0..16 {
        wal.append(Signal::Traces, &body).expect("warmup append");
    }
    if fsync_each {
        wal.sync().expect("warmup sync");
    }

    let interval = rate.map(|r| Duration::from_secs_f64(size as f64 / r));
    let mut samples = Vec::with_capacity(n);
    let started = Instant::now();
    for i in 0..n {
        if let Some(interval) = interval {
            // Sleep to the slot, then time only the append. Timing from the
            // slot instead would fold `thread::sleep`'s overshoot — a
            // millisecond or more on a loaded macOS scheduler — into a number
            // that is supposed to be about `write(2)`. The cost of that choice
            // is that this cannot observe coordinated omission; it does not
            // need to, because nothing here queues behind a slow append.
            let deadline = started + interval * i as u32;
            if let Some(wait) = deadline.checked_duration_since(Instant::now()) {
                std::thread::sleep(wait);
            }
        }
        let t = Instant::now();
        wal.append(Signal::Traces, &body).expect("append");
        if fsync_each {
            wal.sync().expect("sync");
        }
        samples.push(t.elapsed());
    }
    let wall = started.elapsed();

    samples.sort_unstable();
    let _ = std::fs::remove_dir_all(&dir);

    Stats {
        n,
        p50: pct(&samples, 0.50),
        p90: pct(&samples, 0.90),
        p99: pct(&samples, 0.99),
        max: *samples.last().expect("at least one sample"),
        bytes_per_sec: (n * size) as f64 / wall.as_secs_f64(),
    }
}

/// Nearest-rank percentile over an already-sorted slice.
///
/// Nearest-rank, not interpolated: a p99 that is a real observed sample is a
/// latency that really happened, and an interpolated one is not.
fn pct(sorted: &[Duration], q: f64) -> Duration {
    debug_assert!(!sorted.is_empty());
    let rank = (q * sorted.len() as f64).ceil() as usize;
    sorted[rank.saturating_sub(1).min(sorted.len() - 1)]
}

fn ms(d: Duration) -> f64 {
    d.as_secs_f64() * 1000.0
}

fn human(bytes: usize) -> String {
    if bytes >= 1 << 20 {
        format!("{} MiB", bytes >> 20)
    } else {
        format!("{} KiB", bytes >> 10)
    }
}
