//! `mira-operator` — the controller that scales a Mira tier.
//!
//! A separate binary from `mira`, and separate on purpose. The engine's crate
//! count is a published product property and its dependency budget refuses an
//! HTTP client outright — `mira update` shells out to `curl` rather than carry
//! one. Talking to the Kubernetes API means TLS, which means ~220 crates. None
//! of that belongs in the binary a user runs to store telemetry.
//!
//! The split is also the architectural claim. Principle 4 says *Mira* holds no
//! coordination state; this process holds all of it, in one place, outside the
//! data path. Kill it and every Mira pod keeps ingesting and serving — only the
//! scaling stops.

// Through the library, like `crdgen`, rather than re-declaring the modules
// here: `mod controller;` in a bin that also has a `[lib]` compiles the whole
// crate a second time, and `cargo test` then runs every unit test twice under
// two target names. Same binary, half the build.
use mira_operator::{controller, lease};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info,kube=warn".into()),
        )
        .init();

    // Infers in-cluster config from the service account, falling back to
    // `~/.kube/config` when run on a laptop. The fallback is what makes
    // `cargo run` against a kind cluster the inner loop.
    let client = kube::Client::try_default().await?;

    // Before the controller and not beside it. A standby waits here rather
    // than reconciling and discovering afterwards that it should not have —
    // see `lease.rs` for what two controllers do to one tier, and for the two
    // things this does not cover.
    let ns = client.default_namespace().to_string();
    let l = lease::Lease::new(&client, &ns, lease::identity());
    l.hold().await?;
    tokio::spawn(l.renew_forever());

    tracing::info!("mira-operator watching MiraCluster resources");
    controller::run(client).await?;
    Ok(())
}
