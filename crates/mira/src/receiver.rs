//! OTLP receivers.
//!
//! Two listeners, because OTLP is two protocols. 4317 is OTLP/gRPC and is served
//! by tonic. 4318 is OTLP/HTTP — a plain HTTP/1.1 POST of protobuf to
//! `/v1/{traces,metrics,logs}` — which tonic cannot serve; that one is axum,
//! already in the tree via tonic's `router` feature, so it costs no dependency.

use axum::Router;
use axum::body::Bytes;
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use prost::Message;
use tonic::codec::CompressionEncoding;
use tonic::{Request, Status};
use tonic_types::{ErrorDetails, StatusExt};

use mira_proto::collector::logs::v1::logs_service_server::{LogsService, LogsServiceServer};
use mira_proto::collector::logs::v1::{ExportLogsServiceRequest, ExportLogsServiceResponse};
use mira_proto::collector::metrics::v1::metrics_service_server::{
    MetricsService, MetricsServiceServer,
};
use mira_proto::collector::metrics::v1::{
    ExportMetricsServiceRequest, ExportMetricsServiceResponse,
};
use mira_proto::collector::trace::v1::trace_service_server::{TraceService, TraceServiceServer};
use mira_proto::collector::trace::v1::{ExportTraceServiceRequest, ExportTraceServiceResponse};

use crate::pipeline::{Ingest, Rejected};

/// One write handle per signal. The type parameter is what keeps a metrics
/// request from being handed to the logs flusher; `Ingest` is generic precisely
/// so that the mistake is a compile error rather than a corrupt block.
#[derive(Clone)]
pub struct Receivers {
    pub logs: Ingest<ExportLogsServiceRequest>,
    pub traces: Ingest<ExportTraceServiceRequest>,
    pub metrics: Ingest<ExportMetricsServiceRequest>,
    /// The largest export either listener will decode, in bytes.
    ///
    /// One number for both, because a batch that a collector sends happily over
    /// 4317 and that fails over 4318 is the worst kind of bug to be handed: it
    /// depends on a transport nobody changed. Applied three times — as the
    /// axum body limit, as tonic's `max_decoding_message_size`, and as the
    /// ceiling on what a gzip body may inflate to.
    pub max_request_bytes: usize,
}

/// Map a rejection onto a gRPC status.
///
/// Overload is never a partial success: the OTLP spec says the client MUST NOT
/// retry a partial success, so using it for backpressure destroys the data and
/// records the failure as the sender's fault. It is always a status code, and
/// always with `RetryInfo` attached — `grpc-retry-pushback-ms` alone is only
/// honoured by clients that configured a gRPC retry policy, which OTLP
/// exporters do not.
///
/// The retryable set is closed and `INTERNAL` is not in it, so it is reserved
/// for the one refusal that really is permanent. A write that failed on a full
/// disk answered `INTERNAL` would have the exporter drop the batch it is holding
/// rather than send it again a second later, which is data loss chosen by a
/// status code.
fn status_for(r: Rejected) -> Status {
    // 250ms for the queue, which drains in one block age; a second for a failed
    // write, which is usually a disk needing longer than that anyway.
    let retry = |ms| ErrorDetails::with_retry_info(Some(std::time::Duration::from_millis(ms)));
    match r {
        Rejected::Busy => {
            Status::with_error_details(tonic::Code::Unavailable, "ingest queue full", retry(250))
        }
        Rejected::Closed => Status::unavailable("shutting down"),
        Rejected::Unavailable(e) => {
            Status::with_error_details(tonic::Code::Unavailable, e, retry(1_000))
        }
        Rejected::Failed(e) => Status::internal(e),
    }
}

/// `accept_compressed` on every service, because the spec says MUST and the
/// stock exporter says default.
///
/// Without it tonic answers a compressed request with `UNIMPLEMENTED`, and
/// OTLP calls that a permanent failure: the exporter drops the batch instead of
/// retrying it. `send_compressed` is deliberately absent — the response is an
/// empty message, and gzipping nothing costs a round of deflate per export.
///
/// `max_decoding_message_size` is set from the same field the HTTP listener
/// uses; tonic's own default is 4 MiB and axum's is 2 MiB, and leaving the two
/// listeners disagreeing means a batch size that works on one port and fails
/// on the other.
impl Receivers {
    pub fn logs_server(&self) -> LogsServiceServer<Self> {
        LogsServiceServer::new(self.clone())
            .accept_compressed(CompressionEncoding::Gzip)
            .max_decoding_message_size(self.max_request_bytes)
    }
    pub fn traces_server(&self) -> TraceServiceServer<Self> {
        TraceServiceServer::new(self.clone())
            .accept_compressed(CompressionEncoding::Gzip)
            .max_decoding_message_size(self.max_request_bytes)
    }
    pub fn metrics_server(&self) -> MetricsServiceServer<Self> {
        MetricsServiceServer::new(self.clone())
            .accept_compressed(CompressionEncoding::Gzip)
            .max_decoding_message_size(self.max_request_bytes)
    }
}

#[tonic::async_trait]
impl LogsService for Receivers {
    async fn export(
        &self,
        request: Request<ExportLogsServiceRequest>,
    ) -> Result<tonic::Response<ExportLogsServiceResponse>, Status> {
        self.logs
            .submit(request.into_inner())
            .await
            .map_err(status_for)?;
        // No partial_success: everything we accepted is durable by now, and
        // anything we could not accept was reported as a status above.
        Ok(tonic::Response::new(ExportLogsServiceResponse::default()))
    }
}

#[tonic::async_trait]
impl TraceService for Receivers {
    async fn export(
        &self,
        request: Request<ExportTraceServiceRequest>,
    ) -> Result<tonic::Response<ExportTraceServiceResponse>, Status> {
        self.traces
            .submit(request.into_inner())
            .await
            .map_err(status_for)?;
        Ok(tonic::Response::new(ExportTraceServiceResponse::default()))
    }
}

#[tonic::async_trait]
impl MetricsService for Receivers {
    async fn export(
        &self,
        request: Request<ExportMetricsServiceRequest>,
    ) -> Result<tonic::Response<ExportMetricsServiceResponse>, Status> {
        self.metrics
            .submit(request.into_inner())
            .await
            .map_err(status_for)?;
        Ok(tonic::Response::new(ExportMetricsServiceResponse::default()))
    }
}

/// OTLP/HTTP on 4318.
pub fn http_router(r: Receivers) -> Router {
    // Each route is the same three lines with a different field and response
    // type; a macro says that once instead of three times.
    macro_rules! signal {
        ($field:ident, $resp:ty, $json:path) => {
            post(
                |axum::extract::State(r): axum::extract::State<Receivers>,
                 h: HeaderMap,
                 b: Bytes| async move {
                    let max = r.max_request_bytes;
                    export(&r.$field, &h, b, max, <$resp>::default(), $json).await
                },
            )
        };
    }
    let max = r.max_request_bytes;
    Router::new()
        // OTLP/HTTP log export. Protobuf or JSON, gzip either way.
        .route(
            "/v1/logs",
            signal!(logs, ExportLogsServiceResponse, crate::json::logs),
        )
        // OTLP/HTTP trace export. Protobuf or JSON, gzip either way.
        .route(
            "/v1/traces",
            signal!(traces, ExportTraceServiceResponse, crate::json::traces),
        )
        // OTLP/HTTP metric export. Protobuf or JSON, gzip either way.
        .route(
            "/v1/metrics",
            signal!(metrics, ExportMetricsServiceResponse, crate::json::metrics),
        )
        // Explicit, because axum's default is 2 MiB and that is below what a
        // stock collector's batch processor produces at its own default of
        // 8192 records. The exporter treats 413 as permanent and drops the
        // batch, so a limit set too low is data loss rather than a slow start.
        .layer(axum::extract::DefaultBodyLimit::max(max))
        .with_state(r)
}

/// Which body encoding the client sent.
///
/// OTLP/HTTP names exactly two, and a request that claims a third has to be
/// refused rather than guessed at: `415` tells the exporter to stop, where
/// mis-decoding it as protobuf produces a `400` that reads like corrupt data.
/// A missing `content-type` is treated as protobuf, which is what every
/// pre-JSON client that omitted it meant.
enum Encoding {
    Protobuf,
    Json,
}

fn encoding(h: &HeaderMap) -> Option<Encoding> {
    let Some(ct) = h.get(header::CONTENT_TYPE) else {
        return Some(Encoding::Protobuf);
    };
    // Compare the media type only; charset and boundary parameters are the
    // client's business.
    let ct = ct.to_str().unwrap_or_default();
    match ct.split(';').next().unwrap_or_default().trim() {
        "application/x-protobuf" | "application/protobuf" | "" => Some(Encoding::Protobuf),
        "application/json" => Some(Encoding::Json),
        _ => None,
    }
}

/// Undo `Content-Encoding` before anything tries to parse the body.
///
/// Returning the body untouched for a missing or `identity` header is the
/// common path. An encoding we do not implement is `415`, not a decode failure:
/// the exporter has to be told to stop offering it, and a protobuf parser fed
/// deflate reports "invalid wire type" — a message that sends whoever reads it
/// looking for corruption instead of a header.
///
/// `max` caps what comes *out*, which the body limit does not: a few kilobytes
/// of gzipped zeros expand to gigabytes and `read_to_end` will allocate every
/// one of them. It is the same number as the body limit rather than a multiple
/// of it, because that makes one configured value mean one thing — the largest
/// export Mira will decode — however it arrived. A ratio would be the obvious
/// alternative and does not work: a real batch, the same attribute keys over
/// and over, reaches about 35:1, and there is no ratio above that which a bomb
/// cannot also sit under.
fn inflate(h: &HeaderMap, body: Bytes, max: usize) -> Result<Bytes, (StatusCode, String)> {
    let ce = h
        .get(header::CONTENT_ENCODING)
        .map(|v| v.to_str().unwrap_or_default().trim().to_ascii_lowercase());
    match ce.as_deref() {
        None | Some("") | Some("identity") => Ok(body),
        Some("gzip") | Some("x-gzip") => {
            use std::io::Read;
            let mut out = Vec::new();
            // `take` is the whole defence: one byte past the cap and the read
            // stops, so the refusal costs the cap and not the bomb.
            flate2::read::GzDecoder::new(&body[..])
                .take(max as u64 + 1)
                .read_to_end(&mut out)
                .map_err(|e| {
                    (
                        StatusCode::BAD_REQUEST,
                        format!("failed to decompress gzip body: {e}"),
                    )
                })?;
            if out.len() > max {
                return Err((
                    StatusCode::PAYLOAD_TOO_LARGE,
                    format!(
                        "gzip body inflates past the {max} byte limit; \
                         raise ingest.max_request_bytes or lower the sender's batch size"
                    ),
                ));
            }
            Ok(out.into())
        }
        Some(other) => Err((
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            format!("unsupported content-encoding {other}; expected gzip or identity"),
        )),
    }
}

/// Decode, submit, and answer. Generic over the signal because the three
/// endpoints differ only in which types they name.
///
/// The response is echoed back in the request's own encoding, which the spec
/// requires: a JSON client gets `{}`, not a protobuf empty message that its
/// parser will choke on.
async fn export<R: Message + Default, T: Message>(
    ingest: &Ingest<R>,
    headers: &HeaderMap,
    body: Bytes,
    max: usize,
    ok: T,
    from_json: fn(&yaml_rust2::Yaml) -> Result<R, String>,
) -> Response {
    let json = match encoding(headers) {
        Some(Encoding::Protobuf) => false,
        Some(Encoding::Json) => true,
        None => {
            return (
                StatusCode::UNSUPPORTED_MEDIA_TYPE,
                "expected application/x-protobuf or application/json",
            )
                .into_response();
        }
    };

    let body = match inflate(headers, body, max) {
        Ok(b) => b,
        Err((code, e)) => return fail(json, code, &e),
    };

    let decoded = if json {
        std::str::from_utf8(&body)
            .map_err(|e| e.to_string())
            .and_then(crate::api::parse)
            .and_then(|doc| from_json(&doc))
    } else {
        R::decode(body).map_err(|e| e.to_string())
    };
    let req = match decoded {
        Ok(r) => r,
        Err(e) => return fail(json, StatusCode::BAD_REQUEST, &e),
    };

    match ingest.submit(req).await {
        Ok(()) if json => (
            StatusCode::OK,
            [(header::CONTENT_TYPE, "application/json")],
            "{}",
        )
            .into_response(),
        Ok(()) => (
            StatusCode::OK,
            [(header::CONTENT_TYPE, "application/x-protobuf")],
            ok.encode_to_vec(),
        )
            .into_response(),
        Err(Rejected::Busy) => (
            [(header::RETRY_AFTER, "1")],
            fail(json, StatusCode::SERVICE_UNAVAILABLE, "ingest queue full"),
        )
            .into_response(),
        Err(Rejected::Closed) => fail(json, StatusCode::SERVICE_UNAVAILABLE, "shutting down"),
        // 503 and not 500 for the same reason as `UNAVAILABLE` above: 500 is
        // outside OTLP/HTTP's retryable set, so it drops the batch.
        Err(Rejected::Unavailable(e)) => (
            [(header::RETRY_AFTER, "1")],
            fail(json, StatusCode::SERVICE_UNAVAILABLE, &e),
        )
            .into_response(),
        Err(Rejected::Failed(e)) => fail(json, StatusCode::INTERNAL_SERVER_ERROR, &e),
    }
}

/// An error in the encoding the client asked for.
///
/// OTLP wants a `google.rpc.Status`; for the JSON case that is a two-field
/// object, and hand-writing it costs less than a serializer. The protobuf case
/// keeps returning text — encoding a `Status` there means another generated type
/// for a path no exporter parses.
fn fail(json: bool, code: StatusCode, message: &str) -> Response {
    if !json {
        return (code, message.to_owned()).into_response();
    }
    // The only characters a message here can contain that JSON forbids.
    let escaped = message.replace('\\', "\\\\").replace('"', "\\\"");
    (
        code,
        [(header::CONTENT_TYPE, "application/json")],
        // code 2 is UNKNOWN in google.rpc.Code; the HTTP status carries the
        // detail an exporter actually branches on.
        format!(r#"{{"code":2,"message":"{escaped}"}}"#),
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    use axum::body::Body;
    use axum::http::{HeaderValue, Request};
    use mira_core::wal;
    use tokio::sync::mpsc;
    use tower::ServiceExt;

    /// Which encoding `encoding` picked, as something a table can compare.
    /// `Encoding` deliberately has no `PartialEq` — nothing in production
    /// compares two of them — so the test names them instead of widening the
    /// type's API for its own convenience.
    fn picked(h: &HeaderMap) -> &'static str {
        match encoding(h) {
            Some(Encoding::Protobuf) => "protobuf",
            Some(Encoding::Json) => "json",
            None => "refused",
        }
    }

    /// The `content-type` is what decides whether the body is parsed as
    /// protobuf, parsed as KYAML, or refused with a 415 — and getting it wrong
    /// is never a clean failure: a JSON body handed to `prost` reports "invalid
    /// wire type", which reads like corruption rather than a header. The
    /// missing header in particular has to mean protobuf, because every OTLP
    /// client that predates the JSON encoding omits it, and refusing those is a
    /// silent outage for the oldest exporters in the fleet.
    #[test]
    fn the_content_type_decides_the_parser_and_a_missing_one_means_protobuf() {
        let with = |v: Option<&[u8]>| {
            let mut h = HeaderMap::new();
            if let Some(v) = v {
                h.insert(header::CONTENT_TYPE, HeaderValue::from_bytes(v).unwrap());
            }
            h
        };
        for (sent, want) in [
            (None, "protobuf"),
            (Some(&b"application/x-protobuf"[..]), "protobuf"),
            (Some(&b"application/protobuf"[..]), "protobuf"),
            (Some(&b""[..]), "protobuf"),
            // The parameters are the client's business, not ours.
            (Some(&b"application/json; charset=utf-8"[..]), "json"),
            (Some(&b" application/json "[..]), "json"),
            (Some(&b"text/plain"[..]), "refused"),
            (Some(&b"application/x-thrift"[..]), "refused"),
            // A header that is not UTF-8 is unreadable, not unknown: it is
            // treated as absent rather than crashing the handler on `to_str`.
            (Some(&[0xff][..]), "protobuf"),
        ] {
            assert_eq!(picked(&with(sent)), want, "content-type {sent:?}");
        }
    }

    /// A queue in the state named, with no flusher behind it. Both of these
    /// refusals are decided by `submit` before anything is dequeued, so a real
    /// pipeline is not needed to reach them — and a queue filled by wedging a
    /// live flusher would be a race rather than a state.
    async fn refusing<R>(closed: bool) -> (Ingest<R>, Box<dyn std::any::Any>)
    where
        R: prost::Message + 'static,
    {
        let (tx, rx) = mpsc::channel(1);
        // Capacity one, and the one permit taken: `try_reserve` then reports
        // `Full`, which is what an ingest queue that is not draining looks like.
        let held = tx.clone().reserve_owned().await.ok();
        let ingest = Ingest {
            tx: [tx].into(),
            turn: std::sync::Arc::default(),
            rejects: &crate::pipeline::REJECTS[0],
            wal: None,
            signal: wal::Signal::Logs,
        };
        // Closed is the flusher being gone: drop both ends of the queue and
        // keep nothing alive.
        let keep: Box<dyn std::any::Any> = if closed {
            drop((rx, held));
            Box::new(())
        } else {
            Box::new((rx, held))
        };
        (ingest, keep)
    }

    /// Neither refusal is the sender's fault, and OTLP/HTTP's retryable set is
    /// closed: anything outside 429/502/503/504 has the exporter drop the batch
    /// it is holding. A full queue answered 500 — or 200 — is data loss chosen
    /// by a status code, and the `retry-after` is what stops a shedding node
    /// being hammered by every exporter at once while it drains.
    #[tokio::test]
    async fn a_queue_that_cannot_take_an_export_answers_503_and_keeps_the_batch_alive() {
        for (closed, want_body, want_retry) in [
            (false, "ingest queue full", true),
            (true, "shutting down", false),
        ] {
            let (logs, _keep) = refusing(closed).await;
            let (traces, _keep_t) = refusing(closed).await;
            let (metrics, _keep_m) = refusing(closed).await;
            let app = http_router(Receivers {
                logs,
                traces,
                metrics,
                max_request_bytes: 1 << 20,
            });
            let res = app
                .oneshot(
                    Request::builder()
                        .method("POST")
                        .uri("/v1/logs")
                        .header("content-type", "application/x-protobuf")
                        .body(Body::from(
                            ExportLogsServiceRequest::default().encode_to_vec(),
                        ))
                        .unwrap(),
                )
                .await
                .unwrap();

            assert_eq!(
                res.status(),
                StatusCode::SERVICE_UNAVAILABLE,
                "closed={closed} must stay inside the retryable set"
            );
            assert_eq!(
                res.headers().get(header::RETRY_AFTER).is_some(),
                want_retry,
                "closed={closed} pushback"
            );
            let body = axum::body::to_bytes(res.into_body(), 1 << 16)
                .await
                .unwrap();
            assert_eq!(
                String::from_utf8_lossy(&body),
                want_body,
                "the reason has to name which of the two it was"
            );
        }
    }

    /// The status is the retry policy. OTLP names the retryable codes exactly —
    /// `UNAVAILABLE` is in the set, `INTERNAL` is not — so an exporter handed
    /// `INTERNAL` for a full disk drops the batch instead of resending it thirty
    /// seconds later when the disk has room. `RetryInfo` rides along because the
    /// bare code leaves the backoff to the client.
    #[test]
    fn a_write_that_failed_is_retryable_and_an_impossible_export_is_not() {
        for r in [Rejected::Busy, Rejected::Unavailable("no space".into())] {
            let s = status_for(r);
            assert_eq!(s.code(), tonic::Code::Unavailable);
            assert!(
                s.get_error_details().retry_info().is_some(),
                "an exporter with no retry policy needs the pushback"
            );
        }
        assert_eq!(
            status_for(Rejected::Closed).code(),
            tonic::Code::Unavailable,
            "a draining node must be retried elsewhere, not written off"
        );
        assert_eq!(
            status_for(Rejected::Failed("too wide".into())).code(),
            tonic::Code::Internal,
            "an export no block can hold must not be retried forever"
        );
    }
}
