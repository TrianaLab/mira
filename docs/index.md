---
# Page notes live in the front matter, not in an HTML comment: an HTML comment is
# served to every visitor.
#
# `template` opts this page into the landing hero in overrides/home.html — the
# wordmark, headline, tagline and the three buttons all live there, not here. The
# <h1 hidden> below suppresses the meaningless "<h1>Home</h1>" Material injects
# into any page whose markdown has no h1; the hero carries the real one.
template: home.html
description: An OTLP-native telemetry storage engine and short-term memory layer for AI agents. OTLP in, immutable Arrow IPC blocks out, queried straight from mmap or over MCP.
---

<h1 hidden>Short-term memory for autonomous systems</h1>

```sh
mira --data-dir ./data          # OTLP/gRPC 4317, OTLP/HTTP + UI + MCP 4318
mira mira --data-dir ./data     # the same views in the terminal, no server needed
```

That is the whole of it. [**See it work**](demo.md) is one command and five
screens of real output — an error log, the trace behind it, the service map, a
firing alert, and what the whole run cost in memory and disk.

## What makes it different

Other backends treat OTLP as an ingestion format and transform it into something
else — ClickHouse rows, Parquet files, a TSDB — and every transformation is a
place fidelity can go missing. Mira's storage layout *is* the OpenTelemetry
Resource-Scope-Signal model, stored as Apache Arrow IPC, so there is no
transformation step for a field to fall out of.

| | |
|---|---|
| **Zero-copy reads** | Hot blocks are uncompressed and 64-byte aligned, so a query reads Arrow buffers straight out of the mapping. A test walks every buffer of every column and requires all of them to point inside the `mmap`. |
| **The filesystem is the manifest** | Blocks are named `{min_ts}-{max_ts}-{node}-{seq}`, so the time index is the directory listing. No catalogue to keep in sync with the data. |
| **No coordination state** | No Raft, no membership, no external metadata store. Two active replicas share one volume by having different `--node` names. |
| **Four read surfaces, one binary** | Query API, MCP, browser UI and terminal UI over the same read path. 5.62 MiB stripped, 117 crates, no `protoc` and no node toolchain to build it. |
| **Agents read, not export** | `POST /mcp` is eight tools over that read path. An agent sitting next to the data skips the protocol entirely — `mira mira --data-dir` maps the blocks with no server, no port and no serialisation. |

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
  process runs* — eleven keys — and holds no value that affects how the engine
  performs.

## Where to go next

| | |
|---|---|
| [See it work](demo.md) | one command, then the screens and the numbers |
| [Install](install.md) | one script, a container, or `cargo install` |
| [Quickstart](quickstart.md) | fill it, query it, and the four read surfaces |
| [Connect an agent](agents.md) | MCP wiring, the eight tools, a worked investigation |
| [Configuration](config.md) | eleven keys, KYAML, `${env:…}` interpolation |
| [End-to-end testing](internals/e2e.md) | a live binary, a real collector, the load harness |
| [Architecture](architecture.md) | the reasoning behind every non-obvious choice |
| [Market position](market.md) | who else is in this space, and where the line is |
