# 0. Corrections to the original brief

The brief specified several mechanisms by name. Seven do not survive contact
with the formats involved.

| Brief said | What is actually true | What Mira does instead |
| --- | --- | --- |
| **Delta-of-delta timestamps** | Arrow IPC has no per-column encodings. Its entire encoding surface is whole-buffer LZ4/ZSTD plus the Dictionary and RunEndEncoded layouts. Implementing DoD means inventing a buffer layout no Arrow reader understands — which forfeits zero-copy, since you must decode into a fresh allocation. Separately, Gorilla's 12× rests on samples landing on exact interval boundaries; OTLP `time_unix_nano` is a wall-clock read with 10⁵–10⁷ ns of jitter between consecutive deltas. | Plain `Timestamp(Nanosecond)`. Sort by time within a block; take the size win at the cold tier from a generic compressor. |
| **Resource/Scope dedup at block headers** | The Arrow mechanism for a "block header" is `Schema.custom_metadata`, exactly one map per file. That expresses dedup only if a block holds exactly one Resource. A block from any multi-tenant collector holds hundreds. | OTAP section 6.3: `resource_id`/`scope_id` `UInt16` columns in the root table plus separate attribute tables keyed by `parent_id`. A resource with 40 attributes shared by 10,000 records costs 40 rows and 10,000 `u16`s. |
| **Dictionary-encode high-cardinality maps** | Half backwards. Dictionary encoding is a *low*-cardinality technique with a hard ceiling — `Dictionary<UInt16,_>` raises `DictionaryKeyOverflowError` past 65,536 distinct values, and `http.url` and `trace_id` are the highest-cardinality data in the system. But the *repetition* the brief was reaching for is real: a few hundred distinct attribute values across a few hundred thousand rows. | Two key widths. Enumerable columns — attribute *keys*, `severity_text`, metric name and unit — take a `UInt16` key and seal the block at `DICT_CAP` rather than fail. The attribute *value* string column `attrs.str` takes a `UInt32` key, which a 32 MiB block cannot fill; it is the one *value* column that is dictionary-encoded, and it is worth 12.9× → 66.2× on one real block's cold ratio (section 11). `attrs.bytes`/`attrs.ser` stay plain `Binary`. |
| **Zero-copy ingestion** | Impossible on the OTLP path. `prost` memcpies every string unconditionally; varints must be decoded. Even on OTAP, `StreamDecoder` only avoids a copy when the whole message body is one contiguous `Buffer`, and an HTTP/2 body split across DATA frames is `extend_from_slice`'d. | Say **zero-copy queries**, not zero-copy ingestion. The ingest goal is *allocation-lean*: one unavoidable memcpy of the request body, then no per-field heap allocation. |
| **Lock-free ring buffer for ingestion** | Cargo-culted from LMAX, where an item is a 150 ns order struct. Here an item is an export request costing 10⁵–10⁶ ns to decode and encode, arriving 10²–10⁴ times per second. The queue is four orders of magnitude from being the bottleneck, and a lock-free queue cannot express backpressure. | A bounded `tokio::sync::mpsc` per flusher shard. A full set of them parks the caller for up to `ADMIT_WAIT` (section 4), which propagates backpressure out as HTTP/2 flow control; only a timeout sheds. Revisit if a queue ever appears in a profile. |
| **4317 and 4318 both served by tonic** | 4318 is not gRPC. Per the OTLP spec it is plain HTTP/1.1 POST of protobuf or JSON to `/v1/{traces,metrics,logs}`. | Two listeners: tonic on 4317, axum on 4318. axum is already in the tree via tonic's `router` feature, so it costs no dependency. |
| **A reflective proto3-JSON decoder for OTLP/HTTP JSON** | OTLP JSON is *not* canonical proto3 JSON. Ids are hex where every other `bytes` field is base64 — and a 32-character hex string is itself valid base64, so a generic decoder does not fail, it silently yields 24 bytes of nonsense for every `trace_id`. 64-bit integers are strings. Field names may be either dialect within one document. | `crates/mira/src/json.rs`: a hand-written decoder over the YAML 1.2 loader already in the tree (`api::parse`; YAML 1.2 is a superset of JSON, so KYAML bodies work for free — section 1). No new dependency, and the two deviations are handled where they occur rather than configured around. |

Three more, less structural:

- **`partial_success` is not a backpressure signal.** The OTLP spec says the
  client MUST NOT retry a partial success, so reporting overload that way
  permanently destroys the data. Overload is always a status code — `UNAVAILABLE` or `RESOURCE_EXHAUSTED` —
  with `google.rpc.RetryInfo` attached, because `grpc-retry-pushback-ms` is only
  honoured by clients that configured a gRPC retry policy, which OTLP exporters
  do not.
- **OTAP is not the default path.** OTLP on 4317/4318 is the universal path;
  OTAP is a collector-tier bandwidth optimisation worth roughly 2× over
  OTLP+zstd. Mira adopts the OTAP *data model* as its storage layout and will add
  the *wire protocol* as a second receiver, but the architecture must not assume
  OTAP is how data arrives.
- **The cold tier is worth one C dependency.** `zstd-sys` vendors its own source
  and builds it with `cc`, a dent in "nothing to install to build Mira" — though
  a linker was already required, so the delta is a vendored C compile, not a new
  prerequisite. No walkover: the pure-Rust `lz4_flex` keeps the tree C-free and
  also clears the 0.35 target — 0.189 on logs, 0.173 on traces
  (section 11). ZSTD is 1.5× smaller and read back faster in every pass, and the
  compression throughput it gives up — 808 MiB/s against LZ4's 976 — has nowhere
  to land: compaction is an hour-old block inside `spawn_blocking`, not the
  ingest path.

## One correctness hazard worth naming on its own

OTAP `id` and `parent_id` values are unique only **within a single
`BatchArrowRecords`**. Persisting them verbatim and then joining
`logs.id = log_attrs.parent_id` across batches produces a silent cross-product —
wrong answers, no error. Mira rebases every id into a dense, block-local
`UInt32` at ingest, which makes any join *inside* a block unconditionally
correct and removes the need for a partition-discriminant column on every table
and an extra predicate on every join.

## 0.1 What is not true yet

The README carries four of these; this is all of them.

- **Not zero-copy *ingestion*.** "Zero-copy" in this document always means
  queries.
- **No block cache.** Every query re-opens each block it touches, though the CRC
  is done once per file per process (section 3.3). Measured, it is a few
  milliseconds of a 14 ms query, not the 10x page faults were (section 11).
- **OTAP is the data model, not yet the wire protocol.** No language SDK emits
  OTAP; the receiver will be a second listener over a layout already shaped for
  it.
- ~~**No cross-replica query fan-out.**~~ Built, in a second process: `mira
  proxy` (section 12.2) merges `/api/v1/query` across a static list of replicas;
  a storage node still has no peer list, the part principle 4 protects. Still
  absent: peer-to-peer broadcast, and the reads the proxy refuses to merge rather
  than approximate (correlate, map, metrics, entities).
- **No entity *predicate*.** The entity key each block stores (section 7.2) is
  read by `/api/v1/entities` and `correlate`'s `peers`, but no query document
  accepts it as a *filter*, so "everything this pod emitted" is still an
  attribute predicate — `{"attr":"service.instance.id","eq":"..."}` — which
  misses exactly the rollout that changed the attribute.
- **No bucket-level histogram queries.** Histograms, exponential histograms and
  summaries are *stored* whole — `bucket_counts`, `bounds_id`, `scale`,
  `quantile` are all on disk — but the query surface hands each back as
  `<name>.count` and `<name>.sum`, so "p99 of `http.server.duration`" is not a
  question this engine answers yet.

---
