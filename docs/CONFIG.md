# Configuration

```
mira [--config FILE] [--node NAME] [--grpc ADDR] [--http ADDR]
     [--data-dir PATH] [--retention DURATION]
     [--max-request-bytes SIZE] [--version]
```

Precedence is **flag > file > default**. Everything works with no config file at
all; the file exists for the cases a flag cannot express, chiefly interpolation.

## The format is KYAML

Principle 5. KYAML is a strict subset of YAML 1.2 — collections written
explicitly with `{}` and `[]`, every string double-quoted, indentation carrying
no meaning and present only for the reader. It is still YAML, so every YAML
parser and editor reads it; the point is that no YAML parser can *guess* about
it.

Unquoted scalars are resolved by pattern-match against a table, and which table
depends on the YAML version and the implementation. Against Mira's parser
(yaml-rust2, YAML 1.2):

| written | parsed as | what Mira does |
|---|---|---|
| `node: 0x1f` | integer `31` | refused at boot — coerced back it would be `"31"`, a different name |
| `node: False` | boolean | refused at boot — coerced back it would be `"false"`, a different case |
| `node: null` | null | key treated as absent; the default is used |
| `node: no` | string | accepted as `"no"` — but only because this is YAML 1.2; in 1.1 it is `false` |

The last row is the argument. The correct reading of an unquoted scalar is not a
property of the document, it is a property of whoever is reading it. Quoting
makes it a property of the document, for two characters.

Mira does not merely tolerate this, it enforces it: **every value it reads is a
string, and anything that arrived as another type is a startup error.** Which
error depends on what arrived. A scalar the parser resolved to a number or a
boolean is told to quote it, because that is the fix — coercing back with
`to_string()` is exactly what turns `0x1f` into `31` without anyone noticing. A
list or a map where a value belongs, or a value where a section belongs, is a
*shape* error naming the path (`storage.dir: expected a string, found a map`);
quoting does not fix a shape. The one exception is `null`, which means "absent,
use the default" at every level — `{ "listen": null }` boots on the default
ports — because that is how a templating layer writes "not set".

Keys are quoted too, and trailing commas are allowed (YAML 1.2 permits them in
flow collections; verified against our parser in `config.rs`'s tests). Both exist
for the same reason: a generated or machine-edited config should never have to
choose between two spellings, and appending a line should never mean editing the
line above it.

## The whole surface

```yaml
{
  "node": "${env:HOSTNAME,mira-0}",     # this replica's name

  "listen": {
    "grpc": "0.0.0.0:4317",             # OTLP/gRPC
    "http": "0.0.0.0:4318",             # OTLP/HTTP
  },

  "storage": {
    "dir": "/var/lib/mira/${node}",
    "retention": "7d",                  # ms | s | m | h | d; bare number is seconds
  },

  "ingest": {
    "max_request_bytes": "16MiB",       # b | k/kb/kib | m/mb/mib | g/gb/gib; bare number is bytes
  },
}
```

That is every key, and it is a closed set: anything else stops the process at
boot, naming the outermost key it does not recognise and listing the six. An
empty section counts — `{ "cluster": {} }` is `unknown key "cluster"`, because a
section an operator is about to fill in is the one most likely to be believed
accepted. `storage.retension` taken in silence boots on the 7-day default and
surfaces a week later as a full disk, and a file has no business being the
lenient half of an interface whose flags already answer `unknown flag --nope`.

If you are looking for a block size, a flush interval, a buffer depth, a cache
size or a compaction threshold, they do not exist and will not be added — see
principle 2c in [ARCHITECTURE.md](ARCHITECTURE.md) §1. Those are numbers the
engine is better placed to choose than you are, and every one of them exposed is
a number that will be set wrong in production and never revisited.

What *is* configurable is everything the engine genuinely cannot know: where to
listen, where to write, how long to keep data, what this replica is called, and
how large an export its senders produce.

## `ingest.max_request_bytes`

The largest export Mira will decode, on either port. It is the axum body limit
on 4318, tonic's `max_decoding_message_size` on 4317, and the ceiling on what a
gzip body may inflate to — one number, so a batch cannot succeed on one
transport and fail on the other.

This is the one size knob, and it exists because the size of an export is a
property of the *sender*, not of the engine. A collector with
`batch/send_batch_max_size` raised, an application exporting 100k-attribute
spans, a `telemetrygen` run — none of that is knowable from here. The default,
16 MiB, is eight times axum's default and four times tonic's, and comfortably
above what a stock collector produces at its own default of 8192 records.

Units are binary: `MB` means `MiB`. Every other size in this system is binary,
and a `MB` that meant 10^6 next to a page size that meant 2^20 would be a trap.

Set it too low and you lose data rather than throughput: 4318 answers `413`,
which OTLP classes as permanent, so the exporter drops the batch instead of
retrying it. If exports are being rejected, both error messages name this key.

## Interpolation

| syntax | meaning |
|---|---|
| `${env:NAME}` | environment variable; **missing is a startup error** |
| `${env:NAME,default}` | everything after the first comma is the default — untrimmed, later commas included, and itself expanded |
| `${dotted.path}` | another key in this file, resolved recursively |
| `$${` | a literal `${` |

A missing `${env:NAME}` with no default stops the process rather than expanding
to an empty string. Empty-string defaulting is how a staging cluster ends up
writing into a production path, so it is worth the startup failure.

Defaults nest, so `${env:A,${env:B,fallback}}` works — the scan counts depth to
find the *matching* `}` rather than the first one, which with `A` set would weld
a stray brace onto the value, and `node` ends up in every block directory name.
An unbalanced `${` is a startup error quoting the string it could not terminate.

References resolve recursively and a cycle is a startup error naming the cycle:

```yaml
{
  "node": "${a}",
  "a": "${node}",     # error: reference cycle: node -> a -> node
}
```

There is deliberately no second `MIRA_*` environment-override mechanism.
`${env:...}` already covers every case, and does it visibly in one file. Two ways
to set the same value is the complexity this is meant to avoid.

## `node`, and why it matters

`node` names this replica. It is hashed into every block directory name
(`{min_ts}-{max_ts}-{node}-{seq}`), which is what lets several active replicas
write to one volume without coordinating. In Kubernetes set it from the pod name,
which the scheduler already guarantees is unique:

```yaml
{ "node": "${env:HOSTNAME}" }
```

The resolved name and its hash are logged at startup:

```
INFO mira: mira listening grpc=… http=… node=mira-1 node_id="a2c1d111"
```

Two replicas showing the same `node_id` while sharing a volume is the one
misconfiguration to watch for, and that log line is where you diagnose it —
nothing else names it. What collides is *not* the block directory: that name
carries `{min_ts}-{max_ts}` as well, and in the run below 107 blocks landed
without one repeat. It is the staging path, `.tmp/{signal}-{node_id}-{seq}`,
which has no timestamp in it, which both replicas resume from the same `seq` at
boot because they scanned the same volume, and which `publish` clears before it
writes. So the two take turns deleting each other's half-written block.
Reproduced here, two replicas both `--node collide` against one directory:

```
ERROR mira::pipeline: block not published signal="logs" seq=0 error=io error on
  /tmp/mira-collide/.tmp/logs-24bdd5d9-000000000000:
  Directory not empty (os error 66)
ERROR mira::pipeline: block not published signal="traces" seq=4 error=io error on
  /tmp/mira-collide/.tmp/traces-24bdd5d9-000000000004/resource_attrs.arrow:
  No such file or directory (os error 2)
```

Nothing is acked that is not durable, so the sender gets a retryable status and
sends it again. But it is a race rather than the clean refusal it looks like:
that run lost two blocks out of 107, and a quieter one loses none and looks
perfectly healthy.

## Kubernetes

One volume per replica. A `StatefulSet` gives each pod a stable name and rebinds
it to the same PVC across reschedules, so `HOSTNAME` names both the replica and
the disk its blocks are on:

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

Applied on kind at `replicas: 2`, that comes up as `node=mira-0
node_id="c3c18a15"` and `node=mira-1 node_id="a2c1d111"`, each bound to its own
PVC.

Ingest goes through one Service — any replica accepts any export, because
nothing has to land on a particular node. **A query does not**: there is no
fan-out, so a replica answers only from its own blocks. Address a specific pod
(a headless Service gives you `mira-0.mira`), or run one replica. Mira does not
query the others and does not replicate, both consequences of holding no
coordination state; [ARCHITECTURE.md](ARCHITECTURE.md) §12 has the rest.

Several pods sharing one `/data` is the topology the `node_id` in the block
directory name exists for, and the only one where any replica answers for all of
them — but it is not deployable on Kubernetes today. Every backend an RWX PVC is
in practice — NFS (EFS, Filestore, most CSI drivers), CephFS, Azure Files over
SMB/CIFS, Lustre, 9P — is on the list `block::fs_type` refuses, because mmap over
a network filesystem raises `SIGBUS` with no recovery path
([ARCHITECTURE.md](ARCHITECTURE.md) §9). `volumeMode: Block` is not a way round
it: a Block PVC is a raw device under
`volumeDevices`, unformatted and with no mount path, so it can never be
`"dir": "/data"` — and the cluster filesystems you would format one with are on
the same refuse list.
