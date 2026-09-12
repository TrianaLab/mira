//! Static-rule alerting: a KYAML document in, webhooks out.
//!
//! # A rule is a query, not a new language
//!
//! Every alerting system this replaces ships a second language — PromQL, a
//! DSL, a builder UI — and that language is the reason "why did this page?" is
//! hard to answer: the thing that fired is not the thing you can look at. Here
//! a rule *embeds a `/api/v1/query` document*, verbatim. The operator builds
//! the query in the UI, presses nothing, pastes it into the rules file, and the
//! link in the resulting page opens the same query in the same UI. There is one
//! filter grammar in this product and this is not a second one.
//!
//! # Two metrics, and why that is enough
//!
//! `count` is how many records matched. `ratio` is that over a second query's
//! count. Between them they express both of the rules everyone actually writes:
//!
//! * *error rate above 5% over a minute* — numerator `status_code = 2`,
//!   denominator the same filter without it, `ratio > 5%`.
//! * *p95 latency above 500 ms* — numerator adds `duration_nano > 500000000`,
//!   same denominator, `ratio > 5%`.
//!
//! The second one is not an approximation of a quantile, it *is* the quantile:
//! `p95(d) > T` and `|{d > T}| / |d| > 0.05` are the same statement. That
//! identity is why there is no digest in this file and no `p95(...)` in the
//! grammar — a percentile threshold was always a counting question wearing a
//! statistics hat, and Mira can already count exactly.
//!
//! # Counting is free
//!
//! [`query::search_open`] with `limit: 0` returns no rows and an exact
//! `rows_matched`: the early exit needs a held hit to fire and there are none,
//! so every matching block is scanned, and the per-wave trim throws the hits
//! away before they accumulate. So an evaluation costs one scan and allocates
//! nothing per match — the same code path, the same zone maps and the same
//! bloom filters the UI's query uses, which is also why a rule cannot drift
//! from what the operator sees.
//!
//! # One replica evaluates
//!
//! Principle 4 is no coordination state, and N replicas reading the same block
//! directory would each fire the same rule N times. There is no lease and no
//! leader here: alerting is off unless `alerts.rules` names a file, so the
//! deployment enables it on one replica and that is the whole mechanism. State
//! — which rules are firing, and since when — is in memory, so a restart
//! re-evaluates from scratch and re-pages anything still breaching.
//!
//! ponytail: dedup is the operator pointing one replica at the rules file. A
//! lease file in the block directory is the upgrade path if that stops being
//! enough, and it stays inside "the block directory is the manifest".
//!
//! # Why HTTPS is a Cargo feature
//!
//! Slack, Discord and PagerDuty are all HTTPS, and in-process TLS costs eleven
//! crates against a tree of 117 — `ring` among them, which would be the second
//! C dependency in a binary whose README states it has one. Both of those are
//! published product properties (section 11), so the default build posts over HTTP and
//! refuses an `https://` target at load time, naming the feature. Build with
//! `--features webhook-tls` and it dials TLS directly.

use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use mira_core::json::Json;
use mira_core::query::{self, Op, Search, Signal, Target, Value};
use yaml_rust2::Yaml;

use crate::api::{self, Api};
use crate::config;

/// What the whole rules file parsed to.
pub struct Rules {
    /// How often every rule is evaluated.
    pub every: Duration,
    /// Prefix for the links in a notification, e.g. `https://mira.example.com`.
    /// Empty means the payloads carry no link — an alert with a link to
    /// `http://0.0.0.0:4318` is worse than one with none.
    pub link_base: String,
    pub rules: Vec<Rule>,
    pub targets: Vec<Target_>,
}

pub struct Rule {
    pub name: String,
    /// The numerator. Window and `limit` are set per evaluation.
    query: Search,
    /// The denominator, for `ratio`. Absent for `count`.
    of: Option<Search>,
    over: Duration,
    metric: Metric,
    cmp: Cmp,
    threshold: f64,
    /// How long the comparison must hold before the rule fires. Zero fires on
    /// the first breach.
    hold: Duration,
    severity: String,
    /// Indices into [`Rules::targets`], resolved at load so a typo in a rule's
    /// `notify` is a startup error rather than a page that never arrives.
    notify: Vec<usize>,
}

/// A webhook endpoint.
///
/// Deliberately not `Debug`, and neither is anything holding one: a Slack
/// webhook URL *is* the credential and so is PagerDuty's routing key, so the
/// derive that makes them one careless `{:?}` away from a log line is the one
/// thing this type must not have.
pub struct Target_ {
    pub name: String,
    url: String,
    format: Format,
    /// PagerDuty's Events v2 routing key. Ignored by the other formats.
    key: String,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Format {
    Slack,
    Discord,
    Pagerduty,
    /// Mira's own alert object, for anything else.
    Json,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Metric {
    Count,
    Ratio,
}

#[derive(Clone, Copy)]
enum Cmp {
    Gt,
    Gte,
    Lt,
    Lte,
}

impl Cmp {
    fn holds(self, v: f64, t: f64) -> bool {
        match self {
            Cmp::Gt => v > t,
            Cmp::Gte => v >= t,
            Cmp::Lt => v < t,
            Cmp::Lte => v <= t,
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Cmp::Gt => ">",
            Cmp::Gte => ">=",
            Cmp::Lt => "<",
            Cmp::Lte => "<=",
        }
    }
}

/// What one rule is currently doing. Reported verbatim by `/api/v1/alerts`.
#[derive(Default, Clone)]
pub struct State {
    /// When the comparison first held, in wall nanoseconds. Cleared the moment
    /// it stops holding, which is what makes `hold` a *sustained* breach rather
    /// than a count of breaching evaluations.
    since: Option<i64>,
    firing: Option<i64>,
    value: f64,
    matched: usize,
    total: Option<usize>,
    /// The last evaluation's failure, if it failed. A rule whose query is
    /// unanswerable is not a quiet rule.
    error: Option<String>,
    at: i64,
}

impl State {
    /// Advance the state machine one evaluation, and say whether that is a
    /// notification: `Some(true)` fired, `Some(false)` resolved, `None` nothing
    /// changed.
    ///
    /// A method rather than a `match` inside [`Engine::tick`] because the only
    /// interesting thing in this file has a clock in it, and a test that drives
    /// the clock has to drive *this* — a test carrying its own copy of these
    /// arms would pass while the engine did something else, which is how the
    /// last round of bugs in this repository survived its unit tests.
    fn advance(&mut self, breaching: bool, now: i64, hold: i64) -> Option<bool> {
        match (breaching, self.since, self.firing) {
            (false, _, Some(_)) => {
                (self.since, self.firing) = (None, None);
                Some(false)
            }
            (false, _, None) => {
                self.since = None;
                None
            }
            (true, None, _) => {
                self.since = Some(now);
                // A rule with no `for` fires on the evaluation that first
                // breached, not on the next one.
                (hold == 0).then(|| {
                    self.firing = Some(now);
                    true
                })
            }
            (true, Some(began), None) if now - began >= hold => {
                self.firing = Some(now);
                Some(true)
            }
            (true, Some(_), _) => None,
        }
    }

    fn phase(&self) -> &'static str {
        match (self.firing.is_some(), self.since.is_some()) {
            (true, _) => "firing",
            (_, true) => "pending",
            _ => "ok",
        }
    }
}

// ---------------------------------------------------------------- parsing

impl Rules {
    /// No `alerts.rules` in the config. Still an [`Engine`], so the endpoint,
    /// the TUI pane and the MCP tool all answer "no rules" rather than 404.
    pub fn off() -> Rules {
        Rules {
            every: Duration::from_secs(15),
            link_base: String::new(),
            rules: Vec::new(),
            targets: Vec::new(),
        }
    }

    pub fn load(path: &Path) -> Result<Rules, String> {
        let text = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
        Rules::parse(&text).map_err(|e| format!("{}: {e}", path.display()))
    }

    pub fn parse(text: &str) -> Result<Rules, String> {
        let doc = api::parse(text)?;
        api::known(&doc, &["every", "link_base", "notify", "rules"])?;
        let every = match doc["every"].as_str() {
            Some(s) => config::duration(s).map_err(|e| format!("every: {e}"))?,
            None => Duration::from_secs(15),
        };
        let link_base = match &doc["link_base"] {
            Yaml::BadValue | Yaml::Null => String::new(),
            y => y
                .as_str()
                .ok_or("link_base must be a quoted string")?
                .trim_end_matches('/')
                .to_owned(),
        };
        let targets = match &doc["notify"] {
            Yaml::BadValue | Yaml::Null => Vec::new(),
            Yaml::Array(a) => a.iter().map(target).collect::<Result<Vec<_>, _>>()?,
            _ => return Err("`notify` must be a list of webhook targets".into()),
        };
        let rules = match &doc["rules"] {
            Yaml::Array(a) => a
                .iter()
                .map(|y| rule(y, &targets))
                .collect::<Result<Vec<_>, _>>()?,
            _ => return Err("`rules` must be a list of rules".into()),
        };
        // A name is the dedup key in every payload format below and the handle
        // an operator silences by, so two rules sharing one is not a cosmetic
        // problem — it is two alerts that cancel each other's PagerDuty
        // incident.
        for (i, r) in rules.iter().enumerate() {
            if rules[..i].iter().any(|o| o.name == r.name) {
                return Err(format!("two rules named {:?}", r.name));
            }
        }
        Ok(Rules {
            every,
            link_base,
            rules,
            targets,
        })
    }
}

fn target(y: &Yaml) -> Result<Target_, String> {
    api::known(y, &["name", "url", "format", "key"])?;
    let name = y["name"]
        .as_str()
        .ok_or("a notify target needs a quoted `name`")?
        .to_owned();
    let url = y["url"]
        .as_str()
        .ok_or_else(|| format!("notify {name:?}: needs a quoted `url`"))?
        .to_owned();
    let format = match y["format"].as_str().unwrap_or("json") {
        "slack" => Format::Slack,
        "discord" => Format::Discord,
        "pagerduty" => Format::Pagerduty,
        "json" => Format::Json,
        other => {
            return Err(format!(
                "notify {name:?}: unknown format {other:?}; expected slack discord pagerduty json"
            ));
        }
    };
    if url.starts_with("https://") && !cfg!(feature = "webhook-tls") {
        // Refused here rather than at the first page, because the first page is
        // exactly when nobody is reading logs.
        return Err(format!(
            "notify {name:?}: this build posts over HTTP only. Rebuild with \
             `--features webhook-tls` for a direct https:// target, or point it \
             at a local egress proxy."
        ));
    }
    if !url.starts_with("http://") && !url.starts_with("https://") {
        return Err(format!("notify {name:?}: url must be http:// or https://"));
    }
    if format == Format::Pagerduty && y["key"].as_str().unwrap_or_default().is_empty() {
        return Err(format!(
            "notify {name:?}: pagerduty needs `key`, the Events v2 routing key"
        ));
    }
    Ok(Target_ {
        name,
        url,
        format,
        key: y["key"].as_str().unwrap_or_default().to_owned(),
    })
}

fn rule(y: &Yaml, targets: &[Target_]) -> Result<Rule, String> {
    api::known(
        y,
        &[
            "name", "query", "of", "over", "when", "for", "severity", "notify",
        ],
    )?;
    let name = y["name"]
        .as_str()
        .ok_or("a rule needs a quoted `name`")?
        .to_owned();
    let at = |e: String| format!("rule {name:?}: {e}");
    let query = windowless(&y["query"], "query").map_err(at)?;
    let of = match &y["of"] {
        Yaml::BadValue | Yaml::Null => None,
        d => Some(windowless(d, "of").map_err(at)?),
    };
    let over = config::duration(y["over"].as_str().unwrap_or("1m")).map_err(&at)?;
    let hold = config::duration(y["for"].as_str().unwrap_or("0s")).map_err(&at)?;
    let (metric, cmp, threshold) = when(y["when"].as_str().unwrap_or_default()).map_err(at)?;
    if metric == Metric::Ratio && of.is_none() {
        return Err(at("`ratio` needs `of`, the denominator query".into()));
    }
    if metric == Metric::Count && of.is_some() {
        return Err(at(
            "`of` is the denominator of a `ratio`; `count` has none".into()
        ));
    }
    let notify = match &y["notify"] {
        // A rule that names no target still evaluates and still shows up in
        // `/api/v1/alerts` and the TUI. That is the useful default for writing
        // a rule you do not yet trust enough to page on.
        Yaml::BadValue | Yaml::Null => Vec::new(),
        Yaml::Array(a) => a
            .iter()
            .map(|n| {
                let n = n
                    .as_str()
                    .ok_or_else(|| at("notify names are strings".into()))?;
                targets
                    .iter()
                    .position(|t| t.name == n)
                    .ok_or_else(|| at(format!("notify {n:?} is not a target in `notify`")))
            })
            .collect::<Result<Vec<_>, _>>()?,
        _ => return Err(at("`notify` must be a list of target names".into())),
    };
    Ok(Rule {
        name,
        query,
        of,
        over,
        metric,
        cmp,
        threshold,
        hold,
        severity: y["severity"].as_str().unwrap_or("warning").to_owned(),
        notify,
    })
}

/// A rule's embedded query document, with the four keys the engine owns
/// refused rather than ignored.
///
/// `over` is the window and `limit` is always zero, so a `from` in the document
/// would be silently overwritten every tick — the exact class of mistake
/// `known` exists to catch one level up.
fn windowless(doc: &Yaml, field: &str) -> Result<Search, String> {
    if doc.is_badvalue() || doc.is_null() {
        return Err(format!("`{field}` is required and is a query document"));
    }
    for k in ["from", "to", "limit", "after"] {
        if !doc[k].is_badvalue() {
            return Err(format!(
                "{field}: `{k}` is the engine's; the window is `over` and the \
                 limit is always zero because this counts rather than reads"
            ));
        }
    }
    let mut s = api::search_doc(doc, 0)?;
    s.limit = 0;
    Ok(s)
}

/// `count > 100`, `ratio >= 5%`, `count < 1`.
///
/// Split on the operator rather than on whitespace: `ratio>0.05` is what
/// someone types and refusing it teaches nothing.
fn when(s: &str) -> Result<(Metric, Cmp, f64), String> {
    let bad = || {
        format!(
            "when: expected `count <op> <number>` or `ratio <op> <number>`, \
             op one of > >= < <=, got {s:?}"
        )
    };
    // Longest first, so `>=` is not read as `>` followed by junk.
    let (cmp, at) = [
        (Cmp::Gte, ">="),
        (Cmp::Lte, "<="),
        (Cmp::Gt, ">"),
        (Cmp::Lt, "<"),
    ]
    .into_iter()
    .find_map(|(c, sym)| s.find(sym).map(|i| (c, (i, sym.len()))))
    .ok_or_else(bad)?;
    let metric = match s[..at.0].trim() {
        "count" => Metric::Count,
        "ratio" => Metric::Ratio,
        _ => return Err(bad()),
    };
    let rhs = s[at.0 + at.1..].trim();
    // `5%` is how a human writes an error-rate threshold and `0.05` is what it
    // compares against. Accepting only one of them is a footgun either way.
    let (num, scale) = match rhs.strip_suffix('%') {
        Some(n) => (n.trim(), 0.01),
        None => (rhs, 1.0),
    };
    let v: f64 = num.parse().map_err(|_| bad())?;
    Ok((metric, cmp, v * scale))
}

// ------------------------------------------------------------- evaluation

pub struct Engine {
    pub rules: Rules,
    state: Mutex<Vec<State>>,
}

impl Default for Engine {
    fn default() -> Engine {
        Engine::new(Rules::off())
    }
}

impl Engine {
    pub fn new(rules: Rules) -> Engine {
        let state = Mutex::new(vec![State::default(); rules.rules.len()]);
        Engine { rules, state }
    }

    /// Evaluate every rule once and dispatch whatever changed.
    pub async fn tick(&self, api: &Api) {
        let now = api::now_nanos();
        for (i, r) in self.rules.rules.iter().enumerate() {
            let (value, matched, total, error) = match count_pair(api, r, now).await {
                Ok(v) => v,
                Err(e) => {
                    let mut st = self.state.lock().expect("alert state");
                    st[i].error = Some(e.clone());
                    st[i].at = now;
                    tracing::warn!(rule = %r.name, error = %e, "alert rule failed");
                    continue;
                }
            };
            let breaching = r.cmp.holds(value, r.threshold);

            // Lock, decide, unlock. The dispatch below awaits, and a `MutexGuard`
            // held across an await is both a deadlock waiting for a second
            // evaluator and a `!Send` future.
            let event = {
                let mut st = self.state.lock().expect("alert state");
                let s = &mut st[i];
                (s.value, s.matched, s.total, s.error, s.at) = (value, matched, total, error, now);
                s.advance(breaching, now, r.hold.as_nanos() as i64)
                    .map(|firing| (firing, s.clone()))
            };
            if let Some((firing, snapshot)) = event {
                self.dispatch(r, &snapshot, firing).await;
            }
        }
    }

    async fn dispatch(&self, r: &Rule, s: &State, firing: bool) {
        tracing::info!(
            rule = %r.name, severity = %r.severity, value = s.value,
            state = if firing { "firing" } else { "resolved" },
            "alert"
        );
        let link = link(&self.rules.link_base, r);
        for &t in &r.notify {
            let t = &self.rules.targets[t];
            let body = payload(t, r, s, firing, &link);
            if let Err(e) = post(&t.url, body).await {
                tracing::warn!(rule = %r.name, target = %t.name, error = %e, "webhook failed");
            }
        }
    }

    /// Every rule's current state, as `/api/v1/alerts` returns it.
    pub fn json(&self) -> String {
        let st = self.state.lock().expect("alert state");
        let mut j = Json::new();
        j.obj(|j| {
            j.key("alerts");
            j.arr(|j| {
                for (r, s) in self.rules.rules.iter().zip(st.iter()) {
                    j.obj(|j| {
                        j.key("name");
                        j.str(&r.name);
                        j.key("state");
                        j.str(s.phase());
                        j.key("severity");
                        j.str(&r.severity);
                        j.key("metric");
                        j.str(match r.metric {
                            Metric::Count => "count",
                            Metric::Ratio => "ratio",
                        });
                        j.key("op");
                        j.str(r.cmp.as_str());
                        j.key("threshold");
                        j.f64(r.threshold);
                        j.key("value");
                        j.f64(s.value);
                        j.key("matched");
                        j.u64(s.matched as u64);
                        j.key("total");
                        match s.total {
                            Some(t) => j.u64(t as u64),
                            None => j.null(),
                        }
                        j.key("over_nano");
                        j.u64_str(r.over.as_nanos() as u64);
                        j.key("for_nano");
                        j.u64_str(r.hold.as_nanos() as u64);
                        j.key("since");
                        match s.since {
                            Some(t) => j.i64_str(t),
                            None => j.null(),
                        }
                        j.key("firing_since");
                        match s.firing {
                            Some(t) => j.i64_str(t),
                            None => j.null(),
                        }
                        j.key("evaluated_at");
                        j.i64_str(s.at);
                        j.key("signal");
                        j.str(signal_name(r));
                        // The predicate, not just a link to it: a reader with no
                        // browser — the TUI, an agent — needs the terms.
                        j.key("filter");
                        j.str(&filter_of(r));
                        j.key("link");
                        j.str(&link(&self.rules.link_base, r));
                        j.key("error");
                        match &s.error {
                            Some(e) => j.str(e),
                            None => j.null(),
                        }
                    });
                }
            });
            j.key("every_nano");
            j.u64_str(self.rules.every.as_nanos() as u64);
        });
        j.into_string()
    }
}

/// Run a rule's queries and reduce them to one number.
///
/// Two scans for a ratio, not one over a superset filtered twice: the terms are
/// AND-ed and the denominator is a different conjunction, so there is no single
/// scan that answers both, and two exact counts beat one estimate.
async fn count_pair(
    api: &Api,
    r: &Rule,
    now: i64,
) -> Result<(f64, usize, Option<usize>, Option<String>), String> {
    let from = now - r.over.as_nanos() as i64;
    let matched = count(api, &r.query, from, now).await?;
    let total = match &r.of {
        Some(q) => Some(count(api, q, from, now).await?),
        None => None,
    };
    let value = match (r.metric, total) {
        (Metric::Count, _) => matched as f64,
        // Zero traffic is not a 100% error rate, and paging as though it were
        // is how an alert wakes someone up for a deployment that is simply
        // idle. A `count` rule is the way to alert on the absence of traffic.
        (Metric::Ratio, Some(0)) | (Metric::Ratio, None) => 0.0,
        (Metric::Ratio, Some(t)) => matched as f64 / t as f64,
    };
    Ok((value, matched, total, None))
}

async fn count(api: &Api, q: &Search, from: i64, to: i64) -> Result<usize, String> {
    let mut q = q.clone();
    (q.from, q.to, q.limit) = (from, to, 0);
    let dir = api.data_dir.clone();
    let open = api.open(q.signal.dir()).await;
    // Same reason every handler in `api.rs` does this: an mmap page fault
    // stalls the OS thread it lands on, and the evaluator shares a runtime with
    // the ingest listeners.
    tokio::task::spawn_blocking(move || query::search_open(&dir, &q, &open))
        .await
        .map_err(|e| e.to_string())?
        .map(|r| r.stats.rows_matched)
        .map_err(|e| e.to_string())
}

// ----------------------------------------------------------------- links

/// The UI URL that shows the rows this rule counted.
///
/// The UI keeps all of its view state in the location hash (`route.svelte.js`),
/// which is what makes this possible at all: there is no saved view to create
/// and no id to store, so a link is a pure function of the rule. The `q`
/// grammar is the UI's, so the terms are re-spelled here — twenty lines against
/// a page that lands the on-call on the exact rows, rather than on a home
/// screen and a re-derivation of what fired at 3am.
fn link(base: &str, r: &Rule) -> String {
    if base.is_empty() {
        return String::new();
    }
    let range = format!("-{}s", r.over.as_secs().max(1));
    format!(
        "{base}/#/{}?q={}&range={range}",
        signal_name(r),
        urlencode(&filter_of(r))
    )
}

fn signal_name(r: &Rule) -> &'static str {
    match r.query.signal {
        Signal::Logs => "logs",
        Signal::Traces => "traces",
    }
}

/// The rule's `where` terms, spelled in the filter-bar grammar both UIs read.
///
/// Split out of [`link`] because it is the half that is useful without a
/// `link_base`: the TUI has no URL to open, and an agent asked "why did this
/// fire" wants the predicate rather than a link it cannot click. Both surfaces
/// paste this straight into a filter box, so the alert and the query it came
/// from cannot drift into two different spellings.
fn filter_of(r: &Rule) -> String {
    let q: Vec<String> = r
        .query
        .terms
        .iter()
        .map(|t| {
            let (kind, key) = match &t.target {
                Target::Field(f) => ("field", f.as_str()),
                Target::Attr(a) => ("attr", a.as_str()),
            };
            // A non-string scalar is its own text; only a string can carry the
            // space that the UI's tokeniser would split on.
            let v = match &t.value {
                Value::Str(s) => s.clone(),
                Value::Int(i) => i.to_string(),
                Value::Double(d) => d.to_string(),
                Value::Bool(b) => b.to_string(),
            };
            let v = if v.contains(' ') || v.is_empty() {
                format!("\"{}\"", v.replace('"', ""))
            } else {
                v
            };
            format!("{kind}:{key}{}{v}", op_symbol(t.op))
        })
        .collect();
    q.join(" ")
}

fn op_symbol(op: Op) -> &'static str {
    match op {
        Op::Eq => "=",
        Op::Ne => "!=",
        Op::Lt => "<",
        Op::Lte => "<=",
        Op::Gt => ">",
        Op::Gte => ">=",
        Op::Contains => "~",
    }
}

/// Percent-encode everything outside the unreserved set.
///
/// Not a general URL encoder and not trying to be: this escapes a query-string
/// *value*, so the conservative set is correct and the aggressive one is safe.
fn urlencode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char);
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

// -------------------------------------------------------------- dispatch

/// One line of prose per alert, shared by every format that takes prose.
fn summary(r: &Rule, s: &State, firing: bool) -> String {
    let value = match r.metric {
        Metric::Ratio => format!("{:.2}%", s.value * 100.0),
        Metric::Count => format!("{:.0}", s.value),
    };
    let threshold = match r.metric {
        Metric::Ratio => format!("{:.2}%", r.threshold * 100.0),
        Metric::Count => format!("{:.0}", r.threshold),
    };
    let head = if firing { "FIRING" } else { "RESOLVED" };
    let over = human(r.over);
    match (r.metric, s.total) {
        (Metric::Ratio, Some(t)) => format!(
            "[{head}] {} — {} {} {} over {over} ({} of {t} records)",
            r.name,
            value,
            r.cmp.as_str(),
            threshold,
            s.matched
        ),
        _ => format!(
            "[{head}] {} — {} records {} {} over {over}",
            r.name,
            value,
            r.cmp.as_str(),
            threshold
        ),
    }
}

fn human(d: Duration) -> String {
    let s = d.as_secs();
    match s {
        0 => "0s".into(),
        s if s % 86_400 == 0 => format!("{}d", s / 86_400),
        s if s % 3_600 == 0 => format!("{}h", s / 3_600),
        s if s % 60 == 0 => format!("{}m", s / 60),
        s => format!("{s}s"),
    }
}

fn payload(t: &Target_, r: &Rule, s: &State, firing: bool, link: &str) -> String {
    let text = summary(r, s, firing);
    let mut j = Json::new();
    match t.format {
        Format::Slack => j.obj(|j| {
            j.key("text");
            // Slack's mrkdwn link form. Plain text with a bare URL renders too,
            // so a target misconfigured as slack is ugly rather than broken.
            j.str(&match link.is_empty() {
                true => text.clone(),
                false => format!("{text}\n<{link}|open in Mira>"),
            });
        }),
        Format::Discord => j.obj(|j| {
            j.key("content");
            j.str(&match link.is_empty() {
                true => text.clone(),
                false => format!("{text}\n{link}"),
            });
        }),
        Format::Pagerduty => j.obj(|j| {
            j.key("routing_key");
            j.str(&t.key);
            j.key("event_action");
            j.str(if firing { "trigger" } else { "resolve" });
            // The rule name, so the resolve closes the incident the trigger
            // opened. This is the reason two rules may not share a name.
            j.key("dedup_key");
            j.str(&r.name);
            j.key("payload");
            j.obj(|j| {
                j.key("summary");
                j.str(&text);
                j.key("severity");
                // Events v2 takes exactly these four. Anything else is a 400,
                // so an unrecognised severity degrades to `warning` rather than
                // losing the page.
                j.str(match r.severity.as_str() {
                    s @ ("critical" | "error" | "warning" | "info") => s,
                    _ => "warning",
                });
                j.key("source");
                j.str("mira");
            });
            if !link.is_empty() {
                j.key("links");
                j.arr(|j| {
                    j.obj(|j| {
                        j.key("href");
                        j.str(link);
                        j.key("text");
                        j.str("open in Mira");
                    });
                });
            }
        }),
        Format::Json => j.raw(&alert_json(r, s, firing, link)),
    }
    j.into_string()
}

fn alert_json(r: &Rule, s: &State, firing: bool, link: &str) -> String {
    let mut j = Json::new();
    j.obj(|j| {
        j.key("rule");
        j.str(&r.name);
        j.key("state");
        j.str(if firing { "firing" } else { "resolved" });
        j.key("severity");
        j.str(&r.severity);
        j.key("summary");
        j.str(&summary(r, s, firing));
        j.key("value");
        j.f64(s.value);
        j.key("threshold");
        j.f64(r.threshold);
        j.key("matched");
        j.u64(s.matched as u64);
        j.key("total");
        match s.total {
            Some(t) => j.u64(t as u64),
            None => j.null(),
        }
        j.key("over_nano");
        j.u64_str(r.over.as_nanos() as u64);
        j.key("at");
        j.i64_str(s.at);
        j.key("link");
        j.str(link);
    });
    j.into_string()
}

/// POST a JSON body, with a deadline.
///
/// No retry. A webhook that is down stays down for longer than the evaluation
/// period, so a retry loop turns one missed page into a queue of stale ones —
/// and the state machine already re-pages on the next transition. The failure
/// is logged, and `/api/v1/alerts` carries it.
async fn post(url: &str, body: String) -> Result<(), String> {
    let req = hyper::Request::builder()
        .method(hyper::Method::POST)
        .uri(url)
        .header(hyper::header::CONTENT_TYPE, "application/json")
        .body(Full::new(Bytes::from(body)))
        .map_err(|e| e.to_string())?;
    let fut = client().request(req);
    let resp = tokio::time::timeout(WEBHOOK_TIMEOUT, fut)
        .await
        .map_err(|_| format!("no response in {}", human(WEBHOOK_TIMEOUT)))?
        .map_err(|e| e.to_string())?;
    let status = resp.status();
    // Drained rather than dropped: an undrained body leaves the connection
    // unusable, so the pool opens a new one for every alert.
    let _ = resp.into_body().collect().await;
    match status.is_success() {
        true => Ok(()),
        false => Err(format!("HTTP {}", status.as_u16())),
    }
}

const WEBHOOK_TIMEOUT: Duration = Duration::from_secs(10);

type Client = hyper_util::client::legacy::Client<Connector, Full<Bytes>>;

#[cfg(not(feature = "webhook-tls"))]
type Connector = hyper_util::client::legacy::connect::HttpConnector;

#[cfg(feature = "webhook-tls")]
type Connector = hyper_rustls::HttpsConnector<hyper_util::client::legacy::connect::HttpConnector>;

/// One pooled client for the process. Alerts are rare and bursty, and a fresh
/// TCP (and TLS) handshake per page is the whole latency of a page.
fn client() -> &'static Client {
    static C: std::sync::OnceLock<Client> = std::sync::OnceLock::new();
    C.get_or_init(|| {
        let b = hyper_util::client::legacy::Client::builder(hyper_util::rt::TokioExecutor::new());
        #[cfg(not(feature = "webhook-tls"))]
        {
            b.build_http()
        }
        #[cfg(feature = "webhook-tls")]
        {
            b.build(
                hyper_rustls::HttpsConnectorBuilder::new()
                    .with_webpki_roots()
                    .https_or_http()
                    .enable_http1()
                    .build(),
            )
        }
    })
}

pub fn router(api: Api) -> axum::Router {
    axum::Router::new()
        .route("/api/v1/alerts", axum::routing::get(handler))
        .with_state(api)
}

/// Every rule this node evaluates and what it is currently doing.
///
/// An empty list means this node has no rules file — alerting is off here, not
/// all clear. `link` on a firing rule opens the records behind it.
async fn handler(axum::extract::State(api): axum::extract::State<Api>) -> axum::response::Response {
    use axum::response::IntoResponse;
    (
        [(axum::http::header::CONTENT_TYPE, "application/json")],
        api.alerts.json(),
    )
        .into_response()
}

/// Start the evaluator, if this deployment has rules.
pub fn spawn(api: Api) {
    let engine = Arc::clone(&api.alerts);
    if engine.rules.rules.is_empty() {
        return;
    }
    tracing::info!(
        rules = engine.rules.rules.len(),
        targets = engine.rules.targets.len(),
        every = %human(engine.rules.every),
        "alerting"
    );
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(engine.rules.every);
        // A slow scan must not queue up ticks and then run them back to back:
        // that turns a struggling evaluator into a busy loop over the same
        // blocks.
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tick.tick().await;
            engine.tick(&api).await;
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    const DOC: &str = r#"{
      "every": "5s",
      "link_base": "https://mira.example.com/",
      "notify": [ { "name": "oncall", "url": "http://127.0.0.1:9/hook", "format": "slack" } ],
      "rules": [
        { "name": "checkout-errors",
          "over": "1m", "for": "2m", "severity": "critical", "notify": ["oncall"],
          "query": { "signal": "traces", "where": [
             { "attr": "service.name", "eq": "checkout" },
             { "field": "status_code", "eq": 2 } ] },
          "of":    { "signal": "traces", "where": [
             { "attr": "service.name", "eq": "checkout" } ] },
          "when":  "ratio > 5%" },
        { "name": "any-log", "query": { "signal": "logs" }, "when": "count>=1" }
      ]
    }"#;

    /// The file `make demo` loads and `docs/config.md` points at.
    ///
    /// A documented example that does not parse is worse than no example, and
    /// this one is also the only place the schema is written out in full — so
    /// it is checked at compile time, against the parser, rather than by
    /// whoever next runs the demo.
    #[test]
    fn the_shipped_example_rules_file_parses() {
        let r = Rules::parse(include_str!("../../../docs/e2e/alerts.kyaml")).expect("alerts.kyaml");
        assert_eq!(r.every, Duration::from_secs(15));
        assert_eq!(r.link_base, "http://localhost:4318");
        // No targets: the demo has nothing to POST to, and a rule with no
        // target must still evaluate rather than be quietly dropped.
        assert!(r.targets.is_empty());
        let names: Vec<&str> = r.rules.iter().map(|x| x.name.as_str()).collect();
        assert_eq!(
            names,
            [
                "shop-error-rate",
                "checkout-p95-latency",
                "card-declines",
                "inventory-outage"
            ]
        );
        // The percentile example has to be a ratio rule with both counts, or
        // the comment above it in the file is a lie.
        let p95 = &r.rules[1];
        assert!(matches!(p95.metric, Metric::Ratio));
        assert!(p95.of.is_some());
        assert!((p95.threshold - 0.05).abs() < 1e-12);

        // The spellings both UIs paste into a filter box. Pinned here because
        // they are the contract with a parser in another language: the browser's
        // `parseFilter` has a test over these same four strings, so a change to
        // `filter_of` that the JS tokeniser cannot read fails on this side first.
        let filters: Vec<String> = r.rules.iter().map(filter_of).collect();
        assert_eq!(
            filters,
            [
                "field:status_code=2",
                "attr:service.name=checkout field:duration_nano>250000000",
                "attr:exception.type=payments.CardDeclined",
                "attr:service.name=inventory field:severity_number>=21",
            ]
        );
    }

    #[test]
    fn a_rules_file_parses_to_what_it_says() {
        let r = Rules::parse(DOC).unwrap();
        assert_eq!(r.every, Duration::from_secs(5));
        // The trailing slash is stripped, or every link would have two.
        assert_eq!(r.link_base, "https://mira.example.com");
        assert_eq!(r.rules.len(), 2);
        let a = &r.rules[0];
        assert_eq!(a.threshold, 0.05);
        assert_eq!(a.hold, Duration::from_secs(120));
        assert_eq!(a.notify, vec![0]);
        assert!(a.of.is_some());
        // `limit: 0` is what makes an evaluation a count rather than a read.
        assert_eq!(a.query.limit, 0);
        assert_eq!(r.rules[1].severity, "warning");
    }

    #[test]
    fn a_percentile_threshold_is_a_ratio_threshold() {
        // The module doc's claim, as an executable statement: p95 > 500ms is
        // "more than 5% of spans took longer than 500ms", and the rule that
        // says so is an ordinary ratio rule with a range term.
        let r = Rules::parse(
            r#"{ "rules": [ { "name": "p95", "over": "5m", "when": "ratio > 5%",
                 "query": { "signal": "traces", "where": [
                    { "field": "duration_nano", "gt": 500000000 } ] },
                 "of":    { "signal": "traces" } } ] }"#,
        )
        .unwrap();
        let rule = &r.rules[0];
        assert!(matches!(rule.metric, Metric::Ratio));
        assert!(rule.cmp.holds(0.06, rule.threshold));
        assert!(!rule.cmp.holds(0.04, rule.threshold));
    }

    /// Every operator and every scalar the query grammar has, spelled back out.
    ///
    /// `filter_of` is the one place a rule's predicate is turned into text, and
    /// both UIs paste that text into a filter box and re-parse it. An operator
    /// this function has no symbol for, or a value it quotes wrongly, is a
    /// link that opens rows the rule did not count.
    #[test]
    fn every_operator_and_scalar_survives_the_trip_through_a_filter_box() {
        let r = Rules::parse(
            r#"{ "rules": [ { "name": "all-ops", "over": "1m", "when": "count > 0",
                 "query": { "signal": "logs", "where": [
                    { "attr": "service.name", "ne": "checkout" },
                    { "field": "severity_number", "lt": 17 },
                    { "field": "severity_number", "lte": 16 },
                    { "attr": "http.route", "contains": "/api" },
                    { "attr": "sampling.ratio", "eq": 0.25 },
                    { "attr": "deployment.canary", "eq": true },
                    { "attr": "http.target", "eq": "GET /a b" },
                    { "attr": "empty", "eq": "" } ] } } ] }"#,
        )
        .unwrap();
        assert_eq!(
            filter_of(&r.rules[0]),
            "attr:service.name!=checkout field:severity_number<17 \
             field:severity_number<=16 attr:http.route~/api attr:sampling.ratio=0.25 \
             attr:deployment.canary=true attr:http.target=\"GET /a b\" attr:empty=\"\""
        );
    }

    #[test]
    fn every_way_to_write_a_rule_wrong_is_refused_by_name() {
        // `unwrap_err` would need `Rules: Debug`, and `Rules` holds webhook
        // credentials — see [`Target_`].
        let bad = |doc: &str, want: &str| {
            let e = Rules::parse(doc).err().expect("should not have parsed");
            assert!(e.contains(want), "{e:?} should mention {want:?}");
        };
        bad(
            r#"{ "rules": [ { "name": "a", "query": {}, "when": "count ~ 1" } ] }"#,
            "when",
        );
        bad(
            r#"{ "rules": [ { "name": "a", "query": {}, "when": "p95 > 1" } ] }"#,
            "when",
        );
        bad(
            r#"{ "rules": [ { "name": "a", "query": {}, "when": "ratio > 1" } ] }"#,
            "of",
        );
        bad(
            r#"{ "rules": [ { "name": "a", "query": {}, "of": {}, "when": "count > 1" } ] }"#,
            "denominator",
        );
        // The window is the engine's, and a `from` that looked accepted and was
        // overwritten every tick is the silent failure this refuses.
        bad(
            r#"{ "rules": [ { "name": "a", "query": { "from": "-1h" }, "when": "count > 1" } ] }"#,
            "over",
        );
        bad(
            r#"{ "rules": [ { "name": "a", "when": "count > 1" } ] }"#,
            "required",
        );
        bad(
            r#"{ "rules": [ { "name": "a", "query": {}, "when": "count > 1", "nope": "x" } ] }"#,
            "nope",
        );
        bad(
            r#"{ "rules": [ { "name": "a", "query": {}, "when": "count>1", "notify": ["ghost"] } ] }"#,
            "ghost",
        );
        // A bare name where a list belongs. YAML would happily read it, and
        // reading it as "no targets" is a rule that evaluates and never pages.
        bad(
            r#"{ "notify": [ { "name": "n", "url": "http://x/" } ],
                 "rules": [ { "name": "a", "query": {}, "when": "count>1", "notify": "n" } ] }"#,
            "list of target names",
        );
        bad(
            r#"{ "notify": [ { "name": "n", "url": "http://x/" } ],
                 "rules": [ { "name": "a", "query": {}, "when": "count>1", "notify": [7] } ] }"#,
            "notify names are strings",
        );
        bad(
            r#"{ "rules": [ { "name": "a", "query": {}, "when": "count>1" },
                            { "name": "a", "query": {}, "when": "count>1" } ] }"#,
            "two rules named",
        );
        bad(
            r#"{ "notify": [ { "name": "pd", "url": "http://x/", "format": "pagerduty" } ], "rules": [] }"#,
            "routing key",
        );
        bad(
            r#"{ "notify": [ { "name": "n", "url": "ftp://x/" } ], "rules": [] }"#,
            "http://",
        );
        // The shapes of the document itself, not of a rule inside it. Each one
        // is a plausible typo whose silent reading would be "alerting is off".
        bad(r#"{ "every": "soon", "rules": [] }"#, "every");
        bad(r#"{ "link_base": 4318, "rules": [] }"#, "link_base");
        bad(r#"{ "notify": { "name": "n" }, "rules": [] }"#, "notify");
        bad(r#"{ "rules": { "name": "a" } }"#, "rules");
        bad(r#"{ "rules": [], "alerts": [] }"#, "alerts");
        bad(
            r#"{ "notify": [ { "url": "http://x/" } ], "rules": [] }"#,
            "name",
        );
        bad(r#"{ "notify": [ { "name": "n" } ], "rules": [] }"#, "url");
        bad(
            r#"{ "notify": [ { "name": "n", "url": "http://x/", "format": "email" } ], "rules": [] }"#,
            "email",
        );
        bad(
            r#"{ "notify": [ { "name": "n", "url": "http://x/", "to": "me" } ], "rules": [] }"#,
            "to",
        );
    }

    /// A rules file is read from disk, and both ways that fails say which file.
    ///
    /// The path matters more than it looks: a node refuses to start on a bad
    /// rules file, so this message is the entire diagnosis someone gets from a
    /// container that exited.
    #[test]
    fn a_rules_file_is_loaded_by_path_and_names_the_path_when_it_cannot_be() {
        let dir = std::env::temp_dir().join(format!("mira-rules-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("mkdir");
        let path = dir.join("alerts.kyaml");
        std::fs::write(&path, DOC).expect("write");
        let r = Rules::load(&path).expect("load");
        assert_eq!(r.rules.len(), 2);

        std::fs::write(&path, "{ rules: nope }").expect("write");
        let e = Rules::load(&path).err().expect("should not have parsed");
        assert!(e.contains("alerts.kyaml") && e.contains("rules"), "{e}");

        let missing = dir.join("gone.kyaml");
        let e = Rules::load(&missing).err().expect("should not have opened");
        assert!(e.contains("gone.kyaml"), "{e}");
        std::fs::remove_dir_all(&dir).ok();
    }

    /// The other direction: a threshold that fires when a number gets too low.
    ///
    /// `<` and `<=` exist because the alert nobody writes until the outage is
    /// "traffic stopped" — a rule whose breach is an absence, where every
    /// greater-than rule in the file goes quiet at exactly the wrong moment.
    #[test]
    fn a_rule_can_fire_on_too_little_rather_than_too_much() {
        let r = Rules::parse(
            r#"{ "rules": [
                 { "name": "traffic-gone", "over": "5m", "when": "count < 100",
                   "query": { "signal": "traces" } },
                 { "name": "success-rate", "over": "5m", "when": "ratio <= 99%",
                   "query": { "signal": "traces", "where": [ { "field": "status_code", "eq": 1 } ] },
                   "of":    { "signal": "traces" } } ] }"#,
        )
        .expect("rules");

        let quiet = &r.rules[0];
        assert_eq!(quiet.cmp.as_str(), "<");
        assert!(quiet.cmp.holds(3.0, 100.0));
        assert!(!quiet.cmp.holds(100.0, 100.0));

        let rate = &r.rules[1];
        assert_eq!(rate.cmp.as_str(), "<=");
        assert!((rate.threshold - 0.99).abs() < 1e-12);
        assert!(rate.cmp.holds(0.99, 0.99));
        assert!(!rate.cmp.holds(0.999, 0.99));

        // And the prose an operator is paged with reads the right way round.
        let s = State {
            value: 3.0,
            matched: 3,
            total: None,
            at: 0,
            ..State::default()
        };
        assert_eq!(
            summary(quiet, &s, true),
            "[FIRING] traffic-gone — 3 records < 100 over 5m"
        );
    }

    #[cfg(not(feature = "webhook-tls"))]
    #[test]
    fn an_https_target_is_refused_at_load_by_a_build_that_cannot_dial_it() {
        let e =
            Rules::parse(r#"{ "notify": [ { "name": "s", "url": "https://x/" } ], "rules": [] }"#)
                .err()
                .expect("an https target should not load in this build");
        assert!(e.contains("webhook-tls"), "{e:?}");
    }

    /// The state machine, driven by hand. `for` is the only part of it with a
    /// clock, and the bug it exists to prevent — a rule that fires because it
    /// breached twice with a recovery in between — is invisible without one.
    #[test]
    fn for_needs_a_sustained_breach_not_a_repeated_one() {
        let e = Engine::new(Rules::parse(DOC).unwrap());
        let hold = e.rules.rules[0].hold.as_nanos() as i64;
        let step = |breaching: bool, now: i64| -> (Option<bool>, &'static str) {
            let mut st = e.state.lock().unwrap();
            let s = &mut st[0];
            (s.advance(breaching, now, hold), s.phase())
        };
        const MIN: i64 = 60_000_000_000;
        assert_eq!(step(true, 0), (None, "pending"));
        // Three minutes later, but it recovered in between, so the clock restarts.
        assert_eq!(step(false, MIN), (None, "ok"));
        assert_eq!(step(true, 2 * MIN), (None, "pending"));
        assert_eq!(step(true, 3 * MIN), (None, "pending"));
        assert_eq!(step(true, 4 * MIN), (Some(true), "firing"));
        // Already firing: no second page.
        assert_eq!(step(true, 5 * MIN), (None, "firing"));
        assert_eq!(step(false, 6 * MIN), (Some(false), "ok"));
    }

    #[test]
    fn a_link_lands_on_the_rows_that_fired() {
        let r = Rules::parse(DOC).unwrap();
        let l = link(&r.link_base, &r.rules[0]);
        assert!(l.starts_with("https://mira.example.com/#/traces?q="), "{l}");
        // The UI's own `q` grammar (`api.js`), percent-encoded: an operator
        // clicking this gets the query in the search box, not a home page.
        assert!(l.contains("attr%3Aservice.name%3Dcheckout"), "{l}");
        assert!(l.contains("field%3Astatus_code%3D2"), "{l}");
        assert!(l.ends_with("&range=-60s"), "{l}");
        // No base configured means no link, rather than one pointing at 0.0.0.0.
        assert_eq!(link("", &r.rules[0]), "");
    }

    #[test]
    fn each_format_says_the_same_thing_in_its_own_words() {
        let r = Rules::parse(DOC).unwrap();
        let rule = &r.rules[0];
        let s = State {
            value: 0.12,
            matched: 24,
            total: Some(200),
            ..State::default()
        };
        let link = link(&r.link_base, rule);
        let text = summary(rule, &s, true);
        assert!(text.contains("FIRING"), "{text}");
        assert!(text.contains("12.00%"), "{text}");
        assert!(text.contains("24 of 200"), "{text}");
        assert!(text.contains("over 1m"), "{text}");

        let slack = payload(&r.targets[0], rule, &s, true, &link);
        assert!(slack.starts_with(r#"{"text":"[FIRING]"#), "{slack}");
        assert!(slack.contains("|open in Mira>"), "{slack}");

        let pd = Target_ {
            name: "pd".into(),
            url: "http://x/".into(),
            format: Format::Pagerduty,
            key: "rk".into(),
        };
        let fire = payload(&pd, rule, &s, true, &link);
        assert!(fire.contains(r#""event_action":"trigger""#), "{fire}");
        assert!(fire.contains(r#""dedup_key":"checkout-errors""#), "{fire}");
        assert!(fire.contains(r#""severity":"critical""#), "{fire}");
        let clear = payload(&pd, rule, &s, false, &link);
        assert!(clear.contains(r#""event_action":"resolve""#), "{clear}");
        // Same dedup key both ways, or the resolve opens a second incident.
        assert!(
            clear.contains(r#""dedup_key":"checkout-errors""#),
            "{clear}"
        );

        let raw = Target_ {
            format: Format::Json,
            ..Target_ {
                name: "j".into(),
                url: "http://x/".into(),
                format: Format::Json,
                key: String::new(),
            }
        };
        let j = payload(&raw, rule, &s, true, &link);
        assert!(j.contains(r#""rule":"checkout-errors""#), "{j}");
        assert!(j.contains(r#""state":"firing""#), "{j}");
        assert!(j.contains(r#""total":200"#), "{j}");
        // A `count` rule has no denominator at all, and the raw format says so
        // with `null`. A zero there is a ratio of nothing over nothing to
        // whatever is reading this, which is the one thing it is not.
        let counted = payload(&raw, &r.rules[1], &State::default(), true, "");
        assert!(counted.contains(r#""total":null"#), "{counted}");
        assert!(counted.contains(r#""matched":0"#), "{counted}");

        // Discord has no link markup, so the URL goes on its own line. It is
        // still the same one Slack wraps — a reader comparing two channels must
        // not land on two different sets of rows.
        let dis = Target_ {
            name: "d".into(),
            url: "http://x/".into(),
            format: Format::Discord,
            key: String::new(),
        };
        let d = payload(&dis, rule, &s, true, &link);
        assert!(d.starts_with(r#"{"content":"[FIRING]"#), "{d}");
        assert!(d.contains(&link), "{d}");

        // No `link_base` configured: every format still pages, and none of them
        // emits a dangling separator where the URL would have been.
        for t in [&r.targets[0], &dis] {
            let p = payload(t, rule, &s, true, "");
            assert!(p.ends_with(r#"over 1m (24 of 200 records)"}"#), "{p}");
        }
    }

    #[test]
    fn a_severity_pagerduty_does_not_know_degrades_rather_than_400s() {
        let mut r = Rules::parse(DOC).unwrap();
        r.rules[0].severity = "sev1".into();
        let pd = Target_ {
            name: "pd".into(),
            url: "http://x/".into(),
            format: Format::Pagerduty,
            key: "rk".into(),
        };
        let out = payload(&pd, &r.rules[0], &State::default(), true, "");
        assert!(out.contains(r#""severity":"warning""#), "{out}");
    }

    #[test]
    fn an_idle_service_is_not_a_hundred_percent_error_rate() {
        // 0/0. The arithmetic answer is NaN and the operational answer is "no
        // traffic, no alert"; paging on a deployment that is merely quiet is
        // the classic false positive this avoids.
        let r = Rules::parse(DOC).unwrap();
        let rule = &r.rules[0];
        assert!(!rule.cmp.holds(0.0, rule.threshold));
    }

    #[test]
    fn durations_round_trip_the_way_they_were_written() {
        assert_eq!(human(Duration::from_secs(60)), "1m");
        assert_eq!(human(Duration::from_secs(90)), "90s");
        assert_eq!(human(Duration::from_secs(7200)), "2h");
        assert_eq!(human(Duration::from_secs(86_400)), "1d");
        assert_eq!(human(Duration::ZERO), "0s");
    }

    /// A webhook that answers is not a webhook that accepted.
    ///
    /// Slack retires an incoming-webhook URL with a 403 and PagerDuty rejects a
    /// stale routing key with a 400: in both cases the POST succeeds at every
    /// layer this process controls, and the page is not delivered. Treating a
    /// non-2xx as success is silent — nothing is retried, by design — so the
    /// status is the only evidence, and it has to reach the log line and
    /// `/api/v1/alerts` with the number in it.
    ///
    /// The 204 afterwards is served on the *same* socket, and the server here
    /// accepts exactly one: that is what makes draining the failed response
    /// body a correctness property rather than tidiness. An undrained body is
    /// never returned to the pool, so dropping the `collect` moves the second
    /// page onto a second connection and this test sees it.
    #[tokio::test]
    async fn a_webhook_that_refuses_the_page_is_a_failure_that_names_the_status() {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let url = format!("http://{}/hook", listener.local_addr().expect("addr"));
        let seen = std::thread::spawn(move || {
            let (mut sock, _) = listener.accept().expect("accept");
            // So a client that opens a *second* connection fails this test in
            // ten seconds rather than hanging it forever on a read nobody will
            // answer — or on a write nobody is draining.
            sock.set_read_timeout(Some(Duration::from_secs(10)))
                .expect("timeout");
            sock.set_write_timeout(Some(Duration::from_secs(10)))
                .expect("timeout");
            let mut bodies = Vec::new();
            for i in 0..2 {
                let mut head = Vec::new();
                let mut byte = [0u8; 1];
                while !head.ends_with(b"\r\n\r\n") && sock.read(&mut byte).unwrap_or(0) == 1 {
                    head.push(byte[0]);
                }
                let text = String::from_utf8_lossy(&head).to_lowercase();
                let len: usize = text
                    .split("content-length:")
                    .nth(1)
                    .and_then(|t| t.split("\r\n").next())
                    .and_then(|t| t.trim().parse().ok())
                    .unwrap_or(0);
                let mut body = vec![0u8; len];
                let read = sock.read_exact(&mut body).is_ok();
                bodies.push(match read {
                    true => String::from_utf8_lossy(&body).into_owned(),
                    false => "<nothing arrived on this connection>".into(),
                });
                // A body on the refusal, because that is what a real endpoint
                // sends — an error page, not a word. Big enough that it cannot
                // have arrived alongside the header: a five-byte body is
                // already in the client's read buffer by the time the status
                // is, and a connection like that is reusable whether anyone
                // drained it or not, so a small one would prove nothing.
                const PAGE: usize = 1 << 20;
                let reply: Vec<u8> = match i {
                    0 => {
                        let mut r = format!(
                            "HTTP/1.1 500 Internal Server Error\r\ncontent-length: {PAGE}\r\n\r\n"
                        )
                        .into_bytes();
                        r.extend(std::iter::repeat_n(b'x', PAGE));
                        r
                    }
                    _ => b"HTTP/1.1 204 No Content\r\ncontent-length: 0\r\n\r\n".to_vec(),
                };
                let _ = sock.write_all(&reply);
            }
            listener.set_nonblocking(true).expect("nonblocking");
            (bodies, listener.accept().is_ok())
        });

        assert_eq!(
            post(&url, r#"{"text":"first"}"#.into()).await,
            Err("HTTP 500".to_owned())
        );
        // 204 is a success and plenty of receivers answer with it, so "2xx"
        // rather than "200" is the contract.
        assert_eq!(post(&url, r#"{"text":"second"}"#.into()).await, Ok(()));
        let (bodies, reconnected) = seen.join().expect("receiver");
        assert_eq!(
            bodies,
            [r#"{"text":"first"}"#, r#"{"text":"second"}"#],
            "both bodies arrived intact, down one socket"
        );
        assert!(
            !reconnected,
            "the refusal's body was drained, so the pooled connection survived it"
        );

        // And a URL no request can be built from fails before the socket: the
        // text is the builder's, so neither a connector error nor the ten-second
        // timeout can satisfy this.
        assert_eq!(
            post("http://[bad", "{}".into()).await,
            Err("invalid authority".to_owned())
        );
    }

    #[test]
    fn the_alerts_document_reports_every_rule_including_the_quiet_ones() {
        let e = Engine::new(Rules::parse(DOC).unwrap());
        let j = e.json();
        assert!(j.contains(r#""name":"checkout-errors""#), "{j}");
        assert!(j.contains(r#""state":"ok""#), "{j}");
        assert!(j.contains(r#""name":"any-log""#), "{j}");
        // 64-bit values are strings on the wire (section 7.6) and the two clients read
        // them with a coercion that expects that.
        assert!(j.contains(r#""over_nano":"60000000000""#), "{j}");
        assert!(j.contains(r#""error":null"#), "{j}");
    }
}
