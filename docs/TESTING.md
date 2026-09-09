# Testing Mira end to end

Everything below has been run against a live instance. Nothing here is a plan.

Four levels, cheapest first: the in-process test suite, a live binary fed by
synthetic OTLP, the load harness that scores all four performance axes at once,
and a stock OpenTelemetry Collector in front of it. Do the first before every
commit and the last before believing anything.

`cargo` is not on `PATH` in a non-login shell:

```sh
export PATH="$HOME/.cargo/bin:$PATH"
```

## 1. The suite

```sh
cargo test --workspace
```

This includes the in-process end-to-end tests in `crates/mira/src/e2e.rs`, which
drive the real router — OTLP/HTTP, OTLP/gRPC, the query API, MCP and the UI — via
`tower::ServiceExt::oneshot`. No sockets, no ports, no cleanup. They cover
protobuf and JSON bodies, gzip on both listeners, and the flush-to-query round
trip, so a break in the ingest path fails here before you get as far as a
terminal.

Two of the targets in it are not unit tests and are worth knowing by name:

| Target | What it is |
| --- | --- |
| `crates/mira/tests/cli.rs` | The binary as an operator meets it — argv, exit codes, and a SIGTERM mid-flight that has to leave the block on disk. |
| `crates/mira-core/tests/differential.rs` | The query engine against a reference model. Random stores, random queries, and an oracle that shares no code with the engine. |

The differential test is the closest thing here to a proof. It builds a store of
one to four blocks from random OTLP exports, computes the expected answer with
forty lines of `Vec::filter`, and requires the engine to agree — on the rows, on
their order, on `rows_matched`, and on every page of a cursor walk. What that
pins down is the part no fixture reaches: that block pruning and the Bloom
sidecars never drop a block holding a match, that the cross-block merge is
ordered, and that paging visits every row exactly once.

It is seeded, so it is reproducible. A failure prints the seed:

```sh
MIRA_DIFF_SEED=12858170866899772564 cargo test -p mira-core --test differential
```

That is the loop to stay in. The rest of this document is for the things a
unit test cannot reach: a real socket, a real exporter, and real volume.

## 2. A live instance and the built-in generator

```sh
cargo build --release
./target/release/mira --data-dir /tmp/mira-dev
```

`4317` is OTLP/gRPC, `4318` is OTLP/HTTP plus the query API, MCP and the UI.

```sh
curl -s localhost:4318/health
{"status":"ok","logs":{"shed":0,"failed":0},"traces":{"shed":0,"failed":0},"metrics":{"shed":0,"failed":0}}
```

`/readyz` is the same answer — there is no warm-up and no cluster to join, so
there is no state in which Mira is alive and not ready. The two counters are why
the probe carries a body at all: `shed` is exports refused before the queue,
`failed` is exports accepted and then NACKed. A node that is up and losing data
is the case an up/down probe cannot report.

In another shell, fill it:

```sh
cargo run --release --example loadgen -- --for 30s --conns 8
```

`loadgen` is a fake shop — four services, five routes, logs, spans and metrics
including a histogram — over OTLP/HTTP. Its *content* is deterministic: every
field derives from a counter, so the nth record is the same record in every run.
Its *volume* is not — timestamps come from the wall clock and the run ends on a
deadline, so the record count moves with the machine.

Each connection is its own `service.instance.id`, so `--conns 8` gives eight
instances of each of the four services. That is deliberate: a cumulative counter
belongs to one producer, and eight connections reporting their own totals into
one series would make it fall backwards on nearly every point — which reads as a
restart, and turns the rate chart into a sawtooth.

Use it for volume and for having something to look at. Use telemetrygen (§4)
for fidelity to what real SDKs emit.

## 3. The load harness — all four axes, one command

§11 of the architecture scores four axes, and the reason it scores them together
is that any one of them is easy to win alone. Cache everything and query p99
looks wonderful until you read the resident set; seal tiny blocks and ingest
flies until you count the files. So `loadgen` reports all four from one run:

```sh
./target/release/examples/loadgen \
  --for 30s --conns 64 --batch 8192 --readers 8 \
  --pid $(pgrep -n mira) --data-dir /tmp/mira-dev
```

```
--for 30s        how long to run
--conns 64       writer connections   (0 = read-only)
--readers 8      query threads        (0 = write-only, the default)
--batch 8192     records per export   (the Collector batch processor's default)
--pid N          the server's pid, for the resident-set axis
--data-dir PATH  the server's data dir, for the cost-per-GB axis
--addr HOST:PORT
```

`--pid` and `--data-dir` are how the last two axes get measured *from outside*
the process, which they have to be: Mira reads through `mmap`, so most of what
it costs a machine is page cache it never allocated and a heap counter would
report a flattering number. `ps` and a directory walk cannot be fooled that way.

### Reading it

Three shapes, and they answer different questions. Run them in this order —
write-only to build a store, then read-only against it, then mixed:

```sh
# 1. ingest, memory and bytes-per-record, with nothing competing
./target/release/examples/loadgen --for 30s --conns 64 --batch 8192 \
  --pid $P --data-dir /tmp/mira-dev

# 2. query latency at steady state, on the store run 1 just built
./target/release/examples/loadgen --for 20s --conns 0 --readers 8 \
  --pid $P --data-dir /tmp/mira-dev

# 3. both at once, which is the only one an operator will recognise
./target/release/examples/loadgen --for 30s --conns 64 --batch 8192 --readers 8 \
  --pid $P --data-dir /tmp/mira-dev
```

On an Apple M3 Pro (12 cores), server and harness sharing them:

```
== 1. write-only ==
ingest   352782 records/s   46.1 MiB/s wire   0 shed   0 resets
         5292032 logs + 5292032 spans + 7752 points in 30.0s, 64 conns x 8192 records
         ack p50 430.6ms  p99 2509.0ms  max 2545.4ms
memory   peak RSS 615 MiB
storage  1.77 GiB on disk   +1.77 GiB this run   179 B/record   1.31x the wire bytes

== 2. read-only, 10.6M records on disk ==
query    50 queries/s over 1018 queries
         tail      p50  62.19ms  p99 138.55ms  p999 144.66ms  max 165.63ms   327680 matched
         attr      p50  72.48ms  p99 184.70ms  p999 185.46ms  max 296.63ms    65536 matched
         errors    p50  56.32ms  p99 132.80ms  p999 172.35ms  max 288.84ms    10485 matched
         trace     p50  21.38ms  p99 145.36ms  p999 169.59ms  max 301.63ms        8 matched
         page x10  p50 653.85ms  p99 1101.81ms  p999 1149.39ms  max 1173.31ms  327680 matched  10.0 pages/walk
         series    p50  28.78ms  p99 145.20ms  p999 157.62ms  max 157.81ms      2584 matched
memory   peak RSS 1062 MiB
storage  1.77 GiB on disk
```

The six query classes are not one shape benchmarked six times. `tail` should
read one block and stop; `attr` is what the Bloom sidecar exists for; `errors`
has no index and must scan its window; `trace` has no useful time bound at all,
so every block is a candidate and only `trace.idx` prunes; `page x10` walks ten
pages of a cursor, because "page ten costs what page one cost" is the entire
claim of a keyset cursor and it is only true if someone measures it; `series` is
the metrics route. An engine can be fast at any one of these and unusable.

The `matched` column is not decoration. It is `rows_matched` from each response,
and a query mix that matches nothing measures the empty path and reports
beautiful numbers.

### Four things this harness will teach you, in the order it taught us

**Use enough connections or you measure the timer, not the engine.** Mira acks
an export only once the block holding it is durable, because OTLP's retryable
status set covers exports in flight at a crash. A block seals on size *or* age,
so a run that never reaches the size threshold sits at the age timer — 2 s — and
a per-connection ceiling of one batch per 2 s:

| | records/s | ack p50 |
|---|---|---|
| `--conns 4 --batch 2000` | 2.6k | 2024 ms — the timer |
| `--conns 16 --batch 4096` | 21k | 2063 ms — still the timer |
| `--conns 64 --batch 8192` | 353k | 431 ms — the engine |

Same binary, same data, 135× apart. Raise both until `p50` drops away from the
block age, and only then read the throughput number.

**Query latency is linear in block rows, not in `limit`.** `tail` asks for 100
records and `blocks_scanned` is 1, yet it costs 62 ms — because `rows_matched`
is an honest count, so the whole block's match set is computed before the head
of it is taken. At a 32 MiB target block that is ~330k rows. It is the reason
`select_nth_unstable` replaced a full sort in `query::search`: the sort was
O(n log n) to keep 100 of 327,680, and removing it took this run from 35 to 50
queries/s and `tail` p99 from 230 ms to 139 ms. What is left is the O(n) count
itself, and the lever on it is `target_block_bytes`.

**Do not compare a mixed run's query numbers to a read-only run's.** In a mixed
run the store grows underneath the readers, so an early query answers from a
nearly-empty store and a late one does not. The p50 that comes out is a mixture
of two workloads, not a percentile of one. Worse, on a *fresh* store the paging
class reports ~1.9 pages/walk rather than 10 — not a paging bug, just most walks
running out of rows. Build the store first, then measure reads on it.

**The storage line is a delta.** `+N GiB this run` and `B/record` are computed
against a `du` taken before the run, so pointing the harness at a store that
already has blocks in it still gives a true cost per record. The total on the
same line is not a delta, and is the one to watch across a retention sweep.

Two caveats that are properties of the harness, not the engine. The generator
runs on the same 12 cores as the server, so every number here is a floor —
`--conns 64 --readers 8` is 72 client threads competing with the thing they are
measuring. And a `--conns 0` run reports no ingest section at all, and its
storage line is the total only — with nothing written, the `+N GiB this run` and
`B/record` halves are suppressed rather than reported as zero.

## 4. telemetrygen — the OpenTelemetry project's own generator

`telemetrygen` ships in `opentelemetry-collector-contrib` and produces data
shaped the way the SDKs shape it, which is the point: it is not our idea of a
span.

```sh
go install github.com/open-telemetry/opentelemetry-collector-contrib/cmd/telemetrygen@latest
export PATH="$HOME/go/bin:$PATH"
```

```sh
telemetrygen traces  --otlp-endpoint 127.0.0.1:4317 --otlp-insecure --rate 0 \
  --traces 200 --child-spans 3 --service checkout --status-code Error
telemetrygen logs    --otlp-endpoint 127.0.0.1:4318 --otlp-insecure --otlp-http --rate 0 \
  --logs 500 --service checkout
telemetrygen metrics --otlp-endpoint 127.0.0.1:4317 --otlp-insecure --rate 0 \
  --metrics 300 --service checkout
```

**`--rate 0` is not optional.** The default is `--rate 1`, meaning one item per
second per worker, and a bounded run will look like a hang for as many minutes
as you asked for items. `0` means unthrottled; the three commands above finish
in seconds.

`--otlp-http` switches to the HTTP listener and the endpoint port with it. The
split above is deliberate: traces and metrics over gRPC, logs over HTTP, so one
pass exercises both receivers.

## 5. Reading it back

Blocks are sealed on size or age, so wait a couple of seconds after the last
export before querying. The server logs `block published` when one lands.

```sh
curl -s localhost:4318/api/v1/query -H 'content-type: application/json' \
  -d '{"signal":"traces","limit":2}'

curl -s localhost:4318/api/v1/query -H 'content-type: application/json' \
  -d '{"signal":"logs","where":[{"attr":"service.name","eq":"checkout"}],"limit":5}'
```

`signal` is `logs`, `traces` or `spans` — `spans` is an alias for `traces`.
**Not `metrics`**: metrics are a different shape and have their own two routes,
both `POST`.

```sh
curl -s -X POST localhost:4318/api/v1/metrics/names -H 'content-type: application/json' -d '{}'
curl -s -X POST localhost:4318/api/v1/metrics/query -H 'content-type: application/json' \
  -d '{"name":"gen","max_points":2}'
```

Each route implements its own top-level keys and refuses the rest by name, so
`limit` here is a `400`: `unknown query key "limit"; expected one of name from to
where max_series max_points`. A key nobody implements used to answer `200` over
the unfiltered window — `{"signal":"logs","filters":[…]}` looks like a filter,
is not one, and came back as rows that passed no predicate at all.

Every response carries a `stats` object:

```json
{"blocks_total":2,"blocks_scanned":2,"rows_scanned":600,"rows_matched":1}
```

A response that filled `limit` also carries `next`. Pass it back as `after` to
get the following page; when `next` is absent you have them all.

```sh
curl -s localhost:4318/api/v1/query -H 'content-type: application/json' \
  -d '{"signal":"logs","limit":5,"after":"1757241600000000000.2718281828.7.41"}'
```

Quoted because KYAML quotes every string, not because it has to be — four
dot-separated fields are not a number to any resolver, and the unquoted form
parses identically. There is no `offset`; §7.6 of the architecture says why.

`blocks_scanned` well below `blocks_total` is the sidecar pruning working.
`blocks_scanned == blocks_total` on a filtered query over many blocks means it
is not, and that is the number to watch when you touch anything in
`crates/mira-core/src/query.rs`.

Bodies are KYAML, of which JSON is a subset — `content-type: application/json`
above is just what `curl` users expect to type. `application/yaml` works
identically.

### The other three surfaces

```sh
open http://localhost:4318/                    # the UI, served from the binary
./target/release/mira mira --data-dir /tmp/mira-dev   # the same views in the terminal
./target/release/mira mira --addr localhost:4318      # ...or against a running server
curl -s -X POST localhost:4318/mcp -H 'content-type: application/json' \
  -d '{"jsonrpc":"2.0","id":1,"method":"tools/list"}'
```

`mira mira` with `--data-dir` reads the block directory in-process and needs no
server; with `--addr` it queries one over HTTP. It needs a real terminal — see
CLAUDE.md for the headless recipe, and count the `q`s in the key string you feed
it: each one leaves one mode, so `2t\rq` stops in the trace waterfall and hangs
until it is killed, where `2t\rqqq` unwinds span, waterfall, list and exits.

## 6. A stock Collector in front

This is the test that matters, because it is the only one where the client is
not ours. The stock exporter gzips by default on both transports, batches on its
own schedule, and will drop a batch permanently rather than retry if the server
answers `UNIMPLEMENTED` — so "the collector is happy" is a real result.

```sh
docker compose -f docs/e2e/compose.yaml up -d --build
```

Two containers on one network: Mira (`4317`/`4318` published) and
`otel/opentelemetry-collector-contrib` (`14317`/`14318` published). The
collector config is [`docs/e2e/otelcol.yaml`](e2e/otelcol.yaml) and is
unremarkable on purpose — the `otlp` and `otlphttp` exporters out of the box,
pointed at a hostname. There is no Mira-specific component.

Aim the generators at the collector's ports and query Mira's:

```sh
telemetrygen traces --otlp-endpoint 127.0.0.1:14317 --otlp-insecure --rate 0 \
  --traces 200 --child-spans 3 --service checkout --status-code Error
telemetrygen logs --otlp-endpoint 127.0.0.1:14318 --otlp-insecure --otlp-http --rate 0 \
  --logs 500 --service checkout
telemetrygen metrics --otlp-endpoint 127.0.0.1:14317 --otlp-insecure --rate 0 \
  --metrics 300 --service checkout

curl -s localhost:4318/api/v1/query -H 'content-type: application/json' \
  -d '{"signal":"traces","limit":1}'
```

```sh
docker compose -f docs/e2e/compose.yaml logs -f          # both, interleaved
docker compose -f docs/e2e/compose.yaml down -v          # -v drops the data volume
```

Two things about the setup that cost an hour to learn:

- **Mira has to be in the container too.** Collector-in-Docker against
  Mira-on-the-host does not work on Docker Desktop for Mac:
  `host.docker.internal` resolves to an IPv6 ULA that is not routable from the
  container, and the bridge gateway is not either.
- **The data volume is named, not a bind mount.** Mira `mmap`s its blocks, and a
  Docker Desktop bind mount is FUSE, where an I/O hiccup arrives as `SIGBUS`
  rather than as an error (docs/ARCHITECTURE.md §9). Mira warns about a FUSE
  data directory at startup rather than refusing, because the filesystem magic
  number cannot tell a local FUSE mount from gcsfuse.

The collector will log `"otlp" alias is deprecated; use "otlp_grpc" instead`.
That is about the exporter's own name in recent contrib builds, not about
anything Mira did. The old names are kept in the config because they work on
every version.

## 7. The OpenTelemetry Demo

Twenty instrumented microservices in eight languages, producing continuous
traffic across all three signals from a load generator that drives a real
storefront. It is the closest thing to a production workload you can start with
one command, and it is the only test here where neither the client nor the data
is ours.

Two files in this repo do the whole integration, and the demo has a documented
seam for each — both are empty upstream and both are loaded last, so nothing in
the demo checkout is patched:

| | |
|---|---|
| [`docs/e2e/demo/otelcol-config-extras.yml`](e2e/demo/otelcol-config-extras.yml) | adds `otlp_grpc/mira` and `otlp_http/mira` to the collector's three pipelines |
| [`docs/e2e/demo/compose.mira.yaml`](e2e/demo/compose.mira.yaml) | adds Mira as a service on the demo's compose network |

```sh
git clone --depth 1 https://github.com/open-telemetry/opentelemetry-demo /tmp/otel-demo
docker build -t mira .
cp docs/e2e/demo/otelcol-config-extras.yml /tmp/otel-demo/src/otel-collector/
docker compose -f /tmp/otel-demo/compose.yaml -f docs/e2e/demo/compose.mira.yaml up -d
```

The core `compose.yaml` alone is enough — around 8 GB of images and about 6 GB
of RAM. The `compose.full.yaml` and `compose.observability.yaml` layers add
Kafka, Jaeger, Prometheus and OpenSearch, which are the demo's *own* backends
and are not needed to prove anything about Mira.

Give it two minutes and query Mira on `4318`, published to the host:

```sh
curl -s -X POST localhost:4318/api/v1/metrics/names -H 'content-type: application/json' -d '{}'
curl -s localhost:4318/api/v1/query -H 'content-type: application/json' \
  -d '{"signal":"traces","from":"-15m","to":"now","limit":1}'
./target/release/mira mira --addr localhost:4318     # all three tabs, live
```

What came back on this machine, four minutes in:

- **285 distinct metric names** — 206 sums, 58 gauges, 21 histograms — from the
  OTLP exporters of eight SDKs *plus* the collector's `host_metrics`,
  `docker_stats`, `nginx`, `redis`, `postgresql` and Prometheus receivers. That
  spread of instrument shapes is the reason to run this: no generator produces
  it.
- **Spans from Envoy's C++ SDK, Go, Python, .NET, Java and Node**, with resource
  and scope attributes merged onto the record — `telemetry.sdk.language`,
  `service.namespace`, `otel.scope.name` all intact.
- **Cross-service correlation on real trace ids.** One `frontend-proxy` trace
  came back as six spans over three services, root present and no orphans, with
  `blocks_scanned: 3` of `blocks_total: 102` — the trace sidecar pruning 97% of
  the store on data nobody designed for it.
- **No export failures.** `docker logs otel-collector` shows no permanent errors
  toward either Mira exporter. The startup noise in that log is the demo's own
  `postgresql` and `prometheus/ad` receivers racing their targets.

One thing to expect and not misread: **the newest traces look incomplete.** A
parent span ends after its children, so it is exported after them, and Mira only
answers from sealed blocks. Query a trace ninety seconds old and it is whole.
The waterfall does not currently say "this may still be filling", which is worth
knowing before you go bug-hunting.

`profiles` is deliberately not routed to Mira — it is a fourth OTLP signal with
its own protobuf, Mira does not accept it, and wiring it up would only produce a
permanent export error every few seconds.

Tear it down with the same two `-f` files:

```sh
docker compose -f /tmp/otel-demo/compose.yaml -f docs/e2e/demo/compose.mira.yaml down -v
```

## 8. Cleanup

```sh
docker compose -f docs/e2e/compose.yaml down -v
rm -rf /tmp/mira-dev
```

A Mira data directory is only blocks and sidecars — there is no state anywhere
else, no metadata store and nothing registered with anything, so deleting the
directory is a complete uninstall. That is principle 4 being testable rather
than claimed.
