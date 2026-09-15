# Performance — verification, restart and the ack contracts

## A query verifies the tables it reads, and 0.0.3 read tables no query wanted

The per-table CRC32 shipped in 0.0.3 (section 3.3); what changed is that
`Block::open` hashed every attribute level of every block it opened, including
blocks a scan was about to reject. Those tables are **42.9% of a logs block**
and **45.7% of a traces block** for as long as the block is plain.

`scripts/measure/lazy-detail.sh` alternates two binaries inside each of nine
passes over a 115-block corpus ingested minutes before the run, **none of it
compacted**. Two changes landed in one release, so there are three arms:

| case | cache alone | split alone | both |
| --- | ---: | ---: | ---: |
| scan-miss-logs, 50/50 blocks, 8,898,000 rows | −18.6% | **−30.8%** | **−43.3%** |
| scan-miss-traces, 53/53 blocks, 8,898,000 rows | −23.9% | **−46.8%** | **−62.2%** |
| scan-attr (control) | −26.4% | −2.3% | −23.5% |
| page-100 (control) | −27.9% | +2.0% | −25.4% |

"Cache alone" is the verification map against 0.0.3, "split alone" this binary
against the map-only build, "both" this binary against 0.0.3. The third column
is their **product** rather than their sum — 0.814 × 0.692 predicts −43.7%
against −43.3% measured — which is what "the two changes stack" (section 3.3)
has to mean.

### The controls are the point of the middle column

`scan-attr` is the *same query shape* as `scan-miss-logs` but its predicate
names an attribute, and `page-100` renders a hundred rows, so both builds read
the attribute tables in both cases. Neither may move under the split and neither
does — and what says so is the **sign** of the nine per-pass deltas, not the
median: both treatments are negative in 9 of 9, both controls change sign. A
control whose median is small but whose deltas all point one way would be a real
effect called noise.

The controls do move in the other two columns — the map saves a re-hash on every
reopen — and since the harness warms each server twice per case, those are
warm-map figures, the upper bound.

### The split survives compaction

Against the *same* corpus once the cold tier has finished with it — 137 blocks,
all cold, fingerprinted after every pass — the split is **−33.5%** and
**−56.0%**, controls −13.4% and +2.0%. The byte shares do not predict that: the
attribute tables are **5.5%** of
a compacted logs block and **6.3%** of a traces one, but they compress **66.3×**
against the root table's **5.14×**, so what the split skips on a cold block is
not bytes to hash, it is an inflate.

## The verification cache, and the number that nearly kept it out

The other half of the idea is to remember that a block was verified so a later
open can skip the hash (section 3.3); the "cache alone" column prices it at
**−18.6% to −27.9%**, near-uniform because, unlike the split, it helps every
query that reopens a block. Two numbers taken while the answer was still no are
kept, one a lesson in what a measurement may decide.

### The one that decides nothing

`scripts/measure/block-reopens.sh` records **21 block opens in total** across 18
queries over 126 distinct blocks — block spread, not the quantity a
process-scoped cache is priced on, which is how often a long-lived server
reopens the same path across thousands of queries. Read as a reopen count it
says a cache is pointless, and it nearly was.

### The other is the ceiling

A throwaway build whose CRC comparison was patched to always pass (reverted
immediately, not in the tree) measures verification made free rather than merely
cached: against the split-only binary, scan-miss-logs −18.0%, scan-miss-traces
−29.5%, scan-attr −26.0%, page-100 −24.1%. It brackets
the measured cache rather than bounding it: the runs share neither corpus nor
baseline, so −26.4% against a −26.0% ceiling is two methods agreeing, each
putting the CRC at about a quarter of an open.

### The two objections raised while the answer was no

The key is not the path — the tier replaces a file under a verified path an hour
after it lands — but length, mtime and `ino` together behind a settle window.
And the footprint is capped: `VERIFIED_CAP` at 65,536 entries, cleared wholesale
rather than evicted, with the LRU named as the upgrade path.

## A restart replays past the slowest shard, and it is the allowed direction

`scripts/measure/restart-replay.sh`, three paired runs per binary, one restart
each: the rows the corpus gained across the restart were 0.255% on this branch
and 1.318% on 0.0.3, zero in the other four. Both medians are 0% and the largest
excursion is the **baseline's** — a property of the log, not a regression in the
change.

`Wal::watermark_for` returns the oldest unpublished sequence across a signal's
shards, so one slow shard pins the watermark low and a boot replays frames a
published block already covers — too low being the allowed direction of error,
duplicated rows over lost ones, and no loss was seen. It is a measurable
cost of the no-coordination-state rule, and it invalidates any comparison across
a restart: `offload-cycle.sh` drains to `replayed=0` first.

## What changed the shape of the curve was admission, not arithmetic

The first revision shed the moment the queue was full, and at 96 connections
that read 333,373 records/s with 93% of exports getting a 503 — four cores busy
doing work that was thrown away, because tonic and axum both decode a request
before the handler sees it. `ADMIT_WAIT` (section 4) parks a full queue for up
to five seconds instead: same queue depth, 578,294 records/s and **nothing
shed**, on 1.61 cores instead of 3.85. Those two are a paired A/B from one
sitting; the published 96-connection row is the later sweep's 1,136,941.

The bound is the connection count — every waiter is a request already in memory
— where a deeper queue is bounded by nothing: `--queue 2048` also removes the
shedding, by buffering ~20 GiB of anonymous memory on an 18 GiB machine.

## The two ack rows are two chosen contracts, not a fast path and a slow one

With the log on — the default — the ack is a `write(2)` into the page cache,
priced alone by `cargo bench -p miradb-core --bench wal_bench` at p50 7 µs for a
4 KiB body. What a client sees also includes the decode and the queue: p50
8.5 ms, p99 55 ms at four connections. With the log *off* the run is
block-seal-bound, which is what p50 657 ms and p99 2,647 ms say — the block
filling, then a durable publish landing in front of a waiter. Neither costs
read-your-writes anything, since the open block is queryable (section 4), so the
choice is purely about what survives power loss.

## Per core, with no I/O in the path

`cargo bench -p miradb-core --bench encode_bench` decodes and appends 1.09M log
records/s, 1.06M spans/s and 2.86M data points/s on one thread — ten-run
medians on a desktop, to be read as "about a million a second per core". The
split inside them is the useful part: appending a log record costs 0.158 µs and
*sealing* it costs 0.372 µs. Almost all of that is sidecar construction —
`attrs::index`, `zone::index` and `bloom::build` each walk every attribute
row — which is why section 4's open-block snapshots skip
them, and why the 32 MiB block target is a read-path decision the write path can
afford.

## Resident footprint is the axis with no number

Peak RSS moves by a factor of seven across the ingest sweep alone — 232 MiB at
1 connection, 689 MiB at 4, 1,648 MiB at 96 in
[End-to-end testing section 3](../internals/e2e.md#3-the-load-harness) — and it
is not this row: it counts every mapped block page a query touched, plus three
signals' builders and every in-flight decode.

Sharding moved this row *down*, which was not the goal. At four connections peak
RSS is 689 MiB where one flusher needed 1,366 MiB. Six open blocks per signal is
strictly more block state than one, so the saving is the queue: an export never
queued is never resident. What holds the bound is section 5's refusal of
`concat_batches` — a property of the code and a test, not a measurement, so this
row stays an intention.
