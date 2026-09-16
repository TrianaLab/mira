# 11. Performance model

The four axes, each with a target and a measurement. Measured on an Apple M3 Pro
(12 cores, 18 GiB), release build.

Every figure in this section comes from **one corpus and one binary**, because a
table assembled from runs weeks apart is a table whose rows cannot be divided by
each other. The corpus is `loadgen --conns 4 --batch 8192 --for 40s` over
loopback: 27,066,368 log records, 27,066,368 spans and 39,648 data points, 8.33
GiB of Arrow across 1,652 tables in 137 log blocks, 155 trace blocks and 24
metric blocks. The ingest rows are separate 30 s runs against a fresh server,
each the median of three; the query rows are that corpus, read back after a
restart, and every one of them is a paired A/B against the 0.0.1 binary run
back to back in the same sitting.

## The corpora

Several sections need a corpus this page cannot be:

| corpus | shape | what it is for |
| --- | --- | --- |
| the table's own | 8.33 GiB, 1,652 tables, 137 log / 155 trace / 24 metric blocks | every row of the table above |
| **small** | 137 blocks, 3,599,317,452 bytes (3.352 GiB), 62 logs / 64 traces / 11 metrics | cold tier, block reopens — round-tripping 8.33 GiB through an upload takes long enough that the box moves underneath it |
| **small, compacted** | those same 137 blocks after the cold tier has finished with them: 718 tables, 440,421,916 bytes, all 137 marked `cold` | the compacted arm of the lazy-open entry |
| **plain** | 115 blocks, 611 tables, 2,927,859,966 bytes, 50 logs / 53 traces / 12 metrics, none compacted | the main arm of the lazy-open entry, which needs attribute tables that are still uncompressed |
| per-run | built fresh by the script that reads it | restart replay, and the second sitting's 9.6 GiB / 5.01 GiB pair |

Each is internally paired; **none of their numbers may be divided into the table
above**.

Four of the five are reproducible from `scripts/measure/`: `restart-replay.sh`,
`offload-cycle.sh`, `block-reopens.sh`, `lazy-detail.sh`. **The second sitting's
9.6 GiB / 5.01 GiB pair is not** — built by hand, so reproducible in method and
not in corpus: `loadgen` stops on `--for <duration>`, so the *volume* depends
on how fast the box was. See
[the measurement contract](../internals/measurement.md).

The plain one exists because **a corpus is not a constant while a server is
running on it.** The cold tier compacts aged blocks from inside the running
server, so a long A/B starts plain, finishes compacted, and measures how far
through the transition each pass landed. It invalidated a table this section
published; `lazy-detail.sh` now fingerprints the corpus before every pass and
stops if it moved.

The **second sitting** exists because the checksum cache (section 3.3) landed
after the table was measured. It covers the query rows only: two corpora from
the same generator, read by this binary and by 04561ed back to back. Where the
sittings disagree the disagreement is the finding, written up under the last
row.

Six flushers per signal rather than one (section 4) changed the *shape* of that
corpus: 137 log blocks of ~204,800 rows where one flusher sealed 87 of
~330,000. Smaller blocks, more of them — which matters below: what a query pays
per block dominates what it pays per row.

## The four axes are scored together

That is why the generator that produced this table is also the load harness: `loadgen --readers N --pid N --data-dir P` reports ingest, six
classes of query latency, the server's resident set and the bytes it added per
record from one run. An engine measured one axis at a time is an engine that is
fast at whichever one its authors were watching.
[End-to-end testing section 3](../internals/e2e.md#3-the-load-harness) is how to
drive it and what it teaches.

## Reading the two query columns

"First" is the first call after a process restart; "steady" is the same call repeated. The gap between them is
virtual-memory work — establishing 316 blocks' worth of mappings and faulting
them in — and for every row that prunes, so is nearly all of the steady figure.
This used to say **neither column is a cold-disk number**, because 8.33 GiB fits
in this machine's 18 GiB of page cache. That is arithmetic, not a measurement,
and the second sitting shows it does not hold for the last row: 18 GiB shared
with a VM, Docker and a browser does not keep 8 GiB of corpus resident, and the
same scan over a corpus that *does* stay resident is three times cheaper per
row. Treat the pruning rows as warm and the last row as partly not.

| Axis | Target | Measured | |
| --- | --- | --- | --- |
| Ingest throughput | ≥ 1 M records/s/core | **886k records/s/core** — 629,384 records/s on 0.71 cores; **1,350,502 records/s** aggregate at four connections and a plateau peak of **1,537,875** at thirty-two, on 1.75 and 2.23 cores, nothing shed at any shape | ~ |
| Resident footprint | ≤ 2 × the open block's target size | **232 MiB** at one connection, **689 MiB** at four, **1,648 MiB** at 96 — [see below](performance-durability.md#resident-footprint-is-the-axis-with-no-number) | ~ |
| Ack latency | — | p50 **8.5 ms**, p99 **55 ms** at four connections, log on; p50 **657 ms**, p99 **2,647 ms** with it off | [see below](performance-durability.md#the-two-ack-rows-are-two-chosen-contracts-not-a-fast-path-and-a-slow-one) |
| Query: attribute value, absent | ≤ 10 ms | **8.3 ms** first, **2.6 ms** steady, 0 of 137 blocks | ✓ |
| Query: attribute value, matching | ≤ 10 ms | **4.1 ms** first, **4.5 ms** steady, 1 of 137 blocks, 204,800 rows | ✓ |
| Query: unfiltered `limit 100` | ≤ 10 ms | **29.1 ms** first, **4.6 ms** steady, 1 of 137 blocks, 204,800 rows | ✓ |
| Query: trace by id | ≤ 10 ms | **8.3 ms** first, **4.7 ms** steady, 2 of 155 blocks, 73,728 rows — first sitting; the second measures the checksum cache 1.47× under it | ✓ |
| Query: metric names | — | **8.9 ms** first, **4.7 ms** steady, 24 of 24 blocks | — |
| Query: substring, no time bound, prunes nothing | — | **1,441 ms** first, **885 ms** steady, 137 of 137 blocks, 27.1 M rows — first sitting, and the row the second sitting has the most to say about | [see below](performance-query.md#the-unpruned-row-is-bound-by-what-the-first-column-is-bound-by) |
| Cost per GB ingested | ≤ 0.35 B/B | **1.20 B/B** hot, **0.14 B/B** compacted | ✓ |
| Binary size | ≤ 20 MB stripped with UI + query + MCP | **6.06 MiB** / 122 crates | ✓ |

## The ingest row is per core, and that is the denominator to argue with

Aggregate throughput is a property of the offered load: raise `--conns` and it
moves without a line of the server changing. Per core is measured rather than
inferred — the harness takes the server's CPU-seconds either side of the run,
so 0.71 and 1.75 are consumed CPU over wall clock, not a core count somebody
chose. The server is **not CPU-bound at any shape measured here**: the fastest
row, 1,537,875 records/s at thirty-two connections, costs 2.23 of twelve cores,
and the per-core rate is *highest* at one connection — 886k records/s/core,
where there is nothing to contend over. Throughput **plateaus** between sixteen
and thirty-two connections rather than peaking at a point — the two are within
2% of each other on medians whose passes span 6% and 16% — then falls away to
1,136,941 at 96. That is a new shape: before `ingest.shards` (section 4) the
curve peaked at four connections and declined from there, and 96 returned
734,142. Ten idle cores at the plateau means the ceiling is somewhere other
than the engine's arithmetic, and [the next page](performance-ingest.md#the-plateau-is-one-mutex-held-across-a-write2)
names it. Two caveats stay on
the aggregate whatever the cause: the generator is co-resident and encoding
8192 protobuf records per batch inside the same loop, so part of the per-batch
cost is the harness's, and separating them wants the generator on a second
machine, the one thing a single-laptop harness cannot do. Treat the aggregate
as a floor and the per-core figure as the comparable one.
