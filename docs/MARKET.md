---
description: Where Mira actually sits against Loki, Tempo, VictoriaLogs, Quickwit, ClickHouse, SigNoz and the SaaS vendors — six axes with every competitor number footnoted, four where Mira is ahead, seven where it is not.
---

# Market position

**For:** anyone comparing Mira against something they already run.

**How to read this.** Every competitor number below is published by its vendor or
a third party, on *their* hardware and *their* workload. Nobody ran Mira's
workload and Mira ran nobody else's. Mira's own figures are single-machine
measurements on one Apple M3 Pro (12 cores, 18 GiB), one process, generator
co-resident — reproduce them with [the load harness](TESTING.md#3-the-load-harness).

The `=` column says whether a claim can honestly sit in the same row as Mira's.
It is `no` for 128 of the 133 claims surveyed, and the four-word reason says why.
The only fair reading of a row marked `no` is order-of-magnitude, and a row
marked `no` for *wrong axis* is not a reading at all.

Tables are per axis rather than one row per competitor, because no competitor
publishes the same axis as its neighbour and a row per engine would be a column
of blanks.

## Who else is in this space

| Class | Who | The structural weakness — one that follows from a commitment they cannot reverse |
|---|---|---|
| **Composed OSS stack** | Grafana LGTM, kube-prometheus-stack | Loki's index is a label index; [its own docs](https://grafana.com/docs/loki/latest/get-started/labels/) concede it "was not designed to support high cardinality label values". The failure mode is an ingester OOM during an incident. Operationally, 20+ pods across three upgrade paths. |
| **OTel-native single binary** | SigNoz, OpenObserve, Uptrace, ClickStack, Coroot, Dash0 | "Single binary" with ClickHouse or DataFusion + S3 inside — a real dependency and a real tuning surface. OpenObserve ships ~450 `ZO_*` environment variables while marketing simplicity. |
| **Log specialists** | VictoriaLogs, Parseable, Quickwit | Genuinely fast and genuinely simple. Single-signal, non-OTLP-native data models, own DSLs. The hardest class to beat and the least worth attacking on tuning-free, which is parity. |
| **Commercial SaaS** | Datadog, Honeycomb, New Relic, Dynatrace, Chronosphere, Grafana Cloud | Per-GB and per-host billing makes the customer's own cost control the product, and the agent surface is metered too. An agent that wants to fire 400 exploratory queries cannot afford to on any of them. |
| **Warehouse-backed** | ClickHouse direct, Databricks, Snowflake + OTel | Powerful and general; you own the schema, the ingestion, the retention and the query language. Mira should not contest the "we already have a data team" segment. |

## Ingest, one node

Mira's row is a **consumed-CPU** measurement and every other row is a
**provisioned-CPU** one. That is the whole difficulty of this table: 1.78 cores
is CPU-seconds the server actually burned over the wall clock of the run, and
nobody else publishes that, so their vCPU column is a purchase order and Mira's
is a meter reading. Both bases are given below rather than picking the flattering
one.

| Engine | Published | Their hardware | = | Reason |
|---|---|---|---|---|
| **Mira** | **1,458,967 rec/s, 191 MiB/s** at 137 B | M3 Pro 12c / 18 GiB, 1.78 busy | — | 4 connections, nothing shed |
| Mira, higher median | 1,565,941 rec/s, 2.18 busy | same box, eight connections | — | 38% spread across three passes |
| Mira, per-core ceiling | 604,166 rec/s, 0.68 cores | same box, one connection | — | 891k rec/s per consumed core |
| Mira, 96 connections | 795,505 rec/s, 1.73 busy | same box, 96 connections | — | nothing shed, ack p99 2.5 s |
| Mira, 1 core [^m1] | 1.10M rec/s, 214 MiB/s | M3 Pro, one core | — | bench, not server |
| GreptimeDB 1.0 [^g1] | 621,367 rows/s OTLP | M4 Max 16c / 48 GB | no | disclaims absolute rate |
| otel-arrow OTAP [^oa1] | 540,309 logs/s at 110 B | 4 SUT cores of 64 | no | transport only, no store |
| Loki 2.9 [^q2] | 73.8k docs/s at 872 B | n2-standard-16, 2.75 vCPU | no | 17% CPU, unsaturated run |
| VictoriaLogs [^v1] | 66 MB/s | 4 vCPU / 8 GiB pod | no | offered load, not ceiling |
| SigNoz stack [^s1] | ~55,000 log lines/s | c6a.4xlarge, whole stack | no | collector plus store |
| Parseable [^p1] | ~133 MiB/s per node | 4 nodes, 12 generators | no | cluster sum, 2% network |
| Quickwit [^q2] | 33,000 docs/s at 872 B | n2-standard-16, 2.2 vCPU | no | inverted index to GCS |
| Quickwit at Binance [^qb] | 6.6 MB/s/vCPU | 700 pods, 2,800 vCPU | no | requested vCPU, unmeasured |
| Elasticsearch 7.x [^e1] | 22,000 docs/s at 1.2 KB | 1 node, 8 vCPU | no | inverted index, HDD |
| Elasticsearch 7.x [^e1] | 220,000 docs/s at 135 B | 3 nodes, 24 vCPU | no | cluster sum, unloaded |
| Jaeger + Scylla [^j1] | ~8,000 spans/s | ~20 backend cores | no | backend only, nodes unstated |
| Elastic APM [^ea] | 127,000 events/s | Elastic Cloud 32 GB | no | undefined events, no storage |

Per core, on the basis the peers publish: 191 MiB/s across the 12 cores the
process was given is **15.9 MiB/s per provisioned core**, against Quickwit's
6.75 MB/s/vCPU on a c5.xlarge [^q1] and Parseable's ~8.3 MiB/s/vCPU [^p1] —
2.5x and 1.9x. Ahead, but on a different record and a different workload, so
read it as an ordering rather than a ratio.

On consumed CPU it is 107.1 MiB/s per core, a 16x gap, and that number should
not be quoted against these rows. Nobody else reports utilisation, so the gap it
measures is partly Mira's and partly the fact that a benchmark rig provisions
headroom it does not use. The reason to record it at all is what it says about
Mira rather than about them: at the operating point the engine leaves ten of
twelve cores idle, so the ingest ceiling on this box is not the engine's
arithmetic.

GreptimeDB is the only other single-process figure on laptop silicon and it is
the one to be held to: 621,367 rows/s on 16 cores and 48 GB against 1,458,967 on
12 and 18. Their record is not this record, so the ordering is real and the
ratio is not.

## Resident set

| Engine | Published | At what rate | = | Reason |
|---|---|---|---|---|
| **Mira** | **244 MiB peak** | 604k rec/s, whole process | — | ps-sampled, includes both UIs |
| GreptimeDB 0.12 [^g2] | 408 MB | 20,000 rows/s, c5d.2xlarge | no | 18x lower offered rate |
| ClickHouse [^v2] | 1.12 GiB | 10,000 spans/s, 4 vCPU | no | cgroup budget, capped rate |
| VictoriaLogs [^v2] | 1.15 GiB | 10,000 spans/s, same box | no | cgroup budget, capped rate |
| Grafana Tempo [^v2] | 4.26 GiB, then OOM | 10,000 spans/s, same box | no | cgroup budget, capped |
| Quickwit indexer [^q3] | 4.9 GB avg, 6.8 peak | c5.xlarge, 1 of 24 | no | heap knob, indexing only |
| SigNoz stack [^s1] | ~6 GB | 55,000 logs/s, c6a.4xlarge | no | whole VM, several processes |
| vlagent forwarder [^vl] | 27.91 MiB | 10,000 logs/s, 1-core cap | no | forwards, stores nothing |

Every peer here is measured inside a memory cgroup, so each number is a budget
partly consumed rather than an intrinsic floor; Mira's is an uncapped laptop
process. The load-bearing column is the operating point, not the megabytes:
Mira's 244 MiB is at 604,166 records/s, the peers' at 10,000–20,000.

One caveat that belongs next to the number rather than in a footnote, because it
is the row's weakness: Mira's RSS is **not flat in connection count**, and 244
MiB is the one-connection row, not the throughput headline. The same process
reaches 862 MiB at the four connections that produce 1,458,967 records/s, 2,040
MiB at eight and 2,244 MiB at ninety-six. RSS counts mapped block pages and more
concurrency keeps more blocks open, so past four connections Mira sits above
ClickHouse's 1.12 GiB rather than below it. Both ends of that range are in the
README's table; quoting only the low end would be quoting the sweep's best case
as its result.

## Artifact — stripped binary

The one axis that is genuinely like-for-like. A byte count of a file has no
hardware, no workload, no record size and no cluster to aggregate. Five of eight
peers are the same architecture and OS as Mira's, and both sides are stripped.
Vendors publish zips, tarballs and container layers; unpacking them moves every
number **up**, so these are the unpacked figures.

| Artifact | Stripped binary | vs Mira | Deps | = |
|---|---|---|---|---|
| **Mira** | **5.62 MiB** (arm64 macOS) | 1.0x | 117 crates | — |
| VictoriaLogs 1.52 [^b1] | 16.26 MiB (amd64 Linux) | 2.9x | 98 Go packages | yes |
| VictoriaLogs + Traces [^b1] | 32.44 MiB, two binaries | 5.8x | — | yes |
| Grafana Tempo 3.0.3 [^b2] | 93.94 MiB (arm64 Linux) | 16.9x | 425 modules | yes |
| otel-arrow OTAP [^b3] | 103.35 MiB (arm64 Linux) | 18.6x | — | yes |
| Grafana Mimir 3.2.1 [^b4] | 104.67 MiB (arm64 macOS) | 18.9x | 331 modules | yes |
| Grafana Loki 3.7.7 [^b5] | 138.34 MiB (arm64 macOS) | 24.9x | 407 modules | yes |
| Quickwit 0.9.0 [^b6] | 144.29 MiB (arm64 macOS) | 26.0x | 1,171 lock entries | yes |
| Parseable 3.2.0 [^b7] | 152.44 MiB (arm64 macOS) | 27.5x | 462 crates | yes |
| ClickHouse 26.3 [^b8] | 153.80 MiB (arm64 macOS) | 27.7x | — | yes |
| ClickStack all-in-one [^b9] | 486.67 MiB image (arm64) | — | 4 processes | no |

The dependency column is directional only: a Go module ships many packages, and
Go's stdlib absorbs HTTP, TLS and compression that Rust pulls in as crates. On
the unit that actually matches a crate — a compiled package — VictoriaLogs is 98
against Mira's 117 external crates, i.e. parity, and it also has exactly one C
dependency (`gozstd`), so that differentiator is a wash against that engine
specifically. The Rust rows are apples to apples: Parseable is 462 crates under
Mira's own `cargo tree --edges normal` invocation.

## Compression

Two denominators are in circulation and mixing them is the most common error on
this axis. Mira publishes both.

| Engine | Ratio | Denominator | = |
|---|---|---|---|
| **Mira, wire** [^m2] | 7.0x (0.14 B/B) | OTLP protobuf on the wire | — |
| **Mira, internal** [^m2] | 8.8x logs, 8.0x traces | uncompressed Arrow columns | — |
| ClickHouse `otel_logs` [^c1] | 14.1x | engine-internal column bytes | no |
| LogHouse fleet [^c2] | ~16x | engine-internal column bytes | no |
| VictoriaLogs [^v3] | 11.2x | engine-internal column bytes | no |
| GreptimeDB [^g2] | 7.7x | raw log text | no |
| ClickHouse vs NDJSON [^oo] | 2.14x | raw input bytes on disk | ~ |
| Quickwit [^q4] | 3.7x | raw input bytes | no |
| Elasticsearch LogsDB [^el] | 1.8x | Elasticsearch standard mode | no |

The 2.14x row is the closest thing to a shared denominator with Mira's 0.14 B/B
— same class of measurement, raw input bytes in, bytes on disk out — but it is a
deliberately untuned ClickHouse schema (`ORDER BY (_timestamp)`, default LZ4) run
by a competitor, so treat it as a floor for ClickHouse, not a result.

## Query

No two engines here measure the same query, and Mira has no full-text term query
at all, so this table is context rather than comparison.

| Engine | Published | Corpus / hardware | = | Reason |
|---|---|---|---|---|
| **Mira** [^m3] | 1.5 ms absent value | 87 blocks, 0 opened | — | pruning, not scanning |
| **Mira** [^m3] | 1.2 ms ordering predicate | 77 blocks, 0 opened | — | zone map, 0 opened |
| **Mira** [^m3] | 13.3 ms trace by id | 28.8M spans, 1 block of 77 | — | bloom sidecar hit |
| **Mira** [^m3] | 175 ms unpruned scan | 24.0M rows, 87 of 87 | — | every block opened |
| VictoriaLogs [^v1] | 266 ms absent line | 300 GB, 4 vCPU | no | bloom scan vs prune |
| VictoriaLogs [^v1] | 2.2 s absent line | 500 GB, 4 vCPU | no | bloom scan vs prune |
| Quickwit [^q2] | 0.6 s term over 212 GB | n2-standard-16, GCS | yes | no Mira counterpart exists |
| ClickHouse [^c3] | 0.68 s hot, 1.54 s cold | 9 queries, 1B rows | no | 40x rows, 2.7x cores |
| Elasticsearch [^c3] | 1.78 s hot, 9.55 s cold | 9 queries, 1B rows | no | 40x rows, 2.7x cores |
| Datadog Husky [^dd] | p50 2.01 ms per fragment | unstated fleet | no | leaf call, mostly cache |
| Honeycomb Retriever [^hc] | p90 2.5 s | Lambda fan-out fleet | no | fleet-wide, all shapes |

## Cost

Mira publishes no dollar figure, only bytes on disk, so there is no Mira row.
This table shows what the axis looks like; it is not one Mira wins.

| Vendor | List price | Basis | = |
|---|---|---|---|
| Quickwit on S3 [^q5] | $8.4 per ingested TB/month | 2023 model, object store | no |
| ClickHouse Cloud [^c4] | $276/mo for 14,336 GiB | 2023 list, self-published | no |
| Elastic Serverless [^es] | $0.07/GB in, $0.017/GB-mo | "as low as", tier floor | no |
| Grafana Cloud [^gc] | $0.55/GB combined | entry rate card | no |
| New Relic [^nr] | $0.40/GB ingested | plus $349/user/mo Pro | no |
| Datadog [^dd2] | $0.10/GB + $1.70/M indexed | queryable fraction unfixed | no |
| Honeycomb [^hc2] | $150 per 50M events | bills events, no byte size | no |

## Where Mira is ahead

**1. Artifact size, and it is not close.** 5.62 MiB stripped: one binary, three
signals, query API, MCP surface and two UIs. The nearest peer is VictoriaLogs at
16.26 MiB — 2.9x — and that binary covers logs only; matching Mira's signal
coverage takes VictoriaLogs plus VictoriaTraces, two processes and 32.44 MiB.
Everything else surveyed is 94–153 MiB, 17x to 28x. This is the strongest claim
in the document precisely because it is a byte count and not a measurement: no
hardware, no workload, no denominator to argue about. Every correction applied
while verifying it — zip to binary, tarball to binary, image layer to executable
— moved the gap wider.

**2. Resident set at the operating point.** 244 MiB peak RSS for the whole
process at 604,166 records/s. The peers that publish RSS do it at 10,000–20,000
records or spans per second: Tempo 4.26 GiB then OOM, VictoriaLogs 1.15 GiB,
ClickHouse 1.12 GiB, Quickwit's indexer 4.9 GB average. GreptimeDB's 408 MB is
at 30x less load. The claim that survives scrutiny is the ratio of footprint to
offered rate, not the absolute figure — at the 1,458,967 records/s four-connection
row the same process is 862 MiB, and by eight connections it is 2,040 MiB, above
ClickHouse. The mechanism behind the ratio
is in the code rather than in a tuning flag: the flusher's refusal to
`concat_batches`.

**3. Pruned-query latency.** 1.5 ms for an attribute value present in none of 87
blocks, 0 blocks opened; 1.2 ms for an ordering predicate nothing satisfies
across 77 blocks; 13.3 ms to fetch every span of one trace out of 28.8M, opening
1 block of 77. The closest published negative-query number is VictoriaLogs at
266 ms over 300 GB. State it as pruning effectiveness rather than scan speed —
VictoriaLogs is bloom-scanning where Mira is skipping the file — and it is
defensible at two orders of magnitude.

**4. Supply-chain surface.** 117 crates on `cargo tree --edges normal` for one
target triple, with `zstd-sys` as the only C dependency. Parseable, measured with
the identical command, is 462 (3.9x); Quickwit is 1,171 lockfile entries against
Mira's 211 (5.6x); the Go stacks carry 331–425 modules. The honest exception is
VictoriaLogs at 98 vendored packages, which is parity and also one C dependency.

## Where Mira is not ahead

Seven rows, and they do not have one answer. Two name a cause inside this
repository and a path to closing it; five are what the design costs, and no
amount of work closes them without giving up the thing that makes the rest of
this page true. Both kinds are listed, each with which kind it is, because a
comparison page on which the author wins everything is a page nobody finishes
reading.

### Gaps with a cause and a path

**Ingest throughput.** Nothing here is a win worth leading with. 1,458,967
records/s beats GreptimeDB's 621,367 on smaller hardware, but their record is
not this record and neither party ran the other's workload, so the ordering
survives and the ratio does not. On the per-provisioned-core basis the peers
publish, 15.9 MiB/s against Quickwit's 6.75 and Parseable's ~8.3 is under 2.5x
on a different record — an ordering, not a result. And the shape of the sweep is
a limitation in its own right: throughput plateaus between four and eight
connections and is 55% of the four-connection rate by 96, so an operator whose
collector fleet opens many connections gets less than this table's headline and
has no knob to tune it back — Mira does not expose a concurrency limit.

The cause is one line of `crates/mira/src/pipeline.rs`: there is one bounded
channel and one flusher task *per signal*, so ninety-six connections sending
logs are ninety-six producers against one consumer, and the curve is that
consumer's service time. What that curve no longer includes is *shedding*: the
same sweep used to return a 503 to 93% of exports at 96 connections, and making
a full queue wait for a slot rather than reject took it to nothing shed and
double the throughput. The remaining gap costs latency instead — ack p99 2.5 s
at 96 connections against 46 ms at four. Closing it means sharding the flusher
within a signal, each shard owning its own block sequence — which the block
directory already tolerates, since it is the manifest and a sequence is just a
filename. It is not a redesign; it is the row this page most expects to move.

**Query at scale.** The unpruned scan is 175 ms steady over 24.0M rows — 137M
rows/s — and 1.6 s the first time, before the page cache is warm. ClickHouse
answers nine heterogeneous queries in 0.68 s hot over 1 billion rows and 642 GiB
uncompressed, roughly 40x the rows on 2.7x the cores. Per row scanned Mira is
about an order of magnitude behind, and the cold figure is another 9x on top of
that. Mira's query numbers are a pruning result, not a scan result, and when
pruning does not fire it does not win.

The path exists and it is long: the scan evaluates predicates row-wise over
Arrow arrays where ClickHouse runs a vectorised engine with SIMD kernels and
twenty years of them. This is a programme, not a patch, and nothing on this page
should be read as a promise that it lands soon. It is listed here rather than
under the design because there is no principle stopping it — only work.

### Gaps that are what the design costs

**Compression on the denominator everyone else publishes.** 0.14 bytes on disk
per byte of OTLP protobuf is the number an operator can predict a bill from, and
it is better grounded than what the competitors print. But on the denominator
they actually print — uncompressed engine-internal columns to compressed — Mira
is 8.8x on logs and 8.0x on traces against ClickHouse's 14.1x and VictoriaLogs'
11.2x. That is a loss, and it is the like-for-like comparison.

The cause is that rows land in arrival order and are compressed in arrival
order, so ZSTD sees interleaved services where ClickHouse's `ORDER BY` has
handed its codec long runs of one value. The obvious answer is to apply the same
trick — sort a block by its low-cardinality columns before the flush,
dictionary-encode the string columns — and this entry sits in *this* section
rather than the previous one because both were measured on real blocks and both
lost. Every sort key tried came out at or below the unsorted ratio, because
arrival order is time order and time already carries the locality; the attribute
values that do repeat are dictionary-encoded already, and encoding the rest
makes logs *worse*. ARCHITECTURE.md's ["What compresses and what does
not"](ARCHITECTURE.md#what-compresses-and-what-does-not) has the numbers. Mira
loses this row to a schema an operator declares up front and Mira, being
OTLP-native, does not get to ask for.

**Full-text search.** No inverted index, so no term query, so no row. Quickwit's
0.6 s for a 3%-selectivity term across 212 GB on one 16-vCPU node — the only
query-latency claim in the survey that clears every comparability test — has no
Mira counterpart at all.

Building one is possible and it is not planned, and the reason is who is asking.
A term query is a human's interface: you do not know the shape of what you are
looking for, so you type a word and read what comes back. An agent does not work
that way. It arrives with a structured hypothesis — this service, this severity,
this attribute, this window — and what it needs is for that filter to prune, not
for a word to rank. Mira is a short-term memory for agents before it is a search
box for people, so an inverted index would be a second file per block, a term
dictionary and a posting-list format spent on the reader this engine is not for.
Losing the row is the correct outcome, not a deferral.

**Horizontal scale.** No cross-replica query fan-out, by design. Quickwit at
Binance sustains 18.5 GB/s across 2,800 vCPU; Datadog and Honeycomb operate
fleets whose size they decline to publish. Mira's answer is independent replicas
behind an L4 balancer and a replication factor of one — a lost disk is lost data
for that node's share. A scope decision, not a benchmark result, but a buyer
reads it as a loss and should hear it here rather than discover it.

This one cannot be closed. Fan-out needs a replica to know which replicas exist
and which of them holds what, and that is membership and a shared catalogue,
which is coordination state — the one thing the stateless principle spends
everything else to avoid. Winning this row means becoming
the thing every other row on this page is winning against. The answer is not a
better implementation, it is a second process in front, and that is the
operator's choice to make rather than Mira's to ship.

**Cost per GB.** No measured dollar figure, only bytes on disk. Quickwit's $8.4
per ingested TB per month is structurally unreachable for any engine on local
block storage — the like-for-like gp3 figure is ~$29.2 — and that is a property
of object storage, not a tuning difference. Matching it means putting blocks in
a bucket, and a block in a bucket cannot be read by `mmap`, which is the read
path. Not a gap: a different product.

**Ack latency with the log off.** p50 657 ms, p99 2.6 s, block-seal-bound.
Nobody else publishes an ack latency so there is no row to lose, but the number
is bad on its own terms and is why the write-ahead log is [the
default](CONFIG.md#ingestwal). It stays on the list because the mode still
exists and someone will run it; with the log on the ack costs a `write(2)` — p50
7.6 ms, p99 46 ms at four connections — and this row is not the shipped one.

## Claims rejected

Not "wrong" — most are true statements. Rejected means they cannot appear as a
number in a comparison table without misleading someone.

**Vendor scale claims with no methodology.** No hardware, no denominator, no
measurement.

- Jaeger, "several billion spans per day" at Uber [^j2]. Cluster-wide,
  post-sampling, unstated span size, no node count. The arithmetic is the danger:
  2–5e9/day is 23k–58k spans/s, *lower* than a laptop, while the phrase reads a
  thousand times higher.
- Datadog Husky, "more than 100 trillion events" per day [^dd3]. Fleet aggregate
  over an unpublished machine count, and the same page uses 100 trillion for the
  *stored corpus* too, so the number is not pinned to one meaning.
- Honeycomb, "on the order of 100,000 events per second" [^hc3]. An incidental
  descriptor of a customer environment on a page whose only measured result is
  query latency.
- Quickwit, "1 PB per day / 14 million docs/s" [^q6]. A user's screenshot the
  vendor is relaying; Quickwit's own reproducible ceiling on the same page is
  400 MB/s.
- Quickwit at Binance, "1.6 PB per day" [^qb]. The denominator is Kubernetes
  *requests*, not observed CPU.
- VictoriaLogs, "absorb around a GiB of logs per second even on slow, low-IOPS
  HDDs" [^v3]. Prose on an internals page, no hardware, no record size.
- Parseable, "up to 90% and average 75%" compression [^p2]. No dataset, no codec,
  no before/after byte counts, and it contradicts the same vendor's "~10:1"
  elsewhere.

**Numbers that are not the axis they are labelled as.**

- VictoriaLogs ClickBench "44,266 rows/s" [^v4]. Derived from a `load_time` whose
  timed region includes a 23.7 GB `wget` and a gunzip to ~75 GB. It is a download
  speed with an engine attached.
- TrueFoundry's "VictoriaLogs 318 GiB vs Loki 501 GiB" filed as compression
  [^v5]. A storage delta between two competitors with no raw baseline; the
  methodology contradicts itself by ~78x and Loki's figure exceeds the stated
  corpus.
- Datadog Husky's "1 GiB to process the column" filed as resident memory [^dd4].
  It is 15,000 x 75 KiB — arithmetic on a field cap, describing the design Husky
  *rejected* in the next sentence. A footnote does not repair that; readers
  compare cells.
- Elastic APM's "100 unsampled transactions/sec" [^ea2] and Elastic's "15 nodes x
  64 GB" [^e2]. Both are inputs to, or outputs of, a provisioning formula. 34 of
  those 64 GB is explicitly OS page cache.
- Honeycomb's "20 s to 0.2 s" [^hc] and Datadog's "a few hundred milliseconds
  increase in the median" [^dd5]. Self-relative deltas with no absolute baseline
  on either side.
- Husky's fragment p50 of 2.01 ms [^dd]. Per 1,000 fragment queries, 300 are
  pruned at the metadata service and 560 by the result cache; 3.4% scan data at
  all. It is a cache lookup, and a user query is the fan-out envelope over
  hundreds of fragments.
- Quickwit Lambda's "less than 100 ms" [^q7]. The authors' own words: the
  function "only checks that the metastore didn't change and serves back the
  earlier results". They call it unfair and exclude it.
- Quickwit's "1 indexer, 236 h, 27 MB/s" [^q1]. 236 h is exactly 23 TB / 27 MB/s;
  the post performs that division itself. No 236-hour run happened.

**Benchmarks against a straw configuration.**

- OpenObserve's ClickHouse at 2.14x [^oo] — admittedly untuned, so kept in the
  table labelled as a floor.
- VictoriaMetrics' log-collector benchmark [^vl]: vlagent's vendor benchmarking
  vlagent, every competitor at Helm-chart defaults, two collectors dropped from
  the tables for losing logs.
- ClickHouse vs Elasticsearch at 4.95x storage [^c3]: the page itself notes a
  tuned ES config was ~20% smaller, and ES OSS cannot disable `_source`.
- TrueFoundry's Loki [^v1]: reported at "4 vCPUs (100% throttled)" against a
  65 MB/s generator, so the two systems did not store the same bytes.
- Elastic's own 220,000 docs/s [^e1]: pure indexing, zero query load; the same
  post drops to 173,000 under 1,000 ops/s of search.

**Wrong artifact.** Rejected as published, corrected and kept. Loki's release zip
is 40.6 MiB and the binary inside it is 138.34 MiB (`__gopclntab` alone is
53.8 MiB and survives `-s -w`); Tempo's tarball is 58.5 MiB against a 93.94 MiB
binary; VictoriaLogs' is 11.1 MiB against 16.26; Quickwit's is 70.5 MiB against
144.29. Every correction moves the number away from Mira, which is why the
corrected table is the one published. Mimir is *not* corrected upward: its
Makefile passes `-s -w`, so 104.67 MiB is already stripped.

## Where the line is

Each refusal with the sentence a user gets. These are not gaps waiting on a
sprint; each one buys something in the tables above.

| Refused | What the user is told |
|---|---|
| **SQL** | "Read the blocks with pyarrow or polars, and hand the table to DuckDB." Load-bearing, not stylistic: the query surface is a closed set of operations with no parser, planner or optimiser, and that is the only thing bounding the schedule against DataFusion. **The day SQL is promised, DataFusion becomes the correct choice.** |
| **DataFusion** | 47 direct dependencies, ~1.5M SLoC transitive, 68–92 MB binary, against 5.62 MiB. |
| **Replication of your data** | "A lost disk is lost data for that node's share. Export to two replicas from your Collector." A replication factor above one requires a placement decision, and placement *is* coordination state. |
| **Separation of storage and compute** | Needs a scheduler, a metadata service and membership. Scale by adding independent replicas behind an L4 balancer; retention is the rebalancer. |
| **Stored dashboards, saved views, user preferences** | "The link *is* the saved view; curated dashboards are files you commit." A saved dashboard must survive a restart and agree across replicas, which is exactly the coordination state principle 4 refuses — and it would be the first mutable row in the system. |
| **A Grafana datasource plugin** | "Install nothing. Point Grafana's built-in Loki and Tempo datasources at Mira." A signed plugin is a second artifact in a second language with a third-party review and a signing subscription. There is no version of that where "one binary" is literally true. |
| **Ingest-side shaping: drop rules, sampling, transforms** | "Shaping belongs in the Collector, and here is a reference `otelcol` config." Shaping rules are a filter graph and the Collector already is one. The buyer's real question is "who is costing me money", which is cost *attribution*, not a quota. |
| **Loki's cardinality guards** (`max_streams_per_user`, `max_label_names_per_series: 15`) | "Those exist to protect Loki's index. Mira has no such index." Refusing them is a feature claim, not a gap. |
| **Prometheus `remote_read`** | It would make Mira a dumb sample pipe streaming raw points to a Prometheus that evaluates locally — the worst possible shape for a columnar store, and it forfeits every pushdown the engine exists to do. |
| **Prometheus `remote_write` receiver** | "Run the Collector's `prometheusreceiver` and export OTLP." Flat label sets carry no Resource, no Scope and no semconv; every synthesised series lands in the no-identity bucket. Keep the lossy hop outside Mira. |
| **Tiering knobs** (`offloadPeriod`, cache path, cache size, eviction policy) | Exactly one new flag will ever exist: `--offload <uri>`, because a URI is an address. Every competitor's tiering config is a documented foot-gun. |
| **Per-record deletion (GDPR erasure)** | This one hurts: it breaks block immutability, which is what reader safety rests on. The answer is tenant-as-path-prefix so deletion stays an unlink of whole directories, plus short retention. Regulated buyers should be told no explicitly rather than find out at audit. |
| **SAML** | "Put an OIDC bridge in front of Mira." SAML needs a certificate and a metadata store; OIDC needs a JWKS fetch. |
| **A Kubernetes operator / CRDs** | A controller reconciling a stateless single binary is a second process managing a thing with no state to reconcile. |
| **Iceberg / a catalog** | Catalog, manifests and snapshots are coordination state and a second product. Parquet *export* is revisited when someone names Athena or Trino with a workload attached; it costs ~20 crates. |
| **Profiles as a fourth signal** | Deferred, not refused, and the gate is external: the signal is Alpha and the proto is still removing fields. No placeholder table in the schema. |

The reasoning behind each of these is in
[the architecture document](ARCHITECTURE.md) — this page records the decision, not the
argument.

[^m1]: `cargo bench -p mira-core --bench encode_bench`; [Architecture section 11](ARCHITECTURE.md#11-performance-model).
[^m2]: `cargo run --release -p mira-core --example tier` over all 948 tables of a 7.35 GiB corpus; [Architecture section 11](ARCHITECTURE.md#11-performance-model).
[^m3]: [Architecture section 11](ARCHITECTURE.md#11-performance-model), the query rows; steady state, server-reported `elapsed_us`.
[^g1]: <https://greptime.com/blogs/2026-03-24-ingestion-protocol-benchmark>
[^g2]: <https://greptime.com/blogs/2025-03-10-log-benchmark-greptimedb>
[^oa1]: <https://github.com/open-telemetry/otel-arrow>
[^v1]: <https://www.truefoundry.com/blog/victorialogs-vs-loki>
[^v2]: <https://victoriametrics.com/blog/dev-note-distributed-tracing-with-victorialogs/>
[^v3]: <https://victoriametrics.com/blog/victorialogs-internals-columnar-storage-on-disk/>
[^v4]: <https://github.com/ClickHouse/ClickBench/blob/main/victorialogs/results/20260511/c6a.4xlarge.json>
[^v5]: <https://www.truefoundry.com/blog/victorialogs-vs-loki>
[^vl]: <https://victoriametrics.com/blog/log-collectors-benchmark-2026/>
[^s1]: <https://signoz.io/blog/logs-performance-benchmark/>
[^p1]: <https://www.parseable.com/blog/the-economics-and-physics-of-100-tb-telemetry-data-per-day>
[^p2]: <https://www.parseable.com/blog/performance-is-table-stakes>
[^q1]: <https://quickwit.io/blog/benchmarking-quickwit-engine-on-an-adversarial-dataset>
[^q2]: <https://quickwit.io/blog/benchmarking-quickwit-loki>
[^q3]: <https://quickwit.io/blog/benchmarking-quickwit-engine-on-an-adversarial-dataset>
[^q4]: <https://quickwit.io/blog/benchmarking-quickwit-loki>
[^q5]: <https://quickwit.io/blog/benchmarking-quickwit-engine-on-an-adversarial-dataset>
[^q6]: <https://quickwit.io/blog/quickwit-0.8>
[^q7]: <https://quickwit.io/blog/quickwit-lambda-search-performance>
[^qb]: <https://quickwit.io/blog/quickwit-binance-story>
[^e1]: <https://www.elastic.co/blog/benchmarking-and-sizing-your-elasticsearch-cluster-for-logs-and-metrics>
[^e2]: <https://www.elastic.co/blog/benchmarking-and-sizing-your-elasticsearch-cluster-for-logs-and-metrics>
[^ea]: <https://www.elastic.co/docs/troubleshoot/observability/apm/processing-performance>
[^ea2]: <https://www.elastic.co/guide/en/apm/server/7.15/sizing-guide.html>
[^el]: <https://www.elastic.co/observability-labs/blog/elasticsearch-logsdb-index-mode-storage-savings>
[^es]: <https://www.elastic.co/pricing/serverless-observability>
[^c1]: <https://clickhouse.com/blog/storing-log-data-in-clickhouse-fluent-bit-vector-open-telemetry>
[^c2]: <https://clickhouse.com/blog/a-quadrillion-rows-across-the-three-cloud-scaling-loghouse>
[^c3]: <https://clickhouse.com/blog/elasticsearch-log-analytics-clickhouse>
[^c4]: <https://clickhouse.com/blog/clickhouse-cloud-vs-elastic-datadog-observability-costs>
[^oo]: <https://openobserve.ai/blog/openobserve-vs-clickhouse-one-billion-logs-benchmark/>
[^j1]: <https://grafana.com/blog/2020/07/30/how-to-maximize-span-ingestion-while-limiting-writes-per-second-to-a-scylla-backend-with-jaeger-tracing/>
[^j2]: <https://www.jaegertracing.io/docs/1.76/features/>
[^hc]: <https://www.honeycomb.io/blog/virtualizing-storage-engine>
[^hc2]: <https://www.honeycomb.io/pricing>
[^hc3]: <https://www.honeycomb.io/blog/virtualizing-storage-engine>
[^dd]: <https://www.datadoghq.com/blog/engineering/husky-query-architecture/>
[^dd2]: <https://www.datadoghq.com/pricing/>
[^dd3]: <https://www.datadoghq.com/blog/engineering/husky-query-architecture/>
[^dd4]: <https://www.datadoghq.com/blog/engineering/husky-storage-compaction/>
[^dd5]: <https://www.datadoghq.com/blog/engineering/introducing-husky/>
[^nr]: <https://newrelic.com/pricing>
[^gc]: <https://grafana.com/pricing/>
[^b1]: <https://github.com/VictoriaMetrics/VictoriaLogs/releases/tag/v1.52.0>
[^b2]: <https://github.com/grafana/tempo/releases/tag/v3.0.3>
[^b3]: <https://github.com/open-telemetry/otel-arrow/actions/runs/34422416665>
[^b4]: <https://github.com/grafana/mimir/releases/tag/mimir-3.2.1>
[^b5]: <https://github.com/grafana/loki/releases/tag/v3.7.7>
[^b6]: <https://github.com/quickwit-oss/quickwit/releases/tag/v0.9.0>
[^b7]: <https://github.com/parseablehq/parseable/releases/tag/v3.2.0>
[^b8]: <https://github.com/ClickHouse/ClickHouse/releases/tag/v26.3.33.24-lts>
[^b9]: <https://clickhouse.com/docs/clickstack/deployment/all-in-one>
