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

The image is 8.6 MB compressed — `distroless/base-nossl` plus `libgcc_s.so.1`, which is
the complete set of things the binary needs. Use a **named volume, not a bind mount**:
Mira `mmap`s its blocks, and a bind mount on Docker Desktop is FUSE, where an I/O hiccup
arrives as `SIGBUS` rather than an error.

| | x86_64 | arm64 |
|---|---|---|
| Linux (glibc >= 2.34) | `x86_64-unknown-linux-gnu` | `aarch64-unknown-linux-gnu` |
| macOS | `x86_64-apple-darwin` | `aarch64-apple-darwin` |

Every release ships a CycloneDX SBOM, `SHA256SUMS`, a cosign signature and one SLSA
provenance attestation covering the whole matrix; the published image is the same bytes
as the tarball, not a second compile.

**From source**, which until the first tag is the only one of these that resolves —
Rust 1.85+, and a `cc` for `zstd-sys`. No `protoc` (the OTLP protos are compiled by
`protox` in a build script) and no node toolchain (the browser UI is built and
committed):

```sh
cargo install --locked --git https://github.com/TrianaLab/mira mira
```

Full install notes, including the glibc floor and why there is no musl build:
[docs/install.md](docs/install.md).

## Quickstart

```sh
mira --data-dir ./data &                                   # 1. run it
cargo run --release --example loadgen -- --for 10s --conns 8   # 2. fill it

curl -s localhost:4318/api/v1/query -H 'content-type: application/json' \
  -d '{"signal":"logs","where":[{"attr":"service.name","eq":"checkout"}],"limit":5}'
```

Blocks seal on size or age, so give it a second after the last export — the server logs
`block published` when one lands. Every response carries a `stats` object
(`{"blocks_total":12,"blocks_scanned":1,...}`), which is the sidecar pruning made
visible, and a `next` cursor when it filled `limit`.

`make demo` does all of the above with a four-service shop scenario and a working alert
rule. The whole end-to-end story, including the OpenTelemetry project's own
`telemetrygen` and a stock Collector in front of Mira in Docker, is
[docs/TESTING.md](docs/TESTING.md).

## The three surfaces

Same data, same filter grammar, same read path. Nothing to deploy for any of them.

### Terminal — `mira mira`

```
 mira   1 logs    2 traces    3 metrics                                        local ./data
 filter  (none) — press / to add one, e.g. service.name=checkout                 last 1h  limit 200
 19:19:52.500 INFO   checkout         POST /pay 200 in 16ms
 19:19:52.500 INFO   checkout         GET /items 200 in 15ms
 19:19:52.500 INFO   checkout         POST /checkout 200 in 14ms
 19:19:52.500 INFO   checkout         GET /cart 200 in 13ms
 19:19:52.500 INFO   checkout         GET /health 200 in 12ms
── record 1 of 200 ─────────────────────────────────────────────────────────────────────────────────
  time_unix_nano            2026-09-10 19:19:52.500
  severity_number           9
  body                      POST /pay 200 in 16ms
  trace_id                  000000000003ab3f555555555556fe6a
  span_id                   ac972876beb5e5eb
 1/12 blocks · 69632 rows scanned · 69632 matched · 153.8ms
 ↑↓ move  enter detail  t trace  c frame  m map  a alerts  d node  f follow  / filter  ? help
```

`t` opens the trace the row belongs to, as a waterfall:

```
 19:19:52.503    1.98ms checkout         POST /pay                      ▂▂▂▂▂▂▂▂▂▂▂▂▂
 19:19:52.503    1.66ms checkout         GET /cart                      ▂▂▂▂▂▂▂▂▂▂
 19:19:52.503    1.34ms checkout         GET /items                     ▂▂▂▂▂▂▂▂
 19:19:52.503    2.18ms checkout         POST /checkout                 ▂▂▂▂▂▂▂▂▂▂▂▂▂▂
── span 1 of 200 ───────────────────────────────────────────────────────────────────────────────────
  parent_span_id            0e5faebd3f6b69d7
  name                      POST /pay
  kind                      client
  duration_nano             1.98ms
```

`c` widens the current filter into the *frame* around it — the time extent, the traces,
and every service that took part in them:

```
 frame 2026-09-10 18:23:34.551 → 2026-09-10 19:23:34.551  ·  60m00s
── 4 services ──────────────────────────────────────────────────────────────────────────────────────
  checkout  ×4
  frontend  ×4
  inventory  ×4
  payments  ×4
── 1000 traces ─────────────────────────────────────────────────────────────────────────────────────
  0000000000039c80555555555556c9d5
 esc back  ↑↓ move  enter service→filter, trace→waterfall
```

Every line leads back into a query. `m` is the service map, `a` the alert rules, `d` the
node itself — uptime, peak RSS, disk headroom, and per signal the rows, blocks and bytes
on disk per row. It reads a **running replica** (`--addr host:4318`) or a **block
directory in-process** (`--data-dir`), and the second one is the point: a detached PVC or
a dead pod's volume is still readable with nothing running.

### Browser — `http://localhost:4318/`

Served out of `include_bytes!`; there is no second thing to deploy.

```
┌─ mira ──── logs │ traces │ metrics │ map │ alerts ──────────────────── [tail ●] ─┐
│ filter  service.name=checkout duration_nano>10ms          last 1h ▾   limit 200  │
│ services ▾  checkout · frontend · inventory · payments                           │
├──────────────────────────────────────────────────────────────────────────────────┤
│ 19:19:52.500  INFO   checkout   POST /pay 200 in 16ms                            │
│ 19:19:52.500  INFO   checkout   GET /items 200 in 15ms          ┌─ Frame ───────┐│
│ 19:19:52.500  ERROR  payments   card declined                   │ 60m00s        ││
│ …                                                               │ 4 services    ││
│                                                                 │ 1000 traces   ││
├─────────────────────────────────────────────────────────────────┴───────────────┤│
│ detail  ·  attributes  ·  events  ·  links  ·  exemplars → the traces behind it  ││
└──────────────────────────────────────────────────────────────────────────────────┘
```

The whole view lives in the URL, which is why an alert webhook can link straight back
into it without anything being saved server-side.

### CLI

| | |
|---|---|
| `mira` | run the server |
| `mira mira` (`mira tui`) | the terminal UI — `--data-dir` or `--addr` |
| `mira update` | replace this binary with the latest release — `--version`, `--dry-run` |
| `--data-dir PATH` | where blocks go (`./mira-data`) |
| `--grpc ADDR` / `--http ADDR` | listen addresses (`0.0.0.0:4317` / `:4318`) |
| `--retention DURATION` | TTL: `7d`, `12h`, `30m`, `500ms` (`7d`) |
| `--node NAME` | this replica's identity (`mira`) |
| `--config FILE` | KYAML; flags override it |

Everything a flag sets, the config file sets too, with `${env:VAR,default}`
interpolation — [docs/CONFIG.md](docs/CONFIG.md) is the whole surface.

### MCP — `POST /mcp`

Eight tools over JSON-RPC, no session id, so any replica answers any call. A model asks
the same questions the UI does, through the same read path.

```sh
claude mcp add --transport http mira http://localhost:4318/mcp   # or any MCP client
curl -s -X POST localhost:4318/mcp -H 'content-type: application/json' \
  -d '{"jsonrpc":"2.0","id":1,"method":"tools/list"}'
```

Client configuration, the tool list, and a worked investigation from a firing alert to
the failing dependency: **[docs/agents.md](docs/agents.md)**.

## What Mira is

Four pillars. Every number in them is measured on this machine and reproducible with the
load harness in this repository; the reasoning behind each choice is in
[docs/ARCHITECTURE.md](docs/ARCHITECTURE.md).

**1. Agentic memory, and native MCP.** `POST /mcp` speaks JSON-RPC and exposes eight
tools — `query_records`, `get_trace`, `query_metric`, `list_metrics`, `correlate`,
`service_map`, `list_services`, `list_alerts` — which are the same eight questions the
browser UI asks, over the same code. Arguments are **validated, not guessed**: a
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

All four surfaces — browser UI, terminal UI, MCP, query API — speak one filter grammar
over one read path. Spans come back with their events and links, series with the
exemplars naming the traces behind them, nested attributes as the arrays and maps they
actually are, and 64-bit integers as JSON strings in both directions, because that is
what OTLP/JSON says and `time_unix_nano` is ~1.7e18 — a bare number would not fail past
2^53, it would round.

## Performance

Measured on an **Apple M3 Pro (12 cores, 18 GiB)**, one process, `cargo run --release
--example loadgen`. Resident set is `ps` sampled from outside the process, because Mira
reads through `mmap` and a heap counter would miss the page cache that is most of what it
actually costs a machine.

| | |
|---|---|
| ingest, 4 connections × 8192 records — the shape that reproduces | **1,458,967 records/s**, 191 MiB/s of wire bytes, on 1.78 server cores — 817k records/s/core |
| ingest, 8 connections × 8192 records — the higher median | 1,565,941 records/s on 2.18 cores, but a 38% spread across three passes against 2.5% at four |
| ingest, 1 connection × 8192 records | 604,166 records/s on 0.68 cores — **891k records/s/core**, the per-core ceiling |
| ingest, 96 connections × 8192 records | 795,505 records/s, **nothing shed**, ack p50 269 ms / p99 2.5 s |
| ack a client sees, default `ingest.wal` | p50 7.6 ms, p99 46 ms at four connections; p50 5.2 ms, p99 22 ms at one |
| ack a client sees, log off (block-seal-bound) | p50 657 ms, p99 2.6 s |
| the log append inside that ack | p50 7 µs, p99 39 µs for a 4 KiB body — p50 0.24 ms for 1 MiB |
| attribute value that is in one block, of 87 | 8.9 ms — 24,576 matched |
| attribute value that is in none | 1.5 ms — 0 blocks opened |
| every span of one trace, 28.8M spans on disk | 13.3 ms — 1 block of 77 |
| `duration_nano > 100s` when nothing is that slow | 1.2 ms — 0 blocks opened |
| metric names | 3.7 ms — all 16 metric blocks |
| no time bound, a substring filter that prunes nothing | 175 ms — 87 of 87 blocks, 24.0M rows scanned, 0 matched |
| bytes on disk per byte on the wire | **1.20** hot, **0.14** once compacted |
| peak resident set, ingesting at 1.46M records/s | 862 MiB — 244 MiB at one connection, 2,040 MiB at eight |
| peak resident set, 8 readers over 21.8M records | 1,624 MiB |

**Four ingest rows, because one would be a claim about the load and not about the
engine.** The synthetic record is 137 B on the wire, and that figure is derived rather
than asserted: nothing is shed at any shape, so wire bytes ÷ records is exact, and
across twenty-one runs it lands between 136.7 and 137.3 B.

Throughput **plateaus between four and eight connections** rather than peaking at a
point, and the two rows are both in the table because picking one would be an
editorial decision dressed as a measurement. Eight has the higher median; four is the
number quoted everywhere else here, because it is the shape that reproduces — three
passes span 2.5% at four connections and 38% at eight — and because the plateau costs
862 MiB and a 46 ms ack p99 at four against 2,040 MiB and 447 ms at eight. Past eight
it falls with concurrency while the tail grows: 96 connections is 55% of the
four-connection rate at a 2.5 s p99.

What it no longer does at any shape is *shed*. An export that finds the queue full
waits up to five seconds for room instead of taking a 503, and that one change took
the 96-connection row from 333k records/s with 93% of exports shed to shed-free —
twenty-one consecutive runs across the whole sweep refused nothing. Shedding threw
away a decode the server had already paid for, in exchange for a retry that made it
pay again.

The interesting number in the table is not the rate. It is that the plateau costs
**1.78 of 12 cores** — Mira is not CPU-bound at any shape measured here, ten cores sit
idle at the fastest row, and the per-core rate falls monotonically from 891k at one
connection to 461k at 96. What the extra connections buy is contention, not work.

Each ingest row is the median of three 30 s runs against a fresh server, and the
machine does not get to be quiet: managed daemons and a container VM hold two to four
cores throughout. That is not a footnote — an earlier pass of this identical sweep,
taken while a 294%-CPU virtual machine was running, read 1,165,623 at four connections,
20% low, with nothing in the output to say so. The full sweep, the per-pass spreads and
what to quote from your own run are in [docs/TESTING.md](docs/TESTING.md).

Query rows are one store — the 7.35 GiB the four-connection run left behind, 28.8M logs
and 28.8M spans in 180 blocks — and they are steady-state, with all of it in page cache.
The first call after a process restart is 2–12× slower while 180 mappings are
established and faulted in: 18.9 ms against 8.9 ms for the matching attribute, 1.59 s
against 175 ms for the scan that prunes nothing. That gap is virtual-memory work, not
I/O; on this machine the data never leaves RAM once it has been read, and short of
`purge` there is no way back to a genuinely cold cache.

Three things moved these numbers, and the reasoning for each is in
[architecture section 11](docs/ARCHITECTURE.md#11-performance-model): Bloom sidecars took the
absent-value case from 10.4 s to single-digit milliseconds; zone maps took the ordering
predicate from 81.6 ms to about 1 ms; and one `madvise(MADV_WILLNEED)` took the
unprunable full scan from 10.1 s to under 2 s on the first call, because `mmap` was
faulting in 16 KB at a time.

### Against the market

Every competitor figure below is **the vendor's own published number**, linked, quoted
with the hardware they ran it on. Nobody ran Mira's workload and Mira did not run
theirs, so the hardware column is not decoration — it is the only honest way to read a
throughput table. Where a vendor's number beats Mira's, it is in the table.

**Ingest, one node.** Read the hardware column before the rate column.

| | published rate | on their hardware |
|---|---|---|
| **Mira** | **1,458,967 records/s** (137 B records, nothing shed) | 1 process, M3 Pro 12 core / 18 GB, **1.78 cores busy**, generator co-resident |
| [GreptimeDB 1.0 standalone](https://greptime.com/blogs/2026-03-24-ingestion-protocol-benchmark) | 621,367 rows/s (OTLP/HTTP) | M4 Max 16 core / 48 GB, 5 workers, batch 1000 |
| [Parseable](https://www.parseable.com/blog/the-economics-and-physics-of-100-tb-telemetry-data-per-day) | ~133 MiB/s per node | 4 x c7gn.4xlarge (16 vCPU), 12 separate generator instances |
| [SigNoz](https://signoz.io/blog/logs-performance-benchmark/) (ClickHouse + collector) | ~55,000 log lines/s | c6a.4xlarge (16 vCPU) for the whole stack, 3 generator VMs |
| [Quickwit](https://quickwit.io/blog/benchmarking-quickwit-engine-on-an-adversarial-dataset) | 27 MB/s per active indexer | c5.xlarge (4 vCPU), splits to S3 |
| [VictoriaLogs](https://victoriametrics.com/blog/dev-note-distributed-tracing-with-victorialogs/) as a traces store | 30,000 spans/s at 1.2 cores | 4 vCPU / 8 GiB — offered load, not a measured ceiling |
| [Elasticsearch](https://www.elastic.co/blog/benchmarking-and-sizing-your-elasticsearch-cluster-for-logs-and-metrics) | 22,000 events/s | 1 node, 8 vCPU, 1 shard |

GreptimeDB's is the only other single-process figure on laptop silicon, so it is the
row to read Mira's against: 621,367 rows/s on 16 cores and 48 GB, against 1,458,967 on
12 cores and 18 GB. Mira is the faster of the two on smaller hardware, but 2.3x is not
the claim and cannot be — their record is not this record, their batch is 1,000 against
this one's 8,192, and nobody ran both. "Same order, and the smaller machine did not
lose" is what the pair supports. What is not close is the cores: Mira's rate is on 1.78
of them, measured as CPU-seconds consumed over wall clock. GreptimeDB does not publish
a utilisation figure, so 621,367 ÷ 16 is not their per-core number either — it is only
the bound their published pair permits.

**Resident set.** Mira's 244 MiB is peak RSS for the whole process while ingesting at
604k records/s on one connection — receivers, WAL, writer, query API and both UIs
included. It is not flat in connection count: the same process reaches 862 MiB at four
connections and 2,244 MiB at 96, because RSS counts every mapped block page as well as
the builders, and more concurrency means more blocks open at once. 244 MiB is a single
sender's footprint, not a floor across all of them.

| | published RSS | at |
|---|---|---|
| **Mira** | **244 MiB** | 604,166 rec/s, one process, everything in it |
| [GreptimeDB 0.12](https://greptime.com/blogs/2025-03-10-log-benchmark-greptimedb) | 408 MB | 20,000 rows/s, c5d.2xlarge |
| [ClickHouse](https://victoriametrics.com/blog/dev-note-distributed-tracing-with-victorialogs/) | 1.12 GiB | 10,000 spans/s, 4 vCPU / 8 GiB |
| [VictoriaLogs](https://victoriametrics.com/blog/dev-note-distributed-tracing-with-victorialogs/) | 1.15 GiB | 10,000 spans/s, same box |
| [Grafana Tempo](https://victoriametrics.com/blog/dev-note-distributed-tracing-with-victorialogs/) | 4.26 GiB | 10,000 spans/s, same box |
| [Quickwit](https://quickwit.io/blog/benchmarking-quickwit-engine-on-an-adversarial-dataset) indexer | 4.9 GB avg, 6.8 GB peak | c5.xlarge |
| [SigNoz](https://signoz.io/blog/logs-performance-benchmark/) stack | ~6 GB | 55,000 logs/s, c6a.4xlarge |

**Artifact.** This one *is* apples to apples — a stripped binary is a stripped binary,
and every figure is a byte count from the vendor's own release. Architecture in
brackets, because it moves the number by a few percent and nothing more.

| | stripped binary | crates or modules |
|---|---|---|
| **Mira** | **5.62 MiB** (arm64 macOS) | **117** crates |
| [VictoriaLogs 1.52](https://github.com/VictoriaMetrics/VictoriaLogs/releases/latest) | 16.26 MiB (amd64 Linux) | 98 vendored Go packages |
| [Grafana Tempo 3.0.3](https://api.github.com/repos/grafana/tempo/releases/latest) | 93.94 MiB (arm64 Linux) | 425 modules (105 direct) |
| [otel-arrow OTAP engine](https://github.com/open-telemetry/otel-arrow/actions/runs/34422416665) | 103.35 MiB (arm64 Linux) | — |
| [Grafana Mimir 3.2.1](https://api.github.com/repos/grafana/mimir/releases/latest) | 104.67 MiB (arm64 macOS) | 331 modules (98 direct) |
| [Grafana Loki 3.7.7](https://api.github.com/repos/grafana/loki/releases/latest) | 138.34 MiB (arm64 macOS) | 407 modules (143 direct) |
| [Quickwit 0.9.0](https://github.com/quickwit-oss/quickwit/releases/latest) | 144.29 MiB (arm64 macOS) | 1,171 lockfile entries |
| [Parseable 3.2.0](https://github.com/parseablehq/parseable/releases/latest) | 152.44 MiB (arm64 macOS) | 462 crates |
| [ClickStack all-in-one](https://clickhouse.com/docs/clickstack/deployment/all-in-one) | 486.67 MiB image (arm64) | ClickHouse + HyperDX + collector + MongoDB |

VictoriaLogs is the one to beat and it is not far off — 98 Go packages against Mira's 117
crates, and it is a genuinely small binary. The 3x is real but it is a 3x, not the 25x
the Go observability stacks carry.

**Compression needs two numbers, because everyone publishes the flattering one.**
Mira's **0.14** is bytes on disk per byte of OTLP protobuf *on the wire* — the ratio you
can predict a bill from, and the one no vendor publishes, because the wire bytes are a
much smaller denominator than uncompressed rows. On the denominator everyone *does*
publish — uncompressed columnar bytes against compressed — Mira is **8.38x**: 7.35 GiB
of plain Arrow IPC down to 898 MiB, measured by `mira-core`'s `tier` example over all
948 tables of a real store, through the same `write_table_zstd` the retention sweep
calls. Against the published field: ClickHouse [14.1x for its `otel_logs`
schema](https://clickhouse.com/blog/storing-log-data-in-clickhouse-fluent-bit-vector-open-telemetry)
and [~16x
fleet-wide](https://clickhouse.com/blog/a-quadrillion-rows-across-the-three-cloud-scaling-loghouse),
VictoriaLogs
[11.2x](https://victoriametrics.com/blog/victorialogs-internals-columnar-storage-on-disk/),
GreptimeDB [7.7x](https://greptime.com/blogs/2025-03-10-log-benchmark-greptimedb),
Quickwit [3.7x](https://quickwit.io/blog/benchmarking-quickwit-loki). Mira is ahead of
GreptimeDB and Quickwit and behind ClickHouse and VictoriaLogs on that measure — and
the measure is still not one-to-one, because each of those denominators is that
vendor's own uncompressed representation and no two are the same shape. Quote 0.14 for
a bill, 8.38x for a ratio, and neither as a head-to-head.

## Scope

Three boundaries worth knowing before you deploy it. The full list, with the reasoning,
is [architecture section 0.1](docs/ARCHITECTURE.md#01-what-is-not-true-yet).

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

Read [architecture section 0](docs/ARCHITECTURE.md#0-corrections-to-the-original-brief) first if the change is
structural — it lists the mechanisms from the original brief that do not survive contact
with the formats, and re-proposing one is the most common way to waste an afternoon.
[CONTRIBUTING.md](CONTRIBUTING.md) is the walkthrough; releases are in
[CHANGELOG.md](CHANGELOG.md); vulnerabilities go to [SECURITY.md](SECURITY.md), not to an
issue.

## Licence

Apache-2.0.
