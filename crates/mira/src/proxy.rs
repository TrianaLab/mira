//! `mira proxy`: one read and write surface in front of N storage nodes.
//!
//! # Why this can hold no state
//!
//! The thing that usually forces a coordinator on a fan-out query is paging: N
//! nodes each answer "the newest 100" and the merger has to remember, for every
//! reader, how far into each node's stream it had got. Mira does not need that,
//! and the reason is [`Cursor`]. A cursor is `(ts, node, seq, row)` where `node`
//! is [`mira_core::block::node_id`] — so it is already a *global* total order
//! over every row on every replica, with no coordination anywhere, and it was
//! that before this file existed.
//!
//! So the whole protocol is: send the same `after` cursor to every replica, ask
//! each for the newest `limit` rows behind it, merge the answers on that same
//! order, cut to `limit`, and hand back the cursor of the last row emitted.
//! Every row not emitted sorts strictly after that cursor on every replica, so
//! the next page is exact — no duplicates, no gaps — and the proxy has
//! forgotten the reader by the time the response is written. Principle 4 is
//! satisfied by construction rather than by care.
//!
//! # What it refuses
//!
//! `/api/v1/query` and the three OTLP endpoints, and nothing else. `correlate`,
//! `map`, `metrics/query` and `entities` all answer with something built by
//! walking one node's blocks — a trace assembled from the spans that are local,
//! a service map from the edges that are local. Merging those is not "sort and
//! cut": two nodes each holding half a trace produce two partial frames, and
//! there is no cursor to interleave them on. So this proxy answers 501 and says
//! which endpoint to call directly. A plausible subset is the failure mode
//! `identity.rs` refuses a hash for, and it would be worse here, because
//! nothing in the response would say it was partial.
//!
//! # Where the reads are not merged, the writes are placed
//!
//! Ingest routes on [`resource_key`], the same 64-bit entity identity the
//! storage layer already joins on, so one entity's records land on one replica.
//! That is not for the query path above — that one fans out regardless — it is
//! so each replica's blocks stay entity-local: the `_entity` block filter keeps
//! its selectivity, and "everything this pod emitted" remains a question one
//! node can answer completely, which is what the 501 above is holding the door
//! open for.

use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::Router;
use axum::body::Bytes;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use http_body_util::{BodyExt, Full};
use prost::Message;

use mira_core::identity::resource_key;
use mira_core::query::{self, Cursor};

use mira_proto::collector::logs::v1::{ExportLogsServiceRequest, ExportLogsServiceResponse};
use mira_proto::collector::metrics::v1::{
    ExportMetricsServiceRequest, ExportMetricsServiceResponse,
};
use mira_proto::collector::trace::v1::{ExportTraceServiceRequest, ExportTraceServiceResponse};

use crate::api;

/// The replica list, resolved once at startup and never revisited.
///
/// Static on purpose. A membership protocol is the one mechanism principle 4
/// names, and the thing it would buy — a replica joining without a restart —
/// is not worth a second consensus implementation in a binary whose whole claim
/// is that it has none. The list is a config key and a rolling restart.
#[derive(Clone)]
pub struct Proxy {
    replicas: Arc<[String]>,
    max_request_bytes: usize,
}

impl Proxy {
    pub fn new(replicas: Vec<String>, max_request_bytes: usize) -> Result<Proxy, String> {
        if replicas.is_empty() {
            return Err("mira proxy needs at least one --replica URI".into());
        }
        Ok(Proxy {
            replicas: replicas.into(),
            max_request_bytes,
        })
    }
}

pub fn router(p: Proxy) -> Router {
    let max = p.max_request_bytes;
    Router::new()
        .route("/api/v1/query", post(query_handler))
        .route("/v1/logs", post(logs_handler))
        .route("/v1/traces", post(traces_handler))
        .route("/v1/metrics", post(metrics_handler))
        // Every read this proxy cannot merge, named rather than 404'd: a 404
        // reads as "old build" and sends whoever hit it looking at versions.
        .route("/api/v1/correlate", post(unmergeable))
        .route("/api/v1/map", post(unmergeable))
        .route("/api/v1/metrics/query", post(unmergeable))
        .route("/api/v1/metrics/names", post(unmergeable))
        .route("/api/v1/entities", post(unmergeable))
        .layer(axum::extract::DefaultBodyLimit::max(max))
        .with_state(p)
}

async fn unmergeable(uri: axum::http::Uri) -> Response {
    let path = uri.path().to_owned();
    fail(
        StatusCode::NOT_IMPLEMENTED,
        &format!(
            "{path} is answered from one node's blocks and cannot be merged across replicas; \
             query a replica directly. See `mira proxy` in the usage."
        ),
    )
}

// ---------------------------------------------------------------- reads

/// Fan a record search out to every replica and merge the pages.
async fn query_handler(State(p): State<Proxy>, body: String) -> Response {
    let t = Instant::now();
    // Parsed before anything is spliced, so a document this proxy then rewrites
    // is still reported back to the caller in the words of what they sent.
    let caller = match api::parse(&body) {
        Ok(d) => d,
        Err(e) => return fail(StatusCode::BAD_REQUEST, &e),
    };
    // `with_cursors` can add the key and cannot replace one: a mapping carrying
    // it twice is not a document at all, and splicing blind turns a caller who
    // asked for cursors — the one who most obviously meant to — into a KYAML
    // duplicate-key error from a replica. So a caller who spelled it out has
    // their bytes forwarded unaltered, whichever way they spelled it; `"false"`
    // is then refused below rather than silently overridden.
    let doc = match caller["cursors"].is_badvalue() {
        true => with_cursors(&body),
        false => body.clone(),
    };
    // Parsed here as well as on the replicas, for two things that are only
    // knowable locally: `limit`, which is where the merged page is cut, and
    // whether the line above actually did what it says. A document that reached
    // the replicas without `cursors` set would come back without them and the
    // merge would fall back to whatever order the replicas answered in — the
    // one failure this file must not have, so it is an assertion and not a
    // comment.
    let q = match api::parse(&doc).and_then(|d| api::search_doc(&d, api::now_nanos())) {
        Ok(q) if q.cursors => q,
        Ok(_) => {
            return fail(
                StatusCode::BAD_REQUEST,
                "the query document set `cursors` to false; the proxy needs them to merge",
            );
        }
        Err(e) => return fail(StatusCode::BAD_REQUEST, &e),
    };
    // Whether the *caller* wanted them, which is a different question: the
    // proxy always asks the replicas, and passes them on only if asked itself.
    let wanted = api::search_doc(&caller, api::now_nanos()).is_ok_and(|c: query::Search| c.cursors);

    let answers = match fanout(&p, "/api/v1/query", TEXT, doc.into_bytes()).await {
        Ok(a) => a,
        Err(e) => return fail(StatusCode::BAD_GATEWAY, &e),
    };
    let bodies: Vec<String> = answers.into_iter().map(|(_, b)| b).collect();
    match merge(&bodies, q.limit, wanted) {
        Ok(r) => json_ok(api::envelope("rows", &r, t.elapsed())),
        Err(e) => fail(StatusCode::BAD_GATEWAY, &e),
    }
}

/// Interleave the replicas' pages on the cursor order and cut to `limit`.
///
/// The rows are moved as the bytes the replica sent. Re-serialising them would
/// mean a second renderer to keep in step with `api::envelope`, and the first
/// one is the specification — a proxy that reformats what it forwards is a
/// place for the two to drift.
fn merge(bodies: &[String], limit: usize, keep_cursors: bool) -> Result<query::Results, String> {
    let mut stats = query::Stats::default();
    let mut more = false;
    let mut rows: Vec<(Cursor, &str)> = Vec::new();
    for body in bodies {
        let fields = object(body)?;
        let page = pieces(field(&fields, "rows")?)?;
        let cursors = match field(&fields, "cursors") {
            Ok(cs) => pieces(cs)?,
            // A page with no rows has no cursors, and `api::envelope` leaves
            // the key out rather than writing `[]` — see its doc. So a missing
            // key is only evidence of a mismatched build when there were rows
            // to place, and treating it as one otherwise would let any replica
            // holding nothing for this window fail the whole read.
            Err(_) if page.is_empty() => Vec::new(),
            Err(_) => {
                return Err(
                    "a replica answered without `cursors`; every node behind one \
                            proxy has to be the same build"
                        .into(),
                );
            }
        };
        if page.len() != cursors.len() {
            return Err("a replica's `rows` and `cursors` disagree in length".into());
        }
        for (row, c) in page.into_iter().zip(cursors) {
            rows.push((unquote(c)?.parse()?, row));
        }
        let s = object(field(&fields, "stats")?)?;
        stats.blocks_total += count(&s, "blocks_total")?;
        stats.blocks_scanned += count(&s, "blocks_scanned")?;
        stats.rows_scanned += count(&s, "rows_scanned")?;
        stats.rows_matched += count(&s, "rows_matched")?;
        more |= field(&fields, "next").is_ok();
    }

    // Sorted, then cut — and `more` is set by the cut as well as by any replica
    // that said it was holding more. Both have to count: N short pages can
    // still exceed `limit` between them, and one full page means that replica
    // has more even when the merged set does not overflow.
    rows.sort_unstable_by_key(|(c, _)| c.key());
    more |= rows.len() > limit;
    rows.truncate(limit);

    let mut json = String::from("[");
    for (i, (_, row)) in rows.iter().enumerate() {
        if i > 0 {
            json.push(',');
        }
        json.push_str(row);
    }
    json.push(']');
    Ok(query::Results {
        json,
        stats,
        // The cursor of the last row emitted, which every unemitted row on
        // every replica sorts strictly after. That is what makes the next page
        // exact without the proxy remembering anything: see the module docs.
        next: more.then(|| rows.last().map(|(c, _)| *c)).flatten(),
        cursors: match keep_cursors {
            true => rows.iter().map(|(c, _)| *c).collect(),
            false => Vec::new(),
        },
    })
}

/// The caller's document with `cursors` added.
///
/// Textual because the alternative is a KYAML writer for `Search`, and a round
/// trip through one would normalise whatever the caller wrote — including the
/// `where` terms, which is the part most likely to have a spelling this file
/// has not thought of. The caller's bytes are left exactly as they arrived and
/// one key is added; `query_handler` then parses the result and refuses it if
/// this did not take.
///
/// Added, never replaced, so the caller's document must not carry the key
/// already — `query_handler` checks that, because a mapping with `cursors`
/// twice is not a document and the replica rejects the whole read.
fn with_cursors(body: &str) -> String {
    let b = body.trim();
    match b.strip_prefix('{') {
        // A flow mapping: splice the key in behind the brace. `{}` has no entry
        // to separate from, so no comma.
        Some(rest) if rest.trim_start().starts_with('}') => {
            format!("{{\"cursors\": \"true\"{rest}")
        }
        Some(rest) => format!("{{\"cursors\": \"true\", {rest}"),
        // A block mapping, or an empty body, which is a valid empty search.
        None => format!("{b}\n\"cursors\": \"true\"\n"),
    }
}

// ---------------------------------------------------------------- writes

/// The three OTLP endpoints differ only in which types they name.
macro_rules! ingest {
    ($name:ident, $path:literal, $field:ident, $req:ident, $resp:ident, $json:path) => {
        async fn $name(State(p): State<Proxy>, h: HeaderMap, b: Bytes) -> Response {
            let (json, req) =
                match crate::receiver::decode::<$req>(&h, b, p.max_request_bytes, $json) {
                    Ok(v) => v,
                    Err((json, code, e)) => return crate::receiver::fail(json, code, &e),
                };
            // One sub-export per replica, each holding the resource entries
            // whose entity hashes to it. Empty ones are not sent: a replica
            // that received nothing from this batch has nothing to acknowledge,
            // and an empty export is a round trip for no rows.
            let n = p.replicas.len();
            let mut parts: Vec<Vec<_>> = (0..n).map(|_| Vec::new()).collect();
            for (i, e) in req.$field.into_iter().enumerate() {
                let attrs = e.resource.as_ref().map_or(&[][..], |r| &r.attributes);
                parts[slot(attrs, i, n)].push(e);
            }
            let bodies: Vec<(usize, Vec<u8>)> = parts
                .into_iter()
                .enumerate()
                .filter(|(_, part)| !part.is_empty())
                .map(|(i, part)| (i, $req { $field: part }.encode_to_vec()))
                .collect();
            match scatter(&p, $path, bodies).await {
                Ok(()) => accepted(json, &$resp::default()),
                // Retryable refusals carry the same `Retry-After` a node's own
                // would, so an exporter behind the proxy backs off exactly as it
                // does in front of one.
                Err((code, e)) if code == StatusCode::SERVICE_UNAVAILABLE => (
                    [(header::RETRY_AFTER, "1")],
                    crate::receiver::fail(json, code, &e),
                )
                    .into_response(),
                Err((code, e)) => crate::receiver::fail(json, code, &e),
            }
        }
    };
}

ingest!(
    logs_handler,
    "/v1/logs",
    resource_logs,
    ExportLogsServiceRequest,
    ExportLogsServiceResponse,
    crate::json::logs
);
ingest!(
    traces_handler,
    "/v1/traces",
    resource_spans,
    ExportTraceServiceRequest,
    ExportTraceServiceResponse,
    crate::json::traces
);
ingest!(
    metrics_handler,
    "/v1/metrics",
    resource_metrics,
    ExportMetricsServiceRequest,
    ExportMetricsServiceResponse,
    crate::json::metrics
);

/// Which replica a resource's records belong on.
///
/// The identity hash, so every record describing one entity lands on one node
/// whatever batch it arrived in and whatever order the batches came in — that
/// is the whole property, and it holds without the proxy remembering a thing.
///
/// [`mira_core::identity::NO_IDENTITY`] is spread by position instead. A
/// resource with no identifying attribute has no entity to keep together, by
/// the argument in `identity.rs`, so hashing it to a fixed slot would pile
/// every unidentified sender in a deployment onto replica zero and buy nothing
/// for it.
fn slot(attrs: &[mira_proto::common::v1::KeyValue], i: usize, n: usize) -> usize {
    match resource_key(attrs) {
        mira_core::identity::NO_IDENTITY => i % n,
        // Modulo and not consistent hashing: the replica list is static (see
        // `Proxy`), so the only event that remaps keys is an operator editing
        // it and restarting, and at that point the blocks already written do
        // not move either way. Consistent hashing buys a smaller remap for a
        // rebalance this design does not have.
        //
        // ponytail: `% n` over a static list. If replicas ever join without a
        // restart, this is the line that has to become a ring — and the
        // membership protocol it would need is the thing principle 4 refuses,
        // so the upgrade is the whole design, not this expression.
        k => (k % n as u64) as usize,
    }
}

/// Send each sub-export to its replica and answer once they all have.
///
/// All of them, not a quorum: a 200 to an OTLP exporter means the batch is
/// stored, and there is no second place for the part that was not. A partial
/// failure is a 503 for the whole export, so the exporter retries the whole
/// batch — which re-delivers the sub-exports that did land.
///
/// ponytail: at-least-once on a partial failure, which is what OTLP already is
/// end to end (a collector that times out on a 200 in flight does the same
/// thing). Making it exactly-once needs an idempotency key on the export and a
/// seen-set on the node, which is per-reader coordination state — principle 4
/// again — for a duplicate the query layer is already built to tolerate.
async fn scatter(p: &Proxy, path: &str, bodies: Vec<(usize, Vec<u8>)>) -> Result<(), Status> {
    let mut set = tokio::task::JoinSet::new();
    for (i, body) in bodies {
        let url = format!("{}{path}", p.replicas[i]);
        set.spawn(async move {
            let r = send(&url, PROTOBUF, body).await;
            (url, r)
        });
    }
    let mut worst: Option<Status> = None;
    while let Some(joined) = set.join_next().await {
        let (url, r) = joined.map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
        let bad = match r {
            Err(e) => Some((StatusCode::SERVICE_UNAVAILABLE, format!("{url}: {e}"))),
            Ok((s, _)) if s.is_success() => None,
            Ok((s, b)) => Some((
                retryable(s),
                format!("{url}: HTTP {} {}", s.as_u16(), text(&b)),
            )),
        };
        // The most retryable failure wins, because the exporter's decision is
        // about the whole batch: one replica's 400 next to another's 503 has to
        // come back as the 503, or a transient overload on one node is reported
        // as a permanent error and the batch is dropped.
        if let Some(bad) = bad {
            worst = Some(match worst {
                Some(w) if w.0 == StatusCode::SERVICE_UNAVAILABLE => w,
                _ => bad,
            });
        }
    }
    match worst {
        Some(w) => Err(w),
        None => Ok(()),
    }
}

type Status = (StatusCode, String);

/// A replica's status, mapped to what this proxy should tell the exporter.
///
/// Anything that is not the sender's fault becomes a 503, which is in OTLP's
/// retryable set; a 400 or a 413 is passed through, because retrying those
/// produces the same answer forever.
fn retryable(s: StatusCode) -> StatusCode {
    match s.as_u16() {
        400 | 413 | 415 => s,
        _ => StatusCode::SERVICE_UNAVAILABLE,
    }
}

// ---------------------------------------------------------------- transport

const TEXT: &str = "text/plain; charset=utf-8";
const PROTOBUF: &str = "application/x-protobuf";

/// The same body to every replica, concurrently.
async fn fanout(
    p: &Proxy,
    path: &str,
    ct: &'static str,
    body: Vec<u8>,
) -> Result<Vec<(String, String)>, String> {
    let mut set = tokio::task::JoinSet::new();
    for r in p.replicas.iter() {
        let url = format!("{r}{path}");
        let body = body.clone();
        set.spawn(async move {
            let out = send(&url, ct, body).await;
            (url, out)
        });
    }
    let mut out = Vec::with_capacity(p.replicas.len());
    while let Some(joined) = set.join_next().await {
        let (url, r) = joined.map_err(|e| e.to_string())?;
        // Every replica or none. Dropping the one that did not answer and
        // merging the rest would return a page that is silently missing
        // whatever that node held — and nothing in the response would say so.
        // A read that cannot be complete is an error, not a shorter list.
        let (status, body) = r.map_err(|e| format!("{url}: {e}"))?;
        if !status.is_success() {
            return Err(format!(
                "{url}: HTTP {} {}",
                status.as_u16(),
                text(body.as_ref())
            ));
        }
        out.push((
            url,
            String::from_utf8(body.into()).map_err(|e| e.to_string())?,
        ));
    }
    Ok(out)
}

/// How long a replica has to answer.
///
/// Generous because a cold wide scan on a replica is seconds and the caller
/// asked for it; the bound exists so that a wedged node fails the request
/// rather than holding the connection until the client gives up, which is the
/// failure that looks like the proxy is broken.
const REPLICA_TIMEOUT: Duration = Duration::from_secs(60);

async fn send(url: &str, ct: &'static str, body: Vec<u8>) -> Result<(StatusCode, Bytes), String> {
    let req = hyper::Request::builder()
        .method(hyper::Method::POST)
        .uri(url)
        .header(hyper::header::CONTENT_TYPE, ct)
        .body(Full::new(Bytes::from(body)))
        .map_err(|e| e.to_string())?;
    let resp = tokio::time::timeout(REPLICA_TIMEOUT, client().request(req))
        .await
        .map_err(|_| format!("no answer in {}s", REPLICA_TIMEOUT.as_secs()))?
        .map_err(|e| e.to_string())?;
    let status = resp.status();
    let body = resp
        .into_body()
        .collect()
        .await
        .map_err(|e| e.to_string())?
        .to_bytes();
    Ok((status, body))
}

type Client = hyper_util::client::legacy::Client<
    hyper_util::client::legacy::connect::HttpConnector,
    Full<Bytes>,
>;

/// One pooled client for the process, keep-alive included: a fresh TCP
/// handshake per replica per query is most of the latency of a small one.
///
/// Plain HTTP with no TLS option, unlike the webhook dispatcher. A replica is
/// this deployment's own node on its own network, and `--replica` refuses any
/// other scheme at load — so there is no second place where the eleven crates
/// behind `webhook-tls` have to be argued about.
fn client() -> &'static Client {
    static C: std::sync::OnceLock<Client> = std::sync::OnceLock::new();
    C.get_or_init(|| {
        hyper_util::client::legacy::Client::builder(hyper_util::rt::TokioExecutor::new())
            .build_http()
    })
}

// ---------------------------------------------------------------- JSON, read only

/// Split a JSON object into its keys and the raw text of their values.
///
/// Not a JSON parser and not trying to be: it finds boundaries and hands back
/// slices of the input, because the rows have to leave this process as the
/// bytes that entered it. Every byte it is pointed at came out of
/// `api::envelope`, so the shapes it refuses are the ones that say a replica is
/// running a build this one does not understand — which is worth an error and
/// not a guess.
///
/// ponytail: enough of a scanner to walk one envelope. It is not a general JSON
/// reader and must not grow into one — if something here ever needs the values
/// themselves, `api::parse` already reads JSON as the KYAML subset it is
/// (principle 5), and the reason this does not use it is only that a round trip
/// through `Yaml` would re-render the rows.
fn object(s: &str) -> Result<Vec<(&str, &str)>, String> {
    pieces(s)?.into_iter().map(field_of).collect()
}

/// The comma-separated pieces inside one `[...]` or `{...}`, as slices.
fn pieces(s: &str) -> Result<Vec<&str>, String> {
    let s = s.trim();
    let inner = s
        .strip_prefix(['[', '{'])
        .and_then(|r| r.strip_suffix([']', '}']))
        .ok_or_else(|| format!("expected a JSON array or object, got {s:.40}"))?;
    let (mut out, mut depth, mut start) = (Vec::new(), 0i32, 0usize);
    let (mut quoted, mut escaped) = (false, false);
    for (i, b) in inner.bytes().enumerate() {
        match b {
            _ if escaped => escaped = false,
            b'\\' if quoted => escaped = true,
            b'"' => quoted = !quoted,
            _ if quoted => {}
            b'[' | b'{' => depth += 1,
            b']' | b'}' => depth -= 1,
            b',' if depth == 0 => {
                out.push(inner[start..i].trim());
                start = i + 1;
            }
            _ => {}
        }
    }
    if depth != 0 || quoted {
        return Err("unbalanced JSON in a replica's answer".into());
    }
    let last = inner[start..].trim();
    // An empty collection has no last piece; anything else does, even if the
    // producer left a trailing comma.
    if !last.is_empty() || !out.is_empty() {
        out.push(last);
    }
    Ok(out)
}

/// One `"key": value` piece of an object.
fn field_of(p: &str) -> Result<(&str, &str), String> {
    let bad = || format!("expected a JSON member, got {p:.40}");
    let rest = p.strip_prefix('"').ok_or_else(bad)?;
    // The keys in an envelope are `blocks_total` and friends, so there is no
    // escape to step over — and a key that needed one did not come from Mira.
    let end = rest.find('"').ok_or_else(bad)?;
    let value = rest[end + 1..]
        .trim_start()
        .strip_prefix(':')
        .ok_or_else(bad)?;
    Ok((&rest[..end], value.trim()))
}

fn field<'a>(fields: &[(&'a str, &'a str)], key: &str) -> Result<&'a str, String> {
    fields
        .iter()
        .find(|(k, _)| *k == key)
        .map(|(_, v)| *v)
        .ok_or_else(|| format!("a replica's answer has no {key:?}"))
}

fn count(fields: &[(&str, &str)], key: &str) -> Result<usize, String> {
    field(fields, key)?
        .parse()
        .map_err(|e| format!("{key}: {e}"))
}

fn unquote(s: &str) -> Result<&str, String> {
    s.strip_prefix('"')
        .and_then(|r| r.strip_suffix('"'))
        .ok_or_else(|| format!("expected a quoted cursor, got {s:.40}"))
}

/// A replica's error body, for quoting back inside this proxy's own.
fn text(b: &[u8]) -> String {
    String::from_utf8_lossy(b).chars().take(200).collect()
}

// ---------------------------------------------------------------- responses

fn json_ok(body: String) -> Response {
    (
        StatusCode::OK,
        [(header::CONTENT_TYPE, "application/json")],
        body,
    )
        .into_response()
}

fn accepted<T: Message>(json: bool, ok: &T) -> Response {
    match json {
        true => (
            StatusCode::OK,
            [(header::CONTENT_TYPE, "application/json")],
            "{}",
        )
            .into_response(),
        false => (
            StatusCode::OK,
            [(header::CONTENT_TYPE, "application/x-protobuf")],
            ok.encode_to_vec(),
        )
            .into_response(),
    }
}

/// The same JSON error body a node returns, under a status this file picks.
fn fail(code: StatusCode, msg: &str) -> Response {
    api::error(code, msg)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A replica's answer, rendered by the renderer a replica actually uses.
    ///
    /// Hand-written JSON here would check this file against my idea of the
    /// envelope; `api::envelope` checks it against the envelope, so a key that
    /// moves or a quote that changes fails the merge rather than passing both
    /// sides of a copy.
    ///
    /// Each row carries its own cursor as its only field, which is what makes
    /// the assertions below readable: the merged `rows` array says, in order,
    /// exactly which row came from where.
    fn page(rows: &[(i64, u32, u64)], more: bool) -> String {
        let cursors: Vec<Cursor> = rows
            .iter()
            .map(|&(ts, node, seq)| Cursor {
                ts,
                node,
                seq,
                row: 0,
            })
            .collect();
        let mut json = String::from("[");
        for (i, c) in cursors.iter().enumerate() {
            if i > 0 {
                json.push(',');
            }
            json.push_str(&format!("{{\"body\":\"{c}\"}}"));
        }
        json.push(']');
        api::envelope(
            "rows",
            &query::Results {
                json,
                stats: query::Stats {
                    blocks_total: 4,
                    blocks_scanned: 1,
                    rows_scanned: 10,
                    rows_matched: rows.len(),
                    dropped_series: 0,
                },
                next: more.then(|| *cursors.last().unwrap()),
                cursors,
            },
            Duration::from_micros(7),
        )
    }

    fn bodies(cursor: &str) -> Vec<String> {
        vec![cursor.to_owned()]
    }

    /// The claim in the module docs, on the smallest case that can break it:
    /// two replicas whose pages interleave rather than concatenate.
    #[test]
    fn a_merged_page_is_the_replicas_rows_in_the_global_cursor_order() {
        let a = page(&[(100, 1, 9), (80, 1, 9), (60, 1, 8)], false);
        let b = page(&[(90, 2, 3), (70, 2, 3), (50, 2, 1)], false);
        let r = merge(&[a, b], 10, false).unwrap();
        assert_eq!(
            r.json,
            "[{\"body\":\"100.1.9.0\"},{\"body\":\"90.2.3.0\"},{\"body\":\"80.1.9.0\"},\
             {\"body\":\"70.2.3.0\"},{\"body\":\"60.1.8.0\"},{\"body\":\"50.2.1.0\"}]"
        );
        // Both replicas said they were done and the merged set fits, so this is
        // the last page. `next: Some` here is the bug that makes a reader poll
        // forever.
        assert!(r.next.is_none());
        // Summed, not taken from one answer: the cost of the query is what all
        // the replicas spent on it.
        assert_eq!(r.stats.blocks_total, 8);
        assert_eq!(r.stats.rows_scanned, 20);
        assert_eq!(r.stats.rows_matched, 6);
        // Not asked for, so not carried: a caller who did not ask sees the same
        // body a single node would have sent.
        assert!(r.cursors.is_empty());
    }

    /// A tie on `ts` across two nodes is the case a per-node order cannot
    /// resolve and `Cursor::key` can. Without `node` in the key these two rows
    /// sort by whichever replica answered first, which is a different page on
    /// every request.
    #[test]
    fn rows_sharing_a_nanosecond_on_different_nodes_still_have_one_order() {
        let a = page(&[(100, 7, 1)], false);
        let b = page(&[(100, 3, 1)], false);
        let one = merge(&[a.clone(), b.clone()], 10, false).unwrap();
        let other = merge(&[b, a], 10, false).unwrap();
        assert_eq!(one.json, other.json);
        assert_eq!(
            one.json,
            "[{\"body\":\"100.7.1.0\"},{\"body\":\"100.3.1.0\"}]"
        );
    }

    /// `limit` cuts the merged set, and the cut is what makes `next` the last
    /// *emitted* row rather than the last row anyone held.
    #[test]
    fn the_page_is_cut_at_limit_and_next_is_the_last_row_emitted() {
        let a = page(&[(100, 1, 9), (80, 1, 9)], false);
        let b = page(&[(90, 2, 3), (70, 2, 3)], false);
        let r = merge(&[a, b], 3, false).unwrap();
        assert_eq!(
            r.json,
            "[{\"body\":\"100.1.9.0\"},{\"body\":\"90.2.3.0\"},{\"body\":\"80.1.9.0\"}]"
        );
        assert_eq!(r.next.unwrap().to_string(), "80.1.9.0");
    }

    /// The other way a page is not the last one: nothing overflowed here, but a
    /// replica said it was holding more. Missing this is a reader that stops
    /// three pages early and never learns it did.
    #[test]
    fn a_replica_holding_more_makes_the_merged_page_not_the_last_one() {
        let a = page(&[(100, 1, 9)], true);
        let b = page(&[(90, 2, 3)], false);
        let r = merge(&[a, b], 10, false).unwrap();
        assert_eq!(r.json.matches("body").count(), 2);
        assert_eq!(r.next.unwrap().to_string(), "90.2.3.0");
    }

    /// Asked for, so carried — and index-aligned with the merged rows, not with
    /// the order any one replica sent.
    #[test]
    fn cursors_are_passed_on_only_when_the_caller_asked_and_match_the_merged_rows() {
        let a = page(&[(100, 1, 9), (60, 1, 8)], false);
        let b = page(&[(90, 2, 3)], false);
        let r = merge(&[a, b], 10, true).unwrap();
        let got: Vec<String> = r.cursors.iter().map(Cursor::to_string).collect();
        assert_eq!(got, ["100.1.9.0", "90.2.3.0", "60.1.8.0"]);
    }

    /// Every answer, or an error. A replica running a build without the
    /// `cursors` key would otherwise contribute rows in whatever order it sent
    /// them, which is the one failure this file must not have.
    #[test]
    fn an_answer_this_proxy_cannot_place_is_an_error_and_not_a_guess() {
        let without = api::envelope(
            "rows",
            &query::Results {
                json: "[{\"body\":\"x\"}]".into(),
                stats: query::Stats::default(),
                next: None,
                cursors: Vec::new(),
            },
            Duration::ZERO,
        );
        let why = |body: &str| merge(&bodies(body), 10, false).err().expect("accepted");
        assert!(why(&without).contains("the same build"));

        // Rows and cursors that disagree: index alignment is the whole contract
        // between the two arrays, and a shorter one silently drops rows.
        let short = page(&[(100, 1, 9), (80, 1, 9)], false).replace(
            ",\"cursors\":[\"100.1.9.0\",\"80.1.9.0\"]",
            ",\"cursors\":[\"100.1.9.0\"]",
        );
        assert!(why(&short).contains("disagree in length"));

        // A statistic that is not a number. Every one of these is summed across
        // the replicas, so a replica from a build that renders them differently
        // has to stop the read rather than contribute a zero.
        let odd =
            page(&[(100, 1, 9)], false).replace("\"rows_matched\":1", "\"rows_matched\":\"1\"");
        assert!(why(&odd).contains("rows_matched"), "{odd}");

        for broken in ["", "{}", "not json", "{\"rows\":[]}"] {
            assert!(merge(&bodies(broken), 10, false).is_err(), "{broken:?}");
        }
    }

    /// An empty page is a page. `pieces` has to tell `[]` from `[x]` or the
    /// first replica with nothing to say takes the request down.
    #[test]
    fn a_replica_with_no_rows_contributes_none_and_breaks_nothing() {
        let r = merge(&[page(&[], false), page(&[(90, 2, 3)], false)], 10, false).unwrap();
        assert_eq!(r.json, "[{\"body\":\"90.2.3.0\"}]");
        assert!(r.next.is_none());
    }

    /// `with_cursors` has one job and `query_handler` refuses the request if it
    /// did not do it — so the check is that the document *parses back* with the
    /// flag set, on every shape a caller can send.
    #[test]
    fn every_document_shape_comes_back_with_cursors_on() {
        let docs = [
            // Flow mapping, the shape the docs and the UI send.
            r#"{"signal":"logs","limit":5}"#,
            // Empty flow mapping: valid, and the one with no entry to separate
            // the new key from.
            "{}",
            "{ }",
            // Block mapping, which is what a human writing KYAML by hand sends.
            "signal: \"traces\"\nlimit: 5",
            // No document at all, which `search_doc` reads as every default.
            "",
            "  \n",
        ];
        for doc in docs {
            let out = with_cursors(doc);
            let q = api::parse(&out).and_then(|d| api::search_doc(&d, 0));
            // One assertion rather than an unwrap and then a check: the failure
            // that matters here is "the splice produced something unreadable",
            // and that wants the same message as "it parsed but lost the key".
            assert!(
                q.as_ref().is_ok_and(|q| q.cursors),
                "{doc:?} became {out:?}: {q:?}"
            );
        }
        // And the rest of the document survives the splice.
        let q = api::parse(&with_cursors(r#"{"signal":"traces","limit":7}"#))
            .and_then(|d| api::search_doc(&d, 0))
            .unwrap();
        assert_eq!(q.limit, 7);
        assert!(matches!(q.signal, query::Signal::Traces));
    }

    /// The placement property, stated as the test: one entity, one replica,
    /// whatever batch it arrives in and whatever else is in that batch.
    #[test]
    fn one_entity_lands_on_one_replica_from_any_position_in_any_batch() {
        let kv = |k: &str, v: &str| mira_proto::common::v1::KeyValue {
            key: k.into(),
            value: Some(mira_proto::common::v1::AnyValue {
                value: Some(mira_proto::common::v1::any_value::Value::StringValue(
                    v.into(),
                )),
            }),
        };
        let pod = [
            kv("service.name", "checkout"),
            kv("service.instance.id", "7"),
        ];
        // Reordered and with a non-identifying attribute added, which is the
        // drift `identity.rs` is built to see through — the point being that
        // placement inherits that property rather than re-deriving it.
        let drifted = [
            kv("k8s.node.name", "node-4"),
            kv("service.instance.id", "7"),
            kv("service.name", "checkout"),
        ];
        for n in 1..8 {
            let want = slot(&pod, 0, n);
            assert!(want < n);
            for i in 0..5 {
                assert_eq!(slot(&pod, i, n), want, "n={n} i={i}");
                assert_eq!(slot(&drifted, i, n), want, "n={n} i={i} drifted");
            }
        }

        // Nothing identifying: spread by position, so a deployment of
        // unidentified senders does not pile onto one node.
        let n = 3;
        let spread: Vec<usize> = (0..6).map(|i| slot(&[], i, n)).collect();
        assert_eq!(spread, [0, 1, 2, 0, 1, 2]);
    }

    /// The scanner, on the shapes an envelope actually contains — a row whose
    /// string value holds the characters it splits on being the one that turns
    /// a merge into nonsense rather than into an error.
    #[test]
    fn the_scanner_splits_on_structure_and_not_on_punctuation_inside_strings() {
        assert_eq!(pieces("[]").unwrap(), Vec::<&str>::new());
        assert_eq!(pieces("{ }").unwrap(), Vec::<&str>::new());
        assert_eq!(pieces("[1, 2]").unwrap(), ["1", "2"]);
        assert_eq!(
            pieces(r#"[{"a":{"b":[1,2]}},{"c":"x,y"}]"#).unwrap(),
            [r#"{"a":{"b":[1,2]}}"#, r#"{"c":"x,y"}"#]
        );
        // A log body with a brace and an escaped quote in it, which is most log
        // bodies that were ever worth reading.
        assert_eq!(
            pieces(r#"[{"body":"{\"level\": \"warn\"}, retrying"},{"body":"ok"}]"#).unwrap(),
            [
                r#"{"body":"{\"level\": \"warn\"}, retrying"}"#,
                r#"{"body":"ok"}"#
            ]
        );
        for bad in ["", "[1,2", "{\"a\":1", "[\"unterminated]"] {
            assert!(pieces(bad).is_err(), "{bad:?}");
        }

        let o = object(r#"{"rows":[{"a":1}],"stats":{"n":2},"next":"1.2.3.4"}"#).unwrap();
        assert_eq!(field(&o, "rows").unwrap(), "[{\"a\":1}]");
        assert_eq!(
            count(&object(field(&o, "stats").unwrap()).unwrap(), "n").unwrap(),
            2
        );
        assert_eq!(unquote(field(&o, "next").unwrap()).unwrap(), "1.2.3.4");
        assert!(field(&o, "cursors").is_err());
        assert!(unquote("1.2.3.4").is_err());
        assert!(field_of("\"a\" 1").is_err());
    }

    /// A proxy with no replicas answers nothing, so it is refused at startup
    /// rather than at the first query.
    #[test]
    fn a_proxy_with_no_replicas_is_refused_where_the_operator_can_see_it() {
        assert!(Proxy::new(Vec::new(), 1).is_err());
        assert!(Proxy::new(vec!["http://127.0.0.1:1".into()], 1).is_ok());
    }

    /// What the exporter is told to do about each kind of refusal. The 503s are
    /// the ones that matter: reporting a transient overload as permanent drops
    /// the batch.
    #[test]
    fn only_the_senders_own_fault_is_passed_back_unretryable() {
        for pass in [400u16, 413, 415] {
            let s = StatusCode::from_u16(pass).unwrap();
            assert_eq!(retryable(s), s);
        }
        for retry in [429u16, 500, 502, 503, 504] {
            assert_eq!(
                retryable(StatusCode::from_u16(retry).unwrap()),
                StatusCode::SERVICE_UNAVAILABLE
            );
        }
    }

    /// The three ways a replica can fail that are not a status code.
    ///
    /// A refused connection is the one every other test in this file exercises.
    /// These are the rest of `send`: an address that is not a URL, a node that
    /// accepts the connection and then says nothing, and one that announces a
    /// body longer than the one it sends before hanging up. Each has to come
    /// back as an error naming itself — a `None` or an empty page here is a
    /// silently short answer, which is the failure mode this whole file exists
    /// to not have.
    #[tokio::test(start_paused = true)]
    async fn a_replica_that_answers_badly_is_an_error_and_not_a_short_page() {
        // Refused before a socket is opened: a space is not a URI.
        let e = send("http://exa mple/v1/logs", PROTOBUF, Vec::new())
            .await
            .expect_err("a malformed address was accepted");
        assert!(!e.is_empty());

        // `start_paused` is what makes the 60s bound a test and not a wait:
        // tokio advances its clock as soon as nothing is runnable, so the timer
        // fires the moment this task is the only thing left.
        let (_deaf, url) = deaf();
        let e = send(&url, PROTOBUF, Vec::new())
            .await
            .expect_err("silence was read as an answer");
        assert!(e.contains("no answer in 60s"), "{e}");

        // Ten bytes promised, three sent, socket closed. hyper reports a body
        // that stops halfway on the request rather than on the body — the two
        // share one connection error — so this lands on the same line of `send`
        // as a refusal. It is here for the wire shape, which is the one a
        // replica killed mid-response actually produces.
        let e = send(
            &raw(b"HTTP/1.1 200 OK\r\nContent-Length: 10\r\n\r\nabc"),
            PROTOBUF,
            Vec::new(),
        )
        .await
        .expect_err("a truncated body was read as an answer");
        assert!(!e.is_empty());
    }

    /// A port the kernel accepts a connection on and nothing ever answers.
    ///
    /// Silence has to be a listener nobody accepts from, not a server that
    /// writes nothing: writing nothing still drops the socket, dropping it
    /// sends a FIN, and a client that reads EOF reports a dead connection
    /// rather than waiting out its timeout. Which of the two won used to be a
    /// race between the kernel's delivery and the paused clock, and it lost on
    /// Linux under coverage instrumentation. The handshake completes into the
    /// backlog with no `accept`, so the connection is open and mute for as long
    /// as the caller holds the listener.
    fn deaf() -> (std::net::TcpListener, String) {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = l.local_addr().unwrap();
        (l, format!("http://{addr}"))
    }

    /// A listener that writes one canned reply to one connection and closes.
    ///
    /// Not an `axum::Router`: what these need is a server that is *wrong* at the
    /// HTTP level, which a correct server cannot be asked to be.
    fn raw(reply: &'static [u8]) -> String {
        use std::io::Write;
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = l.local_addr().unwrap();
        std::thread::spawn(move || {
            let Ok((mut s, _)) = l.accept() else { return };
            // Not reading the request first: the reply is canned either way, and
            // a read that blocks on a client which has already sent everything
            // is a hang rather than a test.
            let _ = s.write_all(reply);
        });
        format!("http://{addr}")
    }
}
