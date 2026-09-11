//! The end-to-end test: OTLP protobuf in one end of the real router, JSON out
//! the other.
//!
//! Everything else in this repo is a unit test of one layer. This is the only
//! test that would catch a receiver wired to the wrong flusher, a block written
//! where the reader does not look, a query that parses but never matches, or an
//! acknowledgement returned before the data is findable. It drives the actual
//! `Router` — the same value `main` hands to `axum::serve` — so the only thing
//! it does not exercise is the TCP socket.
//!
//! It leans hard on read-your-writes: a 200 on `/v1/logs` is a promise that the
//! next query can see the data, so nothing here sleeps. Under the shipped
//! default that promise is the open-block read path (section 4) — the export is a
//! frame in the log and a row in a builder, and the query reaches into the
//! builder. [`boot_sealing`] is the other contract, for the handful of tests
//! that are about the block directory itself.

use std::future::IntoFuture;
use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use prost::Message;
use tower::ServiceExt;

use mira_proto::collector::logs::v1::ExportLogsServiceRequest;
use mira_proto::collector::trace::v1::ExportTraceServiceRequest;
use mira_proto::common::v1::{AnyValue, InstrumentationScope, KeyValue, any_value};
use mira_proto::logs::v1::{LogRecord, ResourceLogs, ScopeLogs};
use mira_proto::resource::v1::Resource;
use mira_proto::trace::v1::{ResourceSpans, ScopeSpans, Span};

use crate::{api, pipeline, receiver};

/// Everything the process logs while the returned guard is alive.
///
/// Shared with `main.rs` and `alert.rs` because several of the lines those
/// modules exist to produce — the recovery report, the alerting banner, the
/// firing/resolved transition — are only *evaluated* when a subscriber is
/// interested in them. Without one they are dead text: a field expression that
/// panics, or that names the wrong variable, is invisible to every test and
/// fires the first time someone runs the binary with `RUST_LOG` set.
///
/// Global and permanent, with the *routing* thread-local rather than the
/// subscriber — which is the opposite of the obvious arrangement, and it has to
/// be, because `set_default` cannot win the race it looks like it wins.
///
/// A callsite's `Interest` is decided once, process-wide, the first time it is
/// hit, and `never` means the macro body never runs again. The first thread to
/// reach `alert::dispatch`'s `info!` is usually a test that drives the engine
/// with no subscriber installed at all — so the callsite is cached dead, and a
/// capturing test on another thread then asserts against a log that is missing
/// a line its own code definitely reached. Measured at four failures in fifteen
/// runs of the bin suite, and a mutex around the capturing tests does not
/// touch it: the test that poisons the callsite is not one of them.
///
/// One subscriber installed once, interested in everything, fixes it by leaving
/// no moment at which the answer is `never`. Its writer looks up the calling
/// thread's buffer and discards when there is none, so capture stays per-test —
/// sound for the same reason the old arrangement was, that these are
/// `#[tokio::test]`s on a current-thread runtime and an `.await` resumes on the
/// thread it suspended on.
pub struct Captured(Option<Arc<std::sync::Mutex<Vec<u8>>>>);

impl Captured {
    pub fn text(&self) -> String {
        let buf = self.0.as_ref().expect("a capture always has a buffer");
        String::from_utf8_lossy(&buf.lock().expect("log buffer")).into_owned()
    }
}

impl std::io::Write for Captured {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        if let Some(sink) = &self.0 {
            sink.lock().expect("log buffer").extend_from_slice(buf);
        }
        // Not capturing on this thread: formatted and dropped. Formatting it
        // anyway is the point — a field expression that panics has to panic in
        // every test, not only in the three that read the output back.
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

thread_local! {
    static SINK: std::cell::RefCell<Option<Arc<std::sync::Mutex<Vec<u8>>>>> =
        const { std::cell::RefCell::new(None) };
}

/// The writer half of the one subscriber, resolved per event.
struct ToCallingThread;

impl tracing_subscriber::fmt::MakeWriter<'_> for ToCallingThread {
    type Writer = Captured;
    fn make_writer(&self) -> Captured {
        Captured(SINK.with(|s| s.borrow().clone()))
    }
}

/// Stops routing to this thread's buffer. The subscriber stays installed, so
/// no callsite is ever re-evaluated and none of this can regress to `never`.
pub struct CaptureGuard(());

impl Drop for CaptureGuard {
    fn drop(&mut self) {
        SINK.with(|s| *s.borrow_mut() = None);
    }
}

pub fn capture() -> (CaptureGuard, Captured) {
    static INSTALLED: std::sync::Once = std::sync::Once::new();
    INSTALLED.call_once(|| {
        tracing::subscriber::set_global_default(
            tracing_subscriber::fmt()
                .with_writer(ToCallingThread)
                .with_ansi(false)
                .without_time()
                .with_max_level(tracing::Level::TRACE)
                .finish(),
        )
        .expect("nothing else in this test binary sets a global subscriber");
    });
    let buf = Arc::new(std::sync::Mutex::new(Vec::new()));
    SINK.with(|s| *s.borrow_mut() = Some(Arc::clone(&buf)));
    (CaptureGuard(()), Captured(Some(buf)))
}

/// The capture helper is the thing several assertions *elsewhere* are made of.
///
/// [`Captured::make_writer`] has to hand out another handle on the same buffer
/// rather than a fresh one. Get that wrong and every `log.text()` in the tree
/// returns the empty string — which fails the assertions that a line is
/// present, and silently *passes* the ones that a line is absent
/// (`a_node_with_rules_evaluates_them_without_being_asked` below, the recovery
/// report in `main.rs`). A helper that can make a test vacuous is worth a test
/// of its own instead of being trusted by the tests that stand on it.
#[test]
fn the_log_capture_hands_the_writer_and_the_reader_one_buffer() {
    use std::io::Write;
    use tracing_subscriber::fmt::MakeWriter;

    let (guard, log) = capture();
    let mut w = ToCallingThread.make_writer();
    // `write_all` retries whatever the count says was left, so a `write` that
    // under-reports hangs the suite rather than failing it.
    assert_eq!(w.write(b"one ").unwrap(), 4);
    w.write_all(b"two").unwrap();
    // A `Vec` buffers nothing, but the no-op still has to report success: the
    // `Tee` and `EitherWriter` combinators propagate a flush error outwards and
    // a subscriber built on one would lose the line.
    w.flush().unwrap();
    assert_eq!(log.text(), "one two");

    // And the same buffer through the whole subscriber, which is the only way
    // this helper is ever really used.
    tracing::info!(marker = "captured", "hello");
    drop(guard);
    let text = log.text();
    assert!(text.starts_with("one two"), "{text}");
    assert!(text.contains(r#"marker="captured""#), "{text}");

    // With the guard gone this thread is every other test in the binary: the
    // writer still has to accept the whole slice and report it written. A
    // short count here would wedge `write_all` inside the formatter — for a
    // line nobody is even reading — and take the suite with it.
    let mut off = ToCallingThread.make_writer();
    assert_eq!(off.write(b"nobody is listening").unwrap(), 19);
    off.flush().unwrap();
    assert_eq!(
        log.text(),
        text,
        "a discarded line reached a dropped buffer"
    );
}

fn kv(k: &str, v: &str) -> KeyValue {
    KeyValue {
        key: k.into(),
        value: Some(AnyValue {
            value: Some(any_value::Value::StringValue(v.into())),
        }),
    }
}

/// A data directory this test owns, emptied.
///
/// Split out of [`wire_with`] because the emptying is exactly what a restart
/// test must not do: a second node over the *same* directory is the whole of
/// crash recovery, and a harness that can only start from nothing cannot say
/// anything about it.
fn fresh_dir(name: &str) -> std::path::PathBuf {
    let root = std::env::temp_dir().join(format!("mira-e2e-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    root
}

/// A fresh data directory with the three flushers running against it, and the
/// block age turned down so the test waits on flushes measured in milliseconds
/// rather than the 2s that is right in production.
fn wire(name: &str) -> (receiver::Receivers, api::Api, std::path::PathBuf) {
    wire_with(name, true)
}

/// As [`wire`], with `wal` choosing which acknowledgement contract the test is
/// asserting against. On — the shipped default — acknowledged means logged, so
/// a query has to reach into the open block to find it. Off means acknowledged
/// is published, which is the only way a test can say "two blocks" and mean it.
fn wire_with(name: &str, wal: bool) -> (receiver::Receivers, api::Api, std::path::PathBuf) {
    let root = fresh_dir(name);

    let cfg = Arc::new(pipeline::Config {
        data_dir: root.clone(),
        max_block_age: Duration::from_millis(100),
        wal: wal.then(|| {
            Arc::new(mira_core::wal::Wal::open(&root, mira_core::block::node_id("mira")).unwrap())
        }),
        ..Default::default()
    });
    let (logs, o_logs, _) = pipeline::spawn::<mira_core::logs::LogsBuilder>(cfg.clone());
    let (traces, o_traces, _) = pipeline::spawn::<mira_core::traces::TracesBuilder>(cfg.clone());
    let (metrics, o_metrics, _) = pipeline::spawn::<mira_core::metrics::MetricsBuilder>(cfg);
    let recv = receiver::Receivers {
        logs,
        traces,
        metrics,
        // The shipped default, not a test-only number: the limits these tests
        // assert against are the ones an operator gets out of the box.
        max_request_bytes: crate::config::Config::default().max_request_bytes,
    };
    let api = api::Api {
        data_dir: Arc::new(root.clone()),
        // The real slots the flushers write to, so these tests exercise the
        // open-block read path the binary serves rather than a disk-only
        // subset of it.
        open: [o_logs, o_traces, o_metrics],
        // No rules file: `/api/v1/alerts` answers with an empty list, which is
        // the shape a node without `alerts.rules` serves.
        alerts: Arc::default(),
    };
    (recv, api, root)
}

/// The 4318 listener: OTLP/HTTP, the query API, MCP and the UI on one router.
///
/// Layered exactly as `serve_with` layers it, timing middleware included: what
/// these tests assert is only worth anything if it is the stack the binary
/// serves, and a middleware that runs in production and not here is a layer
/// nothing has ever asserted against.
fn boot(name: &str) -> (Router, std::path::PathBuf) {
    boot_with(name, true)
}

/// As [`boot`], with the log off: every acknowledged export is a published
/// block by the time the caller sees the 200. Tests that count blocks, or that
/// assert what happens when a publish *fails*, are asserting about that
/// contract and cannot be written against the logged one.
fn boot_sealing(name: &str) -> (Router, std::path::PathBuf) {
    boot_with(name, false)
}

/// As [`boot`], with a rules file loaded and the `Api` handed back so a test
/// can drive the evaluator's clock itself.
///
/// The evaluator's own timer is real time on a background task, so a test that
/// waited for it would be a sleep and a flake. `Engine::tick` is the whole of
/// one evaluation, so calling it directly tests the same code the timer calls.
fn boot_alerting(name: &str, rules: &str) -> (Router, api::Api, std::path::PathBuf) {
    let (recv, mut api, root) = wire_with(name, true);
    api.alerts = Arc::new(crate::alert::Engine::new(
        crate::alert::Rules::parse(rules).expect("rules"),
    ));
    let app = receiver::http_router(recv)
        .merge(api::router(api.clone()).layer(axum::middleware::from_fn(crate::timed)))
        .merge(crate::alert::router(api.clone()));
    (app, api, root)
}

fn boot_with(name: &str, wal: bool) -> (Router, std::path::PathBuf) {
    let (recv, api, root) = wire_with(name, wal);
    (router_for(recv, api), root)
}

/// The 4318 stack, composed exactly as `serve_with` composes it.
///
/// One function rather than one copy per booter. The parity tests below compare
/// what this serves against what `mira_core` computes and against what the TUI
/// parses, and a router assembled a second way here would make all three agree
/// about something the binary never serves.
fn router_for(recv: receiver::Receivers, api: api::Api) -> Router {
    receiver::http_router(recv)
        .merge(api::router(api.clone()).layer(axum::middleware::from_fn(crate::timed)))
        .merge(crate::mcp::router(api.clone()))
        .merge(crate::alert::router(api))
        .merge(crate::ui::router())
}

/// The 4317 listener, with the socket taken off.
///
/// `into_axum_router` is the same stack of services `main` hands to
/// `Server::serve`, reachable through `oneshot` — which is what lets a test
/// assert on tonic's own framing and decompression without an ephemeral port
/// and a client stub the shipped binary would then have to carry.
fn boot_grpc(name: &str) -> (Router, Router, std::path::PathBuf) {
    let (recv, api, root) = wire(name);
    let grpc = tonic::service::Routes::default()
        .add_service(recv.logs_server())
        .add_service(recv.traces_server())
        .add_service(recv.metrics_server())
        .into_axum_router();
    (grpc, api::router(api), root)
}

async fn get(
    app: &Router,
    path: &str,
    if_none_match: Option<&str>,
) -> (StatusCode, Vec<u8>, String) {
    let mut req = Request::builder().method("GET").uri(path);
    if let Some(tag) = if_none_match {
        req = req.header("if-none-match", tag);
    }
    let res = app
        .clone()
        .oneshot(req.body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = res.status();
    let etag = res
        .headers()
        .get("etag")
        .map(|v| v.to_str().unwrap().to_owned())
        .unwrap_or_default();
    let bytes = axum::body::to_bytes(res.into_body(), 16 << 20)
        .await
        .unwrap();
    (status, bytes.to_vec(), etag)
}

async fn post(app: &Router, path: &str, content_type: &str, body: Vec<u8>) -> (StatusCode, String) {
    let res = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(path)
                .header("content-type", content_type)
                .body(Body::from(body))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = res.status();
    let bytes = axum::body::to_bytes(res.into_body(), 16 << 20)
        .await
        .unwrap();
    (status, String::from_utf8_lossy(&bytes).into_owned())
}

/// `post`, plus a `content-encoding` the caller chooses.
///
/// The body is sent exactly as given rather than compressed here, because half
/// the point is to send a header that does not match the bytes.
async fn post_enc(
    app: &Router,
    path: &str,
    content_type: &str,
    content_encoding: &str,
    body: Vec<u8>,
) -> (StatusCode, String) {
    let res = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(path)
                .header("content-type", content_type)
                .header("content-encoding", content_encoding)
                .body(Body::from(body))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = res.status();
    let bytes = axum::body::to_bytes(res.into_body(), 16 << 20)
        .await
        .unwrap();
    (status, String::from_utf8_lossy(&bytes).into_owned())
}

/// One gRPC call, framed by hand.
///
/// The frame is a flag byte saying whether the payload is compressed, four
/// bytes of big-endian length, then the payload. Returns `grpc-status`, which
/// arrives in the headers when the call fails before any message is written
/// and in the trailers when it does not; both are the status, and a caller that
/// only reads one of them scores a failure as a pass.
async fn grpc(app: &Router, service: &str, encoding: Option<&str>, payload: Vec<u8>) -> String {
    use http_body_util::BodyExt;

    let mut frame = vec![encoding.is_some() as u8];
    frame.extend_from_slice(&(payload.len() as u32).to_be_bytes());
    frame.extend_from_slice(&payload);

    let mut req = Request::builder()
        .method("POST")
        .uri(format!("/opentelemetry.proto.collector.{service}/Export"))
        .header("content-type", "application/grpc")
        .header("te", "trailers");
    if let Some(e) = encoding {
        req = req.header("grpc-encoding", e);
    }
    let res = app
        .clone()
        .oneshot(req.body(Body::from(frame)).unwrap())
        .await
        .unwrap();
    // gRPC reports everything, including its errors, under HTTP 200.
    assert_eq!(res.status(), StatusCode::OK);

    let status =
        |h: &axum::http::HeaderMap| h.get("grpc-status").map(|v| v.to_str().unwrap().to_owned());
    if let Some(s) = status(res.headers()) {
        return s;
    }
    let body = res.into_body().collect().await.unwrap();
    body.trailers().and_then(status).unwrap_or_default()
}

fn gzip(body: &[u8]) -> Vec<u8> {
    use std::io::Write;
    let mut e = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
    e.write_all(body).unwrap();
    e.finish().unwrap()
}

async fn otlp(app: &Router, path: &str, msg: impl Message) {
    let (status, body) = post(app, path, "application/x-protobuf", msg.encode_to_vec()).await;
    assert_eq!(status, StatusCode::OK, "{path} rejected the export: {body}");
}

async fn query(app: &Router, doc: &str) -> String {
    let (status, body) = post(app, "/api/v1/query", "application/json", doc.into()).await;
    assert_eq!(status, StatusCode::OK, "query failed: {body}");
    body
}

pub(crate) fn logs_export(service: &str, base_ts: u64, n: usize) -> ExportLogsServiceRequest {
    ExportLogsServiceRequest {
        resource_logs: vec![ResourceLogs {
            resource: Some(Resource {
                attributes: vec![kv("service.name", service), kv("deployment.env", "prod")],
                ..Default::default()
            }),
            scope_logs: vec![ScopeLogs {
                scope: Some(InstrumentationScope {
                    name: "mira.e2e".into(),
                    ..Default::default()
                }),
                log_records: (0..n)
                    .map(|i| LogRecord {
                        time_unix_nano: base_ts + i as u64,
                        // Half the records are errors, so a severity filter has
                        // something to actually exclude.
                        severity_number: if i % 2 == 0 { 17 } else { 9 },
                        severity_text: if i % 2 == 0 { "ERROR" } else { "INFO" }.into(),
                        body: Some(AnyValue {
                            value: Some(any_value::Value::StringValue(format!(
                                "{service} handled request {i}"
                            ))),
                        }),
                        attributes: vec![kv(
                            "http.method",
                            if i % 2 == 0 { "POST" } else { "GET" },
                        )],
                        trace_id: vec![0xab; 16].into(),
                        span_id: vec![0xcd; 8].into(),
                        ..Default::default()
                    })
                    .collect(),
                ..Default::default()
            }],
            ..Default::default()
        }],
    }
}

/// The status code is the retry policy, over HTTP as over gRPC. OTLP/HTTP's
/// retryable set is 429/502/503/504 and nothing else, so a volume that cannot
/// take a block has to answer 503 with a `Retry-After` — a 500 there tells the
/// exporter to drop a batch that the same disk would have stored a minute
/// later. The export that can never fit a block is the opposite case and gets
/// the 500, so the sender stops resending it.
#[tokio::test]
async fn a_volume_that_cannot_take_a_block_is_retryable_and_a_too_wide_export_is_not() {
    let (app, root) = boot_sealing("unwritable");
    // A file where the staging directory belongs: every publish fails at its
    // first `create_dir_all`, which is a full or detached volume without one.
    std::fs::write(root.join(".tmp"), b"not a directory").unwrap();

    let res = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/logs")
                .header("content-type", "application/x-protobuf")
                .body(Body::from(
                    logs_export("checkout", 1_000, 1).encode_to_vec(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(
        res.headers()
            .get("retry-after")
            .map(|v| v.to_str().unwrap()),
        Some("1"),
        "an exporter with no backoff of its own needs the pushback"
    );

    // 70k distinct attribute keys: no empty block can hold it, so retrying is
    // the same answer for ever.
    let mut wide = logs_export("checkout", 1_000, 1);
    wide.resource_logs[0].scope_logs[0].log_records[0].attributes = (0..70_000)
        .map(|i| kv(&format!("k{i}"), "v"))
        .collect::<Vec<_>>();
    let (status, body) = post(
        &app,
        "/v1/logs",
        "application/x-protobuf",
        wide.encode_to_vec(),
    )
    .await;
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR, "{body}");
    let _ = std::fs::remove_dir_all(&root);
}

/// The whole product in one function: export, then find it.
///
/// Note the absence of any sleep between the export and the query. That is the
/// assertion, not an omission.
#[tokio::test]
async fn otlp_logs_are_queryable_the_moment_the_export_is_acknowledged() {
    let (app, root) = boot("logs");
    otlp(&app, "/v1/logs", logs_export("checkout", 1_000, 6)).await;
    otlp(&app, "/v1/logs", logs_export("payments", 5_000, 4)).await;

    // A window that predates the data finds nothing, which proves the time
    // bounds are load-bearing and not decorative.
    let none = query(&app, r#"{"signal":"logs","from":0,"to":500}"#).await;
    assert!(none.contains(r#""rows":[]"#), "{none}");

    let all = query(&app, r#"{"signal":"logs","from":0,"to":100000}"#).await;
    assert!(all.contains("checkout handled request 0"), "{all}");
    assert!(all.contains("payments handled request 3"), "{all}");
    // Newest first: the payments batch is later, so it leads.
    let first_body = all.find("payments handled request 3").unwrap();
    assert!(first_body < all.find("checkout handled request 0").unwrap());

    // Resource attribute filter. `service.name` lives on the resource, three
    // tables away from the record it selects.
    let checkout = query(
        &app,
        r#"{"signal":"logs","from":0,"to":100000,
            "where":[{"attr":"service.name","eq":"checkout"}]}"#,
    )
    .await;
    assert_eq!(checkout.matches("handled request").count(), 6, "{checkout}");
    assert!(!checkout.contains("payments"), "{checkout}");

    // Record attribute AND a root field, in one query.
    let errs = query(
        &app,
        r#"{"signal":"logs","from":0,"to":100000,
            "where":[{"attr":"http.method","eq":"POST"},
                     {"field":"severity_number","gte":17}]}"#,
    )
    .await;
    assert_eq!(errs.matches("handled request").count(), 5, "{errs}");

    // The scope name and the merged attribute view survive the round trip, and
    // the internal join keys do not leak out.
    assert!(
        checkout.contains(r#""otel.scope.name":"mira.e2e""#),
        "{checkout}"
    );
    assert!(
        checkout.contains(r#""deployment.env":"prod""#),
        "{checkout}"
    );
    assert!(!checkout.contains(r#""resource_id""#), "{checkout}");

    // Stats are how a caller — or an agent deciding whether to narrow — knows
    // what the query cost. One source, not two: with the log on, both exports
    // are still in the same open block, which is the read path being asked for
    // rows that have never been written to disk.
    assert!(checkout.contains(r#""blocks_total":1"#), "{checkout}");
    assert!(checkout.contains(r#""rows_matched":6"#), "{checkout}");
    assert_eq!(
        mira_core::block::scan(&root, "logs").unwrap().len(),
        0,
        "nothing has been published yet, so every row above came from the open block"
    );
}

/// Paging over the wire, which is a different claim from paging in the engine:
/// the cursor has to survive being printed into JSON, read back out of a YAML
/// document, and re-parsed — and the reader has to be able to tell it is done.
#[tokio::test]
async fn a_paged_read_reassembles_the_one_shot_answer() {
    let (app, _root) = boot("paging");
    // Three exports, so a page boundary lands inside a block and between two.
    for base in [1_000, 2_000, 3_000] {
        otlp(&app, "/v1/logs", logs_export("checkout", base, 5)).await;
    }
    // The rows array, brackets stripped, so pages concatenate.
    let rows = |body: &str| {
        let s = body.find(r#""rows":["#).unwrap() + 8;
        body[s..body.find(r#"],"stats":"#).unwrap()].to_owned()
    };

    let all = query(&app, r#"{"signal":"logs","from":0,"to":100000}"#).await;
    assert_eq!(all.matches("handled request").count(), 15, "{all}");
    assert!(
        !all.contains(r#""next""#),
        "a short page is the last page: {all}"
    );

    let mut pages = Vec::new();
    let mut after = String::new();
    for _ in 0..10 {
        let body = query(
            &app,
            &format!(r#"{{"signal":"logs","from":0,"to":100000,"limit":4{after}}}"#),
        )
        .await;
        pages.push(rows(&body));
        // `next` absent is the terminator. A reader never has to ask for an
        // empty page to find out it has them all.
        let Some(i) = body.find(r#""next":""#) else {
            break;
        };
        let c = &body[i + 8..];
        after = format!(r#","after":"{}""#, &c[..c.find('"').unwrap()]);
    }
    assert_eq!(pages.len(), 4, "15 rows, 4 a page");
    assert_eq!(
        pages.join(","),
        rows(&all),
        "pages must reassemble the whole"
    );

    // A cursor is a string. Unquoted it is a YAML float, and `1757241600000000000.2`
    // has already lost the digits that made it a cursor by the time we see it.
    for (doc, want) in [
        (r#"{"signal":"logs","after":1.2}"#, "it is a string"),
        (
            r#"{"signal":"logs","after":"1.2.3"}"#,
            "pass back the `next` field verbatim",
        ),
    ] {
        let (status, body) = post(&app, "/api/v1/query", "application/json", doc.into()).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
        assert!(body.contains(want), "{body}");
    }
}

/// A cursor handed out over the open block still points at the same row after
/// that block is sealed and published.
///
/// This is the one thing section 4's design buys and nothing else in the suite
/// would catch. A snapshot holds the builder's rows from row zero in publish
/// order, so row `n` of the snapshot is row `n` of the eventual block and a
/// cursor over one is a cursor over the other. Get that wrong — rebase the row
/// index, or dedupe by anything other than `(node, seq)` — and the failure is a
/// duplicated or a skipped record exactly at the boundary, which every
/// one-shot query in this file would still pass.
#[tokio::test]
async fn a_cursor_taken_from_the_open_block_survives_the_seal() {
    let (app, root) = boot("seal-cursor");
    for base in [1_000, 2_000, 3_000] {
        otlp(&app, "/v1/logs", logs_export("checkout", base, 5)).await;
    }
    let rows = |body: &str| {
        let s = body.find(r#""rows":["#).unwrap() + 8;
        body[s..body.find(r#"],"stats":"#).unwrap()].to_owned()
    };
    let next = |body: &str| {
        body.find(r#""next":""#).map(|i| {
            let c = &body[i + 8..];
            c[..c.find('"').unwrap()].to_owned()
        })
    };

    // Page one, taken while nothing has been written to disk.
    let first = query(&app, r#"{"signal":"logs","from":0,"to":100000,"limit":4}"#).await;
    assert_eq!(
        mira_core::block::scan(&root, "logs").unwrap().len(),
        0,
        "the point of this test is that page one predates the block"
    );
    let cursor = next(&first).expect("15 rows, 4 a page");

    // Seal it. `max_block_age` is 100ms here, and the export that follows is
    // what makes the flusher notice the deadline has passed.
    tokio::time::sleep(Duration::from_millis(200)).await;
    otlp(&app, "/v1/logs", logs_export("checkout", 9_000, 1)).await;
    // Through [`seal`] rather than a two-second poll of its own: a seal is a
    // publish, and on a runner with the whole suite on it the publish is behind
    // however long the other tests' fsyncs take. Two seconds is enough on an
    // idle machine and is a flake on a loaded one, which is the worst of both —
    // the test fails for a reason that has nothing to do with cursors.
    assert_eq!(
        seal(&root, "logs").await.len(),
        1,
        "the block never sealed, so this test proves nothing"
    );

    // Now finish the read with the cursor from before the seal. The 16th
    // record is deliberately outside the window: what is being asserted is
    // that the pre-seal answer and the post-seal continuation join up
    // exactly, not that nothing arrived in between.
    let mut pages = vec![rows(&first)];
    let mut after = format!(r#","after":"{cursor}""#);
    for _ in 0..10 {
        let body = query(
            &app,
            &format!(r#"{{"signal":"logs","from":0,"to":8000,"limit":4{after}}}"#),
        )
        .await;
        pages.push(rows(&body));
        let Some(c) = next(&body) else { break };
        after = format!(r#","after":"{c}""#);
    }
    let joined = pages.join(",");
    assert_eq!(
        joined.matches("handled request").count(),
        15,
        "a row was duplicated or dropped across the seal: {joined}"
    );
    let one_shot = query(&app, r#"{"signal":"logs","from":0,"to":8000}"#).await;
    assert_eq!(joined, rows(&one_shot), "pages must reassemble the whole");
}

#[tokio::test]
async fn otlp_spans_are_queryable_and_ids_come_back_as_hex() {
    let (app, _root) = boot("traces");
    let trace_id = vec![
        0x4b, 0xf9, 0x2f, 0x35, 0x77, 0xb3, 0x4d, 0xa6, 0xa3, 0xce, 0x92, 0x9d, 0x0e, 0x0e, 0x47,
        0x36,
    ];
    otlp(
        &app,
        "/v1/traces",
        ExportTraceServiceRequest {
            resource_spans: vec![ResourceSpans {
                resource: Some(Resource {
                    attributes: vec![kv("service.name", "frontend")],
                    ..Default::default()
                }),
                scope_spans: vec![ScopeSpans {
                    scope: Some(InstrumentationScope {
                        name: "mira.e2e".into(),
                        ..Default::default()
                    }),
                    spans: (0..3)
                        .map(|i| Span {
                            trace_id: trace_id.clone().into(),
                            span_id: vec![i as u8 + 1; 8].into(),
                            name: format!("GET /checkout/{i}"),
                            start_time_unix_nano: 1_000 + i * 10,
                            end_time_unix_nano: 1_500 + i * 10,
                            attributes: vec![kv("http.route", "/checkout/:id")],
                            ..Default::default()
                        })
                        .collect(),
                    ..Default::default()
                }],
                ..Default::default()
            }],
        },
    )
    .await;

    let hex = "4bf92f3577b34da6a3ce929d0e0e4736";
    let found = query(
        &app,
        &format!(
            r#"{{"signal":"traces","from":0,"to":100000,
                 "where":[{{"field":"trace_id","eq":"{hex}"}}]}}"#
        ),
    )
    .await;
    assert_eq!(found.matches("GET /checkout/").count(), 3, "{found}");
    // FixedSizeBinary on disk, hex on the wire — a UI can put this straight into
    // a `traceparent` header.
    assert!(found.contains(&format!(r#""trace_id":"{hex}""#)), "{found}");
    assert!(found.contains(r#""span_id":"0101010101010101""#), "{found}");
    assert!(found.contains(r#""http.route":"/checkout/:id""#), "{found}");
}

/// The frame algebra over the wire (section 7.3): one log line, and the whole
/// call chain it belongs to.
///
/// This is the correlation claim the product is sold on, and every part of it
/// is cross-signal — the anchor is a log, the walk reads traces, and the labels
/// come from all three signal directories. The window is deliberately too
/// narrow to contain the spans: `traces` has to widen it, which is the step
/// that makes "a log written after the request it describes" findable at all.
#[tokio::test]
async fn a_frame_walk_over_http_turns_one_log_line_into_a_call_chain() {
    let (app, _root) = boot("frame");
    let hex = "4bf92f3577b34da6a3ce929d0e0e4736";
    let trace_id: Vec<u8> = (0..16)
        .map(|i| u8::from_str_radix(&hex[i * 2..i * 2 + 2], 16).unwrap())
        .collect();
    // One identity shape for every resource in the test, so the log's entity
    // key and the api span's entity key are the same number. They are joined on
    // that key and nothing else (section 7.2) — give the two resources different
    // attributes and the frame reports four services for a chain of three.
    let res = |svc: &str| Resource {
        attributes: vec![kv("service.name", svc), kv("service.instance.id", "7f3a")],
        ..Default::default()
    };
    let scope = || {
        Some(InstrumentationScope {
            name: "mira.e2e".into(),
            ..Default::default()
        })
    };
    let span = |svc: &str, id: u8, parent: u8, at: u64| ResourceSpans {
        resource: Some(res(svc)),
        scope_spans: vec![ScopeSpans {
            scope: scope(),
            spans: vec![Span {
                trace_id: trace_id.clone().into(),
                span_id: vec![id; 8].into(),
                parent_span_id: if parent == 0 {
                    Vec::new()
                } else {
                    vec![parent; 8]
                }
                .into(),
                name: format!("{svc} work"),
                start_time_unix_nano: at,
                end_time_unix_nano: at + 500,
                ..Default::default()
            }],
            ..Default::default()
        }],
        ..Default::default()
    };
    otlp(
        &app,
        "/v1/traces",
        ExportTraceServiceRequest {
            resource_spans: vec![
                span("gateway", 1, 0, 1_000),
                span("api", 2, 1, 1_100),
                span("db", 3, 2, 1_200),
            ],
        },
    )
    .await;
    otlp(
        &app,
        "/v1/logs",
        ExportLogsServiceRequest {
            resource_logs: vec![ResourceLogs {
                resource: Some(res("api")),
                scope_logs: vec![ScopeLogs {
                    scope: scope(),
                    log_records: vec![LogRecord {
                        time_unix_nano: 50_000,
                        severity_number: 17,
                        severity_text: "ERROR".into(),
                        body: Some(AnyValue {
                            value: Some(any_value::Value::StringValue("checkout failed".into())),
                        }),
                        trace_id: trace_id.clone().into(),
                        ..Default::default()
                    }],
                    ..Default::default()
                }],
                ..Default::default()
            }],
        },
    )
    .await;

    let call = |path: &'static str, doc: String| {
        let app = app.clone();
        async move {
            let (status, body) = post(&app, path, "application/json", doc.into()).await;
            assert_eq!(status, StatusCode::OK, "{path}: {body}");
            body
        }
    };

    // Anchored on the log, in a window that holds the log and nothing else.
    let f = call(
        "/api/v1/correlate",
        r#"{"signal":"logs","from":40000,"to":60000,
            "where":[{"field":"severity_text","eq":"ERROR"}],
            "expand":["traces","peers"]}"#
            .into(),
    )
    .await;
    assert!(f.contains(&format!(r#""{hex}""#)), "{f}");
    for svc in ["gateway", "api", "db"] {
        assert!(f.contains(&format!(r#""name":"{svc}""#)), "{svc}: {f}");
    }
    // `traces` moved `from` back to the first span, 39 microseconds before the
    // window the caller asked for.
    assert!(f.contains(r#""from":"1000""#), "{f}");
    assert!(f.contains(r#""truncated":false"#), "{f}");

    let m = call("/api/v1/map", r#"{"from":0,"to":100000}"#.into()).await;
    // Edges name their endpoints by entity key, not by service name: the key is
    // the identity, and two deployments of one service are two nodes with the
    // same label. So the graph has to be read the way the UI reads it, through
    // `nodes`.
    let key = |svc: &str| {
        let at = m.find(&format!(r#","name":"{svc}""#)).expect(&m);
        let k = m[..at].rsplit_once(r#""key":""#).expect(&m).1;
        k.trim_end_matches('"').to_owned()
    };
    for (from, to) in [
        ("entry".to_owned(), key("gateway")),
        (key("gateway"), key("api")),
        (key("api"), key("db")),
    ] {
        let edge = format!(r#""from":"{from}","to":"{to}""#);
        assert!(m.contains(&edge), "{edge} missing: {m}");
    }
    assert!(m.contains(r#""unresolved":0"#), "{m}");

    let e = call("/api/v1/entities", r#"{"from":0,"to":100000}"#.into()).await;
    assert_eq!(e.matches(r#""name":"#).count(), 3, "{e}");
}

/// Metrics: the point of this one is that a series survives being split across
/// two blocks.
///
/// Ids are rebased per block — that is what makes the attribute joins array
/// stores rather than hash joins — so nothing block-local can identify a series
/// across blocks. If the value-based grouping key is wrong in any detail, the
/// same counter comes back as two series with half the points each, and every
/// chart in the product is quietly wrong.
#[tokio::test]
async fn a_metric_series_survives_being_split_across_two_blocks() {
    use mira_proto::collector::metrics::v1::ExportMetricsServiceRequest;
    use mira_proto::metrics::v1::metric::Data;
    use mira_proto::metrics::v1::number_data_point::Value as NumValue;
    use mira_proto::metrics::v1::{
        AggregationTemporality, HistogramDataPoint, Metric, NumberDataPoint, ResourceMetrics,
        ScopeMetrics, Sum,
    };

    // The log off, so "two exports" really is "two blocks": what this test is
    // about is the merge across a block boundary, and with the log on both
    // exports would land in one open block and never cross one.
    let (app, _root) = boot_sealing("metrics");

    // Two exports, therefore two blocks, each carrying half of the same two
    // series: one for GET and one for POST.
    let export = |base: u64| ExportMetricsServiceRequest {
        resource_metrics: vec![ResourceMetrics {
            resource: Some(Resource {
                attributes: vec![kv("service.name", "checkout")],
                ..Default::default()
            }),
            scope_metrics: vec![ScopeMetrics {
                scope: Some(InstrumentationScope {
                    name: "mira.e2e".into(),
                    ..Default::default()
                }),
                metrics: vec![Metric {
                    name: "http.server.requests".into(),
                    unit: "1".into(),
                    data: Some(Data::Sum(Sum {
                        aggregation_temporality: AggregationTemporality::Cumulative as i32,
                        is_monotonic: true,
                        data_points: ["GET", "POST"]
                            .iter()
                            .enumerate()
                            .flat_map(|(m, method)| {
                                (0..2).map(move |i| NumberDataPoint {
                                    time_unix_nano: base + i as u64,
                                    attributes: vec![kv("http.method", method)],
                                    value: Some(NumValue::AsInt(
                                        (base as i64) + i + m as i64 * 100,
                                    )),
                                    ..Default::default()
                                })
                            })
                            .collect(),
                    })),
                    ..Default::default()
                }],
                ..Default::default()
            }],
            ..Default::default()
        }],
    };
    otlp(&app, "/v1/metrics", export(1_000)).await;
    otlp(&app, "/v1/metrics", export(5_000)).await;

    let (status, body) = post(
        &app,
        "/api/v1/metrics/query",
        "application/json",
        br#"{"name":"http.server.requests","from":0,"to":100000}"#.to_vec(),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");

    // Two series, not four: the two blocks merged.
    assert_eq!(
        body.matches(r#""name":"http.server.requests""#).count(),
        2,
        "{body}"
    );
    // Four points each, in ascending time order across the block boundary.
    // Both halves of a point are quoted: the timestamp is nanoseconds and the
    // value of an integer sum is an OTLP `int64`, and OTLP/JSON writes both as
    // strings — see `mira_core::json::Json::i64_str`.
    assert!(
        body.contains(r#"["1000","1000"],["1001","1001"],["5000","5000"],["5001","5001"]"#),
        "{body}"
    );
    assert!(
        body.contains(r#"["1000","1100"],["1001","1101"],["5000","5100"],["5001","5101"]"#),
        "{body}"
    );
    // The descriptor and the inherited resource attribute both come along.
    assert!(
        body.contains(r#""kind":"sum","temporality":2,"monotonic":true"#),
        "{body}"
    );
    // All four attribute levels merged into one sorted object: the point's own
    // `http.method`, the scope's name, and the resource's `service.name`.
    assert!(
        body.contains(
            r#""attributes":{"http.method":"GET","otel.scope.name":"mira.e2e","service.name":"checkout"}"#
        ),
        "{body}"
    );
    assert!(body.contains(r#""blocks_scanned":2"#), "{body}");

    // `max_points` has to cut a *window*, not a sample. Points arrive in
    // block-scan order, so a cap applied as "refuse once full" kept whichever
    // ones the directory listing reached first and the sort at render time
    // then presented that arbitrary subset as a contiguous series. Newest
    // wins, which is the rule `limit` already uses on the record side.
    let (status, capped) = post(
        &app,
        "/api/v1/metrics/query",
        "application/json",
        br#"{"name":"http.server.requests","from":0,"to":100000,"max_points":2}"#.to_vec(),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{capped}");
    assert!(
        capped.contains(r#"["5000","5000"],["5001","5001"]"#),
        "{capped}"
    );
    assert!(
        capped.contains(r#"["5000","5100"],["5001","5101"]"#),
        "{capped}"
    );
    assert!(
        !capped.contains(r#"["1000","#),
        "the older half must be gone\n{capped}"
    );
    // And it says so, rather than returning a short series that looks whole.
    assert_eq!(
        capped.matches(r#""dropped_points":2"#).count(),
        2,
        "{capped}"
    );

    // A filter on a resource attribute and one on a point attribute have to work
    // the same way, even though they live three tables apart.
    let (_, only_post) = post(
        &app,
        "/api/v1/metrics/query",
        "application/json",
        br#"{"from":0,"to":100000,"where":[{"attr":"http.method","eq":"POST"},
                                           {"attr":"service.name","eq":"checkout"}]}"#
            .to_vec(),
    )
    .await;
    assert_eq!(only_post.matches(r#""points""#).count(), 1, "{only_post}");
    assert!(only_post.contains(r#""http.method":"POST""#), "{only_post}");

    // The name listing is what a UI puts in a dropdown, so it must work with no
    // body at all — and with none, the default window is the last hour, which
    // this test's 1970-era timestamps sit well outside. Blocks pruned without
    // being opened is the right answer, not an empty database.
    let (status, recent) = post(&app, "/api/v1/metrics/names", "application/json", vec![]).await;
    assert_eq!(status, StatusCode::OK, "{recent}");
    assert!(
        recent.contains(r#""names":[],"stats":{"blocks_total":2,"blocks_scanned":0"#),
        "{recent}"
    );

    let (_, names) = post(
        &app,
        "/api/v1/metrics/names",
        "application/json",
        br#"{"from":0,"to":100000}"#.to_vec(),
    )
    .await;
    assert!(
        names.contains(r#"{"name":"http.server.requests","unit":"1","kind":"sum"}"#),
        "{names}"
    );

    // A histogram is invisible unless it comes back as something, so it comes
    // back as the two numbers that answer "how often" and "how much".
    otlp(
        &app,
        "/v1/metrics",
        ExportMetricsServiceRequest {
            resource_metrics: vec![ResourceMetrics {
                scope_metrics: vec![ScopeMetrics {
                    metrics: vec![Metric {
                        name: "http.server.duration".into(),
                        unit: "ms".into(),
                        data: Some(Data::Histogram(mira_proto::metrics::v1::Histogram {
                            aggregation_temporality: AggregationTemporality::Delta as i32,
                            data_points: vec![HistogramDataPoint {
                                time_unix_nano: 9_000,
                                count: 42,
                                sum: Some(1234.5),
                                explicit_bounds: vec![1.0, 5.0, 10.0],
                                bucket_counts: vec![10, 20, 10, 2],
                                ..Default::default()
                            }],
                        })),
                        ..Default::default()
                    }],
                    ..Default::default()
                }],
                ..Default::default()
            }],
        },
    )
    .await;
    let (_, hist) = post(
        &app,
        "/api/v1/metrics/query",
        "application/json",
        br#"{"name":"http.server.duration","from":0,"to":100000}"#.to_vec(),
    )
    .await;
    assert!(
        hist.contains(r#""name":"http.server.duration.count""#),
        "{hist}"
    );
    assert!(
        hist.contains(r#""name":"http.server.duration.sum""#),
        "{hist}"
    );
    // `.count` is a `uint64` and so is quoted; `.sum` is a double and is not.
    assert!(hist.contains(r#"["9000","42"]"#), "{hist}");
    assert!(hist.contains(r#"["9000",1234.5]"#), "{hist}");
}

/// The metric-to-trace edge, which is the one correlation other backends drop.
///
/// An exemplar hangs off a data point, and a histogram point produces *two*
/// derived series, so the assertion that matters is that both of them carry it:
/// whichever of `.count` and `.sum` is on screen, the spike in it has to point
/// at the same trace.
#[tokio::test]
async fn a_histogram_carries_its_exemplars_into_both_derived_series() {
    use mira_proto::collector::metrics::v1::ExportMetricsServiceRequest;
    use mira_proto::metrics::v1::metric::Data;
    use mira_proto::metrics::v1::{
        AggregationTemporality, HistogramDataPoint, Metric, ResourceMetrics, ScopeMetrics,
    };

    let (app, _root) = boot("exemplars");
    otlp(
        &app,
        "/v1/metrics",
        ExportMetricsServiceRequest {
            resource_metrics: vec![ResourceMetrics {
                scope_metrics: vec![ScopeMetrics {
                    metrics: vec![Metric {
                        name: "rpc.duration".into(),
                        unit: "ms".into(),
                        data: Some(Data::Histogram(mira_proto::metrics::v1::Histogram {
                            aggregation_temporality: AggregationTemporality::Delta as i32,
                            data_points: vec![HistogramDataPoint {
                                time_unix_nano: 9_000,
                                count: 3,
                                sum: Some(30.0),
                                explicit_bounds: vec![10.0],
                                bucket_counts: vec![2, 1],
                                exemplars: vec![mira_proto::metrics::v1::Exemplar {
                                    time_unix_nano: 8_900,
                                    trace_id: vec![0xab; 16].into(),
                                    span_id: vec![0xcd; 8].into(),
                                    value: Some(
                                        mira_proto::metrics::v1::exemplar::Value::AsDouble(29.5),
                                    ),
                                    ..Default::default()
                                }],
                                ..Default::default()
                            }],
                        })),
                        ..Default::default()
                    }],
                    ..Default::default()
                }],
                ..Default::default()
            }],
        },
    )
    .await;

    let (_, hist) = post(
        &app,
        "/api/v1/metrics/query",
        "application/json",
        br#"{"name":"rpc.duration","from":0,"to":100000}"#.to_vec(),
    )
    .await;
    assert_eq!(
        hist.matches(r#""trace_id":"abababababababababababababababab""#)
            .count(),
        2,
        "{hist}"
    );
    assert!(hist.contains(r#""span_id":"cdcdcdcdcdcdcdcd""#), "{hist}");
    assert!(hist.contains(r#""double":29.5"#), "{hist}");
    // The exemplar's own timestamp, not the point's. Quoted, like every other
    // 64-bit integer Mira emits.
    assert!(hist.contains(r#""time_unix_nano":"8900""#), "{hist}");
}

/// A bad query has to fail as JSON with a usable message. The caller is often a
/// model, and "400 Bad Request" with an empty body teaches it nothing.
#[tokio::test]
async fn a_malformed_query_returns_a_readable_json_error() {
    let (app, _root) = boot("badq");
    let (status, body) = post(
        &app,
        "/api/v1/query",
        "application/json",
        br#"{"signal":"logs","where":[{"attr":"a","matches":"b"}]}"#.to_vec(),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(body.starts_with(r#"{"error":"#), "{body}");
    assert!(body.contains("unknown term key"), "{body}");
}

/// The UI ships inside the binary, so "is it there" is a compile-time question
/// and "is it served" is this test. It also pins the revalidation path: if the
/// ETag ever stops matching itself, every reload re-downloads the bundle.
#[tokio::test]
async fn the_ui_is_served_from_the_binary_and_revalidates() {
    let (app, _root) = boot("ui");

    let (status, body, _) = get(&app, "/", None).await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        String::from_utf8_lossy(&body).contains(r#"<div id="app">"#),
        "index.html is not the built bundle"
    );

    let (status, body, etag) = get(&app, "/app.js", None).await;
    assert_eq!(status, StatusCode::OK);
    assert!(!body.is_empty() && !etag.is_empty());

    let (status, body, _) = get(&app, "/app.js", Some(&etag)).await;
    assert_eq!(status, StatusCode::NOT_MODIFIED);
    assert!(body.is_empty(), "a 304 must not carry the body");

    // Every view lives under the hash (`/#/logs`), so a *path* that is not one
    // of the three assets is a request for something that does not exist. It
    // used to get index.html and a 200, which meant `/health` and `/metrics`
    // reported success in HTML to whatever was probing them.
    let (status, _, _) = get(&app, "/logs", None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

/// The agent surface, driven the way a client drives it: initialize, list the
/// tools, then call one and read the answer out of the content block.
///
/// The assertion that matters is the last one. `get_trace` is the tool an agent
/// reaches for constantly, and it has no time bounds at all — if it ever stops
/// finding a trace outside the default hour window, every incident
/// investigation that starts with a trace id from yesterday comes back empty.
#[tokio::test]
async fn an_mcp_client_can_list_the_tools_and_call_them() {
    let (app, _root) = boot("mcp");

    let rpc = async |body: &str| {
        let (status, out) = post(&app, "/mcp", "application/json", body.into()).await;
        assert_eq!(status, StatusCode::OK, "{out}");
        out
    };

    let init = rpc(r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#).await;
    assert!(init.contains(r#""id":1"#), "{init}");
    assert!(init.contains(r#""protocolVersion""#), "{init}");
    assert!(init.contains(r#""name":"mira""#), "{init}");

    // The notification every client sends next. It has no id, so it takes no
    // reply — answering it with an error is how a session dies on message two.
    let (status, body) = post(
        &app,
        "/mcp",
        "application/json",
        br#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#.to_vec(),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED);
    assert!(body.is_empty(), "a notification takes no reply: {body}");

    let tools = rpc(r#"{"jsonrpc":"2.0","id":2,"method":"tools/list"}"#).await;
    for t in ["query_records", "get_trace", "query_metric", "list_metrics"] {
        assert!(tools.contains(&format!(r#""name":"{t}""#)), "{tools}");
    }

    otlp(&app, "/v1/logs", logs_export("checkout", 1_000, 4)).await;
    let rows = rpc(r#"{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{
             "name":"query_records","arguments":{
               "signal":"logs","from":0,"to":100000,
               "where":[{"field":"severity_text","eq":"ERROR"}]}}}"#)
    .await;
    assert!(rows.contains(r#""isError":false"#), "{rows}");
    // The rows arrive as escaped JSON inside the text block, so the envelope is
    // visible through the escaping.
    assert!(rows.contains(r#"blocks_scanned"#), "{rows}");
    assert_eq!(
        rows.matches("checkout handled request").count(),
        2,
        "{rows}"
    );

    // A tool failure is a result with isError, not a JSON-RPC error: a model has
    // to be able to read the reason and try again.
    let bad = rpc(r#"{"jsonrpc":"2.0","id":4,"method":"tools/call",
             "params":{"name":"get_trace","arguments":{"trace_id":"nope"}}}"#)
    .await;
    assert!(bad.contains(r#""isError":true"#), "{bad}");
    assert!(bad.contains("hex trace id"), "{bad}");

    let missing = rpc(r#"{"jsonrpc":"2.0","id":5,"method":"frobnicate"}"#).await;
    assert!(missing.contains(r#""code":-32601"#), "{missing}");

    // Spans at time 1000-1500, which is fifty-five years outside the default
    // window. get_trace has to find them anyway.
    otlp(
        &app,
        "/v1/traces",
        ExportTraceServiceRequest {
            resource_spans: vec![ResourceSpans {
                resource: Some(Resource::default()),
                scope_spans: vec![ScopeSpans {
                    spans: (0..3)
                        .map(|i| Span {
                            trace_id: vec![0x5a; 16].into(),
                            span_id: vec![i as u8 + 1; 8].into(),
                            name: format!("GET /pay/{i}"),
                            start_time_unix_nano: 1_000 + i * 10,
                            end_time_unix_nano: 1_500 + i * 10,
                            ..Default::default()
                        })
                        .collect(),
                    ..Default::default()
                }],
                ..Default::default()
            }],
        },
    )
    .await;
    let trace = rpc(r#"{"jsonrpc":"2.0","id":6,"method":"tools/call","params":{
             "name":"get_trace","arguments":{
               "trace_id":"5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a"}}}"#)
    .await;
    assert!(trace.contains(r#""isError":false"#), "{trace}");
    assert_eq!(trace.matches("GET /pay/").count(), 3, "{trace}");
}

/// OTLP/HTTP with a JSON body.
///
/// The point of this test is the places OTLP JSON is *not* canonical protobuf
/// JSON, because those are the ones a generic decoder gets wrong quietly:
/// 64-bit integers arrive as strings, ids arrive as hex rather than base64, and
/// enums may be spelled by name. It also mixes `lowerCamelCase` and the original
/// proto field names within one document, which the spec allows and real
/// pipelines produce.
#[tokio::test]
async fn otlp_json_bodies_decode_with_the_deviations_the_spec_requires() {
    let (app, _root) = boot("json");

    // Logs: camelCase throughout, string nanos, hex ids, enum by name, and one
    // attribute of every AnyValue kind that survives to a stored row.
    let body = r#"{
      "resourceLogs": [{
        "resource": {"attributes": [{"key":"service.name","value":{"stringValue":"json-svc"}}]},
        "scopeLogs": [{
          "scope": {"name":"mira.json","version":"1.2.3"},
          "logRecords": [{
            "timeUnixNano": "1000000",
            "observedTimeUnixNano": "1000001",
            "severityNumber": "SEVERITY_NUMBER_ERROR",
            "severityText": "ERROR",
            "body": {"stringValue": "json log one"},
            "traceId": "5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a",
            "spanId": "0102030405060708",
            "attributes": [
              {"key":"http.status_code","value":{"intValue":"200"}},
              {"key":"retry","value":{"boolValue":true}},
              {"key":"ratio","value":{"doubleValue":1.5}},
              {"key":"blob","value":{"bytesValue":"aGVsbG8="}}
            ]
          }]
        }]
      }]
    }"#;
    let (status, answer) = post(&app, "/v1/logs", "application/json", body.into()).await;
    assert_eq!(status, StatusCode::OK, "{answer}");
    // A JSON request is answered in JSON, not with a protobuf empty message.
    assert_eq!(answer, "{}");

    let rows = query(&app, r#"{"signal":"logs","from":0,"to":9000000}"#).await;
    assert!(rows.contains("json log one"), "{rows}");
    // Hex in, hex out, byte for byte. This is the assertion that fails if the id
    // was ever treated as base64.
    assert!(
        rows.contains(r#""trace_id":"5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a""#),
        "{rows}"
    );
    assert!(rows.contains(r#""span_id":"0102030405060708""#), "{rows}");
    assert!(rows.contains(r#""severity_number":17"#), "{rows}");
    assert!(rows.contains(r#""service.name":"json-svc""#), "{rows}");
    assert!(rows.contains(r#""otel.scope.version":"1.2.3""#), "{rows}");
    // `"200"` was a string in the document because proto3 JSON writes 64-bit
    // integers that way; it has to have been stored as the integer 200 and it
    // comes back out quoted for the same reason it went in quoted. The `eq`
    // below is the half that proves the type: a stored string would not match
    // an integer term.
    assert!(rows.contains(r#""http.status_code":"200""#), "{rows}");
    let by_int = query(
        &app,
        r#"{"signal":"logs","from":0,"to":9000000,
            "where":[{"attr":"http.status_code","eq":200}]}"#,
    )
    .await;
    assert!(by_int.contains("json log one"), "{by_int}");
    assert!(rows.contains(r#""retry":true"#), "{rows}");
    assert!(rows.contains(r#""ratio":1.5"#), "{rows}");

    // Traces: the *other* dialect. Every field name here is the original proto
    // name, and the timestamps are JSON numbers rather than strings.
    let body = r#"{
      "resource_spans": [{
        "resource": {"attributes": [{"key":"service.name","value":{"string_value":"json-svc"}}]},
        "scope_spans": [{
          "scope": {"name":"mira.json"},
          "spans": [{
            "trace_id": "aabbccddeeff00112233445566778899",
            "span_id": "1111111111111111",
            "parent_span_id": "",
            "name": "GET /json",
            "kind": 2,
            "start_time_unix_nano": 2000000,
            "end_time_unix_nano": 2000500,
            "status": {"code": "STATUS_CODE_ERROR", "message": "boom"},
            "events": [{"time_unix_nano": "2000100", "name": "cache.miss"}],
            "links": [{"trace_id": "99887766554433221100ffeeddccbbaa",
                       "span_id": "2222222222222222"}]
          }]
        }]
      }]
    }"#;
    let (status, answer) = post(&app, "/v1/traces", "application/json", body.into()).await;
    assert_eq!(status, StatusCode::OK, "{answer}");

    let spans = query(
        &app,
        r#"{"signal":"traces","from":0,"to":9000000,
            "where":[{"field":"trace_id","eq":"aabbccddeeff00112233445566778899"}]}"#,
    )
    .await;
    assert!(spans.contains("GET /json"), "{spans}");
    assert!(spans.contains(r#""span_id":"1111111111111111""#), "{spans}");
    // An absent parent is null — not eight bytes of zero, which would join to a
    // real span, and not an error.
    assert!(!spans.contains("parent_span_id"), "{spans}");
    // Events and links come back nested under the span. A link's ids point out
    // of this block — that is what makes it a link — so they are the one place
    // the reader must not rebase.
    assert!(spans.contains(r#""events":[{"#), "{spans}");
    assert!(spans.contains(r#""name":"cache.miss""#), "{spans}");
    assert!(
        spans.contains(r#""links":[{"trace_id":"99887766554433221100ffeeddccbbaa""#),
        "{spans}"
    );

    // Metrics: an int-valued gauge point, which is the shape that would silently
    // become a double if the oneof were decoded by value rather than by key.
    let body = r#"{
      "resourceMetrics": [{
        "scopeMetrics": [{
          "metrics": [{
            "name": "json.requests",
            "unit": "1",
            "sum": {
              "isMonotonic": true,
              "aggregationTemporality": "AGGREGATION_TEMPORALITY_CUMULATIVE",
              "dataPoints": [{"timeUnixNano": "3000000", "asInt": "42",
                              "attributes": [{"key":"route","value":{"stringValue":"/json"}}]}]
            }
          }]
        }]
      }]
    }"#;
    let (status, answer) = post(&app, "/v1/metrics", "application/json", body.into()).await;
    assert_eq!(status, StatusCode::OK, "{answer}");
    let (status, names) = post(
        &app,
        "/api/v1/metrics/names",
        "application/json",
        br#"{"from":0,"to":9000000}"#.to_vec(),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{names}");
    assert!(names.contains("json.requests"), "{names}");
}

/// The three ways a JSON export is refused, and the shape of each refusal.
#[tokio::test]
async fn otlp_json_refusals_are_json() {
    let (app, _root) = boot("json-errors");

    // An encoding OTLP does not define. 415 rather than a 400 that would read
    // like the body was corrupt.
    let (status, _) = post(&app, "/v1/logs", "text/plain", b"hello".to_vec()).await;
    assert_eq!(status, StatusCode::UNSUPPORTED_MEDIA_TYPE);

    // A trace id of the wrong length. Truncating it would store a span that can
    // never be joined and never explain why, so it is an error.
    let short = r#"{"resourceLogs":[{"scopeLogs":[{"logRecords":[
        {"timeUnixNano":"1","traceId":"abcd"}]}]}]}"#;
    let (status, body) = post(&app, "/v1/logs", "application/json", short.into()).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    // The error comes back in the encoding the client asked for.
    assert!(body.starts_with(r#"{"code":"#), "{body}");
    assert!(body.contains("traceId"), "{body}");

    // Not a document at all.
    let (status, body) = post(&app, "/v1/logs", "application/json", b"{[".to_vec()).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert!(body.starts_with(r#"{"code":"#), "{body}");

    // Protobuf is still the default when nothing says otherwise, and an empty
    // export is a valid one.
    let (status, body) = post(&app, "/v1/logs", "application/x-protobuf", vec![]).await;
    assert_eq!(status, StatusCode::OK, "{body}");
}

/// The first request most deployments will ever send.
///
/// `compression: gzip` is the default on both stock collector exporters and the
/// OTLP spec makes it a MUST for a server, so a receiver that only speaks plain
/// bodies rejects every batch — and rejects it with a 400, which OTLP calls
/// permanent, so the exporter drops the data rather than retrying it.
#[tokio::test]
async fn gzipped_exports_are_accepted_on_both_body_encodings() {
    let (app, _root) = boot("gzip");

    let msg = logs_export("checkout", 1_000, 4).encode_to_vec();
    let (status, body) = post_enc(
        &app,
        "/v1/logs",
        "application/x-protobuf",
        "gzip",
        gzip(&msg),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");

    let doc = br#"{"resourceLogs":[{"scopeLogs":[{"logRecords":[
        {"timeUnixNano":"2000","body":{"stringValue":"gzipped json arrived"}}]}]}]}"#;
    let (status, body) = post_enc(&app, "/v1/logs", "application/json", "gzip", gzip(doc)).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body, "{}", "a JSON client gets a JSON answer");

    // Both landed, which is the part a 200 alone would not prove.
    let all = query(&app, r#"{"signal":"logs","from":0,"to":100000}"#).await;
    assert!(all.contains("checkout handled request 3"), "{all}");
    assert!(all.contains("gzipped json arrived"), "{all}");

    // `identity` is the spec's name for "not compressed" and means what an
    // absent header means.
    let (status, body) = post_enc(
        &app,
        "/v1/logs",
        "application/x-protobuf",
        "identity",
        msg.clone(),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");

    // An encoding we do not implement is 415, so the exporter is told to stop
    // offering it. Feeding deflate to a protobuf parser produces "invalid wire
    // type", which sends whoever reads it looking for corrupt data.
    let (status, body) = post_enc(
        &app,
        "/v1/logs",
        "application/x-protobuf",
        "br",
        msg.clone(),
    )
    .await;
    assert_eq!(status, StatusCode::UNSUPPORTED_MEDIA_TYPE, "{body}");

    // A header that lies about the bytes is the client's error, not corruption
    // of ours.
    let (status, body) = post_enc(&app, "/v1/logs", "application/x-protobuf", "gzip", msg).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert!(body.contains("decompress"), "{body}");

    // A bomb: 80 MiB of zeros in 80 KB of gzip. The body limit bounds the
    // request, not what comes out of it, so the cap has to be its own thing.
    let (status, body) = post_enc(
        &app,
        "/v1/logs",
        "application/json",
        "gzip",
        gzip(&vec![b'0'; 80 << 20]),
    )
    .await;
    assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE, "{body}");
    assert!(body.starts_with(r#"{"code":"#), "{body}");
}

/// `ingest.max_request_bytes` is one number applied three times.
///
/// A batch a collector sends happily over 4317 and that fails over 4318 is the
/// worst kind of bug to be handed — it depends on a transport nobody changed —
/// and since an exporter reads 413 as permanent and drops the batch, the
/// disagreement costs data rather than latency. So the axum body limit, tonic's
/// `max_decoding_message_size` and the gzip inflation cap are asserted here
/// against the same configured number, at a size small enough to test cheaply.
#[tokio::test]
async fn one_size_limit_governs_both_transports_and_gzip() {
    const LIMIT: usize = 4 << 10;
    let (mut recv, _api, _root) = wire("max-request-bytes");
    recv.max_request_bytes = LIMIT;
    let grpc_app = tonic::service::Routes::default()
        .add_service(recv.logs_server())
        .into_axum_router();
    let http = receiver::http_router(recv.clone());

    let over = vec![b'0'; LIMIT + 1];

    // 4318, uncompressed: axum's `DefaultBodyLimit`, whose own default is 2 MiB.
    let (status, body) = post(&http, "/v1/logs", "application/json", over.clone()).await;
    assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE, "{body}");

    // 4318, gzipped: 4 KiB of zeros compresses to a few dozen bytes, so the body
    // limit never sees this one. The cap on what comes *out* is the same number.
    let (status, body) = post_enc(&http, "/v1/logs", "application/json", "gzip", gzip(&over)).await;
    assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE, "{body}");
    assert!(body.contains("max_request_bytes"), "{body}");

    // 4317: tonic checks the length prefix, so this never reaches prost. It
    // answers OUT_OF_RANGE (11), which OTLP classes as *retryable* where the
    // 413 above is permanent — so the two transports agree on the size and
    // disagree on the verdict. Pinned rather than papered over: the sender
    // retries an export that can never fit until its queue gives up.
    // ponytail: tonic's own status, hand-roll the framing check if the retry
    // storm ever shows up in a real deployment.
    assert_eq!(
        grpc(&grpc_app, "logs.v1.LogsService", None, over).await,
        "11"
    );

    // And the line is a ceiling, not a wall: a real export under it still lands
    // on both transports.
    let msg = logs_export("checkout", 1_000, 3).encode_to_vec();
    assert!(msg.len() < LIMIT, "{} bytes", msg.len());
    assert_eq!(
        grpc(&grpc_app, "logs.v1.LogsService", None, msg.clone()).await,
        "0"
    );
    let (status, body) = post(&http, "/v1/logs", "application/x-protobuf", msg).await;
    assert_eq!(status, StatusCode::OK, "{body}");
}

/// The same requirement on 4317, where it is tonic's framing rather than ours.
///
/// Without `accept_compressed` this comes back `UNIMPLEMENTED` (12), which OTLP
/// classes as permanent — the exporter drops the batch instead of retrying it,
/// so the failure is silent on both ends.
#[tokio::test]
async fn a_gzipped_grpc_export_is_accepted_on_every_signal() {
    let (grpc_app, api, _root) = boot_grpc("grpc-gzip");

    let logs = logs_export("checkout", 1_000, 3).encode_to_vec();
    let svc = "logs.v1.LogsService";
    assert_eq!(grpc(&grpc_app, svc, Some("gzip"), gzip(&logs)).await, "0");
    // Uncompressed still works, which is the other half of the MUST.
    assert_eq!(grpc(&grpc_app, svc, None, logs).await, "0");

    // An empty export is valid, and the point here is the three services are
    // configured alike — one of them left plain is the bug this catches.
    for svc in [
        "trace.v1.TraceService",
        "metrics.v1.MetricsService",
        "logs.v1.LogsService",
    ] {
        assert_eq!(
            grpc(&grpc_app, svc, Some("gzip"), gzip(&[])).await,
            "0",
            "{svc} refused a gzipped export"
        );
    }

    // 12 is UNIMPLEMENTED: the encoding is refused by name, not mis-decoded.
    assert_eq!(grpc(&grpc_app, svc, Some("deflate"), gzip(&[])).await, "12");

    // Both of the accepted logs exports landed, in one block or two.
    let all = query(&api, r#"{"signal":"logs","from":0,"to":100000}"#).await;
    assert_eq!(all.matches("handled request").count(), 6, "{all}");
}

/// Alerting end to end: real blocks, real counts, real state transitions.
///
/// The unit tests in `alert.rs` drive the state machine and the payload
/// formats with hand-built values. What only this can show is that the number
/// the machine is driven by came out of the storage engine — that `limit: 0`
/// really counts every matching row rather than the zero rows it returns, and
/// that a rule sees data through the open block the way every other reader
/// does, with no flush in between.
#[tokio::test]
async fn a_rule_counts_what_the_query_api_would_have_returned() {
    // `logs_export` makes every other record an ERROR, so ten records from one
    // service is a 50% error rate — comfortably over a 5% threshold, and a
    // number this test can assert exactly rather than approximately.
    let now = crate::api::now_nanos() as u64;
    let (app, api, _) = boot_alerting(
        "alerting",
        r#"{
          "rules": [
            { "name": "checkout-error-rate", "over": "1m", "when": "ratio > 5%",
              "severity": "critical",
              "query": { "signal": "logs", "where": [
                 { "attr": "service.name", "eq": "checkout" },
                 { "field": "severity_number", "gte": 17 } ] },
              "of":    { "signal": "logs", "where": [
                 { "attr": "service.name", "eq": "checkout" } ] } },
            { "name": "ghost-traffic", "over": "1m", "when": "count > 0",
              "query": { "signal": "logs", "where": [
                 { "attr": "service.name", "eq": "not-deployed" } ] } }
          ]
        }"#,
    );

    // Nothing ingested yet: the ratio rule must read 0/0 as "no traffic", not
    // as a 100% error rate. This is the false page the arithmetic invites.
    api.alerts.tick(&api).await;
    let quiet = String::from_utf8(get(&app, "/api/v1/alerts", None).await.1).unwrap();
    assert_eq!(quiet.matches(r#""state":"ok""#).count(), 2, "{quiet}");

    otlp(&app, "/v1/logs", logs_export("checkout", now, 10)).await;
    otlp(&app, "/v1/logs", logs_export("payments", now, 4)).await;
    api.alerts.tick(&api).await;

    let (status, bytes, _) = get(&app, "/api/v1/alerts", None).await;
    assert_eq!(status, StatusCode::OK);
    let body = String::from_utf8(bytes).unwrap();
    // Five of ten, and the denominator excluded payments — so both queries ran
    // and both were filtered, rather than one of them counting the block.
    assert!(body.contains(r#""matched":5"#), "{body}");
    assert!(body.contains(r#""total":10"#), "{body}");
    assert!(body.contains(r#""value":0.5"#), "{body}");
    assert!(body.contains(r#""state":"firing""#), "{body}");
    assert!(body.contains(r#""severity":"critical""#), "{body}");
    // The rule with no matching traffic stays quiet, and stays *reported*: an
    // alerting surface that only lists what is firing cannot answer "is the
    // rule I wrote yesterday actually evaluating?".
    assert!(body.contains(r#""name":"ghost-traffic""#), "{body}");
    assert_eq!(body.matches(r#""state":"ok""#).count(), 1, "{body}");
    assert!(body.contains(r#""error":null"#), "{body}");

    // The count is the query API's own, not a second implementation of it: the
    // same filter over the same window, through HTTP, agrees at 5.
    let window = format!(
        r#""signal":"logs","from":{},"to":{},
           "where":[{{"attr":"service.name","eq":"checkout"}},
                    {{"field":"severity_number","gte":17}}]"#,
        now.saturating_sub(60_000_000_000),
        now + 60_000_000_000
    );
    let counted = query(&app, &format!("{{{window},\"limit\":100}}")).await;
    assert!(counted.contains(r#""rows_matched":5"#), "{counted}");
    // A limit the scan can reach lets it stop early, and then `rows_matched`
    // is only what it got to — at this size it never stops, so the difference
    // is invisible here. `limit: 0` is what makes the evaluator's count exact
    // at every size; that is `alert::count`'s reason for existing.
}

/// A webhook receiver on a real socket.
///
/// A mock at the `post` boundary would test everything except the part that can
/// actually be wrong: that the client builds a request a server accepts, that
/// the body is the JSON the receiving product's schema wants, and that the
/// connection is left reusable. So this is a socket, and `alert::post` is the
/// real hyper client talking to it.
///
/// Reads headers to the blank line and then exactly `content-length` bytes:
/// the client pools its connections and keeps them open, so a read to EOF here
/// would hang forever rather than fail.
fn webhook(
    calls: usize,
) -> (
    String,
    std::net::SocketAddr,
    std::sync::mpsc::Receiver<String>,
) {
    use std::io::{Read, Write};
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("addr");
    let url = format!("http://{addr}/hook");
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        for sock in listener.incoming().take(calls) {
            let mut sock = sock.expect("accept");
            let mut head = Vec::new();
            let mut byte = [0u8; 1];
            while !head.ends_with(b"\r\n\r\n") {
                if sock.read(&mut byte).unwrap_or(0) == 0 {
                    break;
                }
                head.push(byte[0]);
            }
            let text = String::from_utf8_lossy(&head).to_lowercase();
            let len: usize = text
                .split("content-length:")
                .nth(1)
                .and_then(|t| t.split("\r\n").next())
                .and_then(|t| t.trim().parse().ok())
                .unwrap_or(0);
            let mut body = vec![0u8; len];
            sock.read_exact(&mut body).expect("body");
            let _ = sock.write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 0\r\n\r\n");
            let _ = tx.send(String::from_utf8_lossy(&body).into_owned());
        }
    });
    (url, addr, rx)
}

/// One page when it starts, one when it stops, and nothing in between.
///
/// The state machine's unit tests prove the *edges*; this proves that an edge
/// reaches a webhook — over a socket, in the receiving product's own JSON, with
/// the link that opens the rows that fired. It also proves the two things a
/// dispatcher gets wrong under load: that a target which cannot be reached does
/// not stop the ones that can, and that a rule which is still firing on the
/// next evaluation does not page again.
#[tokio::test]
async fn a_transition_pages_once_and_a_dead_target_does_not_swallow_a_live_one() {
    let now = crate::api::now_nanos() as u64;
    let (hook, hook_addr, got) = webhook(3);
    // A connection that closes before sending a byte. Health checkers and port
    // scanners do this constantly, and the header loop reads one byte at a time
    // — without the EOF break it spins on `read` returning 0 and the receiver
    // never serves the page behind it. Asserted as an empty body arriving,
    // because "the thread is still alive" is the property.
    drop(std::net::TcpStream::connect(hook_addr).expect("probe"));
    assert_eq!(
        got.recv_timeout(Duration::from_secs(5)).expect("probe"),
        "",
        "a client that sends nothing must not wedge the receiver"
    );
    // Port 1 is reserved and nothing listens on it, so this refuses immediately
    // rather than costing the test the 10-second webhook timeout. It is first in
    // `notify` on purpose: a dispatcher that stops at the first failure would
    // then never reach the one this test asserts on.
    let rules = format!(
        r#"{{ "link_base": "http://mira.test",
              "notify": [ {{ "name": "dead",   "url": "http://127.0.0.1:1/x", "format": "json" }},
                          {{ "name": "oncall", "url": "{hook}",               "format": "slack" }} ],
              "rules": [
                {{ "name": "checkout-errors", "over": "1m", "when": "ratio > 40%",
                   "severity": "critical", "notify": ["dead", "oncall"],
                   "query": {{ "signal": "logs", "where": [
                      {{ "attr": "service.name", "eq": "checkout" }},
                      {{ "field": "severity_number", "gte": 17 }} ] }},
                   "of":    {{ "signal": "logs" }} }} ] }}"#
    );
    let (app, api, _) = boot_alerting("webhook", &rules);

    // Nothing stored: an empty denominator is 0, not 100%, so no page.
    api.alerts.tick(&api).await;

    otlp(&app, "/v1/logs", logs_export("checkout", now, 10)).await;
    api.alerts.tick(&api).await;
    let fired = got.recv_timeout(Duration::from_secs(5)).expect("fire");
    assert!(
        fired.starts_with(r#"{"text":"[FIRING] checkout-errors"#),
        "{fired}"
    );
    assert!(
        fired.contains(r"50.00% > 40.00% over 1m (5 of 10 records)"),
        "{fired}"
    );
    // Slack's own link form, and the URL is the rule's filter — paste it in a
    // browser and you are looking at the five records that fired it. Spelled out
    // rather than rebuilt from `link()`, because a test that calls the function
    // it is checking would pass through any encoding change at all.
    let expect = "<http://mira.test/#/logs?q=attr%3Aservice.name%3Dcheckout".to_owned()
        + "%20field%3Aseverity_number%3E%3D17&range=-60s|open in Mira>";
    assert!(fired.contains(&expect), "{fired}");

    // Still firing, so still quiet: `for` is a sustained breach, not a repeat.
    api.alerts.tick(&api).await;

    // Twenty records from another service move the denominator, not the
    // numerator: 5 of 30 is 16.67%, and the rule resolves.
    otlp(&app, "/v1/logs", logs_export("payments", now, 20)).await;
    api.alerts.tick(&api).await;
    let resolved = got.recv_timeout(Duration::from_secs(5)).expect("resolve");
    assert!(
        resolved.starts_with(r#"{"text":"[RESOLVED] checkout-errors"#),
        "{resolved}"
    );
    assert!(
        resolved.contains(r"16.67% > 40.00% over 1m (5 of 30 records)"),
        "{resolved}"
    );

    let body = String::from_utf8(get(&app, "/api/v1/alerts", None).await.1).unwrap();
    assert!(body.contains(r#""state":"ok""#), "{body}");
    // The webhook failing is not the rule failing: `error` is what went wrong
    // evaluating, and conflating the two would have an operator debugging a
    // filter when the problem is a Slack URL.
    assert!(body.contains(r#""error":null"#), "{body}");
}

/// The evaluator runs itself.
///
/// Every other alerting test drives `Engine::tick` directly, which is the right
/// unit to test — but it leaves the one thing that makes alerting *happen* on a
/// running node, the timer, exercised by nothing. `every: 50ms` so waiting for
/// it is a wait rather than a sleep, and the assertion is on the evaluation
/// stamp moving off zero rather than on a duration.
#[tokio::test]
async fn a_node_with_rules_evaluates_them_without_being_asked() {
    // No rules is the default deployment, and it must not start a task that
    // wakes up forever to iterate an empty list.
    let (_, quiet, _) = boot_alerting("timer-off", r#"{ "rules": [] }"#);
    let (guard, quiet_log) = capture();
    crate::alert::spawn(quiet.clone());
    drop(guard);
    assert!(quiet.alerts.json().contains(r#""alerts":[]"#));
    assert_eq!(quiet_log.text(), "", "no rules, no banner and no evaluator");

    let (app, api, _) = boot_alerting(
        "timer-on",
        r#"{ "every": "50ms", "rules": [
             { "name": "any-log", "over": "1m", "when": "count >= 1",
               "query": { "signal": "logs" } } ] }"#,
    );
    otlp(
        &app,
        "/v1/logs",
        logs_export("checkout", crate::api::now_nanos() as u64, 4),
    )
    .await;
    // The one line that tells an operator alerting is on and with what. It is
    // the only confirmation a rules file was loaded at all, and every value in
    // it is a field expression that no test evaluates without a subscriber.
    let (guard, log) = capture();
    crate::alert::spawn(api.clone());
    drop(guard);
    let banner = log.text();
    assert!(banner.contains("rules=1"), "{banner}");
    assert!(banner.contains("targets=0"), "{banner}");
    // Not the period: `human` floors below a second, so this file's 50ms reads
    // as `0s`. Harmless in the one line nobody sets a sub-second period for,
    // and not worth pinning as though it were intended.
    assert!(banner.contains("alerting"), "{banner}");

    // Polled rather than slept through: a fixed sleep is either a flake or a
    // second of wall clock, and this is done as soon as one tick has landed.
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        let body = api.alerts.json();
        if body.contains(r#""state":"firing""#) {
            assert!(body.contains(r#""matched":4"#), "{body}");
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "timer never fired: {body}"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// A rule whose query cannot run is broken, not satisfied.
///
/// The two are one `?` apart and they page in opposite directions: read as a
/// count of zero, a `count < 100` traffic rule fires the moment a block goes
/// bad and a `ratio` rule resolves itself. Both spellings of that are an
/// on-call woken by a corrupt file, or — worse — not woken by a real outage
/// because the alert already "resolved". So the failure is recorded against
/// the rule, published on `/api/v1/alerts`, and the state machine is not
/// advanced at all. The other rule still evaluating is the second half: one
/// unreadable signal must not take alerting down with it.
#[tokio::test]
async fn a_rule_whose_query_fails_records_the_failure_instead_of_a_count() {
    let now = crate::api::now_nanos();
    // Both thresholds are `>= 0`, which every count in existence satisfies:
    // if the failure were swallowed as a zero, `broken` would fire too.
    let (app, api, root) = boot_alerting(
        "corrupt",
        r#"{ "rules": [
             { "name": "broken", "over": "1m", "when": "count >= 0",
               "query": { "signal": "logs" } },
             { "name": "live",   "over": "1m", "when": "count >= 0",
               "query": { "signal": "traces" } } ] }"#,
    );

    // A block directory the catalog accepts holding a table it cannot read:
    // a truncated write, a bad sector, half a restored backup. Named in the
    // window the rule scans, or it would be pruned before it was opened.
    const NANOS_PER_HOUR: i64 = 3_600 * 1_000_000_000;
    let dir = root
        .join("logs")
        .join(format!("p={}", now.div_euclid(NANOS_PER_HOUR)))
        .join(format!(
            "{:020}-{:020}-{:08x}-{:012}-{:020}",
            now - 1_000,
            now,
            0u32,
            1u64,
            0u64
        ));
    std::fs::create_dir_all(&dir).expect("block dir");
    std::fs::write(dir.join("logs.arrow"), b"not an arrow file").expect("corrupt table");

    let (guard, log) = capture();
    api.alerts.tick(&api).await;
    drop(guard);

    // The operator's two channels: the log line naming the rule that failed,
    // and the transition for the one that did not.
    let text = log.text();
    assert!(text.contains("alert rule failed"), "{text}");
    assert!(text.contains("rule=broken"), "{text}");
    assert!(text.contains("rule=live"), "{text}");
    assert!(text.contains(r#"state="firing""#), "{text}");

    let body = String::from_utf8(get(&app, "/api/v1/alerts", None).await.1).unwrap();
    // Exactly one of the two fired, and the one that did not is the one whose
    // blocks are unreadable — still `ok`, with the reason attached.
    assert_eq!(body.matches(r#""state":"firing""#).count(), 1, "{body}");
    assert_eq!(body.matches(r#""state":"ok""#).count(), 1, "{body}");
    assert!(body.contains("logs.arrow"), "{body}");
    assert_eq!(body.matches(r#""error":null"#).count(), 1, "{body}");

    let _ = std::fs::remove_dir_all(&root);
}

// ---------------------------------------------------------------------------
// A record's life, a node's death, and the two contracts that have to hold
// across both.
//
// Everything below drives one of four properties: that a record reads back
// identically at every stage of the pipeline, that a kill costs nothing that
// was acknowledged and duplicates nothing that was, that the engine, the API
// and both of the TUI's transports answer the same question the same way while
// a writer is running, and that the evaluator's verdict is a function of the
// data and of nothing else.
// ---------------------------------------------------------------------------

/// The `rows` array of a query envelope, verbatim.
///
/// Comparing the array as text is the point. `assert_eq!(count, count)` passes
/// through a reordering, a re-rendered timestamp and a dropped null, and every
/// one of those is a real regression at one of the stage boundaries below.
fn rows_of(body: &str) -> String {
    let s = body.find(r#""rows":["#).expect("no rows array") + 8;
    body[s..body.find(r#"],"stats":"#).expect("no stats object")].to_owned()
}

/// Every row's `body`, in the order the answer put them in.
///
/// Coarser than [`rows_of`] and used only where a concurrent writer makes the
/// full text legitimately different between two reads: the *sequence* is still
/// exact, so it still catches a row that moved, changed or vanished.
fn bodies_of(rows: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut rest = rows;
    while let Some(i) = rest.find(r#""body":""#) {
        let s = &rest[i + 8..];
        let e = s.find('"').expect("unterminated body");
        out.push(s[..e].to_owned());
        rest = &s[e..];
    }
    out
}

/// The pipeline configuration the crash tests boot under, over a directory that
/// may already hold data.
///
/// Ten minutes of block age, against [`wire_with`]'s hundred milliseconds. Every
/// test below that uses this is asserting about something *other* than the age
/// clock — what a kill costs, what a shutdown flushes, what straddles a block
/// boundary — and each of them would be a race against it. A block that appears
/// here appeared for a reason the test named.
fn node_cfg(root: &std::path::Path, wal: bool) -> pipeline::Config {
    pipeline::Config {
        data_dir: root.to_path_buf(),
        max_block_age: Duration::from_secs(600),
        wal: wal.then(|| {
            Arc::new(mira_core::wal::Wal::open(root, mira_core::block::node_id("mira")).unwrap())
        }),
        ..Default::default()
    }
}

/// One running node: the router the binary serves, and the flusher handles that
/// make its shutdown a fact rather than a hope.
///
/// [`wire_with`] discards those handles, which is right for a test that only
/// reads. A crash test cannot use it for two reasons: it empties the directory,
/// so there is no second node over the same data, and without the handles there
/// is no difference between "asked the flushers to stop" and "they stopped" —
/// which is the difference between asserting about recovery and asserting about
/// a race.
struct Node {
    app: Router,
    api: api::Api,
    flushers: [tokio::task::JoinHandle<()>; 3],
}

impl Node {
    /// The shutdown `main` performs on a signal: drop every [`pipeline::Ingest`]
    /// — the router owns all three — and each flusher seals what is open,
    /// publishes it and returns. Awaiting the handles is the whole contract.
    async fn stop(self) {
        let Node { app, api, flushers } = self;
        drop((app, api));
        for h in flushers {
            // Bounded rather than a bare await: an `Ingest` clone left alive
            // anywhere in the stack turns "graceful shutdown" into a test that
            // hangs until CI times out, with no line to point at.
            tokio::time::timeout(Duration::from_secs(20), h)
                .await
                .expect("a flusher did not stop; some Ingest clone outlived the router")
                .expect("flusher panicked");
        }
    }

    /// A kill: the tasks stop wherever they are, nothing is sealed, and what
    /// survives is whatever the log already holds. That last clause is the claim
    /// every test that calls this is making.
    fn kill(self) {
        for h in &self.flushers {
            h.abort();
        }
        forget_open_blocks();
    }
}

/// Drop this test's claim on the process-wide open-block gauges.
///
/// A real crash takes the counters with the process. In-process it does not:
/// [`pipeline::REJECTS`] is a `static`, `open_since` is only cleared on the two
/// paths a killed — or runtime-dropped — flusher will never reach, and a test
/// that ends with rows still in an open block therefore leaves that signal
/// reading as open for the rest of the binary's run. `/api/v1/stats` reports
/// the gauge, `stats_reports_what_this_node_is_doing_with_its_disk` asserts it
/// is `null`, and a leak here fails that test instead of this one.
///
/// ponytail: the counters are process-global because the binary is one node —
/// the right fix is per-node state, and it is a `main.rs` change, not a test's.
fn forget_open_blocks() {
    for r in &pipeline::REJECTS {
        r.open_since.store(0, std::sync::atomic::Ordering::Relaxed);
    }
}

/// Boot a node the way `serve_with` boots one: open the log, start the three
/// flushers, replay whatever the log holds that no block covers, and only then
/// serve.
///
/// The replay finishes *before* the router exists for the reason `main` runs it
/// before binding. A replayed frame keeps the sequence it already has, so a live
/// export accepted alongside it would be numbered above a frame no block covers
/// yet, and the next watermark would step straight over that frame.
async fn restart_with(cfg: pipeline::Config) -> Node {
    let root = cfg.data_dir.clone();
    let id = cfg.node;
    let cfg = Arc::new(cfg);
    let (logs, o_logs, h_logs) = pipeline::spawn::<mira_core::logs::LogsBuilder>(cfg.clone());
    let (traces, o_traces, h_traces) =
        pipeline::spawn::<mira_core::traces::TracesBuilder>(cfg.clone());
    let (metrics, o_metrics, h_metrics) =
        pipeline::spawn::<mira_core::metrics::MetricsBuilder>(cfg);
    crate::replay(&root, id, logs.clone(), traces.clone(), metrics.clone())
        .await
        .expect("replay");
    let recv = receiver::Receivers {
        logs,
        traces,
        metrics,
        max_request_bytes: crate::config::Config::default().max_request_bytes,
    };
    let api = api::Api {
        data_dir: Arc::new(root),
        open: [o_logs, o_traces, o_metrics],
        alerts: Arc::default(),
    };
    Node {
        app: router_for(recv, api.clone()),
        api,
        flushers: [h_logs, h_traces, h_metrics],
    }
}

async fn restart(root: &std::path::Path, wal: bool) -> Node {
    restart_with(node_cfg(root, wal)).await
}

/// Wait for one more published block than there were, and hand back the
/// directory as it now stands.
///
/// Polled off `scan`, not slept through: a fixed sleep long enough to be safe on
/// a loaded runner is most of a suite's runtime, and one short enough not to be
/// is a flake. The deadline is the point of the helper — a block that never
/// seals fails here, rather than three assertions later as an empty answer that
/// reads like a query bug.
async fn seal(root: &std::path::Path, signal: &str) -> Vec<mira_core::block::BlockRef> {
    let before = mira_core::block::scan(root, signal).unwrap().len();
    let deadline = std::time::Instant::now() + Duration::from_secs(20);
    loop {
        let blocks = mira_core::block::scan(root, signal).unwrap();
        if blocks.len() > before {
            return blocks;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "{signal} never sealed; everything below this would have proved nothing"
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

/// Put a router on a real loopback socket and hand back `host:port`.
///
/// Everything else here reaches the router through `oneshot`, which needs no
/// port. The TUI cannot be reached that way: `tui::source` is a blocking
/// `std::net::TcpStream` by design, so the only way to assert that the TUI
/// parses what the API emits is to make the API emit it over TCP.
async fn serve(app: Router) -> String {
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap().to_string();
    // `axum::serve` is not a future, it is `IntoFuture`, so it needs the one
    // conversion — but not an `async move` block around it, which would only
    // reach its own closing brace if the server ever stopped.
    tokio::spawn(axum::serve(l, app).into_future());
    addr
}

/// One record's whole life, and the same bytes out of it at every stage.
///
/// Accepted, framed in the log, read out of the open block, sealed on the age
/// clock, published, mapped, rewritten into the cold tier, unlinked by
/// retention. What this catches that nothing else in the suite does is a stage
/// that answers *almost* the same thing: a snapshot that renders a timestamp
/// differently from the block it becomes, a ZSTD round trip that loses a
/// column's nulls, a cold block whose rows come back in another order. Counting
/// rows is blind to all three, so the assertion is on the bytes.
#[tokio::test]
async fn a_record_reads_back_identically_at_every_stage_of_its_life() {
    let root = fresh_dir("lifecycle");
    // Long enough that the first three assertions are not a race against the
    // seal, short enough that waiting for it is free.
    let n = restart_with(pipeline::Config {
        max_block_age: Duration::from_millis(300),
        ..node_cfg(&root, true)
    })
    .await;
    let doc = r#"{"signal":"logs","from":0,"to":8000,"limit":1000}"#;
    let id = mira_core::block::node_id("mira");

    // 1. Accepted. Under the shipped default a 200 means the frame is in the
    //    log, so the frame has to be there and the block has to not.
    otlp(&n.app, "/v1/logs", logs_export("checkout", 1_000, 200)).await;
    assert!(
        mira_core::block::scan(&root, "logs").unwrap().is_empty(),
        "publishing before the acknowledgement is the other contract, and \
         boot_sealing is the test for it"
    );
    let framed: u64 = std::fs::read_dir(root.join(".wal"))
        .unwrap()
        .map(|e| e.unwrap().metadata().unwrap().len())
        .sum();
    assert!(framed > 0, "acknowledged without being logged");

    // 2. Open block. This answer is what every later stage has to match.
    let open = rows_of(&query(&n.app, doc).await);
    assert_eq!(open.matches("handled request").count(), 200);

    // 3. Sealed and published, off the age clock.
    let published = seal(&root, "logs").await;
    assert_eq!(published.len(), 1);
    assert_eq!(
        published[0].wal_hi, 1,
        "a block published under a log claims the frames it holds, or the next \
         boot replays them into a second copy"
    );
    assert_eq!(
        rows_of(&query(&n.app, doc).await),
        open,
        "the seal changed the answer"
    );

    // 4. Mapped, and zero-copy: the hot tier's whole claim is that a query reads
    //    the mapping rather than a decoded copy of it.
    let dir = published[0].dir.clone();
    let table = dir.join("logs.arrow");
    let plain_bytes = std::fs::metadata(&table).unwrap().len();
    let mapped = mira_core::block::open_table(&table).unwrap();
    let rows_of_table = |t: &mira_core::block::MappedTable| -> usize {
        t.batches.iter().map(|b| b.num_rows()).sum()
    };
    let mapped_rows = rows_of_table(&mapped);
    assert_eq!(mapped_rows, 200);
    let hot = mapped.zero_copy_ratio();
    assert_eq!(
        hot.0, hot.1,
        "a hot block copied buffers out of its mapping"
    );

    // 5. Cold tier. The cutoff is above the block's `max_ts`, so this is exactly
    //    the call the sweep makes an hour later, without the hour.
    assert_eq!(
        mira_core::block::compact(&root, "logs", id, i64::MAX).unwrap(),
        1
    );
    assert!(
        dir.join("cold").exists(),
        "the marker is written last, so its absence means an unfinished rewrite"
    );
    assert!(
        std::fs::metadata(&table).unwrap().len() < plain_bytes,
        "a table that did not shrink was not ZSTD-rewritten"
    );
    let cold = mira_core::block::open_table(&table)
        .unwrap()
        .zero_copy_ratio();
    assert!(
        cold.0 < cold.1 && cold.0 < hot.0,
        "a compressed block cannot be zero-copy — decompression has to allocate — \
         so a cold table reporting {cold:?} against the hot {hot:?} means the \
         rewrite left plain batches behind"
    );
    // The reader that mapped the block before the sweep keeps reading the inode
    // it mapped — POSIX holds an unlinked file open under its mappings — which
    // is the entire reason compaction needs no reader lease and no refcount.
    assert_eq!(
        rows_of_table(&mapped),
        mapped_rows,
        "the rename pulled the mapping out from under a live reader"
    );
    assert_eq!(
        rows_of(&query(&n.app, doc).await),
        open,
        "compaction changed the answer"
    );

    // The invalid transition. A published block is rewritten once and never
    // again: a sweep that recompressed cold tables would spend the volume's
    // whole read bandwidth every minute for as long as the data lives.
    assert_eq!(
        mira_core::block::compact(&root, "logs", id, i64::MAX).unwrap(),
        0,
        "the cold block was compacted a second time"
    );

    // 6. Expired. Unlinked, never truncated: a reader holding a mapping into a
    //    truncated file takes SIGBUS, and nothing here knows who is reading.
    assert_eq!(
        mira_core::block::expire(&root, "logs", i64::MAX).unwrap(),
        1
    );
    assert!(mira_core::block::scan(&root, "logs").unwrap().is_empty());
    assert!(
        rows_of(&query(&n.app, doc).await).is_empty(),
        "an expired block is still being answered from"
    );
    assert_eq!(
        rows_of_table(&mapped),
        mapped_rows,
        "the unlink took the mapping with it"
    );
    n.stop().await;
}

/// The two seal triggers that are not the age clock, and the boot that must
/// undo neither of them.
///
/// [`node_cfg`] puts the age at ten minutes in both halves, which is the
/// assertion: a block that appears at all appeared for the other reason. Size
/// first, then the shutdown flush — and then the half a shutdown flush is only
/// worth anything with, which is that the next boot replays nothing it
/// published.
#[tokio::test]
async fn a_block_seals_on_size_and_on_shutdown_and_neither_is_replayed_afterwards() {
    let doc = r#"{"signal":"logs","from":0,"to":8000,"limit":100}"#;

    // Size. One byte of target, so the first non-empty builder is already over
    // it and the seal happens on the turn that appended the rows.
    let root = fresh_dir("seal-size");
    let n = restart_with(pipeline::Config {
        target_block_bytes: 1,
        ..node_cfg(&root, true)
    })
    .await;
    otlp(&n.app, "/v1/logs", logs_export("checkout", 1_000, 4)).await;
    assert_eq!(seal(&root, "logs").await.len(), 1);
    n.stop().await;

    // Shutdown. Nothing else can seal this one: the target is the shipped 32 MB
    // and the age clock outlives the test.
    let root = fresh_dir("seal-shutdown");
    let n = restart(&root, true).await;
    otlp(&n.app, "/v1/logs", logs_export("checkout", 1_000, 5)).await;
    let before = rows_of(&query(&n.app, doc).await);
    assert!(
        mira_core::block::scan(&root, "logs").unwrap().is_empty(),
        "something sealed early and the assertion below is now free"
    );
    n.stop().await;
    assert_eq!(
        mira_core::block::scan(&root, "logs").unwrap().len(),
        1,
        "the shutdown cost the block that was filling"
    );

    // The frame is covered now, so the next boot must not touch it. Get this
    // wrong and every restart re-ingests the whole log — silently, as duplicate
    // rows, which is the failure an operator can least detect.
    let watermarks = mira_core::block::wal_watermarks(&root).unwrap();
    assert_eq!(watermarks[0], 1, "the block did not claim its frame");
    let node = mira_core::block::node_id("mira");
    let mut seen: Vec<u64> = Vec::new();
    let mut sink = |_: mira_core::wal::Signal, seq: u64, _: &[u8]| {
        seen.push(seq);
        Ok(())
    };
    let again = mira_core::wal::Wal::replay(&root, node, watermarks, &mut sink).unwrap();
    assert_eq!(
        (again.replayed, again.skipped),
        (0, 1),
        "a frame inside a published block was replayed anyway"
    );

    // The same log and the same sink under a watermark that covers nothing.
    // Without it the pass above is only a count: a watermark one too high skips
    // the frame *after* the one the block holds and reads as `(0, 1)` here too,
    // and the frame it ate would be gone at the next real boot. Only the
    // sequence the sink is handed says which frame the watermark is one past —
    // and since the first pass replayed nothing, that is where it came from.
    let all = mira_core::wal::Wal::replay(&root, node, [0; 3], &mut sink).unwrap();
    assert_eq!((all.replayed, all.skipped), (1, 0));
    assert_eq!(
        seen,
        [0],
        "the watermark is not one past the frame the block published"
    );

    // And the same property through the real boot, because the assertion above
    // is about `wal.rs` and this one is about the rows a reader gets.
    let n = restart(&root, true).await;
    let after = query(&n.app, doc).await;
    assert_eq!(after.matches("handled request").count(), 5, "{after}");
    assert_eq!(rows_of(&after), before, "the restart moved a row");
    n.stop().await;
    assert_eq!(
        mira_core::block::scan(&root, "logs").unwrap().len(),
        1,
        "the second shutdown published a duplicate or an empty block"
    );
}

/// A kill with the block still open, which is the window the log exists for.
///
/// Every export here was acknowledged, and under the shipped default an
/// acknowledgement is a frame and nothing else on disk. The two assertions are a
/// pair and both are needed: every acknowledged record comes back, and no record
/// comes back twice. Recovery is easy to get right in one direction only, and
/// the cost is silent either way — a lost export looks like a sender that never
/// sent, a duplicated one like a sender that retried.
#[tokio::test]
async fn a_kill_with_the_block_open_loses_no_acknowledged_export_and_duplicates_none() {
    let root = fresh_dir("crash-open");
    let doc = r#"{"signal":"logs","from":0,"to":8000,"limit":100}"#;

    let n = restart(&root, true).await;
    for base in [1_000u64, 2_000, 3_000] {
        otlp(&n.app, "/v1/logs", logs_export("checkout", base, 5)).await;
    }
    let before = rows_of(&query(&n.app, doc).await);
    assert_eq!(before.matches("handled request").count(), 15);
    assert!(
        mira_core::block::scan(&root, "logs").unwrap().is_empty(),
        "the data reached disk before the kill, so this proves nothing"
    );
    n.kill();

    let n = restart(&root, true).await;
    let after = query(&n.app, doc).await;
    assert_eq!(
        after.matches("handled request").count(),
        15,
        "recovery lost or duplicated a record: {after}"
    );
    // Not only the count: a replay that reordered the frames would keep it and
    // move every cursor a client is holding.
    assert_eq!(rows_of(&after), before);
    n.stop().await;

    assert_eq!(mira_core::block::wal_watermarks(&root).unwrap()[0], 3);
    let n = restart(&root, true).await;
    let third = query(&n.app, doc).await;
    assert_eq!(
        third.matches("handled request").count(),
        15,
        "the second boot replayed frames the first one had published: {third}"
    );
    n.stop().await;
}

/// A kill between the seal and the rename.
///
/// `publish` stages the tables under `.tmp` and makes them visible with one
/// atomic rename, so there is no half-published block for recovery to repair —
/// but there is a staging directory holding tables for data that was
/// acknowledged, and `.tmp` is on no read path. Both halves matter: the leftover
/// has to go, and the records have to come back from the log rather than from
/// it.
///
/// The leftover is written by hand rather than raced for. The window is a few
/// hundred microseconds wide, and a test that tried to land a kill inside it
/// would assert nothing most of the time and flake the rest.
#[tokio::test]
async fn a_kill_between_the_seal_and_the_rename_drops_the_staging_dir_and_not_the_data() {
    let root = fresh_dir("crash-staging");
    let doc = r#"{"signal":"logs","from":0,"to":8000,"limit":100}"#;
    let id = mira_core::block::node_id("mira");

    let n = restart(&root, true).await;
    otlp(&n.app, "/v1/logs", logs_export("checkout", 1_000, 5)).await;
    n.kill();

    let staged = root.join(".tmp").join(format!(
        "logs-{id:08x}-000000000000-{:020}-{:020}",
        1_000, 1_004
    ));
    std::fs::create_dir_all(&staged).unwrap();
    std::fs::write(staged.join("logs.arrow"), b"half a table").unwrap();

    let n = restart(&root, true).await;
    let body = query(&n.app, doc).await;
    assert_eq!(
        body.matches("handled request").count(),
        5,
        "the export was acknowledged on the log and the log did not bring it back: {body}"
    );
    assert!(
        !staged.exists(),
        "the staging directory outlived the process that made it; nothing will \
         ever look in .tmp again, so that is leaked disk for the volume's life"
    );
    n.stop().await;
    assert_eq!(mira_core::block::scan(&root, "logs").unwrap().len(), 1);
}

/// A torn tail costs the frame that was in flight and nothing behind it.
///
/// This is the normal shape of a segment after a hard kill: `append` is three
/// `write_all`s and the process died between them, so the last frame has no
/// checksum. The sender never saw a 200 for it — the acknowledgement is returned
/// after the third write — so losing it is correct, and replaying half of it
/// would not be. Every frame before it was acknowledged and has to come back.
///
/// `wal.rs`'s own torn-tail test is this property at the reader. This is the
/// same property at the pipeline: the frames become rows, through the real
/// flushers and a real publish.
#[tokio::test]
async fn a_torn_wal_tail_costs_only_the_frame_that_was_in_flight() {
    let root = fresh_dir("crash-torn");
    let doc = r#"{"signal":"logs","from":0,"to":8000,"limit":100}"#;

    let n = restart(&root, true).await;
    otlp(&n.app, "/v1/logs", logs_export("checkout", 1_000, 5)).await;
    otlp(&n.app, "/v1/logs", logs_export("payments", 2_000, 5)).await;
    n.kill();

    // Chop four bytes: the second frame now has no checksum, which is where a
    // kill inside `append` leaves it.
    let seg = std::fs::read_dir(root.join(".wal"))
        .unwrap()
        .map(|e| e.unwrap().path())
        .find(|p| p.extension().is_some_and(|e| e == "wal"))
        .expect("a segment");
    let len = std::fs::metadata(&seg).unwrap().len();
    std::fs::OpenOptions::new()
        .write(true)
        .open(&seg)
        .unwrap()
        .set_len(len - 4)
        .unwrap();

    // `wal: true`, which is the path `serve_with` runs: opening the log over a
    // torn directory is a boot condition rather than an error, and the test
    // below is the one that holds that.
    let n = restart(&root, true).await;
    let body = query(&n.app, doc).await;
    assert_eq!(
        body.matches("checkout handled request").count(),
        5,
        "the whole frame in front of the tear did not survive: {body}"
    );
    assert_eq!(
        body.matches("payments handled request").count(),
        0,
        "half a frame was replayed; the checksum exists to stop exactly that"
    );
    n.stop().await;
    assert_eq!(
        mira_core::block::wal_watermarks(&root).unwrap()[0],
        1,
        "the recovered block claimed a frame the tear had swallowed"
    );
}

/// A crashed node has to start.
///
/// A torn tail is what every hard kill leaves behind, and `Wal::replay` is built
/// for it: it ends the segment rather than failing it, and reports it in
/// `Replayed::torn_segments`. `Wal::open` walks the same frames to resume its
/// sequence counter, and it used to propagate the tear with `?` — so
/// `serve_with`'s `Wal::open(&cfg.data_dir, node)?` turned an ordinary crash
/// into a node that would not boot at all. It now ends the segment the way
/// `replay` does, and resumes past the tear rather than onto it.
#[tokio::test]
async fn a_torn_wal_tail_does_not_stop_the_node_from_booting() {
    let root = fresh_dir("crash-torn-boot");
    let id = mira_core::block::node_id("mira");
    let n = restart(&root, true).await;
    otlp(&n.app, "/v1/logs", logs_export("checkout", 1_000, 5)).await;
    n.kill();

    let seg = std::fs::read_dir(root.join(".wal"))
        .unwrap()
        .map(|e| e.unwrap().path())
        .find(|p| p.extension().is_some_and(|e| e == "wal"))
        .expect("a segment");
    let len = std::fs::metadata(&seg).unwrap().len();
    std::fs::OpenOptions::new()
        .write(true)
        .open(&seg)
        .unwrap()
        .set_len(len - 4)
        .unwrap();

    // `expect` rather than `assert!(is_ok())` with the error formatted into the
    // message: it prints the same `Debug` on failure without putting the call
    // that produces it on a line only a failing run ever reaches.
    mira_core::wal::Wal::open(&root, id).expect(
        "a hard kill leaves a torn tail, so refusing to open the log refuses \
         every restart after a crash",
    );
}

/// A crash in the middle of the cold rewrite.
///
/// `compact_block` renames each table into place and writes the `cold` marker
/// last, so a kill inside it leaves a directory with some tables ZSTD-encoded
/// and some not. That has to read correctly — the codec is per-batch IPC
/// metadata, not a property of the directory — and the next sweep has to finish
/// the job rather than skip it. The state is reconstructed rather than raced
/// for, for the same reason as the staging test.
#[tokio::test]
async fn a_kill_inside_the_cold_rewrite_reads_correctly_and_is_finished_next_sweep() {
    let (app, root) = boot_sealing("crash-compact");
    let doc = r#"{"signal":"logs","from":0,"to":8000,"limit":100}"#;
    let id = mira_core::block::node_id("mira");

    otlp(&app, "/v1/logs", logs_export("checkout", 1_000, 8)).await;
    let dir = mira_core::block::scan(&root, "logs").unwrap()[0]
        .dir
        .clone();
    let plain = rows_of(&query(&app, doc).await);
    assert_eq!(plain.matches("handled request").count(), 8);

    // Keep the uncompressed tables, compact, then put one back and remove the
    // marker: tables half rewritten, marker absent, which is exactly where a
    // kill inside the rewrite leaves the directory.
    let keep = root.join("uncompressed");
    std::fs::create_dir_all(&keep).unwrap();
    for e in std::fs::read_dir(&dir).unwrap() {
        let p = e.unwrap().path();
        if p.extension().is_some_and(|x| x == "arrow") {
            std::fs::copy(&p, keep.join(p.file_name().unwrap())).unwrap();
        }
    }
    assert_eq!(
        mira_core::block::compact(&root, "logs", id, i64::MAX).unwrap(),
        1
    );
    std::fs::copy(keep.join("logs.arrow"), dir.join("logs.arrow")).unwrap();
    std::fs::remove_file(dir.join("cold")).unwrap();

    assert_eq!(
        rows_of(&query(&app, doc).await),
        plain,
        "a half-compacted block did not read back"
    );
    // The absent marker is what makes the next sweep retry rather than declare
    // the block done and leave it half-compressed for ever.
    assert_eq!(
        mira_core::block::compact(&root, "logs", id, i64::MAX).unwrap(),
        1,
        "the unfinished rewrite was not retried"
    );
    assert!(dir.join("cold").exists());
    assert_eq!(rows_of(&query(&app, doc).await), plain);
    forget_open_blocks();
}

/// The engine, the API, the CLI's directory reader and the TUI's HTTP client,
/// all answering the same question.
///
/// Four boundaries, one answer. `mira_core::query::search` is the engine with
/// nothing on top; `/api/v1/query` adds the envelope, the router and the timing
/// layer; `Source::Local` is the CLI reading a detached volume with no server
/// anywhere; `Source::Remote` is the TUI over a real socket, parsing the reply
/// with the KYAML loader rather than a JSON parser. Any of the four disagreeing
/// means one class of user is looking at different data from another, and
/// nothing else in this suite covers the last two at all.
///
/// The 64-bit contract (section 7.6) is asserted at each of them, both ways
/// round: a nanosecond timestamp is a *string* and a `severity_number` is a bare
/// number. That asymmetry is deliberate and it is easy to break in the direction
/// that fails nothing — a nanosecond timestamp emitted as a JSON number loses
/// its last three digits in every client that parses it into an IEEE double,
/// silently.
#[tokio::test]
async fn the_engine_the_api_and_both_ui_transports_answer_row_for_row() {
    // Acknowledged means published here, so the directory reader — which has no
    // open block and cannot have one, because nothing is writing to a detached
    // volume — is comparable at all.
    let (app, root) = boot_sealing("parity");
    let addr = serve(app.clone()).await;
    let doc = r#"{"signal":"logs","from":0,"to":8000,"limit":100}"#;

    otlp(&app, "/v1/logs", logs_export("checkout", 1_000, 6)).await;
    otlp(&app, "/v1/logs", logs_export("payments", 2_000, 6)).await;

    // The engine, with nothing above it.
    let q = crate::api::parse_search(doc, crate::api::now_nanos()).expect("parse");
    let direct = mira_core::query::search(&root, &q).expect("search");
    let engine = rows_of(&crate::api::envelope("rows", &direct, Duration::ZERO));
    assert_eq!(engine.matches("handled request").count(), 12, "{engine}");

    let over_http = query(&app, doc).await;
    assert_eq!(rows_of(&over_http), engine, "the API moved a row");

    // Both TUI transports. `post` blocks on a `TcpStream`, so it goes to the
    // blocking pool: driving it from a runtime thread would deadlock it against
    // the server it is talking to.
    let (local, remote) = {
        let (d, b, a) = (root.clone(), doc.to_owned(), addr.clone());
        tokio::task::spawn_blocking(move || {
            let route = "/api/v1/query";
            (
                crate::tui::Source::Local(d).post(route, &b).expect("local"),
                crate::tui::Source::Remote(a)
                    .post(route, &b)
                    .expect("remote"),
            )
        })
        .await
        .unwrap()
    };

    for (what, parsed) in [("local", &local), ("remote", &remote)] {
        let rows = parsed["rows"].as_vec().expect(what);
        assert_eq!(rows.len(), 12, "{what}");
        // Same set, same order — checked against the engine's own rendering
        // rather than against the other transport, because two wrong answers
        // that agree would pass a comparison between themselves.
        for (i, row) in rows.iter().enumerate() {
            let body = row["body"].as_str().expect("body");
            assert!(engine.contains(body), "{what} row {i} is not the engine's");
        }
        let first = &rows[0];
        assert_eq!(
            first["body"].as_str().unwrap(),
            "payments handled request 5",
            "{what} did not order newest-first"
        );

        // Section 7.6 at this boundary, and the documented consequence is right
        // here: `as_i64` answers `None` to a quoted number, so a reader that
        // reached for the obvious accessor and fell back to zero would stamp the
        // epoch on every row and never fail.
        assert_eq!(first["time_unix_nano"].as_i64(), None, "{what}");
        assert_eq!(
            first["time_unix_nano"]
                .as_str()
                .and_then(|s| s.parse().ok()),
            Some(2_005i64),
            "{what}"
        );
        // The other direction: 32-bit and narrower stay bare numbers, so a
        // reader that assumed *everything* was quoted would be just as wrong.
        assert_eq!(first["severity_number"].as_i64(), Some(9), "{what}");
        assert_eq!(parsed["stats"]["rows_matched"].as_i64(), Some(12), "{what}");
    }

    // The same contract in the bytes, before anything has parsed them: the
    // assertions above go through the KYAML loader, which would be just as happy
    // with a document that quoted every number in it.
    assert!(
        over_http.contains(r#""time_unix_nano":"2005""#),
        "{over_http}"
    );
    assert!(over_http.contains(r#""severity_number":9,"#), "{over_http}");
    assert!(
        !over_http.contains(r#""time_unix_nano":2005"#),
        "a nanosecond timestamp went out as a JSON number: {over_http}"
    );
    forget_open_blocks();
}

/// The same parity, with a writer running.
///
/// Exact equality is the wrong assertion here, and asserting it would only teach
/// the suite to sleep: two reads a millisecond apart legitimately see different
/// amounts of data. What must hold instead is monotonicity, and it is a stronger
/// statement than it sounds. Rows come back newest-first over a fixed window
/// with nothing being deleted, so *every earlier answer is a suffix of every
/// later one* — which fails if a row disappears, if a row is rewritten, if two
/// rows swap, or if a seal briefly shows the open block and the published block
/// at once. Section 4's `(node, seq)` dedupe and the snapshot held across the
/// publish are what make it true, and nothing else here would notice either of
/// them breaking.
///
/// The chain starts before the writer does, so it is anchored at zero rows and
/// ends at all of them: growth is observed by construction rather than hoped
/// for on a loaded runner.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_reader_under_a_live_writer_never_sees_a_row_vanish_or_arrive_out_of_order() {
    let (app, _root) = boot("parity-live");
    let addr = serve(app.clone()).await;
    let doc = r#"{"signal":"logs","from":0,"to":900000,"limit":10000}"#;
    let (batches, per) = (12usize, 5usize);

    // One probe of each transport, as a body sequence. Two sources in one chain
    // is deliberate: the TUI and the API read the same node, so an answer from
    // either has to be consistent with every answer already given by both.
    let probe = |addr: String| {
        let app = app.clone();
        let doc = doc.to_owned();
        async move {
            let http = bodies_of(&rows_of(&query(&app, &doc).await));
            let tui = tokio::task::spawn_blocking(move || {
                crate::tui::Source::Remote(addr).post("/api/v1/query", &doc)
            })
            .await
            .unwrap()
            .expect("remote");
            // Rendered back out of the parse, so a row the loader mangled fails
            // the sequence check rather than sliding through as raw text.
            let tui = tui["rows"]
                .as_vec()
                .expect("rows")
                .iter()
                .map(|r| r["body"].as_str().expect("body").to_owned())
                .collect::<Vec<_>>();
            [http, tui]
        }
    };

    let mut seen: Vec<String> = Vec::new();
    let check = |answers: [Vec<String>; 2], seen: &mut Vec<String>| {
        for bodies in answers {
            assert!(
                bodies.ends_with(seen),
                "a row disappeared or moved under a live writer.\n  had: {seen:?}\n  got: {bodies:?}"
            );
            *seen = bodies;
        }
    };
    check(probe(addr.clone()).await, &mut seen);
    assert!(seen.is_empty(), "the writer has not started yet");

    let writer = {
        let app = app.clone();
        tokio::spawn(async move {
            for b in 0..batches {
                // A distinct service per batch and a strictly increasing base,
                // so newest-first is a total order over distinct bodies and the
                // suffix property is exact rather than positional.
                let batch = logs_export(&format!("svc{b}"), 1_000 + b as u64 * 100, per);
                otlp(&app, "/v1/logs", batch).await;
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
    };

    let mut rounds = 0;
    loop {
        let done = writer.is_finished();
        check(probe(addr.clone()).await, &mut seen);
        rounds += 1;
        if done {
            break;
        }
        tokio::time::sleep(Duration::from_millis(3)).await;
    }
    writer.await.unwrap();
    assert!(rounds >= 2, "the reader never overlapped the writer");

    let all = query(&app, doc).await;
    assert_eq!(
        all.matches("handled request").count(),
        batches * per,
        "the writer's last batch is missing: {all}"
    );
    // Every row whole. A body without its timestamp — or a timestamp without its
    // row — is what a reader would see if a snapshot were taken mid-append, and
    // counting rows would not notice.
    let rows = rows_of(&all);
    assert_eq!(
        rows.matches(r#""time_unix_nano":""#).count(),
        bodies_of(&rows).len()
    );
    forget_open_blocks();
}

/// A count exactly at the threshold, and one over it.
///
/// `>= n` at exactly `n` has to fire and `> n` at exactly `n` has to not. Both
/// are asserted on the same data in the same evaluation, because an off-by-one
/// here is either a rule that pages a minute early for ever or one that never
/// pages at all, and the two look identical from inside the engine.
#[tokio::test]
async fn a_count_at_the_threshold_fires_gte_and_not_gt() {
    let now = crate::api::now_nanos() as u64;
    let (app, api, _root) = boot_alerting(
        "alert-threshold",
        r#"{ "rules": [
             { "name": "at", "over": "1m", "when": "count >= 5",
               "query": { "signal": "logs", "where": [
                  { "attr": "service.name", "eq": "checkout" },
                  { "field": "severity_number", "gte": 17 } ] } },
             { "name": "over", "over": "1m", "when": "count > 5",
               "query": { "signal": "logs", "where": [
                  { "attr": "service.name", "eq": "checkout" },
                  { "field": "severity_number", "gte": 17 } ] } } ] }"#,
    );

    // Ten records, every other one an ERROR: exactly five.
    otlp(&app, "/v1/logs", logs_export("checkout", now, 10)).await;
    api.alerts.tick(&api).await;
    let at_five = String::from_utf8(get(&app, "/api/v1/alerts", None).await.1).unwrap();
    assert!(at_five.contains(r#""matched":5"#), "{at_five}");
    assert!(
        at_five.contains(r#""name":"at","state":"firing""#),
        "a count exactly at a `>=` threshold did not fire: {at_five}"
    );
    assert!(
        at_five.contains(r#""name":"over","state":"ok""#),
        "a count exactly at a `>` threshold fired: {at_five}"
    );

    // One more error, and only the strict rule changes its mind.
    otlp(&app, "/v1/logs", logs_export("checkout", now + 100, 2)).await;
    api.alerts.tick(&api).await;
    let at_six = String::from_utf8(get(&app, "/api/v1/alerts", None).await.1).unwrap();
    assert!(at_six.contains(r#""matched":6"#), "{at_six}");
    assert!(
        at_six.contains(r#""name":"over","state":"firing""#),
        "{at_six}"
    );
    assert!(
        at_six.contains(r#""name":"at","state":"firing""#),
        "{at_six}"
    );
    forget_open_blocks();
}

/// Nothing to count is not a breach.
///
/// Three ways a window ends up empty and none of them may page: no data at all,
/// data the filter excludes, and data that is real but older than `over`. The
/// third is the one that survives review, because the rule looks right and the
/// node is busy — a window bound out by a factor of a thousand would count an
/// hour of history into a one-minute rule and page on traffic that stopped
/// yesterday.
#[tokio::test]
async fn an_empty_window_never_breaches_in_either_metric() {
    let now = crate::api::now_nanos() as u64;
    let (app, api, _root) = boot_alerting(
        "alert-empty",
        r#"{ "rules": [
             { "name": "ratio", "over": "1m", "when": "ratio > 0%",
               "query": { "signal": "logs", "where": [
                  { "field": "severity_number", "gte": 17 } ] },
               "of":    { "signal": "logs" } },
             { "name": "count", "over": "1m", "when": "count > 0",
               "query": { "signal": "logs" } },
             { "name": "no-match", "over": "1m", "when": "count > 0",
               "query": { "signal": "logs", "where": [
                  { "attr": "service.name", "eq": "not-deployed" } ] } } ] }"#,
    );

    // Nothing stored. 0/0 is 0.0 and not 100%, which is the false page the
    // arithmetic invites and the one an operator would never trust again.
    api.alerts.tick(&api).await;
    let nothing = api.alerts.json();
    assert_eq!(nothing.matches(r#""state":"ok""#).count(), 3, "{nothing}");
    assert_eq!(nothing.matches(r#""value":0,"#).count(), 3, "{nothing}");
    assert_eq!(nothing.matches(r#""error":null"#).count(), 3, "{nothing}");

    // Real data, two hours old: outside every rule's window, so still nothing.
    otlp(
        &app,
        "/v1/logs",
        logs_export("checkout", now - 2 * 3_600_000_000_000, 20),
    )
    .await;
    api.alerts.tick(&api).await;
    let stale = api.alerts.json();
    assert_eq!(
        stale.matches(r#""state":"ok""#).count(),
        3,
        "a rule counted data older than its own window: {stale}"
    );
    assert_eq!(stale.matches(r#""matched":0"#).count(), 3, "{stale}");

    // In the window now — and the rule whose filter matches nothing still has to
    // stay quiet, and stay *reported*, with a null error: "no rows" and "the
    // query failed" are the two things an alerting screen must never render the
    // same way.
    otlp(&app, "/v1/logs", logs_export("checkout", now, 4)).await;
    api.alerts.tick(&api).await;
    let live = api.alerts.json();
    assert!(
        live.contains(r#""name":"count","state":"firing""#),
        "{live}"
    );
    assert!(live.contains(r#""name":"no-match","state":"ok""#), "{live}");
    assert_eq!(live.matches(r#""error":null"#).count(), 3, "{live}");
    forget_open_blocks();
}

/// A window that straddles a seal counts both halves of itself, once each.
///
/// Half the records are in a published block and half are still in the builder,
/// which is the state a node is in for most of every `max_block_age`. A rule
/// that reached only the disk would under-count for two seconds out of every two
/// — a threshold that never trips on a spike shorter than a block — and one that
/// double-counted the overlap would page on traffic that does not exist.
///
/// The split is forced, not timed: the first half is published by a shutdown and
/// the second is ingested by a node whose age clock is ten minutes away, so the
/// window straddles the boundary for the whole of the test rather than for a
/// hundred milliseconds of it.
#[tokio::test]
async fn an_alert_window_that_straddles_a_block_boundary_counts_each_record_once() {
    let now = crate::api::now_nanos() as u64;
    let root = fresh_dir("alert-straddle");
    // The engine outside the router: `tick` reads through the `Api` it is handed
    // and keeps its own state, so a rule set does not have to be baked into a
    // node at boot to be evaluated against one.
    let engine = crate::alert::Engine::new(
        crate::alert::Rules::parse(
            r#"{ "rules": [
                 { "name": "errors", "over": "5m", "when": "count >= 1",
                   "query": { "signal": "logs", "where": [
                      { "attr": "service.name", "eq": "checkout" },
                      { "field": "severity_number", "gte": 17 } ] } } ] }"#,
        )
        .expect("rules"),
    );

    // Six records, three of them errors, on disk.
    let n = restart(&root, true).await;
    otlp(&n.app, "/v1/logs", logs_export("checkout", now, 6)).await;
    n.stop().await;
    assert_eq!(mira_core::block::scan(&root, "logs").unwrap().len(), 1);

    // Four more, two of them errors, and they stay open.
    let n = restart(&root, true).await;
    otlp(&n.app, "/v1/logs", logs_export("checkout", now + 1_000, 4)).await;
    engine.tick(&n.api).await;
    let body = engine.json();
    assert_eq!(
        mira_core::block::scan(&root, "logs").unwrap().len(),
        1,
        "the second half sealed, so nothing straddled and this proved nothing"
    );
    assert!(
        body.contains(r#""matched":5"#),
        "3 on disk plus 2 open is 5; a rule that saw one side saw 3 or 2: {body}"
    );
    n.stop().await;
}

/// The same window twice is the same verdict, and the state machine runs both
/// ways round.
///
/// Determinism first: two evaluations over unchanged data must produce an
/// identical document apart from the stamp saying when they ran. A rule that
/// drifted — a window computed off a moving `now` against block boundaries, a
/// count that depended on scan order — would flap, and a flapping rule is
/// indistinguishable from a real incident at three in the morning.
///
/// Then firing to resolved and back, on a ratio whose numerator and denominator
/// are moved independently. The resolve has to clear `firing_since` and the
/// second fire has to set a new one: a resolve that left it standing makes "how
/// long has this been broken" read as the first outage, for ever.
#[tokio::test]
async fn two_evaluations_of_one_window_agree_and_the_transitions_run_both_ways() {
    let now = crate::api::now_nanos() as u64;
    let (app, api, _root) = boot_alerting(
        "alert-determinism",
        r#"{ "rules": [
             { "name": "checkout-errors", "over": "5m", "when": "ratio > 40%",
               "severity": "critical",
               "query": { "signal": "logs", "where": [
                  { "attr": "service.name", "eq": "checkout" },
                  { "field": "severity_number", "gte": 17 } ] },
               "of":    { "signal": "logs" } } ] }"#,
    );
    // Everything but the evaluation stamp, which is a clock reading and the one
    // field that is *supposed* to move between two identical evaluations.
    let stable = |body: &str| {
        let i = body.find(r#""evaluated_at":""#).expect("stamp");
        let rest = &body[i + 16..];
        format!("{}{}", &body[..i], &rest[rest.find('"').unwrap()..])
    };
    let firing_since = |body: &str| {
        let i = body.find(r#""firing_since":""#).expect("firing_since") + 16;
        body[i..i + body[i..].find('"').unwrap()]
            .parse::<i64>()
            .expect("a nanosecond stamp")
    };

    otlp(&app, "/v1/logs", logs_export("checkout", now, 10)).await;
    api.alerts.tick(&api).await;
    let first = api.alerts.json();
    assert!(first.contains(r#""state":"firing""#), "{first}");
    assert!(first.contains(r#""value":0.5"#), "{first}");

    api.alerts.tick(&api).await;
    let second = api.alerts.json();
    assert_eq!(
        stable(&first),
        stable(&second),
        "the same window evaluated twice disagreed with itself"
    );
    assert_ne!(first, second, "the evaluation stamp did not move");

    // Section 7.6 on this document too: every nanosecond field is a string, and
    // `matched` and `total` — row counts, not clocks — are not.
    assert!(second.contains(r#""over_nano":"300000000000""#), "{second}");
    assert!(second.contains(r#""for_nano":"0""#), "{second}");
    assert!(second.contains(r#""matched":5,"total":10"#), "{second}");
    let fired_at = firing_since(&second);
    assert!(fired_at > 0, "{second}");

    // Twenty records from another service move the denominator and not the
    // numerator: 5 of 30 is 16.7%, under the threshold.
    otlp(&app, "/v1/logs", logs_export("payments", now, 20)).await;
    api.alerts.tick(&api).await;
    let resolved = api.alerts.json();
    assert!(resolved.contains(r#""state":"ok""#), "{resolved}");
    assert!(
        resolved.contains(r#""since":null,"firing_since":null"#),
        "the resolve left the firing clock running: {resolved}"
    );

    // And back. Forty more checkout records, all errors: 45 of 70 is 64%.
    let mut spike = logs_export("checkout", now + 1_000, 40);
    for r in &mut spike.resource_logs[0].scope_logs[0].log_records {
        r.severity_number = 17;
    }
    otlp(&app, "/v1/logs", spike).await;
    api.alerts.tick(&api).await;
    let again = api.alerts.json();
    assert!(again.contains(r#""state":"firing""#), "{again}");
    assert!(again.contains(r#""matched":45,"total":70"#), "{again}");
    assert!(
        firing_since(&again) > fired_at,
        "the second incident reported the first one's start time"
    );
    forget_open_blocks();
}
