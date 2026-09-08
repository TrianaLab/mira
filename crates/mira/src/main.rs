//! Mira: OTLP in, immutable Arrow blocks out, one binary.

mod api;
mod config;
#[cfg(test)]
mod e2e;
mod mcp;
mod pipeline;
mod receiver;
mod ui;

use std::path::{Path, PathBuf};

use config::Config;

const USAGE: &str = "mira [--config FILE] [--node NAME] [--grpc ADDR] [--http ADDR]
     [--data-dir PATH] [--retention DURATION] [--peers a:1,b:2] [--version]

Flags override the config file, which overrides the defaults. Every value can
also come from the file via ${env:VAR} — see docs/CONFIG.md.";

/// Precedence is flag > file > default. Hand-rolled: the flag set exists only to
/// override the file, so a parser crate would be more code than the thing it
/// parses.
fn load() -> Result<Config, String> {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    if argv.iter().any(|a| a == "-h" || a == "--help") {
        println!("{USAGE}");
        std::process::exit(0);
    }
    if argv.iter().any(|a| a == "-V" || a == "--version") {
        println!("mira {}", env!("CARGO_PKG_VERSION"));
        std::process::exit(0);
    }

    // The file has to be read first so flags can override it.
    let mut cfg = match argv.iter().position(|a| a == "--config") {
        Some(i) => Config::load(Path::new(argv.get(i + 1).ok_or("--config needs a value")?))?,
        None => Config::default(),
    };

    let mut it = argv.into_iter();
    while let Some(flag) = it.next() {
        let mut value = || it.next().ok_or_else(|| format!("{flag} needs a value"));
        match flag.as_str() {
            "--config" => {
                value()?;
            }
            "--node" => cfg.node = value()?,
            "--grpc" => cfg.grpc = value()?.parse().map_err(|e| format!("--grpc: {e}"))?,
            "--http" => cfg.http = value()?.parse().map_err(|e| format!("--http: {e}"))?,
            "--data-dir" => cfg.data_dir = PathBuf::from(value()?),
            "--retention" => cfg.retention = config::duration(&value()?)?,
            "--peers" => {
                cfg.peers = value()?
                    .split(',')
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(str::to_owned)
                    .collect()
            }
            other => return Err(format!("unknown flag {other}\n\n{USAGE}")),
        }
    }
    Ok(cfg)
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "mira=info,mira_core=info".into()),
        )
        .init();

    let cfg = load().map_err(|e| -> Box<dyn std::error::Error> { e.into() })?;
    std::fs::create_dir_all(&cfg.data_dir)?;

    let node = mira_core::block::node_id(&cfg.node);
    let (grpc_addr, http_addr) = (cfg.grpc, cfg.http);
    let peers = cfg.peers.len();

    let data_dir = std::sync::Arc::new(cfg.data_dir.clone());
    let pcfg = std::sync::Arc::new(pipeline::Config {
        data_dir: cfg.data_dir,
        node,
        retention: cfg.retention,
        ..Default::default()
    });
    // One flusher, channel and block sequence per signal, so a slow flush on one
    // cannot stall another.
    let (logs, h_logs) = pipeline::spawn::<mira_core::logs::LogsBuilder>(pcfg.clone());
    let (traces, h_traces) = pipeline::spawn::<mira_core::traces::TracesBuilder>(pcfg.clone());
    let (metrics, h_metrics) = pipeline::spawn::<mira_core::metrics::MetricsBuilder>(pcfg.clone());
    let flushers = [h_logs, h_traces, h_metrics];
    let recv = receiver::Receivers {
        logs,
        traces,
        metrics,
    };
    pipeline::spawn_retention(pcfg);

    // One `watch` rather than two oneshots because both servers need the same
    // edge and neither owns it.
    let (stop, stop_rx) = tokio::sync::watch::channel(());
    let stopped = |mut rx: tokio::sync::watch::Receiver<()>| async move {
        let _ = rx.changed().await;
    };

    let grpc = tokio::spawn(
        tonic::transport::Server::builder()
            .add_service(recv.logs_server())
            .add_service(recv.traces_server())
            .add_service(recv.metrics_server())
            .serve_with_shutdown(grpc_addr, stopped(stop_rx.clone())),
    );

    let listener = tokio::net::TcpListener::bind(http_addr).await?;
    // One listener for all of it: `/v1/*` is OTLP in, `/api/v1/*` is query out,
    // `/mcp` is the agent surface, `/` and `/{file}` are the UI. The UI's
    // wildcard is one segment deep, so it cannot swallow any of the others.
    let api = api::Api { data_dir };
    let serve = axum::serve(
        listener,
        receiver::http_router(recv)
            .merge(api::router(api.clone()))
            .merge(mcp::router(api))
            .merge(ui::router()),
    )
    .with_graceful_shutdown(stopped(stop_rx));
    // `IntoFuture`, not `Future`, so it cannot be spawned directly.
    let http = tokio::spawn(async move { serve.await });

    // The node id is logged because it is the only externally visible thing that
    // distinguishes two replicas' blocks, and a collision is diagnosed here.
    tracing::info!(
        grpc = %grpc_addr, http = %http_addr,
        node = %cfg.node, node_id = format!("{node:08x}"), peers,
        "mira listening"
    );

    let (mut grpc, mut http) = (grpc, http);
    tokio::select! {
        r = &mut grpc => r??,
        r = &mut http => r??,
        _ = shutdown() => tracing::info!("draining"),
    }

    // Shutdown is three ordered steps and the order is the whole point.
    //
    // 1. Stop accepting, but let in-flight exports run to their acknowledgement.
    //    Skipping this is not a data-loss bug — the queued job is still sealed
    //    below — it is a *duplicate* bug: the exporter sees a reset, OTLP tells it
    //    to retry, and it re-sends data that did get stored. Every rolling
    //    restart would double-write whatever was in flight.
    // 2. Await the servers, which drops the last `Ingest` clone per signal and so
    //    closes each flusher's channel.
    // 3. A flusher answering a closed channel seals whatever is open, publishes it
    //    and acks the waiters (`pipeline::flusher`). Exiting before that lands is
    //    what turns step 1's ack into a reset after all.
    //
    // Step 1 depends on `max_block_age` to make progress: an in-flight export is
    // waiting on a block that only seals on size or age, and with the listener
    // closed no new data will grow it. So the grace here is bounded below by that
    // constant, not chosen freely.
    //
    // ponytail: bounded because a hung fsync must not outlive the orchestrator's
    // grace period and become a SIGKILL with no explanation in the log.
    let _ = stop.send(());
    let drain = async {
        let _ = (&mut grpc).await;
        let _ = (&mut http).await;
        for h in flushers {
            let _ = h.await;
        }
    };
    if tokio::time::timeout(std::time::Duration::from_secs(15), drain)
        .await
        .is_err()
    {
        tracing::warn!("did not drain in 15s; exiting anyway");
    }
    tracing::info!("stopped");
    Ok(())
}

/// Resolve on the first stop signal.
///
/// SIGTERM matters as much as SIGINT here: it is what every container
/// orchestrator sends, so without this arm a rolling restart kills the process
/// mid-block and the exporters waiting on that block see a reset.
async fn shutdown() {
    #[cfg(unix)]
    if let Ok(mut term) = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
    {
        tokio::select! {
            _ = tokio::signal::ctrl_c() => return,
            _ = term.recv() => return,
        }
    }
    let _ = tokio::signal::ctrl_c().await;
}
