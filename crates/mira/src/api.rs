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
use std::sync::Arc;

use axum::Router;
use axum::extract::State;
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use yaml_rust2::{Yaml, YamlLoader};

use mira_core::query::{self, Op, Search, Signal, Target, Term, Value};

#[derive(Clone)]
pub struct Api {
    pub data_dir: Arc<PathBuf>,
}

pub fn router(api: Api) -> Router {
    Router::new()
        .route("/api/v1/query", post(query_handler))
        .with_state(api)
}

/// Largest `limit` a caller can ask for.
///
/// Results are materialized into one JSON string in memory, so this is a real
/// memory bound and not a policy. An agent asking for everything gets a lot,
/// but not the process.
const MAX_LIMIT: usize = 10_000;

async fn query_handler(State(api): State<Api>, body: String) -> Response {
    let now = now_nanos();
    let q = match parse_search(&body, now) {
        Ok(q) => q,
        Err(e) => return bad_request(&e),
    };
    let dir = api.data_dir.clone();
    let out = tokio::task::spawn_blocking(move || query::search(&dir, &q)).await;
    match out {
        Ok(Ok(r)) => json_ok(format!(
            "{{\"rows\":{},\"stats\":{{\"blocks_total\":{},\"blocks_scanned\":{},\
             \"rows_scanned\":{},\"rows_matched\":{}}}}}",
            r.json,
            r.stats.blocks_total,
            r.stats.blocks_scanned,
            r.stats.rows_scanned,
            r.stats.rows_matched
        )),
        Ok(Err(e)) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("query task panicked: {e}"),
        )
            .into_response(),
    }
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
/// }
/// ```
///
/// A term names its target with `attr` or `field` and its operator with the
/// other key, so `{"attr": "x", "eq": 1}` rather than
/// `{"target": ..., "op": ..., "value": ...}`. Two keys instead of three, and
/// the shape reads as the thing it means — which matters more than usual here,
/// because a large share of these documents will be written by a model that has
/// seen the schema once.
pub fn parse_search(text: &str, now: i64) -> Result<Search, String> {
    let docs = YamlLoader::load_from_str(text).map_err(|e| format!("not valid KYAML: {e}"))?;
    let doc = docs.first().ok_or("empty query")?;

    let signal = match doc["signal"].as_str() {
        Some(s) => Signal::parse(s).ok_or(format!("unknown signal {s:?}"))?,
        None => Signal::Logs,
    };

    // Default window is the last hour. A query with no bounds at all would scan
    // the whole retention period, which is the one mistake that turns a fast
    // engine into a slow one, and it is the mistake an agent makes first.
    let from = time_field(&doc["from"], now, now - 3_600_000_000_000)?;
    let to = time_field(&doc["to"], now, now)?;
    if from > to {
        return Err(format!("from ({from}) is after to ({to})"));
    }

    let limit = match &doc["limit"] {
        Yaml::BadValue | Yaml::Null => 100,
        Yaml::Integer(n) if *n > 0 => (*n as usize).min(MAX_LIMIT),
        other => return Err(format!("limit must be a positive integer, got {other:?}")),
    };

    let terms = match &doc["where"] {
        Yaml::BadValue | Yaml::Null => Vec::new(),
        Yaml::Array(a) => a.iter().map(parse_term).collect::<Result<_, _>>()?,
        _ => return Err("`where` must be a list of terms".into()),
    };

    Ok(Search {
        signal,
        from,
        to,
        terms,
        limit,
    })
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
                ))
            }
            "field" => {
                target = Some(Target::Field(
                    v.as_str().ok_or("`field` must be a string")?.to_owned(),
                ))
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
        // A whole document that is not a mapping should not panic on indexing.
        assert!(parse_search("[1,2,3]", 0).is_ok());
    }

    #[test]
    fn limit_is_capped_rather_than_refused() {
        let q = parse_search(r#"{"limit":9999999}"#, 0).unwrap();
        assert_eq!(q.limit, MAX_LIMIT);
    }
}
