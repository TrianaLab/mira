//! Copy a sealed block to an object store before retention unlinks it.
//!
//! The whole module is one idea. A block directory's name already carries its
//! own time range, node and sequence (section 3.2), so a flat namespace of
//! those names *is* the catalog: [`Target::list`] is one call to the store's
//! own list API and nothing here writes an index, a manifest or a marker
//! beside the blocks. There is no second thing to keep consistent, which is
//! what principle 4 asks for — kill the process mid-sweep and the only state
//! is what the store lists, which is the same answer before and after.
//!
//! Four limits, all deliberate:
//!
//! * Only sealed, immutable blocks already past the retention cutoff are
//!   uploaded. The hot path does not reach this module and the upload happens
//!   on the retention worker's `spawn_blocking` thread, once a minute.
//! * An offloaded block is never mapped. [`Target::pull`] copies it back into
//!   the data directory and the read path opens it there, so section 9's
//!   refusal to `mmap` a networked filesystem holds unchanged: this is a read
//!   of bytes, not a mapping of them.
//! * A block is deleted locally only after its copy is visible in the store
//!   under its final name. The direction of the failure is fixed — a crash
//!   leaves two copies, never zero.
//! * `file://` is the only scheme implemented. See [`Target::parse`].

use std::fs::{self, File};
use std::io;
use std::path::{Path, PathBuf};

use crate::block::{self, BlockRef};
use crate::error::{Error, IoContext, Result};

/// Where offloaded blocks go. Parsed from `--offload`.
#[derive(Debug, Clone)]
pub struct Target {
    root: PathBuf,
}

impl Target {
    /// Parse an offload URI.
    ///
    /// Everything after `file://` is the path, so `file:///srv/cold` is
    /// absolute and `file://./cold` is relative to the working directory.
    /// There is no host field; a mounted bucket, an NFS export or a second
    /// local volume are all the same thing to this module.
    ///
    /// `s3://` is not implemented, and the error says so rather than failing
    /// later with something vaguer. The reason is the dependency budget the
    /// README states as a product property: this tree has no HTTP client, no
    /// HMAC/SHA implementation and no XML parser, and SigV4 signing plus
    /// `ListObjectsV2` response parsing needs all three. That is not an
    /// argument that it should never exist — it is the number whoever adds it
    /// has to be willing to move, stated where they will hit it.
    pub fn parse(uri: &str) -> Result<Target> {
        match uri.strip_prefix("file://") {
            Some(p) if !p.is_empty() => Ok(Target {
                root: PathBuf::from(p),
            }),
            _ => Err(Error::OffloadScheme { uri: uri.into() }),
        }
    }

    /// Every block the store holds for `signal`, oldest first.
    ///
    /// This is [`block::scan`] against the store's root and nothing else: the
    /// same `readdir` that rebuilds the local catalog at boot rebuilds the
    /// remote one, because the two namespaces are the same namespace. If this
    /// function needed anything the local one does not, the naming scheme
    /// would not be carrying its weight.
    pub fn list(&self, signal: &str) -> Result<Vec<BlockRef>> {
        block::scan(&self.root, signal)
    }

    /// Copy one local block into the store. `false` if it was already there.
    ///
    /// Staged under `.tmp` and renamed, for the reason [`block::publish`]
    /// stages: the store's list API is the catalog, so a half-copied block
    /// visible under its final name is a catalog entry that does not open.
    ///
    /// "Already there" is checked against the bytes, not only the name,
    /// because `false` is what licenses the caller to unlink the local copy —
    /// `block::expire_with` on the retention sweep, the operator deleting a
    /// drained replica's volume on `mira offload push` exiting 0. A replica
    /// re-created on a fresh volume keeps its node id and restarts its
    /// sequence, so it reissues `(node, seq)` pairs the store may still hold,
    /// and a name taken by somebody else's rows is the one case where "already
    /// archived" is a lie with a volume behind it.
    pub fn push(&self, signal: &str, b: &BlockRef) -> Result<bool> {
        let Some((partition, name)) = split(&b.dir) else {
            return Ok(false);
        };
        let dest_dir = self.root.join(signal).join(partition);
        let dest = dest_dir.join(name);
        if dest.exists() {
            return match differs(&b.dir, &dest)? {
                None => Ok(false),
                Some(why) => Err(Error::OffloadCollision { dest, why }),
            };
        }
        copy_block(
            &b.dir,
            &self.root,
            &dest_dir,
            &dest,
            &format!("{signal}-{name}"),
        )
    }

    /// Copy one offloaded block back into a local data directory. `false` if
    /// it was already there.
    ///
    /// `node` only names the staging directory, so that a restore killed
    /// half-way leaves a directory [`block::sweep_staging`] already knows how
    /// to clear at the next start.
    ///
    /// It lands under [`block::name_covering_no_log`] rather than the name the
    /// store holds, because the log the stored name refers to is the one on the
    /// volume this block is being rescued *from*.
    pub fn pull(&self, signal: &str, b: &BlockRef, data_dir: &Path, node: u32) -> Result<bool> {
        let Some((partition, name)) = split(&b.dir) else {
            return Ok(false);
        };
        let name = block::name_covering_no_log(name);
        let dest_dir = data_dir.join(signal).join(partition);
        let dest = dest_dir.join(&name);
        if dest.exists() {
            return Ok(false);
        }
        let staging = format!("{signal}-{node:08x}-restore-{name}");
        copy_block(&b.dir, data_dir, &dest_dir, &dest, &staging)
    }
}

/// Why two copies of one block name are not the same block, or `None`.
///
/// Names and sizes, not content. A block is sealed and immutable, its files
/// are written once and the store's copy came from `copy_files` — so a byte
/// that differs under a name and a length that both match has no writer in
/// this design. Reading both sides to compare them would put the block's whole
/// size through the retention sweep once a minute for every block already in
/// the store, to rule out a case nothing can produce.
///
/// The same `is_file` filter as [`copy_files`], which walks past directories:
/// a stray one beside the tables is not part of the block and was never
/// copied, so counting it would report a mismatch against the store's faithful
/// copy of it.
fn differs(src: &Path, dest: &Path) -> Result<Option<String>> {
    let (mut ours, mut theirs) = (sizes(src)?, sizes(dest)?);
    // Names only, when one side has gone cold and the other has not: `compact`
    // ZSTD-encodes every table in place and drops the marker beside them, so no
    // size on either side is a size on the other. That is not an edge case — it
    // happens to every block a sweep pushed while it was still inside its own
    // hour, so comparing sizes across the boundary fails the *steady state*: an
    // error on every subsequent sweep, and retention unable to expire the block
    // it had already archived.
    //
    // ponytail: the name set is what is left, and it does not discriminate a
    // reissued `(node, seq)` — two blocks of the same signal have the same
    // table names. Narrower than the check it replaces, and only on the
    // handful of blocks mid-boundary; the fix is an identity in the block
    // rather than one derived from its bytes, which is a format change.
    let marker = |v: &[(String, u64)]| v.iter().any(|(n, _)| n == block::COLD_MARKER);
    let across = marker(&ours) != marker(&theirs);
    if across {
        ours.retain(|(n, _)| n != block::COLD_MARKER);
        theirs.retain(|(n, _)| n != block::COLD_MARKER);
    }
    for (name, len) in &ours {
        match theirs.iter().find(|(n, _)| n == name) {
            None => return Ok(Some(format!("{name} is missing from it"))),
            Some((_, there)) if !across && there != len => {
                return Ok(Some(format!("{name} is {there} bytes there, {len} here")));
            }
            Some(_) => {}
        }
    }
    let extra = theirs
        .iter()
        .find(|(n, _)| !ours.iter().any(|(o, _)| o == n));
    Ok(extra.map(|(n, _)| format!("it holds {n}, which this block does not")))
}

/// `(file name, length)` for the regular files directly in `dir`, sorted.
fn sizes(dir: &Path) -> Result<Vec<(String, u64)>> {
    let mut out = Vec::new();
    for entry in fs::read_dir(dir).ctx(dir)? {
        let entry = entry.ctx(dir)?;
        let meta = entry.metadata().ctx(entry.path())?;
        if meta.is_file() {
            out.push((entry.file_name().to_string_lossy().into_owned(), meta.len()));
        }
    }
    out.sort();
    Ok(out)
}

/// `(partition, block name)` out of `…/<signal>/p=<hour>/<block>`.
fn split(dir: &Path) -> Option<(&std::ffi::OsStr, &str)> {
    let name = dir.file_name()?.to_str()?;
    let partition = dir.parent()?.file_name()?;
    Some((partition, name))
}

/// Stage a whole block directory into `root/.tmp`, then rename it into place.
fn copy_block(
    src: &Path,
    root: &Path,
    dest_dir: &Path,
    dest: &Path,
    staging_name: &str,
) -> Result<bool> {
    let staging = root.join(".tmp");
    // The pid, because two replicas sharing a volume both see this block in
    // their own sweep and both will try to copy it. `publish` solves the same
    // collision by putting the content's identity in the staging name; here
    // the content's identity is already in `staging_name` and what has to be
    // distinguished is the two *copiers* of identical bytes.
    let tmp = staging.join(format!("{staging_name}-{}", std::process::id()));
    fs::create_dir_all(&staging).ctx(&staging)?;
    // Unlike `publish`, clearing a leftover here is unambiguous: the source is
    // an immutable sealed block and the destination name is derived from it, so
    // a directory left by a killed copy of *this* block can only hold a prefix
    // of the bytes about to be written again.
    if let Err(e) = fs::remove_dir_all(&tmp) {
        if e.kind() != io::ErrorKind::NotFound {
            return Err(e).ctx(tmp);
        }
    }
    fs::create_dir(&tmp).ctx(&tmp)?;

    let copied = copy_files(src, &tmp).and_then(|()| {
        fsync_dir(&tmp)?;
        fs::create_dir_all(dest_dir).ctx(dest_dir)?;
        match fs::rename(&tmp, dest) {
            Ok(()) => {}
            // The other replica won the race described above and put the same
            // immutable bytes there first. `rename` onto a non-empty directory
            // is `ENOTEMPTY`, which is the same outcome as the `exists` check
            // at the top of `push` and is not a failure of this sweep.
            Err(_) if dest.exists() => return Ok(false),
            Err(e) => return Err(e).ctx(dest),
        }
        fsync_dir(dest_dir)?;
        Ok(true)
    });

    // Unconditional: on success the rename took it, on `Ok(false)` the bytes
    // are already there under someone else's copy, and on error it is a
    // prefix. Leaving it costs a whole block of store until the next
    // `sweep_staging`, and this is one `rmdir` on a directory that is usually
    // already gone.
    let _ = fs::remove_dir_all(&tmp);
    copied
}

fn copy_files(src: &Path, dst: &Path) -> Result<()> {
    for entry in fs::read_dir(src).ctx(src)? {
        let entry = entry.ctx(src)?;
        let from = entry.path();
        // A block directory is flat: `<table>.arrow` files, the sidecars and
        // the `cold` marker. Anything else in there is not part of the block
        // and copying it would put bytes in the store that no reader opens.
        // `DirEntry::file_name` rather than `Path::file_name` because the first
        // cannot fail and the second returns an `Option` nothing can produce.
        if from.is_file() {
            copy_file(&from, &dst.join(entry.file_name()))?;
        }
    }
    Ok(())
}

/// Copy one file's bytes, giving the destination a current modification time.
///
/// Deliberately not `fs::copy`. On macOS that is `fcopyfile(COPYFILE_ALL)`,
/// which carries the source's `mtime` across to the copy — so a restore lands
/// a two-month-old stamp on bytes written seconds ago. Retention keys on the
/// `max_ts` in the directory name, so nothing here changes either way; what
/// changes is every reader *outside* this process that treats `(len, mtime)` as
/// a path's identity, because a restore is precisely the case where a path's
/// contents change. Writing the bytes rather than cloning the file makes the
/// timestamp current as a consequence of the write, with no call anyone has to
/// remember. `offload_restore_stamps_current_mtime` fails if this becomes
/// `fs::copy`.
fn copy_file(from: &Path, to: &Path) -> Result<()> {
    let mut r = File::open(from).ctx(from)?;
    let mut w = File::create(to).ctx(to)?;
    io::copy(&mut r, &mut w).ctx(to)?;
    // The local copy is about to be unlinked, so this one is the only one.
    crate::sync_all(&w).ctx(to)
}

fn fsync_dir(path: &Path) -> Result<()> {
    crate::sync_all(&File::open(path).ctx(path)?).ctx(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, SystemTime};

    fn block(root: &Path, signal: &str, min_ts: i64, seq: u64, body: &[u8]) -> PathBuf {
        let dir = root
            .join(signal)
            .join(format!("p={}", min_ts / 3_600_000_000_000))
            .join(format!(
                "{min_ts:020}-{:020}-{:08x}-{seq:012}-{:020}",
                min_ts + 1,
                7,
                0
            ));
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("logs.arrow"), body).unwrap();
        fs::write(dir.join("attr.idx"), b"sidecar").unwrap();
        dir
    }

    #[test]
    fn only_file_scheme_is_implemented() {
        assert!(Target::parse("file:///srv/cold").is_ok());
        let e = Target::parse("s3://bucket/prefix").unwrap_err().to_string();
        // The message has to name the scheme that was asked for and the one
        // that works, because the operator reading it typed the first.
        assert!(
            e.contains("s3://bucket/prefix") && e.contains("file://"),
            "{e}"
        );
        assert!(Target::parse("file://").is_err());
    }

    #[test]
    fn push_list_pull_round_trips() {
        let tmp = tempdir("rt");
        let (local, store) = (tmp.join("data"), tmp.join("cold"));
        let dir = block(&local, "logs", 7_200_000_000_000, 1, b"hello");
        let t = Target::parse(&format!("file://{}", store.display())).unwrap();

        // A block directory is flat, so anything with children in it belongs to
        // something else — a half-written compaction, an editor's backup dir.
        // It is walked past rather than recursed into, because the copy's cost
        // is the block's size and nothing here bounds what a stray directory
        // holds. Mutation check: recurse and this shows up in the store.
        fs::create_dir(dir.join("not-a-table")).unwrap();
        fs::write(dir.join("not-a-table").join("junk"), b"x").unwrap();

        let b = block::scan(&local, "logs").unwrap().remove(0);
        assert!(t.push("logs", &b).unwrap());
        // Idempotent: the second sweep sees it and does not copy it again.
        assert!(!t.push("logs", &b).unwrap());

        // The catalog is the store's own listing, with no index written.
        let listed = t.list("logs").unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].min_ts, b.min_ts);
        assert_eq!(listed[0].seq, b.seq);
        assert_eq!(listed[0].node, b.node);
        assert!(!store.join(".tmp").join("logs-x").exists());
        assert!(
            !listed[0].dir.join("not-a-table").exists(),
            "a directory inside the block was copied into the store"
        );

        fs::remove_dir_all(&dir).unwrap();
        assert!(t.pull("logs", &listed[0], &local, 7).unwrap());
        assert_eq!(fs::read(dir.join("logs.arrow")).unwrap(), b"hello");
        // Sidecars ride along: they are optional to the reader, but a block
        // that comes back without them comes back slower.
        assert_eq!(fs::read(dir.join("attr.idx")).unwrap(), b"sidecar");
        assert_eq!(block::scan(&local, "logs").unwrap(), vec![b]);
        assert!(!t.pull("logs", &listed[0], &local, 7).unwrap());
    }

    /// A restored block describes a log that no longer exists.
    ///
    /// `wal_hi` is a position in the log of the volume that *wrote* the block,
    /// and a restore lands it beside a log that starts at sequence 0 — a
    /// re-created pod, a new claim. [`block::wal_watermarks`] takes the maximum
    /// over the names it finds and filters only by node, and `node` is a hash
    /// of `--node`, which the operator keeps stable across exactly this. So the
    /// fresh log is handed a watermark thousands of sequences ahead of itself,
    /// and the next replay skips every frame under it: acked data, dropped
    /// silently, in the one situation the log exists for.
    ///
    /// `publish` states the rule this rests on — too high is the dangerous
    /// direction, too low only costs a re-ingest.
    #[test]
    fn a_restored_block_claims_no_progress_in_the_log_it_lands_beside() {
        let tmp = tempdir("restore-watermark");
        let (old, store, fresh) = (tmp.join("old"), tmp.join("cold"), tmp.join("fresh"));
        let dir = old.join("logs").join("p=2").join(format!(
            "{:020}-{:020}-{:08x}-{:012}-{:020}",
            7_200_000_000_000u64, 7_200_000_000_001u64, 7, 1, 5000
        ));
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("logs.arrow"), b"hello").unwrap();

        let t = Target::parse(&format!("file://{}", store.display())).unwrap();
        let b = block::scan(&old, "logs").unwrap().remove(0);
        assert_eq!(b.wal_hi, 5000);
        assert!(t.push("logs", &b).unwrap());
        assert!(
            t.pull("logs", &t.list("logs").unwrap()[0], &fresh, 7)
                .unwrap()
        );

        assert_eq!(
            block::wal_watermarks(&fresh, 7).unwrap(),
            [0, 0, 0],
            "a fresh log at sequence 0 was told 5000 of its frames are already published"
        );
        // Still one block, still readable: only the claim about the log moved.
        let landed = block::scan(&fresh, "logs").unwrap();
        assert_eq!(landed.len(), 1);
        assert_eq!(
            fs::read(landed[0].dir.join("logs.arrow")).unwrap(),
            b"hello"
        );
    }

    /// A copy killed half-way leaves nothing in the catalog, and the next
    /// sweep still lands the block.
    ///
    /// This is the whole reason `copy_block` stages and renames instead of
    /// writing into the final name: the store's listing *is* the catalog, so a
    /// directory holding two of a block's eight tables under its real name is
    /// not a slow upload, it is a catalog entry that does not open. Mutation
    /// check: copy straight into `dest` and the first assertion here fails.
    #[test]
    fn a_killed_copy_is_not_in_the_listing_and_does_not_block_the_retry() {
        let tmp = tempdir("killed");
        let (local, store) = (tmp.join("data"), tmp.join("cold"));
        block(&local, "logs", 7_200_000_000_000, 1, b"hello");
        let t = Target::parse(&format!("file://{}", store.display())).unwrap();
        let b = block::scan(&local, "logs").unwrap().remove(0);

        // What a kill mid-copy leaves behind, built by hand because there is
        // no seam to fail the real one through: the staging directory this
        // push would use, holding a prefix of the block.
        let name = b.dir.file_name().unwrap().to_str().unwrap();
        let half = store
            .join(".tmp")
            .join(format!("logs-{name}-{}", std::process::id()));
        fs::create_dir_all(&half).unwrap();
        fs::write(half.join("logs.arrow"), b"hel").unwrap();
        assert!(t.list("logs").unwrap().is_empty(), "staging is not catalog");

        assert!(t.push("logs", &b).unwrap());
        let listed = t.list("logs").unwrap();
        assert_eq!(listed.len(), 1);
        // The whole block, not the three bytes the dead copy had written.
        assert_eq!(
            fs::read(listed[0].dir.join("logs.arrow")).unwrap(),
            b"hello"
        );
    }

    /// Three failure shapes a round trip cannot produce, and `copy_block` has
    /// to tell apart: a block whose directory cannot be named, a staging path
    /// that is not a directory, and another replica landing the same immutable
    /// bytes first. The third is the one worth the test — read as a failure it
    /// would keep a block that is already safely in the store.
    #[test]
    fn a_copy_that_cannot_land_says_which_way_it_failed() {
        let tmp = tempdir("fail");
        let (local, store) = (tmp.join("data"), tmp.join("cold"));
        block(&local, "logs", 7_200_000_000_000, 1, b"hello");
        let t = Target::parse(&format!("file://{}", store.display())).unwrap();
        let b = block::scan(&local, "logs").unwrap().remove(0);
        let name = b.dir.file_name().unwrap().to_str().unwrap().to_string();
        let staged = store
            .join(".tmp")
            .join(format!("logs-{name}-{}", std::process::id()));

        // A `BlockRef` with no partition above it has no address in the store.
        // `scan` cannot produce one; it is skipped rather than copied to a
        // name that would not list.
        let orphan = block::BlockRef {
            dir: PathBuf::from("loose"),
            ..b
        };
        assert!(!t.push("logs", &orphan).unwrap());
        assert!(!t.pull("logs", &orphan, &local, 7).unwrap());

        // Staging occupied by a *file*. Clearing a leftover directory is
        // unambiguous and silent; this is not a leftover, so it is an error
        // rather than something to delete.
        fs::create_dir_all(staged.parent().unwrap()).unwrap();
        fs::write(&staged, b"not a directory").unwrap();
        assert!(t.push("logs", &b).is_err());
        assert!(t.list("logs").unwrap().is_empty());
        fs::remove_file(&staged).unwrap();

        // The race the staging name's pid exists for: the other replica
        // renamed first, so `rename` gets `ENOTEMPTY` after this copy's
        // `exists` check already passed. Called through `copy_block` because
        // `push` returns at that check and never reaches the race.
        let (partition, name) = split(&b.dir).unwrap();
        let dest_dir = store.join("logs").join(partition);
        let dest = dest_dir.join(name);
        fs::create_dir_all(&dest).unwrap();
        fs::write(dest.join("logs.arrow"), b"theirs").unwrap();
        let staging = format!("logs-{name}");
        assert!(!copy_block(&b.dir, &store, &dest_dir, &dest, &staging).unwrap());
        // Theirs, untouched — and the copy this call made is not left behind
        // in `.tmp` holding a second copy of the block.
        assert_eq!(fs::read(dest.join("logs.arrow")).unwrap(), b"theirs");
        assert!(!staged.exists());
    }

    /// "Already there" is what licenses the unlink, so it has to be decided
    /// from the bytes and not from the name.
    ///
    /// A replica re-created on a fresh volume keeps its node id and restarts
    /// its sequence — `offload_cmd`'s own doc says it "reissues `(node, seq)`
    /// pairs that are still alive wherever they were copied" — so a second
    /// drain of the same ordinal can find the first drain's archive under the
    /// name the new block wants. Decided by name, that block is reported
    /// `present`, `mira offload push` exits 0, and the operator deletes a
    /// volume whose rows are in no store: the name in the catalog belongs to
    /// somebody else's bytes.
    ///
    /// Mutation check: drop the comparison and `push` returns `Ok(false)` for
    /// every case below.
    #[test]
    fn a_name_in_the_store_over_other_bytes_is_not_a_block_that_was_pushed() {
        let tmp = tempdir("collide");
        let (local, store) = (tmp.join("data"), tmp.join("cold"));
        block(&local, "logs", 7_200_000_000_000, 1, b"hello");
        let t = Target::parse(&format!("file://{}", store.display())).unwrap();
        let b = block::scan(&local, "logs").unwrap().remove(0);
        assert!(t.push("logs", &b).unwrap());
        let there = t.list("logs").unwrap().remove(0).dir;

        // Same names, different bytes. This is the collision itself.
        fs::write(there.join("logs.arrow"), b"someone else's rows").unwrap();
        let e = t.push("logs", &b).unwrap_err().to_string();
        assert!(
            e.contains("logs.arrow") && e.contains(&*there.to_string_lossy()),
            "{e}"
        );

        // A short copy under the final name, which staging is supposed to make
        // impossible and a store nobody else writes to would never hold.
        fs::write(there.join("logs.arrow"), b"hello").unwrap();
        fs::remove_file(there.join("attr.idx")).unwrap();
        assert!(t.push("logs", &b).is_err());

        // And a file the local block does not have. Restored to equality, the
        // push goes back to the idempotent `false` the sweep runs on.
        fs::write(there.join("attr.idx"), b"sidecar").unwrap();
        assert!(!t.push("logs", &b).unwrap());
        fs::write(there.join("stray"), b"x").unwrap();
        assert!(t.push("logs", &b).is_err());
    }

    /// The same block on the two sides of the cold boundary is the same block.
    ///
    /// `compact` ZSTD-encodes every table in place and drops a `cold` marker
    /// beside them, so a block pushed while hot and swept again after it aged
    /// out of its hour has the same table names and none of the same sizes.
    /// Compared byte-for-byte that reads as somebody else's rows under this
    /// block's name: `push` raises `OffloadCollision`, `mira offload push`
    /// exits non-zero, and the operator will not delete a drained replica's
    /// volume — whose blocks are, in fact, already archived.
    ///
    /// Mutation check: compare sizes across the boundary too, and every
    /// assertion below turns into an error.
    #[test]
    fn a_block_that_went_cold_after_it_was_pushed_is_still_that_block() {
        let tmp = tempdir("cold");
        let (local, store) = (tmp.join("data"), tmp.join("cold"));
        let dir = block(&local, "logs", 7_200_000_000_000, 1, b"hello");
        let t = Target::parse(&format!("file://{}", store.display())).unwrap();
        let b = block::scan(&local, "logs").unwrap().remove(0);
        assert!(t.push("logs", &b).unwrap());

        // What `compact_block` leaves behind: smaller tables, plus the marker.
        fs::write(dir.join("logs.arrow"), b"zstd").unwrap();
        fs::write(dir.join("cold"), b"").unwrap();
        assert!(!t.push("logs", &b).unwrap());

        // And the other direction, which is what a restore then a re-drain
        // does: the store holds the cold copy, the local one is hot again.
        let there = t.list("logs").unwrap().remove(0).dir;
        fs::write(there.join("logs.arrow"), b"zstd").unwrap();
        fs::write(there.join("cold"), b"").unwrap();
        fs::remove_file(dir.join("cold")).unwrap();
        fs::write(dir.join("logs.arrow"), b"hello").unwrap();
        assert!(!t.push("logs", &b).unwrap());

        // Still a collision when the *names* disagree. Compression cannot add
        // or drop a table, so this is the check that survives the boundary.
        fs::remove_file(there.join("attr.idx")).unwrap();
        assert!(t.push("logs", &b).is_err());
    }

    /// The invariant [section
    /// 6.1](https://miradb.dev/architecture/retention/#61-offload-a-copy-before-the-unlink)
    /// states: a restore rewrites what is at a path, so the path's `mtime` has
    /// to say the bytes are new.
    ///
    /// Mutation check: implement `copy_file` with `fs::copy` on macOS, or add
    /// any explicit `utimensat` that replays the source timestamp, and the
    /// restored file keeps 1990 and this fails.
    #[test]
    fn offload_restore_stamps_current_mtime() {
        let tmp = tempdir("mtime");
        let (local, store) = (tmp.join("data"), tmp.join("cold"));
        let dir = block(&local, "logs", 7_200_000_000_000, 1, b"hello");
        let t = Target::parse(&format!("file://{}", store.display())).unwrap();
        let b = block::scan(&local, "logs").unwrap().remove(0);
        t.push("logs", &b).unwrap();
        fs::remove_dir_all(&dir).unwrap();

        // 1990, so the assertion cannot pass by the copy happening to be quick.
        let listed = t.list("logs").unwrap();
        set_mtime(&listed[0].dir.join("logs.arrow"), 631_152_000);
        t.pull("logs", &listed[0], &local, 7).unwrap();

        let age = SystemTime::now()
            .duration_since(
                fs::metadata(dir.join("logs.arrow"))
                    .unwrap()
                    .modified()
                    .unwrap(),
            )
            .expect("restored mtime is in the past");
        assert!(
            age < Duration::from_secs(60),
            "restored mtime is stale: {age:?}"
        );
    }

    fn set_mtime(path: &Path, secs: i64) {
        let times = [
            libc::timeval {
                tv_sec: secs as libc::time_t,
                tv_usec: 0,
            },
            libc::timeval {
                tv_sec: secs as libc::time_t,
                tv_usec: 0,
            },
        ];
        let c = std::ffi::CString::new(path.to_str().unwrap()).unwrap();
        // SAFETY: `c` is a live NUL-terminated string and `times` is a live
        // array of exactly the two `timeval`s `utimes` reads. Both outlive the
        // call, which writes through neither pointer.
        assert_eq!(unsafe { libc::utimes(c.as_ptr(), times.as_ptr()) }, 0);
    }

    fn tempdir(tag: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!("mira-offload-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&p);
        fs::create_dir_all(&p).unwrap();
        p
    }
}
