//! Where an export's latency actually goes, measured from inside the process.
//!
//! These exist because the 32-to-96-connection plateau could not be explained
//! from outside. A closed-loop load generator reports `connections × batch /
//! ack latency`, so every hypothesis predicts the same throughput curve, and
//! the server's CPU was flat at 2.23 of 12 cores at both ends of the sweep —
//! which rules out "it is working harder" and says nothing about what it is
//! doing instead. The answer needed a number from inside: the parts of
//! `submit` do not add up to `submit`, and the gap is the finding.
//!
//! They are also the reason the finding is checkable. Section 11's numbers are
//! reproducible on the machine that produced them, and a diagnosis resting on
//! instrumentation that was deleted before the commit is not.
//!
//! **Cost.** Two `Instant::now()` pairs and five relaxed atomics per export.
//! On aarch64-apple-darwin `Instant::now()` is `mach_absolute_time`, ~25 ns, so
//! the whole of it is under 150 ns against a critical section measured at 2.2
//! ms — five parts per million, and it is on the path whose *milliseconds* are
//! the subject. Nothing here allocates and nothing takes a lock; a counter that
//! contended would be measuring itself.
//!
//! **Reading them.** Nothing prints unless the dump task is running, which
//! `mira` only starts when its target is enabled:
//!
//! ```sh
//! RUST_LOG=mira=info,mira_core=info,mira::probe=debug mira --data-dir ./data
//! ```
//!
//! Counters are cumulative from start, so two dumps five seconds apart are a
//! rate and one dump is a mean over the run.

use std::sync::atomic::{AtomicU64, Ordering::Relaxed};

/// Count, total, maximum and two tail buckets for one interval.
///
/// Not a histogram: the question these answer is "which of these three numbers
/// is the other two", and a mean plus a max plus how many samples were absurd
/// settles that in four atomics. A real histogram would be the right shape for
/// an SLO and the wrong shape for an afternoon.
#[derive(Default)]
pub struct Probe {
    pub n: AtomicU64,
    pub ns: AtomicU64,
    pub max_ns: AtomicU64,
    pub over_10ms: AtomicU64,
    pub over_100ms: AtomicU64,
}

impl Probe {
    pub const fn new() -> Self {
        Self {
            n: AtomicU64::new(0),
            ns: AtomicU64::new(0),
            max_ns: AtomicU64::new(0),
            over_10ms: AtomicU64::new(0),
            over_100ms: AtomicU64::new(0),
        }
    }

    pub fn record(&self, ns: u64) {
        self.n.fetch_add(1, Relaxed);
        self.ns.fetch_add(ns, Relaxed);
        self.max_ns.fetch_max(ns, Relaxed);
        if ns > 10_000_000 {
            self.over_10ms.fetch_add(1, Relaxed);
        }
        if ns > 100_000_000 {
            self.over_100ms.fetch_add(1, Relaxed);
        }
    }

    fn line(&self, name: &str) -> String {
        let n = self.n.load(Relaxed);
        let ns = self.ns.load(Relaxed);
        format!(
            "{name:<18} n={n:<9} total={:>9.3}s mean={:>8.3}ms max={:>9.3}ms >10ms={:<8} >100ms={}",
            ns as f64 / 1e9,
            ns.checked_div(n).unwrap_or(0) as f64 / 1e6,
            self.max_ns.load(Relaxed) as f64 / 1e6,
            self.over_10ms.load(Relaxed),
            self.over_100ms.load(Relaxed),
        )
    }
}

/// Entering [`crate::wal::Wal::append_then`] to holding the log's mutex.
///
/// The headline. There is one mutex for all three signals and all shards, and
/// this is what every appender pays for the one inside it.
pub static WAL_LOCK_WAIT: Probe = Probe::new();
/// Holding it: the CRC over the body, the three `write_all`s, the enqueue.
pub static WAL_HELD: Probe = Probe::new();
/// Of that, just the three `write_all` syscalls.
pub static WAL_WRITE: Probe = Probe::new();
/// `encode_to_vec` — the re-encode an export pays to be framed.
pub static WAL_ENCODE: Probe = Probe::new();
/// Admission: `reserve()`, plus the `ADMIT_WAIT` park if every shard was full.
pub static SUBMIT_ADMIT: Probe = Probe::new();
/// The whole of `submit`, so the parts can be checked against the sum. They do
/// not add up, which is the entire reason the rest of this module exists.
pub static SUBMIT_TOTAL: Probe = Probe::new();
/// Waiting for the publish, on the no-log ack path.
pub static SUBMIT_WAIT_ACK: Probe = Probe::new();

/// How late a task that asked to sleep 50 ms actually woke.
///
/// The one number here that owes nothing to the client. Ack latency cannot
/// separate "the server is working hard" from "the server cannot schedule
/// anything"; this can, because it measures a task that does no work at all.
/// A 50 ms sleep returning 110 ms late means every runtime worker was blocked
/// in a syscall, and no property of the connection count or the load generator
/// explains it.
pub static RUNTIME_LAG: Probe = Probe::new();

/// Callers currently inside `append_then`, and the high-water mark.
///
/// The mark is the proof, not the gauge. `std::sync::Mutex` on
/// aarch64-apple-darwin is the pthread backend, so a contended `lock()` parks
/// the OS thread in the kernel — and a parked tokio worker runs no other task
/// and is not replaced. A mark equal to the worker count means every worker
/// was in here at once and the runtime could make no progress at all.
pub static WAL_INFLIGHT: AtomicU64 = AtomicU64::new(0);
pub static WAL_INFLIGHT_MAX: AtomicU64 = AtomicU64::new(0);

/// Records elapsed time into `p` when dropped, so an early `return` inside the
/// measured function still lands a sample.
pub struct Scope<'a> {
    p: &'a Probe,
    t: std::time::Instant,
}

impl<'a> Scope<'a> {
    pub fn new(p: &'a Probe) -> Self {
        Self {
            p,
            t: std::time::Instant::now(),
        }
    }
}

impl Drop for Scope<'_> {
    fn drop(&mut self) {
        self.p.record(self.t.elapsed().as_nanos() as u64);
    }
}

pub struct WalScope;

impl Drop for WalScope {
    fn drop(&mut self) {
        WAL_INFLIGHT.fetch_sub(1, Relaxed);
    }
}

pub fn wal_scope() -> WalScope {
    let now = WAL_INFLIGHT.fetch_add(1, Relaxed) + 1;
    WAL_INFLIGHT_MAX.fetch_max(now, Relaxed);
    WalScope
}

pub fn dump() -> String {
    let mut s = String::from("\n");
    for (name, p) in [
        ("submit.total", &SUBMIT_TOTAL),
        ("submit.admit", &SUBMIT_ADMIT),
        ("submit.wait_ack", &SUBMIT_WAIT_ACK),
        ("wal.encode", &WAL_ENCODE),
        ("wal.lock_wait", &WAL_LOCK_WAIT),
        ("wal.held", &WAL_HELD),
        ("wal.write", &WAL_WRITE),
        ("runtime.lag", &RUNTIME_LAG),
    ] {
        s.push_str(&p.line(name));
        s.push('\n');
    }
    s.push_str(&format!(
        "wal.inflight_max   {}\n",
        WAL_INFLIGHT_MAX.load(Relaxed)
    ));
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A probe with no samples must not divide by zero, and the tail buckets
    /// have to count the right side of their thresholds. Both are the kind of
    /// thing that is only wrong in the dump nobody reads until 3am.
    #[test]
    fn a_probe_summarises_what_it_was_given() {
        let p = Probe::new();
        assert!(p.line("empty").contains("mean=   0.000ms"));
        p.record(1_000_000);
        p.record(50_000_000);
        p.record(500_000_000);
        let line = p.line("x");
        assert!(line.contains("n=3"), "{line}");
        // 551 ms over three samples.
        assert!(line.contains("mean= 183.667ms"), "{line}");
        assert!(line.contains("max=  500.000ms"), "{line}");
        // Strictly over: 1 ms is in neither bucket, 50 ms is in one, 500 in both.
        assert!(line.contains(">10ms=2"), "{line}");
        assert!(line.contains(">100ms=1"), "{line}");
    }

    /// The dump is the whole point of the module, and the failure it can have
    /// is silent: a probe added above and not listed below simply never
    /// appears, and the run it was added for is measured without it. So the
    /// test names every probe the diagnosis in `docs/architecture.md` section
    /// 11 is read off, and a nine-line dump is what that section quotes.
    #[test]
    fn the_dump_names_every_probe_the_diagnosis_reads() {
        let _scope = wal_scope();
        let out = dump();
        for name in [
            "submit.total",
            "submit.admit",
            "submit.wait_ack",
            "wal.encode",
            "wal.lock_wait",
            "wal.held",
            "wal.write",
            "runtime.lag",
            "wal.inflight_max",
        ] {
            assert!(out.contains(name), "{name} is missing from:{out}");
        }
        assert_eq!(out.trim_start().lines().count(), 9, "{out}");
        // `wal_scope` is live, so the high-water mark has seen at least one.
        assert!(WAL_INFLIGHT_MAX.load(Relaxed) >= 1);
        assert!(WAL_INFLIGHT.load(Relaxed) >= 1);
    }
}
