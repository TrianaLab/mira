# mira-operator

![Version: 0.1.0](https://img.shields.io/badge/Version-0.1.0-informational?style=flat-square) ![Type: application](https://img.shields.io/badge/Type-application-informational?style=flat-square) ![AppVersion: 0.1.0](https://img.shields.io/badge/AppVersion-0.1.0-informational?style=flat-square)

<!--
Links to the repository listing, not to /packages/helm/mira/mira-operator —
that page 404s until the first release publishes this chart. The repository
slug stays `mira` (only its display name changed), so both URLs here are
stable across the rename.
-->
[![Artifact Hub](https://img.shields.io/endpoint?url=https://artifacthub.io/badge/repository/mira)](https://artifacthub.io/packages/search?repo=mira)

The controller that scales a Mira tier on free disk, and drains a replica's blocks to cold storage before its volume is deleted. Install it, then apply a MiraCluster.

This is the only chart Mira publishes. There used to be a second one that
installed a StatefulSet directly, and it was removed rather than kept beside
this: two charts is two answers to "how do I run Mira on Kubernetes", and the
one that cannot scale, cannot drain and cannot be told a ceiling is the wrong
answer to ship as the default. A tier is a `MiraCluster` now.

## Why not an HPA

Mira has no metric that moves when it needs another replica. Section 11 measures
**2.23 of twelve cores at 1,537,875 records/s** — a CPU-target HPA reads
single-digit utilisation at saturation, and a memory-target one reads page
cache, which is the `mmap` working as designed. The quantity that actually runs
out is **disk**, and an HPA has never been able to scale a StatefulSet on its
own volumes filling.

So the trigger is `free_fraction` from `/api/v1/stats`, and something has to
read it. That something also has to sequence a scale-*in*, which an HPA could
never do safely: the volume about to be deleted holds blocks no other replica
has, because nothing replicates and nothing rebalances.

## Does this break "no coordination state"?

No, and the distinction is worth being precise about. Principle 4 says **Mira**
holds no coordination state — no Raft, no membership, no external metadata
store, the block directory is the manifest. A controller is not Mira.

The test is what happens when you delete this Deployment: every Mira pod keeps
ingesting, keeps serving queries and keeps its blocks readable, because none of
them ever asked the operator anything. Only the scaling stops. That is the line
between a coordinator and coordination state — and it is the same delegation the
engine already makes to the platform for pod identity and volume lifecycle.

## Install

```bash
helm install mira-operator oci://ghcr.io/trianalab/charts/mira-operator \
  --version 0.1.0 \
  --namespace mira-system --create-namespace
```

The CRD ships in `crds/`, so Helm installs it on first install and **never
upgrades or deletes it**. That is Helm's rule, not this chart's. This matters
more here than it usually does: the API server **prunes** a field the CRD does
not declare rather than rejecting it, so a `MiraCluster` written against a newer
schema than the cluster holds loses those fields silently, with no error
anywhere. On a chart upgrade that changes the schema, apply the CRD yourself
first, or pull it out of the chart you are about to install:

```bash
helm pull oci://ghcr.io/trianalab/charts/mira-operator \
  --version 0.1.0 --untar
kubectl apply -f mira-operator/crds/miraclusters.yaml
```

Not a `raw.githubusercontent.com` URL at a tag, which is the obvious thing to
write and would 404: the chart is versioned independently of the engine and
there is no `v0.1.0` tag on the repository. `helm
pull` also gets you the CRD from the exact artifact you are installing rather
than from whatever the branch says today.

Then create a tier:

```yaml
apiVersion: mira.miradb.dev/v1alpha1
kind: MiraCluster
metadata:
  name: telemetry
spec:
  image: ghcr.io/trianalab/mira:0.0.4
  replicas: 1          # floor
  maxReplicas: 5       # ceiling; there is no "unbounded"
  storage:
    size: 50Gi
    className: gp3
  scaling:
    upWhenFreeBelow: 0.15
    downWhenFreeAbove: 0.60
    cooldownSeconds: 600
  offload: "file:///cold/${node}"
  proxy:
    replicas: 2
```

## How scaling decides

Read from every replica each reconcile, then:

- **Out** when the *fullest* replica drops below `upWhenFreeBelow`. The fullest,
  not the mean: `route` sends a resource to `hash(resource) % n` and resources
  are not the same size, so a mean of 0.4 across ten replicas is compatible with
  one at 0.02 — and it is the one at 0.02 that stops accepting writes.
- **In** when *every* replica is above `downWhenFreeAbove`. Every, not the mean,
  because the drained replica's share lands on whoever is left.
- **Neither** if any replica is unreachable, or answered `null` for
  `free_fraction` — which is Mira saying it could not `statfs` its own volume.
  Read as 0 that means "scale out"; read as 1 it means "delete a volume". It
  must mean neither.

The two thresholds must not meet. Adjacent thresholds oscillate, each action
creating the condition for its opposite, and every cycle moves a whole
replica's dataset through `offload push`. The operator refuses such a spec and
reports `Degraded` rather than acting on it.

## What a scale-in actually does

`spec.offload` is **required** before the operator will ever shrink a tier, and
unset it simply never does. The cost of not shrinking is a bill; the cost of
shrinking without an archive is the data.

1. `status.draining` is written **first**, so a controller that restarts
   mid-sequence resumes instead of orphaning a volume.
2. The StatefulSet scales down. The pod goes; the claim stays.
3. A Job runs `mira offload push` against the released claim. This is why the
   pod has to go first — the claim is `ReadWriteOnce` and a Job cannot attach it
   while the pod holds it.
4. **Only on success**, the claim is deleted.

A failed drain stops at step 3 with the claim intact and the phase `Degraded`:
the tier is one replica smaller and every block is still on disk.

### It does not re-home the blocks

`offload push` copies to the cold store and the blocks stay there, queryable
again only after a deliberate `mira offload restore`. That is section 12.4's
"No rebalancing, ever" rather than an omission — and the same `ReadWriteOnce`
constraint that forced the order above forbids the reverse: a restore has to
mount a *surviving* replica's volume, which its running pod holds. Automating it
would mean taking a healthy replica down in order to grow it.

## Permissions

One `ClusterRole`, no wildcards, and `delete` on exactly two kinds — the claim a
drained replica leaves behind and the Job that drained it. There is deliberately
no `delete` on pods, statefulsets or deployments: a tier is resized through
`statefulsets/scale`, and a bug that reached for `delete` should get a 403.

Set `rbac.namespaces` to a list to get a `Role` in each instead of one
`ClusterRole`. The CRD is still cluster-scoped to install.

## Replicas

`replicaCount` is bounded to exactly 1 by `values.schema.json`, and the
Deployment's strategy is `Recreate`. There is no leader election — kube-rs has
never shipped one ([kube-rs/kube#485](https://github.com/kube-rs/kube/issues/485),
open since 2021) — so two controllers would both reconcile every `MiraCluster`
and both act on the same reading, moving two replicas for one decision. A moment
with no controller is safe; Mira keeps serving either way.

## Verifying the chart

```bash
cosign verify \
  --new-bundle-format=false \
  --certificate-oidc-issuer https://token.actions.githubusercontent.com \
  --certificate-identity-regexp 'github.com/TrianaLab/mira/.github/workflows/release.yml' \
  ghcr.io/trianalab/charts/mira-operator:0.1.0
```

## Values

| Key | Type | Default | Description |
|-----|------|---------|-------------|
| affinity | object | `{}` | Affinity. |
| fullnameOverride | string | `""` | Overrides the full generated resource name. |
| image.pullPolicy | string | `"IfNotPresent"` | Pull policy. |
| image.repository | string | `"ghcr.io/trianalab/mira-operator"` | Operator image repository. |
| image.tag | string | `""` | Overrides the image tag. Defaults to the chart's appVersion. |
| imagePullSecrets | list | `[]` | Image pull secrets for a private registry. |
| logLevel | string | `"info,kube=warn"` | Log filter, in `tracing-subscriber` `EnvFilter` syntax. `kube=warn` keeps the client's per-request lines out of a log that should be one line per decision. |
| nameOverride | string | `""` | Overrides the chart name in resource names. |
| nodeSelector | object | `{}` | Node selector. |
| podAnnotations | object | `{}` | Pod annotations. |
| podLabels | object | `{}` | Pod labels. |
| podSecurityContext | object | `{"fsGroup":65532,"runAsGroup":65532,"runAsNonRoot":true,"runAsUser":65532,"seccompProfile":{"type":"RuntimeDefault"}}` | Pod-level security context. The operator writes nothing and needs no identity beyond its token. |
| rbac.create | bool | `true` | Create the ClusterRole and binding the operator needs. |
| rbac.namespaces | list | `[]` | Restrict the operator to a list of namespaces by creating a Role in each instead of one ClusterRole. Empty means cluster-wide. |
| replicaCount | int | `1` | Replicas. One, and raising it does nothing useful: there is no leader election in the tree (kube-rs has never shipped one — kube-rs/kube#485, open since 2021), so two controllers would both reconcile every MiraCluster and both decide to scale it. The Deployment's `Recreate` strategy below is the other half of that: a rolling update would briefly run two. |
| resources | object | `{"limits":{"memory":"128Mi"},"requests":{"cpu":"10m","memory":"64Mi"}}` | Resource requests and limits. The controller is idle between reconciles; the ceiling exists to make it evictable rather than because it is reached. |
| securityContext | object | `{"allowPrivilegeEscalation":false,"capabilities":{"drop":["ALL"]},"readOnlyRootFilesystem":true}` | Container security context. |
| serviceAccount.annotations | object | `{}` | Annotations for the ServiceAccount (IRSA, Workload Identity). |
| serviceAccount.create | bool | `true` | Create a ServiceAccount. |
| serviceAccount.name | string | `""` | Name to use. Generated from the fullname when empty. |
| tolerations | list | `[]` | Tolerations. |

## Maintainers

| Name | Email | Url |
| ---- | ------ | --- |
| edu-diaz | <edudiazasencio@gmail.com> | <https://edudiaz.dev> |
