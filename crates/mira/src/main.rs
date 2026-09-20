//! Mira: OTLP in, immutable Arrow blocks out, one binary.

mod alert;
mod api;
mod config;
#[cfg(test)]
mod e2e;
mod json;
mod mcp;
mod pipeline;
mod proxy;
mod rca;
mod receiver;
mod telemetry;
mod term;
mod tui;
mod ui;
mod update;

use std::io::IsTerminal;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering::Relaxed;
use std::time::Duration;

use axum::Router;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use clap::{Arg, ArgAction, ArgMatches, value_parser};

use config::Config;

/// The whole command tree, in one place.
///
/// This was a `const USAGE` and a flat `match` per mode, on the argument that a
/// parser crate is more code than the flag set it parses. That was true of the
/// parsing and false of everything around it: the usage text, the generated CLI
/// page and — once shell completion was asked for — a script per shell all
/// restate the same flags, with nothing holding them together. One tree is what
/// `--help`, `mira completion` and the parser now all read, so a flag is
/// accepted, documented and completed in the same edit.
///
/// Five crates against a tree of 148: `clap`, `clap_builder`, `clap_lex`,
/// `anstyle` and `clap_complete`. `derive` is off because it costs `syn` and
/// its tree to generate a struct this file would rather write out, and `color`
/// is off because everything else this binary prints already decides its own
/// colour from `is_terminal`.
fn cli() -> clap::Command {
    clap::Command::new("mira")
        .version(env!("CARGO_PKG_VERSION"))
        .about("OTLP in, immutable Arrow blocks out, one binary.")
        .after_help(
            "Flags override the config file, which overrides the defaults. Every value\n\
             can also come from the file via ${env:VAR} — see https://miradb.dev/config/.",
        )
        // The bare command is the storage node, so it carries the server's flags
        // itself rather than behind a `serve` verb nobody had to type before.
        .arg(config_arg())
        .arg(
            Arg::new("node")
                .long("node")
                .value_name("NAME")
                .help("this replica's name, hashed into its block directory names"),
        )
        .arg(
            Arg::new("grpc")
                .long("grpc")
                .value_name("ADDR")
                .value_parser(value_parser!(SocketAddr))
                .help("where OTLP/gRPC listens"),
        )
        .arg(http_arg())
        .arg(data_dir_arg())
        .arg(
            Arg::new("retention")
                .long("retention")
                .value_name("DURATION")
                .value_parser(config::duration)
                .help("how long a block is kept before it is deleted"),
        )
        .arg(offload_arg())
        .arg(max_request_bytes_arg())
        .arg(
            Arg::new("queue")
                .long("queue")
                .value_name("N")
                .value_parser(config::positive)
                .help("exports queued for one signal before the next has to wait"),
        )
        .arg(
            Arg::new("shards")
                .long("shards")
                .value_name("N")
                .value_parser(config::whole)
                .help("flushers per signal, or 0 for one per two cores"),
        )
        .arg(
            Arg::new("telemetry-interval")
                .long("telemetry-interval")
                .value_name("DURATION")
                .value_parser(config::duration)
                .help("how often --self-telemetry samples this node's counters"),
        )
        .arg(
            Arg::new("alerts")
                .long("alerts")
                .value_name("FILE")
                .value_parser(value_parser!(PathBuf))
                .help("KYAML alerting rules; absent means alerting is off"),
        )
        .arg(
            Arg::new("wal")
                .long("wal")
                .action(ArgAction::SetTrue)
                .help("acknowledge on the write-ahead log rather than on the block"),
        )
        .arg(
            Arg::new("self-telemetry")
                .long("self-telemetry")
                .action(ArgAction::SetTrue)
                .help("store this node's own counters in this node"),
        )
        .subcommand(
            clap::Command::new("mira")
                // `tui` stays because it is what someone types when they have
                // not read the usage, and answering that is cheaper than a "no
                // such subcommand" they have to think about.
                .visible_alias("tui")
                .about("open the terminal UI over the same blocks")
                .after_help(
                    "With --data-dir it reads a block directory in-process and needs no\n\
                     server running; with --addr it queries one over HTTP.",
                )
                .arg(config_arg())
                .arg(data_dir_arg())
                .arg(
                    Arg::new("addr")
                        .long("addr")
                        .value_name("HOST[:PORT]")
                        .value_parser(tui::parse_addr)
                        .help("query a running node instead of reading a directory"),
                ),
        )
        .subcommand(
            clap::Command::new("proxy")
                .about("one OTLP and query surface in front of N storage nodes")
                .after_help(
                    "It stores nothing: exports are split by entity and forwarded, and\n\
                     /api/v1/query is answered by merging every replica's page on the\n\
                     cursor order. The reads it cannot merge — correlate, map, metrics\n\
                     and entities — answer 501 naming themselves rather than returning\n\
                     one node's share of the answer.",
                )
                .arg(config_arg())
                .arg(http_arg())
                .arg(max_request_bytes_arg())
                .arg(
                    Arg::new("replica")
                        .long("replica")
                        .value_name("URL")
                        // Repeatable, unlike every other flag here, because the
                        // value is a list and `--replica a --replica b` is what
                        // a process manager's args array already looks like. The
                        // file form is one comma-separated string; see
                        // `config::replicas`.
                        .action(ArgAction::Append)
                        .value_parser(config::replicas)
                        .help("a storage node to forward to; repeatable"),
                ),
        )
        .subcommand(
            clap::Command::new("offload")
                .about("move blocks between a data directory and an object store")
                .after_help(
                    "--offload sends a block to an object store just before retention\n\
                     deletes it, under the same directory name it had locally — so the\n\
                     store's own listing is the catalog and there is nothing else to keep\n\
                     in sync.\n\n\
                     `push` is the same copy in the other direction and deletes nothing.\n\
                     It is how a volume a scale-down left behind is re-homed: push it,\n\
                     then restore it into a node that is still running. Give it a URI of\n\
                     its own — file:///archive/${node} — so the restore pulls back one\n\
                     node's blocks rather than the whole archive.",
                )
                .subcommand_required(true)
                .arg_required_else_help(true)
                .subcommand(offload_verb("list", "list what the store holds"))
                .subcommand(
                    offload_verb(
                        "restore",
                        "copy every block the store has and this node does not into --data-dir",
                    )
                    .arg(data_dir_arg()),
                )
                .subcommand(
                    offload_verb("push", "copy this node's blocks into the store")
                        .arg(data_dir_arg()),
                ),
        )
        .subcommand(update::cli())
        .subcommand(
            clap::Command::new("completion")
                .about("print a shell completion script")
                .after_help(
                    "The script is generated from this binary's own command tree, so it\n\
                     completes exactly the flags this version accepts:\n\n  \
                     mira completion bash > /etc/bash_completion.d/mira\n  \
                     mira completion zsh  > \"${fpath[1]}/_mira\"\n  \
                     mira completion fish > ~/.config/fish/completions/mira.fish",
                )
                .arg(
                    Arg::new("shell")
                        .required(true)
                        .value_name("SHELL")
                        .value_parser(value_parser!(clap_complete::Shell)),
                ),
        )
}

/// The five flags more than one mode takes, written once.
///
/// A subcommand that shares a flag has to share its spelling, its value name and
/// its help line too, or `mira --help` and `mira proxy --help` describe the same
/// thing two ways.
fn config_arg() -> Arg {
    Arg::new("config")
        .long("config")
        .value_name("FILE")
        .value_parser(value_parser!(PathBuf))
        .help("KYAML configuration file; flags override what it sets")
}

fn data_dir_arg() -> Arg {
    Arg::new("data-dir")
        .long("data-dir")
        .value_name("PATH")
        .value_parser(value_parser!(PathBuf))
        .help("the block directory, which is the whole manifest")
}

fn http_arg() -> Arg {
    Arg::new("http")
        .long("http")
        .value_name("ADDR")
        .value_parser(value_parser!(SocketAddr))
        .help("where OTLP/HTTP, the query API, MCP and the UI listen")
}

fn max_request_bytes_arg() -> Arg {
    Arg::new("max-request-bytes")
        .long("max-request-bytes")
        .value_name("SIZE")
        .value_parser(config::bytes)
        .help("the largest export either listener will decode")
}

fn offload_arg() -> Arg {
    Arg::new("offload")
        .long("offload")
        .value_name("URI")
        .help("where a block goes before retention unlinks it")
}

/// The three `offload` verbs, which differ only in direction.
///
/// `--data-dir` is added by the two that copy rather than here: `list` reads the
/// store's own listing and never touches a local directory, and a flag that does
/// nothing is what `config::check_keys` exists to prevent.
fn offload_verb(name: &'static str, about: &'static str) -> clap::Command {
    clap::Command::new(name)
        .about(about)
        .arg(config_arg())
        .arg(offload_arg())
}

/// One getter for a value that may not have been given *or* may not exist here.
///
/// `ArgMatches::get_one` panics on an id the matched subcommand does not
/// declare, and one [`Config`] is built for the server, the proxy and the three
/// `offload` verbs, which declare three different subsets of the same flags.
/// "Not given" and "not a flag of this mode" are the same answer to
/// [`load_from`], so they are the same answer here.
fn opt<T: Clone + Send + Sync + 'static>(m: &ArgMatches, id: &str) -> Option<T> {
    m.try_get_one::<T>(id).ok().flatten().cloned()
}

/// [`opt`] for a repeatable flag, flattened.
///
/// Twice, because each `--replica` already parses to a comma-separated list:
/// once over the occurrences and once over what each one held.
fn many(m: &ArgMatches, id: &str) -> Vec<String> {
    m.try_get_many::<Vec<String>>(id)
        .ok()
        .flatten()
        .into_iter()
        .flatten()
        .flatten()
        .cloned()
        .collect()
}

/// Precedence is flag > file > default.
///
/// The file is read first and anything clap actually saw then overwrites it. No
/// flag carries a `default_value`, which is what keeps "not given" distinct from
/// "given the same value the default has" — `--retention 7d` has to win over a
/// file that says an hour even when 7d is also the built-in.
fn load_from(m: &ArgMatches) -> Result<Config, String> {
    let mut cfg = match opt::<PathBuf>(m, "config") {
        Some(p) => Config::load(&p)?,
        None => Config::default(),
    };
    if let Some(v) = opt::<String>(m, "node") {
        cfg.node = v;
    }
    if let Some(v) = opt::<SocketAddr>(m, "grpc") {
        cfg.grpc = v;
    }
    if let Some(v) = opt::<SocketAddr>(m, "http") {
        cfg.http = v;
    }
    if let Some(v) = opt::<PathBuf>(m, "data-dir") {
        cfg.data_dir = v;
    }
    if let Some(v) = opt::<Duration>(m, "retention") {
        cfg.retention = v;
    }
    if let Some(v) = opt::<String>(m, "offload") {
        cfg.offload = Some(v);
    }
    if let Some(v) = opt::<usize>(m, "max-request-bytes") {
        cfg.max_request_bytes = v;
    }
    if let Some(v) = opt::<usize>(m, "queue") {
        cfg.queue = v;
    }
    if let Some(v) = opt::<usize>(m, "shards") {
        cfg.shards = v;
    }
    if let Some(v) = opt::<Duration>(m, "telemetry-interval") {
        cfg.telemetry_interval = v;
    }
    if let Some(v) = opt::<PathBuf>(m, "alerts") {
        cfg.alerts = Some(v);
    }
    cfg.replicas.extend(many(m, "replica"));
    // These two take no value. They are the settings whose file form has to be
    // able to say `false` — to turn off what an inherited config turned on — and
    // whose flag form never does, because a flag is only ever typed to enable
    // something the file did not. So they are an or, not an assignment.
    cfg.wal |= opt::<bool>(m, "wal").unwrap_or_default();
    cfg.self_telemetry |= opt::<bool>(m, "self-telemetry").unwrap_or_default();
    Ok(cfg)
}

/// `mira offload list`, `mira offload restore` and `mira offload push`.
///
/// All three are `readdir` against one side or the other and nothing else.
/// There is no state here to be stale, no index to rebuild and nothing to
/// reconcile with a running server: a `restore` into the data directory of a
/// live node is a block appearing in it, which is the same event as a publish
/// and is already how the read path learns about anything (section 4).
///
/// `push` copies and does not unlink, and that is the design rather than
/// caution. A node derives two numbers from the blocks it still holds locally,
/// and emptying the directory walks both backwards: the WAL watermark
/// ([`mira_core::block::wal_watermarks`]) falls to `0` for any signal left with
/// no block, so the next boot replays a log that has already been absorbed, and
/// the block sequence resumes at `max(seq) + 1` over the local scan, so the
/// node reissues `(node, seq)` pairs that are still alive wherever they were
/// copied — and `Cursor` is documented as a total order over exactly that pair.
/// The case this verb exists for is a volume that is about to be deleted, where
/// freeing space on it buys nothing at all, let alone those two.
///
/// ponytail: `push` reads the directory live, so a block being compacted at
/// that moment — `<table>.arrow.<node>.tmp` renamed over `<table>.arrow` — can
/// be copied mid-rewrite. It is meant for a volume whose server is stopped,
/// which is the case it was asked for; holding it against a running one needs
/// the compaction lock this module deliberately does not have.
fn offload_cmd(verb: &str, m: &ArgMatches) -> Result<(), String> {
    let cfg = load_from(m)?;
    // Not `required(true)` on the flag, because `storage.offload` in --config is
    // the other way to say it and a node that already has one in its file should
    // not have to repeat it here.
    let uri = cfg
        .offload
        .as_deref()
        .ok_or("mira offload needs --offload URI, or storage.offload in --config")?;
    let target = mira_core::offload::Target::parse(uri).map_err(|e| e.to_string())?;
    let node = mira_core::block::node_id(&cfg.node);
    if verb == "restore" {
        // The same guard `serve_with` runs, for the same reason and before the
        // same `mmap`: restoring a corpus onto a network mount and then
        // pointing a server at it is a SIGBUS deferred by one command.
        mira_core::block::check_filesystem(&cfg.data_dir).map_err(|e| e.to_string())?;
    }
    let (mut blocks, mut bytes) = (0u64, 0u64);
    for signal in pipeline::SIGNALS {
        // The verb chooses which side is the source. Both sides are the same
        // `scan` over the same names, which is the property section 3.2's
        // naming scheme exists to have — a store needs no listing code of its
        // own, and neither does reading one backwards.
        let source = match verb {
            "push" => mira_core::block::scan(&cfg.data_dir, signal),
            _ => target.list(signal),
        };
        for b in source.map_err(|e| e.to_string())? {
            let name = b.dir.file_name().unwrap_or_default().to_string_lossy();
            let size = block_bytes(&b.dir);
            blocks += 1;
            bytes += size;
            let what = match verb {
                "restore" => match target
                    .pull(signal, &b, &cfg.data_dir, node)
                    .map_err(|e| e.to_string())?
                {
                    true => "restored",
                    false => "present",
                },
                "push" => match target.push(signal, &b).map_err(|e| e.to_string())? {
                    true => "pushed",
                    false => "present",
                },
                _ => "",
            };
            println!(
                "{signal:<8} {} .. {}  {:>10}  {name} {what}",
                tui::stamp(b.min_ts),
                tui::stamp(b.max_ts),
                size,
            );
        }
    }
    println!("{blocks} blocks, {bytes} bytes");
    Ok(())
}

/// Sum of the block's files. Not derivable from the name — the name carries
/// everything needed to *prune*, which is the claim the layout makes; how many
/// bytes are behind it is a `stat` per file and only this command wants it.
fn block_bytes(dir: &Path) -> u64 {
    std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| e.metadata().ok())
        .filter(|m| m.is_file())
        .map(|m| m.len())
        .sum()
}

/// The two background tasks `mira_core::diag` needs to be readable.
///
/// A function rather than the body of the `if` that gates it, because
/// `tracing::enabled!` reads the *current* dispatcher and a `tokio::spawn`ed
/// task does not inherit the scoped one a test can install — so the gate and
/// the tasks behind it cannot both be reached through the call site, and the
/// instrument the ingest diagnosis is read from would be the one part of it
/// never run under test.
fn spawn_probes() {
    // A task that does no work at all, so how late it wakes is a property
    // of the runtime and not of the load. See `diag::RUNTIME_LAG`.
    tokio::spawn(async {
        loop {
            let t = std::time::Instant::now();
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            mira_core::diag::RUNTIME_LAG
                .record(t.elapsed().as_nanos().saturating_sub(50_000_000) as u64);
        }
    });
    tokio::spawn(async {
        let mut t = tokio::time::interval(std::time::Duration::from_secs(5));
        loop {
            t.tick().await;
            // Cumulative from start, so two dumps are a rate and one is a
            // mean over the run.
            tracing::debug!(target: "mira::probe", "{}", mira_core::diag::dump());
        }
    });
}

/// Where a `mira mira` invocation should read from.
///
/// `--addr` wins if given; otherwise the same `data_dir` the server would use,
/// so `mira mira --config mira.yaml` looks at exactly the directory that config
/// writes to.
fn tui_source(m: &ArgMatches) -> Result<tui::Source, String> {
    let cfg = match opt::<PathBuf>(m, "config") {
        Some(p) => Config::load(&p)?,
        None => Config::default(),
    };
    Ok(match opt::<String>(m, "addr") {
        Some(a) => tui::Source::Remote(a),
        None => tui::Source::Local(opt::<PathBuf>(m, "data-dir").unwrap_or(cfg.data_dir)),
    })
}

/// The guards `serve_with` runs, for a `mira mira --data-dir` that maps exactly
/// the same blocks with no server in front of it.
///
/// Without them [`mira_core::block::scan`] reads `ENOENT` as an empty directory,
/// so a typo or a volume that never mounted prints `no rows in this window ·
/// 0/0 blocks` — indistinguishable from a healthy empty store, and "is the data
/// gone" is the question this feature exists to answer at 3am. A network
/// filesystem is worse: it reaches `mmap` and leaves on `SIGBUS`, with nothing
/// printed at all, where the server would have refused with a paragraph naming
/// the mount.
///
/// `check_writable` is deliberately not here. The TUI writes nothing, and a
/// read-only mount is the normal way to look at a detached volume.
///
/// The two cases with nothing to check are decided here rather than at the call
/// site, because both of them are rules about this check and not about the
/// caller. A remote source is somebody else's directory: the server answering
/// on that address ran these on its own way up, and this process never maps one
/// of its blocks. And off a terminal `tui::run` refuses a redirected stdin or
/// stdout on its first line, which outranks a diagnosis of a directory nothing
/// was going to be drawn from anyway.
fn check_source(src: &tui::Source, on_a_tty: bool) -> Result<(), Box<dyn std::error::Error>> {
    let tui::Source::Local(dir) = src else {
        return Ok(());
    };
    if !on_a_tty {
        return Ok(());
    }
    if !dir.is_dir() {
        return Err(format!(
            "{} is not a directory. `mira mira --data-dir` reads an existing block \
             directory in place and creates nothing, so this is a volume that never \
             mounted, a typo, or the path a different replica writes to. An empty \
             but real directory is fine and shows no rows.",
            dir.display()
        )
        .into());
    }
    // `check_filesystem`'s FUSE arm warns rather than refusing — the magic
    // number cannot tell gcsfuse from a local overlay, so only a human can — and
    // this process installs no subscriber, because one stray line lands in the
    // middle of a frame. So: one stderr subscriber for the length of the call,
    // before `Term::enter` takes the screen. Routing it rather than reprinting
    // the sentence keeps the wording in `block.rs`, where the check lives.
    let to_stderr = tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_ansi(std::io::stderr().is_terminal())
        .without_time()
        .finish();
    tracing::subscriber::with_default(to_stderr, || mira_core::block::check_filesystem(dir))?;
    Ok(())
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

/// A refusal from the parser, as an error from this binary.
///
/// `-h` and `-V` come through here too and are not refusals: clap prints them
/// on stdout and exits 0, which is what `mira -h | less` needs, so they take
/// its exit rather than this one. Everything else would take clap's other exit
/// — its own `error:` prefix, on stderr, code 2 — and this binary's contract is
/// `mira: <sentence>` and code 1 for anything a supervisor might read. The
/// sentence is clap's; only the prefix and the code are ours.
fn clap_error(e: &clap::Error) -> Box<dyn std::error::Error> {
    if !e.use_stderr() {
        e.exit();
    }
    e.to_string()
        .trim_start_matches("error: ")
        .trim_end()
        .to_owned()
        .into()
}

fn run() -> Result<(), Box<dyn std::error::Error>> {
    // `-h`, `--help` and `-V` are clap's now, on every subcommand rather than
    // only the two that used to hoist them — and `mira update --version v0.1.0`
    // installs that tag instead of printing this binary's version, because the
    // root's version flag is not propagated into a subcommand that declares its
    // own. Both of those were hand-written special cases here.
    let m = cli().try_get_matches().map_err(|e| clap_error(&e))?;
    let string = |e: String| -> Box<dyn std::error::Error> { e.into() };

    // Before the tracing subscriber: the installer writes its own progress to
    // this terminal, and an `info!` line interleaved with a `sudo` prompt is a
    // prompt someone does not answer.
    if let Some(("update", sub)) = m.subcommand() {
        return update::run(sub).map_err(string);
    }
    if let Some(("completion", sub)) = m.subcommand() {
        let shell = sub
            .get_one::<clap_complete::Shell>("shell")
            .copied()
            .expect("required");
        clap_complete::generate(shell, &mut cli(), "mira", &mut std::io::stdout());
        return Ok(());
    }
    if let Some(("offload", sub)) = m.subcommand() {
        let (verb, args) = sub.subcommand().expect("subcommand_required");
        return offload_cmd(verb, args).map_err(string);
    }
    if let Some(("mira", sub)) = m.subcommand() {
        // No tracing subscriber on this path, and no runtime. Both write to the
        // terminal the TUI has just taken over, and one stray `info!` in the
        // middle of a frame corrupts the whole screen.
        let src = tui_source(sub).map_err(string)?;
        // The same two guards `serve_with` runs, for the same reason and before
        // the same mmap — see `check_source`, which is also where the two cases
        // it has nothing to say about are written down.
        let on_a_tty = std::io::stdin().is_terminal() && std::io::stdout().is_terminal();
        check_source(&src, on_a_tty)?;
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
        .with_ansi(std::io::stdout().is_terminal())
        .init();
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    // After the subscriber and inside the runtime, unlike the three above: the
    // proxy is a server and wants both, it just has no storage under it.
    match m.subcommand() {
        Some(("proxy", sub)) => rt.block_on(proxy_cmd(sub)),
        _ => rt.block_on(serve(&m)),
    }
}

/// `mira proxy`: the HTTP surface of [`proxy`] and nothing else.
///
/// No data directory, no flusher, no gRPC listener. gRPC is left off rather
/// than proxied because splitting an export by entity means decoding it, and a
/// tonic service that decodes in order to re-encode to three HTTP clients is a
/// second transport to keep in step for a hop that is inside one deployment.
/// Point collectors at 4318; see `docs/architecture/replicas.md` section 12.
async fn proxy_cmd(m: &ArgMatches) -> Result<(), Box<dyn std::error::Error>> {
    let cfg = load_from(m).map_err(|e| -> Box<dyn std::error::Error> { e.into() })?;
    let p = proxy::Proxy::new(cfg.replicas.clone(), cfg.max_request_bytes)
        .map_err(|e| -> Box<dyn std::error::Error> { e.into() })?;
    let socket = tokio::net::TcpListener::bind(cfg.http).await?;
    let addr = socket.local_addr()?;
    tracing::info!(
        http = %addr,
        replicas = %cfg.replicas.join(" "),
        max_request_bytes = cfg.max_request_bytes,
        "mira proxy listening"
    );
    axum::serve(socket, proxy::router(p))
        .with_graceful_shutdown(shutdown())
        .await?;
    Ok(())
}

async fn serve(m: &ArgMatches) -> Result<(), Box<dyn std::error::Error>> {
    let cfg = load_from(m).map_err(|e| -> Box<dyn std::error::Error> { e.into() })?;
    serve_with(cfg, shutdown()).await
}

/// The server proper, with its stop edge passed in rather than taken from the
/// process. A test can hold that edge; nothing else needs to.
async fn serve_with(
    cfg: Config,
    stop_signal: impl std::future::Future<Output = ()>,
) -> Result<(), Box<dyn std::error::Error>> {
    // A setting only the other mode reads is a setting that does nothing, and
    // doing nothing in silence is what `config::check_keys` exists to prevent.
    // The likely typo is `mira --replica ...` for `mira proxy --replica ...`,
    // which would otherwise start a storage node that looks like a proxy.
    if !cfg.replicas.is_empty() {
        return Err(
            "--replica / proxy.replicas is read by `mira proxy`; this is a storage node".into(),
        );
    }

    // First, because a rules file that does not parse is a deployment that
    // believes it is being paged and is not. Nothing has been created, bound or
    // mapped at this point, so the failure is a message and an exit rather than
    // a half-started node.
    let rules = match &cfg.alerts {
        Some(p) => alert::Rules::load(p)?,
        None => alert::Rules::off(),
    };

    // Named, because `File exists (os error 17)` on its own sends whoever reads
    // it looking for a bug in Mira rather than at the path they passed.
    std::fs::create_dir_all(&cfg.data_dir).map_err(|e| {
        format!(
            "cannot create data directory {}: {e}. Mira makes this path on first \
             start, so this is a parent that is not writable or something that is \
             not a directory already sitting there (in Kubernetes: a subPath that \
             names a file, or a volume mounted readOnly).",
            cfg.data_dir.display()
        )
    })?;
    // Before anything is mapped. A network mount is not a slow start, it is a
    // SIGBUS the first time the server hiccups, and by then there is a process
    // to explain rather than a flag to change.
    mira_core::block::check_filesystem(&cfg.data_dir)?;
    mira_core::block::check_writable(&cfg.data_dir)?;

    let node = mira_core::block::node_id(&cfg.node);

    // Both sockets are bound here, before a flusher starts and long before the
    // line that says which addresses Mira is listening on. tonic binds inside
    // its own future, so a port still held by the container that is shutting
    // down used to surface as `mira: transport error` *after* the log line
    // claiming the address — the most common way a start fails, reported as the
    // least useful sentence Mira can print.
    let taken = |addr: std::net::SocketAddr, what: &str, e: std::io::Error| {
        format!(
            "cannot bind {addr} for {what}: {e}. Nothing has started yet, so this \
             is another process on the port — most often the previous instance \
             still draining (in Kubernetes: a terminationGracePeriodSeconds \
             shorter than the drain takes, or two replicas sharing a hostPort)."
        )
    };
    let grpc_socket = tonic::transport::server::TcpIncoming::bind(cfg.grpc)
        .map_err(|e| taken(cfg.grpc, "OTLP/gRPC", e))?
        // `serve_with_incoming` ignores the builder's TCP settings, and tonic's
        // default is nodelay on: without this every small export pays a Nagle
        // delay that `serve` would not have charged it.
        .with_nodelay(Some(true));
    let http_socket = tokio::net::TcpListener::bind(cfg.http)
        .await
        .map_err(|e| taken(cfg.http, "OTLP/HTTP and the query API", e))?;
    // The bound addresses, not the requested ones: `--grpc 127.0.0.1:0` is a
    // real thing to ask for and the "listening" line is the only place the
    // chosen port is ever written down.
    let (grpc_addr, http_addr) = (grpc_socket.local_addr()?, http_socket.local_addr()?);

    // Read before `cfg.data_dir` moves into the pipeline config, and before the
    // uptime clock starts, so the "listening" line below can print what was
    // actually resolved rather than what was asked for.
    let data_dir = std::sync::Arc::new(cfg.data_dir.clone());
    let _ = *START;
    // Opened before the flushers, because they take a handle to it, and before
    // anything is served, because replay has to reach the flushers ahead of the
    // first live export or the sequences interleave.
    let wal = match cfg.wal {
        true => Some(std::sync::Arc::new(mira_core::wal::Wal::open(
            &cfg.data_dir,
            node,
        )?)),
        false => None,
    };
    // The node *name*, not the hashed `node` above: a series is labelled with
    // what an operator typed, and the hash is a filename detail.
    let node_name = cfg.node.clone();
    let pcfg = std::sync::Arc::new(pipeline::Config {
        data_dir: cfg.data_dir,
        node,
        retention: cfg.retention,
        // Parsed here rather than in the sweep, so `--offload s3://…` is a
        // refusal to start with the sentence explaining why, not a warning
        // logged every minute while retention quietly deletes.
        offload: cfg
            .offload
            .as_deref()
            .map(mira_core::offload::Target::parse)
            .transpose()?,
        queue: cfg.queue,
        shards: pipeline::shard_count(
            cfg.shards,
            std::thread::available_parallelism().map_or(1, |n| n.get()),
        ),
        wal: wal.clone(),
        ..Default::default()
    });
    // One channel and block sequence per signal per shard, so a slow flush on
    // one cannot stall another.
    let (logs, o_logs, h_logs) = pipeline::spawn::<mira_core::logs::LogsBuilder>(&pcfg);
    let (traces, o_traces, h_traces) = pipeline::spawn::<mira_core::traces::TracesBuilder>(&pcfg);
    let (metrics, o_metrics, h_metrics) =
        pipeline::spawn::<mira_core::metrics::MetricsBuilder>(&pcfg);
    let flushers = [h_logs, h_traces, h_metrics];
    // In `pipeline::SIGNALS` order, which is what `Api::open` indexes with.
    let open_blocks = [o_logs, o_traces, o_metrics];
    if wal.is_some() {
        replay(
            &pcfg.data_dir,
            node,
            logs.clone(),
            traces.clone(),
            metrics.clone(),
        )
        .await?;
    }
    // After the replay, so the first sample is of a node that has finished
    // recovering rather than of one part-way through it.
    //
    // The handle is kept rather than detached, because the sampler holds an
    // `Ingest` clone and a flusher only stops when the last one drops. Detached,
    // it is the one holder that never lets go: every stop would sit out the full
    // `DRAIN_GRACE` waiting for a channel that cannot close, and turning
    // self-telemetry on would silently make each restart fifteen seconds slower.
    let sampler = cfg.self_telemetry.then(|| {
        tracing::info!(
            interval_s = cfg.telemetry_interval.as_secs(),
            "storing this node's own telemetry in this node"
        );
        tokio::spawn(telemetry::run(
            node_name,
            pcfg.data_dir.clone(),
            cfg.telemetry_interval,
            metrics.clone(),
        ))
    });
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
            .serve_with_incoming_shutdown(grpc_socket, stopped(stop_rx.clone())),
    );

    // One listener for all of it: `/v1/*` is OTLP in, `/api/v1/*` is query out
    // and `/api/v1/stats` is this node describing itself, `/mcp` is the agent
    // surface, `/health` and `/readyz` are the probes, `/` and `/{file}` are the
    // UI. The UI's wildcard is one segment deep, so it cannot swallow any of the
    // others.
    let api = api::Api {
        data_dir: std::sync::Arc::clone(&data_dir),
        open: open_blocks,
        alerts: std::sync::Arc::new(alert::Engine::new(rules)),
        // Cloned from the receiver rather than kept back from `spawn`, so there
        // is visibly one handle and the router is what holds it: every copy of
        // `Api` here lives in the served router and is dropped with it, which
        // is what lets the logs flusher finish.
        logs: Some(recv.logs.clone()),
    };
    // Always routed, even with no rules: `/api/v1/alerts` answering `[]` is how
    // the UI, the TUI and an agent learn that alerting is off, and a 404 is
    // indistinguishable from an old build.
    //
    // Without the ingest handle, and that is not tidiness: the evaluator's task
    // never returns, so an `Ingest` clone in it is one the flusher never sees
    // dropped and every shutdown on an alerting node would sit out the full
    // `DRAIN_GRACE`. The evaluator only ever reads.
    alert::spawn(api::Api {
        logs: None,
        ..api.clone()
    });
    // The two tasks `mira_core::diag` needs to be readable, spawned only when
    // someone has asked to read it. The probes in `submit` and in the log are
    // always on and cost ~150 ns an export; these two are a 20 Hz wakeup and a
    // formatted line, which is not worth running on every node forever for an
    // answer nobody is looking at.
    if tracing::enabled!(target: "mira::probe", tracing::Level::DEBUG) {
        spawn_probes();
    }
    let serve = axum::serve(
        http_socket,
        receiver::http_router(recv)
            // Only the query router is timed. Layering the merged router would
            // put OTLP exports and UI asset fetches in the same average, which
            // is a latency number that means nothing.
            .merge(api::router(api.clone()).layer(axum::middleware::from_fn(timed)))
            .merge(mcp::router(api.clone()))
            .merge(alert::router(api))
            .merge(ops_router(std::sync::Arc::clone(&data_dir)))
            .merge(ui::router()),
    )
    .with_graceful_shutdown(stopped(stop_rx));
    // `IntoFuture`, not `Future`, so it cannot be spawned directly.
    let http = tokio::spawn(async move { serve.await });

    // One line, still one line, because it is what gets grepped out of a pod log
    // and pasted into an issue. What it carries is every resolved value whose
    // being wrong is silent: the node id, because it is the only externally
    // visible thing that distinguishes two replicas' blocks and a collision is
    // diagnosed here; the data directory, because a config that resolved
    // `${env:MIRA_DATA}` to nothing writes a week of telemetry into `./data`
    // inside a container and loses it at the next restart; the retention,
    // because it is the one value whose mistake is irreversible — too short and
    // the sweep has already deleted what it was going to delete by the time
    // anyone reads this line; the request cap, because too small is a 413 the
    // sender reports as Mira being broken. The UI URL is spelled out because
    // `http://0.0.0.0:4318/` is not a thing anyone guesses from `http=0.0.0.0:4318`.
    //
    // `retention` is printed in the syntax `--retention` accepts, so the line
    // round-trips: whatever it says can be pasted back in.
    tracing::info!(
        grpc = %grpc_addr, http = %http_addr,
        ui = %format!("http://{http_addr}/"),
        node = %cfg.node, node_id = format!("{node:08x}"),
        data_dir = %data_dir.display(),
        retention = %format!("{}s", cfg.retention.as_secs()),
        max_request_bytes = cfg.max_request_bytes,
        "mira listening"
    );

    let (mut grpc, mut http) = (grpc, http);
    let mut flushers = flushers;
    let mut wedged = false;
    tokio::select! {
        r = &mut grpc => r??,
        r = &mut http => r??,
        // A flusher cannot see its channel close while the listeners still hold
        // an `Ingest`, so one that returns before the stop signal has failed and
        // said why on its way out. Carrying on is what this used to do: the other
        // two signals keep working, that one answers every export with a 503
        // forever, and every probe stays green. A crashloop is the honest shape
        // of "this process cannot store logs" — the orchestrator reports it, and
        // the restart is the recovery for the case that caused it, a data
        // directory that was not there yet.
        _ = first_stopped(&mut flushers) => wedged = true,
        _ = stop_signal => tracing::info!("draining"),
    }

    // Before the drain, and awaited so the cancellation has actually landed:
    // dropping the sampler's `Ingest` clone is what lets the metrics flusher see
    // its channel close. A self-sample lost to the abort is the least important
    // row this process will ever not write.
    if let Some(s) = sampler {
        s.abort();
        let _ = s.await;
    }
    drain(stop, grpc, http, flushers, DRAIN_GRACE).await;
    tracing::info!("stopped");
    if wedged {
        // The other two signals were still drained above; only then is this
        // process allowed to be a failed one.
        return Err("a flusher stopped, so one signal can no longer be stored; \
                    exiting for the supervisor to restart (the cause is logged above)"
            .into());
    }
    Ok(())
}

/// How long a stop is allowed to take before the process leaves without it.
///
/// Bounded below by `pipeline::Config::max_block_age`: step 1 of [`drain`] only
/// makes progress once the block an in-flight export is waiting on seals, and
/// with the listener closed nothing else will grow it. So this is a multiple of
/// that constant, not a number chosen freely.
const DRAIN_GRACE: std::time::Duration = std::time::Duration::from_secs(15);

/// Stop accepting, then let everything already in flight land.
///
/// Three ordered steps, and the order is the whole point.
///
/// 1. Stop accepting, but let in-flight exports run to their acknowledgement.
///    Skipping this is not a data-loss bug — the queued job is still sealed
///    below — it is a *duplicate* bug: the exporter sees a reset, OTLP tells it
///    to retry, and it re-sends data that did get stored. Every rolling restart
///    would double-write whatever was in flight.
/// 2. Await the servers, which drops the last `Ingest` clone per signal and so
///    closes each flusher's channel.
/// 3. A flusher answering a closed channel seals whatever is open, publishes it
///    and acks the waiters (`pipeline::flusher`). Exiting before that lands is
///    what turns step 1's ack into a reset after all.
///
/// ponytail: bounded by `grace` because a hung fsync must not outlive the
/// orchestrator's grace period and become a SIGKILL with no explanation in the
/// log. Whatever had not landed by then is lost, which is why the bound is a
/// last resort and not a policy.
async fn drain<G, H>(
    stop: tokio::sync::watch::Sender<()>,
    grpc: tokio::task::JoinHandle<G>,
    http: tokio::task::JoinHandle<H>,
    flushers: [pipeline::Flushers; 3],
    grace: std::time::Duration,
) {
    let _ = stop.send(());
    let landed = async move {
        let _ = grpc.await;
        let _ = http.await;
        for h in flushers {
            // Awaited unconditionally: `first_stopped` may already have run one
            // of these to completion, and a drained set answers at once rather
            // than panicking the way a twice-polled `JoinHandle` would.
            let _ = h.await;
        }
    };
    if tokio::time::timeout(grace, landed).await.is_err() {
        tracing::warn!(?grace, "did not drain in time; exiting anyway");
    }
}

/// Push everything the log holds that no block claims back through the
/// flushers, before the listeners open.
///
/// Ordering is the reason this is not a background task. A replayed frame keeps
/// the sequence it already has, and `Ingest::replay` puts that sequence back
/// among the log's unpublished before it queues the frame, which is what stops
/// a shard sealing beside it from computing a watermark over it — but that only
/// covers frames already read off disk. A frame still sitting in a segment is
/// in nobody's pending set, so a live export sealing first would claim a
/// watermark past it and the next boot would not replay it. Draining the whole
/// log before the first new export is framed is the blunt guarantee that this
/// never happens. It is also the reason nobody is waiting: the client that sent
/// this either got its answer before the crash or gave up long before this
/// process started.
///
/// One `spawn_blocking` for the whole log, not one per frame. The channel is
/// bounded at `ingest.queue` and the sends are blocking, so the flushers set the pace and
/// a multi-gigabyte log is decoded at the rate it can be sealed rather than
/// into memory all at once.
async fn replay(
    dir: &Path,
    node: u32,
    logs: pipeline::Ingest<mira_proto::collector::logs::v1::ExportLogsServiceRequest>,
    traces: pipeline::Ingest<mira_proto::collector::trace::v1::ExportTraceServiceRequest>,
    metrics: pipeline::Ingest<mira_proto::collector::metrics::v1::ExportMetricsServiceRequest>,
) -> Result<(), Box<dyn std::error::Error>> {
    use mira_core::wal::Signal;

    let dir = dir.to_path_buf();
    let started = std::time::Instant::now();
    let done = tokio::task::spawn_blocking(move || {
        let watermarks = mira_core::block::wal_watermarks(&dir, node)?;
        let mut undecodable = 0u64;
        let out = mira_core::wal::Wal::replay(&dir, node, watermarks, |signal, seq, body| {
            let pushed = match signal {
                Signal::Logs => logs.replay(body, seq),
                Signal::Traces => traces.replay(body, seq),
                Signal::Metrics => metrics.replay(body, seq),
            };
            match pushed {
                Ok(()) | Err(pipeline::Rejected::Failed(_)) => {
                    // A frame that passed its checksum and then would not decode
                    // is one export, and it is already unrecoverable — stopping
                    // the boot over it would lose every frame behind it too.
                    undecodable += u64::from(pushed.is_err());
                    Ok(())
                }
                // The flusher is gone, so nothing after this would land either.
                Err(_) => Err(mira_core::Error::WalCorrupt {
                    path: dir.clone(),
                    why: "the flusher for this signal stopped during replay",
                }),
            }
        })?;
        Ok::<_, mira_core::Error>((out, undecodable))
    })
    .await??;

    let (out, undecodable) = done;
    if undecodable > 0 {
        tracing::error!(
            frames = undecodable,
            "write-ahead log frames passed their checksum and would not decode as OTLP; \
             those exports are gone"
        );
    }
    if out.torn_segments > 0 {
        // Expected after a hard kill, and only after one. Info, not warn: the
        // torn frame is the export that was mid-write when the process died,
        // which the sender never got an acknowledgement for and has retried.
        tracing::info!(
            segments = out.torn_segments,
            "write-ahead log segments ended in a torn frame; that is what a crash looks like"
        );
    }
    if out.replayed > 0 || out.skipped > 0 {
        tracing::info!(
            replayed = out.replayed,
            skipped = out.skipped,
            bytes = out.bytes,
            elapsed_ms = started.elapsed().as_millis() as u64,
            "recovered from the write-ahead log"
        );
    }
    Ok(())
}

/// Resolve as soon as any one flusher has returned.
async fn first_stopped(flushers: &mut [pipeline::Flushers; 3]) {
    let [logs, traces, metrics] = flushers;
    tokio::select! {
        _ = logs => {}
        _ = traces => {}
        _ = metrics => {}
    }
}

/// The three endpoints that answer for the process rather than for the data:
/// liveness, readiness, and everything this node counts about itself.
///
/// Here rather than in `api.rs` because `/health` and `/readyz` must not touch
/// the block directory — no query engine, nothing that can be slow, nothing that
/// can fail for a reason that is not the process's fault. `/api/v1/stats` does
/// open the directory, which is why it is a query-namespace URL and not a probe.
///
/// `/health` and `/readyz` used to be the same handler, on the argument that
/// Mira has no warm-up and no cluster to join, so there is no state in which it
/// is alive and not ready. A node whose disk is full is exactly that state: the
/// process is fine, answers every request, and cannot store a byte. That is the
/// state readiness exists for — it is what takes the node out of the Service's
/// endpoints so the exporters retry somewhere that can — so the two are now
/// different answers. Liveness stays a constant 200: a flusher that stops takes
/// the process with it (`serve_with`), so answering at all *is* the liveness
/// answer, and restarting a node whose volume is full fixes nothing.
fn ops_router(data_dir: std::sync::Arc<PathBuf>) -> Router {
    Router::new()
        .route("/health", get(health))
        .route("/readyz", get(readyz))
        .route("/api/v1/stats", get(stats))
        .with_state(data_dir)
}

/// Liveness, plus what the ingest path has refused so far.
async fn health() -> Response {
    // A flusher that stops takes the process with it (see `serve_with`), so
    // answering at all is the liveness answer. The counters are here so that the
    // probe and the log agree about how much has been refused, and because a
    // shedding node is the case where an operator is looking at exactly this.
    let mut j = mira_core::json::Json::new();
    j.obj(|j| {
        j.key("status");
        j.str("ok");
        for r in &pipeline::REJECTS {
            j.key(r.signal);
            j.obj(|j| {
                j.key("shed");
                j.u64(r.shed.load(Relaxed));
                j.key("failed");
                j.u64(r.failed.load(Relaxed));
            });
        }
    });
    json_ok(j.into_string())
}

/// Readiness: 200 while exports can be made durable, 503 once they cannot.
async fn readyz() -> Response {
    ready(pipeline::stalled())
}

/// Readiness is one question: can this instance accept an export and make it
/// durable? Everything else about the process is `/health`'s.
///
/// Split from the handler so both answers are testable without a disk that has
/// actually filled up — and so the only input is the one fact the decision turns
/// on. The threshold itself is `pipeline::UNREADY_AFTER`, and its whole job is to
/// make this a *sustained* condition: a probe that flips on one failed publish
/// would deregister a node for every EIO and every restart of the volume
/// underneath it, and an endpoint list that changes every ten seconds costs more
/// exports than the node it was protecting.
fn ready(stalled: Option<(&'static str, u64)>) -> Response {
    let mut j = mira_core::json::Json::new();
    j.obj(|j| match stalled {
        None => {
            j.key("status");
            j.str("ok");
        }
        Some((signal, secs)) => {
            j.key("status");
            j.str("unavailable");
            j.key("signal");
            j.str(signal);
            j.key("stalled_s");
            j.u64(secs);
            j.key("reason");
            j.str(
                "this node has not been able to store an export for this signal; \
                 the usual cause is a full or unwritable volume",
            );
        }
    });
    let code = match stalled {
        None => axum::http::StatusCode::OK,
        Some(_) => axum::http::StatusCode::SERVICE_UNAVAILABLE,
    };
    (
        code,
        [(axum::http::header::CONTENT_TYPE, "application/json")],
        j.into_string(),
    )
        .into_response()
}

/// Queries served and what they cost, since start.
///
/// Sum and max rather than a histogram: a p99 needs buckets, buckets need a
/// registry, and a registry is the metrics subsystem this endpoint exists to not
/// be. The mean says whether the engine is in the shape it was benchmarked in
/// and the max says whether anything pathological has run at all, which is the
/// pair an operator acts on; a real p99 is measured at the caller, where the
/// queue in front of Mira is also counted.
static QUERIES: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
static QUERY_NANOS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
static QUERY_MAX_NANOS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// When the process started, so the counters below have a denominator. Forced in
/// `serve_with` rather than left to first touch, or "uptime" would mean "seconds
/// since someone first asked".
static START: std::sync::LazyLock<std::time::Instant> =
    std::sync::LazyLock::new(std::time::Instant::now);

/// Wrapped around the query router only, so OTLP writes and UI asset fetches do
/// not land in the read latency an operator is reading.
async fn timed(req: axum::extract::Request, next: axum::middleware::Next) -> Response {
    let t = std::time::Instant::now();
    let res = next.run(req).await;
    let ns = t.elapsed().as_nanos() as u64;
    QUERIES.fetch_add(1, Relaxed);
    QUERY_NANOS.fetch_add(ns, Relaxed);
    QUERY_MAX_NANOS.fetch_max(ns, Relaxed);
    res
}

/// Everything this node knows about itself, in one document.
///
/// The shape is the argument. Mira is a telemetry backend, so the wrong move is
/// to grow a second one inside it: no exposition format, no registry, no
/// histograms, no scrape endpoint on a second port and no crate to render any of
/// that. What is served is the counters the ingest path already keeps, plus the
/// two facts only the filesystem has, in the same JSON object every `/api/v1`
/// read already answers with — so the UI's `fetch`, an agent and `curl | jq` all
/// speak it without being told. Rates are left to the reader: `uptime_s` is the
/// denominator, and two polls give the interval rate, which is the only rate
/// that is true of *now* rather than of the whole run.
async fn stats(
    axum::extract::State(dir): axum::extract::State<std::sync::Arc<PathBuf>>,
) -> Response {
    // `scan` and `statfs` are filesystem work, and a cold readdir over a week of
    // blocks stalls the OS thread it lands on exactly like a query does. So it
    // goes where queries go.
    let disk = tokio::task::spawn_blocking(move || {
        (
            mira_core::block::free_fraction(&dir).ok(),
            pipeline::SIGNALS.map(|s| mira_core::block::scan(&dir, s).ok().map(|b| b.len() as u64)),
        )
    })
    .await;
    // `null`, not zero, when the filesystem would not answer: "no blocks" and "I
    // could not look" are different operational facts and a zero conflates them.
    let (free, on_disk) = disk.unwrap_or((None, [None; 3]));

    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    // `then_some` and not `then`: one saturating subtraction is cheaper to do
    // than to defer behind a closure, and that closure is a body only a signal
    // with a block already open would ever enter.
    let age = |since: u64| (since != 0).then_some(now.saturating_sub(since));
    let queries = QUERIES.load(Relaxed);

    let mut j = mira_core::json::Json::new();
    j.obj(|j| {
        j.key("uptime_s");
        j.u64(START.elapsed().as_secs());
        j.key("peak_rss_bytes");
        j.u64(peak_rss());
        j.key("free_fraction");
        match free {
            Some(f) => j.f64(f),
            None => j.null(),
        }
        // Zero on any volume that implements write barriers. Non-zero says this
        // one does not, and that the durability promise here is `fsync`'s
        // rather than `F_FULLFSYNC`'s — see `mira_core::sync_all`.
        j.key("degraded_syncs");
        j.u64(mira_core::degraded_syncs());
        j.key("queries");
        j.obj(|j| {
            j.key("count");
            j.u64(queries);
            j.key("mean_ms");
            j.f64(QUERY_NANOS.load(Relaxed) as f64 / queries.max(1) as f64 / 1e6);
            j.key("max_ms");
            j.f64(QUERY_MAX_NANOS.load(Relaxed) as f64 / 1e6);
        });
        j.key("signals");
        j.obj(|j| {
            for (r, blocks) in pipeline::REJECTS.iter().zip(on_disk) {
                j.key(r.signal);
                j.obj(|j| {
                    for (k, v) in [
                        ("shed", r.shed.load(Relaxed)),
                        ("failed", r.failed.load(Relaxed)),
                        ("refused", r.refused.load(Relaxed)),
                        ("blocks_published", r.published.load(Relaxed)),
                        ("rows", r.rows.load(Relaxed)),
                        ("bytes", r.bytes.load(Relaxed)),
                    ] {
                        j.key(k);
                        j.u64(v);
                    }
                    // Absent-as-null again: nothing open, and never stalled, are
                    // both "no age to report" rather than "an age of zero".
                    for (k, v) in [
                        ("blocks_on_disk", blocks),
                        ("open_block_age_s", age(r.open_since.load(Relaxed))),
                        ("stalled_s", age(r.stalled_since.load(Relaxed))),
                    ] {
                        j.key(k);
                        match v {
                            Some(v) => j.u64(v),
                            None => j.null(),
                        }
                    }
                });
            }
        });
    });
    json_ok(j.into_string())
}

/// High-water mark of this process's resident set, in bytes.
///
/// Resident footprint is one of the four axes performance is scored on (section 11),
/// so a node that cannot report it can only be measured from outside with a
/// sampler — which is how the 64-connection run in the last sweep came back
/// with an implausible 26 MiB: `ps` simply missed the peak. `getrusage` cannot
/// miss it, because the kernel keeps the maximum rather than the instant.
///
/// The peak and not the current value because the peak is the number that has
/// to fit in the container's limit, and because it is one portable call:
/// `ru_maxrss` is bytes on macOS and kibibytes on Linux, which is the whole of
/// the platform difference and is why this is not inlined at the call site.
fn peak_rss() -> u64 {
    // SAFETY: `getrusage` writes the whole struct or returns -1; the zeroed
    // value is a valid `rusage` either way.
    let mut ru: libc::rusage = unsafe { std::mem::zeroed() };
    // The return code is not checked, because the zeroed struct is already the
    // answer if it failed: `getrusage(RUSAGE_SELF)` documents only EFAULT and
    // EINVAL, both of which are this call site being wrong rather than anything
    // that can happen at runtime, and a `ru_maxrss` the kernel never touched
    // reports 0 — which is what a "cannot measure" branch would have returned.
    //
    // SAFETY: `ru` is a live, correctly-typed `rusage` the kernel may write in
    // full; there is no other precondition on `RUSAGE_SELF`.
    unsafe { libc::getrusage(libc::RUSAGE_SELF, &mut ru) };
    // `#[cfg]` and not `cfg!`, which compiles both arms and runs one: the arm
    // for the platform this is not built for can never execute, so under `cfg!`
    // it is a line no test can ever reach.
    #[cfg(target_os = "macos")]
    const UNIT: u64 = 1;
    #[cfg(not(target_os = "macos"))]
    const UNIT: u64 = 1024;
    ru.ru_maxrss.max(0) as u64 * UNIT
}

fn json_ok(body: String) -> Response {
    (
        [(axum::http::header::CONTENT_TYPE, "application/json")],
        body,
    )
        .into_response()
}

/// Resolve on the first stop signal.
///
/// SIGTERM matters as much as SIGINT here: it is what every container
/// orchestrator sends, so without this arm a rolling restart kills the process
/// mid-block and the exporters waiting on that block see a reset.
///
/// Two `#[cfg]` bodies and not one with a `cfg!` in it: `cfg!` compiles both,
/// so the arm for the platform this is not built for is a line no test on this
/// platform can ever reach.
#[cfg(unix)]
async fn shutdown() {
    // `expect`, rather than falling back to ^C alone. `signal` fails only for a
    // signal that cannot be caught and on a runtime with no signal driver —
    // both of them this call site being wrong — and a silent fall-back to ^C is
    // a node in a container that nothing but SIGKILL can stop, which is exactly
    // the failure this function exists to prevent, reported nowhere.
    let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        .expect("a SIGTERM handler on the serving runtime");
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {}
        _ = term.recv() => {}
    }
}

/// ^C only: there is no SIGTERM to wait for.
#[cfg(not(unix))]
async fn shutdown() {
    let _ = tokio::signal::ctrl_c().await;
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A command line, through the real tree.
    ///
    /// Whitespace-split, so no test here can pass a value with a space in it —
    /// which is what the `--version` charset test in `update.rs` is for.
    fn argv(s: &str) -> Result<ArgMatches, String> {
        cli()
            .try_get_matches_from(std::iter::once("mira").chain(s.split_whitespace()))
            .map_err(|e| e.to_string())
    }

    /// The server's `Config` from a command line, with clap's refusals and the
    /// config file's flattened into the one `String` the caller asserts on.
    fn load(s: &str) -> Result<Config, String> {
        load_from(&argv(s)?)
    }

    /// `mira offload <verb> …`, reached the way [`run`] reaches it.
    ///
    /// Going through the tree rather than calling [`offload_cmd`] with a verb
    /// name directly is what keeps the two refusals that are now clap's — a
    /// missing verb and an unknown one — inside the same assertion table as the
    /// two that are still this file's.
    fn offload(s: &str) -> Result<(), String> {
        let m = argv(&format!("offload {s}"))?;
        let (verb, sub) = m
            .subcommand_matches("offload")
            .and_then(ArgMatches::subcommand)
            .expect("subcommand_required");
        offload_cmd(verb, sub)
    }

    fn tmp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("mira-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    /// Both stop edges the tests below drive `serve_with` with, as one type.
    ///
    /// `serve_with` is generic over this future, so `ready(())` in one test and
    /// `pending()` in the next compile two whole servers, each running only the
    /// half its own callers reach — including under a coverage run, which counts
    /// each copy separately and so reports lines as unrun that another copy of
    /// the same source ran.
    async fn stop_edge(immediately: bool) {
        if !immediately {
            std::future::pending::<()>().await;
        }
    }

    /// The edge that never fires is still pending once something polls it.
    ///
    /// Less circular than it looks. Three tests below hand `serve_with` this
    /// future to prove the server *keeps* serving, and every one of them would
    /// pass for the wrong reason if it resolved. And `serve_with` only polls
    /// its stop edge when it has nothing else to do, so whether that arm runs
    /// inside any given test run is otherwise a race against the request the
    /// test is making — which is a line of coverage that moves on its own.
    #[tokio::test]
    async fn the_stop_edge_fires_when_it_is_told_to_and_never_otherwise() {
        let zero = std::time::Duration::ZERO;
        let never = tokio::time::timeout(zero, stop_edge(false));
        assert!(never.await.is_err(), "the never-stop edge stopped");
        tokio::time::timeout(zero, stop_edge(true))
            .await
            .expect("the immediate edge did not fire");
    }

    /// The probe tasks wake up and write into the statics the diagnosis reads.
    ///
    /// `start_paused` is what makes this an assertion rather than a stopwatch:
    /// the clock jumps to each deadline with no wall time passing, so two of
    /// the 50 ms periods fit in the 120 ms below with nothing left to a busy
    /// box, and the lag the sampler computes is `~0 - 50 ms` saturated to zero.
    /// `tokio::time::interval` fires its first tick immediately, so the 5 s
    /// dump runs once inside the same window. Mutation check: drop the
    /// `.record(…)` and the count stays where it started; record
    /// `t.elapsed()` without subtracting the period and `max_ns` is no longer
    /// zero, because real time did pass even though virtual time did the
    /// waiting.
    #[tokio::test(start_paused = true)]
    async fn the_probe_tasks_sample_the_runtime_and_dump_it() {
        use std::sync::atomic::Ordering::Relaxed;
        let lag = &mira_core::diag::RUNTIME_LAG;
        let before = lag.n.load(Relaxed);
        spawn_probes();
        tokio::time::sleep(std::time::Duration::from_millis(120)).await;
        assert_eq!(
            lag.n.load(Relaxed),
            before + 2,
            "the lag sampler did not run once per 50 ms period"
        );
        assert_eq!(
            lag.max_ns.load(Relaxed),
            0,
            "a wake that cost no real time was reported as late"
        );
    }

    /// `mira offload list` and `mira offload restore`: a `readdir` and a copy,
    /// and four refusals that happen before either.
    ///
    /// The block here is a directory with the right *name* and nothing valid
    /// inside it, which is the point — neither verb opens a table. The name is
    /// the catalog entry (section 3.2), so a command that needed to read the
    /// bytes to list them would mean the naming scheme was not carrying its
    /// weight. Mutation check: make `restore` unconditional and the last call
    /// overwrites the block it already restored instead of reporting it
    /// present.
    #[test]
    fn offload_list_and_restore_read_the_store_and_nothing_else() {
        let dir = tmp("offload-cmd");
        let (store, data) = (dir.join("cold"), dir.join("data"));
        std::fs::create_dir_all(&data).unwrap();
        let uri = format!("file://{}", store.display());

        // A missing verb, a wrong one, a missing URI and a scheme this binary
        // does not implement. All four are refused before anything is opened.
        for (args, want) in [
            ("", "list"),
            ("sync --offload file:///x", "unrecognized subcommand 'sync'"),
            ("list", "--offload"),
            ("list --offload s3://bucket", "s3://bucket"),
        ] {
            let e = offload(args).unwrap_err();
            assert!(e.contains(want), "`mira offload {args}` said: {e}");
        }

        // An empty store is an empty catalog, not an error: there is no index
        // whose absence could mean something different.
        offload(&format!("list --offload {uri}")).unwrap();

        let name = format!("{:020}-{:020}-{:08x}-{:012}-{:020}", 1_000, 2_000, 7, 1, 0);
        let block = store.join("logs").join("p=0").join(&name);
        std::fs::create_dir_all(&block).unwrap();
        std::fs::write(block.join("logs.arrow"), b"bytes").unwrap();

        offload(&format!("list --offload {uri}")).unwrap();
        assert_eq!(
            std::fs::read_dir(&data).unwrap().count(),
            0,
            "`list` copies nothing"
        );

        let restore = format!("restore --offload {uri} --data-dir {}", data.display());
        offload(&restore).unwrap();
        let local = data.join("logs").join("p=0").join(&name);
        assert_eq!(std::fs::read(local.join("logs.arrow")).unwrap(), b"bytes");

        // Re-runnable, which is what makes it usable from a script that does
        // not track what it has already fetched.
        std::fs::write(local.join("logs.arrow"), b"local edit").unwrap();
        offload(&restore).unwrap();
        assert_eq!(
            std::fs::read(local.join("logs.arrow")).unwrap(),
            b"local edit",
            "a block already present is left alone"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `mira offload push` is `restore` read backwards, and it removes nothing.
    ///
    /// The unlinking version of this verb is the one that cannot be built, and
    /// the assertion that the source survives is that argument rather than
    /// tidiness: a node reads both its WAL watermark and its next block
    /// sequence off the blocks it still holds, so emptying the directory walks
    /// both backwards and the next boot replays an absorbed log under reissued
    /// sequence numbers. Mutation check: make `push` unconditional and the last
    /// call overwrites an archived block with a volume someone has since
    /// edited, which is the one direction a backup verb must never go.
    #[test]
    fn offload_push_copies_out_of_the_data_dir_and_unlinks_nothing() {
        let dir = tmp("offload-push");
        let (store, data) = (dir.join("cold"), dir.join("data"));
        let uri = format!("file://{}", store.display());

        let name = format!("{:020}-{:020}-{:08x}-{:012}-{:020}", 1_000, 2_000, 7, 1, 0);
        let local = data.join("logs").join("p=0").join(&name);
        std::fs::create_dir_all(&local).unwrap();
        std::fs::write(local.join("logs.arrow"), b"bytes").unwrap();

        let push = format!("push --offload {uri} --data-dir {}", data.display());
        offload(&push).unwrap();
        let remote = store.join("logs").join("p=0").join(&name);
        assert_eq!(std::fs::read(remote.join("logs.arrow")).unwrap(), b"bytes");
        assert!(local.exists(), "push copies the block, it does not move it");

        // The same block again: already archived, so a no-op rather than a
        // re-copy, and the verb still succeeds.
        offload(&push).unwrap();

        // Different bytes under the same name is the collision, not a no-op —
        // reporting it as archived is what would let a scale-in delete the
        // volume holding the only copy.
        std::fs::write(local.join("logs.arrow"), b"local edit").unwrap();
        offload(&push).unwrap_err();
        assert_eq!(
            std::fs::read(remote.join("logs.arrow")).unwrap(),
            b"bytes",
            "a block already in the store is left alone"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Flag > file > default, and every flag lands in the field it names.
    ///
    /// clap parses; [`load_from`] still decides which field each value reaches,
    /// and the failure that can produce is the quiet kind: a flag written into
    /// the wrong field starts a server that looks exactly right until someone
    /// reads a block from the wrong directory. It is also what holds the arg ids
    /// to the fields — [`opt`] answers `None` for an id no subcommand declares,
    /// so a typo here would be a flag that parses and then does nothing, and the
    /// assertions below are what catch it.
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

        let c = load(&format!("--config {f}")).unwrap();
        assert_eq!(c.node, "from-file");
        assert_eq!(c.data_dir, PathBuf::from("/from/file"));
        assert_eq!(c.retention, std::time::Duration::from_secs(3 * 3600));
        assert_eq!(c.max_request_bytes, 1 << 20);
        assert_eq!(c.http.port(), 2);

        // The same file, every value overridden.
        let c = load(&format!(
            "--config {f} --node cli --grpc 127.0.0.1:3 --http 127.0.0.1:4 \
             --data-dir /from/cli --retention 30s --max-request-bytes 2MiB"
        ))
        .unwrap();
        assert_eq!(c.node, "cli");
        assert_eq!(c.grpc.port(), 3);
        assert_eq!(c.http.port(), 4);
        assert_eq!(c.data_dir, PathBuf::from("/from/cli"));
        assert_eq!(c.retention, std::time::Duration::from_secs(30));
        assert_eq!(c.max_request_bytes, 2 << 20);

        // Everything the file above does not set, including the two that take
        // no value. Those two are interleaved with the rest rather than listed
        // apart, because a boolean flag that silently swallowed the next
        // argument would still pass every assertion in a block of its own.
        let c = load(
            "--queue 4096 --shards 8 --wal --alerts /from/cli.kyaml \
             --self-telemetry --telemetry-interval 1m",
        )
        .unwrap();
        assert_eq!(c.queue, 4096);
        assert_eq!(c.shards, 8);
        assert_eq!(c.alerts, Some(PathBuf::from("/from/cli.kyaml")));
        assert!(c.self_telemetry);
        assert!(c.wal);
        assert_eq!(c.telemetry_interval, std::time::Duration::from_secs(60));

        // A URI is a string here; the scheme is refused where it is parsed.
        let c = load("--offload file:///srv/cold").unwrap();
        assert_eq!(c.offload.as_deref(), Some("file:///srv/cold"));

        // No arguments at all is the shipped configuration.
        let d = load("").unwrap();
        assert_eq!(d.node, Config::default().node);
        assert!(!d.self_telemetry, "self-telemetry is opt-in");

        for (args, want) in [
            ("--nope", "unexpected argument '--nope'"),
            // Deleted along with the fan-out that never existed. It is an
            // unknown flag now, which is the whole point of deleting it.
            ("--peers a:1", "unexpected argument '--peers'"),
            ("--node", "a value is required for '--node <NAME>'"),
            ("--config", "a value is required for '--config <FILE>'"),
            ("--grpc nope", "invalid value 'nope' for '--grpc <ADDR>'"),
            ("--http nope", "invalid value 'nope' for '--http <ADDR>'"),
            ("--retention nope", "not a duration"),
            ("--max-request-bytes nope", "not a size"),
            ("--queue nope", "not a whole number"),
            ("--queue 0", "at least 1"),
            // `--shards 0` is *not* here: zero is the documented auto value.
            // A negative is rejected rather than wrapped, which is what keeps
            // `-1` from arriving as 18 quintillion shards that `clamp` then
            // silently turns into 16. It is refused a step earlier than the
            // rest of this table — `allow_negative_numbers` is off, so `-1`
            // reads as a flag and never reaches `config::whole`.
            ("--shards nope", "not a whole number"),
            ("--shards -1", "unexpected argument '-1'"),
            ("--telemetry-interval nope", "not a duration"),
            ("--config /no/such/file.yaml", "/no/such/file.yaml"),
        ] {
            let e = load(args).unwrap_err();
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

        // Rendered rather than matched, so a wrong *variant* is a diff on the
        // left of the assertion instead of a panic in a `let ... else`: which
        // of the three answers came back is exactly what is under test.
        let src = |a: &str| {
            let out = argv(&format!("mira {a}")).and_then(|m| {
                tui_source(m.subcommand_matches("mira").expect("the tui subcommand"))
            });
            match out {
                Ok(tui::Source::Local(p)) => format!("local {}", p.display()),
                Ok(tui::Source::Remote(a)) => format!("remote {a}"),
                // First line only: clap's refusals carry the whole usage after a
                // blank line, which is the tree's contract to render, not this
                // test's to assert.
                Err(e) => format!("error {}", e.lines().next().unwrap_or_default()),
            }
        };
        let default_dir = Config::default().data_dir;
        for (args, want) in [
            (format!("--config {f}"), "local /from/file".to_owned()),
            (
                format!("--config {f} --data-dir /from/cli"),
                "local /from/cli".to_owned(),
            ),
            (String::new(), format!("local {}", default_dir.display())),
            // `--addr` wins outright: a remote source has no directory to read.
            ("--addr host:9999".into(), "remote host:9999".to_owned()),
            (
                "--nope".into(),
                "error error: unexpected argument '--nope' found".to_owned(),
            ),
            (
                "--data-dir".into(),
                "error error: a value is required for '--data-dir <PATH>' but none was supplied"
                    .to_owned(),
            ),
            (
                "--config".into(),
                "error error: a value is required for '--config <FILE>' but none was supplied"
                    .to_owned(),
            ),
        ] {
            assert_eq!(src(&args), want, "{args:?}");
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
    /// router itself — and that the one line it logs on the way up carries what
    /// was resolved rather than what was asked for.
    #[tokio::test]
    async fn the_server_starts_from_a_config_and_drains_when_stopped() {
        let dir = tmp("serve");
        let cfg = Config {
            data_dir: dir.join("data"),
            grpc: "127.0.0.1:0".parse().unwrap(),
            http: "127.0.0.1:0".parse().unwrap(),
            // On, with an interval no test run reaches: this asserts that the
            // sampler is started and announced, not what it samples — that is
            // `telemetry`'s own test, and a 15-second default here would make
            // this one's output depend on how slow the machine is. It also makes
            // the elapsed-time assertion below a regression test for the sampler
            // holding the metrics flusher open: detached, this stop took exactly
            // `DRAIN_GRACE` every time.
            self_telemetry: true,
            telemetry_interval: std::time::Duration::from_secs(3600),
            ..Config::default()
        };

        // A file, because `File` is a `MakeWriter` and writes to one land without
        // a flush — a buffer would need a type of its own here.
        let log = dir.join("start.log");
        // Thread-local, so it holds for this test alone — and `#[tokio::test]`
        // is a current-thread runtime, so it holds for everything the server
        // does on it too.
        let _logging = tracing::subscriber::set_default(
            tracing_subscriber::fmt()
                .with_writer(std::fs::File::create(&log).unwrap())
                .without_time()
                // Or every field is wrapped in the escapes that make it readable
                // on a terminal, which this is not.
                .with_ansi(false)
                .finish(),
        );

        let t = std::time::Instant::now();
        serve_with(cfg.clone(), stop_edge(true)).await.unwrap();
        assert!(
            t.elapsed() < std::time::Duration::from_secs(15),
            "timed out"
        );
        // Created, not required to exist: an operator points `--data-dir` at a
        // path and expects the first start to make it.
        assert!(cfg.data_dir.is_dir());

        // `--http 127.0.0.1:0` is a real thing to ask for, and this line is the
        // only place the port that was actually chosen is ever written down. It
        // used to be logged from the requested address, which reads as correct
        // and sends whoever pastes it at a closed port.
        let logged = std::fs::read_to_string(&log).unwrap();
        assert!(logged.contains("mira listening"), "{logged}");
        // No bound port is spelled `0`, in any of the three fields that carry one.
        assert!(!logged.contains("127.0.0.1:0"), "{logged}");
        assert!(logged.contains("ui=http://127.0.0.1:"), "{logged}");
        assert!(
            logged.contains("storing this node's own telemetry"),
            "{logged}"
        );
        // In the syntax `--retention` accepts, so the line round-trips.
        assert!(
            logged.contains(&format!("retention={}s", cfg.retention.as_secs())),
            "{logged}"
        );

        // A port already taken is reported, not survived, and the message names
        // the address — on both listeners. The gRPC half is the one that used to
        // print `transport error` *after* logging that it was listening on it.
        let held = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = held.local_addr().unwrap();
        for taken in [
            Config {
                http: addr,
                ..cfg.clone()
            },
            Config {
                grpc: addr,
                ..cfg.clone()
            },
        ] {
            let e = serve_with(taken, stop_edge(false))
                .await
                .unwrap_err()
                .to_string();
            assert!(e.contains(&addr.to_string()), "{e}");
            assert!(e.to_lowercase().contains("address"), "{e}");
        }

        // A data directory that cannot be made says which path and why, rather
        // than `File exists (os error 17)`.
        let file = dir.join("a-file");
        std::fs::write(&file, b"").unwrap();
        let e = serve_with(
            Config {
                data_dir: file.clone(),
                ..cfg
            },
            stop_edge(false),
        )
        .await
        .unwrap_err()
        .to_string();
        assert!(e.contains(&file.display().to_string()), "{e}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The two things `serve_with` does before anything is bound or mapped, and
    /// the order they are in.
    ///
    /// A rules file is read first *because* nothing has been created yet: a
    /// deployment that believes it is being paged and is not is the worst
    /// possible half-start, so it has to be a message and an exit rather than a
    /// node that is up with alerting quietly off. The log directory is opened
    /// next, and a failure there has to name the path — `File exists` on its
    /// own sends whoever reads it looking for a bug in Mira.
    #[tokio::test]
    async fn a_node_refuses_to_start_on_a_rules_file_or_a_log_it_cannot_open() {
        let dir = tmp("boot-guards");
        let rules = dir.join("alerts.kyaml");
        let cfg = |alerts: Option<PathBuf>, data_dir: PathBuf| Config {
            data_dir,
            grpc: "127.0.0.1:0".parse().unwrap(),
            http: "127.0.0.1:0".parse().unwrap(),
            alerts,
            ..Config::default()
        };

        // A rules file that parses is loaded and the node comes up with it.
        std::fs::write(
            &rules,
            r#"{ "rules": [ { "name": "any-log", "over": "1m", "when": "count >= 1",
                              "query": { "signal": "logs" } } ] }"#,
        )
        .unwrap();
        serve_with(
            cfg(Some(rules.clone()), dir.join("ok")),
            std::future::ready(()),
        )
        .await
        .expect("a node with rules starts");

        // One that does not is a refusal naming the file, before the data
        // directory it was given has been created.
        std::fs::write(&rules, "{ rules: nope }").unwrap();
        let never_made = dir.join("not-made");
        let e = serve_with(
            cfg(Some(rules.clone()), never_made.clone()),
            std::future::pending(),
        )
        .await
        .unwrap_err()
        .to_string();
        assert!(e.contains("alerts.kyaml"), "{e}");
        assert!(!never_made.exists(), "the boot got past the rules file");

        // `.wal` occupied by a regular file: `create_dir_all` cannot make the
        // log directory, and an unopenable log is a start that would have
        // acknowledged exports it could not recover.
        let wal_blocked = dir.join("wal-blocked");
        std::fs::create_dir_all(&wal_blocked).unwrap();
        std::fs::write(wal_blocked.join(".wal"), b"not a directory").unwrap();
        let cfg = Config {
            wal: true,
            ..cfg(None, wal_blocked.clone())
        };
        let e = serve_with(cfg, std::future::pending())
            .await
            .unwrap_err()
            .to_string();
        assert!(e.contains(".wal"), "{e}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A flusher that cannot start takes the process down with it.
    ///
    /// The alternative is the failure nobody notices: two signals keep working,
    /// the third answers every export with a 503 until someone restarts it, and
    /// the process stays up and green throughout. Here `logs` is a regular file,
    /// so the logs flusher cannot scan its directory and returns immediately.
    #[tokio::test]
    async fn a_flusher_that_cannot_start_stops_the_server() {
        let dir = tmp("wedged");
        std::fs::write(dir.join("logs"), b"not a directory").unwrap();
        let cfg = Config {
            data_dir: dir.clone(),
            grpc: "127.0.0.1:0".parse().unwrap(),
            http: "127.0.0.1:0".parse().unwrap(),
            // Off, or the replay reaches the same broken directory first and
            // this stops being a test about the flusher. That boot order is
            // itself correct — a log that cannot be read is a worse thing to
            // start over than a directory that cannot be scanned — but it is
            // not what is being asserted here.
            wal: false,
            ..Config::default()
        };
        // `pending`, so the only thing that can end this is the flusher.
        let e = serve_with(cfg, stop_edge(false))
            .await
            .unwrap_err()
            .to_string();
        assert!(e.contains("flusher"), "{e}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// [`drain`]'s last resort. The happy path is
    /// `the_server_starts_from_a_config_and_drains_when_stopped`; this is the
    /// branch that one can never take, because there the flushers do land.
    /// A real `grace` rather than a paused clock: `tokio`'s `test-util` is a
    /// feature the workspace does not carry, and a millisecond costs less than
    /// carrying it.
    #[tokio::test]
    async fn a_drain_that_never_lands_leaves_anyway() {
        let (stop, rx) = tokio::sync::watch::channel(());
        let wedged = || tokio::spawn(std::future::pending::<()>());
        drain(
            stop,
            wedged(),
            wedged(),
            std::array::from_fn(|_| pipeline::Flushers::wedged()),
            std::time::Duration::from_millis(1),
        )
        .await;
        // It returned, which is the assertion — an unbounded `drain` would still
        // be awaiting the first handle. The listeners were told to stop before
        // the wait began, so a caller that gave up still stopped accepting.
        assert!(
            rx.has_changed().is_err(),
            "drain owns the sender to the end"
        );
    }

    async fn text(r: Response) -> (axum::http::StatusCode, String) {
        let (parts, body) = r.into_parts();
        let body = axum::body::to_bytes(body, 1 << 20).await.unwrap();
        (parts.status, String::from_utf8(body.to_vec()).unwrap())
    }

    /// The probe answers, and it answers with the numbers an operator wants
    /// while ingest is unhappy — the same ones the warn! lines count.
    #[tokio::test]
    async fn health_reports_every_signals_rejections() {
        let (status, body) = text(health().await).await;
        assert_eq!(status, axum::http::StatusCode::OK);
        assert!(body.starts_with(r#"{"status":"ok""#), "{body}");
        for signal in pipeline::SIGNALS {
            assert!(body.contains(&format!(r#""{signal}":{{"shed":"#)), "{body}");
        }
    }

    /// Readiness is no longer liveness under a second name.
    ///
    /// The state that broke the old argument is a node that answers every
    /// request and cannot store a byte: it stayed 200 and stayed in the
    /// Service's endpoints while NACKing 100% of exports. A 503 is what moves
    /// that traffic to a replica that can take it, and the body says which
    /// signal and for how long so the reason is in the probe's own log.
    #[tokio::test]
    async fn readiness_fails_only_once_a_signal_has_been_unable_to_store() {
        let (status, body) = text(ready(None)).await;
        assert_eq!(status, axum::http::StatusCode::OK);
        assert_eq!(body, r#"{"status":"ok"}"#);

        let (status, body) = text(ready(Some(("logs", 300)))).await;
        assert_eq!(status, axum::http::StatusCode::SERVICE_UNAVAILABLE);
        assert!(body.contains(r#""signal":"logs""#), "{body}");
        assert!(body.contains(r#""stalled_s":300"#), "{body}");

        // The live wiring, which is healthy in a test binary: this is the only
        // thing that proves `/readyz` is not still `/health`'s handler.
        let (status, _) = text(readyz().await).await;
        assert_eq!(status, axum::http::StatusCode::OK);
    }

    /// The self-telemetry an agent or an operator reads instead of six numbers
    /// and a log stream. Every field is asserted by name because the shape is
    /// the API: this is what a dashboard and an MCP client bind to.
    #[tokio::test]
    async fn stats_reports_what_this_node_is_doing_with_its_disk() {
        let dir = tmp("stats");
        let (status, body) =
            text(stats(axum::extract::State(std::sync::Arc::new(dir.clone()))).await).await;
        assert_eq!(status, axum::http::StatusCode::OK);
        for key in [
            r#""uptime_s":"#,
            r#""free_fraction":"#,
            r#""queries":{"count":"#,
            r#""mean_ms":"#,
            r#""max_ms":"#,
        ] {
            assert!(body.contains(key), "{key} missing from {body}");
        }
        for signal in pipeline::SIGNALS {
            assert!(body.contains(&format!(r#""{signal}":{{"shed":"#)), "{body}");
        }
        for key in [
            "refused",
            "blocks_published",
            "rows",
            "bytes",
            "blocks_on_disk",
            "open_block_age_s",
            "stalled_s",
        ] {
            assert!(body.contains(&format!(r#""{key}":"#)), "{key}: {body}");
        }
        // An empty directory is zero blocks, not an unreadable one, and nothing
        // is open in a process with no flusher: `null` says so without a zero
        // that would read as a real measurement.
        assert!(body.contains(r#""blocks_on_disk":0"#), "{body}");
        assert!(body.contains(r#""open_block_age_s":null"#), "{body}");
        // A fraction, not a byte count and not a percentage — read as the
        // number it is rather than as the text it happened to render to, or a
        // volume with everything free (`"free_fraction":1`, which a fresh tmpfs
        // or a scratch CI disk really does report) fails a test about the
        // shape of the document.
        let free = api::parse(&body).expect("the document is KYAML")["free_fraction"]
            .as_f64()
            .expect("a readable volume reports a fraction");
        assert!(
            free > 0.0 && free <= 1.0,
            "free_fraction is a fraction of the volume: {free}"
        );

        // A path the filesystem will not answer for — a detached volume, a
        // `subPath` that vanished. "I could not look" is not "the disk is
        // empty": a zero here reads as a volume with no room left and takes a
        // healthy node out of rotation, so the answer is `null` and the
        // endpoint still returns 200 rather than failing the whole document.
        let gone = std::sync::Arc::new(dir.join("no-such-volume"));
        let (status, body) = text(stats(axum::extract::State(gone)).await).await;
        assert_eq!(status, axum::http::StatusCode::OK);
        assert!(body.contains(r#""free_fraction":null"#), "{body}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The peak resident set is one of the four axes performance is scored on,
    /// so it has to be a real measurement in the units the field name claims.
    ///
    /// The unit is the whole hazard: `ru_maxrss` is bytes on macOS and
    /// kibibytes on Linux, and getting it backwards is not a visible failure —
    /// it is a number 1024× out that a dashboard renders without complaint. A
    /// test process is comfortably inside these bounds on either platform;
    /// either mistake leaves it outside one of them.
    #[test]
    fn the_peak_resident_set_is_reported_in_bytes() {
        let rss = peak_rss();
        assert!(rss > 1 << 20, "{rss} bytes is below a running process");
        assert!(rss < 100 << 30, "{rss} bytes is a unit mistake, not an RSS");
    }

    /// `mira mira --data-dir` maps blocks with no server in front of it, so it
    /// needs the guards the server runs or it answers the one question it exists
    /// for with a lie: `block::scan` reads ENOENT as an empty directory, and a
    /// detached volume comes out as `0/0 blocks` — the same screen a healthy
    /// empty store draws.
    #[test]
    fn the_tui_refuses_a_data_directory_the_server_would_have_refused() {
        let dir = tmp("tui-guard");
        let local = |p: &Path| tui::Source::Local(p.to_path_buf());
        // A real directory passes both guards, empty or not: an empty block
        // directory is a normal thing to point this at.
        check_source(&local(&dir), true).unwrap();

        for bad in [dir.join("nope"), {
            let f = dir.join("a-file");
            std::fs::write(&f, b"").unwrap();
            f
        }] {
            let e = check_source(&local(&bad), true).unwrap_err().to_string();
            assert!(e.contains(&bad.display().to_string()), "{e}");
            assert!(e.contains("not a directory"), "{e}");
            // Off a terminal the same path is accepted, because `tui::run`'s
            // own refusal comes first and is the more useful answer: this
            // diagnosis is about a screen that was never going to be drawn.
            check_source(&local(&bad), false).unwrap();
            // And a remote source is never this path at all, however unusable
            // the same string would be as a directory: those blocks are mapped
            // by the server on the other end, which ran these guards itself.
            let remote = tui::Source::Remote(bad.display().to_string());
            check_source(&remote, true).unwrap();
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Boot recovery end to end: every frame no block claims goes back through
    /// its own signal's flusher, before anything is served.
    ///
    /// The three signals share one log, so a replay that read the frame header
    /// wrongly would hand a span to the log encoder. And a frame that passed its
    /// checksum and then will not decode has to be survivable, because stopping
    /// on it would abandon every frame behind it — one dead export instead of
    /// the whole log. It stays unclaimed until a later block of that signal
    /// covers it, which is the next export, so it does not pin the log either.
    #[tokio::test]
    async fn a_boot_replays_every_frame_no_block_claims() {
        use mira_core::wal::Signal;
        use mira_proto::collector::metrics::v1::ExportMetricsServiceRequest;
        use mira_proto::collector::trace::v1::ExportTraceServiceRequest;
        use mira_proto::metrics::v1::metric::Data;
        use mira_proto::metrics::v1::number_data_point::Value as NumValue;
        use mira_proto::metrics::v1::{
            Gauge, Metric, NumberDataPoint, ResourceMetrics, ScopeMetrics,
        };
        use mira_proto::trace::v1::{ResourceSpans, ScopeSpans, Span};
        use prost::Message as _;

        let dir = tmp("replay");
        let node = mira_core::block::node_id("replaynode");
        let spans = ExportTraceServiceRequest {
            resource_spans: vec![ResourceSpans {
                scope_spans: vec![ScopeSpans {
                    spans: vec![Span {
                        trace_id: vec![0x11; 16].into(),
                        span_id: vec![0x22; 8].into(),
                        name: "GET /".into(),
                        start_time_unix_nano: 3_000,
                        end_time_unix_nano: 3_500,
                        ..Default::default()
                    }],
                    ..Default::default()
                }],
                ..Default::default()
            }],
        };
        let points = ExportMetricsServiceRequest {
            resource_metrics: vec![ResourceMetrics {
                scope_metrics: vec![ScopeMetrics {
                    metrics: vec![Metric {
                        name: "process.cpu".into(),
                        data: Some(Data::Gauge(Gauge {
                            data_points: vec![NumberDataPoint {
                                time_unix_nano: 4_000,
                                value: Some(NumValue::AsDouble(0.5)),
                                ..Default::default()
                            }],
                        })),
                        ..Default::default()
                    }],
                    ..Default::default()
                }],
                ..Default::default()
            }],
        };

        // A crash: four frames appended, nothing sealed. The last is not an OTLP
        // export — field 1 tagged as a varint, where the schema has a message.
        {
            let wal = mira_core::wal::Wal::open(&dir, node).unwrap();
            let logs = e2e::logs_export("checkout", 2_000, 4).encode_to_vec();
            wal.append(Signal::Logs, &logs).unwrap();
            wal.append(Signal::Traces, &spans.encode_to_vec()).unwrap();
            wal.append(Signal::Metrics, &points.encode_to_vec())
                .unwrap();
            wal.append(Signal::Logs, b"\x08").unwrap();
        }

        let wal = std::sync::Arc::new(mira_core::wal::Wal::open(&dir, node).unwrap());
        let pcfg = std::sync::Arc::new(pipeline::Config {
            data_dir: dir.clone(),
            node,
            wal: Some(wal),
            max_block_age: std::time::Duration::from_millis(50),
            ..Default::default()
        });
        let (logs, _ol, h_logs) = pipeline::spawn::<mira_core::logs::LogsBuilder>(&pcfg);
        let (traces, _ot, h_traces) = pipeline::spawn::<mira_core::traces::TracesBuilder>(&pcfg);
        let (metrics, _om, h_metrics) =
            pipeline::spawn::<mira_core::metrics::MetricsBuilder>(&pcfg);

        replay(&dir, node, logs.clone(), traces.clone(), metrics.clone())
            .await
            .unwrap();
        drop((logs, traces, metrics));
        for h in [h_logs, h_traces, h_metrics] {
            h.await.unwrap();
        }

        for signal in pipeline::SIGNALS {
            let published = mira_core::block::scan(&dir, signal).unwrap();
            assert_eq!(published.len(), 1, "{signal} did not store its frame");
        }
        // A block claims the first sequence of its signal that nothing covers,
        // not one past the frame it happens to hold — under shards the two are
        // different numbers, and only the first is safe to skip on the next
        // boot. Here nothing of any signal is left outstanding: frames 0, 1 and
        // 2 are in blocks, and frame 3 was retired when it failed to decode. So
        // all three claim the whole log, and the next boot replays nothing.
        //
        // Sequence 3 being dropped rather than pinned is the point of that
        // retirement: a frame that will never decode must not hold a watermark,
        // or every frame published behind it is replayed on every boot forever.
        assert_eq!(
            mira_core::block::wal_watermarks(&dir, node).unwrap(),
            [4, 4, 4]
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// What a boot after a hard kill says out loud.
    ///
    /// Recovery is silent work on a path nobody watches, so the report is the
    /// only way an operator learns that this start replayed anything — and the
    /// torn tail is the line that distinguishes "we crashed" from "a byte on
    /// this volume went bad", which are different pages. Both are field
    /// expressions inside `tracing` macros, which means they are only ever
    /// *evaluated* under a subscriber: without one here, a typo in them ships.
    #[tokio::test]
    async fn a_boot_after_a_hard_kill_reports_the_torn_tail_and_what_it_recovered() {
        use mira_core::wal::Signal;
        use prost::Message as _;

        let dir = tmp("torn-replay");
        let node = mira_core::block::node_id("tornnode");
        {
            let wal = mira_core::wal::Wal::open(&dir, node).unwrap();
            for i in 0..2 {
                let body = e2e::logs_export("checkout", 2_000 + i, 1).encode_to_vec();
                wal.append(Signal::Logs, &body).unwrap();
            }
            wal.sync().unwrap();
        }
        // Four bytes off the tail: the last frame loses its checksum, which is
        // exactly what a process killed mid-write leaves behind.
        let seg = dir
            .join(".wal")
            .join(format!("{node:08x}-{:020}.wal", 0u64));
        let len = std::fs::metadata(&seg).unwrap().len();
        std::fs::OpenOptions::new()
            .write(true)
            .open(&seg)
            .unwrap()
            .set_len(len - 4)
            .unwrap();

        let pcfg = std::sync::Arc::new(pipeline::Config {
            data_dir: dir.clone(),
            node,
            max_block_age: std::time::Duration::from_millis(50),
            ..Default::default()
        });
        let (logs, _ol, h_logs) = pipeline::spawn::<mira_core::logs::LogsBuilder>(&pcfg);
        let (traces, _ot, h_traces) = pipeline::spawn::<mira_core::traces::TracesBuilder>(&pcfg);
        let (metrics, _om, h_metrics) =
            pipeline::spawn::<mira_core::metrics::MetricsBuilder>(&pcfg);

        let (guard, log) = e2e::capture();
        replay(&dir, node, logs.clone(), traces.clone(), metrics.clone())
            .await
            .expect("a torn tail is a recovery, not a refusal");
        drop(guard);

        let text = log.text();
        assert!(text.contains("torn frame"), "{text}");
        assert!(text.contains("segments=1"), "{text}");
        assert!(
            text.contains("recovered from the write-ahead log"),
            "{text}"
        );
        // The whole frame before the tear, and only it.
        assert!(text.contains("replayed=1"), "{text}");
        assert!(text.contains("elapsed_ms="), "{text}");

        drop((logs, traces, metrics));
        for h in [h_logs, h_traces, h_metrics] {
            h.await.unwrap();
        }
        assert_eq!(mira_core::block::scan(&dir, "logs").unwrap().len(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A replay that cannot deliver stops the boot.
    ///
    /// The alternative is the quietest possible data loss: the flusher for one
    /// signal is gone, every frame for it is dropped on the floor, the node
    /// finishes starting and answers `/readyz` with a 200 — and the log those
    /// frames were in is truncated by the next block that gets published. So
    /// the first undeliverable frame ends the boot with an error naming the
    /// directory, and the supervisor restarts into a working process.
    #[tokio::test]
    async fn a_replay_with_nowhere_to_put_a_frame_refuses_to_finish_the_boot() {
        use mira_core::wal::Signal;
        use prost::Message as _;

        let dir = tmp("replay-closed");
        let node = mira_core::block::node_id("closednode");
        {
            let wal = mira_core::wal::Wal::open(&dir, node).unwrap();
            wal.append(
                Signal::Logs,
                &e2e::logs_export("checkout", 2_000, 1).encode_to_vec(),
            )
            .unwrap();
            wal.sync().unwrap();
        }

        let pcfg = std::sync::Arc::new(pipeline::Config {
            data_dir: dir.clone(),
            node,
            ..Default::default()
        });
        let (logs, _ol, mut h_logs) = pipeline::spawn::<mira_core::logs::LogsBuilder>(&pcfg);
        let (traces, _ot, _ht) = pipeline::spawn::<mira_core::traces::TracesBuilder>(&pcfg);
        let (metrics, _om, _hm) = pipeline::spawn::<mira_core::metrics::MetricsBuilder>(&pcfg);
        // The one failure mode a replay cannot route around: the receiving end
        // of this signal's channel is gone, so no retry and no other signal
        // makes the frame land.
        h_logs.abort();
        let _ = h_logs.await;

        let e = replay(&dir, node, logs, traces, metrics)
            .await
            .expect_err("a frame with nowhere to go must stop the boot");
        let e = e.to_string();
        assert!(e.contains("flusher"), "{e}");
        assert!(e.contains(&dir.display().to_string()), "{e}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
