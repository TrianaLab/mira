# Mira — Architecture

**Status:** proposal for review. Nothing is committed. The workspace under
`crates/` compiles and its tests pass; treat it as an executable sketch of this
document, not as v1.

---

## 0. Corrections to the original brief

The brief specified several mechanisms by name. Six of them do not survive
contact with the formats involved. They are listed first because they change the
shape of everything below, and because the reasons are not obvious from the
outside.

| Brief said | What is actually true | What Mira does instead |
|---|---|---|
| **Delta-of-delta timestamps** | Arrow IPC has no per-column encodings. Its entire encoding surface is whole-buffer LZ4/ZSTD plus the Dictionary and RunEndEncoded layouts. Implementing DoD means inventing a buffer layout no Arrow reader understands — which forfeits zero-copy, since you must decode into a fresh allocation. Separately, Gorilla's 12× rests on samples landing on exact interval boundaries; OTLP `time_unix_nano` is a wall-clock read with 10⁵–10⁷ ns of jitter between consecutive deltas. | Plain `Timestamp(Nanosecond)`. Sort by time within a block; take the size win at the cold tier from a generic compressor. |
| **Resource/Scope dedup at block headers** | The Arrow mechanism for a "block header" is `Schema.custom_metadata`, exactly one map per file. That expresses dedup only if a block holds exactly one Resource. A block from any multi-tenant collector holds hundreds. | OTAP §6.3: `resource_id`/`scope_id` `UInt16` columns in the root table plus separate attribute tables keyed by `parent_id`. A resource with 40 attributes shared by 10,000 records costs 40 rows and 10,000 `u16`s. |
| **Dictionary-encode high-cardinality maps** | Backwards. Dictionary encoding is a *low*-cardinality technique with a hard ceiling — `Dictionary<UInt16,_>` raises `DictionaryKeyOverflowError` past 65,536 distinct values. `http.url` and `trace_id` are the highest-cardinality data in the system. | Dictionary-encode only enumerable columns: attribute *keys*, `severity_text`. Attribute *values* are plain `Utf8`/`Binary`. |
| **Zero-copy ingestion** | Impossible on the OTLP path. `prost` memcpies every string unconditionally; varints must be decoded. Even on OTAP, `StreamDecoder` only avoids a copy when the whole message body is one contiguous `Buffer`, and an HTTP/2 body split across DATA frames is `extend_from_slice`'d. | Say **zero-copy queries**, not zero-copy ingestion. The ingest goal is *allocation-lean*: one unavoidable memcpy of the request body, then no per-field heap allocation. |
| **Lock-free ring buffer for ingestion** | Cargo-culted from LMAX, where an item is a 150 ns order struct. Here an item is an export request costing 10⁵–10⁶ ns to decode and encode, arriving 10²–10⁴ times per second. The queue is four orders of magnitude from being the bottleneck, and a lock-free queue cannot express backpressure. | A bounded `tokio::sync::mpsc` per shard. `send().await` propagates backpressure out as HTTP/2 flow control. Revisit if a queue ever appears in a profile. |
| **4317 and 4318 both served by tonic** | 4318 is not gRPC. Per the OTLP spec it is plain HTTP/1.1 POST of protobuf or JSON to `/v1/{traces,metrics,logs}`. | Two listeners: tonic on 4317, axum on 4318. axum is already in the tree via tonic's `router` feature, so it costs no dependency. |
| **A reflective proto3-JSON decoder for OTLP/HTTP JSON** | OTLP JSON is *not* canonical proto3 JSON. Ids are hex where every other `bytes` field is base64 — and a 32-character hex string is itself valid base64, so a generic decoder does not fail, it silently yields 24 bytes of nonsense for every `trace_id`. 64-bit integers are strings. Field names may be either dialect within one document. | `crates/mira/src/json.rs`: a hand-written decoder over the YAML 1.2 loader already in the tree (`api::parse`; YAML 1.2 is a superset of JSON, so KYAML bodies work for free — §1). No new dependency, and the two deviations are handled where they occur rather than configured around. |

Three more, less structural but worth stating:

- **`partial_success` is not a backpressure signal.** The OTLP spec says the
  client MUST NOT retry a partial success. Reporting overload that way
  permanently destroys the data and records the failure as the sender's fault.
  Overload is always a status code — `UNAVAILABLE` or `RESOURCE_EXHAUSTED` —
  and always with `google.rpc.RetryInfo` attached, because
  `grpc-retry-pushback-ms` is only honoured by clients that configured a gRPC
  retry policy, which OTLP exporters do not.
- **OTAP is not the default path.** No OpenTelemetry language SDK emits OTAP.
  The only production implementations are the Go `otelarrowreceiver`/`exporter`
  in collector-contrib. OTLP on 4317/4318 is the universal path; OTAP is a
  collector-tier bandwidth optimisation worth roughly 2× over OTLP+zstd. Mira
  adopts the OTAP *data model* as its storage layout from day one, and will add
  the OTAP *wire protocol* as a second receiver — but the architecture must not
  assume OTAP is how data arrives.
- **The cold tier is worth one C dependency.** `zstd-sys` vendors its own source
  and builds it with `cc`, which is a real dent in "nothing to install to build
  Mira" — though a linker was already required, so the practical delta is a
  vendored C compile, not a new prerequisite. The pure-Rust alternative is
  LZ4_FRAME via `lz4_flex`, which Arrow IPC supports and which would have kept
  the tree C-free. Measured against each other on the same blocks (§11), LZ4
  also clears the 0.35 target — 0.221 on logs, 0.200 on traces — so this was not
  the walkover it looked like. ZSTD wins on both axes at once: 1.7× smaller
  *and* faster to write on traces (989 vs 534 MiB/s), because LZ4's speed
  advantage is a decompression property and the compress side of the ratio it is
  being asked for is not where it is strong. 3 crates and 0.5 MB of binary for
  another 1.7× on disk, with no throughput given up, is worth the `cc`.

### One correctness hazard worth naming on its own

OTAP `id` and `parent_id` values are unique only **within a single
`BatchArrowRecords`**. Persisting them verbatim and then joining
`logs.id = log_attrs.parent_id` across batches produces a silent cross-product —
wrong answers, no error. Mira rebases every id into a dense, block-local
`UInt32` at ingest. That makes any join *inside* a block unconditionally correct
and removes the need for a partition-discriminant column on every table and an
extra predicate on every join.

---

## 1. Principles, and the mechanism each one buys

The five principles are constraints, not aspirations. Each needs a mechanism or
it is decoration.

**Performance is the product.** The four axes — ingest throughput per core,
resident footprint, query p99, cost per GB — conflict pairwise. Compression cuts
cost per GB and raises query latency. Large blocks raise throughput and raise
footprint. Mira resolves them by **tiering**, not by claiming all four at once:
hot blocks are uncompressed, 64-byte aligned and mmapped; cold blocks get
compression and give up zero-copy. §11 states the target for each axis and how
it is measured.

**Agentic**, in all four senses the owner selected:
- *LLM-queryable surface* — a native MCP server (`rmcp`) over the same query
  engine, so an agent investigating an incident issues one call instead of
  composing PromQL and TraceQL. Block footers will carry sketches (HLL, t-digest,
  top-K) specifically so exploratory "what is unusual here" queries are answerable
  without a scan.
- *Telemetry for AI workloads* — OTel GenAI semantic conventions as a first-class
  case. Concretely this means the attribute table must handle multi-kilobyte
  prompt/completion strings without pathology, which is exactly why attribute
  values are plain `Utf8` and not dictionary keys.
- *Self-driving* — no tuning knobs. Block size, flush cadence and memory ceiling
  adapt to observed load. There **is** a config file (`docs/CONFIG.md`), and it
  is not a contradiction: it describes *where the process runs* — addresses, data
  directory, retention policy, replica name, peers — and contains no value that
  affects how the engine performs. The boundary is structural, not documentary.
  `pipeline::Config` holds `target_block_bytes` and `max_block_age`, the two
  numbers an operator would most want to tune, and there is no path from the YAML
  to either.
- *Agent-based internals* — the shard tasks, flusher and retention worker are a
  supervised message-passing mesh already. This is a description of the design,
  not a licence to build an actor framework.

**OTLP-first.** The Arrow schemas in `crates/mira-core/src/schema.rs` *are* the
OTLP Resource-Scope-Signal model. There is no transformation step to a generic
relational or inverted-index store, and therefore no place for one to lose
fidelity.

**Single binary, no operational overhead, stateless.** Stateless means *no
coordination state*: no cluster membership, no Raft, no external metadata store.
The concrete mechanism is in §3.2 — the filesystem is the manifest. This also
rules out DataFusion: it would give SQL for free at a cost of 47 direct
dependencies, a ~1.5M SLoC transitive tree and a 68–92 MB binary. Mira
hand-rolls the ~2,000 LOC of query logic it needs. The skeleton as it stands is
**2.6 MB stripped, 107 crates** — that will grow as traces, metrics, query and
MCP land, but it sets the scale the design is defending.

**KYAML-first, everywhere.** Every text format Mira reads or writes — the config
file, dashboard definitions, saved queries, MCP examples, anything added later —
is KYAML: a strict subset of YAML 1.2 with collections written explicitly as `{}`
and `[]`, every string double-quoted, and indentation carrying no meaning.

The mechanism, without which this is a style guide: **the parser refuses
unquoted scalars.** Every value in a Mira config is a string, so a value that
arrives as any other type is a startup error naming the key and saying to quote
it (`config.rs::scalar`). Nothing is coerced back with `to_string()`, because
that is the step that turns `0x1f` into `31` and `False` into `false` with no
diagnostic — and `node` is hashed into every block directory name, so a silently
altered string is a replica writing somewhere nobody expects.

The reason this is a principle and not a preference is the one stated when it was
chosen: it is for the model. An unquoted YAML scalar's type is decided by a
resolution table that varies across YAML 1.1 and 1.2 and across implementations,
so the same document means different things to different readers. A human
usually notices; a model generating config has no feedback loop and will emit the
majority-spelling from its training data, which is YAML 1.1. Quoting removes the
decision instead of hoping it goes the right way. The cost is two characters and
some visual noise. The benefit is that generated config is unambiguous by
construction, which is the precondition for anything else agentic touching it.

Trailing commas are allowed for the same reason — appending a key should not mean
editing the line above it, which is exactly the diff-shaped mistake a generator
makes. That YAML 1.2 permits them in flow collections is verified against our
parser in `config.rs`'s tests, not assumed from the spec.

---

## 2. Workspace

```
mira/
├── Cargo.toml                  # workspace, one pinned Arrow version
├── docs/ARCHITECTURE.md
└── crates/
    ├── mira-proto/             # vendored .proto + codegen. No hand-written code.
    │   ├── build.rs            # protox (pure Rust) -> tonic-prost-build
    │   └── proto/opentelemetry/...
    ├── mira-core/              # the engine, as a library
    │   ├── schema.rs           # Arrow schemas == on-disk layout
    │   ├── logs.rs             # OTLP -> Arrow
    │   ├── block.rs            # publish, scan, expire, mmap read
    │   └── error.rs
    └── mira/                   # the binary
        ├── main.rs             # flags, listeners, supervision
        ├── receiver.rs         # tonic on 4317, axum on 4318
        └── pipeline.rs         # channel, flusher, retention worker
```

**Three crates, not the five in the brief.** Each boundary here pays rent:
`mira-proto` isolates codegen and the protox blast radius; `mira-core` is what
benchmarks and integration tests link against; `mira` is a thin binary. A
`mira-storage`/`mira-core` split of zero LOC would buy no build parallelism —
cargo already parallelises codegen units within a crate — and would force the
public API boundary to be frozen before anyone knows where it belongs. Split
`mira-storage` out the first time someone needs the block format without the
OTLP encoder. `mira-cli` is `main.rs` until the flag set outgrows twenty lines.

### Why the protos are vendored

The `opentelemetry-proto` crate is the obvious choice and it is the wrong one,
for two independent reasons:

1. Its codegen does not call `prost_build::Config::bytes(["."])`, so every
   `trace_id`, `span_id` and `AnyValue::BytesValue` decodes as a fresh `Vec<u8>`.
   That is one heap allocation per field per record on the hottest path.
2. It declares `opentelemetry` and `opentelemetry_sdk` as **non-optional**
   dependencies — `src/proto.rs` re-exports from `transform::common`, which uses
   them — so they cannot be feature-gated away. Measured cost: +12 crates,
   including a full SDK and `rand`, even with `--no-default-features`.

Vendoring costs 1,725 lines of `.proto` and a 30-line `build.rs`, and `protox`
compiles them in pure Rust, so building Mira never needs a `protoc` on `PATH`.
It is also unavoidable anyway the moment OTAP is in scope: the
`ArrowTracesService`/`ArrowLogsService` definitions are not in the
`opentelemetry-proto` crate's codegen input list.

### Arrow version pin

One Arrow version across the workspace, declared once in
`[workspace.dependencies]`. Two majors in one graph means
`arrow_58::RecordBatch` and `arrow_59::RecordBatch` are different types and the
resulting error is unreadable. This is also the reason Mira does not depend on
`otel-arrow-dfe-quiver` — see §10.

---

## 3. Data layout

The central claim: **the in-memory layout and the on-disk layout are the same
bytes.** There is no serialisation step, because Arrow IPC *is* the memory
format with a framing header. `write_table` streams the builders' buffers
straight out; `open_table` maps the file and hands the same buffers back.

### 3.1 Schemas

A signal is a **star schema**, following OTAP §6.3 rather than a flattened
one-row-per-record table.

`logs` (root):

| column | type | note |
|---|---|---|
| `id` | `UInt32` | dense, block-local; the join key |
| `time_unix_nano` | `Timestamp(ns)` | plain; falls back to observed time when unset |
| `observed_time_unix_nano` | `Timestamp(ns)` | nullable |
| `severity_number` | `Int32` | |
| `severity_text` | `Dictionary<UInt16, Utf8>` | ~24 distinct values in practice |
| `body` | `Utf8` | string bodies, the common case |
| `body_ser` | `Binary` | anything else, encoded; nothing is dropped |
| `trace_id` | `FixedSizeBinary(16)` | nullable |
| `span_id` | `FixedSizeBinary(8)` | nullable |
| `flags`, `dropped_attributes_count` | `UInt32` | |
| `resource_id`, `scope_id` | `UInt16` | foreign keys, block-local |

`resources` — one row per distinct resource, so tens of rows against hundreds of
thousands in the root table:

| column | type | note |
|---|---|---|
| `id` | `UInt16` | block-local; what `logs.resource_id` points at |
| `key` | `UInt64` | **stable entity identity**, the cross-block join key (§7.1) |
| `dropped_attributes_count` | `UInt32` | |

`id` and `key` are deliberately *not* one-to-one: two resources whose attribute
sets differ only in a non-identifying attribute get two `id`s and one `key`.

`log_attrs` / `resource_attrs` / `scope_attrs` share **one** schema, so a single
builder, a single reader and a single semi-join helper cover all three levels:

| column | type |
|---|---|
| `parent_id` | `UInt32` |
| `key` | `Dictionary<UInt16, Utf8>` |
| `type` | `UInt8` — OTAP discriminant, `0..7` |
| `str` / `int` / `double` / `bool` / `bytes` / `ser` | `Utf8` / `Int64` / `Float64` / `Boolean` / `Binary` / `Binary` |

Exactly one value column is non-null per row; `type` says which. The other five
cost one validity bit each.

Two deliberate deviations from OTAP, both cheap to reverse:
- `ser` holds the protobuf encoding of the `AnyValue`, not CBOR. We own both
  ends, and it costs zero dependencies. Switch to CBOR when a third party needs
  to read a block.
- `parent_id` is `UInt32` and block-local rather than the wire's per-batch id.
  See §0.

### 3.2 On disk

```
<data>/logs/p=<epoch_hour>/<min_ts:020>-<max_ts:020>-<node:08x>-<seq:012>/
    logs.arrow  log_attrs.arrow  resources.arrow  resource_attrs.arrow  scope_attrs.arrow
    attr.idx
<data>/traces/p=<epoch_hour>/<min_ts:020>-<max_ts:020>-<node:08x>-<seq:012>/
    spans.arrow  span_attrs.arrow  resources.arrow  resource_attrs.arrow  scope_attrs.arrow
    span_events.arrow  span_event_attrs.arrow  span_links.arrow  span_link_attrs.arrow
    attr.idx  trace.idx
```

The node id is §12.1. `attr.idx` and `trace.idx` are **sidecars** — `(name,
bytes)` pairs produced at seal, written and fsynced by `publish` next to the
Arrow tables and inside the same atomic rename. Sidecars are always derived and
always optional: a reader that does not find one, or finds a damaged one, falls
back to the scan it would have done anyway (§7.4). That is what makes them safe
to add without a format version — and it is the only reason a filter is allowed
on this path at all, because a filter that can be wrong must only ever be wrong
in the direction of extra work.

**The filesystem is the manifest.** Every reason an LSM engine needs a MANIFEST
file is absent: Mira publishes exactly one immutable object per commit, never
mutates a published block, never deletes live data, and has no multi-file atomic
operation. The directory name carries the whole pruning key, so building the
catalog at boot is one `readdir` per partition with **zero file opens** — and
there is no metadata state that can disagree with the data. That is what makes
the process stateless in the sense that matters: kill it, restart it, or point a
second read-only process at the same directory; there is nothing to reconcile.

This also supplies the thing the Arrow IPC format itself does not have. IPC
carries **no statistics** — no per-chunk min/max, no null counts, no page index —
so without an external index every temporal query degrades to "scan every batch
in every file." Putting the time range in the directory name is the whole index.

### 3.3 Integrity

arrow-rs's own reader seeks straight to the trailer and never validates the
leading `ARROW1` magic, and arrow-ipc has no checksum at all, so a valid footer
over a corrupt body decodes into wrong answers with no error. Mira adds both:

- `ARROW1` magic is checked on open.
- A CRC32 of the record-batch body, with the exact byte length it covers, is
  written into the IPC footer's `custom_metadata`. (The length is stored rather
  than derived, because `finish()` emits an end-of-stream marker between the last
  batch and the footer.) The block stays a standard Arrow IPC file that any
  Arrow reader can open; the checksum is additive.

Because the CRC proves the bytes are exactly what Mira wrote, the read path sets
`skip_validation(true)`. Without it, every read of a `Utf8` column runs
`std::str::from_utf8` over the entire values buffer — a full sequential scan that
faults in every page of string data, defeating the point of demand paging.

`corrupt_body_is_caught_not_returned_as_data` in `crates/mira-core/src/lib.rs`
flips one bit mid-body and asserts the read fails.

### 3.4 Alignment

Blocks are written at **64-byte** alignment and read with
`require_alignment(true)`.

The correctness floor is `align_of::<T>()` — 8 for `i64`, 16 for the widest thing
we store. 64 is the cache-line/SIMD figure and costs a few padding bytes per
buffer. `require_alignment(true)` is not there for correctness; it is a
**fail-loud regression guard**. With it off (the arrow-rs default) a misaligned
buffer causes arrow-rs to *silently allocate and memcpy the whole body out of the
mapping* — turning a zero-copy read into a full copy with no signal at all.

`roundtrip_is_zero_copy_and_prunable` walks every buffer of every column,
including child data, and asserts each one's pointer lands inside the mapping.
It currently reports n/n across `Int64`, `Utf8`, `Dictionary`,
`FixedSizeBinary(16)` and `Binary`. This test is the single most load-bearing
thing in the repo: if it ever regresses, Mira has quietly become a store that
memcpies its working set on every read.

### 3.5 Compression

Hot blocks are **uncompressed**, and this is forced, not preferred. Arrow IPC
body compression makes the reader decompress into fresh allocations —
`read_buffer` returns a slice of the mmap when the codec is `None` and calls
`decompress_to_buffer` when it is not. There is no in-place path. Compression and
mmap zero-copy are mutually exclusive; you pick one per tier.

So there are two tiers of the same format. An hour after a block's newest row —
`block::COLD_AFTER_NS`, which is the partition width, so the boundary is derived
rather than configured — the retention sweep rewrites each of its tables
ZSTD-compressed and drops a `cold` marker in the directory. Nothing else
changes: the codec is per-batch IPC metadata, so the reader needs no flag and a
directory holding both tiers reads correctly, which it does whenever a rewrite
is interrupted. The marker is written last, so an interrupted block has no
marker and the next sweep finishes it.

Two constraints shape the rewrite. It stages each table beside its target and
`rename`s, never truncating in place: a reader may have the old file mapped, and
truncating under a mapping is a `SIGBUS` on the next page touched, not an error
anyone can catch. Unlinking by rename is the same guarantee `expire` already
depends on. And the staged name carries the node id, because two replicas
sharing a volume both see the block go cold and would otherwise write the same
temporary file.

§11 has the measurement: 0.127 of plain size on logs, 0.142 on traces, and reads
that come back *faster* than uncompressed ones.

---

## 4. Ingest path

```
gRPC 4317 (tonic) ─┐
                   ├─> Ingest::submit ─> mpsc(128) ─> flusher ─> spawn_blocking ─> publish
HTTP 4318 (axum) ──┘         │                                        │
                             └────────── oneshot ack ─────────────────┘
```

`submit` uses `try_reserve`, not `send().await`: shedding *before* the decode
work is the difference between a fast NACK and an unbounded latency tail. A full
queue returns `UNAVAILABLE` with `RetryInfo(250ms)`.

The export is acknowledged **only after the block directory rename is durable**.
OTLP's retryable status set — plus "if the server disconnects without returning a
response, the client SHOULD retry" — covers exports in flight at a crash. Acking
earlier is the one window in which data is lost while the client believes it was
stored. Because acknowledgement latency would otherwise be bounded by the
caller's own traffic, `max_block_age` (2 s) is a first-class flush trigger
alongside size; the smoke test measures a 2.03 s round trip for a single-record
export, which is exactly that bound.

**No WAL.** Publish is write-tmp → fsync files → fsync tmpdir → rename dir →
fsync parent. Directory rename is atomic on POSIX, so a block is either wholly
visible or wholly absent. There is no torn state, therefore nothing for recovery
to replay. Crash recovery is `scan()` — the same `readdir` the read path already
does — and the sequence counter resumes from the highest published block.

**A note on sharding.** The design is one shard per listener/core
(`SO_REUSEPORT`), not sharding by resource hash. Resource cardinality in real
fleets is bimodal — a handful of huge resources carrying 90% of volume, plus a
long near-idle tail — so hash sharding gives a permanently hot shard *and* a
small-file explosion in the tail. Files per flush interval should be a function
of core count, known at startup, not of the customer's topology. The skeleton
runs a single shard; the boundary is already in the right place.

**Decoder affinity, when OTAP lands.** OTAP §4.4 mandates decoder state per
(gRPC stream, payload_type, schema_id), strictly ordered. That is
connection-affine by construction. The OTAP receiver will decode on the
per-connection task and push `Arc`'d Arrow buffers onward — which is another
reason the "one global lock-free ring buffer" shape was wrong.

---

## 5. Flusher state machine

One task, one open block, three transitions:

```
        ┌──────────────── recv_many(≤64) ────────────────┐
        v                                                │
    [ OPEN ] ──append──> [ ACCUMULATING ] ───────────────┘
        │                      │
        │                      ├─ approx_bytes ≥ target (32 MB) ─┐
        │                      ├─ age ≥ max_block_age (2 s) ─────┤
        │                      ├─ no dictionary headroom ────────┤
        │                      └─ channel closed ────────────────┤
        v                                                        v
    [ IDLE ]                                            [ SEALING ]
   (timer pushed out)                                            │
                                        builder.finish() → 5 RecordBatches
                                                                 │
                                              spawn_blocking: publish()
                                                                 │
                                                  ack every waiter, reset
```

Five details that are easy to get wrong:

- The age clock starts when the **first job of a block** arrives, not at the last
  flush, so latency is bounded from when data actually showed up.
- The timer must not reset its own deadline before the age check reads it. (It
  did in the first draft; the symptom was a block that never flushed and a client
  that hung forever.)
- When idle with no waiters, the deadline is pushed forward so a stale deadline
  does not spin the loop.
- **A `UInt16` dictionary filling up is a seal trigger, not an error.** The check
  is `has_headroom_for(&req)` *before* the append and is deliberately
  conservative — it assumes every key in the request is new — because an Arrow
  builder cannot be rolled back, so an overflow discovered mid-append would leave
  a half-written row that fails `RecordBatch::try_new` at every subsequent flush.
  Deferred requests go into the next block and keep arrival order. Overflow costs
  a slightly small block; it never costs a caller its data.
- **A failed `finish()` replaces the builder.** `finish` resets column builders as
  it goes, so a failure part way through leaves one that can never seal again. The
  first version kept it, which turned any single flush error into a node that
  rejected everything until restarted.

Accumulation lives in Arrow's typed builders, not in a `Vec<RecordBatch>`
concatenated at flush. `RecordBatch` is immutable and has no append, and
`concat_batches` costs roughly 2× peak memory for the duration of the concat —
which is a footprint regression on one of the four axes.

**Blocking work never runs on a runtime worker.** `publish` fsyncs; a cold mmap
read takes a hard page fault that stalls the entire OS thread with no yield point
and no signal to tokio. Both go through `spawn_blocking`.

---

## 6. Retention worker

A 60-second tick that computes `now - ttl`, calls `scan()`, and `remove_dir_all`s
every block whose `max_ts` is older. That is the whole worker.

TTL is a directory unlink, not a compaction: there is no read-modify-write of
live data, so retention costs no IO bandwidth and cannot interfere with ingest.

**No reader lease protocol is needed**, and this is a real result rather than an
optimism. POSIX specifies that `mmap()` adds a reference to the file that
`close()` does not remove, and that the reference persists until the last
mapping goes away. A query holding an `Arc<Mmap>` therefore keeps reading correct
data out of an unlinked file. The `Arc` *is* the refcount and the kernel holds
the inode. The one rule: never truncate or rewrite a published block — that
gives readers `SIGBUS`, whereas unlinking does not.

---

## 7. Correlation

Correlation is the headline feature, so it gets designed rather than assumed.

**What everyone else ships is a client-side join across two databases.** Grafana's
trace-to-logs is a datasource config: the operator hand-writes a mapping from
span attributes to a LogQL query, and clicking a span fires a second query at a
different system. Two stores, two query languages, two retention windows, two
clocks, one YAML file that has to stay correct as the semantic conventions move
under it. It fails *open*: when a log line carries no `trace_id` the panel comes
back empty and the user concludes there were no logs. Most logs in the wild carry
no `trace_id`.

Mira's structural advantage is that there is nothing to join *across*. Every
signal for a time window is in the same block, behind the same resource table,
in the same sort order, reachable by the same engine. That makes correlation a
storage-layer primitive, and it makes available an axis that a cross-system join
cannot have at all: **entity identity**.

### 7.1 The join keys, in order of precision

| key | applies when | mechanism |
|---|---|---|
| `trace_id` | the record was traced | exact; §7.4 |
| `span_id` / parent | the record names a span | exact |
| span link | async or fan-in causality | `span_links` table (with traces) |
| exemplar | a metric datapoint sampled a trace | exemplar `trace_id` (with metrics) |
| **entity + time** | **always** | `resources.key`, §7.2 |

The ladder matters more than any single rung. An investigation that starts at an
untraced error log gets nothing from the first four, and *everything* from the
fifth. Degrading from "the exact trace" to "everything this pod emitted in the
surrounding five seconds" is the difference between a correlation feature and a
correlation demo.

### 7.2 Entity identity — `resources.key`

`resource_id` is block-local by design (§0), so it cannot be the cross-block join
key. The obvious substitute — equality of the resource's attribute set — is
wrong, and wrong in the worst way. A pod that starts reporting one extra
attribute mid-hour becomes two entities, and every "show me everything from this
pod" answer silently returns a plausible subset. The query succeeds. Nobody
notices.

So identity is a 64-bit hash over only the attributes OTel semconv defines as
*identifying*, at the most specific level present:

```
service.name + service.instance.id (+ service.namespace)
k8s.pod.uid (+ k8s.container.name)
container.id
host.id | host.name (+ process.pid)
service.name (+ service.namespace)
otherwise: NO_IDENTITY (0)
```

First match wins; the candidate index is folded into the hash so `host.id="abc"`
and `container.id="abc"` cannot collide. The list is **fixed, not configurable** —
an identity rule two operators can set differently is not an identity rule. See
`identity.rs`; the drift-and-reorder case is a test, because it is the failure
that would be invisible in production.

The last rung is a sentinel and not a hash, and that is deliberate. Hashing
whatever attributes happen to be present reproduces the exact bug this section
opens with: the hash changes when an attribute is added, the entity forks, and
the answer is a plausible subset. `key = 0` means *this resource has no stable
identity*, and the entity expander (§7.3) must refuse it with an error naming the
attribute that would fix it — `service.instance.id`, `k8s.pod.uid`,
`container.id` or `host.id`. A refusal is actionable; a plausible subset is not.
Every OTel SDK sets `service.name`, so the sentinel means an exporter with
resource detection switched off, which is worth telling the operator about.

The physical consequence is the interesting part. Resolving *"all signals from
this entity"* inside a block is: read `resources.arrow` (tens of rows, one page),
match `key`, build a 65536-bit `resource_id` bitset — 8 KB, branch-free, one load
and one test per root row. A flattened store carrying `ResourceAttributes
Map(String,String)` on every row pays a map probe per row over the entire scan
for the same question. This is the OTAP star schema paying for itself, and it is
the one place Mira's layout produces an asymptotic advantage rather than a
constant-factor one.

### 7.3 The frame algebra

A **frame** is a bounded region of telemetry:

```
Frame {
    time:     [from, to)      // nanoseconds
    entities: {u64}           // resource keys;  empty = unconstrained
    traces:   {[u8;16]}       //                 empty = unconstrained
    spans:    {[u8;8]}        //                 empty = unconstrained
}
```

Every correlation operation is `Frame → Frame`. Nothing else exists. That closure
property is the whole design: an investigation is a walk over frames, every
intermediate state is a legal query, and there is no way to construct something
that is not executable.

| expander | reads | writes |
|---|---|---|
| `by_trace` | `traces` from matched rows | widens `traces`, widens `time` to the traces' own extent |
| `by_span` | `spans`, parent/child | widens `spans` |
| `by_link` | `span_links` | widens `traces` |
| `by_exemplar` | metric exemplars | widens `traces` |
| `by_entity` | `resource_id → resources.key` | widens `entities` |
| `around(d)` | — | widens `time` by ±d, keeps `entities` |
| `peers` | `traces → resource_id → key` | widens `entities` to everything that shared a trace |

`peers` is the one worth calling out: it answers *"which other services were
involved in the traces this pod took part in, in this window"* as a two-hop join
over data already in the block. That is a service map, computed on demand, with
no service-map to maintain, no metrics-generator sidecar, and no second write
path. Compare Tempo, which runs a separate metrics-generator writing to a
separate Prometheus to answer the same question.

Materialisation is `fetch(frame, signals) → rows`, and each predicate is
individually cheap:

- `time` → directory-name pruning, **before any file is opened**.
- `entities` → §7.2, one small table per surviving block.
- `traces` / `spans` → §7.4.

Within a block rows are in arrival order, which is near-time-ordered but not
guaranteed, so the residual time filter is a full compare over one `i64` column.
That runs at memory bandwidth and is not worth a sort at seal time.

### 7.4 Indexes: what is needed and what is not

- **Time** — directory names. Built.
- **Entity** — `resources.key`, written at seal. Built. At query time the key set
  of a block is cached after first read; tens of `u64` per block means ~4 MB for
  ten thousand blocks, so no on-disk filter is warranted.
- **Trace** — the one that genuinely needs an index. Block level is **built**;
  within-block is not, and does not need to be yet. A block's min/max trace id
  spans the whole range and prunes nothing, so:
  - Block level: a Bloom filter over the block's distinct trace ids, 10 bits per
    key with k=7, in a `trace.idx` sidecar written at publish and `read` **on
    demand, not at boot** — so the zero-file-opens boot property survives. Sized
    on distinct ids, not rows: spans of a trace arrive in one export and land
    adjacent, so dropping adjacent duplicates costs one 16-byte compare per row
    and makes the filter 8× smaller. Both 64-bit halves of the id go through a
    splitmix64 finalizer before Kirsch-Mitzenmacher double hashing — W3C only
    requires a trace id to be non-zero, X-Ray puts an epoch in the first four
    bytes, and the bit index reads only the low bits, so without the finalizer a
    structured id set maps onto a handful of bits and the filter answers "maybe"
    to everything. Every damage path — missing, short, wrong magic, future
    version, bad CRC32 — answers "scan the block". A false positive costs one
    wasted read; a false negative silently loses spans.

    Measured over 4.1 GB / 25M spans / 84 blocks, fetching one 8-span trace:

    | | blocks scanned | rows scanned | cold | warm |
    |---|---|---|---|---|
    | without | 84 | 25,000,000 | 14.3 s | 14.3 s |
    | with | 1 | 212,992 | 250 ms | 20 ms |

    The filters cost 65 KB per block — 5.3 MB against 4.1 GB, 0.13%. That warm
    number is the tell: the cost was never paging, it was opening and CRC-checking
    83 blocks that could not have held the trace.
  - Within a block: none on disk, and none in memory either — a linear scan of the
    mapped `trace_id` column of one block is the 20 ms above. Sorting and caching
    a permutation is the upgrade if that ever stops being true.
- **Attribute value** — an `attr.idx` sidecar, same machinery, **built**. This is
  the other query with no useful time bound, and it is less obvious than the
  trace one: *"any record with `k8s.pod.name = api-7f9`"* has no early exit,
  because `limit` never fills, so proving a negative reads every block in
  retention. Measured over 6.4 GB / 25M logs / 69 blocks: **10.4 s → 71 ms cold,
  5.7 ms warm**, 69 blocks scanned → 0. A matching value goes 286 ms → 116 ms
  cold, 26 → 19 ms warm.

  The filter holds every distinct `(key, value)` pair from *every* attribute
  table in the block — record, resource, scope, and span event/link — because the
  query layer searches all three levels and a filter that missed one would prune
  blocks holding real matches. It is built from the schema, not from a list of
  table names, so a fourth level cannot be forgotten into a correctness bug.
  Sized by distinct pairs, it is **70 bytes per block** on the load generator's
  corpus and grows only where cardinality is high — which is where it prunes
  best. Past a million distinct pairs it writes nothing and the block is scanned.

  **The subtle part is what gets indexed.** A query scalar is compared against
  whatever type the SDK happened to store: `{attr: http.status_code, eq: "200"}`
  matches a stored integer `200`, because the comparison parses rather than
  making the caller know the SDK's choice (§7.6). A filter over the *typed* bytes
  would disagree with that rule and prune the block holding the row — not a slow
  query, a row that silently does not exist. So the indexed key is the value's
  **decimal text**, which makes one probe cover every type whose arm can parse
  it. Doubles are the exception and are not indexed at all: `200`, `200.0` and
  `2e2` are one number and three strings. The block sets a `HAS_DOUBLE` flag
  instead, and any query whose value reads as a number scans a block that has
  them. Blocks with no float attributes — most of them — pay nothing for the rule.

**Why not sort blocks by `trace_id` instead?** There is exactly one physical
order, and every query has a time bound while only some have a trace bound. Time
wins.

### 7.5 Why this is the agentic surface

The frame algebra *is* the MCP tool set: `anchor`, six expanders, `fetch`,
`summarize`. An agent handed SQL over a five-table star schema with EAV attribute
tables will write wrong joins — silently wrong, because a missing `parent_id`
predicate returns a cross product that looks like data. An agent handed seven
closed operations cannot express a wrong join at all, and every call returns a
frame it can bound before materialising. That is the difference between an
investigation loop and a timeout. §8.

### 7.6 Query, outside correlation

A predicate on an attribute is a **relational semi-join**, not a column filter:
filter `log_attrs` on `(key, active-value-column)` → collect the `parent_id` set
→ semi-join into `logs.id`. arrow-rs ships no join kernel (`arrow-select` has
`filter`, `take`, `interleave`, `concat`, and no join), so this is roughly 120
lines of `FxHashSet`-based helper covering all six value columns and all three
attribute levels. Correct only because ids were rebased at ingest (§0).

**"Vector matching" is settled**: it means cross-signal correlation, as above, not
the PromQL sense (`on`/`ignoring`, `group_left`). The PromQL reading would need a
full evaluator and a series-major on-disk layout — a different sort order from
§3 — and is out of scope. This was the one open question that could have changed
§3; it does not.

---

## 8. Agentic surface

**Built**: `POST /mcp` on the same listener as everything else, JSON-RPC 2.0 over
Streamable HTTP, four tools — `query_records`, `get_trace`, `query_metric`,
`list_metrics`. Hand-rolled rather than `rmcp`: the protocol at this scope is a
method dispatch over a JSON document, we already parse those (§1, KYAML), and the
SDK's session model is the thing we specifically do not want.

Two decisions worth keeping:

- **No `Mcp-Session-Id`.** Streamable HTTP permits a server to hand out a session
  id and then require it on every later request, which makes the server a thing
  with memory that a load balancer must route back to. Issuing none is principle
  4 applied to the agent surface: any replica answers any request, and killing one
  loses nothing.
- **The same read path as the UI.** The tools call `api::search_doc`,
  `api::series_doc` and `api::bounds` — the same parsers the HTTP API uses — and
  the same `query`/`series` functions, through the same `envelope()`. A separate
  "agent API" is a second read path to keep correct, and the first thing it does
  is drift.

`get_trace` exists as its own tool rather than as a `trace_id` term because the
default window is one hour: an agent handed yesterday's trace id would otherwise
get an empty result with nothing to explain it. It searches all of retention, and
§7.4's block filter is what makes that affordable.

Still not built:

- **Block-footer sketches** — HyperLogLog for cardinality, t-digest for
  latency quantiles, top-K for attribute values — in `Schema.custom_metadata`.
  This is the correct use of `custom_metadata`: per-block summary statistics, not
  attribute dedup. They exist so an agent asking "what is unusual in this hour"
  gets an answer from footers rather than from a scan, which is the difference
  between a usable investigation loop and a timeout.
- **GenAI conventions** need no special code. They need the attribute table to
  handle multi-kilobyte prompt strings without pathology, which plain `Utf8`
  values already do and dictionary keys would not.

---

## 9. Durability and failure model

| Failure | Behaviour |
|---|---|
| Crash mid-block | Unacked exports are re-sent by the client (OTLP retryable set). Nothing on disk is torn — the block was never renamed. `.tmp` is cleaned on next publish. |
| Crash mid-rename | Directory rename is atomic. Either state is consistent. |
| Bit rot in a block | CRC32 mismatch on open → typed error, not wrong answers. |
| Truncated / non-Arrow file | `ARROW1` check → typed error. |
| Disk full | `publish` fails, waiters get `INTERNAL`, client retries. No partial block is visible. |
| Reader holds a block being expired | Safe by POSIX unlink semantics (§6). |
| **SIGTERM / SIGINT** | Stop accepting, let in-flight exports reach their ack, then close the flusher channels so each open block is sealed and published. Bounded at 15 s. |
| **Network filesystem** | **Not safe.** mmap on NFS/CIFS raises `SIGBUS` with no recovery path. Needs a `statfs` `f_type` check that disables mmap and falls back to a heap-read path. *Not yet implemented — see §10.* |

**Shutdown is about duplicates, not loss.** A hard kill loses nothing that was
acknowledged, because an ack *is* an fsync (§4). What it costs is the other
direction: an export that was received, queued and then cut off is still sealed
and published by the drain, but the exporter saw a reset, and OTLP tells it to
retry — so every rolling restart would double-write whatever was in flight. That
is why the sequence is ordered: stop accepting first, drain the servers, and only
then close the flusher channels. The graceful window is bounded below by
`max_block_age`, since a waiting export is waiting on a block that no new data
will grow once the listener is closed. SIGTERM is handled alongside SIGINT
because SIGTERM is what an orchestrator actually sends.

**macOS.** Rust's `File::sync_all()` and `sync_data()` both compile to
`fcntl(F_FULLFSYNC)` on Apple targets. That is correct durability for free and a
large throughput cliff, and it is why the smoke test's 2 s is dominated by
`max_block_age` rather than by the fsync. Docker-for-Mac volumes can return
`EINVAL`/`ENOTSUP`, where std will not fall back; that needs a wrapper that
degrades to `libc::fsync` and increments a counter, so silent durability loss is
observable. *Not yet implemented.*

---

## 10. What is deliberately not here

- **The frame expanders of §7.3.** A span query returns its links and a series
  returns its exemplars, so both out-edges of §7.1 are readable — but the caller
  follows them itself. `by_link`, `by_exemplar` and `peers` as one-call
  operations are not here.
- **A block cache.** Every query re-opens and re-CRCs every block it touches. The
  fix is a process-local `Arc<MappedTable>` map invalidated by `expire`. This was
  assumed to be the next big win and it is not: §11 measures the per-block cost as
  dominated by faulting the mapping in, which the `MADV_WILLNEED` hint already
  addresses. A cache saves the `open` and the CRC — real, small.
- **A dependency on `otel-arrow-dfe-quiver` 0.54.1.** It is an embeddable
  Arrow segment store from the OTel Arrow maintainers, Apache-2.0, and it already
  ships a CRC32 WAL with replay, immutable IPC segments, `SegmentReader::open_mmap`,
  64-byte `STREAM_ALIGNMENT`, `MADV_DONTNEED` on release and a disk budget. It is
  the single best piece of prior art here and its `ARCHITECTURE.md` is worth
  reading before touching the block format. Mira does not depend on it because:
  it pins `arrow ^58.3` (incompatible with 59.x in one graph), its API is
  explicitly unstable pre-1.0, its subscriber model does not match, and — the
  decisive gap it names itself — **it carries no statistics and no time index**,
  which is precisely Mira's differentiator. What Mira takes from it is the format
  decisions, which this document already reflects.
  Related: do **not** depend on `otel-arrow-dfe-pdata`; it pulls
  `datafusion ^53` non-optionally for two imports.
- **DataFusion.** §1.
- **`statfs` network-filesystem guard** and the **`F_FULLFSYNC` fallback**. Both
  are in §9 and both are real; neither is written.
- **The query-side half of `NO_IDENTITY`.** The sentinel is written (§7.2); the
  expander that must refuse it does not exist yet, because the query layer does
  not. It is the first thing that layer owes.

---

## 11. Performance model

The four axes, each with a target and a measurement. Measured on an Apple M3 Pro
(12 cores, 36 GB), release build, `loadgen --conns 96 --batch 8192 --for 90s`
over loopback — 6.4 GiB on the wire, 8.4 GiB on disk, 25.2 M log records, 25.2 M
spans, 69 log blocks holding 4.56 GB of log Arrow.

Read the two query columns carefully. **Neither is a cold-disk number**: 8.4 GiB
fits in this machine's page cache, so after one pass everything is resident and
short of `purge` there is no way back. "First" is the first call after a process
restart — the pages are in RAM but not in this process's address space, so it
measures establishing 276 mappings and faulting them in. "Steady" is the same
call repeated. The gap between them is virtual-memory work, not I/O.

| Axis | Target | Measured | |
|---|---|---|---|
| Ingest throughput | ≥ 1 M records/s/core | **544,658 records/s / 71.2 MiB/s** aggregate, 0 shed, 0 resets | ✗ |
| Resident footprint | ≤ 2 × the open block's target size | holds; no `concat_batches` regression | ✓ |
| Ack latency | — | p50 **498 ms**, p99 **2,371 ms** | see below |
| Query: attribute value, absent | ≤ 10 ms | **104 ms** first, **6.1 ms** steady, 0 of 69 blocks | ✓ |
| Query: attribute value, matching | ≤ 10 ms | **17 ms** first, **14.5 ms** steady, 1 of 69 blocks | ~ |
| Query: trace by id | ≤ 10 ms | **84 ms** first, **14.3 ms** steady, 1 of 86 blocks | ~ |
| Query: metric names | — | **28 ms** first, **5.5 ms** steady | — |
| Query: field predicate, no time bound | — | **2.9 s** first, **1.9 s** steady, 69 of 69 blocks, 25.2 M rows | see below |
| Cost per GB ingested | ≤ 0.35 B/B | **1.31 B/B** hot, **~0.17 B/B** compacted | ✓ |
| Binary size | ≤ 20 MB stripped with UI + query + MCP | **4.5 MB** / 113 crates | ✓ |

Reading these honestly:

- **Ingest is aggregate, not per core**, and it is not the interesting number
  anyway: at 96 connections the run is fsync-bound, which is what the ack
  latencies say. p50 of ~500 ms is the block filling; p99 of 2.4 s is a durable
  publish landing in front of a waiter. Both are *chosen* — acks come after
  durability (§9), so read-your-writes is free and every e2e test queries with no
  sleep after export. The per-core encode number needs a criterion bench on
  `append_request` with no I/O in the path, which does not exist yet.
- **Blocks not opened is the whole game.** The two sidecar filters (§7.4) took
  the block count from "all of them" to one or zero, which is worth between 60×
  and 1800× and is the reason these rows are in milliseconds at all. Everything
  below is about the cost of the blocks that *are* opened.
- **The per-block cost is page faults, not the scan.** A query that opens one
  91 MB block answers in ~14 ms, of which the CRC32 of the whole body (§3.3) and
  the dictionary scan are a few. Multiply that by 69 and the last row of the
  table should be ~1 s; before this was fixed it was **10.1 s**, and it did not
  improve on repetition, which ruled out disk — the data was already resident.
  What it was: `mmap` faults 16 KB at a time and `open_table` touches every page
  anyway, so a 4.56 GB scan took ~278 K single-page faults with no readahead. One
  `madvise(MADV_WILLNEED)` at map time — the mapping is about to be read end to
  end, so there is nothing speculative about the hint — took that row from
  **10.1 s to 1.9 s**, and every other query row down with it.
- **A block cache is worth much less than it looks.** It was the obvious next
  lever and the measurement says otherwise: what it saves is the `open` and the
  CRC, and those are the small part of a ~14 ms single-block query. It stays on
  the §10 list, but as a small win, not the missing 10×.
- **Cost per GB is 1.31 B/B while a block is hot and about 0.17 once it is
  compacted.** Not the sidecars either way: all 189 `attr.idx` files together are
  12.9 KB, and the trace filters are 5.5 MB against 8.4 GB. What inflates the hot
  number is the `ATTRS` table, which carries six typed value columns and writes
  all six for every row — a string attribute pays 8 bytes for a null `int`, 8 for
  a null `double` and 4-byte offsets each for null `bytes`/`ser`, roughly 24
  bytes of padding per attribute row.

  That padding is also almost free to compress, which is what the cold tier
  (§3.5) collects. Measured by `cargo run --release -p mira-core --example tier`
  over 8 real blocks per signal — not a `zstd` CLI estimate, the actual
  `write_table_zstd` path:

  | | plain | zstd | ratio | lz4 | ratio |
  |---|---|---|---|---|---|
  | `log_attrs.arrow` | 258.3 MiB | 19.7 MiB | 0.076 | 36.9 MiB | 0.143 |
  | `logs.arrow` | 283.9 MiB | 49.3 MiB | 0.174 | 82.9 MiB | 0.292 |
  | **logs, 8 blocks** | **542.3 MiB** | **69.1 MiB** | **0.127** | 119.9 MiB | 0.221 |
  | `span_attrs.arrow` | 205.6 MiB | 14.9 MiB | 0.073 | 27.8 MiB | 0.135 |
  | `spans.arrow` | 194.1 MiB | 41.7 MiB | 0.215 | 52.0 MiB | 0.268 |
  | **traces, 8 blocks** | **399.8 MiB** | **56.7 MiB** | **0.142** | 79.9 MiB | 0.200 |
  | metrics, 8 blocks | 1.2 MiB | 0.3 MiB | 0.271 | — | — |

  1.31 B/B × ~0.13 is **~0.17 B/B**, comfortably under the 0.35 target. The
  metrics ratio is worse only because that corpus is 1.2 MiB — too small for
  per-buffer framing to disappear into the payload. The tiny `resources` and
  `scope_attrs` tables sit near 1.0 for the same reason and are not worth the
  rewrite; compaction does them anyway because skipping them is more code than
  it saves bytes.

  Compression runs at **1160 MiB/s** on logs and **989 MiB/s** on traces, one
  core, so a block is a few tens of milliseconds on a path already inside
  `spawn_blocking` and off the ingest critical path entirely — it is the
  retention sweep, an hour after the data landed. The `MAX_COMPACT_PER_SWEEP`
  cap of 8 blocks a minute exists for the first pass over an existing volume,
  not for the steady state.

  **The open question from the previous revision is answered, and the answer is
  the opposite of what the design assumed.** The worry was that inflating a
  compressed buffer into the heap would cost more latency than the pages it
  saves, and that the age threshold would therefore have to be conservative.
  Measured over the same 8 blocks, opening the compacted copy is *faster*:

  | | read plain | read zstd | |
  |---|---|---|---|
  | logs, cold cache | 0.68 s | **0.54 s** | 8 blocks, 542 MiB |
  | logs, warm | 0.33 s | **0.26 s** | same 8 blocks |
  | traces, cold cache | 0.58 s | **0.26 s** | 8 blocks, 400 MiB |
  | traces, warm | 0.23 s | **0.19 s** | same 8 blocks |

  The absolute numbers move with how much of the corpus is resident — this
  machine has 36 GB of page cache against 8.4 GiB of bench data, so there is no
  genuinely cold measurement here — but the sign does not: compressed is faster
  in every run. Both the page-fault path (§3.3's `MADV_WILLNEED` over 8× fewer
  pages) and the CRC32 (over 8× fewer bytes) shrink with the file, and together
  they more than pay for decompression. So the cold tier costs the read path nothing measurable
  — only the zero-copy property, which is an allocation cost, not a latency one.
  The threshold is set at one hour for the reason in §3.5, which is that it is
  the partition width and therefore not a knob; nothing in the measurement argues
  for waiting longer.

Two invariants guard the design rather than the numbers, and both are already
tests: n/n buffers zero-copy on read of a hot block, and a corrupted body never
returns as data. The cold tier is held to the second and deliberately not the
first — `compaction_shrinks_aged_blocks_without_changing_what_they_answer`
asserts that a compacted block gives up zero-copy while the block beside it,
still hot, keeps it.

The honest headline for the README is **"zero-copy queries over immutable Arrow
blocks, allocation-lean OTLP ingest."** Not "zero-copy ingestion" — that claim
does not survive anyone reading `prost`.

---

## 12. Multiple active replicas

The requirement: N replicas, all active, scaling horizontally, with no hard
coordination. This is principle 4 — "stateless means no coordination state" —
cashed out as a deployment topology.

**Shared-nothing ingest, scatter-gather query, discovery borrowed from the
platform.**

### 12.1 Ingest

Any L4 load balancer. Each replica owns its own disk and writes its own blocks.
There is no consistent-hashing ring, no shard map, no routing logic — not because
we skipped it, but because nothing has to land on a particular node. Blocks are
independent immutable objects with no global ordering and no cross-block merge,
so "which replica received this export" is not a question anything downstream can
ask. That falls out of the storage design rather than being a feature bolted onto
it.

Two things had to change to make concurrent writers safe, and both are in:

- The block directory name carries a **node id** (§3.2), so two replicas cannot
  allocate the same name. Fixing this also fixed a live single-node bug: `seq`
  was resumed from `scan().last()`, and `scan` sorts by `(min_ts, seq)`, so a
  restart following a backlog replay could reuse a sequence number and wedge the
  node on `ENOTEMPTY` forever.
- Retention tolerates a losing race on `remove_dir_all` (§6). Two replicas
  expiring the same block is not a conflict.

### 12.2 Query

A query arriving at any replica is broadcast to `cluster.peers`, executed locally
on each, and merged. **The frame algebra of §7.3 is what makes this work**: a
frame is a small value, so broadcasting it is free, and merging two nodes'
results is a set union.

The reason it is a set union — rather than a distributed join — is the entity
identity of §7.2, and this is the load-bearing connection between the two
requirements. Block-local ids never leave a node; they are meaningless off-box.
The only identifiers that cross the wire are the globally stable ones:
`resources.key`, `trace_id`, `span_id`, timestamps. Had entity identity stayed
"equality of the resource attribute set", cross-node correlation would have
needed a cluster-wide resource dictionary — which is coordination state, and the
principle forbids it. One decision paid for both features.

Fan-out uses the same query API as an external client, with a hop flag so a peer
does not re-broadcast.

**Partial results are reported, never hidden.** A scatter-gather over seven nodes
with one down must not quietly return six sevenths of the data and let the user
draw a conclusion from it. Every response names which peers answered.

### 12.3 Discovery without membership

`cluster.peers` is a list of addresses, and in Kubernetes it is a headless
Service: DNS already enumerates every replica, and the platform already keeps
that current. Mira stores nothing about the cluster. There is no gossip, no
heartbeat, no join/leave protocol and no split brain — not because they are
solved but because there is no membership to be wrong about. A peer that does not
answer is a peer whose data is absent from this answer, and the answer says so.

### 12.4 What scales, and what this deliberately does not buy

| | |
|---|---|
| Ingest throughput | Linear. Nodes are independent. |
| Storage capacity | Linear. |
| Query capacity | Linear. Latency for one query is the slowest peer. |

- **No replication.** A lost disk is lost data for that node's share. The answer
  is client-side fan-out — an OTel Collector can export to two Mira replicas —
  which costs Mira zero coordination. A replication factor above one requires a
  placement decision, and placement *is* coordination state. This is the sharpest
  edge of principle 4 and it should be stated to users in exactly these terms
  rather than buried.
- **No deduplication.** OTLP is at-least-once; an export retried after a timeout
  can land on two replicas and be stored twice. Suppressing that needs a global
  index.
- **No rebalancing, ever.** New nodes get new data; existing data stays where it
  is and ages out. **Retention is the rebalancer** — a cluster is evenly loaded
  one retention period after any scale-out, with zero bytes moved. This is a real
  dividend of retention-bounded storage that an unbounded store cannot collect.
- **A replica's identity is its disk.** A StatefulSet with a PVC. On ephemeral
  disk, a rescheduled pod's unexpired data is gone.

### 12.5 Shared-volume mode

If replicas do share one filesystem (an RWX PVC), the design already works
unmodified — block names are unique per writer, publishes are independent
renames, and `scan` sees every writer's blocks, so any node answers any query
with no fan-out at all. It is gated on one thing: **mmap over NFS raises SIGBUS
with no recovery path** (§9), so the `statfs` guard has to land first. Object
storage is a larger question — it forecloses mmap entirely — and is deferred to
the market survey rather than guessed at here.
