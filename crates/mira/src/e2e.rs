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
//! It leans hard on ack-after-durability: `submit` returns once the block is
//! fsynced and renamed, so a 200 on `/v1/logs` is a promise that the next query
//! can see the data. If that promise ever breaks, this test fails, which is
//! precisely the point of asserting it here instead of sleeping.

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

fn kv(k: &str, v: &str) -> KeyValue {
    KeyValue {
        key: k.into(),
        value: Some(AnyValue {
            value: Some(any_value::Value::StringValue(v.into())),
        }),
    }
}

/// A router backed by a fresh data directory, with the block age turned down so
/// the test waits on flushes measured in milliseconds rather than the 2s that is
/// right in production.
fn boot(name: &str) -> (Router, std::path::PathBuf) {
    let root = std::env::temp_dir().join(format!("mira-e2e-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();

    let cfg = Arc::new(pipeline::Config {
        data_dir: root.clone(),
        max_block_age: Duration::from_millis(100),
        ..Default::default()
    });
    let recv = receiver::Receivers {
        logs: pipeline::spawn::<mira_core::logs::LogsBuilder>(cfg.clone()).0,
        traces: pipeline::spawn::<mira_core::traces::TracesBuilder>(cfg.clone()).0,
        metrics: pipeline::spawn::<mira_core::metrics::MetricsBuilder>(cfg.clone()).0,
    };
    let api = api::Api {
        data_dir: Arc::new(root.clone()),
    };
    let app = receiver::http_router(recv)
        .merge(api::router(api.clone()))
        .merge(crate::mcp::router(api))
        .merge(crate::ui::router());
    (app, root)
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

async fn otlp(app: &Router, path: &str, msg: impl Message) {
    let (status, body) = post(app, path, "application/x-protobuf", msg.encode_to_vec()).await;
    assert_eq!(status, StatusCode::OK, "{path} rejected the export: {body}");
}

async fn query(app: &Router, doc: &str) -> String {
    let (status, body) = post(app, "/api/v1/query", "application/json", doc.into()).await;
    assert_eq!(status, StatusCode::OK, "query failed: {body}");
    body
}

fn logs_export(service: &str, base_ts: u64, n: usize) -> ExportLogsServiceRequest {
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

/// The whole product in one function: export, then find it.
///
/// Note the absence of any sleep between the export and the query. That is the
/// assertion, not an omission.
#[tokio::test]
async fn otlp_logs_are_queryable_the_moment_the_export_is_acknowledged() {
    let (app, _root) = boot("logs");
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
    // what the query cost.
    assert!(checkout.contains(r#""blocks_total":2"#), "{checkout}");
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

    let (app, _root) = boot("metrics");

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
    assert!(
        body.contains("[1000,1000],[1001,1001],[5000,5000],[5001,5001]"),
        "{body}"
    );
    assert!(
        body.contains("[1000,1100],[1001,1101],[5000,5100],[5001,5101]"),
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
    assert!(hist.contains("[9000,42]"), "{hist}");
    assert!(hist.contains("[9000,1234.5]"), "{hist}");
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

    // A deep link the browser reloads is the app's own route, not a missing
    // file, so it gets the app back rather than a 404.
    let (status, body, _) = get(&app, "/logs", None).await;
    assert_eq!(status, StatusCode::OK);
    assert!(String::from_utf8_lossy(&body).contains("<div id=\"app\">"));
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
