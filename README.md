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
- **Blocks prune themselves.** The directory name is the time index; a Bloom
  sidecar per block answers "could this hold that trace id / that attribute
  value" before anything is opened. Every damaged or missing sidecar reads as
  "scan me", so the worst a filter can do is waste a read.
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
| ingest, 96 connections × 8192 records | 544,658 records/s, 71 MiB/s, 0 shed |
| ack latency (fsync-bound) | p50 498 ms, p99 2.4 s |
| attribute value that is in one block, of 69 | 14.5 ms |
| attribute value that is in none | 6.1 ms — 0 blocks opened |
| every span of one trace, 25M spans on disk | 14.3 ms — 1 block of 86 |
| metric names | 5.5 ms |
| no time bound, no filter that prunes | 1.9 s — 69 blocks, 25.2M rows, 4.56 GB |
| bytes on disk per byte on the wire | 1.31 |

Ack latency is the block sealing, not the queue: an export is acknowledged when
its block is durable, so under light load it waits out `max_block_age`.

Query times are steady-state on a machine where all 8.4 GiB stays in page cache;
first call after a restart is 2–15× slower while the mappings are established.

Two things moved these. The Bloom sidecars mean a query with no time bound —
"every span of this trace", "any record with this attribute value" — opens the
one block that can answer instead of all of retention; the absent-value case went
from 10.4 s to 6 ms. And what was left after that turned out to be `mmap`
faulting in 16 KB at a time: since every block open reads the whole body to check
its CRC, one `madvise(MADV_WILLNEED)` took the unprunable full scan from 10.1 s
to 1.9 s.

1.31 bytes per byte is four times the target and the honest weak spot — blocks
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
- No block cache: every query re-opens and re-CRCs each block it touches. This
  looked like the next big win until it was measured — it is worth a few
  milliseconds of a 14 ms query, not the 10× that page-fault behaviour was.
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
