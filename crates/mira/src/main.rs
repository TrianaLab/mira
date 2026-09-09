//! Mira: OTLP in, immutable Arrow blocks out, one binary.

mod api;
mod config;
#[cfg(test)]
mod e2e;
mod json;
mod mcp;
mod pipeline;
mod receiver;
mod term;
mod tui;
mod ui;

use std::path::{Path, PathBuf};

use config::Config;

const USAGE: &str = "mira [--config FILE] [--node NAME] [--grpc ADDR] [--http ADDR]
     [--data-dir PATH] [--retention DURATION] [--peers a:1,b:2]
     [--max-request-bytes SIZE] [--version]

mira mira [--config FILE] [--data-dir PATH] [--addr HOST[:PORT]]

Flags override the config file, which overrides the defaults. Every value can
also come from the file via ${env:VAR} — see docs/CONFIG.md.

`mira mira` opens the terminal UI. With --data-dir it reads a block directory
in-process and needs no server running; with --addr it queries one over HTTP.
`mira tui` is the same thing, for anyone who guesses that first.";

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
    load_from(argv)
}

/// [`load`] without the two flags that end the process, so it can be called.
fn load_from(argv: Vec<String>) -> Result<Config, String> {
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
            "--max-request-bytes" => cfg.max_request_bytes = config::bytes(&value()?)?,
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

/// Where a `mira mira` invocation should read from.
///
/// `--addr` wins if given; otherwise the same `data_dir` the server would use,
/// so `mira mira --config mira.yaml` looks at exactly the directory that config
/// writes to.
fn tui_source(argv: &[String]) -> Result<tui::Source, String> {
    let mut cfg = match argv.iter().position(|a| a == "--config") {
        Some(i) => Config::load(Path::new(argv.get(i + 1).ok_or("--config needs a value")?))?,
        None => Config::default(),
    };
    let mut addr = None;
    let mut it = argv.iter().cloned();
    while let Some(flag) = it.next() {
        let mut value = || it.next().ok_or_else(|| format!("{flag} needs a value"));
        match flag.as_str() {
            "--config" => {
                value()?;
            }
            "--data-dir" => cfg.data_dir = PathBuf::from(value()?),
            "--addr" => addr = Some(tui::parse_addr(&value()?)?),
            other => return Err(format!("unknown flag {other}\n\n{USAGE}")),
        }
    }
    Ok(match addr {
        Some(a) => tui::Source::Remote(a),
        None => tui::Source::Local(cfg.data_dir),
    })
}

fn main() {
    if let Err(e) = run() {
        // Returning the error from `main` would print it with `Debug`, which
        // for `Error::Io` is a struct dump with `Os { code: 13, .. }` in it and
        // for `Error::NetworkFilesystem` throws away the paragraph explaining
        // what to do instead. Everything that reaches here is aimed at whoever
        // typed the command; they need the sentence, not the struct.
        eprintln!("mira: {e}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), Box<dyn std::error::Error>> {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    // `mira mira` is the name; `tui` stays because it is what someone types when
    // they have not read the usage, and answering that is cheaper than a
    // "no such flag" they have to think about.
    if argv.first().is_some_and(|a| a == "mira" || a == "tui") {
        if argv.iter().any(|a| a == "-h" || a == "--help") {
            println!("{USAGE}");
            return Ok(());
        }
        // No tracing subscriber on this path, and no runtime. Both write to the
        // terminal the TUI has just taken over, and one stray `info!` in the
        // middle of a frame corrupts the whole screen.
        let src = tui_source(&argv[1..]).map_err(|e| -> Box<dyn std::error::Error> { e.into() })?;
        return tui::run(src).map_err(Into::into);
    }

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "mira=info,mira_core=info".into()),
        )
        // Colour only for a terminal. `with_ansi` defaults to on and does not
        // check, so without this every field name in every line reaches a log
        // file, a collector, or an agent wrapped in escape codes.
        .with_ansi(std::io::IsTerminal::is_terminal(&std::io::stdout()))
        .init();
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?
        .block_on(serve())
}

async fn serve() -> Result<(), Box<dyn std::error::Error>> {
    let cfg = load().map_err(|e| -> Box<dyn std::error::Error> { e.into() })?;
    serve_with(cfg, shutdown()).await
}

/// The server proper, with its stop edge passed in rather than taken from the
/// process. A test can hold that edge; nothing else needs to.
async fn serve_with(
    cfg: Config,
    stop_signal: impl std::future::Future<Output = ()>,
) -> Result<(), Box<dyn std::error::Error>> {
    std::fs::create_dir_all(&cfg.data_dir)?;
    // Before anything is mapped. A network mount is not a slow start, it is a
    // SIGBUS the first time the server hiccups, and by then there is a process
    // to explain rather than a flag to change.
    mira_core::block::check_filesystem(&cfg.data_dir)?;
    mira_core::block::check_writable(&cfg.data_dir)?;

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
        max_request_bytes: cfg.max_request_bytes,
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
    // The bound address, not the requested one. `--http 127.0.0.1:0` is a real
    // thing to ask for and the line below is the only place the chosen port is
    // ever written down. tonic binds its own socket out of reach, so the gRPC
    // half of that line cannot say the same and a `:0` there stays a `:0`.
    let http_addr = listener.local_addr()?;
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
        _ = stop_signal => tracing::info!("draining"),
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

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(s: &str) -> Vec<String> {
        s.split_whitespace().map(str::to_owned).collect()
    }

    fn tmp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("mira-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    /// Flag > file > default, and every flag lands in the field it names.
    ///
    /// This parser is hand-rolled, and the failure it can produce is the quiet
    /// kind: a flag written into the wrong field starts a server that looks
    /// exactly right until someone reads a block from the wrong directory.
    #[test]
    fn flags_override_the_file_which_overrides_the_defaults() {
        let dir = tmp("load");
        let file = dir.join("mira.yaml");
        std::fs::write(
            &file,
            r#"{ "node": "from-file",
                 "listen": { "grpc": "127.0.0.1:1", "http": "127.0.0.1:2" },
                 "storage": { "dir": "/from/file", "retention": "3h" },
                 "ingest": { "max_request_bytes": "1MiB" } }"#,
        )
        .unwrap();
        let f = file.display();

        let c = load_from(argv(&format!("--config {f}"))).unwrap();
        assert_eq!(c.node, "from-file");
        assert_eq!(c.data_dir, PathBuf::from("/from/file"));
        assert_eq!(c.retention, std::time::Duration::from_secs(3 * 3600));
        assert_eq!(c.max_request_bytes, 1 << 20);
        assert_eq!(c.http.port(), 2);

        // The same file, every value overridden. `--config` is seen twice — once
        // to find the file and once by the loop, which must consume its value
        // rather than read it as a flag.
        let c = load_from(argv(&format!(
            "--config {f} --node cli --grpc 127.0.0.1:3 --http 127.0.0.1:4 \
             --data-dir /from/cli --retention 30s --max-request-bytes 2MiB \
             --peers a:1,,b:2,"
        )))
        .unwrap();
        assert_eq!(c.node, "cli");
        assert_eq!(c.grpc.port(), 3);
        assert_eq!(c.http.port(), 4);
        assert_eq!(c.data_dir, PathBuf::from("/from/cli"));
        assert_eq!(c.retention, std::time::Duration::from_secs(30));
        assert_eq!(c.max_request_bytes, 2 << 20);
        // Empty entries are dropped, so a trailing comma is not a peer named "".
        assert_eq!(c.peers, ["a:1", "b:2"]);

        // No arguments at all is the shipped configuration.
        let d = load_from(vec![]).unwrap();
        assert_eq!(d.node, Config::default().node);

        for (args, want) in [
            ("--nope", "unknown flag --nope"),
            ("--node", "--node needs a value"),
            ("--config", "--config needs a value"),
            ("--grpc nope", "--grpc:"),
            ("--http nope", "--http:"),
            ("--retention nope", "not a duration"),
            ("--max-request-bytes nope", "not a size"),
            ("--config /no/such/file.yaml", "/no/such/file.yaml"),
        ] {
            let e = load_from(argv(args)).unwrap_err();
            assert!(e.contains(want), "{args:?} said {e:?}");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `mira mira` reads the directory the matching `mira serve` would write to,
    /// which is the whole reason it takes `--config` at all.
    #[test]
    fn the_tui_reads_the_directory_its_config_writes_to() {
        let dir = tmp("tui-src");
        let file = dir.join("mira.yaml");
        std::fs::write(&file, r#"{ "storage": { "dir": "/from/file" } }"#).unwrap();
        let f = file.display();

        let local = |a: &str| {
            let Ok(tui::Source::Local(p)) = tui_source(&argv(a)) else {
                panic!("expected a local source for {a:?}")
            };
            p
        };
        assert_eq!(local(&format!("--config {f}")), PathBuf::from("/from/file"));
        assert_eq!(
            local(&format!("--config {f} --data-dir /from/cli")),
            PathBuf::from("/from/cli")
        );
        assert_eq!(local(""), Config::default().data_dir);

        // `--addr` wins outright: a remote source has no directory to read.
        let tui::Source::Remote(a) = tui_source(&argv("--addr host:9999")).unwrap() else {
            panic!("expected a remote source")
        };
        assert_eq!(a, "host:9999");

        for (args, want) in [
            ("--nope", "unknown flag --nope"),
            ("--data-dir", "--data-dir needs a value"),
            ("--config", "--config needs a value"),
        ] {
            let Err(e) = tui_source(&argv(args)) else {
                panic!("{args:?} was accepted")
            };
            assert!(e.contains(want), "{args:?} said {e:?}");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Start everything, then stop it.
    ///
    /// The assertion is that this returns at all. Shutdown is three ordered
    /// steps — stop accepting, await the servers so the last `Ingest` clone
    /// drops, then let each flusher seal what it holds — and if any link in that
    /// chain is wrong the drain never completes and the 15s timeout fires. It
    /// also proves the two listeners and the retention worker start from a
    /// `Config` alone, which is the one thing `e2e.rs` cannot say: it builds the
    /// router itself.
    #[tokio::test]
    async fn the_server_starts_from_a_config_and_drains_when_stopped() {
        let dir = tmp("serve");
        let cfg = Config {
            data_dir: dir.join("data"),
            grpc: "127.0.0.1:0".parse().unwrap(),
            http: "127.0.0.1:0".parse().unwrap(),
            ..Config::default()
        };
        let t = std::time::Instant::now();
        serve_with(cfg.clone(), std::future::ready(()))
            .await
            .unwrap();
        assert!(
            t.elapsed() < std::time::Duration::from_secs(15),
            "timed out"
        );
        // Created, not required to exist: an operator points `--data-dir` at a
        // path and expects the first start to make it.
        assert!(cfg.data_dir.is_dir());

        // A port already taken is reported, not survived. This is the arm that
        // turns "mira is running" into a process that answers nothing.
        let held = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let taken = Config {
            http: held.local_addr().unwrap(),
            ..cfg
        };
        let e = serve_with(taken, std::future::pending())
            .await
            .unwrap_err()
            .to_string();
        assert!(e.to_lowercase().contains("address"), "{e}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
