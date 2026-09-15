# 10. What is deliberately not here

## `fetch`, and four of the seven expanders

The algebra itself is built and
section 7.3 says so in its title: `mira_core::frame` is `Frame`, `anchor`,
`expand` over `Traces`/`Peers`/`Around`, `map`, `names_of` and `entities`,
served at `POST /api/v1/frame`, `/api/v1/map` and `/api/v1/entities`. What is
not there is `fetch` — a `Frame` holds identities and there is no operation
that renders the records for them, so every widening is another scan — and the
four expanders section 7.3 cut with its reasons beside them.

## A block cache

Every query re-opens every block it touches. The fix is a
process-local `Arc<MappedTable>` map invalidated by `expire`. This was assumed
to be the next big win and it is not: section 11 measures the per-block cost as
dominated by faulting the mapping in, which the `MADV_WILLNEED` hint already
addresses. Half of what such a cache would have saved is taken already and
separately — the CRC is verified once per file per process (section 3.3), which
needs no cached mapping and so has none of a cache's invalidation surface. What
is left to save is the `mmap` and the two child indexes: real, small.

## A dependency on `otel-arrow-dfe-quiver` 0.54.1

It is an embeddable
Arrow segment store from the OTel Arrow maintainers, Apache-2.0, and it already
ships a CRC32 WAL with replay, immutable IPC segments, `SegmentReader::open_mmap`,
64-byte `STREAM_ALIGNMENT`, `MADV_DONTNEED` on release and a disk budget. It is
the single best piece of prior art here and its `ARCHITECTURE.md` is worth
reading before touching the block format. Mira does not depend on it because:
it pins `arrow ^58.3` (incompatible with 59.x in one graph), its API is
explicitly unstable pre-1.0, its subscriber model does not match, and — the
decisive gap it names itself — **it carries no statistics and no time index**,
which is precisely Mira's differentiator. What Mira takes from it is the format
decisions, which this document already reflects.
Related: do **not** depend on `otel-arrow-dfe-pdata`; it pulls
`datafusion ^53` non-optionally for two imports.

## DataFusion, in any build

Section 1. It is not *rejected* — 50.0 MiB and
271 crates behind a cargo feature would be paid only by whoever asks for SQL,
and DataFusion 55 pins arrow v59.3.0, exactly Mira's pin, so there would be no
second Arrow in the tree. But no such feature exists: `crates/mira` declares
`default = []` and `webhook-tls`, `datafusion` is in no manifest and no
lockfile, and `--features sql` does not resolve. It is a shape held open, not a
build option.

## The `F_FULLFSYNC` fallback

Section 9. Not a weaker fsync — the opposite, and it
was settled by measuring rather than by reading. On this machine
`File::sync_all()` costs 4,230 us, `fcntl(F_FULLFSYNC)` 4,213 us and a bare
`libc::fsync(2)` 28 us: `sync_all` *is* `F_FULLFSYNC` on Apple targets, so
section 11's ack latencies are measured against the stronger barrier and nothing is
owed for the ordinary case. The Docker-for-Mac `EINVAL`/`ENOTSUP` path section 9
names **is** written, and so is the counting section 9 insists on:
`mira_core::sync_all`/`sync_data` wrap every sync in `block.rs` and `wal.rs`,
degrade to `libc::fsync` on exactly those two errnos, and report the count as
`degraded_syncs` on `/api/v1/stats`. So is the `statfs` guard beside it:
`block::check_filesystem`, called once before anything is mapped. What stays on
this list is the *weaker* fsync — trading the barrier for throughput by
default, which would make an ack mean less than it says.

## Hand-written SIMD on the decode path

Protobuf varint decoding is
inherently serial — each field's length says where the next begins — and
branchy on wire type, so there is no vector formulation of the inner loop. The
published SIMD protobuf work targets fixed-width packed repeated fields, which
OTLP barely uses. Section 0 row 4 settled the adjacent claim: zero-copy ingest
is impossible here because `prost` memcpies every string. Where SIMD would pay
is scan-side predicate evaluation, and the route there is not intrinsics — it
is keeping those loops autovectorizable, a tight loop over `&[i64]` with no
bounds checks and no branches, which LLVM turns into NEON unasked.

## `io_uring`

Three reasons, in order. It is not a feature flag but a second
runtime: `tokio-uring` has `!Send` futures and its own `start()`, so gating it
means two spellings of every I/O path rather than a `#[cfg]`. It is blocked
where Mira runs — Docker's default seccomp profile has denied `io_uring_setup`
since the 2023 escape CVEs, and GKE Autopilot and most hardened clusters
disable it outright, so performance would depend on whether the sandbox
allows a syscall. And Mira is not I/O-bound: 190 MiB/s against an NVMe that
does GB/s, on 1.75 of twelve cores. Revisit when a profile shows syscall
overhead above ~5% of ingest CPU. The third reason used to name the log's
group commit as the trigger; that fix is rejected on the numbers two entries
into the plateau discussion below, so the trigger is now the flusher measuring
submission-bound rather than device-bound.

## ~~The query-side half of `NO_IDENTITY`~~

Built. `Frame::add_entity`
(`frame.rs:82`) drops the sentinel with the reason this entry asked for — "an
entity set containing the sentinel means *every resource nobody described*,
which is not an entity" — and `frame::entities` exposes `resources.key` on
`POST /api/v1/entities`, the MCP `list_services` tool and the TUI, so there is
now something for it to be refused by. The entry is kept struck through rather
than deleted because the reasoning for the refusal is what section 7.2 is
pointing at.

---
