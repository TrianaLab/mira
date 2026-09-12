# Configuration

**For:** whoever runs the process. <!-- BEGIN GENERATED: count -->
12
<!-- END GENERATED: count -->
keys. One of them changes how the engine runs, and it is there to correct a
number the runtime can read wrong, not to tune anything.

Precedence is **flag > file > default**. Everything works with no config file at
all; the file exists for what a flag cannot express, chiefly interpolation. The
flag spellings are in the [CLI reference](reference/cli.md), which is
`mira --help` verbatim.

## Every key

A closed set. Anything else stops the process at boot, naming the outermost key
it does not recognise and listing every key it knows — an empty section counts,
so `{ "cluster": {} }` is `unknown key "cluster"`.

<!-- BEGIN GENERATED: keys -->
| Key | Flag | Type | Default | What it sets |
| --- | --- | --- | --- | --- |
| [`node`](https://miradb.dev/api/mira/config/struct.Config.html#structfield.node) | `--node` | string | `mira` | This replica's name. Hashed into the block directory name so that replicas sharing a volume cannot collide (see `mira_core::block`). |
| [`listen.grpc`](https://miradb.dev/api/mira/config/struct.Config.html#structfield.grpc) | `--grpc` | host:port | `0.0.0.0:4317` | Where OTLP/gRPC listens. |
| [`listen.http`](https://miradb.dev/api/mira/config/struct.Config.html#structfield.http) | `--http` | host:port | `0.0.0.0:4318` | Where OTLP/HTTP, the query API, the MCP endpoint and the web UI listen — one port, because they are one surface over one set of blocks. |
| [`storage.dir`](https://miradb.dev/api/mira/config/struct.Config.html#structfield.data_dir) | `--data-dir` | path | `./mira-data` | The block directory. It is the whole manifest: no catalogue, no index file, nothing outside it to keep in sync. |
| [`storage.retention`](https://miradb.dev/api/mira/config/struct.Config.html#structfield.retention) | `--retention` | duration | `7d` | How long a block is kept. Retention is a delete of whole blocks, so the oldest data disappears in block-sized steps rather than row by row. |
| [`ingest.max_request_bytes`](https://miradb.dev/api/mira/config/struct.Config.html#structfield.max_request_bytes) | `--max-request-bytes` | size | `16MiB` | The largest export either listener will decode. See `receiver::Receivers::max_request_bytes` for why it is one number. |
| [`ingest.queue`](https://miradb.dev/api/mira/config/struct.Config.html#structfield.queue) | `--queue` | count | `128` | How many exports may be queued for one signal's flusher before the next one has to wait for a slot — and is shed with a 503 only if none frees up within `pipeline::ADMIT_WAIT`. |
| [`ingest.shards`](https://miradb.dev/api/mira/config/struct.Config.html#structfield.shards) | `--shards` | count | `0` | How many flushers a signal runs, or 0 for "one per two cores". |
| [`ingest.wal`](https://miradb.dev/api/mira/config/struct.Config.html#structfield.wal) | `--wal` | boolean | `true` | Acknowledge an export once it is a frame in the write-ahead log, rather than once the block holding it has been published. |
| [`telemetry.self`](https://miradb.dev/api/mira/config/struct.Config.html#structfield.self_telemetry) | `--self-telemetry` | boolean | `false` | Store this node's own telemetry in this node, as ordinary metrics. |
| [`telemetry.interval`](https://miradb.dev/api/mira/config/struct.Config.html#structfield.telemetry_interval) | `--telemetry-interval` | duration | `15s` | How often `Config::self_telemetry` samples this node's counters. |
| [`alerts.rules`](https://miradb.dev/api/mira/config/struct.Config.html#structfield.alerts) | `--alerts` | path | `unset` | A KYAML file of alerting rules (`crate::alert`), or none. |
<!-- END GENERATED: keys -->

**Durations** are `500ms`, `30s`, `5m`, `2h`, `7d`; a bare number is seconds.
**Sizes** are `b`, `k`/`kb`/`kib`, `m`/`mb`/`mib`, `g`/`gb`/`gib`,
case-insensitive; a bare number is bytes. Units are binary throughout, so `MB`
means `MiB` — every other size in this system is binary and a `MB` that meant
10^6 beside a page size that meant 2^20 would be a trap.

`--wal` and `--self-telemetry` are the flags that take no value: a file has to
be able to say `false` to undo what an inherited config turned on, and a flag is
only ever typed to turn something on.

There is no block size, flush interval, buffer depth, cache size or compaction
threshold, and there will not be. That is the *self-driving* half of principle 2
in [Architecture section 1](architecture.md#1-principles-and-the-mechanism-each-one-buys):
the two numbers an operator would most want to tune are constants in the binary,
with no path from this file to either. `ingest.shards` is not the exception it
looks like: the engine sizes it from the core count on its own, and the key
exists to tell it something `available_parallelism` cannot see rather than to
search for a better value.

## The file

KYAML: a strict subset of YAML 1.2 with explicit `{}` and `[]`, every string
double-quoted, and indentation that carries no meaning. Any YAML parser reads
it; no YAML parser can guess about it.

```yaml
{
  "node": "${env:HOSTNAME,mira-0}",

  "listen": {
    "grpc": "0.0.0.0:4317",
    "http": "0.0.0.0:4318",
  },

  "storage": {
    "dir": "/var/lib/mira/${node}",
    "retention": "7d",
  },

  "ingest": {
    "max_request_bytes": "16MiB",
    "queue": "128",
    "shards": "0",
    "wal": "true",
  },

  "telemetry": {
    "self": "false",
    "interval": "15s",
  },

  "alerts": {
    "rules": "/etc/mira/alerts.kyaml",
  },
}
```

Trailing commas are allowed. Every value Mira reads is a **string**, and
anything that arrived as another type is a startup error:

| written | what happens |
|---|---|
| `node: 0x1f` | refused — YAML resolved it to `31`, and coercing back would be a different name |
| `node: False` | refused — coerced back it would be `"false"`, a different case |
| `"storage": { "dir": {} }` | `storage.dir: expected a string, found a map` — quoting does not fix a shape |
| `"listen": "0.0.0.0:4317"` | `listen: expected a map of settings, found a value` |
| `node: null` | treated as absent; the default is used. So is `"listen": null` |

`null` meaning "absent, use the default" at every level is deliberate: it is how
a templating layer writes "not set".

## Interpolation

| syntax | meaning |
|---|---|
| `${env:NAME}` | environment variable; **missing is a startup error**, not an empty string |
| `${env:NAME,default}` | everything after the first comma is the default — untrimmed, later commas included, and itself expanded |
| `${dotted.path}` | another key in this file, resolved recursively |
| `$${` | a literal `${` |

Defaults nest: `${env:A,${env:B,fallback}}` finds the *matching* `}`, not the
first one. An unbalanced `${` is a startup error quoting the string it could not
terminate, and a reference cycle is a startup error naming the cycle
(`reference cycle: node -> a -> node`). Resolution is lazy, so an unused key
with a broken reference does not stop the process booting.

There is no second `MIRA_*` override mechanism. `${env:...}` covers every case
in one visible file.

## `ingest.wal`

`"true"` or `"false"`, and nothing else — no `yes`, no `on`, no `1`. It chooses
when an export is acknowledged, which is the same thing as choosing what an
acknowledgement means. Read-your-writes holds either way: a query reads the open
block as well as the published ones.

**On (the default).** Acknowledged means logged: the export has been framed and
`write(2)`n to `wal/`, so it is in the page cache and the block that will hold
it is a background reorganisation of data already recoverable. p50 7 µs, p99
39 µs for a 4 KiB body. It survives the process dying, `panic = "abort"`, `SIGKILL` and the OOM
killer. It does not survive power loss or a kernel panic for up to
`WAL_SYNC_PERIOD` (250 ms), which is how often the background flusher `fsync`s.

**Off.** Acknowledged means published: the export is in a sealed, `fsync`ed,
renamed block directory. Nothing acknowledged is lost by anything short of
losing the disk. The cost is latency — an export waits for its block to fill or
age out, so p50 is 657 ms and p99 is 2.6 s. On Apple targets a durable
acknowledgement cannot beat 4.2 ms in any case, because `sync_all` is
`F_FULLFSYNC`.

At boot the log is replayed into the flushers before either port is served, so
frames the last process logged but never sealed are re-ingested in order. Each
block records the log position it covers in its directory name — the fifth
field — so replay skips what is already stored and the log is truncated behind
the lowest such position across the three signals. There is still no manifest,
and recovery is still three `readdir`s.

## `alerts.rules`

A path to a file of alert rules, or nothing. Nothing is the default, so
`/api/v1/alerts` answering an empty list means *alerting is off here*, not
*everything is healthy*; both UIs say so in those words.

That default is also the coordination mechanism. Nothing elects an evaluator, so
in a fleet exactly one replica is given the key and it is the one that pages.
Three replicas pointed at the same rules file send three copies of every alert.

A rules file that does not parse stops the process at boot, before anything is
created, bound or mapped.

### The schema

[`e2e/alerts.kyaml`](e2e/alerts.kyaml) is the worked example — loaded by
`make demo` and parsed by a unit test, so it cannot drift. In outline:

```yaml
{
  every: 15s,                             # evaluation interval; default 15s
  link_base: "https://mira.example.com",  # where an alert's link points
  notify: [
    { name: oncall, url: "http://...", format: slack },   # slack | discord
    { name: pager,  url: "http://...", format: pagerduty, # | pagerduty | json
      key: "${env:PD_ROUTING_KEY}" },
  ],
  rules: [
    {
      name: shop-error-rate,              # unique; the dedup key in every payload
      query: { signal: traces, where: [ { field: status_code, eq: 2 } ] },
      of:    { signal: traces },          # the denominator; omit for a count rule
      over:  1m,                          # the window each evaluation counts over
      when:  "ratio > 2%",                # count | ratio, then > >= < <=
      for:   30s,                         # how long it must hold before firing
      severity: critical,                 # free text; PagerDuty maps four of them
      notify: [ oncall ],                 # default: every target
    },
  ],
}
```

`query` and `of` are `/api/v1/query` documents, verbatim. A rule may not set
`from`, `to`, `limit` or `after` inside one — `over` is the window and the
evaluator sets the rest, so those are refused rather than silently overwritten.

Two metrics and no more. `count` is how many records matched; `ratio` is `query`
over `of`. Between them they express every threshold this engine answers
exactly, percentiles included: **`p95(duration) > 250ms` is exactly
`|{d > 250ms}| / |d| > 5%`**, the same inequality written two ways. A ratio over
an empty denominator reads as `0`, so zero traffic is not a 100% error rate.

### Webhooks

One JSON POST per target when a rule starts firing and one when it stops, with a
10-second timeout and no retry — the next evaluation is along in `every` seconds
anyway. Slack and Discord get their own body shapes, PagerDuty gets Events v2
with `dedup_key` set to the rule name so a resolve closes the incident it
opened, and `json` gets Mira's own document.

An `https://` target needs a build with `--features webhook-tls`. The default
build refuses one when the rules file is *loaded*, not at the first page; point
it at a local egress proxy and the default build is enough. The published
binaries and image are the default build, so the crate count and binary size in
the README describe what you downloaded.

```sh
cargo install --locked --path crates/mira --features webhook-tls
```

## `ingest.max_request_bytes`

The largest export Mira will decode, on either port: the axum body limit on
4318, tonic's `max_decoding_message_size` on 4317, and the ceiling on what a
gzip body may inflate to — one number, so a batch cannot succeed on one
transport and fail on the other.

The size of an export is a property of the *sender*, which the engine cannot
know. The default is eight times axum's and four times tonic's, and comfortably
above what a stock collector produces at its own default of 8192 records. Set it
too low and you lose data rather than throughput: 4318 answers `413`, which OTLP
classes as permanent, so the exporter drops the batch instead of retrying. Both
error messages name this key.

## `ingest.queue`

How many exports may be waiting for one signal's flushers, across all of them —
see `ingest.shards` below for how the depth is divided up. Full does not mean
refused: the next export waits up to five seconds (`pipeline::ADMIT_WAIT`) for a
slot, and only gets a `503` if none frees up. OTLP classes that as retryable, so
a collector backs off and sends it again — correct backpressure, and not a loss.

That wait is why the default of 128 is small and stays small. An earlier
revision shed the moment the queue was full, and a wide collector fleet then
spent its time being told to retry: 93% of exports refused at 96 connections,
and a third of the two-connection rate, because every shed export is decoded,
refused, retried and decoded again. Parking instead took the same row to nothing
shed at double the throughput, and it has refused nothing in twenty-one
consecutive runs of the sweep since — see [End-to-end testing section
3](internals/e2e.md#3-the-load-harness) for the numbers and the box they were measured
on.

So this knob buys queueing, not throughput. The flushers drain at the rate they
drain, and a queue deep enough to hide a permanently overloaded node has only
moved the shed into a latency tail. Size it to absorb a burst, not to avoid a
`503`: each slot can hold a decoded export, so the worst case is
`ingest.queue * ingest.max_request_bytes * 3` resident — 6 GiB at the defaults,
and eight times that at 1024. Raising it spends memory an operator has and the
engine cannot know whether they have it, which is the whole reason it is a key
rather than a constant.

## `ingest.shards`

How many flusher tasks each signal runs. `0`, the default, means one per two
cores as the process sees them, capped at 16 (`pipeline::MAX_SHARDS`) — six per
signal on the 12-core box the numbers here were taken on, sixteen on anything
from 32 cores up.

Per two cores, because a flusher is a consumer and the protobuf decode in front
of it is the producer: a single flusher per signal absorbed 1.35M records/s in
[the A/B in End-to-end testing section 3](internals/e2e.md#3-the-load-harness),
and the anchors below burn 1.75 cores to produce that — so a flusher per core
would leave most of them waiting on the decode.
[Architecture section 4](architecture.md#4-ingest-path) is the rest
of it, including why a shard may be keyed on the core count and on nothing about
the data.

So this is not a throughput knob: the engine already reads the number it wants,
and this key is here for the case where what it reads is a lie. A cgroup CPU
quota is invisible to `available_parallelism`, so a 96-core host running Mira at
2 CPUs would otherwise start the full sixteen flushers per signal and publish
sixteen files per seal window. Set it to the quota; `1` is the pre-shard
behaviour, exactly.

Shards **split** `ingest.queue` rather than multiplying it — each gets
`queue / shards` slots — so the worst-case resident bytes in the section above
are the same whatever this is. What setting it too high costs is blocks and
cores instead: a shard that takes work carries its own open block and publishes
its own file per seal window, and flushers past the core count compete with the
decode that feeds them. Setting it to `1` on a wide machine puts one flusher
back on the critical path of every export for that signal.

## `telemetry.self`

Mira storing Mira's own telemetry, in Mira. Off by default; `--self-telemetry`
or `"telemetry": { "self": "true" }` turns it on, and
`telemetry.interval` (15s) is how often it samples.

There is no exporter, no scrape endpoint and no second port. A timer builds an
OTLP `ExportMetricsServiceRequest` and hands it to the same `submit` an incoming
export goes through, so the metrics land in ordinary metric blocks and every
existing surface reads them: `/api/v1/metrics/names` lists them, the TUI charts
them, an alert rule can fire on them, and an agent can ask about them through
MCP. It is the fastest way to see the engine work — `mira --self-telemetry` and
the metrics tab has content within one interval, with no collector at all.

What it emits, all prefixed `mira.`: `uptime`, `process.memory.peak`,
`query.count`, `query.duration.max`, `query.duration.mean`, `storage.free`,
`storage.blocks`, and per-signal `ingest.rows`, `.bytes`, `.blocks`, `.shed`,
`.failed`, `.refused` and `.open_block.age`. The signal is an attribute rather
than three metric names, so `signal` is a group-by and not a naming convention.
A measurement that could not be taken is left out rather than reported as zero.

The cost is honest and small: it is ingest, so it competes with real ingest for
the same flusher, and its rows are stored on the disk being measured. At the
default interval that is a few dozen points a minute. It counts itself — the
`ingest.rows` it reports include the rows it just wrote.

## Sizing

Three axes driven by three different things: CPU by the record rate, memory by
the number of concurrent exporters, and disk by the record rate again — through
whichever of two costs applies. Every row is anchored to a measured point in
[End-to-end testing section 3](internals/e2e.md#3-the-load-harness), the median of
three full passes on a 12-core M3 Pro; between the anchors it is linear
interpolation and nothing more.

| Workload | Ingest | Exporters | CPU | Memory | Disk/day |
|---|---|---|---|---|---|
| Laptop, CI, one service | ≤ 10k records/s | 1 | `250m` | `256Mi` | 16 GiB |
| A team's services | 100k records/s | 1–2 | `500m` | `512Mi` | 1.3 TiB |
| A cluster | 600k records/s | 1–2 | `1` | `512Mi` | 7.7 TiB |
| A busy cluster | 1.5M records/s | 4 | `2` | `1024Mi` | 19 TiB |
| Past the plateau | 1.5M records/s | 8+ | `2500m` | `2048Mi` | 19 TiB |

Multiply the last column by `storage.retention` — 7 days by default — for the
volume. `records/s` is logs plus spans plus data points, which is what the
harness counts and what `/api/v1/stats` reports.

**CPU is the record rate over one number.** The engine's own rate is 886k
records/s per core at one exporter and falls to 515k at 96, so budget
**500k records/s per core** and you are covered anywhere on that curve; under
four exporters you will get closer to 850k. The measured anchors are 0.71 cores
at 629k records/s, 1.75 at 1.35M and 2.23 at 1.54M. There is deliberately
no CPU limit in the chart, for the reason its comment gives: throttling the
ingest path does not shed load, it queues it.

**Memory is the exporter count, not the rate.** Peak RSS goes 232 MiB at one
exporter, 314 at two, 689 at four, 1,243 at eight — and then flattens: 1,575 MiB
at 16, 1,495 at 32 and 1,648 at 96, while the throughput over that same range
rises 2% and then *falls* by a quarter. What the extra connections buy is
in-flight decodes, not work, which is why the memory column tracks the middle
column and not the one beside it. Sizing from RSS is
conservative on purpose: Mira reads blocks through `mmap`, mapped pages are
clean, and a cgroup reclaims them under pressure instead of OOM-killing — which
is why a memory *limit* is safe here when it usually is not.

**Disk has two costs, and the cutover is a throughput rather than an age.** A
hot block costs 164 bytes on disk per 137-byte wire record; compaction rewrites
it ZSTD at 8.38x an hour later, taking the same record to about 20 bytes. But
the sweep compacts at most 8 blocks per signal per minute
(`block::MAX_COMPACT_PER_SWEEP`), and a block is sealed at a 32 MiB target,
which is ~205k rows ([End-to-end testing section
3](internals/e2e.md#3-the-load-harness)) — so one signal can be compacted at
~1.6M rows/min, **about 27k records/s**, which for the usual half-logs
half-spans stream is about 55k records/s in total. Below that, size the volume
at 20 B/record; above it compaction is permanently behind and the cost stays at
164. The first row above is the compacted number, the last four the hot one,
and that step is the whole reason the column jumps 80x between rows that
differ 10x in rate.

A full volume takes the replica out of the Service via `/readyz` rather than
losing data, so the failure mode of under-sizing is a stopped intake and not a
corrupt store. `storage.retention` is the knob that prevents it.

## Several replicas

`node` is hashed into every block directory name
(`{min_ts}-{max_ts}-{node}-{seq}-{wal_hi}`), which is what lets several active
replicas write to one volume without coordinating. The resolved name and its
hash are logged at startup:

```
INFO mira: mira listening grpc=… http=… node=mira-1 node_id="a2c1d111"
```

Two replicas sharing a volume with the same `node_id` write duplicate data,
which OTLP's at-least-once contract already permits — the staging path carries
the block's timestamp range too, so they cannot mix each other's tables. Give
each replica a distinct `--node` anyway; that log line is where you check it.

On Kubernetes, one volume per replica. A `StatefulSet` gives each pod a stable
name and rebinds it to the same PVC across reschedules, so `HOSTNAME` names both
the replica and the disk its blocks are on:

```yaml
{
  "node": "${env:HOSTNAME}",
  "storage": {
    "dir": "/data",                             # where the PVC is mounted
    "retention": "${env:MIRA_RETENTION,7d}",
  },
}
```

```yaml
volumeClaimTemplates:                           # StatefulSet.spec
  - metadata: { name: data }
    spec:
      accessModes: ["ReadWriteOnce"]
      resources: { requests: { storage: 100Gi } }
```

Ingest goes through one Service — any replica accepts any export. **A query does
not**: there is no fan-out, so a replica answers only from its own blocks.
Address a specific pod (a headless Service gives you `mira-0.mira`), or run one
replica.

Several pods sharing one `/data` is the topology the `node_id` exists for, and
the only one where any replica answers for all of them — but it is not
deployable on Kubernetes today. Every backend an RWX PVC is in practice — NFS
(EFS, Filestore, most CSI drivers), CephFS, Azure Files, Lustre, 9P — is on the
list `block::fs_type` refuses, because mmap over a network filesystem raises
`SIGBUS` with no recovery path. `volumeMode: Block` is not a way round it: a
Block PVC is a raw device under `volumeDevices`, unformatted and with no mount
path, so it can never be `"dir": "/data"` — and the cluster filesystems you
would format one with are on the same refuse list.
