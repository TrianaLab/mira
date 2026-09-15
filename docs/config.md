# Configuration

**For:** whoever runs the process. <!-- BEGIN GENERATED: count -->
14
<!-- END GENERATED: count -->
keys. Precedence is **flag > file > default**; everything works with no config
file, and the file exists for what a flag cannot express, chiefly interpolation.
The flag spellings are in the [CLI reference](reference/cli.md).

## Every key

A closed set: anything else stops the process at boot, naming the outermost key
it does not recognise. An empty section counts, so `{ "cluster": {} }` is
`unknown key "cluster"`.

<!-- BEGIN GENERATED: keys -->
| Key | Flag | Type | Default | What it sets |
| --- | --- | --- | --- | --- |
| [`node`](https://miradb.dev/api/mira/config/struct.Config.html#structfield.node) | `--node` | string | `mira` | This replica's name. Hashed into the block directory name so that replicas sharing a volume cannot collide (see `mira_core::block`). |
| [`listen.grpc`](https://miradb.dev/api/mira/config/struct.Config.html#structfield.grpc) | `--grpc` | host:port | `0.0.0.0:4317` | Where OTLP/gRPC listens. |
| [`listen.http`](https://miradb.dev/api/mira/config/struct.Config.html#structfield.http) | `--http` | host:port | `0.0.0.0:4318` | Where OTLP/HTTP, the query API, the MCP endpoint and the web UI listen — one port, because they are one surface over one set of blocks. |
| [`storage.dir`](https://miradb.dev/api/mira/config/struct.Config.html#structfield.data_dir) | `--data-dir` | path | `./mira-data` | The block directory. It is the whole manifest: no catalogue, no index file, nothing outside it to keep in sync. |
| [`storage.retention`](https://miradb.dev/api/mira/config/struct.Config.html#structfield.retention) | `--retention` | duration | `7d` | How long a block is kept. Retention is a delete of whole blocks, so the oldest data disappears in block-sized steps rather than row by row. |
| [`storage.offload`](https://miradb.dev/api/mira/config/struct.Config.html#structfield.offload) | `--offload` | uri | `unset` | Where a block goes before retention unlinks it, or `None` to unlink it outright. Off by default: retention deleting data is the documented behaviour, and a flag that silently started keeping everything would be a disk bill nobody asked for. See `mira_core::offload`. |
| [`ingest.max_request_bytes`](https://miradb.dev/api/mira/config/struct.Config.html#structfield.max_request_bytes) | `--max-request-bytes` | size | `16MiB` | The largest export either listener will decode. See `receiver::Receivers::max_request_bytes` for why it is one number. |
| [`ingest.queue`](https://miradb.dev/api/mira/config/struct.Config.html#structfield.queue) | `--queue` | count | `128` | How many exports may be queued for one signal's flusher before the next one has to wait for a slot — and is shed with a 503 only if none frees up within `pipeline::ADMIT_WAIT`. |
| [`ingest.shards`](https://miradb.dev/api/mira/config/struct.Config.html#structfield.shards) | `--shards` | count | `0` | How many flushers a signal runs, or 0 for "one per two cores". |
| [`ingest.wal`](https://miradb.dev/api/mira/config/struct.Config.html#structfield.wal) | `--wal` | boolean | `true` | Acknowledge an export once it is a frame in the write-ahead log, rather than once the block holding it has been published. |
| [`telemetry.self`](https://miradb.dev/api/mira/config/struct.Config.html#structfield.self_telemetry) | `--self-telemetry` | boolean | `false` | Store this node's own telemetry in this node, as ordinary metrics. |
| [`telemetry.interval`](https://miradb.dev/api/mira/config/struct.Config.html#structfield.telemetry_interval) | `--telemetry-interval` | duration | `15s` | How often `Config::self_telemetry` samples this node's counters. |
| [`alerts.rules`](https://miradb.dev/api/mira/config/struct.Config.html#structfield.alerts) | `--alerts` | path | `unset` | A KYAML file of alerting rules (`crate::alert`), or none. |
| [`proxy.replicas`](https://miradb.dev/api/mira/config/struct.Config.html#structfield.replicas) | `--replica` | uri,uri,… | `unset` | The storage nodes `mira proxy` sits in front of, and nothing else reads. |
<!-- END GENERATED: keys -->

**Durations** are `500ms`, `30s`, `5m`, `2h`, `7d`; a bare number is seconds.
**Sizes** are `b`, `k`/`kb`/`kib`, `m`/`mb`/`mib`, `g`/`gb`/`gib`,
case-insensitive; a bare number is bytes, and units are binary throughout, so
`MB` means `MiB`. There is no block size, flush interval, cache size or
compaction threshold, and there will not be — the *self-driving* half of
[principle 2](architecture/principles.md).

## The file

KYAML: a strict subset of YAML 1.2 with explicit `{}` and `[]`, every string
double-quoted, indentation that carries no meaning.

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

Trailing commas are allowed. Every value Mira reads is a **string**; another
type is a startup error:

| written | what happens |
| --- | --- |
| `node: 0x1f` | refused — YAML resolved it to `31`, and coercing back would be a different name |
| `node: False` | refused — coerced back it would be `"false"`, a different case |
| `"storage": { "dir": {} }` | `storage.dir: expected a string, found a map` — quoting does not fix a shape |
| `"listen": "0.0.0.0:4317"` | `listen: expected a map of settings, found a value` |
| `node: null` | treated as absent; the default is used. So is `"listen": null`, at every level — which is how a templating layer writes "not set" |

## Interpolation

| syntax | meaning |
| --- | --- |
| `${env:NAME}` | environment variable; **missing is a startup error**, not an empty string |
| `${env:NAME,default}` | everything after the first comma is the default — untrimmed, later commas included, and itself expanded. Defaults nest: `${env:A,${env:B,fallback}}` finds the *matching* `}`, not the first one |
| `${dotted.path}` | another key in this file, resolved recursively |
| `$${` | a literal `${` |

An unbalanced `${` is a startup error, and so is a reference cycle
(`reference cycle: node -> a -> node`). Resolution is lazy, so an unused key
with a broken reference does not stop the boot.

## `storage.offload`

Nothing is the default, so retention still means delete. Set it and the sweep
copies each expiring block to that URI *before* it unlinks the local one — two
copies, then one, never zero.

**An offloaded block is not queryable.** Getting it back is explicit:

```sh
mira offload list    --offload file:///backup/mira
mira offload restore --offload file:///backup/mira --data-dir ./mira-data
mira offload push    --offload file:///backup/mira --data-dir ./mira-data
```

`restore` copies every block not already local and is safe to re-run. `push` is
the other direction and **unlinks nothing**; run it against a stopped server.

**`file://` is the only scheme**: a mounted bucket (`s3fs`, `gcsfuse`,
`rclone mount`), an NFS export, a second disk. A native `s3://` is refused at
startup
([Architecture section 6.1](architecture/retention.md#61-offload-a-copy-before-the-unlink)).

## `ingest.wal`

`"true"` or `"false"`, and nothing else — no `yes`, no `on`, no `1`.

**On (the default).** Acknowledged means logged: the export has been framed and
`write(2)`n to `.wal/`. p50 7 µs, p99 39 µs for a 4 KiB body. It survives the
process dying, `SIGKILL` and the OOM killer, but not power loss for up to
`WAL_SYNC_PERIOD` (250 ms), how often the background flusher `fsync`s.

**Off.** Acknowledged means published: the export is in a sealed, `fsync`ed,
renamed block directory, and nothing acknowledged is lost by anything short of
losing the disk. The cost is latency: p50 657 ms, p99 2.6 s.

## `alerts.rules`

Nothing is the default, so
`/api/v1/alerts` answering an empty list means *alerting is off here*, not
*everything is healthy*. Nothing elects an evaluator, so in a fleet exactly one
replica is given the key and it pages. A rules file that does not parse stops
the process at boot.

### The schema

[`e2e/alerts.kyaml`](e2e/alerts.kyaml) is the worked example. In outline:

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
`from`, `to`, `limit` or `after` inside one: `over` is the window.

### Webhooks

One JSON POST per target when a rule starts firing and one when it stops, with a
10-second timeout and no retry. Slack, Discord, PagerDuty (Events v2) and `json`
each get their own body shape. An `https://` target needs a build with
`--features webhook-tls`, refused when the rules file is *loaded*, not at the
first page.

```sh
cargo install --locked --path crates/mira --features webhook-tls
```

## `ingest.max_request_bytes`

The axum body limit on 4318, tonic's `max_decoding_message_size` on 4317, and
the ceiling on what a gzip body may inflate to are one number. Set it too low
and you lose data rather than
throughput: 4318 answers `413`, which OTLP classes as permanent, so the exporter
drops the batch instead of retrying.

## `ingest.queue`

Full does not mean refused: the next export waits up to five seconds
(`pipeline::ADMIT_WAIT`) for a slot, and only gets a `503` if none frees up,
which OTLP classes as retryable.

This knob buys queueing, not throughput. Each slot holds a decoded export, so
the worst case is `ingest.queue * ingest.max_request_bytes * 3` resident, 6 GiB
at the defaults. Raise it when a wide collector fleet shows up as `shed` in
`/health`; lower it when the RSS ceiling is the binding constraint.

## `ingest.shards`

`0`, the default, means one flusher task per two cores as the process sees them,
capped at 16 (`pipeline::MAX_SHARDS`).

The key is here for the case where that count is a lie. A cgroup CPU quota is
invisible to `available_parallelism`, so a 96-core host running Mira at 2 CPUs
would otherwise start sixteen flushers per signal, each publishing its own file
per seal window. Set it to the quota; `1` is the pre-shard behaviour, exactly.
Shards **split** `ingest.queue` — each gets `queue / shards` slots.

## `telemetry.self`

Off by default; `--self-telemetry` or `"telemetry": { "self": "true" }` turns it
on, and `telemetry.interval` (15s) is how often it samples. It is ingest: the
metrics land in ordinary metric blocks and compete with real ingest for the same
flusher.

What it emits, all prefixed `mira.`: `uptime`, `process.memory.peak`,
`query.count`, `query.duration.max`, `query.duration.mean`, `storage.free`,
`storage.blocks`, and per-signal `ingest.rows`, `.bytes`, `.blocks`, `.shed`,
`.failed`, `.refused` and `.open_block.age`.

## Sizing

Every row is anchored to a measured point in
[End-to-end testing section 3](internals/e2e.md#3-the-load-harness), the median
of three passes on a 12-core M3 Pro; between the anchors, linear interpolation.

| Workload | Ingest | Exporters | CPU | Memory | Disk/day |
| --- | --- | --- | --- | --- | --- |
| Laptop, CI, one service | ≤ 10k records/s | 1 | `250m` | `256Mi` | 16 GiB |
| A team's services | 100k records/s | 1–2 | `500m` | `512Mi` | 1.3 TiB |
| A cluster | 600k records/s | 1–2 | `1` | `512Mi` | 7.7 TiB |
| A busy cluster | 1.5M records/s | 4 | `2` | `1024Mi` | 19 TiB |
| Past the plateau | 1.5M records/s | 8+ | `2500m` | `2048Mi` | 19 TiB |

Multiply the last column by `storage.retention` for the volume. `records/s` is
logs plus spans plus data points, which is what `/api/v1/stats` reports.

### CPU

**CPU is the record rate over one number.** The engine's own rate is 886k
records/s per core at one exporter and falls to 515k at 96, so budget
**500k records/s per core**.

### Memory

**Memory is the exporter count, not the rate.** Peak RSS goes 232 MiB at one
exporter, 314 at two, 689 at four, 1,243 at eight, and then flattens: 1,575 MiB
at 16, 1,495 at 32, 1,648 at 96. A memory *limit* is safe here: Mira reads
blocks through `mmap`, mapped pages are clean, and a cgroup reclaims them under
pressure instead of OOM-killing.

### Disk

**Disk has two costs, and the cutover is a throughput, not an age.** A
hot block costs 164 bytes on disk per 137-byte wire record; compaction rewrites
it ZSTD at 8.38x an hour later, taking the same record to about 20 bytes. But
the sweep compacts at most 8 blocks per signal per minute
(`block::MAX_COMPACT_PER_SWEEP`) — **about 27k records/s** per signal, about
55k in total. Below that, size the volume at 20 B/record; above it
compaction is permanently behind and the cost stays at 164.

## Several replicas

`node` is hashed into every block directory name
(`{min_ts}-{max_ts}-{node}-{seq}-{wal_hi}`), so several active replicas can
write to one volume without coordinating. Both are logged at startup:

```text
INFO mira: mira listening grpc=… http=… node=mira-1 node_id="a2c1d111"
```

Two replicas sharing a volume with the same `node_id` also share a write-ahead
log, and a log has one writer: the second refuses to start. Give each a distinct
`--node`. On Kubernetes, one volume per replica, and `HOSTNAME` names both:

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

Ingest goes through one Service — any replica accepts any export. **A query
does not**: a storage node never fans out. Address a specific pod, or put
`mira proxy` in front:

```yaml
{
  "proxy": {
    "replicas": "http://mira-0.mira:4318,http://mira-1.mira:4318",
  },
}
```

That is a second Deployment of the same image, stateless and with no PVC,
merging `/api/v1/query` across every replica
([architecture section 12.2](architecture/replicas.md#122-query-mira-proxy)).

Several pods sharing one `/data` is not deployable on Kubernetes today:
`block::fs_type` refuses every backend an RWX PVC is in practice, because mmap
over a network filesystem raises `SIGBUS` with no recovery path.
