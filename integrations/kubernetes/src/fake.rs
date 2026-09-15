//! A `kube::Client` with a recorder where the API server should be.
//!
//! kube-rs has no envtest — there is no Rust equivalent of standing up a real
//! kube-apiserver and etcd for a test. What it does have is [`Client::new`],
//! which takes any `tower::Service`, so the seam is one layer lower: instead of
//! asserting on cluster state after a reconcile, these tests assert on the
//! exact sequence of HTTP requests it *issued*. For this operator that is the
//! stronger assertion anyway — the two things that must not regress are the
//! drain's ordering and which write claims the lease, and an ordering is a
//! property of the request log rather than of the final state.
//!
//! ponytail: no request matching beyond the path, and every reply is canned.
//! Reach for a real control plane the day a test needs the API server's own
//! behaviour — defaulting, admission, a conflict on resource version — rather
//! than this crate's behaviour. `tests/apiserver.rs` is that seam.

use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};

use http::{Request, Response, StatusCode};
use http_body_util::BodyExt;
use kube::Client;
use kube::client::Body;
use serde_json::json;
use tower::Service;

/// `(method, path) -> (status, body)`.
type Reply = Arc<dyn Fn(&str, &str) -> (StatusCode, serde_json::Value) + Send + Sync>;

#[derive(Clone)]
pub struct Fake {
    calls: Arc<Mutex<Vec<(String, String, serde_json::Value)>>>,
    reply: Reply,
}

/// What a Kubernetes 404 actually looks like on the wire. `get_opt` turns this
/// into `None`, and it only recognises it by the parsed `Status`.
pub fn not_found() -> serde_json::Value {
    json!({
        "kind": "Status", "apiVersion": "v1", "status": "Failure",
        "code": 404, "reason": "NotFound", "message": "not found",
    })
}

/// A 409, which is what optimistic concurrency looks like: the object moved
/// between the read and the write.
pub fn conflict() -> serde_json::Value {
    json!({
        "kind": "Status", "apiVersion": "v1", "status": "Failure",
        "code": 409, "reason": "Conflict", "message": "the object has been modified",
    })
}

impl Fake {
    pub fn new(
        reply: impl Fn(&str, &str) -> (StatusCode, serde_json::Value) + Send + Sync + 'static,
    ) -> Self {
        Self {
            calls: Arc::new(Mutex::new(Vec::new())),
            reply: Arc::new(reply),
        }
    }

    pub fn client(&self, ns: &str) -> Client {
        Client::new(self.clone(), ns)
    }

    /// `METHOD /path`, in the order they were issued.
    pub fn log(&self) -> Vec<String> {
        self.calls
            .lock()
            .unwrap()
            .iter()
            .map(|(m, p, _)| format!("{m} {p}"))
            .collect()
    }

    /// The body of the first request whose line contains `needle`.
    ///
    /// The lock is dropped before the panic arm runs, and that is not
    /// tidiness: `log()` takes the same mutex, a `std::sync::Mutex` is not
    /// reentrant, and building the failure message while still holding it
    /// deadlocks the thread. An assertion that should have failed in
    /// milliseconds instead hung the whole suite with no output.
    pub fn body(&self, needle: &str) -> serde_json::Value {
        let found = self
            .calls
            .lock()
            .unwrap()
            .iter()
            .find(|(m, p, _)| format!("{m} {p}").contains(needle))
            .map(|(_, _, b)| b.clone());
        found.unwrap_or_else(|| panic!("no request matching {needle:?} in {:?}", self.log()))
    }
}

impl Service<Request<Body>> for Fake {
    type Response = Response<Body>;
    type Error = std::convert::Infallible;
    type Future = std::pin::Pin<
        Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send + 'static>,
    >;

    fn poll_ready(&mut self, _: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, req: Request<Body>) -> Self::Future {
        let (calls, reply) = (self.calls.clone(), self.reply.clone());
        Box::pin(async move {
            let method = req.method().to_string();
            let path = req.uri().path().to_owned();
            let bytes = req.into_body().collect().await.unwrap().to_bytes();
            let body = serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
            let (code, out) = reply(&method, &path);
            calls.lock().unwrap().push((method, path, body));
            Ok(Response::builder()
                .status(code)
                .header("content-type", "application/json")
                .body(Body::from(serde_json::to_vec(&out).unwrap()))
                .unwrap())
        })
    }
}
