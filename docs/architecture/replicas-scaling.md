# Multiple active replicas — scaling, sharing and the coordinator

## 12.3 Discovery without membership

The replica set is a list of addresses given to the proxy at startup and never
revisited. Mira stores nothing about the cluster: no gossip, no heartbeat, no
join/leave, no split brain — not because they are solved but because there is no
membership to be wrong about.

In Kubernetes that list is the pods of a StatefulSet, whose DNS names are
stable. A headless Service is how an OTLP exporter reaches the proxy, not how
the proxy finds its replicas: a DNS answer that changes underneath a running
process is the membership event this design avoids. Adding a replica is a config
change and a restart of the proxy.

A replica that does not answer fails the query it was part of (section 12.2.1).
There is no liveness tracking and no ejection: that would be membership.

## 12.4 What scales, and what this deliberately does not buy

| | |
| --- | --- |
| Ingest throughput | Linear. Nodes are independent. |
| Storage capacity | Linear. |
| Query capacity | Linear; every replica answers independently. Direct to a node, that answer covers the whole dataset on a shared volume and that node's share shared-nothing. Through `mira proxy` it covers all of them, and one query's latency becomes the slowest replica's. |

- **No replication.** A lost disk is lost data for that node's share, the
  sharpest edge of principle 4. Client-side fan-out is the answer: an OTel
  Collector can export to two Mira replicas. A replication factor above one
  requires a placement decision, and placement *is* coordination state.
- **No deduplication.** OTLP is at-least-once; a retried export can be stored on
  two replicas, and suppressing that needs a global index.
- **No rebalancing, ever.** **Retention is the rebalancer**: a cluster is evenly
  loaded one retention period after any scale-out, with zero bytes moved.
  Scaling *in* collects no such dividend: the removed replica's PVC survives
  unread, so its blocks have to be re-homed with `mira offload push` and `mira
  offload restore` (section 6.1). No lifecycle hook does it: a pod cannot tell a
  scale-in from a rolling restart, so a `preStop` hook would evacuate every
  replica on the next image bump.
- **A replica's identity is its disk.** A StatefulSet with a PVC. On ephemeral
  disk, a rescheduled pod's unexpired data is gone.

## 12.5 Shared-volume mode

If replicas do share one filesystem the design works unmodified: block names are
unique per writer, publishes are independent renames into a staging path unique
per block (section 12.6), and `scan` sees every writer's blocks, so any node
answers any query without fan-out.

The scope is narrower than it looks: **mmap over a network filesystem raises
SIGBUS with no recovery path**, enforced by the `statfs` check at startup
(section 9), and that eliminates every RWX PVC anyone provisions, since RWX in
practice means NFS, CephFS or Azure Files.

So the supported shape is **several processes on one host** sharing a local
directory, the mode `--node` exists for, and it is **not covered by a test**:
section 12.6's measurement was a run done by hand. Across hosts the answer stays
shared-nothing, with `mira proxy` (section 12.2).

## 12.6 The staging path is the one place two writers can still collide

The final block name, `{min_ts}-{max_ts}-{node}-{seq}-{wal_hi}`, is unique per
writer. The *staging* name, `.tmp/{signal}-{node}-{seq}`, was not: `node` comes
from `--node` and defaults to `mira`, so two replicas started with the defaults
over one directory walk the same sequence.

A two-process run lost 2 publishes in 107 that way, all retryable NACKs from a
failed rename — the benign half. The other half was permitted and not observed:
B's `remove_dir_all` empties the directory A is writing tables into, A keeps
writing by path, and whichever wins the rename publishes a block whose tables
came from two sealed sets — no error, and a query returning rows that never
coexisted.

The fix puts the timestamp range the final name already carries into the staging
name: the path is then unique per block *content*, and a misconfigured `--node`
is duplicated data rather than a silent mix. `create_dir` rather than
`create_dir_all` makes a collision fail loudly, and `block::sweep_staging` clears
at boot what a never-reused name leaks.

Principle 4 buys freedom from coordination *state*, not from thinking about
concurrency: on a shared filesystem every path a writer creates is part of the
argument.

## 12.7 The coordinator lives outside the binary

**Built**: `integrations/kubernetes`, a second Cargo workspace producing a second
binary, `mira-operator`, and a `MiraCluster` CRD. It owns the StatefulSet and is
the only chart Mira publishes: a controller that only writes `spec.replicas` on
a Helm-owned object loses the value on the next `helm upgrade`, and on the way
down that unwinds *after* the drain has copied the blocks out. Everything above
resizes a tier by hand; that argument is about *Mira*, not about whether
something else may hold the decision.

### Why not an HPA

Mira has no metric that moves when it needs another replica. Section 11 measures
2.23 of twelve cores at 1,537,875 records/s, so a CPU-target HPA reads
single-digit utilisation at saturation, and a memory-target one reads page
cache. What runs out is **disk**, which no HPA can scale a StatefulSet on, so
the trigger is `free_fraction` from `/api/v1/stats`.

### Why this does not violate principle 4

The principle constrains Mira, and a controller is not Mira. The test is what
happens when it is deleted: every pod keeps ingesting, serving and reading its
blocks, because none of them ever asked it anything. Only the scaling stops.

### The two thresholds are asymmetric on purpose

Out when the *fullest* replica drops below `upWhenFreeBelow`; in when *every*
replica is above `downWhenFreeAbove`. Not the mean either time: `route` sends a
resource to `hash(resource) % n` (section 12.1) and resources differ in size, so
a mean of 0.4 across ten replicas is compatible with one at 0.02 that has
stopped accepting writes. Nor may they meet: adjacent thresholds oscillate, and
every cycle moves a replica's whole dataset through `offload push`, so such a
spec is refused as `Degraded`.

### A scale-in is section 12.4's manual procedure, sequenced

`status.draining` is written *first*, so a controller that restarts mid-sequence
resumes instead of orphaning a volume. The pod goes while the claim stays,
because the drain Job cannot attach a `ReadWriteOnce` claim the pod still holds.
Only on success is the claim deleted, and then the Job; a failed drain stops
before both and reports `Degraded`. `spec.offload` is required before a tier
will ever shrink: the cost of not shrinking is a bill, the cost of shrinking
without an archive is the data.

### Two ways to delete a volume and report success

`spec.offload` is a `file://` URL, so the archive is a mount, and a drain Job
with nothing mounted at it archives into its own container filesystem and exits
0; `spec.coldStorageClaim` is therefore validated as *required* alongside
`offload`, which makes that configuration unrepresentable. And
`kubernetes.io/pvc-protection` holds a claim alive while any scheduled pod
references it, a *completed* pod counts and nothing removes a finished Job, so
the claim deleted a step earlier stayed `Terminating` for ever, logged as
released.

### It does not re-home the blocks afterwards

That is 12.4's "no rebalancing, ever", and `ReadWriteOnce` forbids it anyway: a
restore has to mount a *surviving* replica's volume, which its pod holds.

**ponytail:** the election is soft. A `coordination.k8s.io` Lease makes exactly
one replica reconcile — kube-rs ships none
([kube-rs/kube#485](https://github.com/kube-rs/kube/issues/485)) — but with no
fencing token a stuck renewal can leave the holder mid-reconcile when a
standby's clock says the lease expired: seconds of overlap after a failure, not
permanent concurrency by configuration.

### The CRD is the other ceiling

`apiextensions` **prunes** a field the CRD does not declare rather than
rejecting it, so a cluster on a stale schema loses those fields silently. The
CRD is therefore generated from the Rust types by `crdgen`, with `make
operator-crd-check` failing when the checked-in copy disagrees. Helm installs
`crds/` once and never upgrades it, so a schema change needs it applied by hand
first; [Install](../install.md#kubernetes) says so.

---
