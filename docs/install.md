---
description: Build Mira from source with cargo, or run the container. Rust 1.85 and a cc — no protoc, no node toolchain.
---

# Install

Nothing is tagged yet, so building it is the only path that works today. The
whole prerequisite list is **Rust 1.85 or newer** and a `cc`, which `zstd-sys`
needs to compile the C source it vendors — the linker already required one, so
the practical delta is a vendored compile rather than a new thing to install.

Nothing else. No `protoc`: the OTLP protos are compiled by `protox` in a build
script. No node toolchain: the browser UI is built and committed under
`crates/mira/ui/dist`.

## From source

```sh
git clone https://github.com/TrianaLab/mira && cd mira
cargo install --locked --path crates/mira        # -> ~/.cargo/bin/mira
```

Or build without installing:

```sh
cargo build --release                            # -> ./target/release/mira
```

## From a release

From the first tag on, the release workflow publishes a stripped binary for
linux and macOS on x86_64 and arm64, a CycloneDX SBOM, a `SHA256SUMS` and one
SLSA provenance attestation covering every file in it.

```sh
V=0.1.0; T=x86_64-unknown-linux-gnu
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
9, Amazon Linux 2023, Debian 12, Ubuntu 22.04+ and the `distroless/cc-debian12`
base the image uses. There is deliberately no musl build: it compiles, but
musl's mallocng costs the ingest path more than the Alpine coverage is worth,
and fixing that means linking jemalloc and giving up "`zstd-sys` is the one C
dependency". On Alpine, build from source.

## Docker

The image is one binary on `distroless/cc` and one volume. Published images
carry the same bytes as the tarball rather than a second compile, so the digest
`gh attestation verify` checks is about one artifact.

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

## Where it will refuse to start

For the same reason, Mira `statfs`es its data directory at startup and **refuses
to start on a network filesystem** — NFS, SMB, CephFS and friends. The error
names the filesystem it found and says what to point `--data-dir` at instead.
FUSE is a warning rather than a refusal, because the magic number cannot tell
`gcsfuse` from a local one.

That rules out an RWX PVC on Kubernetes; [Configuration](CONFIG.md) has the
topology that works instead.

## Check it runs

```sh
mira --version
mira --data-dir ./data
```

Then [Quickstart](quickstart.md) fills it and reads it back.
