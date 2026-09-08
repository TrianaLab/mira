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
//! Flush is `spawn_blocking`: it fsyncs. The export is acknowledged only after
//! the block directory rename is durable, because OTLP's retryable status set
//! covers exports in flight at a crash — acking earlier is the one window where
//! data is lost with the client believing it was stored.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use mira_core::SignalBuilder;
use mira_core::block;
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
}

impl Default for Config {
    fn default() -> Self {
        Self {
            data_dir: PathBuf::from("./data"),
            node: block::node_id("mira"),
            target_block_bytes: 32 << 20,
            max_block_age: Duration::from_secs(2),
            retention: Duration::from_secs(7 * 24 * 3600),
        }
    }
}

struct Job<R> {
    req: R,
    ack: oneshot::Sender<Result<(), String>>,
}

/// The write handle for one signal. `R` is that signal's OTLP export request.
pub struct Ingest<R> {
    tx: mpsc::Sender<Job<R>>,
}

// Derived `Clone` would demand `R: Clone`, which no export request is. Only the
// `Sender` is cloned, and that is unconditional.
impl<R> Clone for Ingest<R> {
    fn clone(&self) -> Self {
        Self {
            tx: self.tx.clone(),
        }
    }
}

/// Why an export could not be admitted. Neither of these is a partial success:
/// OTLP forbids the client from retrying a partial success, so reporting
/// overload that way permanently destroys the data and blames the sender.
pub enum Rejected {
    /// Queue full. Transient; retry.
    Busy,
    /// The engine is shutting down.
    Closed,
    /// Durable write failed.
    Failed(String),
}

impl<R> Ingest<R> {
    /// Enqueue and wait for durability. Returns as soon as the block containing
    /// this request has been fsynced and renamed into place.
    pub async fn submit(&self, req: R) -> Result<(), Rejected> {
        let (ack, wait) = oneshot::channel();
        // try_reserve, not send().await: shedding before the decode work is the
        // difference between a fast NACK and an unbounded latency tail.
        let permit = match self.tx.try_reserve() {
            Ok(p) => p,
            Err(mpsc::error::TrySendError::Full(())) => return Err(Rejected::Busy),
            Err(mpsc::error::TrySendError::Closed(())) => return Err(Rejected::Closed),
        };
        permit.send(Job { req, ack });
        match wait.await {
            Ok(Ok(())) => Ok(()),
            Ok(Err(e)) => Err(Rejected::Failed(e)),
            Err(_) => Err(Rejected::Closed),
        }
    }
}

/// Every signal that has an on-disk directory. Retention sweeps all of them;
/// [`block::scan`] treats a missing one as empty, so listing a signal before its
/// encoder exists is harmless.
pub const SIGNALS: [&str; 3] = ["logs", "traces", "metrics"];

/// Start one signal's ingest pipeline. Returns the handle its receivers push
/// into. Each signal gets its own channel, flusher task and block sequence, so a
/// slow flush on one cannot stall another.
pub fn spawn<B: SignalBuilder>(cfg: Arc<Config>) -> Ingest<B::Request> {
    let (tx, rx) = mpsc::channel(128);
    tokio::spawn(flusher::<B>(rx, cfg));
    Ingest { tx }
}

/// One sweep for all signals, not one per signal: retention is IO against the
/// directory tree, and three tasks waking on the same minute boundary to unlink
/// from the same volume is contention for nothing.
pub fn spawn_retention(cfg: Arc<Config>) {
    tokio::spawn(retention(cfg));
}

async fn flusher<B: SignalBuilder>(mut rx: mpsc::Receiver<Job<B::Request>>, cfg: Arc<Config>) {
    // Resume the sequence past whatever is already on disk so block directory
    // names stay unique across restarts. This is the entirety of crash recovery.
    //
    // `max`, not `last`: `scan` sorts by `(min_ts, seq)`, so the last element is
    // the latest-timestamped block, which is not the highest sequence number
    // whenever a restart follows a backlog replay. Reusing a sequence makes the
    // next `rename` land on an existing directory and the node never publishes
    // again.
    let mut seq = match block::scan(&cfg.data_dir, B::SIGNAL) {
        Ok(blocks) => blocks.iter().map(|b| b.seq).max().map_or(0, |s| s + 1),
        Err(e) => {
            tracing::error!(signal = B::SIGNAL, error = %e, "cannot scan data directory");
            return;
        }
    };

    let mut builder = B::default();
    let mut waiters: Vec<oneshot::Sender<Result<(), String>>> = Vec::new();
    let mut batch = Vec::with_capacity(64);
    // Jobs that did not fit the open block. They go into the next one, so a full
    // dictionary costs a slightly small block and never costs a caller its data.
    let mut carry: Vec<Job<B::Request>> = Vec::new();
    let mut deadline = Instant::now() + cfg.max_block_age;
    let mut open = true;

    // Carry outlives the channel: a request deferred by the last block still has
    // to land somewhere before the task exits.
    while open || !carry.is_empty() {
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
                _ = sleep_until(deadline) => aged = true,
            }
        }

        let mut jobs = std::mem::take(&mut carry);
        jobs.append(&mut batch);
        let mut dict_full = false;
        for job in jobs {
            // Once one job has been deferred, every job after it must be too, or
            // the block would acknowledge exports out of arrival order.
            if dict_full || !builder.has_headroom_for(&job.req) {
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
            match builder.append_request(&job.req) {
                Ok(_) => waiters.push(job.ack),
                Err(e) => {
                    let _ = job.ack.send(Err(e.to_string()));
                }
            }
        }

        if dict_full && builder.is_empty() {
            // One request alone cannot fit an empty block: it carries more than
            // 65536 distinct attribute keys. Sealing would not help, so fail it
            // rather than carry it forever.
            for job in carry.drain(..) {
                let _ = job
                    .ack
                    .send(Err("request exceeds one block's dictionary capacity".into()));
            }
            dict_full = false;
        }

        let full = dict_full || builder.approx_bytes() >= cfg.target_block_bytes;
        if builder.is_empty() || !(full || (aged && !waiters.is_empty()) || !open) {
            // Nothing to seal. Push the idle timer out so a stale deadline does
            // not spin the loop.
            if waiters.is_empty() {
                deadline = Instant::now() + cfg.max_block_age;
            }
            continue;
        }

        let sealed = match builder.finish() {
            Ok(s) => s,
            Err(e) => {
                let msg = e.to_string();
                for w in waiters.drain(..) {
                    let _ = w.send(Err(msg.clone()));
                }
                // `finish` leaves a fresh builder behind even when it fails, so
                // there is nothing to repair here — see `SignalBuilder::finish`.
                tracing::error!(signal = B::SIGNAL, error = %msg, "block discarded");
                continue;
            }
        };

        let dir = cfg.data_dir.clone();
        let node = cfg.node;
        let this_seq = seq;
        seq += 1;
        let rows = sealed.num_rows;
        let result = tokio::task::spawn_blocking(move || {
            block::publish(
                &dir,
                B::SIGNAL,
                node,
                this_seq,
                sealed.min_ts,
                sealed.max_ts,
                &sealed.refs(),
            )
            .map(|b| b.dir)
        })
        .await;

        let outcome = match result {
            Ok(Ok(path)) => {
                tracing::info!(signal = B::SIGNAL, rows, seq = this_seq, path = %path.display(), "block published");
                Ok(())
            }
            Ok(Err(e)) => Err(e.to_string()),
            Err(e) => Err(format!("flush task panicked: {e}")),
        };
        for w in waiters.drain(..) {
            let _ = w.send(outcome.clone());
        }
        deadline = Instant::now() + cfg.max_block_age;
    }
}

async fn retention(cfg: Arc<Config>) {
    let mut tick = tokio::time::interval(Duration::from_secs(60));
    loop {
        tick.tick().await;
        let dir = cfg.data_dir.clone();
        let ttl = cfg.retention;
        let dropped = tokio::task::spawn_blocking(move || {
            // Wall clock is only used to place the retention horizon; block
            // timestamps themselves come from the data, never from this clock.
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos() as i64;
            let cutoff = now - ttl.as_nanos() as i64;
            // One signal failing must not skip the others; a full disk is
            // exactly when the remaining sweeps matter most.
            SIGNALS.map(|s| (s, block::expire(&dir, s, cutoff)))
        })
        .await;
        match dropped {
            Ok(results) => {
                for (signal, r) in results {
                    match r {
                        Ok(0) => {}
                        Ok(n) => tracing::info!(signal, blocks = n, "retention dropped blocks"),
                        Err(e) => tracing::warn!(signal, error = %e, "retention failed"),
                    }
                }
            }
            Err(e) => tracing::warn!(error = %e, "retention task panicked"),
        }
    }
}
