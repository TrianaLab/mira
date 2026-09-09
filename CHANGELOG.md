# Changelog

Notable changes to Mira, in the format of
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), versioned by
[Semantic Versioning](https://semver.org/spec/v2.0.0.html).

Mira has never been released. There are no tags, no published binaries and no
`0.1.0`; the workspace version is `0.0.1` and the only thing that exists is the
tip of `main`. Everything below is therefore under `Unreleased`, and that is
the honest shape of this file rather than an oversight. Being pre-1.0, anything
here may change: the block format, the query document, the config keys, the
`/mcp` tool set.

## [Unreleased]

### Added

- **OTLP ingest for logs, traces and metrics** over OTLP/gRPC `4317` and
  OTLP/HTTP `4318`, protobuf or proto3-JSON, plain or gzipped — which is what a
  stock OpenTelemetry Collector sends, since both its OTLP exporters compress
  by default.
- **Storage as the OTAP star schema in Apache Arrow IPC.** Blocks are
  immutable, sealed on size or age, published by rename, and read back out of
  `mmap` with no buffer copies on the hot tier — asserted by a test that walks
  every buffer of every column and requires all of them to point inside the
  mapping.
- **Durable acknowledgement.** An export returns once its block is fsynced and
  renamed into place. No write-ahead log, because there is no torn state to
  replay; read-your-writes falls out of it.
- **Block pruning.** The directory name is the time index, and a Bloom sidecar
  per block answers "could this hold that trace id, that attribute value"
  before anything is opened. A damaged or missing sidecar reads as "scan me".
- **A cold tier in the same format.** The retention sweep rewrites aged blocks
  ZSTD-compressed in place, to about 0.13 of their size. The codec is per-batch
  IPC metadata, so no reader is told which tier it is on, and an interrupted
  rewrite leaves a directory that still reads correctly.
- **Retention by unlink**, dropping whole block directories; in-flight readers
  keep working, guaranteed by POSIX.
- **A query API** on `POST /api/v1/query`: attribute predicates, time bounds,
  keyset pagination via `next`/`after`, and a `stats` object per response that
  makes the pruning visible. An unknown top-level key is refused by name rather
  than answered over the unfiltered window.
- **MCP on `/mcp`** — four tools over JSON-RPC, no session id, so any replica
  can answer any call, through the same read path the UI uses.
- **A browser UI served from the binary** — records, trace waterfalls and
  metric charts out of `include_bytes!`, with the built bundle committed under
  `crates/mira/ui/dist` so there is no Node toolchain in the build.
- **A terminal UI**, `mira mira`: the same three tabs, filter grammar and trace
  waterfall over `termios` raw mode and ANSI, with no TUI framework and zero
  crates added. It reads a running replica over `--addr` or a block directory
  in-process over `--data-dir`, so a detached volume is still readable with
  nothing running.
- **Correlation edges on the read path** — a span comes back with its events
  and links, a metric series with the exemplars naming the traces behind it.
- **Nested attributes and structured log bodies** decoded from OTLP `AnyValue`
  and returned as nested JSON by the query API and MCP, printed inline by both
  UIs.
- **KYAML configuration** with `${env:VAR,default}` interpolation, every flag
  also settable from the file — [`docs/CONFIG.md`](docs/CONFIG.md) is the whole
  surface.
- **SIGTERM drains**: stop accepting, let in-flight exports reach their ack,
  seal and publish the open blocks, exit.
- **`/health` and `/readyz`**, the same 200 and the same JSON body of per-signal
  `shed` and `failed` counts, neither of which opens the block directory. A path
  the binary does not serve is a 404.
- **Two active replicas on one host** sharing a data directory with no
  coordination: the block name carries a node id derived from `--node`, so
  publishes are independent renames and either replica's scan sees both.
- **A startup refusal on network filesystems.** `mmap` over NFS, SMB or CephFS
  turns a server hiccup into `SIGBUS`; a `statfs` at startup names the
  filesystem and says what to point `--data-dir` at instead. FUSE warns rather
  than refuses.
- **A load harness**, `cargo run --release --example loadgen`, that scores all
  four performance axes in one run: ingest throughput, ack latency, six classes
  of query latency to p999, peak resident set and bytes on disk per record.
- **A differential test for the query engine** — random stores, random queries
  and a reference model sharing no code with the engine, seeded so a failure
  replays exactly.
- **A distroless Docker image**, one binary and one volume.

### Security

- `cargo-deny` gates advisories, licences, bans and sources in CI; `Cargo.lock`
  is committed and the documented install path is `cargo install --locked`.
- Reporting is GitHub private vulnerability reporting — see
  [`SECURITY.md`](SECURITY.md).

### Known limitations

Tracked here because they are the difference between what the README promises
and what a reader might assume; the README's "What is not true yet" is the
authoritative list.

- Ingestion is allocation-lean, not zero-copy: `prost` memcpies every string.
- No block cache — every query re-opens and re-CRCs the blocks it touches.
- No cross-replica query fan-out, and no peer list to configure.
- No entity selector: every block stores each resource's entity key and no read
  surface reads it yet.
- No `F_FULLFSYNC` fallback for volumes that answer `EINVAL`/`ENOTSUP`.
- No authentication, authorisation or TLS. Mira expects to sit behind something
  that has them.

[Unreleased]: https://github.com/TrianaLab/mira/commits/main
