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
Across the six tables that carry one there are 39 marked rows. 12 are Mira's own
and take `—`; of the 27 competitor claims, **19 are `no`**, seven are `yes` and
one is `~`.
<!-- END GENERATED: market-claim-tally -->
A row marked `no` tells you which number is larger, not by how much.

## Who else is in this space

| Class | Who | Where they are stuck — a weakness that follows from a choice they cannot undo |
| --- | --- | --- |
| **Composed OSS stack** | Grafana LGTM, kube-prometheus-stack | Loki's index is a label index; [its own docs](https://grafana.com/docs/loki/latest/get-started/labels/) concede it "was not designed to support high cardinality label values". |
| **OTel-native single binary** | SigNoz, OpenObserve, Uptrace, ClickStack, Coroot, Dash0 | "Single binary" with ClickHouse or DataFusion and S3 inside — a real dependency and a real thing to tune. |
| **Log specialists** | VictoriaLogs, Parseable, Quickwit | Genuinely fast and genuinely simple. One signal each, their own data model rather than OpenTelemetry's, their own query language. The hardest class to beat, and no use attacking on "nothing to tune": that one is a draw. |
| **Commercial SaaS** | Datadog, Honeycomb, New Relic, Dynatrace, Chronosphere, Grafana Cloud | Billing per GB and per host makes your own cost control their product. |
| **Warehouse-backed** | ClickHouse direct, Databricks, Snowflake + OTel | General, and you own the schema, the ingestion, the retention and the query language. Mira should not contest the "we already have a data team" segment. |

## Ingest, one node

Mira's row counts the CPU the process actually burned. Only Loki's 2.75 and
Quickwit's 2.2 [^q2] are measured that way; the rest count the CPU the machine
was given, busy or not.

| Engine | Published | Their hardware | = | Reason |
| --- | --- | --- | --- | --- |
| **Mira** | **1,350,502 rec/s, 176.5 MiB/s** at 137 B | M3 Pro 12c / 18 GiB, 1.75 busy | — | 4 connections, nothing shed |
| Mira, sweep peak | 1,537,875 rec/s, 2.23 busy | same box, 32 connections | — | 1.14x the paired row |
| Mira, per-core ceiling | 629,384 rec/s, 0.71 cores | same box, one connection | — | 886k rec/s per consumed core |
| Mira, 96 connections | 1,136,941 rec/s, 2.23 busy | same box, 96 connections | — | nothing shed, ack p99 2.7 s |
| GreptimeDB 1.0 [^g1] | 621,367 rows/s OTLP | M4 Max 16c / 48 GB | no | disclaims absolute rate |
| Loki 2.9 [^q2] | 73.8k docs/s at 872 B | n2-standard-16, 2.75 vCPU | no | 17% CPU, unsaturated run |
| VictoriaLogs [^v1] | 66 MB/s | 4 vCPU / 8 GiB pod | no | offered load, not ceiling |
| SigNoz stack [^s1] | ~55,000 log lines/s | c6a.4xlarge, whole stack | no | collector plus store |
| Parseable [^p1] | ~133 MiB/s per node | 4 nodes, 12 generators | no | cluster sum, 2% network |
| Quickwit [^q2] | 33,000 docs/s at 872 B | n2-standard-16, 2.2 vCPU | no | inverted index to GCS |

## Resident set

| Engine | Published | At what rate | = | Reason |
| --- | --- | --- | --- | --- |
| **Mira** | **232 MiB peak** | 629k rec/s, whole process | — | ps-sampled, includes both UIs |
| GreptimeDB 0.12 [^g2] | 408 MB | 20,000 rows/s, c5d.2xlarge | no | 18x lower offered rate |
| ClickHouse [^v2] | 1.12 GiB | 10,000 spans/s, 4 vCPU | no | memory limit, capped rate |
| VictoriaLogs [^v2] | 1.15 GiB | 10,000 spans/s, same box | no | memory limit, capped rate |
| Grafana Tempo [^v2] | 4.26 GiB, then OOM | 10,000 spans/s, same box | no | memory limit, capped |
| SigNoz stack [^s1] | ~6 GB | 55,000 logs/s, c6a.4xlarge | no | whole VM, several processes |

The three [^v2] rows were measured inside a memory limit, so each of those is a
budget partly spent rather than a floor; Mira's is a laptop process with no
limit. The column that matters is the rate. And Mira's own memory grows with
connections: the same process reaches 689 MiB at four and 1,648 MiB at
ninety-six, above ClickHouse's 1.12 GiB.

## Artifact — stripped binary

These are unpacked sizes.

| Artifact | Stripped binary | vs Mira | Deps | = |
| --- | --- | --- | --- | --- |
| **Mira** | **6.20 MiB** (arm64 macOS) | 1.0x | 149 crates | — |
| VictoriaLogs 1.52 [^b1] | 16.26 MiB (amd64 Linux) | 2.6x | 98 Go packages | yes |
| Grafana Tempo 3.0.3 [^b2] | 93.94 MiB (arm64 Linux) | 15.2x | 425 modules | yes |
| Grafana Loki 3.7.7 [^b5] | 138.34 MiB (arm64 macOS) | 22.3x | 407 modules | yes |
| Quickwit 0.9.0 [^b6] | 144.29 MiB (arm64 macOS) | 23.3x | 1,171 lock entries | yes |
| Parseable 3.2.0 [^b7] | 152.44 MiB (arm64 macOS) | 24.6x | 462 crates | yes |
| ClickHouse 26.3 [^b8] | 153.80 MiB (arm64 macOS) | 24.8x | — | yes |

Read the dependency column as a direction, not a score: VictoriaLogs' 98 against
Mira's 149 is the same order, and it has one C dependency (`gozstd`) as Mira does.

## Compression

| Engine | Ratio | Denominator | = |
| --- | --- | --- | --- |
| **Mira, wire** [^m2] | 7.0x (0.14 B/B) | OTLP protobuf on the wire | — |
| **Mira, internal** [^m2] | 8.8x logs, 7.9x traces | uncompressed Arrow columns | — |
| ClickHouse `otel_logs` [^c1] | 14.1x | engine-internal column bytes | no |
| VictoriaLogs [^v3] | 11.2x | engine-internal column bytes | no |
| ClickHouse vs NDJSON [^oo] | 2.14x | raw input bytes on disk | ~ |

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
| Quickwit [^q2] | 0.6 s term over 212 GB | n2-standard-16, GCS | yes | no Mira counterpart exists |
| ClickHouse [^c3] | 0.68 s hot, 1.54 s cold | 9 queries, 1B rows | no | 37x rows, 2.7x cores |

## Cost

Mira publishes no dollar figure, so there is no Mira row.

| Vendor | List price | Basis | = |
| --- | --- | --- | --- |
| Quickwit on S3 [^q5] | $8.4 per ingested TB/month | 2023 model, object store | no |
| Elastic Serverless [^es] | $0.07/GB in, $0.017/GB-mo | "as low as", tier floor | no |
| Grafana Cloud [^gc] | $0.55/GB combined | entry rate card | no |
| Datadog [^dd2] | $0.10/GB + $1.70/M indexed | queryable fraction unfixed | no |

## Where Mira is ahead

| | |
| --- | --- |
| **Size of the thing you install** | 6.20 MiB stripped; the artifact table above has everything else surveyed. |
| **Memory at the working rate** | 232 MiB while taking 629,384 records/s. The peers publish theirs at 10,000 to 55,000 records/s. |
| **Queries that can skip files** | 2.56 ms to show an attribute value is in none of 137 blocks, against VictoriaLogs at 266 ms over 300 GB. |
| **Code you inherit** | 149 crates on `cargo tree --edges normal`; Parseable under the identical command is 462. |

## Where Mira is not ahead

### Gaps with a cause and a path

| Gap | Where it stands |
| --- | --- |
| **Ingest throughput** | 1,350,502 records/s against GreptimeDB's 621,367, and the rate used to sag as connections piled up, because one task flushed each signal: by ninety-six connections it was down to just over half the four-connection rate. Split within a signal, it now holds 84.2% of the four-connection rate and 73.9% of peak at ninety-six connections, nothing shed, acknowledgements at p99 2,661 ms against 55 ms at four. |
| **What sets the ceiling** | One lock on the write-ahead log, held while the log writes. Both obvious fixes were tried and rejected: a log per signal moves the wait onto the device, where time spent writing climbs 1.94x and 2.39x; a RAM disk is worth 1.096x at thirty-two connections and 1.005x at ninety-six, and the repeated runs disagreed about the direction. On that RAM disk, 92% of the time between a batch arriving and its acknowledgement goes on waiting for room to put it, so what actually limits it is sealing and publishing a block. |
| **Queries that cannot skip files** | A scan of everything is 885 ms over 27,066,368 rows once warm, and 1,441 ms on the first call after a restart, depending on whether the data still sits in the operating system's file cache. When Mira cannot skip files, it does not win. |

### Gaps that are what the design costs

| Gap | Where it stands |
| --- | --- |
| **Compression, on everyone else's denominator** | 8.8x on logs and 7.9x on traces against ClickHouse's 14.1x and VictoriaLogs' 11.2x. Rows are compressed in arrival order; ClickHouse sorts first, which hands its compressor long runs of one value. Sorting before sealing was tried: the best key gained 1.4%, which does not pay for the locality it costs. |
| **Full-text search** | There is none, so no row against Quickwit's 0.6 s word search over 212 GB. |
| **Running across machines** | A node answers only from its own files, and nothing is copied to a second node: lose a node and you lose its data. Cold data has a way out — `--offload <uri>` copies a sealed file to an object store before retention deletes it, and the store's own listing is the catalogue: 14.2 s to upload 3.35 GiB, a median 16.6 s to fetch it back [^m4]. Hot data has `mira proxy`, which merges reads across nodes and keeps nothing itself. Its cost is measured and the case for it is not: ingest through it runs at 0.767x a single node at four connections, and a wide unfiltered read takes 3.89x as long [^m5]. The [operator](install.md#kubernetes) adds a replica when the fullest one runs out of room and archives a drained one on the way out — room to grow, not redundancy. Nothing moves data that is already written. |
| **Cost per GB** | No measured dollar figure, only bytes on disk. Quickwit's $8.4 per ingested TB per month is out of reach for anything serving reads off local disk, where the gp3 figure is ~$29.2. |
| **Acknowledgement latency with the log off** | p50 657 ms, p99 2.6 s, spent waiting for a block to be sealed. Nobody else publishes an acknowledgement latency, and it is why the write-ahead log is [on by default](config.md#ingestwal): with it on, p50 8.5 ms and p99 55 ms at four connections. |

### Numbers this page has withdrawn

| Withdrawn | Why |
| --- | --- |
| A ceiling of 1.7 to 2.6 M records/s at any connection count | Read off the largest term in the log's time budget. Put the log on a RAM disk and the queue re-forms on admission, so that arithmetic was never the ceiling. |
| 175 ms over 24.0M rows, 137M rows/s, for a scan of everything | Does not reproduce. The 885 ms row in the query table above is the current figure. |
| A 1.55x median for the checksum cache's share of the read path | The per-pass readings spread far too wide to support one median, and the cache and the loading of attribute tables on demand cannot be separated by this harness. |
| A `series` slowdown, reported here as a regression | The metrics query code is byte-identical across the change and the two binaries differ in both directions. |
| Upload and restore "agreeing within 6%" | Two samples agreeing by luck. |

## Where the line is

What Mira will not do, and the answer you get when you ask for it.

| Refused | What the user is told |
| --- | --- |
| **SQL** | "Read the blocks with pyarrow or polars, and hand the table to DuckDB." **The day SQL is promised, DataFusion becomes the correct choice.** |
| **DataFusion** | 47 direct dependencies, ~1.5M SLoC transitive, 50.0 MiB binary, against 6.20 MiB. |
| **Replication of your data** | "A lost disk is lost data for that node's share. Export to two replicas from your Collector." |
| **Separation of storage and compute** | Scale by adding independent replicas behind an L4 balancer; retention is the rebalancer. |
| **Stored dashboards, saved views, user preferences** | "The link *is* the saved view; curated dashboards are files you commit." |
| **A Grafana datasource plugin** | "Install nothing. Mira ships the UI." Nor is there a plugin-free route: Mira serves no Loki `query_range` and no Tempo `/api/traces`, so the built-in datasources have nothing to point at. |
| **Ingest-side shaping: drop rules, sampling, transforms** | "Shaping belongs in the Collector, and here is a reference `otelcol` config." |
| **Loki's cardinality guards** (`max_streams_per_user`, `max_label_names_per_series: 15`) | "Those exist to protect Loki's index. Mira has no such index." |
| **Prometheus `remote_read`** | It would make Mira a dumb sample pipe, streaming raw points to a Prometheus that does the work itself. |
| **Prometheus `remote_write` receiver** | "Run the Collector's `prometheusreceiver` and export OTLP." |
| **Tiering knobs** (`offloadPeriod`, cache path, cache size, eviction policy) | "There is one flag and it is an address: `--offload <uri>`." Shipped, and still one flag: the period is `storage.retention`, and there is no cache to size because reads never consult the object store — `mira offload restore` is the whole retrieval path. |
| **`s3://`, and every other scheme** | "Mount the bucket. Everything after `file://` is a path, so `file:///srv/cold` and a mounted bucket are the same thing." A scheme Mira does not know is refused at startup, not at the first sweep. |
| **Per-record deletion (GDPR erasure)** | The answer is a directory per tenant, so deletion stays the removal of whole directories, plus short retention. |
| **SAML** | "Put an OIDC bridge in front of Mira." |
| **Iceberg / a catalog** | Parquet *export* is revisited when someone names Athena or Trino with a workload attached; it costs ~20 crates. |
| **Profiles as a fourth signal** | Deferred, not refused, and the gate is external: the signal is Alpha and the proto is still removing fields. No placeholder table in the schema. |

[^m2]: `cargo run --release -p miradb-core --example tier` over all 1,652 tables of an 8.33 GiB corpus.
[^m3]: [Architecture section 11](architecture/performance.md), the query rows; steady state, server-reported `elapsed_us`.
[^m4]: `scripts/measure/offload-cycle.sh`; the upload figure is the offload sweep less the plain unlink sweep, medians of two runs each; the restore figure is the median of three runs, which spread 176.1 to 268.5 MiB/s.
[^m5]: `scripts/measure/proxy-ab.sh`; medians of nine per-pass ratios, both arms in every pass, B first; [Architecture section 12.2.5](architecture/replicas.md#1225-what-the-hop-costs-on-one-box).
[^g1]: <https://greptime.com/blogs/2026-03-24-ingestion-protocol-benchmark>
[^g2]: <https://greptime.com/blogs/2025-03-10-log-benchmark-greptimedb>
[^v1]: <https://www.truefoundry.com/blog/victorialogs-vs-loki>
[^v2]: <https://victoriametrics.com/blog/dev-note-distributed-tracing-with-victorialogs/>
[^v3]: <https://victoriametrics.com/blog/victorialogs-internals-columnar-storage-on-disk/>
[^s1]: <https://signoz.io/blog/logs-performance-benchmark/>
[^p1]: <https://www.parseable.com/blog/the-economics-and-physics-of-100-tb-telemetry-data-per-day>
[^q2]: <https://quickwit.io/blog/benchmarking-quickwit-loki>
[^q5]: <https://quickwit.io/blog/benchmarking-quickwit-engine-on-an-adversarial-dataset>
[^es]: <https://www.elastic.co/pricing/serverless-observability>
[^c1]: <https://clickhouse.com/blog/storing-log-data-in-clickhouse-fluent-bit-vector-open-telemetry>
[^c3]: <https://clickhouse.com/blog/elasticsearch-log-analytics-clickhouse>
[^oo]: <https://openobserve.ai/blog/openobserve-vs-clickhouse-one-billion-logs-benchmark/>
[^dd2]: <https://www.datadoghq.com/pricing/>
[^gc]: <https://grafana.com/pricing/>
[^b1]: <https://github.com/VictoriaMetrics/VictoriaLogs/releases/tag/v1.52.0>
[^b2]: <https://github.com/grafana/tempo/releases/tag/v3.0.3>
[^b5]: <https://github.com/grafana/loki/releases/tag/v3.7.7>
[^b6]: <https://github.com/quickwit-oss/quickwit/releases/tag/v0.9.0>
[^b7]: <https://github.com/parseablehq/parseable/releases/tag/v3.2.0>
[^b8]: <https://github.com/ClickHouse/ClickHouse/releases/tag/v26.3.33.24-lts>
