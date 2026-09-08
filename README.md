# Mira

An OTLP-native telemetry storage engine in a single binary.

Mira's internal layout *is* the OpenTelemetry Resource-Scope-Signal model. Where
other backends treat OTLP as an ingestion format and transform it into
ClickHouse rows, Parquet files or a TSDB, Mira stores the OTAP star schema
directly as Apache Arrow IPC and reads it back out of `mmap` with no buffer
copies.

```
mira --data-dir ./data          # OTLP/gRPC 4317, OTLP/HTTP + UI + MCP 4318
```

No cluster membership, no Raft, no external metadata store, no `protoc` to
build. The block directory is the only state; the filesystem is the manifest.

## What is true today

- **Logs, traces and metrics**, stored in their own layouts, over **OTLP/gRPC
  4317** and **OTLP/HTTP 4318**.
- **The UI is in the binary.** Open `http://localhost:4318/` — records, trace
  waterfalls and metric charts, served from `include_bytes!`. Nothing to deploy
  beside it.
- **MCP on `/mcp`.** Four tools over JSON-RPC, no session id, so any replica can
  answer any call. A model asks the same four questions the UI does, through the
  same read path.
- **Zero-copy queries.** Blocks are read straight out of their mapping —
  asserted, not assumed: a test walks every buffer of every column and requires
  all of them to point inside the mapping.
- **Durable acknowledgement.** An export returns only once its block is fsynced
  and renamed into place. No write-ahead log, because there is no torn state to
  replay. Read-your-writes falls out of it: the e2e tests query with no sleep
  after the export.
- **Retention by unlink.** TTL drops whole block directories; in-flight readers
  keep working, guaranteed by POSIX.
- **Two active replicas on one volume** need no coordination: the block name
  carries a node id derived from the replica's own name.
- 4.0 MB, 110 crates, no `protoc`, no node toolchain to build.

Measured on an Apple M3 Pro (12 cores), one process, `cargo run --release
--example loadgen`:

| | |
|---|---|
| ingest, 96 connections × 8192 records | 520,889 records/s, 68 MiB/s |
| ack latency (fsync-bound) | p50 472 ms, p99 2.5 s |
| filtered log scan, 786k rows | 310 ms |
| every span of one trace, 25M spans on disk | 250 ms cold, 20 ms warm |
| metric series, 35 blocks | 140 ms |
| bytes on disk per byte on the wire | 1.34 |

Ack latency is the block sealing, not the queue: an export is acknowledged when
its block is durable, so under light load it waits out `max_block_age`.

1.34 bytes per byte is four times the target and the honest weak spot — blocks
are written uncompressed. `zstd -3` over a real block gets 8.2×, which would put
it at 0.16; why that is a tiering decision rather than a flag is
[§11](docs/ARCHITECTURE.md).

## What is not true yet

- **Protobuf only on the wire.** OTLP/HTTP with a JSON body is rejected; the
  three endpoints decode protobuf.
- **Not** zero-copy *ingestion*. That is not achievable through protobuf —
  `prost` memcpies every string, unconditionally. The ingest goal is
  allocation-lean: one unavoidable copy of the request body, then no per-field
  heap allocation.
- **No compression tier**, and it is the largest gap between design and
  measurement. Arrow IPC compresses per buffer and a compressed buffer has to be
  inflated into the heap, so switching it on would end the zero-copy story
  wholesale. The answer is aged blocks rewritten compressed, hot blocks left
  mapped — not built.
- No block cache: every query re-opens and re-CRCs each block it touches, which
  is most of the 310 ms above.
- OTAP is the data model, not yet the wire protocol. OTLP on 4317/4318 is the
  universal path; no language SDK emits OTAP today.
- Do not put the data directory on NFS or CIFS — `mmap` there raises `SIGBUS`
  with no recovery path, and the guard for it is not written.

## Layout

```
crates/mira-proto   vendored OTLP .proto + pure-Rust codegen (protox)
crates/mira-core    Arrow schemas, OTLP encoder, block writer, mmap reader
crates/mira         the binary: receivers, ingest pipeline, retention
```

Design, and the reasoning behind every non-obvious choice, is in
[docs/ARCHITECTURE.md](docs/ARCHITECTURE.md). Start with §0 — it lists the six
mechanisms from the original brief that do not survive contact with the formats,
and what replaced them.

## Licence

Apache-2.0.
