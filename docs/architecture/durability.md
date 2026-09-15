# 9. Durability and failure model

| Failure | Behaviour |
| --- | --- |
| Crash mid-block | Unacked exports are re-sent by the client (OTLP retryable set). Nothing on disk is torn — the block was never renamed. `.tmp` is cleaned on next publish. |
| Crash mid-rename | Directory rename is atomic. Either state is consistent. |
| Bit rot in a block | CRC32 mismatch on open → typed error, not wrong answers. |
| Truncated / non-Arrow file | `ARROW1` check → typed error. |
| Disk full | `publish` fails; waiters get `UNAVAILABLE` + `RetryInfo` on 4317 and `503` + `Retry-After` on 4318. No partial block is visible. |
| Reader holds a block being expired | Safe by POSIX unlink semantics (section 6). |
| **SIGTERM / SIGINT** | Stop accepting, let in-flight exports reach their ack, then close the flusher channels so each open block is sealed and published. Bounded at 15 s. |
| **Network filesystem** | **Not safe, and refused.** mmap on NFS/CIFS/CephFS raises `SIGBUS` with no recovery path. `block::check_filesystem` runs one `statfs` on the data directory before anything is mapped and fails startup with the filesystem named — by `f_fstypename` on macOS, by `f_type` magic on Linux. FUSE warns instead of refusing: the magic is the same for `gcsfuse` (fatal) and a local userspace filesystem (fine). A heap-read fallback was considered and rejected — it would silently delete the property the whole design is built on, which is a worse failure than not starting. |
| **Unwritable data directory** | **Refused, at startup.** `create_dir_all` returns `Ok` for a directory that already exists whatever its mode, so a `readOnly` volume mount or a wrong-uid path otherwise reaches a listening socket and fails one export at a time under load. `block::check_writable` writes and removes a pid-named probe file next to the `statfs` call. Both are the same bet: a startup that refuses is cheaper to diagnose than a server that half-works. |

## The status is the retry policy

OTLP's retryable set is closed.
`UNAVAILABLE` is in it and `INTERNAL` is not, so a conformant exporter handed
`INTERNAL` for a full disk does not wait for the disk to have room — it drops the
batch it is holding and reports the loss as permanent. Which status a failed
publish answers with therefore decides whether the data survives, and every
reason a publish can fail — no space, `EIO`, a volume that went read-only — is
transient by that test. A panicking flush never reaches the decision: the release
profile is `panic = "abort"`, so it takes the process down and the export is
retried against the restart, the same path as any other crash. The one exception
is an export that cannot fit an *empty* block: 70,000 distinct attribute keys
against a `UInt16` dictionary will not fit the next one either, so it keeps
`INTERNAL`/`500`. Telling a sender to keep trying something that cannot work is
worse than telling it the truth, and it is the only case where the truth is
permanent.

## The WAL watermark, and which way it is allowed to be wrong

A block's fifth
directory field is `wal_hi`, and boot replays every frame at or above
`block::wal_watermarks`, the maximum over the blocks of that signal. The field is
exclusive: a block holding frame 0 claims 1. Too high and the frames it skipped
are gone; too low and they are ingested twice, duplicate rows the `(node, seq)`
dedupe rule cannot catch because the blocks genuinely differ. One of those is
recoverable and the other is not, so every choice here leans low.

`max(seq) + 1` stopped being correct the moment a signal had several flushers:
shards seal independently and out of order, so a block whose highest sequence is
40 says nothing about 39 sitting in a sibling's open block. The log tracks a set
instead — `Wal::pending`, one `BTreeSet` per signal of every sequence handed out
and not yet published. `watermark_for(signal, seqs)` answers with the oldest
member not in `seqs`, or `next_seq` when this block is the last of them. It runs
before the publish, so a sibling sealing in the same instant counts this block's
frames against its own answer; `published` retires a sequence only once the
rename is durable.

Replay puts a frame *back* into `pending` (`Wal::reframed`) before decoding it,
since a shard that sealed in between would step over it. A frame that fails to
decode is retired on the spot: it will never decode, and leaving it in the set
would pin the signal's watermark at its sequence for the life of the volume.

## Probes are not the UI, and the listening line is not a promise

`/health`
and `/readyz` were one handler once — Mira has no warm-up and no cluster to join,
so there is no state in which it is alive and not ready. A full volume is exactly
that state, so they are two answers now: liveness is a constant 200 plus the
per-signal shed and failed counts, so the probe and the log agree about how much
has been refused; readiness is the single question of whether an export can still
be made durable. Log replay is the one startup task with unbounded duration, and
it runs after both sockets are bound but *before* either accept loop starts, so a
probe during replay waits in the kernel backlog — never answered 200 about data
the process has not recovered yet.

They also exist so that something other than the UI answers at those paths. The
asset router 404s a path it does not know and deliberately has no SPA fallback:
every view in the app lives under the URL hash, so an unknown *path* is a probe
pointed somewhere wrong. Answering one with 200 and a page of HTML made every
wrong guess report success. Both listen sockets are bound before anything logs
`mira listening`, for the same reason: during a CrashLoopBackOff the log is all
the operator has, and a listening line in front of a bind that then fails is
dishonesty rather than terseness — the error names the address and the likely
cause instead.

## Shutdown is about duplicates, not loss

A hard kill loses nothing that was
acknowledged, because an ack *is* an fsync (section 4). It costs the other
direction: an export that was received, queued and then cut off is still sealed
and published by the drain, but the exporter saw a reset, and OTLP tells it to
retry — so every rolling restart would double-write whatever was in flight. Hence
the ordered sequence: stop accepting first, drain the servers, and only then
close the flusher channels. The graceful window is bounded below by
`max_block_age`, since a waiting export is waiting on a block that no new data
will grow once the listener is closed. SIGTERM is handled alongside SIGINT
because SIGTERM is what an orchestrator actually sends.

## macOS

Rust's `File::sync_all()` and `sync_data()` both compile to
`fcntl(F_FULLFSYNC)` on Apple targets: correct durability for free, and a large
throughput cliff, and why a single-record export's 2 s ack (section 4) is
`max_block_age` rather than the fsync. Docker-for-Mac volumes can return
`EINVAL`/`ENOTSUP`, and std will not fall back — `os_fsync` and `os_datasync` in
`sys/fs/unix.rs` are a bare `fcntl(F_FULLFSYNC)` on any Apple target — so a
publish that a plain `fsync(2)` would have satisfied fails, and the node goes
unready over a volume that works. `mira_core::sync_all` and `sync_data` are the
wrapper: try the barrier, degrade to `libc::fsync` on exactly those two errnos,
and count it. Every sync in `block.rs` and `wal.rs` goes through them.

The counter is the part that matters. A fallback nobody can see is silent
durability loss, worse than the failure it replaces, so `/api/v1/stats` reports
`degraded_syncs` — zero on a volume with write barriers, and non-zero is an
operator's evidence that the promise on this node is `fsync`'s rather than
`F_FULLFSYNC`'s. The terminal UI's node pane prints it in the header, but only
once it is non-zero: a line reading `0` on every healthy node is a line the eye
learns to skip.

---
