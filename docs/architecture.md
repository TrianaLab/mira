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
| **Dictionary-encode high-cardinality maps** | Half backwards. Dictionary encoding is a *low*-cardinality technique with a hard ceiling — `Dictionary<UInt16,_>` raises `DictionaryKeyOverflowError` past 65,536 distinct values, and `http.url` and `trace_id` are the highest-cardinality data in the system. But the *repetition* the brief was reaching for is real: a few hundred distinct attribute values across a few hundred thousand rows. | Two key widths. Enumerable columns — attribute *keys*, `severity_text`, metric name and unit — take a `UInt16` key and seal the block at `DICT_CAP` rather than fail. The attribute *value* string column `attrs.str` takes a `UInt32` key, which a 32 MiB block cannot fill; it is the one *value* column that is dictionary-encoded, and it is worth 12.9× → 66.2× on one real block's cold ratio (section 11). `attrs.bytes`/`attrs.ser` stay plain `Binary`. |
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
- **No block cache.** Every query re-opens each block it touches — though no
  longer re-CRCs it, which is a different thing and is now done once per file
  per process (section 3.3). What is left of the open is `mmap`, the dictionary
  scan and the two child indexes. This looked like the next big win until it was
  measured against the two that were taken instead: it is worth a few
  milliseconds of a 14 ms query, not the 10x that page-fault behaviour was
  (section 11).
- **OTAP is the data model, not yet the wire protocol.** No language SDK emits
  OTAP; the only production implementations are the Go
  `otelarrowreceiver`/`exporter` in collector-contrib. OTLP on 4317/4318 is the
  universal path, and the OTAP receiver is a second listener over a storage
  layout that is already shaped for it.
- ~~**No cross-replica query fan-out.**~~ Built, in a second process rather
  than in the storage node: `mira proxy` (section 12.2) merges `/api/v1/query`
  across a static list of replicas and routes OTLP exports between them by
  entity. A storage node still reads only the block directory it was pointed at
  and still has no peer list — that is the part principle 4 is protecting, and
  the proxy holds nothing durable either. What is *not* built is peer-to-peer
  broadcast between nodes, and the reads the proxy refuses to merge rather than
  approximate (correlate, map, metrics, entities) still have to be addressed at
  a replica. Section 12.2.4 is honest about the remaining gap, which is that
  nothing has measured the single-node ceiling this closes.
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
  name. Three keys reach the engine and none of them is a tuning surface.
  `ingest.shards` is a *correction*: the runtime's count of the cores it has can
  be wrong (section 4), and the default asks nobody. `ingest.queue` buys queueing
  and not throughput — the flusher drains at the rate it drains — so it is
  memory an operator with spare memory can spend on absorbing a burst.
  `ingest.wal` is a *promise*, not a speed: on means an ack survives SIGKILL at a
  p99 in the microseconds, off means it survives power loss at a p99 of 2.6 s,
  and nothing the engine can measure says which one a deployment wants. The
  boundary is structural, not documentary. It is also closed — fourteen keys, and
  an unknown one is a startup error naming it — because the alternative is what
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
release profile it is **50.0 MiB and 271 crates**, against 5.76 MiB and 117. An
order of magnitude is still an order of magnitude, so it is out of the binary
everyone downloads. There is no `--features sql` in the tree: `crates/mira`
declares `default = []` and `webhook-tls`, and nothing else. The feature is the
*shape* a SQL surface would take if one is ever asked for — a gate, not a
default — and section 10 keeps it on the not-built list until someone asks. What
DataFusion does not do, either way, is displace the hand-rolled ~2,000 LOC fast path, because a
4.5 ms point lookup that already prunes to one block of 137 has nothing to gain
from a planner. With traces, metrics, query, MCP and both UIs in it, the default
build is **5.76 MiB stripped, 117 crates** — the scale the design is defending.

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
| `str` | `Dictionary<UInt32, Utf8>` — the only *value* column that is dictionary-encoded |
| `int` / `double` / `bool` / `bytes` / `ser` | `Int64` / `Float64` / `Boolean` / `Binary` / `Binary` |

Exactly one value column is non-null per row; `type` says which. The other five
cost one validity bit each.

`str` is a `UInt32` key rather than `key`'s `UInt16` because attribute values are
unbounded in principle — a trace id, a URL, a GenAI prompt — so a `u16` would seal
a high-cardinality tenant's block every few thousand rows. The wider index costs
0.7% of compressed bytes and buys a ceiling a 32 MiB block cannot reach.

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
<data>/.wal/<node:08x>-<first_seq:020>.wal          # only with ingest.wal on
```

The log is the one thing under `<data>` that is not a block, and the leading dot
is load-bearing: everything that walks the store filters on the three signal
directories, and a hidden sibling is one a shell glob does not sweep into a block
listing by accident. A segment is named for the *first* sequence it holds, which
is what lets `Wal::open` refuse to resume onto the name of a segment that ended
torn (section 4).

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

Two things then decide what a query actually pays for that guarantee: *which*
tables it verifies, and *how often* it verifies each one. They are independent
and this release changes both.

**The CRC is per table, and that is what lets a query skip one.** Each `.arrow`
file carries its own `mira.crc32`, so `block::open_table` verifies exactly the
one file it is opening and nothing else in the block. `Block::open` therefore
takes the root table and stops; `Block::detail` maps the three attribute levels
and, for traces, the child tables, and it is called on exactly two occasions —
a predicate that names an attribute, because the predicate is evaluated against
those tables, and a block that produced a row, because rendering a row emits
its attributes. A block that matches nothing and is asked nothing about
attributes is never hashed past its root, and on the measured corpus that is
42.9% of a *plain* logs block and 45.7% of a plain traces block left unread —
5.5% and 6.3% once the cold tier has compacted it, where the saving is
decompression rather than bytes (section 11).

None of that weakens the guarantee. What changes is *which* tables a query
reads; every table it reads is verified in full before a byte of it is
returned, and a corrupt attribute table that no query has touched is caught by
the first query that touches it.
`a_corrupt_attribute_table_is_caught_by_every_query_that_reads_it_and_no_other`
corrupts one `log_attrs.arrow` and pins all three halves of that: the scan that
does not read it succeeds, the attribute predicate fails `BadChecksum`, and so
does the query that renders one of its rows. Going back to an eager load fails
the first; rendering without the verified load fails the third.

**And the CRC is paid once per file per process, not once per open.** It is the
right thing to do on a first read and pure waste on a second: a published block
never changes, so a scan that reopens the same corpus re-hashes bytes this
process has already hashed. `open_table` consults a process-scoped map of
`path -> (len, mtime, ino)` before hashing and writes to it after. Not the path
alone — `compact` renames a new table over an existing name, which is a
different file that must re-verify.

All three fields, and each one covers a way the bytes under a path change that
the others cannot see. The length catches a rewrite of a different size. The
mtime catches a rewrite of the same size, and alone it is not enough, because a
length-preserving write inside one filesystem mtime tick does not move it; the
settle window below is what closes that. The inode catches a
**replacement**, and nothing else does: `std::fs::copy` on APFS is
`fcopyfile(COPYFILE_ALL)` and preserves the source's mtime to the nanosecond, as
do `cp -p`, `rsync -a`, `tar -xp` and every backup agent worth running. A block
restored from a copy of itself therefore arrives at a path this process has
verified, wearing a length and an mtime it remembers, carrying bytes it has
never hashed. That is the one failure this cache must not have — a bad block
served as good data — and the length and the mtime are both blind to it.

The settle window is the rest of that argument. An entry is only recorded once
the file has been untouched for longer than any filesystem's mtime granularity,
which makes a later write necessarily a later tick and necessarily a miss. In
production that excludes only a block being written right now, which is the one
that should be re-read anyway.

Keying on the inode *instead* would be worse than either, which is the shape
this was first written in. Retention unlinks block directories continuously and
an inode number is reusable the moment its last link goes, so a fresh table
could be handed the number of an expired one and inherit its verdict. Composed,
that is a non-issue: a reused inode would also have to arrive at the same path
under the same length and the same mtime.

`a_block_replaced_under_a_verified_path_is_checksummed_again` is the regression
test, and it is worth saying that it failed against the two-field key — the
restored block was served from the cache, unchecksummed, exactly as described.
Three fields also mean a restore is free to preserve whatever it likes: the
earlier design put the obligation on the caller, who had to stamp a current
mtime by hand and had no way to be told they had forgotten.

That leaves `skip_validation(true)` above resting on a weaker premise on a
second open — not "the CRC just proved these bytes" but "this process proved
them earlier" — and it is worth being exact about which half of that is new.
The strong reading was never available under `mmap`: a clean page can be
evicted between the hash and the scan that reads it, and re-faulted from disk,
so even a single open verifies the bytes as of the CRC and not as of the read.
Closing *that* would mean copying the body out and validating the copy, which
is the zero-copy property this section exists to protect. What the cache
changes is the width of the window, from one open to one process; what it costs
is detection of media that rots under a mapping this process has already
verified. A restart re-verifies everything, which is why the map is
process-scoped and not persisted.

What it is worth is [section 11](#11-performance-model), and the two changes
stack rather than compete: the lazy load removes opens that should never have
happened, and the cache removes the repeat hashes on the opens that remain.
Section 11 measures each against the binary that preceded it and then both
together, because the cache's own figures were first taken against the eager
`Block::open` that no longer exists.

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
the write path. So the frame goes to `.wal/` first and the ack costs a `write(2)`,
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

A 60-second tick that does three things per signal, in this order, and nothing
else:

1. **Expire.** Compute `now - ttl`, call `scan()`, and `remove_dir_all` every
   block whose `max_ts` is older. With `--offload`, the copy to the object store
   is the hook that runs just before each unlink (section 12.4).
2. **Compact.** Rewrite up to `MAX_COMPACT_PER_SWEEP = 8` blocks per signal whose
   `max_ts` is older than `COLD_AFTER_NS`, table by table, ZSTD into a `.tmp` and
   rename. Expire runs first on purpose: compressing a block this sweep is about
   to delete is pure wasted bandwidth.
3. **Reclaim.** One `statfs`. If free space is under `MIN_FREE = 10%`, drop
   oldest-first until it is not, logging every block and why. `--offload`
   deliberately does not apply here — an unreachable store must not be able to
   stop the floor from reclaiming.

TTL is a directory unlink, so step 1 costs no IO bandwidth and cannot interfere
with ingest. Step 2 is the one that does read and write live data, and section 11
prices it: it is bounded to eight blocks per signal per sweep precisely so that
the bandwidth it costs is a constant an operator can reason about rather than a
function of how far behind it is.

**No reader lease protocol is needed**, and this is a real result rather than an
optimism. POSIX specifies that `mmap()` adds a reference to the file that
`close()` does not remove, and that the reference persists until the last
mapping goes away. A query holding an `Arc<Mmap>` therefore keeps reading correct
data out of an unlinked file. The `Arc` *is* the refcount and the kernel holds
the inode. The one rule: never truncate or rewrite a published block — that
gives readers `SIGBUS`, whereas unlinking does not.

### 6.1 `--offload`: a copy before the unlink

`--offload <uri>` puts a copy of a block in an object store immediately before
retention unlinks it. One flag, because a URI is an address, and the only
tiering knob there will be: no offload period, no cache path, no cache size, no
eviction policy. The period is `storage.retention`, because the block leaving
the disk *is* the event.

**The naming is the catalogue.** An offloaded block lands at
`<uri>/<signal>/p=<epoch_hour>/<block>` — byte for byte the layout of section
3.2, time range still in the directory name. The store's own list API is
therefore the manifest exactly as `readdir` is locally: `mira offload list` is
`block::scan` pointed at the other root, parsing `min_ts`, `max_ts`, node and
sequence back out of the names it gets. Nothing is written that a future binary
has to understand and nothing records what has been uploaded.

**The ordering is the design.** The copy is a hook `expire_with` runs just
before each `remove_dir_all`, so the failure direction is fixed at *two copies,
never zero*: a copy that fails logs, keeps its block, and is retried next sweep.
The alternative — a separate upload pass that marks what it has done — needs a
marker both passes agree on, which is coordination state, which is what
principle 4 spends everything to avoid. Doing the copy inside the unlink makes
the filesystem's own presence and absence the marker.

**Staging, so a killed copy is not a block.** Each block is written to
`<root>/.tmp/<signal>-<name>-<pid>` and renamed into place, so a partial
directory never appears in a listing and never blocks the retry. A rename that
loses to another replica's is success, not an error — both wrote the same
immutable bytes. The restore path stages under the same
`<node:08x>-restore-` shape `block::sweep_staging` already clears at boot, so a
killed restore costs no new code.

**`file://` only, and that is not a placeholder.** Everything after the prefix
is a path, so `file:///srv/cold`, `file://./cold` and a bucket already mounted
into the filesystem all work. `s3://` is refused at startup, and the reason is
the dependency budget rather than the effort: signing a request needs
HMAC-SHA256 and reading a listing needs an XML parser, and neither is in the
crate graph the README counts. An operator who wants S3 mounts it; `mira` never
learns what a bucket is.

**An offloaded block is not `mmap`-able, and nothing pretends otherwise.** Reads
never consult the store. `mira offload restore` copies blocks back into a data
directory and the server picks them up on its next scan — that is the whole
retrieval path. No transparent fetch, no cache tier, no partially-local block.
It is also what keeps section 9's refusal to `mmap` a networked filesystem
intact: the mapped file is always the local one.

**`mira offload push` is the same copy with the ends swapped, and it unlinks
nothing.** The retention hook offloads a block because that block was about to
be deleted; the verb offloads a whole data directory because the *volume* is —
most often one a scale-in left behind, holding blocks no query can reach any
more (section 12.4). Push them under a URI of their own, `restore` them into a
node that is still running, and they are back in a catalogue something reads.

Deleting the local copy afterwards is the obvious next step, and it is wrong.
A node derives two numbers from the blocks it still holds and neither survives
an emptied directory: `wal_watermarks` returns `0` for a signal with no block,
so the next boot replays a log whose frames were absorbed long ago, and the
block sequence resumes at `max(seq) + 1` over the local scan, so the node
reissues `(node, seq)` pairs that are still alive wherever they were copied —
the pair the cursor's total order is built on. Freeing space on a volume that is
about to be deleted buys nothing, and it does not begin to pay for those. The
verb copies; deleting the volume is the operator's next step anyway, and it is
the only unlink in the procedure.

**The copy is a `read`/`write` loop on purpose**, not `fs::copy`. On macOS
`fs::copy` is `fclonefileat`/`fcopyfile(COPYFILE_ALL)` and *preserves mtime*, so
a block restored from a two-month-old offload would land with a two-month-old
stamp on bytes written seconds ago. Nothing in this tree reads mtime in anger —
retention keys on the `max_ts` in the directory name, so a restored block's
expiry is the same either way — but a restore is the one operation that changes
what is at a path, and `(len, mtime)` is how everything *outside* Mira notices
that: `rsync`, a backup agent, `find -mtime`, any cache keyed on a path's
identity. Writing the bytes makes the stamp current as a consequence of the
write, with no call anyone has to remember.
`offload_restore_stamps_current_mtime` is the test that fails if someone reaches
for the faster call.

The measured cost of all of it is in section 11.

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
| **entity + time** | **always** | `resources.key`, section 7.2 — readable as a frame, not yet selectable as a predicate; section 7.4 |

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
- **Entity** — `resources.key`, written at seal and read by the frame algebra
  but not by any *predicate*. `frame::entity_keys` loads the column for
  `anchor`, `expand`/`Peers`, `map`, `names_of` and `entities`, and the key
  surfaces on `POST /api/v1/entities`, the MCP `list_services` tool and the TUI's
  entity pane. What is still missing is the selector: `query::Search` has
  `signal`, `from`, `to`, `terms`, `limit` and `after` and no entity member, so
  no query prunes a block by entity. That is why the key-set cache that would
  make one cheap — tens of `u64` per block, ~4 MB for ten thousand blocks, and
  therefore no on-disk filter warranted — is not written either. The
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
shape rather than a join, because entity ids number in the tens. It covers every
attribute level and four of the six value columns — `str`, `int`, `double` and
`bool`. `bytes` and `ser` (the OTAP Bytes, Slice and Map values) are decoded and
returned in a result but are not filterable in V0: `AttrPred::test` dispatches
on the *stored* type and has no arm for them, so a predicate against one matches
nothing rather than erroring. A filter over a serialised map is a path
expression, which is a query language, which is section 0.

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
kilobyte per distinct series until the process died. `max_points` reports what
it refused, as `dropped_points` on the series it trimmed, because a chart
silently missing its spikes is the failure mode the cap exists to prevent.
`max_series` does not, and that is a gap rather than a decision:
`Stats::dropped_series` is computed (`series.rs:184`) and then dropped on the
floor, because `api::envelope` is the only serialiser on every read surface and
does not carry it — so the 65th series is invisible to HTTP, MCP and the TUI
alike, which the TUI's own `ponytail:` at `tui.rs:510` says out loud. The fix is
one field in `envelope` and a badge; it is unbuilt because it widens the
response object every read surface shares. `get_trace` does not page either — a trace is one page or
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

**Probes are not the UI, and the listening line is not a promise.** `/health`
and `/readyz` were one handler once — Mira has no warm-up and no cluster to
join, so there is no state in which it is alive and not ready. A full volume is
exactly that state, so they are two answers now: liveness is a constant 200 plus
the per-signal shed and failed counts, so the probe and the log agree about how
much has been refused, and readiness is the single question of whether an export
can still be made durable. `ingest.wal` does not add a third state, though it is
the first thing that could have: log replay is the one startup task with
unbounded duration, and it runs after both sockets are bound but *before* either
accept loop starts — so a probe during replay completes its handshake into the
kernel backlog and then waits there, and is never answered 200 about data the
process has not recovered yet. They also exist so that something other than the UI answers at
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

- **`fetch`, and four of the seven expanders.** The algebra itself is built and
  section 7.3 says so in its title: `mira_core::frame` is `Frame`, `anchor`,
  `expand` over `Traces`/`Peers`/`Around`, `map`, `names_of` and `entities`,
  served at `POST /api/v1/frame`, `/api/v1/map` and `/api/v1/entities`. What is
  not there is `fetch` — a `Frame` holds identities and there is no operation
  that renders the records for them, so every widening is another scan — and the
  four expanders section 7.3 cut with its reasons beside them.
- **A block cache.** Every query re-opens every block it touches. The fix is a
  process-local `Arc<MappedTable>` map invalidated by `expire`. This was assumed
  to be the next big win and it is not: section 11 measures the per-block cost as
  dominated by faulting the mapping in, which the `MADV_WILLNEED` hint already
  addresses. Half of what such a cache would have saved is taken already and
  separately — the CRC is verified once per file per process (section 3.3), which
  needs no cached mapping and so has none of a cache's invalidation surface.
  What is left for it to save is the `mmap` and the two child indexes: real,
  small.
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
- **DataFusion, in any build.** section 1. It is not *rejected* — 50.0 MiB and
  271 crates behind a cargo feature would be paid only by whoever asks for SQL,
  and DataFusion 55 pins arrow v59.3.0, exactly Mira's pin, so there would be no
  second Arrow in the tree. But no such feature exists: `crates/mira` declares
  `default = []` and `webhook-tls`, `datafusion` is in no manifest and no
  lockfile, and `--features sql` does not resolve. It is a shape held open, not a
  build option.
- **The `F_FULLFSYNC` fallback** (section 9). Not a weaker fsync — the opposite, and it
  was settled by measuring rather than by reading. On this machine
  `File::sync_all()` costs 4,230 us, `fcntl(F_FULLFSYNC)` 4,213 us and a bare
  `libc::fsync(2)` 28 us: `sync_all` *is* `F_FULLFSYNC` on Apple targets, so
  section 11's ack latencies are measured against the stronger barrier and nothing is
  owed for the ordinary case. The Docker-for-Mac `EINVAL`/`ENOTSUP` path section 9
  names **is** written, and so is the counting section 9 insists on:
  `mira_core::sync_all`/`sync_data` wrap every sync in `block.rs` and `wal.rs`,
  degrade to `libc::fsync` on exactly those two errnos, and report the count as
  `degraded_syncs` on `/api/v1/stats`. So is the `statfs` guard beside it:
  `block::check_filesystem`, called once before anything is mapped. What stays on
  this list is the *weaker* fsync — trading the barrier for throughput by
  default, which would make an ack mean less than it says.
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
  overhead above ~5% of ingest CPU. The third reason used to name the log's
  group commit as the trigger; that fix is rejected on the numbers two bullets
  into the plateau discussion below, so the trigger is now the flusher measuring
  submission-bound rather than device-bound.
- ~~**The query-side half of `NO_IDENTITY`.**~~ Built. `Frame::add_entity`
  (`frame.rs:82`) drops the sentinel with the reason this bullet asked for — "an
  entity set containing the sentinel means *every resource nobody described*,
  which is not an entity" — and `frame::entities` exposes `resources.key` on
  `POST /api/v1/entities`, the MCP `list_services` tool and the TUI, so there is
  now something for it to be refused by. The bullet is kept struck through rather
  than deleted because the reasoning for the refusal is what section 7.2 is
  pointing at.

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
back to back in the same sitting.

Several bullets below are the exception and say so where they appear, because
each needs a corpus this one cannot be — a full round-trip through an upload, a
scan repeated ninety times, or blocks the cold tier has not reached yet:

| corpus | shape | what it is for |
|---|---|---|
| the table's own | 8.33 GiB, 1,652 tables, 137 log / 155 trace / 24 metric blocks | every row of the table above |
| **small** | 137 blocks, 3,599,317,452 bytes (3.352 GiB), 62 logs / 64 traces / 11 metrics | cold tier, block reopens — round-tripping 8.33 GiB through an upload takes long enough that the box moves underneath it |
| **small, compacted** | those same 137 blocks after the cold tier has finished with them: 718 tables, 440,421,916 bytes, all 137 marked `cold` | the compacted arm of the lazy-open bullet |
| **plain** | 115 blocks, 611 tables, 2,927,859,966 bytes, 50 logs / 53 traces / 12 metrics, none compacted | the main arm of the lazy-open bullet, which needs attribute tables that are still uncompressed |
| per-run | built fresh by the script that reads it | restart replay, and the second sitting's 9.6 GiB / 5.01 GiB pair |

The restart-replay corpus is per-run because what it measures is a difference
across a restart rather than any absolute. All of them are internally paired and
**none of their numbers may be divided into the table above**.

Four of the five are reproducible from a script in `scripts/measure/`, named in
the bullet that uses them: `restart-replay.sh`, `offload-cycle.sh`,
`block-reopens.sh`, `lazy-detail.sh`. **The second sitting's 9.6 GiB / 5.01 GiB
pair is not.** It was built by hand and there is no script that rebuilds it, so
that one bullet is reproducible in method and not in corpus. The obstacle is
worth naming because it is fixable and general: `loadgen`'s payload is
deterministic — the same arguments produce the same bytes — but its stopping
condition is `--for <duration>`, so the *volume* a run produces depends on how
fast the box was. A record-count stopping condition would make a corpus
described by an exact byte count something a second person can rebuild. See
[the measurement contract](internals/measurement.md).

The plain one is there for a reason worth stating once: **a corpus is not a
constant while a server is running on it.** The cold tier compacts blocks that
have aged past their partition hour from inside the server a harness keeps
starting, so a long A/B over fresh blocks starts plain and finishes compacted,
both arms drift together, and the result is a measure of how far through the
transition each pass landed. That is not hypothetical — it invalidated a table
this section published, and the withdrawal is under the lazy-open bullet.
`lazy-detail.sh` now fingerprints the corpus before the run and after every pass
and refuses to continue if it moved.

There is now a **second sitting**, and naming it is better than folding it in.
It exists because the checksum cache (section 3.3) landed after the table was
measured, and it covers the query rows only: two corpora from the same
generator, both read by this binary and by 04561ed back to back. It does not
re-measure ingest, footprint or cost per GB, because the write path is
byte-identical across that change and replacing good numbers with ones taken on
a busier machine is not an improvement. Where the two sittings disagree the
disagreement is the finding, and it is written up under the last row.

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

Read the two query columns carefully. "First" is the first call after a process
restart; "steady" is the same call repeated. The gap between them is
virtual-memory work — establishing 316 blocks' worth of mappings and faulting
them in — and for every row that prunes, so is nearly all of the steady figure.

This paragraph used to go on to say that **neither column is a cold-disk
number**, because 8.33 GiB fits in this machine's 18 GiB of page cache. That is
arithmetic, not a measurement, and the second sitting shows it does not hold for
the last row: 18 GiB shared with a VM, Docker and a browser does not keep 8 GiB
of corpus resident, and the same scan over a corpus that *does* stay resident is
three times cheaper per row. Treat the pruning rows as warm and the last row as
partly not.

| Axis | Target | Measured | |
|---|---|---|---|
| Ingest throughput | ≥ 1 M records/s/core | **886k records/s/core** — 629,384 records/s on 0.71 cores; **1,350,502 records/s** aggregate at four connections and a plateau peak of **1,537,875** at thirty-two, on 1.75 and 2.23 cores, nothing shed at any shape | ~ |
| Resident footprint | ≤ 2 × the open block's target size | **232 MiB** at one connection, **689 MiB** at four, **1,648 MiB** at 96 — see below | ~ |
| Ack latency | — | p50 **8.5 ms**, p99 **55 ms** at four connections, log on; p50 **657 ms**, p99 **2,647 ms** with it off | see below |
| Query: attribute value, absent | ≤ 10 ms | **8.3 ms** first, **2.6 ms** steady, 0 of 137 blocks | ✓ |
| Query: attribute value, matching | ≤ 10 ms | **4.1 ms** first, **4.5 ms** steady, 1 of 137 blocks, 204,800 rows | ✓ |
| Query: unfiltered `limit 100` | ≤ 10 ms | **29.1 ms** first, **4.6 ms** steady, 1 of 137 blocks, 204,800 rows | ✓ |
| Query: trace by id | ≤ 10 ms | **8.3 ms** first, **4.7 ms** steady, 2 of 155 blocks, 73,728 rows — first sitting; the second measures the checksum cache 1.47× under it | ✓ |
| Query: metric names | — | **8.9 ms** first, **4.7 ms** steady, 24 of 24 blocks | — |
| Query: substring, no time bound, prunes nothing | — | **1,441 ms** first, **885 ms** steady, 137 of 137 blocks, 27.1 M rows — first sitting, and the row the second sitting has the most to say about | see below |
| Cost per GB ingested | ≤ 0.35 B/B | **1.20 B/B** hot, **0.14 B/B** compacted | ✓ |
| Binary size | ≤ 20 MB stripped with UI + query + MCP | **5.76 MiB** / 117 crates | ✓ |

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
  means the ceiling is somewhere other than the engine's arithmetic, and the next
  bullet names it. Two caveats stay on the aggregate whatever the cause: the
  generator is co-resident and encoding 8192 protobuf records per batch inside
  the same loop, so part of the per-batch cost is the harness's, and separating
  them wants the generator on a second machine, which is the one thing a
  single-laptop harness cannot do. Treat the aggregate as a floor and the
  per-core figure as the comparable one.
- **The plateau is one mutex held across a `write(2)`, and the measurement ships
  with the binary.** The sweep above cannot see the cause from outside: a
  closed-loop generator reports `connections × batch / ack latency`, so every
  hypothesis predicts the same curve, and CPU is flat at 2.23 cores at both ends
  of it. `mira_core::diag` answers it from inside — two `Instant::now()` pairs
  and five relaxed atomics per export, under 150 ns against a critical section
  measured in milliseconds — and prints only when its target is enabled:

  ```sh
  RUST_LOG=mira=info,mira_core=info,mira::probe=debug mira --data-dir ./data
  ```

  Three runs, `loadgen --for 20s --batch 8192`, fresh store each, means over the
  whole run — `scripts/measure/ingest-probe.sh` is the whole sequence, this
  table and the worker-count A/B in the next bullet:

  | | 4 conns | 32 conns | 96 conns |
  |---|---|---|---|
  | records/s | 1,297,149 | 1,917,983 | 1,814,829 |
  | ack p50 | 8.3 ms | 64.3 ms | 199.9 ms |
  | `submit.total` | 7.571 ms | 22.117 ms | 26.177 ms |
  | `submit.admit` | 0.000 ms | 0.002 ms | 0.001 ms |
  | `wal.encode` | 0.733 ms | 1.545 ms | 1.400 ms |
  | `wal.lock_wait` | 3.920 ms | 18.152 ms | 22.067 ms |
  | `wal.held` | 2.915 ms | 2.272 ms | 2.611 ms |
  | of which `wal.write` | 2.849 ms | 2.042 ms | 2.422 ms |
  | `runtime.lag`, ticks | 9.7 ms, 418 | 33.9 ms, 238 | 80.2 ms, 192 |
  | `wal.inflight_max` | 4 | 12 | 12 |

  Read down a column rather than across, for the reason three bullets below.
  **`wal.lock_wait` is 52% of `submit.total` at four connections and 82% and 84%
  at thirty-two and ninety-six.** There is one `Wal` behind all three signals and
  all `ingest.shards` shards; `wal.held` times the append count is 15.2 s, 14.7 s
  and 18.6 s of a twenty-second run, so **the single mutex is occupied 74% to 93%
  of the wall clock**, and 90–98% of what it is held for is the three
  `write_all`s. At 2.2–2.9 ms an append the log serialises at most 345 to 440
  appends per second, and at ~5,100 records an append that is a hard **1.7 to
  2.6 M records/s whatever the connection count**. Connections past the plateau
  add waiters, not appends.

  `wal.inflight_max` is what makes it a runtime failure rather than only a
  throughput one. `std::sync::Mutex` on aarch64-apple-darwin is the pthread
  backend, so a contended `lock()` parks the OS thread in the kernel — and a
  parked tokio worker runs no other task and is not replaced. A high-water mark
  of exactly 12 on a 12-worker runtime says every worker was inside
  `append_then` at once. `runtime.lag` is the same fact with nothing borrowed
  from the client: a task that asks to sleep 50 ms and does no work at all wakes
  9.7 ms late at four connections, 33.9 at thirty-two and 80.2 at ninety-six.
  The tick counts beside those — 418, 238 and 192 — are **not** out of a fixed
  denominator, and an earlier draft of this table said they were out of 400,
  which is impossible on the face of it since 418 is larger. `RUNTIME_LAG` is a
  free-running probe spawned at process launch and never reset, so its count is
  over the whole process lifetime and is not comparable across columns; the load
  window is 20 s of that. What *is* comparable is the cadence the lag implies:
  a 50 ms sleep that wakes 9.7, 33.9 and 80.2 ms late completes **16.8, 11.9 and
  7.7 wake-ups per second** against the 20 it asked for. Ack latency cannot
  separate "working hard" from "cannot schedule anything". This can, and it says
  the second.
- **Each obvious suspect is ruled out by a number rather than an argument.**
  Admission backpressure: `submit.admit` — `reserve()` plus the `ADMIT_WAIT`
  park — is a mean of 0.000 to 0.002 ms with a maximum of 6.5 ms at every shape,
  so the bounded channel is not where the time goes, and `reserve()` is a
  first-fit `try_reserve` across shards touching atomics only, so shard dispatch
  goes with it. Park/wake under saturation is real but it is *inside*
  `wal.lock_wait`, not beside it. And more runtime workers is not the fix, which
  is the paired A/B worth keeping: 96 connections, same binary, same box, back to
  back, `TOKIO_WORKER_THREADS` 12 against 48 — **2,229,315 records/s against
  1,675,695, a 25% loss, `wal.lock_wait` up 6.4x from 16.3 ms to 104.5 ms and
  `wal.inflight_max` from 12 to 47**, while the total time the mutex was *held*
  barely moved, 14.98 s against 14.50 s. Four times the workers bought four
  times the queue and the same serialised section.
- **Both proposed fixes were built or priced, and both are rejected.** The
  bullets above say where the time goes and they are right. They were then read
  as saying what the *rate* is, and that does not follow — a queue forms at
  whatever is slowest to acquire, which need not be what is slowest to finish.

  *One log per signal* was implemented and measured against the binary it
  replaces: `scripts/measure/wal-split-ab.sh`, nine paired passes at 4/32/96
  connections across three sittings minutes apart, B before A inside each pass.
  **records/s signs split at every shape** — medians 0.948, 1.039 and 0.909 with
  4, 6 and 2 of 9 passes favourable — so no throughput figure from it is
  quotable, and the 1.125x that the first sitting's thirty-two-connection column
  reported on 3 of 3 passes is withdrawn by the other six. The mechanism is
  unambiguous where the rate is not: `wal.lock_wait` does fall, 0.63x at four
  connections and 0.795x at thirty-two, and `wal.write` takes all of it back at
  1.94x and 2.39x with **all nine passes agreeing at both shapes**. The reason is
  the device, and it is measurable with no Mira code in the loop — two concurrent
  appenders at the measured 790 KiB frame return 0.98x the aggregate bandwidth of
  one and three return 0.86x, both signs split. Three mutexes are free; a second
  appender is not. The diff is on the `wal-per-signal` branch, not deleted.

  *Group commit* needs no arm of its own, because the envelope both fixes share
  was measured directly. `scripts/measure/wal-volume.sh` symlinks `<data-dir>/.wal`
  at a RAM disk and changes nothing else, which deletes the serialised section
  rather than shortening it: `wal.write` −89% to 0.245 ms, `wal.held` −83% to
  0.395 ms, `wal.lock_wait` −93% to 1.245 ms, `wal.inflight_max` off its pin at
  10 of 12 and `runtime.lag` from 33.9 ms to 2.967 ms. Throughput moves
  **1.096x at thirty-two connections on 3 of 3 passes, and 1.005x at ninety-six
  with signs split**. A *perfect* log fix is worth ten percent at one shape and
  nothing at the other; group commit writes the same bytes down the same fd, so
  it cannot be worth more than that and is not worth building.

  That run needs the sweep period shortened to truncate inside it — a bounded
  log is what the four withdrawn RAM-disk figures in that script's header lacked —
  so it is a one-line build (`ticks % 240` → `ticks % 4` in `wal_maintenance`),
  the same binary in both arms, and both arms now assert `0 shed` before their
  number is read.
- **The constraint behind the log is admission, which is the flusher.** The same
  RAM-disk dump says where the queue re-forms once the log is free, and it is not
  where the bullet two up ruled it out. `submit.admit` is 0.000–0.002 ms in every
  disk-backed dump in the table and was dismissed on exactly that reading; with
  the log's write removed it is **44.054 ms of a 47.910 ms `submit.total`, 92%**,
  while `wal.encode`, `wal.lock_wait`, `wal.held` and `wal.write` together come to
  3.7 ms. Admission blocks when no `Config::queue` slot frees on any shard, so
  what bounds ingest is the rate at which blocks seal and publish. The log was
  the louder constraint, not the binding one.

  What is *not* claimed here is why the flusher is slow. Two candidates are open
  and this measurement does not separate them: its own CPU — Arrow encode plus
  zstd, against 2.23 of twelve cores busy, which is consistent with a few
  saturated flushers on an idle box — or the volume it shares with the log, which
  the concurrent-appender control says is already at its limit with one writer.
  Separating them is one probe around the seal, not an argument. **What is
  settled is the envelope: anything spent on the log's mutex is spent inside 10%**,
  so the next measurement belongs on the flusher and not here.
- **What did land on the log, which is little.** What
  did land is `Wal::sync()` taking its `F_FULLFSYNC` outside the lock rather than
  inside it: structurally right given the 4,230 µs section 10 already publishes
  for that call, and **not measured to move any number in the table above**. The
  sweep does call it — four times a second for the life of the process, so ~80
  times in a 20 s probe run — but at 4,230 µs on a 250 ms period that is a ~2%
  duty cycle, which lands on whichever exports are unlucky rather than on a mean
  taken over hundreds of thousands of them. The half a 20 s run never reaches is
  truncation, which is the 240th tick.
- **The box, and a number withdrawn.** This machine is not quiet: an
  idle-before baseline swung between 6% and 81% busy across consecutive runs,
  and two runs of the *identical* 96-connection configuration minutes apart
  returned 1,814,829 and 2,229,315 records/s, a 23% spread. **The 26% fall from
  32 to 96 connections that the ingest row publishes did not reproduce on the
  day the diagnosis was measured — the fall was 5%.** The published rates were
  taken on a quieter day and are left as they were rather than restated from a
  noisier one, and the diagnosis deliberately rests on ratios taken inside one
  process during one run, which do not care what the rest of the machine was
  doing. Reproducing it should move the absolute rates and leave the ratios
  alone.
- **`--offload` costs the retention sweep and nothing else.**
  `scripts/measure/offload-cycle.sh`, one corpus, one box, back to back: 137
  blocks, 3,599,317,452 bytes (3.352 GiB), three signals (62 logs, 64 traces, 11
  metrics). The same sweep over the same corpus is **0.868 s and 0.518 s** as the
  unlink it always was and **14.999 s and 14.844 s** with `--offload file://`, so
  against the medians the copy is 14.229 s — **241.2 MiB/s**, which is `read` +
  `write` + `fsync` per file on this volume. `mira offload restore` brings it all
  back in **12.786 s, 16.642 s and 19.489 s**, a median of 16.642 s =
  **206.3 MiB/s** but a spread of 176.1 to 268.5 MiB/s across three runs.

  An earlier revision of this bullet read the two directions "agreeing within
  6%" as a check that neither was doing something clever. It is withdrawn: at
  n=1 each that agreement was a coincidence of two samples, and the restore
  samples alone vary by 52%. What the three restores support is weaker and
  honest — they bracket the upload figure rather than contradict it, and the
  volume, not the code, is what this section can see. `mira offload list` over
  all 137 blocks is **0.050 s**, one `readdir` per partition, which is the entire
  catalogue. A second `restore` copies **0** blocks. Both costs land on the
  retention `spawn_blocking` thread, so the ingest rows above are unchanged by
  the flag.

  Compatibility is checked two ways, because a query comparison alone would
  not catch a silently re-encoded block. The same two queries — page one of the
  newest logs, and a predicate that prunes nothing so every block is opened —
  return **byte-identical** responses over the original and the restored corpus.
  `diff -r` over all 137 restored block directories against the store reports
  **no difference at all**. A third check — a second, independent upload into a
  different prefix, compared against the first — would be the one that shows the
  writer copies bytes rather than re-encoding them
  reproducibly-but-differently. `offload-cycle.sh` does **not** run it; it uses
  one prefix, and an earlier draft of this paragraph claimed three checks where
  the script performs two. The check is also the one that needs measuring least:
  `offload::copy_file` is `io::copy` over the two descriptors, deliberately not
  `fs::copy` (which on macOS is `fcopyfile(COPYFILE_ALL)` and would carry the
  source `mtime` across), so there is no encoder in the path for a re-encode to
  hide in. One caveat is
  itself a measured result: an md5 of a whole query response is the wrong
  instrument and was withdrawn as one here, because two processes over a single
  unchanged directory differ in `elapsed_us` and nowhere else — that field is
  normalised before the comparison above, and the fact that it is the *only*
  unstable field is what makes the comparison worth anything.
- **A query verifies the tables it reads, and 0.0.3 read tables no query
  wanted.** The per-table CRC32 shipped in 0.0.3 (section 3.3), so nothing about
  the format changed here; what changed is that `Block::open` mapped and hashed
  every attribute level of every block it opened, including blocks a scan was
  about to reject. Those tables are **42.9% of a logs block** (645,830,556 of
  1,506,872,204 bytes) and **45.7% of a traces block** (645,882,486 of
  1,413,774,770) for as long as the block is plain, and 5.5% and 6.3% once the
  cold tier has been over it — which is a result in its own right and is the
  last paragraph here.

  `scripts/measure/lazy-detail.sh`, 9 passes × 5 reps per case per build, two
  binaries alternating inside each pass, `blocks_scanned` printed beside every
  median and equal to `blocks_total` in every row. The corpus is 115 blocks —
  50 logs, 53 traces, 12 metrics — 611 tables and 2,927,859,966 bytes, ingested
  minutes before the run and **none of it compacted**. Three arms, because two
  changes landed in one release and either would otherwise be credited with the
  other's work:

  | case | cache alone | split alone | both |
  |---|---:|---:|---:|
  | scan-miss-logs, 50/50 blocks, 8,898,000 rows | −18.6% | **−30.8%** | **−43.3%** |
  | scan-miss-traces, 53/53 blocks, 8,898,000 rows | −23.9% | **−46.8%** | **−62.2%** |
  | scan-attr (control) | −26.4% | −2.3% | −23.5% |
  | page-100 (control) | −27.9% | +2.0% | −25.4% |

  "Cache alone" is the verification map against 0.0.3, "split alone" is this
  binary against the one carrying only the map, "both" is this binary against
  0.0.3 — end to end, 68,869 → 40,806 µs on the logs scan, 48,425 → 18,625 on
  traces. The third column is not the sum of the first two and does not have to
  be, but it is close to their **product**: 0.814 × 0.692 predicts −43.7%
  against −43.3% measured, 0.761 × 0.532 predicts −59.5% against −62.2%, 0.736
  × 0.977 predicts −28.1% against −23.5%, 0.721 × 1.020 predicts −26.5% against
  −25.4%. Two independent multipliers on one read path is what "the two changes
  stack" (section 3.3) has to mean, and that arithmetic is the check on it.

  The controls are the point of the middle column. `scan-attr` is the *same
  query shape* as `scan-miss-logs` — same corpus, every block scanned, zero
  matches — except that its predicate names an attribute, so both builds must
  read the attribute tables; `page-100` renders a hundred rows, so both builds
  must read theirs. Neither may move under the split and neither does, but the
  median alone does not say that: what says it is the **sign** of the nine
  per-pass deltas. Both treatments are negative in 9 passes of 9 (logs −38.6 to
  −3.5, traces −59.9 to −42.2); both controls change sign (scan-attr −17.1 to
  +30.6, page-100 −21.2 to +34.6). A control whose median is small but whose
  deltas all point one way would be a real effect being called noise, and this
  is the distinction that catches it.

  The controls do move in the other two columns, by about as much as everything
  else, and that is the verification map doing exactly what it should: it saves
  a re-hash on every query that reopens a block, including the queries that read
  the attribute tables. A caveat that belongs to those two columns and not to
  the middle one: the harness runs one server per build per pass with two
  warm-ups per case inside it, so the map is full before the first timed sample.
  Those are warm-map figures, the upper bound, and a client that reconnects to a
  fresh server pays the first hash again. The middle column is free of that,
  because both of its binaries carry the map and both are warmed the same way.

  Traces gains more than logs because of what is left after the map and the
  hash come out. The traces predicate is over `name`, a dictionary column
  resolved once and then matched on u16 codes, so nearly all of that query
  *was* the map and the hash. The logs predicate is a substring scan over a
  `Utf8` `body` column, which is real work that not-hashing does not remove.
  The 31% is the floor, not the headline.

  **The table published here before this run was taken while the cold tier was
  rewriting the corpus underneath it, and it is withdrawn.** It read −25.0% and
  −43.6% against 0.0.3 over 137 blocks. `compact` rewrites a block ZSTD-encoded
  once it has aged out of its partition hour, eight blocks per signal per sweep,
  from inside the server this harness starts fourteen times — so a run that
  begins on a plain corpus ends on a compacted one, both arms drift upward
  together across the passes, and a pooled median then reports how far through
  that transition each pass happened to land. It is why the harness now
  fingerprints the tables before the run and after every pass and refuses to
  continue if they moved, why it reports a per-pass paired delta beside the
  pooled one, and why "none of it compacted" is stated above as a property of
  the corpus rather than assumed.

  Run against the *same* corpus after the tier has finished with it — 137
  blocks, 718 tables, 440,421,916 bytes, all 137 marked cold — the split is
  **−33.5%** and **−56.0%**, controls −13.4% and +2.0% and both changing sign
  across the nine passes. So the split survives compaction, which is not what
  the byte shares predict: the attribute tables are **5.5%** of a compacted logs
  block (11,874,948 of 216,220,380 bytes) and **6.3%** of a traces one. They
  compress **66.3×** against the root table's **5.14×** — thirteen times better
  — so on a cold block what the split skips is not mostly bytes to hash, it is
  an inflate. The 1.11× that compaction costs a warm read, further down this
  section, is a cost this stops paying on tables nothing asked for.
- **The per-process verification cache: what it is worth, and the number that
  nearly kept it out.** The second half of the same idea is to remember that a
  block was verified so a later open can skip the hash. It is in this release
  (section 3.3) and the "cache alone" column above prices it: **−18.6% to
  −27.9%** across the four cases, near-uniform because, unlike the split, it
  helps every query that reopens a block rather than only the ones that read no
  attributes. Two other numbers were taken while the answer was still going to
  be no. Both are kept, because one of them is a lesson in what a measurement is
  allowed to decide.

  The one that decides nothing first, with what it does not show stated plainly.
  `scripts/measure/block-reopens.sh` runs 18 representative queries over 126
  distinct blocks and records **21 block opens in total** — 12 queries open
  exactly one block, 3 open three, 3 open none. That is a measure of how much
  block spread a sample of queries has, and it is **not** the quantity a
  process-scoped cache is priced on: the cache lives for the process, so what
  decides it is how often a long-lived server reopens the same path across
  thousands of queries against a hot recent window, and eighteen queries against
  a denominator of 126 blocks cannot see that. Read as a reopen count it says a
  cache is pointless. It was very nearly read that way, against a change that
  then measured a quarter off every case in the table.

  The other is the ceiling, from a throwaway build whose CRC comparison was
  patched to always pass (built into a scratch target dir and reverted
  immediately; it is not in the tree and not behind a flag). Against the
  split-only binary, 5 × 5 samples: scan-miss-logs −18.0% (58,031 → 47,572 µs),
  scan-miss-traces −29.5% (32,954 → 23,239), scan-attr −26.0% (63,662 →
  47,085), page-100 −24.1% (3,865 → 2,933). That is verification made free
  rather than merely cached, so it is more than a cache can reach — a cache
  still pays the first hash of every file.

  It brackets the measured cache rather than bounding it, and the honest reason
  is that the two runs share neither corpus nor baseline: −26.4% measured
  against a −26.0% ceiling is two methods agreeing inside their spread, not a
  cache beating its own limit. What the pair is good for is that they approach
  the same quantity from opposite ends and land in the same place — on this
  shape of corpus the CRC is about a quarter of an open, and a map that rarely
  misses recovers about a quarter.

  Two caveats survive, because the ceiling is a method and not a constant. It is
  corpus- and block-shape-dependent: it is the CRC's share of the read path, so
  a corpus of fewer, larger blocks spends a larger fraction of each open inside
  the hash and would measure a bigger share from the same method. And "make
  verification free and re-time it" is the method, whatever mechanism does the
  making-free — a patched comparison and a pre-warmed verification map are
  measuring the same quantity, so two such figures taken on two corpora are not
  in dispute with each other.

  The two objections this bullet raised while the answer was no are both
  answered in the shipped version rather than argued away. The key must not be
  the path, because the tier replaces a file under a verified path an hour after
  it lands: it is keyed on length, mtime and `ino` together behind a settle
  window, and section 3.3 is the argument for each of the four parts. And the
  footprint is capped rather than unbounded — `VERIFIED_CAP` at 65,536 entries
  against ~1,800 tables for the largest corpus here, cleared wholesale rather
  than evicted, with the LRU named as the upgrade path in the comment that sets
  it.
- **A restart replays past the slowest shard, and it is the allowed direction.**
  `scripts/measure/restart-replay.sh`, three paired runs per binary, one restart
  each: the rows the corpus gained across the restart were 0.255% (+26,000 of
  10,188,000), 0% and 0% on this branch and 0%, 1.318% (+130,000 of 9,866,000)
  and 0% on 0.0.3. Traces gained nothing in any of the six. Both medians are 0%
  and the largest single excursion is the **baseline's**, so this is not a
  regression in the change — it is a property of the log, surfaced by measuring
  for it.

  The mechanism is `Wal::watermark_for`, which returns the oldest unpublished
  sequence across a signal's shards. One slow shard pins the watermark low, and
  a boot then replays frames that a published block already covers. The
  function's own comment says being too low is the allowed direction of error:
  duplicated rows over lost ones. No loss was observed in any run. It is
  recorded here because it is a measurable cost of the no-coordination-state
  rule (there is no committed cursor to reconcile against) and because it
  invalidates any before/after corpus comparison taken across a restart —
  `offload-cycle.sh` drains to `replayed=0` before it takes a baseline for
  exactly this reason.
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
  path* costs **16 to 17 ns/row**. So the scan is a third of what the dearest
  predicate pays and under 3% of what the cheap ones do; the rest is
  `Block::open` — the `mmap`'s minor faults, the dictionary scan and the two
  child indexes. The clearest statement of it is that `limit 1` and the whole
  block cost the same per row: asking for one row and asking for all of them are
  the same query, because the block had to be opened either way.

  The CRC32 of every table body (section 3.3) used to be in that number, re-paid on
  every open of a file that by construction never changes. It is now verified
  once per process (section 3.3), and the harness prices the difference rather than
  inferring it: it publishes the block, times the read path with verification
  on, backdates the files so the cache accepts them, and times it again. At
  2,000,000 rows on a 386.1 MiB block, 202 bytes/row, nine paired passes per
  binary alternating on the same box, the whole-block row is **1.37×** on the
  binary carrying only the cache and **1.47×** on the one that also has the lazy
  split — re-verification is 27% and 32% of the read path there, and across the
  four rows the medians run 1.37× to 1.75×.

  Those two arms are **not separable**, and that is the result rather than a
  failure of it. Per pass the whole-block row ranges 1.14–1.59× on the one and
  1.22–2.21× on the other, and the ranges overlap almost entirely. The mechanism
  is in the harness: every case it times ends in `assert!(hit > 0)`, so every
  case returns rows, and a block that returns a row has its attributes rendered
  and therefore its attribute tables read. The split changes which tables an
  open reads only for a query that reads none of them, and by construction this
  harness has no such case. The corpus A/B above is where those live.

  The figure this paragraph carried from three passes — median 1.55×, **35–39%**
  of the read path — sits inside both arms' ranges and is **withdrawn as a
  median**: three samples of a quantity that moves between 1.14× and 2.21× do
  not carry two significant figures. Between a quarter and two fifths of a
  single block's read path is what this harness supports.

  **The arithmetic that used to close this paragraph closed on a coincidence,
  and it is withdrawn.** It read: a full scan CRCs 4,371 MiB of log blocks, and
  4.58 GB in 885 ms is 5.2 GB/s, which is what `crc32fast` does here — therefore
  the unpruned scan is integrity-check-bound. Warm on this machine `crc32fast`
  is nearer **27 GB/s**, so 5.2 GB/s was never its rate and the agreement was
  luck. On a corpus that stays resident, removing the redundant CRC outright
  moves an unpruned scan by about **1.1×** — the 5.01 GiB row of the table below,
  89.1 ms to 79.8 ms, cache against 0.0.3 with no lazy split in either. Worth
  having, and not what "integrity-check-bound" promises.

  It is also not a constant, and the corpus A/B at the top of this section is
  where that shows. The same comparison — the cache alone, against 0.0.3 — is
  **1.23×** on an unpruned logs scan and **1.31×** on traces over 2.93 GiB of
  plain blocks, against 1.12× here over 5.01 GiB. The ratio moves with the
  corpus because the denominator does: the paragraph below the table says an
  unpruned scan is bound by whether the corpus fits in page cache, and a fixed
  saving against a growing bound is a shrinking ratio. Quote 1.1× as this
  corpus's figure, not as the change's.

  Where the cache pays better is the query that opens little and re-opens it
  often — trace by id over one block of 139,264 rows goes from 3.79 ms to
  **2.58 ms**, 1.47×, because there the CRC is a large share of a small amount of
  work. That row is **unchanged by the lazy split**, by the same mechanism as the
  harness above: a trace lookup returns rows, rows are rendered with their
  attributes, so both binaries read the same tables. The rows that prune to one
  block are unchanged by the *cache* too, and have to be: it saves the *second*
  verification, so a query that opens a table once in a process's life pays
  exactly what it paid before.

  What the unpruned row is bound by is the thing the *first* column is bound by,
  which this section named above the table and then did not follow down. The
  second sitting says so by changing only the corpus size. Both binaries, same
  predicate, ~187 K rows per block either way, back to back:

  | Corpus | This binary | 04561ed | Per row | Spread within one binary |
  |---|---|---|---|---|
  | 9.6 GiB, 168 log blocks, 31,170,560 rows | 847 ms | 981 / 1,992 ms | 27–64 ns | **2.6×** |
  | 5.01 GiB, 48 log blocks, 9,011,200 rows | **79.8 ms** | 89.1 ms | **8.9 / 9.9 ns** | 1.5× |

  On the 9.6 GiB corpus the two arms are **not separable** — one binary against
  itself ranged 570 ms to 1,469 ms across five consecutive calls, and the second
  pre-fix pass landed at twice the first. That is the measurement, not a failure
  of it: the OS compressor grew by 1.6 GiB during that run, and what was being
  timed was eviction. On the 5.01 GiB corpus, ten interleaved samples per arm,
  the medians separate cleanly and the per-row cost is **three times lower on
  the same binaries**. **An unpruned scan is bound by whether the corpus fits in
  page cache**, and 18 GiB of RAM on a machine doing anything else does not hold
  9 GiB of it. The 885 ms in the table is 32.7 ns/row, which is the upper row's
  regime — so "neither is a cold-disk number", above the table, was a claim
  about the page cache that the page cache did not honour.

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
  almost no rows to filter, and `series` is the metrics route, which read 575–616
  ms p50 before and 686–702 after.

  **`series` is not on this path at all, and saying so took a controlled A/B.**
  `series.rs` is byte-identical across the change, and the only vectorised
  function reachable from it, `attr_parents`, is called from inside
  `q.terms.iter()` — empty for the harness's query, which carries no `where`. So
  the measured query executes none of the changed instructions, and two binaries
  differing only in the read path confirm it: 449.7 ms against 445.1 on one pass
  and 524.5 against 608.7 on the next, which normalise to 43.6, 42.5, 47.6 and
  47.5 µs per matched row — differences in both directions, all smaller than one
  binary's spread against itself. The mix explains the rest: eight closed-loop
  readers issue `series` 25% more often once the other five classes are five
  times cheaper, and `Scan::wave` spawns a thread per block per wave, so the
  classes that did speed up multiplied their spawn and shootdown rate by about
  the same factor. `series` fans out to nothing and absorbs it.

  **Chasing it did find a real defect, and it is the same shape as the one
  above.** `collect_attrs` scanned the whole attribute table per parent and runs
  once per matched data point, so the metrics path kept exactly the quadratic
  semi-join section 7.6 removed from the log path — missed because `series_open`
  loads its tables directly instead of through `query::Block::open`, so it never
  saw `Attrs`. It uses it now. `series_cost_per_point` prices the result at a
  flat 0.9–1.1 µs/point where it used to rise with the point count (2.1, 6.9,
  31.6 µs at 2 K, 10 K and 50 K), which is 4.28 ms → 2.47, 69.4 → 9.0 and
  1,582 → 53.9. On the load harness it is neutral, and the arithmetic says why:
  its metrics blocks hold ~1,600 points against ~2,000 attribute rows, so the
  join is ~7% of the query and twenty-two blocks × ten tables of `mmap`-and-CRC
  is the rest. Series prunes on the directory name alone — no bloom, no zone
  probe — and its block loop is sequential where `search` claims helpers from
  `SPARE`. Those two are the levers left on this route, and both are larger than
  a patch.

  The mix's `tail` class matched nothing on this corpus, because the data is
  older than the window `tail` asks for, so its numbers measure the empty path
  and are not quoted here — the unfiltered `limit 100` row of the table is the
  honest version of the same question.

  What is left is not O(n) in the block's row count: `scan_cost_per_row` prices
  that term at 0.047 ns/row, so a 204,800-row block spends about 10 µs of the
  4.6 ms it takes. What is left is O(bytes) in the block's *size*, paid at open,
  which also means `target_block_bytes` is **not** the lever it was once written
  up as. Halving it halves the bytes each block maps and hashes on its first
  open, and doubles the number of
  blocks, so a query that prunes to one block gets faster and a query that prunes
  to none gets nothing. It is not changed here because it only ever helped the
  first kind, the tradeoff runs the other way for compression ratio and directory
  size, and the number to tune it against is a workload nobody has yet. Sharding
  has already moved it in that direction by accident: six sealers per signal make
  a log block 204,800 rows where one made 330,000.
- **A block cache was talked into being the obvious next lever twice, in
  opposite directions, and neither time by a measurement of the cache.** It was
  first ruled a small win on the belief that `open` and the CRC were the small
  part of a single-block query. They are not. `scan_cost_per_row` at 2,000,000
  rows, median of four passes, prices the dearest predicate — `body contains` —
  at **4.99 ns/row** against a whole read path of **18.23 ns/row** while the
  checksum is being verified and **11.44 ns/row** once the file has settled, so
  `open` alone is **56%** of that query and `open` plus the CRC is **73%**. On
  the cheapest predicate, `no term` at 0.04 ns/row, it is essentially all of it.
  (This entry said "55%" before, without saying which of the two paths it meant;
  it was the settled one.) So the entry was rewritten to
  call the cache the obvious next lever. That does not follow either: "the term
  a cache would attack dominates" is an argument for attacking the term, not for
  attacking it with a cache.

  Two changes in this release take that term apart without a cache, and they
  attack different halves of it. The **lazy attribute-table load** removes opens
  that should never have happened: the tables a query never reads are never
  mapped and never hashed, **30.8% off a logs scan and 46.8% off a traces one**
  measured against the binary that already carries the map — the two figures this
  entry quoted before, 25% and 44%, are the ones withdrawn earlier in this
  section for having been taken while the cold tier rewrote the corpus. The
  **process-scoped verification map** (section 3.3) removes the repeat hashes on
  the opens that remain, and it holds no mappings and needs no invalidation.
  Neither pays a resident byte. What a real cache would add on top of both is
  the `mmap` and the two child indexes, it still cannot help the first open in a
  process, and it does pay in resident memory — the axis this section already
  scores worst. It stays on the section 10 list.
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
  8.4× fewer bytes to touch and CRC32 over 8.4× fewer of them. The second half
  of that is worth less than an earlier revision of this paragraph claimed: the
  CRC is paid once per file per process now (section 3.3), so on a compacted
  block that is read more than once it is 8.4× fewer bytes of a term that is
  already amortised to near nothing. The first half is the one that carries the
  comparison. Cold — which
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

**Shared-nothing ingest, a stateless merging proxy for reads, discovery
borrowed from the platform.** Ingest (section 12.1) was coordination-free
already, and the shared-volume topology of section 12.5 answers the same
requirement with no fan-out at all. The fan-out half is now built too:
`mira proxy` (section 12.2), shipping with the hash-based ingest routing that
section 12.2 used to say must never arrive without it. What is still not built
is peer-to-peer broadcast *between storage nodes*, and section 12.2.4 says why
the proxy replaced that design rather than preceding it — along with the part
of this that is not yet earned, which is the evidence that any of it is needed.

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

### 12.2 Query — `mira proxy`

A storage node answers from the blocks it can see: every writer's, on a shared
volume (section 12.5); its own, shared-nothing. It does not fan out and it has
no peer set — `cluster.peers`, which once named one in the config file, was
deleted rather than left in place because it was parsed, logged and read by
nothing. An unread key is worse than a missing one: it is a setting an operator
configures, sees accepted, and believes is in effect, and it sent them to point
a headless Service at a feature that did not exist.

The fan-out lives in a **second process that stores nothing**. `mira proxy` is
the same binary under a subcommand, given a static list of replica addresses
(`--replica http://host:port`, or `proxy.replicas` in the file), serving the
OTLP endpoints and `/api/v1/query` on one HTTP listener and no gRPC one. It is
`crates/mira/src/proxy.rs`, roughly six hundred lines, and it cost **zero new
dependencies**: `hyper`, `hyper-util` and `http-body-util` were already direct
dependencies of the binary for the webhook dispatcher.

#### 12.2.1 Why it can hold no state — the cursor was already global

Paging is what usually forces a coordinator on a fan-out read. N nodes each
answer "the newest hundred", the merger cuts to a hundred, and the rows it did
not emit are now *somewhere* — so for the second page it has to remember, per
reader, how far into each node's stream it had got. That per-reader position is
coordination state, it has to survive the proxy restarting, and it is the reason
scatter-gather usually comes with a session store attached.

Mira does not need one, and the reason predates this feature. The keyset cursor
of section 8 is `(ts, node, seq, row)` where `node` is `block::node_id` — so the
sort key is **already total across every row on every replica**, not merely
within one. Nothing was added to make that true; keyset
paging needed a key intrinsic to the record rather than to the query, the node
id was in it because block names have to be unique per writer (section 3.2), and
that is the whole of it.

So the protocol is: broadcast the caller's document, with the same `after`, to
every replica; each returns the newest `limit` rows strictly behind that cursor
out of its own blocks; sort the union on the cursor order; cut to `limit`; hand
back the cursor of the last row emitted. Every row no replica emitted sorts
strictly after that cursor *on every replica*, so the next page is exact — no
duplicate, no gap — and by the time the response is written the proxy has
forgotten the reader exists. Principle 4 is satisfied by construction rather
than by care, which is the only way it survives a maintainer who has not read
this section.

Two details of the merge are load-bearing:

- **`next` is set by either side.** A replica that reported its own `next`
  means that node is holding more; the merged set exceeding `limit` means the
  cut is holding more. Only checking the second misses the case where N short
  pages fit under `limit`, and the reader stops pages early believing it is
  done.
- **A read that cannot be complete is an error.** One replica timing out fails
  the whole query rather than returning the others' rows. The alternative is the
  scatter-gather failure this document has always refused: six sevenths of the
  data, with nothing in the response saying so. The old design answered that
  with a per-response list of which peers replied; all-or-nothing is the same
  guarantee with nothing to render, nothing to parse, and no way for a caller to
  ignore it.

#### 12.2.2 What the node had to grow, and what it did not

One thing, and it is smaller than it sounds. A rendered row is an OTLP record
and carries no cursor, so a merger holding two nodes' rows could not tell which
came first. The node now accepts `"cursors": "true"` on a search document and
returns a `"cursors"` array beside `"rows"`, index-aligned, absent otherwise.

Beside the rows and not inside them, because of principle 3: the rendered row
*is* the OTLP record, and a reader that did not ask for cursors should not have
to step over one in every object to find the fields it came for. Absent rather
than empty for the same reason `next` is absent on the last page — an empty
array is a value a client has to interpret.

The proxy adds that key textually rather than by re-serialising the document
through a KYAML writer. A round trip would normalise the `where` terms, which is
exactly the part of a caller's document most likely to have a spelling this file
has not thought of. It then parses the result back and **refuses the request**
if the flag did not take, because a document that reached the replicas without
it would come back without cursors and the merge would silently fall back to
whatever order the replicas answered in.

What the node did *not* grow is a hop flag, a peer-aware code path, or any
notion that it is part of a set. A replica behind a proxy is byte-for-byte the
binary that runs alone.

#### 12.2.3 What it refuses, and the routing that keeps the door open

The proxy serves `/api/v1/query` and the three OTLP endpoints. `correlate`,
`map`, `metrics/query`, `metrics/names` and `entities` answer **501 naming the
path** and telling the caller to query a replica directly.

They are refused rather than approximated because each is built by walking one
node's blocks — a trace assembled from the spans that are local, a service map
from the edges that are local. Merging those is not "sort and cut": two nodes
each holding half a trace produce two partial frames and there is no cursor to
interleave them on. A plausible subset is the failure mode section 7.2 refuses
to hash for, and it is worse here, because nothing in the response would say it
was partial. 501 and not 404: a 404 reads as "old build" and sends whoever hit
it looking at versions.

**Hash-based ingest routing is what holds that door open, and it ships here
because this is the only release it is allowed to ship in.** The proxy splits
an export resource by resource on `resource_key` — the same 64-bit entity
identity of section 7.2, the one the storage layer already joins on — so every
record describing one entity lands on one replica whatever batch it arrived in.
`NO_IDENTITY` is spread by position instead: a resource with no identifying
attribute has no entity to keep together, and hashing it to a fixed slot would
pile every unidentified sender in a deployment onto replica zero.

That placement buys nothing for the merged read above, which fans out
regardless. What it buys is that each replica's blocks stay entity-local — the
`_entity` block filter keeps its selectivity, and "everything this pod emitted"
stays a question one node can answer *completely* rather than a fragment of.
That is the property the 501 is waiting on: a future `entities` or `correlate`
on the proxy is a routed call to the one node that has the whole answer, not a
merge. Routing without fan-out would have been strictly worse than no routing,
which is why the ordering constraint was there.

Modulo and not a consistent hash ring, marked `ponytail:` at the line. The
replica list is static, so the only event that remaps keys is an operator
editing it and restarting — and at that point the blocks already written do not
move either way, because **retention is the rebalancer** (section 12.4).
Consistent hashing buys a smaller remap for a rebalance this design does not
have.

A partial ingest failure is a 503 for the whole export, so the exporter retries
the batch and re-delivers the sub-exports that did land. That is at-least-once,
which is what OTLP already is end to end; making it exactly-once needs an
idempotency key and a seen-set on the node, which is per-sender coordination
state for a duplicate section 12.4 already says the system tolerates.

#### 12.2.4 What this replaced, and what it has not earned

The design this section used to hold was peer-to-peer: a query arriving at any
replica is broadcast to its peers, executed locally, merged, with a hop flag so
a peer does not re-broadcast and a per-response list of which peers answered.
The proxy is strictly less machinery for the same result — no peer set in the
storage node, no hop flag, no partial-results rendering, and the one process
that does hold a list of addresses holds nothing durable, so killing and
restarting it reconciles nothing. The set-union argument that made the old
design work is untouched and is why this one works too: block-local ids never
leave a node, and the only identifiers that cross the wire are the globally
stable ones — `resources.key`, `trace_id`, `span_id`, timestamps. Had entity
identity stayed "equality of the resource attribute set", any cross-node read
would have needed a cluster-wide resource dictionary, which is coordination
state. One decision in section 7.2 paid for the merge, the routing, and the
correlation this still refuses.

**What has not been earned is the case for building it.** The previous version
of this section said the proxy was cheap and still should not be built, because
nothing had measured a single node's ceiling to be the binding constraint. That
measurement was then attempted — it is the plateau work in section 11 — and it
did not deliver the confirmation. It established the opposite of the premise
everyone was working from: the log's mutex is not the ceiling, both proposed
fixes are rejected, and once the log is free the queue re-forms at block seal
and publish. Nothing in it says a *node* is saturated at a rate a real workload
reaches, and it was taken on a laptop rather than on production-representative
hardware.

So the honest status is: the mechanism is built, tested and free of new
dependencies, its *cost* is measured in 12.2.5, and the argument that it is
*needed* rests on an instruction to build it rather than on a number. The two
objections the old text raised have both been answered by the design — the
partial-results contract became all-or-nothing, and the second deployable is a
subcommand of the same binary — but "no workload has reached the ceiling" is not
one of them, and it still stands. Section 11's query finding also still stands:
an unpruned scan is bound by whether the corpus fits page cache, and fan-out
does not change that, since each replica still scans its own share off its own
disk. What fan-out buys is capacity — more disks, more page cache, more cores —
not a faster answer to the same query.

#### 12.2.5 What the hop costs, on one box

`scripts/measure/proxy-ab.sh`. Arm A is one node with the generator pointed
straight at it, the shape every number in section 11 was taken in. Arm B is two
replicas and a proxy, with the generator pointed at the proxy. Same binary in
both arms — the proxy is a subcommand, so there is no preserved-binary dance and
no version skew. Both arms send `--records`, not `--for`, so the bytes and the
corpus the read leg then scans are identical. Paired and alternating, B first,
nine passes a shape across three sittings minutes apart, median of the per-pass
ratios; two asserted controls, `0 shed` and the paged read returning no row
twice, the second counted on the cursor because it is unique by construction.

Read the ratios as a cost and never as scaling. Arm B runs three servers and the
generator on the same twelve cores, on one disk, so both arms contend for the
same everything and arm B pays an extra process to do it. The one question this
box can answer is what the extra hop costs; what a second machine would buy is
not on it.

| Shape | Ingest B/A | Read B/A |
|---|---|---|
| 4 connections | **0.767x**, 0 of 9 | **3.89x**, 9 of 9 |
| 32 connections | 0.973, 3 of 9 — split | **2.76x**, 9 of 9 |
| 96 connections | 1.044, 5 of 9 — split | 3.882, 8 of 9 — split |

Two things reproduce. **The proxy costs ingest at four connections** — every one
of nine passes, spread 0.666 to 0.939 — and it is the shape where that should be
true: there is no concurrency to hide the extra hop, the per-record
`resource_key` and the re-encode behind. **The wide unfiltered read is several
times slower through the proxy** at four and thirty-two connections, unanimously,
and the proxy's `elapsed_us` is why: it starts before the fan-out and stops after
the merge, so it contains both replicas' entire reads plus the hop. A node's own
number measures one node's read; the proxy's measures the slowest replica.

Everything else is noise and is registered as noise. Ingest at thirty-two and at
ninety-six connections split 3 of 9 and 5 of 9 — at the shapes where the node is
already saturated the extra process is lost in the variance, in both directions.
The read at ninety-six split 8 of 9, on a pass that returned 0.620 and another
that returned 40.684; that is three servers and a generator on one laptop, not
the merge. All six figures are in `measurements.kyaml` including the three that
did not reproduce, for the reason section 11 gives: an unregistered median is one
somebody re-quotes next quarter.

None of this is the Phase 1 question and none of it answers it. It prices the
mechanism on hardware where fan-out cannot pay, which is the only hardware
available. 12.2.4 still stands.

### 12.3 Discovery without membership

The replica set is a list of addresses given to the proxy at startup and never
revisited. Mira stores nothing about the cluster. There is no gossip, no
heartbeat, no join/leave protocol and no split brain — not because they are
solved but because there is no membership to be wrong about.

In Kubernetes the list is the pods of a StatefulSet, which have stable DNS names
by construction, so it is a literal list in the proxy's config. A headless
Service resolving to all of them is the *other* half — it is how an OTLP
exporter reaches the proxy, or reaches the nodes directly when no proxy is
deployed — but it is not how the proxy finds its replicas, because a DNS answer
that changes underneath a running process is exactly the membership event this
design has nothing to do about. Adding a replica is a config change and a
restart of the proxy, which is a rolling restart of a stateless process.

A replica that does not answer fails the query it was part of (section 12.2.1).
There is no liveness tracking and no ejection: that would be membership, and a
proxy that remembered which nodes it had given up on would be holding exactly
the state this design refuses.

### 12.4 What scales, and what this deliberately does not buy

| | |
|---|---|
| Ingest throughput | Linear. Nodes are independent. |
| Storage capacity | Linear. |
| Query capacity | Linear; every replica answers independently. Direct to a node, that answer covers the whole dataset on a shared volume and that node's share shared-nothing. Through `mira proxy` it covers all of them, and one query's latency becomes the slowest replica's. |

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
  Scaling *in* collects no such dividend: Kubernetes retains the removed
  replica's PVC, but nothing reads it, so its blocks have to be re-homed with
  `mira offload push` and `mira offload restore` (section 6.1). No lifecycle
  hook does it for you, and that is a finding rather than a gap — a pod cannot
  tell a scale-in from a rolling restart, so a hook wired to `preStop` would
  evacuate every replica on the next image bump. Asking the API server which it
  was is the membership read this principle refuses *inside Mira*; section 12.7
  is what happens when something outside Mira asks instead.
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
host** sharing a local directory, and it is the mode the `--node` flag exists
for. It is **not covered by a test**, and that is worth stating plainly: the
measurement in section 12.6 was a two-process run done by hand, and what is in
the tree is unit coverage of the *mechanisms* it turned up — a failed publish
leaving no staging directory, `sweep_staging` filtering by signal and node —
rather than of two writers racing. Covering it properly means two `mira`
processes over one `TempDir` and an assertion that no published block mixes two
sealed sets, which is a test level `docs/internals/testing.md` does not have yet. Across hosts, shared-nothing is the supported shape
and `mira proxy` (section 12.2) is the answer to covering the whole dataset.
A cluster filesystem would work in principle and is not claimed. Object storage
is a larger question — it forecloses mmap entirely — and is deferred to the
market survey rather than guessed at here.

### 12.6 The staging path is the one place two writers can still collide

Everything above rests on writers never touching each other's bytes, and the
final block name delivers that: `{min_ts}-{max_ts}-{node}-{seq}-{wal_hi}` is unique per
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

### 12.7 The coordinator lives outside the binary

**Built**: `integrations/kubernetes`, a second Cargo workspace producing a
second binary, `mira-operator`, and a `MiraCluster` CRD. It is the only chart
Mira publishes; the one that installed a StatefulSet directly was removed with
it, because two charts is two answers to "how do I run Mira on Kubernetes" and
the one that cannot scale, cannot drain and cannot be told a ceiling is the
wrong default.

Everything above says a Mira tier is resized by hand. Section 12.4's scale-in
paragraph is the reason: the sequencing a safe scale-in needs — drain first,
then remove the volume — is not something a pod can decide about itself, and
section 12.3 refuses to let it ask. That argument is correct and it is about
*Mira*. It says nothing about whether something else may hold the decision.

**Why not an HPA.** Mira has no metric that moves when it needs another replica.
Section 11 measures 2.23 of twelve cores at 1,537,875 records/s, so a CPU-target
HPA reads single-digit utilisation at saturation and a memory-target one reads
page cache, which is the `mmap` working as designed. The quantity that runs out
is **disk**, and an HPA has never been able to scale a StatefulSet on its own
volumes filling. So the trigger is `free_fraction` from `/api/v1/stats` — the
number section 9's `statfs` already computes — and something has to read it.

**Why this does not violate principle 4.** The principle constrains Mira: no
Raft, no membership, no external metadata store, the block directory is the
manifest. A controller is not Mira, and the test of that is what happens when
the controller is deleted. Every Mira pod keeps ingesting, keeps serving and
keeps its blocks readable, because none of them ever asked it anything. Only the
scaling stops. That is the line between a coordinator and coordination state,
and it is the same delegation the engine already makes to the platform for pod
identity and volume lifecycle, moved up one level — Kubernetes already knows the
membership, and reading it from the API server is not a consensus protocol.

**It owns the whole topology**, rather than autoscaling a StatefulSet somebody
else installed. A controller that only writes `spec.replicas` on an object Helm
owns loses the value on the next `helm upgrade`, which reasserts the count from
the chart: the scale-out silently unwinds, and on the way down it unwinds
*after* the drain has copied the blocks out. One writer for the field that
matters.

**The two thresholds are asymmetric on purpose.** Out when the *fullest* replica
drops below `upWhenFreeBelow`; in when *every* replica is above
`downWhenFreeAbove`. Not the mean either time — `route` sends a resource to
`hash(resource) % n` (section 12.1) and resources are not the same size, so a
mean of 0.4 across ten replicas is compatible with one at 0.02, and it is the
one at 0.02 that stops accepting writes. A replica that is unreachable, or that
answers `null` for `free_fraction` because it could not `statfs` its own volume,
means *neither* decision: read as 0 it says scale out, read as 1 it says delete
a volume. The two thresholds must also not meet, and a spec where they do is
refused as `Degraded` rather than acted on — adjacent thresholds oscillate, and
every cycle of that loop moves one replica's whole dataset through `offload
push`.

**A scale-in is section 12.4's manual procedure, sequenced.** `status.draining`
is written *first*, so a controller that restarts mid-sequence resumes instead
of orphaning a volume; the StatefulSet scales down and the pod goes while the
claim stays; a Job runs `mira offload push` against the released claim — which
is why the pod has to go first, since the claim is `ReadWriteOnce` and a Job
cannot attach it while the pod holds it; and only on success is the claim
deleted. A failed drain stops before that last step, phase `Degraded`, one
replica smaller and every block still on disk. `spec.offload` is required before
the tier will ever shrink, and unset it simply never does: the cost of not
shrinking is a bill, the cost of shrinking without an archive is the data.

It does **not** re-home the blocks afterwards. That is 12.4's "no rebalancing,
ever" rather than an omission, and the same `ReadWriteOnce` constraint that
forced the order above forbids the reverse — a restore has to mount a
*surviving* replica's volume, which its running pod holds. Automating it would
mean taking a healthy replica down in order to grow it.

**ponytail:** there is no leader election, so `replicaCount` is bounded to
exactly 1 by the chart's schema and the Deployment strategy is `Recreate`.
kube-rs has never shipped one ([kube-rs/kube#485](https://github.com/kube-rs/kube/issues/485),
open since 2021); two controllers would both reconcile every `MiraCluster` and
both act on the same reading, moving two replicas for one decision. A moment
with no controller is safe, because Mira keeps serving either way. The upgrade
path is a `Lease`-based election, on the day a single-replica controller is the
thing that hurts.

The other ceiling is the CRD itself. `apiextensions` **prunes** a field the CRD
does not declare rather than rejecting it, so a cluster holding a stale schema
loses those fields silently, with no error anywhere. The CRD is therefore
generated from the Rust types by `crdgen` and `make operator-crd-check` fails
the build when the checked-in copy disagrees — and because Helm installs `crds/`
once and never upgrades it, a chart upgrade that changes the schema needs the
CRD applied by hand first. [Install](install.md#kubernetes) says so in the place
someone will read it.

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
