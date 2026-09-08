# Configuration

```
mira [--config FILE] [--node NAME] [--grpc ADDR] [--http ADDR]
     [--data-dir PATH] [--retention DURATION] [--peers a:1,b:2]
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

| written | parsed as | reaching Mira as |
|---|---|---|
| `node: 0x1f` | integer `31` | `"31"` — the text changed |
| `node: False` | boolean | `"false"` — the case changed |
| `node: null` | null | key treated as absent; the default is used |
| `node: no` | string | `"no"` — but only because this is YAML 1.2; in 1.1 it is `false` |

The last row is the argument. The correct reading of an unquoted scalar is not a
property of the document, it is a property of whoever is reading it. Quoting
makes it a property of the document, for two characters.

Mira does not merely tolerate this, it enforces it: **every value it reads is a
string, and a value that arrived as any other type is a startup error** telling
you to quote it. Coercing back with `to_string()` is exactly what turns `0x1f`
into `31` without anyone noticing.

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

  "cluster": {
    "peers": "${env:MIRA_PEERS,}",      # scatter-gather targets; empty = single node
  },
}
```

That is every key. If you are looking for a block size, a flush interval, a
buffer depth, a cache size or a compaction threshold, they do not exist and will
not be added — see principle 2c in `ARCHITECTURE.md` §1. Those are numbers the
engine is better placed to choose than you are, and every one of them exposed is
a number that will be set wrong in production and never revisited.

What *is* configurable is everything the engine genuinely cannot know: where to
listen, where to write, how long to keep data, what this replica is called, and
who its peers are.

## Interpolation

| syntax | meaning |
|---|---|
| `${env:NAME}` | environment variable; **missing is a startup error** |
| `${env:NAME,default}` | everything after the first comma is the default, verbatim |
| `${dotted.path}` | another key in this file, resolved recursively |
| `$${` | a literal `${` |

A missing `${env:NAME}` with no default stops the process rather than expanding
to an empty string. Empty-string defaulting is how a staging cluster ends up
writing into a production path, so it is worth the startup failure.

Defaults nest, so `${env:A,${env:B,fallback}}` works.

References resolve recursively and a cycle is a startup error naming the cycle:

```yaml
{
  "node": "${a}",
  "a": "${node}",     # error: reference cycle: node -> a -> node
}
```

Resolution is **lazy** — only keys the binary actually reads are expanded, so an
unused key with a broken reference will not stop the process from booting.

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
INFO mira: mira listening grpc=… http=… node=mira-1 node_id="a2c1d111" peers=0
```

Two replicas showing the same `node_id` while sharing a volume is the one
misconfiguration to watch for. It is not silent — the second publisher fails on
`ENOTEMPTY` — but the log line is where you diagnose it.

## Kubernetes

```yaml
{
  "node": "${env:HOSTNAME}",
  "storage": {
    "dir": "/data/${node}",
    "retention": "${env:MIRA_RETENTION,7d}",
  },
  "cluster": { "peers": "${env:MIRA_PEERS,}" },
}
```

Point `MIRA_PEERS` at the addresses behind a headless Service. Mira keeps no
cluster state of its own; DNS is the membership. See `ARCHITECTURE.md` §12 for
what that does and does not buy — in particular, there is no replication, and
that is a deliberate consequence of having no coordination state.
