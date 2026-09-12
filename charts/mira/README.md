# mira

![Version: 0.0.3](https://img.shields.io/badge/Version-0.0.3-informational?style=flat-square) ![Type: application](https://img.shields.io/badge/Type-application-informational?style=flat-square) ![AppVersion: 0.0.3](https://img.shields.io/badge/AppVersion-0.0.3-informational?style=flat-square)  [![Artifact Hub](https://img.shields.io/endpoint?url=https://artifacthub.io/badge/repository/mira)](https://artifacthub.io/packages/helm/mira/mira)

OTLP-native telemetry storage engine in a single binary — OTLP in, immutable Arrow blocks out, queried straight from mmap. One StatefulSet, one data directory, no sidecar and nothing to coordinate.

## What it installs

| Resource | Why |
|---|---|
| `StatefulSet` | One container, no sidecar, no init container. See [Why a StatefulSet](#why-a-statefulset). |
| `ConfigMap` | Mira's config file, in KYAML, rendered from `config.*`. Optionally the alert rules beside it. |
| `Service` | ClusterIP, ports 4317 (OTLP/gRPC) and 4318 (OTLP/HTTP, query API, MCP, UI). |
| `Service` (headless) | Per-pod DNS. The only way to address one replica, which matters because a query is answered from that replica's own blocks. |
| `ServiceAccount` | Identity only — no Role, no ClusterRole, and no token mounted. Mira never calls the API server. |
| `Ingress` | Optional, off by default, 4318 only. |

There is no operator, no CRD, no leader election, no metadata store and no
migration job, and none of those are gaps: Mira holds **no coordination state**.
The block directory is the manifest.

## Install

```bash
helm install mira oci://ghcr.io/trianalab/charts/mira \
  --namespace observability --create-namespace
```

Pin the version:

```bash
helm install mira oci://ghcr.io/trianalab/charts/mira \
  --version 0.0.3 \
  --namespace observability --create-namespace
```

Point an exporter at `mira.observability.svc:4317`, then port-forward 4318 for
the UI, the query API and MCP:

```bash
kubectl port-forward -n observability svc/mira 4318:4318
```

## Why a StatefulSet

Mira reads every block through `mmap`. On a network filesystem an I/O hiccup is
delivered as `SIGBUS` — a signal, not an `io::Error`, with nothing to catch and
no way to unwind — so Mira `statfs`es its data directory at startup and
**refuses to boot on NFS, SMB, CIFS, CephFS, 9P or AFS**.

That rules out the one shape a `Deployment` could use: a single RWX PVC shared
by every pod, since every backend an RWX PVC is in practice is on that list.
What is left is one RWO volume per replica, which is `volumeClaimTemplates`, which
is a StatefulSet. Ordinal identity then comes free, and it is the honest model
anyway — nothing replicates and nothing rebalances, so a pod that came back
bound to a different volume would come back having lost its blocks.

Set `persistence.storageClass` to a local or block-backed class (`local-path`,
EBS `gp3`, GCE PD, Azure Disk). `persistence.enabled=false` swaps in an
`emptyDir` for kind, CI and demos.

## Sizing

The defaults below — `500m` CPU, `512Mi` requested, `2Gi` limit, a `20Gi` PVC —
are the "a team's services" row of
[the sizing table](https://miradb.dev/config/#sizing), which is measured rather
than guessed and gives CPU, memory and disk per day against the record rate.
Two things it will tell you that are not obvious from here: memory tracks the
number of concurrent exporters and not the ingest rate at all, and the on-disk
cost per record is 8x higher above ~38k records/s per signal, where compaction
stops keeping up.

## Replicas

`replicaCount` defaults to 1, and more replicas do not mean higher availability.
Every replica is a whole Mira with its own store: ingest through the Service is
spread across all of them, and a query through the Service is answered by
whichever one kube-proxy picked, from that replica's blocks only. There is no
fan-out and no replication. Scale up when one core cannot keep up with the
export rate; address a specific replica through the headless Service's per-pod
name when you need to read what it stored.

## Configuration

Mira's config file is KYAML (a strict subset of YAML 1.2, so a JSON superset).
The chart renders `config.*` into it and mounts it at `/etc/mira/mira.yaml`; the
pod's whole command line is `--config /etc/mira/mira.yaml`. The keys under
`config` are the closed set from
[Configuration](https://miradb.dev/config/) —
an unknown key stops Mira at boot rather than being ignored, and this chart's
`values.schema.json` is closed for the same reason.

`listen` and `storage.dir` are deliberately not values: the container always
listens on `0.0.0.0:4317` and `0.0.0.0:4318` and always writes to `/data`. A
Service already remaps ports and a `volumeMount` already remaps paths; making
them values would only let you break the probes.

### Alerting

`config.alerts.rules` takes the rules document inline; the chart writes it into
the same ConfigMap and points `alerts.rules` at it. It is **refused at
`replicaCount > 1`**: nothing elects an evaluator, so every replica would
evaluate the same rules and page separately. Run the evaluating replica as its
own single-replica release.

## Health

| Probe | Path | Why |
|---|---|---|
| startup | `/health` | Mira replays its write-ahead log before serving either port. A liveness probe alone would kill it mid-replay, forever. |
| liveness | `/health` | A constant 200. A flusher that stops takes the process with it, so answering at all is the liveness answer. |
| readiness | `/readyz` | Goes false on a *sustained* flush stall — a full or unwritable volume — which takes the replica out of the Service so exporters retry somewhere that can store them. Restarting it would fix nothing. |

All three are HTTP on 4318 and none of them can be `exec`: the image is
distroless, with no shell and no `curl`.

There is no `ServiceMonitor` in this chart, because Mira exposes no Prometheus
endpoint to scrape — `/api/v1/stats` answers JSON, and the terminal UI's node
pane reads it. There is no `PodDisruptionBudget` either: with one replica the
only PDB that means anything blocks every node drain, and the data survives the
reschedule.

## Security

The defaults are the hardened ones, and they describe the image rather than
constrain it: `distroless:nonroot`, uid 65532, no shell.

- `runAsNonRoot`, `runAsUser: 65532`, `fsGroup: 65532` (without which a fresh
  PVC stays root-owned and Mira cannot write its first block)
- `readOnlyRootFilesystem: true` — Mira writes to `/data` and nowhere else
- `capabilities: drop: [ALL]` — both ports are above 1024
- `seccompProfile: RuntimeDefault`
- `automountServiceAccountToken: false` — Mira never calls the API server

## Verifying the chart

The chart and the image are signed with [cosign](https://docs.sigstore.dev/)
using keyless (OIDC) signing:

```bash
cosign verify \
  --new-bundle-format=false \
  --certificate-oidc-issuer https://token.actions.githubusercontent.com \
  --certificate-identity-regexp 'github.com/TrianaLab/mira/.github/workflows/release.yml' \
  ghcr.io/trianalab/charts/mira:0.0.3
```

## Values

| Key | Type | Default | Description |
|-----|------|---------|-------------|
| affinity | object | `{}` | Affinity rules. Worth setting when `replicaCount > 1` and the volumes are node-local: two replicas on one node share that node's disk bandwidth and die together. |
| config.alerts.rules | string | `""` | The alert rules document, inline, in KYAML. Empty means this node evaluates nothing and pages nobody, which is the default and is why an empty `/api/v1/alerts` means "alerting is off here" rather than "all clear". The chart writes it into the same ConfigMap and points `alerts.rules` at the file. It is refused at `replicaCount > 1`: nothing elects an evaluator, so three replicas would send three of every page. docs/config.md has the schema. |
| config.ingest.maxRequestBytes | string | `"16MiB"` | The largest export Mira will decode, on either port. 16MiB is eight times axum's default and four times tonic's, and comfortably above what a stock collector produces at its own default batch size. Too low is worse than it sounds: 4318 answers 413, which OTLP classes as permanent, so the exporter drops the batch instead of retrying it. |
| config.ingest.queue | int | `128` | How many exports may be waiting for one signal's flushers. A full queue parks the next export for up to five seconds rather than refusing it, so this buys burst absorption and not throughput. Each slot can hold a decoded export, so the worst case is this times `maxRequestBytes` times three signals resident — check it against `resources.limits.memory` before raising it. |
| config.ingest.shards | int | `0` | How many flusher tasks each signal runs, or 0 for one per two cores as the process sees them. A `resources.limits.cpu` quota is read correctly on its own, so leave this at 0 if you set one. It is for the cases that are not a quota: a `cpu.shares`/`cpu.weight` relative weight, or no limit at all on a large node, both of which read as the whole machine — on a 96-core node that is the capped sixteen flushers per signal for a pod that will get two cores. Set it to the whole cores the pod actually gets. Shards split `queue` between them rather than multiplying it, so the memory arithmetic above does not move; 16 is the ceiling the engine enforces. |
| config.ingest.wal | bool | `true` | Write-ahead log. Off means an export is acknowledged only once it is in a sealed, fsynced block — p99 around 2.6s, and read-your-writes holds. On means acknowledged once written to the log — p99 under 5ms, survives the process dying, does not survive the machine dying for up to 250ms, and what you just sent is not queryable yet. Both are correct; no measurement here can pick for you, so this is the binary's own default rather than a second opinion from the chart. |
| config.node | string | `"${env:POD_NAME}"` | This replica's name. It is hashed into every block directory name, which is what lets replicas share a volume without coordinating, so it has to be unique per pod. `POD_NAME` comes from the downward API rather than `HOSTNAME`, which is set by the container runtime and not guaranteed by Kubernetes — and a missing `${env:...}` with no default is a startup error, by design, so guessing is not an option. |
| config.storage.retention | string | `"7d"` | How long to keep data. Units are `ms`, `s`, `m`, `h` or `d`, and a bare number is seconds. Seven days is the default the engine ships with; it is the only knob standing between an export rate and a full volume. |
| config.telemetry.interval | string | `"15s"` | How often it samples, when `self` is on. Same duration syntax as `storage.retention`. |
| config.telemetry.self | bool | `false` | Mira storing its own counters, in itself, as ordinary metrics. No exporter, no scrape endpoint, no second port and no collector: turn it on and the metrics tab has content within one interval. It is the fastest way to see the engine work, and the cost is one export per interval competing with real ingest for the same flusher. |
| extraEnv | list | `[]` | Extra environment variables, verbatim `core/v1` EnvVar entries. The reason this exists: an alert rule that pages PagerDuty reads its routing key with `${env:PD_ROUTING_KEY}`, and that belongs in a Secret, not in values. |
| fullnameOverride | string | `""` | Override the full release name. |
| image.pullPolicy | string | `"IfNotPresent"` | `IfNotPresent`, because the tag is an immutable release version: re-pulling it on every restart costs a registry round trip and can never return anything different. Use `Always` only if you retag. |
| image.repository | string | `"ghcr.io/trianalab/mira"` | One binary on `distroless/cc`. The published image carries the same bytes as the release tarball rather than a second compile, so one attestation covers both. |
| image.tag | string | `""` | Overrides the image tag (default is the chart appVersion). Deliberately empty: the chart version, the app version and the image tag are one number (`scripts/check_drift.py` enforces it), so pinning it here would only be a fourth place for it to drift. |
| imagePullSecrets | list | `[]` | Pull secrets, for a private mirror of the image. Empty because the public image needs none. |
| ingress.annotations | object | `{}` | Annotations on the Ingress. Only 4318 is routed: OTLP/gRPC needs a per-controller backend-protocol annotation and an h2c-capable data path, so gRPC ingress is left to whoever knows which controller they run. |
| ingress.className | string | `""` | IngressClass name. |
| ingress.enabled | bool | `false` | Off. The UI, the query API and MCP all live on 4318, so an Ingress is how a human reaches Mira from outside the cluster — but it publishes an unauthenticated read surface, so turning it on is a decision. |
| ingress.hosts | list | `[{"host":"mira.local","paths":[{"path":"/","pathType":"Prefix"}]}]` | Hosts and paths. |
| ingress.tls | list | `[]` | TLS blocks, verbatim. |
| nameOverride | string | `""` | Override the chart name in resource names and labels. |
| nodeSelector | object | `{}` | Node selector for the pods. |
| persistence.accessMode | string | `"ReadWriteOnce"` | `ReadWriteOnce` is the only mode that works, and it is a knob only so that `ReadWriteOncePod` (stricter, 1.29+) can be chosen. There is no RWX option: every backend an RWX PVC is in practice — NFS, EFS, Filestore, CephFS, Azure Files — is refused at boot, because `mmap` over a network filesystem delivers an I/O hiccup as SIGBUS, a signal with nothing to catch. |
| persistence.annotations | object | `{}` | Annotations on the generated PVCs. Note that PVCs outlive `helm uninstall` by Kubernetes' own default, which for a telemetry store is the right way round — delete them by hand when you mean it. |
| persistence.enabled | bool | `true` | A PVC per replica, via `volumeClaimTemplates`. A replica's identity *is* its data: nothing replicates and nothing rebalances, so a pod that comes back on an empty volume comes back having lost that node's share of the history. Turn this off only for kind, CI or a demo. |
| persistence.size | string | `"20Gi"` | 20Gi holds roughly a week of a small service's telemetry at Mira's on-disk cost. Size it from `retention` x your export rate; a full volume takes the replica out of the Service via `/readyz` rather than losing data. |
| persistence.storageClass | string | `""` | Empty means the cluster default StorageClass. **Point this at a local or block-backed class** (`local-path`, EBS `gp3`, GCE PD, Azure Disk). Mira mmaps every block it reads: on a network-backed class the process does not get an error, it gets SIGBUS and dies mid-query — which is why Mira checks the filesystem type at startup and refuses to run on NFS, SMB, CephFS or 9P at all. |
| podAnnotations | object | `{}` | Extra annotations on the pod. The config checksum is added automatically, so a `helm upgrade` that only changes `config` still rolls the pods. |
| podLabels | object | `{}` | Extra labels on the pod. |
| podSecurityContext.fsGroup | int | `65532` | The one that is load-bearing rather than decorative. A freshly provisioned PVC is mounted root-owned; without `fsGroup` the CSI driver never chowns it, Mira cannot create its first block directory, and the failure surfaces as a permission error per export minutes after a pod that looked healthy. |
| podSecurityContext.runAsGroup | int | `65532` | gid 65532, matching the image. |
| podSecurityContext.runAsNonRoot | bool | `true` | The image is `distroless:nonroot`, so this is a statement of what is already true rather than a constraint being imposed: a pod that suddenly needs root has been tampered with and should fail to schedule. |
| podSecurityContext.runAsUser | int | `65532` | uid 65532 — `nonroot` in the distroless base, and the owner of `/data` in the image. |
| podSecurityContext.seccompProfile.type | string | `"RuntimeDefault"` | `RuntimeDefault`. Mira makes ordinary syscalls — `mmap`, `statfs`, `fsync`, sockets — and needs no exemption from the default filter. |
| replicaCount | int | `1` | Replicas. One, because a replica is a whole Mira: it holds no coordination state (principle 4), so a second one is a second, independent store. Two replicas double ingest capacity and *halve* what any one query can see, since a query is answered from that replica's own blocks with no fan-out. Raise this when one core cannot keep up with the export rate, not for availability. |
| resources.limits.memory | string | `"2Gi"` | A memory ceiling is safe here and worth having: Mira reads blocks through `mmap`, and mapped file pages are clean and reclaimable, so the cgroup reclaims them under pressure instead of OOM-killing. Without a limit the resident set looks unbounded to the scheduler and the node evicts something else instead. |
| resources.requests.cpu | string | `"500m"` | Ingest is CPU-bound (decode, encode, compress). Half a core is what one replica needs to keep up with a stock collector's default batch rate. |
| resources.requests.memory | string | `"512Mi"` | Enough for the open blocks of all three signals plus the query working set; everything else is page cache. |
| securityContext.allowPrivilegeEscalation | bool | `false` | Nothing in this image is setuid and there is no shell to escalate into. |
| securityContext.capabilities.drop | list | `["ALL"]` | Drop everything. Mira binds 4317 and 4318, both above 1024, so it does not even want `NET_BIND_SERVICE`. |
| securityContext.readOnlyRootFilesystem | bool | `true` | Read-only root filesystem. Mira writes to exactly one place, the data directory, and that is a volume — so a writable root would only ever be used by something that is not Mira. |
| securityContext.runAsNonRoot | bool | `true` | Repeated at container level so the guarantee survives someone loosening the pod-level context. |
| securityContext.runAsUser | int | `65532` | uid 65532, as above. |
| service.annotations | object | `{}` | Annotations on the Service (internal load balancer, topology hints). |
| service.grpcPort | int | `4317` | OTLP/gRPC. The Service port; the container port is fixed at 4317 because the image, the probes and every doc say so. |
| service.httpPort | int | `4318` | OTLP/HTTP *and* the query API *and* MCP *and* the UI. One port, because it is one binary and one axum router. |
| service.type | string | `"ClusterIP"` | `ClusterIP`. OTLP is an in-cluster protocol: collectors and SDKs are pods. Expose 4318 through `ingress` below if a browser needs the UI. |
| serviceAccount.annotations | object | `{}` | Annotations on the ServiceAccount (IRSA, Workload Identity). |
| serviceAccount.automount | bool | `false` | No API token in the pod. Mira never talks to the API server, so a mounted credential is pure blast radius: it is the difference between a compromised ingest process and a compromised cluster client. |
| serviceAccount.create | bool | `true` | Create a ServiceAccount. Mira needs no RBAC whatsoever — it never calls the API server — but a named identity is what a NetworkPolicy, a PodSecurity exemption or an audit rule binds to, and sharing `default` with everything else in the namespace makes all three meaningless. |
| serviceAccount.name | string | `""` | Name override (defaults to the fullname). |
| startupProbeFailureThreshold | int | `60` | How long the kubelet waits for a startup probe, in units of 5 seconds. Mira replays its write-ahead log before it serves either port, so a node with a large log is legitimately slow to answer — and a liveness probe on its own would kill it mid-replay, forever. 60 x 5s = five minutes. |
| terminationGracePeriodSeconds | int | `60` | Seconds Kubernetes waits after SIGTERM. Mira drains: it stops accepting, finishes the exports in flight and seals the open block of each signal, so killing it early is how you turn a clean shutdown into a WAL replay on the way back up. |
| tolerations | list | `[]` | Tolerations for the pods. |

## Maintainers

| Name | Email | Url |
| ---- | ------ | --- |
| edu-diaz | <edudiazasencio@gmail.com> | <https://edudiaz.dev> |
