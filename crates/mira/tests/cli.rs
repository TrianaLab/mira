//! The binary as an operator meets it: argv in, exit code out, SIGTERM in the
//! middle.
//!
//! `main`, `run`, `load` and `shutdown` are only reachable by exec'ing the
//! thing. A unit test inside the bin crate never calls its own `main`, `-h` and
//! `-V` end the process rather than returning a value, and a signal handler
//! needs a process to send a signal to. Coverage still counts: the child
//! inherits `LLVM_PROFILE_FILE` and writes a profraw that gets merged.

use std::io::{Read, Write};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use prost::Message;

use mira_proto::collector::logs::v1::ExportLogsServiceRequest;
use mira_proto::common::v1::{AnyValue, InstrumentationScope, KeyValue, any_value};
use mira_proto::logs::v1::{LogRecord, ResourceLogs, ScopeLogs};
use mira_proto::resource::v1::Resource;

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
        assert!(out.contains("mira mira"), "{flag}: {out:?}");
    }

    // `mira: ` and nothing on stdout. Returning the error from `main` instead
    // would print it with `Debug`, which is a struct dump.
    let (code, out, err) = mira(&["--nope"]);
    assert_eq!(code, Some(1));
    assert!(err.starts_with("mira: unknown flag --nope"), "{err:?}");
    assert_eq!(out, "");

    // The TUI arm takes its own --help and reaches neither the tracing
    // subscriber nor the runtime: both write to the terminal it is about to
    // take over, and one stray line lands in the middle of a frame.
    let (code, out, _) = mira(&["mira", "--help"]);
    assert_eq!(code, Some(0));
    assert!(out.contains("mira tui"), "{out:?}");

    // `tui` is the alias, and it refuses a stdin that is not a terminal rather
    // than spraying escape codes down whatever pipe it was given.
    let (code, _, err) = mira(&["tui", "--data-dir", "/nonexistent"]);
    assert_eq!(code, Some(1));
    assert!(err.contains("needs stdin and stdout on a tty"), "{err:?}");

    // A bad flag on that arm is reported by the arm, not by the server parser
    // it never reaches.
    let (code, _, err) = mira(&["mira", "--nope"]);
    assert_eq!(code, Some(1));
    assert!(err.contains("unknown flag --nope"), "{err:?}");
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

    let mut child = Command::new(MIRA)
        .args([
            "--grpc",
            "127.0.0.1:0",
            "--http",
            "127.0.0.1:0",
            "--data-dir",
        ])
        .arg(&dir)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();

    // Drained on a thread: a child that fills the pipe while this side is
    // waiting on it is a deadlock, not a slow test.
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
    let logged = |marker: &str| {
        (0..600).any(|_| {
            if log.lock().unwrap().contains(marker) {
                return true;
            }
            std::thread::sleep(Duration::from_millis(50));
            false
        })
    };

    // Port 0 means the kernel picked it, and the only place it is written down
    // is the line the server logs on the way up — which is also the point of
    // logging it.
    let up = logged("mira listening");
    let port = up.then(|| log.lock().unwrap().clone()).and_then(|l| {
        let tail = l.split("http=127.0.0.1:").nth(1)?.to_owned();
        tail.chars()
            .take_while(char::is_ascii_digit)
            .collect::<String>()
            .parse::<u16>()
            .ok()
    });

    let posted = port.map(|p| post(p, "/v1/logs", &one_log().encode_to_vec()));
    // SIGTERM rather than `child.kill`, which is SIGKILL and proves nothing.
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

/// OTLP/HTTP is a plain POST of protobuf, so this is a plain socket. A client
/// crate for four lines of HTTP/1.1 would be a dependency the README has to
/// account for.
fn post(port: u16, path: &str, body: &[u8]) -> String {
    let mut s = std::net::TcpStream::connect(("127.0.0.1", port)).unwrap();
    write!(
        s,
        "POST {path} HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/x-protobuf\r\n\
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
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos() as u64;
    ExportLogsServiceRequest {
        resource_logs: vec![ResourceLogs {
            resource: Some(Resource {
                attributes: vec![KeyValue {
                    key: "service.name".into(),
                    value: Some(AnyValue {
                        value: Some(any_value::Value::StringValue("mira.cli".into())),
                    }),
                }],
                ..Default::default()
            }),
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
