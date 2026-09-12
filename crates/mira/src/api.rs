//! The query API.
//!
//! Shares the OTLP/HTTP listener rather than taking a port of its own: `/v1/*`
//! is OTLP, `/api/v1/*` is query, `/` is the UI. One port is one thing to
//! expose, one thing to firewall and one thing to get wrong, which is the whole
//! argument for a single binary applied one level down.
//!
//! Queries are KYAML documents, and the parser is the one `config.rs` already
//! uses. That is not a coincidence or a saving — it is the point of the
//! KYAML-first principle. KYAML is a strict YAML 1.2 subset with explicit `{}`
//! and `[]`, quoted strings and permitted trailing commas, which makes valid
//! JSON valid KYAML: a browser can `JSON.stringify` a query and an agent can
//! write one with comments in it, and both arrive at the same parser. There is
//! no second query language and no JSON dependency.
//!
//! Every handler hops to `spawn_blocking` before touching a block. Reads are
//! mmap reads, and a cold page fault stalls the OS thread it lands on with no
//! yield point and no signal to the scheduler; run one on a tokio worker and it
//! blocks every other connection that worker owns.

use std::path::PathBuf;
use std::sync::{Arc, LazyLock};

use axum::Router;
use axum::extract::State;
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use tokio::sync::Semaphore;
use yaml_rust2::parser::{Event, EventReceiver, Parser};
use yaml_rust2::{Yaml, YamlLoader};

use mira_core::frame::{self, Expand};
use mira_core::query::{self, Op, Search, Signal, Target, Term, Value};
use mira_core::series::{self, SeriesQuery};

use crate::pipeline;

#[derive(Clone, Default)]
pub struct Api {
    pub data_dir: Arc<PathBuf>,
    /// The three flushers' open blocks, in [`pipeline::SIGNALS`] order.
    ///
    /// Default slots have no flusher behind them and always read empty, which is
    /// what a unit test wanting only an `Api` gets. See
    /// [`mira_core::signal::Open`].
    pub open: pipeline::OpenSlots,
    /// The alert evaluator's rules and their current state.
    ///
    /// Here rather than in a state of its own because three surfaces read it —
    /// `/api/v1/alerts`, the MCP tool and the TUI pane — and the whole point of
    /// [`crate::alert`] is that they are looking at one object. The default is
    /// an engine with no rules, which answers "alerting is off".
    pub alerts: Arc<crate::alert::Engine>,
}

impl Api {
    /// The open block for one signal, as the read path wants it — everything
    /// acknowledged before this call, published or not.
    ///
    /// Awaited before the query rather than inside it: this is where
    /// read-your-writes is bought, and paying for it here keeps the scan itself
    /// synchronous and off the runtime. One entry per flusher shard, which is
    /// what `search_open` takes — the plumbing was already the general one
    /// before a signal had more than one open block at a time.
    pub(crate) async fn open(&self, signal: &str) -> Vec<Arc<mira_core::signal::Open>> {
        let Some(i) = pipeline::SIGNALS.iter().position(|s| *s == signal) else {
            return Vec::new();
        };
        self.open[i].fresh().await
    }

    /// Every signal's open block, in [`pipeline::SIGNALS`] order.
    ///
    /// For the two frame walks that cross signals. Parallel rather than
    /// concatenated because sequence numbers are per-signal: a flat list would
    /// let the logs snapshot collide with a traces block on `(node, seq)`.
    pub(crate) async fn open_all(&self) -> Vec<Vec<Arc<mira_core::signal::Open>>> {
        let mut out = Vec::with_capacity(pipeline::SIGNALS.len());
        for s in pipeline::SIGNALS {
            out.push(self.open(s).await);
        }
        out
    }
}

pub fn router(api: Api) -> Router {
    Router::new()
        .route("/api/v1/query", post(query_handler))
        .route("/api/v1/metrics/query", post(series_handler))
        .route("/api/v1/metrics/names", post(names_handler))
        .route("/api/v1/correlate", post(correlate_handler))
        .route("/api/v1/map", post(map_handler))
        .route("/api/v1/entities", post(entities_handler))
        .with_state(api)
}

/// Largest `limit` a caller can ask for.
///
/// Results are materialized into one JSON string in memory, so this is a real
/// memory bound and not a policy. An agent asking for everything gets a lot,
/// but not the process.
const MAX_LIMIT: usize = 10_000;

/// Log records or spans matching a filter, newest first.
///
/// The one read every other read is defined in terms of: an alert rule's
/// `query`, a UI list and an agent's `query_records` are all this document.
/// `next` in the response is the cursor to hand back as `after` for the page
/// behind it, and `stats` says how much was scanned to answer.
async fn query_handler(State(api): State<Api>, body: String) -> Response {
    let q = match parse_search(&body, now_nanos()) {
        Ok(q) => q,
        Err(e) => return bad_request(&e),
    };
    let dir = api.data_dir.clone();
    let open = api.open(q.signal.dir()).await;
    run("rows", move || query::search_open(&dir, &q, &open)).await
}

/// One metric's series over a window, with the exemplars naming the traces
/// behind the points.
async fn series_handler(State(api): State<Api>, body: String) -> Response {
    let q = match parse_series(&body, now_nanos()) {
        Ok(q) => q,
        Err(e) => return bad_request(&e),
    };
    let dir = api.data_dir.clone();
    let open = api.open("metrics").await;
    run("series", move || series::series_open(&dir, &q, &open)).await
}

/// Which metric names a window holds. An empty body means *right now*.
///
/// ```yaml
/// { "from": "-1h", "to": "now" }
/// ```
async fn names_handler(State(api): State<Api>, body: String) -> Response {
    let now = now_nanos();
    // An empty body is a valid request for "what is there right now", which is
    // the first thing a UI or an agent asks.
    let doc = match window(if body.trim().is_empty() { "{}" } else { &body }, now) {
        Ok(w) => w,
        Err(e) => return bad_request(&e),
    };
    let dir = api.data_dir.clone();
    let open = api.open("metrics").await;
    run("names", move || {
        series::names_open(&dir, doc.0, doc.1, &open)
    })
    .await
}

/// The frame around a filter: its time extent, the traces it touches and the
/// services that took part — one round trip where a client makes three.
///
/// ```yaml
/// {
///   "signal": "logs",
///   "from": "-15m",
///   "where": [ { "field": "severity_number", "gte": 17 } ],
///   "expand": [ "traces", "around:2s", "peers" ],
/// }
/// ```
///
/// One call is one investigation step: *what was going on around the thing I
/// searched for*. The answer is a frame — a window, the traces it covers and
/// the services that took part — and every field of it is an input to an
/// ordinary `/api/v1/query`, which is what keeps the algebra closed.
async fn correlate_handler(State(api): State<Api>, body: String) -> Response {
    let now = now_nanos();
    let (q, ops) = match parse_correlate(&body, now) {
        Ok(v) => v,
        Err(e) => return bad_request(&e),
    };
    let dir = api.data_dir.clone();
    // Both signals, because `anchor` reads the one the search names and the
    // span-side expanders always read traces.
    let anchored = api.open(q.signal.dir()).await;
    let all = api.open_all().await;
    run("frame", move || correlate(&dir, &q, &ops, &anchored, &all)).await
}

/// Anchor, walk, label. Shared with the MCP tool of the same name, so an agent
/// and the UI are reading one implementation rather than two.
pub(crate) fn correlate(
    dir: &std::path::Path,
    q: &Search,
    ops: &[Expand],
    anchored: &[Arc<mira_core::signal::Open>],
    all: &[Vec<Arc<mira_core::signal::Open>>],
) -> mira_core::error::Result<query::Results> {
    let (f, a) = frame::anchor(dir, q, anchored)?;
    // The span-side expanders read traces and nothing else, so they get the
    // traces slot rather than the whole set.
    let traces = all.get(1).map_or(&[][..], Vec::as_slice);
    let (f, w) = frame::expand(dir, &f, ops, traces)?;
    let names = frame::names_of(dir, &f, all)?;
    let mut j = mira_core::json::Json::new();
    f.write_json(&mut j, &names);
    Ok(query::Results {
        json: j.into_string(),
        stats: query::Stats {
            blocks_total: a.blocks_total + w.blocks_total,
            blocks_scanned: a.blocks_scanned + w.blocks_scanned,
            rows_scanned: a.rows_scanned + w.rows_scanned,
            rows_matched: a.rows_matched + w.rows_matched,
            ..Default::default()
        },
        next: None,
    })
}

/// The service map over a window.
///
/// ```yaml
/// { "from": "-15m", "to": "now", "max_spans": "50000" }
/// ```
///
/// `max_spans` bounds the walk rather than the answer: a map is built by
/// reading spans and joining them by parent, so the honest limit is on how many
/// are read, not on how many edges come back.
async fn map_handler(State(api): State<Api>, body: String) -> Response {
    let now = now_nanos();
    let doc = match parse(if body.trim().is_empty() { "{}" } else { &body }) {
        Ok(d) => d,
        Err(e) => return bad_request(&e),
    };
    let (from, to, max_spans) = match map_doc(&doc, now) {
        Ok(v) => v,
        Err(e) => return bad_request(&e),
    };
    let dir = api.data_dir.clone();
    let open = api.open("traces").await;
    run("map", move || frame::map(&dir, from, to, max_spans, &open)).await
}

/// Every service that produced anything in a window.
///
/// ```yaml
/// { "from": "-15m", "to": "now" }
/// ```
async fn entities_handler(State(api): State<Api>, body: String) -> Response {
    let now = now_nanos();
    let (from, to) = match window(if body.trim().is_empty() { "{}" } else { &body }, now) {
        Ok(w) => w,
        Err(e) => return bad_request(&e),
    };
    let dir = api.data_dir.clone();
    let open = api.open_all().await;
    run("entities", move || frame::entities(&dir, from, to, &open)).await
}

/// How many searches may be on the blocking pool at once.
///
/// The bound is the point, not the number. Every read hops to that pool — and
/// so does every `publish` fsync, which is the ingest path's durability
/// barrier. Unbounded, a burst of wide scans takes every thread tokio will hand
/// out and the flusher queues behind them, so a slow query becomes an ingest
/// stall. A permit keeps the two apart without a second runtime.
///
/// One per core, because a search already fans out across cores by itself
/// (`mira_core::query::search_open`), so the (n+1)th finishes sooner waiting
/// for a permit than time-slicing against n others. Four when the count is
/// unavailable: enough that the UI's three panes never serialise.
static SCANS: LazyLock<Semaphore> =
    LazyLock::new(|| Semaphore::new(std::thread::available_parallelism().map_or(4, |n| n.get())));

/// Run a search on the blocking pool, holding a permit for as long as it takes.
///
/// `spawn_blocking` is not optional here: reads are mmap reads, and a cold page
/// fault stalls the OS thread with no yield point, taking every other connection
/// that tokio worker owns down with it.
pub(crate) async fn scan<T: Send + 'static>(
    f: impl FnOnce() -> T + Send + 'static,
) -> Result<T, tokio::task::JoinError> {
    // Dropped when this returns, which is after the read has finished rather
    // than after it has started — a permit released at `spawn_blocking` would
    // bound nothing.
    let _permit = SCANS.acquire().await.ok();
    tokio::task::spawn_blocking(f).await
}

async fn run(
    field: &'static str,
    f: impl FnOnce() -> mira_core::error::Result<query::Results> + Send + 'static,
) -> Response {
    let t = std::time::Instant::now();
    match scan(f).await {
        Ok(Ok(r)) => json_ok(envelope(field, &r, t.elapsed())),
        Ok(Err(e)) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("query task panicked: {e}"),
        )
            .into_response(),
    }
}

/// The response body every read returns: the rows under their own key, and what
/// the query cost. The cost is not decoration — it is what tells a caller its
/// filter was too broad, and an agent has no other way to find that out.
///
/// `elapsed` is wall time around the read, queueing for a scan permit included.
/// That is the number the caller actually waited, and it is the one worth
/// showing: server time excluding the queue is a figure only the server can
/// enjoy. Microseconds because a hot query here is single-digit milliseconds
/// and "0 ms" is not a measurement.
pub fn envelope(field: &str, r: &query::Results, elapsed: std::time::Duration) -> String {
    format!(
        "{{\"{field}\":{},\"stats\":{{\"blocks_total\":{},\"blocks_scanned\":{},\
         \"rows_scanned\":{},\"rows_matched\":{},\"elapsed_us\":{}}}{}}}",
        r.json,
        r.stats.blocks_total,
        r.stats.blocks_scanned,
        r.stats.rows_scanned,
        r.stats.rows_matched,
        elapsed.as_micros(),
        // Absent rather than null on the last page, so `if (doc.next)` is the
        // whole of a reader's paging logic.
        r.next
            .map(|c| format!(",\"next\":\"{c}\""))
            .unwrap_or_default()
    )
}

fn json_ok(body: String) -> Response {
    (
        StatusCode::OK,
        [(header::CONTENT_TYPE, "application/json")],
        body,
    )
        .into_response()
}

/// Errors come back as JSON too, so a caller has one thing to parse. An agent
/// that has to distinguish a JSON success from a text/plain failure will
/// eventually feed the failure to a JSON parser and report the parse error
/// instead of the actual problem.
fn bad_request(msg: &str) -> Response {
    let mut j = mira_core::json::Json::new();
    j.obj(|j| {
        j.key("error");
        j.str(msg);
    });
    (
        StatusCode::BAD_REQUEST,
        [(header::CONTENT_TYPE, "application/json")],
        j.into_string(),
    )
        .into_response()
}

pub fn now_nanos() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos() as i64
}

/// Parse a query document.
///
/// ```yaml
/// {
///   "signal": "logs",
///   "from": "-15m",
///   "to": "now",
///   "where": [
///     { "attr": "service.name", "eq": "checkout" },
///     { "field": "severity_number", "gte": 17 },
///     { "attr": "http.route", "contains": "/api" },
///   ],
///   "limit": 100,
///   "after": "1757241600000000000.2718281828.7.41",
/// }
/// ```
///
/// `after` is the `next` field of a previous response, passed back verbatim, and
/// is how you read past `limit`. There is no `offset`: on a store still being
/// written to, a batch arriving between two pages shifts every row down, so an
/// offset reader sees a row twice or never — and it costs the engine the whole
/// prefix on every page.
///
/// A term names its target with `attr` or `field` and its operator with the
/// other key, so `{"attr": "x", "eq": 1}` rather than
/// `{"target": ..., "op": ..., "value": ...}`. Two keys instead of three, and
/// the shape reads as the thing it means — which matters more than usual here,
/// because a large share of these documents will be written by a model that has
/// seen the schema once.
pub fn parse_search(text: &str, now: i64) -> Result<Search, String> {
    search_doc(&parse(text)?, now)
}

/// [`parse_search`] on an already-parsed document, for callers that received one
/// nested inside something else — an MCP tool call, say.
pub fn search_doc(doc: &Yaml, now: i64) -> Result<Search, String> {
    known(doc, &["signal", "from", "to", "where", "limit", "after"])?;
    let signal = match doc["signal"].as_str() {
        Some(s) => Signal::parse(s).ok_or(format!("unknown signal {s:?}"))?,
        None => Signal::Logs,
    };
    let (from, to) = bounds(doc, now)?;
    let limit = positive(doc, "limit", 100, MAX_LIMIT)?;
    let after = match &doc["after"] {
        Yaml::BadValue | Yaml::Null => None,
        // A cursor is a string even though it is all digits and dots: KYAML
        // quotes every scalar, and `1757241600000000000.2718281828.7.41` read
        // as a float would come back as a different cursor entirely.
        y => Some(
            y.as_str()
                .ok_or("after: quote the cursor; it is a string")?
                .parse()?,
        ),
    };
    Ok(Search {
        signal,
        from,
        to,
        terms: terms(doc)?,
        limit,
        after,
    })
}

/// Parse a metrics query document.
///
/// ```yaml
/// {
///   "name": "http.server.request.duration",
///   "from": "-1h",
///   "where": [ { "attr": "service.name", "eq": "checkout" } ],
///   "max_series": 50,
/// }
/// ```
///
/// Same `where` grammar as [`parse_search`], because a caller who has learned
/// one filter syntax should not have to learn a second one to look at a chart.
pub fn parse_series(text: &str, now: i64) -> Result<SeriesQuery, String> {
    series_doc(&parse(text)?, now)
}

pub fn series_doc(doc: &Yaml, now: i64) -> Result<SeriesQuery, String> {
    known(
        doc,
        &["name", "from", "to", "where", "max_series", "max_points"],
    )?;
    let (from, to) = bounds(doc, now)?;
    Ok(SeriesQuery {
        name: doc["name"].as_str().map(str::to_owned),
        from,
        to,
        terms: terms(doc)?,
        max_series: positive(doc, "max_series", 200, 2_000)?,
        max_points: positive(doc, "max_points", 5_000, 100_000)?,
    })
}

/// A correlate document: a search, plus the walk to apply to the frame it
/// anchors.
pub fn parse_correlate(text: &str, now: i64) -> Result<(Search, Vec<Expand>), String> {
    correlate_doc(&parse(text)?, now)
}

/// [`parse_correlate`] on an already-parsed document, for the MCP side.
pub fn correlate_doc(doc: &Yaml, now: i64) -> Result<(Search, Vec<Expand>), String> {
    known(
        doc,
        &["signal", "from", "to", "where", "limit", "after", "expand"],
    )?;
    Ok((correlate_search(doc, now)?, expands(&doc["expand"])?))
}

/// The window and the span budget of a service-map request.
pub fn map_doc(doc: &Yaml, now: i64) -> Result<(i64, i64, usize), String> {
    known(doc, &["from", "to", "max_spans"])?;
    let (from, to) = bounds(doc, now)?;
    Ok((from, to, positive(doc, "max_spans", 1_000_000, 20_000_000)?))
}

/// The search half of a correlate document.
///
/// `search_doc` would refuse `expand` as an unknown key, and relaxing it there
/// would let a misspelled key through on `/api/v1/query` — which is the one
/// mistake the strictness exists to catch. Stripping the key here costs a clone
/// of a document that is a handful of scalars.
fn correlate_search(doc: &Yaml, now: i64) -> Result<Search, String> {
    let mut map = doc.as_hash().cloned().unwrap_or_default();
    map.remove(&Yaml::String("expand".into()));
    search_doc(&Yaml::Hash(map), now)
}

/// `["traces", "around:2s", "peers"]`.
///
/// A list and not a set: the operations do not commute, and `around` before
/// `traces` is overwritten by the extent `traces` measures.
fn expands(y: &Yaml) -> Result<Vec<Expand>, String> {
    let items = match y {
        Yaml::BadValue | Yaml::Null => return Ok(Vec::new()),
        Yaml::Array(a) => a,
        _ => return Err("`expand` must be a list of steps".into()),
    };
    items
        .iter()
        .map(|s| {
            let s = s.as_str().ok_or("each `expand` step must be a string")?;
            match s.split_once(':') {
                Some(("around", d)) => {
                    Ok(Expand::Around(crate::config::duration(d)?.as_nanos() as i64))
                }
                None if s == "traces" => Ok(Expand::Traces),
                None if s == "peers" => Ok(Expand::Peers),
                _ => Err(format!(
                    "unknown expand step {s:?}; expected traces, peers, or around:<duration>"
                )),
            }
        })
        .collect()
}

/// The `from`/`to` pair of any query document.
pub fn window(text: &str, now: i64) -> Result<(i64, i64), String> {
    window_doc(&parse(text)?, now)
}

/// [`window`] on an already-parsed document, for the MCP side.
pub fn window_doc(doc: &Yaml, now: i64) -> Result<(i64, i64), String> {
    known(doc, &["from", "to"])?;
    bounds(doc, now)
}

/// Refuse a document that is not a mapping, or that carries a key this endpoint
/// does not implement.
///
/// Every other reader here indexes by key and defaults what is missing, so a
/// misspelled `where` is indistinguishable from no `where` at all and the answer
/// is a confident 200 over the whole window — the one failure a caller cannot
/// see in the response it gets. `parse_term` has always been this strict one
/// level down; this is the same rule at the top of the document.
pub fn known(doc: &Yaml, keys: &[&str]) -> Result<(), String> {
    // A tool call that carries no `arguments` at all is a legal MCP request and
    // means the same thing as an empty document.
    if doc.is_badvalue() || doc.is_null() {
        return Ok(());
    }
    let map = doc.as_hash().ok_or("a query must be a mapping")?;
    for k in map.keys() {
        let k = k.as_str().ok_or("query keys must be strings")?;
        if !keys.contains(&k) {
            return Err(format!(
                "unknown query key {k:?}; expected one of {}",
                keys.join(" ")
            ));
        }
    }
    Ok(())
}

/// One KYAML document, or a readable reason it is not one.
pub fn parse(text: &str) -> Result<Yaml, String> {
    if has_alias(text) {
        return Err(
            "KYAML has no anchors or aliases: write the value out instead of \
             referring to an anchor with `*`"
                .into(),
        );
    }
    let docs = match YamlLoader::load_from_str(text) {
        Ok(docs) => docs,
        // JSON spells a non-BMP character as a surrogate pair and `json.dumps`
        // does so by default, but YAML 1.2 has no surrogates and the loader
        // refuses one — which would make an ASCII-escaping JSON client the one
        // client principle 5 does not get for free. Retrying only a document
        // that has already failed keeps the rewrite away from every valid one:
        // a single-quoted `'\ud83d\ude00'` is twelve literal characters, and
        // it parses on the first attempt.
        Err(e) => YamlLoader::load_from_str(&fold_surrogates(text))
            .map_err(|_| format!("not valid KYAML: {e}"))?,
    };
    docs.into_iter().next().ok_or("empty query".into())
}

/// Does this document use a YAML alias?
///
/// It has to be answered before the loader sees the text, because yaml-rust2
/// resolves an alias by deep-cloning the anchored node into the tree. Seven
/// levels of nine-way reuse is 356 bytes on the wire and a gigabyte of `Yaml` in
/// the loader, nine levels is under 400 bytes and the OOM killer \u2014 and with
/// `panic = "abort"` that takes every open block and in-flight export with it.
/// The endpoints that route through here are unauthenticated on 4318, and
/// neither `ingest.max_request_bytes` nor the inflate cap sees anything wrong
/// with a body this small.
///
/// Bounding the expansion would be the answer if aliases were a feature Mira
/// owed anyone. KYAML has no anchors and no aliases, so the whole mechanism is
/// refused instead \u2014 which is also the diagnosis, rather than a limit the caller
/// has to reverse-engineer from a truncated document.
///
/// The scan is a second pass over the token stream, so it is skipped for the
/// bodies that cannot contain an alias: the token always starts with `*`, and
/// `memchr` over the body costs a fraction of what parsing it does. A `*` inside
/// a quoted string \u2014 a log body, a wildcard in a filter \u2014 pays for the pass and
/// changes nothing else.
fn has_alias(text: &str) -> bool {
    if !text.contains('*') {
        return false;
    }
    #[derive(Default)]
    struct Spy(bool);
    impl EventReceiver for Spy {
        fn on_event(&mut self, ev: Event) {
            self.0 |= matches!(ev, Event::Alias(_));
        }
    }
    let mut spy = Spy::default();
    // A syntax error is not this function's to report: the loader hits the same
    // one and says where it is.
    let _ = Parser::new_from_str(text).load(&mut spy, true);
    spy.0
}

/// `\ud83d\ude00` becomes the character it encodes. Everything else is copied
/// through untouched, an unpaired surrogate included: that is not a character,
/// and folding it into a replacement one would turn a rejected query into a
/// silently different one.
fn fold_surrogates(text: &str) -> String {
    fn hex4(s: &str) -> Option<u32> {
        u32::from_str_radix(s.strip_prefix("\\u")?.get(..4)?, 16).ok()
    }
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(i) = rest.find("\\u") {
        out.push_str(&rest[..i]);
        let folded = match (hex4(&rest[i..]), rest.get(i + 6..).and_then(hex4)) {
            (Some(h @ 0xD800..=0xDBFF), Some(l @ 0xDC00..=0xDFFF)) => {
                char::from_u32(0x10000 + ((h - 0xD800) << 10) + (l - 0xDC00))
            }
            _ => None,
        };
        match folded {
            Some(c) => {
                out.push(c);
                rest = &rest[i + 12..];
            }
            None => {
                out.push_str("\\u");
                rest = &rest[i + 2..];
            }
        }
    }
    out.push_str(rest);
    out
}

/// Default window is the last hour. A query with no bounds would scan the whole
/// retention period, which is the one mistake that turns a fast engine into a
/// slow one, and it is the mistake an agent makes first.
pub fn bounds(doc: &Yaml, now: i64) -> Result<(i64, i64), String> {
    let from = time_field(&doc["from"], now, now - 3_600_000_000_000)?;
    let to = time_field(&doc["to"], now, now)?;
    if from > to {
        return Err(format!("from ({from}) is after to ({to})"));
    }
    Ok((from, to))
}

fn terms(doc: &Yaml) -> Result<Vec<Term>, String> {
    match &doc["where"] {
        Yaml::BadValue | Yaml::Null => Ok(Vec::new()),
        Yaml::Array(a) => a.iter().map(parse_term).collect(),
        _ => Err("`where` must be a list of terms".into()),
    }
}

/// A positive integer bound, defaulted and capped rather than refused. Every
/// one of these caps a materialization that happens in memory.
fn positive(doc: &Yaml, key: &str, default: usize, max: usize) -> Result<usize, String> {
    match &doc[key] {
        Yaml::BadValue | Yaml::Null => Ok(default),
        Yaml::Integer(n) if *n > 0 => Ok((*n as usize).min(max)),
        other => Err(format!("{key} must be a positive integer, got {other:?}")),
    }
}

fn parse_term(y: &Yaml) -> Result<Term, String> {
    let map = y.as_hash().ok_or("each `where` term must be a mapping")?;
    let mut target = None;
    let mut opval = None;
    for (k, v) in map {
        let k = k.as_str().ok_or("term keys must be strings")?;
        match k {
            "attr" => {
                target = Some(Target::Attr(
                    v.as_str().ok_or("`attr` must be a string")?.to_owned(),
                ));
            }
            "field" => {
                target = Some(Target::Field(
                    v.as_str().ok_or("`field` must be a string")?.to_owned(),
                ));
            }
            other => {
                let op = Op::parse(other).ok_or(format!(
                    "unknown term key {other:?}; expected attr, field, or one of \
                     eq ne lt lte gt gte contains"
                ))?;
                opval = Some((op, scalar(v)?));
            }
        }
    }
    let target = target.ok_or("a term needs `attr` or `field`")?;
    let (op, value) = opval.ok_or("a term needs an operator, such as `eq`")?;
    Ok(Term { target, op, value })
}

/// A query scalar.
///
/// Unlike `config.rs::scalar`, which refuses anything YAML guessed at, this
/// keeps the guesses. The two are answering different questions: a config value
/// of `0x1f` almost certainly meant the string, whereas a query comparing
/// against `31` means the number, and JSON — which is where these documents come
/// from — has already committed to that distinction in its syntax.
fn scalar(y: &Yaml) -> Result<Value, String> {
    Ok(match y {
        Yaml::String(s) => Value::Str(s.clone()),
        Yaml::Integer(i) => Value::Int(*i),
        Yaml::Boolean(b) => Value::Bool(*b),
        Yaml::Real(r) => Value::Double(r.parse().map_err(|_| format!("{r:?} is not a number"))?),
        other => return Err(format!("{other:?} is not a comparable value")),
    })
}

/// `now`, `-15m`, or absolute nanoseconds.
///
/// Relative is the form a UI and an agent both reach for, and it is the form
/// that survives being pasted into a chat and run an hour later. Absolute
/// nanoseconds are the form the API returns, so a value from a result can be
/// fed straight back in.
fn time_field(y: &Yaml, now: i64, default: i64) -> Result<i64, String> {
    match y {
        Yaml::BadValue | Yaml::Null => Ok(default),
        Yaml::Integer(n) => Ok(*n),
        Yaml::String(s) if s == "now" => Ok(now),
        Yaml::String(s) => {
            let (sign, rest) = match s.strip_prefix('-') {
                Some(r) => (-1i64, r),
                None => (1, s.strip_prefix('+').unwrap_or(s)),
            };
            let d = crate::config::duration(rest)?;
            Ok(now + sign * (d.as_nanos() as i64))
        }
        other => Err(format!("{other:?} is not a time")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The query document is written here as a browser would send it — bare
    /// JSON — because "KYAML is a superset of JSON" is load-bearing for the API
    /// and not something to take on faith from a spec.
    #[test]
    fn json_from_a_browser_parses_as_kyaml() {
        let now = 1_000_000_000_000_000_000;
        let q = parse_search(
            r#"{"signal":"traces","from":"-15m","to":"now","limit":50,
                "where":[{"attr":"service.name","eq":"checkout"},
                         {"field":"duration_nano","gte":500000000},
                         {"field":"name","contains":"GET"}]}"#,
            now,
        )
        .unwrap();
        assert_eq!(q.signal, Signal::Traces);
        assert_eq!(q.to, now);
        assert_eq!(q.from, now - 900_000_000_000);
        assert_eq!(q.limit, 50);
        assert_eq!(q.terms.len(), 3);
        assert!(matches!(&q.terms[0].target, Target::Attr(k) if k == "service.name"));
        assert_eq!(q.terms[1].op, Op::Gte);
        assert_eq!(q.terms[1].value, Value::Int(500_000_000));
        assert_eq!(q.terms[2].op, Op::Contains);
    }

    /// ...and the same query from a JSON client that escapes non-ASCII, which
    /// `json.dumps` does by default. A character outside the BMP arrives as a
    /// surrogate pair, which YAML 1.2 has no notion of, so without the fold the
    /// one client that does not work for free is a JSON one.
    #[test]
    fn json_surrogate_escapes_are_folded_into_the_character() {
        let q = parse_search(
            r#"{"where":[{"field":"body","contains":"caf\u00e9 \ud83d\ude00"}]}"#,
            0,
        )
        .unwrap();
        assert_eq!(q.terms[0].value, Value::Str("café 😀".into()));
        // Half a pair is not a character, so it stays the loader's error rather
        // than becoming a replacement character in a filter that then silently
        // matches nothing.
        let err =
            parse_search(r#"{"where":[{"field":"body","contains":"\ud83d"}]}"#, 0).unwrap_err();
        assert!(err.contains("not valid KYAML"), "{err}");
    }

    /// ...and the same query in house style, with the trailing commas and the
    /// comment that JSON cannot carry. This is what makes the format worth
    /// having: an agent can annotate its own query.
    #[test]
    fn kyaml_with_comments_and_trailing_commas_parses_the_same() {
        let q = parse_search(
            r#"{
              "signal": "logs",
              # only the errors
              "where": [
                { "field": "severity_number", "gte": 17 },
              ],
            }"#,
            0,
        )
        .unwrap();
        assert_eq!(q.signal, Signal::Logs);
        assert_eq!(q.terms.len(), 1);
        assert_eq!(q.limit, 100);
        // An unbounded query would scan all of retention, so the default window
        // is an hour rather than everything.
        assert_eq!(q.from, -3_600_000_000_000);
    }

    #[test]
    fn malformed_queries_say_what_is_wrong() {
        let err = |s: &str| parse_search(s, 0).unwrap_err();
        assert!(err(r#"{"signal":"jaeger"}"#).contains("unknown signal"));
        assert!(err(r#"{"where":[{"attr":"a","like":"b"}]}"#).contains("unknown term key"));
        assert!(err(r#"{"where":[{"eq":"b"}]}"#).contains("needs `attr` or `field`"));
        assert!(err(r#"{"where":[{"attr":"a"}]}"#).contains("needs an operator"));
        assert!(err(r#"{"from":"now","to":"-1h"}"#).contains("is after"));
        assert!(err(r#"{"limit":0}"#).contains("positive integer"));
        // A key nothing implements is a typo, and answering a typo with a page
        // of unfiltered rows is worse than answering it with an error: the
        // caller has no way to tell that its filter was dropped.
        assert!(err(r#"{"signal":"logs","filters":[]}"#).contains("unknown query key"));
        assert!(err(r#"{"query":{"signal":"logs"}}"#).contains("unknown query key"));
        // A whole document that is not a mapping is not a query either.
        assert!(err("[1,2,3]").contains("must be a mapping"));
        let step = parse_series(r#"{"name":"m","step":"1m"}"#, 0).unwrap_err();
        assert!(step.contains("step"), "{step}");
        let w = window(r#"{"from":0,"limit":5}"#, 0).unwrap_err();
        assert!(w.contains("limit"), "{w}");
        // A container is not a scalar and not a time. Both readers default what
        // is missing, so the alternative to refusing these is comparing against
        // whatever `unwrap_or_default` produced — a filter that matches nothing
        // and a window that says it was honoured.
        assert!(err(r#"{"where":[{"attr":"a","eq":[1,2]}]}"#).contains("not a comparable value"));
        assert!(err(r#"{"where":[{"attr":"a","eq":{}}]}"#).contains("not a comparable value"));
        assert!(err(r#"{"from":[1]}"#).contains("is not a time"));
        assert!(err(r#"{"to":{"at":1}}"#).contains("is not a time"));
    }

    /// Unknown keys are refused, so the known set has to be exactly what the
    /// shipped clients send — a false rejection breaks the browser UI, the
    /// terminal UI and every MCP tool at once. The terminal UI's documents are
    /// checked in `tui.rs` against the code that builds them; these are the
    /// browser's and the agent's.
    #[test]
    fn every_document_the_clients_send_is_accepted() {
        for d in [
            r#"{"signal":"logs","from":"-1h","to":"now","where":[],"limit":200}"#,
            r#"{"signal":"traces","from":0,"to":"now","limit":2000,
                "where":[{"field":"trace_id","eq":"ab"}]}"#,
            r#"{"signal":"logs","limit":100,"after":"1757241600000000000.2718281828.7.41"}"#,
        ] {
            assert!(parse_search(d, 0).is_ok(), "{d}");
        }
        for d in [
            r#"{"name":"m","from":"-1h","to":"now","where":[]}"#,
            r#"{"name":"m","from":"-1h","to":"now","max_series":64,"max_points":400,"where":[]}"#,
        ] {
            assert!(parse_series(d, 0).is_ok(), "{d}");
        }
        for d in [
            r#"{"signal":"logs","from":"-1h","to":"now","where":[],"limit":200,
                "expand":["traces","peers"]}"#,
            r#"{"expand":[]}"#,
            "{}",
        ] {
            assert!(parse_correlate(d, 0).is_ok(), "{d}");
        }
        assert!(window(r#"{"from":"-1h","to":"now"}"#, 0).is_ok());
        assert!(window("{}", 0).is_ok());
        // An MCP tool call is allowed to carry no `arguments` member at all.
        assert!(search_doc(&Yaml::BadValue, 0).is_ok());
        assert!(correlate_doc(&Yaml::BadValue, 0).is_ok());
        assert!(map_doc(&Yaml::BadValue, 0).is_ok());
    }

    /// The walk is a list because the steps do not commute, and the wire
    /// spelling of a step is the only place the algebra meets a string — so
    /// both the parse and every way of getting it wrong are checked here.
    #[test]
    fn an_expansion_walk_parses_in_order_and_says_what_it_does_not_know() {
        let (q, ops) = parse_correlate(
            r#"{"signal":"traces","expand":["around:2s","traces","peers"]}"#,
            0,
        )
        .unwrap();
        assert_eq!(q.signal.dir(), "traces");
        assert_eq!(
            ops,
            [Expand::Around(2_000_000_000), Expand::Traces, Expand::Peers]
        );
        for (d, want) in [
            (r#"{"expand":"traces"}"#, "list of steps"),
            (r#"{"expand":[7]}"#, "must be a string"),
            (r#"{"expand":["sideways"]}"#, "sideways"),
            (r#"{"expand":["around:soon"]}"#, "soon"),
            (r#"{"expand":["peers:1"]}"#, "peers:1"),
            (r#"{"expanded":[]}"#, "unknown query key"),
        ] {
            assert!(parse_correlate(d, 0).unwrap_err().contains(want), "{d}");
        }
    }

    /// A YAML alias bomb: six levels of nine-way reuse, 250 bytes on the wire,
    /// 531,441 leaves once expanded. It is short of the seven levels that took a
    /// live server to 1.02 GB on purpose — if this guard ever regresses the test
    /// should fail, not take the runner's memory with it.
    ///
    /// Every unauthenticated endpoint on 4318 routes through `parse`, so the
    /// refusal is checked here rather than once per handler.
    #[test]
    fn an_alias_bomb_is_refused_before_it_is_expanded() {
        let bomb = r#"{"a":&a "lol",
          "b":&b [*a,*a,*a,*a,*a,*a,*a,*a,*a],
          "c":&c [*b,*b,*b,*b,*b,*b,*b,*b,*b],
          "d":&d [*c,*c,*c,*c,*c,*c,*c,*c,*c],
          "e":&e [*d,*d,*d,*d,*d,*d,*d,*d,*d],
          "f":&f [*e,*e,*e,*e,*e,*e,*e,*e,*e],
          "g":[*f,*f,*f,*f,*f,*f,*f,*f,*f]}"#;
        let err = parse(bomb).unwrap_err();
        // The reader of this is a model as often as a person, and "invalid
        // document" would leave both of them editing at random.
        assert!(err.contains("alias"), "{err}");
        assert!(err.contains("KYAML"), "{err}");
        // Every entry point that takes a body reaches the same guard.
        assert!(parse_search(bomb, 0).is_err());
        assert!(parse_series(bomb, 0).is_err());
        assert!(window(bomb, 0).is_err());
        // An anchor nothing refers to expands to nothing, so it is only the
        // alias that is refused.
        assert!(parse(r#"{"signal":&s "logs"}"#).is_ok());

        // The mechanism being guarded against, at a depth that is safe to load:
        // the loader materialises each alias as a full copy rather than sharing
        // it, so every level multiplies the tree by nine.
        let three = r#"{"a":&a "lol","b":&b [*a,*a,*a,*a,*a,*a,*a,*a,*a],
          "c":&c [*b,*b,*b,*b,*b,*b,*b,*b,*b],"d":[*c,*c,*c,*c,*c,*c,*c,*c,*c]}"#;
        let expanded = YamlLoader::load_from_str(three).unwrap().remove(0);
        assert_eq!(expanded["d"].as_vec().unwrap().len(), 9);
        assert_eq!(expanded["d"][8][8].as_vec().unwrap().len(), 9);
        assert_eq!(expanded["d"][8][8][8].as_str(), Some("lol"));
    }

    /// The scan is skipped unless the body contains a `*`, and a `*` that is
    /// part of a string is not an alias — a filter looking for a wildcard in a
    /// log line has to keep working, and it is the one input that pays for the
    /// pass.
    #[test]
    fn a_star_inside_a_string_is_not_an_alias() {
        let q = parse_search(r#"{"where":[{"field":"body","contains":"rate *"}]}"#, 0).unwrap();
        assert_eq!(q.terms[0].value, Value::Str("rate *".into()));
    }

    #[test]
    fn limit_is_capped_rather_than_refused() {
        let q = parse_search(r#"{"limit":9999999}"#, 0).unwrap();
        assert_eq!(q.limit, MAX_LIMIT);
    }

    /// Status, content type and body of one handler's answer.
    async fn answer(res: Response) -> (StatusCode, String, String) {
        let status = res.status();
        let ct = res
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default()
            .to_owned();
        let body = axum::body::to_bytes(res.into_body(), usize::MAX)
            .await
            .unwrap();
        (status, ct, String::from_utf8(body.to_vec()).unwrap())
    }

    /// Each of the six endpoints validates its own document, and each one
    /// refuses with a JSON error object under a 400.
    ///
    /// Both halves are load-bearing. Skipping the check is not a crash — it is
    /// a confident 200 over the whole window with the caller's filter silently
    /// dropped, which is the one failure that does not appear in the response.
    /// And an error that came back as `text/plain` would be fed to a JSON
    /// parser by every client here, which then reports the parse error instead
    /// of the reason.
    #[tokio::test]
    async fn every_endpoint_refuses_a_document_it_cannot_honour_with_a_json_400() {
        let api = Api::default();
        let st = || State(api.clone());
        for (res, want) in [
            (
                query_handler(st(), r#"{"signal":"jaeger"}"#.into()).await,
                "unknown signal",
            ),
            (
                series_handler(st(), r#"{"max_series":0}"#.into()).await,
                "max_series must be a positive integer",
            ),
            (
                names_handler(st(), r#"{"from":"yesterday"}"#.into()).await,
                "yesterday",
            ),
            (
                correlate_handler(st(), r#"{"expand":"traces"}"#.into()).await,
                "`expand` must be a list of steps",
            ),
            // The service map is the one endpoint that parses and validates in
            // two steps, so it has two ways to answer 400.
            (
                map_handler(st(), "{not: [kyaml".into()).await,
                "not valid KYAML",
            ),
            (
                map_handler(st(), r#"{"max_spans":0}"#.into()).await,
                "max_spans must be a positive integer",
            ),
            (
                entities_handler(st(), r#"{"limit":5}"#.into()).await,
                "unknown query key \\\"limit\\\"",
            ),
        ] {
            let (status, ct, body) = answer(res).await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "{want}: {body}");
            assert_eq!(ct, "application/json", "{want}");
            assert!(body.starts_with(r#"{"error":""#), "{want}: {body}");
            assert!(body.contains(want), "{body}");
        }

        // The three endpoints whose document is optional treat an empty body as
        // "everything, now" rather than as a malformed request — it is the
        // first call the UI and an agent both make.
        let dir = std::env::temp_dir().join(format!("mira-api-empty-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let api = Api {
            data_dir: Arc::new(dir.clone()),
            ..Default::default()
        };
        let st = || State(api.clone());
        for res in [
            names_handler(st(), String::new()).await,
            map_handler(st(), "  ".into()).await,
            entities_handler(st(), String::new()).await,
        ] {
            let (status, ct, body) = answer(res).await;
            assert_eq!(status, StatusCode::OK, "{body}");
            assert_eq!(ct, "application/json");
            assert!(body.contains("\"stats\":"), "{body}");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A read that fails, and a read that panics, are both a 500 with the
    /// reason in the body — not a dropped connection and not a dead process.
    ///
    /// `spawn_blocking` catches the unwind, so the alternative to reporting the
    /// `JoinError` is a request that never answers while the server carries on
    /// as if nothing happened. The store is broken the way a half-written data
    /// directory is: a plain file where a signal's directory belongs.
    #[tokio::test]
    async fn a_failed_or_panicking_scan_answers_500_with_the_reason() {
        let dir = std::env::temp_dir().join(format!("mira-api-broken-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("logs"), b"not a directory").unwrap();
        let api = Api {
            data_dir: Arc::new(dir.clone()),
            ..Default::default()
        };
        let (status, _, body) = answer(query_handler(State(api), "{}".into()).await).await;
        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR, "{body}");
        assert!(!body.is_empty(), "a 500 with no reason is not a report");
        // The read returned an error; it did not take the process near a panic.
        assert!(!body.contains("panicked"), "{body}");

        // The panic is deliberate: this is the arm that exists because a read
        // runs on a pool thread whose unwind tokio hands back as a `JoinError`.
        let (status, _, body) = answer(run("rows", || panic!("a page fault, say")).await).await;
        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
        assert!(body.contains("query task panicked"), "{body}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A signal name nothing implements has no open block, rather than an index
    /// into an array of three.
    ///
    /// Every handler reaches this with a string, and `correlate` reaches it
    /// with one the caller chose. Out of range would be a panic on the request
    /// path, and with `panic = "abort"` in the release profile that is the
    /// process and every in-flight export with it.
    ///
    /// Exactly one slot is served, because three empty ones cannot tell an
    /// unknown name apart from a known one: a lookup that answered slot 0 for
    /// everything it did not recognise would read identically, and it would
    /// serve logs to a caller asking about profiles.
    #[tokio::test]
    async fn an_unknown_signal_has_no_open_block_rather_than_an_index_out_of_range() {
        let dir = std::env::temp_dir().join(format!("mira-api-slots-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let pcfg = Arc::new(pipeline::Config {
            data_dir: dir.clone(),
            node: 0x51,
            // With the log on, the acknowledgement is the frame and not the
            // seal — which is the only way to hold an export that is both
            // acknowledged and still in the builder, which is what an open
            // block *is*. Without it `submit` would wait out `max_block_age`
            // below and there would be nothing open to find.
            wal: Some(Arc::new(mira_core::wal::Wal::open(&dir, 0x51).unwrap())),
            // Long enough that nothing seals mid-test: the block has to still
            // be open for the slot to have anything in it.
            max_block_age: std::time::Duration::from_secs(3_600),
            ..Default::default()
        });
        let (ingest, logs_slot, flusher) = pipeline::spawn::<mira_core::logs::LogsBuilder>(&pcfg);
        assert!(
            ingest
                .submit(crate::e2e::logs_export("checkout", 1_000, 3))
                .await
                .is_ok()
        );
        let api = Api {
            data_dir: Arc::new(dir.clone()),
            open: [logs_slot, Default::default(), Default::default()],
            ..Default::default()
        };

        let logs = api.open("logs").await;
        assert_eq!(logs.len(), 1, "the served slot answers with its open block");
        assert_eq!(logs[0].sealed.num_rows, 3);
        // Neither an unknown name nor another signal reaches into it.
        for absent in ["profiles", "", "traces", "metrics"] {
            assert!(api.open(absent).await.is_empty(), "{absent}");
        }

        // One slot per signal and in signal order, not a flat concatenation:
        // sequence numbers are per-signal and a flat list would let logs
        // collide with traces on `(node, seq)`.
        let all = api.open_all().await;
        assert_eq!(all.len(), pipeline::SIGNALS.len());
        let i = pipeline::SIGNALS.iter().position(|s| *s == "logs").unwrap();
        for (j, blocks) in all.iter().enumerate() {
            assert_eq!(blocks.len(), usize::from(j == i), "slot {j}");
        }

        drop(ingest);
        flusher.await.unwrap();
        let _ = std::fs::remove_dir_all(&dir);
    }
}
