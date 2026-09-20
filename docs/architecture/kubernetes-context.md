# 14. Cluster context

**Built**: `integrations/kubernetes/src/events.rs`, in the operator. It puts
what Kubernetes knows — the kill, the pull failure, the pod that would not
schedule — onto the same timeline as the telemetry, as OTLP logs. It is off
unless configured, it cannot change a cluster, and it costs the engine nothing:
zero new crates, and no code, because `/v1/logs` was already there.
[Section 15](rca.md) is the other half, the document written over that
timeline.

## 14.1 Telemetry does not contain the kill

An application tells you what it was doing. It cannot tell you that the kubelet
terminated it for exceeding a memory limit, that its image could not be pulled,
or that no node had room for it — those are facts about the process, held by
the thing that ended it, and none of them has ever been on the OTLP wire.

So an RCA written from telemetry alone names the exception at the top of the
stack and stops, and the sentence it is missing — *the process was killed
thirty seconds earlier for exceeding its memory limit* — is the one that makes
the exception make sense. Everything below serves getting that line onto the
timeline.

## 14.2 The engine will never hold a Kubernetes client

`kube` plus `k8s-openapi` is around 220 crates. The README states the engine's
crate count and [section 11](performance.md) scores binary size as an axis, so
that is not a trade-off to weigh: it is an order of magnitude over the whole
dependency budget, for a feature that is inert everywhere Kubernetes is not.

It is also principle 4. A Mira pod that can read the API server is one that
could read membership, and the moment a replica can discover its peers somebody
will make it coordinate with them. The operator already exists as a separate
binary in a separate workspace for that reason
([section 12](replicas-scaling.md)), and is already the process allowed to talk
to the API server. So this lives there, and the engine's side of it is an OTLP
endpoint it already serves.

## 14.3 Logs, not a second read surface

The alternative was an MCP endpoint on the operator — `list_pods`,
`get_pod_events` — with the agent joining them against Mira's records by hand.
The join is the reason it was rejected. Two surfaces means the agent correlates
two result sets on timestamps, in its own head, with no shared window and no
shared entity key: precisely the work [section 7](correlation.md) exists to do
in the engine. `correlate` cannot widen a frame it cannot see, `query_records`
cannot filter across it, and retention does not expire it.

As log records the problem disappears. A Kubernetes Event lands in the same
blocks, inside the same `correlate` frame, expired by the same retention worker
and visible in the same panes. The engine needed no new code: the operator
POSTs OTLP/HTTP JSON to `/v1/logs`.

## 14.4 `k8s.pod.uid`, and the `service.name` that was refused

An exported Event has to join to the telemetry, and the join key is the entity
key in [`mira_core::identity`](https://miradb.dev/api/mira_core/identity/index.html).
Its ladder is ordered: `service.name` + `service.instance.id` first,
`k8s.pod.uid` second, then `container.id`, `host.id`, `host.name`. The exporter
emits the second rung — the uid, not the name, because a name is reused by the
next pod in the ReplicaSet and a uid never is.

It deliberately does **not** synthesise a `service.name`. That is the first
rung, so it would win, and every Event in a namespace would collapse to one
entity key: a Deployment's scaling event, a Node's pressure condition and a
Job's failure all filed as the same entity. The identity module's own warning
is that a plausible subset of the resource attributes produces a confident
wrong key, and inventing the top rung is the purest form of it. An Event about
a non-pod object therefore has no entity key at all, which is correct, and is
still found by `k8s.object.kind`, `k8s.object.name` and `k8s.namespace.name`.

## 14.5 Two watches, because the reason is on the pod

`Reason: OOMKilled` is not reliably an Event. It is
`status.containerStatuses[].lastState.terminated.reason`, with the exit code
beside it, and the same holds for `ImagePullBackOff` and
`CreateContainerConfigError`: the Event stream carries a `BackOff` with a prose
message, and the *structured* reason is only ever on the pod. Watching Events
alone would miss the three most common Kubernetes root causes in the one field
that names them.

So there are two watches, and the pod one reads `lastState` before `state` — a
container that was OOM-killed and then restarted reads as `state.running`, with
the kill recorded only in the state it left. The exporter dedupes on a
per-container token of reason, exit code and termination time, so a pod
resynced every few minutes produces one record per transition.

### An Event with no timestamp is dropped

The ladder is `eventTime`, `lastTimestamp`, `firstTimestamp`,
`creationTimestamp`. If none is set the line's position in a timeline would be
a guess, and a line in the wrong place is worse than a line that is not there,
because a reader believes the order.

`ponytail:` on operator restart the Event watch skips the relist and starts
from new records only, so a restart loses the window rather than duplicating
it. Mira has no dedup key, so a duplicated Event is indistinguishable from a
real repeat — the worse failure of the two. The upgrade path is a
resourceVersion checkpoint on the Lease the operator already holds.

## 14.6 Read-only, and not by omission

Every verb either half needs is `get`, `list` or `watch`. There is no
counterpart to `events.rs` that mutates a cluster and no MCP tool that restarts
anything, and that is a decision rather than a stage not yet built.

The reasoning is what a narrow remediation surface would cost. To be useful it
needs `delete pod`, `patch deployment` and `rollout restart`; to be safe it
needs an approval model, an audit trail and a rollback for when it was wrong —
durable state, on a process whose entire argument is that it holds none. And
the agent asking for it already has `kubectl`. Mira's contribution is the
evidence and the write-up; a tool that both diagnoses and acts is one where
nobody can check the diagnosis against what was changed.

`render_rca`'s remediation section says so in the rendered document, not only
here. The reader of an RCA is the one who needs to know none of it has happened
yet.

## 14.7 One chart value, because two would disagree

`clusterEvents.endpoint` sets the environment variable *and* creates the Role.
An endpoint without the permission is a 403 in a loop that looks like a network
problem; a permission without an endpoint is a grant nobody is using. Deriving
both from one value makes either state unrepresentable.

It is off by default, and that matters more than the feature does. The Role
grants `get`/`list`/`watch` on pods, and a pod spec contains
`spec.containers[].env` — including any secret inlined there. The exporter
reads only `status` and emits no spec field, but "the code only reads status"
is not something an auditor can check, and a granted verb is one bug away from
being used. So an install that never points the operator at a Mira does not
hold the permission at all, and one that does should scope it with
`rbac.namespaces`.

## 14.8 What the operator side costs

One direct edge on `http`, already in its lockfile underneath `hyper-util`;
`serde_json` and the HTTP client were there for the stats poller. The engine
gained nothing at all.
