---
description: How Mira compares with Loki, Tempo, VictoriaLogs, Quickwit, ClickHouse, SigNoz and the paid services — four things it does better, eight it does not.
---

# Market position

**For:** anyone comparing Mira against something they already run.

Every competitor figure here was published by its vendor or by a third party, on
their hardware and their workload. Mira's own were measured on one laptop — an
Apple M3 Pro, 12 cores, 18 GiB — and you can re-run them with
[the load harness](internals/e2e.md#3-the-load-harness);
[the measurement contract](internals/measurement.md) says what each one counts.

The `=` column says whether a figure can honestly sit beside Mira's.
<!-- BEGIN GENERATED: market-claim-tally -->
Across the six tables that carry one there are 62 marked rows. 13 are Mira's own
and take `—`; of the 49 competitor claims, **38 are `no`**, 10 are `yes` and one
is `~`.
<!-- END GENERATED: market-claim-tally -->
A row marked `no` tells you which number is larger. It does not give you a ratio.

## Who else is in this space

| Class | Who | Where they are stuck — a weakness that follows from a choice they cannot undo |
| --- | --- | --- |
| **Composed OSS stack** | Grafana LGTM, kube-prometheus-stack | Loki's index is a label index; [its own docs](https://grafana.com/docs/loki/latest/get-started/labels/) concede it "was not designed to support high cardinality label values". The failure mode is an ingester running out of memory during an incident. Operationally, 20+ pods across three upgrade paths. |
| **OTel-native single binary** | SigNoz, OpenObserve, Uptrace, ClickStack, Coroot, Dash0 | "Single binary" with ClickHouse or DataFusion and S3 inside — a real dependency and a real thing to tune. OpenObserve ships ~450 `ZO_*` environment variables while marketing simplicity. |
| **Log specialists** | VictoriaLogs, Parseable, Quickwit | Genuinely fast and genuinely simple. One signal each, their own data model rather than OpenTelemetry's, their own query language. The hardest class to beat, and no use attacking on "nothing to tune": that one is a draw. |
| **Commercial SaaS** | Datadog, Honeycomb, New Relic, Dynatrace, Chronosphere, Grafana Cloud | Billing per GB and per host makes your own cost control their product, and queries are metered too. An agent that wants to fire 400 exploratory queries cannot afford to on any of them. |
| **Warehouse-backed** | ClickHouse direct, Databricks, Snowflake + OTel | General, and you own the schema, the ingestion, the retention and the query language. Mira should not contest the "we already have a data team" segment. |

## Ingest, one node

Mira's row counts the CPU the process actually burned. Almost every other row
counts the CPU the machine was given, busy or not. Only Loki's 2.75 and
Quickwit's 2.2 [^q2] are measured the way Mira's is, and that run sat at 17% CPU,
so nothing in it was pushed to its limit.

| Engine | Published | Their hardware | = | Reason |
| --- | --- | --- | --- | --- |
| **Mira** | **1,350,502 rec/s, 176.5 MiB/s** at 137 B | M3 Pro 12c / 18 GiB, 1.75 busy | — | 4 connections, nothing shed |
| Mira, sweep peak | 1,537,875 rec/s, 2.23 busy | same box, 32 connections | — | 1.14x the paired row |
| Mira, per-core ceiling | 629,384 rec/s, 0.71 cores | same box, one connection | — | 886k rec/s per consumed core |
| Mira, 96 connections | 1,136,941 rec/s, 2.23 busy | same box, 96 connections | — | nothing shed, ack p99 2.7 s |
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

Counted the way the others count — 176.5 MiB/s spread over all twelve cores —
Mira is 14.7 MiB/s per core, against Quickwit's 6.75 MB/s/vCPU [^q1] and
Parseable's ~8.3 MiB/s/vCPU [^p1]. Per core actually used it is 100.9 MiB/s, and
that figure should not be quoted against these rows: only the [^q2] pair say how
busy their machines were.

## Resident set

| Engine | Published | At what rate | = | Reason |
| --- | --- | --- | --- | --- |
| **Mira** | **232 MiB peak** | 629k rec/s, whole process | — | ps-sampled, includes both UIs |
| GreptimeDB 0.12 [^g2] | 408 MB | 20,000 rows/s, c5d.2xlarge | no | 18x lower offered rate |
| ClickHouse [^v2] | 1.12 GiB | 10,000 spans/s, 4 vCPU | no | memory limit, capped rate |
| VictoriaLogs [^v2] | 1.15 GiB | 10,000 spans/s, same box | no | memory limit, capped rate |
| Grafana Tempo [^v2] | 4.26 GiB, then OOM | 10,000 spans/s, same box | no | memory limit, capped |
| Quickwit indexer [^q3] | 4.9 GB avg, 6.8 peak | c5.xlarge, 1 of 24 | no | heap knob, indexing only |
| SigNoz stack [^s1] | ~6 GB | 55,000 logs/s, c6a.4xlarge | no | whole VM, several processes |
| vlagent forwarder [^vl] | 27.91 MiB | 10,000 logs/s, 1-core cap | no | forwards, stores nothing |

Every peer was measured inside a memory limit, so each figure is a budget partly
spent rather than a floor; Mira's is a laptop process with no limit on it. The
column that matters is the rate. And Mira's own memory grows with connections:
the same process reaches 689 MiB at four and 1,648 MiB at ninety-six, above
ClickHouse's 1.12 GiB.

## Artifact — stripped binary

The one fair comparison on the page: a byte count has no hardware and no workload
behind it. These are unpacked sizes, and every vendor's zip, tarball and image
layer is larger again.

| Artifact | Stripped binary | vs Mira | Deps | = |
| --- | --- | --- | --- | --- |
| **Mira** | **6.20 MiB** (arm64 macOS) | 1.0x | 149 crates | — |
| VictoriaLogs 1.52 [^b1] | 16.26 MiB (amd64 Linux) | 2.6x | 98 Go packages | yes |
| VictoriaLogs + Traces [^b1] | 32.44 MiB, two binaries | 5.2x | — | yes |
| Grafana Tempo 3.0.3 [^b2] | 93.94 MiB (arm64 Linux) | 15.2x | 425 modules | yes |
| otel-arrow OTAP [^b3] | 103.35 MiB (arm64 Linux) | 16.7x | — | yes |
| Grafana Mimir 3.2.1 [^b4] | 104.67 MiB (arm64 macOS) | 16.9x | 331 modules | yes |
| Grafana Loki 3.7.7 [^b5] | 138.34 MiB (arm64 macOS) | 22.3x | 407 modules | yes |
| Quickwit 0.9.0 [^b6] | 144.29 MiB (arm64 macOS) | 23.3x | 1,171 lock entries | yes |
| Parseable 3.2.0 [^b7] | 152.44 MiB (arm64 macOS) | 24.6x | 462 crates | yes |
| ClickHouse 26.3 [^b8] | 153.80 MiB (arm64 macOS) | 24.8x | — | yes |
| ClickStack all-in-one [^b9] | 486.67 MiB image (arm64) | — | 4 processes | no |

Read the dependency column as a direction, not a score: VictoriaLogs' 98 against
Mira's 149 is the same order, and it has one C dependency (`gozstd`) as Mira does.

## Compression

Everyone divides by something different, and mixing the two is the usual mistake.

| Engine | Ratio | Denominator | = |
| --- | --- | --- | --- |
| **Mira, wire** [^m2] | 7.0x (0.14 B/B) | OTLP protobuf on the wire | — |
| **Mira, internal** [^m2] | 8.8x logs, 7.9x traces | uncompressed Arrow columns | — |
| ClickHouse `otel_logs` [^c1] | 14.1x | engine-internal column bytes | no |
| LogHouse fleet [^c2] | ~16x | engine-internal column bytes | no |
| VictoriaLogs [^v3] | 11.2x | engine-internal column bytes | no |
| GreptimeDB [^g2] | 7.7x | raw log text | no |
| ClickHouse vs NDJSON [^oo] | 2.14x | raw input bytes on disk | ~ |
| Quickwit [^q4] | 3.7x | raw input bytes | no |
| Elasticsearch LogsDB [^el] | 1.8x | Elasticsearch standard mode | no |

The 2.14x row divides by the same thing Mira's 0.14 B/B does, but it is a
deliberately untuned ClickHouse schema run by a competitor: a floor, not a result.

## Query

No two engines here measure the same query, so this is context, not comparison.

| Engine | Published | Corpus / hardware | = | Reason |
| --- | --- | --- | --- | --- |
| **Mira** [^m3] | 2.56 ms absent value | 137 blocks, 0 opened | — | pruning, not scanning |
| **Mira** [^m3] | 4.49 ms matching value | 27.1M rows, 1 block of 137 | — | pruned to one block |
| **Mira** [^m3] | 4.74 ms trace by id | 27.1M spans, 2 blocks of 155 | — | bloom sidecar hit |
| **Mira** [^m3] | 885 ms unpruned scan | 27.07M rows, 137 of 137 | — | every block opened, corpus over page cache |
| VictoriaLogs [^v1] | 266 ms absent line | 300 GB, 4 vCPU | no | bloom scan vs prune |
| VictoriaLogs [^v1] | 2.2 s absent line | 500 GB, 4 vCPU | no | bloom scan vs prune |
| Quickwit [^q2] | 0.6 s term over 212 GB | n2-standard-16, GCS | yes | no Mira counterpart exists |
| ClickHouse [^c3] | 0.68 s hot, 1.54 s cold | 9 queries, 1B rows | no | 37x rows, 2.7x cores |
| Elasticsearch [^c3] | 1.78 s hot, 9.55 s cold | 9 queries, 1B rows | no | 37x rows, 2.7x cores |
| Datadog Husky [^dd] | p50 2.01 ms per fragment | unstated fleet | no | leaf call, mostly cache |
| Honeycomb Retriever [^hc] | p90 2.5 s | Lambda fan-out fleet | no | fleet-wide, all shapes |

## Cost

Mira publishes no dollar figure, so there is no Mira row.

| Vendor | List price | Basis | = |
| --- | --- | --- | --- |
| Quickwit on S3 [^q5] | $8.4 per ingested TB/month | 2023 model, object store | no |
| Elastic Serverless [^es] | $0.07/GB in, $0.017/GB-mo | "as low as", tier floor | no |
| Grafana Cloud [^gc] | $0.55/GB combined | entry rate card | no |
| New Relic [^nr] | $0.40/GB ingested | plus $349/user/mo Pro | no |
| Datadog [^dd2] | $0.10/GB + $1.70/M indexed | queryable fraction unfixed | no |
| Honeycomb [^hc2] | $150 per 50M events | bills events, no byte size | no |

## Where Mira is ahead

| | |
| --- | --- |
| **Size of the thing you install** | 6.20 MiB stripped; the artifact table above has everything else surveyed. |
| **Memory at the working rate** | 232 MiB while taking 629,384 records/s. The peers publish theirs at 10,000 to 20,000 records/s. |
| **Queries that can skip files** | 2.56 ms to show an attribute value is in none of 137 blocks, against VictoriaLogs at 266 ms over 300 GB. |
| **Code you inherit** | 149 crates on `cargo tree --edges normal`; Parseable under the identical command is 462. |

## Where Mira is not ahead

### Gaps with a cause and a path

| Gap | Where it stands |
| --- | --- |
| **Ingest throughput** | 1,350,502 records/s against GreptimeDB's 621,367, and the rate used to sag as connections piled up, because one task flushed each signal: by ninety-six connections it was down to just over half the four-connection rate. Split within a signal, it now holds 84.2% of the four-connection rate and 73.9% of peak at ninety-six connections, nothing shed, acknowledgements at p99 2,661 ms against 55 ms at four. The 26% fall from 32 to 96 connections in the table above did not reproduce on the day the cause was measured: that day it was 5%. |
| **What sets the ceiling** | One lock on the write-ahead log, held while the log writes. Both obvious fixes are rejected: a log per signal moves the wait onto the device — time spent waiting for the lock falls to 0.63–0.795x while time spent writing climbs 1.94x and 2.39x — and its own records/s came back with the repeated runs disagreeing about which way it went, so no throughput figure from it would be honest; and a RAM disk is worth 1.096x at thirty-two connections and 1.005x at ninety-six, where again the runs disagreed about the direction. With the log out of the way, 92% of the time between a batch arriving and its acknowledgement goes on waiting for room to put it, so what actually limits it is sealing and publishing a block. |
| **Queries that cannot skip files** | A scan of everything is 885 ms over 27,066,368 rows once warm, and 1,441 ms on the first call after a restart, depending on whether the data still sits in the operating system's file cache. Opening the main tables up front and the attribute tables only when a query asks for them is 1.8x and 2.6x faster than 0.0.3. When Mira cannot skip files, it does not win. |

### Gaps that are what the design costs

| Gap | Where it stands |
| --- | --- |
| **Compression, on everyone else's denominator** | 8.8x on logs and 7.9x on traces against ClickHouse's 14.1x and VictoriaLogs' 11.2x. Rows are compressed in the order they arrive; ClickHouse sorts first, which hands its compressor long runs of one value. Every sort key tried here came out at or below the unsorted ratio. |
| **Full-text search** | There is none, so no row against Quickwit's 0.6 s word search over 212 GB. The reader Mira is built for arrives with a structured guess to narrow on, not a word to rank. |
| **Running across machines** | A node answers only from its own files, and nothing is copied to a second node: lose a node and you lose its data. Cold data has a way out — `--offload <uri>` copies a sealed file to an object store before retention deletes it, and the store's own listing is the catalogue: 14.2 s to upload 3.35 GiB, a median 16.6 s to fetch it back [^m4]. Hot data has `mira proxy`, which merges reads across nodes and keeps nothing itself. Its cost is measured and the case for it is not: ingest through it runs at 0.767x a single node at four connections, and a wide unfiltered read takes 3.89x as long [^m5]. The [operator](install.md#kubernetes) adds a replica when the fullest one runs out of room and archives a drained one on the way out — room to grow, not redundancy. Nothing moves data that is already written. |
| **Cost per GB** | No measured dollar figure, only bytes on disk. Quickwit's $8.4 per ingested TB per month is out of reach for anything serving reads off local disk, where the gp3 figure is ~$29.2. That gap is object storage, not tuning. |
| **Acknowledgement latency with the log off** | p50 657 ms, p99 2.6 s, spent waiting for a block to be sealed. Nobody else publishes an acknowledgement latency, and it is why the write-ahead log is [on by default](config.md#ingestwal): with it on, p50 8.5 ms and p99 55 ms at four connections. |

### Numbers this page has withdrawn

Five figures this page once carried are gone, so anyone who remembers one can
see what happened to it.

| Withdrawn | Why |
| --- | --- |
| A ceiling of 1.7 to 2.6 M records/s at any connection count | Read off the largest term in the log's time budget. Put the log on a RAM disk and the queue re-forms on admission instead, so that arithmetic was never the ceiling. |
| 175 ms over 24.0M rows, 137M rows/s, for a scan of everything | Does not reproduce. The 885 ms row in the query table above is the current figure. |
| A 1.55x median for the checksum cache's share of the read path | The per-pass readings spread far too wide to support one median, and the cache and the loading of attribute tables on demand cannot be separated by this harness. |
| A `series` slowdown, reported here as a regression | The metrics query code is byte-identical across the change and the two binaries differ in both directions. What moved was the mix of queries, not the route. |
| Upload and restore "agreeing within 6%" | Two samples agreeing by luck. Restore ranges 176.1 to 268.5 MiB/s across three runs; the 16.6 s above is the median of them. |

## Claims rejected

Rejected does not mean wrong. It means the claim cannot sit in a row without
misleading someone.

**Vendor scale claims with no method behind them.**

| Claim | Why it cannot sit in a row |
| --- | --- |
| Jaeger, "several billion spans per day" at Uber [^j2] | Cluster-wide, post-sampling, unstated span size, no node count. The arithmetic is the danger: 2–5e9/day is 23k–58k spans/s, *lower* than a laptop, while the phrase reads a thousand times higher. |
| Datadog Husky, "more than 100 trillion events" per day [^dd3] | Fleet aggregate over an unpublished machine count, and the same page uses 100 trillion for the *stored corpus* too, so the number is not pinned to one meaning. |
| Honeycomb, "on the order of 100,000 events per second" [^hc3] | An incidental descriptor of a customer environment on a page whose only measured result is query latency. |
| Quickwit, "1 PB per day / 14 million docs/s" [^q6] | A user's screenshot the vendor is relaying; Quickwit's own reproducible ceiling on the same page is 400 MB/s. |
| Quickwit at Binance, "1.6 PB per day" [^qb] | The denominator is Kubernetes *requests*, not observed CPU. |
| VictoriaLogs, "absorb around a GiB of logs per second even on slow, low-IOPS HDDs" [^v3] | Prose on an internals page, no hardware, no record size. |
| Parseable, "up to 90% and average 75%" compression [^p2] | No dataset, no codec, no before/after byte counts, and it contradicts the same vendor's "~10:1" elsewhere. |

**Numbers that are not the axis they are labelled as.**

| Claim | Why it cannot sit in a row |
| --- | --- |
| VictoriaLogs ClickBench "44,266 rows/s" [^v4] | Derived from a `load_time` whose timed region includes a 23.7 GB `wget` and a gunzip to ~75 GB. It is a download speed with an engine attached. |
| TrueFoundry's "VictoriaLogs 318 GiB vs Loki 501 GiB" filed as compression [^v5] | A storage delta between two competitors with no raw baseline; the methodology contradicts itself by ~78x and Loki's figure exceeds the stated corpus. |
| Datadog Husky's "1 GiB to process the column" filed as resident memory [^dd4] | It is 15,000 x 75 KiB — arithmetic on a field cap, describing the design Husky *rejected* in the next sentence. A footnote does not repair that; readers compare cells. |
| Elastic APM's "100 unsampled transactions/sec" [^ea2] and Elastic's "15 nodes x 64 GB" [^e2] | Both are inputs to, or outputs of, a provisioning formula. 34 of those 64 GB is explicitly OS page cache. |
| Honeycomb's "20 s to 0.2 s" [^hc] and Datadog's "a few hundred milliseconds increase in the median" [^dd5] | Self-relative deltas with no absolute baseline on either side. |
| Husky's fragment p50 of 2.01 ms [^dd] | Per 1,000 fragment queries, 300 are pruned at the metadata service and 560 by the result cache; 3.4% scan data at all. It is a cache lookup, and a user query is the fan-out envelope over hundreds of fragments. |
| Quickwit Lambda's "less than 100 ms" [^q7] | The authors' own words: the function "only checks that the metastore didn't change and serves back the earlier results". They call it unfair and exclude it. |
| Quickwit's "1 indexer, 236 h, 27 MB/s" [^q1] | 236 h is exactly 23 TB / 27 MB/s; the post performs that division itself. No 236-hour run happened. |

**Benchmarks against a straw configuration.**

| Benchmark | Why it cannot sit in a row |
| --- | --- |
| OpenObserve's ClickHouse at 2.14x [^oo] | Admittedly untuned, so kept in the table labelled as a floor. |
| VictoriaMetrics' log-collector benchmark [^vl] | vlagent's vendor benchmarking vlagent, every competitor at Helm-chart defaults, two collectors dropped from the tables for losing logs. |
| ClickHouse vs Elasticsearch at 4.95x storage [^c3] | The page itself notes a tuned ES config was ~20% smaller, and ES OSS cannot disable `_source`. |
| TrueFoundry's Loki [^v1] | Reported at "4 vCPUs (100% throttled)" against a 65 MB/s generator, so the two systems did not store the same bytes. |
| Elastic's own 220,000 docs/s [^e1] | Pure indexing, zero query load; the same post drops to 173,000 under 1,000 ops/s of search. |

**Wrong artifact.** Loki publishes a 40.6 MiB zip and the binary inside it is
138.34 MiB. Figures like that are corrected and kept, and every correction so far
has made the competitor's number larger, not smaller.

## Where the line is

What Mira will not do, and the answer you get when you ask for it.

| Refused | What the user is told |
| --- | --- |
| **SQL** | "Read the blocks with pyarrow or polars, and hand the table to DuckDB." Load-bearing, not stylistic: the read surface is a closed set of operations with no parser, planner or optimiser, and that closedness is the only thing keeping DataFusion out. **The day SQL is promised, DataFusion becomes the correct choice.** |
| **DataFusion** | 47 direct dependencies, ~1.5M SLoC transitive, 50.0 MiB binary, against 6.20 MiB. |
| **Replication of your data** | "A lost disk is lost data for that node's share. Export to two replicas from your Collector." Keeping a second copy means deciding where it lives, and that decision is state the nodes would have to agree on. |
| **Separation of storage and compute** | Needs a scheduler, a metadata service and a membership list. Scale by adding independent replicas behind an L4 balancer; retention is the rebalancer. |
| **Stored dashboards, saved views, user preferences** | "The link *is* the saved view; curated dashboards are files you commit." A saved dashboard has to survive a restart and agree across replicas — exactly the shared state principle 4 refuses — and it would be the first thing in the system that changes after it is written. |
| **A Grafana datasource plugin** | "Install nothing. Mira ships the UI." A signed plugin is a second artifact in a second language, with a third-party review and a signing subscription, and no version of that leaves "one binary" literally true. Nor is there a plugin-free route: Mira serves no Loki `query_range` and no Tempo `/api/traces`, so the built-in datasources have nothing to point at, and adding them would put two more query dialects on the surface whose smallness is what keeps DataFusion out. |
| **Ingest-side shaping: drop rules, sampling, transforms** | "Shaping belongs in the Collector, and here is a reference `otelcol` config." Shaping rules are a filter graph and the Collector already is one. The buyer's real question is "who is costing me money", which is cost *attribution*, not a quota. |
| **Loki's cardinality guards** (`max_streams_per_user`, `max_label_names_per_series: 15`) | "Those exist to protect Loki's index. Mira has no such index." Refusing them is a feature claim, not a gap. |
| **Prometheus `remote_read`** | It would make Mira a dumb sample pipe, streaming raw points to a Prometheus that does the work itself — the worst possible shape for a columnar store, and it throws away every filter the engine exists to apply before reading. |
| **Prometheus `remote_write` receiver** | "Run the Collector's `prometheusreceiver` and export OTLP." Flat label sets carry no resource, no scope and no OpenTelemetry conventions, so every series they make lands with no identity attached. Keep the lossy hop outside Mira. |
| **Tiering knobs** (`offloadPeriod`, cache path, cache size, eviction policy) | "There is one flag and it is an address: `--offload <uri>`." Shipped, and still one flag: the period is `storage.retention`, because the block leaving the disk *is* the event, and there is no cache to size because reads never consult the object store — `mira offload restore` is the whole retrieval path. Every competitor's tiering config is a documented foot-gun. |
| **`s3://`, and every other scheme** | "Mount the bucket. Everything after `file://` is a path, so `file:///srv/cold` and a mounted bucket are the same thing." Signing a request needs HMAC-SHA256 and reading a listing needs an XML parser; neither is in the 149-crate graph this page publishes, and that count is a product property. A scheme Mira does not know is refused at startup, not at the first sweep. |
| **Per-record deletion (GDPR erasure)** | This one hurts: it would mean rewriting a sealed file, and readers are safe only because sealed files never change. The answer is a directory per tenant, so deletion stays the removal of whole directories, plus short retention. Regulated buyers should be told no explicitly rather than find out at audit. |
| **SAML** | "Put an OIDC bridge in front of Mira." SAML needs a certificate and a metadata store; OIDC needs a JWKS fetch. |
| **A Kubernetes operator / CRDs** | Was refused — "a controller reconciling a stateless single binary is a second process managing a thing with no state to reconcile." Now shipped, because the refusal named the wrong subject: scaling *in* has state — which replica is draining, whether its volume has been archived — and `mira-operator` is a separate binary holding it. Mira itself still coordinates nothing, and `spec.replicas` is a floor rather than a desired count, so the controller can permit a drain and never order one. |
| **Iceberg / a catalog** | Catalog, manifests and snapshots are coordination state and a second product. Parquet *export* is revisited when someone names Athena or Trino with a workload attached; it costs ~20 crates. |
| **Profiles as a fourth signal** | Deferred, not refused, and the gate is external: the signal is Alpha and the proto is still removing fields. No placeholder table in the schema. |

[^m1]: `cargo bench -p miradb-core --bench encode_bench`.
[^m2]: `cargo run --release -p miradb-core --example tier` over all 1,652 tables of an 8.33 GiB corpus.
[^m3]: [Architecture section 11](architecture/performance.md), the query rows; steady state, server-reported `elapsed_us`.
[^m4]: `scripts/measure/offload-cycle.sh`; the upload figure is the offload sweep less the plain unlink sweep, medians of two runs each.
[^m5]: `scripts/measure/proxy-ab.sh`; medians of nine per-pass ratios, both arms in every pass, B first; [Architecture section 12.2.5](architecture/replicas.md#1225-what-the-hop-costs-on-one-box).
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
