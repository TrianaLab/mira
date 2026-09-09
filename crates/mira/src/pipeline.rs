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
use tokio::task::JoinHandle;
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
///
/// The `JoinHandle` is the shutdown contract: drop every [`Ingest`] clone and the
/// flusher seals whatever is open, acks everyone waiting on it and returns. A
/// caller that exits without awaiting it turns a graceful stop into a reset for
/// those waiters.
pub fn spawn<B: SignalBuilder>(cfg: Arc<Config>) -> (Ingest<B::Request>, JoinHandle<()>) {
    let (tx, rx) = mpsc::channel(128);
    (Ingest { tx }, tokio::spawn(flusher::<B>(rx, cfg)))
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
            match builder.append_request(&job.req) {
                // An export carrying no records is legal — the Collector emits
                // one whenever a batch empties out — and there is nothing in it
                // to make durable. Parking its caller behind a block that will
                // never be sealed, because nothing was added to seal, strands
                // that caller for as long as it is willing to wait.
                Ok(0) => {
                    let _ = job.ack.send(Ok(()));
                }
                Ok(_) => waiters.push(job.ack),
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
                    let _ = job.ack.send(Err(e.to_string()));
                }
            }
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
            block::publish(&dir, B::SIGNAL, node, this_seq, &sealed).map(|b| b.dir)
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
        let node = cfg.node;
        let swept = tokio::task::spawn_blocking(move || {
            // Wall clock is only used to place the horizons; block timestamps
            // themselves come from the data, never from this clock.
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos() as i64;
            let cutoff = now - ttl.as_nanos() as i64;
            // One signal failing must not skip the others; a full disk is
            // exactly when the remaining sweeps matter most.
            SIGNALS.map(|s| {
                let dropped = block::expire(&dir, s, cutoff);
                // Expire first: compressing a block this sweep is about to
                // delete is pure wasted bandwidth.
                let cold = block::compact(&dir, s, node, now - block::COLD_AFTER_NS);
                (s, dropped, cold)
            })
        })
        .await;
        match swept {
            Ok(results) => {
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
        let (tx, h) = spawn::<LogsBuilder>(c);
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
        let (tx, h) = spawn::<LogsBuilder>(c);
        match tx.submit(wide(70_000)).await {
            Err(Rejected::Failed(e)) => {
                assert!(e.contains("65535") || e.contains("dictionary"), "{e}")
            }
            _ => panic!("70k distinct keys cannot fit a u16 dictionary"),
        }
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
        let (tx, h) = spawn::<LogsBuilder>(c);
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

    /// Shedding is a fast NACK before the decode work, and it has to be
    /// distinguishable from shutdown: `Busy` is retryable and `Closed` is not,
    /// and an exporter that confuses them either drops good data or hammers a
    /// draining node.
    #[tokio::test]
    async fn a_full_queue_sheds_and_a_closed_one_says_so() {
        let (tx, rx) = mpsc::channel::<Job<ExportLogsServiceRequest>>(1);
        let ingest = Ingest { tx };
        let req = || ExportLogsServiceRequest::default();

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

        // The flusher is gone. In flight becomes `Closed` because the ack sender
        // dropped with it; new work becomes `Closed` because the channel did.
        drop(rx);
        assert!(matches!(pending.await.unwrap(), Err(Rejected::Closed)));
        assert!(matches!(ingest.submit(req()).await, Err(Rejected::Closed)));
    }

    /// A data directory that cannot be scanned stops the flusher at startup
    /// rather than at the first flush. Sequence numbers are resumed from what is
    /// on disk, so a pipeline that could not read it would reuse a sequence and
    /// never publish again — failing loudly here is the cheaper end of that.
    #[tokio::test]
    async fn an_unreadable_data_directory_stops_the_flusher_at_startup() {
        let (c, dir) = cfg("unscannable");
        std::fs::write(dir.join("logs"), b"not a directory").unwrap();
        let (tx, h) = spawn::<LogsBuilder>(c);
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
        let (tx, h) = spawn::<LogsBuilder>(Arc::clone(&c));
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
}
