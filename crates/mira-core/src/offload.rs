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

    pub fn root(&self) -> &Path {
        &self.root
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
    pub fn push(&self, signal: &str, b: &BlockRef) -> Result<bool> {
        let Some((partition, name)) = split(&b.dir) else {
            return Ok(false);
        };
        let dest_dir = self.root.join(signal).join(partition);
        let dest = dest_dir.join(name);
        if dest.exists() {
            return Ok(false);
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
    pub fn pull(&self, signal: &str, b: &BlockRef, data_dir: &Path, node: u32) -> Result<bool> {
        let Some((partition, name)) = split(&b.dir) else {
            return Ok(false);
        };
        let dest_dir = data_dir.join(signal).join(partition);
        let dest = dest_dir.join(name);
        if dest.exists() {
            return Ok(false);
        }
        let staging = format!("{signal}-{node:08x}-restore-{name}");
        copy_block(&b.dir, data_dir, &dest_dir, &dest, &staging)
    }
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
            return Err(Error::Io {
                path: tmp,
                source: e,
            });
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
            Err(e) => {
                return Err(Error::Io {
                    path: dest.into(),
                    source: e,
                });
            }
        }
        fsync_dir(dest_dir)?;
        Ok(true)
    });

    if copied.is_err() {
        let _ = fs::remove_dir_all(&tmp);
    }
    copied
}

fn copy_files(src: &Path, dst: &Path) -> Result<()> {
    for entry in fs::read_dir(src).ctx(src)? {
        let from = entry.ctx(src)?.path();
        // A block directory is flat: `<table>.arrow` files, the sidecars and
        // the `cold` marker. Anything else in there is not part of the block
        // and copying it would put bytes in the store that no reader opens.
        if !from.is_file() {
            continue;
        }
        let Some(name) = from.file_name() else {
            continue;
        };
        copy_file(&from, &dst.join(name))?;
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

        fs::remove_dir_all(&dir).unwrap();
        assert!(t.pull("logs", &listed[0], &local, 7).unwrap());
        assert_eq!(fs::read(dir.join("logs.arrow")).unwrap(), b"hello");
        // Sidecars ride along: they are optional to the reader, but a block
        // that comes back without them comes back slower.
        assert_eq!(fs::read(dir.join("attr.idx")).unwrap(), b"sidecar");
        assert_eq!(block::scan(&local, "logs").unwrap(), vec![b]);
        assert!(!t.pull("logs", &listed[0], &local, 7).unwrap());
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

    /// The invariant section 3.3 states: anything that reconstructs a file at
    /// a path this process may already have verified must stamp a current
    /// `mtime` rather than preserve a stored one.
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
