//! MCP, on the same port as everything else.
//!
//! The agentic principle's first reading: a model should be able to point at
//! Mira and ask, without a translation layer in between. That is one endpoint,
//! `POST /mcp`, speaking JSON-RPC 2.0 over Streamable HTTP.
//!
//! Eight tools, and they are the same eight questions the UI asks —
//! deliberately. An agent and a human looking at the same incident should be
//! reading the same numbers out of the same code path; a separate "agent API"
//! is a second read path to keep correct, and the first thing it does is
//! drift.
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
 "description":"Search logs or spans. Returns matching records newest-first with all attributes merged in, plus the blocks and rows the scan touched. Terms are AND-ed. Use `attr` for OpenTelemetry attributes (service.name, http.route, k8s.pod.name) and `field` for columns of the record itself (severity_text, severity_number, body, name, duration_nano, status_code, trace_id, span_id). Attributes are searched at all three levels - record, resource and scope - so you do not need to know where the SDK put them. Time bounds default to the last hour; widening them costs blocks scanned. `rows_matched` above `limit` means you are seeing a page: narrow the filter, or page with `after`.",
 "inputSchema":{"type":"object","properties":{
   "signal":{"type":"string","enum":["logs","traces"],"description":"default logs"},
   "from":{"type":"string","description":"'-15m', 'now', or absolute nanoseconds. Default -1h."},
   "to":{"type":"string","description":"same forms. Default now."},
   "where":{"type":"array","description":"AND-ed terms, each {attr|field: <name>, <op>: <value>} where op is one of eq ne lt lte gt gte contains","items":{"type":"object"}},
   "limit":{"type":"integer","description":"default 100, max 10000"},
   "after":{"type":"string","description":"the `next` value from a previous response, verbatim, to continue where it stopped. Absent `next` means that was the last page. There is no offset: a store still being written to shifts under one."}}}},

{"name":"get_trace",
 "description":"Every span of one trace, by trace id, over all of retention. Prefer this to query_records with a trace_id filter: blocks carry a trace-id index, and this is the call that uses it. Returns spans newest-first; parent_span_id links them into the tree.",
 "inputSchema":{"type":"object","required":["trace_id"],"properties":{
   "trace_id":{"type":"string","description":"32 hex characters, as returned in any span or log record"},
   "limit":{"type":"integer","description":"default 1000, max 10000"}}}},

{"name":"query_metric",
 "description":"One metric as time series, grouped by attribute set. Each series carries its identifying attributes, its `temporality` as the OTLP enum (0 unspecified, 1 delta, 2 cumulative), whether it is `monotonic`, and its points. Points are the values as stored: no step, no aggregation and no rate, so a cumulative sum is the running counter and a per-second rate is yours to derive by subtracting consecutive points and dividing by the gap between their timestamps. Omit `name` at your peril - it scans every metric in the window.",
 "inputSchema":{"type":"object","properties":{
   "name":{"type":"string","description":"exact metric name, from list_metrics"},
   "from":{"type":"string"},"to":{"type":"string"},
   "where":{"type":"array","description":"same term grammar as query_records","items":{"type":"object"}},
   "max_series":{"type":"integer","description":"default 200"},
   "max_points":{"type":"integer","description":"default 5000"}}}},

{"name":"list_metrics",
 "description":"Metric names present in a time window, with unit and kind. Call this before query_metric rather than guessing a name.",
 "inputSchema":{"type":"object","properties":{
   "from":{"type":"string"},"to":{"type":"string"}}}},

{"name":"correlate",
 "description":"The frame around what a search matched: the time window, the traces those records belong to, and the services that took part. This is the tool for 'what else was happening', and it replaces the usual three round trips of query, read a trace id, query again. `expand` is an ordered walk, and the order matters: 'traces' widens the window to the real start and end of the traces found, which you almost always want first because a log line is written after the request it describes; 'peers' then adds every service that appears in those traces; 'around:<duration>' pads the window by hand. Every field of the answer is an input to another call - feed a trace to get_trace, a service name to query_records as {attr: service.name, eq: <name>}, and from/to to anything. `truncated` true means the frame hit its cap and is a sample, so narrow the search before drawing conclusions.",
 "inputSchema":{"type":"object","properties":{
   "signal":{"type":"string","enum":["logs","traces"],"description":"what to anchor on, default logs"},
   "from":{"type":"string"},"to":{"type":"string"},
   "where":{"type":"array","description":"same term grammar as query_records","items":{"type":"object"}},
   "expand":{"type":"array","description":"ordered steps, each one of: traces, peers, around:<duration> such as around:30s","items":{"type":"string"}}}}},

{"name":"service_map",
 "description":"Who calls whom in a time window, computed from parent_span_id at read time. Nodes are services with their span and error counts; edges carry calls, errors, average and max duration in nanoseconds. The edge from \"entry\" is traffic arriving from outside the traced system. `unresolved` counts spans whose parent was not in the sample - if it is a large fraction of the spans, raise max_spans or narrow the window before trusting a thin edge. Use this to find the failing dependency before querying its records.",
 "inputSchema":{"type":"object","properties":{
   "from":{"type":"string"},"to":{"type":"string"},
   "max_spans":{"type":"integer","description":"span budget, default 1000000. Newest blocks are read first, so a small budget is a recent sample, not a truncated one."}}}},

{"name":"list_services",
 "description":"Every service that emitted anything in a time window, with the stable entity key Mira identifies it by. Call this before filtering on service.name rather than guessing at the spelling. Two entries with the same name and different keys are two distinct instances or deployments.",
 "inputSchema":{"type":"object","properties":{
   "from":{"type":"string"},"to":{"type":"string"}}}},

{"name":"list_alerts",
 "description":"Every alerting rule this node evaluates and what it is currently doing: state is one of ok, pending (breaching but has not held for `for_nano` yet) and firing. `value` is the last evaluation, `threshold` and `op` are what it is compared against, and `matched`/`total` are the record counts behind it - a ratio rule counts `matched` of `total`, a count rule counts `matched`. `link` opens the same records in Mira's UI. An empty list means this node has no rules file, not that everything is healthy. `error` non-null means the rule could not be evaluated, which is not the same as not firing. Rules are static KYAML in a file, so this tool reads and never writes.",
 "inputSchema":{"type":"object","properties":{}}}
]"#;

/// The MCP endpoint: JSON-RPC in, one of the eight tools in [`TOOLS`] out.
///
/// Streamable HTTP with no session and no SSE, because every tool here is a
/// single request and a single response. An agent points at this URL and has
/// the same reads the UI does.
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
            &format!(
                "{{\"protocolVersion\":\"{PROTOCOL}\",\"capabilities\":{{\"tools\":{{}}}},\
                 \"serverInfo\":{{\"name\":\"mira\",\"version\":\"{}\"}}}}",
                env!("CARGO_PKG_VERSION")
            ),
        ),
        "tools/list" => result(&id, &format!("{{\"tools\":{TOOLS}}}")),
        "tools/call" => call(api, &id, params).await,
        "ping" => result(&id, "{}"),
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
            Ok(q) => {
                let open = api.open(q.signal.dir()).await;
                blocking("rows", move || {
                    mira_core::query::search_open(&dir, &q, &open)
                })
                .await
            }
            Err(e) => Err(e),
        },
        "get_trace" => match trace_search(args) {
            Ok(q) => {
                let open = api.open(q.signal.dir()).await;
                blocking("rows", move || {
                    mira_core::query::search_open(&dir, &q, &open)
                })
                .await
            }
            Err(e) => Err(e),
        },
        "query_metric" => match api::series_doc(args, now) {
            Ok(q) => {
                let open = api.open("metrics").await;
                blocking("series", move || series::series_open(&dir, &q, &open)).await
            }
            Err(e) => Err(e),
        },
        "list_metrics" => match api::window_doc(args, now) {
            Ok((from, to)) => {
                let open = api.open("metrics").await;
                blocking("names", move || series::names_open(&dir, from, to, &open)).await
            }
            Err(e) => Err(e),
        },
        "correlate" => match api::correlate_doc(args, now) {
            Ok((q, ops)) => {
                let anchored = api.open(q.signal.dir()).await;
                let all = api.open_all().await;
                blocking("frame", move || {
                    api::correlate(&dir, &q, &ops, &anchored, &all)
                })
                .await
            }
            Err(e) => Err(e),
        },
        "service_map" => match api::map_doc(args, now) {
            Ok((from, to, max_spans)) => {
                let open = api.open("traces").await;
                blocking("map", move || {
                    mira_core::frame::map(&dir, from, to, max_spans, &open)
                })
                .await
            }
            Err(e) => Err(e),
        },
        "list_services" => match api::window_doc(args, now) {
            Ok((from, to)) => {
                let open = api.open_all().await;
                blocking("entities", move || {
                    mira_core::frame::entities(&dir, from, to, &open)
                })
                .await
            }
            Err(e) => Err(e),
        },
        // No window and no scan: this is in-memory state, so it does not go
        // through `blocking` and has nothing to report as blocks scanned.
        "list_alerts" => match api::known(args, &[]) {
            Ok(()) => Ok(api.alerts.json()),
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
            });
        });
        j.key("isError");
        j.bool(out.is_err());
    });
    result(id, &j.into_string())
}

/// "Every span of this trace", which is the one query with no useful time
/// bound: you look a trace up because you do not know when it happened. The
/// window is therefore all of it, and the block-level trace-id filter is what
/// makes that affordable.
fn trace_search(args: &Yaml) -> Result<Search, String> {
    // The other three tools get this from `search_doc`/`series_doc`/`window_doc`;
    // this one builds its query by hand, so it has to ask. A misspelled `limit`
    // is a page size the model believes it set.
    api::known(args, &["trace_id", "limit"])?;
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
        // A trace is one page or it is a broken trace. 10,000 spans is already
        // past what any waterfall can show, and an agent handed "here is a
        // third of a trace, ask again" will reason about the third.
        after: None,
    })
}

/// Same rule as the HTTP API, through the same door: a cold mmap fault stalls
/// the OS thread it lands on, and tokio has no way to see that happen. Sharing
/// `api::scan` also shares its permit, so an agent's reads and a browser's are
/// bounded together rather than each getting the whole pool.
async fn blocking(
    field: &'static str,
    f: impl FnOnce() -> mira_core::error::Result<mira_core::query::Results> + Send + 'static,
) -> Result<String, String> {
    let t = std::time::Instant::now();
    match api::scan(f).await {
        Ok(Ok(r)) => Ok(api::envelope(field, &r, t.elapsed())),
        Ok(Err(e)) => Err(e.to_string()),
        Err(e) => Err(format!("query task panicked: {e}")),
    }
}

fn result(id: &Yaml, payload: &str) -> Response {
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::sync::Arc;

    /// A data directory with one logs block in it, so the tools that succeed
    /// have something to have succeeded on.
    fn api(name: &str) -> (Api, PathBuf) {
        let dir = std::env::temp_dir().join(format!("mira-mcp-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let mut b = mira_core::logs::LogsBuilder::new();
        b.append_request(&crate::e2e::logs_export(
            "checkout",
            api::now_nanos() as u64,
            4,
        ))
        .unwrap();
        let sealed = b.finish().unwrap();
        mira_core::block::publish(&dir, "logs", mira_core::block::node_id("a"), 0, 0, &sealed)
            .unwrap();
        (
            Api {
                data_dir: Arc::new(dir.clone()),
                ..Default::default()
            },
            dir,
        )
    }

    async fn rpc(api: &Api, body: &str) -> (StatusCode, String) {
        let res = handler(State(api.clone()), body.to_owned()).await;
        let status = res.status();
        let bytes = axum::body::to_bytes(res.into_body(), usize::MAX)
            .await
            .unwrap();
        (status, String::from_utf8(bytes.to_vec()).unwrap())
    }

    /// A tool call answers with `isError` inside a 200, never a JSON-RPC error.
    /// This is the distinction the module header makes and the one a client is
    /// most likely to get wrong, so every tool is driven through both sides of
    /// it: the argument document that parses, and the one that does not.
    #[tokio::test]
    async fn a_tool_that_cannot_answer_says_so_in_its_result_not_in_the_protocol() {
        let (api, _dir) = api("tools");
        let call = |name: &str, args: &str| {
            format!(
                r#"{{"jsonrpc":"2.0","id":1,"method":"tools/call",
                     "params":{{"name":"{name}","arguments":{args}}}}}"#
            )
        };
        let ok = [
            ("query_records", r#"{"limit":2}"#),
            (
                "get_trace",
                r#"{"trace_id":"4bf92f3577b34da6a3ce929d0e0e4736","limit":5}"#,
            ),
            ("query_metric", r#"{"name":"http.server.duration"}"#),
            ("list_metrics", "{}"),
            (
                "correlate",
                r#"{"expand":["traces","around:30s","peers"],"limit":2}"#,
            ),
            ("service_map", r#"{"max_spans":100}"#),
            ("list_services", "{}"),
            ("list_alerts", "{}"),
        ];
        for (name, args) in ok {
            let (s, body) = rpc(&api, &call(name, args)).await;
            assert_eq!(s, StatusCode::OK, "{name}");
            assert!(body.contains(r#""isError":false"#), "{name}: {body}");
        }

        // A tool that needs no arguments may be called with no `arguments`
        // member at all, and that is a request, not a malformed document.
        let (_, body) = rpc(
            &api,
            r#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"list_metrics"}}"#,
        )
        .await;
        assert!(body.contains(r#""isError":false"#), "{body}");

        // Each tool parses its arguments with a different function, so each one
        // needs its own way of being wrong.
        let bad = [
            ("query_records", r#"{"signal":"metrics"}"#, "unknown signal"),
            ("get_trace", r#"{"trace_id":"nope"}"#, "hex trace id"),
            ("query_metric", r#"{"where":"errors"}"#, "list of terms"),
            ("list_metrics", r#"{"from":"yesterday"}"#, "yesterday"),
            ("teleport", "{}", "unknown tool"),
            // A misspelled key is a filter that was silently dropped, which the
            // model cannot see in a page of unfiltered rows.
            ("query_records", r#"{"filters":[]}"#, "unknown query key"),
            ("query_metric", r#"{"step":"1m"}"#, "unknown query key"),
            ("list_metrics", r#"{"name":"http"}"#, "unknown query key"),
            ("correlate", r#"{"expand":["sideways"]}"#, "sideways"),
            ("correlate", r#"{"expand":"traces"}"#, "list of steps"),
            ("correlate", r#"{"signal":"metrics"}"#, "unknown signal"),
            ("service_map", r#"{"max_spans":0}"#, "max_spans"),
            ("service_map", r#"{"limit":5}"#, "unknown query key"),
            ("list_services", r#"{"to":"soon"}"#, "soon"),
            // `list_alerts` takes no arguments at all, so every key is a
            // misspelling — including a window, which it does not honour and
            // must not appear to.
            ("list_alerts", r#"{"from":"-1h"}"#, "unknown query key"),
            (
                "get_trace",
                r#"{"trace_id":"4bf92f3577b34da6a3ce929d0e0e4736","limitt":5}"#,
                "unknown query key",
            ),
        ];
        for (name, args, want) in bad {
            let (s, body) = rpc(&api, &call(name, args)).await;
            assert_eq!(s, StatusCode::OK, "{name}");
            assert!(body.contains(r#""isError":true"#), "{name}: {body}");
            assert!(body.contains(want), "{name}: {body}");
        }
    }

    /// A read that fails — or panics — underneath the tool is still the tool's
    /// answer, not a dead connection: the model is the one that has to decide
    /// what to do next.
    ///
    /// `spawn_blocking` catches the unwind and hands it back as a `JoinError`.
    /// Dropping it would leave the request hanging with the server otherwise
    /// healthy, and a hung tool call is the one failure an agent cannot reason
    /// about at all.
    #[tokio::test]
    async fn a_broken_store_is_reported_to_the_model_rather_than_thrown() {
        let panicked = blocking("rows", || panic!("a page fault, say"))
            .await
            .unwrap_err();
        assert!(panicked.contains("query task panicked"), "{panicked}");

        let dir = std::env::temp_dir().join(format!("mira-mcp-broken-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("logs"), b"not a directory").unwrap();
        let api = Api {
            data_dir: Arc::new(dir),
            ..Default::default()
        };
        let (s, body) = rpc(
            &api,
            r#"{"jsonrpc":"2.0","id":1,"method":"tools/call",
                "params":{"name":"query_records","arguments":{}}}"#,
        )
        .await;
        assert_eq!(s, StatusCode::OK);
        assert!(body.contains(r#""isError":true"#), "{body}");
    }

    /// The handshake, and the three ways a request can not be a tool call. The
    /// notification case is the one that breaks sessions: every client sends
    /// `notifications/initialized` immediately after `initialize`, and a reply
    /// to it — even a correct-looking error — ends the session on message two.
    #[tokio::test]
    async fn the_protocol_surface_echoes_ids_and_stays_quiet_when_there_is_none() {
        let (api, _dir) = api("proto");
        let (_, body) = rpc(
            &api,
            r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#,
        )
        .await;
        assert!(body.contains(PROTOCOL), "{body}");
        assert!(body.contains(env!("CARGO_PKG_VERSION")), "{body}");

        let (_, body) = rpc(&api, r#"{"jsonrpc":"2.0","id":2,"method":"tools/list"}"#).await;
        for tool in [
            "query_records",
            "get_trace",
            "query_metric",
            "list_metrics",
            "correlate",
            "service_map",
            "list_services",
        ] {
            assert!(body.contains(tool), "{tool} missing from tools/list");
        }
        // These descriptions are the model's only documentation of the data, so
        // they may not promise arithmetic the engine does not do: points come
        // back as stored, and the temporality legend is what lets the model do
        // the subtraction itself.
        assert!(!body.contains("as rates"), "{body}");
        assert!(body.contains("2 cumulative"), "{body}");

        // A string id comes back quoted and escaped, because the client matches
        // on it byte for byte.
        let (_, body) = rpc(&api, r#"{"jsonrpc":"2.0","id":"a\"b","method":"ping"}"#).await;
        assert!(
            body.starts_with(r#"{"jsonrpc":"2.0","id":"a\"b","result":{}}"#),
            "{body}"
        );

        let (s, body) = rpc(
            &api,
            r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#,
        )
        .await;
        assert_eq!(s, StatusCode::ACCEPTED);
        assert!(body.is_empty(), "{body}");

        // A method nobody knows, with an id, is a real JSON-RPC error — and a
        // null id there is still a reply, since the client is waiting for one.
        let (_, body) = rpc(&api, r#"{"jsonrpc":"2.0","id":true,"method":"levitate"}"#).await;
        assert!(body.contains(r#""id":null"#), "{body}");
        assert!(
            body.contains("-32601") && body.contains("levitate"),
            "{body}"
        );

        let (_, body) = rpc(&api, "{not: [kyaml").await;
        assert!(body.contains("-32700"), "{body}");
    }

    /// `get_trace` is the one tool with no time bound, so the id is the only
    /// thing narrowing it. A malformed id has to be refused before the scan,
    /// not turned into a filter that matches nothing after reading retention.
    #[test]
    fn a_trace_lookup_insists_on_a_whole_trace_id() {
        let doc = |s: &str| api::parse(s).unwrap();
        let q = trace_search(&doc(r#"{"trace_id":" 4BF92F3577B34DA6A3CE929D0E0E4736 "}"#)).unwrap();
        assert_eq!(q.signal, Signal::Traces);
        assert_eq!((q.from, q.to), (0, i64::MAX));
        assert_eq!(q.limit, 1_000);
        assert!(q.after.is_none());
        assert_eq!(q.terms.len(), 1);

        let short = trace_search(&doc(r#"{"trace_id":"ab","limit":7}"#)).unwrap_err();
        assert!(short.contains("16-byte"), "{short}");
        // A span id is 8 bytes and looks like a trace id to anyone not counting.
        assert!(trace_search(&doc(r#"{"trace_id":"0102030405060708"}"#)).is_err());
        assert!(trace_search(&doc("{}")).unwrap_err().contains("required"));
        // The key check the other three tools get from their `*_doc` parser: a
        // dropped `limit` is a page size the model thinks it set.
        let typo = trace_search(&doc(
            r#"{"trace_id":"4bf92f3577b34da6a3ce929d0e0e4736","limitt":5}"#,
        ))
        .unwrap_err();
        assert!(
            typo.contains("unknown query key") && typo.contains("limitt"),
            "{typo}"
        );

        let limit = |s: &str| {
            trace_search(&doc(&format!(
                r#"{{"trace_id":"4bf92f3577b34da6a3ce929d0e0e4736","limit":{s}}}"#
            )))
            .unwrap()
            .limit
        };
        assert_eq!(limit("7"), 7);
        assert_eq!(limit("99999"), 10_000);
        // Zero and "all of them" both mean the default rather than an empty page.
        assert_eq!(limit("0"), 1_000);
        assert_eq!(limit(r#""lots""#), 1_000);
    }
}
