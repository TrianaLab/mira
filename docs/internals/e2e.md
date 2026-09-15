# Testing Mira end to end

**For:** contributors reproducing the published numbers rather than trusting
them. To *look* at Mira working: [See it work](../demo.md).

`cargo` is not on `PATH` in a non-login shell:

```sh
export PATH="$HOME/.cargo/bin:$PATH"
```

## 1. The suite

```sh
cargo test --workspace
```

`crates/mira/src/e2e.rs` drives the real router — OTLP/HTTP, OTLP/gRPC, the
query API, MCP and the UI — via `tower::ServiceExt::oneshot`. Two targets in it
are not unit tests:

| Target | What it is |
| --- | --- |
| `crates/mira/tests/cli.rs` | The binary as an operator meets it — argv, exit codes, and a SIGTERM mid-flight that has to leave the block on disk. |
| `crates/mira-core/tests/differential.rs` | The query engine against a reference model: random stores, random queries, and an oracle sharing no code with the engine. Checks the rows, their order, `rows_matched`, and every page of a cursor walk. |

A failure prints its seed:

```sh
MIRA_DIFF_SEED=12858170866899772564 cargo test -p miradb-core --test differential
```

## 2. A live instance and the built-in generator

```sh
make build      # two `cargo build --release --locked -p miradb` runs: --bin mira, then --example loadgen
./target/release/mira --data-dir /tmp/mira-dev
```

`cargo build --release` alone does not build an example, and naming both in one
invocation links 371 KiB of `tokio/test-util` and `tower` into the shipped
artifact.

`4317` is OTLP/gRPC, `4318` is OTLP/HTTP plus the query API, MCP and the UI.

```sh
curl -s localhost:4318/health
{"status":"ok","logs":{"shed":0,"failed":0},"traces":{"shed":0,"failed":0},"metrics":{"shed":0,"failed":0}}
```

`/readyz` is a different handler: liveness is a constant 200, readiness turns
503 after 120 seconds of failing publishes (`pipeline::UNREADY_AFTER`):

```json
{"status":"unavailable","signal":"logs","stalled_s":180,
 "reason":"this node has not been able to store an export for this signal; the usual cause is a full or unwritable volume"}
```

### The built-in generator

`loadgen` has two modes:

```sh
./target/release/examples/loadgen --demo --for 45m       # something to look at
./target/release/examples/loadgen --for 30s --conns 8    # something to measure
```

**`--demo` is the realistic one**, and what `make demo` runs: four services in
a call graph, eight spans per trace. In that mode `--for` is the **width of the
history**, not the length of the run, because the UI opens on the last hour.

Without `--demo` it is the load harness, keyed by connection: cumulative
totals from eight writers in *one* series would fall backwards.

A third mode asserts the demo's invariants in memory:

```sh
./target/release/examples/loadgen --selftest
loadgen --selftest ok: 512 spans over 4 services, 25 failed, 5 exceptions, 12 links, 96 exemplars
```

## 3. The load harness

All four performance axes from one run:

```sh
./target/release/examples/loadgen \
  --for 30s --conns 64 --batch 8192 --readers 8 \
  --pid $(pgrep -n mira) --data-dir /tmp/mira-dev
```

```text
--for 30s        stop on a clock     (with --demo: how much history to lay down)
--records N      stop on a count      (use this for anything you will publish)
--conns 64       writer connections   (0 = read-only)
--readers 8      query threads        (0 = write-only, the default)
--batch 8192     records per export   (the Collector batch processor's default)
--pid N          the server's pid, for the resident-set axis
--data-dir PATH  the server's data dir, for the cost-per-GB axis
--emit PATH      append the run's figures as JSON, for measurements.kyaml
--addr HOST:PORT
--demo           the realistic generator instead of the harness (section 2)
--selftest       assert the demo's invariants in memory and exit (section 2)
```

`--records N` divides the rounds evenly across the connections, so it leaves a
reproducible corpus where `--for` does not. `--emit` appends the run's figures
in the shape [the measurement contract](measurement.md) defines;
`scripts/measure/conn-sweep.sh` produces the published set.

### Reading it

Run three shapes in order: write-only to build a store, read-only against it,
then mixed.

```sh
./target/release/examples/loadgen --for 30s --conns 64 --batch 8192 \
  --pid $P --data-dir /tmp/mira-dev              # 1. ingest, memory, B/record
./target/release/examples/loadgen --for 20s --conns 0 --readers 8 \
  --pid $P --data-dir /tmp/mira-dev              # 2. query, on run 1's store
./target/release/examples/loadgen --for 30s --conns 64 --batch 8192 --readers 8 \
  --pid $P --data-dir /tmp/mira-dev              # 3. both, the operator's shape
```

On an Apple M3 Pro (12 cores), server and harness sharing them, **with
`ingest.wal` off**:

```text
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

Six query classes, six shapes: `attr` is what the Bloom sidecar exists for,
`page x10` is the keyset cursor's claim that page ten costs what page one cost.
A mix that matches nothing reports beautiful numbers.

### What this harness teaches

#### Connections

**With the log off, use enough connections or you measure the timer:**

| | records/s | ack p50 |
| --- | --- | --- |
| `--conns 4 --batch 2000` | 2.6k | 2024 ms — the timer |
| `--conns 16 --batch 4096` | 21k | 2063 ms — still the timer |
| `--conns 64 --batch 8192` | 353k | 431 ms — the engine |

**With the log on — the default — the advice reverses**: the ack is a
`write(2)` into the page cache and does not wait for the seal. Same box,
8192-record batches, 30 s each, one fresh server per row, median of three
passes:

| `--conns` | records/s | MiB/s | cores | per core | ack p50 / p99 | peak RSS | the three passes |
| --- | --- | --- | --- | --- | --- | --- | --- |
| 1 | 629,384 | 82.2 | 0.71 | **886,147** | 5.2 ms / 19 ms | 232 MiB | 635k / 609k / 629k |
| 2 | 990,420 | 129.4 | 1.18 | 840,880 | 6.0 ms / 27 ms | 314 MiB | 990k / 1,023k / 911k |
| **4** | **1,350,502** | 176.5 | 1.75 | 770,631 | 8.5 ms / 55 ms | 689 MiB | 1,374k / 1,351k / 1,244k |
| 8 | 1,474,946 | 192.7 | 2.10 | 710,944 | 13.7 ms / 173 ms | 1,243 MiB | 1,564k / 1,475k / 1,436k |
| 16 | 1,508,709 | 197.1 | 2.28 | 662,725 | 30.8 ms / 610 ms | 1,575 MiB | 1,509k / 1,429k / 1,517k |
| **32** | **1,537,875** | 200.9 | 2.23 | 690,358 | 66.5 ms / 963 ms | 1,495 MiB | 1,538k / 1,539k / 1,329k |
| 96 | 1,136,941 | 148.5 | 2.23 | 515,288 | 247.3 ms / 2,661 ms | 1,648 MiB | 1,427k / 1,137k / 1,026k |

**Nothing shed in any of the twenty-one runs**, because `ADMIT_WAIT`
(`pipeline.rs`) parks a full queue for up to five seconds.

**Throughput plateaus between sixteen and thirty-two connections**: 16 and 32
are within 2% on medians whose passes span 6% and 16%. Four connections is the
shape that reproduces best — 10% across three passes against 39% at 96.

#### What `ingest.shards` changed

Until 0.0.3 a signal had one flusher, so connections past the point where that
consumer saturated bought contention rather than work and the curve *fell*:

| `--conns` | one flusher | six flushers | |
| --- | --- | --- | --- |
| 1 | 605,006 | 629,384 | 1.04x |
| 2 | 923,636 | 990,420 | 1.07x |
| 4 | 1,353,967 | 1,350,502 | 1.00x |
| 8 | 1,240,618 | 1,474,946 | 1.19x |
| 16 | 1,216,557 | 1,508,709 | 1.24x |
| 32 | 1,093,645 | 1,537,875 | 1.41x |
| 96 | 734,142 | 1,136,941 | **1.55x** |

Six because this box has twelve cores and the default is half of them
([Architecture section 4](../architecture/ingest.md)). Dispatch is first
fit from shard 0, so a node not saturating one flusher never starts a second.

Peak RSS fell with it, 689 MiB against 1,366 MiB at four connections: one
flusher keeps its 128 slots full of decoded exports at 1.29 MiB each.

#### Per core, not aggregate

`--pid` prints the server's CPU time delta across the run:

```console
$ ./target/release/examples/loadgen --records 20000000 --conns 4 --batch 8192 \
    --pid $P --data-dir /tmp/mira-e2e
ingest   1199561 records/s   156.7 MiB/s wire   0 shed   0 resets
         9961472 logs + 9961472 spans + 14592 points in 16.6s, 4 conns x 8192 records
cpu      1.31 cores busy   912891 records/s/core
```

The per-core figure divides by the *unrounded* core count, so `1199561 / 1.31`
is 915,695 and the line says 912,891.

Quote the per-core rate, not the aggregate: it falls from 886k at one
connection to 515k at 96.

#### Query cost, per row and per point

**Query latency is linear in block bytes, not in `limit`**: `tail` asks for 100
records and scans 1 block, yet costs 62 ms.

```sh
MIRA_BENCH_ROWS=2000000 cargo test --release -p miradb-core --lib scan_cost_per_row -- --nocapture
```

It puts a predicate at **0.04–5.3 ns/row** against **16.4–22.7 ns/row** through
the whole read path, or **9.7–15.8** once the file has settled. The rest is
`Block::open` faulting the mapping in.

Section 11 withdrew a three-pass median because the whole-block ratio moves
1.14× to 2.21× on this box. The metrics route has the same instrument, per
point:

```sh
MIRA_BENCH_POINTS=50000 cargo test --release -p miradb-core --lib series_cost_per_point -- --nocapture
series: 50000 points in 53.905708ms  1.078 us/point  60 series
```

Its attribute join stayed quadratic because nothing priced it: 1,582 ms at
50,000 points in a block before the run search, 53.9 after. **A mix measures
the mix.**

| | |
| --- | --- |
| **Do not compare a mixed run's query numbers to a read-only run's.** | In a mixed run the store grows underneath the readers. On a *fresh* store the paging class reports ~1.9 pages/walk rather than 10 — not a bug, just most walks running out of rows. Build the store first, then measure reads on it. |
| **The storage line is a delta.** | `+N GiB this run` and `B/record` are computed against a `du` taken before the run, so a store that already has blocks in it still gives a true cost per record. The total on the same line is not a delta. |
| **Every number here is a floor.** | The generator runs on the same 12 cores as the server — `--conns 64 --readers 8` is 72 client threads competing with the thing they measure. A `--conns 0` run reports no ingest section and its storage line is the total only. |

### The one-off scripts

Five things the harness cannot express. Each prints a figure
[architecture section 11](../architecture/performance.md) quotes:

| script | answers |
| --- | --- |
| `ingest-probe.sh` | where an export's milliseconds go at 4, 32 and 96 connections, plus the `TOKIO_WORKER_THREADS` 12-against-48 A/B — this is the ingest-ceiling diagnosis |
| `offload-cycle.sh` | ingest → offload → list → restore → read back, and the same retention sweep without `--offload` as the thing the offload sweep is measured against |
| `lazy-detail.sh` | paired A/B of two binaries over one corpus, alternating pass by pass, medians with the sample count asserted |
| `block-reopens.sh` | how many times one process opens the same block, which is the input to the verification-cache decision |
| `restart-replay.sh` | how many rows a corpus gains across a restart, on each of two binaries |

Three habits worth stealing:

| | |
| --- | --- |
| **Abort if the reclaimer fired.** | The three that compare two corpora or two binaries — `lazy-detail.sh`, `offload-cycle.sh`, `restart-replay.sh` — grep the server log for `nearly full`, because a reclaimed corpus is a faster scan and the A/B then reports the volume rather than the code. `ingest-probe.sh` and `block-reopens.sh` have no such guard and do not need one: neither compares two corpora, and both report a ratio taken inside a single run. |
| **Assert the sample count before publishing a median.** | A response body has no trailing newline, so a capture that forgets to re-line-break them silently "medians" one value, and only the count catches it. |
| **Fingerprint the corpus.** | `lazy-detail.sh` takes table count and total bytes before the run and after every pass, and stops the moment it moves. |

**A corpus is not a constant while a server is running on it**: the cold tier
compacts blocks that have aged out of their partition hour, so both arms of an
A/B drift upward together. That bug shipped and published a table that had to
be withdrawn. Leave a server on the corpus until
`find "$CORPUS" -name cold | wc -l` stops climbing.

`lazy-detail.sh`'s knobs:

| var | what it is for |
| --- | --- |
| `A`, `B` | the two binaries. `B` runs first in every pass |
| `ALABEL`, `BLABEL` | the column headers. Section 11 reads these back as "which binary", so set them whenever `B` is not 0.0.3 |
| `CORPUS`, `OUT` | the block directory to measure over, and where the raw samples land |
| `PASSES`, `REPS`, `WARM` | passes of the pair, timed samples per case per pass, and untimed warm-ups before them |
| `REUSE=1` | skip the measuring entirely and re-report over the samples already in `OUT` — which is how you get a second statistic out of a run without paying for it twice, and why `OUT` is no longer cleared unconditionally |

**The paired delta is the answer**: the median of the per-pass differences
cancels drift. When the pooled median beside it disagrees, the pooled one is
measuring the volume.

## 4. telemetrygen

`telemetrygen` shapes data the way the SDKs do: it is not our idea of a span.

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

**`--rate 0` is not optional**: the default is one item per second per worker,
so a bounded run looks like a hang.

The split is deliberate: traces and metrics over gRPC, logs over HTTP, so one
pass exercises both receivers.

## 5. Reading it back

**`localhost` can answer from the wrong Mira**: with the section 7 stack up,
Docker publishes `*:4318` on IPv6 and macOS resolves it to `::1` first. Use
`127.0.0.1`, or bring the container down.

```sh
curl -s localhost:4318/api/v1/query -H 'content-type: application/json' \
  -d '{"signal":"traces","limit":2}'

curl -s localhost:4318/api/v1/query -H 'content-type: application/json' \
  -d '{"signal":"logs","where":[{"attr":"service.name","eq":"checkout"}],"limit":5}'
```

`signal` is `logs`, `traces` or `spans`. **Not `metrics`**, which has its own
two routes, both `POST`.

```sh
curl -s -X POST localhost:4318/api/v1/metrics/names -H 'content-type: application/json' -d '{}'
curl -s -X POST localhost:4318/api/v1/metrics/query -H 'content-type: application/json' \
  -d '{"name":"gen","max_points":2}'
```

Each route refuses keys it does not implement:

```json
{"error":"unknown query key \"limit\"; expected one of name from to where max_series max_points"}
```

Every response carries a `stats` object:

```json
{"blocks_total":1,"blocks_scanned":1,"rows_scanned":15909,"rows_matched":3840,"elapsed_us":102021}
```

`blocks_scanned` well below `blocks_total` is the sidecar pruning working. A
response that filled `limit` carries `next`, to pass back as `after`.

```sh
curl -s localhost:4318/api/v1/query -H 'content-type: application/json' \
  -d '{"signal":"logs","limit":5,"after":"1789067716048102000.841272296.0.14535"}'
```

There is no `offset`; bodies are KYAML, of which JSON is a subset. The
correlation routes take the filter you are looking at:

```sh
curl -s -X POST localhost:4318/api/v1/correlate -H 'content-type: application/json' \
  -d '{"signal":"traces","where":[{"attr":"service.name","eq":"checkout"}]}'
curl -s -X POST localhost:4318/api/v1/map -H 'content-type: application/json' -d '{}'
curl -s -X POST localhost:4318/api/v1/entities -H 'content-type: application/json' -d '{}'
curl -s localhost:4318/api/v1/alerts
```

### The other three surfaces

```sh
open http://localhost:4318/                    # the UI, served from the binary
./target/release/mira mira --data-dir /tmp/mira-dev   # the same views in the terminal
./target/release/mira mira --addr localhost:4318      # ...or against a running server
curl -s -X POST localhost:4318/mcp -H 'content-type: application/json' \
  -d '{"jsonrpc":"2.0","id":1,"method":"tools/list"}'
```

`mira mira` with `--data-dir` reads the block directory in-process; with
`--addr` it queries a server. It needs a pty and an explicit quit:

```sh
printf ']q' | script -q /dev/null ./target/release/mira mira --data-dir /tmp/mira-dev 2>&1 \
  | tr -d '\r' | sed -e 's/\x1b\[[0-9;?]*[a-zA-Z]//g'

# force a size, for a layout you want to look at rather than grep
sh -c 'stty rows 50 cols 200; printf "2t\rqqq" | script -q /dev/null ./target/release/mira mira --data-dir /tmp/mira-dev'
```

Count the `q`s: each leaves one mode. The layout unit tests pass even when
content is being *lost* — `Row` clips at `max` — so a layout change is not
verified until you have looked at it.

## 6. A tier the operator built, on a real cluster

```sh
make operator-e2e
```

About fifteen minutes; it needs `docker`, `kind`, `kubectl` and `helm`. The
client is a stock `otel/opentelemetry-collector-contrib`.
`integrations/kubernetes/e2e/run.sh`, in order:

| | |
| --- | --- |
| | `kind create cluster`, then `make operator-apiserver` — the API-server tests first, because a CRD the server prunes a field out of fails in thirty seconds rather than after two image builds |
| | `docker build` the engine and the operator, `kind load`, and `helm install` the chart **as published** — a broken template or a missing RBAC rule fails here rather than in somebody's cluster — then the operator's lease, held under its own pod name |
| A | a `MiraCluster` becomes a running tier: StatefulSet 2/2, proxy 1/1, and five objects the CR never named |
| B | `integrations/kubernetes/e2e/telemetry.yaml` — the collector, config byte-for-byte from the compose scenario — plus four one-shot `telemetrygen` Jobs, and all three signals come back |
| C | `spec.replicas: 3`, and a row written straight at `tel-2` comes back *through the proxy* |
| D | `spec.replicas: 2`, and the drained replica's blocks are on the cold volume before its claim is deleted |
| E | `helm uninstall` the operator, and the tier still ingests and serves |

E is the assertion the architecture rests on: principle 4 says Mira holds no
coordination state, so a controller must not be Mira.

Five non-obvious things:

| | |
| --- | --- |
| **The generators use three different services.** | Ingest routes on `hash(resource) % n`, so one `--service` for all four puts the entire corpus on one replica — C's merge assertion then passes against a proxy that is only forwarding, and D archives an empty volume. C's fifth generator goes straight at `tel-2.tel-headless:4317` for the same reason: a row that provably lives on exactly one replica is the only honest test of a merge. |
| **Metrics are asked of the replicas, not of the proxy.** | `/api/v1/metrics/names` is built by walking one node's blocks and there is no cursor to merge two nodes' answers on, so a proxy answers `501` and says so. `scripts/wait-for-signals.sh 240 assert traces logs` covers the two a proxy can merge; an in-cluster Pod asks both replicas for the third. |
| **The scale thresholds are set so `Down` always wins**, the opposite of what it looks like it should be. | `spec.replicas` is a floor: raising it grows the tier outright, lowering it only *permits* a shrink, because a drain archives a volume and then deletes it and the operator wants the replicas to agree the data fits first. So D cannot patch the floor and wait — it has to make that agreement unconditional, and `downWhenFreeAbove: 0.002` is true on any node this suite could run on at all. The scale decision itself belongs to the request-log tests; what D tests is floor, drain, archive, claim, in order. |
| **Both images are built inside Docker**, not copied in from the host. | A Mach-O binary in a Linux image fails four minutes later as silence. |
| **The forward is on `14318`.** | `make demo` binds `4318`, and a port-forward that cannot bind is a failure fifteen minutes into a run that had nothing wrong with it. |

Debugging one:

```sh
KEEP=1 make operator-e2e        # leave the cluster up on failure
kind export kubeconfig --name mira-operator-e2e     # the run's own is a temp file
kubectl --context kind-mira-operator-e2e -n mira-e2e get miracluster,sts,po,pvc
kubectl --context kind-mira-operator-e2e -n mira-system logs deploy/mira-operator
kind delete cluster --name mira-operator-e2e
```

The run keeps `KUBECONFIG` in a temp file, so it cannot leave your shell
pointed at Kind, and prints objects, events and operator logs before deleting
anything.

## 7. The OpenTelemetry Demo

Twenty instrumented microservices in eight languages — the only test where
neither the client nor the data is ours. Two files do the integration:

| | |
| --- | --- |
| [`docs/e2e/demo/otelcol-config-extras.yml`](../e2e/demo/otelcol-config-extras.yml) | adds `otlp_grpc/mira` and `otlp_http/mira` to the collector's three pipelines |
| [`docs/e2e/demo/compose.mira.yaml`](../e2e/demo/compose.mira.yaml) | adds Mira as a service on the demo's compose network |

```sh
git clone --depth 1 https://github.com/open-telemetry/opentelemetry-demo /tmp/otel-demo
docker build -t mira .
cp docs/e2e/demo/otelcol-config-extras.yml /tmp/otel-demo/src/otel-collector/
docker compose -f /tmp/otel-demo/compose.yaml -f docs/e2e/demo/compose.mira.yaml up -d
```

The core `compose.yaml` alone is enough: about 8 GB of images and 6 GB of RAM.
Query Mira on `4318` after two minutes:

```sh
curl -s -X POST localhost:4318/api/v1/metrics/names -H 'content-type: application/json' -d '{}'
curl -s localhost:4318/api/v1/query -H 'content-type: application/json' \
  -d '{"signal":"traces","from":"-15m","to":"now","limit":1}'
./target/release/mira mira --addr localhost:4318     # all three tabs, live
```

Four minutes in, this machine had **285 distinct metric names** (206 sums, 58
gauges, 21 histograms) and spans from Envoy's C++ SDK, Go, Python, .NET, Java
and Node.

**The newest traces look incomplete**: a parent span is exported after its
children, so query one ninety seconds old. `profiles` is deliberately not
routed to Mira.

```sh
docker compose -f /tmp/otel-demo/compose.yaml -f docs/e2e/demo/compose.mira.yaml down -v
```

## 8. Cleanup

```sh
kind delete cluster --name mira-operator-e2e
make demo-clean
rm -rf /tmp/mira-dev
```

A Mira data directory is only blocks and sidecars, so deleting it is a complete
uninstall.
