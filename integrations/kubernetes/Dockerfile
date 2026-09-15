# syntax=docker/dockerfile:1@sha256:ecfaec9ed6d810b56388c508f4121597bfbba70d41a6dfeee4d8cad5f295fc32

# The operator's image, and deliberately not the engine's.
#
# The root Dockerfile takes prebuilt binaries out of the release matrix: the
# image then carries the exact bytes that were checksummed and attested, so
# there is one artifact and one provenance statement. The operator has no
# matrix — it is not in the tarballs, it is not on crates.io, and it is not
# something anyone downloads and runs on a laptop. It only ever exists as a
# container, so cross-compiling it here from source is the whole story rather
# than a second, unverified copy of one.
#
# The cost is real and it is the reason this file is separate: a multi-platform
# build of this one *does* need QEMU, because the compile stage runs a command.
# `.github/workflows/release.yml` builds the two platforms as two native jobs
# instead and joins them with `buildx imagetools create`, so QEMU never runs
# there either. A plain `docker build` here is single-platform and native.
#
# Build from the repository root:
#   docker build -f integrations/kubernetes/Dockerfile -t mira-operator .
# The context is the root, not integrations/kubernetes/, because Cargo.lock and
# the nested workspace both have to be in it.

# ---------------------------------------------------------------------------
# compile
# ---------------------------------------------------------------------------
FROM rust:1-slim-bookworm@sha256:ebd900bae66fd508b466cef82d64a83a5fb34682e4c8b2797a42908bddc95a57 AS compile

# No `cc`. The engine needs it for zstd-sys; the operator's tree has no C
# dependency at all — kube-rs is on rustls with the `ring` backend, which
# vendors its own assembly and builds with rustc alone. If this line ever has
# to come back, something pulled in openssl-sys and the fix is the feature
# flag, not the package.

WORKDIR /src
COPY . .

WORKDIR /src/integrations/kubernetes

# `--locked` against the nested workspace's own Cargo.lock. That lock file is
# committed for exactly this reason: an operator that resolves a different kube
# version in the release build than in CI is an operator whose API compatibility
# was never actually tested.
#
# Cache mounts so a rebuild after a one-line change is not a cold build. The
# target dir is the mount, so it does not survive the layer: the binary has to
# be copied out inside the same RUN.
RUN --mount=type=cache,target=/usr/local/cargo/registry,sharing=locked \
    --mount=type=cache,target=/src/integrations/kubernetes/target,sharing=locked \
    cargo build --release --locked --bin mira-operator \
    && cp target/release/mira-operator /mira-operator

# ---------------------------------------------------------------------------
# runtime
# ---------------------------------------------------------------------------
# The same base as the engine, pinned to the same digests, for the same reason:
# this is still a dynamically linked glibc binary. Having no C dependency
# removes zstd-sys, not libc — `readelf -d` still lists libc, libm and libgcc_s.
# `distroless/static` would therefore not boot, and getting there through musl
# would swap the allocator under a long-running controller for no measured
# reason. TLS is rustls with webpki roots compiled in, so there is no CA bundle
# here either.
#
# Both digests are multi-platform indexes, not single manifests, so linux/amd64
# and linux/arm64 resolve from these two pins. Re-resolve with:
#   docker buildx imagetools inspect gcr.io/distroless/cc-debian12:nonroot
FROM gcr.io/distroless/cc-debian12:nonroot@sha256:9dac0a79194e45a7da0158a9c6da57b217585af0786db3845d1f0ec1a0dd182f AS libgcc

FROM gcr.io/distroless/base-nossl-debian12:nonroot@sha256:be40c00dfabd86576d92666e87e406714d5618342de1a0c213ad232de255172e

ARG VERSION=0.0.0-dev
ARG REVISION=unknown
LABEL org.opencontainers.image.title="mira-operator" \
      org.opencontainers.image.description="Kubernetes controller that sizes a Mira tier on free disk and drains a replica to cold storage before removing it" \
      org.opencontainers.image.source="https://github.com/TrianaLab/mira" \
      org.opencontainers.image.url="https://miradb.dev" \
      org.opencontainers.image.documentation="https://miradb.dev" \
      org.opencontainers.image.vendor="TrianaLab" \
      org.opencontainers.image.licenses="Apache-2.0" \
      org.opencontainers.image.version="${VERSION}" \
      org.opencontainers.image.revision="${REVISION}"

# Same reasoning as the engine's image: `panic = "abort"` does not remove the
# _Unwind_* NEEDED entry, and base-nossl is the only distroless variant that
# does not carry it.
COPY --from=libgcc /lib/*-linux-gnu/libgcc_s.so.1 /lib/
COPY --from=compile /mira-operator /mira-operator

# uid 65532, from the base image's :nonroot tag. No WORKDIR and no writable
# directory anywhere: the chart sets readOnlyRootFilesystem, and a controller
# that wanted scratch space would be a controller holding state.
USER nonroot:nonroot

# No port. The operator serves nothing — it is a client of the API server and
# of each replica's /api/v1/stats, and it is never dialled. No /metrics either:
# Mira's own answer to "how do I see this" is OTLP, and a controller that
# exported Prometheus would be the one component in the tree that did not.
#
# ponytail: that means the only way to see why it is not scaling is the log.
# The upgrade path is OTLP export through the same `telemetry.self` switch the
# engine has, the day someone is actually operating one of these.

ENTRYPOINT ["/mira-operator"]
