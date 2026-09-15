# 3. Data layout

The central claim: **the in-memory layout and the on-disk layout are the same
bytes.** There is no serialisation step, because Arrow IPC *is* the memory
format with a framing header: `write_table` streams the builders' buffers
straight out, `open_table` maps the file and hands them back.

## 3.1 Schemas

A signal is a **star schema** (OTAP section 6.3), not a flattened
one-row-per-record table.

`logs` (root):

| column | type | note |
| --- | --- | --- |
| `id` | `UInt32` | dense, block-local; the join key |
| `time_unix_nano` | `Timestamp(ns)` | plain; falls back to observed time when unset |
| `observed_time_unix_nano` | `Timestamp(ns)` | nullable |
| `severity_number` | `Int32` | nullable |
| `severity_text` | `Dictionary<UInt16, Utf8>` | nullable; ~24 distinct values in practice |
| `event_name` | `Dictionary<UInt16, Utf8>` | nullable; OTLP logs.proto field 12, what makes a record an Event. A dictionary because an event name is enumerable by definition |
| `body` | `Utf8` | nullable; string bodies, the common case |
| `body_ser` | `Binary` | nullable; anything else, protobuf-encoded; decoded back on read, so nothing is dropped in either direction |
| `trace_id` | `FixedSizeBinary(16)` | nullable |
| `span_id` | `FixedSizeBinary(8)` | nullable |
| `flags` | `UInt32` | nullable |
| `dropped_attributes_count` | `UInt32` | |
| `resource_id`, `scope_id` | `UInt16` | foreign keys, block-local |

`event_name` was added after blocks had been written, and a published block is
never rewritten (section 6), yet the thirteen-column ones still read back: a root
table is never touched positionally, only through `column_by_name` and the
*file's* own schema, so a column costs no format version.

`resources`, one row per distinct resource:

| column | type | note |
| --- | --- | --- |
| `id` | `UInt16` | block-local; what `logs.resource_id` points at |
| `key` | `UInt64` | **stable entity identity**, the cross-block join key (section 7.1) |
| `dropped_attributes_count` | `UInt32` | |

`id` and `key` are deliberately *not* one-to-one: two resources differing only in
a non-identifying attribute get two `id`s and one `key`.

`log_attrs`, `resource_attrs` and `scope_attrs` share **one** schema — one
builder, one reader, one semi-join helper:

| column | type |
| --- | --- |
| `parent_id` | `UInt32` |
| `key` | `Dictionary<UInt16, Utf8>` |
| `type` | `UInt8` — OTAP discriminant, `0..7` |
| `str` | `Dictionary<UInt32, Utf8>` — the only *value* column that is dictionary-encoded |
| `int` / `double` / `bool` / `bytes` / `ser` | `Int64` / `Float64` / `Boolean` / `Binary` / `Binary` |

Exactly one value column is non-null per row; `type` says which. `str` is a
`UInt32` key rather than `key`'s `UInt16` because attribute values are
unbounded, so a `u16` would seal a high-cardinality block every few thousand
rows; the wider index costs 0.7% of compressed bytes.

Two deviations from OTAP, both cheap to reverse:

- `ser` holds the protobuf encoding of the `AnyValue`, not CBOR: we own both
  ends and it costs zero dependencies, so a structured body comes back as the
  JSON it was. Switch to CBOR when a third party needs to read a block.
- `parent_id` is `UInt32` and block-local rather than the wire's per-batch id.
  See section 0.

## 3.2 On disk

```text
<data>/logs/p=<epoch_hour>/<min_ts:020>-<max_ts:020>-<node:08x>-<seq:012>-<wal_hi:020>/
    logs.arrow  log_attrs.arrow  resources.arrow  resource_attrs.arrow  scope_attrs.arrow
    attr.idx  zone.idx  trace.idx
<data>/traces/p=<epoch_hour>/<min_ts:020>-<max_ts:020>-<node:08x>-<seq:012>-<wal_hi:020>/
    spans.arrow  span_attrs.arrow  resources.arrow  resource_attrs.arrow  scope_attrs.arrow
    span_events.arrow  span_event_attrs.arrow  span_links.arrow  span_link_attrs.arrow
    attr.idx  zone.idx  trace.idx
<data>/metrics/p=<epoch_hour>/<min_ts:020>-<max_ts:020>-<node:08x>-<seq:012>-<wal_hi:020>/
    metrics.arrow  metric_attrs.arrow  resources.arrow  resource_attrs.arrow  scope_attrs.arrow
    number_dp.arrow  hist_dp.arrow  hist_bounds.arrow  exp_hist_dp.arrow  summary_dp.arrow
    dp_attrs.arrow  exemplars.arrow  exemplar_attrs.arrow
    attr.idx  zone.idx
<data>/.wal/<node:08x>-<first_seq:020>.wal          # only with ingest.wal on
```

The log is the one thing under `<data>` that is not a block, and the leading dot
keeps a shell glob from sweeping a hidden sibling into a block listing.

`publish` skips an empty table: a traces block normally holds five of the nine
above, a metrics block eight of thirteen. Thirteen is what OTLP's point types
cost — four point tables, because gauge and sum share a column set and the three
others do not, plus `hist_bounds`, which interns the boundary array a
histogram's points repeat.

`wal_hi` is the log position the block covers (section 4); the node id is
section 12.1. `attr.idx`, `zone.idx` and `trace.idx` are **sidecars**:
`(name, bytes)` pairs produced at seal and fsynced inside the block's atomic
rename. They are derived and optional: a reader that does not find one, or finds
a damaged one, falls back to the scan (section 7.4). That is what makes them
safe to add without a format version — a filter that can be wrong is only ever
wrong in the direction of extra work.

### The filesystem is the manifest

Every reason an LSM engine needs a MANIFEST file is absent: one immutable object
per commit, no mutation of a published block, no deletion of live data, no
multi-file atomic operation. The directory name carries the whole pruning key,
so building the catalog at boot is one `readdir` per partition with **zero file
opens**, and no metadata state can disagree with the data.

It also supplies the statistics IPC does not carry: without min/max or a page
index, every temporal query is "scan every batch in every file."

## 3.3 Integrity

arrow-rs never validates the leading `ARROW1` magic and arrow-ipc has no
checksum at all, so a valid footer over a corrupt body decodes into wrong
answers with no error. Mira checks the magic on open and writes a CRC32 of the
record-batch body into the IPC footer's `custom_metadata`, with the exact byte
length it covers. The block stays a standard Arrow IPC file.

Because the CRC proves the bytes are exactly what Mira wrote, the read path sets
`skip_validation(true)`. Without it, every read of a `Utf8` column runs
`std::str::from_utf8` over the whole values buffer, faulting in every page of
string data. `corrupt_body_is_caught_not_returned_as_data` flips one bit mid-body
and asserts the read fails.

### The CRC is per table, and that is what lets a query skip one

Each `.arrow` file carries its own `mira.crc32`, so `block::open_table` verifies
only the file it opens. `Block::open` takes the root table and stops;
`Block::detail` maps the attribute and child tables, and only a predicate that
names an attribute or a block that produced a row calls it — rendering a row
emits its attributes. A block that matches nothing and is asked nothing about
attributes is never hashed past its root: 42.9% of a *plain* logs block and 45.7%
of a plain traces block left unread, 5.5% and 6.3% once the cold tier has
compacted it (section 11).

Every table a query does read is verified in full before a byte of it is returned
(`a_corrupt_attribute_table_is_caught_by_every_query_that_reads_it_and_no_other`).

### Paid once per file per process, not once per open

Right on a first read and pure waste on a second, since a published block never
changes. `open_table` consults a process-scoped map of `path -> (len, mtime,
ino)` before hashing — not the path alone, because `compact` renames a new table
over an existing name.

### Why the key is three fields

Each field covers a change the others cannot see. The length catches a rewrite
of a different size; the mtime catches one of the same size, once the entry has
settled for longer than any filesystem's mtime granularity. The inode catches a
**replacement**, and nothing else does: `cp -p`, `rsync -a` and every backup
agent preserve the source's mtime to the nanosecond, so a block restored from a
copy of itself arrives at a verified path wearing a length and an mtime this
process remembers, carrying bytes it has never hashed.

### What the weaker premise costs

On a second open the premise is weaker — this process proved these bytes
earlier, not the CRC just now — and under `mmap` a clean page can be evicted
after the hash and re-faulted from disk anyway. A restart re-verifies
everything. What it is worth is [section 11](performance.md).

## 3.4 Alignment

Blocks are written at **64-byte** alignment and read with
`require_alignment(true)`. The correctness floor is `align_of::<T>()` — 8 for
`i64`, 16 for the widest thing we store; 64 is the cache-line/SIMD figure, at a
few padding bytes per buffer.

`require_alignment(true)` is a **fail-loud regression guard**, not a correctness
requirement: with it off — the arrow-rs default — a misaligned buffer makes
arrow-rs *silently memcpy the whole body out of the mapping*.
`roundtrip_is_zero_copy_and_prunable` asserts that every buffer pointer, child
data included, lands inside the mapping — if it regresses, Mira memcpies its
working set on every read.

## 3.5 Compression

Hot blocks are **uncompressed**, and this is forced rather than preferred:
`read_buffer` returns a slice of the mmap only when the codec is `None`.
Compression and mmap zero-copy are mutually exclusive; you pick one per tier.

An hour after a block's newest row — `block::COLD_AFTER_NS`, the partition
width, so the boundary is derived rather than configured — the retention sweep
rewrites each of its tables ZSTD-compressed and drops a `cold` marker last, so
the next sweep finishes an interrupted one. The codec is per-batch IPC metadata,
so a directory holding both tiers reads correctly.

The rewrite stages each table beside its target and `rename`s, never truncating
in place: a reader may have the old file mapped, and truncating under a mapping
is a `SIGBUS`. The staged name carries the node id, or two replicas sharing a
volume would collide.

section 11 measures 0.113 of plain size on logs, 0.126 on traces, and reads that
come back *faster* than uncompressed ones.

---
