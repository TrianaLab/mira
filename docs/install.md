---
description: Install Mira with one script, a container, cargo, or the Kubernetes operator. Linux and macOS, x86_64 and arm64.
---

# Install

**For:** anyone who wants the binary on their machine.

One binary, called `mira`. Linux (glibc 2.34 or newer) and macOS, on x86_64 and
arm64.

## The one-liner

```sh
curl -fsSL https://miradb.dev/install.sh | bash
```

[Read it first](https://miradb.dev/install.sh) — that URL is the script.

| | |
| --- | --- |
| `--version v0.4.3` | a specific release instead of the latest |
| `--no-sudo` | never escalate; fails instead if the directory is not writable |
| `--no-verify` | skip the provenance check — that the download came out of a build in this repository, which needs `gh`. The checksum is still enforced |
| `MIRA_INSTALL_DIR` | where it lands; default `/usr/local/bin`, and it must already exist |
| `GH_TOKEN` | avoids the anonymous 60-requests-an-hour limit on the version lookup |

## Updating

```sh
mira update              # to the latest release
mira update --dry-run    # print the command it would run, and stop
mira update --version v0.4.3
```

It replaces **the binary that is running**, wherever that is, so a copy in
`~/.local/bin` is replaced rather than shadowed.
`MIRA_INSTALL_DIR` still wins, and it stops with `mira vX is already installed`
when there is nothing to do. If a package manager or `cargo` put the binary
there, keep updating it that way.

## With cargo

```sh
cargo install --locked miradb                    # -> ~/.cargo/bin/mira
```

## From source

You need **Rust 1.88 or newer** and a C compiler.

```sh
git clone https://github.com/TrianaLab/mira && cd mira
cargo install --locked --path crates/mira        # -> ~/.cargo/bin/mira
cargo build --release                            # or: ./target/release/mira
```

## From a release

```sh
V=0.4.3; T=x86_64-unknown-linux-gnu
base=https://github.com/TrianaLab/mira/releases/download/v$V
curl -sSLO $base/mira-$V-$T.tar.gz -O $base/SHA256SUMS
sha256sum -c SHA256SUMS --ignore-missing
gh attestation verify mira-$V-$T.tar.gz --repo TrianaLab/mira
tar -xzf mira-$V-$T.tar.gz --strip-components=1 mira-$V-$T/mira
```

`gh attestation verify` needs `gh` installed. Every release carries one signed
build record covering every file named in `SHA256SUMS` — the tarballs and the
SBOM — so the command proves this tarball came out of a build in this
repository. Verify the tarball, before you extract it: the binary inside is not
a subject of its own.

Linux builds are made on `ubuntu-22.04`, so they need glibc **2.34** or newer —
RHEL 9, Amazon Linux 2023, Debian 12, Ubuntu 22.04 and up. There is no musl
build. On Alpine, build from source.

## Docker

```sh
docker run -p 4317:4317 -p 4318:4318 -v mira-data:/data ghcr.io/trianalab/mira:latest
```

Or build it locally:

```sh
docker build -t mira .
docker run -p 4317:4317 -p 4318:4318 -v mira-data:/data mira
```

!!! warning "Use a named volume, not a bind mount"

    Mira reads its files by mapping them into memory. A bind mount on Docker
    Desktop goes through FUSE, where a disk hiccup arrives as a kill signal.

### Health checks

The image has no `HEALTHCHECK`, because it carries no shell and no `curl`.
Mira answers both probes itself on 4318:

```yaml
readinessProbe:
  httpGet: { path: /readyz, port: 4318 }
livenessProbe:
  httpGet: { path: /health, port: 4318 }
```

## Kubernetes

One Mira needs no chart: the [container above](#docker) is the whole deployment
— one image, one volume, two ports. That is a Deployment, or a StatefulSet if
the volume is to outlive a reschedule.

Running **several** of them is the part worth automating: a volume each, a
`mira proxy` in front, and a decision about when there should be one more. The
only chart here installs that controller rather than Mira:

```sh
helm install mira-operator oci://ghcr.io/trianalab/charts/mira-operator \
  --version 0.3.0 --namespace mira-system --create-namespace
```

[See it on Kubernetes](demo-cluster.md) puts all of it on a laptop in one
command.

### The tier

```yaml
apiVersion: mira.miradb.dev/v1alpha1
kind: MiraCluster
metadata:
  name: telemetry
  namespace: observability
spec:
  image: ghcr.io/trianalab/mira:0.4.3
  replicas: 1          # floor
  maxReplicas: 5       # ceiling; there is no "unbounded"
  storage: { size: 50Gi, className: gp3 }
  resources:                    # unset is BestEffort; the drain inherits this
    requests: { cpu: 2500m, memory: 2048Mi }
  offload: "file:///cold/${node}"
  coldStorageClaim: mira-cold   # must already exist; see below
  proxy: { replicas: 2, resources: { requests: { cpu: 500m, memory: 512Mi } } }
  route:                        # optional; needs the Gateway API CRDs
    parentRefs: [{ name: edge }]
    hostnames: [mira.example.com]
```

`offload` needs `coldStorageClaim`. `file://` is the only scheme the offload
target understands, so the archive has to be a mount.

Sizes for `resources` are in [Configuration](config.md#sizing).

Two things a `MiraCluster` **cannot express yet**: a ServiceAccount per tier,
and arbitrary `config.*` keys.

### Cluster context

```sh
helm upgrade mira-operator oci://ghcr.io/trianalab/charts/mira-operator \
  --namespace mira-system --reuse-values \
  --set clusterEvents.endpoint=http://telemetry-proxy.observability.svc:4318
```

| | |
| --- | --- |
| What it does | The operator watches Kubernetes Events and container state and ships both to that Mira as OTLP logs, keyed on `k8s.pod.uid`. |
| What it grants | The same value creates the Role that reads them: `get`/`list`/`watch` on pods and events, cluster-wide unless `rbac.namespaces` scopes it. Nothing in it writes. |

### What the chart carries

The chart is signed with cosign and carries no build record
([releases.md](internals/releases.md#what-is-signed-and-what-is-not) has the
per-artifact table):

```sh
cosign verify \
  --new-bundle-format=false \
  --certificate-oidc-issuer https://token.actions.githubusercontent.com \
  --certificate-identity-regexp 'github.com/TrianaLab/mira/.github/workflows/release.yml' \
  ghcr.io/trianalab/charts/mira-operator:0.3.0
```

The `MiraCluster` definition needs care: Kubernetes silently **drops** a field
its schema does not name, so a stale one loses data quietly. Apply it by hand
before any chart upgrade that changes it, because Helm installs `crds/` once
and never upgrades it.

Every value: [chart reference](reference/chart.md), which also has how a
scale-in is sequenced. Every field: [`MiraCluster`](reference/crd.md).

## Where it will refuse to start

Mira checks its data directory at startup and **refuses to start on a network
filesystem** — NFS, SMB, CephFS and friends. The error names the filesystem and
what to point `--data-dir` at instead. FUSE gets a warning rather than a
refusal.

That rules out a shared (RWX) volume on Kubernetes: the operator always asks
for `ReadWriteOnce` and there is no field to change it.
[Configuration](config.md) has the topology that works instead.

## Check it runs

```sh
mira --version
mira --data-dir ./data
```

Then [Quickstart](quickstart.md) fills it and reads it back.
