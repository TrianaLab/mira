<img src="docs/assets/mira-wordmark.svg" alt="Mira" height="64">

An OTLP-native telemetry storage engine in a single binary.

Mira's internal layout *is* the OpenTelemetry Resource-Scope-Signal model. Where
other backends treat OTLP as an ingestion format and transform it into
ClickHouse rows, Parquet files or a TSDB, Mira stores the OTAP star schema
directly as Apache Arrow IPC and reads it back out of `mmap` with no buffer
copies.

```
mira --data-dir ./data          # OTLP/gRPC 4317, OTLP/HTTP + UI + MCP 4318
mira mira --data-dir ./data     # the same views in the terminal, no server needed
```

No cluster membership, no Raft, no external metadata store, no `protoc` to
build. The block directory is the only state; the filesystem is the manifest.

Documentation: **<https://trianalab.github.io/mira/>**

## Install

Rust 1.85 or newer, and a `cc` for `zstd-sys`, which vendors its own C source.
No `protoc`: the OTLP protos are compiled by `protox` in a build script. No node
toolchain: the browser UI is built and committed under `crates/mira/ui/dist`.

```sh
git clone https://github.com/TrianaLab/mira && cd mira
cargo install --locked --path crates/mira        # -> ~/.cargo/bin/mira
```

Or without installing:

```sh
cargo build --release                            # -> ./target/release/mira
```

Or in Docker, where the image is one binary on `distroless/cc` and one volume:

```sh
docker build -t mira .
docker run -p 4317:4317 -p 4318:4318 -v mira-data:/data mira
```

Use a **named volume, not a bind mount**. Mira `mmap`s its blocks, and a bind
mount on Docker Desktop is FUSE, where an I/O hiccup arrives as `SIGBUS` rather
than as an error.

### From a release

Nothing is tagged yet, so `cargo install` above is the only path that works
today. From the first tag on, `.github/workflows/release.yml` publishes a
stripped binary for linux and macOS on x86_64 and arm64, a CycloneDX SBOM, a
`SHA256SUMS`, and one SLSA provenance attestation covering all of them:

```sh
V=0.1.0; T=x86_64-unknown-linux-gnu
base=https://github.com/TrianaLab/mira/releases/download/v$V
curl -sSLO $base/mira-$V-$T.tar.gz -O $base/SHA256SUMS
sha256sum -c SHA256SUMS --ignore-missing
tar -xzf mira-$V-$T.tar.gz --strip-components=1 mira-$V-$T/mira
gh attestation verify mira --repo TrianaLab/mira     # provenance, not just integrity
```

The linux builds come off `ubuntu-22.04`, so the glibc floor is **2.34** —
RHEL 9, Amazon Linux 2023, Debian 12, Ubuntu 22.04+ and `distroless/cc-debian12`.
There is no musl build: it compiles, but mallocng costs Mira's ingest path more
than the Alpine coverage is worth, and fixing that means linking jemalloc and
losing "`zstd-sys` is the one C dependency". Build from source on Alpine.

The published image is the same bytes, not a second compile:

```sh
docker run -p 4317:4317 -p 4318:4318 -v mira-data:/data ghcr.io/trianalab/mira:latest
```

## Use it

```sh
mira --data-dir ./data
```

That is the whole configuration. It listens on OTLP/gRPC `4317` and OTLP/HTTP
`4318`, and `4318` also serves the query API, MCP and the UI. Point any OTLP
exporter at it — no Mira-specific collector component exists, or is needed.

```
--data-dir PATH            where blocks go            (./mira-data)
--grpc ADDR --http ADDR    listen addresses           (0.0.0.0:4317 / :4318)
--retention DURATION       TTL: 7d, 12h, 30m, 500ms   (7d)
--node NAME                this replica's identity    (mira)
--config FILE              KYAML; flags override it
```

Everything a flag sets, the config file sets too, with `${env:VAR,default}`
interpolation — [docs/CONFIG.md](docs/CONFIG.md) is the whole surface. Two
replicas on one host can share a data directory with no coordination: give each
a different `--node`. Across hosts the shape is shared-nothing — a data
directory shared between machines is a network filesystem, and Mira refuses to
start on one.

Fill it, then read it back:

```sh
cargo run --release --example loadgen -- --for 10s --conns 8

curl -s localhost:4318/api/v1/query -H 'content-type: application/json' \
  -d '{"signal":"logs","where":[{"attr":"service.name","eq":"checkout"}],"limit":5}'
```

Blocks seal on size or age, so wait a couple of seconds after the last export;
the server logs `block published` when one lands. Every response carries a
`stats` object — `{"blocks_total":2,"blocks_scanned":1,...}` — which is the
sidecar pruning, visible. A response that filled `limit` also carries `next`;
pass it back as `after` for the following page.

Then the other three surfaces:

```sh
open http://localhost:4318/           # the UI, served out of the binary
mira mira --data-dir ./data           # the same views in the terminal
curl -s -X POST localhost:4318/mcp -H 'content-type: application/json' \
  -d '{"jsonrpc":"2.0","id":1,"method":"tools/list"}'
```

## Test it

```sh
cargo test --workspace
```

That includes the in-process end-to-end suite in `crates/mira/src/e2e.rs`, which
drives the real router — OTLP/HTTP, OTLP/gRPC, the query API, MCP and the UI —
with no sockets and nothing to clean up. It is the loop to stay in.

The query engine also has a differential test — random stores, random queries,
and a reference model that shares no code with the engine — which is the closest
thing here to a proof that pruning never drops a match and that paging visits
every row exactly once. It is seeded, so a failure replays exactly.

For the things a unit test cannot reach — a real socket, a real exporter, real
volume — [docs/TESTING.md](docs/TESTING.md) is a transcript rather than a plan:
a live binary fed by the built-in `loadgen`, then the OpenTelemetry project's own
`telemetrygen`, then a stock Collector in front of it in Docker
(`docker compose -f docs/e2e/compose.yaml up -d --build`). That last one is the
test that matters, because it is the only one where the client is not ours.

### Benchmark it

The same `loadgen` is the load harness, and it scores all four axes in one run —
because any one of them is easy to win alone:

```sh
cargo run --release --example loadgen -- \
  --for 30s --conns 64 --batch 8192 --readers 8 \
  --pid $(pgrep -n mira) --data-dir ./data
```

`--conns 0` is read-only, `--readers 0` is write-only, and it prints ingest
throughput, ack latency, six classes of query latency to p999, the server's peak
resident set and the bytes it added to disk per record. [docs/TESTING.md
§3](docs/TESTING.md) is how to read the output, and the four things the harness
teaches you about the engine on the way.

## What is true today

- **Logs, traces and metrics**, stored in their own layouts, over **OTLP/gRPC
  4317** and **OTLP/HTTP 4318**, protobuf or JSON, plain or gzipped — which is
  what a stock collector sends, since both its OTLP exporters compress by
  default.
- **The UI is in the binary.** Open `http://localhost:4318/` — records, trace
  waterfalls and metric charts, served from `include_bytes!`. Nothing to deploy
  beside it.
- **The UI is also in the terminal.** `mira mira` gives the same three tabs, the
  same filter grammar and the same trace waterfall over a `termios` raw mode and
  ANSI — no TUI framework, zero crates added. It reads either a running replica
  (`--addr host:4318`) or a block directory in-process (`--data-dir`), and the
  second one is the point: a detached PVC or a dead pod's volume is still
  readable with nothing running.
- **MCP on `/mcp`.** Four tools over JSON-RPC, no session id, so any replica can
  answer any call. A model asks the same four questions the UI does, through the
  same read path.
- **A query document is checked, not guessed.** A top-level key the endpoint does
  not implement is refused, naming it and listing the ones that exist, so
  `{"signal":"logs","filters":[…]}` never comes back as an answer over the
  unfiltered window. On the query API that refusal is a 400; on `/mcp` it is a 200
  carrying `isError: true`, which is how the protocol says a tool fails, and all
  four tools do it. The client most likely to misspell a key is a model.
- **Correlation edges are readable, not just stored.** A span comes back with its
  events and its links; a metric series comes back with the exemplars that name
  the traces behind it. "Which trace made this spike" is one query, not a second
  system.
- **Nested attributes come back as what they are.** An array or kvlist
  attribute — `process.command_args`, `gen_ai.input.messages` — and a structured
  log body come out of one decoder over the OTLP `AnyValue` the block holds:
  nested JSON from the query API and from MCP, printed inline by both UIs rather
  than elided as `[2 items]`. One place they are not: the terminal's message
  column, which leaves a structured body to the detail pane, because it is one
  cell of a table against a pane that has the width — and that pane stops
  recursing four levels down, where it falls back to the summary. Readable, not
  filterable: no operator compares a list. `LogRecord.event_name` has a column of
  its own and filters like any field, in both UIs' filter boxes too — and the
  terminal's list of which names are columns is diffed against `mira-core`'s
  schema by a test, so a column added there cannot quietly become an attribute
  predicate that matches nothing.
- **Zero-copy queries on the hot tier.** A hot block is read straight out of its
  mapping — asserted, not assumed: a test walks every buffer of every column and
  requires all of them to point inside the mapping. A compacted block gives that
  up, because decompression has to allocate somewhere — so the compaction test
  asserts both sides of the boundary, and it cannot move by accident.
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
- **Two tiers of one format.** A block is written uncompressed so it can be read
  out of its mapping; an hour later the retention sweep rewrites it
  ZSTD-compressed to **0.13** of its size. The codec is per-batch IPC metadata,
  so nothing has to be told which tier it is reading, and an interrupted rewrite
  leaves a directory holding both — which reads correctly and gets finished on
  the next sweep.
- **SIGTERM drains.** Stop accepting, let in-flight exports reach their ack, seal
  and publish the open blocks, exit. A rolling restart costs neither the data nor
  the duplicates a reset-then-retry would have written.
- **Probes that answer for the process.** `/health` and `/readyz` are the same 200
  and the same JSON body — per-signal `shed` and `failed` counts — and neither
  opens the block directory. A path the binary does not serve is a 404, so a probe
  aimed at `/healthz` fails instead of collecting the UI's index page.
- **Two active replicas on one host** need no coordination to share a data
  directory: the block name carries a node id derived from the replica's own
  name, so publishes are independent renames and either replica's `scan` sees
  both. Across hosts this is shared-nothing — see the next bullet, which is the
  same sentence read from the other end.
- **It refuses to start on a network filesystem.** `mmap` over NFS, SMB, CephFS
  and friends turns a server hiccup into `SIGBUS` — a signal, not an error, with
  nothing to catch. A `statfs` at startup names the filesystem and says what to
  point `--data-dir` at instead. FUSE is a warning rather than a refusal,
  because the magic number cannot tell `gcsfuse` from a local one.
- 4.73 MiB stripped, 117 crates, no `protoc`, no node toolchain to build.
  `zstd-sys` is the one C dependency and it vendors its own source, so it needs
  a `cc` — which the linker already required — and nothing installed. `flate2`
  is on its pure-Rust backend so gzip did not change that.

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
| bytes on disk per byte on the wire | 1.31 hot, 0.17 once compacted |
| peak resident set, ingesting at 353k records/s | 615 MiB |
| peak resident set, 8 readers over 10.6M records | 1.06 GiB |

Resident set is `ps`, sampled from outside the process, because Mira reads
through `mmap` and a heap counter would miss the page cache that is most of what
it actually costs a machine.

The query rows were measured before `select_nth_unstable` replaced a full sort
of the match set in `query::search`; on the harness's store that change took
`tail` p99 from 230 ms to 139 ms, so treat them as a ceiling.

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

A block is written uncompressed so it can be read out of its mapping, and 1.31
bytes per byte is what that costs. An hour later the retention sweep rewrites it
ZSTD-compressed: measured over 8 real blocks per signal, **0.127** of the plain
size for logs and **0.142** for traces, which takes the stored figure to about
0.17. The surprise was the read side — a compacted block opens *faster* than a
plain one, in every run, because it is 8× fewer pages to fault and 8× fewer
bytes to CRC and that beats the decompression. Reproduce with `cargo run
--release -p mira-core --example tier -- <partition dir>`; it prints LZ4_FRAME
beside ZSTD, which is how the one C dependency in the tree got justified.

## What is not true yet

- **Not** zero-copy *ingestion*. That is not achievable through protobuf —
  `prost` memcpies every string, unconditionally. The ingest goal is
  allocation-lean: one unavoidable copy of the request body, then no per-field
  heap allocation.
- No block cache: every query re-opens and re-CRCs each block it touches. This
  looked like the next big win until it was measured — it is worth a few
  milliseconds of a 14 ms query, not the 10× that page-fault behaviour was.
- OTAP is the data model, not yet the wire protocol. OTLP on 4317/4318 is the
  universal path; no language SDK emits OTAP today.
- No cross-replica query fan-out. A query reads the block directory it was pointed
  at and nothing scatters it: two replicas sharing a volume both answer for all of
  it, two replicas with a volume each answer for half. There is no peer list to
  configure.
- No entity selector. Every block stores the entity key of each resource it holds
  — the identity that survives an attribute changing mid-rollout — and no read
  surface reads it. What answers "everything this pod emitted" today is an
  attribute predicate over a time range: `{"attr":"service.instance.id","eq":"…"}`.
- No `F_FULLFSYNC` fallback. `File::sync_all` *is* `fcntl(F_FULLFSYNC)` on Apple
  targets — 4.2 ms on this machine against 28 µs for a bare `fsync(2)`, and the ack
  latencies above were measured against it, not against the weaker one. The gap is
  the volumes that answer `EINVAL`/`ENOTSUP`, Docker-for-Mac among them: std does
  not fall back there, and a wrapper that did would have to count it rather than
  quietly weaken the ack.

## Layout

```
crates/mira-proto   vendored OTLP .proto + pure-Rust codegen (protox)
crates/mira-core    Arrow schemas, OTLP encoder, block writer, mmap reader
crates/mira         the binary: receivers, ingest pipeline, retention,
                    query API, MCP, the browser UI and the terminal UI
```

Design, and the reasoning behind every non-obvious choice, is in
[docs/ARCHITECTURE.md](docs/ARCHITECTURE.md). Start with §0 — it lists the
seven mechanisms from the original brief that do not survive contact with the
formats, and what replaced them.

## Contributing

Every gate CI runs is a target in the [`Makefile`](Makefile), and CI calls
nothing else — so `make check` before you push is the same set of checks, in the
same order:

```sh
make            # the target list
make check      # fmt, clippy, tests, rustdoc, UI, supply chain, drift, docs, coverage
```

[CONTRIBUTING.md](CONTRIBUTING.md) is the walkthrough, including why the crate
count and the binary size are checked by a script. Read [ARCHITECTURE.md
§0](docs/ARCHITECTURE.md) first if the change is structural — re-proposing one
of the seven is the most common way to waste an afternoon here. Releases are in
[CHANGELOG.md](CHANGELOG.md); vulnerabilities go to
[SECURITY.md](SECURITY.md), not to an issue.

## Licence

Apache-2.0.
