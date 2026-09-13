---
description: What every published Mira number actually measures — the numerator, the denominator, what is inside the measurement and what is outside it — and the five ways a competitor's number is not the same quantity.
---

# The measurement contract

**For:** anyone about to divide one of Mira's numbers by somebody else's, and
anyone about to publish a new one.

A throughput figure is not a number. It is a number, a denominator, a stopping
condition, a machine, and a list of things that were not counted — and two
engines quoting "records per second" are usually measuring four different
quantities. This page fixes Mira's half of that so a reader can do the
comparison themselves rather than take ours.

Three claims it is built on, in order of how much they matter:

1. **Every published figure is in one file.** `measurements.kyaml` at the root
   of the repository. The number lives there once; every document that quotes it
   is listed beside it; and `make measurements-check` fails the build the day a
   document and the registry disagree. The table at the bottom of this page is
   generated from it.
2. **Every figure names the command that produces it again** — or says
   `unscripted`, which is a gap admitted rather than a gap hidden.
3. **Nothing here is a vendor benchmark.** One laptop, one process, the
   generator on the same twelve cores as the thing it measures. That is a
   weakness and it is priced below rather than apologised for.

## 1. What a run is

The harness is `crates/mira/examples/loadgen.rs`, and
[end-to-end testing section 3](e2e.md#3-the-load-harness) is how to drive it. It
reports all four axes from one run on purpose: an engine measured one axis at a
time is an engine that is fast at whichever one its authors were watching.

```sh
./target/release/examples/loadgen \
  --records 54172384 --conns 4 --batch 8192 \
  --pid $(pgrep -n mira) --data-dir /tmp/mira-dev --emit run.json
```

**A record is one log record, one span, or one metric data point.** Not one
export, not one row of a batch of eight thousand, not one byte. `--batch 8192`
puts 8,192 log records and 8,192 spans in each export and twelve data points
beside them, so a "records/s" figure is `logs + spans + points` over wall clock.
Every peer figure in [market position](../market.md) is normalised to the same
unit where its publisher gave enough to do so, and marked `no` where they did
not.

**A record is acknowledged or it is not counted.** Mira answers an export only
once the block or the log holding it is durable, so the count behind every rate
here is a count of 200s. Exports the engine shed with a 503 are retried by the
harness, counted separately, and printed: a headline throughput number that
quietly dropped a third of its offered load is the commonest way an ingest
benchmark lies. Every row of the published sweep shed nothing, and a run of your
own that sheds is measuring a machine that cannot keep up rather than this curve.

**The stopping condition is a record count, not a clock.** `--records N` divides
the rounds evenly across the connections before the first byte is sent, so the
corpus a run leaves behind is the same corpus on a fast box and a slow one, and a
second person can rebuild it byte for byte. The number above is not round for a
reason: a round is `2 x 8192 + 12` records, four connections doing 826 rounds
each is `4 x 826 x 16,396 = 54,172,384`, and that lands on exactly the
27,066,368 logs, 27,066,368 spans and 39,648 points every query row below
divides by. `--for <duration>` still exists and is still the right flag for a
soak, but a number published off a `--for` run is a number whose *corpus* cannot
be reproduced — only its method. **The figures in the table below predate
`--records`**: they were taken off `--for` runs, the arithmetic above is how the
flag was sized to land on the corpus they describe, and the first sweep to use it
is the one that replaces them.

**Every field the generator writes derives from a counter.** No random source,
no clock in the payload except the timestamps. The same arguments produce the
same bytes.

## 2. The four axes

[Architecture section 11](../architecture.md#11-performance-model) scores four
axes together, and each is a different kind of quantity. What follows is the
definition; the values are in the table at the bottom.

### Ingest throughput

**Numerator.** Records acknowledged, as defined above.

**Denominator — and this is the whole difficulty of the market table.** Two are
published and they are not interchangeable:

| | What it is | When to use it |
|---|---|---|
| wall clock | records ÷ seconds of the run | comparing two Mira runs |
| **consumed CPU** | records ÷ (CPU-seconds the *server* burned ÷ seconds of the run) | comparing against anything else |

The consumed-CPU denominator is a meter reading. The harness takes
`ps -o cputime=` on the server pid either side of the run and differences it, so
`1.75 cores` is CPU the process actually spent, not a core count somebody
provisioned. Nearly every competitor figure divides by a purchase order instead —
an `n2-standard-16`, a 4-vCPU pod — and the two answer different questions. A
provisioned denominator flatters an engine that leaves headroom idle and
punishes one that does not; a consumed denominator flatters an engine that is
never saturated. Mira is in the second category at the operating point it
publishes, which is why the per-core figure is quoted as a **ceiling** rather
than as an operating point.

**Inside the measurement:** protobuf decode, OTLP validation, the attribute
flattening, the Arrow build, the log append, the block seal and its fsync.

**Outside it:** the generator's own CPU, which is on the same twelve cores and
is encoding 8,192 protobuf records per batch inside its own loop. Part of the
per-batch cost is therefore the harness's, and separating them wants the
generator on a second machine — the one thing a single-laptop harness cannot do.
**Treat every aggregate rate here as a floor.**

**Why the aggregate rate is the weaker number.** It is a property of the offered
load as much as of the engine: raise `--conns` and it moves without a line of the
server changing. The shape published for a paired comparison is four
connections, because it is the shape that reproduces — 10% spread across three
passes, against 39% at ninety-six.

### Resident footprint

**What.** Peak RSS of the server process, sampled from outside it with `ps -o
rss=` at 4 Hz, plus peak anonymous memory at 0.5 Hz.

**From outside, and both numbers, for one reason each.** From outside because
Mira reads through `mmap`: most of what it costs a machine is page cache the
process never allocated, and a heap counter would report a figure that is
flattering and wrong. Both numbers because RSS counts every mapped block page a
query touched, so on a read-heavy run RSS measures the corpus rather than the
engine — the anonymous figure is what the process actually allocated, and that
one is the open blocks, the in-flight decodes and the staging copies.

**Inside it:** the whole process. Both UIs, the query path, the MCP surface, the
mapped blocks.

**Outside it:** nothing. There is no cgroup and no heap cap. Every peer figure it
is quoted against *is* inside a memory limit, which makes each of those a budget
partly consumed rather than an intrinsic floor.

**It is not flat in connection count.** The headline footprint is the
one-connection row and is labelled with its shape everywhere it appears. Quoting
it beside the ninety-six-connection throughput would be two rows of the same
table pretending to be one measurement.

### Query latency

**What.** The server's own `elapsed_us` from the response envelope — time inside
the query handler — not a client round trip. Loopback and HTTP framing are
excluded deliberately: they are the same for every query and would bury the
differences the table is about.

**Two columns, and they are different questions.** "First" is the first call
into a freshly started process. "Steady" is the same call repeated. The gap
between them is virtual-memory work: establishing 316 blocks' worth of mappings
and faulting them in. For every row that prunes, nearly all of the steady figure
is that too.

**What is published beside every latency, and why it is not decoration:**

- `blocks_total` and `blocks_scanned` — a query that pruned everything and a
  query that scanned everything are not comparable, and a fast number with
  `blocks_scanned: 0` is a measurement of the sidecar, not of the scan.
- `rows_scanned` and `rows_matched` — a query mix that matches nothing measures
  the empty path and reports beautiful numbers.

**Warm, except where it says otherwise.** 8.33 GiB nearly fits this machine's 18
GiB of page cache, so the pruning rows are warm. The unprunable scan is not: 18
GiB shared with a VM, Docker and a browser does not keep 8 GiB of corpus
resident, and the same scan over a corpus that does stay resident is three times
cheaper per row. Neither column is a cold-disk number and neither claims to be.

### Cost per GB

**What.** Bytes the data directory grew during the run, divided by uncompressed
OTLP protobuf bytes the harness put on the wire. A ratio of two deltas: a total
over a store that already had blocks in it would be meaningless on both sides.

**Two figures, and the second is the one for sizing.** A hot block carries its
sidecars and is not yet compressed, so the ratio is above 1.0 while it is hot.
The cold tier rewrites it ZSTD-compressed once it ages past its partition hour,
and the compacted ratio is what a retention budget should use. The hot figure is
a transient, not a worse measurement of the same thing.

**Outside it:** the write-ahead log, which is bounded and truncated rather than
retained, and anything the offload target holds.

## 3. The box

Apple M3 Pro, 12 cores, 18 GiB, macOS, release build, generator co-resident.

**This machine is not quiet, and the noise is part of the reading.** An
idle-before baseline swung between 6% and 81% busy across consecutive runs, and
two runs of an identical 96-connection configuration minutes apart returned
1,814,829 and 2,229,315 records/s — a 23% spread. That is larger than several of
the effects this project has published, which is why the rules below are rules
and not advice:

- **Medians over passes, never one run.** The published sweep is the median of
  three full passes and prints the individual passes beside it, because on some
  rows they disagree by more than the medians do.
- **Paired, alternating, inside one sitting.** Every A/B in
  `scripts/measure/` runs both arms in each pass, B then A, and reports the
  median of the per-pass deltas rather than the delta of the pooled medians. The
  box drifts across a run; a per-pass delta cancels the drift and a pooled one
  measures it.
- **Check the sign of every pass, not just the median.** A control whose median
  is small but whose per-pass deltas all point one way is a real effect being
  called noise. That distinction is the only thing separating the two treatment
  rows of the lazy-open A/B from its two controls.
- **Fingerprint the corpus.** A corpus is not a constant while a server is
  running on it: the cold tier compacts blocks from inside the server the harness
  keeps restarting, so a long A/B over fresh blocks starts plain and finishes
  compacted. `lazy-detail.sh` refuses to continue if the table count or byte
  total moves.
- **Look at what else is running.** An earlier pass of the published sweep, taken
  with a 294%-CPU virtual machine and a `go build` on the box, read 14% under the
  median on the same binary and the same command. Nothing in the output says so.

A figure taken on a noisy day is left as it was measured rather than restated
from a quieter one, and where a re-measurement disagrees the disagreement is
written up. Two numbers this project published have been **withdrawn** for
failing the rules above; both withdrawals are still in
[architecture section 11](../architecture.md#11-performance-model), because a
retracted measurement is more useful to a reader than a quiet edit.

## 4. Reading somebody else's number against one of these

[Market position](../market.md) footnotes every competitor figure and marks each
one `yes`, `no` or `~` for whether it can honestly sit in the same row. These are
the five reasons a row gets `no`, and they are worth knowing whoever you are
comparing:

1. **Provisioned CPU against consumed CPU.** The commonest one. Their vCPU column
   is a purchase order; Mira's core count is a meter reading. A run that is
   explicitly unsaturated — 17% CPU — has a denominator measuring an offered load
   rather than a ceiling.
2. **Offered load against a ceiling.** "X records/s at 4 vCPU" usually means the
   generator was configured to send X and the engine kept up. It is an upper
   bound on nothing.
3. **Cluster sums.** Four nodes and twelve generators produce a number that is
   not a per-node figure and does not divide into one, because the network and
   the coordination are in it.
4. **A different record.** 137 bytes against 872 against 1.2 KiB. Record size
   moves a records/s figure by an order of magnitude and moves a MiB/s figure the
   other way. Compare one or the other, having checked both.
5. **A different amount of engine.** Transport-only, collector-plus-store,
   indexing-only, whole-VM. Several published figures include no storage at all
   and several include three processes.

The only fair reading of a row marked `no` is order-of-magnitude. A row marked
`no` for the wrong axis is not a reading at all.

## 5. Re-measuring

```sh
make build
scripts/measure/conn-sweep.sh                     # hours; writes /tmp/mira-sweep/run.json
make measurements-ingest RUN=/tmp/mira-sweep/run.json
```

`conn-sweep.sh` runs four connection counts three times each and appends one
JSON object per pass, so the file that comes out is the whole sweep rather than
its last row. `measurements-ingest` takes the **median per key across those
passes** — the rule in section 3, applied by the tool rather than by hand. An
even number of passes takes the upper of the two middles instead of averaging
them, because the average of two passes is a rate no pass measured.

It then compares each median against the registry, prints the keys that moved
with the file and line of every site still quoting the old figure, and exits
non-zero. `WRITE=1` updates `measurements.kyaml` and rewrites the table below.
Keys the sweep measures but no document publishes — the per-core rate at four
connections, say — are listed and skipped; a key whose *stem* is unknown is an
error, because that is what a renamed emitter looks like.

It deliberately does **not** rewrite the prose at those sites. The sentence
around a number is almost always a claim about it — "worth between a quarter and
two fifths", "a 26% fall from thirty-two to ninety-six" — and a tool that swapped
the digits and left the claim would produce a document that passes every check
here and is wrong. Naming the sites is the part a machine can do correctly.

**What is not scripted yet.** The rows marked "no script" below are reproducible
in method and not in corpus: the section 11 query table was read back by hand
from a restarted process, and the log-off acknowledgement pair came from a
configuration the sweep does not run. Both are fixable and neither is fixed. The
honest consequence is that re-measuring them is a morning's work rather than one
command, and that is exactly the reason a figure drifts.

## The registry

Every number, what it measures, and the command that produces it again.
Generated from `measurements.kyaml` — edit that file, not this table.

<!-- BEGIN GENERATED: measurement-registry -->
| Quantity | Measured | What the number is | Reproduce |
|---|---|---|---|
| `ingest.records_per_s.conns1` | **629384** records/s | Records the server acknowledged, divided by the harness's wall clock. A record is one log record, one span or one metric data point — not one export and not one byte. Acknowledged means the 200 came back, which under ack-after-durability means the bytes are in the log. Shed exports are not counted and the run asserts there were none. | `scripts/measure/conn-sweep.sh`, quoted in 3 places |
| `ingest.records_per_s.conns4` | **1350502** records/s | As ingest.records_per_s.conns1, at four writer connections — the shape quoted for a paired comparison because it is the one that reproduces, 10% across three passes against 39% at ninety-six. | `scripts/measure/conn-sweep.sh`, quoted in 3 places |
| `ingest.records_per_s.conns32` | **1537875** records/s | As ingest.records_per_s.conns1, at thirty-two. The peak of a plateau rather than a peak: sixteen and thirty-two are within 2% of each other on medians whose passes span 6% and 16%. | `scripts/measure/conn-sweep.sh`, quoted in 4 places |
| `ingest.records_per_s.conns96` | **1136941** records/s | As ingest.records_per_s.conns1, at ninety-six — past the plateau, where added connections buy waiters on one log mutex rather than appends. | `scripts/measure/conn-sweep.sh`, quoted in 4 places |
| `ingest.cores.conns1` | **0.71** cores | CPU-seconds the *server* process burned during the run, divided by wall clock. A meter reading, not a core count somebody chose: the harness takes `ps -o cputime=` either side of the run and differences it. The generator's own CPU is not in it, and the generator is on the same twelve cores. | `scripts/measure/conn-sweep.sh`, quoted in 3 places |
| `ingest.cores.conns4` | **1.75** cores | As ingest.cores.conns1, at four connections. | `scripts/measure/conn-sweep.sh`, quoted in 3 places |
| `ingest.cores.conns32` | **2.23** cores | As ingest.cores.conns1, at thirty-two — and, to two decimals, the same at ninety-six, which is the observation the plateau diagnosis rests on: ten of twelve cores idle at both ends of a 26% throughput fall. | `scripts/measure/conn-sweep.sh`, quoted in 4 places |
| `ingest.per_core.conns1` | **886147** records/s/core | ingest.records_per_s.conns1 divided by ingest.cores.conns1. The comparable figure: an aggregate rate is a property of the offered load and moves with `--conns` without a line of the server changing. Highest at one connection, where there is nothing to contend over, so it is a ceiling and not an operating point. | `scripts/measure/conn-sweep.sh`, quoted in 5 places |
| `ingest.per_core.conns96` | **515288** records/s/core | As ingest.per_core.conns1, at ninety-six connections. | `scripts/measure/conn-sweep.sh`, quoted in 1 place |
| `ingest.wire_mib_s.conns4` | **176.5** MiB/s | Uncompressed OTLP protobuf written to the socket, including request headers, divided by wall clock. Not bytes on disk and not a compressed figure — the comparison peers publish a wire rate, so this is the one that can sit beside theirs. At four connections Mira's record averages 137 B. | `scripts/measure/conn-sweep.sh`, quoted in 2 places |
| `ingest.ack_p50_ms.conns4` | **8.5** ms | Milliseconds from the first byte of an export written to the socket to the 200 read back, p50 over every export of the run. With `ingest.wal` on — the default — that is a `write(2)` into the page cache and not a block seal. Client-side, so it includes the loopback round trip. | `scripts/measure/conn-sweep.sh`, quoted in 3 places |
| `ingest.ack_p99_ms.conns4` | **55** ms | As ingest.ack_p50_ms.conns4, at p99. | `scripts/measure/conn-sweep.sh`, quoted in 3 places |
| `ingest.ack_p99_ms.conns96` | **2661** ms | As ingest.ack_p99_ms.conns4, at ninety-six — eighteen times the four- connection figure for 14% more throughput, which is what the plateau costs. | `scripts/measure/conn-sweep.sh`, quoted in 3 places |
| `ingest.ack_p50_ms.logoff` | **657** ms | As ingest.ack_p50_ms.conns4 but with `ingest.wal` off, where the acknowledgement waits for the block seal rather than a page-cache write. A different quantity from the row above, not a worse measurement of the same one. | **no script** — reproducible in method only, quoted in 3 places |
| `ingest.ack_p99_ms.logoff` | **2647** ms | As ingest.ack_p50_ms.logoff, at p99. | **no script** — reproducible in method only, quoted in 1 place |
| `rss_mib.conns1` | **232** MiB | Peak resident set of the server process over the run, `ps -o rss=` at 4 Hz. Whole process: both UIs, the query path and every mapped block page a query touched. Uncapped — no memory cgroup, unlike every peer figure it is quoted against. | `scripts/measure/conn-sweep.sh`, quoted in 3 places |
| `rss_mib.conns4` | **689** MiB | As rss_mib.conns1, at four connections. | `scripts/measure/conn-sweep.sh`, quoted in 4 places |
| `rss_mib.conns96` | **1648** MiB | As rss_mib.conns1, at ninety-six. RSS is not flat in connection count and this row is why the headline footprint figure is labelled with its shape. | `scripts/measure/conn-sweep.sh`, quoted in 3 places |
| `query.attr_absent.steady_ms` | **2.6** ms | The server's own `elapsed_us`, not a client round trip: time inside the query handler for an attribute equality that matches nothing anywhere in the store. Zero of 137 blocks opened — the Bloom sidecar answers it — so this prices the pruning path and nothing else. | **no script** — reproducible in method only, quoted in 2 places |
| `query.attr_absent.first_ms` | **8.3** ms | As query.attr_absent.steady_ms, on the first call after a process restart. The gap between first and steady is virtual-memory work — establishing 316 blocks' worth of mappings and faulting them in. | **no script** — reproducible in method only, quoted in 1 place |
| `query.attr_match.first_ms` | **4.1** ms | As query.attr_absent.first_ms, for an attribute equality that does match: 1 of 137 blocks opened, 204,800 rows scanned. | **no script** — reproducible in method only, quoted in 1 place |
| `query.unfiltered_100.first_ms` | **29.1** ms | As query.attr_absent.first_ms, for `limit 100` with no predicate. One block, 204,800 rows — the cost is the block open, not the limit. | **no script** — reproducible in method only, quoted in 1 place |
| `query.trace_by_id.steady_ms` | **4.7** ms | As query.attr_absent.steady_ms, for every span of one trace id out of 27.1M spans on disk. A trace id carries no time bound, so every block is a candidate and only `trace.idx` prunes: 2 of 155 blocks opened, 73,728 rows. | **no script** — reproducible in method only, quoted in 3 places |
| `query.metric_names.first_ms` | **8.9** ms | As query.attr_absent.first_ms, for the metric-name catalogue: 24 of 24 blocks, because every metric block can hold a name no other one does. | **no script** — reproducible in method only, quoted in 1 place |
| `query.substring_scan.steady_ms` | **885** ms | As query.attr_absent.steady_ms, for a substring with no time bound over 27,066,368 rows and 137 of 137 blocks — the unprunable case, and the only row of the table that is not warm: 8.33 GiB does not stay resident in 18 GiB shared with a VM, Docker and a browser. | **no script** — reproducible in method only, quoted in 3 places |
| `query.substring_scan.first_ms` | **1441** ms | As query.substring_scan.steady_ms, on the first call after a restart. | **no script** — reproducible in method only, quoted in 2 places |
| `cost.hot_bytes_per_byte` | **1.20** B/B | Bytes the data directory grew during the run, divided by uncompressed OTLP protobuf bytes the harness sent. Above 1.0 because a hot block carries sidecars and is not yet compressed. A delta on both sides: a total over a store that already had blocks in it would be meaningless. | `scripts/measure/conn-sweep.sh`, quoted in 1 place |
| `cost.compacted_bytes_per_byte` | **0.14** B/B | As cost.hot_bytes_per_byte, once the cold tier has rewritten the block ZSTD-compressed. The figure to quote for retention sizing; the hot one is the transient. | `scripts/measure/offload-cycle.sh`, quoted in 4 places |
| `wal.append_p50_us` | **7** us | p50 of one durable log append for a 4 KiB body, measured inside the process. It is a buffered `write(2)` plus the fixed per-append work, not an fsync — the fsync is the sweep's, four times a second, and is fsync.full_us below. | **no script** — reproducible in method only, quoted in 3 places |
| `fsync.full_us` | **4230** us | `File::sync_all()` on this volume, which on macOS is `F_FULLFSYNC` — a real platter-or-flash barrier, not a page-cache flush. Quoted as the cost the log sweep pays 4 Hz, a ~2% duty cycle on a 250 ms period. | **no script** — reproducible in method only, quoted in 1 place |
| `corpus.records.logs` | **27066368** records | Log records in the section 11 corpus, and separately the span count — the generator emits one batch of each per round, so the two are equal by construction and a difference between them is a bug in the run. | `scripts/measure/conn-sweep.sh`, quoted in 7 places |
| `corpus.records.points` | **39648** points | Metric data points in the section 11 corpus. Three orders of magnitude below the other two signals, which is why a metrics block holds ~1,600 points and the metrics join's cost is invisible in a mixed run. | `scripts/measure/conn-sweep.sh`, quoted in 1 place |
| `corpus.gib` | **8.33** GiB | Arrow IPC bytes on disk for the section 11 corpus, sidecars included. | `scripts/measure/conn-sweep.sh`, quoted in 2 places |
| `corpus.tables` | **1652** tables | Arrow tables across the corpus — the file count `find -name '*.arrow'` returns. Not the block count: a block is a directory of tables, and how many depends on how many attribute levels the signal has. | `scripts/measure/conn-sweep.sh`, quoted in 2 places |
| `corpus.blocks.logs` | **137** blocks | Log blocks in the section 11 corpus, and the denominator of every "N of 137 blocks" in the query table. Six flushers per signal seal ~204,800 rows each; one flusher sealed 87 blocks of ~330,000 for the same bytes. | `scripts/measure/conn-sweep.sh`, quoted in 5 places |

<!-- END GENERATED: measurement-registry -->
