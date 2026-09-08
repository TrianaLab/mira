//! OTLP receivers.
//!
//! Two listeners, because OTLP is two protocols. 4317 is OTLP/gRPC and is served
//! by tonic. 4318 is OTLP/HTTP — a plain HTTP/1.1 POST of protobuf to
//! `/v1/{traces,metrics,logs}` — which tonic cannot serve; that one is axum,
//! already in the tree via tonic's `router` feature, so it costs no dependency.
//!
//! Metrics is deliberately absent rather than stubbed on the gRPC side: tonic
//! answers an unregistered service with `UNIMPLEMENTED`, which is exactly the
//! right thing and exactly what a hand-written stub would have to say.

use axum::Router;
use axum::body::Bytes;
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use prost::Message;
use tonic::{Request, Status};
use tonic_types::{ErrorDetails, StatusExt};

use mira_proto::collector::logs::v1::logs_service_server::{LogsService, LogsServiceServer};
use mira_proto::collector::logs::v1::{ExportLogsServiceRequest, ExportLogsServiceResponse};
use mira_proto::collector::trace::v1::trace_service_server::{TraceService, TraceServiceServer};
use mira_proto::collector::trace::v1::{ExportTraceServiceRequest, ExportTraceServiceResponse};

use crate::pipeline::{Ingest, Rejected};

/// Both write handles. The type parameter is what keeps a metrics request from
/// being handed to the logs flusher; `Ingest` is generic precisely so that the
/// mistake is a compile error rather than a corrupt block.
#[derive(Clone)]
pub struct Receivers {
    pub logs: Ingest<ExportLogsServiceRequest>,
    pub traces: Ingest<ExportTraceServiceRequest>,
}

/// Map a rejection onto a gRPC status.
///
/// Overload is never a partial success: the OTLP spec says the client MUST NOT
/// retry a partial success, so using it for backpressure destroys the data and
/// records the failure as the sender's fault. It is always a status code, and
/// always with `RetryInfo` attached — `grpc-retry-pushback-ms` alone is only
/// honoured by clients that configured a gRPC retry policy, which OTLP
/// exporters do not.
fn status_for(r: Rejected) -> Status {
    match r {
        Rejected::Busy => Status::with_error_details(
            tonic::Code::Unavailable,
            "ingest queue full",
            ErrorDetails::with_retry_info(Some(std::time::Duration::from_millis(250))),
        ),
        Rejected::Closed => Status::unavailable("shutting down"),
        Rejected::Failed(e) => Status::internal(e),
    }
}

impl Receivers {
    pub fn logs_server(&self) -> LogsServiceServer<Self> {
        LogsServiceServer::new(self.clone())
    }
    pub fn traces_server(&self) -> TraceServiceServer<Self> {
        TraceServiceServer::new(self.clone())
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

/// OTLP/HTTP on 4318.
pub fn http_router(r: Receivers) -> Router {
    Router::new()
        .route(
            "/v1/logs",
            post(
                |axum::extract::State(r): axum::extract::State<Receivers>, b: Bytes| async move {
                    export(&r.logs, b, ExportLogsServiceResponse::default()).await
                },
            ),
        )
        .route(
            "/v1/traces",
            post(
                |axum::extract::State(r): axum::extract::State<Receivers>, b: Bytes| async move {
                    export(&r.traces, b, ExportTraceServiceResponse::default()).await
                },
            ),
        )
        .route("/v1/metrics", post(unimplemented))
        .with_state(r)
}

/// Decode, submit, and answer. Generic over the signal because the three
/// endpoints differ only in which two protobuf types they name.
async fn export<R: Message + Default, T: Message>(
    ingest: &Ingest<R>,
    body: Bytes,
    ok: T,
) -> Response {
    let req = match R::decode(body) {
        Ok(r) => r,
        Err(e) => return (StatusCode::BAD_REQUEST, e.to_string()).into_response(),
    };
    match ingest.submit(req).await {
        Ok(()) => (
            StatusCode::OK,
            [(header::CONTENT_TYPE, "application/x-protobuf")],
            ok.encode_to_vec(),
        )
            .into_response(),
        Err(Rejected::Busy) => (
            StatusCode::SERVICE_UNAVAILABLE,
            [(header::RETRY_AFTER, "1")],
            "ingest queue full",
        )
            .into_response(),
        Err(Rejected::Closed) => (StatusCode::SERVICE_UNAVAILABLE, "shutting down").into_response(),
        Err(Rejected::Failed(e)) => (StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    }
}

async fn unimplemented() -> impl IntoResponse {
    (
        StatusCode::NOT_IMPLEMENTED,
        "metrics ingest is not wired up yet",
    )
}
