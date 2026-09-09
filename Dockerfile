# Mira is one binary and one directory, so the image is one binary and one
# volume. Nothing to configure, nothing beside it to deploy.
FROM rust:1-slim AS build

# `cc` is for zstd-sys, which vendors its own C source (docs/ARCHITECTURE.md §0).
# No protoc: the OTLP protos are compiled by protox in mira-proto's build script.
RUN apt-get update \
    && apt-get install -y --no-install-recommends gcc libc6-dev \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /src
COPY . .
RUN cargo build --release -p mira

# `cc` rather than `static`: zstd is linked against glibc.
FROM gcr.io/distroless/cc-debian12
COPY --from=build /src/target/release/mira /mira

# 4317 is OTLP/gRPC. 4318 is OTLP/HTTP, the query API, MCP and the UI.
EXPOSE 4317 4318

# A named volume, not a bind mount from the host: Mira mmaps its blocks, and a
# bind mount on Docker Desktop is FUSE, where a hiccup is a SIGBUS rather than
# an error (§9). Mira warns about that at startup rather than refusing, because
# the magic number cannot tell a local FUSE mount from gcsfuse.
VOLUME /data

ENTRYPOINT ["/mira"]
CMD ["--data-dir", "/data", "--grpc", "0.0.0.0:4317", "--http", "0.0.0.0:4318"]
