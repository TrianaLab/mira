//! MCP, on the same port as everything else.
//!
//! The agentic principle's first reading: a model should be able to point at
//! Mira and ask, without a translation layer in between. That is one endpoint,
//! `POST /mcp`, speaking JSON-RPC 2.0 over Streamable HTTP.
//!
//! Four tools, and they are the same four questions the UI asks — deliberately.
//! An agent and a human looking at the same incident should be reading the same
//! numbers out of the same code path; a separate "agent API" is a second read
//! path to keep correct, and the first thing it does is drift.
//!
//! Stateless, and not by accident. Streamable HTTP lets a server hand out an
//! `Mcp-Session-Id` and then requires every later request to carry it, which
//! makes the server a thing with memory that a load balancer has to route back
//! to the same replica. We issue none: every request carries everything it
//! needs, any replica can answer it, and killing one loses nothing. That is
//! principle 4 applied to the agent surface.
//!
//! No SSE either. The responses here are single JSON documents that arrive when
//! the scan finishes, so a stream would be one event and a teardown.

use axum::Router;
use axum::extract::State;
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use yaml_rust2::Yaml;

use mira_core::json::Json;
use mira_core::query::{Op, Search, Signal, Target, Term, Value};
use mira_core::series;

use crate::api::{self, Api};

/// The revision of the MCP spec these messages conform to.
const PROTOCOL: &str = "2025-06-18";

pub fn router(api: Api) -> Router {
    Router::new().route("/mcp", post(handler)).with_state(api)
}

/// Tool definitions, verbatim.
///
/// Written by hand rather than generated, because this text is the prompt. A
/// model chooses a tool and fills its arguments from these descriptions alone,
/// so what belongs here is the shape of the data and the mistake to avoid —
/// not a restatement of the parameter names.
const TOOLS: &str = r#"[
{"name":"query_records",
 "description":"Search logs or spans. Returns matching records newest-first with all attributes merged in, plus the blocks and rows the scan touched. Terms are AND-ed. Use `attr` for OpenTelemetry attributes (service.name, http.route, k8s.pod.name) and `field` for columns of the record itself (severity_text, severity_number, body, name, duration_nano, status_code, trace_id, span_id). Attributes are searched at all three levels - record, resource and scope - so you do not need to know where the SDK put them. Time bounds default to the last hour; widening them costs blocks scanned.",
 "inputSchema":{"type":"object","properties":{
   "signal":{"type":"string","enum":["logs","traces"],"description":"default logs"},
   "from":{"type":"string","description":"'-15m', 'now', or absolute nanoseconds. Default -1h."},
   "to":{"type":"string","description":"same forms. Default now."},
   "where":{"type":"array","description":"AND-ed terms, each {attr|field: <name>, <op>: <value>} where op is one of eq ne lt lte gt gte contains","items":{"type":"object"}},
   "limit":{"type":"integer","description":"default 100, max 10000"}}}},

{"name":"get_trace",
 "description":"Every span of one trace, by trace id, over all of retention. Prefer this to query_records with a trace_id filter: blocks carry a trace-id index, and this is the call that uses it. Returns spans newest-first; parent_span_id links them into the tree.",
 "inputSchema":{"type":"object","required":["trace_id"],"properties":{
   "trace_id":{"type":"string","description":"32 hex characters, as returned in any span or log record"},
   "limit":{"type":"integer","description":"default 1000, max 10000"}}}},

{"name":"query_metric",
 "description":"One metric as time series, grouped by attribute set. Each series carries its identifying attributes and its points. Sums are reported as rates where the temporality allows it. Omit `name` at your peril - it scans every metric in the window.",
 "inputSchema":{"type":"object","properties":{
   "name":{"type":"string","description":"exact metric name, from list_metrics"},
   "from":{"type":"string"},"to":{"type":"string"},
   "where":{"type":"array","description":"same term grammar as query_records","items":{"type":"object"}},
   "max_series":{"type":"integer","description":"default 200"},
   "max_points":{"type":"integer","description":"default 5000"}}}},

{"name":"list_metrics",
 "description":"Metric names present in a time window, with unit and kind. Call this before query_metric rather than guessing a name.",
 "inputSchema":{"type":"object","properties":{
   "from":{"type":"string"},"to":{"type":"string"}}}}
]"#;

async fn handler(State(api): State<Api>, body: String) -> Response {
    let doc = match api::parse(&body) {
        Ok(d) => d,
        Err(e) => return rpc_error(&Yaml::Null, -32700, &e),
    };
    let id = doc["id"].clone();
    let method = doc["method"].as_str().unwrap_or_default();
    let params = &doc["params"];

    match method {
        "initialize" => result(
            &id,
            format!(
                "{{\"protocolVersion\":\"{PROTOCOL}\",\"capabilities\":{{\"tools\":{{}}}},\
                 \"serverInfo\":{{\"name\":\"mira\",\"version\":\"{}\"}}}}",
                env!("CARGO_PKG_VERSION")
            ),
        ),
        "tools/list" => result(&id, format!("{{\"tools\":{TOOLS}}}")),
        "tools/call" => call(api, &id, params).await,
        "ping" => result(&id, "{}".into()),
        // A notification has no id and takes no reply. `notifications/initialized`
        // is the one every client sends, and answering it with an error is how a
        // session fails on its second message.
        _ if id.is_badvalue() || id.is_null() => StatusCode::ACCEPTED.into_response(),
        other => rpc_error(&id, -32601, &format!("unknown method {other:?}")),
    }
}

async fn call(api: Api, id: &Yaml, params: &Yaml) -> Response {
    let name = params["name"].as_str().unwrap_or_default();
    let args = &params["arguments"];
    let now = api::now_nanos();
    let dir = api.data_dir.clone();

    // Tool failures are results, not protocol errors: a model that asked for a
    // metric that does not exist needs to read the reason and try again, and a
    // JSON-RPC error is something its client may swallow before it ever sees it.
    let out = match name {
        "query_records" => match api::search_doc(args, now) {
            Ok(q) => blocking("rows", move || mira_core::query::search(&dir, &q)).await,
            Err(e) => Err(e),
        },
        "get_trace" => match trace_search(args) {
            Ok(q) => blocking("rows", move || mira_core::query::search(&dir, &q)).await,
            Err(e) => Err(e),
        },
        "query_metric" => match api::series_doc(args, now) {
            Ok(q) => blocking("series", move || series::series(&dir, &q)).await,
            Err(e) => Err(e),
        },
        "list_metrics" => match api::bounds(args, now) {
            Ok((from, to)) => blocking("names", move || series::names(&dir, from, to)).await,
            Err(e) => Err(e),
        },
        other => Err(format!("unknown tool {other:?}")),
    };

    let mut j = Json::new();
    j.obj(|j| {
        j.key("content");
        j.arr(|j| {
            j.obj(|j| {
                j.key("type");
                j.str("text");
                j.key("text");
                match &out {
                    Ok(text) => j.str(text),
                    Err(e) => j.str(e),
                }
            })
        });
        j.key("isError");
        j.bool(out.is_err());
    });
    result(id, j.into_string())
}

/// "Every span of this trace", which is the one query with no useful time
/// bound: you look a trace up because you do not know when it happened. The
/// window is therefore all of it, and the block-level trace-id filter is what
/// makes that affordable.
fn trace_search(args: &Yaml) -> Result<Search, String> {
    let id = args["trace_id"]
        .as_str()
        .ok_or("trace_id is required, as 32 hex characters")?
        .trim();
    if mira_core::query::unhex(id).is_none_or(|b| b.len() != 16) {
        return Err(format!("{id:?} is not a 16-byte hex trace id"));
    }
    let limit = match &args["limit"] {
        Yaml::Integer(n) if *n > 0 => (*n as usize).min(10_000),
        _ => 1_000,
    };
    Ok(Search {
        signal: Signal::Traces,
        from: 0,
        to: i64::MAX,
        terms: vec![Term {
            target: Target::Field("trace_id".into()),
            op: Op::Eq,
            value: Value::Str(id.to_owned()),
        }],
        limit,
    })
}

/// Same `spawn_blocking` rule as the HTTP API: a cold mmap fault stalls the OS
/// thread it lands on, and tokio has no way to see that happen.
async fn blocking(
    field: &'static str,
    f: impl FnOnce() -> mira_core::error::Result<mira_core::query::Results> + Send + 'static,
) -> Result<String, String> {
    match tokio::task::spawn_blocking(f).await {
        Ok(Ok(r)) => Ok(api::envelope(field, &r)),
        Ok(Err(e)) => Err(e.to_string()),
        Err(e) => Err(format!("query task panicked: {e}")),
    }
}

fn result(id: &Yaml, payload: String) -> Response {
    json(format!(
        "{{\"jsonrpc\":\"2.0\",\"id\":{},\"result\":{payload}}}",
        rpc_id(id)
    ))
}

fn rpc_error(id: &Yaml, code: i32, message: &str) -> Response {
    let mut j = Json::new();
    j.str(message);
    json(format!(
        "{{\"jsonrpc\":\"2.0\",\"id\":{},\"error\":{{\"code\":{code},\"message\":{}}}}}",
        rpc_id(id),
        j.into_string()
    ))
}

/// JSON-RPC ids are a number, a string, or absent. Echoed back exactly, because
/// that is how the client matches the reply to the call.
fn rpc_id(id: &Yaml) -> String {
    match id {
        Yaml::Integer(n) => n.to_string(),
        Yaml::String(s) => {
            let mut j = Json::new();
            j.str(s);
            j.into_string()
        }
        _ => "null".into(),
    }
}

/// Always 200 with a JSON body, even for an error object: in JSON-RPC the
/// failure is in the payload, and a client that sees a 4xx may never parse far
/// enough to find out what went wrong.
fn json(body: String) -> Response {
    (
        StatusCode::OK,
        [(header::CONTENT_TYPE, "application/json")],
        body,
    )
        .into_response()
}
