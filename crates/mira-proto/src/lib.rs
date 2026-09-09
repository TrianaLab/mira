//! OTLP wire types, generated at build time from the protos vendored under
//! `proto/` (upstream opentelemetry-proto v1.9.0).
//!
//! We do not depend on the `opentelemetry-proto` crate: it declares
//! `opentelemetry` and `opentelemetry_sdk` as non-optional dependencies (~12
//! extra crates including a full SDK and `rand`), and its codegen does not
//! enable `bytes = "bytes"`, so every trace_id would be a heap allocation.

#![forbid(unsafe_code)]
// Every doc comment under this crate root except the header above is a comment
// from upstream's .proto files, reproduced verbatim by prost. Upstream writes
// `<version>` and `<signal>` as prose placeholders, which rustdoc reads as
// unclosed HTML. The fix belongs upstream; rewriting generated code in a build
// script to satisfy a lint would be a worse thing than the lint. Scoped to this
// crate only, so the same lints still bite on hand-written docs elsewhere.
#![allow(rustdoc::invalid_html_tags)]

pub mod common {
    pub mod v1 {
        include!(concat!(
            env!("OUT_DIR"),
            "/opentelemetry.proto.common.v1.rs"
        ));
    }
}

pub mod resource {
    pub mod v1 {
        include!(concat!(
            env!("OUT_DIR"),
            "/opentelemetry.proto.resource.v1.rs"
        ));
    }
}

pub mod logs {
    pub mod v1 {
        include!(concat!(env!("OUT_DIR"), "/opentelemetry.proto.logs.v1.rs"));
    }
}

pub mod trace {
    pub mod v1 {
        include!(concat!(env!("OUT_DIR"), "/opentelemetry.proto.trace.v1.rs"));
    }
}

pub mod metrics {
    pub mod v1 {
        include!(concat!(
            env!("OUT_DIR"),
            "/opentelemetry.proto.metrics.v1.rs"
        ));
    }
}

pub mod collector {
    pub mod logs {
        pub mod v1 {
            include!(concat!(
                env!("OUT_DIR"),
                "/opentelemetry.proto.collector.logs.v1.rs"
            ));
        }
    }
    pub mod trace {
        pub mod v1 {
            include!(concat!(
                env!("OUT_DIR"),
                "/opentelemetry.proto.collector.trace.v1.rs"
            ));
        }
    }
    pub mod metrics {
        pub mod v1 {
            include!(concat!(
                env!("OUT_DIR"),
                "/opentelemetry.proto.collector.metrics.v1.rs"
            ));
        }
    }
}
