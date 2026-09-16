//! The binary as an operator meets it: argv in, exit code out, SIGTERM in the
//! middle.
//!
//! `main`, `run` and `shutdown` are only reachable by exec'ing the thing. A
//! unit test inside the bin crate never calls its own `main`, `-h` and `-V` end
//! the run rather than returning a value, and a signal handler needs a process
//! to send a signal to. Coverage still counts: the child inherits
//! `LLVM_PROFILE_FILE` and writes a profraw that gets merged.

use std::io::{Read, Write};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use prost::Message;

use mira_proto::collector::logs::v1::ExportLogsServiceRequest;
use mira_proto::collector::metrics::v1::ExportMetricsServiceRequest;
use mira_proto::collector::trace::v1::ExportTraceServiceRequest;
use mira_proto::common::v1::{AnyValue, InstrumentationScope, KeyValue, any_value};
use mira_proto::logs::v1::{LogRecord, ResourceLogs, ScopeLogs};
use mira_proto::metrics::v1::metric::Data;
use mira_proto::metrics::v1::number_data_point::Value as NumValue;
use mira_proto::metrics::v1::{Gauge, Metric, NumberDataPoint, ResourceMetrics, ScopeMetrics};
use mira_proto::resource::v1::Resource;
use mira_proto::trace::v1::{ResourceSpans, ScopeSpans, Span};

const MIRA: &str = env!("CARGO_BIN_EXE_mira");

fn mira(args: &[&str]) -> (Option<i32>, String, String) {
    let out = Command::new(MIRA).args(args).output().unwrap();
    (
        out.status.code(),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

/// Every argv that answers and exits without serving anything.
///
/// The split between the two streams is the point of most of these: usage on
/// stdout so `mira -h | less` works, errors on stderr so a pipe stays clean,
/// and an exit code that a supervisor can read.
#[test]
fn the_command_line_answers_before_it_starts_a_server() {
    let (code, out, err) = mira(&["--version"]);
    assert_eq!((code, err.as_str()), (Some(0), ""));
    assert_eq!(out.trim(), format!("mira {}", env!("CARGO_PKG_VERSION")));

    for flag in ["-h", "--help"] {
        let (code, out, _) = mira(&[flag]);
        assert_eq!(code, Some(0), "{flag}");
        assert!(out.contains("Usage: mira"), "{flag}: {out:?}");
    }

    // `mira: ` and nothing on stdout, and exit 1 rather than the 2 clap would
    // take on its own. Returning the error from `main` instead would print it
    // with `Debug`, which is a struct dump; letting clap exit would give a
    // supervisor a different code depending on which layer refused.
    let (code, out, err) = mira(&["--nope"]);
    assert_eq!(code, Some(1));
    assert!(
        err.starts_with("mira: unexpected argument '--nope'"),
        "{err:?}"
    );
    assert_eq!(out, "");

    // The TUI arm takes its own --help and reaches neither the tracing
    // subscriber nor the runtime: both write to the terminal it is about to
    // take over, and one stray line lands in the middle of a frame.
    let (code, out, _) = mira(&["mira", "--help"]);
    assert_eq!(code, Some(0));
    assert!(out.contains("Usage: mira mira"), "{out:?}");

    // `tui` is the alias, and it refuses a stdin that is not a terminal rather
    // than spraying escape codes down whatever pipe it was given.
    let (code, _, err) = mira(&["tui", "--data-dir", "/nonexistent"]);
    assert_eq!(code, Some(1));
    assert!(err.contains("needs stdin and stdout on a tty"), "{err:?}");

    // A bad flag on that arm is reported against that arm's flag set, not the
    // server's: `--grpc` is a real flag of the binary and not of this mode.
    let (code, _, err) = mira(&["mira", "--grpc", "127.0.0.1:0"]);
    assert_eq!(code, Some(1));
    assert!(err.contains("unexpected argument '--grpc'"), "{err:?}");

    // Every subcommand, not just the bare binary. `proxy` and `offload` reach
    // the config parser, which knows nothing about `--help` and reported it as
    // an unknown flag — so the two commands whose usage somebody is most likely
    // to ask for were the two that answered with an error and exit 1.
    for cmd in ["proxy", "offload"] {
        for flag in ["-h", "--help"] {
            let (code, out, err) = mira(&[cmd, flag]);
            assert_eq!((code, err.as_str()), (Some(0), ""), "mira {cmd} {flag}");
            assert!(
                out.contains(&format!("Usage: mira {cmd}")),
                "mira {cmd} {flag}: {out:?}"
            );
        }
    }

    // `offload` on its own is the whole verb list rather than a bare error,
    // because the thing somebody typing it does not know is which verbs exist.
    let (code, out, err) = mira(&["offload"]);
    assert_eq!((code, out.as_str()), (Some(1), ""));
    for verb in ["list", "restore", "push"] {
        assert!(err.contains(verb), "mira offload: {err:?}");
    }

    // With a verb, past clap and into the verb itself — the one refusal of the
    // four that is still `offload_cmd`'s, and the only way to reach the dispatch
    // in `run` that hands it the two halves clap split off.
    let (code, out, err) = mira(&["offload", "list"]);
    assert_eq!((code, out.as_str()), (Some(1), ""));
    assert!(
        err.starts_with("mira: ") && err.contains("--offload"),
        "{err:?}"
    );

    // `update` keeps `--version` for itself: it *takes a value*, the tag to
    // install. The root's version flag is not propagated into a subcommand that
    // declares its own, which is what stops `mira update --version v0.1.0`
    // being a print of this binary's version and an exit.
    let (code, out, _) = mira(&["update", "--help"]);
    assert_eq!(code, Some(0));
    assert!(out.contains("Usage: mira update"), "{out:?}");
    let (code, out, _) = mira(&["update", "--version", "v0.1.0", "--dry-run"]);
    assert_eq!(code, Some(0));
    assert!(out.contains("v0.1.0"), "{out:?}");

    // And `mira completion <shell>` is a script for that shell, off the same
    // tree — so a flag added above is completable without a second edit here.
    let (code, out, err) = mira(&["completion", "bash"]);
    assert_eq!((code, err.as_str()), (Some(0), ""));
    assert!(out.contains("--self-telemetry"), "{out:?}");
    let (code, _, err) = mira(&["completion", "tcsh"]);
    assert_eq!(code, Some(1));
    assert!(err.contains("invalid value 'tcsh'"), "{err:?}");
}

/// A SIGTERM is a rolling restart, not a crash.
///
/// It is what every container orchestrator sends, and it takes a different arm
/// of `shutdown` than ^C does. If that arm is missing the process dies
/// mid-block: the exporters waiting on that block see a reset, OTLP tells them
/// to retry, and the restart double-writes whatever was in flight.
///
/// So: start the real binary, put one export through the real listener, then
/// SIGTERM it and require both a zero exit and the block on disk.
#[test]
fn a_sigterm_stops_the_server_with_the_data_on_disk() {
    let dir = std::env::temp_dir().join(format!("mira-cli-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);

    let (mut child, log) = spawn_logged(&[
        "--grpc",
        "127.0.0.1:0",
        "--http",
        "127.0.0.1:0",
        "--data-dir",
        dir.to_str().unwrap(),
    ]);
    let logged = |marker: &str| logged(&log, marker);

    // Port 0 means the kernel picked it, and the only place it is written down
    // is the line the server logs on the way up — which is also the point of
    // logging it.
    let up = logged("mira listening");
    let port = up.then(|| port_of(&log)).flatten();

    let posted = port.map(|p| post(p, "/v1/logs", PROTOBUF, &one_log().encode_to_vec()));
    // SIGTERM rather than `child.kill`, which is SIGKILL and proves nothing.
    // SAFETY: `kill` dereferences nothing, so the only hazard is signalling the
    // wrong process. `child` has not been waited on yet — `child.wait()` is
    // below — so the kernel still holds its zombie slot and the pid cannot have
    // been recycled onto someone else's process.
    let signalled = unsafe { libc::kill(child.id() as i32, libc::SIGTERM) } == 0;
    let stopped = signalled && logged("stopped");
    if !stopped {
        let _ = child.kill();
    }
    let status = child.wait().unwrap();

    let tail = log.lock().unwrap().clone();
    assert!(up, "never came up:\n{tail}");
    assert!(
        posted
            .as_deref()
            .is_some_and(|r| r.starts_with("HTTP/1.1 200")),
        "export rejected: {posted:?}\n{tail}"
    );
    assert!(stopped, "did not drain after SIGTERM:\n{tail}");
    assert!(status.success(), "exited {status}:\n{tail}");

    // One export, one block, and the block is a directory of Arrow files —
    // `data/logs/p=<hour>/<name>/logs.arrow`.
    let block = first(&first(&dir.join("logs")));
    assert!(block.join("logs.arrow").is_file(), "{block:?}");
    let _ = std::fs::remove_dir_all(&dir);
}

/// `mira proxy` as an operator starts it: a second process of the same binary,
/// no data directory under it, and the storage node behind it reached over a
/// real socket.
///
/// The merge itself is level 3 — the proxy's router in-process, in `e2e.rs`.
/// What only a subprocess can say is that the *subcommand* is wired: that it
/// parses its own flags, refuses an empty replica list before binding anything,
/// comes up on the port it was given, and that the storage node refuses the
/// same key so a missing `proxy` word cannot start half a deployment.
#[test]
fn the_proxy_subcommand_serves_the_node_behind_it_and_the_node_refuses_to_be_one() {
    // The likely typo, and the reason `proxy.replicas` is checked on both sides:
    // a storage node that quietly ignored it would look like a running proxy.
    // The flag is refused by the tree — `--replica` is `mira proxy`'s and the
    // server does not declare it — and the key by the server, which is the case
    // that survives one config file being shared by both deployments.
    let (code, out, err) = mira(&["--replica", "http://127.0.0.1:4318"]);
    assert_eq!((code, out.as_str()), (Some(1), ""));
    assert!(err.contains("unexpected argument '--replica'"), "{err:?}");

    let shared = std::env::temp_dir().join(format!("mira-cli-shared-{}.yaml", std::process::id()));
    std::fs::write(
        &shared,
        "{ proxy: { replicas: \"http://127.0.0.1:4318\" } }",
    )
    .unwrap();
    let (code, out, err) = mira(&["--config", shared.to_str().unwrap()]);
    assert_eq!((code, out.as_str()), (Some(1), ""));
    assert!(err.contains("is read by `mira proxy`"), "{err:?}");
    let _ = std::fs::remove_file(&shared);

    // And a proxy with nothing to proxy fails before it binds, with the usage,
    // rather than serving 502s to whoever finds it.
    let (code, _, err) = mira(&["proxy", "--http", "127.0.0.1:0"]);
    assert_eq!(code, Some(1));
    assert!(err.contains("--replica"), "{err:?}");

    // A value the parser rejects, which is a different refusal from the one
    // above: that one is the proxy saying it has no replicas after it parsed,
    // this one is the shared `--http`, and `mira proxy` has to carry its errors
    // out too rather than start on a default the operator did not ask for.
    let (code, _, err) = mira(&["proxy", "--http", "not-an-address"]);
    assert_eq!(code, Some(1));
    assert!(
        err.contains("invalid value 'not-an-address' for '--http <ADDR>'"),
        "{err:?}"
    );

    let dir = std::env::temp_dir().join(format!("mira-cli-proxy-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let (mut node, nlog) = spawn_logged(&[
        "--grpc",
        "127.0.0.1:0",
        "--http",
        "127.0.0.1:0",
        "--data-dir",
        dir.to_str().unwrap(),
    ]);
    let replica = logged(&nlog, "mira listening")
        .then(|| port_of(&nlog))
        .flatten()
        .map(|p| format!("http://127.0.0.1:{p}"));

    let (mut px, plog) = spawn_logged(&[
        "proxy",
        "--http",
        "127.0.0.1:0",
        "--replica",
        replica.as_deref().unwrap_or("http://127.0.0.1:1"),
    ]);
    let up = logged(&plog, "mira proxy listening");
    let port = up.then(|| port_of(&plog)).flatten();

    // Through the proxy both ways: the export it splits and re-encodes, and the
    // read it fans out and merges — over one replica, which is the arithmetic
    // that has to work before two do.
    let exported = port.map(|p| post(p, "/v1/logs", PROTOBUF, &one_log().encode_to_vec()));
    let read = port.map(|p| {
        post(
            p,
            "/api/v1/query",
            "application/json",
            br#"{"signal":"logs","limit":5}"#,
        )
    });
    // The other two OTLP routes are one macro with different type names, so
    // they are not retested in detail — only that the subcommand actually
    // mounted them. A missing route is a 404 an exporter retries forever.
    let others: Vec<String> = port
        .map(|p| {
            vec![
                post(p, "/v1/traces", PROTOBUF, &one_span().encode_to_vec()),
                post(p, "/v1/metrics", PROTOBUF, &one_point().encode_to_vec()),
            ]
        })
        .unwrap_or_default();

    for c in [&node, &px] {
        // SAFETY: as in the SIGTERM test above — neither child has been waited
        // on, so neither pid can have been recycled.
        unsafe { libc::kill(c.id() as i32, libc::SIGTERM) };
    }
    let (pstatus, nstatus) = (px.wait().unwrap(), node.wait().unwrap());

    let (ntail, ptail) = (nlog.lock().unwrap().clone(), plog.lock().unwrap().clone());
    assert!(replica.is_some(), "the replica never came up:\n{ntail}");
    assert!(up, "the proxy never came up:\n{ptail}");
    // It says what it is in front of, because the list is static config and the
    // log line is the only place a running proxy states it.
    assert!(ptail.contains("replicas=http://127.0.0.1:"), "{ptail}");
    assert!(
        exported
            .as_deref()
            .is_some_and(|r| r.starts_with("HTTP/1.1 200")),
        "export rejected: {exported:?}\n{ptail}"
    );
    assert!(
        read.as_deref().is_some_and(|r| r.contains("\"hello\"")),
        "the row did not come back through the proxy: {read:?}\n{ptail}"
    );
    assert_eq!(others.len(), 2, "{ptail}");
    for r in &others {
        assert!(
            r.starts_with("HTTP/1.1 200"),
            "export rejected: {r}\n{ptail}"
        );
    }
    assert!(pstatus.success(), "the proxy exited {pstatus}:\n{ptail}");
    assert!(nstatus.success(), "the node exited {nstatus}:\n{ntail}");
    let _ = std::fs::remove_dir_all(&dir);
}

/// A child of this binary with both its streams drained into one buffer.
///
/// On a thread, because a child that fills the pipe while this side is waiting
/// on it is a deadlock and not a slow test.
fn spawn_logged(args: &[&str]) -> (std::process::Child, Arc<Mutex<String>>) {
    let mut child = Command::new(MIRA)
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let log = Arc::new(Mutex::new(String::new()));
    for stream in [
        Box::new(child.stdout.take().unwrap()) as Box<dyn Read + Send>,
        Box::new(child.stderr.take().unwrap()),
    ] {
        let sink = log.clone();
        std::thread::spawn(move || {
            let mut stream = stream;
            let mut buf = [0u8; 4096];
            while let Ok(n) = stream.read(&mut buf) {
                if n == 0 {
                    return;
                }
                sink.lock()
                    .unwrap()
                    .push_str(&String::from_utf8_lossy(&buf[..n]));
            }
        });
    }
    (child, log)
}

/// Up to thirty seconds for a marker to show up in a child's output.
fn logged(log: &Arc<Mutex<String>>, marker: &str) -> bool {
    (0..600).any(|_| {
        if log.lock().unwrap().contains(marker) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(50));
        false
    })
}

/// The port out of a `http=127.0.0.1:NNNN` line.
fn port_of(log: &Arc<Mutex<String>>) -> Option<u16> {
    let tail = log
        .lock()
        .unwrap()
        .split("http=127.0.0.1:")
        .nth(1)?
        .to_owned();
    tail.chars()
        .take_while(char::is_ascii_digit)
        .collect::<String>()
        .parse()
        .ok()
}

fn first(dir: &PathBuf) -> PathBuf {
    let mut entries: Vec<_> = std::fs::read_dir(dir)
        .unwrap_or_else(|e| panic!("{dir:?}: {e}"))
        .map(|e| e.unwrap().path())
        .collect();
    entries.sort();
    entries
        .into_iter()
        .next()
        .unwrap_or_else(|| panic!("{dir:?} is empty"))
}

const PROTOBUF: &str = "application/x-protobuf";

/// OTLP/HTTP is a plain POST of protobuf, so this is a plain socket. A client
/// crate for four lines of HTTP/1.1 would be a dependency the README has to
/// account for.
fn post(port: u16, path: &str, content_type: &str, body: &[u8]) -> String {
    let mut s = std::net::TcpStream::connect(("127.0.0.1", port)).unwrap();
    write!(
        s,
        "POST {path} HTTP/1.1\r\nHost: localhost\r\nContent-Type: {content_type}\r\n\
         Content-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    )
    .unwrap();
    s.write_all(body).unwrap();
    let mut res = String::new();
    // The ack waits for the block to be durable, which waits for the age timer.
    s.set_read_timeout(Some(Duration::from_secs(30))).unwrap();
    s.read_to_string(&mut res).unwrap();
    res
}

fn one_log() -> ExportLogsServiceRequest {
    let now = nanos();
    ExportLogsServiceRequest {
        resource_logs: vec![ResourceLogs {
            resource: Some(named("mira.cli")),
            scope_logs: vec![ScopeLogs {
                scope: Some(InstrumentationScope {
                    name: "mira.cli".into(),
                    ..Default::default()
                }),
                log_records: vec![LogRecord {
                    time_unix_nano: now,
                    severity_number: 9,
                    severity_text: "INFO".into(),
                    body: Some(AnyValue {
                        value: Some(any_value::Value::StringValue("hello".into())),
                    }),
                    ..Default::default()
                }],
                ..Default::default()
            }],
            ..Default::default()
        }],
    }
}

/// The smallest span and the smallest point that are still worth storing —
/// enough for the proxy to have a resource entry to place and a replica to
/// have a row to write, and nothing beyond that, because what they are here to
/// prove is that the route exists.
fn one_span() -> ExportTraceServiceRequest {
    let now = nanos();
    ExportTraceServiceRequest {
        resource_spans: vec![ResourceSpans {
            resource: Some(named("mira.cli")),
            scope_spans: vec![ScopeSpans {
                spans: vec![Span {
                    trace_id: vec![0xab; 16].into(),
                    span_id: vec![0xcd; 8].into(),
                    name: "cli".into(),
                    start_time_unix_nano: now,
                    end_time_unix_nano: now + 1,
                    ..Default::default()
                }],
                ..Default::default()
            }],
            ..Default::default()
        }],
    }
}

fn one_point() -> ExportMetricsServiceRequest {
    ExportMetricsServiceRequest {
        resource_metrics: vec![ResourceMetrics {
            resource: Some(named("mira.cli")),
            scope_metrics: vec![ScopeMetrics {
                metrics: vec![Metric {
                    name: "cli.requests".into(),
                    data: Some(Data::Gauge(Gauge {
                        data_points: vec![NumberDataPoint {
                            time_unix_nano: nanos(),
                            value: Some(NumValue::AsInt(1)),
                            ..Default::default()
                        }],
                    })),
                    ..Default::default()
                }],
                ..Default::default()
            }],
            ..Default::default()
        }],
    }
}

fn nanos() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos() as u64
}

fn named(service: &str) -> Resource {
    Resource {
        attributes: vec![KeyValue {
            key: "service.name".into(),
            value: Some(AnyValue {
                value: Some(any_value::Value::StringValue(service.into())),
            }),
        }],
        ..Default::default()
    }
}
