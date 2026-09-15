# 7. Correlation

**What everyone else ships is a client-side join across two databases**, wired by
a YAML mapping that has to stay correct as the conventions move, and it fails
*open*: a log line with no `trace_id` gives an empty panel. Mira has nothing to
join *across* — every signal for a time window is in one block — so correlation
is a storage-layer primitive, on an axis a cross-system join cannot have:
**entity identity**.

## 7.1 The join keys, in order of precision

| key | applies when | mechanism |
| --- | --- | --- |
| `trace_id` | the record was traced | exact; section 7.4 |
| `span_id` / parent | the record names a span | exact |
| span link | async or fan-in causality | `span_links` table (with traces) |
| exemplar | a metric datapoint sampled a trace | exemplar `trace_id` (with metrics) |
| **entity + time** | **always** | `resources.key`, section 7.2 — readable as a frame, not yet selectable as a predicate; section 7.4 |

An investigation that starts at an untraced error log gets nothing from the first
four rungs and *everything* from the fifth: degrading to "everything this pod
emitted in the surrounding five seconds" is the difference between a correlation
feature and a correlation demo.

## 7.2 Entity identity — `resources.key`

`resource_id` is block-local by design (section 0), so it cannot be the
cross-block join key, and equality of the resource's attribute set is worse: a
pod that adds one attribute mid-hour becomes two entities, and "show me
everything from this pod" silently returns a plausible subset.

Identity is therefore a 64-bit hash over the attributes semconv defines as
*identifying*, at the most specific level present:

```text
service.name + service.instance.id (+ service.namespace)
k8s.pod.uid (+ k8s.container.name)
container.id
host.id | host.name (+ process.pid)
service.name (+ service.namespace)
otherwise: NO_IDENTITY (0)
```

First match wins, and the list is **fixed, not configurable**: an identity rule
two operators can set differently is not an identity rule. `key = 0` is a
sentinel the entity expander refuses, naming the attribute that would fix it.

Resolving *"all signals from this entity"* is then a `resources.arrow` pass and a
65536-bit `resource_id` bitset, one test per root row, where a flattened
`ResourceAttributes Map(String,String)` pays a map probe per row.

## 7.3 The frame algebra — built, smaller than designed

`mira_core::frame` is `Frame`, `anchor`, `expand` and `map`, served at
`/api/v1/correlate`, `/api/v1/map` and `/api/v1/entities` and reached from all
three surfaces (sections 8.1 to 8.3).

```text
Frame {
    time:     [from, to)      // nanoseconds
    entities: {u64}           // resource keys;  empty = unconstrained
    traces:   {[u8;16]}       //                 empty = unconstrained
}
```

Every operation is `Frame → Frame` and the algebra holds nothing else: an
investigation is a walk over frames, and every intermediate state is a legal
query.

| expander | reads | writes | |
| --- | --- | --- | --- |
| `Traces` | `traces` from matched rows | widens `traces`, widens `time` to the traces' own extent | built |
| `Peers` | `traces → resource_id → key` | widens `entities` to everything that shared a trace | built |
| `Around(d)` | — | widens `time` by ±d, keeps `entities` | built |
| `by_span` | `spans`, parent/child | widens `spans` | cut |
| `by_link` | `span_links` | widens `traces` | cut |
| `by_exemplar` | metric exemplars | widens `traces` | cut |
| `by_entity` | `resource_id → resources.key` | widens `entities` | cut |

Three expanders where seven were designed; the cut four are what a `trace_id`
query already does, edges the row carries out to the caller, or `anchor`'s own
output. An expander with no caller is an expander with no test.

There is no `fetch` either: a frame's members are exactly the terms a query
document already takes, so it would be section 7.6's query in a second spelling.
`anchor` runs the caller's predicate through `query::search_open` for the same
reason — one piece of code decides both.

## 7.4 Indexes: what is needed and what is not

### Time

Directory names. Built.

### Entity

`resources.key` is written at seal and read by the frame algebra, but by no
*predicate*: `query::Search` has no entity member. A key has to be in the blocks
before a reader can use it, so one written today is answerable across retention
on the day a reader lands.

### Trace

The one that genuinely needs an index. A block's min/max trace id spans the whole
range and prunes nothing, so instead a Bloom filter over its distinct trace ids
in a `trace.idx` sidecar, read **on demand, not at boot**, so the zero-file-opens
boot property survives.

| 4.1 GB / 25M spans / 84 blocks | blocks scanned | rows scanned | cold | warm |
| --- | --- | --- | --- | --- |
| without | 84 | 25,000,000 | 14.3 s | 14.3 s |
| with | 1 | 212,992 | 250 ms | 20 ms |

Every damage path answers "scan the block", because a false negative loses spans.
**Logs blocks carry the same filter**, because a trace investigation is the spans
and then the logs written under them.

### Attribute value

An `attr.idx` sidecar, **built**. *"Any record with `k8s.pod.name = api-7f9`"*
has no early exit, because `limit` never fills, so proving a negative reads every
block in retention: over 6.4 GB / 25M logs / 69 blocks, **10.4 s → 71 ms cold,
5.7 ms warm**.

**The subtle part is what gets indexed.** A query scalar is compared against
whatever type the SDK happened to store (section 7.6), so a filter over the
*typed* bytes would prune the block holding the row. The indexed key is therefore
the value's **decimal text**. Doubles are the exception: `200`, `200.0` and `2e2`
are one number and three strings.

### Ordered comparison

A `zone.idx` sidecar, **built**. A Bloom filter answers *"is this value in this
block"* and an ordering has no value to hash, so *"anything that returned 5xx"*
reads every block in retention. The sidecar holds one `(min, max)` pair per
numeric thing the block contains — **776 bytes per traces block**.

| 1.02 GiB / 3.0M spans / 46 blocks | blocks scanned | rows scanned | warm |
| --- | --- | --- | --- |
| without | 46 | 3,014,656 | 81.6 ms |
| with | 0 | 0 | 2.3 ms |

**A key the map does not hold prunes the block**, so the *absence* of an entry is
load-bearing and the map has to be complete. **Text that parses as a number is a
number**: half the SDKs send `http.response.status_code` as text, and a range
over the `int` and `double` columns would prune away the block holding `"503"`.

### Why not sort blocks by `trace_id` instead?

There is one physical order, and every query has a time bound while only some
have a trace bound. Time wins.

## 7.5 Why the frame algebra is the agentic surface

The algebra *is* the MCP tool set: `correlate` is `anchor` plus a walk,
`service_map` is `map`, `list_services` is `entities`. An agent handed SQL over a
star schema with EAV attribute tables will write wrong joins, silently wrong: a
missing `parent_id` predicate returns a cross product that looks like data.
Closed operations cannot express one.

## 7.6 Query, outside correlation

A predicate on an attribute is a **relational semi-join**, not a column filter:
filter `log_attrs` on `(key, active-value-column)`, collect the `parent_id` set,
semi-join into `logs.id`. arrow-rs ships no join kernel, so this is hand-written.
`bytes` and `ser` are returned but not filterable in V0: a filter over a
serialised map is a path expression, which is a query language, which is
section 0.

### Every level means the span's children too

`Span.recordException` writes `exception.type` and `exception.stacktrace` onto an
*event*, so "which spans threw a `NullPointerException`" is a child-level filter
with no span-level equivalent. Leaving events and links out gave the worst shape
a search can have: the value plainly visible under `events[].attributes` and a
filter for that key returning nothing.

### Responses render 64-bit integers as JSON strings

OTLP/JSON writes `int64` and `uint64` as strings, and a response Mira cannot feed
back to itself as a request body is not a round trip. A JSON number is an IEEE754
double to most parsers, and 2^53 is where a double stops counting: a bare
`time_unix_nano` does not fail there, it silently rounds. Readers doing
arithmetic pay — `Yaml::as_i64` answers `None` to a string, so behind each
`unwrap_or(0)` the TUI drew a plausible zero rather than an error.

### Comparison dispatches on the stored type, not the query's

`eq: "200"` finds a stored integer and `eq: 200` finds a stored string. On a
string column equality is *defined* as equality with `canon()`, not "both parse
to the same number": the looser rule would match the stored `"200.0"` against
`eq: 200`, which the index, holding only that text, would have pruned away first.

### Paging is keyset, and there is no `offset`

The cursor is `ts.node.seq.row`, every component intrinsic to the record, so it
survives blocks being flushed and retired between two pages, and `after` prunes
whole blocks by `min_ts` before any is opened. An `offset` shifts under a reader
whenever a batch lands, and makes the last page the most expensive one.

### The scan fans out into whatever cores are idle

The fan-out budget is **shared by the process, and never waited for**: a search
takes what is free and otherwise runs serially, because inter-query parallelism
claims every core first under load. Reads and `publish` fsyncs share tokio's
blocking pool, so `api::scan` bounds query concurrency a level up: a slow query
was an ingest stall.

### "Vector matching" is settled

It means cross-signal correlation, as above, not the PromQL sense
(`on`/`ignoring`, `group_left`), which would need a full evaluator and a
series-major layout — a different sort order from section 3.

---
