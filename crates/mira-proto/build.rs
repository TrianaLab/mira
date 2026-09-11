//! Generates OTLP bindings from the vendored protos in `proto/`.
//!
//! Two deliberate choices, both load-bearing:
//!
//! 1. `protox` compiles the FileDescriptorSet in pure Rust, so building Mira
//!    never needs a `protoc` on PATH. Single binary, zero build deps.
//! 2. `.bytes(["."])` makes every `bytes` field decode as `bytes::Bytes`
//!    aliasing the gRPC frame buffer instead of a freshly allocated `Vec<u8>`.
//!    trace_id/span_id/parent_span_id are on the hottest path in the system;
//!    the upstream `opentelemetry-proto` crate does NOT do this, which is the
//!    main reason we vendor.

use std::path::PathBuf;

const PROTOS: &[&str] = &[
    "opentelemetry/proto/collector/logs/v1/logs_service.proto",
    "opentelemetry/proto/collector/trace/v1/trace_service.proto",
    "opentelemetry/proto/collector/metrics/v1/metrics_service.proto",
];

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("proto");
    println!("cargo:rerun-if-changed={}", root.display());

    let fds = protox::compile(PROTOS, [&root])?;

    let mut cfg = prost_build::Config::new();
    cfg.bytes(["."]);

    tonic_prost_build::configure()
        .build_client(false)
        .build_server(true)
        .compile_fds_with_config(fds, cfg)?;

    Ok(())
}
