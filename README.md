<img src="docs/assets/mira-wordmark.svg" alt="Mira" height="64">

**An OTLP-native telemetry storage engine and short-term memory layer for AI agents and
infrastructure — one 5.62 MiB binary.** Arrow + `mmap` + ZSTD. Logs, traces and metrics
in; a web UI, a terminal UI and an MCP server out. No cluster, no sidecar, no database
beside it.

```sh
mira --data-dir ./data          # OTLP/gRPC 4317 · OTLP/HTTP + query API + UI + MCP 4318
mira mira --data-dir ./data     # the same views in your terminal, no server needed
```

The block directory is the only state. Point any OTLP exporter at it and open
`http://localhost:4318/`, or point an agent at `POST /mcp`.

<img src="docs/assets/ui/trace.png" alt="A trace waterfall in Mira's web UI: eight nested spans over 76.08ms across frontend, inventory, checkout and payments, the five failed ones in red, with the query cost in the header — 8 matched of 30,720 scanned, 1 block, 47.5ms.">

See it work: **<https://miradb.dev/demo/>** — one command, then that screen and
five more, from data you generated a minute earlier. Documentation:
**<https://miradb.dev>** · Why this exists: **[MANIFESTO.md](MANIFESTO.md)**

---

## Install

```sh
curl -fsSL https://miradb.dev/install.sh | bash
```

The installer picks your target, checks the release's `SHA256SUMS`, and — if the GitHub
CLI is on `PATH` — verifies the SLSA provenance attestation before it moves anything into
place. `--version v0.0.1` pins, `--no-sudo` installs without root, `MIRA_INSTALL_DIR`
picks the directory.

```sh
docker run -p 4317:4317 -p 4318:4318 -v mira-data:/data ghcr.io/trianalab/mira:latest
helm install mira oci://ghcr.io/trianalab/charts/mira
```

Linux glibc >= 2.34 and macOS, x86_64 and arm64. The image is 8.6 MB compressed and the
published bytes are the tarball's, not a second compile. Use a **named volume, not a bind
mount**: Mira `mmap`s its blocks, and a bind mount on Docker Desktop is FUSE, where an I/O
hiccup arrives as `SIGBUS` rather than an error.

**From source**, which until the first tag is the only one of these that resolves —
Rust 1.85+, and a `cc` for `zstd-sys`. No `protoc` (the OTLP protos are compiled by
`protox` in a build script) and no node toolchain (the browser UI is built and
committed):

```sh
cargo install --locked --git https://github.com/TrianaLab/mira mira
```

Every release ships a CycloneDX SBOM, `SHA256SUMS`, a cosign signature and one SLSA
provenance attestation covering the whole matrix. The glibc floor, why there is no musl
build, Kubernetes and the health checks: **[docs/install.md](docs/install.md)**.

## Quickstart

```sh
mira --data-dir ./data &                                   # 1. run it
cargo run --release --example loadgen -- --for 10s --conns 8   # 2. fill it

curl -s localhost:4318/api/v1/query -H 'content-type: application/json' \
  -d '{"signal":"logs","where":[{"attr":"service.name","eq":"checkout"}],"limit":5}'
```

Every response carries a `stats` object (`{"blocks_total":12,"blocks_scanned":1,...}`),
which is the sidecar pruning made visible, and a `next` cursor when it filled `limit`.
`make demo` does all of the above with a four-service shop scenario and a working alert
rule. Walkthrough: **[docs/quickstart.md](docs/quickstart.md)**.

## Four surfaces, one read path

Same data, same filter grammar, same code underneath. Nothing to deploy for any of them.

| | | |
|---|---|---|
| **Browser** | `http://localhost:4318/` | Served out of `include_bytes!`. The whole view lives in the URL, which is why an alert webhook can link straight back into it with nothing saved server-side. |
| **Terminal** | `mira mira` | The same three tabs, the trace waterfall, the service map, the node view. Reads a running replica (`--addr`) **or a block directory in-process** (`--data-dir`) — a detached PVC is still readable with nothing running. |
| **MCP** | `POST /mcp` | Eight tools over JSON-RPC, no session id, so any replica answers any call. |
| **HTTP** | `/api/v1/…` | `query`, `correlate`, `map`, `entities`, `metrics`, `alerts`. |

Both UIs, screen by screen, from data you generated a minute earlier:
**[docs/demo.md](docs/demo.md)**. Connecting an agent, the tool list and a worked
investigation from a firing alert to the failing dependency:
**[docs/agents.md](docs/agents.md)**. Every flag and config key, with
`${env:VAR,default}` interpolation: **[docs/config.md](docs/config.md)** and
**[docs/reference/cli.md](docs/reference/cli.md)**.

## What Mira is

Four pillars. Every number in them is measured on this machine and reproducible with the
load harness in this repository; the reasoning behind each choice is in
[docs/architecture.md](docs/architecture.md).

**1. Agentic memory, and native MCP.** The eight MCP tools are the same eight questions
the browser UI asks, over the same code. Arguments are **validated, not guessed**: a
top-level key the endpoint does not implement is refused *by name*, so
`{"signal":"logs","filters":[…]}` never comes back as a confident answer over the
unfiltered window. A tool that cannot answer returns `isError` inside a successful
response rather than a protocol error, because "no rows for that service" and
"malformed request" are different facts and a model that receives both as a transport
failure learns nothing from either. Every field of every answer is an input to another
call, and `truncated: true` says when a frame is a sample. Why this is the shape it is:
[MANIFESTO.md](MANIFESTO.md).

**2. Zero-copy, high-density engine.** The storage layout *is* the OTLP model —
Resource-Scope-Signal, stored as Apache Arrow IPC and read straight out of the mapping
with no buffer copies, asserted by a test that walks every buffer of every column and
requires all of them to point inside the mapping. Blocks prune themselves before they
are opened: the directory name is the time index, a Bloom sidecar answers *could this
hold that trace id*, and a zone map over every numeric column answers the question no
Bloom filter can — *could anything in here be slower than a second*. A block is written
uncompressed so it stays mappable, then rewritten ZSTD-compressed to **0.12** of its
size an hour later (**0.14** bytes on disk per byte on the wire), and reading one costs
nothing measurable: warm, a compacted block opens within noise of a plain one, and cold
— which is what an hour-old block is — 8.4x fewer pages to fault beats the inflate.
5.62 MiB stripped, 117 crates — `zstd-sys` is the one C dependency and it vendors its
own source.

**3. Correlation algebra, in milliseconds.** `/api/v1/correlate` takes the filter you are
already looking at and returns the frame around it — the real window, the traces those
records belong to, the services that took part — and every operation on a frame returns
a frame, so an investigation is a walk with no illegal state in it. That is one call
where the obvious client makes three. `/api/v1/map` is the service map that falls out of
the same join, computed on read: no metrics generator, no second write path, no
Prometheus beside it. Alert rules embed a query document *verbatim* and threshold it as a
count or a ratio of two counts, which covers percentiles exactly rather than
approximately: `p95(d) > 250ms` **is** `|{d > 250ms}| / |d| > 5%`. No sketch to maintain,
no second dialect to learn. The pruning is what makes it loop-able — 1.5 ms to prove a
value is in **none** of 87 blocks, 1.2 ms for an ordering predicate nothing satisfies,
both with zero blocks opened.

**4. Zero-ops, local and edge.** One self-contained binary with no coordination state:
no Raft, no membership, no external metadata store, and the block directory is the
manifest. Two replicas share one volume by having different `--node` names — publishes
are independent renames and either replica's scan sees both. OTLP in on gRPC 4317 and
HTTP 4318, protobuf or JSON, plain or gzipped, which is what a stock collector already
sends; an export is acked on a `write(2)` into the WAL (p50 7 µs), and a record is
queryable the moment it is acked because queries read the open block too. `SIGTERM`
drains rather than drops, retention is `unlink` so in-flight readers keep working, and
it refuses to start on NFS, SMB or CephFS because `mmap` over those turns a server
hiccup into a `SIGBUS` you cannot catch. `mira mira --data-dir ./data` opens the same
blocks with **no server running at all** — the read path is a directory of mappings, so
an agent co-located with the data needs no port and no serialisation.

All four surfaces speak one filter grammar over one read path. Spans come back with their
events and links, series with the exemplars naming the traces behind them, nested
attributes as the arrays and maps they actually are, and 64-bit integers as JSON strings
in both directions, because that is what OTLP/JSON says and `time_unix_nano` is ~1.7e18 —
a bare number would not fail past 2^53, it would round.

## Performance

Measured on an **Apple M3 Pro (12 cores, 18 GiB)**, one process, `cargo run --release
--example loadgen`. Resident set is `ps` sampled from outside the process, because Mira
reads through `mmap` and a heap counter would miss the page cache that is most of what it
actually costs a machine.

| | |
|---|---|
| ingest, 4 connections × 8192 records — the shape that reproduces | **1,458,967 records/s**, 191 MiB/s of wire bytes, on 1.78 server cores — 817k records/s/core |
| ingest, 1 connection × 8192 records | 604,166 records/s on 0.68 cores — **891k records/s/core**, the per-core ceiling |
| ingest, 96 connections × 8192 records | 795,505 records/s, **nothing shed**, ack p50 269 ms / p99 2.5 s |
| ack a client sees, default `ingest.wal` | p50 7.6 ms, p99 46 ms at four connections |
| the log append inside that ack | p50 7 µs, p99 39 µs for a 4 KiB body |
| attribute value that is in one block, of 87 | 8.9 ms — 24,576 matched |
| attribute value that is in none | 1.5 ms — 0 blocks opened |
| every span of one trace, 28.8M spans on disk | 13.3 ms — 1 block of 77 |
| `duration_nano > 100s` when nothing is that slow | 1.2 ms — 0 blocks opened |
| no time bound, a substring filter that prunes nothing | 175 ms — 87 of 87 blocks, 24.0M rows scanned, 0 matched |
| bytes on disk per byte on the wire | **1.20** hot, **0.14** once compacted |
| peak resident set, ingesting at 1.46M records/s | 862 MiB — 244 MiB at one connection |

The interesting number is not the rate. It is that the plateau costs **1.78 of 12
cores** — Mira is not CPU-bound at any shape measured here, and the per-core rate falls
monotonically from 891k at one connection to 461k at 96. What the extra connections buy
is contention, not work. Throughput plateaus between four and eight connections rather
than peaking at a point; nothing is shed at any shape, because an export that finds the
queue full waits up to five seconds for room instead of taking a 503.

Each ingest row is the median of three 30 s runs against a fresh server. Query rows are
one store — 7.35 GiB, 28.8M logs and 28.8M spans in 180 blocks — steady-state, with all
of it in page cache; the first call after a restart is 2–12× slower while the mappings
are faulted in. The full sweep, the per-pass spreads, the eight-connection row and what
to quote from your own run: **[docs/internals/e2e.md](docs/internals/e2e.md)**. What
moved these numbers, and why:
**[architecture section 11](docs/architecture.md#11-performance-model)**.

### Against the market

Every competitor figure is the vendor's own published number, linked, quoted with the
hardware they ran it on — nobody ran Mira's workload and Mira did not run theirs, which
is why the comparison lives in a document with room for the caveats rather than in a
league table here. The short version: on ingest, the only other single-process figure on
laptop silicon is GreptimeDB's 621k rows/s on 16 cores against Mira's 1.46M on 12, and
"same order, and the smaller machine did not lose" is what that pair supports. On
artifact size VictoriaLogs is the one to beat and it is a 3x, not the 25x the Go
observability stacks carry. On compression, quote **0.14** bytes on disk per wire byte
for a bill and **8.38x** against uncompressed columnar for a ratio, and neither as a
head-to-head. All four tables, with sources and with the places Mira is *behind*:
**[docs/market.md](docs/market.md)**.

## Scope

Three boundaries worth knowing before you deploy it. The full list, with the reasoning,
is [architecture section 0.1](docs/architecture.md#01-what-is-not-true-yet).

- **Ingestion is not zero-copy** — that is not achievable through protobuf, since `prost`
  memcpies every string unconditionally. *Queries* are zero-copy; ingestion is
  allocation-lean.
- **No cross-replica query fan-out, and no OTAP on the wire.** A query reads the block
  directory it was pointed at; nothing scatters it. Two replicas sharing a volume both
  answer for all of it, two replicas with a volume each answer for half. OTAP is the data
  model, not yet the protocol — no language SDK emits it.
- **No entity predicate and no block cache.** `/api/v1/entities` lists the entities a
  window holds, but no query document accepts an entity key as a filter, so "everything
  this pod emitted" is still `{"attr":"service.instance.id","eq":"…"}`. Every query
  re-opens and re-CRCs the blocks it touches — measured at a few ms of a 14 ms query, not
  the 10× that page-fault behaviour was.

## Layout

```
crates/mira-proto   vendored OTLP .proto + pure-Rust codegen (protox)
crates/mira-core    Arrow schemas, OTLP encoder, block writer, mmap reader
crates/mira         the binary: receivers, ingest pipeline, retention,
                    query API, MCP, the browser UI and the terminal UI
```

## Contributing

Every gate CI runs is a target in the [`Makefile`](Makefile), and CI calls nothing else:

```sh
make            # the target list
make check      # fmt, clippy, tests, rustdoc, UI, supply chain, drift, docs, coverage
```

Read [architecture section 0](docs/architecture.md#0-corrections-to-the-original-brief) first if the change is
structural — it lists the mechanisms from the original brief that do not survive contact
with the formats, and re-proposing one is the most common way to waste an afternoon.
[CONTRIBUTING.md](CONTRIBUTING.md) is the walkthrough, with
[the test levels](docs/internals/testing.md) and
[how a release is cut](docs/internals/releases.md) behind it; shipped changes are in
[CHANGELOG.md](CHANGELOG.md); vulnerabilities go to [SECURITY.md](SECURITY.md), not to an
issue.

## Licence

Apache-2.0.
