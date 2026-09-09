//! Immutable block storage.
//!
//! A block is a *directory* holding one Arrow IPC file per table:
//!
//! ```text
//! <root>/logs/p=<epoch_hour>/<min_ts:020>-<max_ts:020>-<node:08x>-<seq:012>/
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
/// Only for the cold tier (§11): a compressed block cannot be read out of its
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
/// LZ4; see the decisions table in `docs/ARCHITECTURE.md`.
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
    w.write_metadata(CRC_KEY, format!("{crc:08x}"));
    w.write_metadata(CRC_LEN_KEY, len.to_string());
    w.finish()?;

    let mut buf = w.into_inner()?;
    buf.flush().ctx(path)?;
    buf.inner
        .into_inner()
        .map_err(|e| Error::Io {
            path: path.to_path_buf(),
            source: e.into_error(),
        })?
        .sync_all()
        .ctx(path)?;
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
}

impl BlockRef {
    /// True if this block could contain a row in `[from, to]`. The whole point
    /// of the naming scheme: pruning without opening a single file.
    pub fn overlaps(&self, from: i64, to: i64) -> bool {
        self.min_ts <= to && self.max_ts >= from
    }
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

/// `{min_ts}-{max_ts}-{node}-{seq}`.
///
/// `node` is what makes two active replicas sharing a volume safe. Without it,
/// two writers allocate the same `seq` and the second `rename` lands on a
/// non-empty directory — `ENOTEMPTY`, and a node that can never publish again.
fn dir_name(min_ts: i64, max_ts: i64, node: u32, seq: u64) -> String {
    format!("{min_ts:020}-{max_ts:020}-{node:08x}-{seq:012}")
}

fn parse_dir_name(name: &str) -> Option<(i64, i64, u32, u64)> {
    let mut parts = name.split('-');
    let min = parts.next()?.parse().ok()?;
    let max = parts.next()?.parse().ok()?;
    let node = u32::from_str_radix(parts.next()?, 16).ok()?;
    let seq = parts.next()?.parse().ok()?;
    if parts.next().is_some() {
        return None;
    }
    Some((min, max, node, seq))
}

fn fsync_dir(path: &Path) -> Result<()> {
    File::open(path).ctx(path)?.sync_all().ctx(path)
}

/// Atomically publish a set of tables as one block under `<root>/<signal>/`.
///
/// Returns the published directory. The caller must not acknowledge the
/// originating OTLP export until this returns: OTLP's retryable status set
/// covers exports in flight at a crash, so acking before durability is the one
/// window in which data is silently lost with the client believing otherwise.
pub fn publish(
    root: &Path,
    signal: &str,
    node: u32,
    seq: u64,
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
        f.sync_all().ctx(&path)?;
    }
    fsync_dir(&tmp)?;

    let signal_dir = root.join(signal);
    let partition = signal_dir.join(format!("p={}", min_ts.div_euclid(NANOS_PER_HOUR)));
    fs::create_dir_all(&partition).ctx(&partition)?;
    let dir = partition.join(dir_name(min_ts, max_ts, node, seq));
    fs::rename(&tmp, &dir).ctx(&dir)?;
    fsync_dir(&partition)?;
    // And the directory naming the partition. fsyncing `partition` persists the
    // entries inside it, not the entry for it in its own parent — so on the
    // first block of a new hour the block is durable and the directory holding
    // it is unflushed metadata, which loses an acked block on XFS and btrfs
    // (ext4's ordered journal happens to cover it). One extra fsync per 32 MB
    // block, against the per-table fsyncs already paid.
    fsync_dir(&signal_dir)?;

    Ok(BlockRef {
        dir,
        min_ts,
        max_ts,
        node,
        seq,
    })
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

/// Rebuild the catalog from the filesystem. This is the entire boot sequence for
/// the read path: no manifest to replay, no WAL to recover.
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
            let Some((min_ts, max_ts, node, seq)) = dir
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
                Err(e) => {
                    return Err(Error::Io {
                        path: block.dir,
                        source: e,
                    });
                }
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
    let Some(fs) = fs_type(path)? else {
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
/// The probe carries the pid because replicas may share a directory (§10) and
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

/// The mount's filesystem type, if it is one of the ones that matter. `None`
/// means "nothing to say about it", which is every local filesystem.
fn fs_type(path: &Path) -> Result<Option<String>> {
    // statfs(2) needs the path to exist; the caller creates the data directory
    // before this runs, but a bare `Ok(None)` if it does not is friendlier than
    // an ENOENT that says nothing about filesystems.
    if !path.exists() {
        return Ok(None);
    }
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
/// Measured on real blocks (`cargo run --release -p mira-core --example tier`):
/// 0.127 of the plain size for logs, 0.142 for traces, at ~900 MiB/s on one
/// core. Reads of the compressed block came back *faster* than of the plain one
/// — 8× fewer pages to fault and 8× fewer bytes to CRC more than pays for the
/// decompression — so the tier costs the read path nothing except the zero-copy
/// property, and that only for data old enough that nothing is scanning it.
///
/// Crash safety is the trick `publish` already uses: write beside the target,
/// then rename. A crash leaves a directory with some tables compressed and some
/// not, which reads correctly — the codec is per-batch IPC metadata, not a
/// property of the directory — and the absent marker makes the next sweep
/// finish the job.
pub fn compact(root: &Path, signal: &str, node: u32, cutoff_ns: i64) -> Result<usize> {
    let mut done = 0;
    for block in scan(root, signal)? {
        if done == MAX_COMPACT_PER_SWEEP {
            break;
        }
        if block.max_ts >= cutoff_ns || block.dir.join(COLD_MARKER).exists() {
            continue;
        }
        match compact_block(&block.dir, node) {
            Ok(()) => done += 1,
            // Expired out from under the sweep, by this node's own retention or
            // another replica's. Nothing to compact is not a failure.
            Err(Error::Io { source, .. }) if source.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(e),
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
    File::create(dir.join(COLD_MARKER))
        .ctx(dir)?
        .sync_all()
        .ctx(dir)?;
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
    let footer_start = trailer - footer_len;
    let footer = root_as_footer(&buffer[footer_start..trailer])
        .map_err(|e| arrow_schema::ArrowError::ParseError(e.to_string()))?;

    let meta = |key: &'static str| -> Result<&str> {
        footer
            .custom_metadata()
            .into_iter()
            .flatten()
            .find(|kv| kv.key() == Some(key))
            .and_then(|kv| kv.value())
            .ok_or(Error::MissingMetadata {
                path: path.to_path_buf(),
                key,
            })
    };
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

    let schema = Arc::new(arrow_ipc::convert::fb_to_schema(footer.schema().ok_or(
        Error::MissingMetadata {
            path: path.to_path_buf(),
            key: "schema",
        },
    )?));

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
    // produced from arrays it had already validated.
    //
    // The gap is the tail. The CRC is stamped before the footer exists, so the
    // footer flatbuffer — the schema and the block offsets used below — is
    // outside the checked range. Most corruption there still fails loudly: the
    // `root_as_footer` above runs the flatbuffer verifier, an offset outside the
    // file panics in `slice_with_length`, and one that lands on non-message
    // bytes fails verification in `read_message`. What is not covered is a
    // corrupt-but-verifiable footer — a schema that names a wider type than the
    // body holds would decode past the end of a buffer with the checks off.
    // Extending the CRC over the footer is the fix if that stops being
    // theoretical.
    let decoder = unsafe {
        FileDecoder::new(schema, footer.version())
            .with_require_alignment(true)
            .with_skip_validation(true)
    };
    let mut decoder = decoder;

    for block in footer.dictionaries().iter().flatten() {
        let n = block.bodyLength() as usize + block.metaDataLength() as usize;
        let data = buffer.slice_with_length(block.offset() as usize, n);
        decoder
            .read_dictionary(block, &data)
            .map_err(|source| Error::Misaligned {
                path: path.to_path_buf(),
                source,
            })?;
    }

    let mut batches = Vec::new();
    for block in footer.recordBatches().iter().flatten() {
        let n = block.bodyLength() as usize + block.metaDataLength() as usize;
        let data = buffer.slice_with_length(block.offset() as usize, n);
        if let Some(rb) =
            decoder
                .read_record_batch(block, &data)
                .map_err(|source| Error::Misaligned {
                    path: path.to_path_buf(),
                    source,
                })?
        {
            batches.push(rb);
        }
    }

    Ok(MappedTable {
        batches,
        mapping: base..base + len,
    })
}
