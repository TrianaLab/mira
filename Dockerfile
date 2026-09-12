# syntax=docker/dockerfile:1@sha256:ecfaec9ed6d810b56388c508f4121597bfbba70d41a6dfeee4d8cad5f295fc32

# Mira is one binary and one directory, so the image is one binary and one
# volume. Nothing to configure, nothing beside it to deploy.
#
# There are two ways in and one runtime stage:
#
#   docker build -t mira .                          compiles from source
#   docker build --build-arg BIN=prebuilt ...       reuses dist/linux/<arch>/mira
#
# .github/workflows/release.yml takes the second. That is not only speed: the
# bytes in the published image are then byte-identical to the bytes in the
# tarball that was checksummed and attested, so there is one artifact and one
# provenance statement rather than a second compile nobody verified. The first
# is what a contributor types, and it has to keep working — hence both.
ARG BIN=compile

# ---------------------------------------------------------------------------
# prebuilt — release.yml unpacks the matrix tarballs into dist/linux/<arch>/
# ---------------------------------------------------------------------------
# TARGETARCH is amd64 or arm64, set by buildx per platform. Because this stage
# and the runtime stage below run no commands, a multi-platform build needs no
# QEMU: buildx is only relabelling and copying files the host already has.
#
# BuildKit prunes stages nothing references, so a plain `docker build .` never
# looks at this one and a missing dist/ is not an error.
FROM scratch AS prebuilt
ARG TARGETARCH
COPY dist/linux/${TARGETARCH}/mira /mira

# ---------------------------------------------------------------------------
# compile — from source, the way the README says to
# ---------------------------------------------------------------------------
FROM rust:1-slim-bookworm@sha256:ebd900bae66fd508b466cef82d64a83a5fb34682e4c8b2797a42908bddc95a57 AS compile

# `cc` is for zstd-sys, which vendors its own C source and is the only C
# dependency in the tree (docs/architecture.md section 0). No protoc: protox compiles
# the OTLP protos in mira-proto's build script. No node: crates/mira/ui/dist is
# committed and `include_bytes!`d.
RUN apt-get update \
    && apt-get install -y --no-install-recommends gcc libc6-dev \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /src
COPY . .

# ponytail: `cargo` rather than `make build`, which is the source of truth
# everywhere else. Reaching it from here costs make *and* python3 in the build
# image — the Makefile shells out to python3 for the MSRV at parse time — to
# run one command that is already spelled out below. Wire the Makefile in if a
# second flag ever appears here.
#
# Cache mounts so a rebuild after a one-line change is not a cold fat-LTO
# build. /src/target is the mount, so it does not survive the layer: the binary
# has to be copied out inside the same RUN.
RUN --mount=type=cache,target=/usr/local/cargo/registry,sharing=locked \
    --mount=type=cache,target=/src/target,sharing=locked \
    cargo build --release --locked -p miradb \
    && cp target/release/mira /mira

# Resolves to `compile` or `prebuilt`; everything below is identical either way.
FROM ${BIN} AS binary

# ---------------------------------------------------------------------------
# libgcc — the one library base-nossl does not carry
# ---------------------------------------------------------------------------
# `panic = "abort"` does not remove this: the NEEDED entry survives, because
# std's backtrace and __rust_start_panic paths still reference _Unwind_*.
# Dropping it for real needs -Z build-std on nightly, which is not a trade worth
# making for 100 KB.
#
# Taken from `cc` rather than from the compile stage because the prebuilt path
# (release.yml) has no Debian stage at all — its `binary` is FROM scratch. This
# stage runs no command, so a multi-platform build still needs no QEMU, and
# buildx resolves the right architecture out of the same multi-platform index.
FROM gcr.io/distroless/cc-debian12:nonroot@sha256:9dac0a79194e45a7da0158a9c6da57b217585af0786db3845d1f0ec1a0dd182f AS libgcc

# ---------------------------------------------------------------------------
# runtime
# ---------------------------------------------------------------------------
# zstd-sys links against glibc, so the binary is dynamic. `readelf -d` lists
# exactly three NEEDED entries — libc, libm, libgcc_s — so those three plus the
# loader are the entire runtime. `base-nossl` supplies the first two and the
# loader; the third is copied in below, which is the whole reason this is not
# `distroless/static`.
#
# Not `cc`, which is what this used to be: `cc` additionally ships libssl3,
# libcrypto3, libstdc++6, libgomp1 and the gconv charset modules, none of which
# anything here dlopens. Measured, on this machine: 11.85 MB -> 8.59 MB
# compressed, and 29 Trivy findings -> 18, with 7 of the 11 removed being
# OpenSSL. Carrying the most CVE-churned package in Debian for a library with no
# consumer means being paged for it forever. Mira has no CA bundle either, and
# does not need one — it *does* dial out (alert webhooks, alert.rs), but the
# optional `webhook-tls` feature is rustls with `with_webpki_roots()`, so the
# roots are compiled into the binary.
#
# Not `scratch` (4.11 MB, and it boots): a scratch image has no package
# database, so Trivy reports zero findings because it can see nothing, not
# because the libc CVEs are gone. That converts a gate that can fail into one
# that cannot. Not `crt-static` either — rustc has no static-pie for
# *-unknown-linux-gnu, so it costs ASLR on a process that parses untrusted OTLP
# off the network.
#
# The digest is the multi-platform index, not one manifest, so linux/amd64 and
# linux/arm64 both resolve from this single pin. Re-resolve it with:
#   docker buildx imagetools inspect gcr.io/distroless/base-nossl-debian12:nonroot
FROM gcr.io/distroless/base-nossl-debian12:nonroot@sha256:be40c00dfabd86576d92666e87e406714d5618342de1a0c213ad232de255172e

ARG VERSION=0.0.0-dev
ARG REVISION=unknown
LABEL org.opencontainers.image.title="mira" \
      org.opencontainers.image.description="OTLP-native telemetry storage engine and short-term memory layer for AI agents" \
      org.opencontainers.image.source="https://github.com/TrianaLab/mira" \
      org.opencontainers.image.url="https://miradb.dev" \
      org.opencontainers.image.documentation="https://miradb.dev" \
      org.opencontainers.image.vendor="TrianaLab" \
      org.opencontainers.image.licenses="Apache-2.0" \
      org.opencontainers.image.version="${VERSION}" \
      org.opencontainers.image.revision="${REVISION}"

# The glob is the multiarch triple directory (`aarch64-linux-gnu` or
# `x86_64-linux-gnu`), so this line is architecture-independent. It lands in
# /lib rather than back in the triple directory: /lib is a default search path
# built into the loader, so it resolves after the ld.so.cache miss without the
# Dockerfile having to know which triple it is on.
COPY --from=libgcc /lib/*-linux-gnu/libgcc_s.so.1 /lib/
COPY --from=binary /mira /mira

# uid 65532, from the base image's :nonroot tag, which is also where the
# /etc/passwd entry comes from. Set before WORKDIR on purpose: BuildKit creates
# a missing WORKDIR owned by the current USER, which is the only way to get a
# writable /data into a stage that has no shell to chown with.
USER nonroot:nonroot
WORKDIR /data

# 4317 is OTLP/gRPC. 4318 is OTLP/HTTP, and also the query API, MCP and the UI.
EXPOSE 4317 4318

# No VOLUME. It is the one declaration here that costs something: Kubernetes
# ignores it outright, and on Docker it creates an anonymous volume on every run
# without -v, which accumulates. Mount one yourself, and make it a named volume
# rather than a host bind mount — Mira mmaps its blocks and a bind mount on
# Docker Desktop is FUSE, where a hiccup arrives as SIGBUS rather than an error.
# That reasoning, and the probe configuration below, are in docs/install.md.

# No HEALTHCHECK: distroless has no shell and no curl, and adding either to run
# a probe would double the image. Point Kubernetes or compose at
# http://<host>:4318/readyz — same check, no extra bytes.

ENTRYPOINT ["/mira"]
# Only --data-dir is load-bearing. The bind addresses are already
# 0.0.0.0:4317/4318 by default (config.rs), and restating them here is not free:
# any `docker run mira <flag>` replaces the whole CMD, so the redundancy would
# silently take the addresses away with it.
CMD ["--data-dir", "/data"]
