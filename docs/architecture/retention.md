# 6. Retention worker

A 60-second tick that does three things per signal, in this order:

1. **Expire.** Compute `now - ttl`, call `scan()`, and `remove_dir_all` every
   block whose `max_ts` is older. With `--offload`, the copy to the object store
   is the hook that runs just before each unlink (section 12.4).
2. **Compact.** Rewrite up to `MAX_COMPACT_PER_SWEEP = 8` blocks per signal whose
   `max_ts` is older than `COLD_AFTER_NS`, table by table, ZSTD into a `.tmp` and
   rename. Expire runs first on purpose: compressing a block this sweep is about
   to delete is pure wasted bandwidth.
3. **Reclaim.** One `statfs`. If free space is under `MIN_FREE = 10%`, drop
   oldest-first until it is not, logging every block and why. `--offload`
   deliberately does not apply here — an unreachable store must not stop the
   floor from reclaiming.

TTL is a directory unlink, so step 1 costs no IO bandwidth. Step 2 reads and
writes live data, and section 11 prices it: the bound of eight blocks keeps that
bandwidth a constant an operator can reason about rather than a function of how
far behind the sweep is.

**No reader lease protocol is needed.** POSIX specifies that `mmap()` adds a
reference to the file that `close()` does not remove, persisting until the last
mapping goes away, so a query holding an `Arc<Mmap>` keeps reading correct data
out of an unlinked file. The one rule: never truncate or rewrite a published
block — that gives readers `SIGBUS`, whereas unlinking does not.

## 6.1 `--offload`: a copy before the unlink

`--offload <uri>` puts a copy of a block in an object store immediately before
retention unlinks it. One flag, because a URI is an address, and the only
tiering knob there will be: no offload period, no cache path, no cache size, no
eviction policy. The period is `storage.retention`, because the block leaving
the disk *is* the event.

### The naming is the catalogue

An offloaded block lands at `<uri>/<signal>/p=<epoch_hour>/<block>` — byte for byte the layout of section
3.2, time range still in the directory name. The store's own list API is
therefore the manifest exactly as `readdir` is locally: `mira offload list` is
`block::scan` pointed at the other root, parsing `min_ts`, `max_ts`, node and
sequence back out of the names it gets. Nothing is written that a future binary
has to understand and nothing records what has been uploaded.

### The ordering is the design

The copy is a hook `expire_with` runs just
before each `remove_dir_all`, so the failure direction is fixed at *two copies,
never zero*: a copy that fails logs, keeps its block, and is retried next sweep.
A separate upload pass that marks what it has done needs a marker both passes
agree on, which is coordination state, which is what principle 4 spends
everything to avoid. Doing the copy inside the unlink makes the filesystem's own
presence and absence the marker.

### Staging, so a killed copy is not a block

Each block is written to
`<root>/.tmp/<signal>-<name>-<pid>` and renamed into place, so a partial
directory never appears in a listing and never blocks the retry. A rename that
loses to another replica's is success, not an error — both wrote the same
immutable bytes. The restore path stages under the same `<node:08x>-restore-`
shape `block::sweep_staging` already clears at boot, so a killed restore costs
no new code.

### `file://` only, and that is not a placeholder

Everything after the prefix is a path, so `file:///srv/cold`, `file://./cold` and a bucket already mounted
into the filesystem all work. `s3://` is refused at startup for the dependency
budget rather than the effort: signing a request needs HMAC-SHA256 and reading a
listing needs an XML parser, and neither is in the crate graph the README
counts. An operator who wants S3 mounts it; `mira` never learns what a bucket is.

### An offloaded block is not `mmap`-able

Nothing pretends otherwise: reads never consult the store. `mira offload restore` copies blocks back into a data
directory and the server picks them up on its next scan — the whole retrieval
path. No transparent fetch, no cache tier, no partially-local block. That is
also what keeps section 9's refusal to `mmap` a networked filesystem intact: the
mapped file is always the local one.

### `mira offload push` is the same copy with the ends swapped

It unlinks nothing. The retention hook offloads a block because that block was about to
be deleted; the verb offloads a whole data directory because the *volume* is —
most often one a scale-in left behind, holding blocks no query can reach any
more (section 12.4). Push them under a URI of their own, `restore` them into a
node that is still running, and they are back in a catalogue something reads.

Deleting the local copy afterwards is the obvious next step, and it is wrong. A
node derives two numbers from the blocks it still holds and neither survives an
emptied directory. `wal_watermarks` returns `0` for a signal with no block, so
the next boot replays a log whose frames were absorbed long ago. And the block
sequence resumes at `max(seq) + 1` over the local scan, so the node reissues
`(node, seq)` pairs that are still alive wherever they were copied — the pair
the cursor's total order is built on. Freeing space on a volume that is about to
be deleted does not begin to pay for those. The verb copies; deleting the volume
is the operator's next step anyway, and the only unlink in the procedure.

### The copy is a `read`/`write` loop on purpose

Not `fs::copy`. On macOS
`fs::copy` is `fclonefileat`/`fcopyfile(COPYFILE_ALL)` and *preserves mtime*, so
a block restored from a two-month-old offload would land with a two-month-old
stamp on bytes written seconds ago. Nothing in this tree reads mtime in anger —
retention keys on the `max_ts` in the directory name — but a restore is the one
operation that changes what is at a path, and `(len, mtime)` is how everything
*outside* Mira notices that: `rsync`, a backup agent, `find -mtime`, any cache
keyed on a path's identity. Writing the bytes makes the stamp current as a
consequence of the write, with no call anyone has to remember.
`offload_restore_stamps_current_mtime` fails if someone reaches for the faster
call.

The measured cost of all of it is in section 11.

---
