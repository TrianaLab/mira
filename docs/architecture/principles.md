# 1. Principles, and the mechanism each one buys

The five principles are constraints, not aspirations. Each needs a mechanism or
it is decoration.

## Performance is the product

The four axes — ingest throughput per core,
resident footprint, query p99, cost per GB — conflict pairwise: compression cuts
cost per GB and raises query latency, large blocks raise throughput and
footprint. Mira resolves them by **tiering** rather than claiming all four at
once — hot blocks are uncompressed, 64-byte aligned and mmapped; cold blocks get
compression and give up zero-copy. section 11 states the target for each axis
and how it is measured.

## Agentic, in all four senses the owner selected

- *LLM-queryable surface* — a native MCP server (hand-rolled, section 8.1) over
  the same query engine, so an agent investigating an incident issues one call
  instead of composing PromQL and TraceQL.
- *Telemetry for AI workloads* — the layout is designed around the OTel GenAI
  semantic conventions: attribute values are plain `Utf8` rather than dictionary
  keys because the table must take multi-kilobyte prompt/completion strings
  without pathology.
- *Self-driving* — no tuning knobs. `pipeline::Config::default` holds
  `target_block_bytes` and `max_block_age` as constants the YAML cannot reach.
  Adapting them to observed load is the ambition and is not built; what
  is built is that neither can be set wrong from outside. The config file
  ([Configuration](../config.md)) says *where the process runs*: three of its
  keys reach the engine and none is a tuning surface. The set is closed at
  fourteen, an unknown key a startup error, because the alternative is what
  `cluster.peers` was (section 12.2): a key read by nothing that still looks like
  a setting.
- *Agent-based internals* — the flusher tasks, three signals × `ingest.shards`
  of them, and the retention worker are a message-passing mesh, and the
  flushers are supervised: one that returns before the stop signal takes the
  process with it. A crashloop is the honest shape of "this node cannot store
  logs", where carrying on leaves one signal answering 503 forever behind a probe
  that stays green. `spawn_retention` drops its handle, so a retention sweep that
  stopped is invisible until a disk fills.

## OTLP-first

The Arrow schemas in `crates/mira-core/src/schema.rs` *are* the
OTLP Resource-Scope-Signal model. There is no transformation step to a generic
relational or inverted-index store, and therefore no place for one to lose
fidelity. The claim is only as good as the column list, though, which is the
shape every fidelity loss here takes: `LogRecord.event_name` was decoded off the
wire and had nowhere to land. Not a transformation — a missing field.

## Single binary, no operational overhead, stateless

Stateless means *no coordination state*: no cluster membership, no Raft, no external metadata store.
The mechanism is section 3.2 — the filesystem is the manifest. It also rules out
DataFusion *from the default build*: SQL for free at a cost of 47 direct
dependencies and a ~1.5M SLoC transitive tree. The binary cost was estimated here
at 68–92 MB, too pessimistic — at Mira's release profile it is **50.0 MiB and 271
crates**, against 5.76 MiB and 117, and an order of magnitude is still an order
of magnitude. There is no `--features sql` in the tree: `crates/mira` declares
`default = []` and `webhook-tls` and nothing else. The feature is the *shape* a
SQL surface would take if one is ever asked for, and section 10 keeps it on the
not-built list until someone asks. What DataFusion would not displace either way
is the hand-rolled ~2,000 LOC fast path: a 4.5 ms point lookup that already
prunes to one block of 137 has nothing to gain from a planner. With traces,
metrics, query, MCP and both UIs in it, the default build is **5.76 MiB
stripped, 117 crates** — the scale the design is defending.

## KYAML-first, everywhere

Every text format Mira reads or writes is KYAML: a strict subset of YAML 1.2
with collections written explicitly as `{}` and `[]`, every string
double-quoted, and indentation carrying no meaning.

The mechanism, without which this is a style guide: **the parser refuses
unquoted scalars.** Every value in a Mira config is a string, so one that
arrives as any other type is a startup error naming the key and saying to quote
it (`config.rs::scalar`). Nothing is coerced back with `to_string()`, the step
that turns `0x1f` into `31` and `False` into `false` with no diagnostic — and
`node` is hashed into every block directory name, so a silently altered string
is a replica writing somewhere nobody expects.

It is a principle, not a preference, because it is for the model. An
unquoted scalar's type is decided by a resolution table that varies across YAML
1.1 and 1.2 and across implementations, so the same document means different
things to different readers. A human usually notices; a model generating config
has no feedback loop and emits the majority spelling from its training data,
YAML 1.1. Two characters of noise buy config that is unambiguous by
construction, the precondition for anything else agentic touching it.

Trailing commas are allowed for the same reason — appending a key should not
mean editing the line above it, the diff-shaped mistake a generator makes. That
YAML 1.2 permits them is verified in `config.rs`'s tests, not assumed from the
spec.

---
