//! Immutable block storage.
//!
//! A block is a *directory* holding one Arrow IPC file per table:
//!
//! ```text
//! <root>/logs/p=<epoch_hour>/<min_ts:020>-<max_ts:020>-<node:08x>-<seq:012>-<wal_hi:020>/
//!     logs.arrow  log_attrs.arrow  resources.arrow  resource_attrs.arrow  scope_attrs.arrow
//! ```
//!
//! The filesystem *is* the manifest. Every reason a LSM engine needs a MANIFEST
//! file is absent here: Mira publishes exactly one immutable object per commit
//! via one directory rename, never mutates a published block, and never needs a
//! multi-file atomic operation. The directory name carries the full pruning key,
//! so booting is one `readdir` per partition with zero file opens, and there is
//! no metadata state that can disagree with the data. That is what makes the
//! process stateless in the sense that matters: kill it, restart it, point a
//! second one at the same directory read-only — nothing to reconcile.
//!
//! Publish is write-tmp / fsync-files / fsync-tmpdir / rename-dir / fsync-parent
//! / fsync-grandparent — the last because the partition directory itself is a
//! new entry in `<root>/<signal>` on the first block of every hour.
//! Directory rename is atomic on POSIX, so a block is either wholly visible or
//! wholly absent; there is no torn state for recovery to clean up, and therefore
//! no write-ahead log.
//!
//! Retention is `remove_dir_all` on the block directory. POSIX guarantees this is
//! safe against in-flight readers: a mapping holds a reference to the inode that
//! `close()` does not drop, so a query holding an `Arc<Mmap>` keeps reading
//! correct data from an unlinked file until it drops the mapping. The `Arc` is
//! the refcount; no lease protocol is needed.

use std::fs::{self, File};
use std::io::{self, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::ptr::NonNull;
use std::sync::Arc;

use arrow_array::RecordBatch;
use arrow_buffer::Buffer;
use arrow_ipc::MetadataVersion;
use arrow_ipc::reader::{FileDecoder, read_footer_length};
use arrow_ipc::root_as_footer;
use arrow_ipc::writer::{FileWriter, IpcWriteOptions};
use memmap2::Mmap;

use crate::error::{Error, IoContext, Result};
use crate::signal::Sealed;

/// Leading bytes of every Arrow IPC file. arrow-rs's own reader seeks straight
/// to the trailer and never checks this, so a truncated-from-the-front file
/// would decode as garbage; we check it ourselves.
const MAGIC: &[u8; 6] = b"ARROW1";

/// CRC32 of the record-batch body, stored in the footer's custom metadata along
/// with the exact byte length it covers. arrow-ipc has no checksum of its own: a
/// valid footer over a corrupt body decodes silently into wrong answers.
///
/// The length is stored rather than derived from the footer offset because
/// `finish()` appends an end-of-stream marker *after* the last batch and before
/// the footer, so "everything before the footer" and "everything the writer had
/// emitted when we snapshotted" differ by those bytes.
const CRC_KEY: &str = "mira.crc32";
const CRC_LEN_KEY: &str = "mira.crc32.len";

/// On-disk format version, stamped into every table's footer metadata beside
/// the CRC — a place that already exists, so no second file and no second
/// fsync. Read before the reader trusts anything else in the file.
///
/// What it is for: three of the five logs tables are read positionally
/// (`attrs.rs`, `query.rs`: `column(3)` is the `str` value column), so inserting
/// a field into a schema in `schema.rs` does not fail against blocks already on
/// disk — it reinterprets them. Without a version there is nothing a reader can
/// look at to tell the two layouts apart.
///
/// ponytail: this records the version and refuses the future — a block from a
/// newer Mira is a named error, not a misparse — and that is all. It does not
/// make an *older* block readable by a newer binary, because there is no
/// migration to run and one version of one layout to run it on. The upgrade
/// path, in order: move the positional readers onto `column_by_name` (which the
/// root tables already use, which is why `LogRecord.event_name` could be added
/// without rewriting a block); then a column insertion needs no bump at all.
/// Until that lands, the rule is that any change to a published table's column
/// list bumps [`FORMAT_VERSION`], and the compatibility branch for the older
/// layout goes next to the check in [`open_table`].
const FORMAT_KEY: &str = "mira.format";

/// The format this binary writes, and the highest it will read.
pub const FORMAT_VERSION: u32 = 1;

/// The version blocks written before [`FORMAT_KEY`] existed are treated as.
///
/// They are byte-identical to version 1 — the key was added without changing
/// anything else about the file — so absent means 1 rather than "refuse it".
/// A format change that orphaned every block already on somebody's disk would
/// be a worse bug than the one the version exists to catch.
const LEGACY_VERSION: u32 = 1;

/// The ZSTD level every compressed table is written at.
///
/// The same as arrow-ipc's default, and set explicitly anyway: the ratio in
/// `docs/architecture.md` section 11 is a published number, and an upstream
/// default that moved would move it without anything in this tree changing.
///
/// 3 and not higher, which was measured and rejected. Over 8 real blocks per
/// signal, level 9 buys **2.4%** fewer bytes (8.41x against 8.21x) and takes
/// **1.4x** as long to rewrite a block. Compaction is a background sweep sharing
/// cores with ingest, and a sweep that falls behind leaves blocks uncompressed,
/// so 2.4% does not buy the risk. Nearly all of the ratio comes from the
/// dictionary column in [`crate::schema::ATTRS`], not from the level.
const ZSTD_LEVEL: i32 = 3;

/// Where the schema message starts: `ARROW1` padded up to [`ALIGNMENT`], which
/// is exactly how `FileWriter` places it (`pad_to_alignment` over the same
/// constant we hand it in [`write_table_with`]).
const HEADER_LEN: usize = MAGIC.len().next_multiple_of(ALIGNMENT);

/// The prefix of every encapsulated IPC message since the legacy format was
/// retired. We write V5 with `write_legacy_ipc_format` off, so a message that
/// does not start with it is not a message this wrote.
const CONTINUATION: [u8; 4] = [0xff; 4];

/// A block file whose own metadata does not describe it.
///
/// Every length and offset in [`open_table`] is read *out of the file*, which
/// makes it attacker-controlled-equivalent — a disk bit flip being the
/// realistic case. Unchecked, they index the mapping out of bounds, and
/// `panic = "abort"` (workspace release profile) turns that into a process
/// death rather than a caught error: every open block and in-flight export
/// dies with it, the bad block is still on disk afterwards, and the restart
/// hits the same byte. One bad byte becomes an unattended crashloop. As an
/// error it is one table a query skips.
///
/// ponytail: `io::ErrorKind::InvalidData` inside `Error::Io` rather than an
/// `Error::CorruptBlock` of its own, following the NUL-byte case in
/// [`fs_type`]. Every caller today treats a block it cannot read the same way
/// whatever the reason; give it a variant when one of them needs to match on
/// it.
fn corrupt(path: &Path, what: String) -> Error {
    Error::Io {
        path: path.to_path_buf(),
        source: io::Error::new(io::ErrorKind::InvalidData, what),
    }
}

/// Buffer alignment for published blocks.
///
/// The *correctness* floor is `align_of::<T>()` — 16 bytes for the widest thing
/// we store. 64 is the cache-line / SIMD figure and costs a few padding bytes
/// per buffer. Blocks are written at 64 and read with `require_alignment(true)`
/// so that a regression in the write path fails loudly instead of making
/// arrow-rs quietly memcpy the entire body out of the mapping.
const ALIGNMENT: usize = 64;

const NANOS_PER_HOUR: i64 = 3_600 * 1_000_000_000;

/// A `Write` that hashes everything passing through it.
///
/// Sits between the IPC writer and the `BufWriter` so it sees the byte stream
/// exactly as it will land on disk, and can be read mid-stream (via
/// `FileWriter::get_ref`) to stamp the CRC into the footer before `finish()`.
struct CrcWriter<W> {
    inner: W,
    hasher: crc32fast::Hasher,
    written: u64,
}

impl<W: Write> CrcWriter<W> {
    fn new(inner: W) -> Self {
        Self {
            inner,
            hasher: crc32fast::Hasher::new(),
            written: 0,
        }
    }

    /// Bytes hashed so far, and their CRC.
    fn checksum(&self) -> (u64, u32) {
        (self.written, self.hasher.clone().finalize())
    }
}

impl<W: Write> Write for CrcWriter<W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let n = self.inner.write(buf)?;
        self.hasher.update(&buf[..n]);
        self.written += n as u64;
        Ok(n)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

/// Write one table as a self-contained, checksummed, 64-byte-aligned IPC file
/// and fsync it. Uncompressed on purpose: IPC body compression forces the reader
/// to decompress into fresh allocations, which is mutually exclusive with the
/// zero-copy mmap read path.
///
/// Never point this at a path some reader may have mapped. It truncates, and a
/// mapping over a truncated file is a SIGBUS on the next page touched, not an
/// error anyone can catch. Replacing a published table means staging a new file
/// and renaming, which is what [`compact`] does.
pub fn write_table(path: &Path, batch: &RecordBatch) -> Result<()> {
    write_table_with(path, std::slice::from_ref(batch), None)
}

/// The same file, ZSTD-compressed per buffer.
///
/// Only for the cold tier (section 11): a compressed block cannot be read out of its
/// mapping, so this trades the zero-copy property for bytes. The retention
/// worker applies it to blocks old enough that nothing is scanning them, which
/// is where the trade is free.
///
/// The reader needs no flag — the IPC metadata records the codec per batch, so
/// a directory can hold both tiers at once and does, mid-rewrite.
pub fn write_table_zstd(path: &Path, batch: &RecordBatch) -> Result<()> {
    write_table_with(
        path,
        std::slice::from_ref(batch),
        Some(arrow_ipc::CompressionType::ZSTD),
    )
}

/// The same again as LZ4_FRAME, so the `tier` example can price the pure-Rust
/// alternative against the C one on real blocks. Nothing in the engine writes
/// LZ4; see the decisions table in `docs/architecture.md`.
pub fn write_table_lz4(path: &Path, batch: &RecordBatch) -> Result<()> {
    write_table_with(
        path,
        std::slice::from_ref(batch),
        Some(arrow_ipc::CompressionType::LZ4_FRAME),
    )
}

fn write_table_with(
    path: &Path,
    batches: &[RecordBatch],
    codec: Option<arrow_ipc::CompressionType>,
) -> Result<()> {
    let Some(first) = batches.first() else {
        return Ok(());
    };
    let file = File::create(path).ctx(path)?;
    let opts = IpcWriteOptions::try_new(ALIGNMENT, false, MetadataVersion::V5)?;
    let opts = match codec {
        // LZ4 rejects a configured level outright rather than ignoring one, so
        // the level goes on only where it means something.
        Some(arrow_ipc::CompressionType::ZSTD) => opts
            .try_with_compression(codec)?
            .try_with_compression_level(Some(ZSTD_LEVEL))?,
        Some(c) => opts.try_with_compression(Some(c))?,
        None => opts,
    };
    let mut w = FileWriter::try_new_with_options(
        CrcWriter::new(BufWriter::new(file)),
        &first.schema(),
        opts,
    )?;
    for batch in batches {
        w.write(batch)?;
    }

    // Everything written so far is the body; the footer is emitted by finish().
    let (len, crc) = w.get_ref().checksum();
    w.write_metadata(FORMAT_KEY, FORMAT_VERSION.to_string());
    w.write_metadata(CRC_KEY, format!("{crc:08x}"));
    w.write_metadata(CRC_LEN_KEY, len.to_string());
    w.finish()?;

    let mut buf = w.into_inner()?;
    buf.flush().ctx(path)?;
    let file = buf.inner.into_inner().map_err(|e| Error::Io {
        path: path.to_path_buf(),
        source: e.into_error(),
    })?;
    crate::sync_all(&file).ctx(path)?;
    Ok(())
}

/// A published block, discovered by reading directory names.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlockRef {
    pub dir: PathBuf,
    pub min_ts: i64,
    pub max_ts: i64,
    /// Which replica wrote this block. See [`node_id`].
    pub node: u32,
    pub seq: u64,
    /// One past the highest write-ahead-log sequence whose records are in this
    /// block, or `0` for a block published before the log existed.
    ///
    /// This is the entire manifest. Principle 4 says there is no coordination
    /// state and the block directory *is* the manifest, so the one fact WAL
    /// replay needs — how far the log has been absorbed — is carried in the
    /// name rather than in a file someone has to keep consistent with the
    /// blocks. Recovery therefore stays the `readdir` section 4 promises it is.
    pub wal_hi: u64,
}

impl BlockRef {
    /// True if this block could contain a row in `[from, to]`. The whole point
    /// of the naming scheme: pruning without opening a single file.
    pub fn overlaps(&self, from: i64, to: i64) -> bool {
        self.min_ts <= to && self.max_ts >= from
    }
}

/// One candidate the read path may have to open: a published block directory,
/// or the snapshot of a block that is still open (section 4).
///
/// Everything a scan needs before it opens anything — the range to prune on, the
/// identity a cursor is built from, and where the tables are. The two cases
/// differ in exactly two ways: a snapshot has no directory to read sidecars out
/// of, and its tables are already in memory.
pub(crate) struct Src<'a> {
    pub node: u32,
    pub seq: u64,
    pub min_ts: i64,
    pub max_ts: i64,
    /// `None` for a snapshot. Skipping the sidecar probes is not a special
    /// case: "no filter file" already means "scan me" for every block published
    /// before that filter existed.
    pub dir: Option<&'a Path>,
    tables: Option<&'a [(&'static str, RecordBatch)]>,
}

impl<'a> Src<'a> {
    pub fn disk(b: &'a BlockRef) -> Src<'a> {
        Src {
            node: b.node,
            seq: b.seq,
            min_ts: b.min_ts,
            max_ts: b.max_ts,
            dir: Some(b.dir.as_path()),
            tables: None,
        }
    }

    pub fn open(o: &'a crate::signal::Open) -> Src<'a> {
        Src {
            node: o.node,
            seq: o.seq,
            min_ts: o.sealed.min_ts,
            max_ts: o.sealed.max_ts,
            dir: None,
            tables: Some(&o.sealed.tables),
        }
    }

    pub fn overlaps(&self, from: i64, to: i64) -> bool {
        self.min_ts <= to && self.max_ts >= from
    }

    /// One named table, or `None` if this source does not have it.
    ///
    /// A missing table on disk is not an error: retention unlinking an expired
    /// block underneath a scan is normal. An open block's tables are already
    /// Arrow, under the same names the published files would carry — including
    /// the empty ones, which `publish` skips and which read identically to
    /// absent everywhere downstream.
    pub fn load(&self, name: &str) -> Result<Option<RecordBatch>> {
        match (self.tables, self.dir) {
            (Some(tables), _) => Ok(tables
                .iter()
                .find(|(n, _)| *n == name)
                .map(|(_, b)| b.clone())),
            // `write_table` emits exactly one record batch per file, so row
            // numbers are unambiguous and there is never a second batch to
            // stitch.
            (None, Some(dir)) => Ok(open_table_opt(&dir.join(format!("{name}.arrow")))?
                .and_then(|t| t.batches.first().cloned())),
            (None, None) => Ok(None),
        }
    }
}

/// The published blocks under `root/signal`, plus any open-block snapshots that
/// have not yet been published under the same `(node, seq)`.
///
/// The published copy wins a collision: it is at least as complete, and it is
/// the one whose cursors readers already hold. `(node, seq)` is enough to
/// recognise it because the flusher snapshots under the sequence it will publish
/// under — see [`crate::signal::Open`].
pub(crate) fn sources<'a>(
    disk: &'a [BlockRef],
    open: &'a [std::sync::Arc<crate::signal::Open>],
) -> Vec<Src<'a>> {
    disk.iter()
        .map(Src::disk)
        .chain(
            open.iter()
                .filter(|o| !disk.iter().any(|b| b.node == o.node && b.seq == o.seq))
                .map(|o| Src::open(o)),
        )
        .collect()
}

/// A replica's writer identity, derived from its name with no coordination.
///
/// The block name has to be unique across every process that can ever write to
/// this directory tree, and it has to be so without asking anyone. Hashing the
/// replica name gets that for free: in Kubernetes the name is the pod name,
/// which the scheduler already guarantees is unique, so uniqueness is inherited
/// from a namespace that exists rather than invented by a protocol we would then
/// have to operate.
/// Truncated to 32 bits: two replicas colliding needs ~77k of them on one
/// volume, and a collision degrades to the loud `ENOTEMPTY` above rather than to
/// anything silent.
pub fn node_id(name: &str) -> u32 {
    crate::identity::hash64(name.as_bytes()) as u32
}

/// `{min_ts}-{max_ts}-{node}-{seq}-{wal_hi}`.
///
/// `node` is what makes two active replicas sharing a volume safe. Without it,
/// two writers allocate the same `seq` and the second `rename` lands on a
/// non-empty directory — `ENOTEMPTY`, and a node that can never publish again.
///
/// Splitting on `-` is only safe because [`crate::signal::Sealed`] clamps both
/// timestamps non-negative; a negative one would format with a leading `-` and
/// make the name unparseable, which is to say invisible to `scan`.
fn dir_name(min_ts: i64, max_ts: i64, node: u32, seq: u64, wal_hi: u64) -> String {
    format!("{min_ts:020}-{max_ts:020}-{node:08x}-{seq:012}-{wal_hi:020}")
}

/// Parses both the five-field name above and the four-field name that predates
/// the write-ahead log.
///
/// The old form has to keep working: a block directory written by an earlier
/// build is still a valid block, and there is no migration step to run because
/// there is no metadata store to migrate. A missing `wal_hi` reads as `0`,
/// which is correct — those blocks came from a Mira with no log, so no
/// sequence is covered by them and replay must not skip anything on their
/// account.
fn parse_dir_name(name: &str) -> Option<(i64, i64, u32, u64, u64)> {
    let mut parts = name.split('-');
    let min = parts.next()?.parse().ok()?;
    let max = parts.next()?.parse().ok()?;
    let node = u32::from_str_radix(parts.next()?, 16).ok()?;
    let seq = parts.next()?.parse().ok()?;
    let wal_hi = match parts.next() {
        Some(field) => field.parse().ok()?,
        None => 0,
    };
    if parts.next().is_some() {
        return None;
    }
    Some((min, max, node, seq, wal_hi))
}

fn fsync_dir(path: &Path) -> Result<()> {
    crate::sync_all(&File::open(path).ctx(path)?).ctx(path)
}

/// Atomically publish a set of tables as one block under `<root>/<signal>/`.
///
/// Returns the published directory.
///
/// Acking an OTLP export requires that its records are recoverable, and until
/// [`crate::wal`] existed this call was the only thing that made them so — so
/// the caller had to wait for it. It no longer does, provided the export is in
/// the log: `wal_hi` is what records that, and it must be whatever
/// [`crate::wal::Wal::watermark_for`] answers for the sequences in `sealed` —
/// an *exclusive* watermark over the signal as a whole, not one past this
/// block's own highest sequence, because sibling shards are filling their own
/// blocks from the same log and hold sequences below it that are not here. Pass
/// `0` when there is no log.
///
/// Getting `wal_hi` too high is the dangerous direction. [`wal_watermarks`]
/// takes the maximum over every block of the signal, so a watermark that claims
/// sequences no block holds makes replay skip them — silent loss, with the
/// client holding a 200. Too low only costs a re-ingest.
pub fn publish(
    root: &Path,
    signal: &str,
    node: u32,
    seq: u64,
    wal_hi: u64,
    sealed: &Sealed,
) -> Result<BlockRef> {
    let (min_ts, max_ts) = (sealed.min_ts, sealed.max_ts);
    // The staging name carries `node` for the same reason the final one does:
    // two replicas on one volume must not stage into the same directory. It
    // also carries the timestamp range, because `node` alone is not enough: two
    // replicas started with the same `--node` — a misconfiguration, but a silent
    // one — walk the same `seq` from 0 and collide. The observed cost was two
    // lost publishes in 107, both retryable; the unobserved one is worse, since
    // B's `remove_dir_all` can empty a directory A is still writing tables into
    // and the winner then publishes a block assembled from two sealed sets.
    // Adding the range the final name already carries makes the path unique per
    // block content, which is exactly the granularity the collision needs.
    let staging = root.join(".tmp");
    let tmp = staging.join(format!(
        "{signal}-{node:08x}-{seq:012}-{min_ts:020}-{max_ts:020}"
    ));
    fs::create_dir_all(&staging).ctx(&staging)?;
    // `create_dir`, not `create_dir_all`: the latter succeeds on a directory
    // that already exists, which is how a leftover from a killed publish gets
    // silently merged into this one. Colliding here is a retryable error, and
    // `sweep_staging` clears the leftover at the next boot.
    fs::create_dir(&tmp).ctx(&tmp)?;

    let signal_dir = root.join(signal);
    let partition = signal_dir.join(format!("p={}", min_ts.div_euclid(NANOS_PER_HOUR)));
    let dir = partition.join(dir_name(min_ts, max_ts, node, seq, wal_hi));
    let staged = stage(&tmp, sealed).and_then(|()| {
        fs::create_dir_all(&partition).ctx(&partition)?;
        fs::rename(&tmp, &dir).ctx(&dir)?;
        fsync_dir(&partition)?;
        // And the directory naming the partition. fsyncing `partition` persists
        // the entries inside it, not the entry for it in its own parent — so on
        // the first block of a new hour the block is durable and the directory
        // holding it is unflushed metadata, which loses an acked block on XFS
        // and btrfs (ext4's ordered journal happens to cover it). One extra
        // fsync per 32 MB block, against the per-table fsyncs already paid.
        fsync_dir(&signal_dir)
    });

    if let Err(e) = staged {
        unwind_staging(&tmp);
        return Err(e);
    }

    Ok(BlockRef {
        dir,
        min_ts,
        max_ts,
        node,
        seq,
        wal_hi,
    })
}

/// Unwind the staging directory of a publish that did not complete.
///
/// Every step of [`publish`] `?`s, and the staging name is unique per block
/// content, so nothing reuses it: without this the tables already written are
/// stranded under `.tmp`, where `expire` — which only ever scans
/// `<root>/<signal>` — will never see them. That matters because the flusher
/// retries every couple of seconds, so the failure this path exists for (a full
/// disk, a read-only mount) leaks on the order of a thousand directories an hour
/// per signal, and the space is still gone once the operator has freed the disk.
/// `sweep_staging` keeps its boot-time role: it is for the kill -9 that never
/// reaches this line.
///
/// Returns nothing, deliberately. The publish failure is the one the caller has
/// to act on, and a cleanup that could not run is a leaked directory the next
/// boot's sweep collects anyway — turning it into the returned error would
/// replace the reason the publish failed with the reason the tidy-up did.
fn unwind_staging(tmp: &Path) {
    if let Err(rm) = fs::remove_dir_all(tmp) {
        // NotFound is the normal shape of "the rename succeeded and a later
        // fsync did not": there is nothing left at `tmp` to remove, and the
        // block is published.
        if rm.kind() != io::ErrorKind::NotFound {
            tracing::warn!(
                path = %tmp.display(),
                error = %rm,
                "cannot remove the staging directory of a failed publish",
            );
        }
    }
}

/// Write one sealed block's files into an already-created staging directory.
///
/// Split out of [`publish`] only so that the failure of any step in it has one
/// place to be cleaned up from.
fn stage(tmp: &Path, sealed: &Sealed) -> Result<()> {
    // An empty table is not written. Arrow IPC framing for a zero-row table is
    // ~1 KB for three columns and ~2.5 KB for nine (measured), which is nothing
    // against a full 32 MB block and most of a block sealed by the age timer on
    // a quiet node. A traces block has nine tables and typically five of them
    // have any rows: the four event and link tables — span_events,
    // span_links and the attribute table of each — stay empty unless a service
    // emits events or links, which most do not. Measured on the smoke corpus,
    // every traces block writes exactly those five and skips 8 KB of framing
    // against 18 KB of tables; a metrics block writes eight of thirteen.
    //
    // The reader treats a missing file as an empty table, which it has to do
    // anyway: it is also how a block written by an older version that did not
    // have the table reads back.
    for (name, batch) in &sealed.tables {
        if batch.num_rows() > 0 {
            write_table(&tmp.join(format!("{name}.arrow")), batch)?;
        }
    }
    // Sidecars are written with the same durability as the tables: a block that
    // lands with a stale or missing index is one the reader would either skip
    // wrongly or scan slowly, and only the first of those is a correctness bug —
    // but both are avoidable for one fsync each of files `bloom` sizes by row
    // count — 1 KB apiece on the blocks a quiet node writes, 65 KB at the top
    // end, against tens of MB of tables.
    for (name, bytes) in &sealed.sidecars {
        let path = tmp.join(name);
        let mut f = File::create(&path).ctx(&path)?;
        f.write_all(bytes).ctx(&path)?;
        crate::sync_all(&f).ctx(&path)?;
    }
    fsync_dir(tmp)
}

/// Remove staging directories this node left behind for this signal.
///
/// Since the staging name carries the block's timestamp range it is never
/// reused, so a publish killed between staging and `rename` leaks a directory
/// that nothing else will ever clear. Sweeping is filtered by signal *and* node
/// rather than emptying `.tmp` wholesale, because another replica on the same
/// volume may have a publish in flight — deleting under it is exactly the
/// corruption the unique staging name exists to prevent. Called once per signal
/// at boot, before that signal's flusher can publish anything.
pub fn sweep_staging(root: &Path, signal: &str, node: u32) -> Result<usize> {
    let tmp = root.join(".tmp");
    let prefix = format!("{signal}-{node:08x}-");
    let entries = match fs::read_dir(&tmp) {
        Ok(d) => d,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(0),
        Err(e) => {
            return Err(Error::Io {
                path: tmp,
                source: e,
            });
        }
    };
    let mut removed = 0;
    for entry in entries {
        let path = entry.ctx(&tmp)?.path();
        if path
            .file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|n| n.starts_with(&prefix))
        {
            fs::remove_dir_all(&path).ctx(&path)?;
            removed += 1;
        }
    }
    Ok(removed)
}

/// How far the write-ahead log has been absorbed into published blocks, per
/// signal, in the order [`crate::wal::Signal::index`] uses.
///
/// This is the whole of WAL recovery's input, and it is three `readdir`s — the
/// same ones the read path does at boot anyway. No manifest file, so nothing
/// that can disagree with the blocks it describes.
///
/// The maximum over the blocks, not the last one published: `scan` sorts by
/// timestamp, and a block covering an older hour can be published after a
/// newer one when a late export arrives. Taking the last would then walk the
/// watermark backwards and replay data that is already stored.
///
/// A signal with no blocks gets `0` — replay everything the log holds for it,
/// which is right, because nothing has absorbed any of it.
pub fn wal_watermarks(root: &Path) -> Result<crate::wal::Watermarks> {
    let mut out = [0u64; 3];
    for signal in crate::wal::Signal::ALL {
        out[signal.index()] = scan(root, signal.as_str())?
            .iter()
            .map(|b| b.wal_hi)
            .max()
            .unwrap_or(0);
    }
    Ok(out)
}

/// Rebuild the catalog from the filesystem. This is the entire boot sequence for
/// the read path: no manifest to replay, and the write-ahead log's recovery
/// point rides in the block names rather than in a file of its own.
pub fn scan(root: &Path, signal: &str) -> Result<Vec<BlockRef>> {
    let base = root.join(signal);
    let mut out = Vec::new();
    let partitions = match fs::read_dir(&base) {
        Ok(d) => d,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(out),
        Err(e) => {
            return Err(Error::Io {
                path: base,
                source: e,
            });
        }
    };
    for partition in partitions {
        let partition = partition.ctx(&base)?.path();
        if !partition.is_dir() {
            continue;
        }
        for entry in fs::read_dir(&partition).ctx(&partition)? {
            let dir = entry.ctx(&partition)?.path();
            let Some((min_ts, max_ts, node, seq, wal_hi)) = dir
                .file_name()
                .and_then(|n| n.to_str())
                .and_then(parse_dir_name)
            else {
                continue;
            };
            out.push(BlockRef {
                dir,
                min_ts,
                max_ts,
                node,
                seq,
                wal_hi,
            });
        }
    }
    out.sort_by_key(|b| (b.min_ts, b.seq));
    Ok(out)
}

/// Drop every block whose newest row is older than `cutoff_ns`.
///
/// TTL is a directory unlink, not a compaction: there is no read-modify-write of
/// live data, so retention costs no IO bandwidth and cannot interfere with
/// ingest.
pub fn expire(root: &Path, signal: &str, cutoff_ns: i64) -> Result<usize> {
    let mut dropped = 0;
    for block in scan(root, signal)? {
        if block.max_ts < cutoff_ns {
            match fs::remove_dir_all(&block.dir) {
                Ok(()) => dropped += 1,
                // Another replica sharing this volume expired it first. Racing
                // to delete the same immutable block is not a conflict, and
                // aborting here would leave the rest of the sweep undone.
                Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                // Anything else — a directory this process cannot traverse, a
                // file the kernel refuses to unlink — is about *this* block and
                // says nothing about the next one. Returning here stopped the
                // sweep for the whole signal, so one undeletable block meant
                // retention silently stopped reclaiming space for every other
                // block beside it, which is the failure retention exists to
                // prevent. Log it and keep going; the count returned is the
                // blocks actually dropped, so a sweep that reclaimed nothing
                // still reports nothing.
                Err(e) => tracing::warn!(
                    block = %block.dir.display(),
                    error = %e,
                    "cannot expire block; skipping it",
                ),
            }
        }
    }
    Ok(dropped)
}

/// Refuse to start on a filesystem the read path cannot survive.
///
/// Every block is read through `mmap`. On a network filesystem a server that
/// goes away, or a file that changes length underneath a mapping, is delivered
/// as `SIGBUS` — a signal, not an `io::Error`. There is nothing to catch and no
/// way to unwind; the process dies mid-query. The atomicity this design rests on
/// is also weaker there: NFS `rename` is atomic on the server but a client may
/// still serve a cached negative lookup, and `fsync` semantics vary by mount
/// option. Both are reasons to say no at startup rather than at 3am.
///
/// Called once, on the data directory, before anything is published or mapped.
pub fn check_filesystem(path: &Path) -> Result<()> {
    check_fs_type(path, fs_type(path)?)
}

/// The decision [`check_filesystem`] makes, split from the mount it makes it
/// about for the same reason [`check_format`] takes a string rather than a
/// file: the rule is the part that can be wrong, and a test cannot conjure an
/// NFS mount to state it over.
fn check_fs_type(path: &Path, fs: Option<String>) -> Result<()> {
    let Some(fs) = fs else {
        return Ok(());
    };
    // FUSE is the ambiguous one and it has to stay a warning: the magic number
    // is identical for `gcsfuse` and `s3fs`, which are exactly as fatal as NFS,
    // and for a perfectly local userspace filesystem, which is fine. Refusing
    // would strand the second case; staying silent would strand the first.
    if fs == "fuse" {
        tracing::warn!(
            path = %path.display(),
            "data directory is on a FUSE filesystem. If it is network-backed \
             (gcsfuse, s3fs, rclone), mmap will raise SIGBUS and kill the \
             process; if it is local, ignore this."
        );
        return Ok(());
    }
    Err(Error::NetworkFilesystem {
        path: path.to_path_buf(),
        fs,
    })
}

/// Refuse to start on a data directory that cannot be written to.
///
/// `create_dir_all` returns `Ok` for a directory that already exists whatever
/// its mode, so a read-only mount, a wrong-uid volume or a typo pointing at
/// someone else's path gets all the way to a listening socket. The first
/// evidence is then a flush error per export, minutes later, under load, which
/// reads as a Mira fault rather than as a mount that was never writable — the
/// same reasoning as [`check_filesystem`], and the same one line in `main`.
///
/// The probe carries the pid because replicas may share a directory (section 10) and
/// two of them starting together must not race each other's cleanup.
pub fn check_writable(path: &Path) -> Result<()> {
    let probe = path.join(format!(".mira-write-probe-{}", std::process::id()));
    // The error names the directory, not the probe: the probe is an
    // implementation detail and nobody should go looking for that filename.
    let fail = |source| Error::NotWritable {
        path: path.to_path_buf(),
        source,
    };
    fs::write(&probe, []).map_err(fail)?;
    fs::remove_file(&probe).map_err(fail)
}

/// One filled `statfs`, shared by the two things that ask the mount a question.
fn statfs(path: &Path) -> Result<libc::statfs> {
    let c = std::ffi::CString::new(path.as_os_str().as_encoded_bytes()).map_err(|_| Error::Io {
        path: path.to_path_buf(),
        source: io::Error::new(io::ErrorKind::InvalidInput, "path contains a NUL byte"),
    })?;
    // SAFETY: `statfs` is POD — integers and fixed byte arrays — so all-zero is
    // a valid value to hold until the call below fills it. Only fields `statfs`
    // itself writes are read afterwards, and only on the success path.
    let mut buf: libc::statfs = unsafe { std::mem::zeroed() };
    // SAFETY: `c` is a `CString` — NUL-terminated by construction, rejected
    // above if the path had an interior NUL — and it is still live at the call.
    // `&mut buf` is a correctly typed, writable `statfs` the kernel fills.
    if unsafe { libc::statfs(c.as_ptr(), &mut buf) } != 0 {
        return Err(Error::Io {
            path: path.to_path_buf(),
            source: io::Error::last_os_error(),
        });
    }
    Ok(buf)
}

/// How much of the filesystem holding `path` is still free, as a fraction of
/// its total size.
///
/// `f_bavail`, not `f_bfree`: the difference is the root reserve (5% on a
/// default ext4), which is space the process cannot write into and therefore
/// not space Mira has. Both terms of the ratio are in blocks of `f_bsize`, so
/// the block size cancels and is not in the arithmetic.
pub fn free_fraction(path: &Path) -> Result<f64> {
    let buf = statfs(path)?;
    if buf.f_blocks == 0 {
        // A mount that reports no blocks at all — some pseudo-filesystems do —
        // is not a mount that can fill up, and calling it 0% free would stop
        // ingest on a node with nothing wrong with it.
        return Ok(1.0);
    }
    Ok(buf.f_bavail as f64 / buf.f_blocks as f64)
}

/// The mount's filesystem type, if it is one of the ones that matter. `None`
/// means "nothing to say about it", which is every local filesystem.
fn fs_type(path: &Path) -> Result<Option<String>> {
    // statfs(2) needs the path to exist; the caller creates the data directory
    // before this runs, but a bare `Ok(None)` if it does not is friendlier than
    // an ENOENT that says nothing about filesystems.
    if !path.exists() {
        return Ok(None);
    }
    let buf = statfs(path)?;

    // macOS reports the type by name, which is both readable and complete.
    #[cfg(target_os = "macos")]
    {
        let name: Vec<u8> = buf
            .f_fstypename
            .iter()
            .take_while(|&&c| c != 0)
            .map(|&c| c as u8)
            .collect();
        let name = String::from_utf8_lossy(&name).into_owned();
        Ok(match name.as_str() {
            "nfs" | "smbfs" | "cifs" | "webdav" | "afpfs" | "ftp" => Some(name),
            n if n.contains("fuse") => Some("fuse".into()),
            _ => None,
        })
    }

    // Linux reports a magic number. Listed rather than ranged because the set of
    // filesystems that break `mmap` is small, specific and does not grow often;
    // anything unrecognised is treated as local, which is the right default for
    // a check whose false positive is "Mira will not start".
    #[cfg(not(target_os = "macos"))]
    {
        // Masked to 32 bits: `f_type` is `__fsword_t`, which is i64 on x86_64
        // glibc but i32 on some musl and 32-bit targets, where a magic with the
        // high bit set (CIFS, SMB2) arrives sign-extended.
        let ty = (buf.f_type as u64) & 0xffff_ffff;
        Ok(match ty {
            0x6969 => Some("NFS".into()),
            0x517b => Some("SMB".into()),
            0xff53_4d42 => Some("CIFS".into()),
            0xfe53_4d42 => Some("SMB2".into()),
            0x0102_1997 => Some("9P".into()),
            0x5346_414f => Some("AFS".into()),
            0x00c3_6400 => Some("CephFS".into()),
            0x0116_1970 => Some("GFS2".into()),
            0x7461_636f => Some("OCFS2".into()),
            0x0bd0_0bd0 => Some("Lustre".into()),
            0x6573_5546 => Some("fuse".into()),
            _ => None,
        })
    }
}

/// The cold-tier marker. Its presence means every table in the block is already
/// ZSTD-encoded, so a sweep can skip the directory without opening a file.
const COLD_MARKER: &str = "cold";

/// A block goes cold once it has aged out of the hour it was partitioned into.
///
/// Reusing the partition width means the tier boundary is derived rather than
/// configured: the same instant that stops new rows landing next to this block
/// is the one that stops queries with a default window from reaching it.
pub const COLD_AFTER_NS: i64 = NANOS_PER_HOUR;

/// ponytail: a flat cap per sweep, so the first pass over an existing volume
/// drains at a few hundred MB a minute instead of saturating the disk for an
/// hour. Make it adaptive when a real deployment says the backlog matters.
const MAX_COMPACT_PER_SWEEP: usize = 8;

/// Rewrite aged blocks ZSTD-compressed, in place.
///
/// Measured by `cargo run --release -p miradb-core --example tier` over section
/// 11's corpus: 0.113 of the plain size for logs, 0.126 for traces, at 634
/// MiB/s on one core. Warm, reading a compacted block back costs 1.11× the
/// plain read — the inflate is real and it is small. Cold, which is the only
/// state a block old enough to be compacted is in, 8.4× fewer pages to fault
/// and 8.4× fewer bytes to CRC more than pays for it. So the tier costs the
/// read path the zero-copy property and a tenth of a warm read, and only for
/// data nothing is scanning any more.
///
/// Crash safety is the trick `publish` already uses: write beside the target,
/// then rename. A crash leaves a directory with some tables compressed and some
/// not, which reads correctly — the codec is per-batch IPC metadata, not a
/// property of the directory — and the absent marker makes the next sweep
/// finish the job.
pub fn compact(root: &Path, signal: &str, node: u32, cutoff_ns: i64) -> Result<usize> {
    let (mut done, mut tried) = (0, 0);
    for block in scan(root, signal)? {
        // The budget counts attempts, not successes. A block that fails is a
        // block whose tables were read and CRC'd before it failed, so charging
        // only the successes would let a directory full of unreadable blocks
        // re-read every one of them on every sweep, for ever.
        if tried == MAX_COMPACT_PER_SWEEP {
            break;
        }
        if block.max_ts >= cutoff_ns || block.dir.join(COLD_MARKER).exists() {
            continue;
        }
        tried += 1;
        match compact_block(&block.dir, node) {
            Ok(()) => done += 1,
            // Expired out from under the sweep, by this node's own retention or
            // another replica's. Nothing to compact is not a failure.
            Err(Error::Io { source, .. }) if source.kind() == io::ErrorKind::NotFound => {}
            // One block that cannot be rewritten — a corrupt table, a bad
            // checksum, a permission the sweep does not have — used to end the
            // sweep, which meant compaction for the whole signal stopped
            // permanently at the oldest broken block and every block behind it
            // stayed uncompressed. Skipping keeps the block exactly as
            // expirable as it was: `expire` walks the same `scan` and unlinks
            // the directory without opening a file, so the block that cannot be
            // compacted is still the block retention takes.
            Err(e) => tracing::warn!(
                block = %block.dir.display(),
                error = %e,
                "cannot compact block; skipping it",
            ),
        }
    }
    Ok(done)
}

fn compact_block(dir: &Path, node: u32) -> Result<()> {
    // Collected before rewriting: entries created during an open `read_dir` may
    // or may not be returned, and one of these renames lands on a name the
    // iterator has not reached yet.
    let mut tables = Vec::new();
    for entry in fs::read_dir(dir).ctx(dir)? {
        let path = entry.ctx(dir)?.path();
        if path.extension().is_some_and(|e| e == "arrow") {
            tables.push(path);
        }
    }

    for path in tables {
        let table = open_table(&path)?;
        // The staging name carries the node for the same reason `publish`'s
        // does: two replicas sharing this volume both see this block go cold,
        // and one truncating the other's half-written file would put garbage
        // under the rename.
        let tmp = path.with_extension(format!("{node:08x}.tmp"));
        write_table_with(&tmp, &table.batches, Some(arrow_ipc::CompressionType::ZSTD))?;
        // Rename rather than rewrite in place. A reader that already mapped the
        // old inode keeps reading it — POSIX holds an unlinked file open under
        // its mappings — which is the same guarantee `expire` depends on and
        // what keeps `open_table`'s immutability claim true.
        fs::rename(&tmp, &path).ctx(&path)?;
    }

    // Marker last, so a crash mid-rewrite is retried rather than declared done.
    crate::sync_all(&File::create(dir.join(COLD_MARKER)).ctx(dir)?).ctx(dir)?;
    fsync_dir(dir)
}

/// A table read straight out of its mapping.
///
/// The `RecordBatch` buffers point into the mmap; the mapping is kept alive by
/// the `Arc` that every `Buffer` holds, so `MappedTable` can be dropped while
/// batches derived from it are still in use.
pub struct MappedTable {
    pub batches: Vec<RecordBatch>,
    /// Address range of the mapping, so the zero-copy property can be asserted
    /// rather than assumed.
    mapping: std::ops::Range<usize>,
}

impl MappedTable {
    /// `(buffers_pointing_into_the_mapping, buffers_total)`.
    ///
    /// Walks every buffer of every column including child data, so a partially
    /// copied nested array shows up. `require_alignment(true)` should already
    /// make a copy impossible; this is the assertion that says so out loud.
    ///
    /// A cold block (see [`compact`]) reports 0/n on purpose: decompression has
    /// to allocate. Only the hot tier is held to `inside == total`.
    pub fn zero_copy_ratio(&self) -> (usize, usize) {
        let (mut inside, mut total) = (0, 0);
        for batch in &self.batches {
            for col in batch.columns() {
                let mut stack = vec![col.to_data()];
                while let Some(d) = stack.pop() {
                    for buf in d.buffers() {
                        total += 1;
                        if self.mapping.contains(&(buf.as_ptr() as usize)) {
                            inside += 1;
                        }
                    }
                    stack.extend(d.child_data().iter().cloned());
                }
            }
        }
        (inside, total)
    }
}

/// [`open_table`], but a missing file means an empty table rather than an error.
///
/// This is the normal way to read any table that can legitimately have no rows,
/// which is most of them: `publish` skips writing a zero-row table, so a traces
/// block from a service that emits no span links simply has no
/// `span_links.arrow`. It is also how a block written before a table existed
/// reads back, which is the same case a version from now.
pub fn open_table_opt(path: &Path) -> Result<Option<MappedTable>> {
    match open_table(path) {
        Err(Error::Io { source, .. }) if source.kind() == std::io::ErrorKind::NotFound => Ok(None),
        other => other.map(Some),
    }
}

/// Decide whether this binary is allowed to read a block declaring `declared`
/// as its [`FORMAT_KEY`].
///
/// Refusing the future is the whole job: a newer block read as the current
/// format would not fail, it would answer wrongly, because the readers that go
/// through a table by column position cannot tell a shifted column from the one
/// they wanted.
fn check_format(path: &Path, declared: Option<&str>) -> Result<()> {
    let version = match declared {
        None => LEGACY_VERSION,
        Some(v) => v
            .parse::<u32>()
            .map_err(|_| corrupt(path, format!("{FORMAT_KEY} is `{v}`, not a version")))?,
    };
    if version > FORMAT_VERSION {
        return Err(Error::Io {
            path: path.to_path_buf(),
            source: io::Error::new(
                io::ErrorKind::Unsupported,
                format!(
                    "block format version {version} was written by a newer Mira; \
                     this binary reads up to version {FORMAT_VERSION}"
                ),
            ),
        });
    }
    Ok(())
}

/// One encapsulated IPC message, described entirely by the *checksummed* bytes
/// it starts at.
///
/// Nothing in here comes from the footer, and that is the point. The footer's
/// `dictionaries` and `recordBatches` vectors are the file's table of contents,
/// they sit outside the CRC, and a flatbuffer vector's length is a single `u32`
/// — so clearing `recordBatches` is *one bit flip* that makes a block read back
/// `Ok` with no rows in it. Not a panic and not an error: a whole table of
/// telemetry silently gone, reported as success, which is the worst of the three
/// outcomes and the only one no caller can react to.
///
/// The body does not need that table of contents. Every message declares its own
/// metadata length in the 8-byte encapsulation prefix and its own body length in
/// its flatbuffer header, both inside `[0, body_len)`, so the messages chain: the
/// next one starts exactly [`Framed::stride`] bytes after this one. Walking that
/// chain costs one extra flatbuffer verify per message — a few hundred bytes each,
/// against a CRC of the whole body — and takes the footer out of the read path.
struct Framed {
    /// What arrow-rs is told about the message. Only `metaDataLength` is read
    /// back out of it (`read_record_batch` slices `data` by it); the offset is
    /// relative to `data`, which starts at the message, so it is zero.
    block: arrow_ipc::Block,
    /// Exactly this message: metadata, padding and body, and nothing after it.
    data: Buffer,
    header: arrow_ipc::MessageHeader,
    version: MetadataVersion,
}

impl Framed {
    /// Distance to the next message. `metaDataLength` is padded to [`ALIGNMENT`]
    /// by the writer and is at least 8, so this always advances.
    fn stride(&self) -> usize {
        self.block.metaDataLength() as usize + self.block.bodyLength() as usize
    }
}

/// Frame the message at `offset`, bounds-checking every number it declares
/// against the region the CRC covers.
///
/// The checks are not belt-and-braces. `Buffer::slice_with_length` panics on a
/// range it does not like, `panic = "abort"` makes that a process death, and the
/// numbers being checked were read off a disk — see [`corrupt`].
fn message_at(path: &Path, buffer: &Buffer, offset: usize, body_len: usize) -> Result<Framed> {
    let bad = |why: String| corrupt(path, format!("message at offset {offset}: {why}"));
    // Written as a subtraction from `body_len` rather than an addition to
    // `offset`, so that an offset near `usize::MAX` is rejected instead of
    // wrapping into the range it is being tested against.
    if offset > body_len || body_len - offset < 8 {
        return Err(bad(format!(
            "outside the {body_len} bytes the checksum covers"
        )));
    }
    let head = &buffer[offset..offset + 8];
    if head[..4] != CONTINUATION {
        return Err(bad("no message framing here".into()));
    }
    // `write_encoded_data` declares the padded metadata length *after* the
    // 8-byte prefix, so the header the decoder skips is exactly 8 more.
    let declared = i32::from_le_bytes(head[4..].try_into().expect("4 bytes"));
    let Some(meta) = declared.checked_add(8).filter(|m| *m >= 8) else {
        return Err(bad(format!("{declared} is not a metadata length")));
    };
    let meta = meta as usize;
    if body_len - offset < meta {
        return Err(bad(format!(
            "{meta} bytes of metadata run past the {body_len} bytes the checksum covers"
        )));
    }
    // The verifier here is what makes the two accessors below safe to call.
    let message = arrow_ipc::root_as_message(&buffer[offset + 8..offset + meta])
        .map_err(|e| bad(format!("not a readable IPC message: {e}")))?;
    let body = message.bodyLength();
    let room = (body_len - offset - meta) as i64;
    if body < 0 || body > room {
        return Err(bad(format!(
            "a {body}-byte body runs past the {body_len} bytes the checksum covers"
        )));
    }
    Ok(Framed {
        block: arrow_ipc::Block::new(0, meta as i32, body),
        data: buffer.slice_with_length(offset, meta + body as usize),
        header: message.header_type(),
        version: message.version(),
    })
}

/// Open one table of a block with no buffer copies.
///
/// This is a blocking call that can take a hard page fault. It must never run on
/// a tokio worker: a cold fault stalls the whole OS thread with no yield point
/// and no signal to the runtime. Callers go through `spawn_blocking` or a
/// dedicated reader pool.
pub fn open_table(path: &Path) -> Result<MappedTable> {
    let file = File::open(path).ctx(path)?;
    // SAFETY: the obligation is that nothing modifies or truncates this file
    // while the mapping lives — a truncation is a SIGBUS on the next page
    // touched, which no in-process check can catch. What discharges it is
    // Mira's own discipline, not the kernel: a block becomes visible by one
    // directory rename and is never written again ([`publish`]); [`compact`]
    // replaces a table by renaming a new file over the name, which unlinks the
    // old inode rather than truncating it; [`expire`] is `remove_dir_all`,
    // also unlink. POSIX keeps an unlinked inode alive under its mappings, so
    // every path Mira has lands on the safe side.
    //
    // That argument covers this process and every replica sharing the volume,
    // because they all run this code. It does not cover a third party editing
    // a block file in place, and nothing here can — the data directory is
    // Mira's, and that is a deployment property, not a checkable one.
    let mmap = unsafe { Mmap::map(&file) }.ctx(path)?;
    // Every open CRCs the whole body, so every page is touched. Faulting them in
    // one at a time caps a cold scan at fault latency; asking for the file up
    // front lets the kernel read ahead. A hint, so a failure is not an error.
    let _ = mmap.advise(memmap2::Advice::WillNeed);

    if mmap.len() < MAGIC.len() + 10 || &mmap[..MAGIC.len()] != MAGIC {
        return Err(Error::BadMagic {
            path: path.to_path_buf(),
        });
    }

    let len = mmap.len();
    let base = mmap.as_ptr() as usize;
    let ptr = NonNull::new(mmap.as_ptr().cast_mut()).expect("mmap is never null");
    // SAFETY: `ptr` and `len` are `mmap`'s own `as_ptr`/`len`, so they describe
    // exactly the mapped region and nothing beyond it, page-aligned. Moving the
    // `Mmap` into the `Arc` moves an (address, length) pair, not the mapping,
    // so `ptr` is still the same live region afterwards — and the `Arc` is the
    // allocation owner, so `munmap` runs only after the last `Buffer` sliced
    // from it is dropped. `cast_mut` is to fit the signature; `Buffer` is
    // read-only and never writes through it, which matters because the mapping
    // is `PROT_READ`.
    let buffer = unsafe { Buffer::from_custom_allocation(ptr, len, Arc::new(mmap)) };

    let trailer = len - 10;
    let footer_len = read_footer_length(buffer[trailer..].try_into().expect("10 bytes"))?;
    // `read_footer_length` only rejects a negative length, so a garbled one is
    // still a number this would subtract past zero and then slice with. See
    // [`corrupt`] for why that must not be a panic.
    if footer_len > trailer {
        return Err(corrupt(
            path,
            format!("footer says it is {footer_len} bytes, in a {len}-byte file"),
        ));
    }
    let footer_start = trailer - footer_len;
    let footer = root_as_footer(&buffer[footer_start..trailer])
        .map_err(|e| arrow_schema::ArrowError::ParseError(e.to_string()))?;

    let find = |key: &str| -> Option<&str> {
        footer
            .custom_metadata()
            .into_iter()
            .flatten()
            .find(|kv| kv.key() == Some(key))
            .and_then(|kv| kv.value())
    };
    let meta = |key: &'static str| -> Result<&str> {
        find(key).ok_or(Error::MissingMetadata {
            path: path.to_path_buf(),
            key,
        })
    };

    // First, before the CRC and before a single byte of the body is read: the
    // version is what says the rest of this file means what this binary thinks
    // it means, so nothing else here is worth checking until it has passed.
    check_format(path, find(FORMAT_KEY))?;

    let expected = u32::from_str_radix(meta(CRC_KEY)?, 16).map_err(|_| Error::MissingMetadata {
        path: path.to_path_buf(),
        key: CRC_KEY,
    })?;
    let body_len: usize = meta(CRC_LEN_KEY)?
        .parse()
        .ok()
        .filter(|&n: &usize| n <= footer_start)
        .ok_or(Error::MissingMetadata {
            path: path.to_path_buf(),
            key: CRC_LEN_KEY,
        })?;
    let actual = crc32fast::hash(&buffer[..body_len]);
    if actual != expected {
        return Err(Error::BadChecksum {
            path: path.to_path_buf(),
            expected,
            actual,
        });
    }

    // Everything from here on comes out of the *body*, not out of the footer.
    //
    // Extending the CRC over the footer was the obvious fix and it is not
    // possible without leaving the format: the checksum lives in the footer's
    // own custom metadata, and a checksum cannot cover the bytes it is stored
    // in. The two places outside the footer are a trailer appended after the
    // final `ARROW1` magic — which makes the file unreadable to every other
    // Arrow implementation, including arrow-rs's own `FileReader`, since they
    // all seek to `len - 10` — and a sidecar file per table, which doubles the
    // file count and the fsyncs and reintroduces the two-file atomicity problem
    // this design does not otherwise have.
    //
    // The other direction is free: an Arrow IPC file's body is *self-describing*
    // and holds a second copy of everything the footer says. The schema is
    // written as the first message as well as into the footer; the messages
    // chain, so the table of contents is derivable; and all of that is inside
    // `[0, body_len)`, which the CRC has just verified. So rather than covering
    // the footer, stop reading it. What is left of it is its custom metadata,
    // and that is self-checking — a corrupt CRC or length fails the CRC, and a
    // corrupt version is refused by [`check_format`].
    //
    // The schema is the half that mattered for memory safety: with validation
    // skipped below, a footer schema naming a wider type than the body holds
    // builds an array over a buffer too short for it, which is undefined
    // behaviour rather than an `Error::BadChecksum`.
    let first = message_at(path, &buffer, HEADER_LEN, body_len)?;
    if first.header != arrow_ipc::MessageHeader::Schema {
        return Err(corrupt(
            path,
            format!(
                "the body starts with {:?}, not a schema message",
                first.header.variant_name().unwrap_or("an unknown message")
            ),
        ));
    }
    let schema = Arc::new(
        arrow_ipc::convert::try_schema_from_ipc_buffer(&first.data).map_err(|e| {
            corrupt(
                path,
                format!("no readable schema at the head of the body: {e}"),
            )
        })?,
    );

    // Skipping validation is what keeps this a mmap read: with it on, every
    // Utf8 column runs `std::str::from_utf8` over the whole values buffer — a
    // sequential scan that faults in every page of string data, which is
    // precisely what demand paging was supposed to avoid.
    //
    // SAFETY: only `with_skip_validation` is unsafe here. It turns off offset
    // bounds, buffer length and UTF-8 checks, so the arrays built below are
    // trusted rather than verified, and an out-of-range offset would read
    // arbitrary memory. What backs the trust is the CRC checked above: it
    // covers `[0, body_len)`, which is every byte the IPC writer emitted before
    // `finish()` — schema, dictionary and record-batch messages and their
    // bodies (see `write_table_with`, which snapshots the CRC at exactly that
    // point). Those bytes are therefore provably the ones arrow-rs's own writer
    // produced from arrays it had already validated, and by the paragraph above
    // they are the only bytes the decoder is shown.
    let mut decoder = unsafe {
        FileDecoder::new(schema, first.version)
            .with_require_alignment(true)
            .with_skip_validation(true)
    };

    // Write order is decode order: `FileWriter` emits a dictionary before the
    // batch that refers to it, so one pass down the chain feeds the decoder in
    // the order it needs. `body_len` is the writer's position at the CRC
    // snapshot, which is the end of the last message — the end-of-stream marker
    // and the footer are emitted by `finish()`, after it — so the walk stops
    // exactly where the messages do.
    let mut batches = Vec::new();
    let mut offset = HEADER_LEN + first.stride();
    while offset < body_len {
        let msg = message_at(path, &buffer, offset, body_len)?;
        offset += msg.stride();
        let fail = |source| Error::Undecodable {
            path: path.to_path_buf(),
            source,
        };
        match msg.header {
            arrow_ipc::MessageHeader::DictionaryBatch => {
                decoder
                    .read_dictionary(&msg.block, &msg.data)
                    .map_err(fail)?;
            }
            arrow_ipc::MessageHeader::RecordBatch => {
                if let Some(rb) = decoder
                    .read_record_batch(&msg.block, &msg.data)
                    .map_err(fail)?
                {
                    batches.push(rb);
                }
            }
            other => {
                return Err(corrupt(
                    path,
                    format!(
                        "{} at offset {offset} is not a dictionary or a record batch",
                        other.variant_name().unwrap_or("an unknown message")
                    ),
                ));
            }
        }
    }

    Ok(MappedTable {
        batches,
        mapping: base..base + len,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use arrow_array::{StringArray, UInt32Array};
    use arrow_schema::{DataType, Field, Schema};

    fn dir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("mira-blk-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    /// Two columns and a string, so that a footer schema naming a wider type
    /// than the body holds has something to be wrong about.
    fn batch() -> RecordBatch {
        let schema = Arc::new(Schema::new(vec![
            Field::new("id", DataType::UInt32, false),
            Field::new("body", DataType::Utf8, true),
        ]));
        RecordBatch::try_new(
            schema,
            vec![
                Arc::new(UInt32Array::from(vec![1, 2, 3])),
                Arc::new(StringArray::from(vec![Some("a"), None, Some("ccc")])),
            ],
        )
        .unwrap()
    }

    /// The same two columns, wide and repetitive enough that ZSTD beats the
    /// plain bytes. `compress_to_vec` keeps the uncompressed copy whenever the
    /// frame comes out larger, so [`batch`] compressed has no frame in it at
    /// all and nothing for the cold-tier test to corrupt.
    fn compressible() -> RecordBatch {
        let n = 4_096u32;
        RecordBatch::try_new(
            batch().schema(),
            vec![
                Arc::new(UInt32Array::from_iter_values(0..n)),
                Arc::new(StringArray::from_iter_values(
                    (0..n).map(|_| "the same line of telemetry, over and over"),
                )),
            ],
        )
        .unwrap()
    }

    fn sealed(min_ts: i64, max_ts: i64) -> Sealed {
        Sealed {
            tables: vec![("logs", batch())],
            sidecars: vec![],
            min_ts,
            max_ts,
            num_rows: 3,
        }
    }

    /// A message header tag past `ENUM_MAX_MESSAGE_HEADER`, so that
    /// `variant_name()` has no name for it either — the reader has to say
    /// something useful about a message type that does not exist, not just
    /// about one it did not want.
    const UNKNOWN_MESSAGE: u8 = 42;

    /// Where the footer starts, by the same three fields the reader uses.
    fn footer_start(bytes: &[u8]) -> usize {
        let n = bytes.len();
        let footer_len = i32::from_le_bytes(bytes[n - 10..n - 6].try_into().unwrap()) as usize;
        n - 10 - footer_len
    }

    /// The two numbers in the footer that describe the checksummed region:
    /// where the CRC's own eight hex digits live in the file, and how many
    /// bytes they cover.
    fn crc_field(bytes: &[u8]) -> (usize, usize) {
        let start = footer_start(bytes);
        let footer = root_as_footer(&bytes[start..bytes.len() - 10]).unwrap();
        let md = footer.custom_metadata().unwrap();
        let find = |key: &str| {
            md.iter()
                .find(|kv| kv.key() == Some(key))
                .and_then(|kv| kv.value())
                .unwrap()
        };
        let hex = find(CRC_KEY);
        assert_eq!(hex.len(), 8, "the CRC is written as eight hex digits");
        (
            hex.as_ptr() as usize - bytes.as_ptr() as usize,
            find(CRC_LEN_KEY).parse().unwrap(),
        )
    }

    /// Make the checksum agree with a body that has been changed underneath it.
    ///
    /// Repairing the CRC is the point of these tests rather than a way around
    /// them. Everything [`open_table`] checks *after* the checksum — the first
    /// message being a schema, the ones after it being dictionaries or batches,
    /// every length staying inside the covered region — exists for bytes the
    /// checksum cannot speak for: crc32 is 32 bits and collides, and the arrays
    /// below it are built with `with_skip_validation(true)`, so the first thing
    /// to notice a structurally wrong body would otherwise be a read off the
    /// end of the mapping. A mutation the checksum accepts is the only way to
    /// reach them, and it is the shape of the corruption they are for.
    ///
    /// The length is left alone deliberately: it is a decimal string of
    /// variable width, so anything that changes it changes the footer's size
    /// too. Every mutation here stays inside `[0, body_len)`.
    fn repair_crc(bytes: &mut [u8]) {
        let (at, body_len) = crc_field(bytes);
        let crc = crc32fast::hash(&bytes[..body_len]);
        bytes[at..at + 8].copy_from_slice(format!("{crc:08x}").as_bytes());
    }

    /// Where the `header_type` union tag of the message at `offset` sits.
    ///
    /// Found by probing rather than by decoding a vtable: the layout inside the
    /// message is arrow-rs's business and a search that *verifies its own
    /// result* cannot land on the wrong byte. Same technique as
    /// [`a_footer_schema_naming_a_wider_type_is_not_the_one_the_read_uses`].
    fn header_tag(bytes: &[u8], offset: usize) -> usize {
        let declared =
            i32::from_le_bytes(bytes[offset + 4..offset + 8].try_into().unwrap()) as usize;
        let meta = offset + 8..offset + 8 + declared;
        for i in meta.clone() {
            let mut probe = bytes.to_vec();
            probe[i] = UNKNOWN_MESSAGE;
            let framed = arrow_ipc::root_as_message(&probe[meta.clone()]);
            if framed.is_ok_and(|m| m.header_type().0 == UNKNOWN_MESSAGE) {
                return i;
            }
        }
        panic!("the message at {offset} has no header tag to corrupt");
    }

    /// The offset of the second message, which for every file this writes is
    /// the record batch: the schema is first and its stride leads here.
    fn second_message(path: &Path, bytes: &[u8]) -> usize {
        let covered = crc_field(bytes).1;
        let buffer = Buffer::from_vec(bytes.to_vec());
        HEADER_LEN
            + message_at(path, &buffer, HEADER_LEN, covered)
                .unwrap()
                .stride()
    }

    /// The error every corruption has to produce: named, and not a panic.
    fn invalid_data(e: &Error, what: &str) {
        let Error::Io { source, .. } = e else {
            panic!("{what}: {e}")
        };
        assert_eq!(source.kind(), io::ErrorKind::InvalidData, "{what}: {e}");
    }

    /// A footer length field that does not describe this file used to be a
    /// subtraction past zero and then a slice with the wrap-around — a panic,
    /// which `panic = "abort"` makes a process death and a crashloop, since the
    /// block is still there on the next start.
    #[test]
    fn a_corrupt_footer_length_is_an_error_not_a_panic() {
        let d = dir("footerlen");
        let path = d.join("logs.arrow");
        write_table(&path, &batch()).unwrap();

        let good = fs::read(&path).unwrap();
        for len in [i32::MAX, good.len() as i32, good.len() as i32 - 9] {
            let mut bytes = good.clone();
            let n = bytes.len();
            bytes[n - 10..n - 6].copy_from_slice(&len.to_le_bytes());
            fs::write(&path, &bytes).unwrap();
            let Err(e) = open_table(&path) else {
                panic!("a footer length of {len} in a {n}-byte file read as a block")
            };
            assert!(
                matches!(&e, Error::Io { source, .. }
                    if source.kind() == io::ErrorKind::InvalidData),
                "{e}"
            );
        }
        let _ = fs::remove_dir_all(&d);
    }

    /// The property the CRC cannot give directly, because it is stamped before
    /// the footer exists: every bit of the uncovered tail either fails the read
    /// or changes nothing about what the read returns. In particular the schema
    /// is the one the writer wrote — it comes from the covered copy at the head
    /// of the body — so no flip here can widen a column and build an array over
    /// a buffer too short for it.
    ///
    /// Exhaustive rather than sampled: the footer of this file is a few hundred
    /// bytes and the whole sweep is a fraction of a second, and the interesting
    /// bits (a length's high bit, an offset's sign bit) are exactly the ones a
    /// sample misses.
    #[test]
    fn no_bit_in_the_unchecked_footer_can_change_what_a_read_returns() {
        let d = dir("footerbits");
        let path = d.join("logs.arrow");
        let want = batch();
        write_table(&path, &want).unwrap();

        let good = fs::read(&path).unwrap();
        let (start, n) = (footer_start(&good), good.len());
        assert!(start < n - 10, "no footer to corrupt");
        let mut flipped = 0;
        for i in start..n {
            for bit in 0..8u8 {
                let mut bytes = good.clone();
                bytes[i] ^= 1 << bit;
                fs::write(&path, &bytes).unwrap();
                if let Ok(t) = open_table(&path) {
                    assert_eq!(
                        t.batches,
                        vec![want.clone()],
                        "byte {i} bit {bit} decoded to other data"
                    );
                } else {
                    flipped += 1;
                }
            }
        }
        // Not every flip has to be caught — a bit in a padding byte or in the
        // unused half of a flatbuffer field is genuinely harmless — but a run
        // where nothing at all was rejected would mean the checks above never
        // ran.
        assert!(flipped > 0, "no corrupt footer was rejected");
        let _ = fs::remove_dir_all(&d);
    }

    /// The corruption a CRC over `[0, body_len)` could never have caught, driven
    /// directly rather than waited for: a footer that parses cleanly and names a
    /// *wider* type than the body holds.
    ///
    /// `Utf8` and `LargeUtf8` are both empty flatbuffer tables, so a footer
    /// naming one and a footer naming the other differ in exactly one byte — the
    /// union tag — which is why this is reachable by a single bit flip and why it
    /// can be constructed by patching one. What it asks for is a buffer of 4-byte
    /// offsets read as 8-byte offsets: with the schema taken from the footer and
    /// `with_skip_validation(true)` below, the last offset is read off the end of
    /// the mapping. Undefined behaviour, not `Error::BadChecksum`.
    ///
    /// The assertion is the strong one — the read returns the *right* data, not
    /// merely an error — because the schema now comes from the checksummed copy
    /// at the head of the body and the footer's copy is read by nobody.
    #[test]
    fn a_footer_schema_naming_a_wider_type_is_not_the_one_the_read_uses() {
        let d = dir("widen");
        let path = d.join("logs.arrow");
        let want = batch();
        write_table(&path, &want).unwrap();

        let good = fs::read(&path).unwrap();
        let (start, end) = (footer_start(&good), good.len() - 10);
        // Raw flatbuffer accessors, not `fb_to_schema`: that one is a `todo!()`
        // on a type tag it does not know, so the probe for "did this patch land
        // on the tag" would itself panic on every patch that landed elsewhere.
        // Which is its own argument for keeping the footer schema out of the
        // read path — but here it is only in the way.
        let body_tag = |bytes: &[u8]| -> Option<u8> {
            let fields = root_as_footer(&bytes[start..end])
                .ok()?
                .schema()?
                .fields()?;
            let body = (fields.len() == 2).then(|| fields.get(1))?;
            (body.name() == Some("body")).then(|| body.type_type().0)
        };
        assert_eq!(body_tag(&good), Some(arrow_ipc::Type::Utf8.0));

        let mut widened = 0;
        for i in start..end {
            let mut bytes = good.clone();
            bytes[i] = arrow_ipc::Type::LargeUtf8.0;
            if body_tag(&bytes) != Some(arrow_ipc::Type::LargeUtf8.0) {
                continue;
            }
            widened += 1;
            fs::write(&path, &bytes).unwrap();
            let t = open_table(&path).expect("a widened footer schema is not read at all");
            assert_eq!(t.batches, vec![want.clone()], "byte {i} widened the read");
        }
        assert!(widened > 0, "the footer schema could not be widened");
        let _ = fs::remove_dir_all(&d);
    }

    /// The chain [`open_table`] walks instead of reading the footer, and the
    /// bounds every link in it is held to. Driven directly, because a body that
    /// declares a length running off the end of itself is not something a bit
    /// flip reaches often and it is the case that reads arbitrary memory.
    #[test]
    fn a_message_that_does_not_fit_the_checked_body_is_refused() {
        let d = dir("extent");
        let path = d.join("logs.arrow");
        write_table(&path, &batch()).unwrap();
        let bytes = fs::read(&path).unwrap();
        // What the CRC covers: everything up to the end-of-stream marker that
        // `finish()` writes just before the footer.
        let covered = footer_start(&bytes) - 8;
        let buffer = Buffer::from_vec(bytes);

        // The schema message, which every Arrow file has at the same place, and
        // then the record batch its stride leads to.
        let schema = message_at(&path, &buffer, HEADER_LEN, covered).unwrap();
        assert_eq!(schema.header, arrow_ipc::MessageHeader::Schema);
        assert_eq!(schema.block.metaDataLength() % ALIGNMENT as i32, 0);
        assert_eq!(schema.block.bodyLength(), 0);

        let at = HEADER_LEN + schema.stride();
        let rb = message_at(&path, &buffer, at, covered).unwrap();
        assert_eq!(rb.header, arrow_ipc::MessageHeader::RecordBatch);
        assert_eq!(rb.data.len(), rb.stride());
        // The property the walk rests on: the chain lands exactly on the end of
        // the checksummed region. If it did not, the loop would either stop
        // short of a batch or read one out of the uncovered tail.
        assert_eq!(
            at + rb.stride(),
            covered,
            "the chain does not end at the CRC"
        );

        let bad = |offset: usize, body_len: usize| {
            let Err(e) = message_at(&path, &buffer, offset, body_len) else {
                panic!("offset {offset} framed a message inside {body_len} covered bytes")
            };
            assert!(
                matches!(&e, Error::Io { source, .. }
                    if source.kind() == io::ErrorKind::InvalidData),
                "{e}"
            );
        };
        bad(covered, covered); // at the end
        bad(usize::MAX, covered); // and far past it, without wrapping
        bad(HEADER_LEN + 1, covered); // no framing there
        bad(HEADER_LEN, HEADER_LEN + 8); // metadata past the checksum
        bad(at, at + rb.block.metaDataLength() as usize); // body past the checksum
        let _ = fs::remove_dir_all(&d);
    }

    /// A version this binary does not understand has to be an error with a name
    /// on it. The alternative is the silent one: a block whose columns moved,
    /// read positionally, answers a query with the wrong column.
    #[test]
    fn a_newer_format_version_is_refused_and_an_older_one_is_not() {
        let path = Path::new("logs.arrow");
        // Written before the key existed. Byte-identical to version 1, so
        // refusing it would orphan every block on disk at upgrade time.
        check_format(path, None).unwrap();
        check_format(path, Some("1")).unwrap();

        let e = check_format(path, Some("2")).unwrap_err();
        assert!(
            matches!(&e, Error::Io { source, .. }
                if source.kind() == io::ErrorKind::Unsupported),
            "{e}"
        );
        let e = check_format(path, Some("banana")).unwrap_err();
        assert!(
            matches!(&e, Error::Io { source, .. }
                if source.kind() == io::ErrorKind::InvalidData),
            "{e}"
        );
    }

    /// A version is only worth stamping if somebody bumps it, and nothing in
    /// `schema.rs` — where the change that needs the bump gets made — says so.
    /// The readers that make it matter are somewhere else again: `attrs.rs`,
    /// `query.rs` and `series.rs` reach into the attribute tables by column
    /// *position* (`column(3)` is the `str` value), so inserting a field into
    /// [`crate::schema::ATTRS`] does not fail against the blocks already on
    /// disk — it reinterprets them, and a query answers with a different
    /// column's data and no error anywhere.
    ///
    /// So the rule is a test rather than a sentence in a doc comment three
    /// files away. Only `ATTRS` is pinned: the root tables are read through
    /// `column_by_name`, which is exactly why `LogRecord.event_name` could be
    /// added without rewriting a block, and pinning those here would be a
    /// tripwire on the wrong wire.
    #[test]
    fn the_positionally_read_columns_are_pinned_to_the_format_version() {
        let names: Vec<&str> = crate::schema::ATTRS
            .fields()
            .iter()
            .map(|f| f.name().as_str())
            .collect();
        assert_eq!(
            names,
            [
                "parent_id",
                "key",
                "type",
                "str",
                "int",
                "double",
                "bool",
                "bytes",
                "ser"
            ],
            "the attribute table's column order changed, and three readers take \
             those columns by index — so every block already written now decodes \
             with the wrong ones. Bump FORMAT_VERSION (currently {FORMAT_VERSION}), \
             put the compatibility branch for the old layout next to `check_format`, \
             and update this list."
        );
    }

    /// And the version actually reaches the disk, next to the CRC. Every table
    /// is written by [`write_table_with`], so one of them is all of them.
    #[test]
    fn a_written_table_carries_its_format_version() {
        let d = dir("version");
        let path = d.join("logs.arrow");
        write_table(&path, &batch()).unwrap();

        let bytes = fs::read(&path).unwrap();
        let footer = root_as_footer(&bytes[footer_start(&bytes)..bytes.len() - 10]).unwrap();
        let stamped = footer
            .custom_metadata()
            .unwrap()
            .iter()
            .find(|kv| kv.key() == Some(FORMAT_KEY))
            .and_then(|kv| kv.value().map(str::to_string));
        assert_eq!(
            stamped.as_deref(),
            Some(FORMAT_VERSION.to_string().as_str())
        );
        assert!(open_table(&path).is_ok());
        let _ = fs::remove_dir_all(&d);
    }

    /// A publish that fails after staging must take its staging directory with
    /// it. `expire` only ever scans `<root>/<signal>`, so anything left under
    /// `.tmp` is invisible to retention until the next boot sweep — and the
    /// flusher retries this every couple of seconds, which is what turns one
    /// full disk into a thousand leaked directories an hour.
    #[test]
    fn a_failed_publish_leaves_no_staging_directory() {
        let root = dir("leak");
        // A file where the signal directory goes, so `create_dir_all` on the
        // partition fails with the staging directory already written.
        fs::write(root.join("logs"), b"not a directory").unwrap();

        assert!(publish(&root, "logs", node_id("a"), 0, 0, &sealed(1_000, 2_000)).is_err());

        let staged: Vec<_> = fs::read_dir(root.join(".tmp"))
            .unwrap()
            .map(|e| e.unwrap().path())
            .collect();
        assert!(staged.is_empty(), "leaked {staged:?}");
        let _ = fs::remove_dir_all(&root);
    }

    /// A block directory written before the write-ahead log existed has four
    /// fields, not five, and it is still a block. There is no manifest to
    /// migrate and no version to bump, so the only place backward compatibility
    /// can live is here — and reading a legacy name as `wal_hi = 0` is not just
    /// lenient, it is the correct answer: nothing published by a Mira without a
    /// log covers any log sequence.
    #[test]
    fn a_block_name_from_before_the_log_parses_with_no_watermark() {
        let legacy = format!("{:020}-{:020}-{:08x}-{:012}", 10, 20, 0xabu32, 7u64);
        assert_eq!(parse_dir_name(&legacy), Some((10, 20, 0xab, 7, 0)));
        assert_eq!(
            parse_dir_name(&dir_name(10, 20, 0xab, 7, 99)),
            Some((10, 20, 0xab, 7, 99))
        );
        // A sixth field is not a name from the future to be read hopefully. The
        // format is positional, so guessing at one more would mean guessing at
        // what it means.
        assert_eq!(parse_dir_name(&format!("{legacy}-1-2")), None);
    }

    /// The watermark is the maximum over a signal's blocks, never the last one
    /// listed. `scan` sorts by `(min_ts, seq)`, so a block covering an older
    /// hour can be published after a newer one — a backlog replay does exactly
    /// that — and taking the last would hand replay a watermark below what is
    /// already on disk.
    #[test]
    fn the_watermark_is_the_highest_per_signal_not_the_newest() {
        let root = dir("watermark");
        let node = node_id("a");
        assert_eq!(wal_watermarks(&root).unwrap(), [0, 0, 0]);

        // Published second, timestamped first: `scan` puts this one at the
        // front, and its watermark is the low one.
        publish(&root, "logs", node, 0, 40, &sealed(5_000, 6_000)).unwrap();
        publish(&root, "logs", node, 1, 9, &sealed(1_000, 2_000)).unwrap();
        publish(&root, "traces", node, 0, 3, &sealed(1_000, 2_000)).unwrap();

        // Indexed by `wal::Signal`: logs, traces, metrics.
        assert_eq!(wal_watermarks(&root).unwrap(), [40, 3, 0]);
        let _ = fs::remove_dir_all(&root);
    }

    /// One block that cannot be read used to end the sweep for the whole
    /// signal: every block behind the broken one stayed uncompressed for ever,
    /// and the same early return in `expire` stopped reclaiming space.
    #[test]
    fn one_unreadable_block_does_not_stop_the_sweep() {
        let root = dir("badblock");
        let node = node_id("a");
        let bad = publish(&root, "logs", node, 0, 0, &sealed(1_000, 2_000))
            .unwrap()
            .dir;
        let good = publish(&root, "logs", node, 1, 0, &sealed(3_000, 4_000))
            .unwrap()
            .dir;
        // Not an Arrow file at all, which is what a table truncated by a full
        // disk looks like from here.
        fs::write(bad.join("logs.arrow"), b"junk").unwrap();

        assert_eq!(compact(&root, "logs", node, i64::MAX).unwrap(), 1);
        assert!(good.join(COLD_MARKER).exists());
        assert!(!bad.join(COLD_MARKER).exists(), "declared cold unread");
        // The invariant the skip must not break: a block that cannot be
        // compacted is still a block retention can take, because expiry never
        // opens a file.
        assert_eq!(expire(&root, "logs", i64::MAX).unwrap(), 2);
        assert!(scan(&root, "logs").unwrap().is_empty());
        let _ = fs::remove_dir_all(&root);
    }

    /// The same for a block that cannot be *deleted*: the rest of the signal
    /// still has to be swept, since a stuck retention is how a disk fills.
    #[test]
    fn one_undeletable_block_does_not_stop_retention() {
        use std::os::unix::fs::PermissionsExt;

        let root = dir("stuck");
        let node = node_id("a");
        // Two partitions, an hour apart, so one can be locked without locking
        // the other.
        let stuck = publish(&root, "logs", node, 0, 0, &sealed(1_000, 2_000))
            .unwrap()
            .dir;
        let free = publish(
            &root,
            "logs",
            node,
            1,
            0,
            &sealed(2 * NANOS_PER_HOUR, 2 * NANOS_PER_HOUR + 1),
        )
        .unwrap()
        .dir;

        let locked = stuck.parent().unwrap().to_path_buf();
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o555)).unwrap();
        // Root ignores the mode bits, so ask the filesystem rather than assume.
        if fs::write(locked.join("canary"), []).is_err() {
            assert_eq!(expire(&root, "logs", i64::MAX).unwrap(), 1);
            assert!(stuck.is_dir(), "the locked block is still there");
            assert!(!free.exists(), "the block beside it was still dropped");
        }
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o755)).unwrap();
        let _ = fs::remove_dir_all(&root);
    }

    /// Free space, for the ingest side to back off on. Only the shape can be
    /// asserted here — the number belongs to whatever volume the test runs on.
    #[test]
    fn free_fraction_is_a_fraction_of_a_real_mount() {
        let d = dir("free");
        let f = free_fraction(&d).unwrap();
        assert!(f > 0.0 && f <= 1.0, "{f} is not a fraction");
        // The temp dir and a file on it are the same mount.
        let path = d.join("logs.arrow");
        write_table(&path, &batch()).unwrap();
        assert!((free_fraction(&path).unwrap() - f).abs() < 0.01);
        // A path that is not there is an error, not a zero: "no space" and "no
        // such directory" are different operational answers.
        assert!(free_fraction(&d.join("nope")).is_err());
        let _ = fs::remove_dir_all(&d);
    }

    /// A path is bytes, not a string, and `statfs` takes a C string. An
    /// interior NUL would otherwise be truncated by the conversion and the
    /// answer would be about a *different* directory — the one whose name is
    /// the prefix — which is worse than a refusal because nothing about it
    /// looks wrong.
    #[test]
    fn a_path_with_a_nul_byte_is_refused_rather_than_truncated() {
        use std::os::unix::ffi::OsStrExt;

        let d = dir("nul");
        let mut raw = d.as_os_str().as_encoded_bytes().to_vec();
        raw.extend_from_slice(b"\0suffix");
        let nul = PathBuf::from(std::ffi::OsStr::from_bytes(&raw));

        // The prefix is a real directory this would happily answer about.
        assert!(free_fraction(&d).is_ok());
        let Err(e) = free_fraction(&nul) else {
            panic!("a path with a NUL byte was measured")
        };
        assert!(
            matches!(&e, Error::Io { source, .. }
                if source.kind() == io::ErrorKind::InvalidInput),
            "{e}"
        );
        let _ = fs::remove_dir_all(&d);
    }

    /// A pseudo-filesystem reports no blocks at all. Dividing by that is a NaN
    /// the ingest side would read as "not above the threshold" — or a 0% free
    /// that stops ingest on a node with nothing wrong with it — so a mount that
    /// cannot fill up answers "empty".
    ///
    /// Conditional on finding one, like
    /// [`one_undeletable_block_does_not_stop_retention`] is conditional on not
    /// being root: `/proc` is Linux's, `/System/Volumes/Data/home` is the
    /// autofs trigger macOS mounts by default, and a host with neither has
    /// nothing to assert this over.
    #[test]
    fn a_mount_that_reports_no_blocks_is_empty_not_full() {
        let pseudo = ["/proc", "/sys", "/System/Volumes/Data/home"]
            .into_iter()
            .map(Path::new)
            .find(|p| statfs(p).is_ok_and(|b| b.f_blocks == 0));
        if let Some(p) = pseudo {
            assert_eq!(free_fraction(p).unwrap(), 1.0, "{}", p.display());
        }
    }

    /// The rule [`check_filesystem`] applies, stated over the three answers
    /// [`fs_type`] can give. FUSE has to stay a warning — the magic number is
    /// the same for `gcsfuse`, which is as fatal as NFS, and for a local
    /// userspace filesystem, which is fine — and everything else it names has
    /// to be fatal, because `SIGBUS` under a mapping is not an error any Rust
    /// can catch.
    #[test]
    fn a_network_filesystem_refuses_to_start_and_fuse_only_warns() {
        let path = Path::new("/data");
        check_fs_type(path, None).unwrap();
        check_fs_type(path, Some("fuse".into())).unwrap();

        let e = check_fs_type(path, Some("NFS".into())).unwrap_err();
        assert!(
            matches!(&e, Error::NetworkFilesystem { fs, .. } if fs == "NFS"),
            "{e}"
        );
        // The operator has to be told which mount type, or the message is a
        // refusal with no next step in it.
        assert!(e.to_string().contains("NFS"), "{e}");
    }

    /// A cleanup that cannot run is not the failure the caller has to act on.
    /// Returning it would replace "the disk is full" with "could not remove a
    /// temporary directory", and the second is the one nobody can fix.
    #[test]
    fn a_staging_directory_that_cannot_be_removed_is_logged_not_returned() {
        use std::os::unix::fs::PermissionsExt;

        let root = dir("unwind");
        let tmp = root.join("logs-0000002a-000000000000-0-0");
        fs::create_dir(&tmp).unwrap();
        fs::write(tmp.join("logs.arrow"), b"a staged table").unwrap();

        fs::set_permissions(&root, fs::Permissions::from_mode(0o555)).unwrap();
        // Root ignores the mode bits, so ask the filesystem rather than assume.
        if fs::create_dir(root.join("canary")).is_err() {
            unwind_staging(&tmp);
            assert!(tmp.is_dir(), "the undeletable staging directory went away");
        }
        fs::set_permissions(&root, fs::Permissions::from_mode(0o755)).unwrap();

        unwind_staging(&tmp);
        assert!(!tmp.exists(), "the deletable one did not");
        // And the ordinary shape of "the rename succeeded and a later fsync did
        // not": nothing left to remove, nothing to say about it.
        unwind_staging(&tmp);
        let _ = fs::remove_dir_all(&root);
    }

    /// `publish` skips a zero-row table, and this is the same rule one level
    /// down. Writing the file anyway would put a header-and-footer-only Arrow
    /// file in the block for every table a signal does not use — which the
    /// reader would then open, map and CRC on every scan.
    #[test]
    fn a_table_with_no_batches_writes_no_file() {
        let d = dir("nobatch");
        let path = d.join("logs.arrow");
        write_table_with(&path, &[], None).unwrap();
        assert!(!path.exists(), "an empty table left a file behind");
        // Which is the same thing the reader sees for a table that was never
        // written, and it is not an error.
        assert!(open_table_opt(&path).unwrap().is_none());
        let _ = fs::remove_dir_all(&d);
    }

    /// Neither constructor can build this — [`Src::disk`] always has a
    /// directory and [`Src::open`] always has tables — so the arm is here for
    /// the day a third one does. It answers "no rows", which is the answer
    /// every other missing table gets, rather than unwrapping something that is
    /// not there.
    #[test]
    fn a_source_with_neither_a_directory_nor_tables_reads_as_empty() {
        let src = Src {
            node: 0,
            seq: 0,
            min_ts: 0,
            max_ts: 1,
            dir: None,
            tables: None,
        };
        assert!(src.load("logs").unwrap().is_none());
        assert!(src.overlaps(0, 1));
    }

    /// The sweep is capped so the first pass over an existing volume does not
    /// saturate the disk, and the cap counts *attempts*: a directory full of
    /// unreadable blocks must not re-read every one of them on every sweep for
    /// ever. What the cap must not do is lose blocks — the ones it did not
    /// reach are still there for the next pass.
    #[test]
    fn a_sweep_compacts_at_most_its_budget_and_the_next_one_finishes() {
        let root = dir("budget");
        let node = node_id("a");
        let blocks = MAX_COMPACT_PER_SWEEP + 1;
        for seq in 0..blocks as u64 {
            publish(&root, "logs", node, seq, 0, &sealed(1_000, 2_000)).unwrap();
        }

        assert_eq!(
            compact(&root, "logs", node, i64::MAX).unwrap(),
            MAX_COMPACT_PER_SWEEP
        );
        let cold = |root: &Path| {
            scan(root, "logs")
                .unwrap()
                .iter()
                .filter(|b| b.dir.join(COLD_MARKER).exists())
                .count()
        };
        assert_eq!(cold(&root), MAX_COMPACT_PER_SWEEP);

        // The next sweep skips the eight already marked and takes the ninth.
        assert_eq!(compact(&root, "logs", node, i64::MAX).unwrap(), 1);
        assert_eq!(cold(&root), blocks);
        // And a third has nothing left to do, which is what the marker is for.
        assert_eq!(compact(&root, "logs", node, i64::MAX).unwrap(), 0);
        let _ = fs::remove_dir_all(&root);
    }

    /// Retention on this node or another replica can unlink a block between the
    /// `scan` that listed it and the rewrite that would have compacted it.
    /// Racing to delete an immutable block is not a conflict, so it must not be
    /// logged as a failure — and the sweep has to carry on to the blocks
    /// behind it.
    ///
    /// A block directory that is a dangling symlink stands in for the race: it
    /// is what `scan` sees for a name whose directory is no longer there, which
    /// is precisely the state the race leaves behind.
    #[test]
    fn a_block_that_vanished_mid_sweep_is_not_a_failure() {
        let root = dir("vanished");
        let node = node_id("a");
        let good = publish(&root, "logs", node, 0, 0, &sealed(1_000, 2_000))
            .unwrap()
            .dir;
        let gone = good
            .parent()
            .unwrap()
            .join(dir_name(1_000, 2_000, node, 99, 0));
        std::os::unix::fs::symlink(root.join("no-such-block"), &gone).unwrap();
        assert_eq!(scan(&root, "logs").unwrap().len(), 2);
        // Pinned to the exact error the sweep swallows. Anything else would
        // take the warning arm instead, which is the same visible outcome and
        // the wrong one: a routine race logged as a block that cannot be
        // compacted is a false alarm on every retention pass.
        assert!(
            matches!(compact_block(&gone, node), Err(Error::Io { source, .. })
                if source.kind() == io::ErrorKind::NotFound),
        );

        assert_eq!(compact(&root, "logs", node, i64::MAX).unwrap(), 1);
        assert!(good.join(COLD_MARKER).exists(), "the block beside it");
        assert!(!gone.join(COLD_MARKER).exists(), "declared cold unread");
        let _ = fs::remove_dir_all(&root);
    }

    /// The 8-byte encapsulation prefix carries a *signed* metadata length read
    /// straight off the disk. Negative, or large enough that adding the prefix
    /// back overflows, has to be a named error before it reaches
    /// `Buffer::slice_with_length` — which panics on a range it does not like,
    /// and `panic = "abort"` makes that the death of every open block in the
    /// process.
    #[test]
    fn a_metadata_length_that_is_not_a_length_is_refused() {
        let d = dir("metalen");
        let path = d.join("logs.arrow");
        write_table(&path, &batch()).unwrap();
        let good = fs::read(&path).unwrap();
        let covered = crc_field(&good).1;

        for declared in [-1i32, -8, i32::MIN, i32::MAX, i32::MAX - 7] {
            let mut bytes = good.clone();
            bytes[HEADER_LEN + 4..HEADER_LEN + 8].copy_from_slice(&declared.to_le_bytes());
            let buffer = Buffer::from_vec(bytes);
            let Err(e) = message_at(&path, &buffer, HEADER_LEN, covered) else {
                panic!("a metadata length of {declared} framed a message")
            };
            invalid_data(&e, &format!("metadata length {declared}"));
            // The diagnostic has to carry the number, or it says nothing about
            // which of the file's several lengths is the broken one.
            assert!(e.to_string().contains(&declared.to_string()), "{e}");
        }
        let _ = fs::remove_dir_all(&d);
    }

    /// The reader takes the schema from the head of the *body*, so the first
    /// thing it has to be sure of is that the body starts with a schema at all.
    /// Anything else there would be handed to `try_schema_from_ipc_buffer` and
    /// then to a decoder built with validation off, which is the combination
    /// that reads off the end of the mapping rather than erroring.
    #[test]
    fn a_body_that_does_not_start_with_a_schema_is_refused() {
        let d = dir("noschema");
        let path = d.join("logs.arrow");
        write_table(&path, &batch()).unwrap();

        let mut bytes = fs::read(&path).unwrap();
        let tag = header_tag(&bytes, HEADER_LEN);
        bytes[tag] = UNKNOWN_MESSAGE;
        repair_crc(&mut bytes);
        fs::write(&path, &bytes).unwrap();

        let Err(e) = open_table(&path) else {
            panic!("a body with no schema at the head of it read as a block")
        };
        invalid_data(&e, "a first message that is not a schema");
        assert!(e.to_string().contains("not a schema message"), "{e}");
        let _ = fs::remove_dir_all(&d);
    }

    /// And every message after it is a dictionary or a record batch. A third
    /// kind is not something to skip: the chain is walked by stride, so a
    /// message the reader does not understand is a message whose length it is
    /// trusting without having understood what it describes.
    #[test]
    fn a_message_after_the_schema_that_is_not_a_batch_is_refused() {
        let d = dir("notabatch");
        let path = d.join("logs.arrow");
        write_table(&path, &batch()).unwrap();

        let mut bytes = fs::read(&path).unwrap();
        let at = second_message(&path, &bytes);
        let tag = header_tag(&bytes, at);
        bytes[tag] = UNKNOWN_MESSAGE;
        repair_crc(&mut bytes);
        fs::write(&path, &bytes).unwrap();

        let Err(e) = open_table(&path) else {
            panic!("a message that is neither a dictionary nor a batch decoded")
        };
        invalid_data(&e, "a message that is not a batch");
        assert!(
            e.to_string()
                .contains("is not a dictionary or a record batch"),
            "{e}"
        );
        let _ = fs::remove_dir_all(&d);
    }

    /// The cold tier's own corruption. A compressed block is the one case where
    /// the body is not read out of the mapping, so it is also the one case
    /// where a byte the checksum happens to accept lands in a decompressor
    /// rather than in an array — and a ZSTD frame that will not decompress has
    /// to be an error naming the failure, not a short buffer an array is then
    /// built over.
    #[test]
    fn a_zstd_frame_that_will_not_decompress_is_an_error() {
        let d = dir("zstd");
        let path = d.join("logs.arrow");
        let want = compressible();
        write_table_zstd(&path, &want).unwrap();
        assert_eq!(open_table(&path).unwrap().batches, vec![want]);

        let mut bytes = fs::read(&path).unwrap();
        let covered = crc_field(&bytes).1;
        // The start of a ZSTD frame, which is what `compress_to_vec` writes
        // after the 8-byte uncompressed length — unless the frame came out
        // bigger than the plain bytes, which is why the batch above is one
        // that compresses.
        let frame = bytes[..covered]
            .windows(4)
            .position(|w| w == [0x28, 0xb5, 0x2f, 0xfd])
            .expect("no compressed frame in a compressed table");
        bytes[frame..frame + 4].copy_from_slice(&[0xff; 4]);
        repair_crc(&mut bytes);
        fs::write(&path, &bytes).unwrap();

        let Err(e) = open_table(&path) else {
            panic!("a block with a broken ZSTD frame decoded")
        };
        // One variant carries every way arrow-rs can refuse a body, so the
        // headline has to be true of all of them and the wrapped source is
        // what says which one this is. Pinned as the substring rather than
        // the whole sentence so a zstd wording change is not a red build.
        assert!(matches!(&e, Error::Undecodable { .. }), "{e}");
        assert!(e.to_string().contains("frame"), "{e}");
        assert!(!e.to_string().contains("align"), "{e}");
        let _ = fs::remove_dir_all(&d);
    }
}
