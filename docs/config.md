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
keys in all. The flag spellings are in the
[CLI reference](reference/cli.md).

## Every key

Anything not on this list stops the process at startup, naming the outermost
key it does not know.

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
there will not be ([principle 2](architecture/principles.md)).

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
| `node: null` | treated as absent; the default is used. So is `"listen": null`, at every level — which is how a templating layer writes "not set" |

## Interpolation

| syntax | meaning |
| --- | --- |
| `${env:NAME}` | environment variable; **missing is a startup error**, not an empty string |
| `${env:NAME,default}` | everything after the first comma is the default — untrimmed, later commas included, and itself expanded |
| `${dotted.path}` | another key in this file, resolved recursively |
| `$${` | a literal `${` |

An unbalanced `${` is a startup error, and so is a loop
(`reference cycle: node -> a -> node`).

## `storage.offload`

Unset by default, so retention means delete. Set it and each expiring block is
copied to that location before the local one goes.

**A copied-out block is not queryable.** Getting it back is explicit:

```sh
mira offload list    --offload file:///backup/mira
mira offload restore --offload file:///backup/mira --data-dir ./mira-data
mira offload push    --offload file:///backup/mira --data-dir ./mira-data
```

`restore` copies back everything not already local and is safe to re-run.
`push` sends the other way and **deletes nothing**; run it against a stopped
server.

**`file://` is the only scheme**: a mounted bucket, an NFS export, a second
disk. A native `s3://` is refused at startup
([Architecture section 6.1](architecture/retention.md#61-offload-a-copy-before-the-unlink)).

## `ingest.wal`

`"true"` or `"false"`, and nothing else — no `yes`, no `on`, no `1`.

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
pages.

### The schema

[`e2e/alerts.kyaml`](e2e/alerts.kyaml) is the worked example. The file has four
keys:

| Key | What it is |
| --- | --- |
| `every` | How often every rule is evaluated. Default `15s` |
| `link_base` | Prefix for the link in a notification, e.g. `https://mira.example.com`. Unset means the payloads carry no link |
| `notify` | The webhook targets: `name`, `url`, `format` (`slack`, `discord`, `pagerduty` or `json`, default `json`), and `key` for PagerDuty's Events v2 routing key |
| `rules` | The rules |

And each rule:

| Key | What it is |
| --- | --- |
| `name` | Unique in the file; the dedup key in every payload |
| `query` | What the rule counts — and the numerator, if it is a `ratio` |
| `of` | The denominator, for a `ratio`. A `count` rule must leave it out |
| `over` | The window each evaluation counts over. Default `1m` |
| `when` | `count` or `ratio`, then `>`, `>=`, `<` or `<=`, then the number — `ratio > 2%` |
| `for` | How long it must hold before firing. Default `0s`, which fires on the first breach |
| `severity` | Free text, default `warning`. PagerDuty takes only `critical`, `error`, `warning`, `info`; anything else goes as `warning` |
| `notify` | Target names. Default none — the rule evaluates and shows in `/api/v1/alerts`, but pages nobody |

`query` and `of` are `/api/v1/query` documents, verbatim, and neither may set
`from`, `to`, `limit` or `after`: `over` is the window.

### Webhooks

One JSON POST per target when a rule starts firing and one when it stops, with
a 10-second timeout and no retry. An `https://` target needs a build with
`--features webhook-tls`.

```sh
cargo install --locked --path crates/mira --features webhook-tls
```

## Ingest limits

| Key | What it costs you |
| --- | --- |
| `ingest.max_request_bytes` | One number for three things: the body limit on 4318, the decode limit on 4317, and the ceiling on what a gzip body may inflate to. Too low and you lose data rather than throughput — 4318 answers `413`, which OTLP classes as permanent. |
| `ingest.queue` | Burst room, not throughput. Each slot holds one decoded export, so the worst case resident is `queue * max_request_bytes * 3`. |
| `ingest.shards` | `0` means one writer task per two cores the process can see, capped at 16 (`pipeline::MAX_SHARDS`). That count honours a cgroup CPU quota, so a container with one needs no help. Set it by hand when the count is a lie: a CPU *share* or *weight* reads as the whole machine, a shared host often sets no quota, a non-Linux runtime leaves nothing to read, and hyperthreads count as cores. Shards **split** `ingest.queue` — each gets `queue / shards` slots. |

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
[End-to-end testing section 4](internals/e2e.md#4-what-the-harness-measured) —
the log on, the median of three passes on a 12-core M3 Pro. Between the anchors
it is linear interpolation, and the CPU cells are M3 Pro cores.

| Workload | Ingest | Exporters | CPU | Memory | Disk/day |
| --- | --- | --- | --- | --- | --- |
| Laptop, CI, one service | ≤ 10k records/s | 1 | `250m` | `256Mi` | 16 GiB |
| A team's services | 100k records/s | 1–2 | `500m` | `512Mi` | 1.3 TiB |
| A cluster | 600k records/s | 1–2 | `1` | `512Mi` | 7.7 TiB |
| A busy cluster | 1.5M records/s | 4 | `2` | `1024Mi` | 19 TiB |
| Past the plateau | 1.5M records/s | 8+ | `2500m` | `2048Mi` | 19 TiB |

Multiply the last column by `storage.retention` for the volume. `records/s` is
logs plus spans plus data points, which is what `/api/v1/stats` reports.

**Budget 500k records/s per core** — the per-core rate falls from 886k at one
exporter to 515k at ninety-six, so size for the bottom of that range.
**Memory follows the exporter count, not the rate.**

**Disk has two costs, and the cutover is a rate, not an age.** A fresh block
costs 164 bytes on disk per 137-byte wire record; an hour later compaction
rewrites it with ZSTD at 8.36x, taking the same record to about 20 bytes. But
the sweep compacts at most 8 blocks per signal per minute
(`block::MAX_COMPACT_PER_SWEEP`) — **about 27k records/s** per signal, about
55k in total. Below that, size the volume at 20 B/record; above it, compaction
is permanently behind and the cost stays at 164.

## Several replicas

`node` is hashed into the name of every block directory a node writes, so
several active replicas can write to one volume without coordinating. The name
and its hash are both logged at startup:

```text
INFO mira: mira listening grpc=… http=… node=mira-1 node_id="a2c1d111"
```

Two replicas sharing a volume with the same `node_id` would share one
write-ahead log, and a log has one writer: the second refuses to start. Give
each a distinct `--node`. On Kubernetes, one volume per replica, and
`"node": "${env:HOSTNAME}"` names both.

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
(`block::fs_type`).
