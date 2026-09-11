//! The write-ahead log, which exists to decouple the acknowledgement from the
//! seal.
//!
//! # Why this exists, given `docs/ARCHITECTURE.md` section 4 says "No WAL"
//!
//! section 4's argument is about *recovery*, and it is still correct: a block
//! directory is renamed into place atomically, so there is no torn state and
//! nothing for a log to replay. This log is not for recovery. It is for
//! latency.
//!
//! Before it, an export was acknowledged only after the block *containing it*
//! was durably published, so under light load the caller waited out
//! `max_block_age` — a measured p50 of 657 ms and p99 of 2,647 ms (section 11). The
//! block is sealed on a timer because nothing else bounds how long a
//! half-filled block sits there, and that timer became the caller's latency.
//! With the log in front, the seal triggers are unchanged and nobody is waiting
//! on them: `max_block_age` goes back to being a statement about the shape of
//! blocks on disk rather than a latency bound.
//!
//! # The durability this buys, stated precisely
//!
//! An append is acknowledged once `write(2)` has returned — the bytes are in
//! the kernel's page cache, not on the platter. That survives everything that
//! kills the *process*: a panic under `panic = "abort"`, SIGKILL, the OOM
//! killer, a flusher that took the process down with it (section 1). It does **not**
//! survive power loss or a kernel panic, because the page cache does not.
//!
//! This is a deliberate choice and it is the reason there is no `fsync` on the
//! acknowledgement path. On the machine section 11 was measured on, one
//! `F_FULLFSYNC` costs 4,230 us, so a durably-fsynced ack could not have a p50
//! below 4.2 ms, let alone a p99 under 5 ms. The same trade is Kafka's
//! `acks=1` and ClickHouse's default. [`Wal::sync`] exists and is called on a
//! timer by a background task, which bounds how much is exposed to a power cut
//! to one sync interval — it is never called by an appender.
//!
//! There is deliberately no userspace buffering. A `BufWriter` would batch the
//! syscalls, but bytes sitting in a `Vec` in this process do not survive the
//! process dying, which is exactly the failure this log is claiming to cover.
//! One `write(2)` per export at a few tens of microseconds is affordable: at
//! section 11's measured 545 k records/s in batches of 8,192 that is roughly 65
//! appends a second.
//!
//! # The frame is the OTLP request, in its canonical protobuf encoding
//!
//! Not the bytes off the wire. Mira has three ways in — gRPC, protobuf over
//! HTTP, and KYAML over HTTP — and only one of them still has bytes by the time
//! anything could log them: tonic decodes before the handler is called, and a
//! KYAML body is not protobuf at all. So the frame body is `encode_to_vec` of
//! the decoded request, which normalises all three transports to one format
//! with one decoder on the replay side.
//!
//! That costs a re-encode. Measured on an 8,192-record log export (1.29 MiB):
//! encode 1.49 ms at 864 MiB/s, against the decode already in the path at
//! 5.29 ms and 244 MiB/s. section 11 measured the whole engine at 190.6 MiB/s, a
//! 6.8 ms budget for that export, so the log adds about 22%. The alternative — a
//! custom tonic `Codec` to keep the wire bytes — buys that 22% back for a codec Mira
//! then owns forever, which is the wrong side of principle 1's trade until
//! something measures it as the bottleneck.
//!
//! Replay feeds the decoded frame through the same `ingest::{logs,traces,
//! metrics}` the network path calls, so there is no second decode path to
//! write, to test or to keep in step with the first.
//!
//! ```text
//! ┌────────┬─────┬────────┬─────┬─────────┬────────┬──────────┬────────┐
//! │ magic  │ ver │ signal │ pad │ seq     │ len    │ body     │ crc32  │
//! │ 4 B    │ 2 B │ 1 B    │ 1 B │ 8 B     │ 4 B    │ len B    │ 4 B    │
//! └────────┴─────┴────────┴─────┴─────────┴────────┴──────────┴────────┘
//!  └──────────────── covered by the CRC ──────────────────────┘
//! ```
//!
//! The CRC covers the header as well as the body, so a corrupted length is
//! caught by the checksum rather than by whatever it would otherwise index
//! into. That is the same lesson as the block footer: a length read out of a
//! file is attacker-controlled-equivalent, and validating it against the file's
//! real extent is not optional.
//!
//! # Recovery, and why there is still no manifest
//!
//! Replay needs exactly one fact — which frames are already inside a published
//! block — and the block directory carries it, so principle 4 survives intact.
//! Block names gain a fifth field, `wal_hi`, the highest sequence whose records
//! are in that block. Boot recovery therefore stays what section 4 says it is: a
//! `readdir`, the same one the read path already does, with no extra I/O and no
//! metadata store to keep consistent. Each signal keeps its own watermark, and
//! that costs nothing because an OTLP export belongs to exactly one signal —
//! `/v1/logs`, `/v1/traces` and `/v1/metrics` are three endpoints.
//!
//! # What this breaks, and what pays for it
//!
//! Read-your-writes. section 11 notes it was free, and it was free *because* the ack
//! waited for the publish — the same rename made the data durable and visible
//! at once. A caller acked here and querying immediately would not see its data
//! until the block seals, which is a latency the log was built to remove.
//!
//! The repair is the open-block query surface: [`crate::query::search_open`]
//! takes the flusher's in-progress builder as a snapshot and scans it alongside
//! the sealed blocks, so read-your-writes holds with the log on or off. It is
//! not free — that snapshot is the one allocation the read path makes — and
//! section 7.6 is why it is paid there rather than in the scan.

use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use crate::error::{Error, IoContext, Result};

/// `MIRAWAL0`, truncated. Present so a stray file in the WAL directory is
/// rejected by name rather than parsed as a frame.
const MAGIC: u32 = 0x4d_57_41_4c; // "MWAL"

/// Bumped when the frame layout changes. A reader that does not recognise a
/// version refuses the segment rather than guessing at its shape, which is the
/// same rule the block format follows.
pub const WAL_VERSION: u16 = 1;

const HEADER_LEN: usize = 20;
const CRC_LEN: usize = 4;

/// The largest frame that will be written or believed on read.
///
/// This is not a tuning knob, it is a bound on trust. `len` is four bytes read
/// out of a file that may have been corrupted, and without a ceiling a garbage
/// value asks for an allocation of up to 4 GiB before the CRC that would have
/// caught it has been checked. 64 MiB is four times the default
/// `ingest.max_request_bytes`, so it cannot refuse a frame the ingest path
/// would have accepted.
const MAX_FRAME_BYTES: u32 = 64 << 20;

/// Roll to a new segment past this size. Segments are the unit of deletion, so
/// this trades how much dead log is retained past its watermark against how
/// many files the directory holds. At section 11's measured 190.6 MiB/s a segment
/// is about a third of a second of ingest.
const SEGMENT_BYTES: u64 = 64 << 20;

/// Which signal a frame belongs to.
///
/// Stored as one byte rather than the string the block directories use,
/// because a frame header should be fixed-width — a variable-length field in
/// front of a length field is how a parser gets confused about where the body
/// starts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
#[repr(u8)]
pub enum Signal {
    Logs = 0,
    Traces = 1,
    Metrics = 2,
}

impl Signal {
    /// The three, in the order their watermarks are indexed.
    pub const ALL: [Signal; 3] = [Signal::Logs, Signal::Traces, Signal::Metrics];

    /// The directory name `block::publish` uses for this signal. Kept in step
    /// with the strings the block layer already writes; a mismatch here would
    /// put a watermark on the wrong signal's blocks.
    pub fn as_str(self) -> &'static str {
        match self {
            Signal::Logs => "logs",
            Signal::Traces => "traces",
            Signal::Metrics => "metrics",
        }
    }

    /// The inverse of [`Signal::as_str`], so a caller holding a
    /// [`crate::signal::SignalBuilder`]'s `SIGNAL` does not have to keep a
    /// fourth copy of the mapping in step with the other three.
    pub fn named(s: &str) -> Option<Signal> {
        Signal::ALL.into_iter().find(|sig| sig.as_str() == s)
    }

    fn from_u8(b: u8) -> Option<Signal> {
        match b {
            0 => Some(Signal::Logs),
            1 => Some(Signal::Traces),
            2 => Some(Signal::Metrics),
            _ => None,
        }
    }

    /// Where this signal's watermark sits in [`Watermarks`].
    pub fn index(self) -> usize {
        self as usize
    }
}

/// One past the last published sequence, per signal, indexed by
/// [`Signal::index`]. Assembled from the block directory listing at boot; see
/// the module docs.
///
/// Exclusive rather than inclusive on purpose. An inclusive "highest published
/// sequence" has no value that means *nothing published*: zero is a real
/// sequence, so the empty state and "sequence 0 is durable" are the same
/// number, and the first export after every restart is silently dropped from
/// the replay. Exclusive makes the empty state `0` and needs no sentinel.
pub type Watermarks = [u64; 3];

struct Inner {
    file: File,
    path: PathBuf,
    /// Bytes written to the current segment, tracked rather than `stat`ed so
    /// the roll check costs nothing.
    written: u64,
    /// The next sequence to hand out. Monotonic across segments and restarts.
    next_seq: u64,
    /// Set by `append`, cleared by `sync`. Without it a quiet server syncs an
    /// unchanged file on every tick, which on macOS is a 4 ms barrier bought
    /// for nothing.
    dirty: bool,
    /// Segments that have been rolled past but not yet forced, handed to the
    /// next [`Wal::sync`].
    ///
    /// This list exists because the obvious alternative — syncing the outgoing
    /// segment inside `roll` — puts a 4 ms `F_FULLFSYNC` on one appender in
    /// every `SEGMENT_BYTES`, and `benches/wal_bench.rs` measured exactly that:
    /// at 1 MiB bodies a segment rolls every 64 appends, so the stall landed on
    /// 1.5% of them and the p99 was 7.2 ms against a 5 ms SLA. A rare stall is
    /// still a stall, and a tail latency is made of rare things.
    retired: Vec<(PathBuf, File)>,
}

/// An append-only log of OTLP export bodies.
///
/// Every method is synchronous and at least one of them issues a syscall that
/// can block under writeback pressure, so **callers must not invoke these from
/// a tokio runtime worker** — section 5's rule that blocking work goes through
/// `spawn_blocking` applies here for the same reason it applies to `publish`.
pub struct Wal {
    inner: Mutex<Inner>,
    dir: PathBuf,
    node: u32,
}

impl Wal {
    /// Open (or create) the log under `<root>/.wal/`, resuming the sequence
    /// counter past anything already on disk.
    ///
    /// Resuming from the *files* rather than from the block watermarks is
    /// deliberate: a sequence that went backwards would let a replay confuse a
    /// new frame for one a block already covers, and the watermark comparison
    /// is `>`, so the failure would be silent data loss rather than an error.
    pub fn open(root: &Path, node: u32) -> Result<Self> {
        let dir = root.join(".wal");
        fs::create_dir_all(&dir).ctx(&dir)?;

        let segments = Self::segments(&dir, node)?;
        // The highest sequence actually present, which is not the same as the
        // last segment's first sequence: a segment may be empty if the process
        // died between creating it and its first append.
        let mut next_seq = 0;
        for (path, first) in &segments {
            let mut torn = false;
            for frame in FrameReader::open(path)? {
                // A half-written frame at the tail is what a crash leaves, and
                // it is the case `replay` is built to stop at rather than fail
                // on. `open` has to agree with it: propagating the error here
                // meant the ordinary crash this log exists to survive left a
                // node that would not start at all.
                let Ok(frame) = frame else {
                    torn = true;
                    break;
                };
                next_seq = next_seq.max(frame.seq + 1);
            }
            // Never resume onto the name of a segment that ended torn. Segments
            // are named for their first sequence, so a crash during the *first*
            // frame of a fresh segment leaves a file whose name is exactly the
            // sequence being resumed at — and reopening it would append behind
            // a tear that stops every future replay at it, losing frames that
            // were acknowledged. One skipped sequence number costs nothing:
            // nothing indexes by sequence, and the watermark comparisons are
            // inequalities.
            if torn {
                next_seq = next_seq.max(first + 1);
            }
        }

        let path = dir.join(format!("{node:08x}-{next_seq:020}.wal"));
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .ctx(&path)?;
        let written = file.metadata().ctx(&path)?.len();

        Ok(Wal {
            inner: Mutex::new(Inner {
                file,
                path,
                written,
                next_seq,
                dirty: false,
                retired: Vec::new(),
            }),
            dir,
            node,
        })
    }

    /// Append one OTLP export body and return the sequence it was given.
    ///
    /// Returns once the bytes are in the page cache. This is the call the
    /// acknowledgement waits on, and it does not fsync — see the module docs
    /// for exactly what that does and does not survive.
    pub fn append(&self, signal: Signal, body: &[u8]) -> Result<u64> {
        self.append_then(signal, body, |_| {})
    }

    /// [`append`](Self::append), running `then` on the new sequence before the
    /// log's lock is released.
    ///
    /// This exists to make one specific race impossible, and it is not a
    /// general-purpose hook.
    ///
    /// A block claims the sequences it contains by publishing one past the
    /// highest of them, and [`crate::block::wal_watermarks`] takes the maximum
    /// over the blocks on disk. That is only a correct watermark if a signal's
    /// frames reach their blocks in sequence order: if frame 5 is still in
    /// flight when the block holding frame 6 is published, the watermark says
    /// 7 and frame 5 is skipped on the next replay — which is silent loss, the
    /// one failure this log exists to prevent.
    ///
    /// Two exporters calling `append` concurrently get their sequences in lock
    /// order but can be preempted between the return and the enqueue, so the
    /// enqueue has to happen under the same lock. It costs nothing: the queue
    /// hand-off is a pointer move into a reserved slot, next to a `write(2)`
    /// that has already been paid for.
    ///
    /// `then` therefore must not block, must not `.await`, and must not touch
    /// this log — re-entering `append` from inside it deadlocks.
    pub fn append_then(&self, signal: Signal, body: &[u8], then: impl FnOnce(u64)) -> Result<u64> {
        let len = u32::try_from(body.len())
            .ok()
            .filter(|n| *n <= MAX_FRAME_BYTES);
        let Some(len) = len else {
            return Err(Error::WalFrameTooLarge {
                len: body.len(),
                max: MAX_FRAME_BYTES,
            });
        };

        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());

        if inner.written >= SEGMENT_BYTES {
            self.roll(&mut inner)?;
        }

        let seq = inner.next_seq;
        let mut header = [0u8; HEADER_LEN];
        header[0..4].copy_from_slice(&MAGIC.to_le_bytes());
        header[4..6].copy_from_slice(&WAL_VERSION.to_le_bytes());
        header[6] = signal as u8;
        header[7] = 0;
        header[8..16].copy_from_slice(&seq.to_le_bytes());
        header[16..20].copy_from_slice(&len.to_le_bytes());

        let mut hasher = crc32fast::Hasher::new();
        hasher.update(&header);
        hasher.update(body);
        let crc = hasher.finalize();

        // One `write_all` per region rather than one buffer built by
        // concatenation: the body can be 16 MiB and copying it to prepend
        // twenty bytes would double the memcpy the ingest path is already
        // trying not to pay twice.
        //
        // A short write partway through leaves a torn frame at the tail, which
        // is the case `FrameReader` is built to stop at. It cannot corrupt a
        // frame that was already complete, because the file is opened in
        // append mode and nothing rewrites what is behind the offset.
        inner.file.write_all(&header).ctx(&inner.path)?;
        inner.file.write_all(body).ctx(&inner.path)?;
        inner.file.write_all(&crc.to_le_bytes()).ctx(&inner.path)?;

        inner.written += (HEADER_LEN + body.len() + CRC_LEN) as u64;
        inner.next_seq += 1;
        inner.dirty = true;
        then(seq);
        Ok(seq)
    }

    /// Force everything appended so far onto the device.
    ///
    /// Called on a timer by a background task to bound power-loss exposure,
    /// never from the acknowledgement path. On Apple targets this is
    /// `F_FULLFSYNC` and costs about 4 ms, which is the whole reason it is not
    /// on the ack path.
    pub fn sync(&self) -> Result<()> {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        if !inner.dirty && inner.retired.is_empty() {
            return Ok(());
        }
        // Retired segments first: they are older, so they are what a power cut
        // would lose the most of. Taken out of the struct rather than iterated
        // in place so a failure part-way through does not re-sync the ones that
        // already succeeded on the next tick.
        for (path, file) in std::mem::take(&mut inner.retired) {
            crate::sync_data(&file).ctx(&path)?;
        }
        if inner.dirty {
            crate::sync_data(&inner.file).ctx(&inner.path)?;
            inner.dirty = false;
        }
        Ok(())
    }

    /// The sequence that will be handed to the next append.
    pub fn next_seq(&self) -> u64 {
        self.inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .next_seq
    }

    /// Delete whole segments whose every frame is below `covered`, the
    /// smallest of the per-signal [`Watermarks`] — same exclusive convention.
    ///
    /// Returns how many segments were removed. Deletion is per segment rather
    /// than per frame because a log is only append-only if nothing ever
    /// rewrites its middle; reclaiming a prefix by truncation would mean
    /// rewriting offsets that a concurrent reader is part-way through.
    ///
    /// The current segment is never removed, whatever its watermark, because
    /// `append` holds it open and unlinking it would leave writes going to a
    /// file with no name.
    pub fn truncate(&self, covered: u64) -> Result<usize> {
        let current = {
            let inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
            inner.path.clone()
        };
        let segments = Self::segments(&self.dir, self.node)?;
        let mut removed = 0;

        for (path, _) in &segments {
            if *path == current {
                continue;
            }
            // The last frame decides, not the first: a segment is only dead
            // once everything in it is covered.
            let mut highest = None;
            for frame in FrameReader::open(path)? {
                highest = Some(frame?.seq);
            }
            match highest {
                // An empty segment is a crash artefact between create and
                // first append; nothing references it and it will never be
                // written to again, so it goes.
                None => {}
                Some(hi) if hi < covered => {}
                Some(_) => continue,
            }
            fs::remove_file(path).ctx(path)?;
            // Drop the retired handle too. Unlinking a file this process still
            // has open is legal on Unix, but the inode — and its blocks —
            // survive until the last descriptor closes, so leaving it there
            // means `truncate` reports space it has not actually freed.
            {
                let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
                inner.retired.retain(|(p, _)| p != path);
            }
            removed += 1;
        }

        if removed > 0 {
            // The unlinks are metadata on the WAL directory, and until that is
            // flushed a crash brings the deleted segments back and replays
            // frames a block already covers. The watermark makes that safe
            // rather than wrong, but replaying gigabytes at every boot is its
            // own outage.
            crate::sync_all(&File::open(&self.dir).ctx(&self.dir)?).ctx(&self.dir)?;
        }
        Ok(removed)
    }

    /// Replay every frame not yet covered by a published block, oldest first.
    ///
    /// `watermarks` is one past the last published sequence per signal, taken
    /// from the block directory listing. A frame is handed to `f` only if its
    /// sequence is at or above its own signal's watermark, so a block that
    /// sealed while another signal's was still open does not cause a
    /// re-ingest.
    ///
    /// A torn or corrupt frame ends the replay of that segment rather than
    /// failing it: the tail of the last segment is exactly where a crash
    /// leaves a half-written frame, and refusing to start because the last
    /// write was interrupted would turn a normal crash into an outage. Frames
    /// before the tear are complete and are replayed.
    ///
    /// `f` is handed the frame's own sequence, not a fresh one. Re-appending a
    /// replayed frame would give it a number above every watermark, so the
    /// block that stored it would claim the new sequence and leave the old one
    /// uncovered — and the next boot would replay it again, forever. Carrying
    /// the original through to the block is what makes replay converge.
    pub fn replay(
        root: &Path,
        node: u32,
        watermarks: Watermarks,
        mut f: impl FnMut(Signal, u64, &[u8]) -> Result<()>,
    ) -> Result<Replayed> {
        let dir = root.join(".wal");
        if !dir.is_dir() {
            return Ok(Replayed::default());
        }
        let mut out = Replayed::default();

        for (path, _) in Self::segments(&dir, node)? {
            for frame in FrameReader::open(&path)? {
                let Ok(frame) = frame else {
                    out.torn_segments += 1;
                    break;
                };
                if frame.seq < watermarks[frame.signal.index()] {
                    out.skipped += 1;
                    continue;
                }
                f(frame.signal, frame.seq, &frame.body)?;
                out.replayed += 1;
                out.bytes += frame.body.len() as u64;
            }
        }
        Ok(out)
    }

    /// Segments for this node, oldest first.
    ///
    /// Ordered by the first sequence in the name rather than by mtime, because
    /// mtime has a one-second resolution on some filesystems and two segments
    /// can share it. Other nodes' segments are skipped: a shared volume (section 12)
    /// has one log per replica and replaying another's would double-write its
    /// data.
    fn segments(dir: &Path, node: u32) -> Result<Vec<(PathBuf, u64)>> {
        let prefix = format!("{node:08x}-");
        let mut out = Vec::new();
        let entries = match fs::read_dir(dir) {
            Ok(entries) => entries,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(out),
            Err(e) => {
                return Err(Error::Io {
                    path: dir.into(),
                    source: e,
                });
            }
        };
        for entry in entries {
            let entry = entry.ctx(dir)?;
            let name = entry.file_name();
            let Some(name) = name.to_str() else { continue };
            let Some(rest) = name.strip_prefix(&prefix) else {
                continue;
            };
            let Some(first) = rest.strip_suffix(".wal") else {
                continue;
            };
            let Ok(first) = first.parse::<u64>() else {
                continue;
            };
            out.push((entry.path(), first));
        }
        out.sort_by_key(|(_, first)| *first);
        Ok(out)
    }

    /// Close the current segment and start the next one.
    ///
    /// Nothing is forced here — the outgoing segment goes on `retired` for the
    /// background [`Wal::sync`] to deal with. See that field for the measured
    /// reason. Opening the new file is two syscalls and does not touch the
    /// device, so the appender that happens to trigger a roll pays microseconds
    /// rather than milliseconds.
    fn roll(&self, inner: &mut Inner) -> Result<()> {
        let path = self
            .dir
            .join(format!("{:08x}-{:020}.wal", self.node, inner.next_seq));
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .ctx(&path)?;
        let old_file = std::mem::replace(&mut inner.file, file);
        let old_path = std::mem::replace(&mut inner.path, path);
        if inner.dirty {
            inner.retired.push((old_path, old_file));
            inner.dirty = false;
        }
        inner.written = 0;
        Ok(())
    }
}

/// What a [`Wal::replay`] did, for the line it gets logged on.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Replayed {
    /// Frames handed to the callback.
    pub replayed: u64,
    /// Frames already covered by a published block.
    pub skipped: u64,
    /// Body bytes replayed.
    pub bytes: u64,
    /// Segments that ended in a torn or corrupt frame. One is normal after a
    /// crash — it is the write that was in flight. More than one means
    /// something else is wrong, which is why they are counted separately from
    /// the frames rather than folded in.
    pub torn_segments: u64,
}

#[cfg_attr(test, derive(Debug))]
struct Frame {
    signal: Signal,
    seq: u64,
    body: Vec<u8>,
}

/// Reads frames out of one segment, stopping at the first that is not whole.
struct FrameReader {
    file: File,
    path: PathBuf,
    done: bool,
}

impl FrameReader {
    fn open(path: &Path) -> Result<FrameReader> {
        Ok(FrameReader {
            file: File::open(path).ctx(path)?,
            path: path.to_path_buf(),
            done: false,
        })
    }

    /// `Ok(None)` is a clean end of segment; `Err` is a tear or corruption,
    /// after which this reader yields nothing further.
    fn next_frame(&mut self) -> Result<Option<Frame>> {
        let mut header = [0u8; HEADER_LEN];
        if !read_exact_or_eof(&mut self.file, &mut header).ctx(&self.path)? {
            return Ok(None);
        }

        let magic = u32::from_le_bytes(header[0..4].try_into().unwrap_or_default());
        let version = u16::from_le_bytes(header[4..6].try_into().unwrap_or_default());
        let len = u32::from_le_bytes(header[16..20].try_into().unwrap_or_default());

        if magic != MAGIC {
            return Err(Error::WalCorrupt {
                path: self.path.clone(),
                why: "bad frame magic",
            });
        }
        if version != WAL_VERSION {
            return Err(Error::WalVersion {
                path: self.path.clone(),
                found: version,
                expected: WAL_VERSION,
            });
        }
        // Checked before the allocation, not after: the CRC that would catch a
        // corrupt length is at the far end of the body this length describes.
        if len > MAX_FRAME_BYTES {
            return Err(Error::WalCorrupt {
                path: self.path.clone(),
                why: "frame length above the maximum",
            });
        }
        let Some(signal) = Signal::from_u8(header[6]) else {
            return Err(Error::WalCorrupt {
                path: self.path.clone(),
                why: "unknown signal in frame header",
            });
        };

        let mut body = vec![0u8; len as usize];
        if !read_exact_or_eof(&mut self.file, &mut body).ctx(&self.path)? {
            return Err(Error::WalCorrupt {
                path: self.path.clone(),
                why: "truncated frame body",
            });
        }
        let mut crc_bytes = [0u8; CRC_LEN];
        if !read_exact_or_eof(&mut self.file, &mut crc_bytes).ctx(&self.path)? {
            return Err(Error::WalCorrupt {
                path: self.path.clone(),
                why: "truncated frame checksum",
            });
        }

        let mut hasher = crc32fast::Hasher::new();
        hasher.update(&header);
        hasher.update(&body);
        if hasher.finalize() != u32::from_le_bytes(crc_bytes) {
            return Err(Error::WalCorrupt {
                path: self.path.clone(),
                why: "frame checksum mismatch",
            });
        }

        Ok(Some(Frame {
            signal,
            seq: u64::from_le_bytes(header[8..16].try_into().unwrap_or_default()),
            body,
        }))
    }
}

impl Iterator for FrameReader {
    type Item = Result<Frame>;

    fn next(&mut self) -> Option<Result<Frame>> {
        if self.done {
            return None;
        }
        match self.next_frame() {
            Ok(Some(frame)) => Some(Ok(frame)),
            Ok(None) => {
                self.done = true;
                None
            }
            Err(e) => {
                self.done = true;
                Some(Err(e))
            }
        }
    }
}

/// `true` if the buffer was filled, `false` on a clean EOF before any byte.
///
/// `Read::read_exact` cannot distinguish "the segment ends here" from "the
/// segment ends in the middle of a frame", and those are a normal end and a
/// tear respectively.
fn read_exact_or_eof(file: &mut impl Read, buf: &mut [u8]) -> std::io::Result<bool> {
    let mut filled = 0;
    while filled < buf.len() {
        match file.read(&mut buf[filled..]) {
            Ok(0) => return Ok(false),
            Ok(n) => filled += n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmpdir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("mira-wal-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn collect(root: &Path, node: u32, wm: Watermarks) -> (Vec<(Signal, Vec<u8>)>, Replayed) {
        let (got, _, stats) = collect_seqs(root, node, wm);
        (got, stats)
    }

    #[allow(clippy::type_complexity)]
    fn collect_seqs(
        root: &Path,
        node: u32,
        wm: Watermarks,
    ) -> (Vec<(Signal, Vec<u8>)>, Vec<u64>, Replayed) {
        let (mut got, mut seqs) = (Vec::new(), Vec::new());
        let stats = Wal::replay(root, node, wm, |s, seq, b| {
            got.push((s, b.to_vec()));
            seqs.push(seq);
            Ok(())
        })
        .unwrap();
        (got, seqs, stats)
    }

    #[test]
    fn a_frame_round_trips_through_replay() {
        let root = tmpdir("roundtrip");
        let wal = Wal::open(&root, 0xab).unwrap();
        assert_eq!(wal.append(Signal::Logs, b"one").unwrap(), 0);
        assert_eq!(wal.append(Signal::Traces, b"two").unwrap(), 1);
        assert_eq!(wal.append(Signal::Metrics, b"three").unwrap(), 2);

        let (got, stats) = collect(&root, 0xab, [0, 0, 0]);
        // The regression this pins: sequence 0 is a real frame and watermark 0
        // means nothing is published, so an inclusive comparison drops the
        // first export of every restart. It did, until the watermark was made
        // exclusive.
        assert_eq!(stats.replayed, 3);
        assert_eq!(got[0], (Signal::Logs, b"one".to_vec()));
        assert_eq!(got[2], (Signal::Metrics, b"three".to_vec()));
    }

    #[test]
    fn a_published_block_is_not_replayed_and_each_signal_counts_separately() {
        let root = tmpdir("watermark");
        let wal = Wal::open(&root, 1).unwrap();
        wal.append(Signal::Logs, b"l0").unwrap(); // seq 0
        wal.append(Signal::Traces, b"t1").unwrap(); // seq 1
        wal.append(Signal::Logs, b"l2").unwrap(); // seq 2
        wal.append(Signal::Traces, b"t3").unwrap(); // seq 3

        // Logs sealed through seq 2 inclusive, traces only through seq 1. Only
        // the traces frame above its own watermark comes back — a logs seal
        // must not suppress an unsealed trace.
        let mut wm = [0u64; 3];
        wm[Signal::Logs.index()] = 3;
        wm[Signal::Traces.index()] = 2;
        let (got, stats) = collect(&root, 1, wm);
        assert_eq!(stats.replayed, 1);
        assert_eq!(stats.skipped, 3);
        assert_eq!(got, vec![(Signal::Traces, b"t3".to_vec())]);
    }

    #[test]
    fn a_torn_tail_ends_the_segment_without_losing_what_came_before() {
        let root = tmpdir("torn");
        let wal = Wal::open(&root, 2).unwrap();
        wal.append(Signal::Logs, b"complete").unwrap();
        wal.append(Signal::Logs, b"also-complete").unwrap();
        wal.sync().unwrap();
        let path = {
            let inner = wal.inner.lock().unwrap();
            inner.path.clone()
        };
        drop(wal);

        // Chop the last four bytes: the second frame now has no checksum,
        // which is exactly what a crash mid-write leaves behind.
        let len = fs::metadata(&path).unwrap().len();
        OpenOptions::new()
            .write(true)
            .open(&path)
            .unwrap()
            .set_len(len - 4)
            .unwrap();

        let (got, stats) = collect(&root, 2, [0, 0, 0]);
        assert_eq!(
            stats.replayed, 1,
            "the whole frame before the tear survives"
        );
        assert_eq!(stats.torn_segments, 1);
        assert_eq!(got, vec![(Signal::Logs, b"complete".to_vec())]);
    }

    #[test]
    fn a_flipped_bit_in_the_body_is_caught_by_the_checksum() {
        let root = tmpdir("bitrot");
        let wal = Wal::open(&root, 3).unwrap();
        wal.append(Signal::Logs, b"the-quick-brown-fox").unwrap();
        wal.sync().unwrap();
        let path = {
            let inner = wal.inner.lock().unwrap();
            inner.path.clone()
        };
        drop(wal);

        let mut bytes = fs::read(&path).unwrap();
        bytes[HEADER_LEN + 3] ^= 0x40;
        fs::write(&path, &bytes).unwrap();

        let (got, stats) = collect(&root, 3, [0, 0, 0]);
        assert!(
            got.is_empty(),
            "a corrupt frame is never handed to the callback"
        );
        assert_eq!(stats.torn_segments, 1);
    }

    #[test]
    fn a_corrupt_length_is_refused_before_it_is_allocated() {
        let root = tmpdir("badlen");
        let wal = Wal::open(&root, 4).unwrap();
        wal.append(Signal::Logs, b"small").unwrap();
        wal.sync().unwrap();
        let path = {
            let inner = wal.inner.lock().unwrap();
            inner.path.clone()
        };
        drop(wal);

        // A length field of 4 GiB. Without the MAX_FRAME_BYTES check this is a
        // 4 GiB allocation on a machine that may not have it, from four bytes
        // on disk — the same shape as the block-footer bug.
        let mut bytes = fs::read(&path).unwrap();
        bytes[16..20].copy_from_slice(&u32::MAX.to_le_bytes());
        fs::write(&path, &bytes).unwrap();

        let mut reader = FrameReader::open(&path).unwrap();
        let err = reader.next().unwrap().unwrap_err();
        assert!(
            matches!(&err, Error::WalCorrupt { why, .. } if why.contains("length")),
            "got {err:?}"
        );
    }

    #[test]
    fn a_frame_larger_than_the_maximum_is_refused_on_append() {
        let root = tmpdir("toobig");
        let wal = Wal::open(&root, 5).unwrap();
        let huge = vec![0u8; MAX_FRAME_BYTES as usize + 1];
        assert!(matches!(
            wal.append(Signal::Logs, &huge),
            Err(Error::WalFrameTooLarge { .. })
        ));
    }

    #[test]
    fn the_sequence_resumes_past_everything_on_disk_after_a_restart() {
        let root = tmpdir("resume");
        let wal = Wal::open(&root, 6).unwrap();
        wal.append(Signal::Logs, b"a").unwrap();
        wal.append(Signal::Logs, b"b").unwrap();
        wal.sync().unwrap();
        drop(wal);

        // A sequence that restarted at 0 would collide with the frames already
        // there, and the watermark comparison would then skip new data as if
        // it were already published.
        let wal = Wal::open(&root, 6).unwrap();
        assert_eq!(wal.next_seq(), 2);
        assert_eq!(wal.append(Signal::Logs, b"c").unwrap(), 2);

        let (got, _) = collect(&root, 6, [0, 0, 0]);
        assert_eq!(got.len(), 3);
    }

    /// Replay hands back the sequence the frame already has, and `append_then`
    /// hands back the one it just assigned. Both are the same requirement seen
    /// from the two ends: a replayed export has to reach its block under its
    /// original number, or the block claims a copy, leaves the original
    /// uncovered, and the next boot replays it again — for ever.
    #[test]
    fn replay_carries_the_original_sequence_and_append_reports_it_under_the_lock() {
        let root = tmpdir("seqs");
        let wal = Wal::open(&root, 3).unwrap();
        let mut seen = Vec::new();
        for body in [b"a", b"b", b"c"] {
            wal.append_then(Signal::Traces, body, |seq| seen.push(seq))
                .unwrap();
        }
        assert_eq!(seen, [0, 1, 2]);

        // Not 0..n of whatever survived the watermark: the first frame is
        // already published, so the two that replay keep 1 and 2.
        let mut wm = [0u64; 3];
        wm[Signal::Traces.index()] = 1;
        let (got, seqs, stats) = collect_seqs(&root, 3, wm);
        assert_eq!(seqs, [1, 2]);
        assert_eq!(got.len(), 2);
        assert_eq!(stats.skipped, 1);
    }

    #[test]
    fn another_nodes_segments_are_left_alone() {
        let root = tmpdir("twonodes");
        let a = Wal::open(&root, 0x11).unwrap();
        let b = Wal::open(&root, 0x22).unwrap();
        a.append(Signal::Logs, b"from-a").unwrap();
        b.append(Signal::Logs, b"from-b").unwrap();
        a.sync().unwrap();
        b.sync().unwrap();

        // Two replicas sharing a volume (section 12) each replay only their own log.
        // Replaying the other's would double-write data it already acked.
        let (got, _) = collect(&root, 0x11, [0, 0, 0]);
        assert_eq!(got, vec![(Signal::Logs, b"from-a".to_vec())]);
    }

    #[test]
    fn truncate_removes_covered_segments_and_never_the_open_one() {
        let root = tmpdir("truncate");
        let wal = Wal::open(&root, 7).unwrap();
        wal.append(Signal::Logs, b"old").unwrap();
        {
            // Force a roll without writing 64 MiB.
            let mut inner = wal.inner.lock().unwrap();
            inner.written = SEGMENT_BYTES;
        }
        wal.append(Signal::Logs, b"new").unwrap();
        assert_eq!(Wal::segments(&wal.dir, 7).unwrap().len(), 2);

        // Watermark covers seq 0, the first segment's only frame.
        assert_eq!(wal.truncate(1).unwrap(), 1);
        let segments = Wal::segments(&wal.dir, 7).unwrap();
        assert_eq!(segments.len(), 1, "the open segment is never unlinked");

        let (got, _) = collect(&root, 7, [0, 0, 0]);
        assert_eq!(got, vec![(Signal::Logs, b"new".to_vec())]);
    }

    #[test]
    fn truncate_keeps_a_segment_whose_tail_is_still_uncovered() {
        let root = tmpdir("truncate-partial");
        let wal = Wal::open(&root, 8).unwrap();
        wal.append(Signal::Logs, b"covered").unwrap(); // seq 0
        wal.append(Signal::Logs, b"not-yet").unwrap(); // seq 1
        {
            let mut inner = wal.inner.lock().unwrap();
            inner.written = SEGMENT_BYTES;
        }
        wal.append(Signal::Logs, b"newest").unwrap(); // seq 2, new segment

        // The last frame decides. Dropping the first segment at a watermark of
        // 1 would lose seq 1, which no block covers yet.
        assert_eq!(wal.truncate(1).unwrap(), 0);
        assert_eq!(wal.truncate(2).unwrap(), 1);
    }

    #[test]
    fn a_wrong_frame_header_is_named_by_what_is_wrong_with_it() {
        // The header fields, one test, because the property is the set: a
        // reader that says "corrupt" for all of them is a reader nobody can
        // debug a real crash with. The body's two failures — a length above the
        // maximum, a checksum that does not match — have their own tests above,
        // since what matters about those is *when* they are checked.
        let root = tmpdir("corrupt-frames");
        let wal = Wal::open(&root, 0x11).unwrap();
        wal.append(Signal::Logs, b"body").unwrap();
        wal.sync().unwrap();
        let good = fs::read(root.join(".wal").join("00000011-00000000000000000000.wal")).unwrap();
        assert_eq!(good.len(), HEADER_LEN + 4 + CRC_LEN);

        let broken = |f: &dyn Fn(&mut Vec<u8>)| -> Error {
            let mut bytes = good.clone();
            f(&mut bytes);
            let path = root.join("mangled.wal");
            fs::write(&path, &bytes).unwrap();
            let mut reader = FrameReader::open(&path).unwrap();
            let e = reader.next().unwrap().unwrap_err();
            // A reader that has yielded an error is finished: the bytes after a
            // frame it could not measure are not frames.
            assert!(reader.next().is_none());
            e
        };
        // `matches!` and not a `match` with a panicking arm: the arm that says
        // "that was not a corrupt frame at all" is a line no passing run
        // executes, and the assertion message carries the same information.
        let corrupt = |f: &dyn Fn(&mut Vec<u8>), want: &str| {
            let e = broken(f);
            assert!(
                matches!(&e, Error::WalCorrupt { why, .. } if *why == want),
                "expected {want:?}, got {e}"
            );
        };

        corrupt(&|b| b[0] ^= 0xff, "bad frame magic");
        corrupt(&|b| b[6] = 0xfe, "unknown signal in frame header");
        corrupt(&|b| b.truncate(HEADER_LEN + 1), "truncated frame body");

        // The version is the one that is not a `WalCorrupt`: a segment written
        // by a future Mira is intact, and saying "corrupt" about it would send
        // whoever downgraded looking for a disk fault.
        let e = broken(&|b| b[4..6].copy_from_slice(&(WAL_VERSION + 7).to_le_bytes()));
        assert!(
            matches!(&e, Error::WalVersion { found, expected, .. }
                if (*found, *expected) == (WAL_VERSION + 7, WAL_VERSION)),
            "expected a version error naming both sides, got {e}"
        );
    }

    #[test]
    fn replaying_an_absent_directory_is_not_an_error() {
        let root = tmpdir("empty");
        let (got, stats) = collect(&root, 9, [0, 0, 0]);
        assert!(got.is_empty());
        assert_eq!(stats, Replayed::default());
    }

    /// Bytes for a frame whose header promises more body than follows it —
    /// exactly what a crash between the header `write_all` and the body one
    /// leaves at the tail of a segment. Hand-built rather than produced by
    /// chopping a real segment, because a *first* frame has to be torn to reach
    /// the case where the segment's name is the sequence being resumed at.
    fn torn_frame_bytes(seq: u64) -> Vec<u8> {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&MAGIC.to_le_bytes());
        bytes.extend_from_slice(&WAL_VERSION.to_le_bytes());
        bytes.push(Signal::Logs as u8);
        bytes.push(0); // pad
        bytes.extend_from_slice(&seq.to_le_bytes());
        bytes.extend_from_slice(&64u32.to_le_bytes()); // promises 64 body bytes
        assert_eq!(bytes.len(), HEADER_LEN);
        bytes.extend_from_slice(b"only-a-few"); // ... and delivers ten
        bytes
    }

    /// A crash mid-`write(2)` leaves half a frame at the tail of a segment, and
    /// the log has to *open* over it: refusing turns the ordinary crash this
    /// log exists to survive into a node that will not boot at all. What must
    /// not happen either is resuming onto the torn segment's own name —
    /// everything appended behind a tear is invisible to every later replay,
    /// which is the silent loss of acknowledged data the log exists to prevent.
    #[test]
    fn a_torn_tail_does_not_stop_the_log_opening_or_get_appended_behind() {
        // The tail of a segment that holds a whole frame before the tear.
        let root = tmpdir("open-torn-tail");
        let wal = Wal::open(&root, 0x4a).unwrap();
        wal.append(Signal::Logs, b"kept").unwrap();
        wal.append(Signal::Logs, b"in-flight").unwrap();
        wal.sync().unwrap();
        let path = {
            let inner = wal.inner.lock().unwrap();
            inner.path.clone()
        };
        drop(wal);
        let len = fs::metadata(&path).unwrap().len();
        OpenOptions::new()
            .write(true)
            .open(&path)
            .unwrap()
            .set_len(len - 4)
            .unwrap();

        let wal = Wal::open(&root, 0x4a).expect("a torn tail is a boot condition, not an error");
        assert_eq!(wal.append(Signal::Logs, b"after").unwrap(), 1);
        wal.sync().unwrap();
        let (got, stats) = collect(&root, 0x4a, [0, 0, 0]);
        assert_eq!(
            got,
            vec![
                (Signal::Logs, b"kept".to_vec()),
                (Signal::Logs, b"after".to_vec())
            ],
            "the frame before the tear and the one after the restart both replay"
        );
        assert_eq!(stats.torn_segments, 1);

        // The harder shape: the crash was during the very first frame of a
        // fresh segment, so the segment's name *is* the sequence a naive resume
        // would pick, and the new frame would be written behind the tear.
        let root = tmpdir("open-torn-head");
        let wal = Wal::open(&root, 0x4b).unwrap();
        wal.append(Signal::Logs, b"kept").unwrap(); // seq 0, segment ...000
        wal.sync().unwrap();
        drop(wal);
        let head = root
            .join(".wal")
            .join(format!("{:08x}-{:020}.wal", 0x4b, 1));
        fs::write(&head, torn_frame_bytes(1)).unwrap();

        let wal = Wal::open(&root, 0x4b).unwrap();
        assert_eq!(
            wal.next_seq(),
            2,
            "the torn segment's own name is not reused"
        );
        wal.append(Signal::Logs, b"after").unwrap();
        wal.sync().unwrap();
        let (got, stats) = collect(&root, 0x4b, [0, 0, 0]);
        assert_eq!(
            got,
            vec![
                (Signal::Logs, b"kept".to_vec()),
                (Signal::Logs, b"after".to_vec())
            ],
            "a frame acked after the restart is still replayable"
        );
        assert_eq!(stats.torn_segments, 1, "the torn segment is still counted");
    }

    /// A rolled segment is handed to the next `sync` instead of being forced by
    /// the appender that rolled it (see `Inner::retired` for the measured
    /// reason). If `sync` did not pick it up, a node that goes quiet after a
    /// roll would leave a whole 64 MiB segment exposed to a power cut for ever
    /// — the roll would have turned the timer off for the data it retired.
    #[test]
    fn the_next_sync_forces_a_segment_the_appender_rolled_away() {
        let root = tmpdir("retired");
        let wal = Wal::open(&root, 0x5a).unwrap();
        wal.append(Signal::Logs, b"in the outgoing segment")
            .unwrap();
        {
            // Rolled by hand: the alternative is 64 MiB of appends, and what is
            // under test is the hand-off, not the size trigger.
            let mut inner = wal.inner.lock().unwrap();
            wal.roll(&mut inner).unwrap();
            assert_eq!(inner.retired.len(), 1, "queued for the timer, not forced");
            assert!(!inner.dirty, "and the new segment has nothing in it");
        }

        wal.sync().unwrap();
        {
            let inner = wal.inner.lock().unwrap();
            assert!(
                inner.retired.is_empty(),
                "taken, so a later tick does not pay for the same barrier again"
            );
        }
        // A quiet log stays quiet: with nothing dirty and nothing retired the
        // next tick must not issue a barrier at all, which is what makes the
        // 250 ms timer free on an idle node.
        wal.sync().unwrap();

        let (got, _) = collect(&root, 0x5a, [0, 0, 0]);
        assert_eq!(
            got,
            vec![(Signal::Logs, b"in the outgoing segment".to_vec())],
            "rolling is not a truncation"
        );
    }

    /// A segment created and never appended to is what a crash between `roll`
    /// and the first append leaves. It has no highest sequence, so a `truncate`
    /// that only deleted segments *below* the watermark would keep it for ever
    /// and leak one file per crash into the directory every boot scans.
    #[test]
    fn truncate_drops_the_empty_segment_a_crash_left_behind() {
        let root = tmpdir("empty-segment");
        let wal = Wal::open(&root, 0x6b).unwrap();
        wal.append(Signal::Logs, b"live").unwrap();
        let stale = root
            .join(".wal")
            .join(format!("{:08x}-{:020}.wal", 0x6b, 7));
        File::create(&stale).unwrap();
        assert_eq!(Wal::segments(&wal.dir, 0x6b).unwrap().len(), 2);

        // Watermark 0: nothing at all is published, so the only reason this
        // file can go is that it holds no frames.
        assert_eq!(wal.truncate(0).unwrap(), 1);
        assert!(!stale.exists());
        let (got, _) = collect(&root, 0x6b, [0, 0, 0]);
        assert_eq!(
            got,
            vec![(Signal::Logs, b"live".to_vec())],
            "the open segment is untouched"
        );
    }

    /// The WAL directory is a directory on somebody's volume: it collects
    /// half-renamed files, another replica's segments and whatever a backup
    /// tool drops. Each one mistaken for a segment is a spurious replay or a
    /// boot failure, so the filter is a correctness property and not tidiness
    /// — and the two ways `read_dir` fails have to stay distinguishable,
    /// because absent is a fresh volume and unreadable is a mount to shout
    /// about. Answering "no segments" for the second would skip the replay and
    /// silently drop everything the log was holding.
    #[test]
    fn segments_lists_only_this_nodes_well_formed_segments() {
        let root = tmpdir("listing");
        let wal = Wal::open(&root, 0x7c).unwrap();
        wal.append(Signal::Logs, b"real").unwrap();
        let dir = root.join(".wal");
        for junk in [
            "0000007c-00000000000000000009.log", // right shape, wrong suffix
            "0000007c-not-a-number.wal",         // suffix, but no sequence
            "0000007c-.wal",                     // empty sequence
            "readme.txt",                        // not ours in any way
            "0000007d-00000000000000000000.wal", // the other replica's
        ] {
            File::create(dir.join(junk)).unwrap();
        }

        let listed = Wal::segments(&dir, 0x7c).unwrap();
        assert_eq!(listed.len(), 1, "only the real segment, got {listed:?}");
        assert_eq!(listed[0].1, 0);

        // A directory that is not there yet is the first boot on a fresh
        // volume, and has to read as empty rather than as an error.
        assert!(
            Wal::segments(&root.join("never-created"), 0x7c)
                .unwrap()
                .is_empty()
        );
        // One that cannot be listed is a different thing and must say so.
        let notdir = root.join("a-file-not-a-dir");
        fs::write(&notdir, b"x").unwrap();
        assert!(matches!(
            Wal::segments(&notdir, 0x7c),
            Err(Error::Io { .. })
        ));
    }

    /// A `read(2)` that fails is not the end of a segment. The two are one
    /// return value apart in `FrameReader` and a world apart in meaning: an
    /// EIO mistaken for a clean end would silently drop every frame behind it
    /// and let `truncate` delete the segment as if it had all been published.
    /// The error also has to name the file, because the operator's next move
    /// is to go and look at that one inode.
    #[test]
    fn a_failed_read_is_never_mistaken_for_the_end_of_a_segment() {
        let root = tmpdir("read-error");
        // Opening a directory read-only succeeds on Unix and the first `read`
        // on it fails with EISDIR: a descriptor whose reads really do fail,
        // driven through the real reader, with no fault injection to arrange.
        // ponytail: this reaches the header read only. The body and checksum
        // reads want a descriptor that succeeds for 20 bytes and then fails,
        // which needs a FUSE mount or an injected `Read` — worth it only if
        // those two lines ever diverge from this one.
        let notafile = root.join("a-directory");
        fs::create_dir_all(&notafile).unwrap();
        let mut reader =
            FrameReader::open(&notafile).expect("opening a directory is not itself the failure");
        let err = reader
            .next_frame()
            .expect_err("a failed read is an error, not an end of segment");
        match &err {
            Error::Io { path, source } => {
                assert_eq!(path, &notafile, "the error names the segment: {err}");
                assert!(
                    source.raw_os_error().is_some(),
                    "the errno is carried through rather than synthesised: {err}"
                );
            }
            other => panic!("a failing read is an io error, got {other}"),
        }

        // And through the iterator, which is how `replay` and `truncate` see
        // it: an `Err` item, not the `None` that would end the segment.
        let mut reader = FrameReader::open(&notafile).unwrap();
        assert!(
            matches!(reader.next(), Some(Err(Error::Io { .. }))),
            "the failure is yielded, not swallowed into an end of segment"
        );
        assert!(reader.next().is_none(), "and the reader is spent after it");

        // The clean end, for contrast, is the *only* thing that produces
        // `None`: a segment with no frames left in it.
        let empty = root.join("empty.wal");
        File::create(&empty).unwrap();
        assert!(
            FrameReader::open(&empty)
                .unwrap()
                .next_frame()
                .unwrap()
                .is_none(),
            "an end of file is `Ok(None)`, and only a tear is an `Err`"
        );
    }

    #[test]
    fn signal_bytes_match_the_block_directory_names() {
        // These strings are the join between a WAL watermark and the block it
        // came from; a rename on one side only would silently put a watermark
        // on the wrong signal.
        assert_eq!(Signal::Logs.as_str(), "logs");
        assert_eq!(Signal::Traces.as_str(), "traces");
        assert_eq!(Signal::Metrics.as_str(), "metrics");
        for s in Signal::ALL {
            assert_eq!(Signal::from_u8(s as u8), Some(s));
        }
        assert_eq!(Signal::from_u8(3), None);
    }
}
