# Performance — cost per GB, and what compresses

## Cost per GB is 1.20 B/B while a block is hot and 0.14 once it is compacted

164.0 bytes on disk per 136.9-byte wire record, and the steadiest figure in this
section: across fifteen benchmark runs it moved between 1.195 and 1.199. Not the
sidecars either way — every `attr.idx` in a store this size together is a few
tens of kilobytes, and the trace filters are single-digit megabytes against
8.33 GiB. What inflates the hot number is the `ATTRS` table, which carries six
typed value columns and writes all six for every row: a string attribute pays 8
bytes for a null `int`, 8 for a null `double` and 4-byte offsets for null
`bytes`/`ser`, roughly 24 bytes of padding per row.

### Read that figure off blocks, not off `du`

The harness's `storage` line counts the whole data directory, and the
write-ahead log is reclaimed on a 60-second tick (`wal_sweep`, section 4) — so a
30-second benchmark ends before the first reclaim and reports about 299
B/record, or 2.18x, most of it a log that would have been gone a minute later.
The same run with `ingest.wal` off reports 164 B/record and 1.20x directly.

### The padding is almost free to compress, and the cold tier collects it

Measured over every one of the 1,652 tables by `cargo run --release -p
miradb-core --example tier -- <data-dir>` — the actual `write_table_zstd` path
the sweep calls, not a `zstd` CLI estimate.

| | plain | zstd | ratio | lz4 | ratio |
| --- | --- | --- | --- | --- | --- |
| `logs/log_attrs.arrow` | 1,871.8 MiB | 27.9 MiB | **67.1x** | 110.6 MiB | 16.9x |
| `logs/logs.arrow` | 2,497.5 MiB | 465.8 MiB | 5.4x | 714.3 MiB | 3.5x |
| **logs, 137 blocks** | **4,371.0 MiB** | **495.1 MiB** | **8.83x** | 826.2 MiB | 5.3x |
| `traces/span_attrs.arrow` | 1,886.0 MiB | 38.0 MiB | **49.7x** | 111.6 MiB | 16.9x |
| `traces/spans.arrow` | 2,244.1 MiB | 482.7 MiB | 4.7x | 601.8 MiB | 3.7x |
| **traces, 155 blocks** | **4,132.0 MiB** | **522.2 MiB** | **7.91x** | 714.9 MiB | 5.8x |
| **metrics, 24 blocks** | **5.0 MiB** | **1.0 MiB** | **4.87x** | 1.5 MiB | 3.4x |
| **all 1,652 tables** | **8,508.0 MiB** | **1,018.3 MiB** | **8.36x** | 1,542.5 MiB | 5.5x |

1.20 B/B ÷ 8.36 is **0.14 B/B**, comfortably under the 0.35 target. It comes
from the two attribute tables, and the reason is the padding above: a column of
nulls is a run, and `log_attrs` compresses **67.1x** against `logs.arrow`'s
5.4x. The metrics ratio is worse because that corpus is 5.0 MiB, too small for
per-buffer framing to disappear into the payload.

### Six sealers per signal did not move any of this

137 log blocks compress to 8.83x where 87 larger ones compressed to 8.84x. A
block is smaller, but a run of nulls in an `ATTRS` column is a run at either
size.

Compression runs at **634 MiB/s** zstd and **777 MiB/s** lz4 on one core — over
the whole 8,508 MiB, 13.4 CPU-seconds and 11.0, read and write included: the
rewrite path, not the codec in isolation. These two were taken on a box that had
been running benchmarks all day; an earlier quiet pass over a smaller corpus
read 808 and 976. A 32 MiB block is therefore ~40 ms, on a path
off ingest entirely: the retention sweep, inside `spawn_blocking`, an hour after
the data landed. The `MAX_COMPACT_PER_SWEEP` cap of 8 blocks a minute exists for
the first pass over an existing volume, not for the steady state.

### The open question from the previous revision is answered

The worry was that inflating a compressed buffer into the heap would cost more
latency than the pages it saves. `tier` now times the read back too. Over all
1,652 tables of this corpus: **9.3 s plain, 10.4 s zstd, 14.4 s lz4** — zstd is
1.11× the plain read, and three passes over the previous, smaller corpus bracket
that at 1.00× to 1.38×.

These are **page-cache-warm**: the file was written microseconds before it was
read, and warm favours plain, because a resident plain block has nothing to
fault while a compressed one has to inflate. Even so the two are within
run-to-run noise: the inflate is real, paid back by touching 8.4× fewer bytes.
Cold — the case that matters, since a block is an hour old before it is
compacted — the arithmetic runs further the same way; an earlier measurement
over a smaller corpus read 0.68 s plain against 0.54 s zstd on logs and 0.58 s
against 0.26 s on traces. Not reproducible from here: `tier` cannot drop this
machine's page cache.

So the cold tier costs the read path nothing measurable — only the zero-copy
property, an allocation cost rather than a latency one. The threshold stays at
one hour, section 3.5's partition width and therefore not a knob; nothing here
argues for waiting longer.

### LZ4 is the one clear loser, which settles a standing question

It is the pure-Rust alternative, and dropping `zstd-sys` would drop the only C
dependency in the tree — but it compresses 5.52× against zstd's 8.36× *and*
reads back slower in every pass. It costs on both axes, so `zstd-sys` stays.

## What compresses and what does not

`schema.rs` makes two encoding choices that look arbitrary, and three more that
are invisible because they are things it does *not* do. All five were measured
on real blocks from the corpus above at `ZSTD_LEVEL = 3`, one table at a time so
the effect is not diluted by the rest of the block.

### `attrs.str` as `dictionary<u32, utf8>` — kept

On one `log_attrs.arrow` of 393,216 rows, the same table written with `str` as
plain `Utf8` compresses **12.9x**; as shipped it compresses **66.2x**. The
dictionary also shrinks the *uncompressed* table, 16.7 MB to 14.1 MB, so it pays
before the codec runs. Attribute values are where the repetition in telemetry
lives, and a dictionary says so explicitly instead of hoping a 128 KiB zstd
window rediscovers it per buffer.

### `u32` for that dictionary's indices, not `u16` — kept, and nearly free

The same table at each index width: 212,290 compressed bytes at `u32`,
210,810 at `u16`, 209,442 at `u8`. Going from `u16` to `u32` costs **0.7% of
compressed bytes** and removes a whole failure class — `key` is `u16` and so
needs `DICT_CAP` and a seal-early rule to stay under it, and values, unlike
keys, are unbounded in principle.

### Dictionary-encoding the other string columns — rejected, it is worse

`logs.arrow` compresses 5.23x as shipped and **4.72x** with `body`
dictionary-encoded on top. `spans.arrow` is 4.56x either way with
`status_message` encoded. Attribute values repeat within a column; a log body is
mostly novel per row, and paying dictionary overhead for a dictionary that never
hits is a straight loss.

### Sorting a block by a low-cardinality column before sealing — rejected

The standard columnar trick, and every key tried came out at or below the
unsorted ratio. Like with like, dictionaries decoded so the sort is not fighting
the encoding:

| table | unsorted | best sort key tried | worst |
| --- | --- | --- | --- |
| `logs.arrow` | 4.93x | `resource_id` 5.00x | `body` 4.52x |
| `spans.arrow` | 4.54x | `resource_id` 4.55x | `duration_nano` 4.29x |
| `log_attrs.arrow` | 9.28x | `key` 8.32x | `str` 6.98x |

Arrival order is already sorted by time, and time carries the locality:
consecutive rows come from the same handful of live resources, scopes and
routes. Re-sorting scatters that, and costs the block the physical property that
matters: `min_ts`/`max_ts` bounding a contiguous range, which is what section
3.2's pruning reads.

### ZSTD level 9 — rejected

Per table, level 3 against level 9: `logs.arrow`
5.23x to 5.36x for **6.9x the time**; `spans.arrow` 4.56x to 4.55x — *worse* —
for 3.9x; `log_attrs.arrow` 66.2x to 69.2x for 3.7x. A few percent of disk for
several times the CPU, on a sweep that shares cores with ingest, is a bad trade
on an axis principle 1 also scores.

One caveat on all five: this corpus comes from the OTLP load generator, so
cardinalities are low — 2 severities, 16 resources, 91 distinct bodies. Low
cardinality is the case *most* favourable to both dictionary encoding and
sorting, and three of the five still came out negative; a production corpus
would move the ratios, not flip a decision that already loses on the friendly
input.

Two invariants guard the design rather than the numbers, and both are tests: n/n
buffers zero-copy on read of a hot block, and a corrupted body never returns as
data. The cold tier is held to the second and deliberately not the
first — `compaction_shrinks_aged_blocks_without_changing_what_they_answer`
asserts a compacted block gives up zero-copy while the hot block beside it keeps
it.

The honest headline for the README is **"zero-copy queries over immutable Arrow
blocks, allocation-lean OTLP ingest."** Not "zero-copy ingestion" — that claim
does not survive anyone reading `prost`.

---
