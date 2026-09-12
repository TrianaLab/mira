# Mira — Architecture

**For:** anyone about to change the engine, and anyone deciding whether its
trade-offs are the ones they want. Not a getting-started page — that is
[See it work](demo.md).

**This document is the *why*.** The *what* is generated from the code and
published at **[miradb.dev/api](https://miradb.dev/api/)** — rustdoc over the
whole workspace including private items, because in a binary crate the
mechanisms are all private. If you want to know what a function does, read
rustdoc; if you want to know why it exists at all, read here. A doc comment and
this document can therefore never disagree about behaviour, because only one of
them describes behaviour.

| If you are looking for | Read |
|---|---|
| the on-disk block format, publish and scan | [`mira_core::block`](https://miradb.dev/api/mira_core/block/index.html), and section 3 below for why it is shaped that way |
| the Arrow schemas, column by column | [`mira_core::schema`](https://miradb.dev/api/mira_core/schema/index.html) |
| filters, pruning and the query executor | [`mira_core::query`](https://miradb.dev/api/mira_core/query/index.html), [`attrs`](https://miradb.dev/api/mira_core/attrs/index.html), [`zone`](https://miradb.dev/api/mira_core/zone/index.html), [`bloom`](https://miradb.dev/api/mira_core/bloom/index.html) |
| the ingest channel, flusher and retention worker | [`mira::pipeline`](https://miradb.dev/api/mira/pipeline/index.html) |
| the write-ahead log | [`mira_core::wal`](https://miradb.dev/api/mira_core/wal/index.html) |
| every HTTP route and its body | [HTTP API](reference/http.md) — generated from the router |
| every flag and config key | [CLI](reference/cli.md), [Configuration](config.md) — generated from the binary and from `Config` |

**Status:** the workspace under `crates/` implements most of this and its tests
pass. Sections marked "still not built" are the exceptions; 0.1 below is the
complete list, and the README's *Scope* is the three-line version of it.

---

## 0. Corrections to the original brief

The brief specified several mechanisms by name. Seven of them do not survive
contact with the formats involved. They are listed first because they change the
shape of everything below, and because the reasons are not obvious from the
outside.

| Brief said | What is actually true | What Mira does instead |
|---|---|---|
| **Delta-of-delta timestamps** | Arrow IPC has no per-column encodings. Its entire encoding surface is whole-buffer LZ4/ZSTD plus the Dictionary and RunEndEncoded layouts. Implementing DoD means inventing a buffer layout no Arrow reader understands — which forfeits zero-copy, since you must decode into a fresh allocation. Separately, Gorilla's 12× rests on samples landing on exact interval boundaries; OTLP `time_unix_nano` is a wall-clock read with 10⁵–10⁷ ns of jitter between consecutive deltas. | Plain `Timestamp(Nanosecond)`. Sort by time within a block; take the size win at the cold tier from a generic compressor. |
| **Resource/Scope dedup at block headers** | The Arrow mechanism for a "block header" is `Schema.custom_metadata`, exactly one map per file. That expresses dedup only if a block holds exactly one Resource. A block from any multi-tenant collector holds hundreds. | OTAP section 6.3: `resource_id`/`scope_id` `UInt16` columns in the root table plus separate attribute tables keyed by `parent_id`. A resource with 40 attributes shared by 10,000 records costs 40 rows and 10,000 `u16`s. |
| **Dictionary-encode high-cardinality maps** | Backwards. Dictionary encoding is a *low*-cardinality technique with a hard ceiling — `Dictionary<UInt16,_>` raises `DictionaryKeyOverflowError` past 65,536 distinct values. `http.url` and `trace_id` are the highest-cardinality data in the system. | Dictionary-encode only enumerable columns: attribute *keys*, `severity_text`. Attribute *values* are plain `Utf8`/`Binary`. |
| **Zero-copy ingestion** | Impossible on the OTLP path. `prost` memcpies every string unconditionally; varints must be decoded. Even on OTAP, `StreamDecoder` only avoids a copy when the whole message body is one contiguous `Buffer`, and an HTTP/2 body split across DATA frames is `extend_from_slice`'d. | Say **zero-copy queries**, not zero-copy ingestion. The ingest goal is *allocation-lean*: one unavoidable memcpy of the request body, then no per-field heap allocation. |
| **Lock-free ring buffer for ingestion** | Cargo-culted from LMAX, where an item is a 150 ns order struct. Here an item is an export request costing 10⁵–10⁶ ns to decode and encode, arriving 10²–10⁴ times per second. The queue is four orders of magnitude from being the bottleneck, and a lock-free queue cannot express backpressure. | A bounded `tokio::sync::mpsc` per flusher shard. A full set of them parks the caller for up to `ADMIT_WAIT` (section 4), which propagates backpressure out as HTTP/2 flow control; only a timeout sheds. Revisit if a queue ever appears in a profile. |
| **4317 and 4318 both served by tonic** | 4318 is not gRPC. Per the OTLP spec it is plain HTTP/1.1 POST of protobuf or JSON to `/v1/{traces,metrics,logs}`. | Two listeners: tonic on 4317, axum on 4318. axum is already in the tree via tonic's `router` feature, so it costs no dependency. |
| **A reflective proto3-JSON decoder for OTLP/HTTP JSON** | OTLP JSON is *not* canonical proto3 JSON. Ids are hex where every other `bytes` field is base64 — and a 32-character hex string is itself valid base64, so a generic decoder does not fail, it silently yields 24 bytes of nonsense for every `trace_id`. 64-bit integers are strings. Field names may be either dialect within one document. | `crates/mira/src/json.rs`: a hand-written decoder over the YAML 1.2 loader already in the tree (`api::parse`; YAML 1.2 is a superset of JSON, so KYAML bodies work for free — section 1). No new dependency, and the two deviations are handled where they occur rather than configured around. |

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
  the tree C-free. Measured against each other on the same blocks (section 11), LZ4
  also clears the 0.35 target — 0.189 on logs, 0.173 on traces — so this was not
  the walkover it looked like. ZSTD wins on the two axes that are paid for
  repeatedly: 1.5× smaller, and read back faster in every pass. What it gives up
  is compression throughput, 808 MiB/s against LZ4's 976 — and that is the one
  cost with nowhere to land, since compaction is an hour-old block inside
  `spawn_blocking`, not the ingest path. 3 crates and 0.5 MB of binary for
  another 1.5× on disk and a faster read is worth the `cc`.

### One correctness hazard worth naming on its own

OTAP `id` and `parent_id` values are unique only **within a single
`BatchArrowRecords`**. Persisting them verbatim and then joining
`logs.id = log_attrs.parent_id` across batches produces a silent cross-product —
wrong answers, no error. Mira rebases every id into a dense, block-local
`UInt32` at ingest. That makes any join *inside* a block unconditionally correct
and removes the need for a partition-discriminant column on every table and an
extra predicate on every join.

### 0.1 What is not true yet

The README carries four of these; this is all of them. Each is a boundary
somebody will otherwise discover by deploying into it, so each says what is
missing, why, and what to do instead today.

- **Not zero-copy *ingestion*.** Not achievable through protobuf — `prost`
  memcpies every string, unconditionally, and varints must be decoded. The
  ingest goal is *allocation-lean*: one unavoidable copy of the request body,
  then no per-field heap allocation. The correction table above has the long
  version; "zero-copy" in this document always means queries.
- **No block cache.** Every query re-opens and re-CRCs each block it touches.
  This looked like the next big win until it was measured against the two that
  were taken instead — it is worth a few milliseconds of a 14 ms query, not the
  10x that page-fault behaviour was (section 11). It becomes worth building the
  day a working set stops fitting in page cache, because that is when the CRC
  read stops being free.
- **OTAP is the data model, not yet the wire protocol.** No language SDK emits
  OTAP; the only production implementations are the Go
  `otelarrowreceiver`/`exporter` in collector-contrib. OTLP on 4317/4318 is the
  universal path, and the OTAP receiver is a second listener over a storage
  layout that is already shaped for it.
- **No cross-replica query fan-out.** A query reads the block directory it was
  pointed at and nothing scatters it: two replicas sharing a volume both answer
  for all of it, two replicas with a volume each answer for half. There is no
  peer list to configure, which is the point (principle 4) and also the
  limitation. Section 12.2 has the shape it would take if it were built, and
  section 12.4 is honest about what that would and would not buy.
- **No entity *predicate*.** The entity key each block stores — the identity
  that survives an attribute changing mid-rollout, section 7.2 — is now read:
  `/api/v1/entities` lists what a window holds and `correlate`'s `peers` returns
  the entities that shared a trace. What no query document accepts is an entity
  key as a *filter*, so the service pickers those two populate hand back a
  `service.name` term, and "everything this pod emitted" is still an attribute
  predicate over a time range: `{"attr":"service.instance.id","eq":"..."}`. The
  gap matters exactly when the attribute changed mid-window, which is the case
  the entity key exists for.
- **No bucket-level histogram queries.** A histogram, an exponential histogram
  and a summary are each *stored* whole — `bucket_counts`, `bounds_id`, `scale`,
  `quantile` are all on disk — but the query surface hands each of them back as
  two derived series, `<name>.count` and `<name>.sum`, the same convention
  Prometheus uses. So "p99 of `http.server.duration`" is not a question this
  engine answers yet; `sum/count` is, and the buckets are there waiting for the
  reader that reads them. Section 13.3 is why alerting does not need them.
- **No `F_FULLFSYNC` fallback.** `File::sync_all` *is* `fcntl(F_FULLFSYNC)` on
  Apple targets — 4.2 ms on the reference machine against 28 us for a bare
  `fsync(2)`, and the published ack latencies were measured against it rather
  than against the weaker call. The gap is volumes that answer
  `EINVAL`/`ENOTSUP`, Docker-for-Mac among them: std does not fall back there,
  and a wrapper that did would have to *count* the fallback rather than quietly
  weaken the acknowledgement it already returned.

---

## 1. Principles, and the mechanism each one buys

The five principles are constraints, not aspirations. Each needs a mechanism or
it is decoration.

**Performance is the product.** The four axes — ingest throughput per core,
resident footprint, query p99, cost per GB — conflict pairwise. Compression cuts
cost per GB and raises query latency. Large blocks raise throughput and raise
footprint. Mira resolves them by **tiering**, not by claiming all four at once:
hot blocks are uncompressed, 64-byte aligned and mmapped; cold blocks get
compression and give up zero-copy. section 11 states the target for each axis and how
it is measured.

**Agentic**, in all four senses the owner selected:
- *LLM-queryable surface* — a native MCP server (hand-rolled, section 8.1) over the same query
  engine, so an agent investigating an incident issues one call instead of
  composing PromQL and TraceQL. Block footers will carry sketches (HLL, t-digest,
  top-K) specifically so exploratory "what is unusual here" queries are answerable
  without a scan.
- *Telemetry for AI workloads* — OTel GenAI semantic conventions as a first-class
  case. Concretely this means the attribute table must handle multi-kilobyte
  prompt/completion strings without pathology, which is exactly why attribute
  values are plain `Utf8` and not dictionary keys.
- *Self-driving* — no tuning knobs. `pipeline::Config::default` holds
  `target_block_bytes` and `max_block_age`, the two numbers an operator would
  most want to tune, and they are constants in the binary: there is no path from
  the YAML to either, and nothing moves them at runtime. Adapting them to
  observed load is the ambition and is not built; what is built is the half that
  matters more, which is that neither can be set wrong from outside. There **is**
  a config file ([Configuration](config.md)), and it is not a contradiction: it describes
  *where the process runs* — addresses, data directory, retention policy, replica
  name. The one key that reaches the engine, `ingest.shards`, is there because the
  runtime's count of the cores it has can be wrong (section 4), and the default
  asks nobody: it is a correction, not a tuning surface. The boundary
  is structural, not documentary. It is also closed — twelve keys, and an unknown
  one is a startup error naming it — because the alternative is what
  `cluster.peers` was (section 12.2): a key read by nothing that still looks like a
  setting in effect.
- *Agent-based internals* — the flusher tasks, three signals × `ingest.shards` of
  them, and the retention worker are a message-passing mesh already, and the
  flushers are supervised: one that returns before the stop signal takes the
  process with it, once the other signals have drained. A crashloop is the honest shape of "this node cannot store logs"
  — an orchestrator reports it, and a restart is the recovery for the case that
  causes it — whereas carrying on leaves one signal answering 503 forever behind
  a probe that stays green. The retention worker is *not* in that select:
  `spawn_retention` drops its handle, so a sweep that stopped is invisible until
  a disk fills. This is a description of the design, not a licence to build an
  actor framework.

**OTLP-first.** The Arrow schemas in `crates/mira-core/src/schema.rs` *are* the
OTLP Resource-Scope-Signal model. There is no transformation step to a generic
relational or inverted-index store, and therefore no place for one to lose
fidelity. The claim is only ever as good as the column list, though, which is
the shape every fidelity loss here takes: `LogRecord.event_name` was decoded off
the wire and had nowhere to land. Not a transformation — a missing field.

**Single binary, no operational overhead, stateless.** Stateless means *no
coordination state*: no cluster membership, no Raft, no external metadata store.
The concrete mechanism is in section 3.2 — the filesystem is the manifest. This also
rules out DataFusion *from the default build*: it would give SQL for free at a
cost of 47 direct dependencies and a ~1.5M SLoC transitive tree. The binary cost
was estimated here at 68–92 MB and that was too pessimistic — built at Mira's
release profile it is **50.0 MiB and 271 crates**, against 5.63 MiB and 117. An
order of magnitude is still an order of magnitude, so it goes behind a
`--features sql` cargo feature rather than into the binary everyone downloads;
what it does not do is displace the hand-rolled ~2,000 LOC fast path, because a
4.5 ms point lookup that already prunes to one block of 137 has nothing to gain
from a planner. With traces, metrics, query, MCP and both UIs in it, the default
build is **5.63 MiB stripped, 117 crates** — the scale the design is defending.

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
├── docs/architecture.md
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

Vendoring costs 1,725 lines of `.proto` and a 36-line `build.rs`, and `protox`
compiles them in pure Rust, so building Mira never needs a `protoc` on `PATH`.
It is also unavoidable anyway the moment OTAP is in scope: the
`ArrowTracesService`/`ArrowLogsService` definitions are not in the
`opentelemetry-proto` crate's codegen input list.

### Arrow version pin

One Arrow version across the workspace, declared once in
`[workspace.dependencies]`. Two majors in one graph means
`arrow_58::RecordBatch` and `arrow_59::RecordBatch` are different types and the
resulting error is unreadable. This is also the reason Mira does not depend on
`otel-arrow-dfe-quiver` — see section 10.

---

## 3. Data layout

The central claim: **the in-memory layout and the on-disk layout are the same
bytes.** There is no serialisation step, because Arrow IPC *is* the memory
format with a framing header. `write_table` streams the builders' buffers
straight out; `open_table` maps the file and hands the same buffers back.

### 3.1 Schemas

A signal is a **star schema**, following OTAP section 6.3 rather than a flattened
one-row-per-record table.

`logs` (root):

| column | type | note |
|---|---|---|
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

An unmarked row is non-null in `schema.rs`. Only `id`, `time_unix_nano`,
`dropped_attributes_count` and the two foreign keys are.

`event_name` was added after blocks had been written, and a published block is
never rewritten (section 6), so the question it raises is what a reader does with the
thirteen-column ones. Nothing: the reader never touches a root table
positionally — it locates columns through `column_by_name` and materialises a
row by walking the *file's* own schema — so an older block reads back exactly as
it did, minus the field. That is the property that lets a column be appended
without a format version, and it is the same property section 3.2 relies on for
sidecars.

`resources` — one row per distinct resource, so tens of rows against hundreds of
thousands in the root table:

| column | type | note |
|---|---|---|
| `id` | `UInt16` | block-local; what `logs.resource_id` points at |
| `key` | `UInt64` | **stable entity identity**, the cross-block join key (section 7.1) |
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
  ends — and both are connected: the read path decodes these bytes back, so an
  array attribute like `process.command_args`, a kvlist like
  `gen_ai.input.messages` and a structured log body come back as the JSON they
  were rather than as a hex dump. It costs zero dependencies. Switch to CBOR when
  a third party needs to read a block. Rendering is not filtering, though:
  `attrs.rs` leaves Slice and Map out of the block's attribute filter and no
  comparison operator accepts them, so a structured value is returned in a row
  and cannot select one.
- `parent_id` is `UInt32` and block-local rather than the wire's per-batch id.
  See section 0.

### 3.2 On disk

```
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
```

Those are the tables a block *can* hold, not the ones it does: `publish` skips an
empty one, so a traces block normally holds five of the nine — the event and link
tables stay empty unless a span carries one — and a metrics block eight of
thirteen. Thirteen is what OTLP's point types cost. Four point tables, because gauge and
sum share a column set and the three others do not; `hist_bounds` interns the
boundary array that every point of a histogram repeats, which measured 1.67×
smaller point rows; `exemplars` and its attributes carry the bridge to a trace,
and point ids are one space across all four point tables so that column needs no
discriminant saying which one to look in. Metrics blocks carry no `trace.idx`,
because nothing looks a trace up in one: search takes `logs` or `traces` only,
and an exemplar's trace id comes back attached to the series that sampled it
rather than being searched for.

The node id is section 12.1, and `wal_hi` is the log position the block covers (section 4) —
zero on a block written with `ingest.wal` off, and absent entirely on one
published before the field existed, which parses the same way. `attr.idx`,
`zone.idx` and `trace.idx` are **sidecars** — `(name, bytes)` pairs produced at
seal, written and fsynced by `publish` next to the Arrow tables and inside the
same atomic rename. Sidecars are always derived and
always optional: a reader that does not find one, or finds a damaged one, falls
back to the scan it would have done anyway (section 7.4). That is what makes them safe
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

section 11 has the measurement: 0.113 of plain size on logs, 0.126 on traces, and reads
that come back *faster* than uncompressed ones.

---

## 4. Ingest path

```
gRPC 4317 (tonic) ─┐                    ┌─ mpsc ─> flusher 0 ─┐
                   ├─> Ingest::submit ──┼─ mpsc ─> flusher 1 ─┼─> spawn_blocking ─> publish
HTTP 4318 (axum) ──┘         │          └─ mpsc ─> flusher k ─┘         │
                             └─────────────── oneshot ack ──────────────┘
```

**gzip on both listeners, because the spec says MUST and the exporter says
default.** Every OTLP server component must accept `none` and `gzip`, and both
of the collector's OTLP exporters compress by default — so a receiver that only
speaks plain bodies does not degrade, it rejects the first batch of every stock
deployment. The failure is worse than it looks: uncompressed protobuf fed a gzip
stream is a `400 invalid wire type`, and OTLP classes 400 as permanent, so the
exporter drops the data rather than retrying it. 4317 is
`accept_compressed(Gzip)` on each service; 4318 reads `Content-Encoding` and
inflates before it decodes, answering an encoding it does not implement with
`415` — the exporter needs to be told to stop offering it, not handed a parse
error that reads like corruption.

**One size limit, `ingest.max_request_bytes`, applied three times.** It is
axum's `DefaultBodyLimit` on 4318, tonic's `max_decoding_message_size` on 4317,
and the ceiling on what a gzip body may inflate to. Three applications because a
limit that bound only what *arrives* would not bind at all under gzip: a few
kilobytes of zeros expand past any memory the process has, and the decoder will
allocate every byte if asked. The inflation cap is absolute rather than a ratio
because a real OTLP batch — the same attribute keys over and over — reaches
about 35:1, so no ratio separates it from a bomb.

One *number*, rather than one per transport, because a batch a collector sends
happily over 4317 and that fails over 4318 is the worst kind of bug to be handed:
it depends on a transport nobody changed. The library defaults disagree — axum's
is 2 MiB and tonic's is 4 MiB — and 2 MiB is below what a stock collector's batch
processor produces at its own default of 8192 records. Mira's default is 16 MiB,
roughly 250k records in one export.

The two transports agree on the size and differ on the verdict: 4318 answers
`413`, which OTLP classes as permanent, and 4317 answers tonic's `OUT_OF_RANGE`,
which OTLP classes as retryable. Neither is wrong and the difference is not
Mira's to fix, but it means an oversized batch is dropped on one port and retried
until the queue gives up on the other. Either way the operator's move is the
same: raise this number or lower the sender's batch size, which is what both
error messages say.

`submit` tries `try_reserve` on every shard before it waits anywhere, and only
waits when all of them are full: `ADMIT_WAIT`, five seconds, on one shard picked
by a turn counter so the parked waiters wake as each queue drains rather than all
behind the same one. A timeout, and only a timeout, returns `UNAVAILABLE` with
`RetryInfo(250ms)`.

The first revision shed the instant the queue was full, on the reasoning that a
fast NACK beats an unbounded latency tail. The tail half is right and
`ADMIT_WAIT` is what bounds it; the "fast" half was wrong. Tonic and axum both
decode the request before the handler is called, so by the time `submit` runs the
expensive part of the export is already paid, and shedding throws it away for a
client that will send the same bytes again a second later. Section 11 has the
A/B: at 96 connections shedding cost 93% of exports, four cores busy, and a third
of the throughput two connections get on one. Parking is bounded by the
connection count — every waiter is a request already in memory — where a deeper
queue is bounded by nothing.

The export is acknowledged **only after the block directory rename is durable**.
OTLP's retryable status set — plus "if the server disconnects without returning a
response, the client SHOULD retry" — covers exports in flight at a crash. Acking
earlier is the one window in which data is lost while the client believes it was
stored. Because acknowledgement latency would otherwise be bounded by the
caller's own traffic, `max_block_age` (2 s) is a first-class flush trigger
alongside size. One `curl` of a single-record export at a running instance comes
back in 2.03–2.05 s, which is that bound and nothing else.

**No WAL.** Publish is write-tmp → fsync files → fsync tmpdir → rename dir →
fsync parent → fsync grandparent. The last one is not belt and braces. Fsyncing
a directory persists the entries *inside* it, not the entry naming it in its own
parent, and the first block of every hour creates the partition directory it
lands in — so with only the parent fsynced, that block is durable inside a
directory whose own name is unflushed metadata, and the ack is a lie. POSIX does
not define what fsync on a directory means at all, and no filesystem that
implements it promises anything about ancestors: a journalling one usually
commits the parent's entry in the same transaction, but "usually" describes how
that journal batched that second, not an interface anything may rely on. One
extra fsync per 32 MB block, against the per-table fsyncs already paid, is
cheaper than depending on it. Directory rename is atomic on POSIX, so a block is
either wholly visible or wholly absent. There is no torn state, therefore
nothing for recovery to replay. Crash recovery is `scan()` — the same `readdir`
the read path already does — and the sequence counter resumes from the highest
published block.

**A log anyway, for latency, not for recovery.** The paragraph above is still
true and `ingest.wal` does not contradict it: there is no torn state, and a
default-configured Mira has nothing to replay. What the log buys is the *ack*.
"Acknowledged means published" costs 2.6 s at p99 (section 11) because the export waits
for its block to fill or age out, and no amount of tuning fixes that — it is the
block size, and shrinking the block to fix the ack would trade the read path for
the write path. So the frame goes to `wal/` first and the ack costs a `write(2)`,
which is 7 µs at p50; the publish becomes a background reorganisation of data
that is already on disk. Two consequences follow, and both are load-bearing.
What the log tracks: not a high-water mark but a *set* — every sequence handed
out and not yet published, held in `Wal::pending` and consulted by
`watermark_for` (section 9). `Wal::append_then` does the enqueue inside the log's
own mutex rather than after it, so a frame is in that set before any shard can
publish past it; two `submit`s preempted between the append and the send would
otherwise let a block claim a frame it never stored. And convergence: a replayed
frame keeps the sequence it already has, so
the block that finally stores it covers the original rather than a copy numbered
above every watermark — the alternative replays the same frames at every boot,
on a log that never truncates. The manifest-free property survives intact: the
new state is a fifth field in a directory name, four-field names from before the
log parse as `wal_hi = 0`, and boot is still three `readdir`s.

It is **on by default**, which it was not until the open-block query surface
landed. The thing that had to be repaired first was visibility, not durability: a
query reads sealed blocks, so acking on the log meant acking before the data was
findable, and section 11's free read-your-writes was gone. section 4 is that repair and it
is the precondition for this default.

**Read-your-writes without a coordinator.** The surface is one method,
`OpenSlot::fresh`, and what makes it exact is an ordering the pipeline already
had. `submit` acknowledges an export only after the job is in the flusher's
channel — `Wal::append_then` puts it there inside the log's mutex — so *every
acknowledged export is queued ahead of any request issued after it*. A reader
therefore does not need a counter, a clock or a watermark: it sends a request
down a side channel, and the flusher answers it only on a turn where both that
channel and the job channel are empty. FIFO does the rest. The answer is
`signal::Open` — a `Sealed` produced by `finish_cloned` rather than `finish`, so
the builder keeps accumulating — carrying the `(node, seq)` the block *will*
publish under. That pair is the whole dedupe rule: `block::sources` drops a
snapshot the instant a directory with the same pair appears, and the published
copy wins because it is at least as complete.

Three properties fall out of taking the snapshot from row zero in publish order.
Row `n` of the snapshot is row `n` of the eventual block, so a cursor handed out
over open data stays exactly valid across the seal — no rebasing, no second
identity for a row, and `e2e::a_cursor_taken_from_the_open_block_survives_the_seal`
is the assertion. Nothing on disk changes, so no reader assumption about
one-batch-per-table moves. And the copy is demand-driven: an idle node with
nobody querying it copies nothing at all, and a node being queried skips the copy
whenever the builder has not grown since the last one, which is the common case
under a live tail. The sidecars — `attrs::index`, `zone::index`, `bloom::build` —
are deliberately *not* built for a snapshot: they walk every attribute row and
the encode bench puts them at well over twice the cost of the append that put
the row there, and an open block is always scanned anyway, so building a filter
that will only ever answer "yes" is pure waste.

The cost when the flusher cannot be reached — its request queue full, or the task
gone — is a query answered from the previous snapshot instead of a fresh one.
That is overload or shutdown, and a query that waits its turn behind an
overloaded ingest path is the worse answer.

**Sharding, and what a shard may be keyed on.** The unit is the core, not the
resource hash. Resource cardinality in real fleets is bimodal — a handful of
huge resources carrying 90% of volume, plus a long near-idle tail — so hash
sharding gives a permanently hot shard *and* a small-file explosion in the tail.
Files per flush interval should be a function of core count, known at startup,
not of the customer's topology.

Each signal runs `ingest.shards` flushers, defaulting to *cores ÷ 2* and capped
at 16. Halved because a flusher is a **consumer**: the producers are the protobuf
decode and the runtime's own work, and giving every core a flusher leaves nothing
to feed them. The knob exists for the case where that count is a fiction.
`available_parallelism` does read a cgroup CPU *quota*, so the common container
is fine; what it cannot read is `cpu.shares`/`cpu.weight`, which is a relative
claim on contention and not a number at all, or a pod with no quota set on a
96-core node, which would otherwise start the capped sixteen flushers a signal
against the two cores it will actually get. `ingest.shards: 1` restores the single-flusher
behaviour exactly.

Four things had to move, and none of them is the one line "shard the flusher"
suggests:

- **Dispatch is first fit from shard 0, not round-robin.** `Ingest::reserve`
  walks the shards in order and takes the first `try_reserve` that succeeds. A
  node doing two exports a second therefore behaves exactly as it did with one
  flusher — one block per seal window, not `shards` nearly-empty ones — and only
  starts using the second shard at the moment the first one's queue stops
  draining, which is the moment the consumer's service time became the curve.
  Round-robin would have reintroduced the small-file explosion the paragraph
  above rejects hash sharding for, from the other direction. Ordering *across*
  shards is not preserved and does not need to be: two exports are two OTLP
  requests and the spec orders neither against the other. Ordering *within* a
  shard still is, which is what the carry rule in section 5 needs.
- **The sequence space is partitioned by stride, not by an allocator.** Shard
  *k* takes `resume + k`, `resume + k + shards`, and so on. A sequence only has
  to be unique, the residues mod `shards` are distinct, and the next restart's
  `resume` is above every stride — so a node that reboots with a different shard
  count is still safe. The alternative, a sixth field in the directory name,
  `parse_dir_name` refuses on purpose. `resume` is scanned once in `spawn` and
  handed to every shard, which is load-bearing: a shard that read the directory
  after a sibling had already published would resume one higher and its stride
  would land on the sibling's next number.
- **The WAL watermark stopped being `max(seq) + 1`.** Shards seal out of order,
  so the highest sequence in a block says nothing about the ones below it. What a
  block claims now is the oldest sequence of its signal that nobody has
  published and that is not in this block — section 9 has the protocol and the
  direction it is allowed to be wrong in.
- **`ingest.queue` became a total, not a depth.** Each shard gets
  `queue.div_ceil(shards)`, so raising the shard count does not multiply the
  worst-case resident cost the way it would if every shard took the configured
  number. The arithmetic in section 11 about `--queue 2048` is about the sum.
- **Health counters became per shard.** `open_since` and `stalled_since` are the
  *oldest non-zero* of the shards', because the question `/healthz` asks is "is
  anything stuck" — with the flushers writing one aggregate directly, a shard
  sealing normally would clear the clock of a sibling sitting on a block it could
  not flush.

Read-your-writes survives, and not by luck. The argument in the previous section
never depended on there being one queue: an acknowledged export is in exactly one
shard's channel until that shard appends it, and each shard answers a `fresh`
request only on a turn where both its channels are empty. `OpenSlot::fresh` sends
every ask before awaiting any answer — awaiting them in turn would put a whole
flusher's backlog between one shard's answer and the next one's question — and
returns one open block per shard. `search_open` already took a list.

`sweep_staging` also moved out of the flusher and into `spawn`, once per signal:
it filters by signal and node, not by sequence, so shard 3 booting a moment late
would have deleted the staging directory shard 0 was already writing tables into.

**Decoder affinity, when OTAP lands.** OTAP section 4.4 mandates decoder state per
(gRPC stream, payload_type, schema_id), strictly ordered. That is
connection-affine by construction. The OTAP receiver will decode on the
per-connection task and push `Arc`'d Arrow buffers onward — which is another
reason the "one global lock-free ring buffer" shape was wrong.

---

## 5. Flusher state machine

One task per shard, one open block each, three transitions:

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
which is a footprint regression on one of the four axes. The open-block read
surface (section 4) was the obvious reason to revisit that and did not need to:
`ArrayBuilder::finish_cloned` materialises a column without consuming its
builder, so a snapshot is one buffer copy per column and the accumulation shape
is untouched. A `Vec<RecordBatch>` would have paid the same copy *and* given the
reader a multi-batch table it has never had to handle.

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
| `trace_id` | the record was traced | exact; section 7.4 |
| `span_id` / parent | the record names a span | exact |
| span link | async or fan-in causality | `span_links` table (with traces) |
| exemplar | a metric datapoint sampled a trace | exemplar `trace_id` (with metrics) |
| **entity + time** | **always** | `resources.key`, section 7.2 — written at seal, not yet selectable; section 7.4 |

The ladder matters more than any single rung. An investigation that starts at an
untraced error log gets nothing from the first four, and *everything* from the
fifth. Degrading from "the exact trace" to "everything this pod emitted in the
surrounding five seconds" is the difference between a correlation feature and a
correlation demo.

The fifth rung is the one the read surfaces do not reach yet. What answers that
question today is an attribute predicate on the identifying attribute itself —
`{"attr": "service.instance.id", "eq": "…"}` and a time range — which the engine
does answer, over `attr.idx` (section 7.4). What it does not do is decide *which*
identifying attribute the producer set, which is the whole of section 7.2.

### 7.2 Entity identity — `resources.key`

`resource_id` is block-local by design (section 0), so it cannot be the cross-block join
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
identity*, and the entity expander (section 7.3) must refuse it with an error naming the
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

### 7.3 The frame algebra — built, smaller than designed

`mira_core::frame` is `Frame`, `anchor`, `expand` and `map`, served at
`/api/v1/correlate`, `/api/v1/map` and `/api/v1/entities`, and reached from all
three surfaces: three MCP tools (section 8.1), the browser's Frame panel and Map view
(section 8.2), and the TUI's `c` and `m` (section 8.3). What shipped is three expanders where
this section designed seven, and no `fetch` at all. Both cuts are recorded below,
next to the design they cut.

A **frame** is a bounded region of telemetry:

```
Frame {
    time:     [from, to)      // nanoseconds
    entities: {u64}           // resource keys;  empty = unconstrained
    traces:   {[u8;16]}       //                 empty = unconstrained
}
```

The sketch had a fourth member, `spans: {[u8;8]}`. It is not there: the one
question it was for — *this span and its children* — is what a `trace_id` query
already answers, and a set nothing reads is a set nothing keeps correct.

Every correlation operation is `Frame → Frame`, and the algebra holds nothing
else. That closure property is the whole point: an investigation is a walk over
frames, every intermediate state is a legal query, and there is no way to
construct something that is not executable.

| expander | reads | writes | |
|---|---|---|---|
| `Traces` | `traces` from matched rows | widens `traces`, widens `time` to the traces' own extent | built |
| `Peers` | `traces → resource_id → key` | widens `entities` to everything that shared a trace | built |
| `Around(d)` | — | widens `time` by ±d, keeps `entities` | built |
| `by_span` | `spans`, parent/child | widens `spans` | cut |
| `by_link` | `span_links` | widens `traces` | cut |
| `by_exemplar` | metric exemplars | widens `traces` | cut |
| `by_entity` | `resource_id → resources.key` | widens `entities` | cut |

Three, not seven. `by_trace` and `by_span` are both what a `trace_id` query
already does; `by_link` and `by_exemplar` are edges the row itself carries out to
the caller, so an agent that wants them follows them with the tool it already
has; `by_entity` is `anchor`'s own output. An expander with no caller is an
expander with no test, and the closure property that makes the algebra worth
having does not need seven operations to hold — it needs every operation to
return a frame, which three do.

`Around` is ordered, not commutative with the others, and the walk is applied in
the order given for that reason: `Around` after `Traces` widens the measured
extent of the traces, `Around` before it is overwritten by the measurement.

`Peers` is the one worth calling out: it answers *"which other services were
involved in the traces this pod took part in, in this window"* as a two-hop join
over data already in the block. That is a service map, computed on demand, with
no service-map to maintain, no metrics-generator sidecar, and no second write
path. Compare Tempo, which runs a separate metrics-generator writing to a
separate Prometheus to answer the same question.

Materialisation was designed as `fetch(frame, signals) → rows`, and each
predicate is individually cheap:

- `time` → directory-name pruning, **before any file is opened**.
- `entities` → section 7.2, one small table per surviving block.
- `traces` / `spans` → section 7.4.

There is no `fetch`. A frame's members are exactly the terms a query document
already takes — a time window, a `service.name`, a `trace_id` — so `fetch` would
have been a second spelling of section 7.6's query, with a second predicate evaluator
to keep in step with the first. `correlate` returns the frame; the caller renders
it with the query it was already using. That is one read path, and it is why
`anchor` evaluates the caller's predicate with `query::search_open` rather than
its own: *the frame around what I am looking at* is only true if what I am
looking at is decided by the same code.

Within a block rows are in arrival order, which is near-time-ordered but not
guaranteed, so the residual time filter is a full compare over one `i64` column.
That runs at memory bandwidth and is not worth a sort at seal time.

### 7.4 Indexes: what is needed and what is not

- **Time** — directory names. Built.
- **Entity** — `resources.key`, written at seal and read by nothing. The write
  half is built; no read surface exposes an entity selector, so the key-set cache
  that would make one cheap — tens of `u64` per block, ~4 MB for ten thousand
  blocks, and therefore no on-disk filter warranted — is not written either. The
  order is deliberate: a key has to be in the blocks before a reader can use it,
  so one written today is answerable across the whole retention window on the day
  a reader lands, and one written then is not.
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
    **Logs blocks carry the same filter**, over their own `trace_id` column.
    A trace investigation is two questions — the spans, then the logs written
    under them — and the second has no more of a time bound than the first. With
    the filter on only one side, `get_trace` read one block and *"the logs for
    this trace"* read all of retention, which is the slower half of the pair and
    the one a human waits on. The column is null for logs outside a trace and
    those rows index nothing, so a block of untraced logs writes no filter and
    pays nothing.
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
  query layer searches every one of them and a filter that missed one would prune
  blocks holding real matches. It is built from the schema, not from a list of
  table names, so a new level cannot be forgotten into a correctness bug.
  Sized by distinct pairs, it is **70 bytes per block** on the load generator's
  corpus and grows only where cardinality is high — which is where it prunes
  best. Past a million distinct pairs it writes nothing and the block is scanned.

  **The subtle part is what gets indexed.** A query scalar is compared against
  whatever type the SDK happened to store: `{attr: http.status_code, eq: "200"}`
  matches a stored integer `200`, because the comparison parses rather than
  making the caller know the SDK's choice (section 7.6). A filter over the *typed* bytes
  would disagree with that rule and prune the block holding the row — not a slow
  query, a row that silently does not exist. So the indexed key is the value's
  **decimal text**, which makes one probe cover every type whose arm can parse
  it. Doubles are the exception and are not indexed at all: `200`, `200.0` and
  `2e2` are one number and three strings. The block sets a `HAS_DOUBLE` flag
  instead, and any query whose value reads as a number scans a block that has
  them. Blocks with no float attributes — most of them — pay nothing for the rule.

  `canon()` is where that text is decided, and it is the *only* place: the
  comparison in `attr_matches` is written against it rather than the other way
  round, so the two cannot drift into a false negative. Every scalar has a text
  form there, including a fractional double — a producer is free to send `0.5`
  as the string `"0.5"` — and having `canon()` answer "unindexable" for that
  case meant every fractional-double equality scanned every block in retention.

- **Ordered comparison** — a `zone.idx` sidecar, **built**. A Bloom filter
  answers exactly one question, *"is this value in this block"*, and an ordering
  has no value to hash. *"Spans slower than a second"*, *"anything that returned
  5xx"*, *"a queue depth over ten thousand"* — the questions a tracing UI is
  actually opened for — therefore read every block in retention no matter how
  few rows came back. So a second sidecar, holding one `(min, max)` pair per
  numeric thing the block contains: every numeric column of the root table, and
  every attribute key with a numeric value at any level. A term whose interval
  cannot reach the block's is a block not opened, for a few hundred bytes read
  and a binary search — **776 bytes per traces block** and 656 per logs block on
  the load generator's corpus, against tens of megabytes of block body it
  decides not to fault in.

  It is not the Bloom filter with different arithmetic. Three of the differences
  are correctness, not tuning:

  - **A key the map does not hold prunes the block.** A Bloom filter's "no" is
    exact and its "yes" is probabilistic; here it is the *absence* of an entry
    that is load-bearing, so the map has to be complete over the block or it
    silently loses rows. That is why `zone::index` is driven off the schema
    exactly as `attrs::index` is, and it is a different thing from writing no
    file at all — a block that exceeds the 4,096-key cap, or one published
    before this existed, has no `zone.idx`, and no file reads as "scan me".
  - **Text that parses as a number is a number.** The rule from section 7.6 again:
    `attr_matches` parses a `str`-typed attribute before an ordered comparison,
    because half the SDKs send `http.response.status_code` as text. A range
    built from the `int` and `double` columns alone would prune away the block
    holding `"503"`.
  - **Text that does not parse falls back to a lexicographic compare**, which no
    interval over the reals describes. Any key holding one of those degrades to
    an unbounded range — the entry stays and answers "maybe" to everything. It
    costs the pruning on that one key rather than the correctness of the file.

  Two intervals per key, `i64` beside `f64`, because the query layer compares
  integers as integers: an `i64` past 2⁵³ rounds when it becomes a double, and
  rounding a *max* down is precisely the false negative this is not allowed to
  produce. 32 extra bytes on entries that number in the dozens.

  Only an *unquoted* number probes it. A quoted scalar means a text comparison
  against a `str` attribute and a numeric one against an `int` column — two
  orderings, and this file describes one. Since an absent key prunes, a probe
  describing the wrong ordering would not merely mislead, it would drop rows.

  Measured over 1.02 GiB / 3.0M spans / 46 blocks, asking for spans slower than
  10 ms when the slowest in the corpus is 8.6 ms — the ordering equivalent of
  the absent-attribute case above, and the one with no early exit, because
  `limit` never fills and the negative has to be proved against all of
  retention:

  | | blocks scanned | rows scanned | warm |
  |---|---|---|---|
  | without | 46 | 3,014,656 | 81.6 ms |
  | with | 0 | 0 | 2.3 ms |


  What it cannot do is show up on a synthetic corpus that has values in it. A
  zone map prunes on *correlation between value and time*, and `loadgen` draws
  its durations from a fixed distribution, so every block's range is the same
  range and an ordering that lands inside it opens all of them. Real telemetry
  is not like that — an incident is a span of timestamps where the numbers
  changed — but a load generator cannot demonstrate that, and a number
  manufactured to make it look like it can is worse than no number.

**Why not sort blocks by `trace_id` instead?** There is exactly one physical
order, and every query has a time bound while only some have a trace bound. Time
wins.

### 7.5 Why the frame algebra is the agentic surface

The algebra *is* the MCP tool set: `correlate` is `anchor` plus a walk,
`service_map` is `map`, `list_services` is `entities`, and they sit beside the
four record tools as section 8.1's eight. An agent asks for the frame around a
predicate and gets back a bounded region it can page through with the tools it
already has — no `fetch`, for section 7.3's reason.

The argument for having built it is what the alternative costs. An agent handed SQL
over a five-table star schema with EAV attribute tables will write wrong joins —
silently wrong, because a missing `parent_id` predicate returns a cross product
that looks like data. An agent handed ten closed operations cannot express a
wrong join at all, and every call returns a frame it can bound before
materialising. That is the difference between an investigation loop and a
timeout.

### 7.6 Query, outside correlation

A predicate on an attribute is a **relational semi-join**, not a column filter:
filter `log_attrs` on `(key, active-value-column)` → collect the `parent_id` set
→ semi-join into `logs.id`. arrow-rs ships no join kernel (`arrow-select` has
`filter`, `take`, `interleave`, `concat`, and no join), so this is a hand-written
one. `attr_parents` walks the attribute table once per term and returns the
parent ids that matched; `attr_rows` scatters those into a `Vec<bool>` over root
rows, which the scan then reads by index. No hash set anywhere, which is only
possible because ids were rebased at ingest (section 0) and are therefore dense
from zero — resource and scope go through a second boolean array of the same
shape rather than a join, because entity ids number in the tens. It covers all
six value columns and every attribute level.

**Every level means the span's children too.** Record, resource and scope are the
obvious three; a span's `events` and `links` carry attributes of their own, and
those are where the fields anyone actually searches for live.
`Span.recordException` — the one API call behind most of the spans someone goes
looking for — writes `exception.type`, `exception.message` and
`exception.stacktrace` onto an *event*, so "which spans threw a
`NullPointerException`" is a child-level filter with no span-level equivalent to
fall back on. Leaving them out gave the worst shape a search can have: the row
came back from an unfiltered query with the value plainly visible under
`events[].attributes`, and a filter for that exact key returned nothing. The row
a child match selects is the span the child hangs off, which is what the caller
asked for; the hop is a `parent_of_id` array built once per block, the inverse of
the `by_parent` index emission already uses, because child ids are dense from
zero per block and so the id is the slot. Note that a child id is *not* a root
row number — every other join in the query layer is exactly that, which makes
treating it as one the obvious mistake, and one that passes any test where each
span has a single event.

**Responses render 64-bit integers as JSON strings**, which is principle 3
deciding a question that would otherwise be decided by taste. OTLP/JSON writes
`int64`, `uint64` and `sfixed64` as strings; section 0 already records that the ingest
decoder has to read them that way, and a response Mira cannot feed back to itself
as a request body is not a round trip. The mechanical reason is the same in both
directions: a JSON number is an IEEE754 double to a browser and to most parsers,
`time_unix_nano` is ~1.7e18, and 2^53 is where a double stops counting — a bare
number does not fail there, it silently rounds. Only the 64-bit columns are
quoted. A `severity_number`, a `status_code` or a `dropped_*` is 32 bits or
narrower, fits a double exactly, and a reader doing arithmetic on it should not
have to parse it first. The cost lands on the readers, and it is worth being
precise about where: a string renders, sorts and keys correctly as it stands, so
only arithmetic pays — the chart's pixel math, the waterfall's bar geometry, a
clock. The browser was written against the wire and converts at exactly those
points. The TUI was not, and `Yaml::as_i64` answers `None` to a string: behind
each `unwrap_or(0)` the log clock sat at the epoch on every row, every waterfall
bar started at t0 with zero width, and the metrics pane drew `no points` over a
series with four hundred of them. Nothing errored, and the unit tests spelled
their fixtures bare and agreed with the bug. That is the failure mode of this
decision — never a parse error, always a plausible zero — so a reader gets one
coercion helper and its fixtures get the wire spelling.

**Comparison dispatches on the stored type, not the query's**, because the SDK
chose the type and the caller should not have to know which. `eq: "200"` finds a
stored integer and `eq: 200` finds a stored string, symmetrically. Making that
symmetric is more delicate than it looks: on a string column, equality is
*defined* as equality with `canon()` — the same decimal text section 7's filter indexes
— and not as "both parse to the same number". The looser rule would match the
stored string `"200.0"` against `eq: 200`, and the index, which holds only the
text `"200.0"`, would have pruned that block away first. A sidecar is allowed to
return blocks that hold nothing; it is not allowed to hide one that does.

Ordering is the exception and *is* numeric on a string column, because ordering
is not in the index — `attr_probes` takes only `Op::Eq` — so there is nothing
there to disagree with. It has to be numeric: lexicographically `"1000"` sorts
below `"400"`, so `gte: 400` would otherwise mean one thing against an SDK that
sends the code as an integer and another against one that sends it as text. A
quoted query scalar still gets a text comparison — asking for `"99"` is asking
about the string.

**Paging is keyset, and there is no `offset`.** A response carries `next` when it
filled `limit`; passing it back as `after` continues where it stopped, and its
absence means that was the last page — a reader never asks for an empty page to
find out it is done. The cursor is `ts.node.seq.row`: the row's timestamp, the
block's node id and sequence, and the row's index inside it. Every component is
intrinsic to the record, so the cursor survives blocks being written, flushed and
retired between two pages — which is exactly what an offset does not. On a live
store `offset: 20000` shifts under the reader whenever a batch lands, so it sees
a row twice or never; it also forces the engine to find and discard twenty
thousand rows before the ones it wants, turning section 7's early exit into a full scan
and making the last page the most expensive one. Here the opposite holds: `after`
prunes whole blocks by `min_ts` before any of them is opened, so page five
touches fewer blocks than page one.

The sort key is total on purpose. Rows sharing a nanosecond are routine — one
batch, one clock read — so a cursor of "the last timestamp I saw" would re-emit
or skip every row tied with it. `Reverse((ts, node, seq, row))` is the one
descending order, used identically by the sort, the truncate and the cursor
filter; if the sort used a shorter key than the filter, truncation could keep a
row that sorts *behind* one it dropped and the next page would repeat it.
`(node, seq)` identifies a block globally with no coordination (section 12), so this
holds across replicas as well as within one. Metrics do not page: `max_series`
and `max_points` cap what a chart can render rather than cut a list short, so
`next` is always absent there. Both caps bound the *scan*, not just the render.
`max_points` compacts a series in place once it holds twice the cap, and
`max_series` evicts the largest key from a `BTreeMap` as the scan runs, which
yields the same set `sort(keys).truncate(max_series)` did — a key that belongs in
the smallest `max_series` can never be the maximum of a map already one over. The
difference is when: accumulating first meant a `query_metric` with no `name`,
over a store with a request id in a data-point attribute, allocated about a
kilobyte per distinct series until the process died. What each cap refused is
reported — `dropped_points` per series, `dropped_series` in `stats` — because a
chart with a series missing and no way to know it is the failure mode both of
them exist to prevent. Neither does `get_trace` — a trace is one page or
it is a broken trace, and an agent handed a third of one will reason about the
third.

**Blocks are scanned in widening waves, into whatever cores are idle.** Blocks
are independent — the only state a block scan produces is its matches and its
row count, and both are merged afterwards — so the scan is embarrassingly
parallel and was not being parallelised. It is now, with two rules that are both
there because of a measurement rather than a preference.

The first is that the waves *widen*: one block, then two, four, eight, sixteen.
The commonest query there is — "the last hundred records" — is answered by the
newest block and exits, so a fixed-width first wave would read eleven more
blocks and throw them away, making the cheap case an order of magnitude dearer
to make the dear case faster. Doubling reaches full width after four waves and
fifteen blocks, which is noise against the scan this exists for. What it costs
is exactness in `blocks_scanned`: a wave that starts before the limit is reached
runs to its end, so the number is now "what the scan read" rather than "the
fewest blocks that could have answered". The two were the same when the loop was
serial and are within one wave of each other now.

The second is that the fan-out budget is **shared by the process, and never
waited for**. A search asks for helper threads and takes what is free; when
nothing is free it runs serially, with no spawn and no queue. That is not
politeness, it is what the numbers said. One search over 37 blocks and 19M rows
went from 1.81 s to 0.20 s — 9×, close to the core count. Eight concurrent
searches on the same twelve cores went nowhere: throughput flat to within noise,
and the short classes' p99 an order of magnitude *worse*, because inter-query
parallelism had already claimed every core and each extra thread was pure
scheduler work. A shared budget makes those the same code path with no mode to
pick and nothing to configure — the idle node fans out, the loaded one does not,
and neither is told which it is.

Threads, and not tokio's blocking pool, for the fan-out: the workers borrow the
mapping and the query rather than owning `Arc`s of them, which `std::thread::scope`
allows and a pool does not, and a block scan is milliseconds against a spawn's
microseconds so there is nothing for a pool to amortise. Query *concurrency* is
bounded one level up, in `api::scan`, and that bound is the fix for a real bug
rather than a tuning knob: every read hops to the blocking pool and so does
every `publish` fsync, so unbounded reads could take every thread tokio would
hand out and leave the flusher — the ingest path's durability barrier — queued
behind them. A slow query was an ingest stall. One permit per core keeps them
apart without a second runtime.

**A name a response hands out is a name the query accepts back.** A histogram or
a summary reads as two derived series called `<name>.count` and `<name>.sum`, and
those are the names a chart legend shows and an agent copies out of its own
previous answer — so the metric name filter has to take them, and it does, by
retrying against the base name once the descriptor dictionary misses. Exact match
runs first, so a metric genuinely called `foo.count` keeps winning its own name,
and the suffix is remembered so only that half of the histogram comes back rather
than both. The asymmetry a reader will hit: `names()` lists what the dictionary
holds, which is base names, so `<name>.count` is queryable and is not in the
list — the round trip is closed in the direction that loses data if it is open.

**"Vector matching" is settled**: it means cross-signal correlation, as above, not
the PromQL sense (`on`/`ignoring`, `group_left`). The PromQL reading would need a
full evaluator and a series-major on-disk layout — a different sort order from
section 3 — and is out of scope. This was the one open question that could have changed
section 3; it does not.

---

## 8. Read surfaces: agents and humans

Three of them — MCP, a browser UI, a terminal UI — and all three go through
`api.rs`'s parsers and `api::envelope`, so there is one query grammar to keep
correct rather than three that drift.

That grammar is closed at the top of a document as well as inside a term. A query
must be a mapping, and a top-level key the endpoint does not implement is a 400
naming it against the set that endpoint does. Leniency here has no upside and one
specific downside: `{"signal": "logs", "filters": [...]}` looks like a filter,
was indistinguishable from no filter at all, and answered 200 over the whole
window — a wrong answer that looks right, which is the only kind a caller cannot
see in the response it got.

### 8.1 Agentic surface

**Built**: `POST /mcp` on the same listener as everything else, JSON-RPC 2.0 over
Streamable HTTP, eight tools — `query_records`, `get_trace`, `query_metric` and
`list_metrics` over the query document, `correlate`, `service_map` and
`list_services` over the frame algebra (section 7.3), and `list_alerts` over the alert
evaluator (section 13). Hand-rolled rather than `rmcp`: the protocol at this scope is a
method dispatch over a JSON document, we already parse those (section 1, KYAML), and the
SDK's session model is the thing we specifically do not want.

Two decisions worth keeping:

- **No `Mcp-Session-Id`.** Streamable HTTP permits a server to hand out a session
  id and then require it on every later request, which makes the server a thing
  with memory that a load balancer must route back to. Issuing none is principle
  4 applied to the agent surface: any replica answers any request, and killing one
  loses nothing.
- **The same read path as the UI.** The tools call `api::search_doc`,
  `api::series_doc` and `api::window_doc` — the same parsers the HTTP API uses —
  and the same `query`/`series` functions, through the same `envelope()`. A separate
  "agent API" is a second read path to keep correct, and the first thing it does
  is drift.

`get_trace` exists as its own tool rather than as a `trace_id` term because the
default window is one hour: an agent handed yesterday's trace id would otherwise
get an empty result with nothing to explain it. It searches all of retention, and
section 7.4's block filter is what makes that affordable.

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

### 8.2 The browser UI

Svelte, built to `crates/mira/ui/dist` and `include_bytes!`'d into the binary,
served from `/` and `/{file}` on the same listener. Nothing to deploy beside the
binary and no CORS, because it is the same origin as the API it calls. The
wildcard is one path segment deep, so it cannot swallow `/v1/*`, `/api/v1/*` or
`/mcp`.

`dist/` is checked into git so `cargo build` never needs node; `npm run build` is
a step taken before committing a UI change. A `build.rs` that shelled out to npm
would make every Rust build depend on a JavaScript toolchain to produce bytes
that did not change. Freshness is an ETag over the bytes, not a hash in the URL,
so an unchanged bundle costs three 304s — one per asset — instead of 60 KB.

Owning the UI follows from owning the API. Mira's query shape is not PromQL, not
LogQL and not SQL — the query document of section 7.6 is the interface — so an off-the-shelf
frontend would have to be taught it, and teaching it means shipping and versioning
a second thing.

The frame algebra surfaces as a Frame panel above the records and a Map view
beside them, and both are built so that **every line leads back into a query**: a
service in the frame ands a `service.name` term into the filter box, a trace opens
the waterfall, a node on the map filters to it. A correlation view that is only a
picture is a dead end, and the algebra's closure property is what makes the
alternative free.

Two smaller ones. The frame panel's state lives in the URL (`?frame=1`) rather
than in the session, because a link to an investigation has to carry the
investigation. And Live is a refetch on a three-second timer, not a stream: every
query here is a millisecond-scale read over mmap'd blocks, and a WebSocket would
be a second protocol, a second server path and a reconnection state machine to
buy back a few hundred milliseconds.

### 8.3 The terminal UI

`mira mira` renders the same three tabs, the same filter grammar and the same trace
waterfall in the terminal, plus the frame algebra on `c` and `m` and a live tail
on `f`. `mira tui` is kept as an alias, because it is what
someone types who has not read the usage, and answering that costs one `||` where
an "unknown flag" costs them a thought. Five decisions:

**No TUI framework.** ratatui is the obvious answer and costs **35 crates that
are not already here** — a 30% increase on a tree of 117, whose size is a stated
property of the product (section 1). That figure is this workspace's, not the crate's:
`rust-version = "1.85"` makes the resolver pick ratatui 0.29, and a project with
no MSRV gets 0.30 and 47 new crates instead, so raising the MSRV raises this
number with it. What ratatui buys is a constraint-solving layout engine and a
damage-tracked cell buffer; this UI has fixed panes and repaints one screenful
per keystroke. So `termios` for raw mode, `TIOCGWINSZ` for the size, `poll(2)`
for input, three `sigaction`s, ANSI for the rest — on `libc`, already in the tree
for section 9's `statfs` guard. Net crates added: **zero**.

The signals are what owning the terminal actually costs. SIGWINCH is installed
for one reason: its default disposition is to *discard* it, and a discarded
signal interrupts nothing, so without a handler a resize repainted nothing. The
handler itself does nothing — the delivery is the message, arriving as the
`EINTR` that sends the loop round to re-read the size. `SA_RESTART` is not set
and would change nothing if it were: `poll(2)` is on signal(7)'s never-restarted
list whatever the flag says, and the `read(2)` beside it runs under VMIN 0 /
VTIME 0, so it never sits in a restartable wait either. SIGTERM and SIGHUP are
installed so a `kill` or a dropped ssh session hands the terminal back rather
than leaving a shell in raw mode on the alternate screen, and that is why
`restore()` is async-signal-safe `libc::write` and not `io::stdout()`, whose lock
the handler may have interrupted its own thread holding.

The cost is `crates/mira/src/term.rs`: 1,127 lines, 534 of them before the test
module, and a `Row` type that tracks visible width separately from bytes, because
inline ANSI makes `len()` a lie. Unix only, which is the same bet `mmap` and
`SIGTERM` already make.

**Two transports, one code path.** `--addr host:4318` POSTs to a running replica.
`--data-dir` calls `mira_core::query` **in-process, with no server anywhere** —
which is the reason the TUI is worth building rather than being a smaller browser
UI. A detached PVC, or the volume of a pod that has already been killed, is still
readable. Both transports return the same envelope string, produced by the same
`api::parse_search` / `api::envelope`, so a filter that works against a server
works against a directory by construction.

**Responses are parsed with the KYAML loader.** JSON is a subset of KYAML and
`yaml-rust2` is already here for the config file (section 1, KYAML-first), so the binary
still has no JSON *parsing* dependency. A test drives the exact envelope shape —
escapes, control characters, nested attributes — rather than trusting the spec.

The whole thing is synchronous: no runtime, no task, no channel. A slow query
freezes the UI for its duration, which is why the frame is painted *before* the
query runs — the freeze always carries a "running" rather than a stale screen. The
tracing subscriber is not initialised on this path; one stray `info!` mid-frame
corrupts the screen.

**The frame and the map are modes, not tabs.** A fourth tab would need arms in
`signal`, `free_text`, `fields`, `reload`, `cursor`, `set_cursor`, the tab bar and
`go`'s `h`/`l` cycle; a mode needs six sites and gets esc-back for free, which is
what these two panes want — they are a detour from a list, the same shape `t` and
the waterfall already have. The map is drawn as a **tree**, not a graph: a
terminal draws trees well, and a tree makes every line something Enter can act
on. It is a depth-first walk from `entry` with children ordered by descending call
volume so the hot path reads first, a `seen` set so a retry cycle terminates, and
any node the walk never reaches appended flat at depth zero — a service omitted
from a service map is the one failure this pane cannot have.

**Follow is the key read timing out.** The loop is synchronous and blocks on
`poll(2)`, so a key that does not arrive within three seconds *is* the tick, and
live tail is a timeout instead of `-1` plus a re-query. No thread, no channel, no
second copy of the app state. Two details make it usable rather than merely
correct: the tick sets the job directly rather than calling `reload`, because a
status bar that strobes "running" every three seconds is worse than none; and the
cursor is clamped to the new row count rather than reset, because a selection
yanked to the top on every tick cannot be read.

### 8.4 The in-process read path, and why a local agent gets it for free

There is a fourth read surface, and it is the one with no protocol at all: open the
block directory and map it.

`mira mira --data-dir PATH` is the shipped consumer of it. It is the *same* binary
and the same query code as `--addr`, with the HTTP client swapped for a direct
call — `main.rs` picks between the two at startup and nothing below that boundary
knows which one it got. No server is running, no port is bound, and no bytes are
serialised: a query walks the directory listing, maps the blocks the time index
and the Bloom sidecars did not prune, and reads Arrow buffers out of the mapping
(section 3.4). The guards in `main.rs` are the whole of the setup — the path must exist,
must be a directory, and must not be on a filesystem where `mmap` can raise
`SIGBUS` (section 9).

This is not a convenience mode bolted onto the side. It falls out of three
decisions already made for other reasons, which is the argument for having made
them:

- The filesystem is the manifest (section 3.2), so there is nothing to ask a server for
  before you can read. A catalogue-based engine cannot offer this at any price:
  the catalogue lives in the process you are trying not to run.
- Blocks are immutable once renamed (section 5), so a reader needs no lock, no lease and
  no coordination with a writer that may be running concurrently — including a
  writer in another process. Retention is `unlink`, and an already-mapped block
  survives it (section 6).
- `mira-core` is a library crate. The reader is `mira-core`'s, not the binary's,
  so a Rust program that wants the same access depends on the crate and calls it;
  the CLI has no privileged path into the data.

**Why this matters for the agent case specifically.** An agent co-located with its
telemetry — in the same pod, on the edge node, inside the sandbox it is reasoning
in — pays three costs to read over HTTP that are pure loss at that distance: a
serialise/deserialise round trip on every result, a port and its lifecycle to
manage, and a process to keep alive between questions. The in-process path removes
all three, and the fact that it is the same code as the server path is what keeps
the two answers identical. A "local mode" with its own reader would be a second
read path, and the first thing a second read path does is drift (section 8.1).

What it deliberately does **not** do is write. Ingest is the server's; two writers
against one directory is the staging-path collision in section 12.6, and the in-process
surface stays read-only so that question never arises.

---

## 9. Durability and failure model

| Failure | Behaviour |
|---|---|
| Crash mid-block | Unacked exports are re-sent by the client (OTLP retryable set). Nothing on disk is torn — the block was never renamed. `.tmp` is cleaned on next publish. |
| Crash mid-rename | Directory rename is atomic. Either state is consistent. |
| Bit rot in a block | CRC32 mismatch on open → typed error, not wrong answers. |
| Truncated / non-Arrow file | `ARROW1` check → typed error. |
| Disk full | `publish` fails; waiters get `UNAVAILABLE` + `RetryInfo` on 4317 and `503` + `Retry-After` on 4318. No partial block is visible. |
| Reader holds a block being expired | Safe by POSIX unlink semantics (section 6). |
| **SIGTERM / SIGINT** | Stop accepting, let in-flight exports reach their ack, then close the flusher channels so each open block is sealed and published. Bounded at 15 s. |
| **Network filesystem** | **Not safe, and refused.** mmap on NFS/CIFS/CephFS raises `SIGBUS` with no recovery path. `block::check_filesystem` runs one `statfs` on the data directory before anything is mapped and fails startup with the filesystem named — by `f_fstypename` on macOS, by `f_type` magic on Linux. FUSE warns instead of refusing: the magic is the same for `gcsfuse` (fatal) and a local userspace filesystem (fine). A heap-read fallback was considered and rejected — it would silently delete the property the whole design is built on, which is a worse failure than not starting. |
| **Unwritable data directory** | **Refused, at startup.** `create_dir_all` returns `Ok` for a directory that already exists whatever its mode, so a `readOnly` volume mount or a wrong-uid path otherwise reaches a listening socket and fails one export at a time under load. `block::check_writable` writes and removes a pid-named probe file next to the `statfs` call. Both are the same bet: a startup that refuses is cheaper to diagnose than a server that half-works. |

**The status is the retry policy, because OTLP's retryable set is closed.**
`UNAVAILABLE` is in it and `INTERNAL` is not, so a conformant exporter handed
`INTERNAL` for a full disk does not wait for the disk to have room — it drops the
batch it is holding and reports the loss as permanent. Which status a failed
publish answers with therefore decides whether the data survives, and every
reason a publish can fail — no space, `EIO`, a volume that went read-only — is
transient by that test. A panicking flush never reaches the decision at all: the
release profile is `panic = "abort"`, so it takes the process down and the export
is retried against the restart, which is the same path as any other crash. The
one exception is an export that cannot fit an *empty* block: 70,000 distinct
attribute keys against a `UInt16` dictionary is not going to fit the next one
either, so it keeps `INTERNAL`/`500`. Telling a sender to keep trying something
that cannot work is worse than telling it the truth, and it is the only case
where the truth is permanent.

**The WAL watermark, and which way it is allowed to be wrong.** A block's fifth
directory field is `wal_hi`, and boot replays every frame at or above
`block::wal_watermarks`, which takes the maximum over the blocks of that signal.
The field is exclusive: a block holding frame 0 claims 1. Too high and the frames
it skipped are gone with the node that held them; too low and they are ingested a
second time, which costs duplicate rows that `block::sources` and the
`(node, seq)` dedupe rule will not catch because they are genuinely different
blocks. One of those is recoverable and the other is not, so every choice here
leans low.

`max(seq) + 1` was correct while a signal had one flusher, and stopped being
correct the moment it had several. Shards seal independently and out of order, so
a block whose highest sequence is 40 says nothing about 39 sitting in a sibling's
open block; publishing 41 would strand it. What the log tracks instead is a set —
`Wal::pending`, one `BTreeSet` per signal, holding every sequence handed out and
not yet published. `watermark_for(signal, seqs)` answers with the oldest member
of that set which is *not* in `seqs`, falling back to `next_seq` when this block
is the last of them. It is called before the publish, not after, so a sibling
sealing in the same instant still counts this block's frames against its own
answer and neither can claim the other's; `published` retires a sequence only
once the rename is durable, so a block that failed to land leaves its frames
holding the line for whoever seals next.

Two edges follow from the set being the authority. A frame read back by replay is
put *back* into `pending` by `Wal::reframed` before it is decoded, because a
shard that sealed in between would otherwise compute a watermark that stepped
over it — replay is precisely when the frames at risk are the ones a crash nearly
lost. And a frame that fails to decode is retired on the spot rather than left
pending: it will not decode on this boot or any other, and leaving it in the set
would pin the signal's watermark at its sequence for the life of the volume,
replaying every frame published behind it on every boot forever. `main::replay`
says out loud that those exports are gone, and the retirement is what makes that
sentence true.

The cost of leaning low is bounded by `max_block_age`: a shard holding an old
frame pins its siblings' watermarks behind it, and it is at most two seconds from
sealing.

**Probes are not the UI, and the listening line is not a promise.** `/health` and
`/readyz` are the same handler — Mira has no warm-up and no cluster to join, so
there is no state in which it is alive and not ready. `ingest.wal` does not
change that, though it is the first thing that could have: log replay is the one
startup task with unbounded duration, and it runs to completion *before* any
listener binds, so a probe during replay finds a closed port rather than a
process answering 200 about data it has not recovered yet. Connection refused is
the honest answer and it is free — answering 200 with the
per-signal shed and failed counts, so the probe and the log agree about how much
has been refused. They also exist so that something other than the UI answers at
those paths: the asset router 404s a path it does not know and deliberately has
no SPA fallback, because every view in the app lives under the URL hash, so an
unknown *path* is a probe pointed somewhere wrong rather than a route needing
rescue. Answering it with 200 and a page of HTML made every wrong guess at a
probe path report success. Both listen sockets are bound before anything logs
`mira listening`, for the same reason: during a CrashLoopBackOff the log is all
the operator has, and a listening line in front of a bind that then fails is
dishonesty rather than terseness — the error names the address and the likely
cause instead.

**Shutdown is about duplicates, not loss.** A hard kill loses nothing that was
acknowledged, because an ack *is* an fsync (section 4). What it costs is the other
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
large throughput cliff, and it is why a single-record export's 2 s ack (section 4) is
`max_block_age` rather than the fsync. Docker-for-Mac volumes can return
`EINVAL`/`ENOTSUP`, and std will not fall back — `os_fsync` and `os_datasync` in
`sys/fs/unix.rs` are a bare `fcntl(F_FULLFSYNC)` on any Apple target — so a
publish that a plain `fsync(2)` would have satisfied fails, and the node goes
unready over a volume that works. `mira_core::sync_all` and `sync_data` are the
wrapper: they try the barrier, degrade to `libc::fsync` on exactly those two
errnos, and count it. Every sync in `block.rs` and `wal.rs` goes through them.

The counter is the part that matters. A fallback nobody can see is silent
durability loss, which is worse than the failure it replaces, so
`/api/v1/stats` reports `degraded_syncs` — zero on a volume with write
barriers, and non-zero is an operator's evidence that the promise on this node
is `fsync`'s rather than `F_FULLFSYNC`'s. The terminal UI's node pane prints it
in the header, but only once it is non-zero: a line reading `0` on every healthy
node is a line the eye learns to skip.

---

## 10. What is deliberately not here

- **The frame algebra of section 7.3** — all of it. No `Frame`, no `anchor`, no
  expander, no `fetch`. A span query returns its links and a series returns its
  exemplars, so both out-edges of section 7.1 are readable, but the caller follows them
  itself and there is no operation that widens a region rather than answering a
  question.
- **A block cache.** Every query re-opens and re-CRCs every block it touches. The
  fix is a process-local `Arc<MappedTable>` map invalidated by `expire`. This was
  assumed to be the next big win and it is not: section 11 measures the per-block cost as
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
- **DataFusion in the default build.** section 1. Behind `--features sql` it is not
  rejected: 50.0 MiB and 271 crates is a real price, but it is paid only by
  whoever asks for SQL, and DataFusion 55 pins arrow v59.3.0 — exactly Mira's
  pin — so there is no second Arrow in the tree.
- **The `F_FULLFSYNC` fallback** (section 9). Not a weaker fsync — the opposite, and it
  was settled by measuring rather than by reading. On this machine
  `File::sync_all()` costs 4,230 us, `fcntl(F_FULLFSYNC)` 4,213 us and a bare
  `libc::fsync(2)` 28 us: `sync_all` *is* `F_FULLFSYNC` on Apple targets, so
  section 11's ack latencies are measured against the stronger barrier and nothing is
  owed for the ordinary case. What is missing is the Docker-for-Mac
  `EINVAL`/`ENOTSUP` path section 9 names, where std does not degrade and the durability
  loss would therefore be silent. The `statfs` guard beside it in that section
  **is** written: `block::check_filesystem`, called once before anything is
  mapped.
- **Hand-written SIMD on the decode path.** Protobuf varint decoding is
  inherently serial — each field's length says where the next begins — and
  branchy on wire type, so there is no vector formulation of the inner loop. The
  published SIMD protobuf work targets fixed-width packed repeated fields, which
  OTLP barely uses. Section 0 row 4 settled the adjacent claim: zero-copy ingest
  is impossible here because `prost` memcpies every string. Where SIMD would pay
  is scan-side predicate evaluation, and the route there is not intrinsics — it
  is keeping those loops autovectorizable, a tight loop over `&[i64]` with no
  bounds checks and no branches, which LLVM turns into NEON unasked.
- **`io_uring`.** Three reasons, in order. It is not a feature flag but a second
  runtime: `tokio-uring` has `!Send` futures and its own `start()`, so gating it
  means two spellings of every I/O path rather than a `#[cfg]`. It is blocked
  where Mira runs — Docker's default seccomp profile has denied `io_uring_setup`
  since the 2023 escape CVEs, and GKE Autopilot and most hardened clusters
  disable it outright, so performance would depend on whether the sandbox
  allows a syscall. And Mira is not I/O-bound: 190 MiB/s against an NVMe that
  does GB/s, on 1.75 of twelve cores. Revisit when a profile shows syscall
  overhead above ~5% of ingest CPU, or when the log's group commit measures
  submission-bound rather than device-bound.
- **The query-side half of `NO_IDENTITY`.** The sentinel is written (section 7.2) and
  the expander that must refuse it is not — not because the query layer is
  missing, it is not, but because no read surface exposes `resources.key` at all
  (section 7.4). There is nothing yet for the sentinel to be refused by.

---

## 11. Performance model

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
back to back in the same sitting; the cold-tier table further down is the same
1,652 tables.

Six flushers per signal rather than one (section 4) changed the *shape* of that
corpus as well as the rate that produced it: 137 log blocks of ~204,800 rows
where one flusher sealed 87 of ~330,000. Same bytes, six sealers, so a block is
smaller and there are more of them — which matters below, because what a query
pays per block turns out to dominate what it pays per row.

Scoring them together is the point, and it is why the generator that produced
this table is also the load harness: `loadgen --readers N --pid N --data-dir P`
reports ingest, six classes of query latency, the server's resident set and the
bytes it added per record from one run. An engine measured one axis at a time is
an engine that is fast at whichever one its authors were watching.
[End-to-end testing section 3](internals/e2e.md#3-the-load-harness) is how to drive
it and what it teaches.

Read the two query columns carefully. **Neither is a cold-disk number**: 8.33
GiB fits in this machine's 18 GiB of page cache, so after one pass everything is
resident and short of `purge` there is no way back. "First" is the first call
after a process restart — the pages are in RAM but not in this process's address
space, so it measures establishing 316 blocks' worth of mappings and faulting
them in. "Steady" is the same call repeated. The gap between them is
virtual-memory work, not I/O.

| Axis | Target | Measured | |
|---|---|---|---|
| Ingest throughput | ≥ 1 M records/s/core | **886k records/s/core** — 629,384 records/s on 0.71 cores; **1,350,502 records/s** aggregate at four connections and a plateau peak of **1,537,875** at thirty-two, on 1.75 and 2.23 cores, nothing shed at any shape | ~ |
| Resident footprint | ≤ 2 × the open block's target size | **232 MiB** at one connection, **689 MiB** at four, **1,648 MiB** at 96 — see below | ~ |
| Ack latency | — | p50 **8.5 ms**, p99 **55 ms** at four connections, log on; p50 **657 ms**, p99 **2,647 ms** with it off | see below |
| Query: attribute value, absent | ≤ 10 ms | **8.3 ms** first, **2.6 ms** steady, 0 of 137 blocks | ✓ |
| Query: attribute value, matching | ≤ 10 ms | **4.1 ms** first, **4.5 ms** steady, 1 of 137 blocks, 204,800 rows | ✓ |
| Query: unfiltered `limit 100` | ≤ 10 ms | **29.1 ms** first, **4.6 ms** steady, 1 of 137 blocks, 204,800 rows | ✓ |
| Query: trace by id | ≤ 10 ms | **8.3 ms** first, **4.7 ms** steady, 2 of 155 blocks, 73,728 rows | ✓ |
| Query: metric names | — | **8.9 ms** first, **4.7 ms** steady, 24 of 24 blocks | — |
| Query: substring, no time bound, prunes nothing | — | **1,441 ms** first, **885 ms** steady, 137 of 137 blocks, 27.1 M rows | see below |
| Cost per GB ingested | ≤ 0.35 B/B | **1.20 B/B** hot, **0.14 B/B** compacted | ✓ |
| Binary size | ≤ 20 MB stripped with UI + query + MCP | **5.63 MiB** / 117 crates | ✓ |

Reading these honestly:

- **The ingest row is per core, and that is the denominator to argue with.**
  Aggregate throughput is a property of the offered load: raise `--conns` and it
  moves without a line of the server changing. Per core is measured rather than
  inferred — the harness takes the server's CPU-seconds either side of the run,
  so 0.71 and 1.75 are consumed CPU over wall clock and not a core count
  somebody chose. Two things fall out of it. The server is **not CPU-bound at
  any shape measured here**: the fastest row, 1,537,875 records/s at thirty-two
  connections, costs 2.23 of twelve cores, and the per-core rate is *highest* at
  one connection — 886k records/s/core, where there is nothing to contend over.
  And throughput **plateaus** between sixteen and thirty-two connections rather
  than peaking at a point — the two are within 2% of each other on medians whose
  passes span 6% and 16% — and then falls away to 1,136,941 at 96. That is a new
  shape: before `ingest.shards` (section 4) the curve peaked at four connections
  and declined from there, and 96 returned 734,142. Ten idle cores at the plateau
  means the ceiling is somewhere other than the engine's arithmetic. This run does not say where, and it
  cannot: the generator is co-resident and encoding 8192 protobuf records per
  batch is inside the same loop, so part of the per-batch cost is the harness's.
  Separating them wants the generator on a second machine, which is the one
  thing a single-laptop harness cannot do. Until then, treat the aggregate as a
  floor and the per-core figure as the comparable one.
- **What changed the shape of the curve was admission, not arithmetic.** The
  first revision shed the moment the queue was full, and at 96 connections that
  read 333,373 records/s with 93% of exports getting a 503 — four cores busy
  doing work that was then thrown away, because tonic and axum both decode a
  request before the handler sees it. `ADMIT_WAIT` (section 4) parks a full
  queue for up to five seconds instead: same shape, same queue depth, 578,294
  records/s and **nothing shed**, on 1.61 cores instead of 3.85, with the ack
  p99 down from 12.4 s to 3.2 s. Those two figures are a paired A/B from one
  sitting and should be read only against each other — the published
  96-connection row is the later three-pass sweep's 1,136,941, measured against
  sharded code on a quieter box. The bound is the connection count — every
  waiter is a request already in memory — where a deeper queue is bounded by
  nothing. Measured against that alternative: `--queue 2048` also removes the
  shedding, by buffering ~20 GiB of anonymous memory on an 18 GiB machine. That
  arithmetic is unaffected by sharding and that is deliberate: `ingest.queue` is
  a per-signal *total* divided across that signal's shards, not a depth each
  shard gets, so the worst case is the same number of resident exports whether
  one flusher holds them or six do.
- **The two ack rows are two chosen contracts, not a fast path and a slow one.**
  With the log on — the default — the ack is a `write(2)` into the page cache,
  and `cargo bench -p miradb-core --bench wal_bench` prices that write on its own
  at p50 7 µs / p99 39 µs for a 4 KiB body and p50 0.24 ms / p99 4.6 ms for a
  1 MiB one. What a client sees is larger than the append, because it includes
  the decode and the queue: p50 8.5 ms, p99 55 ms at four connections.
  With the log *off* the run is block-seal-bound, which is what p50 657 ms and
  p99 2,647 ms say — the block filling, then a durable publish landing in front
  of a waiter. Neither costs read-your-writes anything: the open block is
  queryable (section 4), so the choice is purely about what survives power loss.
  The same bench prices the road not taken, `append + fsync` on the ack path, at
  p50 4.1 ms and 0.9 MiB/s for 4 KiB bodies.
- **Per core, with no I/O in the path**, `cargo bench -p miradb-core --bench
  encode_bench` decodes and appends 1.09M log records/s (203 MiB/s of wire
  bytes), 1.06M spans/s (206 MiB/s) and 2.86M data points/s (254 MiB/s) on one
  thread. Those are medians of ten runs on a desktop with the usual desktop
  things on it, and the spread was 851k–1.18M for logs, 842k–1.11M for spans and
  2.55M–3.09M for data points — read them as "about a million records a second
  per core", not to three digits. The split inside them is the useful part:
  appending a log record costs 0.158 µs and *sealing* it costs 0.372 µs, so a
  seal is well over twice the cost of the append it finalises. Almost all of that is sidecar
  construction — `attrs::index`, `zone::index` and `bloom::build` each walk every
  attribute row — which is why section 4's open-block snapshots skip them, and why the
  32 MiB block target is a read-path decision the write path can afford.
- **Resident footprint is the axis with no number, and saying so is the point of
  scoring them together.** The harness reports peak RSS, and it moves by a
  factor of seven across the ingest sweep alone — 232 MiB at 1 connection,
  314 MiB at 2, 689 MiB at 4, 1,243 MiB at 8, 1,648 MiB at 96 in
  [End-to-end testing section 3](internals/e2e.md#3-the-load-harness) — which is the
  clearest evidence that
  RSS is not this row: it counts every mapped block page a query touched, and on
  the write side it covers three signals' builders plus every in-flight decode,
  not one open block. The harness's anonymous figure is closer and still not it:
  610 MiB against 660 MiB of RSS on a write-only run, against `3 × ingest.shards`
  open blocks whose target is 32 MiB each, so the residue is decode buffers and
  allocator arenas rather than block state, and dividing it by eighteen would be
  inventing an attribution.

  Sharding moved this row *down*, which was not the goal and is worth saying why.
  At four connections peak RSS is 689 MiB where one flusher needed 1,366 MiB, and
  the anonymous figure 745 MiB against 1,446 MiB. Six open blocks per signal is
  strictly more block state than one, so the saving is not block state: it is the
  queue. One flusher behind four connections keeps its slots full of decoded
  exports at ~1.29 MiB each; six flushers drain theirs, and an export that is
  never queued is never resident.
  What actually holds the bound is section 5's refusal of `concat_batches`, which is a
  property of the code and a test, not a measurement. A per-open-block
  anonymous figure needs an allocator hook the tree does not have, and adding
  one to score a row is the wrong trade: the bound is enforced where it
  matters — in the code — and this row stays honest about being an intention.
- **Blocks not opened is the whole game.** The two sidecar filters (section 7.4) took
  the block count from "all of them" to one or zero, which is worth between 60×
  and 1800× and is the reason these rows are in milliseconds at all. Everything
  below is about the cost of the blocks that *are* opened.
- **The per-block cost is opening the block, not scanning it.** This was
  previously written as "page faults, not the scan", which was the right shape
  and the wrong term, and `scan_cost_per_row` now prices both sides of it
  directly. Over two million rows, evaluating a predicate costs **0.047 ns/row**
  for no term at all, 0.485 for a dictionary equality, 1.064 for a resource
  attribute, 2.402 for a record attribute and **5.586** for the most expensive
  shape there is, a UTF-8 `contains`. The *same block through the whole read
  path* costs **24 to 25 ns/row**. So the scan is at most 22% of what a query
  pays, and for most predicates under 2%; the other ~20 ns/row is
  `Block::open` — the `mmap`'s minor faults, the dictionary scan, the two child
  indexes, and the CRC32 of every table body (section 3.3), which by construction
  touches every page. The clearest statement of it is that `limit 1` costs
  24.374 ns/row against the whole block's 25.430: asking for one row and asking
  for all of them are the same query, because the block had to be opened either
  way.

  The arithmetic closes on the last row of the table. A full scan CRCs the
  4,371 MiB of log blocks, and 4.58 GB in 885 ms is 5.2 GB/s, which is what
  `crc32fast` does on this machine. **The unpruned scan is integrity-check-bound,
  not scan-bound** — and that is a tradeoff rather than a bug, because the CRC is
  why a corrupt block is refused instead of served (section 3.3). What it is not
  is a vectorisation problem, which is what this section used to imply.

  The history is still worth keeping, because it is how the page-fault term was
  found: the last row was once **10.1 s** and did not improve on repetition,
  which ruled out disk. `mmap` faults 16 KB at a time and `open_table` touches
  every page anyway, so a multi-gigabyte scan took hundreds of thousands of
  single-page faults with no readahead, and one `madvise(MADV_WILLNEED)` at map
  time removed them.

  **The 175 ms once published for that row does not reproduce and has been
  withdrawn.** Four different full-scan predicates were run against this corpus
  on both the 0.0.1 binary and this one, and every one of the eight lands between
  0.96 s and 1.6 s over 27.1 M rows. The measurement now agrees with this
  section's own prediction — "multiply the per-block cost by the block count and
  the last row should be ~1 s" — rather than with the figure printed next to it,
  which is the direction an error of that size usually resolves in.
- **Sorting the match set to keep a hundred of it was the second lever.** A
  block scan produces every matching row, and the merge then ordered all of them
  before truncating to `limit`. For the query every session opens with — "the
  last 100 records", no predicate, so *every* row of the block matches — that is
  an O(n log n) sort of ~330 K hits to keep 100. `select_nth_unstable` partitions
  in linear time and only the surviving head is ordered; on the load harness
  ([section 3](internals/e2e.md#3-the-load-harness)), on the smaller store that A/B
  was run against,
  that took the whole read mix from 35 to 50 queries/s and `tail` p99 from
  230 ms to 139 ms.

  **The third lever was the attribute semi-join, and it is the largest of the
  three.** Section 7.6 replaced a per-row `attr_matches` with a predicate
  evaluated once per contiguous parent run, and on the corpus this section
  measures that is worth **6.7×** on a matching attribute value (30.2 ms steady
  to 4.5), **5.3×** on the unfiltered `limit 100` above (24.4 ms to 4.6), and
  **3.5×** on a substring that fills its limit. Across the eight-reader read mix
  it is 5.1× on the `attr` class p50 and 4.6× on `errors`, taking the mix from 40
  to 50 queries/s. Two classes did not move: `trace` is a `trace.idx` lookup with
  almost no rows to filter, and `series` is the metrics route, which came out
  slightly *worse* — 575–616 ms p50 before, 686–702 after. That is inside the
  spread of two passes and it is not a win, so it is printed rather than dropped.
  The mix's `tail` class matched nothing on this corpus, because the data is
  older than the window `tail` asks for, so its numbers measure the empty path
  and are not quoted here — the unfiltered `limit 100` row of the table is the
  honest version of the same question.

  What is left is not O(n) in the block's row count: `scan_cost_per_row` prices
  that term at 0.047 ns/row, so a 204,800-row block spends about 10 µs of the
  4.6 ms it takes. What is left is O(bytes) in the block's *size*, paid at open,
  which also means `target_block_bytes` is **not** the lever it was once written
  up as. Halving it halves the bytes each block CRCs and doubles the number of
  blocks, so a query that prunes to one block gets faster and a query that prunes
  to none gets nothing. It is not changed here because it only ever helped the
  first kind, the tradeoff runs the other way for compression ratio and directory
  size, and the number to tune it against is a workload nobody has yet. Sharding
  has already moved it in that direction by accident: six sealers per signal make
  a log block 204,800 rows where one made 330,000.
- **A block cache went from "worth much less than it looks" to the obvious next
  lever, and the measurement is what turned it round.** It was ruled a small win
  on the belief that the `open` and the CRC were the small part of a single-block
  query. They are not: they are ~80% of it, and on the unpruned scan they are
  effectively all of it. A cache that holds an opened block's validated mapping
  is the only thing on the section 10 list that attacks the term that actually
  dominates. What it cannot do is help a first touch, and it trades resident
  memory for it — which is the axis this section already scores worst.
- **Cost per GB is 1.20 B/B while a block is hot and 0.14 once it is
  compacted.** 164.0 bytes on disk per 136.9-byte wire record, and it is the
  steadiest figure in this section: across fifteen benchmark runs it moved
  between 1.195 and 1.199. Not the sidecars either way: all the `attr.idx` files
  of a store this size together are a few tens of kilobytes, a `zone.idx` is 776
  bytes on a traces block and 656 on a logs one, and the trace filters are
  single-digit megabytes against 8.33 GiB. What inflates the hot number is the
  `ATTRS` table, which carries six typed value columns and writes all six for
  every row — a string attribute pays 8 bytes for a null `int`, 8 for a null
  `double` and 4-byte offsets each for null `bytes`/`ser`, roughly 24 bytes of
  padding per attribute row.

  **Read that figure off blocks, not off `du`.** The harness's `storage` line
  counts the whole data directory, and the write-ahead log is reclaimed on a
  60-second tick (`wal_sweep`, section 4) — so a 30-second benchmark ends before
  the first reclaim and reports about 299 B/record, or 2.18x, most of which is a
  log that would have been gone a minute later. The same run with `ingest.wal`
  off reports 164 B/record and 1.20x directly, which is how these two figures
  were reconciled rather than argued about.

  That padding is also almost free to compress, which is what the cold tier
  (section 3.5) collects. Measured by `cargo run --release -p miradb-core --example
  tier -- <data-dir>` over the whole corpus — every one of the 1,652 tables, not a
  sample, and not a `zstd` CLI estimate either: it is the actual
  `write_table_zstd` path the sweep calls.

  | | plain | zstd | ratio | lz4 | ratio |
  |---|---|---|---|---|---|
  | `logs/log_attrs.arrow` | 1,871.8 MiB | 27.9 MiB | **67.1x** | 110.6 MiB | 16.9x |
  | `logs/logs.arrow` | 2,497.5 MiB | 465.8 MiB | 5.4x | 714.3 MiB | 3.5x |
  | **logs, 137 blocks** | **4,371.0 MiB** | **495.1 MiB** | **8.83x** | 826.2 MiB | 5.3x |
  | `traces/span_attrs.arrow` | 1,886.0 MiB | 38.0 MiB | **49.7x** | 111.6 MiB | 16.9x |
  | `traces/spans.arrow` | 2,244.1 MiB | 482.7 MiB | 4.7x | 601.8 MiB | 3.7x |
  | **traces, 155 blocks** | **4,132.0 MiB** | **522.2 MiB** | **7.91x** | 714.9 MiB | 5.8x |
  | **metrics, 24 blocks** | **5.0 MiB** | **1.0 MiB** | **4.87x** | 1.5 MiB | 3.4x |
  | **all 1,652 tables** | **8,508.0 MiB** | **1,018.3 MiB** | **8.36x** | 1,542.5 MiB | 5.5x |

  1.20 B/B ÷ 8.36 is **0.14 B/B**, comfortably under the 0.35 target. The two
  attribute tables are where it comes from and the reason is the padding above:
  a column of nulls is a run, and `log_attrs` compresses **67.1x** against
  `logs.arrow`'s 5.4x. The metrics ratio is worse mostly because that corpus is
  5.0 MiB — too small for per-buffer framing to disappear into the payload — and
  the tiny `resources` and `scope_attrs` tables actually get *larger* under zstd
  (0.96x) for the same reason. Compaction rewrites them anyway: skipping a table
  because it is 23 KB is more code than it saves bytes.

  Six sealers per signal did not move any of this, which is the answer to the
  obvious worry about sharding: 137 log blocks compress to 8.83x where 87 larger
  ones compressed to 8.84x. A block is smaller, but a run of nulls in an `ATTRS`
  column is a run at either size.

  Compression runs at **634 MiB/s** zstd and **777 MiB/s** lz4 on one core — over
  the whole 8,508 MiB, 13.4 CPU-seconds and 11.0, read and write included, so
  that is the rewrite path rather than the codec in isolation. It is sensitive to
  what else the box is doing, and these two were taken on a box that had been
  running benchmarks all day; an earlier quiet pass over a smaller corpus read
  808 and 976. A 32 MiB block is
  therefore ~40 ms, on a path already inside `spawn_blocking` and off the ingest
  critical path entirely: it is the retention sweep, an hour after the data
  landed. The `MAX_COMPACT_PER_SWEEP` cap of 8 blocks a minute exists for the
  first pass over an existing volume, not for the steady state.

  **The open question from the previous revision is answered, and the answer is
  the opposite of what the design assumed.** The worry was that inflating a
  compressed buffer into the heap would cost more latency than the pages it
  saves, and that the age threshold would therefore have to be conservative.
  `tier` now times the read back as well as the write. Over all 1,652 tables of
  this corpus: **9.3 s plain, 10.4 s zstd, 14.4 s lz4** — zstd is 1.11× the plain
  read. Three passes over the previous, smaller corpus put the same pair at
  5.4/7.4, 6.6/6.5 and 5.5/6.2 seconds, a spread of 1.00× to 1.38× that brackets
  it.

  These are **page-cache-warm** — the file was written microseconds before it
  was read — and warm is the half of the comparison that favours plain, because
  a resident plain block has nothing to fault while a compressed one still has
  to inflate. Even so the two are within the run-to-run noise of each other,
  which is the useful result: the inflate is real, and it is paid back by having
  8.4× fewer bytes to touch and CRC32 over 8.4× fewer of them. That second half
  matters more than it looked when this was written, because the CRC is now
  measured as the dominant per-block term rather than a small one. Cold — which
  is the case that matters, since a block is an hour old before it is compacted —
  the arithmetic runs further the same way, because the `MADV_WILLNEED` hint
  above is then faulting 8.4× fewer pages; an earlier measurement over a smaller
  corpus read 0.68 s plain against 0.54 s zstd on logs and 0.58 s against 0.26 s
  on traces. That one is not reproducible from here: `tier` cannot drop this
  machine's page cache, and 18 GiB of it against an 8.33 GiB corpus means nothing
  stays cold for long.

  So the cold tier costs the read path nothing measurable — only the zero-copy
  property, which is an allocation cost, not a latency one. The threshold is set
  at one hour for the reason in section 3.5, which is that it is the partition
  width and therefore not a knob; nothing in the measurement argues for waiting
  longer.

  LZ4 is the one clear loser and that settles a standing question. It is the
  pure-Rust alternative, and dropping `zstd-sys` would be dropping the only C
  dependency in the tree — but it compresses 5.52× against zstd's 8.36× *and*
  reads back slower in every pass. It costs on both axes, so the C dependency
  stays.

### What compresses and what does not

`schema.rs` makes two encoding choices that look arbitrary — `attrs.str` is the
only *value* column that is dictionary-encoded, and its index is a `u32` where
every other dictionary here is a `u16` — and three that are invisible because
they are things the schema does *not* do. All five were measured rather
than reasoned about, on real blocks from the corpus above at `ZSTD_LEVEL = 3`,
one table at a time so the effect is not diluted by the rest of the block.

**`attrs.str` as `dictionary<u32, utf8>` — kept.** On one `log_attrs.arrow` of
393,216 rows, the same table written with `str` as plain `Utf8` compresses
**12.9x**; as shipped it compresses **66.2x**. The dictionary also shrinks the
*uncompressed* table, 16.7 MB to 14.1 MB, so it pays before the codec runs.
Attribute values are where the repetition in telemetry lives — the same
`http.route`, the same pod name, the same status text — and a dictionary is the
encoding that says so explicitly instead of hoping a 128 KiB zstd window
rediscovers it on every buffer.

**`u32` for that dictionary's indices, not `u16` — kept, and it is nearly
free.** The same table at each index width: 212,290 compressed bytes at `u32`,
210,810 at `u16`, 209,442 at `u8`. Going from `u16` to `u32` costs **0.7% of
compressed bytes** and removes a whole failure class — `key` is `u16` and so
needs `DICT_CAP` and a seal-early rule to stay under it, and values, unlike
keys, are unbounded in principle.

**Dictionary-encoding the other string columns — rejected, it makes things
worse.** `logs.arrow` compresses 5.23x as shipped and **4.72x** with `body`
dictionary-encoded on top. `spans.arrow` is 4.56x either way with
`status_message` encoded. The asymmetry is the point: attribute values repeat
within a column, but a log body is mostly novel per row, and paying dictionary
overhead for a dictionary that never hits is a straight loss.

**Sorting a block by a low-cardinality column before sealing — rejected.** The
standard columnar trick, and here every key tried came out at or below the
unsorted ratio. Comparing like with like (dictionaries decoded, so the sort is
not fighting the encoding):

| table | unsorted | best sort key tried | worst |
|---|---|---|---|
| `logs.arrow` | 4.93x | `resource_id` 5.00x | `body` 4.52x |
| `spans.arrow` | 4.54x | `resource_id` 4.55x | `duration_nano` 4.29x |
| `log_attrs.arrow` | 9.28x | `key` 8.32x | `str` 6.98x |

Arrival order is already sorted by time, and time carries the locality —
consecutive rows come from the same handful of live resources, scopes and
routes. Re-sorting by anything else scatters that. It would also cost the block
its one useful physical property, that `min_ts`/`max_ts` bound a contiguous
range, which is what section 3.2's pruning reads.

**ZSTD level 9 — rejected.** Per table, level 3 against level 9: `logs.arrow`
5.23x to 5.36x for **6.9x the time**; `spans.arrow` 4.56x to 4.55x — *worse* —
for 3.9x; `log_attrs.arrow` 66.2x to 69.2x for 3.7x. A few percent of disk for
several times the CPU, on a sweep that shares cores with ingest, is a bad trade
on an axis principle 1 also scores.

One caveat on all five, stated rather than buried: this corpus comes from the
OTLP load generator, so its cardinalities are low — 2 distinct severities, 16
resources, 91 distinct bodies. Low cardinality is the case *most* favourable to
both dictionary encoding and sorting, and three of the five still came out
negative. A production corpus would move the ratios; it would not flip a
decision that already loses on the friendly input.

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
platform.** Ingest (section 12.1) is built, and so is the shared-volume topology of
section 12.5, which answers the same requirement with no fan-out at all. The
scatter-gather half — section 12.2 and section 12.3 — is design; the config key it used to name
is deleted, and the reasoning below says why that is the honest state rather than
a regression.

### 12.1 Ingest

Any L4 load balancer. Each replica owns its own disk and writes its own blocks.
There is no consistent-hashing ring, no shard map, no routing logic — not because
we skipped it, but because nothing has to land on a particular node. Blocks are
independent immutable objects with no global ordering and no cross-block merge,
so "which replica received this export" is not a question anything downstream can
ask. That falls out of the storage design rather than being a feature bolted onto
it.

Two things had to change to make concurrent writers safe, and both are in:

- The block directory name carries a **node id** (section 3.2), so two replicas cannot
  allocate the same name. Fixing this also fixed a live single-node bug: `seq`
  was resumed from `scan().last()`, and `scan` sorts by `(min_ts, seq)`, so a
  restart following a backlog replay could reuse a sequence number and wedge the
  node on `ENOTEMPTY` forever.
- Retention tolerates a losing race on `remove_dir_all` (section 6). Two replicas
  expiring the same block is not a conflict.

### 12.2 Query — still not built

A query is answered from the blocks the replica it arrived at can see: every
writer's, on a shared volume (section 12.5); one writer's, shared-nothing. There is no
fan-out, and `cluster.peers` — which named the peer set in the config file — is
deleted rather than left in place, because it was parsed, logged and read by
nothing. An unread key is worse than a missing one: it is a setting an operator
configures, sees accepted, and believes is in effect, and it sent them to point a
headless Service at a feature that did not exist.

The design, for when it lands: a query arriving at any replica is broadcast to
the peer set, executed locally on each, and merged. **The frame algebra of section 7.3
is what makes this work**: a frame is a small value, so broadcasting it is free,
and merging two nodes' results is a set union.

The reason it is a set union — rather than a distributed join — is the entity
identity of section 7.2, and this is the load-bearing connection between the two
requirements. Block-local ids never leave a node; they are meaningless off-box.
The only identifiers that cross the wire are the globally stable ones:
`resources.key`, `trace_id`, `span_id`, timestamps. Had entity identity stayed
"equality of the resource attribute set", cross-node correlation would have
needed a cluster-wide resource dictionary — which is coordination state, and the
principle forbids it. One decision paid for both features.

Fan-out uses the same query API as an external client, with a hop flag so a peer
does not re-broadcast.

**Partial results must be reported, never hidden.** A scatter-gather over seven
nodes with one down must not quietly return six sevenths of the data and let the
user draw a conclusion from it. Every response has to name which peers answered,
and that requirement is why this is not a two-hour feature.

### 12.3 Discovery without membership — still not built

The peer set is a list of addresses, and in Kubernetes it is a headless
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
| Query capacity | Linear; every replica answers independently. On a shared volume that answer covers the whole dataset, shared-nothing it covers that replica's share until section 12.2 lands — at which point latency for one query becomes the slowest peer. |

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

If replicas do share one filesystem, the design works unmodified — block names
are unique per writer, publishes are independent renames into a staging path
that is unique per block (section 12.6), and `scan` sees every writer's blocks, so any
node answers any query with no fan-out at all.

The honest scope of that is narrower than it first looks, and the earlier
version of this section overstated it. The gate is **mmap over a network
filesystem raises SIGBUS with no recovery path**, enforced by the `statfs` check
at startup (section 9), and it eliminates every RWX PVC anyone actually provisions:
RWX in practice means NFS, CephFS or Azure Files. Nor is "an RWX PVC backed by a
block device" the escape hatch it sounds like — a block device is only
*concurrently* writable through a cluster filesystem (GFS2, OCFS2), because
mounting ext4 or XFS from two nodes at once corrupts it. Mira has never been run
on one.

So the supported shape of shared-volume mode is **several processes on one
host** sharing a local directory: an e2e test covers it, and it is the mode the
`--node` flag exists for. Across hosts, shared-nothing is the supported shape
and query fan-out (section 12.2, unbuilt) is the answer to covering the whole dataset.
A cluster filesystem would work in principle and is not claimed. Object storage
is a larger question — it forecloses mmap entirely — and is deferred to the
market survey rather than guessed at here.

### 12.6 The staging path is the one place two writers can still collide

Everything above rests on writers never touching each other's bytes, and the
final block name delivers that: `{min_ts}-{max_ts}-{node}-{seq}` is unique per
writer by construction. The *staging* name was not. It was
`.tmp/{signal}-{node}-{seq}`, which is unique only as long as the two writers
disagree about `node` — and `node` is derived from `--node`, which defaults to
`mira`. Two replicas started with the defaults share a data directory and walk
the same sequence from the same starting point.

What that costs, measured on a two-process run over one directory: 2 lost
publishes in 107, both `ENOTEMPTY` or `ENOENT` from the rename, both surfaced to
the sender as a retryable NACK. That is the benign half. The other half was
permitted and simply not observed — B's `remove_dir_all` empties the directory
A is midway through writing tables into, A keeps writing the rest by path, and
whichever wins the rename publishes a block whose tables came from two different
sealed sets. No error anywhere, and the corruption is only visible as a query
returning rows that never coexisted.

The fix is the cheapest one available: put the timestamp range the final name
already carries into the staging name too, making the path unique per block
*content* rather than per writer. A misconfigured `--node` is then merely
duplicated data, which OTLP's at-least-once contract already permits, instead of
a silent mix.

Two consequences follow, and both are in the code:

- `create_dir`, not `create_dir_all`, for the staging directory. `create_dir_all`
  succeeds on a directory that already exists, which is the exact mechanism by
  which a leftover gets merged into a live publish. Colliding now fails loudly
  and retryably.
- A staging name that is never reused is a staging name that nothing ever
  cleans, so a publish killed between staging and rename leaks a directory.
  `block::sweep_staging` clears it at boot, filtered by signal *and* node — an
  unfiltered sweep of `.tmp` would delete another replica's in-flight staging
  directory, reintroducing the corruption from the other direction.

The general shape is worth naming, because it will recur: **on a shared
filesystem, every path a writer creates is part of the coordination-free
argument, not just the ones that survive the write.** Principle 4 buys freedom
from coordination *state*; it does not buy freedom from thinking about
concurrency.

---

## 13. Alerting

**Built**: `crates/mira/src/alert.rs`. Static rules in a KYAML file named by
`alerts.rules`, evaluated on a timer, dispatched as JSON webhooks. Off unless
the file is named. The operator-facing half is [config.md](config.md); this
section is why it has the shape it does.

### 13.1 A rule embeds a query document, verbatim

The `query` and `of` keys of a rule are `/api/v1/query` documents parsed by
`api::parse_search` — the same parser the HTTP API, the MCP tools, the browser
UI's filter bar and the TUI's all reach. There is no alert filter grammar.

That is not tidiness, it is the only way the link in a page can be trusted. The
UI keeps every bit of its view state in the location hash (section 8.2), so
`link_base` plus the rule's own terms *is* the saved view — no view has to be
created, stored or garbage-collected, and principle 4 survives an alerting
engine intact. If a rule spelled its predicate differently from the UI, the
link in the 3am page would open a different set of rows than the number that
woke someone up, and nothing would ever detect it. The rule's terms are
re-spelled into the filter-bar grammar once, in `filter_of`, and both the link
and the `filter` field of `/api/v1/alerts` come out of that one function — which
is what lets the TUI's alert pane press Enter and land on the rows.

A rule may not set `from`, `to`, `limit` or `after`. `over` is the window and
the evaluator owns the rest; those keys are refused at load rather than
overwritten in silence.

### 13.2 Counting is free, so there is no aggregation engine

`query::search_open` with `limit: 0` returns an exact `stats.rows_matched` and
no rows. The early exit cannot fire (it tests `hits.last()`, which is always
`None` after a zero-limit trim), the match counter accumulates every wave, and
memory is bounded because nothing is retained. So an evaluation is one scan for
a count rule and two for a ratio rule, over one window, with no rows
materialised and no new code in `mira-core`.

Note that this is a property of the internal call, not of the HTTP endpoint —
`/api/v1/query` still rejects `limit: 0`, because a caller asking for zero rows
through the API has almost certainly made a mistake.

### 13.3 A percentile threshold *is* a ratio threshold

```
p95(d) > T   ⟺   |{ d > T }| / |d| > 0.05
```

Exactly, not approximately: both sides say "more than 5% of the sample exceeds
T". So the two metrics `count` and `ratio` cover every threshold in the original
brief, including `p95(duration) > 500ms`, and there is no t-digest, no sketch in
the block footer to maintain, and no `p95(...)` in the grammar. A sketch here
would be an approximation of something a scan answers exactly and for free
(section 13.2). `alert::tests::a_percentile_threshold_is_a_ratio_threshold` is the
identity as an executable claim.

The corollary is worth stating: this buys thresholds, not *values*. Mira cannot
currently tell you what p95 *is*, only whether it is over a line you named. When
"what is the p95" becomes the question, the answer is the block-footer digest of
section 8.1, and it is a read-path feature rather than an alerting one.

### 13.4 Which replica pages

Nothing elects one. Alerting is off unless a node is pointed at a rules file, so
in a fleet exactly one replica is given the flag and it is the one that pages.
That is the whole coordination mechanism, and it is deployment configuration
rather than state — three replicas sharing the file would send three copies of
every alert, which is a misconfiguration and not a race.

`ponytail:` the upgrade path, if that ever stops being acceptable, is a lease
file in the block directory: an evaluator writes `alerts/lease` with its node id
and a timestamp, and refuses to evaluate if a fresher one exists. That is still
no coordination *service* — the directory is already the manifest (section 3.2) — but
it is coordination state, so it is not worth paying for until someone is
actually running two evaluators by accident.

### 13.5 Dispatch, and why TLS is a Cargo feature

One POST per edge — `ok → firing` and `firing → ok` — with a 10-second timeout
and no retry. A retry queue is durable state on a node that is supposed to have
none, and the next evaluation is along in `every` seconds regardless; a webhook
receiver that was down for one POST will be told again. `for` is a *sustained*
breach, not a repeated one: the state machine (`State::advance`) resets `since`
on any non-breaching evaluation, so a metric that flaps across the line never
accumulates hold time. PagerDuty gets `dedup_key = rule name`, so a resolve
closes the incident its fire opened.

A direct `https://` target costs eleven crates and brings `ring`, which is C and
assembly. "The tree is N crates" and "`zstd-sys` is the only C dependency" are
both stated product properties (section 11, README), so HTTPS is `--features
webhook-tls`: 120 crates by default, 131 with it. Those two are `cargo tree`
counts and so include the three workspace members; the 117 section 11 and the
README state is the same default tree with those three taken out. The default build refuses an
`https://` URL when the rules file is *loaded* — at boot, with the process
exiting on the message — rather than at the first page, because the first page
is precisely when nobody is reading logs. An egress proxy on localhost is the
zero-crate answer and the one most deployments already have.

Neither `Rules`, `Rule` nor `Target_` derives `Debug`. A Slack webhook URL *is*
the credential, and so is a PagerDuty routing key; a derive would put both one
careless `{:?}` away from a log line.

### 13.6 The surfaces

`GET /api/v1/alerts` is always routed, even with no rules — an empty list is how
the UI, the TUI and an agent learn that alerting is *off*, where a 404 is
indistinguishable from an old build. Every rule is reported, firing or not, with
its last value, both record counts, its `filter`, its `link`, and an `error`
that is non-null when the rule could not be evaluated. A rule that failed to
evaluate is not a rule that is quiet, and no surface here conflates them. The
MCP tool `list_alerts` and the TUI's `a` pane read the same document.
