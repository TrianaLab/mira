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
SLSA provenance attestation before moving the binary into place. That last step
is the one worth having: a checksum published beside an artifact proves the two
match, not that either came from this repository.

| | |
|---|---|
| `--version v0.0.2` | a specific release instead of the latest |
| `--no-sudo` | never escalate; fails instead if the directory is not writable |
| `--no-verify` | skip the attestation check (the checksum is still enforced) |
| `MIRA_INSTALL_DIR` | where it lands; default `/usr/local/bin`, and it must already exist |
| `GH_TOKEN` | avoids the anonymous 60-requests-an-hour limit on the version lookup |

Piping a script from the internet into a shell is a decision, not a default.
[Read it first](https://miradb.dev/install.sh) — that URL is not a copy of the
script, it *is* the script, so what you read is byte-for-byte what runs.

## Updating

```sh
mira update              # to the latest release
mira update --dry-run    # print the command it would run, and stop
mira update --version v0.0.2
```

This runs the installer above rather than re-implementing it, so the checksum
and the attestation are checked by exactly the code that checks them on a first
install — one code path, not two that drift. The difference is where it lands:
`mira update` installs over **this binary's own directory**, not
`/usr/local/bin`, so a mira in `~/.local/bin` is replaced rather than shadowed
by a second copy whose precedence depends on `PATH` order. `MIRA_INSTALL_DIR`
still wins if it is set.

There is no `--check`: the installer stops with `mira vX is already installed`
when the running version is the one that would be installed, so running it *is*
the check. If you installed from a package manager or from source, keep using
that instead — this replaces a file, and it does not know what put it there.

## With cargo

```sh
cargo install --locked miradb                    # -> ~/.cargo/bin/mira
```

The crate is `miradb` and the binary it installs is `mira`: `mira` on crates.io
is an unrelated crate that has been there since 2024. Two libraries are
published beside it for anyone embedding the engine rather than running it —
[`miradb-core`](https://docs.rs/miradb-core) is the encoder, block writer and
mmap reader, [`miradb-proto`](https://docs.rs/miradb-proto) is the OTLP
bindings.

## From source

The whole prerequisite list is **Rust 1.85 or newer** and a `cc`, which
`zstd-sys` needs to compile the C source it vendors — the linker already
required one, so the practical delta is a vendored compile rather than a new
thing to install.

Nothing else. No `protoc`: the OTLP protos are compiled by `protox` in a build
script. No node toolchain: the browser UI is built and committed under
`crates/mira/ui/dist`.

```sh
git clone https://github.com/TrianaLab/mira && cd mira
cargo install --locked --path crates/mira        # -> ~/.cargo/bin/mira
cargo build --release                            # or: ./target/release/mira
```

## From a release

From the first tag on, the release workflow publishes a stripped binary for
linux and macOS on x86_64 and arm64, a CycloneDX SBOM, a `SHA256SUMS` and one
SLSA provenance attestation covering every file in it.

```sh
V=0.0.2; T=x86_64-unknown-linux-gnu
base=https://github.com/TrianaLab/mira/releases/download/v$V
curl -sSLO $base/mira-$V-$T.tar.gz -O $base/SHA256SUMS
sha256sum -c SHA256SUMS --ignore-missing
tar -xzf mira-$V-$T.tar.gz --strip-components=1 mira-$V-$T/mira
gh attestation verify mira --repo TrianaLab/mira
```

`sha256sum -c` proves the bytes are the ones the release lists. `gh attestation
verify` is the one that matters: it proves those bytes came out of a workflow
run in this repository, which a checksum published next to the artifact cannot.

The linux builds come off `ubuntu-22.04`, so the glibc floor is **2.34** — RHEL
9, Amazon Linux 2023, Debian 12, Ubuntu 22.04+ and the
`distroless/base-nossl-debian12` base the image uses. That is a measurement, not
a hope: `make glibc-floor` reads the highest `GLIBC_` symbol version the binary
actually references and fails the build above 2.34, so a runner image that moves
under us is a red pull request rather than a binary that will not start. There is deliberately no
musl build: it compiles, but
musl's mallocng costs the ingest path more than the Alpine coverage is worth,
and fixing that means linking jemalloc and giving up "`zstd-sys` is the one C
dependency". On Alpine, build from source.

## Docker

**8.6 MB compressed** (`linux/arm64`, measured off the OCI export), of which
2.9 MB is Mira's own layer — a 6.6 MB binary, and the one thing here that has a
reason to grow. The base is
`distroless/base-nossl-debian12:nonroot` plus one copied `libgcc_s.so.1`, which
is the complete set of things the binary's three `NEEDED` entries require — no
OpenSSL, no libstdc++, no shell, no package manager. Published images carry the
same bytes as the tarball rather than a second compile, so the digest `gh
attestation verify` checks is about one artifact.

```sh
docker run -p 4317:4317 -p 4318:4318 -v mira-data:/data ghcr.io/trianalab/mira:latest
```

Or build it locally, which is what to do until there is a tag:

```sh
docker build -t mira .
docker run -p 4317:4317 -p 4318:4318 -v mira-data:/data mira
```

!!! warning "Use a named volume, not a bind mount"

    Mira `mmap`s its blocks, and a bind mount on Docker Desktop is FUSE — where
    an I/O hiccup arrives as `SIGBUS`, a signal rather than an error, with
    nothing to catch.

    The image declares no `VOLUME`, deliberately: Kubernetes ignores the
    directive, and on Docker it silently creates an anonymous volume on every
    `docker run` without `-v`. Mount one explicitly and you know what you have.

### Health checks

The image has no `HEALTHCHECK` — distroless carries no shell and no `curl`, and
adding either to run a probe would roughly double it. Mira answers both probes
itself on 4318:

```yaml
readinessProbe:
  httpGet: { path: /readyz, port: 4318 }
livenessProbe:
  httpGet: { path: /health, port: 4318 }
```

Neither touches the block directory, so a slow disk does not fail a probe.
`/health` additionally reports per-signal shed and failure counts.

## Kubernetes

The chart is [`charts/mira`](reference/chart.md) in the repository, published to
the same registry as the image, as an OCI artifact:

```sh
helm install mira oci://ghcr.io/trianalab/charts/mira \
  --version 0.0.2 --namespace observability --create-namespace
```

That is a StatefulSet of one, a PVC, and one Service carrying both ports. No
operator, no sidecar, no CRDs and nothing to elect: Mira holds no coordination
state, so the chart has nothing to coordinate. Configuration is the same KYAML
document as everywhere else, rendered into a ConfigMap — the `config.*` values
are [Configuration](config.md)'s keys, camelCased per Helm convention
(`ingest.max_request_bytes` is `config.ingest.maxRequestBytes`), and how much
CPU, memory and disk to give it is [that page's sizing
table](config.md#sizing), every row anchored to a measured point.

The chart is signed the same way the binaries are:

```sh
cosign verify \
  --new-bundle-format=false \
  --certificate-oidc-issuer https://token.actions.githubusercontent.com \
  --certificate-identity-regexp 'github.com/TrianaLab/mira/.github/workflows/release.yml' \
  ghcr.io/trianalab/charts/mira:0.0.2
```

The [chart reference](reference/chart.md) has every value, why it defaults where
it does, and the argument for a StatefulSet.
It is also listed on [Artifact Hub](https://artifacthub.io/packages/helm/mira/mira),
which renders that README, the signature above and the image's current CVE
report against the same coordinate. Every value is covered by a closed
`values.schema.json`, so `helm install` rejects a typo'd key before the cluster
sees it — the same rule Mira's own config file follows.

## Where it will refuse to start

For the same reason, Mira `statfs`es its data directory at startup and **refuses
to start on a network filesystem** — NFS, SMB, CephFS and friends. The error
names the filesystem it found and says what to point `--data-dir` at instead.
FUSE is a warning rather than a refusal, because the magic number cannot tell
`gcsfuse` from a local one.

That rules out an RWX PVC on Kubernetes — which is why the chart's
`persistence.accessMode` offers only the two ReadWriteOnce modes, and why its
`persistence.storageClass` should name a block-backed class.
[Configuration](config.md) has the topology that works instead.

## Check it runs

```sh
mira --version
mira --data-dir ./data
```

Then [Quickstart](quickstart.md) fills it and reads it back.
