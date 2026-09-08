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
        logs: pipeline::spawn::<mira_core::logs::LogsBuilder>(cfg.clone()),
        traces: pipeline::spawn::<mira_core::traces::TracesBuilder>(cfg.clone()),
        metrics: pipeline::spawn::<mira_core::metrics::MetricsBuilder>(cfg.clone()),
    };
    let app = receiver::http_router(recv).merge(api::router(api::Api {
        data_dir: Arc::new(root.clone()),
    }));
    (app, root)
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
