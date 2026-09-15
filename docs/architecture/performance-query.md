# Performance — the block is the unit of query cost

## Blocks not opened is the whole game

The two sidecar filters (section 7.4) took the block count from "all of them" to
one or zero, worth between 60× and 1800×, and the reason these rows are in
milliseconds at all. Everything below is the cost of the blocks that *are*
opened.

## The per-block cost is opening the block, not scanning it

`scan_cost_per_row` prices both sides. Over two million rows, evaluating a
predicate costs **0.047 ns/row** for no term at all, 0.485 for a dictionary
equality, 1.064 for a resource attribute, 2.402 for a record attribute and
**5.586** for a UTF-8 `contains`. The *same block through the whole read path*
costs **16 to 17 ns/row**. So the scan is a third of what the dearest predicate
pays and under 3% of what the cheap ones do; the rest is `Block::open` — the
`mmap`'s minor faults, the dictionary scan and the two child indexes. `limit 1`
and the whole block cost the same per row, because the block had to be opened
either way.

### Verified once per process

The CRC32 of every table body (section 3.3) used to be re-paid on every open of
a file that by construction never changes. At 2,000,000 rows on a 386.1 MiB
block, the whole-block row is **1.37×** with only the cache and **1.47×** with
the lazy split too: 27% and 32% of the read path, and across the four rows the
medians run 1.37× to 1.75×.

The two are not separable here: every case the harness times ends in
`assert!(hit > 0)`, and the split changes what an open reads only for a query
that renders nothing. Per pass the row ranges 1.14–1.59× and 1.22–2.21×, so the
1.55× median once carried here — **35–39%** of the read path — is **withdrawn**.

### The arithmetic that used to close this closed on a coincidence

It read: 4.58 GB of CRC in 885 ms is 5.2 GB/s, which is what `crc32fast` does
here — therefore the unpruned scan is integrity-check-bound. Warm on this
machine `crc32fast` is nearer **27 GB/s**, so the agreement was luck. Removing
the redundant CRC moves a resident unpruned scan by about **1.1×** — the 5.01
GiB row below, 89.1 ms to 79.8 ms — and not by a constant: **1.23×** on logs and
**1.31×** on traces over 2.93 GiB of plain blocks.

### The unpruned row is bound by what the first column is bound by

Both binaries, same predicate, ~187 K rows per block, back to back:

| Corpus | This binary | 04561ed | Per row | Spread within one binary |
| --- | --- | --- | --- | --- |
| 9.6 GiB, 168 log blocks, 31,170,560 rows | 847 ms | 981 / 1,992 ms | 27–64 ns | **2.6×** |
| 5.01 GiB, 48 log blocks, 9,011,200 rows | **79.8 ms** | 89.1 ms | **8.9 / 9.9 ns** | 1.5× |

On the 9.6 GiB corpus the two arms are **not separable** — one binary against
itself ranged 570 ms to 1,469 ms across five consecutive calls, and what was
timed was eviction: the OS compressor grew by 1.6 GiB during the run. On the
5.01 GiB corpus, ten interleaved samples per arm, the medians separate cleanly
and the per-row cost is **three times lower on the same binaries**. **An
unpruned scan is bound by whether the corpus fits in page cache**, and 18 GiB of
RAM on a machine doing anything else does not hold 9 GiB of it. The 885 ms in
the table is 32.7 ns/row, the upper row's regime.

### How the page-fault term was found

The last row was once **10.1 s** and did not improve on repetition, which ruled
out disk. `mmap` faults 16 KB at a time and `open_table` touches every page
anyway, so the scan took hundreds of thousands of single-page faults with no
readahead; one `madvise(MADV_WILLNEED)` at map time removed them. The 175 ms
once published for that row is withdrawn: eight full-scan runs over 27.1 M rows
land between 0.96 s and 1.6 s, which is what per-block cost times block count
predicts.

## Sorting the match set to keep a hundred of it was the second lever

A block scan produces every matching row, and the merge ordered all of them
before truncating to `limit`. For the query every session opens with — "the last
100 records", no predicate, so *every* row of the block matches — that is an
O(n log n) sort of ~330 K hits to keep 100. `select_nth_unstable` partitions in
linear time and only the surviving head is ordered; on the load harness
([section 3](../internals/e2e.md#3-the-load-harness)) that took the read mix
from 35 to 50 queries/s and `tail` p99 from 230 ms to 139 ms.

### The third lever was the attribute semi-join, and the largest of the three

Section 7.6 replaced a per-row `attr_matches` with a predicate evaluated once
per contiguous parent run, worth **6.7×** on a matching attribute value,
**5.3×** on the unfiltered `limit 100` above (24.4 ms to 4.6), and **3.5×** on a
substring that fills its limit. Across the eight-reader read mix it is 5.1× on
the `attr` class p50 and 4.6× on `errors`, taking the mix from 40 to 50
queries/s. Two classes did not move: `trace` has almost no rows to filter, and
`series` is the metrics route, which read 575–616 ms p50 before and 686–702
after.

### `series` is not on this path at all

`series.rs` is byte-identical across the change, and its only vectorised call
sits inside `q.terms.iter()` — empty for the harness's query, which carries no
`where`. Two binaries differing only in the read path normalise to 43.6, 42.5,
47.6 and 47.5 µs per matched row: differences in both directions, smaller than
one binary's own spread. The mix moved instead — eight closed-loop readers issue
`series` 25% more often once the other classes are five times cheaper.

### Chasing it found a real defect, the same shape as the one above

`collect_attrs` scanned the whole attribute table per parent and runs once per
matched data point, so the metrics path kept the quadratic semi-join section 7.6
removed from the log path — missed because `series_open` loads its tables
directly instead of through `query::Block::open`, so it never saw `Attrs`. It
uses it now, and `series_cost_per_point` prices the result at a flat 0.9–1.1
µs/point where it used to rise with the point count, 31.6 µs at 50 K. On the
load harness it is neutral: there the join is ~7% of the query and twenty-two
blocks × ten tables of `mmap`-and-CRC is the rest. The levers left — pruning
past the directory name, and a block loop that is sequential where `search`
claims helpers from `SPARE` — are larger than a patch.

### What is left is O(bytes) in the block's size, not O(rows)

`scan_cost_per_row` prices that term at 0.047 ns/row, so a 204,800-row block
spends about 10 µs of the 4.6 ms it takes. The cost is O(bytes) in the block's
*size*, paid at open, which means `target_block_bytes` is **not** the lever it
was once written up as. Halving it halves the bytes a block maps and hashes at
open and doubles the block count, so a query that prunes to one block gets
faster and one that prunes to none gets nothing — and the tradeoff runs the
other way for compression ratio and directory size.

## A block cache is still not the next lever

`scan_cost_per_row` puts `open` alone at **56%** of a `body contains` query, the
dearest predicate there is, and `open` plus the CRC at **73%**. But "the term a
cache would attack dominates" argues for attacking the term, not for attacking
it with a cache.

Two changes take it apart without one. The **lazy attribute-table load** never
maps or hashes the tables a query does not read: **30.8% off a logs scan and
46.8% off a traces one**. The **process-scoped verification map** (section 3.3)
removes the repeat hashes on the opens that remain, holding no mappings and
needing no invalidation. Neither pays a resident byte. A real cache would add
only the `mmap` and the two child indexes on top of both, could not help the
first open in a process, and would pay in resident memory — the axis this
section scores worst. It stays on the section 10 list.
