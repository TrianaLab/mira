# 12. Multiple active replicas

N replicas, all active, no hard coordination: principle 4 — "stateless means no
coordination state" — as a deployment topology. Ingest (section 12.1) was
coordination-free already; the shared volume of section 12.5 needs no fan-out at
all; `mira proxy` (section 12.2) is the fan-out half. Peer-to-peer broadcast
*between storage nodes* is still unbuilt — section 12.2.4.

## 12.1 Ingest

Any L4 load balancer. Each replica owns its own disk and writes its own blocks.
No ring, no shard map, no routing logic — nothing has to land on a particular
node. Blocks are independent immutable objects with no global ordering and no
cross-block merge, so "which replica received this export" is not a question
anything can ask.

Two things had to change to make concurrent writers safe, and both are in: the
block directory name carries a **node id** (section 3.2), so two replicas cannot
allocate the same name, and retention tolerates a losing race on
`remove_dir_all` (section 6).

## 12.2 Query — `mira proxy`

A storage node answers from the blocks it can see: every writer's on a shared
volume (section 12.5), its own when shared-nothing. It has no peer set;
`cluster.peers` was deleted rather than left in place because nothing read it,
and an unread key is worse than a missing one: an operator sees it accepted and
believes it is in effect.

The fan-out lives in a **second process that stores nothing**. `mira proxy` is
the same binary under a subcommand, given a static list of replica addresses in
`proxy.replicas`, serving the OTLP endpoints and `/api/v1/query` on one HTTP
listener. It cost **zero new dependencies**: the `hyper` stack was already a
direct dependency for the webhook dispatcher.

### 12.2.1 Why it can hold no state — the cursor was already global

Paging is what usually forces a coordinator on a fan-out read: the merger has to
remember, per reader, how far into each node's stream it had got — coordination
state.

Mira does not need one. The keyset cursor of section 8 is `(ts, node, seq, row)`
where `node` is `block::node_id`, so the sort key is **already total across
every row on every replica** — not by design for this, but because block names
must be unique per writer.

So: broadcast the caller's document with the same `after` to every replica, sort
the union on the cursor order, cut to `limit`, return the cursor of the last row
emitted. Every row no replica emitted sorts strictly after that cursor *on every
replica*, so the next page is exact and the proxy has forgotten the reader by the
time the response is written.

**`next` is set by either side**: a replica reporting its own means that node is
holding more, the merged set exceeding `limit` means the cut is, and checking
only the second misses N short pages fitting under `limit`. **A read that cannot
be complete is an error**: one replica timing out fails the whole query rather
than returning six sevenths of the data with nothing saying so.

### 12.2.2 What the node had to grow, and what it did not

A rendered row is an OTLP record and carries no cursor, so a merger could not
tell which of two nodes' rows came first. The node now accepts
`"cursors": "true"` and returns a `"cursors"` array beside `"rows"`,
index-aligned, absent otherwise — beside and not inside because of principle 3:
the rendered row *is* the OTLP record.

The proxy adds that key textually rather than re-serialising through a KYAML
writer, which would normalise the `where` terms, and **refuses the request** if
the flag did not take: without cursors the merge falls back silently to whatever
order the replicas answered in.

What the node did *not* grow is any notion that it is part of a set: a replica
behind a proxy is byte-for-byte the binary that runs alone.

### 12.2.3 What it refuses, and the routing that keeps the door open

`correlate`, `map`, `metrics/query`, `metrics/names` and `entities` answer **501
naming the path** and send the caller to a replica directly. Each is built by
walking one node's blocks, and merging those is not "sort and cut": two nodes
each holding half a trace produce two partial frames with no cursor to interleave
them on, and nothing in the response would say so.

**Hash-based ingest routing holds that door open.** The proxy splits an export
resource by resource on `resource_key`, the 64-bit entity identity of section
7.2, so every record describing one entity lands on one replica. `NO_IDENTITY`
is spread by position instead: it has no entity to keep together, and a fixed
slot would pile every unidentified sender onto replica zero. That buys
entity-local blocks, so a future `entities` or `correlate` is a routed call to
the one node with the whole answer.

Modulo and not a consistent hash ring, marked `ponytail:` at the line: the
replica list is static, and **retention is the rebalancer** (section 12.4). A
partial ingest failure is a 503 for the whole export, so the exporter re-delivers
what did land — at-least-once, which OTLP already is.

### 12.2.4 What this replaced, and what it has not earned

The design this section used to hold was peer-to-peer: a query arriving at any
replica is broadcast to its peers and merged. The proxy is strictly less
machinery for the same result, and holds nothing durable. The set-union argument
that made the old design work is why this one works: block-local ids never leave
a node, and only globally stable identifiers cross the wire. Had entity identity
stayed "equality of the resource attribute set", any cross-node read would have
needed a cluster-wide resource dictionary, which is coordination state.

**What has not been earned is the case for building it.** Nothing has measured a
single node's ceiling to be the binding constraint; the plateau work in section
11 attempted it and established the opposite of the premise everyone was working
from. It was taken on a laptop and does not say a *node* is saturated at a rate
a real workload reaches. The mechanism is built and its cost is measured in
12.2.5, but the argument that it is *needed* rests on an instruction rather than
on a number. Fan-out buys capacity, not a faster answer to the same query: an
unpruned scan is bound by whether the corpus fits page cache.

### 12.2.5 What the hop costs, on one box

`scripts/measure/proxy-ab.sh`. Arm A is one node, generator pointed straight at
it; arm B is two replicas and a proxy, generator pointed at the proxy. Both send
`--records`, not `--for`, so the corpus the read leg scans is identical. Paired
and alternating, B first, nine passes a shape, median of the per-pass ratios,
with `0 shed` and no duplicate row as controls. Read the ratios as a cost and
never as scaling: arm B runs three servers and the generator on the same twelve
cores and one disk.

| Shape | Ingest B/A | Read B/A |
| --- | --- | --- |
| 4 connections | **0.767x**, 0 of 9 | **3.89x**, 9 of 9 |
| 32 connections | 0.973, 3 of 9 — split | **2.76x**, 9 of 9 |
| 96 connections | 1.044, 5 of 9 — split | 3.882, 8 of 9 — split |

Two things reproduce. **The proxy costs ingest at four connections** — all nine
passes, spread 0.666 to 0.939 — the shape where that should be true: no
concurrency to hide the hop, the per-record `resource_key` and the re-encode.
**The wide unfiltered read is several times slower through the proxy**, and the
proxy's `elapsed_us` is why: it starts before the fan-out and stops after the
merge, so it measures the slowest replica rather than one node's read.

The splits are noise and are registered as noise: all six figures are in
`measurements.kyaml`, including the three that did not reproduce. This prices
the hop on the only hardware available, where fan-out cannot pay; 12.2.4 still
stands.
