# Testing Mira end to end

Everything below has been run against a live instance. Nothing here is a plan.

Three levels, cheapest first: the in-process test suite, a live binary fed by
synthetic OTLP, and a stock OpenTelemetry Collector in front of it. Do the first
before every commit and the third before believing anything.

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

In another shell, fill it:

```sh
cargo run --release --example loadgen -- --for 30s --conns 8
```

`loadgen` is a fake shop — four services, five routes, logs, spans and metrics
including a histogram — over OTLP/HTTP. It is deterministic: everything derives
from a counter, so two runs with the same arguments produce the same bytes and
are comparable. It prints achieved rate, which is a floor on what the server
sustained, and it is the front half of the ingest benchmark.

Each connection is its own `service.instance.id`, so `--conns 8` gives eight
instances of each of the four services. That is deliberate: a cumulative counter
belongs to one producer, and eight connections reporting their own totals into
one series would make it fall backwards on nearly every point — which reads as a
restart, and turns the rate chart into a sawtooth.

```
--for 60s      how long to run
--conns 64     concurrent connections
--batch 8192   records per export (the Collector's batch processor default)
--addr HOST:PORT
```

**Use enough connections or you will measure the wrong thing.** Mira acks an
export only once the block containing it is durable, because OTLP's retryable
status set covers exports in flight at a crash (`crates/mira/src/pipeline.rs`).
A block seals on size *or* age, so a run that never reaches the size threshold
sees an ack latency pinned at the age timer — around 2 s — and a per-connection
ceiling of one batch per 2 s. On this machine:

| | records/s | ack p50 |
|---|---|---|
| `--conns 4 --batch 2000` | 2.6k | 2024 ms — the timer |
| `--conns 64 --batch 8192` | 299k | 396 ms — the engine |

Same binary, same data, 115× apart. Raise `--conns` and `--batch` until `p50`
drops away from the block age, and only then read the throughput number.

Use it for volume and for having something to look at. Use telemetrygen below
for fidelity to what real SDKs emit.

## 3. telemetrygen — the OpenTelemetry project's own generator

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

## 4. Reading it back

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
  -d '{"name":"gen","limit":2}'
```

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

Quote it — it is a string, and unquoted YAML reads it as a float. There is no
`offset`; §7.6 of the architecture says why.

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
CLAUDE.md for the headless recipe.

## 5. A stock Collector in front

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

## 6. The OpenTelemetry Demo

The demo is ~15 instrumented microservices producing continuous traffic across
all three signals — the closest thing to a real workload without one. Point its
collector at Mira by adding the same two exporters as
[`docs/e2e/otelcol.yaml`](e2e/otelcol.yaml) to
`src/otel-collector/otelcol-config-extras.yml`, with Mira reachable on the
demo's compose network.

> Not yet run against Mira end to end. Unlike everything above, treat this
> section as a direction rather than a transcript.

## 7. Cleanup

```sh
docker compose -f docs/e2e/compose.yaml down -v
rm -rf /tmp/mira-dev
```

A Mira data directory is only blocks and sidecars — there is no state anywhere
else, no metadata store and nothing registered with anything, so deleting the
directory is a complete uninstall. That is principle 4 being testable rather
than claimed.
