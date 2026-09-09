---
# Page notes live in the front matter, not in an HTML comment: an HTML comment is
# served to every visitor. The <h1 hidden> below is deliberate — the wordmark is
# doing the job of a visible title, and Material injects a meaningless
# "<h1>Home</h1>" into any page whose markdown has no h1 at all.
description: An OTLP-native telemetry storage engine in a single binary. OTLP in, immutable Arrow IPC blocks out, queried straight from mmap.
---

![Mira](assets/mira-wordmark.svg){ .mira-wordmark }

<h1 hidden>An OTLP-native telemetry storage engine in a single binary</h1>

**An OTLP-native telemetry storage engine in a single binary.** OTLP in,
immutable Arrow IPC blocks out, queried straight from `mmap`.

```sh
mira --data-dir ./data          # OTLP/gRPC 4317, OTLP/HTTP + UI + MCP 4318
mira mira --data-dir ./data     # the same views in the terminal, no server needed
```

[Install](install.md){ .md-button .md-button--primary }
[Quickstart](quickstart.md){ .md-button }

## What makes it different

Other backends treat OTLP as an ingestion format and transform it into something
else — ClickHouse rows, Parquet files, a TSDB — and every transformation is a
place fidelity can go missing. Mira's storage layout *is* the OpenTelemetry
Resource-Scope-Signal model: the Arrow schemas in `crates/mira-core` are the OTAP
star schema, stored as Apache Arrow IPC, so there is no transformation step for a
field to fall out of.

- **Zero-copy queries on the hot tier.** A hot block is written uncompressed and
  64-byte aligned, so a query reads its Arrow buffers straight out of the
  mapping. Asserted, not assumed: a test walks every buffer of every column and
  requires all of them to point inside the `mmap`.
- **The filesystem is the manifest.** A block directory is named
  `{min_ts}-{max_ts}-{node}-{seq}`, so the time index is the directory listing,
  and a Bloom sidecar per block answers "could this hold that trace id" before
  anything is opened. There is no catalogue to keep in sync with the data.
- **No coordination state.** No Raft, no membership, no external metadata store.
  Two active replicas share one volume by having different `--node` names —
  publishes are independent renames, and either replica's scan sees both.
- **One binary, four read surfaces.** Query API, MCP, a browser UI and a terminal
  UI, all over the same read path and all inside the executable. 4.73 MiB
  stripped, 117 crates, no `protoc` and no node toolchain to build it.

## What it is deliberately not

- **Not zero-copy *ingestion*.** `prost` memcpies every string, unconditionally;
  that is a property of protobuf, not something to engineer around. The ingest
  goal is allocation-lean instead — one unavoidable copy of the request body.
- **Not a query language.** No SQL, no PromQL, no TraceQL. DataFusion would have
  given SQL for free, at 47 direct dependencies and a 68–92 MB binary; Mira
  hand-rolls the ~2,000 lines of query logic it actually needs.
- **Not clustered.** A query reads the block directory it was pointed at and
  nothing scatters it. There is no peer list, because there is nothing to
  configure one against.
- **Not tunable.** There is no block size, flush interval, buffer depth or cache
  size to set, and there will not be. The config file describes *where the
  process runs* — six keys — and holds no value that affects how the engine
  performs.

[Architecture §0](ARCHITECTURE.md) is the list of mechanisms from the original
brief that do not survive contact with the formats involved, and what replaced
each of them. It is the fastest way to understand the shape of everything else.

## Where to go next

| | |
|---|---|
| [Install](install.md) | build it from source, or run the container |
| [Quickstart](quickstart.md) | fill it, query it, and the four read surfaces |
| [Configuration](CONFIG.md) | six keys, KYAML, `${env:…}` interpolation |
| [End-to-end testing](TESTING.md) | a live binary, a real collector, the load harness |
| [Architecture](ARCHITECTURE.md) | the reasoning behind every non-obvious choice |
| [Market position](MARKET.md) | who else is in this space, and where the line is |
