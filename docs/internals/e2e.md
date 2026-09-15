# Testing Mira end to end

**For:** contributors, and anyone who wants to reproduce the published numbers
rather than trust them. Every command here has been run against a live instance.
To just *look* at Mira working, `make demo` is one command —
[See it work](../demo.md).

`cargo` is not on `PATH` in a non-login shell:

```sh
export PATH="$HOME/.cargo/bin:$PATH"
```

## 1. The suite

```sh
cargo test --workspace
```

Includes `crates/mira/src/e2e.rs`, which drives the real router — OTLP/HTTP,
OTLP/gRPC, the query API, MCP and the UI — via `tower::ServiceExt::oneshot`. No
sockets, no ports, no cleanup. Two targets in it are not unit tests:

| Target | What it is |
| --- | --- |
| `crates/mira/tests/cli.rs` | The binary as an operator meets it — argv, exit codes, and a SIGTERM mid-flight that has to leave the block on disk. |
| `crates/mira-core/tests/differential.rs` | The query engine against a reference model: random stores, random queries, and an oracle sharing no code with the engine. Checks the rows, their order, `rows_matched`, and every page of a cursor walk. |

The differential test is seeded, and a failure prints the seed:

```sh
MIRA_DIFF_SEED=12858170866899772564 cargo test -p miradb-core --test differential
```

## 2. A live instance and the built-in generator

```sh
make build      # two `cargo build --release --locked -p miradb` runs: --bin mira, then --example loadgen
./target/release/mira --data-dir /tmp/mira-dev
```

Name the example: `cargo build --release` alone does **not** build one, and the
`target/release/examples/loadgen` every command below runs is then absent. Name
it in a *second* invocation too: an example compiles against the
dev-dependencies, so selecting both in one command unifies their features into
the binary's own graph and links 371 KiB of `tokio/test-util` and `tower`
middleware into the shipped artifact. `make build` gets this right; the
reasoning is in its comment.

`4317` is OTLP/gRPC, `4318` is OTLP/HTTP plus the query API, MCP and the UI.

```sh
curl -s localhost:4318/health
{"status":"ok","logs":{"shed":0,"failed":0},"traces":{"shed":0,"failed":0},"metrics":{"shed":0,"failed":0}}
```

`shed` is exports refused before the queue, `failed` is exports accepted and
then NACKed: a node that is up and losing data is what an up/down probe cannot
report.

`/readyz` is **not** the same handler. Liveness is a constant 200 — a flusher
that stops takes the process with it, so answering at all is the answer, and
restarting a node whose volume is full fixes nothing. Readiness asks whether an
export can still be made *durable*, so it turns 503 once publishes have been
failing for `pipeline::UNREADY_AFTER` — 120 seconds — the state that should take
a node out of a Service's endpoints. `stalled_s` appears only in that body, so
it is never below 120:

```json
{"status":"unavailable","signal":"logs","stalled_s":180,
 "reason":"this node has not been able to store an export for this signal; the usual cause is a full or unwritable volume"}
```

`GET /api/v1/stats` is the longer version — uptime, peak RSS, disk headroom,
query counters, and per signal the rows, blocks, bytes and open-block age.

In another shell, fill it. `loadgen` has two modes:

```sh
./target/release/examples/loadgen --demo --for 45m       # something to look at
./target/release/examples/loadgen --for 30s --conns 8    # something to measure
```

**`--demo` is the realistic one**, and it is what `make demo` runs. Four
services — `frontend`, `inventory`, `checkout`, `payments` — wired into a call
graph, so every trace is eight spans crossing three service boundaries. One
request in thirteen fails with an `exception` span event carrying
`exception.type`, `exception.message` and `exception.stacktrace`. There are span
links, structured log bodies, array and kvlist attributes, and exemplars naming
trace ids that really exist.

In this mode `--for` is the **width of the history**, not the length of the
run: the data is backdated across that window — the UI opens on the last hour,
so a generator that stamps everything "now" leaves a two-point chart — and the
process exits as soon as the last export is acked.

**Without `--demo` it is the load harness**, the same shop flattened into a
firehose aimed at the ingest path. Its content is deterministic (the nth record
is the same record in every run); its volume is not, because the run ends on a
deadline.

Metrics are keyed by connection — one `service.instance.id` per writer, so
`--conns 8` gives eight instances of each service. Eight connections reporting
cumulative totals into *one* series would make it fall backwards on nearly every
point, which reads as a restart. Logs and spans key theirs off the pod.

A third mode talks to nothing, and asserts in memory the invariants the demo is
only useful if it has — trace tree nesting, spread timestamps, bucket counts
summing to `count`, every exemplar naming a trace that exists:

```sh
./target/release/examples/loadgen --selftest
loadgen --selftest ok: 512 spans over 4 services, 25 failed, 5 exceptions, 12 links, 96 exemplars
```

It is a flag rather than a `#[test]` because Cargo defaults examples to
`test = false`. `make test` runs it, so CI does too.

## 3. The load harness

All four performance axes from one run, because any one of them is easy to win
alone:

```sh
./target/release/examples/loadgen \
  --for 30s --conns 64 --batch 8192 --readers 8 \
  --pid $(pgrep -n mira) --data-dir /tmp/mira-dev
```

```
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

`--pid` and `--data-dir` measure the last two axes *from outside* the process,
which they have to be: Mira reads through `mmap`, so most of what it costs a
machine is page cache it never allocated and a heap counter would report a
flattering number.

`--records` and `--for` are the same run with different stopping conditions, and
only one of them leaves a reproducible corpus behind. `--records N` divides the
rounds evenly across the connections before the first byte goes out, so a slow
box sends exactly the bytes a fast one did and takes longer; `--for` sends
whatever fits in the time, which measures the afternoon. `--emit` writes what the
run measured, keyed the way `measurements.kyaml` is keyed, appending one object
per line so a multi-pass sweep lands in one file —
[the measurement contract](measurement.md) is what those keys mean and
`scripts/measure/conn-sweep.sh` is the sweep that produces the published set.

### Reading it

Run three shapes in this order — write-only to build a store, read-only against
it, then mixed. `--conns 64` below is tuned for the run these captures came
from, which had `ingest.wal` off; on the default configuration throughput
plateaus between sixteen and thirty-two connections and 64 is past it, for the
reason the sweep further down gives:

```sh
./target/release/examples/loadgen --for 30s --conns 64 --batch 8192 \
  --pid $P --data-dir /tmp/mira-dev              # 1. ingest, memory, B/record
./target/release/examples/loadgen --for 20s --conns 0 --readers 8 \
  --pid $P --data-dir /tmp/mira-dev              # 2. query, on run 1's store
./target/release/examples/loadgen --for 30s --conns 64 --batch 8192 --readers 8 \
  --pid $P --data-dir /tmp/mira-dev              # 3. both, the operator's shape
```

On an Apple M3 Pro (12 cores), server and harness sharing them, **with
`ingest.wal` off** — so the ack column is block-seal-bound rather than the
page-cache write the current default makes it:

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

The six query classes are six different shapes, not one benchmarked six times.
`tail` should read one block and stop; `attr` is what the Bloom sidecar exists
for; `errors` has no index and must scan its window; `trace` has no useful time
bound, so every block is a candidate and only `trace.idx` prunes; `page x10`
walks ten pages of a cursor, because "page ten costs what page one cost" is the
whole claim of a keyset cursor; `series` is the metrics route.

The `matched` column is `rows_matched` from each response. A query mix that
matches nothing measures the empty path and reports beautiful numbers.

### What this harness teaches

**With the log off, use enough connections or you measure the timer.** A block
seals on size *or* age, so a run that never reaches the size threshold sits at
the age timer — 2 s — and a per-connection ceiling of one batch per 2 s:

| | records/s | ack p50 |
|---|---|---|
| `--conns 4 --batch 2000` | 2.6k | 2024 ms — the timer |
| `--conns 16 --batch 4096` | 21k | 2063 ms — still the timer |
| `--conns 64 --batch 8192` | 353k | 431 ms — the engine |

Same binary, same data, 135x apart. Raise both until `p50` drops away from the
block age, and only then read the throughput number.

**With the log on — the default — the advice reverses, so do not carry it
over.** The ack is a `write(2)` into the page cache and does not wait for the
seal, so there is no age-timer floor to climb away from, and connections stop
buying throughput long before the cores run out. Same box, same 8192-record
batches, 30 s each, one fresh server per row — **the median of three full
passes**, with the individual passes in the last column because on some rows
they disagree by more than the medians do:

| `--conns` | records/s | MiB/s | cores | per core | ack p50 / p99 | peak RSS | the three passes |
|---|---|---|---|---|---|---|---|
| 1 | 629,384 | 82.2 | 0.71 | **886,147** | 5.2 ms / 19 ms | 232 MiB | 635k / 609k / 629k |
| 2 | 990,420 | 129.4 | 1.18 | 840,880 | 6.0 ms / 27 ms | 314 MiB | 990k / 1,023k / 911k |
| **4** | **1,350,502** | 176.5 | 1.75 | 770,631 | 8.5 ms / 55 ms | 689 MiB | 1,374k / 1,351k / 1,244k |
| 8 | 1,474,946 | 192.7 | 2.10 | 710,944 | 13.7 ms / 173 ms | 1,243 MiB | 1,564k / 1,475k / 1,436k |
| 16 | 1,508,709 | 197.1 | 2.28 | 662,725 | 30.8 ms / 610 ms | 1,575 MiB | 1,509k / 1,429k / 1,517k |
| **32** | **1,537,875** | 200.9 | 2.23 | 690,358 | 66.5 ms / 963 ms | 1,495 MiB | 1,538k / 1,539k / 1,329k |
| 96 | 1,136,941 | 148.5 | 2.23 | 515,288 | 247.3 ms / 2,661 ms | 1,648 MiB | 1,427k / 1,137k / 1,026k |

**Nothing shed, on any row, in any of the twenty-one runs.** That column used to
be the interesting one — an earlier revision shed the moment the queue was full
and gave back 93% 503s at 96 connections — and `ADMIT_WAIT` (`pipeline.rs`)
removed it by parking a full queue for up to five seconds instead. A run of your
own that *does* shed is measuring a machine that cannot keep up, not this curve.

**Throughput climbs to a plateau between sixteen and thirty-two connections.**
Read the last column before believing otherwise: 16 and 32 are within 2% of each
other on medians whose passes span 6% and 16%, so the ordering between them is
noise and the plateau is the honest reading. Four connections is still the number
to quote for a paired comparison, because it is the shape that reproduces —
10% across three passes, against 39% at 96 — and it buys 1.35M records/s at
689 MiB of RSS and a 55 ms ack p99. What thirty-two buys on top is 14% more
throughput for eighteen times the ack p99.

**That shape is new, and it is what `ingest.shards` bought.** Until 0.0.3 a
signal had one flusher, so every connection past the point where that consumer
saturated bought contention rather than work, and the curve *fell* from four
connections onward. The paired A/B — the same three-pass sweep, the same box, the
pre-sharding binary and this one run back to back:

| `--conns` | one flusher | six flushers | |
|---|---|---|---|
| 1 | 605,006 | 629,384 | 1.04x |
| 2 | 923,636 | 990,420 | 1.07x |
| 4 | 1,353,967 | 1,350,502 | 1.00x |
| 8 | 1,240,618 | 1,474,946 | 1.19x |
| 16 | 1,216,557 | 1,508,709 | 1.24x |
| 32 | 1,093,645 | 1,537,875 | 1.41x |
| 96 | 734,142 | 1,136,941 | **1.55x** |

Six because this box has twelve cores and the default is half of them
([Architecture section 4](../architecture.md#4-ingest-path)). The rows below four
connections are unchanged and that is the design: dispatch is first fit from
shard 0, so a node that is not saturating one flusher never starts a second and
goes on producing one block per seal window rather than six nearly-empty ones.
The gain begins exactly where the old curve began to fall.

Peak RSS fell with it, which was not the goal. At four connections it is 689 MiB
against 1,366 MiB, and the harness's anonymous figure 745 MiB against 1,446 MiB.
Six open blocks per signal is *more* block state than one, so the saving is not
block state: it is the queue. One flusher behind four connections keeps its 128
slots full of decoded exports at 1.29 MiB each; six flushers drain theirs, and
the exports that used to sit in the queue are not resident at all.

Divide by cores, not by connections. `--pid` makes the harness take the
server's CPU time either side of the run and print the delta, so the `cpu` line
reports both:

```
$ ./target/release/examples/loadgen --records 20000000 --conns 4 --batch 8192 \
    --pid $P --data-dir /tmp/mira-e2e
ingest   1199561 records/s   156.7 MiB/s wire   0 shed   0 resets
         9961472 logs + 9961472 spans + 14592 points in 16.6s, 4 conns x 8192 records
cpu      1.31 cores busy   912891 records/s/core
```

Do not check that division to two decimals: the per-core figure divides by the
*unrounded* core count, so `1199561 / 1.31` is 915,695 and the line says 912,891.

The aggregate rate is a property of the offered load — raise `--conns` and it
moves without a line of the server changing. Quote the per-core rate: it is a
property of the engine, and the column that behaves, falling across the whole
sweep from 886k at one connection to 515k at 96 while the aggregate
rises and then falls. Ten of twelve cores are idle at the plateau, so the added
connections buy contention.

**Check what else is running before you believe a run.** An earlier pass of this
same sweep, taken with a 294%-CPU virtual machine and a `go build` on the box,
read 1,165,623 at four connections — 14% under the median above, on the same
binary and the same command. Nothing in the output says so; the only tell is
`uptime`. Take three passes and print the load average beside each.

**Query latency is linear in block bytes, not in `limit`.** `tail` asks for 100
records and scans 1 block, yet costs 62 ms. `rows_matched` is an honest count,
so the whole block's match set is computed before the head of it is taken — at a
32 MiB target block that is ~205k rows — but that part is nearly free:
```sh
MIRA_BENCH_ROWS=2000000 cargo test --release -p miradb-core --lib scan_cost_per_row -- --nocapture
```

`-- --nocapture` is not optional. Every figure this test reports is a
`println!`, and libtest swallows stdout on a passing test — without it the
command runs to a silent `ok` and prints nothing.

It puts a predicate at **0.04–5.3 ns/row** against **16.4–22.7 ns/row** for the
same block through the whole read path, or **9.7–15.8** once the file has
settled and the checksum is not re-run (four passes, 2,000,000 rows). The rest is
`Block::open` faulting the mapping in, which is why `limit 1` costs what the
whole block costs: on the dearest predicate, `body contains` at 4.99 ns/row
against a settled path of 11.44, the open is 56% of the query, and on the
cheapest it is essentially all of it.

That test prints **two** columns per predicate, and the second one is the point
of it. It publishes the block, times the read path with checksum verification
on, backdates the files so the process-scoped verification cache accepts them
(`block.rs`'s `SETTLED`), and times it again — so the checksum's share is a
column of one run rather than something inferred from a rate. It reads between a
quarter and two fifths at 2,000,000 rows. Backdating rather than sleeping is
deliberate: the default 4,096-row size of this test runs in `make test`, and two
seconds of waiting does not belong in the normal suite.

**Take more than three passes of it.** The whole-block ratio moves 1.14× to
2.21× pass to pass on this box; nine paired passes put the median at 1.37× and
1.47× on two binaries whose ranges overlap almost entirely. A three-pass median
off that spread is a number with one significant figure wearing two, and section
11 withdrew one for that reason. Every case it times asserts `hit > 0`, so every
case returns rows, and a query that returns rows reads the attribute tables to
render them: a change to *which* tables an open reads is invisible here by
construction, and `scripts/measure/lazy-detail.sh` is the instrument for it.

The metrics route has the same instrument, per point rather than per row:

```sh
MIRA_BENCH_POINTS=50000 cargo test --release -p miradb-core --lib series_cost_per_point -- --nocapture
series: 50000 points in 53.905708ms  1.078 us/point  60 series
```

Both are scaled tests rather than `#[ignore]`d benchmarks, so the default size
runs in `make test` as a correctness check and the same code is the measurement
at a real one. The metrics attribute join stayed quadratic through the release
that removed the same shape from the log path because nothing priced it: at
50,000 points in a block it was 1,582 ms before the run search and 53.9 after.
The load harness does not show it — its metrics blocks hold ~1,600 points, where
the join is about 7% of the query — which is the general lesson: **a mix
measures the mix. Price the term you changed separately, or a 29× fix reads as
noise.**

| | |
|---|---|
| **Do not compare a mixed run's query numbers to a read-only run's.** | In a mixed run the store grows underneath the readers. On a *fresh* store the paging class reports ~1.9 pages/walk rather than 10 — not a bug, just most walks running out of rows. Build the store first, then measure reads on it. |
| **The storage line is a delta.** | `+N GiB this run` and `B/record` are computed against a `du` taken before the run, so a store that already has blocks in it still gives a true cost per record. The total on the same line is not a delta. |
| **Every number here is a floor.** | The generator runs on the same 12 cores as the server — `--conns 64 --readers 8` is 72 client threads competing with the thing they measure. A `--conns 0` run reports no ingest section and its storage line is the total only. |

### The one-off scripts

Five things the harness cannot express, because each needs a second process, a
second binary, a restart in the middle or the internal probes on. They live in
`scripts/measure/`, they are POSIX `sh`, and each prints the figure that a
bullet in [architecture section 11](../architecture.md#11-performance-model)
quotes:

| script | answers |
|---|---|
| `ingest-probe.sh` | where an export's milliseconds go at 4, 32 and 96 connections, plus the `TOKIO_WORKER_THREADS` 12-against-48 A/B — this is the ingest-ceiling diagnosis |
| `offload-cycle.sh` | ingest → offload → list → restore → read back, and the same retention sweep without `--offload` as the thing the offload sweep is measured against |
| `lazy-detail.sh` | paired A/B of two binaries over one corpus, alternating pass by pass, medians with the sample count asserted |
| `block-reopens.sh` | how many times one process opens the same block, which is the input to the verification-cache decision |
| `restart-replay.sh` | how many rows a corpus gains across a restart, on each of two binaries |

Three habits are worth stealing:

| | |
|---|---|
| **Abort if the reclaimer fired.** | The three that compare two corpora or two binaries — `lazy-detail.sh`, `offload-cycle.sh`, `restart-replay.sh` — grep the server log for `nearly full`, because a reclaimed corpus is a faster scan and the A/B then reports the volume rather than the code. `ingest-probe.sh` and `block-reopens.sh` have no such guard and do not need one: neither compares two corpora, and both report a ratio taken inside a single run. |
| **Assert the sample count before publishing a median.** | A response body has no trailing newline, so a capture that forgets to re-line-break them silently "medians" one value, and only the count catches it. |
| **Fingerprint the corpus.** | `lazy-detail.sh` takes table count and total bytes before the run and after every pass, and stops the moment it moves. |

That last one is a bug that shipped, not defensive programming. **A corpus is
not a constant while a server is running on it.** The cold tier compacts
blocks that have aged out of their partition hour, eight per signal per sweep,
from inside the server the harness keeps starting — so a run over freshly
written blocks begins on a plain corpus and ends on a compacted one, both arms
drift upward together, and the pooled median reports how far through the
transition each pass landed. It published a table that had to be withdrawn.
There is no knob to turn it off (`docs/config.md`: no compaction settings,
deliberately), so the harness refuses instead: leave a server on the corpus
until `find "$CORPUS" -name cold | wc -l` stops climbing, then run.

Its knobs, since the defaults are only right for the pair it was written for:

| var | what it is for |
|---|---|
| `A`, `B` | the two binaries. `B` runs first in every pass |
| `ALABEL`, `BLABEL` | the column headers. Section 11 reads these back as "which binary", so set them whenever `B` is not 0.0.3 |
| `CORPUS`, `OUT` | the block directory to measure over, and where the raw samples land |
| `PASSES`, `REPS`, `WARM` | passes of the pair, timed samples per case per pass, and untimed warm-ups before them |
| `REUSE=1` | skip the measuring entirely and re-report over the samples already in `OUT` — which is how you get a second statistic out of a run without paying for it twice, and why `OUT` is no longer cleared unconditionally |

It prints two deltas per case and **the paired one is the answer**: the median of
the per-pass differences, which cancels the drift that every process mapping a
multi-gigabyte corpus puts on the box. The pooled median over all
`PASSES × REPS` samples is printed beside it, and when the two disagree it is the
pooled one that is measuring the volume. Read the two controls to confirm which.

## 4. telemetrygen

`telemetrygen` ships in `opentelemetry-collector-contrib` and shapes data the way
the SDKs do, which is the point: it is not our idea of a span.

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

**`--rate 0` is not optional.** The default is `--rate 1` — one item per second
per worker — and a bounded run then looks like a hang for as many minutes as you
asked for items. `0` is unthrottled; the three commands above finish in seconds.

`--otlp-http` switches to the HTTP listener and the endpoint port with it. The
split above is deliberate: traces and metrics over gRPC, logs over HTTP, so one
pass exercises both receivers.

## 5. Reading it back

**`localhost` can answer from the wrong Mira.** If the demo stack in section 7
is up, Docker has published `*:4318` on IPv6 and macOS resolves `localhost` to
`::1` first — so every command here reaches the *container* rather than the
binary section 2 started, and answers with a `stats` object that is valid,
plausible and about someone else's store. The tell is `blocks_total`: a store
you know is large answering as if it were nearly empty. Use `127.0.0.1`, or
bring the container down.

```sh
curl -s localhost:4318/api/v1/query -H 'content-type: application/json' \
  -d '{"signal":"traces","limit":2}'

curl -s localhost:4318/api/v1/query -H 'content-type: application/json' \
  -d '{"signal":"logs","where":[{"attr":"service.name","eq":"checkout"}],"limit":5}'
```

`signal` is `logs`, `traces` or `spans` — `spans` is an alias for `traces`.
**Not `metrics`**: metrics are a different shape with their own two routes, both
`POST`.

```sh
curl -s -X POST localhost:4318/api/v1/metrics/names -H 'content-type: application/json' -d '{}'
curl -s -X POST localhost:4318/api/v1/metrics/query -H 'content-type: application/json' \
  -d '{"name":"gen","max_points":2}'
```

Each route implements its own top-level keys and refuses the rest by name, so
`limit` here is a `400`:

```json
{"error":"unknown query key \"limit\"; expected one of name from to where max_series max_points"}
```

Every response carries a `stats` object:

```json
{"blocks_total":1,"blocks_scanned":1,"rows_scanned":15909,"rows_matched":3840,"elapsed_us":102021}
```

`blocks_scanned` well below `blocks_total` is the sidecar pruning working.
`blocks_scanned == blocks_total` on a filtered query over many blocks means it
is not, and that is the number to watch when you touch anything in
`crates/mira-core/src/query.rs`.

A response that filled `limit` also carries `next`. Pass it back as `after` for
the following page; when `next` is absent you have them all.

```sh
curl -s localhost:4318/api/v1/query -H 'content-type: application/json' \
  -d '{"signal":"logs","limit":5,"after":"1789067716048102000.841272296.0.14535"}'
```

There is no `offset`. Bodies are KYAML, of which JSON is a subset —
`content-type: application/json` above is just what `curl` users expect to type;
`application/yaml` works identically.

The correlation routes take the filter you are already looking at:

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

`mira mira` with `--data-dir` reads the block directory in-process and needs no
server; with `--addr` it queries one over HTTP.

It needs a real terminal, and stdin EOF does not close it — so driving it from a
script means a pty and an explicit quit. `script -q /dev/null` supplies the pty;
the `sed` strips the escape sequences so the output is diffable:

```sh
printf ']q' | script -q /dev/null ./target/release/mira mira --data-dir /tmp/mira-dev 2>&1 \
  | tr -d '\r' | sed -e 's/\x1b\[[0-9;?]*[a-zA-Z]//g'

# force a size, for a layout you want to look at rather than grep
sh -c 'stty rows 50 cols 200; printf "2t\rqqq" | script -q /dev/null ./target/release/mira mira --data-dir /tmp/mira-dev'
```

Count the `q`s: each one leaves one mode, so `2t\rq` stops in the trace
waterfall and hangs until it is killed, where `2t\rqqq` unwinds span, waterfall,
list and exits. The layout unit tests pass even when content is being *lost* —
`Row` clips at `max` rather than overflowing — so a layout change is not
verified until you have looked at it.

## 6. A tier the operator built, on a real cluster

```sh
make operator-e2e
```

One command, about fifteen minutes, and it needs `docker`, `kind`, `kubectl` and
`helm`. It is the only gate in the tree that stands anything up, and it is also
the only test where the client is not ours — the exporter on the wire is a stock
`otel/opentelemetry-collector-contrib`, which gzips by default on both
transports, batches on its own schedule, and drops a batch permanently rather
than retry if the server answers `UNIMPLEMENTED`.

It replaced a docker compose file that put that same collector in front of one
Mira container. Everything that one asserted is asserted here, through a tier
the operator built, so there is one end-to-end suite rather than two.

`integrations/kubernetes/e2e/run.sh` is the whole thing. In order:

| | |
|---|---|
| | `kind create cluster`, then `make operator-apiserver` — the API-server tests first, because a CRD the server prunes a field out of fails in thirty seconds rather than after two image builds |
| | `docker build` the engine and the operator, `kind load`, and `helm install` the chart **as published** — a broken template or a missing RBAC rule fails here rather than in somebody's cluster |
| A | a `MiraCluster` becomes a running tier: StatefulSet 2/2, proxy 1/1, and four objects the CR never named |
| B | `integrations/kubernetes/e2e/telemetry.yaml` — the collector, config byte-for-byte from the compose scenario — plus four one-shot `telemetrygen` Jobs, and all three signals come back |
| C | `spec.replicas: 3`, and a row written straight at `tel-2` comes back *through the proxy* |
| D | `spec.replicas: 2`, and the drained replica's blocks are on the cold volume before its claim is deleted |
| E | `helm uninstall` the operator, and the tier still ingests and serves |

E is the assertion the architecture rests on: principle 4 says Mira holds no
coordination state, and the whole defence of shipping a controller is that a
controller is not Mira — a testable claim, so it is tested.

Five things about the setup that are not obvious:

| | |
|---|---|
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

The run exports `KUBECONFIG` to a temp file of its own and never calls
`kubectl config use-context`, so it can neither be redirected by the machine's
current context nor leave your shell pointed at Kind. A context that moved
mid-run once had a suite that deletes volumes talking to a GKE cluster.

On failure the script prints the objects, the last thirty events and the
operator's last hundred log lines before it deletes anything — a cluster torn
down before either is read is a re-run.

## 7. The OpenTelemetry Demo

Twenty instrumented microservices in eight languages, continuous traffic across
all three signals — the only test where neither the client nor the data is ours.

Two files do the whole integration, each dropped into a documented seam that is
empty upstream and loaded last, so nothing in the demo checkout is patched:

| | |
|---|---|
| [`docs/e2e/demo/otelcol-config-extras.yml`](../e2e/demo/otelcol-config-extras.yml) | adds `otlp_grpc/mira` and `otlp_http/mira` to the collector's three pipelines |
| [`docs/e2e/demo/compose.mira.yaml`](../e2e/demo/compose.mira.yaml) | adds Mira as a service on the demo's compose network |

```sh
git clone --depth 1 https://github.com/open-telemetry/opentelemetry-demo /tmp/otel-demo
docker build -t mira .
cp docs/e2e/demo/otelcol-config-extras.yml /tmp/otel-demo/src/otel-collector/
docker compose -f /tmp/otel-demo/compose.yaml -f docs/e2e/demo/compose.mira.yaml up -d
```

The core `compose.yaml` alone is enough — around 8 GB of images and about 6 GB
of RAM. The `compose.full.yaml` and `compose.observability.yaml` layers add
Kafka, Jaeger, Prometheus and OpenSearch, which are the demo's *own* backends.

Give it two minutes and query Mira on `4318`, published to the host:

```sh
curl -s -X POST localhost:4318/api/v1/metrics/names -H 'content-type: application/json' -d '{}'
curl -s localhost:4318/api/v1/query -H 'content-type: application/json' \
  -d '{"signal":"traces","from":"-15m","to":"now","limit":1}'
./target/release/mira mira --addr localhost:4318     # all three tabs, live
```

What came back on this machine, four minutes in: **285 distinct metric names**
(206 sums, 58 gauges, 21 histograms) from eight SDKs plus the collector's
`host_metrics`, `docker_stats`, `nginx`, `redis`, `postgresql` and Prometheus
receivers; **spans from Envoy's C++ SDK, Go, Python, .NET, Java and Node** with
`telemetry.sdk.language`, `service.namespace` and `otel.scope.name` intact; one
`frontend-proxy` trace as **six spans over three services**, root present and no
orphans, `blocks_scanned: 3` of `blocks_total: 102`; and **no export failures**
in `docker logs otel-collector` toward either Mira exporter — the startup noise
there is the demo's own receivers racing their targets.

One thing not to misread: **the newest traces look incomplete.** A parent span
ends after its children, so it is exported after them. Query a trace ninety
seconds old and it is whole.

`profiles` is deliberately not routed to Mira — Mira does not accept that signal
and wiring it up produces a permanent export error every few seconds.

```sh
docker compose -f /tmp/otel-demo/compose.yaml -f docs/e2e/demo/compose.mira.yaml down -v
```

## 8. Cleanup

```sh
kind delete cluster --name mira-operator-e2e
make demo-clean
rm -rf /tmp/mira-dev
```

A Mira data directory is only blocks and sidecars — no metadata store and
nothing registered with anything — so deleting the directory is a complete
uninstall.
