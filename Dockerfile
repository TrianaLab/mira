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
# dependency in the tree (docs/ARCHITECTURE.md §0). No protoc: protox compiles
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
    cargo build --release --locked -p mira \
    && cp target/release/mira /mira

# Resolves to `compile` or `prebuilt`; everything below is identical either way.
FROM ${BIN} AS binary

# ---------------------------------------------------------------------------
# runtime
# ---------------------------------------------------------------------------
# distroless/cc rather than scratch: zstd-sys links against glibc, so the binary
# is dynamic. `ldd` on it lists libgcc_s, libm and libc and nothing else — no
# OpenSSL, because there is no TLS anywhere in the tree, and no CA bundle,
# because Mira never dials out. `cc` is therefore the whole runtime, and
# `static` would need a musl build Mira does not ship (see release.yml).
#
# The digest is the multi-platform index, not one manifest, so linux/amd64 and
# linux/arm64 both resolve from this single pin. Re-resolve it with:
#   docker buildx imagetools inspect gcr.io/distroless/cc-debian12:nonroot
FROM gcr.io/distroless/cc-debian12:nonroot@sha256:9dac0a79194e45a7da0158a9c6da57b217585af0786db3845d1f0ec1a0dd182f

ARG VERSION=0.0.0-dev
ARG REVISION=unknown
LABEL org.opencontainers.image.title="mira" \
      org.opencontainers.image.description="OTLP-native telemetry storage engine in a single binary" \
      org.opencontainers.image.source="https://github.com/TrianaLab/mira" \
      org.opencontainers.image.url="https://github.com/TrianaLab/mira" \
      org.opencontainers.image.documentation="https://github.com/TrianaLab/mira/blob/main/README.md" \
      org.opencontainers.image.vendor="TrianaLab" \
      org.opencontainers.image.licenses="Apache-2.0" \
      org.opencontainers.image.version="${VERSION}" \
      org.opencontainers.image.revision="${REVISION}"

COPY --from=binary /mira /mira

# uid 65532, from the base image's :nonroot tag, which is also where the
# /etc/passwd entry comes from. Set before WORKDIR on purpose: BuildKit creates
# a missing WORKDIR owned by the current USER, which is the only way to get a
# writable /data into a stage that has no shell to chown with.
USER nonroot:nonroot
WORKDIR /data

# 4317 is OTLP/gRPC. 4318 is OTLP/HTTP, and also the query API, MCP and the UI.
EXPOSE 4317 4318

# A named volume, not a bind mount from the host: Mira mmaps its blocks, and a
# bind mount on Docker Desktop is FUSE, where a hiccup arrives as SIGBUS rather
# than as an error (§9). Mira warns about that at startup rather than refusing,
# because the magic number cannot tell a local FUSE mount from gcsfuse.
VOLUME /data

# No HEALTHCHECK. Mira answers /health and /readyz on 4318 (main.rs
# `health_router`), but distroless has no shell and no curl, and Mira has no
# subcommand that makes an HTTP request — `mira mira --addr` is a TUI and wants
# a pty. Inventing a probe here would mean either a shell in the image or a
# curl binary beside the one binary that is the product. Point Kubernetes or
# compose at http://<host>:4318/readyz instead; that is the same check without
# the extra 5 MB.

ENTRYPOINT ["/mira"]
CMD ["--data-dir", "/data", "--grpc", "0.0.0.0:4317", "--http", "0.0.0.0:4318"]
