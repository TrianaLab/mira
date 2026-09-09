# Mira — Market Position and Feature Priority

**Status:** decision document. It resolves six market segment surveys into one
roadmap. Where two segments disagreed, the disagreement is named and settled
here, with the reason. Read alongside `ARCHITECTURE.md`; where this document and
that one conflict, this one is newer and §6 says exactly which architectural
claims have to change.

Numbers attributed to "the surveys" come from the six segment reports
(Grafana/LGTM, OTel-native self-hosted challengers, storage architectures,
commercial SaaS, practitioner demand signals, correlation mechanisms). Product
documentation is linked directly.

---

## 1. The position

> **Amendment, after the survey.** The survey recommended making Grafana the UI
> and reaching it by emulating Loki and Tempo. That is overruled: Mira owns its
> query API because the storage layout is new, and a query model this different
> is lossy through anyone else's panel system. Mira therefore ships **its own
> UI**, served by the same binary, over the same query API the MCP surface uses.
> Loki/Tempo emulation is not cancelled — it is demoted from the foundation to an
> optional on-ramp for the Grafana installed base, decided later on its own
> merits (§3 item 2b). Everything below is otherwise unchanged.

**Mira is a single binary that stores OpenTelemetry logs, traces and metrics in
the Resource–Scope–Signal layout itself — immutable Arrow IPC blocks on local
disk, no inverted index over attribute values, no coordination state of any kind
— and serves its own query API and its own UI out of that same binary, so the
whole observability stack is one process and one data directory.** It is for the
platform engineer who already operates an observability stack and has hit one of
two walls: the cardinality wall, where `k8s.pod.name`, `session_id`, `request_id`
or an LLM prompt either collapses the log ingester or gets banned by policy
before it is ever queryable; or the operational wall, where three signals cost
twenty pods, three query languages and a ClickHouse. Against Loki, Mira has no
stream index, so there is no stream count to explode and no 15-label cap — high
cardinality costs a bitset scan, not an OOM. Against SigNoz and the ClickHouse
tier, Mira is one process with one data directory and no second database.
Against both, the claim neither can make: **ask "show me everything this pod
emitted in the five seconds around this error" and get an answer when the log
line carries no `trace_id`** — because entity identity is a stored, stable
column (`resources.key`), not a hand-written YAML mapping fired at a second
system. That column is written at seal and read by nothing yet: the question is
answerable today only as an attribute predicate — `k8s.pod.name` equals this,
over that window — and the entity selector that would use the column is v1 item
7. What Mira does not claim: PromQL, object storage, or any replication of your
data.

The skeptical Grafana user's question is "why would I run a fourth thing?" The
answer has to be that Mira *removes* things: it is the Loki and the Tempo, in one
pod, with the cardinality limit deleted. The skeptical SigNoz user's question is
"why would I leave a mature product?" The answer is the ClickHouse, the
cardinality ceiling, and the correlation epic that has been open for two years
(SigNoz [#12433](https://github.com/SigNoz/signoz/issues/12433), per the
challenger survey — exemplars are accepted and discarded).

---

## 2. The market in one page

Five competitor classes. Each has a structural weakness — one that follows from
an architectural commitment they cannot reverse without rewriting, not one they
could patch next quarter.

| Class | Who | Structural weakness Mira attacks |
|---|---|---|
| **Composed OSS stack** | Grafana LGTM (Loki + Tempo + Mimir + Grafana), kube-prometheus-stack | Loki's index is a label index: [its own docs](https://grafana.com/docs/loki/latest/get-started/labels/) concede it "was not designed to support high cardinality label values", cap structured-metadata index labels, and have retreated from `k8s.pod.name` as a default. The failure mode is an ingester OOM *during an incident* — the surveys cite a field case going from 1,000 to 2,500,000 streams and dying mid-outage. Operationally it is 20+ pods and 8+ StatefulSets across three upgrade paths. Neither is fixable inside their design. |
| **OTel-native single binary** | SigNoz, OpenObserve, Uptrace, HyperDX/ClickStack, Coroot, Dash0 | "Single binary" with ClickHouse (or DataFusion + S3) inside. Real dependency, real tuning surface — OpenObserve ships ~450 `ZO_*` environment variables while marketing simplicity. Correlation is a client-side join they never finished: exemplars dropped, span links dropped. Most gate SSO/RBAC behind a commercial licence, which is where self-hosted evaluations die at procurement. |
| **Log-specialist challengers** | VictoriaLogs, Parseable, Quickwit | Genuinely fast and genuinely simple — VictoriaLogs markets "no need in tuning" verbatim and has the pulls to back it. But single-signal, non-OTLP-native data models, and their own DSLs. VictoriaLogs already capitulated on object storage, which tells you the cost axis is real. This class is the hardest to beat and the least worth attacking head-on; beat it on OTLP fidelity and on being three signals, not on tuning-free (that is parity). |
| **Commercial SaaS** | Datadog, Honeycomb, New Relic, Dynatrace, Chronosphere, Grafana Cloud | Per-GB and per-host billing makes the customer's own cost-control the product. Their AI layer is metered — the surveys report Datadog charging per Bits investigation and Dynatrace billing MCP tools per GB scanned. An agent that wants to fire 400 exploratory queries cannot afford to on any of them. That is the only durable gap against this class, and it is only real if Mira's query latency is real. |
| **Warehouse-backed** | ClickHouse direct, Databricks, Snowflake + OTel | Powerful and general; requires you to own the schema, the ingestion, the retention and the query language. The observability product is the part you build. They win the "we already have a data team" segment and Mira should not contest it. |

**Where Mira actually sits:** it competes in class 2 for mindshare, replaces
class 1 in deployment, and steals class 3's simplicity claim while being
three-signal. It does not compete with class 4 or 5 and should stop implying it
does.

---

## 3. Table stakes: the adoption floor

Ranked by *disqualification strength* — how fast a missing item ends an
evaluation. Everything in this section is a cost of entry. None of it wins a
deal; all of it loses one.

| # | Item | Evidence of demand | Cost | Lands |
|---|---|---|---|---|
| 1 | **Traces as a stored, queryable signal** | Logs-only competes with Loki, not with the category. Every challenger survey named it blocking. Principle 3 also decides it internally: a store whose identity is "the layout *is* Resource-Scope-Signal" that implements one of three signals is a log store with an OTLP parser. | ~600 LOC encoder (spans root + `span_events` + `span_links`, reusing the existing ATTRS schema and rebase pass) + query depth | **shipped** |
| 2 | **A UI in the binary** | A store with no screen is not evaluable. Since the query model is Mira's own (§4.5), every existing panel system is a lossy adapter over it, and the bundled UI is what makes the single-binary claim literal: run one binary, open a browser. VictoriaLogs ships VMUI; Honeycomb's UI *is* its differentiation. | Frontend work, plus ~150 LOC to embed the built assets in the binary | **shipped, and a second one in the terminal** |
| 2b | ~~**Grafana reachability — Loki + Tempo HTTP API emulation**~~ | **Demoted from the foundation** (§1 amendment). Still the cheapest route to the Grafana installed base, and still bends OTLP-first at read time. Decide on its own merits once the native UI and API exist and we know whether adoption is actually blocked on it. | ~4.5k LOC of axum handlers + DSL subsets | **later, gated** |
| 3 | **Compaction (and, riding it, compression)** | Not user-requested — user-fatal. Mira flushes on a 2 s age trigger and writes one directory per seal: an Arrow file per table that has rows, the Bloom sidecars, and a `cold` marker once compaction has been over it. `publish` skips empty tables, so the count is a range — measured on the smoke corpus, four to seven files for a logs block, seven for traces (five of nine tables written), nine for metrics (eight of thirteen). At a seal every two seconds per signal that is tens of thousands of files an hour per shard, Loki's documented 5.5M-file wall arrives in under a month, and boot is O(blocks). Compaction is also the pass every other index rides. | ~800 LOC, off the hot path; hourly pass is single-digit seconds of one core at 100 GB/day | **compression shipped; the merge that removes files is not** |
| 4 | **Helm chart** | Complexity/operational overhead is the #1 self-hosted concern (38% in the CNCF-derived numbers the practitioner survey cites). Nothing enters a cluster without a chart. Refusing this while claiming "no operational overhead" is the most self-defeating decision available. | ~300 lines of YAML. One day. | **v1** |
| 5 | **Self-observability: `/metrics` + one dashboard JSON** | Nobody puts an unproven binary in the ingest path they cannot see. A no-knobs system owes the operator visibility in exchange; "self-driving" without instrumentation is just opaque. | ~150 LOC, ~20 counters, one JSON file | **v1** |
| 6 | **A durability story that is proven, not described** | Top churn trigger in the practitioner corpus. "No WAL" reads as "loses data" until a crash test says otherwise. `ARCHITECTURE.md` §9 named two holes, and they were silent-corruption paths rather than documentation gaps. One is closed: `block::check_filesystem` runs a `statfs` before anything is mapped and refuses to start on NFS/CIFS/CephFS, where mmap turns a server hiccup into `SIGBUS`. The other is open — Docker-for-Mac volumes answer `EINVAL`/`ENOTSUP` to `F_FULLFSYNC`, std does not fall back, and the wrapper that would degrade to `fsync(2)` and count it is unwritten. | ~200 LOC + one CI job that `kill -9`s mid-ingest and asserts every acked export reads back | **`statfs` guard shipped; crash test v1** |
| 7 | **Size-based retention** | Kills the #1 operational churn trigger: the disk fills and the observability stack dies during the outage it existed to explain. | ~50 LOC. `statvfs` the data dir; over a fixed fraction, drop oldest until under. Derived, therefore not a knob. | **v1** |
| 8 | **Tenant in the block path** | Cheap now, expensive forever. Every protocol Mira will impersonate keys tenancy on `X-Scope-OrgID`. This changes a public identifier, so it must land **before the first tag**, not after. | ~100 LOC + a path-safety check at the trust boundary | **v1, first** |
| 9 | **SSO/OIDC + RBAC, in the free tier** | "The pilot works, the security review kills it" is a named churn reason. Every commercial competitor monetises exactly this; Mira is Apache-2.0, so shipping it in the box is simultaneously table stakes and the loudest available differentiator. | ~400 LOC: JWKS fetch + JWT verify as axum middleware, claim→role map in the config file. No user table. | **v1** |
| 10 | **Alerting** | A store you cannot alert on is not a monitoring system. It is the migration exit criterion. | **Zero code.** Once Mira answers as a Loki/Tempo datasource, Grafana-managed rules query it through the same handler. The obligation Mira takes on is honesty about its own latency — hence the query-latency histogram in item 5. | **v1** |
| 11 | **Trace-by-id, and needle-in-haystack text search** | Both are public, unprompted disqualification tests. valyala's "very slow on needle in the haystack" is Loki's stated fatal flaw; "I can't search all traces for `UploadDoc`" is the same complaint in the trace surface. | ~500 LOC, and the two were meant to share one mechanism. The trace half shipped as two sidecars per block — `trace.idx` over distinct trace ids, `attr.idx` over distinct `(key, value)` pairs — read before any Arrow file is opened, so zero-file-opens boot survives. Only equality prunes through them: a body substring is still a scan of every block the time bound leaves, which is the half that is not built. | **trace-by-id shipped; text search still scans** |
| 12 | **Live tail** | Weak as a switching reason; universal as a first-ten-minutes smoke test. Its absence is noticed within an hour of trial, and v1 is a trial-shaped release. | ~100 LOC: SSE/WebSocket at `/loki/api/v1/tail`, polling the newest sealed blocks. 2 s latency, documented as 2 s. | **v1** |
| 13 | **Native MCP server** | Now baseline, not a wedge — Tempo ships `/api/mcp`, Grafana ships Assistant, Honeycomb's is free-tier. Its *absence* is read as a gap. | ~450 LOC of hand-rolled JSON-RPC on the existing router — no `rmcp`, no crate spent; it came in at 251 | **shipped, unmarketed** |
| 14 | **Open-format readability** | The lock-in objection, answered for free — but only as far as the readers actually go. `pyarrow.ipc.open_file` and `polars.read_ipc` open a Mira block today with zero code; both were run against a live one. DuckDB does not: it has no Arrow IPC reader built in, and the community `nanoarrow` extension refuses the schema — *"Schema message field with DictionaryEncoding not supported"* — which is every table carrying an attribute key or a `severity_text`, so four of the five in a logs block. SQL over a block is pyarrow or polars handing DuckDB the table it already read, which is still zero Mira code and worth saying precisely rather than loosely. | One docs page + one CI step that opens a fresh block with a third-party reader. An untested interop claim rots in one release. | **v1** |
| 15 | **Metrics + PromQL subset** | 65% of orgs invest in Prometheus. Refusing metrics forever means refusing metric dashboards, recording rules and alerts. | Large: multi-datapoint-type encoder + evaluator. §7's v2 item 1 for why they belong in the *same release*. | **encoder shipped; PromQL v2** |
| 16 | **Object-storage tier** | Cost per GB is one of Mira's own four stated axes, and this is the axis the whole storage segment argues about. Also a procurement checkbox that loses evaluations before a benchmark runs. VictoriaLogs, the one vendor arguing Mira's position, capitulated. | Invasive but clean over immutable blocks. Hand-rolled SigV4 over the existing HTTP client, ~800 LOC, no `aws-sdk-s3`. | **v2** |
| 17 | **Non-OTLP ingest (Loki push, ES `_bulk`)** | Nobody re-instruments to trial a backend. OTLP-first read as OTLP-only forfeits every migration. | ~400 LOC each; the cost is not LOC, it is the semconv mapping table (see §6.6) | **v2** |

**Honest v1 total: 11–13k LOC** — the release, not the engine. `ARCHITECTURE.md`
§1's "~2,000 LOC of query logic" counts only the query engine, and that estimate
held: `query.rs`, `series.rs` and `api.rs` are ~2,350 non-test lines with the
metrics reader already in them. The two figures do not disagree; plan the
schedule against the larger one. It is still two orders of magnitude under
DataFusion's transitive tree, which is the only comparison that matters for that
decision.

### Three bugs that are table stakes because they are already shipped

These are not roadmap items; they are defects in code that exists. **All three
are fixed in the write path** (see the note after each); they are kept here
because the reasoning is what decides the equivalent question in the traces and
metrics encoders.

1. **`identity.rs` fallback is broken in exactly the way its own docstring
   forbids.** The whole-attribute-set sum at `crates/mira-core/src/identity.rs`
   is order-independent (correct) but still changes when an attribute is added
   or removed (fatal). Entity drift therefore splits the entity anyway, and the
   entity+time correlation rung — the product's headline claim — silently
   returns a plausible subset. **Fix: a `key = 0` sentinel meaning "no stable
   identity", and a query layer that refuses an entity selector on `key == 0`
   with a typed error naming the missing attributes.** Fail loud beats a
   plausible subset. **Fixed at the seal:** `identity::NO_IDENTITY` is written
   and asserted. The refusal is not written, and has nothing to refuse: no read
   surface takes an entity selector, so `resources.key` reaches disk and stops
   there (`ARCHITECTURE.md` §10).
2. **`LogsBuilder::approx_bytes` ignores the string heap** —
   `next_id * 64 + attrs * 48` counts fixed-width columns only. Multi-kilobyte
   GenAI prompts, or any large-body log workload, contribute nothing to the seal
   decision, so blocks balloon past `target_block_bytes` and blow the
   resident-footprint axis. **Fixed:** the four variable-width heaps are measured
   rather than estimated, with a test asserting that 100 records of 32 KB and 100
   short ones are three orders of magnitude apart.
3. **`DictionaryFull` propagates to the client.** Dictionary overflow must seal
   the block and retry into a fresh one. Overflow must never cost the caller
   their data. **Fixed, and it was worse than this:** the failing append had
   already written part of a row, so the block could never seal and the node
   rejected every subsequent export until restarted. The seal decision now runs
   `has_headroom_for` *before* the append, every fallible step in a row runs
   before any column is written, and a failed `finish()` replaces the builder.

---

## 4. The differentiators

Four claims. Two are acquisition claims (visible in the first ten minutes); two
are retention claims (they matter after the migration).

### 4.1 Unbounded attribute cardinality — the acquisition claim

**This is the headline, and it costs zero lines of code.** The EAV side tables
keyed by `parent_id`, with attribute *values* as plain `Utf8` rather than
dictionary keys, are structurally immune to the failure that makes teams abandon
Loki and to the mapping explosion that makes them abandon Elasticsearch. The only
structure over attribute values is a per-block Bloom sidecar that stops writing
itself past a million distinct pairs and leaves the block to be scanned, so there
is no inverted index to explode. High cardinality costs a semi-join scan at memory
bandwidth, which uncompressed mmap makes as fast as that operation can be.

The deliverable is not code, it is a benchmark that ships in CI and a demo:
ingest 1M distinct `session_id` values, assert flat RSS against the §11
"resident ≤ 2× open block" target, run the identical corpus through Loki
side-by-side, and publish `loki_ingester_memory_streams` and ingester RSS next to
Mira's. Then make the Grafana demo `{pod=~".*"}` over 2.5M pod values returning
in Explore.

Why this and not correlation as the headline: **data loss during an incident is
what actually moves buyers**, and this claim is visible inside the tool the user
already has open. Correlation is a bullet on everyone's matrix; a query that
returns instead of OOMing is not.

### 4.2 Correlation — what it has to mean

Every competitor has a correlation row on their feature matrix. Announcing
"correlation" in 2026 reads like announcing HTTPS. Four of the five rungs on
`ARCHITECTURE.md` §7.1's ladder are commodity: `trace_id` joins, `span_id`,
exemplars, span links. Shipping those is *not being embarrassed*, not winning.

**The differentiated claim is exactly one sentence: it works when the log line
has no `trace_id`.** Most logs in the wild carry none. Everyone else's
correlation is a client-side join across two databases configured by a
hand-written YAML mapping, and it fails *open* — the panel returns empty and the
user concludes there were no logs. Mira has the material for a fifth rung nobody
else has: `resources.key`, a stable entity identity stored as a column, which
turns "everything this pod emitted around this error" into an 8 KB `resource_id`
bitset over one small table per block. That is the one place the layout produces
an asymptotic advantage rather than a constant factor — and it is unclaimable
until a read surface selects on that column, which none does. Today the pod
question is an attribute predicate over the EAV tables, which is a scan.

For "powerful correlation" to be a differentiator rather than a checkbox, all
five of these must be true. Any one missing and it is a checkbox:

1. **The fifth rung exists and is correct.** The hash is stable and the column is
   written (§3); what is missing is the selector that reads it. Non-negotiable.
2. **Empty is never bare.** `around(d)` that finds nothing returns the window it
   searched and the widening that *would* have hit. ~20 LOC, and it kills the
   single largest churn reason in the correlation segment: users conclude the
   product is broken when the real answer is a 2-second clock skew. Default
   `d = 2s` each side, widened to 5 s for CONSUMER/PRODUCER/INTERNAL spans or
   spans over 1 s — the same shift Grafana's own trace-to-logs config exposes,
   except Mira derives it instead of asking.
3. **Traversal through span links.** ~80 LOC once `span_links` and the trace
   bloom exist. Tempo has publicly refused this because it would need a full
   database search for the linked trace id; Mira has the bloom, so it does not.
   Weak demand — this is a demo asset and a credible "we go further" claim, not
   a purchase driver.
4. **Service map with no metrics-generator.** The `peers` two-hop join computes
   who-calls-whom on demand, with no in-memory span pairing store, no
   requirement that a trace's spans land on one instance, no TTL, no cardinality
   explosion. ~200 LOC against Tempo's separate generator writing to a separate
   Prometheus. **Be honest about the gap: v1 ships a service map an agent can
   describe and Grafana cannot draw**, because drawing it in Grafana requires a
   Prometheus datasource serving `traces_service_graph_*`, and that drags PromQL
   in behind it. Say that out loud rather than implying a panel exists.
5. **RED metrics computed on read.** ~300 LOC over the traces table, dimensioned
   on demand. There are no pre-aggregated series, so the spanmetrics time-series
   explosion does not exist here by construction. This is the strongest available
   counter to "you have no metrics" in v1, and it is why the Tempo shim is worth
   keeping on the roadmap for its TraceQL-metrics half.

**Resolution of the segment disagreement:** the LGTM segment wanted correlation
demoted to a v2 MCP feature; the correlation and challenger segments called it
the product. Both are right about different halves. Ship all of §7 in v1 —
it is nearly free once traces exist, and its absence is disqualifying. Market
*the fifth rung*, not the noun. "Correlation" goes on the feature matrix in small
type; "works when the log line has no `trace_id`" goes on the landing page.

### 4.3 One binary, and the number that proves it

"Single binary" is occupied — OpenObserve markets it at 21.7k stars with S3 and
DataFusion inside. The defensible version is not the phrase, it is **4.73 MiB
stripped and 117 crates**, three signals and two UIs included, published as a
figure next to competitors' pod counts. Every dependency added for object
storage, JWT or Parquet spends it.

An asset not enforced in CI erodes one convenient dependency at a time. **Make it
a build gate: fail CI above 20 MB stripped or above a pinned crate ceiling, and
require an explicit bump commit to raise either.** Ten lines of CI, and it is the
only part of this claim that will still be true in a year.

### 4.4 Agent economics, not "we have MCP"

MCP is parity — six competitors shipped it in 2025. Two things about Mira's are
not parity, and both are claims a competitor cannot copy without changing their
architecture:

- **Tool shape is a correctness property.** An agent handed SQL over a five-table
  EAV star schema writes joins that are *silently* wrong: a missing `parent_id`
  predicate returns a cross product that looks like data. An agent handed a closed
  set of frame operations — `ARCHITECTURE.md` §7.3, designed and not yet built —
  cannot express a wrong join at all, and every call returns a frame whose
  cardinality it can bound before materialising. The four tools that ship today
  are the narrow version of the same property: there is no join to get wrong, and
  every answer carries the blocks and rows it scanned. Under ten tools either way,
  which is the ceiling before context-window degradation kills the loop.
- **Unmetered exploration.** Datadog and Dynatrace meter their agent surfaces per
  investigation and per GB scanned. An agent firing 400 exploratory queries at a
  local engine is doing something no SaaS agent can afford. That claim is only
  true if principle 1 is real — which is how ingest performance finally becomes
  something a buyer recognises.

Measure it and publish it: **tokens per resolved investigation**, and Pass^3 on
Grafana's o11y-bench task set. A benchmark of agent accuracy and
queries-per-investigation is a far sharper claim than records/s/core, and no
metered competitor can publish a competitive number without pricing themselves
out.

### 4.5 The UI, and the state it refuses to keep

Mira serves its own UI from the same binary, over the same query API the MCP
surface uses. One engine, three consumers: the UI, an agent, and any external
frontend. That is not a convenience — it is the only way the agentic claim in
§4.4 stays honest, because a tool surface nobody drives by hand rots quietly.

The survey argued against this on the grounds that UI ergonomics is a fight a
storage engine loses. That reasoning assumed Grafana was the target. It does not
survive the premise that the query model is new: a frame is not a time series and
not a log stream, and expressing one through a panel system built for those is
lossy in both directions. It also gives up the strongest version of the operational
claim — *one binary, open a browser* against *one binary, then deploy Grafana and
configure a datasource*.

**The one hard rule: the UI adds no mutable state.** This is where a bundled UI
normally collides with principle 4, because dashboards and saved views are
per-user objects that must survive a restart and agree across replicas — the
first coordination state in the system, and the kind nobody backs up.

- **A query fits in a URL.** The link *is* the saved view: shareable,
  bookmarkable, diffable, and the browser owns the history. An agent hands a
  human a link; a human hands an agent a link back.
- **Curated dashboards are read-only files** on disk, in KYAML, GitOps'd. Same
  model Perses chose, and operationally better than a database with no backup story.
- **No user table, no preferences, no annotations.** Preferences live in
  `localStorage`. Annotations are OTLP logs, which Mira already stores.

If someone needs a mutable dashboard store, that is a product in front of Mira,
not a table inside it.

GenAI semconv (principle 2b) rides this for near-zero marginal cost: `gen_ai.*`
attributes already land in the EAV tables, and plain `Utf8` values already handle
multi-KB prompts — *once `approx_bytes` is fixed*. v1 deliverable is "GenAI
telemetry does not degrade Mira", proven by one CI case asserting block size and
RSS stay in budget. The token/cost rollup surface is v2, gated on the
[semconv](https://opentelemetry.io/docs/specs/semconv/gen-ai/) stabilising.
Report tokens and latency, never dollars — a built-in price table is a knob that
goes stale.

---

## 5. Explicit non-goals

Each with the sentence a user gets.

| Refused | What the user is told |
|---|---|
| **A Grafana datasource plugin** | "Install nothing. Point Grafana's built-in Loki and Tempo datasources at Mira." A signed plugin is a second artifact in a second language, reviewed by a third party, with a signing subscription for out-of-catalogue distribution. There is no version of that where "one binary" is literally true. The demand it carries is fully discharged by emulating the built-in APIs. |
| **Stored dashboards, saved views, user preferences** | "The link *is* the saved view. Curated dashboards are files you commit." Mira ships its own UI (§4.5) and stores nothing mutable behind it. A saved dashboard is a per-user object that has to survive a restart and agree across replicas — that is precisely the coordination state principle 4 exists to refuse, and it would be the first mutable row in the entire system. A query fits in a URL, so sharing is a link and the browser owns the history. Curated dashboards are read-only files on disk, GitOps'd, which is Perses' own model and is strictly better operationally than a database nobody backs up. |
| **SQL** | "Mira has no SQL. Read the blocks with pyarrow or polars, and hand the table to DuckDB if you want SQL over them." This is load-bearing, not stylistic: the query surface is a closed set of operations with no parser, planner or optimiser — a query document with fixed keys today, the frame algebra of `ARCHITECTURE.md` §7.3 if that is ever built — and that is the only thing bounding the schedule against DataFusion. **The day SQL is promised, DataFusion becomes the correct choice and the in-house decision reverses.** SQL over a block costs Mira zero binary bytes because it happens in someone else's process, and it answers the lock-in objection with the same sentence — see §3 item 14 for what the sentence may and may not say about DuckDB. |
| **DataFusion** | 47 direct dependencies, ~1.5M SLoC transitive, 68–92 MB binary, against a 4.73 MiB baseline. |
| **Iceberg / a catalog** | Catalog, manifests and snapshots are coordination state and a second product. Parquet *export* is revisited only when someone names Athena or Trino with a workload attached — it costs ~20 crates against the one number nobody else can match. |
| **Separation of storage and compute** | Needs a scheduler, a metadata service and membership — three things principle 4 exists to refuse. Scale by adding independent replicas behind an L4 balancer; retention is the rebalancer. There is no query fan-out, so a replica answers only from the blocks it can see. Sharing a data directory lifts that, but only between processes on one host — across hosts a shared directory is a network filesystem and the `statfs` guard refuses to start on one (`ARCHITECTURE.md` §12.5). |
| **Prometheus `remote_read`** | "It would forfeit every pushdown the engine exists to do." Remote read makes Mira a dumb sample pipe streaming raw points to a Prometheus that then evaluates locally — the worst possible shape for a columnar store. Prometheus 3's own migration notes flag the storage contract as undefined for third-party implementers. No endpoint, and the docs say why rather than leaving a 404. |
| **Prometheus `remote_write` receiver** | "Run the Collector's `prometheusreceiver` and export OTLP." Flat label sets carry no Resource, no Scope and no semconv; synthesising them upward puts a lossy import inside a product whose pitch is fidelity, and every synthesised series lands in the no-identity bucket, which is where the correlation story stops being true. Keep the lossy hop outside Mira, where it is visible and maintained by someone else. |
| **Ingest-side shaping: drop rules, sampling, transforms** | "Shaping belongs in the Collector, and here is a reference `otelcol` config." This is the market's #1 pain and every competitor sells knobs for it, so the refusal has to be argued: shaping rules are a filter graph, and the Collector *already is* a filter graph running upstream. Duplicating it imports exactly the config surface principle 2c forbids, and Chronosphere's own docs admitting pool allocation "does not provide protections for persisted cardinality" is the tell that these knobs do not solve the problem they exist for. What Mira ships instead (v2) is cost *attribution* — bytes-on-disk and attribute cardinality ranked by tenant/service/attribute key, from block footer sketches. The buyer's real question is "who is costing me money", not "give me a quota". |
| **Loki's cardinality guards** (`max_streams_per_user`, `max_label_names_per_series: 15`) | "Those exist to protect Loki's index. Mira has no such index." Refusing them is a feature claim, not a gap. |
| **Per-record deletion (GDPR erasure)** | This one hurts. It breaks block immutability, which is what the reader-safety and no-WAL arguments rest on. The answer offered is tenant-as-path-prefix so deletion stays an unlink of whole directories, plus short retention. Regulated buyers who need per-subject erasure should be told no explicitly rather than discovering it at audit. |
| **SAML** | "Put an OIDC bridge in front of Mira." Keycloak and Dex each do it in one deployment. SAML needs a certificate and metadata store; OIDC needs a JWKS fetch. |
| **Alertmanager-style silences, and an internal ruler with durable state** | "Grafana owns alert state." Silences are the one irreducibly mutable object in alerting and Mira will not hold one. See §6.5 for the standalone case. |
| **A Kubernetes operator / CRDs** | A controller reconciling a stateless single binary is a second process managing a thing with no state to reconcile. It would contradict the pitch it was shipped to support. |
| **Tiering knobs** (`offloadPeriod`, cache path, cache size, eviction policy) | Every competitor's tiering config is a documented foot-gun — VictoriaLogs' offload flags are mutually incompatible with its own retention flags, and the classic S3-tiering failure is a full local disk during merges. Exactly one new flag will ever exist: `--offload <uri>`, because a URI is an address. Publish the comparison against competitors' flag lists; it is a sales asset. |
| **Profiles as a fourth signal** | Deferred, not refused, and the gate is external: the signal is Alpha, upstream says production backends have not emerged, the proto is still removing fields, and OTAP lists profiles as future work. Revisit at Beta or when OTAP lands profiles, whichever is later. **Do not put a placeholder table in the schema.** |
| **W3C Trace Context Level 2 random-`trace_id` flag** | Speculative. The bloom hashes all 16 bytes and gains nothing from knowing 7 are random, and Mira shards by listener, not by trace id. Twenty free lines is still twenty lines someone reads at 3am wondering what they are for. |
| **Replication of your data** | "A lost disk is lost data for that node's share. Export to two Mira replicas from your Collector, or wait for v2 and let the object store be the replica." A replication factor above one requires a placement decision, and placement *is* coordination state. This is the sharpest edge of principle 4 and belongs in the README, not buried in §12. |

---

## 6. Where the principles bend

Ten conflicts. None of them are papered over, and three of them change public
claims that are currently made.

### 6.1 P3 (OTLP-first) vs. Loki/Tempo API emulation — **bends, materially**

**Conflict.** LogQL has a mandatory stream selector, renames `service.name` to
`service_name`, and splits a record into line + structured metadata. That is
Loki's model, not OTLP's. Same class of bend for TraceQL, though smaller —
TraceQL's scoped model (`span.`, `resource.`, `event.`, `link.`) is the closest of
the three DSLs to Resource-Scope-Signal and maps onto the star schema almost
directly.

**Options.** (a) Refuse, and have no door into Grafana. (b) Transform at ingest —
destroys the fidelity claim permanently. (c) Read-time projection.

**Recommendation: (c), with the bend bounded three ways** — the projection rule
is fixed and non-configurable (same reasoning as the identity rule: a mapping two
operators can set differently is not a mapping), it has zero write-path impact,
and the stored bytes stay pure OTAP.

**The public claim that must change:** the README says "no transformation". It
must say **"no transformation at rest."** Ship the LogQL/TraceQL subsets Grafana's
own UIs actually emit, publish the supported-syntax table, and **parse-error
loudly on anything unsupported — never silently narrow a result.** Silent wrong
answers are the churn reason this whole document is organised against.

### 6.2 P1 (zero-copy) vs. compression — **bends, at a tier boundary**

**Conflict.** Compression and mmap zero-copy are mutually exclusive — arrow-rs
hands back a slice of the mapping when the codec is `None` and allocates when it
is not, with no in-place path. Uncompressed forfeits cost-per-GB, one of
principle 1's own four axes, against competitors quoting 10–50×.

**Resolved at the tier boundary, and built.** The collision only exists if you
compress the *hot* block, so nothing compresses at flush: an hour after a block's
newest row the retention sweep rewrites its tables ZSTD-compressed and drops a
`cold` marker. `ARCHITECTURE.md` §3.5 draws the same boundary — it proves the
exclusion and then splits the store in two rather than treating compression as a
whole-store property. Hot blocks stay raw, 64-byte aligned and mmapped, and the
compaction test asserts both halves of the trade: every buffer inside the mapping
for the hot block, fewer than every buffer for the cold one. One codec and no
level flag — the writer names ZSTD and the codec takes its own default, so
"ZSTD-3" names a level nothing in the tree chooses.

What is not built is the narrow read: a cold block decompresses whole tables, not
just the projected columns. It costs no standing memory, because there is no
block cache and the allocation dies with the query, but the per-query arena is
table-shaped rather than column-shaped.

**State the guarantee as "zero-copy queries on the hot tier."** And publish the
ratio with its denominator every time: bytes-on-disk ÷ bytes-of-*OTLP-wire*,
against a 0.35 B/B target — measured at **1.31 hot** and **~0.17 compacted**
(`ARCHITECTURE.md` §11). Competitors' 10–50× is measured against raw JSON, which
is already 2–3× larger than OTLP protobuf.

### 6.3 P1 (all four axes) vs. reality today — **short by two, and not the two this section expected**

Cost per GB is the axis this section was written about, and the cold tier settled
it: 1.31 B/B hot, ~0.17 B/B once the sweep has been over a block, against a 0.35
target (§6.2). What is missing there is not a ratio, it is a destination — every
byte lives on the local volume, so retention is bounded by the PVC and there is
no HA story. The two axes still open in `ARCHITECTURE.md` §11 are ingest
throughput, 544k records/s aggregate against a per-core target, and resident
footprint, which has no number at all and is scored as an intention.

The action on the cost axis is not code, it is one sentence of honesty wherever
the axes are claimed: **local disk, hot and cold; object storage in v2.**
Claiming four axes simultaneously while one of them is unmeasured is the kind of
claim that gets found out in a benchmark thread.

### 6.4 P2c (no tuning knobs) vs. policy — **restate the principle**

**Conflict.** Per-tenant retention, a per-tenant daily byte budget, alert
thresholds, SLO targets and `--offload <uri>` are all things the market requires
and the engine cannot possibly derive.

**Recommendation.** `crates/mira/src/config.rs` already draws the boundary
structurally: deployment description reaches the YAML, tuning does not, and there
is no path from the file to `target_block_bytes` or `max_block_age`. The
principle should be stated in public the way the code already behaves:
**"no performance tuning knobs."** A knob is a number the engine could have
derived and instead asks a human to guess. A retention duration, a byte budget
and an alert threshold are none of those — they are policy. Refusing them does
not make Mira self-driving, it makes it unusable on a platform team.

The line holds absolutely on the query side: **no `max_duration`, no
`max_result_limit`.** Query safety is a wall-clock deadline plus a bytes-scanned
ceiling derived from block sizes, and exceeding it returns an **error naming what
was not scanned**, never a silent partial. Tempo flipped `fail_on_high_lag` to
true for exactly this reason.

### 6.5 P4 (stateless) vs. alerting state — **does not bend in v1**

**Conflict.** Firing/pending/resolved, dedup and silences are coordination state
by definition.

**Recommendation.** Discharge it entirely: Grafana Unified Alerting evaluates
against any backend datasource and owns all of that state. The Loki shim makes
Mira a backend datasource, so alerting-the-capability costs zero code and zero
stored state. The checklist will still say "no built-in alerting" and that is the
correct price. Point users at Grafana's own deprecation of datasource-managed
rules in Cloud when they ask for a Ruler — this is a feature the vendor is
walking away from.

**The gate for revisiting:** users running Mira *without* Grafana, with a
workload attached. If that arrives, the design is Prometheus's own — rules as
config-as-code, and on boot re-evaluate each rule over its own `for:` window
against the block directory to reconstruct pending/firing. The data is the state;
worst case after a restart is one duplicate notification, never a missed one and
never a re-notify storm.

### 6.6 P3 (fidelity) vs. non-OTLP ingest — **bends at the receiver only**

**Conflict.** Nobody re-instruments to trial a backend, but foreign wire formats
carry no Resource and no Scope. A lazy mapping dumps everything into flat
attributes and quietly destroys the entity identity the correlation wedge depends
on.

**Recommendation.** Accept Loki push and Elasticsearch `_bulk` in v2, each with a
**fixed, non-configurable** mapping table onto semconv resource attributes
(`service.name`, `k8s.pod.uid`, `host.name`) applied *before* the identity hash.
Refuse `remote_write` (§5). Everything else reaches Mira through Vector's and
Fluent Bit's OTLP outputs. The OTAP layout is untouched; the bend is entirely at
the edge.

### 6.7 P4 (stateless) vs. identity and RBAC — **does not bend**

Require an external OIDC provider, verify JWTs against cached JWKS, map claims to
roles in the config file. Mira holds zero identity state: no user table, no token
table, no local password. Express a role natively as **a frame constraint** — an
entity-key or tenant predicate ANDed into every query before materialisation —
which makes it structurally impossible to leak rows through a forgotten filter.
Emit the audit trail as OTLP logs into Mira's own tenant, so the audit log is
telemetry rather than new state.

### 6.8 P4 (the block directory is the only truth) vs. object storage — **holds, if disciplined**

**Recommendation for v2.** Offload replaces a block's *contents*, never its
*identity*: the local directory stays as a zero-byte marker with the same name,
so boot is still one `readdir` with zero file opens and the name still carries
the whole pruning key. **The bucket is never LISTed on the hot path** — a LIST is
an explicit offline `mira recover s3://...` after losing the local disk. Offload
the whole block as **one** object with an index footer, not seven: seven files
per block is a 7× PUT-amplification bomb at $0.005/1000 regardless of object
size, and the read path is range-GET anyway.

There is **no cache tier.** A cold block that gets read is re-materialised into
the data directory as an ordinary local block and its marker flips back; the same
disk-pressure controller that pushed it out pushes it out again. Eviction *is*
re-offload, so one controller serves both directions, there is nothing to size,
and a rehydrated block is readable by the existing mmap path. Add the
footer-prefetch trick — pull the IPC footer and the bloom sidecar in one range
GET so a block can be pruned without fetching its body.

**Design the block-name-to-object-key mapping in v1** so v2 is an addition rather
than a rewrite.

### 6.9 P4 (single binary) vs. every feature above — **enforce it or lose it**

`zstd` is already inside the 117-crate tree. JWT/JWKS, SigV4 and later
`promql-parser` will take it to roughly 150–170 and 4.73 MiB to an estimated
8–12 MB. That is inside the 20 MB budget, but only if the budget is a CI gate
rather than a paragraph (§4.3).

### 6.10 P4 vs. live tail's in-memory buffer — **avoided, not bent**

The SaaS segment proposed a ring buffer tapping the ingest mpsc; the practitioner
segment proposed polling the newest sealed blocks. **Take the polling version.**
It is ~100 LOC instead of ~1k, changes nothing about the flusher's single-owner
builder, adds no shared-memory read path, and keeps "the block directory is the
only truth" literally true. The cost is ~2 s latency, which is stated in the
docs. The in-memory tail is added only if someone names a workload where 2 s
fails.

---

## 7. Roadmap

Ordered within each release by demand ÷ cost. Items are not parallel tracks; the
order is the build order.

### v1 — "the Loki and the Tempo you can put cardinality into"

**Decide-now items (before the first tag, because they change public
identifiers):**

| Order | Item | Days |
|---|---|---|
| 0 | Tenant in the block path: `<data>/<tenant>/<signal>/p=<hour>/<min>-<max>-<node>-<seq>/`. Default single-tenant gets `_`. Tenant validated as a path-safe token at the trust boundary — reject `/`, `..`, empty — before it ever reaches a `PathBuf`. | 2 |
| 0 | ~~`identity.rs` `key = 0` sentinel~~ **done** + typed refusal in the query layer (rides item 2). | 1 |
| 0 | ~~`approx_bytes` counts the string heap. `DictionaryFull` seals and retries.~~ **done** | 2 |
| 0 | Block-name → object-key mapping fixed on paper (no code). | 0.5 |

**Then:**

| Order | Item | Est. LOC | Notes |
|---|---|---|---|
| 1 | ~~Traces encoder: spans root + `span_events` + `span_links`, exemplar-ready columns~~ **shipped** | 600 | Was the largest single item; everything downstream depended on it, and does. |
| 2 | ~~EAV semi-join + time predicates~~ + frame algebra + `fetch` + entity predicates | 3–5k | **Half shipped, and the half that shipped is the smaller shape:** a query document with a closed set of keys over `where` terms, ~2,350 non-test lines across `query.rs`, `series.rs` and `api.rs`. The frame algebra of `ARCHITECTURE.md` §7.3 is not built — no `Frame`, no `anchor`, no expander — and no predicate reaches `resources.key`. |
| 3 | ~~ZSTD rewrite at the tier boundary~~ + the merge that removes files + block-shadowing by name arithmetic | 800 | **The rewrite ships, the merge does not.** An aged block has each of its tables rewritten compressed in place and gains a `cold` marker; nothing is merged, so the file count is what it was at seal plus the marker. The writer names ZSTD and takes the codec's own default, so "ZSTD-3" named a level nothing chooses. Design when the merge lands: extend directory names to `<min>-<max>-<node>-<seq_lo>-<seq_hi>`; `scan()` discards any block whose seq range is strictly contained in another's. Merged block shadows its sources the instant it is renamed; a crash before unlink leaves garbage the next pass collects. No manifest, no new atomic primitive. Needs one round-trip merge correctness test. |
| 4 | ~~Bloom sidecars: trace ids + attribute pairs~~ + body tokens + cached in-block `trace_id` sort permutation | 500 | Explicitly not an inverted index: no positions, no ranking, no phrase search. Written at seal by `publish`, not in the compaction pass — a block is prunable from the moment it lands, and the cost is an fsync per sidecar of a file that sizes by row count: 24 B to 1 KB on the blocks a quiet node writes, 65 KB at the top end. The body-token half is why substring search still scans (§3 item 11). |
| — | ~~Loki shim~~ **moved out of v1** (§1 amendment). Kept on the shelf with its design intact: stream identity = the existing `resources.key`, matchers resolve into a 65536-bit `resource_id` bitset, volume comes from a per-block byte counter in `Schema.custom_metadata` at seal. Revisit only if adoption is measurably blocked on reaching Grafana. | 2.5k | later |
| — | ~~Tempo shim~~ **moved out of v1**. Same gate. `/api/echo` and `/api/status/buildinfo` are five lines each and gate the datasource health check, so this is a cheap thing to bolt on later, not a rewrite. | 2k | later |
| 5 | ~~Mira UI: record explorer, trace waterfall, metric charts; query state in the URL; built assets embedded in the binary~~ **shipped, twice** | — | §4.5. No mutable state, no user table: the filter and the window live in the hash, and the second UI is a terminal one over the same query API. No entity view, because nothing reads `resources.key` yet. The UI is the first consumer of the query API, which is what keeps the MCP surface honest. |
| 6 | Read-only KYAML dashboard files, loaded from a directory at boot | 200 | §4.5. GitOps, no database. |
| 7 | Correlation surface: six expanders, `peers`, `around(d)` with the never-bare-empty rule, RED-on-read | 700 | §4.2. |
| 8 | Self-observability: `/metrics` text endpoint, ~20 counters, one dashboard JSON | 150 | **Mira never writes its own telemetry into its own data directory by default.** It is a pull target. |
| 9 | Helm chart: one Deployment (replicas: 1), one PVC, one Service (4317/4318), one ConfigMap | 300 YAML | A Deployment at `replicas: 1` because that is all one PVC supports. Scaling out needs a StatefulSet, and that is not the concession it reads as: `ARCHITECTURE.md` §12.4 is right that a replica's identity *is* its disk, and `volumeClaimTemplates` is the only Kubernetes primitive that gives each replica its own durable one. Stable identity is not consensus — principle 4 refuses coordination state, not ordinals. Publish the resource-count diff against kube-prometheus-stack + Loki + Tempo in the README; that number *is* the pitch. |
| 10 | Durability: CI `kill -9` crash test, ~~`statfs` guard refusing mmap on network filesystems~~, `F_FULLFSYNC` fallback with a visible counter, CRC scrub folded into compaction at zero extra IO | 200 | The guard ships as `block::check_filesystem`, called once before anything is mapped. The crash test is the published artifact, not the prose, and it is the part still owed. |
| 11 | OIDC middleware + role-as-frame-constraint + bearer-token hashes in config | 400 | |
| 12 | Size-based retention from `statvfs`; per-tenant retention and bytes/day | 150 | Quota resets on the epoch-hour partition roll; over-quota returns 429 with the standard OTLP throttling response. |
| 13 | ~~MCP: hand-rolled JSON-RPC on the existing router, read-only, range and result caps~~ **shipped** | 450 | Four tools, 251 lines, unmarketed (§3 item 13). Not the frame algebra as tools — the same four questions the UI asks. |
| 14 | Live tail: poll newest sealed blocks, serve at `/loki/api/v1/tail` | 100 | Drops under load are counted and reported in the stream, not hidden. |
| 15 | Docs: "read your own blocks" (pyarrow / DuckDB / polars, including the star-schema join) + CI third-party-reader step | — | |
| 16 | Benchmarks: 1M-`session_id` cardinality bench vs Loki; GenAI block-size/RSS case; binary-size and crate-count CI gates | — | §4.1 and §4.3. |

**Explicitly not in v1:** PromQL, object storage, non-OTLP ingest, `/patterns`,
TraceQL structural operators, TraceQL metrics.

**If the schedule slips, the cut line is item 14.** Live tail is the only one
whose absence is survivable for one release; MCP was the other and it is already
in. Items 1–13 are the floor.

### v2 — "and the metrics, and the cold tier"

| Order | Item | Why here |
|---|---|---|
| 1 | **PromQL subset over the stored metrics** | The encoder landed ahead of this plan and landed with its reader — `/api/v1/metrics/names` and `/api/v1/metrics/query` — because the rule is *never ship a signal you cannot read*: metrics storage no query engine has exercised freezes the hardest block format forever. It stores strictly as received, with **no delta-to-cumulative conversion at ingest**, because a per-series running total that must survive a restart is precisely the coordination state principle 4 forbids; a point comes back with its OTLP temporality and the caller subtracts. Exemplar `trace_id`/`span_id` are non-optional columns. What is left is the language. Parser: `promql-parser` (buy the boring half); evaluator hand-rolled. Subset: instant/range selectors, matchers, `rate`/`increase`/`delta`, the `_over_time` family, `sum`/`avg`/`min`/`max`/`count`/`quantile` with `by`/`without`, `offset`. Per-block series-hash index built at seal — **not** a second sort order, because vector matching operates on already-materialised series, not on disk order. Publish the supported-function table so nobody discovers a gap during an incident. |
| 2 | **Object-storage offload** (§6.8) + retention beyond the local ceiling | Cost axis, procurement checkbox, and the only honest HA story Mira can tell. |
| 3 | **TraceQL metrics** (`/api/metrics/query_range`) | A real render path in Grafana for RED and the service map with no PromQL behind it. This is the reason the Tempo shim earns its keep. |
| 4 | **Cost attribution** | Bytes-on-disk and attribute cardinality ranked by tenant/service/attribute key, from block-footer sketches (HLL, t-digest, top-K). The knob-free answer to the market's #1 pain. |
| 5 | **SLOs with multi-window burn-rate alerts** | Once alerting exists via Grafana, an SLO is a stored good/total query pair plus a windowed ratio. Definitions in config-as-code; the [SRE Workbook](https://sre.google/workbook/alerting-on-slos/) 14.4×/1h and 6×/6h windows hardcoded — those are the standard, not a knob. Take the scan first; add a seal-time counter **only if p99 misses**, because a rollup is a format commitment and a slow query is not. |
| 6 | **Loki push + Elasticsearch `_bulk` receivers** | §6.6. |
| 7 | **Cross-signal MCP tool** — trace + its logs + its metric window in one call | This is what makes MCP a differentiator rather than parity, and it is only possible once all three signals exist. |
| 8 | **`/patterns`** (drain clustering, 1–2k LOC) + GenAI token/cost rollups | Both were 200-with-empty or absent in v1 and degrade gracefully. |
| 9 | **Span-summary at seal** (`span_summary.arrow`: count/error/latency-digest per service+operation+status, edge counts per client+server pair) | **Only if** the 30-day service map is measured too slow on read. Derived data stored alongside, not a transformation. |

### Later, each with its gate

| Item | Gate |
|---|---|
| Full PromQL (subqueries, `@`, `on`/`ignoring`/`group_left`, full function set) | Conformance complaints from users running real dashboards. |
| TraceQL structural operators (`>>`, `<<`, `>`, `<`, `~`) | Someone writes one. No Grafana UI generates them unprompted, Grafana's own tuning docs call them slow, and the star schema does not materialise parent/child adjacency. When they land: a post-filter over spans already grouped by trace, never a global index. |
| Profiles | OTel profiles at Beta *or* OTAP profiles support, whichever is later. ~12–18 months. |
| Parquet export subcommand | Someone names Athena or Trino with a workload. Costs ~20 crates. |
| OTAP wire receiver | An `otelarrowreceiver` deployment asks for it. Worth ~2× bandwidth over OTLP+zstd, and no language SDK emits it. |
| Standalone rule evaluator | Users running Mira without Grafana (§6.5). |
| A signed Grafana plugin | Never, unless the shims are *measurably* insufficient and someone is funding the second build pipeline. |

---

## 8. How we will know it is working

Leading indicators, in the order they should appear. Each has a threshold,
because a signal without one is a vibe.

**Months 0–3 — does anyone run it?**

- **Someone runs Mira in production without asking permission first.** One
  unsolicited "we've had this in prod for a month" is worth more than 1,000
  stars. Target: one, by month 3.
- **Docker pulls ÷ GitHub stars > 2, by month 3.** Stars measure interest, pulls
  measure trial. Under it, the pitch works and the product does not.
- **Issue mix shifts from "how do I install" to "can it do X".** Installation
  questions after the Helm chart ships mean the chart is wrong. Depth requests
  mean people got in.
- **The cardinality benchmark gets reproduced by a third party.** The claim in
  §4.1 is only worth what someone else's run of it is worth.

**Months 3–9 — does anyone keep it?**

- **One public migration write-up** ("we replaced Loki+Tempo with Mira"), with
  before/after pod counts and RSS. This is the single highest-value artifact in
  the whole list, and it is why item 9 in the v1 roadmap publishes the
  resource-count diff.
- **Retention: instances still ingesting at day 90.** Trial-to-90-day survival is
  the number that separates a starred project from a run one.
- **Someone hits a LogQL/TraceQL parse error and files an issue asking for the
  operator** — rather than silently getting a wrong answer and leaving. Loud
  refusal working as designed looks like this.
- **Tokens per resolved investigation, published and cited.** If nobody cites it,
  the agent-economics claim in §4.4 is not landing and should be demoted.

**Negative signals — what would prove this document wrong.** Watch these as hard
as the positive ones:

- **The top-voted issue at month 3 is "add PromQL."** That falsifies the
  logs-and-traces-first bet, and metrics should be pulled forward from v2.
- **Evaluations die at "does it replicate?" more often than at any other
  question.** That makes the object-storage tier a v1 item and §5's replication
  refusal a liability rather than a principle.
- **Nobody connects an MCP client in six months.** Then MCP is a checkbox and
  should be maintained, not marketed — and §4.4's second bullet is wrong.
- **The Loki shim's supported-syntax table grows past ~2 pages.** That means Mira
  is reimplementing Loki rather than impersonating it, and the bend in §6.1 has
  stopped being bounded.
- **Crate count crosses the CI ceiling twice in one quarter.** The single-binary
  differentiator is eroding, and §4.3's gate is being treated as a formality.

**What is deliberately not a success metric:** GitHub stars, records/s/core in
isolation, and feature-matrix coverage against Datadog. The first is not
adoption, the second is not a number buyers recognise until it is translated into
agent economics or query p99, and the third is a race Mira cannot win and should
not enter.
