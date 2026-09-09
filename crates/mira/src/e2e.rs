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

/// A fresh data directory with the three flushers running against it, and the
/// block age turned down so the test waits on flushes measured in milliseconds
/// rather than the 2s that is right in production.
fn wire(name: &str) -> (receiver::Receivers, api::Api, std::path::PathBuf) {
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
        // The shipped default, not a test-only number: the limits these tests
        // assert against are the ones an operator gets out of the box.
        max_request_bytes: crate::config::Config::default().max_request_bytes,
    };
    let api = api::Api {
        data_dir: Arc::new(root.clone()),
    };
    (recv, api, root)
}

/// The 4318 listener: OTLP/HTTP, the query API, MCP and the UI on one router.
fn boot(name: &str) -> (Router, std::path::PathBuf) {
    let (recv, api, root) = wire(name);
    let app = receiver::http_router(recv)
        .merge(api::router(api.clone()))
        .merge(crate::mcp::router(api))
        .merge(crate::ui::router());
    (app, root)
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
    assert!(capped.contains("[5000,5000],[5001,5001]"), "{capped}");
    assert!(capped.contains("[5000,5100],[5001,5101]"), "{capped}");
    assert!(
        !capped.contains("[1000,"),
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
    assert!(hist.contains("[9000,42]"), "{hist}");
    assert!(hist.contains("[9000,1234.5]"), "{hist}");
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
    // The exemplar's own timestamp, not the point's.
    assert!(hist.contains(r#""time_unix_nano":8900"#), "{hist}");
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
    // integers that way; it has to have been stored as the integer 200.
    assert!(rows.contains(r#""http.status_code":200"#), "{rows}");
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
