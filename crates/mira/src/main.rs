//! Mira: OTLP in, immutable Arrow blocks out, one binary.

mod api;
mod config;
#[cfg(test)]
mod e2e;
mod pipeline;
mod receiver;

use std::path::{Path, PathBuf};

use config::Config;

const USAGE: &str = "mira [--config FILE] [--node NAME] [--grpc ADDR] [--http ADDR]
     [--data-dir PATH] [--retention DURATION] [--peers a:1,b:2]

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
    let recv = receiver::Receivers {
        logs: pipeline::spawn::<mira_core::logs::LogsBuilder>(pcfg.clone()),
        traces: pipeline::spawn::<mira_core::traces::TracesBuilder>(pcfg.clone()),
        metrics: pipeline::spawn::<mira_core::metrics::MetricsBuilder>(pcfg.clone()),
    };
    pipeline::spawn_retention(pcfg);

    let grpc = tonic::transport::Server::builder()
        .add_service(recv.logs_server())
        .add_service(recv.traces_server())
        .add_service(recv.metrics_server())
        .serve(grpc_addr);

    let listener = tokio::net::TcpListener::bind(http_addr).await?;
    // OTLP and the query API share one listener: `/v1/*` in, `/api/v1/*` out.
    let http = axum::serve(
        listener,
        receiver::http_router(recv).merge(api::router(api::Api { data_dir })),
    );

    // The node id is logged because it is the only externally visible thing that
    // distinguishes two replicas' blocks, and a collision is diagnosed here.
    tracing::info!(
        grpc = %grpc_addr, http = %http_addr,
        node = %cfg.node, node_id = format!("{node:08x}"), peers,
        "mira listening"
    );

    tokio::select! {
        r = grpc => r?,
        r = http => r?,
        _ = tokio::signal::ctrl_c() => tracing::info!("shutting down"),
    }
    Ok(())
}
