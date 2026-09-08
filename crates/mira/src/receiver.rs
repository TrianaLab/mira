//! OTLP receivers.
//!
//! Two listeners, because OTLP is two protocols. 4317 is OTLP/gRPC and is served
//! by tonic. 4318 is OTLP/HTTP — a plain HTTP/1.1 POST of protobuf to
//! `/v1/{traces,metrics,logs}` — which tonic cannot serve; that one is axum,
//! already in the tree via tonic's `router` feature, so it costs no dependency.

use axum::Router;
use axum::body::Bytes;
use axum::http::{StatusCode, header};
use axum::response::IntoResponse;
use axum::routing::post;
use prost::Message;
use tonic::{Request, Response, Status};
use tonic_types::{ErrorDetails, StatusExt};

use mira_proto::collector::logs::v1::logs_service_server::{LogsService, LogsServiceServer};
use mira_proto::collector::logs::v1::{ExportLogsServiceRequest, ExportLogsServiceResponse};

use crate::pipeline::{Ingest, Rejected};

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

pub struct Grpc {
    ingest: Ingest,
}

impl Grpc {
    pub fn server(ingest: Ingest) -> LogsServiceServer<Self> {
        LogsServiceServer::new(Self { ingest })
    }
}

#[tonic::async_trait]
impl LogsService for Grpc {
    async fn export(
        &self,
        request: Request<ExportLogsServiceRequest>,
    ) -> Result<Response<ExportLogsServiceResponse>, Status> {
        self.ingest
            .submit(request.into_inner())
            .await
            .map_err(status_for)?;
        // No partial_success: everything we accepted is durable by now, and
        // anything we could not accept was reported as a status above.
        Ok(Response::new(ExportLogsServiceResponse::default()))
    }
}

/// OTLP/HTTP on 4318.
pub fn http_router(ingest: Ingest) -> Router {
    Router::new()
        .route("/v1/logs", post(export_logs))
        .route("/v1/traces", post(unimplemented))
        .route("/v1/metrics", post(unimplemented))
        .with_state(ingest)
}

async fn export_logs(
    axum::extract::State(ingest): axum::extract::State<Ingest>,
    body: Bytes,
) -> impl IntoResponse {
    let req = match ExportLogsServiceRequest::decode(body) {
        Ok(r) => r,
        Err(e) => return (StatusCode::BAD_REQUEST, e.to_string()).into_response(),
    };
    match ingest.submit(req).await {
        Ok(()) => (
            StatusCode::OK,
            [(header::CONTENT_TYPE, "application/x-protobuf")],
            ExportLogsServiceResponse::default().encode_to_vec(),
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
        "only /v1/logs is wired up so far",
    )
}
