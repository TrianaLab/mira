//! The Mira Kubernetes operator, as a library so that both binaries — the
//! controller and the CRD generator — share one definition of the resource.
//!
//! Two binaries and one `crd` module rather than a generator that prints a
//! hand-written YAML file: the CRD that ships in the chart is derived from the
//! Rust types the controller deserialises into, so the two cannot disagree. A
//! checked-in CRD maintained beside the struct is a schema that drifts on the
//! first field anybody adds.

pub mod controller;
pub mod crd;
pub mod resources;
pub mod stats;
