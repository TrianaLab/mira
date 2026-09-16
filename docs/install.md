---
description: Build Mira from source with cargo, or run the container. Rust 1.85 and a cc — no protoc, no node toolchain.
---

# Install

**For:** anyone who wants the binary on their machine. Five minutes, one prerequisite.

## The one-liner

```sh
curl -fsSL https://miradb.dev/install.sh | bash
```

It resolves the latest release, picks the tarball for your platform, checks it
against the published `SHA256SUMS`, and — if `gh` is on the path — verifies the
SLSA provenance attestation before moving the binary into place.

| | |
| --- | --- |
| `--version v0.2.0` | a specific release instead of the latest |
| `--no-sudo` | never escalate; fails instead if the directory is not writable |
| `--no-verify` | skip the attestation check (the checksum is still enforced) |
| `MIRA_INSTALL_DIR` | where it lands; default `/usr/local/bin`, and it must already exist |
| `GH_TOKEN` | avoids the anonymous 60-requests-an-hour limit on the version lookup |

[Read it first](https://miradb.dev/install.sh) — that URL is not a copy of the
script, it *is* the script.

## Updating

```sh
mira update              # to the latest release
mira update --dry-run    # print the command it would run, and stop
mira update --version v0.2.0
```

This runs the installer above, so the checksum and the attestation take the same
code path as a first install. It installs over **this binary's own
directory**, not `/usr/local/bin`, so a mira in `~/.local/bin` is replaced
rather than shadowed. `MIRA_INSTALL_DIR` still wins if it is set.

The installer stops with `mira vX is already installed` when the running version
is the one that would be installed. If you installed from a package manager or
from source, keep using that: this replaces a file and does not know what put it
there.

## With cargo

```sh
cargo install --locked miradb                    # -> ~/.cargo/bin/mira
```

The crate is `miradb` and the binary it installs is `mira`: `mira` on crates.io
is an unrelated crate from 2024. Two libraries are published beside it for
embedding the engine — [`miradb-core`](https://docs.rs/miradb-core) is the
encoder, block writer and mmap reader,
[`miradb-proto`](https://docs.rs/miradb-proto) is the OTLP bindings.

## From source

The prerequisites are **Rust 1.85 or newer** and a `cc`, which
`zstd-sys` needs to compile the C source it vendors. Nothing else: no `protoc`,
because the OTLP protos are compiled by `protox` in a build script, and no node
toolchain, because the browser UI is built and committed under
`crates/mira/ui/dist`.

```sh
git clone https://github.com/TrianaLab/mira && cd mira
cargo install --locked --path crates/mira        # -> ~/.cargo/bin/mira
cargo build --release                            # or: ./target/release/mira
```

## From a release

The release workflow publishes a stripped binary for linux and macOS on x86_64
and arm64, a CycloneDX SBOM, a `SHA256SUMS` and one SLSA provenance attestation
covering every file in it.

```sh
V=0.2.0; T=x86_64-unknown-linux-gnu
base=https://github.com/TrianaLab/mira/releases/download/v$V
curl -sSLO $base/mira-$V-$T.tar.gz -O $base/SHA256SUMS
sha256sum -c SHA256SUMS --ignore-missing
tar -xzf mira-$V-$T.tar.gz --strip-components=1 mira-$V-$T/mira
gh attestation verify mira --repo TrianaLab/mira
```

`sha256sum -c` proves the bytes are the ones the release lists; `gh attestation
verify` proves those bytes came out of a workflow run in this repository.

The linux builds come off `ubuntu-22.04`, so the glibc floor is **2.34** — RHEL
9, Amazon Linux 2023, Debian 12, Ubuntu 22.04+. `make glibc-floor` reads the
highest `GLIBC_` symbol version the binary references and fails the build above
it. There is no musl build: it compiles, but musl's mallocng costs the
ingest path more than the Alpine coverage is worth. On Alpine, build from source.

## Docker

**8.6 MB compressed** (`linux/arm64`, measured off the OCI export), of which
2.9 MB is Mira's own layer — a 6.6 MB binary. The base is
`distroless/base-nossl-debian12:nonroot` plus one copied `libgcc_s.so.1` — no
OpenSSL, no libstdc++, no shell, no package manager. Published images carry the
same bytes as the tarball rather than a second compile.

```sh
docker run -p 4317:4317 -p 4318:4318 -v mira-data:/data ghcr.io/trianalab/mira:latest
```

Or build it locally:

```sh
docker build -t mira .
docker run -p 4317:4317 -p 4318:4318 -v mira-data:/data mira
```

!!! warning "Use a named volume, not a bind mount"

    Mira `mmap`s its blocks, and a bind mount on Docker Desktop is FUSE — where
    an I/O hiccup arrives as `SIGBUS`, a signal rather than an error, with
    nothing to catch.

    The image declares no `VOLUME`: Kubernetes ignores the directive, and on
    Docker it silently creates an anonymous volume on every `docker run`
    without `-v`.

### Health checks

The image has no `HEALTHCHECK` — distroless carries no shell and no `curl`.
Mira answers both probes itself on 4318:

```yaml
readinessProbe:
  httpGet: { path: /readyz, port: 4318 }
livenessProbe:
  httpGet: { path: /health, port: 4318 }
```

Neither touches the block directory, so a slow disk does not fail a probe.
`/health` additionally reports per-signal shed and failure counts.

## Kubernetes

One Mira needs no chart. The [container above](#docker) is the whole deployment:
one image, one volume, two ports. On Kubernetes that is a Deployment — or a
StatefulSet, if the volume is to outlive a reschedule.

What has no hand-written answer is a **tier**: several storage nodes, a volume
each, a `mira proxy` in front of them, and the decision of when there should be
one more. That is what the operator is for: the only chart here installs a
**controller** rather than Mira, from the same registry as the image:

```sh
helm install mira-operator oci://ghcr.io/trianalab/charts/mira-operator \
  --version 0.1.1 --namespace mira-system --create-namespace
```

That is a Deployment of exactly one, a ServiceAccount, a ClusterRole with no
wildcards, and the `MiraCluster` CRD.

### The tier

```yaml
apiVersion: mira.miradb.dev/v1alpha1
kind: MiraCluster
metadata:
  name: telemetry
  namespace: observability
spec:
  image: ghcr.io/trianalab/mira:0.2.0
  replicas: 1          # floor
  maxReplicas: 5       # ceiling; there is no "unbounded"
  storage: { size: 50Gi, className: gp3 }
  resources:                    # unset is BestEffort; the drain inherits this
    requests: { cpu: 2500m, memory: 2048Mi }
  offload: "file:///cold/${node}"
  coldStorageClaim: mira-cold   # must already exist; see below
  proxy: { replicas: 2, resources: { requests: { cpu: 500m, memory: 512Mi } } }
```

`offload` and `coldStorageClaim` are a pair: the operator refuses a spec with
one and not the other, and a tier with neither still scales out, and never in.
`file://` is the only scheme Mira's offload target parses, so the archive is a
*mount* — a drain Job with no claim at `/cold` writes it into its own container,
and the operator deletes the volume it believes it archived.

`kubectl apply` that and the operator builds a StatefulSet, a PVC per replica, a
headless Service publishing not-ready addresses so a replica whose volume filled
stays reachable by name, a `mira proxy` Deployment behind a ClusterIP, both
ConfigMaps on every reconcile, and a PodDisruptionBudget of `maxUnavailable: 1`,
because a replica's blocks are the only copy. What to put in `resources` is
[Configuration's sizing table](config.md#sizing).

Every pod it builds satisfies the `restricted` Pod Security Standard unmodified:
uid 65532, `fsGroup` set, no service-account token, `seccompProfile:
RuntimeDefault`. A replica gets 60 seconds to seal on SIGTERM and a startup
probe worth five minutes, because it replays its log before it answers anything
and a liveness probe alone would kill it part-way through, for ever.

!!! note "A chart that installed a StatefulSet used to exist"

    Three things it could do have **no `MiraCluster` equivalent yet**: an
    Ingress, a ServiceAccount per tier, and arbitrary `config.*` keys. Write the
    Ingress yourself against the proxy Service; the tier's pods run as
    `default`.

### What the chart carries

The chart carries a cosign signature over its digest and no SLSA provenance,
where the tarballs carry `SHA256SUMS` with one provenance attestation and no
cosign signature
([releases.md](internals/releases.md#what-is-signed-and-what-is-not) has the
per-artifact table):

```sh
cosign verify \
  --new-bundle-format=false \
  --certificate-oidc-issuer https://token.actions.githubusercontent.com \
  --certificate-identity-regexp 'github.com/TrianaLab/mira/.github/workflows/release.yml' \
  ghcr.io/trianalab/charts/mira-operator:0.1.1
```

The [chart reference](reference/chart.md) has every value, how a scale-in is
sequenced, and what the operator's lease guarantees. Every value the chart
itself owns — `image`, `rbac`, `serviceAccount`, `replicaCount`, `logLevel` — is
covered by a `values.schema.json` closed at each of those levels, so `helm
install` rejects a typo'd key. The Kubernetes
pass-throughs (`resources`, `securityContext`, `podSecurityContext`,
`nodeSelector`, `tolerations`, `affinity`) stay open.

The [`MiraCluster` fields](reference/crd.md) get the same treatment, and the
stakes are higher: the API server **prunes** a field the CRD does not name
rather than rejecting it, so a stale CRD is silent data loss. So the CRD is
generated from the Rust types and `make operator-crd-check` fails the build when
the two disagree — and a chart upgrade that changes the schema needs the CRD
applied by hand first, since Helm installs `crds/` once and never upgrades it.

## Where it will refuse to start

Mira `statfs`es its data directory at startup and **refuses to start on a
network filesystem** — NFS, SMB, CephFS and friends. The error names the
filesystem and what to point `--data-dir` at instead.
FUSE is a warning rather than a refusal, because the magic number cannot tell
`gcsfuse` from a local one.

That rules out an RWX PVC on Kubernetes: the operator's volume claim template is
hard-coded to `ReadWriteOnce` with no field to change it, and
`spec.storage.className` should name a block-backed class.
[Configuration](config.md) has the topology that works instead.

## Check it runs

```sh
mira --version
mira --data-dir ./data
```

Then [Quickstart](quickstart.md) fills it and reads it back.
