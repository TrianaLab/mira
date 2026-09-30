---
# The description is the search snippet and the shared-link preview: it says what
# the reader gets, in words that need no glossary.
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

It finds the latest release, downloads the build for your machine, checks it
against the published checksums, and — if `gh` is installed — checks that it
came out of a build in this repository. Then it moves the binary into place.

| | |
| --- | --- |
| `--version v0.4.2` | a specific release instead of the latest |
| `--no-sudo` | never escalate; fails instead if the directory is not writable |
| `--no-verify` | skip the build check (the checksum is still enforced) |
| `MIRA_INSTALL_DIR` | where it lands; default `/usr/local/bin`, and it must already exist |
| `GH_TOKEN` | avoids the anonymous 60-requests-an-hour limit on the version lookup |

[Read it first](https://miradb.dev/install.sh) — that URL is not a copy of the
script, it *is* the script.

## Updating

```sh
mira update              # to the latest release
mira update --dry-run    # print the command it would run, and stop
mira update --version v0.4.2
```

Same script, same checks. It replaces **the binary that is running**, wherever
that is, so a copy in `~/.local/bin` is replaced rather than shadowed.
`MIRA_INSTALL_DIR` still wins, and it stops with `mira vX is already installed`
when there is nothing to do. If a package manager or `cargo` put the binary
there, keep updating it that way: this replaces a file and does not know what
put it there.

## With cargo

```sh
cargo install --locked miradb                    # -> ~/.cargo/bin/mira
```

The crate is `miradb` and the binary it installs is `mira`: `mira` on crates.io
is an unrelated crate from 2024. Two libraries sit beside it, for putting the
engine inside your own program — [`miradb-core`](https://docs.rs/miradb-core)
writes and reads the files, [`miradb-proto`](https://docs.rs/miradb-proto)
speaks OpenTelemetry.

## From source

You need **Rust 1.88 or newer** and a C compiler, which one dependency,
`zstd-sys`, uses for the C source it ships. Nothing else: no `protoc`, and no
Node toolchain, because the browser UI is built and committed.

```sh
git clone https://github.com/TrianaLab/mira && cd mira
cargo install --locked --path crates/mira        # -> ~/.cargo/bin/mira
cargo build --release                            # or: ./target/release/mira
```

## From a release

Every release ships a stripped binary for Linux and macOS on x86_64 and arm64,
a `SHA256SUMS`, a list of everything that went into the build (a CycloneDX
SBOM), and one signed record of the build itself, covering every file.

```sh
V=0.4.2; T=x86_64-unknown-linux-gnu
base=https://github.com/TrianaLab/mira/releases/download/v$V
curl -sSLO $base/mira-$V-$T.tar.gz -O $base/SHA256SUMS
sha256sum -c SHA256SUMS --ignore-missing
gh attestation verify mira-$V-$T.tar.gz --repo TrianaLab/mira
tar -xzf mira-$V-$T.tar.gz --strip-components=1 mira-$V-$T/mira
```

`sha256sum -c` proves the bytes are the ones the release lists; `gh attestation
verify` proves they came out of a build in this repository.

Linux builds are made on `ubuntu-22.04`, so they need glibc **2.34** or newer —
RHEL 9, Amazon Linux 2023, Debian 12, Ubuntu 22.04 and up. `make glibc-floor`
reads the highest glibc symbol the binary asks for and fails the build above
that line, so the number here is checked rather than promised. There is no musl
build: it compiles, but musl's allocator costs ingest more than Alpine coverage
is worth. On Alpine, build from source.

## Docker

```sh
docker run -p 4317:4317 -p 4318:4318 -v mira-data:/data ghcr.io/trianalab/mira:latest
```

**8.6 MB compressed** (`linux/arm64`, measured off the OCI export), of which
2.9 MB is Mira's own layer — a 6.6 MB binary. The base is
`distroless/base-nossl-debian12:nonroot` — no shell, no package manager, no
OpenSSL, no libstdc++ — and Mira adds its binary and one copied system library,
`libgcc_s.so.1`. Published images carry the same bytes as the tarball rather
than a second compile.

Or build it locally:

```sh
docker build -t mira .
docker run -p 4317:4317 -p 4318:4318 -v mira-data:/data mira
```

!!! warning "Use a named volume, not a bind mount"

    Mira reads its files by mapping them into memory. A bind mount on Docker
    Desktop goes through FUSE, where a disk hiccup arrives as a kill signal
    rather than an error the process can catch.

    The image declares no `VOLUME`: Kubernetes ignores it, and Docker silently
    creates a throwaway volume on every `docker run` without `-v`.

### Health checks

The image has no `HEALTHCHECK`, because it carries no shell and no `curl`.
Mira answers both probes itself on 4318:

```yaml
readinessProbe:
  httpGet: { path: /readyz, port: 4318 }
livenessProbe:
  httpGet: { path: /health, port: 4318 }
```

Neither reads the data directory, so a slow disk does not fail a probe.
`/health` also reports how much was shed or failed, per signal.

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

That is a Deployment of exactly one, a ServiceAccount, a ClusterRole with no
wildcards, and the `MiraCluster` type. [See it on
Kubernetes](demo-cluster.md) puts all of it on a laptop in one command.

### The tier

```yaml
apiVersion: mira.miradb.dev/v1alpha1
kind: MiraCluster
metadata:
  name: telemetry
  namespace: observability
spec:
  image: ghcr.io/trianalab/mira:0.4.2
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

`offload` needs `coldStorageClaim`: the operator refuses a spec that sets
`offload` without it. A tier with neither still scales out, and never in.
`file://` is the only scheme the offload target understands, so the archive has
to be a mount.

`kubectl apply` that and the operator builds a StatefulSet, a PVC per replica,
a headless Service that keeps a full replica reachable by name, a `mira proxy`
behind a ClusterIP, both ConfigMaps on every reconcile, and a rule that only
one pod may be down at a time — a replica's files are the only copy. Sizes for
`resources` are in [Configuration](config.md#sizing).

`route` adds an `HTTPRoute` where the Gateway API is installed. Give it a
Gateway to attach to and the paths are worked out for you: ingest and queries
go to the proxy, everything else to the headless Service, where the UI is
answered. Remove the field and the route goes with it.

Every pod satisfies the `restricted` Pod Security Standard unmodified: uid
65532, `fsGroup` set, no service-account token, `seccompProfile:
RuntimeDefault`. A replica gets 60 seconds to close its files on SIGTERM and
five minutes to start, because it replays its log first.

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
| What it does | The operator watches Kubernetes Events and container state and ships both to that Mira as OTLP logs, keyed on `k8s.pod.uid` — an `OOMKilled` on the same timeline as the spans, for [an agent](agents.md) writing the RCA. |
| What it grants | The same value creates the Role that reads them: `get`/`list`/`watch` on pods and events, cluster-wide unless `rbac.namespaces` scopes it. Nothing in it writes, and there is no value that makes it write. |
| Unset | No watch is opened and no Role is created. That is the default. |

### What the chart carries

The chart is signed with cosign and carries no build record; the tarballs carry
`SHA256SUMS` and a build record and no cosign signature
([releases.md](internals/releases.md#what-is-signed-and-what-is-not) has the
per-artifact table):

```sh
cosign verify \
  --new-bundle-format=false \
  --certificate-oidc-issuer https://token.actions.githubusercontent.com \
  --certificate-identity-regexp 'github.com/TrianaLab/mira/.github/workflows/release.yml' \
  ghcr.io/trianalab/charts/mira-operator:0.3.0
```

`helm install` rejects a top-level value the chart does not define. How far in
the check goes varies from field to field.

The `MiraCluster` definition needs more care: Kubernetes silently **drops** a
field its schema does not name, so a stale one loses data quietly. The
definition is generated from the Rust types and `make operator-crd-check` fails
the build when the two disagree, which keeps the shipped one current — but
apply it by hand before any chart upgrade that changes it, because Helm
installs `crds/` once and never upgrades it.

Every value: [chart reference](reference/chart.md), which also has how a
scale-in is sequenced. Every field: [`MiraCluster`](reference/crd.md).

## Where it will refuse to start

Mira checks its data directory at startup and **refuses to start on a network
filesystem** — NFS, SMB, CephFS and friends. The error names the filesystem and
what to point `--data-dir` at instead. FUSE gets a warning rather than a
refusal, because Mira cannot tell `gcsfuse` from a local disk.

That rules out a shared (RWX) volume on Kubernetes: the operator always asks
for `ReadWriteOnce` and there is no field to change it, and
`spec.storage.className` should name a disk-backed class.
[Configuration](config.md) has the topology that works instead.

## Check it runs

```sh
mira --version
mira --data-dir ./data
```

Then [Quickstart](quickstart.md) fills it and reads it back.
