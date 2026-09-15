# 2. Workspace

```text
mira/
├── Cargo.toml                  # workspace, one pinned Arrow version
├── docs/architecture/          # this, one page per section
└── crates/
    ├── mira-proto/             # vendored .proto + codegen. No hand-written code.
    │   ├── build.rs            # protox (pure Rust) -> tonic-prost-build
    │   └── proto/opentelemetry/...
    ├── mira-core/              # the engine, as a library
    │   ├── schema.rs           # Arrow schemas == on-disk layout
    │   ├── logs.rs             # OTLP -> Arrow
    │   ├── block.rs            # publish, scan, expire, mmap read
    │   └── error.rs
    └── mira/                   # the binary
        ├── main.rs             # flags, listeners, supervision
        ├── receiver.rs         # tonic on 4317, axum on 4318
        └── pipeline.rs         # channel, flusher, retention worker
```

**Three crates, not the five in the brief.** Each boundary pays rent:
`mira-proto` isolates codegen and the protox blast radius; `mira-core` is what
benchmarks and integration tests link against; `mira` is a thin binary. A
`mira-storage`/`mira-core` split of zero LOC would buy no build parallelism —
cargo already parallelises codegen units within a crate — and would freeze the
public API boundary before anyone knows where it belongs. Split `mira-storage`
out the first time someone needs the block format without the OTLP encoder.
`mira-cli` is `main.rs` until the flag set outgrows twenty lines.

## Why the protos are vendored

The `opentelemetry-proto` crate is the obvious choice and the wrong one, for two
independent reasons:

1. Its codegen does not call `prost_build::Config::bytes(["."])`, so every
   `trace_id`, `span_id` and `AnyValue::BytesValue` decodes as a fresh `Vec<u8>`.
   That is one heap allocation per field per record on the hottest path.
2. It declares `opentelemetry` and `opentelemetry_sdk` as **non-optional**
   dependencies — `src/proto.rs` re-exports from `transform::common`, which uses
   them — so they cannot be feature-gated away. Measured cost: +12 crates,
   including a full SDK and `rand`, even with `--no-default-features`.

Vendoring costs 1,725 lines of `.proto` and a 36-line `build.rs`, and `protox`
compiles them in pure Rust, so building Mira never needs a `protoc` on `PATH`.
It is also unavoidable anyway the moment OTAP is in scope: the
`ArrowTracesService`/`ArrowLogsService` definitions are not in the
`opentelemetry-proto` crate's codegen input list.

## Arrow version pin

One Arrow version across the workspace, declared once in
`[workspace.dependencies]`. Two majors in one graph means
`arrow_58::RecordBatch` and `arrow_59::RecordBatch` are different types and the
resulting error is unreadable. This is also the reason Mira does not depend on
`otel-arrow-dfe-quiver` — see section 10.

---
