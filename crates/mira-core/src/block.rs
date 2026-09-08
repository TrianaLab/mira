//! Immutable block storage.
//!
//! A block is a *directory* holding one Arrow IPC file per table:
//!
//! ```text
//! <root>/logs/p=<epoch_hour>/<min_ts:020>-<max_ts:020>-<seq:012>/
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
//! Publish is write-tmp / fsync-files / fsync-tmpdir / rename-dir / fsync-parent.
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
/// zero-copy mmap read path. Cold-tier compression is a separate, later decision.
pub fn write_table(path: &Path, batch: &RecordBatch) -> Result<()> {
    let file = File::create(path).ctx(path)?;
    let opts = IpcWriteOptions::try_new(ALIGNMENT, false, MetadataVersion::V5)?;
    let mut w = FileWriter::try_new_with_options(
        CrcWriter::new(BufWriter::new(file)),
        &batch.schema(),
        opts,
    )?;
    w.write(batch)?;

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
    min_ts: i64,
    max_ts: i64,
    tables: &[(&str, &RecordBatch)],
) -> Result<BlockRef> {
    // The staging name carries `node` for the same reason the final one does:
    // two replicas on one volume must not stage into the same directory.
    let tmp = root
        .join(".tmp")
        .join(format!("{signal}-{node:08x}-{seq:012}"));
    if tmp.exists() {
        fs::remove_dir_all(&tmp).ctx(&tmp)?;
    }
    fs::create_dir_all(&tmp).ctx(&tmp)?;

    for (name, batch) in tables {
        write_table(&tmp.join(format!("{name}.arrow")), batch)?;
    }
    fsync_dir(&tmp)?;

    let partition = root
        .join(signal)
        .join(format!("p={}", min_ts.div_euclid(NANOS_PER_HOUR)));
    fs::create_dir_all(&partition).ctx(&partition)?;
    let dir = partition.join(dir_name(min_ts, max_ts, node, seq));
    fs::rename(&tmp, &dir).ctx(&dir)?;
    fsync_dir(&partition)?;

    Ok(BlockRef {
        dir,
        min_ts,
        max_ts,
        node,
        seq,
    })
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

/// Open one table of a block with no buffer copies.
///
/// This is a blocking call that can take a hard page fault. It must never run on
/// a tokio worker: a cold fault stalls the whole OS thread with no yield point
/// and no signal to the runtime. Callers go through `spawn_blocking` or a
/// dedicated reader pool.
pub fn open_table(path: &Path) -> Result<MappedTable> {
    let file = File::open(path).ctx(path)?;
    // SAFETY: published blocks are immutable — never rewritten, never truncated,
    // and removed only by unlink, which POSIX guarantees leaves live mappings
    // valid. So the bytes under this mapping cannot change for its lifetime.
    let mmap = unsafe { Mmap::map(&file) }.ctx(path)?;

    if mmap.len() < MAGIC.len() + 10 || &mmap[..MAGIC.len()] != MAGIC {
        return Err(Error::BadMagic {
            path: path.to_path_buf(),
        });
    }

    let len = mmap.len();
    let base = mmap.as_ptr() as usize;
    let ptr = NonNull::new(mmap.as_ptr().cast_mut()).expect("mmap is never null");
    // SAFETY: the Arc<Mmap> handed over as the allocation owner keeps the pages
    // mapped for at least as long as any Buffer derived from them.
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

    // skip_validation is sound here because the CRC above already proves the
    // bytes are exactly what Mira wrote. Without it, every read of a Utf8 column
    // runs std::str::from_utf8 over the whole values buffer — a full sequential
    // scan that faults in every page of string data, which is precisely what
    // demand paging was supposed to avoid.
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
