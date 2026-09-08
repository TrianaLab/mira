# Mira

An OTLP-native telemetry storage engine in a single binary.

Mira's internal layout *is* the OpenTelemetry Resource-Scope-Signal model. Where
other backends treat OTLP as an ingestion format and transform it into
ClickHouse rows, Parquet files or a TSDB, Mira stores the OTAP star schema
directly as Apache Arrow IPC and reads it back out of `mmap` with no buffer
copies.

```
mira --data-dir ./data          # OTLP/gRPC on 4317, OTLP/HTTP on 4318
```

No cluster membership, no Raft, no external metadata store, no `protoc` to
build. The block directory is the only state; the filesystem is the manifest.

## What is true today

- **OTLP/gRPC 4317** and **OTLP/HTTP 4318**. Logs are stored; traces and metrics
  are wired but return `501`.
- **Zero-copy queries.** Blocks are read straight out of their mapping —
  asserted, not assumed: a test walks every buffer of every column and requires
  all of them to point inside the mapping.
- **Durable acknowledgement.** An export returns only once its block is fsynced
  and renamed into place. No write-ahead log, because there is no torn state to
  replay.
- **Retention by unlink.** TTL drops whole block directories; in-flight readers
  keep working, guaranteed by POSIX.
- 2.6 MB stripped, 107 crates.

## What is not true yet

- **Not** zero-copy *ingestion*. That is not achievable through protobuf —
  `prost` memcpies every string, unconditionally. The ingest goal is
  allocation-lean: one unavoidable copy of the request body, then no per-field
  heap allocation.
- No query layer, no MCP server, no traces or metrics encoders.
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
