//! The ingest path.
//!
//! One bounded `tokio::sync::mpsc` channel per shard feeding one flusher task.
//!
//! The brief called for a lock-free ring buffer. That is the right structure
//! when items are ~150ns order structs arriving millions per second; here an
//! item is a whole export request costing 10^5–10^6 ns to decode and encode, and
//! the realistic arrival rate is 10^2–10^4 per second. At that ratio the queue
//! is never the bottleneck, and a bounded async channel buys the thing a
//! lock-free queue cannot: `send().await` applies real backpressure that
//! propagates out as HTTP/2 flow control to the exporter, instead of either
//! spinning or dropping. If a queue ever shows up in a profile, this is one type
//! to change.
//!
//! Flush is `spawn_blocking`: it fsyncs.
//!
//! Where the acknowledgement happens is [`Config::wal`]'s decision, and it is
//! the only one in this file. Without a log the export is acknowledged after
//! the block directory rename is durable, because OTLP's retryable status set
//! covers exports in flight at a crash — acking earlier is the one window where
//! data is lost with the client believing it was stored. That costs a whole
//! `max_block_age` at the tail, which is section 11's 2.4 s p99. With a log the frame
//! *is* the durable record, the publish is a background reorganisation of data
//! that is already safe, and the ack costs a `write(2)`. Everything else here —
//! the queue, the carry, the failure contract — is identical either way.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use mira_core::SignalBuilder;
use mira_core::block;
use mira_core::signal::Open;
use mira_core::wal::{self, Wal};
use tokio::sync::{mpsc, oneshot};
use tokio::time::{Instant, sleep_until};

/// Engine configuration.
///
/// Note what is *not* reachable from `config.rs`: `target_block_bytes` and
/// `max_block_age` are derived constants, not settings. They are the two numbers
/// an operator would most expect to tune and the two the engine is best placed
/// to own, so principle 2c applies and there is no YAML key for either.
pub struct Config {
    pub data_dir: PathBuf,
    /// Writer identity, from `mira_core::block::node_id`. Makes block names
    /// unique across replicas with no coordination.
    pub node: u32,
    /// Seal a block once it reaches roughly this many bytes.
    pub target_block_bytes: usize,
    /// Seal a block after this long regardless of size, so acknowledgement
    /// latency is bounded by time and not by the caller's traffic.
    pub max_block_age: Duration,
    pub retention: Duration,
    /// How many exports may wait for one signal's flusher. See
    /// `crate::config::Config::queue`, which is where the reasoning is.
    ///
    /// Split across [`shards`](Self::shards), so the number an operator sets is
    /// still the number of decoded exports this signal can be holding.
    pub queue: usize,
    /// How many flusher tasks one signal runs, each with its own channel, open
    /// block and block sequence.
    ///
    /// One per core, not one per resource hash — section 4's "A note on
    /// sharding" is explicit about which of those is the right unit, and the
    /// reason is that resource cardinality in real fleets is bimodal, so a hash
    /// gives a permanently hot shard and a small-file explosion in the tail.
    /// Files per flush interval should be a function of core count, known at
    /// startup, not of the customer's topology.
    ///
    /// One, not `available_parallelism`, by default: `main` resolves the real
    /// number from `ingest.shards` and the core count, and everything that
    /// builds a `Config` by hand — every unit test, every e2e node — wants the
    /// deterministic one block per seal that a single shard gives.
    pub shards: usize,
    /// The write-ahead log, shared by all three signals, or `None` to
    /// acknowledge on the block publish as Mira always has.
    ///
    /// One `Option` rather than a separate durability setting, because the two
    /// are the same decision: with a log, an export is recoverable the moment
    /// it is framed and there is nothing left for the acknowledgement to wait
    /// for; without one, the publish is the only thing that makes it
    /// recoverable. A flag that let those disagree would only be able to
    /// express wrong answers.
    ///
    /// Off by default. Turning it on trades read-your-writes — see
    /// `mira_core::wal`'s module docs — and that repair has not landed.
    pub wal: Option<Arc<Wal>>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            data_dir: PathBuf::from("./data"),
            node: block::node_id("mira"),
            target_block_bytes: 32 << 20,
            max_block_age: Duration::from_secs(2),
            retention: Duration::from_secs(7 * 24 * 3600),
            queue: 128,
            shards: 1,
            wal: None,
        }
    }
}

// `pub(crate)` only so `receiver`'s tests can put a queue into the two states
// `submit` refuses from — full and closed — without a flusher behind it.
pub(crate) struct Job<R> {
    req: R,
    ack: oneshot::Sender<Result<(), Rejected>>,
    /// The log sequence this export was framed at, if there is a log. The
    /// flusher takes the maximum over a block and publishes one past it as
    /// `wal_hi`.
    wal_seq: Option<u64>,
}

/// The write handle for one signal. `R` is that signal's OTLP export request.
pub struct Ingest<R> {
    // `pub(crate)` so a test can build one around a queue it controls; see
    // [`Job`]. Nothing outside this module constructs one in anger — `spawn`
    // is the only supported way to get a handle.
    //
    // One sender per flusher shard, and never empty. `Arc<[_]>` rather than
    // `Vec` because every receiver holds a clone of this handle and the shard
    // count is fixed at startup.
    pub(crate) tx: Arc<[mpsc::Sender<Job<R>>]>,
    /// Where the next export that cannot go to shard 0 starts looking. Shared
    /// across clones, because the point of it is to spread waiters over shards
    /// rather than over handles.
    pub(crate) turn: Arc<AtomicU64>,
    pub(crate) rejects: &'static Rejects,
    pub(crate) wal: Option<Arc<Wal>>,
    pub(crate) signal: wal::Signal,
}

// Derived `Clone` would demand `R: Clone`, which no export request is. Only the
// `Sender`s are cloned, and that is unconditional.
impl<R> Clone for Ingest<R> {
    fn clone(&self) -> Self {
        Self {
            tx: self.tx.clone(),
            turn: self.turn.clone(),
            rejects: self.rejects,
            wal: self.wal.clone(),
            signal: self.signal,
        }
    }
}

/// Why an export could not be admitted. None of these is a partial success:
/// OTLP forbids the client from retrying a partial success, so reporting
/// overload that way permanently destroys the data and blames the sender.
///
/// The split between the last two is the whole of the failure contract. OTLP's
/// retryable set is closed — gRPC `UNAVAILABLE` and friends, HTTP 429/502/503/504
/// — and an exporter handed anything outside it drops the batch on the floor. So
/// "we could not write it, try again" and "this export can never be written"
/// cannot share a variant, however similar they look from inside the flusher.
pub enum Rejected {
    /// Queue full. Transient; retry.
    Busy,
    /// The engine is shutting down.
    Closed,
    /// The block this export was in did not become durable — a full disk, an
    /// EIO, a flush task that panicked. Nothing about the export caused it and
    /// the next one may well land, so it is answered like [`Rejected::Busy`].
    Unavailable(String),
    /// This export can never be stored: it does not fit an empty block. A retry
    /// produces the same answer, so the client must be told not to send one.
    Failed(String),
}

/// Wall-clock seconds. Only ever used for rate limits and for ages an operator
/// reads; block timestamps come from the data, never from this clock.
fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

/// True at most once per wall-clock second per gate, for whichever caller gets
/// there first.
///
/// A node in trouble is in trouble thousands of times a second and the log line
/// is worth exactly one of them; the rest is in the counter beside it. `swap`,
/// not load-then-store, so of the many threads arriving in the same second
/// exactly one sees the old value.
fn once_a_second(gate: &AtomicU64) -> bool {
    let now = now_secs();
    gate.swap(now, Relaxed) != now
}

/// How long a signal has to be unable to store anything before this node calls
/// itself unready.
///
/// It has to outlast the automatic recovery, or readiness flaps through every
/// incident it is supposed to report. The two recoveries are a retention sweep,
/// which runs every 60s and is what frees a full volume ([`reclaim`]), and the
/// next flush, which is at most `max_block_age` behind it. Two sweeps gives that
/// path two chances before the endpoint is pulled — and at a 2s block age it is
/// already ~60 consecutive failed publishes, which is nobody's transient.
pub const UNREADY_AFTER: Duration = Duration::from_secs(120);

/// How long an export waits for room in the queue before it is shed.
///
/// Under the OTLP exporter timeout, which is 10s in every SDK that follows the
/// spec's default, so a waiter is answered by this node rather than abandoned
/// by its client — an abandoned request is the one case where the work is paid
/// twice and nobody is told. Over `max_block_age`, which is 2s, so a queue that
/// is full only because a flush is in flight drains within the wait instead of
/// shedding around it. Five seconds sits in the middle of that range.
///
/// This is the tail bound, not a target: at the operating point nothing waits
/// at all. It matters when a sender is faster than the disk, and there the
/// choice is between a slow ack and a 503 that costs the sender a retry and
/// this node the decode it already did.
const ADMIT_WAIT: Duration = Duration::from_secs(5);

/// What each signal has refused, published and been stuck on, since start.
///
/// A handful of counters and one warn a second, not a metrics subsystem. An
/// exporter being NACKed already logs Mira's own reason on its side; what only
/// the server can say is the *rate* and how long it has been going on, which is
/// what an operator reads out of `/health`, `/readyz` and `/api/v1/stats` (see
/// `main`) when deciding whether to grow the disk or the node.
///
/// Static because those three endpoints need all three signals at once and
/// nothing else ever reads them: threading a handle per signal through two
/// routers to reach one probe would be more plumbing than the numbers are worth.
pub struct Rejects {
    /// The signal these count for, so the endpoints can name them.
    pub signal: &'static str,
    /// Exports refused before the queue, because it was full.
    pub shed: AtomicU64,
    /// Exports accepted and then NACKed, because the write did not land.
    pub failed: AtomicU64,
    /// Exports refused permanently, whose records are gone: the client is told
    /// not to retry, so this is the only counter that measures lost data.
    pub refused: AtomicU64,
    /// Blocks and rows that reached the disk, and the bytes they took there.
    pub published: AtomicU64,
    pub rows: AtomicU64,
    pub bytes: AtomicU64,
    /// Unix second the oldest currently open block of this signal took its
    /// first row, or 0 if nothing is open. An age that keeps growing past
    /// `max_block_age` is a flusher that is not flushing.
    ///
    /// Derived from [`clocks`](Self::clocks) and not written directly by a
    /// flusher: with more than one shard per signal, a shard that has just
    /// sealed would otherwise clear a sibling's clock and the stuck flusher
    /// this number exists to expose would read as healthy. The oldest of them,
    /// because the question it answers is "is anything stuck".
    pub open_since: AtomicU64,
    /// Unix second of the first publish failure in the current run of them, or 0
    /// if the last publish worked. See [`UNREADY_AFTER`]. Derived like
    /// `open_since`, and for the same reason.
    pub stalled_since: AtomicU64,
    /// One pair per flusher shard, which is what the flushers actually write.
    ///
    /// A fixed array rather than one sized at `spawn`: this lives in a `static`
    /// that outlives every flusher and is re-entered by the next test in the
    /// process, so a `OnceLock` sized by whoever spawned first would be the
    /// wrong length for whoever spawns second. [`MAX_SHARDS`] pairs is 256
    /// bytes a signal.
    clocks: [ShardClock; MAX_SHARDS],
    /// Rate-limit gates, one per line that can fire per export.
    warned: AtomicU64,
    refuse_warned: AtomicU64,
}

/// What one flusher shard reports about itself. See [`Rejects::open_since`].
struct ShardClock {
    open_since: AtomicU64,
    stalled_since: AtomicU64,
}

impl ShardClock {
    const fn new() -> Self {
        Self {
            open_since: AtomicU64::new(0),
            stalled_since: AtomicU64::new(0),
        }
    }
}

impl Rejects {
    const fn new(signal: &'static str) -> Self {
        Self {
            signal,
            shed: AtomicU64::new(0),
            failed: AtomicU64::new(0),
            refused: AtomicU64::new(0),
            published: AtomicU64::new(0),
            rows: AtomicU64::new(0),
            bytes: AtomicU64::new(0),
            open_since: AtomicU64::new(0),
            stalled_since: AtomicU64::new(0),
            clocks: [const { ShardClock::new() }; MAX_SHARDS],
            warned: AtomicU64::new(0),
            refuse_warned: AtomicU64::new(0),
        }
    }

    /// The oldest non-zero timestamp any shard is reporting, or 0 if none is.
    ///
    /// Recomputed on every write rather than on every read because the readers
    /// are `/healthz`, `/metrics` and the TUI — three calls a second between
    /// them against a hot loop — and [`MAX_SHARDS`] relaxed loads is cheaper
    /// than the branch that would decide when to skip it.
    fn oldest(&self, pick: fn(&ShardClock) -> &AtomicU64) -> u64 {
        self.clocks
            .iter()
            .map(|c| pick(c).load(Relaxed))
            .filter(|&t| t != 0)
            .min()
            .unwrap_or(0)
    }

    /// Report when `shard`'s open block took its first row, or 0 for "nothing
    /// open".
    fn set_open_since(&self, shard: usize, at: u64) {
        self.clocks[shard].open_since.store(at, Relaxed);
        self.open_since
            .store(self.oldest(|c| &c.open_since), Relaxed);
    }

    /// Start the clock on a run of failures in `shard`, or leave it where it is.
    ///
    /// Not a `store`: readiness is about how *long* this has been going on, so
    /// the timestamp that matters is the first failure of the run, not the
    /// latest. One flusher owns each shard slot, so the compare-exchange cannot
    /// lose a race — it is here to keep the first value.
    fn mark_stalled(&self, shard: usize) {
        let _ = self.clocks[shard].stalled_since.compare_exchange(
            0,
            now_secs().max(1),
            Relaxed,
            Relaxed,
        );
        self.stalled_since
            .store(self.oldest(|c| &c.stalled_since), Relaxed);
    }

    /// Zero every open-block clock, shard slots included.
    ///
    /// Only the in-process end-to-end harness calls this — see
    /// `e2e::forget_open_blocks` for why it has to. Clearing the aggregate
    /// alone would not do it: the next shard to open a block recomputes the
    /// aggregate from the slots, and a dead flusher's slot would come back.
    #[cfg(test)]
    pub fn forget_open(&self) {
        for c in &self.clocks {
            c.open_since.store(0, Relaxed);
        }
        self.open_since.store(0, Relaxed);
    }

    /// `shard` stored a block, so its run of failures is over. The signal is
    /// only unstalled once every shard's is.
    fn clear_stalled(&self, shard: usize) {
        self.clocks[shard].stalled_since.store(0, Relaxed);
        self.stalled_since
            .store(self.oldest(|c| &c.stalled_since), Relaxed);
    }

    fn record_shed(&self) {
        self.shed.fetch_add(1, Relaxed);
        if once_a_second(&self.warned) {
            tracing::warn!(
                signal = self.signal,
                "ingest queue full; shedding exports (senders are told to retry)"
            );
        }
    }
}

/// Parallel to [`SIGNALS`].
pub static REJECTS: [Rejects; SIGNALS.len()] = [
    Rejects::new(SIGNALS[0]),
    Rejects::new(SIGNALS[1]),
    Rejects::new(SIGNALS[2]),
];

fn rejects_for(signal: &str) -> &'static Rejects {
    REJECTS
        .iter()
        .find(|r| r.signal == signal)
        .expect("every signal that has a builder has a counter slot")
}

/// Seconds `r` has been unable to store an export, if that is long enough to be
/// worth acting on. Split out from [`stalled`] so the threshold is testable
/// without writing to a process-wide static that three live flushers also own.
fn stall_of(r: &Rejects, now: u64) -> Option<u64> {
    match r.stalled_since.load(Relaxed) {
        0 => None,
        since => {
            let secs = now.saturating_sub(since);
            (secs >= UNREADY_AFTER.as_secs()).then_some(secs)
        }
    }
}

/// The first signal this node has been unable to store for longer than
/// [`UNREADY_AFTER`], and for how many seconds. `None` means every signal is
/// either healthy or has only just started failing.
pub fn stalled() -> Option<(&'static str, u64)> {
    let now = now_secs();
    REJECTS
        .iter()
        .find_map(|r| stall_of(r, now).map(|secs| (r.signal, secs)))
}

impl<R> Ingest<R> {
    /// A slot in the first shard that has one, or `None` if every shard is full
    /// or gone.
    ///
    /// First fit from shard 0 rather than round-robin, and that is the whole
    /// sharding policy. Round-robin spreads a trickle of exports over every
    /// shard, and since each shard owns its own open block, a node doing two
    /// exports a second would publish `shards` nearly-empty blocks every
    /// `max_block_age` instead of one — the small-file explosion section 4
    /// rejects hash sharding for, arrived at from the other direction. First
    /// fit keeps a node that is not saturating one flusher behaving exactly as
    /// it did with one, and starts using the second shard at the moment the
    /// first one's queue stops draining, which is the moment the consumer's
    /// service time became the curve.
    ///
    /// Ordering across shards is not preserved and does not need to be: two
    /// exports are two OTLP requests, the spec orders neither against the
    /// other, and block timestamps come from the data. Ordering *within* a
    /// shard still is, which is what the carry rule in [`flusher`] needs.
    fn reserve(&self) -> Option<mpsc::Permit<'_, Job<R>>> {
        self.tx.iter().find_map(|tx| tx.try_reserve().ok())
    }
}

impl<R: prost::Message> Ingest<R> {
    /// Enqueue and wait for durability.
    ///
    /// What "durable" means here is the one thing [`Config::wal`] decides.
    /// Without a log this returns once the block containing the request has
    /// been fsynced and renamed into place, which is correct and costs a whole
    /// `max_block_age` at the tail. With one it returns once the request is a
    /// frame in the log's page cache, which is section 11's 2.4 s p99 turned into
    /// microseconds and is why the log exists.
    pub async fn submit(&self, req: R) -> Result<(), Rejected> {
        let (ack, wait) = oneshot::channel();
        // Wait for room, and only shed once the wait has run out. The first
        // revision shed the moment the queue was full, on the reasoning that a
        // fast NACK beats an unbounded latency tail. The tail argument is right
        // and [`ADMIT_WAIT`] bounds it; the "fast" was not. Tonic and axum both
        // decode the request before the handler is called, so by the time this
        // runs the expensive part of the export is already paid, and shedding
        // throws it away for a client that will send the same bytes again.
        // Measured at 96 connections that cost more than the queue ever saved:
        // 93% of exports shed, four cores busy, and a third of the throughput
        // two connections get on one core. Parking instead is bounded by the
        // connection count — every waiter is a request already in memory — where
        // a deeper queue is bounded by nothing.
        //
        // Before the log append, not after: an export shed here never happened,
        // whereas one framed and then shed would be replayed into a node whose
        // client has already retried it elsewhere.
        let permit = match self.reserve() {
            Some(p) => p,
            // Every shard is full. Park on one of them rather than on all of
            // them: `reserve` is not cancel-safe enough to race K of them and
            // drop the losers' permits, and at this point the choice of shard
            // does not matter — they are all behind their flusher. The turn
            // counter spreads the parked waiters so they wake as each drains
            // rather than all behind the same one.
            None => {
                let i = self.turn.fetch_add(1, Relaxed) as usize % self.tx.len();
                match tokio::time::timeout(ADMIT_WAIT, self.tx[i].reserve()).await {
                    Ok(Ok(p)) => p,
                    Ok(Err(_)) => return Err(Rejected::Closed),
                    Err(_) => {
                        self.rejects.record_shed();
                        return Err(Rejected::Busy);
                    }
                }
            }
        };
        if let Some(wal) = &self.wal {
            // Re-encoded, not the bytes off the wire: tonic decodes before the
            // handler sees the request, and a KYAML body was never protobuf at
            // all. Measured at 864 MiB/s against the 244 MiB/s decode already in
            // the path — see `mira_core::wal`'s module docs for why owning a
            // tonic `Codec` to avoid it is the worse trade.
            let body = req.encode_to_vec();
            // The enqueue rides inside the append so the queue cannot reorder
            // what the log numbered — see `Wal::append_then`.
            return match wal.append_then(self.signal, &body, move |seq| {
                permit.send(Job {
                    req,
                    ack,
                    wal_seq: Some(seq),
                });
            }) {
                Ok(_) => Ok(()),
                Err(e) => {
                    self.rejects.failed.fetch_add(1, Relaxed);
                    // Only one log error is the sender's to fix, and retrying an
                    // export too large to frame just burns the link.
                    Err(match e {
                        mira_core::Error::WalFrameTooLarge { .. } => {
                            Rejected::Failed(e.to_string())
                        }
                        _ => Rejected::Unavailable(e.to_string()),
                    })
                }
            };
        }
        permit.send(Job {
            req,
            ack,
            wal_seq: None,
        });
        match wait.await {
            Ok(Ok(())) => Ok(()),
            // One counter for both refusals after acceptance: the difference
            // between them is the status code, and the number an operator wants
            // is "how much did not get stored".
            Ok(Err(r)) => {
                self.rejects.failed.fetch_add(1, Relaxed);
                Err(r)
            }
            Err(_) => Err(Rejected::Closed),
        }
    }
}

impl<R: prost::Message + Default> Ingest<R> {
    /// Push one frame recovered from the log back into this signal's flusher,
    /// under the sequence it already has.
    ///
    /// Not [`submit`](Self::submit): the frame is in the log already, so
    /// re-appending it would number it above every watermark and the block
    /// storing it would claim the copy instead of the original — which replays
    /// again on the next boot, and the one after that. Nothing waits for the
    /// ack either; the client that sent this got its answer before the crash,
    /// or gave up long ago.
    ///
    /// Blocking, and deliberately: this is called from a `spawn_blocking` hop
    /// at boot, and the bounded channel is the only thing keeping a multi-
    /// gigabyte log from being decoded into memory faster than it can be
    /// sealed.
    /// The two failures are worth telling apart: [`Rejected::Failed`] is one
    /// frame that will never decode, which is a line in the log and the next
    /// frame; [`Rejected::Closed`] is the flusher being gone, which means the
    /// rest of the replay would go nowhere.
    pub fn replay(&self, body: &[u8], seq: u64) -> Result<(), Rejected> {
        // Back among the unpublished before it is decoded, let alone queued, for
        // the same reason `submit` numbers and enqueues under one lock: a shard
        // that sealed in between would compute a watermark that steps over this
        // frame.
        if let Some(w) = &self.wal {
            w.reframed(self.signal, seq);
        }
        let req = match R::decode(body) {
            Ok(r) => r,
            Err(e) => {
                // Retired on the spot, and that is a decision rather than a
                // leak. This frame will not decode on this boot and will not
                // decode on any other, so leaving it unpublished would hold the
                // signal's watermark at its sequence for the life of the volume
                // — and every frame published after it would be replayed again
                // on every boot, duplicating stored data to keep re-reading one
                // that never becomes readable. `main::replay` says out loud that
                // those exports are gone; this is the line that makes it true.
                if let Some(w) = &self.wal {
                    w.published(self.signal, &[seq]);
                }
                return Err(Rejected::Failed(e.to_string()));
            }
        };
        let job = Job {
            req,
            ack: oneshot::channel().0,
            wal_seq: Some(seq),
        };
        // First fit like `submit`, falling back to blocking on shard 0. Replay
        // is the one caller that must not shed, so the bounded channel is the
        // pacing: see the note above about decoding a multi-gigabyte log.
        match self.reserve() {
            Some(p) => {
                p.send(job);
                Ok(())
            }
            None => self.tx[0].blocking_send(job).map_err(|_| Rejected::Closed),
        }
    }
}

/// Every signal that has an on-disk directory. Retention sweeps all of them;
/// [`block::scan`] treats a missing one as empty, so listing a signal before its
/// encoder exists is harmless.
pub const SIGNALS: [&str; 3] = ["logs", "traces", "metrics"];

/// The most flusher shards one signal will run.
///
/// A ceiling, not a target: it sizes the per-shard health clocks in a `static`
/// and it bounds how many blocks a signal can publish per flush interval. Above
/// this the flushers are no longer the bottleneck — the decode in front of them
/// is — and every extra shard is another open block's worth of resident memory
/// against the footprint axis. Sixteen is also `mira_core::query`'s scan fanout,
/// and a machine wide enough to want more of one wants more of both.
pub const MAX_SHARDS: usize = 16;

/// How many flusher shards to run, given the configured value and what the
/// machine reports.
///
/// Zero means "ask the machine", which is the default and the only value
/// `config.rs` documents as auto. The count is a function of core count and
/// nothing else — not of the resource cardinality, not of the connection count
/// — which is section 4's rule for what a shard may be keyed on.
///
/// Halved, because a shard is a *consumer*: the producers are the decode
/// and the runtime's own work, and giving every core a flusher leaves nothing
/// to feed them. Section 11 measured 1.56M records/s on 2.18 cores with one
/// flusher per signal, so the transcode is around a third of the total and
/// three consumers per two producers would be an idle two-thirds.
pub fn shard_count(configured: usize, cores: usize) -> usize {
    match configured {
        0 => (cores / 2).clamp(1, MAX_SHARDS),
        n => n.min(MAX_SHARDS),
    }
}

/// The handle over one signal's flusher shards: what a single `JoinHandle`
/// meant when there was one of them, kept true now that there are several.
///
/// Awaiting it resolves when every shard has stopped; [`abort`](Self::abort)
/// stops them all where they stand. A [`JoinSet`](tokio::task::JoinSet) and not
/// a task that awaits a `Vec` of handles, because that shape gets the second
/// half wrong: aborting such a task drops only its own future, so the shards
/// keep running, and a shard that outlives the node it belonged to watches its
/// senders drop, reads that as a graceful close, and seals a block — under a
/// sequence, and into a staging directory, that the successor node is already
/// using.
pub struct Flushers(tokio::task::JoinSet<()>);

/// Dropping the handle detaches, as dropping a `JoinHandle` does — a `JoinSet`
/// on its own would abort instead. Read-only test harnesses build a node and
/// keep only the parts they query; turning that into "and stop ingesting" would
/// be a trap, and every caller that means to stop has one of the two ways to
/// say so above.
impl Drop for Flushers {
    fn drop(&mut self) {
        self.0.detach_all();
    }
}

impl Flushers {
    /// Stop every shard where it stands. Nothing is sealed; what survives is
    /// whatever the log already holds — which is the claim every test that
    /// calls this is making.
    ///
    /// Test-only, and that is not an oversight: the binary sets
    /// `panic = "abort"`, so in production a flusher that stops without being
    /// asked takes the process with it and there is nobody left to abort.
    #[cfg(test)]
    pub fn abort(&mut self) {
        self.0.abort_all();
    }

    /// Three shards that will never stop, for the drain path that has to give
    /// up on them.
    #[cfg(test)]
    pub fn wedged() -> Self {
        let mut set = tokio::task::JoinSet::new();
        set.spawn(std::future::pending());
        Self(set)
    }
}

impl std::future::Future for Flushers {
    type Output = Result<(), tokio::task::JoinError>;

    /// Ready once the set has drained. Unlike a `JoinHandle` this is safe to
    /// poll again afterwards — an emptied set answers `Ready` forever — which
    /// is what lets `main` await a handle `first_stopped` may already have run
    /// to completion.
    fn poll(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Self::Output> {
        use std::task::Poll;
        loop {
            match self.0.poll_join_next(cx) {
                Poll::Ready(Some(Ok(()))) => {}
                // Surfaced rather than swallowed: `cargo test` does not build
                // with `panic = "abort"`, so a flusher that panics under test
                // is a `JoinError` here and nothing else anywhere.
                Poll::Ready(Some(Err(e))) => return Poll::Ready(Err(e)),
                Poll::Ready(None) => return Poll::Ready(Ok(())),
                Poll::Pending => return Poll::Pending,
            }
        }
    }
}

/// Start one signal's ingest pipeline. Returns the handle its receivers push
/// into. Each signal gets its own channels, flusher tasks and block sequences,
/// so a slow flush on one cannot stall another.
///
/// [`Flushers`] is the shutdown contract: drop every [`Ingest`] clone and every
/// shard seals whatever is open, acks everyone waiting on it and returns; the
/// handle resolves once they all have. A caller that exits without awaiting it
/// turns a graceful stop into a reset for those waiters.
pub fn spawn<B: SignalBuilder>(cfg: &Arc<Config>) -> (Ingest<B::Request>, OpenSlot, Flushers) {
    let shards = cfg.shards.clamp(1, MAX_SHARDS);
    // Once for the signal, before any shard can publish. Inside the flusher it
    // would be one sweep per shard, and `sweep_staging` filters by signal and
    // node — not by sequence — so shard 3 booting a moment late would delete the
    // staging directory shard 0 was already writing tables into.
    //
    // Not fatal if it fails: a leaked directory under `.tmp` costs disk and
    // nothing else, and refusing to ingest over it would turn a janitorial
    // problem into an outage.
    match block::sweep_staging(&cfg.data_dir, B::SIGNAL, cfg.node) {
        Ok(0) => {}
        Ok(n) => tracing::info!(signal = B::SIGNAL, count = n, "swept stale staging dirs"),
        Err(e) => tracing::warn!(signal = B::SIGNAL, error = %e, "cannot sweep staging dirs"),
    }
    // Resume the sequence past whatever is already on disk so block directory
    // names stay unique across restarts. This is the entirety of crash recovery.
    //
    // `max`, not `last`: `scan` sorts by `(min_ts, seq)`, so the last element is
    // the latest-timestamped block, which is not the highest sequence number
    // whenever a restart follows a backlog replay. Reusing a sequence makes the
    // next `rename` land on an existing directory and the node never publishes
    // again.
    //
    // Scanned here and handed to every shard rather than scanned by each of
    // them, and that is load-bearing: a shard that read the directory after a
    // sibling had already published would resume one higher and its stride
    // would land on the sibling's next sequence. One base, distinct offsets.
    let resume = match block::scan(&cfg.data_dir, B::SIGNAL) {
        Ok(blocks) => Some(blocks.iter().map(|b| b.seq).max().map_or(0, |s| s + 1)),
        Err(e) => {
            tracing::error!(signal = B::SIGNAL, error = %e, "cannot scan data directory");
            None
        }
    };
    let rejects = rejects_for(B::SIGNAL);
    // The configured depth is the signal's, not each shard's: it is a bound on
    // how many decoded exports this node can be holding, and that does not get
    // larger because there are more consumers.
    let depth = cfg.queue.div_ceil(shards).max(1);
    let mut txs = Vec::with_capacity(shards);
    let mut slots = Vec::with_capacity(shards);
    let mut tasks = tokio::task::JoinSet::new();
    for shard in 0..shards {
        let (tx, rx) = mpsc::channel(depth);
        // Eight concurrent askers, because a ninth gets a snapshot at most a
        // millisecond older and waiting in line for one is worth less than that.
        let (ask, asks) = mpsc::channel(8);
        let slot = Shard {
            cur: Arc::default(),
            ask,
        };
        txs.push(tx);
        // No flusher if the data directory could not be read, which drops the
        // receiver and leaves the sender closed: `submit` answers `Closed` and
        // the handle resolves at once. The same thing the flusher's own early
        // return did, decided once instead of `shards` times.
        if let Some(resume) = resume {
            tasks.spawn(flusher::<B>(
                rx,
                asks,
                cfg.clone(),
                slot.clone(),
                shard,
                shards,
                resume,
            ));
        }
        slots.push(slot);
    }
    let ingest = Ingest {
        tx: txs.into(),
        turn: Arc::default(),
        rejects,
        wal: cfg.wal.clone(),
        signal: wal::Signal::named(B::SIGNAL).expect("every signal has a log discriminant"),
    };
    // One handle over all of them, so `main` still holds three. Awaiting every
    // shard rather than the first to finish is the same contract it was: under
    // `panic = "abort"` a flusher that dies takes the process with it, and the
    // one way a shard returns early — an unreadable data directory at startup —
    // is a condition every shard of the signal meets at once.
    (
        ingest,
        OpenSlot {
            shards: slots.into(),
        },
        Flushers(tasks),
    )
}

/// Where the read path asks one flusher shard for a readable copy of its open
/// block (section 4), and where the last copy it produced is cached.
///
/// The snapshot is taken on demand, never on a timer: an idle node with nobody
/// querying it copies nothing. A `Mutex` around the cached `Arc` rather than an
/// `ArcSwap` — the critical section is one pointer clone and a crate for that
/// would be a crate for nothing.
#[derive(Clone)]
struct Shard {
    cur: Arc<Mutex<Option<Arc<Open>>>>,
    /// Handing the flusher somewhere to put an answer. Not generic in the
    /// signal's request type, which is the whole reason the read path can hold
    /// three of these in one array.
    ask: mpsc::Sender<oneshot::Sender<Option<Arc<Open>>>>,
}

/// Every shard of one signal's open blocks, asked together.
///
/// The interesting half is [`OpenSlot::fresh`], and what makes it *fresh*
/// rather than merely recent is the order the queues already enforce. `submit`
/// acknowledges an export only after the job is in some shard's channel, so
/// every acknowledged export is queued before a request issued after it — and
/// if each shard answers only once its own channel is empty, the answers
/// together necessarily contain them all. That is read-your-writes, for the
/// price of FIFOs Mira was already paying, with no shared counter and no clock.
///
/// Sharding does not weaken it, because the argument never depended on there
/// being one queue: an acknowledged export is in exactly one shard's channel
/// until that shard appends it. It does mean the read path has to ask all of
/// them, which is what `fresh` does.
#[derive(Clone, Default)]
pub struct OpenSlot {
    /// Empty for a slot nobody serves: `fresh` returns nothing, forever. That
    /// is exactly what a unit test that only wants an `Api` wants.
    shards: Arc<[Shard]>,
}

impl OpenSlot {
    /// Everything acknowledged before this call, as one readable block per
    /// shard that has anything open.
    ///
    /// Falls back to a shard's last snapshot when its flusher cannot be
    /// reached: the request queue is full, or the task is gone. Both are
    /// overload or shutdown, and a query that waits its turn behind an
    /// overloaded ingest path is a worse answer than one that is a few
    /// milliseconds stale.
    ///
    /// Every ask goes out before any answer is awaited, so the shards work
    /// concurrently; awaiting them in turn would put a whole flusher's backlog
    /// between one shard's answer and the next one's question.
    pub async fn fresh(&self) -> Vec<Arc<Open>> {
        let mut out = Vec::with_capacity(self.shards.len());
        let mut waiting = Vec::with_capacity(self.shards.len());
        for s in self.shards.iter() {
            let (tx, rx) = oneshot::channel();
            match s.ask.try_send(tx) {
                Ok(()) => waiting.push((s, rx)),
                Err(_) => out.extend(s.get()),
            }
        }
        for (s, rx) in waiting {
            out.extend(rx.await.unwrap_or_else(|_| s.get()));
        }
        out
    }
}

impl Shard {
    /// The last snapshot taken, without asking for a new one.
    fn get(&self) -> Option<Arc<Open>> {
        self.lock().clone()
    }

    fn put(&self, v: Option<Arc<Open>>) {
        *self.lock() = v;
    }

    /// A panic in this critical section is not possible — it clones or drops an
    /// `Arc` and nothing else — so poisoning carries no information and
    /// unwrapping it would only turn an impossible bug into an outage.
    fn lock(&self) -> std::sync::MutexGuard<'_, Option<Arc<Open>>> {
        self.cur.lock().unwrap_or_else(|e| e.into_inner())
    }
}

/// The three signals' open blocks, in [`SIGNALS`] order.
pub type OpenSlots = [OpenSlot; SIGNALS.len()];

/// One sweep for all signals, not one per signal: retention is IO against the
/// directory tree, and three tasks waking on the same minute boundary to unlink
/// from the same volume is contention for nothing.
pub fn spawn_retention(cfg: Arc<Config>) {
    tokio::spawn(retention(cfg.clone()));
    if cfg.wal.is_some() {
        tokio::spawn(wal_maintenance(cfg));
    }
}

/// How long an acknowledged export can sit in the page cache before it is on
/// the device.
///
/// This is the entire power-loss exposure window, and it is a constant for the
/// same reason `max_block_age` is: the operator cannot price the trade without
/// knowing what an fsync costs on their volume, and the engine measures that
/// every time it does one. On this machine `F_FULLFSYNC` is ~4 ms (section 10), so a
/// quarter-second period spends under 2% of one thread and bounds the loss at
/// a quarter second of ingest. Shorter buys very little — the exposure is
/// already smaller than a Collector's own batch timeout, so the exporter is
/// holding more unsent data than this window holds unsynced.
pub const WAL_SYNC_PERIOD: Duration = Duration::from_millis(250);

/// Sync the log to the device, and drop the segments every signal has published
/// past.
///
/// Truncation is not on the sync period. It costs three `readdir`s of the block
/// tree — [`block::wal_watermarks`] is the whole manifest, re-derived — and what
/// it can reclaim is whole segments, which only become removable once every
/// signal has published past them. At the rate a demo or a quiet service
/// produces, that is minutes apart and four sweeps a second would be hundreds of
/// scans finding nothing; at section 11's measured 190.6 MiB/s a 64 MiB segment
/// fills in a third of a second, and a minute of them is ~180 files that one `readdir`
/// retires as cheaply as it retires one. The period is set by how much disk a
/// minute of unreclaimed log is worth, which is the same answer at both ends.
async fn wal_maintenance(cfg: Arc<Config>) {
    let Some(wal) = cfg.wal.clone() else { return };
    let mut tick = tokio::time::interval(WAL_SYNC_PERIOD);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut ticks: u64 = 0;
    loop {
        tick.tick().await;
        ticks += 1;
        // `u64::is_multiple_of` reads better but is stable since 1.87, and the
        // workspace MSRV is 1.85.
        wal_sweep(wal.clone(), cfg.data_dir.clone(), ticks % 240 == 0).await;
    }
}

/// One pass of the above. Separate from the loop so a test can drive both kinds
/// of tick without waiting out the sixty seconds of real time between them.
async fn wal_sweep(wal: Arc<Wal>, dir: PathBuf, truncating: bool) {
    // The whole sweep is one blocking hop: `sync` is `F_FULLFSYNC` and truncate
    // is `readdir` plus `unlink`, and neither belongs on a runtime thread that
    // has three flushers' worth of acks to hand out.
    let done = tokio::task::spawn_blocking(move || {
        wal.sync()?;
        if !truncating {
            return Ok(0);
        }
        // The minimum across signals, not each signal's own: one segment holds
        // frames for all three, so it can only go once the last of them has
        // claimed everything in it.
        let covered = block::wal_watermarks(&dir)?.into_iter().min().unwrap_or(0);
        wal.truncate(covered)
    })
    .await;
    match done {
        Ok(Ok(0)) => {}
        Ok(Ok(n)) => tracing::info!(segments = n, "write-ahead log segments removed"),
        // Warn and keep going. A log that cannot sync is still absorbing appends
        // and still replayable after anything short of power loss, so refusing
        // to ingest over it would trade a narrowed durability guarantee for a
        // certain outage.
        Ok(Err(e)) => tracing::warn!(error = %e, "write-ahead log maintenance failed"),
        Err(e) => tracing::warn!(error = %e, "write-ahead log maintenance panicked"),
    }
}

async fn flusher<B: SignalBuilder>(
    mut rx: mpsc::Receiver<Job<B::Request>>,
    mut asks: mpsc::Receiver<oneshot::Sender<Option<Arc<Open>>>>,
    cfg: Arc<Config>,
    open_slot: Shard,
    shard: usize,
    shards: usize,
    resume: u64,
) {
    let rejects = rejects_for(B::SIGNAL);
    // Shard k takes `resume + k`, `resume + k + shards`, and so on, so the
    // shards of a signal partition the sequence space with no allocator and no
    // agreement — a sequence only has to be unique, and the residues mod
    // `shards` are distinct. It stays unique across a restart that runs a
    // different number of shards, because the next `resume` is above every
    // stride. That is the whole cost of a per-shard block sequence: the block
    // directory is the manifest and a sequence is a filename, so nothing else
    // has to know.
    let mut seq = resume.saturating_add(shard as u64);
    let signal = wal::Signal::named(B::SIGNAL).expect("every signal has a log discriminant");

    let mut builder = B::default();
    let mut waiters: Vec<oneshot::Sender<Result<(), Rejected>>> = Vec::new();
    let mut batch = Vec::with_capacity(64);
    // Jobs that did not fit the open block. They go into the next one, so a full
    // dictionary costs a slightly small block and never costs a caller its data.
    let mut carry: Vec<Job<B::Request>> = Vec::new();
    let mut deadline = Instant::now() + cfg.max_block_age;
    let mut open = true;
    // The log sequences this shard has finished with since the last publish,
    // ascending, or empty when there is no log. "Finished with" and not
    // "stored": an empty export and a permanently refused one both leave
    // nothing to recover, so replaying them forever would only keep the log
    // from truncating. A carried job is deliberately absent — it has not landed
    // anywhere yet, and claiming it here is how the watermark would lie.
    //
    // A list rather than the running maximum it used to be, because with more
    // than one shard per signal the maximum is no longer the watermark: see
    // `Wal::watermark_for`, which needs to know exactly which sequences this
    // block is retiring in order to answer what the *others* still hold.
    let mut wal_seqs: Vec<u64> = Vec::new();
    // Readers waiting to be told what is in the block, and the builder size the
    // last answer was taken at. Answered only with an empty `rx` — see
    // [`OpenSlot::fresh`] — so they survive as many loop turns as the backlog
    // takes.
    let mut asked: Vec<oneshot::Sender<Option<Arc<Open>>>> = Vec::new();
    let mut snapped: Option<usize> = None;

    // Carry outlives the channel: a request deferred by the last block still has
    // to land somewhere before the task exits.
    while open || !carry.is_empty() {
        // Before the wait, not after the work: a turn that seals parks again
        // immediately, and a reader answered only on the next arrival would
        // wait for someone else's export.
        answer::<B>(
            &builder,
            &mut asked,
            &mut snapped,
            !carry.is_empty() || !rx.is_empty(),
            &open_slot,
            cfg.node,
            seq,
        );
        let mut aged = false;
        // Skip the wait while there is carry: those jobs are already accepted and
        // unacknowledged, so holding them behind an idle receiver would add a
        // whole block age to their latency.
        if carry.is_empty() {
            tokio::select! {
                n = rx.recv_many(&mut batch, 64) => {
                    if n == 0 {
                        open = false;
                    }
                }
                // A reader wanting the open block. Closing this channel is not a
                // shutdown signal — the `Ingest` handles are — so a `None` here
                // only means nobody will ever ask again.
                who = asks.recv() => {
                    if let Some(who) = who {
                        asked.push(who);
                    }
                }
                _ = sleep_until(deadline) => aged = true,
            }
        }
        // Drained in the same turn as the jobs, so a reader that arrives with a
        // backlog behind it is answered once, after the backlog.
        while let Ok(who) = asks.try_recv() {
            asked.push(who);
        }

        let mut jobs = std::mem::take(&mut carry);
        jobs.append(&mut batch);
        let mut dict_full = false;
        for job in jobs {
            // On an empty block the headroom hint is deliberately not consulted.
            //
            // The hint assumes every attribute in the request introduces a new
            // dictionary key, because counting the distinct ones would mean
            // hashing the whole request on the hot path to answer a question
            // that is almost always "yes, plenty of room". That estimate is the
            // right one when it decides *whether to seal first* — being wrong
            // costs a slightly small block. It is the wrong one when the block
            // is already empty, because then it is not choosing between two
            // blocks, it is rejecting the export outright: a single batch of
            // ~13k records at five attributes each exceeds 65536 attribute rows
            // and used to be NACKed permanently, retry included, for data whose
            // real key cardinality is a few dozen.
            //
            // Sealing cannot help a block with nothing in it, so the only honest
            // test left is the append itself.
            let empty = builder.is_empty();
            // Once one job has been deferred, every job after it must be too, or
            // the block would acknowledge exports out of arrival order.
            if !empty && (dict_full || !builder.has_headroom_for(&job.req)) {
                dict_full = true;
                carry.push(job);
                continue;
            }
            // The first job of a block starts its age clock, so acknowledgement
            // latency is bounded from the moment data arrived rather than from
            // the last flush.
            if waiters.is_empty() {
                deadline = Instant::now() + cfg.max_block_age;
            }
            if let Some(seq) = job.wal_seq {
                wal_seqs.push(seq);
            }
            match builder.append_request(&job.req) {
                // An export carrying no records is legal — the Collector emits
                // one whenever a batch empties out — and there is nothing in it
                // to make durable. Parking its caller behind a block that will
                // never be sealed, because nothing was added to seal, strands
                // that caller for as long as it is willing to wait.
                Ok(0) => {
                    let _ = job.ack.send(Ok(()));
                }
                Ok(_) => {
                    // The reported age tracks unacknowledged rows, not the
                    // deadline: an export carrying no records resets the timer
                    // above without leaving anything open, and an "open block"
                    // that is never sealed because there is nothing in it is
                    // the exact false alarm this number would be read as.
                    if waiters.is_empty() {
                        rejects.set_open_since(shard, now_secs());
                    }
                    waiters.push(job.ack);
                }
                Err(e) => {
                    // The append can fail part-way through, having already
                    // written some of the request's rows. The client will retry
                    // the whole export, so publishing those rows would
                    // guarantee duplicates. Discarding the builder is only safe
                    // — and only necessary — when this job started on an empty
                    // block, which is exactly the case that skipped the hint
                    // above; anything else passed a conservative check and
                    // cannot overflow.
                    if empty {
                        let _ = builder.finish();
                    }
                    // The only permanent refusal in the pipeline: this request
                    // did not fit a block with nothing in it, so no retry of it
                    // ever will.
                    //
                    // Loud, because it is the one refusal that destroys data.
                    // Everything else in this file is answered `Unavailable`
                    // and comes back on the next attempt; this one tells the
                    // exporter not to try, and the exporter obeys. Rate-limited
                    // like the shed warning — a sender in this state is in it
                    // for every export it has — and carrying the running total,
                    // because one line an incident later is not a quantity.
                    let refused = rejects.refused.fetch_add(1, Relaxed) + 1;
                    if once_a_second(&rejects.refuse_warned) {
                        tracing::error!(
                            signal = B::SIGNAL,
                            error = %e,
                            refused,
                            "export permanently refused; its records are gone. The sender \
                             is told not to retry, so nothing will bring them back — the \
                             request does not fit an empty block, which means splitting it \
                             at the sender is the only fix"
                        );
                    }
                    let _ = job.ack.send(Err(Rejected::Failed(e.to_string())));
                }
            }
        }

        let full = dict_full || builder.approx_bytes() >= cfg.target_block_bytes;
        if builder.is_empty() || !(full || (aged && !waiters.is_empty()) || !open) {
            // Push the idle timer out so a stale deadline does not spin the loop.
            if waiters.is_empty() {
                deadline = Instant::now() + cfg.max_block_age;
                rejects.set_open_since(shard, 0);
            }
            continue;
        }

        rejects.set_open_since(shard, 0);
        let sealed = match builder.finish() {
            Ok(s) => s,
            Err(e) => {
                let msg = e.to_string();
                rejects.mark_stalled(shard);
                for w in waiters.drain(..) {
                    // Whose export broke the encoder is not knowable from here,
                    // so nobody is blamed permanently: everyone is told to send
                    // it again.
                    let _ = w.send(Err(Rejected::Unavailable(msg.clone())));
                }
                // `finish` leaves a fresh builder behind even when it fails, so
                // there is nothing to repair here — see `SignalBuilder::finish`.
                //
                // The watermark goes with it. Nothing claimed these sequences,
                // so they stay unpublished, keep every sibling shard's
                // watermark behind them, and come back on the next boot — which
                // is the only reason the callers above could be told to retry
                // without that being a lie about where their data went.
                wal_seqs.clear();
                // Those rows are gone; a snapshot still advertising them would
                // be the read path promising data no restart can produce.
                open_slot.put(None);
                tracing::error!(signal = B::SIGNAL, error = %msg, "block discarded");
                continue;
            }
        };

        let dir = cfg.data_dir.clone();
        let node = cfg.node;
        let this_seq = seq;
        seq += shards as u64;
        let block_seqs = std::mem::take(&mut wal_seqs);
        // Asked before the publish, because the answer is part of the directory
        // name, and answered against what every shard of this signal is still
        // holding rather than against this block alone. The sequences are not
        // retired until the rename lands.
        let block_wal_hi = match &cfg.wal {
            Some(w) => w.watermark_for(signal, &block_seqs),
            None => 0,
        };
        let rows = sealed.num_rows;
        let result = tokio::task::spawn_blocking(move || {
            // The size is measured in the same blocking hop as the write, off
            // the runtime: it is a handful of `stat`s against pages the publish
            // just touched, and it is the only exact answer to "how much disk
            // did this node write" that does not mean walking the whole tree.
            block::publish(&dir, B::SIGNAL, node, this_seq, block_wal_hi, &sealed)
                .map(|b| (dir_bytes(&b.dir), b.dir))
        })
        .await;

        // Held across the publish rather than dropped at `finish`, so the rows
        // stay visible while the rename is in flight; the read path drops the
        // snapshot itself the instant a block with the same `(node, seq)`
        // appears on disk, so the overlap shows nothing twice.
        open_slot.put(None);
        snapped = None;

        let outcome = match result {
            Ok(Ok((bytes, path))) => {
                if let Some(w) = &cfg.wal {
                    w.published(signal, &block_seqs);
                }
                rejects.published.fetch_add(1, Relaxed);
                rejects.rows.fetch_add(rows as u64, Relaxed);
                rejects.bytes.fetch_add(bytes, Relaxed);
                rejects.clear_stalled(shard);
                tracing::info!(signal = B::SIGNAL, rows, bytes, seq = this_seq, path = %path.display(), "block published");
                Ok(())
            }
            // Logged here and not only counted: a disk that filled up at 02:00
            // is the one fact that explains every NACK the senders are about to
            // report, and it is invisible from their side.
            Ok(Err(e)) => {
                rejects.mark_stalled(shard);
                tracing::error!(signal = B::SIGNAL, seq = this_seq, error = %e, "block not published");
                Err(e.to_string())
            }
            Err(e) => {
                rejects.mark_stalled(shard);
                Err(format!("flush task panicked: {e}"))
            }
        };
        for w in waiters.drain(..) {
            // Every failure here is the block's, not any one caller's, so they
            // all get a retryable answer.
            let _ = w.send(outcome.clone().map_err(Rejected::Unavailable));
        }
        deadline = Instant::now() + cfg.max_block_age;
    }
}

/// Answer every reader waiting on the open block, if there is nothing left
/// queued ahead of them (section 4).
///
/// `pending` is the correctness condition, not an optimisation: a reader is
/// promised everything acknowledged before it asked, and an acknowledged export
/// is in the channel or in `carry` until the flusher appends it. Answering with
/// either non-empty would be answering early. Nothing is lost by waiting —
/// non-empty means the loop is about to turn again anyway.
///
/// The snapshot itself is best-effort. One that fails to build is a query that
/// misses the newest rows for a moment; failing the flush over it would turn a
/// read-path nicety into an ingest outage, and the same error is about to be
/// reported properly by the real seal.
fn answer<B: SignalBuilder>(
    builder: &B,
    asked: &mut Vec<oneshot::Sender<Option<Arc<Open>>>>,
    snapped: &mut Option<usize>,
    pending: bool,
    slot: &Shard,
    node: u32,
    seq: u64,
) {
    if asked.is_empty() || pending {
        return;
    }
    // Re-copying a block nothing has been appended to since the last answer
    // would be pure memcpy, and a live tail asks several times a second for
    // exactly that. `approx_bytes` and not a row count because it is the number
    // the builder already keeps; appends only ever grow it.
    let bytes = builder.approx_bytes();
    if builder.is_empty() {
        slot.put(None);
        *snapped = None;
    } else if *snapped != Some(bytes) {
        *snapped = Some(bytes);
        match builder.snapshot() {
            Ok(sealed) => slot.put(Some(Arc::new(Open { node, seq, sealed }))),
            Err(e) => {
                slot.put(None);
                tracing::debug!(signal = B::SIGNAL, error = %e, "open block not snapshotted");
            }
        }
    }
    let cur = slot.get();
    for who in asked.drain(..) {
        let _ = who.send(cur.clone());
    }
}

/// The size of one block, as the filesystem sees it. Best-effort: a block being
/// unlinked by another replica mid-walk is worth a slightly low counter, not an
/// error path on the flush.
fn dir_bytes(dir: &Path) -> u64 {
    std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| e.metadata().ok())
        .map(|m| m.len())
        .sum()
}

/// Free space below which retention stops waiting for the TTL.
///
/// This is not a setting and there is deliberately no key for it. Retention as a
/// TTL alone assumes the ingest rate the window was sized for; the first spike,
/// chatty service or debug level left on fills the volume before the clock
/// expires anything, every `publish` then fails ENOSPC, every export is NACKed,
/// and nothing in the process ever undoes it — the only thing that deletes
/// blocks is a clock that has not advanced far enough. The number the engine
/// needs is not "how full may I get", it is read off the volume every sweep; the
/// only constant here is the margin, and 10% is enough headroom for the blocks
/// in flight (three signals' `target_block_bytes` plus their staging copies) on
/// any volume big enough to hold a day of telemetry, while still leaving the
/// sweep room to act before `publish` starts failing.
const MIN_FREE: f64 = 0.10;

/// Drop the oldest blocks, across every signal, until the volume is back above
/// `min_free`. Returns what was unlinked, oldest first.
///
/// `min_free` is a parameter only so a test can say "pretend the volume is
/// full" without one; the sweep passes [`MIN_FREE`] and nothing else ever will.
///
/// ponytail: one `statfs` per unlink and one full `scan` per sweep that trips.
/// Both are O(blocks) on a path that only runs when the volume is nearly full,
/// where the unlink dominates anyway. If a volume ever spends long enough down
/// here for that to matter, the fix is to stop after freeing a target fraction
/// in one pass rather than re-measuring per block.
fn reclaim(dir: &Path, min_free: f64) -> mira_core::error::Result<Vec<PathBuf>> {
    let mut dropped = Vec::new();
    let mut free = block::free_fraction(dir)?;
    if free >= min_free {
        return Ok(dropped);
    }
    // Oldest first across all three signals at once, not one signal at a time:
    // the volume is shared, so the block worth losing is the oldest one on it. A
    // per-signal sweep would drop an hour-old trace block while a week-old log
    // block sat beside it.
    let mut blocks = Vec::new();
    for s in SIGNALS {
        blocks.extend(block::scan(dir, s)?);
    }
    blocks.sort_by_key(|b| (b.max_ts, b.seq));
    for b in blocks {
        if free >= min_free {
            break;
        }
        // The unlink `block::expire` does, read the same way: another replica
        // sharing this volume getting there first is not a conflict, and a
        // block this process cannot remove says nothing about the next one.
        match std::fs::remove_dir_all(&b.dir) {
            Ok(()) => {
                // WARN and one line per block. Deleting a user's telemetry
                // before they asked is only defensible if it is impossible to
                // miss afterwards, and "which blocks" is the question the
                // person who finds the gap will ask.
                tracing::warn!(
                    block = %b.dir.display(),
                    free = format!("{free:.3}"),
                    "volume is nearly full; dropped a block that had not reached its retention"
                );
                dropped.push(b.dir);
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => tracing::warn!(
                block = %b.dir.display(),
                error = %e,
                "cannot drop block to reclaim space; skipping it",
            ),
        }
        // Re-read rather than subtracting the block's size: compaction, another
        // replica and everything else on this volume are all moving it too.
        free = block::free_fraction(dir)?;
    }
    if !dropped.is_empty() {
        // One line for the whole burst above it, carrying the thing the operator
        // has to change: the per-block warnings say what went, this says why it
        // will keep going.
        tracing::warn!(
            blocks = dropped.len(),
            free = format!("{free:.3}"),
            "dropped blocks ahead of their retention to keep the volume writable; \
             retention is longer than this disk can hold at the current ingest rate"
        );
    }
    Ok(dropped)
}

async fn retention(cfg: Arc<Config>) {
    let mut tick = tokio::time::interval(Duration::from_secs(60));
    loop {
        tick.tick().await;
        let dir = cfg.data_dir.clone();
        let ttl = cfg.retention;
        let node = cfg.node;
        let swept = tokio::task::spawn_blocking(move || {
            // Wall clock is only used to place the horizons; block timestamps
            // themselves come from the data, never from this clock.
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos() as i64;
            // Saturating, and not `now - ttl.as_nanos() as i64`: a retention
            // longer than ~292 years does not fit an `i64` of nanoseconds, and
            // `as` wraps it negative — which puts the cutoff in the *future*
            // and expires the whole volume on the first sweep. `retention:
            // 999999d` is how an operator says "keep it forever", and it used
            // to mean the exact opposite.
            let cutoff = now.saturating_sub(i64::try_from(ttl.as_nanos()).unwrap_or(i64::MAX));
            // One signal failing must not skip the others; a full disk is
            // exactly when the remaining sweeps matter most.
            let results = SIGNALS.map(|s| {
                let dropped = block::expire(&dir, s, cutoff);
                // Expire first: compressing a block this sweep is about to
                // delete is pure wasted bandwidth.
                let cold = block::compact(&dir, s, node, now - block::COLD_AFTER_NS);
                (s, dropped, cold)
            });
            // Last, and only then: the TTL is the policy, and free space is the
            // floor under it. A sweep that expired enough by the clock has
            // nothing to do here and pays one `statfs` to find that out.
            (results, reclaim(&dir, MIN_FREE))
        })
        .await;
        match swept {
            Ok((results, reclaimed)) => {
                // `reclaim` has already logged every block it dropped and why.
                // Only the case where it could not even ask the volume is left,
                // and it is a warning rather than a stop: a sweep that cannot
                // read free space still expired by TTL above.
                if let Err(e) = reclaimed {
                    tracing::warn!(error = %e, "cannot read free space; retention is TTL-only this sweep");
                }
                for (signal, dropped, cold) in results {
                    match dropped {
                        Ok(0) => {}
                        Ok(n) => tracing::info!(signal, blocks = n, "retention dropped blocks"),
                        Err(e) => tracing::warn!(signal, error = %e, "retention failed"),
                    }
                    match cold {
                        Ok(0) => {}
                        Ok(n) => tracing::info!(signal, blocks = n, "compacted blocks to zstd"),
                        Err(e) => tracing::warn!(signal, error = %e, "compaction failed"),
                    }
                }
            }
            Err(e) => tracing::warn!(error = %e, "retention task panicked"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mira_core::logs::LogsBuilder;
    use mira_proto::collector::logs::v1::ExportLogsServiceRequest;
    use mira_proto::common::v1::{AnyValue, KeyValue, any_value};
    use mira_proto::logs::v1::{LogRecord, ResourceLogs, ScopeLogs};

    fn cfg(name: &str) -> (Arc<Config>, PathBuf) {
        let dir = std::env::temp_dir().join(format!("mira-pipe-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        (
            Arc::new(Config {
                data_dir: dir.clone(),
                // Short enough that a test can wait out the age timer, long
                // enough that two submits still land in one `recv_many`.
                max_block_age: Duration::from_millis(50),
                ..Default::default()
            }),
            dir,
        )
    }

    fn blocks(dir: &std::path::Path) -> usize {
        block::scan(dir, "logs").map_or(0, |b| b.len())
    }

    /// Every published logs block's sequence, ascending.
    fn seqs(dir: &std::path::Path) -> Vec<u64> {
        let mut v: Vec<u64> = block::scan(dir, "logs")
            .unwrap()
            .iter()
            .map(|b| b.seq)
            .collect();
        v.sort_unstable();
        v
    }

    /// One record carrying `n` attributes with distinct keys. The key dictionary
    /// is the thing with a ceiling, and distinct keys are the only way to reach
    /// it — a million records sharing one key never do.
    fn wide(n: usize) -> ExportLogsServiceRequest {
        ExportLogsServiceRequest {
            resource_logs: vec![ResourceLogs {
                scope_logs: vec![ScopeLogs {
                    log_records: vec![LogRecord {
                        time_unix_nano: 1_000,
                        attributes: (0..n)
                            .map(|i| KeyValue {
                                key: format!("k{i}"),
                                value: Some(AnyValue {
                                    value: Some(any_value::Value::StringValue("v".into())),
                                }),
                            })
                            .collect(),
                        ..Default::default()
                    }],
                    ..Default::default()
                }],
                ..Default::default()
            }],
        }
    }

    /// A request that does not fit the open block is deferred into the next one,
    /// never refused and never reordered.
    ///
    /// Both submits are acknowledged, so neither caller loses its data, and both
    /// blocks land — which is the difference between "the dictionary is full" and
    /// "your export is rejected". The second is what a client sees as a permanent
    /// failure for data whose real cardinality was fine.
    #[tokio::test]
    async fn a_request_that_does_not_fit_lands_in_the_next_block() {
        let (c, dir) = cfg("carry");
        let (tx, _open, h) = spawn::<LogsBuilder>(&c);
        // 40k distinct keys each: the first fits an empty block, the second
        // cannot join it, and 80k would overflow the u16 dictionary.
        let (a, b) = tokio::join!(tx.submit(wide(40_000)), tx.submit(wide(40_000)));
        assert!(a.is_ok() && b.is_ok(), "both callers must be acknowledged");
        drop(tx);
        h.await.unwrap();
        assert_eq!(blocks(&dir), 2, "the deferred request got its own block");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The case the headroom hint deliberately cannot answer: one request that
    /// is too wide for *any* block. Sealing first cannot help, so the append is
    /// attempted and its failure is the caller's answer — after which the
    /// builder has to be usable again, or the node rejects everything until it
    /// is restarted.
    #[tokio::test]
    async fn an_impossible_request_fails_only_itself() {
        let (c, dir) = cfg("toowide");
        let (tx, _open, h) = spawn::<LogsBuilder>(&c);
        let before = tx.rejects.failed.load(Relaxed);
        let destroyed = tx.rejects.refused.load(Relaxed);
        // `Failed`, not `Unavailable`: this one is permanent, and the receiver
        // turns the two into statuses an exporter treats differently.
        let refusal = tx.submit(wide(70_000)).await;
        assert!(
            matches!(&refusal, Err(Rejected::Failed(e))
                if e.contains("65535") || e.contains("dictionary")),
            "70k distinct keys cannot fit a u16 dictionary, and the refusal has \
             to be the permanent one that names why"
        );
        // `>`, not `== before + 1`: `REJECTS` is process-wide and the other
        // tests in this file refuse logs exports of their own, in parallel.
        assert!(tx.rejects.failed.load(Relaxed) > before);
        // Counted apart from `failed`, because this is the only refusal in the
        // pipeline that destroys data: the sender is told not to retry and it
        // will not. A number nobody can read is a deletion nobody can audit.
        assert!(tx.rejects.refused.load(Relaxed) > destroyed);
        // The next export proves the builder was replaced, not poisoned.
        tx.submit(crate::e2e::logs_export("checkout", 2_000, 4))
            .await
            .unwrap_or_else(|_| panic!("the pipeline is still open for business"));
        drop(tx);
        h.await.unwrap();
        assert_eq!(blocks(&dir), 1, "only the good export was published");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// An export with no records is legal and the Collector sends them. There is
    /// nothing in it to make durable, so parking its caller behind a block that
    /// will never be sealed strands them for as long as they are willing to wait.
    #[tokio::test]
    async fn an_empty_export_is_acknowledged_without_a_block() {
        let (c, dir) = cfg("empty");
        let (tx, _open, h) = spawn::<LogsBuilder>(&c);
        // No timeout needed: if this ever blocks, it blocks forever, and the
        // test harness reports the hang for what it is.
        tx.submit(ExportLogsServiceRequest::default())
            .await
            .unwrap_or_else(|_| panic!("an empty export is not an error"));
        drop(tx);
        h.await.unwrap();
        assert_eq!(blocks(&dir), 0, "nothing to seal, so nothing was sealed");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The whole point of the log, in one assertion: with one configured, the
    /// acknowledgement lands while the block is still open.
    ///
    /// `max_block_age` is a full second here and the submit is not allowed to
    /// take a tenth of it. Without the log that submit is the block age by
    /// definition — it is section 11's 2,647 ms p99 — so a regression that quietly
    /// puts the ack back behind the publish fails this by a factor of ten
    /// rather than by a margin that could be scheduler noise.
    #[tokio::test]
    async fn a_logged_export_is_acknowledged_before_its_block_is_sealed() {
        let dir = std::env::temp_dir().join(format!("mira-pipe-wal-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let node = block::node_id("waltest");
        let wal = Arc::new(Wal::open(&dir, node).unwrap());
        let c = Arc::new(Config {
            data_dir: dir.clone(),
            node,
            max_block_age: Duration::from_secs(1),
            wal: Some(Arc::clone(&wal)),
            ..Default::default()
        });
        let (tx, _open, h) = spawn::<LogsBuilder>(&c);

        let started = std::time::Instant::now();
        tx.submit(crate::e2e::logs_export("checkout", 2_000, 4))
            .await
            .unwrap_or_else(|_| panic!("the log accepted it"));
        let acked = started.elapsed();
        assert!(
            acked < Duration::from_millis(100),
            "acknowledged in {acked:?}, which is the block age, not the log"
        );
        assert_eq!(blocks(&dir), 0, "the ack did not wait for a block");
        assert_eq!(wal.next_seq(), 1, "the export is a frame");

        drop(tx);
        h.await.unwrap();

        // The block claims the frame, so the next boot does not replay it —
        // one past the highest sequence in it, which for the single frame 0
        // is 1.
        let published = block::scan(&dir, "logs").unwrap();
        assert_eq!(published.len(), 1);
        assert_eq!(published[0].wal_hi, 1);
        assert_eq!(block::wal_watermarks(&dir).unwrap(), [1, 0, 0]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A frame that no block claims comes back, under its own sequence, and
    /// then *is* claimed — so the boot after that one replays nothing.
    ///
    /// Convergence is the property, not recovery. Re-appending a replayed frame
    /// instead of carrying its sequence would leave the original uncovered and
    /// replay it again at every start, for ever, growing the log each time.
    #[tokio::test]
    async fn a_replayed_frame_is_claimed_by_the_block_that_finally_stores_it() {
        let dir = std::env::temp_dir().join(format!("mira-pipe-replay-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let node = block::node_id("replaytest");

        // A crash: framed, never sealed. Dropping the log without `sync` is the
        // harsher case — the frames are only in the page cache, which is what
        // an acknowledgement here promises and all it promises.
        let wal = Wal::open(&dir, node).unwrap();
        let body = {
            use prost::Message as _;
            crate::e2e::logs_export("checkout", 2_000, 4).encode_to_vec()
        };
        wal.append(wal::Signal::Logs, &body).unwrap();
        wal.append(wal::Signal::Logs, &body).unwrap();
        drop(wal);

        let wal = Arc::new(Wal::open(&dir, node).unwrap());
        let c = Arc::new(Config {
            data_dir: dir.clone(),
            node,
            max_block_age: Duration::from_millis(50),
            wal: Some(Arc::clone(&wal)),
            ..Default::default()
        });
        let (tx, _open, h) = spawn::<LogsBuilder>(&c);
        let replayed = {
            let tx = tx.clone();
            let dir = dir.clone();
            tokio::task::spawn_blocking(move || {
                Wal::replay(&dir, node, [0, 0, 0], |_, seq, body| {
                    assert!(tx.replay(body, seq).is_ok(), "the flusher took it");
                    Ok(())
                })
                .unwrap()
            })
            .await
            .unwrap()
        };
        assert_eq!(replayed.replayed, 2);

        drop(tx);
        h.await.unwrap();
        assert_eq!(block::wal_watermarks(&dir).unwrap(), [2, 0, 0]);

        // The second boot: every frame is behind the watermark, so nothing is
        // handed back and the log can be truncated.
        let mut handed_back = 0;
        let again = Wal::replay(
            &dir,
            node,
            block::wal_watermarks(&dir).unwrap(),
            |_, _, _| {
                handed_back += 1;
                Ok(())
            },
        )
        .unwrap();
        assert_eq!(
            (again.replayed, again.skipped, handed_back),
            (0, 2, 0),
            "a frame a block already claims must never be replayed again"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A volume that stops accepting blocks NACKs retryably. Nobody is told
    /// their export is stored, and the signal starts counting as stalled —
    /// which is what `/health` reads, and what takes this node out of a load
    /// balancer instead of leaving it silently eating telemetry.
    #[tokio::test]
    async fn a_block_that_cannot_be_published_is_a_retryable_answer() {
        let (c, dir) = cfg("unpublishable");
        let (tx, _open, h) = spawn::<LogsBuilder>(&c);
        // A file where the staging directory belongs: every publish fails at its
        // first `create_dir_all` and none of them can reach the block tree. That
        // is the shape of a full or detached volume without needing one, and it
        // is the one the flusher cannot see at startup — a signal directory it
        // cannot scan stops it before it takes a single export.
        std::fs::write(dir.join(".tmp"), b"not a directory").unwrap();

        let answer = tx.submit(wide(1)).await;
        assert!(
            matches!(&answer, Err(Rejected::Unavailable(why)) if !why.is_empty()),
            "a publish that failed has to be answered retryably, and with a reason: \
             `Failed` would have the exporter drop the batch, and an empty string \
             leaves the operator reading the sender's log for a disk fault"
        );
        // Not asserted here: the stall clock this also starts. `REJECTS` is
        // process-wide and the other tests in this file publish into it, so the
        // threshold is pinned on a local `Rejects` instead — see
        // `a_stall_is_only_reportable_once_it_has_outlasted_the_recovery`.
        drop(tx);
        h.await.unwrap();
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// An export too large to frame is the one log failure the sender can fix,
    /// so it is the one that comes back as permanent. Retrying it would burn the
    /// link forever: the second attempt is the same bytes and the same refusal.
    #[tokio::test]
    async fn an_export_too_large_for_a_frame_is_refused_permanently() {
        let dir = std::env::temp_dir().join(format!("mira-pipe-huge-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let node = block::node_id("hugetest");
        let c = Arc::new(Config {
            data_dir: dir.clone(),
            node,
            wal: Some(Arc::new(Wal::open(&dir, node).unwrap())),
            ..Default::default()
        });
        let (tx, _open, h) = spawn::<LogsBuilder>(&c);

        let mut req = wide(1);
        req.resource_logs[0].scope_logs[0].log_records[0].body = Some(AnyValue {
            value: Some(any_value::Value::StringValue("x".repeat(64 << 20))),
        });
        let answer = tx.submit(req).await;
        assert!(
            matches!(&answer, Err(Rejected::Failed(why)) if why.contains("frame")),
            "an export that can never be framed has to be refused permanently and \
             named as a framing limit; retrying it burns the link on the same bytes"
        );

        drop(tx);
        h.await.unwrap();
        assert_eq!(blocks(&dir), 0, "nothing was framed, so nothing was stored");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The sweep is two jobs on one blocking hop, and only every 240th tick
    /// does the second: a sync every quarter second, a truncate every minute.
    ///
    /// The open segment is never dropped, whichever tick it is, so what the
    /// first half pins is the watermark arithmetic being reached at all, and a
    /// truncate half that fails leaving the sync half done rather than taking
    /// the maintenance task down with it.
    ///
    /// Then the case where the slow tick does have something to remove: the
    /// slow tick is the only thing that ever shrinks the log, and a sweep that
    /// removed a segment on the fast tick — or left a dead one on the slow one
    /// — is the difference between a log that stays bounded and one that drops
    /// frames no block has claimed yet.
    #[tokio::test]
    async fn a_wal_sweep_syncs_every_tick_and_only_truncates_on_the_slow_one() {
        let dir = std::env::temp_dir().join(format!("mira-pipe-sweep-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let node = block::node_id("sweeptest");
        let wal = Arc::new(Wal::open(&dir, node).unwrap());
        wal.append(wal::Signal::Logs, b"a frame").unwrap();

        // The common tick: sync, and nothing else looked at.
        wal_sweep(Arc::clone(&wal), dir.clone(), false).await;
        // The 240th: no block claims that frame, so the covered watermark is 0
        // and the segment holding it stays.
        wal_sweep(Arc::clone(&wal), dir.clone(), true).await;
        assert_eq!(wal.next_seq(), 1, "a sweep renumbers nothing");
        assert_eq!(
            std::fs::read_dir(dir.join(".wal")).unwrap().count(),
            1,
            "the open segment is never dropped"
        );

        // A truncate that cannot read the block tree is a warning, not a stop:
        // the sync half already happened and the next append still lands. A
        // file where the `logs` directory belongs is the cheapest unreadable
        // tree — a *missing* one is legitimately empty, and scans as such.
        let bad = dir.join("unreadable");
        std::fs::create_dir_all(&bad).unwrap();
        std::fs::write(bad.join("logs"), b"not a directory").unwrap();
        wal_sweep(Arc::clone(&wal), bad, true).await;
        wal.append(wal::Signal::Logs, b"another").unwrap();
        assert_eq!(wal.next_seq(), 2);

        // The segment a crash between `roll` and the first append leaves: no
        // frames, so no watermark can ever cover it, and the empty-segment rule
        // is the only thing that will ever get rid of it.
        let stale = dir.join(".wal").join(format!("{node:08x}-{:020}.wal", 9));
        std::fs::File::create(&stale).unwrap();
        wal_sweep(Arc::clone(&wal), dir.clone(), false).await;
        assert!(stale.exists(), "a sync is not a truncation");
        wal_sweep(Arc::clone(&wal), dir.clone(), true).await;
        assert!(!stale.exists(), "the slow tick removed the dead segment");
        assert_eq!(
            std::fs::read_dir(dir.join(".wal")).unwrap().count(),
            1,
            "and left the open one, which is still holding two unclaimed frames"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A queue that stays full past [`ADMIT_WAIT`] sheds, and shedding has to be
    /// distinguishable from shutdown: `Busy` is retryable and `Closed` is not,
    /// and an exporter that confuses them either drops good data or hammers a
    /// draining node.
    ///
    /// `start_paused`, so the five seconds are five seconds of the test's clock.
    /// Tokio only auto-advances once every task is idle, which here is exactly
    /// the state the wait is supposed to end in.
    #[tokio::test(start_paused = true)]
    async fn a_full_queue_sheds_and_a_closed_one_says_so() {
        let (tx, rx) = mpsc::channel::<Job<ExportLogsServiceRequest>>(1);
        let rejects = &REJECTS[0];
        let ingest = Ingest {
            tx: [tx].into(),
            turn: Arc::default(),
            rejects,
            wal: None,
            signal: wal::Signal::Logs,
        };
        let req = || ExportLogsServiceRequest::default();
        // Relative, not absolute: the counters are process-wide and every other
        // test in this binary shares them.
        let before = rejects.shed.load(Relaxed);

        // Nothing is reading, so the first send fills the channel and the
        // second finds no permit. The first never returns; that is the point.
        let pending = tokio::spawn({
            let i = ingest.clone();
            async move { i.submit(req()).await }
        });
        while rx.capacity() > 0 {
            tokio::task::yield_now().await;
        }
        assert!(matches!(ingest.submit(req()).await, Err(Rejected::Busy)));
        // Shedding is counted, because a 503 with no server-side number behind
        // it is a fact the operator can only get from the sender's log.
        assert_eq!(rejects.shed.load(Relaxed), before + 1);

        // The flusher is gone. In flight becomes `Closed` because the ack sender
        // dropped with it; new work becomes `Closed` because the channel did.
        drop(rx);
        assert!(matches!(pending.await.unwrap(), Err(Rejected::Closed)));
        assert!(matches!(ingest.submit(req()).await, Err(Rejected::Closed)));
    }

    /// The other half of that contract, and the one the measurement is about: a
    /// queue that is full *now* but drains inside [`ADMIT_WAIT`] admits the
    /// export instead of shedding it. Without this the sender re-sends bytes
    /// this node has already decoded, which at 96 connections cost 93% of
    /// exports and two thirds of the throughput.
    #[tokio::test(start_paused = true)]
    async fn a_queue_that_drains_inside_the_wait_admits_instead_of_shedding() {
        let (tx, mut rx) = mpsc::channel::<Job<ExportLogsServiceRequest>>(1);
        // Its own counter, not `REJECTS[0]`: this one asserts that *nothing* was
        // shed, and the process-wide slot is being written by whichever other
        // test in this binary is running beside it.
        let rejects: &'static Rejects = Box::leak(Box::new(Rejects::new("logs")));
        let ingest = Ingest {
            tx: [tx].into(),
            turn: Arc::default(),
            rejects,
            wal: None,
            signal: wal::Signal::Logs,
        };
        let req = || ExportLogsServiceRequest::default();

        // Fill it, and leave the filler parked on its ack so the slot stays
        // taken until something reads.
        let first = tokio::spawn({
            let i = ingest.clone();
            async move { i.submit(req()).await }
        });
        while rx.capacity() > 0 {
            tokio::task::yield_now().await;
        }

        // A second export finds no permit and waits. A reader that comes back
        // four seconds later — inside the wait, well past anything `try_reserve`
        // would have tolerated — frees the slot, and the waiter takes it.
        let waiter = tokio::spawn({
            let i = ingest.clone();
            async move { i.submit(req()).await }
        });
        tokio::time::sleep(Duration::from_secs(4)).await;
        let job = rx.recv().await.expect("the filler's job");
        let _ = job.ack.send(Ok(()));
        assert!(matches!(first.await.unwrap(), Ok(())));

        // The waiter is now queued rather than shed. Ack it the same way.
        let job = rx.recv().await.expect("the waiter's job");
        let _ = job.ack.send(Ok(()));
        assert!(matches!(waiter.await.unwrap(), Ok(())));
        assert_eq!(rejects.shed.load(Relaxed), 0, "nothing was shed");
    }

    /// A data directory that cannot be scanned stops the flusher at startup
    /// rather than at the first flush. Sequence numbers are resumed from what is
    /// on disk, so a pipeline that could not read it would reuse a sequence and
    /// never publish again — failing loudly here is the cheaper end of that.
    #[tokio::test]
    async fn an_unreadable_data_directory_stops_the_flusher_at_startup() {
        let (c, dir) = cfg("unscannable");
        std::fs::write(dir.join("logs"), b"not a directory").unwrap();
        let (tx, _open, h) = spawn::<LogsBuilder>(&c);
        h.await.unwrap();
        assert!(matches!(
            tx.submit(ExportLogsServiceRequest::default()).await,
            Err(Rejected::Closed)
        ));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Retention runs on a timer, and `interval` fires its first tick straight
    /// away — so a zero TTL expires everything on the first pass, with no clock
    /// to advance and no sleep to wait out.
    #[tokio::test]
    async fn retention_drops_expired_blocks_on_its_first_pass() {
        let (c, dir) = cfg("retention");
        let (tx, _open, h) = spawn::<LogsBuilder>(&c);
        tx.submit(crate::e2e::logs_export("checkout", 1_000, 4))
            .await
            .unwrap_or_else(|_| panic!("export"));
        drop(tx);
        h.await.unwrap();
        assert_eq!(blocks(&dir), 1);

        spawn_retention(Arc::new(Config {
            data_dir: dir.clone(),
            retention: Duration::ZERO,
            ..Default::default()
        }));
        // The sweep is a `spawn_blocking`, so yielding is not enough to see it.
        for _ in 0..200 {
            if blocks(&dir) == 0 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert_eq!(blocks(&dir), 0, "a block older than its TTL is unlinked");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A volume that fills faster than the TTL expires is the outage the whole
    /// stack exists to explain, and before this it was permanent: every
    /// `publish` ENOSPC, every export NACKed, and the only thing that deletes
    /// blocks a clock that has not advanced far enough.
    ///
    /// The order is the contract, not the count. Three blocks are published with
    /// their timestamps deliberately out of sequence order, so a sweep that
    /// walked the directory as `scan` returns it — or in the order the blocks
    /// were written — would drop the newest first and delete the data the
    /// incident is being read out of.
    #[tokio::test]
    async fn a_full_volume_drops_the_oldest_blocks_before_their_ttl() {
        let (c, dir) = cfg("space");
        let (tx, _open, h) = spawn::<LogsBuilder>(&c);
        // Awaited one at a time: `submit` returns only once the block holding it
        // is durable, so each of these is a block of its own.
        for ts in [3_000_000, 1_000_000, 2_000_000] {
            tx.submit(crate::e2e::logs_export("checkout", ts, 4))
                .await
                .unwrap_or_else(|_| panic!("export"));
        }
        drop(tx);
        h.await.unwrap();
        assert_eq!(blocks(&dir), 3);

        // Publishing is counted, or `/api/v1/stats` is three zeroes. Relative
        // and monotone, because every other test in this binary shares these.
        let logs = rejects_for("logs");
        assert!(logs.published.load(Relaxed) >= 3, "blocks are counted");
        assert!(logs.rows.load(Relaxed) >= 12, "rows are counted");
        assert!(logs.bytes.load(Relaxed) > 0, "bytes on disk are counted");

        // The TTL is the policy and free space is only the floor under it, so a
        // volume with room loses nothing whatever its blocks' ages.
        assert!(reclaim(&dir, 0.0).unwrap().is_empty());

        // A margin no real volume can satisfy stands in for a full disk: every
        // block goes, oldest first, and the returned order is that order.
        let mut want = block::scan(&dir, "logs").unwrap();
        want.sort_by_key(|b| b.max_ts);
        let want: Vec<PathBuf> = want.into_iter().map(|b| b.dir).collect();
        assert_eq!(reclaim(&dir, 2.0).unwrap(), want);
        assert_eq!(blocks(&dir), 0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Runs `f` with a subscriber attached.
    ///
    /// Not decoration. A `tracing` field whose value is a call —
    /// `%dir.display()`, `format!("{free:.3}")` — is not evaluated at all when
    /// nothing is listening, so the code that builds the warnings below only
    /// *runs* under this. Thread-local rather than global, so the flushers the
    /// other tests in this binary are running stay quiet.
    fn listening<T>(f: impl FnOnce() -> T) -> T {
        let sub = tracing_subscriber::fmt()
            .with_max_level(tracing::Level::TRACE)
            .with_test_writer()
            .finish();
        tracing::subscriber::with_default(sub, f)
    }

    /// Polls `done` for two seconds. The sweeps below run on a blocking thread,
    /// so yielding is not enough to see one land, and a fixed sleep is either a
    /// flake on a loaded machine or dead time on an idle one.
    async fn until(mut done: impl FnMut() -> bool) -> bool {
        for _ in 0..200 {
            if done() {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        done()
    }

    /// A block directory with nothing in it. `scan` reads the name and
    /// `reclaim` unlinks the directory; neither opens a table, so a test about
    /// *which* blocks go does not need a flusher to produce them.
    fn fake_block(dir: &Path, signal: &str, max_ts: i64, seq: u64) -> PathBuf {
        let partition = dir.join(signal).join("p=1970-01-01-00");
        std::fs::create_dir_all(&partition).unwrap();
        let block = partition.join(format!(
            "{:020}-{max_ts:020}-{:08x}-{seq:012}-{:020}",
            0, 7, 0
        ));
        std::fs::create_dir_all(&block).unwrap();
        block
    }

    /// Shedding is thousands of exports a second when it happens at all: the
    /// counter has to take every one of them and the log has to take one a
    /// second, or the incident is either invisible or drowned in its own
    /// warnings. A local `Rejects`, not the process-wide one, so the count is
    /// exact rather than a lower bound.
    #[test]
    fn every_shed_export_is_counted_and_at_most_one_a_second_is_logged() {
        let gate = AtomicU64::new(0);
        assert!(once_a_second(&gate), "the first caller in a second speaks");
        assert!(!once_a_second(&gate), "and everyone behind it is silent");

        let r = Rejects::new("logs");
        listening(|| {
            for _ in 0..3 {
                r.record_shed();
            }
        });
        assert_eq!(r.shed.load(Relaxed), 3, "every shed export is counted");
        assert_ne!(
            r.warned.load(Relaxed),
            0,
            "the gate is armed, so the next thousand this second are silent"
        );
    }

    /// A log that cannot take a frame is not the sender's fault unless the
    /// frame is too large, and the difference is the whole failure contract: a
    /// full or detached volume answered `Failed` has every exporter drop the
    /// batch it is holding, which is data loss chosen by an error variant.
    #[tokio::test]
    async fn a_log_failure_that_is_not_the_senders_fault_is_answered_retryably() {
        let dir = std::env::temp_dir().join(format!("mira-pipe-walgone-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let node = block::node_id("walgone");
        let wal = Arc::new(Wal::open(&dir, node).unwrap());
        // One segment's worth, so the next append has to roll to a new file.
        // The size trigger is the only way in from here — `Wal`'s internals are
        // private to `mira-core` — and it buys the one failure that is neither
        // "too large" nor a corrupt disk.
        wal.append(wal::Signal::Logs, &vec![0u8; 64 << 20]).unwrap();
        let c = Arc::new(Config {
            data_dir: dir.clone(),
            node,
            wal: Some(Arc::clone(&wal)),
            ..Default::default()
        });
        let (tx, _open, h) = spawn::<LogsBuilder>(&c);
        // The log's directory, removed under it: the roll cannot create its
        // successor, which is what a volume that went away looks like from
        // inside `append`.
        std::fs::remove_dir_all(dir.join(".wal")).unwrap();

        let answer = tx.submit(wide(1)).await;
        assert!(
            matches!(&answer, Err(Rejected::Unavailable(why)) if !why.is_empty()),
            "a log that cannot write must be retryable and say why"
        );
        drop(tx);
        h.await.unwrap();
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `spawn_retention` only starts the maintenance task when a log is
    /// configured, and the guard inside it is what makes that safe to get
    /// wrong: without it the task would tick four times a second on a node
    /// that has nothing to sync.
    #[tokio::test]
    async fn wal_maintenance_without_a_log_has_nothing_to_do() {
        let (c, dir) = cfg("nowal");
        assert!(
            c.wal.is_none(),
            "the shipped default for this test's config"
        );
        // It returns. If the guard were gone this would tick for ever and the
        // timeout, not the assertion, would be the failure.
        tokio::time::timeout(Duration::from_millis(250), wal_maintenance(c))
            .await
            .expect("a node with no log has no maintenance loop to run");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A staging directory is what a `publish` killed mid-write leaves behind.
    /// Nothing will ever finish it and nothing reads it, so a boot that did not
    /// sweep it would leak a copy of a whole block per crash onto the volume
    /// retention is trying to keep free.
    #[tokio::test]
    async fn a_staging_directory_a_crash_left_behind_is_swept_at_boot() {
        let (c, dir) = cfg("staging");
        let node = c.node;
        let stale = dir
            .join(".tmp")
            .join(format!("logs-{node:08x}-000000000007"));
        std::fs::create_dir_all(&stale).unwrap();
        std::fs::write(stale.join("logs.arrow"), b"half a block").unwrap();
        // Another node's staging directory, on the shared volume of section 12: not
        // this process's to remove, and removing it would delete a block a live
        // replica is part-way through writing.
        let theirs = dir
            .join(".tmp")
            .join(format!("logs-{:08x}-000000000007", 0));
        std::fs::create_dir_all(&theirs).unwrap();

        let (tx, _open, h) = spawn::<LogsBuilder>(&c);
        tx.submit(ExportLogsServiceRequest::default())
            .await
            .unwrap_or_else(|_| panic!("the flusher booted"));
        drop(tx);
        h.await.unwrap();

        assert!(!stale.exists(), "the leaked staging directory is gone");
        assert!(
            theirs.exists(),
            "another replica's is not this node's to take"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A builder that fails where the real ones only fail on a request no
    /// block can hold. `SEAL` picks which of the two seams breaks: the seal
    /// that publishes, or the snapshot the read path asks for. Reaching either
    /// through `LogsBuilder` would take a 65k-key request per attempt, and
    /// what is under test is not the encoder — it is what the flusher promises
    /// when an encoder does fail.
    #[derive(Default)]
    struct Brittle<const SEAL: bool>(LogsBuilder);

    impl<const SEAL: bool> SignalBuilder for Brittle<SEAL> {
        type Request = ExportLogsServiceRequest;
        const SIGNAL: &'static str = "logs";

        fn has_headroom_for(&self, req: &Self::Request) -> bool {
            self.0.has_headroom_for(req)
        }
        fn append_request(&mut self, req: &Self::Request) -> mira_core::error::Result<usize> {
            self.0.append_request(req)
        }
        fn approx_bytes(&self) -> usize {
            self.0.approx_bytes()
        }
        fn is_empty(&self) -> bool {
            self.0.is_empty()
        }
        fn finish(&mut self) -> mira_core::error::Result<mira_core::signal::Sealed> {
            if SEAL {
                // A real one: this is what an overflowing dictionary raises,
                // and `finish` leaves a fresh builder behind either way.
                let _ = self.0.finish();
                return Err(mira_core::Error::DictionaryFull("attr_key"));
            }
            self.0.finish()
        }
        fn snapshot(&self) -> mira_core::error::Result<mira_core::signal::Sealed> {
            if SEAL {
                return self.0.snapshot();
            }
            Err(mira_core::Error::DictionaryFull("attr_key"))
        }
    }

    /// A block that cannot be sealed must not take anybody's data with it. Every
    /// caller is told `Unavailable` — never `Failed`, because whose export broke
    /// the encoder is not knowable from here — and, with a log, the watermark
    /// the discarded block would have claimed is dropped on the floor so the
    /// frames come back on the next boot. A `wal_hi` left standing here is the
    /// one bug in this file that loses acknowledged data silently: the block
    /// never existed, but the log would be truncated as if it had.
    #[tokio::test]
    async fn a_block_that_cannot_be_sealed_nacks_retryably_and_leaves_its_frames_in_the_log() {
        // Without a log the caller is the one waiting on the seal, so it is the
        // one that has to be told.
        let (c, dir) = cfg("brittle-seal");
        let (tx, _open, h) = spawn::<Brittle<true>>(&c);
        let answer = tx.submit(wide(1)).await;
        assert!(
            matches!(&answer, Err(Rejected::Unavailable(why)) if why.contains("dictionary")),
            "a seal that failed is the block's fault, not this caller's"
        );
        drop(tx);
        h.await.unwrap();
        assert_eq!(blocks(&dir), 0, "nothing was published");
        let _ = std::fs::remove_dir_all(&dir);

        // With one, the caller was acknowledged long before the seal, so the
        // promise that survives is the log's.
        let dir = std::env::temp_dir().join(format!("mira-pipe-brittle-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let node = block::node_id("brittletest");
        let wal = Arc::new(Wal::open(&dir, node).unwrap());
        let c = Arc::new(Config {
            data_dir: dir.clone(),
            node,
            max_block_age: Duration::from_millis(50),
            wal: Some(Arc::clone(&wal)),
            ..Default::default()
        });
        let (tx, open, h) = spawn::<Brittle<true>>(&c);
        tx.submit(wide(1))
            .await
            .unwrap_or_else(|_| panic!("the log took it, whatever the block does later"));
        drop(tx);
        h.await.unwrap();

        assert_eq!(
            block::wal_watermarks(&dir).unwrap(),
            [0, 0, 0],
            "a block that was never published claims no sequence"
        );
        assert!(
            open.fresh().await.is_empty(),
            "and advertises no rows the read path could no longer produce"
        );
        // Which is what makes the acknowledgement honest: the frame is still
        // there and the next boot hands it back.
        let replayed = Wal::replay(
            &dir,
            node,
            block::wal_watermarks(&dir).unwrap(),
            |_, _, _| Ok(()),
        )
        .unwrap();
        assert_eq!(
            (replayed.replayed, replayed.skipped),
            (1, 0),
            "the acknowledged export survived the block that could not hold it"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The open-block snapshot is best-effort, and "best-effort" has to mean
    /// *nothing* rather than *something stale*: the read path shows a snapshot
    /// as if it were on disk, so one that could not be rebuilt must clear the
    /// slot. Leaving the last one in place would serve rows from a block that
    /// has since been sealed and republished — the same records twice.
    #[tokio::test]
    async fn an_open_block_that_cannot_be_snapshotted_shows_nothing_rather_than_stale_rows() {
        for (breaks, want) in [(true, false), (false, true)] {
            let dir =
                std::env::temp_dir().join(format!("mira-pipe-snap{breaks}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            let node = block::node_id("snaptest");
            let c = Arc::new(Config {
                data_dir: dir.clone(),
                node,
                // Long enough that nothing seals under the test: what is being
                // read is the block while it is still open.
                max_block_age: Duration::from_secs(30),
                wal: Some(Arc::new(Wal::open(&dir, node).unwrap())),
                ..Default::default()
            });
            // `Brittle<false>` fails `snapshot` and seals fine; `Brittle<true>`
            // is the other way round, so the healthy comparison runs through
            // the same wrapper rather than a different type.
            let (tx, open, h) = if breaks {
                spawn::<Brittle<false>>(&c)
            } else {
                spawn::<Brittle<true>>(&c)
            };
            tx.submit(crate::e2e::logs_export("checkout", 2_000, 4))
                .await
                .unwrap_or_else(|_| panic!("acknowledged by the log"));

            // `fresh` waits for the flusher to drain its queue, so this is not
            // a race: the export is in the builder by the time it answers.
            assert_eq!(
                !open.fresh().await.is_empty(),
                want,
                "breaks={breaks}: a snapshot that failed must clear the slot"
            );
            drop(tx);
            h.await.unwrap();
            let _ = std::fs::remove_dir_all(&dir);
        }
    }

    /// Reclaim stops the moment the volume is back above the floor. It is
    /// deleting telemetry nobody asked it to delete, so "enough" is the whole
    /// contract: a sweep that ran to the end of the list because it only
    /// checked before the first unlink would empty the disk to free one block's
    /// worth of space.
    ///
    /// ponytail: the floor is derived from 128 MiB of real files on the real
    /// volume, because `free_fraction` is a `statfs` and there is nothing to
    /// inject. Something else on this disk moving 64 MiB the wrong way during
    /// the sweep would flap it; the upgrade path is a free-space probe the
    /// caller supplies, which would also make this test instant.
    #[test]
    fn reclaim_stops_as_soon_as_the_volume_is_back_over_the_floor() {
        let dir = std::env::temp_dir().join(format!("mira-pipe-ballast-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let oldest = fake_block(&dir, "logs", 1_000, 0);
        let newer = fake_block(&dir, "logs", 2_000, 1);
        let newest = fake_block(&dir, "traces", 3_000, 2);

        let empty = block::free_fraction(&dir).unwrap();
        // Many synced files rather than one big one, and both halves matter.
        // Synced, because until the extents are allocated `statfs` has not
        // noticed them and `full` below is just `empty`. Many, because APFS
        // returns the space of an unlinked file asynchronously — measured here
        // at up to 176 ms for a single 64 MiB file, which is far longer than
        // the whole sweep — while a directory of 1 MiB files comes back
        // essentially whole by the time the last unlink returns (measured:
        // 0.998 of it, worst of five runs).
        {
            use std::io::Write;
            for i in 0..128 {
                let mut f = std::fs::File::create(oldest.join(format!("{i}.arrow"))).unwrap();
                f.write_all(&vec![0u8; 1 << 20]).unwrap();
                f.sync_all().unwrap();
            }
        }
        let full = block::free_fraction(&dir).unwrap();
        assert!(full < empty, "128 MiB moved the needle: {full} vs {empty}");
        // Halfway between the two, so the sweep starts below the floor and is
        // back above it after exactly one unlink.
        let floor = (full + empty) / 2.0;

        let dropped = listening(|| reclaim(&dir, floor).unwrap());
        assert_eq!(
            dropped,
            vec![oldest],
            "the oldest block, and then it stopped"
        );
        assert!(
            newer.exists() && newest.exists(),
            "nothing else was touched"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// One block the sweep cannot unlink says nothing about the next one.
    /// Returning at the first error stopped retention for the whole volume at
    /// its oldest broken block — the disk stayed full and every export was
    /// NACKed, which is the outage reclaim exists to prevent — and a block
    /// another replica removed first is not an error at all.
    #[test]
    fn a_block_that_cannot_be_dropped_does_not_stop_the_ones_behind_it() {
        let dir = std::env::temp_dir().join(format!("mira-pipe-undrop-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        // A regular file wearing a block's name. `scan` reads names, so it is
        // returned like any other block and `remove_dir_all` refuses it — the
        // same shape as a directory this process cannot traverse.
        let partition = dir.join("logs").join("p=1970-01-01-00");
        std::fs::create_dir_all(&partition).unwrap();
        let impostor = partition.join(format!("{:020}-{:020}-{:08x}-{:012}-{:020}", 0, 1, 7, 0, 0));
        std::fs::write(&impostor, b"not a block").unwrap();

        // A margin no volume can satisfy: the sweep tries everything it can see.
        let dropped = listening(|| reclaim(&dir, 2.0).unwrap());
        assert!(
            dropped.is_empty() && impostor.exists(),
            "nothing was dropped, and the sweep still returned"
        );

        // The same block seen twice — one replica's unlink landing between this
        // sweep's `scan` and its `remove_dir_all` — simulated by listing one
        // tree under two signals.
        let real = fake_block(&dir, "traces", 5_000, 3);
        std::os::unix::fs::symlink(dir.join("traces"), dir.join("metrics")).unwrap();
        let dropped = listening(|| reclaim(&dir, 2.0).unwrap());
        assert_eq!(
            dropped,
            vec![real.clone()],
            "the block is reported once, and the second sighting is not an error"
        );
        assert!(!real.exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Every way one sweep can fail, and the property is the same for all of
    /// them: the loop keeps its next tick. Retention is the only thing that
    /// frees space, so a sweep that took the task down with it would turn one
    /// unreadable signal into a volume that fills up and stays full.
    #[tokio::test]
    async fn a_sweep_that_fails_never_takes_the_retention_loop_with_it() {
        let dir = std::env::temp_dir().join(format!("mira-pipe-sweepfail-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        // `logs` cannot be scanned, so both `expire` and `compact` fail for it;
        // `traces` holds a block a zero TTL expires. One signal failing must
        // not skip the others, which is the half of this that is silent.
        std::fs::write(dir.join("logs"), b"not a directory").unwrap();
        let doomed = fake_block(&dir, "traces", 1_000, 0);

        // The first tick is immediate and the second is a minute away, so a
        // task that is still unfinished after its sweep is a task that took the
        // failure and went back to waiting.
        let sweeping = tokio::spawn(retention(Arc::new(Config {
            data_dir: dir.clone(),
            retention: Duration::ZERO,
            ..Default::default()
        })));
        assert!(
            until(|| !doomed.exists()).await,
            "the signal that could be swept was swept, whatever the broken one did"
        );
        assert!(!sweeping.is_finished(), "and the loop kept its next tick");
        sweeping.abort();

        // A data directory that is not there: `free_fraction` cannot answer, so
        // the sweep is TTL-only rather than a dead task. Nothing on disk changes
        // — the observable is that the task is still there afterwards.
        let sweeping = tokio::spawn(retention(Arc::new(Config {
            data_dir: dir.join("never-created"),
            retention: Duration::ZERO,
            ..Default::default()
        })));
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert!(
            !sweeping.is_finished(),
            "a volume it cannot even measure is not a reason to stop measuring it"
        );
        sweeping.abort();
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `retention: 999999d` is how an operator writes "keep it forever", and
    /// until the cutoff was made saturating it meant the exact opposite: the
    /// TTL in nanoseconds overflowed an `i64`, wrapped negative, and put the
    /// cutoff in the future — where every block on the volume is older than it.
    /// The sweep still has to do its other job while keeping everything.
    #[tokio::test]
    async fn an_absurd_retention_keeps_every_block_and_still_compacts_the_cold_ones() {
        let (c, dir) = cfg("forever");
        let (tx, _open, h) = spawn::<LogsBuilder>(&c);
        // Timestamped in 1970, so it is cold by any clock: the compaction half
        // of the sweep has something to do and the TTL half must not.
        tx.submit(crate::e2e::logs_export("checkout", 1_000, 4))
            .await
            .unwrap_or_else(|_| panic!("export"));
        drop(tx);
        h.await.unwrap();
        assert_eq!(blocks(&dir), 1);

        let cfg = Arc::new(Config {
            data_dir: dir.clone(),
            // 547 years. `Duration::as_nanos` is a `u128` and holds it; an
            // `i64` of nanoseconds does not.
            retention: Duration::from_secs(200_000 * 86_400),
            ..Default::default()
        });
        let sweeping = tokio::spawn(retention(cfg));
        let cold = block::scan(&dir, "logs").unwrap()[0].dir.join("cold");
        assert!(
            until(|| cold.exists()).await,
            "the block was compacted rather than deleted"
        );
        assert!(
            !sweeping.is_finished(),
            "one sweep, and the loop is still there"
        );
        sweeping.abort();

        assert_eq!(
            blocks(&dir),
            1,
            "a retention longer than i64 nanoseconds keeps everything"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Readiness has to be a *sustained* condition. A probe that flipped on the
    /// first failed publish would pull the node out of its Service for every
    /// EIO and every remount, and an endpoint list that changes every ten
    /// seconds loses more exports than the node it is protecting.
    #[test]
    fn a_stall_is_only_reportable_once_it_has_outlasted_the_recovery() {
        let r = Rejects::new("logs");
        let now = 1_700_000_000;
        assert_eq!(stall_of(&r, now), None, "a healthy signal is never unready");

        r.stalled_since.store(now, Relaxed);
        let after = UNREADY_AFTER.as_secs();
        assert_eq!(
            stall_of(&r, now),
            None,
            "one failed publish is not an outage"
        );
        assert_eq!(stall_of(&r, now + after - 1), None);
        assert_eq!(stall_of(&r, now + after), Some(after));

        // The clock is the *first* failure of the run, not the latest one, or a
        // node failing every two seconds would reset itself to healthy forever.
        r.clocks[0].stalled_since.store(1, Relaxed);
        r.mark_stalled(0);
        assert_eq!(r.stalled_since.load(Relaxed), 1);
        // ...and the first failure does start it, from zero.
        let fresh = Rejects::new("logs");
        assert_eq!(fresh.stalled_since.load(Relaxed), 0);
        fresh.mark_stalled(0);
        assert_ne!(fresh.stalled_since.load(Relaxed), 0);

        // Nothing in this test binary has been unable to store for two minutes,
        // so the live answer is the healthy one.
        assert_eq!(stalled(), None);
    }

    /// A signal's health is the worst of its shards, not the last one to report.
    ///
    /// The bug this exists to prevent: with the flushers writing the aggregate
    /// directly, a shard sealing normally would clear `open_since` while a
    /// sibling sat on a block it could not flush, and `/healthz` would call a
    /// stuck node healthy. Oldest-of-nonzero is the only reduction that answers
    /// "is anything stuck" rather than "was the last thing that happened fine".
    #[test]
    fn a_signals_clocks_report_the_worst_shard_not_the_latest_one() {
        let r = Rejects::new("logs");
        r.set_open_since(0, 100);
        r.set_open_since(1, 500);
        assert_eq!(r.open_since.load(Relaxed), 100);

        // Shard 0 seals. Shard 1 is still holding its block open, so the signal
        // still has something open — and it is shard 1's clock now.
        r.set_open_since(0, 0);
        assert_eq!(r.open_since.load(Relaxed), 500);
        r.set_open_since(1, 0);
        assert_eq!(
            r.open_since.load(Relaxed),
            0,
            "nothing open is zero, not min"
        );

        // Same rule for stalls, and the same failure mode: one shard recovering
        // does not make the node ready while another cannot write.
        r.clocks[1].stalled_since.store(900, Relaxed);
        r.mark_stalled(0);
        let both = r.stalled_since.load(Relaxed);
        assert!(both > 0 && both <= 900, "the older of the two, got {both}");
        r.clear_stalled(0);
        assert_eq!(r.stalled_since.load(Relaxed), 900);
        r.clear_stalled(1);
        assert_eq!(r.stalled_since.load(Relaxed), 0);
    }

    /// Shard count is a function of core count and nothing else — section 4's
    /// rule for what a shard may be keyed on.
    #[test]
    fn shards_are_counted_from_cores_and_clamped_at_both_ends() {
        // Auto. Halved because a flusher is a consumer and the decode is the
        // producer; never zero, whatever the machine claims.
        assert_eq!(shard_count(0, 1), 1);
        assert_eq!(shard_count(0, 2), 1);
        assert_eq!(shard_count(0, 12), 6);
        // The ceiling is what keeps a 128-core host from publishing 64 files
        // per seal window per signal.
        assert_eq!(shard_count(0, 128), MAX_SHARDS);
        // Configured wins, up to the same ceiling — this is the cgroup-quota
        // escape hatch, and an operator who types 1 gets the old behaviour.
        assert_eq!(shard_count(1, 128), 1);
        assert_eq!(shard_count(4, 2), 4);
        assert_eq!(shard_count(999, 2), MAX_SHARDS);
    }

    /// A sharded config with the log on, which is the shipped default and the
    /// only setting under which a test can submit without waiting for a seal.
    ///
    /// Without a log `submit` returns at the publish, so a long `max_block_age`
    /// makes every serial submit wait out the age timer and publish a block of
    /// its own — which is the behaviour under test, inverted.
    fn sharded(name: &str, shards: usize) -> (Arc<Config>, PathBuf) {
        let (c, dir) = cfg(name);
        let node = block::node_id(name);
        let mut c = Arc::try_unwrap(c).ok().expect("freshly built");
        c.node = node;
        c.shards = shards;
        c.wal = Some(Arc::new(Wal::open(&dir, node).unwrap()));
        // Long enough that nothing seals on the timer mid-test: the blocks here
        // are sealed by a full dictionary or by the shutdown path.
        c.max_block_age = Duration::from_secs(30);
        (Arc::new(c), dir)
    }

    /// Every shard's blocks land, and no two of them collide on a sequence.
    ///
    /// A collision is not a cosmetic problem: the block directory name is the
    /// manifest, so `rename` onto an existing directory means the node stops
    /// publishing — and with one queue per shard, "shard 2 picks the next
    /// number" is not a thing anything can observe. Stride and offset are all
    /// that keep them apart.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn shards_partition_the_sequence_space() {
        let (c, dir) = sharded("shardseq", 4);
        let (tx, _open, h) = spawn::<LogsBuilder>(&c);
        // 40k distinct keys each: two of these cannot share a `UInt16`
        // dictionary, so whichever shard takes two publishes two, and the
        // sequences still may not collide.
        let mut sent = Vec::new();
        for _ in 0..8 {
            let tx = tx.clone();
            sent.push(tokio::spawn(async move { tx.submit(wide(40_000)).await }));
        }
        for s in sent {
            s.await
                .unwrap()
                .unwrap_or_else(|_| panic!("nothing may be shed: the wait is 5s"));
        }
        drop(tx);
        h.await.unwrap();

        let seqs = seqs(&dir);
        assert_eq!(seqs.len(), 8, "one block per export, none lost");
        let mut uniq = seqs.clone();
        uniq.dedup();
        assert_eq!(uniq, seqs, "two shards reused a sequence: {seqs:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A node that is not saturating one flusher keeps behaving like a node
    /// with one flusher, and its sequences stride by the shard count.
    ///
    /// The reason dispatch is first-fit and not round-robin. Round-robin would
    /// spread a trickle over every shard and publish `shards` nearly-empty
    /// blocks per seal window, which is the small-file explosion section 4
    /// rejects hash sharding for, arrived at from the other direction. Shard
    /// 0's queue has room every time, so `try_reserve` never has to look past
    /// it.
    #[tokio::test]
    async fn a_trickle_stays_on_one_shard_and_strides_its_sequences() {
        let (c, dir) = sharded("shardtrickle", 4);
        let (tx, _open, h) = spawn::<LogsBuilder>(&c);
        for _ in 0..4 {
            tx.submit(crate::e2e::logs_export("checkout", 2_000, 4))
                .await
                .unwrap_or_else(|_| panic!("acknowledged"));
        }
        // Two 40k-key exports: the first joins the open block, the second
        // cannot share its dictionary and so seals it. Two blocks from one
        // shard, which is what makes the stride observable.
        for _ in 0..2 {
            tx.submit(wide(40_000))
                .await
                .unwrap_or_else(|_| panic!("acknowledged"));
        }
        drop(tx);
        h.await.unwrap();
        assert_eq!(
            seqs(&dir),
            vec![0, 4],
            "one shard's blocks, striding by the shard count — four shards must \
             not mean four files for a load one shard can take"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Read-your-writes survives the fan-out: every acknowledged export is
    /// visible in some shard's open block, before anything has been sealed.
    ///
    /// The property section 4 buys from FIFO ordering, re-asserted now that
    /// there is more than one FIFO. It holds for the same reason it did — an
    /// acknowledged export is in exactly one shard's channel until that shard
    /// appends it — but only because `fresh` asks all of them.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn every_shard_answers_the_read_path() {
        let (mut c, dir) = sharded("shardfresh", 4);
        // One slot per shard, so eight concurrent submits have to spill past
        // shard 0 rather than hoping the scheduler spreads them.
        Arc::get_mut(&mut c).unwrap().queue = 4;
        let (tx, open, h) = spawn::<LogsBuilder>(&c);
        let mut sent = Vec::new();
        for i in 0..8 {
            let tx = tx.clone();
            sent.push(tokio::spawn(async move {
                tx.submit(crate::e2e::logs_export("checkout", 2_000 + i * 10, 4))
                    .await
            }));
        }
        for s in sent {
            s.await.unwrap().unwrap_or_else(|_| panic!("acknowledged"));
        }
        let rows: usize = open.fresh().await.iter().map(|o| o.sealed.num_rows).sum();
        assert_eq!(
            rows, 32,
            "eight exports of four records, all of them findable"
        );

        drop(tx);
        h.await.unwrap();
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The second shard takes what the first one cannot.
    ///
    /// `reserve` at the unit level, with no flusher behind either channel: the
    /// first is full, so the job has to land in the second rather than wait out
    /// `ADMIT_WAIT` and shed.
    #[tokio::test]
    async fn a_full_shard_spills_into_the_next_one() {
        let (tx0, _rx0) = mpsc::channel::<Job<ExportLogsServiceRequest>>(1);
        let (tx1, mut rx1) = mpsc::channel::<Job<ExportLogsServiceRequest>>(1);
        // Shard 0's one slot, taken and held.
        let _held = tx0.clone().reserve_owned().await.unwrap();
        let ingest = Ingest {
            tx: [tx0, tx1].into(),
            turn: Arc::default(),
            rejects: Box::leak(Box::new(Rejects::new("logs"))),
            wal: None,
            signal: wal::Signal::Logs,
        };
        let sent = tokio::spawn({
            let i = ingest.clone();
            async move { i.submit(ExportLogsServiceRequest::default()).await }
        });
        let job = rx1
            .recv()
            .await
            .expect("shard 1 gets what shard 0 cannot take");
        let _ = job.ack.send(Ok(()));
        sent.await
            .unwrap()
            .unwrap_or_else(|_| panic!("admitted, not shed"));
        assert_eq!(
            ingest.rejects.shed.load(Relaxed),
            0,
            "spilling to a free shard is not shedding"
        );
    }
}
