---
# The key table and the key count are generated from `crates/mira/src/config.rs`
# by `make reference`; edit the field doc comments there, not the block below.
description: Mira runs with nothing configured. Fourteen keys if you want them — where it listens, where the data goes, how long it is kept, and the one durability choice you have to make yourself.
---

# Configuration

**For:** whoever runs the process.

Mira starts with nothing set. A flag beats the file, the file beats the
default, and there are
<!-- BEGIN GENERATED: count -->
14
<!-- END GENERATED: count -->
keys in all. The file exists for what a flag cannot write, chiefly values
pulled in from the environment. The flag spellings are in the
[CLI reference](reference/cli.md).

## Every key

Anything not on this list stops the process at startup, naming the outermost
key it does not know. An empty section counts, so `{ "cluster": {} }` is
`unknown key "cluster"`.

<!-- BEGIN GENERATED: keys -->
| Key | Flag | Type | Default | What it sets |
| --- | --- | --- | --- | --- |
| [`node`](https://miradb.dev/api/mira/config/struct.Config.html#structfield.node) | `--node` | string | `mira` | This replica's name. It is hashed into the name of every block directory this node writes, so two replicas sharing a volume cannot collide (see `mira_core::block`). |
| [`listen.grpc`](https://miradb.dev/api/mira/config/struct.Config.html#structfield.grpc) | `--grpc` | host:port | `0.0.0.0:4317` | The address OpenTelemetry exporters send to over gRPC. |
| [`listen.http`](https://miradb.dev/api/mira/config/struct.Config.html#structfield.http) | `--http` | host:port | `0.0.0.0:4318` | The address for everything else: OpenTelemetry over HTTP, the query API, the MCP endpoint and the web UI all answer on this one port. |
| [`storage.dir`](https://miradb.dev/api/mira/config/struct.Config.html#structfield.data_dir) | `--data-dir` | path | `./mira-data` | Where the data goes. This directory is all the state there is: no catalogue, no index file, nothing outside it to keep in sync. |
| [`storage.retention`](https://miradb.dev/api/mira/config/struct.Config.html#structfield.retention) | `--retention` | duration | `7d` | How long data is kept. Deleting happens a whole block at a time, so the oldest data goes in steps rather than row by row. |
| [`storage.offload`](https://miradb.dev/api/mira/config/struct.Config.html#structfield.offload) | `--offload` | uri | `unset` | Where to copy a block before retention deletes it, or `None` to just delete it. Off by default: deleting is the documented behaviour, and a flag that silently started keeping everything would be a disk bill nobody asked for. |
| [`ingest.max_request_bytes`](https://miradb.dev/api/mira/config/struct.Config.html#structfield.max_request_bytes) | `--max-request-bytes` | size | `16MiB` | The largest single export either listener will accept. |
| [`ingest.queue`](https://miradb.dev/api/mira/config/struct.Config.html#structfield.queue) | `--queue` | count | `128` | How many exports may wait for a writer before the next one is held back — and refused with a 503 only if no slot frees up within `pipeline::ADMIT_WAIT`. |
| [`ingest.shards`](https://miradb.dev/api/mira/config/struct.Config.html#structfield.shards) | `--shards` | count | `0` | How many writer tasks a signal runs, or 0 for one per two cores. |
| [`ingest.wal`](https://miradb.dev/api/mira/config/struct.Config.html#structfield.wal) | `--wal` | boolean | `true` | Acknowledge an export once it has been written to the log, rather than waiting for the block that holds it to be sealed. |
| [`telemetry.self`](https://miradb.dev/api/mira/config/struct.Config.html#structfield.self_telemetry) | `--self-telemetry` | boolean | `false` | Store this node's own telemetry in this node, as ordinary metrics. |
| [`telemetry.interval`](https://miradb.dev/api/mira/config/struct.Config.html#structfield.telemetry_interval) | `--telemetry-interval` | duration | `15s` | How often self-telemetry samples this node's counters. |
| [`alerts.rules`](https://miradb.dev/api/mira/config/struct.Config.html#structfield.alerts) | `--alerts` | path | `unset` | A file of alerting rules (`crate::alert`), or none — and none means Mira does not alert. |
| [`proxy.replicas`](https://miradb.dev/api/mira/config/struct.Config.html#structfield.replicas) | `--replica` | uri,uri,… | `unset` | The storage nodes `mira proxy` sits in front of, and nothing else reads. |
<!-- END GENERATED: keys -->

| | |
| --- | --- |
| **Durations** | `500ms`, `30s`, `5m`, `2h`, `7d`; a bare number is seconds |
| **Sizes** | `b`, `k`/`kb`/`kib`, `m`/`mb`/`mib`, `g`/`gb`/`gib`, case-insensitive; a bare number is bytes, and units are binary throughout, so `MB` means `MiB` |

There is no block size, flush interval, cache size or compaction threshold, and
there will not be: picking those is Mira's job, not yours
([principle 2](architecture/principles.md)).

## The file

The file is KYAML, a strict subset of YAML: braces and brackets are always
written out, every string is double-quoted, and indentation carries no meaning.

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

`${env:…}` fills a value in from the environment, `${dotted.path}` from another
key in the same file.

| syntax | meaning |
| --- | --- |
| `${env:NAME}` | environment variable; **missing is a startup error**, not an empty string |
| `${env:NAME,default}` | everything after the first comma is the default — untrimmed, later commas included, and itself expanded. Defaults nest: `${env:A,${env:B,fallback}}` finds the *matching* `}`, not the first one |
| `${dotted.path}` | another key in this file, resolved recursively |
| `$${` | a literal `${` |

An unbalanced `${` is a startup error, and so is a loop
(`reference cycle: node -> a -> node`). Resolution is lazy, so a broken
reference in a key nothing uses does not stop the boot.

## `storage.offload`

Unset by default, so retention means delete. Set it and each expiring block is
copied to that location before the local one goes — two copies, then one, never
zero.

**A copied-out block is not queryable.** Getting it back is explicit:

```sh
mira offload list    --offload file:///backup/mira
mira offload restore --offload file:///backup/mira --data-dir ./mira-data
mira offload push    --offload file:///backup/mira --data-dir ./mira-data
```

`restore` copies back everything not already local and is safe to re-run.
`push` sends the other way and **deletes nothing**; run it against a stopped
server.

**`file://` is the only scheme**: a mounted bucket (`s3fs`, `gcsfuse`,
`rclone mount`), an NFS export, a second disk. A native `s3://` is refused at
startup
([Architecture section 6.1](architecture/retention.md#61-offload-a-copy-before-the-unlink)).

## `ingest.wal`

The one durability choice Mira does not make for you. `"true"` or `"false"`,
and nothing else — no `yes`, no `on`, no `1`.

| | On (the default) | Off |
| --- | --- | --- |
| Acknowledged means | written to the log | stored in a sealed, `fsync`ed block |
| Survives | the process dying, `SIGKILL`, the OOM killer | all of that, and power loss — anything short of losing the disk |
| Power loss costs you | up to the last 250 ms (`WAL_SYNC_PERIOD`, how often the log reaches the disk) | nothing |
| What you wait for | p50 7 µs, p99 39 µs for a 4 KiB body | p50 657 ms, p99 2.6 s |

## `alerts.rules`

Unset by default, so alerting is off: `/api/v1/alerts` answering an empty list
means *nobody is watching here*, not *everything is healthy*. Nothing picks an
evaluator for you, so in a fleet exactly one replica is given the key and it
pages. A rules file that does not parse stops the process at startup.

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

`query` and `of` are `/api/v1/query` documents, verbatim, and neither may set
`from`, `to`, `limit` or `after`: `over` is the window.

### Webhooks

One JSON POST per target when a rule starts firing and one when it stops, with
a 10-second timeout and no retry. Slack, Discord, PagerDuty (Events v2) and
`json` each get their own body shape. An `https://` target needs a build with
`--features webhook-tls`, and is refused when the rules file is *loaded*, not
at the first page.

```sh
cargo install --locked --path crates/mira --features webhook-tls
```

## Ingest limits

| Key | What it costs you |
| --- | --- |
| `ingest.max_request_bytes` | One number for three things: the body limit on 4318, the decode limit on 4317, and the ceiling on what a gzip body may inflate to. Too low and you lose data rather than throughput — 4318 answers `413`, which OTLP classes as permanent, so the exporter drops the batch instead of retrying. |
| `ingest.queue` | Burst room, not throughput. Full does not mean refused: the next export waits for a slot (`pipeline::ADMIT_WAIT`), and only gets a `503` — which OTLP classes as retryable — if none frees up. Each slot holds one decoded export, so the worst case resident is `queue * max_request_bytes * 3`, 6 GiB at the defaults. Raise it when a wide collector fleet shows up as `shed` in `/health`; lower it when memory is the binding constraint. |
| `ingest.shards` | `0` means one writer task per two cores the process can see, capped at 16 (`pipeline::MAX_SHARDS`). That count honours a cgroup CPU quota, so a container with one set needs no help here. Set it by hand when the count is a lie: a CPU *share* or *weight* rather than a quota reads as the whole machine, a shared host often sets no quota at all, a non-Linux container runtime leaves nothing to read, and hyperthreads count as cores. `1` is the pre-sharding behaviour, exactly. Shards **split** `ingest.queue` — each gets `queue / shards` slots. |

## `telemetry.self`

Off by default. Turn it on and Mira stores its own counters in itself as
ordinary metrics, sampled every `telemetry.interval` (15s). They are real
ingest: they land in ordinary metric blocks and compete with your data for the
same writer.

```text
mira.uptime   mira.process.memory.peak   mira.storage.free   mira.storage.blocks
mira.query.count   mira.query.duration.max   mira.query.duration.mean
per signal: mira.ingest.rows .bytes .blocks .shed .failed .refused .open_block.age
```

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

**Budget 500k records/s per core.** The engine's own rate is 886k records/s per
core at one exporter and falls to 515k at 96.

### Memory

**Memory follows the exporter count, not the rate.** Peak goes 232 MiB at one
exporter, 314 at two, 689 at four, 1,243 at eight, and then flattens: 1,575 MiB
at 16, 1,495 at 32, 1,648 at 96. Setting a memory *limit* is safe here: Mira
reads files by mapping them, and those pages can be dropped and re-read under
pressure rather than getting the process killed.

### Disk

**Disk has two costs, and the cutover is a rate, not an age.** A fresh block
costs 164 bytes on disk per 137-byte wire record; an hour later compaction
rewrites it with ZSTD at 8.38x, taking the same record to about 20 bytes. But
the sweep compacts at most 8 blocks per signal per minute
(`block::MAX_COMPACT_PER_SWEEP`) — **about 27k records/s** per signal, about
55k in total. Below that, size the volume at 20 B/record; above it, compaction
is permanently behind and the cost stays at 164.

## Several replicas

`node` is hashed into the name of every block directory a node writes
(`{min_ts}-{max_ts}-{node}-{seq}-{wal_hi}`, where `{node}` is that hash in hex,
not the name you set), so several active replicas can write to one volume
without coordinating. The name and its hash are both logged at startup:

```text
INFO mira: mira listening grpc=… http=… node=mira-1 node_id="a2c1d111"
```

Two replicas sharing a volume with the same `node_id` would share one
write-ahead log, and a log has one writer: the second refuses to start. Give
each a distinct `--node`. On Kubernetes, one volume per replica, and `HOSTNAME`
names both:

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

Any replica accepts any export, so ingest can go through one Service. **A query
cannot**: a node only ever answers from its own blocks. Address a specific pod,
or put `mira proxy` in front:

```yaml
{
  "proxy": {
    "replicas": "http://mira-0.mira:4318,http://mira-1.mira:4318",
  },
}
```

That is a second Deployment of the same image, with no storage of its own,
merging `/api/v1/query` across every replica
([architecture section 12.2](architecture/replicas.md#122-query-mira-proxy)).

Several pods sharing one `/data` is not deployable on Kubernetes today: Mira
refuses to start on every filesystem an RWX volume is in practice
(`block::fs_type`), because mapping a file over a network filesystem can kill
the process with no way to recover.
