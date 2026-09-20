//! The incident write-up, as a tool.
//!
//! `render_rca` is the ninth MCP tool and the only one that takes more than it
//! gives back: structured fields in, one markdown document out. It is here
//! because the last mile of an incident is not a query. It is the write-up, and
//! an agent left to format that itself produces a different document every
//! time, with the evidence inlined as prose that nobody can re-run.
//!
//! # Two things make this more than a template
//!
//! Every evidence item carries a citation — a trace id or a query document —
//! and [`Rca::render`] cannot be called until every one of them has been re-run
//! against the store. A citation that returns nothing is an error naming it,
//! not a footnote: an RCA whose evidence has aged out of retention reads
//! exactly like one whose evidence was invented, and telling those apart is
//! most of what the document is worth.
//!
//! The other is `prevention`. The alerting rule the write-up proposes is parsed
//! by the same [`crate::alert::rule`] that loads `alerts.kyaml`, so what comes
//! back in the fence is a rule that will load rather than one that looks like
//! it would. That is the fourth reading of principle 2 — self-tuning — reduced
//! to the only mechanism that is honest about it: the incident emits the rule
//! that would have caught it.
//!
//! # It still does not act
//!
//! Nothing in this file changes anything outside Mira, and there is no
//! counterpart to it that does. The remediation section is text for whoever is
//! on call, and [`Rca::render`] says so in the document's own body. Mira
//! supplies evidence; the fixing is done elsewhere, with tools that were built
//! for it (architecture section 14).

use yaml_rust2::Yaml;

use mira_core::json::Json;
use mira_core::query::{Op, Search, Signal, Target, Term, Value};

use crate::api;

/// The citation could not be checked at all, as opposed to checked and empty.
/// Rendered the same way in the error, because both mean "do not publish this".
const UNREADABLE: usize = usize::MAX;

/// A parsed, not-yet-verified write-up.
///
/// Nothing here is rendered until [`verified`](Rca::verified) is true, which is
/// the type-level half of the promise the module header makes.
#[derive(Debug)]
pub(crate) struct Rca {
    title: String,
    from: i64,
    to: i64,
    summary: String,
    impact: Vec<String>,
    /// Sorted by time at parse, because a timeline given out of order is the
    /// one defect in this document that a reader will believe rather than
    /// notice.
    timeline: Vec<(i64, String)>,
    root_cause: String,
    pub(crate) evidence: Vec<Fact>,
    ruled_out: Vec<String>,
    contributing: Vec<String>,
    remediation: Vec<String>,
    verification: Vec<String>,
    /// An `alerts.kyaml` fragment, already through the rule parser.
    prevention: Option<String>,
    pub(crate) emit: bool,
}

/// One cited claim.
#[derive(Debug)]
pub(crate) struct Fact {
    claim: String,
    /// What to re-run. Never absent: an assertion Mira cannot check belongs in
    /// `summary` or `root_cause`, which is where the document puts the parts
    /// that are judgement.
    pub(crate) cite: Search,
    /// How the citation reads in the rendered document.
    shown: String,
    /// What re-running it found. `None` until the verification pass has been
    /// through.
    pub(crate) found: Option<usize>,
}

impl Fact {
    pub(crate) fn seen(&mut self, rows: Option<usize>) {
        self.found = Some(rows.unwrap_or(UNREADABLE));
    }
}

/// `{"trace_id": ...}` as a query.
///
/// Shared with `mcp::trace_search` so that the id a write-up cites and
/// the id `get_trace` accepts cannot drift into two different notions of
/// well-formed. The window is all of retention for the same reason it is there:
/// you look a trace up because you do not know when it happened.
pub(crate) fn trace_query(id: &str, limit: usize) -> Result<Search, String> {
    let id = id.trim();
    if mira_core::query::unhex(id).is_none_or(|b| b.len() != 16) {
        return Err(format!("{id:?} is not a 16-byte hex trace id"));
    }
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
        after: None,
        cursors: false,
    })
}

/// Parse a `render_rca` argument document.
pub(crate) fn doc(args: &Yaml, now: i64) -> Result<Rca, String> {
    api::known(
        args,
        &[
            "title",
            "from",
            "to",
            "summary",
            "impact",
            "timeline",
            "root_cause",
            "evidence",
            "ruled_out",
            "contributing",
            "remediation",
            "verification",
            "prevention",
            "emit",
        ],
    )?;
    let (from, to) = api::bounds(args, now)?;
    let mut timeline = Vec::new();
    for e in list(args, "timeline")? {
        api::known(e, &["at", "what"])?;
        let at = api::time_field(&e["at"], now, i64::MIN)?;
        if at == i64::MIN {
            return Err("each timeline entry needs `at`, as '-15m' or nanoseconds".into());
        }
        timeline.push((at, text(e, "what", "a timeline entry")?));
    }
    timeline.sort_by_key(|(at, _)| *at);

    let mut evidence = Vec::new();
    for e in list(args, "evidence")? {
        evidence.push(fact(e, now, from, to)?);
    }

    // Parsed through the loader rather than pattern-matched, so the fence in
    // the document is a rule the operator can paste into `alerts.kyaml` and
    // restart on. `&[]` is the target list: a rendered rule may not name a
    // webhook, because the webhooks are in the operator's file and this process
    // has no idea what they are called.
    let prevention = match &args["prevention"] {
        Yaml::BadValue | Yaml::Null => None,
        y => {
            crate::alert::rule(y, &[]).map_err(|e| format!("prevention: {e}"))?;
            Some(kyaml(y))
        }
    };

    Ok(Rca {
        title: text(args, "title", "an RCA")?,
        from,
        to,
        summary: text(args, "summary", "an RCA")?,
        impact: strings(args, "impact")?,
        timeline,
        root_cause: text(args, "root_cause", "an RCA")?,
        evidence,
        ruled_out: strings(args, "ruled_out")?,
        contributing: strings(args, "contributing")?,
        remediation: strings(args, "remediation")?,
        verification: strings(args, "verification")?,
        prevention,
        emit: flag(args, "emit")?,
    })
}

fn fact(y: &Yaml, now: i64, from: i64, to: i64) -> Result<Fact, String> {
    api::known(y, &["claim", "trace_id", "query"])?;
    let claim = text(y, "claim", "an evidence item")?;
    let empty = |v: &Yaml| matches!(v, Yaml::BadValue | Yaml::Null);
    let (mut cite, shown) = match (&y["trace_id"], &y["query"]) {
        (t, q) if empty(t) && empty(q) => {
            return Err(format!(
                "evidence {claim:?} cites nothing: give it `trace_id` or `query`. \
                 A claim Mira cannot re-run is not evidence — put it in `root_cause` \
                 or `contributing` instead."
            ));
        }
        (t, q) if !empty(t) && !empty(q) => {
            return Err(format!(
                "evidence {claim:?} cites both `trace_id` and `query`; pick one"
            ));
        }
        (t, _) if !empty(t) => {
            let id = t.as_str().ok_or("`trace_id` must be a quoted string")?;
            (trace_query(id, 0)?, format!("trace `{}`", id.trim()))
        }
        (_, q) => {
            let mut s = api::search_doc(q, now)?;
            // A citation with no window of its own is a citation about the
            // incident, not about the last hour. Without this, a write-up of
            // something that happened this morning verifies against a window
            // that does not contain it and every query citation comes back
            // dead — which reads as "the agent made it up".
            if empty(&q["from"]) && empty(&q["to"]) {
                (s.from, s.to) = (from, to);
            }
            let shown = match filter(&s.terms) {
                f if f.is_empty() => format!("all {}", s.signal.dir()),
                f => format!("{} where `{f}`", s.signal.dir()),
            };
            (s, shown)
        }
    };
    // Counting is free and reading is not: this pass asks whether the evidence
    // is still there, not what it says. Same trick `alert::Engine` uses.
    cite.limit = 0;
    Ok(Fact {
        claim,
        cite,
        shown,
        found: None,
    })
}

impl Rca {
    /// True once every citation has been re-run.
    pub(crate) fn verified(&self) -> bool {
        self.evidence.iter().all(|f| f.found.is_some())
    }

    /// The citations that came back empty or unreadable, as one message.
    ///
    /// Returned instead of the document rather than alongside it. A markdown
    /// RCA is a thing people paste into a ticket, and handing one back with a
    /// warning above it is handing back a document that will be pasted without
    /// the warning.
    pub(crate) fn dead(&self) -> Option<String> {
        let dead: Vec<&Fact> = self
            .evidence
            .iter()
            .filter(|f| matches!(f.found, Some(0) | Some(UNREADABLE) | None))
            .collect();
        if dead.is_empty() {
            return None;
        }
        let mut out = format!(
            "nothing was rendered and nothing was stored: {} of {} citations do not \
             hold against this store.\n",
            dead.len(),
            self.evidence.len()
        );
        for f in dead {
            let why = match f.found {
                Some(UNREADABLE) | None => "could not be read",
                _ => "matches no records",
            };
            out.push_str(&format!("  - {} ({}) {why}\n", f.claim, f.shown));
        }
        out.push_str(
            "Widen the window, fix the filter, or drop the claim — but do not publish \
             a write-up citing evidence this node cannot produce.",
        );
        Some(out)
    }

    /// The document.
    ///
    /// # Panics
    ///
    /// If a citation has not been re-run. Callers go through
    /// [`dead`](Rca::dead) first, which is only meaningful after the same pass.
    pub(crate) fn render(&self) -> String {
        assert!(self.verified(), "render before the citations were checked");
        let mut m = String::new();
        m.push_str(&format!("# {}\n\n", self.title));
        m.push_str(&format!(
            "*{} → {} ({}). Written by an agent against Mira; every citation below was \
             re-run at render time and returned the record count beside it.*\n\n",
            utc(self.from),
            utc(self.to),
            span(self.to - self.from)
        ));

        m.push_str("## Summary\n\n");
        m.push_str(&self.summary);
        m.push_str("\n\n");
        bullets(&mut m, "Impact", &self.impact);

        if !self.timeline.is_empty() {
            m.push_str("## Timeline\n\n| When (UTC) | What |\n| --- | --- |\n");
            for (at, what) in &self.timeline {
                m.push_str(&format!("| {} | {} |\n", utc(*at), cell(what)));
            }
            m.push('\n');
        }

        m.push_str("## Root cause\n\n");
        m.push_str(&self.root_cause);
        m.push_str("\n\n");

        if !self.evidence.is_empty() {
            m.push_str("## Evidence\n\n");
            for f in &self.evidence {
                // One line per claim, no nesting and no raw HTML. An RCA gets
                // pasted into whatever the team uses, and a two-line bullet is
                // the first thing a renderer that is not GitHub gets wrong.
                m.push_str(&format!(
                    "- {} — {}, {} records\n",
                    f.claim,
                    f.shown,
                    f.found.unwrap_or_default()
                ));
            }
            m.push('\n');
        }

        bullets(&mut m, "Ruled out", &self.ruled_out);
        bullets(&mut m, "Contributing factors", &self.contributing);
        if !self.remediation.is_empty() {
            bullets(&mut m, "Remediation", &self.remediation);
            // In the document, not only in this repository's docs. Whoever
            // reads the RCA is the one who needs to know that none of the above
            // has happened yet.
            m.push_str(
                "> Mira did not apply any of this and cannot: it has no verb that \
                 changes a cluster.\n\n",
            );
        }
        bullets(&mut m, "Verification", &self.verification);

        if let Some(rule) = &self.prevention {
            m.push_str(
                "## Prevention\n\nThe rule that would have caught this, ready for \
                 `alerts.kyaml` — add your own `notify` targets:\n\n```yaml\nrules:\n  - ",
            );
            // The fragment is rendered at zero indent; inside the list it needs
            // four spaces on every line but the first, which the `- ` above
            // already carries.
            m.push_str(&rule.replace('\n', "\n    "));
            m.push_str("\n```\n\n");
        }
        m
    }

    /// The write-up as an OTLP log export, for `emit`.
    ///
    /// A log record and not a new kind of object, which is the whole reason
    /// this is affordable: an RCA is then searchable by `query_records`, framed
    /// by `correlate`, and expired by the same retention as the telemetry it
    /// describes. There is no incident store, no index to maintain and no
    /// second thing to back up (principle 4).
    pub(crate) fn export(
        &self,
        md: &str,
        now: i64,
    ) -> mira_proto::collector::logs::v1::ExportLogsServiceRequest {
        use mira_proto::collector::logs::v1::ExportLogsServiceRequest;
        use mira_proto::common::v1::{AnyValue, InstrumentationScope, KeyValue, any_value};
        use mira_proto::logs::v1::{LogRecord, ResourceLogs, ScopeLogs};
        use mira_proto::resource::v1::Resource;

        let kv = |k: &str, v: String| KeyValue {
            key: k.into(),
            value: Some(AnyValue {
                value: Some(any_value::Value::StringValue(v)),
            }),
        };
        ExportLogsServiceRequest {
            resource_logs: vec![ResourceLogs {
                resource: Some(Resource {
                    // `service.name` is this node, because this node is what
                    // produced the record. Naming the *subject* service here
                    // would file the write-up under that service's entity key
                    // and put a document in the middle of its logs — see the
                    // identity ladder in `mira_core::identity`.
                    attributes: vec![kv("service.name", "mira".into())],
                    ..Default::default()
                }),
                scope_logs: vec![ScopeLogs {
                    scope: Some(InstrumentationScope {
                        name: "mira.rca".into(),
                        ..Default::default()
                    }),
                    log_records: vec![LogRecord {
                        time_unix_nano: now.max(0) as u64,
                        // INFO. An RCA is a document about an incident, not an
                        // incident: filed at ERROR it would show up in every
                        // "show me the errors" query for the rest of retention.
                        severity_number: 9,
                        severity_text: "INFO".into(),
                        event_name: "rca".into(),
                        body: Some(AnyValue {
                            value: Some(any_value::Value::StringValue(md.to_owned())),
                        }),
                        attributes: vec![
                            kv("rca.title", self.title.clone()),
                            kv("rca.window.from", utc(self.from)),
                            kv("rca.window.to", utc(self.to)),
                        ],
                        ..Default::default()
                    }],
                    ..Default::default()
                }],
                ..Default::default()
            }],
        }
    }
}

fn bullets(m: &mut String, heading: &str, items: &[String]) {
    if items.is_empty() {
        return;
    }
    m.push_str(&format!("## {heading}\n\n"));
    for i in items {
        m.push_str(&format!("- {i}\n"));
    }
    m.push('\n');
}

/// A pipe inside a table cell ends the cell. Nothing else in GitHub-flavoured
/// markdown can break a row, and a newline cannot arrive here — KYAML scalars
/// keep theirs, so they are turned into spaces rather than escaped.
fn cell(s: &str) -> String {
    s.replace('|', "\\|").replace('\n', " ")
}

/// A required, quoted scalar.
fn text(doc: &Yaml, key: &str, what: &str) -> Result<String, String> {
    doc[key]
        .as_str()
        .map(str::to_owned)
        .ok_or(format!("{what} needs a quoted `{key}`"))
}

/// An optional list of quoted scalars. Absent is empty; a bare string is not a
/// list of one, because a model that wrote one bullet meant to write a list.
fn strings(doc: &Yaml, key: &str) -> Result<Vec<String>, String> {
    list(doc, key)?
        .iter()
        .map(|y| {
            y.as_str()
                .map(str::to_owned)
                .ok_or(format!("every entry of `{key}` must be a quoted string"))
        })
        .collect()
}

fn list<'a>(doc: &'a Yaml, key: &str) -> Result<&'a [Yaml], String> {
    match &doc[key] {
        Yaml::BadValue | Yaml::Null => Ok(&[]),
        Yaml::Array(a) => Ok(a),
        _ => Err(format!("`{key}` must be a list")),
    }
}

fn flag(doc: &Yaml, key: &str) -> Result<bool, String> {
    match &doc[key] {
        Yaml::BadValue | Yaml::Null => Ok(false),
        Yaml::Boolean(b) => Ok(*b),
        y => y
            .as_str()
            .and_then(|s| s.parse().ok())
            .ok_or(format!("{key}: expected \"true\" or \"false\"")),
    }
}

/// Terms in the filter-bar grammar, so a reader can paste the citation into
/// Mira's UI. The same spelling `alert::filter_of` puts in a page.
fn filter(terms: &[Term]) -> String {
    crate::alert::filter_of(terms)
}

/// UTC, RFC 3339, to the second.
///
/// `tui::stamp` is local time, which is right on a terminal someone is sitting
/// at and wrong in a document that gets pasted into a ticket and read from
/// another timezone — where an unqualified `09:41` is a bug report about the
/// wrong hour. `gmtime_r` for the same reason `tui` uses `localtime_r`: libc is
/// already here and the calendar is the part a hand-rolled version gets wrong.
fn utc(ns: i64) -> String {
    let secs = ns.div_euclid(1_000_000_000) as libc::time_t;
    // SAFETY: identical to `tui::civil` — `tm` is integers plus a `tm_zone`
    // pointer, for which all-zero is valid, and `gmtime_r` writes only into the
    // local it is handed. A `time_t` it cannot represent leaves the zeroes,
    // which render as 1900-01-01: a wrong date, not uninitialised memory.
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    // SAFETY: both arguments are live, correctly typed locals, and the `_r`
    // form writes only into `tm`.
    unsafe { libc::gmtime_r(&secs, &mut tm) };
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z",
        tm.tm_year + 1900,
        tm.tm_mon + 1,
        tm.tm_mday,
        tm.tm_hour,
        tm.tm_min,
        tm.tm_sec
    )
}

/// A window length, for the line under the title.
fn span(ns: i64) -> String {
    match ns.max(0) / 1_000_000_000 {
        s if s >= 86_400 => format!("{}d{}h", s / 86_400, s % 86_400 / 3_600),
        s if s >= 3_600 => format!("{}h{:02}m", s / 3_600, s % 3_600 / 60),
        s if s >= 60 => format!("{}m{:02}s", s / 60, s % 60),
        s => format!("{s}s"),
    }
}

/// A `Yaml` node back out as KYAML.
///
/// yaml-rust2 ships an emitter and it is the wrong one: it writes plain
/// scalars, and this text goes into an `alerts.kyaml`. JSON is the quoted
/// subset of KYAML (principle 5), so writing JSON is writing KYAML — and it is
/// the spelling that keeps `{"gt": 500000000}` a number, which quoting every
/// scalar alike would not.
fn kyaml(y: &Yaml) -> String {
    match y.as_hash() {
        // One top-level key per line, values compact. All-compact is valid and
        // unreadable — this is the one fragment in the document somebody edits
        // by hand — and pretty-printing the nested `where` would be a YAML
        // emitter, which is the thing this function exists to not be.
        Some(h) => h
            .iter()
            .map(|(k, v)| format!("{}: {}", one(k), one(v)))
            .collect::<Vec<_>>()
            .join("\n"),
        None => one(y),
    }
}

fn one(y: &Yaml) -> String {
    let mut j = Json::new();
    write(y, &mut j);
    j.into_string()
}

fn write(y: &Yaml, j: &mut Json) {
    match y {
        Yaml::Hash(h) => j.obj(|j| {
            for (k, v) in h {
                j.key(k.as_str().unwrap_or_default());
                write(v, j);
            }
        }),
        Yaml::Array(a) => j.arr(|j| {
            for v in a {
                write(v, j);
            }
        }),
        Yaml::String(s) => j.str(s),
        Yaml::Integer(n) => j.i64(*n),
        Yaml::Boolean(b) => j.bool(*b),
        Yaml::Real(r) => j.f64(r.parse().unwrap_or_default()),
        _ => j.null(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(s: &str) -> Yaml {
        api::parse(s).unwrap()
    }

    /// 2026-09-19T10:00:00Z.
    const T: i64 = 1_789_812_000_000_000_000;

    /// The one guarantee that separates this from a prompt asking for markdown:
    /// a claim with nowhere to check it is refused at parse, and a claim whose
    /// citation came back empty is refused at render. Both have to be errors the
    /// model can act on, so both name the claim.
    #[test]
    fn a_claim_mira_cannot_re_run_is_not_evidence() {
        let base = r#"{"title":"t","summary":"s","root_cause":"r","#;
        let uncited = doc(
            &parse(&format!(
                r#"{base}"evidence":[{{"claim":"the pods were fine"}}]}}"#
            )),
            T,
        )
        .unwrap_err();
        assert!(uncited.contains("cites nothing"), "{uncited}");
        assert!(uncited.contains("the pods were fine"), "{uncited}");
        assert!(uncited.contains("root_cause"), "{uncited}");

        let both = doc(
            &parse(&format!(
                r#"{base}"evidence":[{{"claim":"c","query":{{}},
                   "trace_id":"4bf92f3577b34da6a3ce929d0e0e4736"}}]}}"#
            )),
            T,
        )
        .unwrap_err();
        assert!(both.contains("pick one"), "{both}");

        // Checked and empty, and never checked at all, are both "do not
        // publish" — a citation the store could not read is not a citation that
        // held.
        let mut r = doc(
            &parse(&format!(
                r#"{base}"evidence":[
                   {{"claim":"checkout 5xx","query":{{"where":[{{"attr":"service.name","eq":"checkout"}}]}}}},
                   {{"claim":"one bad trace","trace_id":"4bf92f3577b34da6a3ce929d0e0e4736"}}]}}"#
            )),
            T,
        )
        .unwrap();
        assert!(!r.verified());
        r.evidence[0].seen(Some(0));
        r.evidence[1].seen(None);
        assert!(r.verified());
        let dead = r.dead().unwrap();
        assert!(dead.contains("2 of 2"), "{dead}");
        assert!(dead.contains("matches no records"), "{dead}");
        assert!(dead.contains("could not be read"), "{dead}");
        // The citation is echoed in the grammar the UI's filter bar takes, so
        // the reader can check it by hand.
        assert!(dead.contains("attr:service.name=checkout"), "{dead}");
        assert!(dead.contains("nothing was stored"), "{dead}");

        r.evidence[0].seen(Some(12));
        r.evidence[1].seen(Some(1));
        assert!(r.dead().is_none());
    }

    /// A citation with no window of its own is about the incident, not about
    /// the last hour — otherwise every write-up of something that happened this
    /// morning fails its own verification. A citation that *does* name a window
    /// keeps it.
    #[test]
    fn a_citation_inherits_the_incidents_window_unless_it_names_one() {
        let r = doc(
            &parse(
                r#"{"title":"t","summary":"s","root_cause":"r",
                    "from":"-6h","to":"-5h","evidence":[
                      {"claim":"a","query":{}},
                      {"claim":"b","query":{"from":"-30m"}}]}"#,
            ),
            T,
        )
        .unwrap();
        assert_eq!(
            (r.evidence[0].cite.from, r.evidence[0].cite.to),
            (r.from, r.to)
        );
        assert_eq!(r.from, T - 6 * 3_600_000_000_000);
        assert_eq!(r.evidence[1].cite.from, T - 30 * 60_000_000_000);
        // Counting is free, reading is not: verification never materialises a
        // row, whichever form the citation took.
        assert!(r.evidence.iter().all(|f| f.cite.limit == 0));
        // A trace citation is the whole of retention, because you look a trace
        // up when you do not know when it happened.
        let t = doc(
            &parse(
                r#"{"title":"t","summary":"s","root_cause":"r","evidence":[
                    {"claim":"a","trace_id":" 4BF92F3577B34DA6A3CE929D0E0E4736 "}]}"#,
            ),
            T,
        )
        .unwrap();
        assert_eq!(
            (t.evidence[0].cite.from, t.evidence[0].cite.to),
            (0, i64::MAX)
        );
        assert_eq!(t.evidence[0].cite.signal, Signal::Traces);

        let bad = doc(
            &parse(
                r#"{"title":"t","summary":"s","root_cause":"r","evidence":[
                    {"claim":"a","trace_id":"0102030405060708"}]}"#,
            ),
            T,
        )
        .unwrap_err();
        assert!(bad.contains("16-byte"), "{bad}");
    }

    /// The prevention rule goes through the loader, so what the document
    /// promises is a rule that will start — not one that looks like it would.
    /// And it comes back as KYAML, because that is the file it is destined for.
    #[test]
    fn a_proposed_rule_is_one_the_alert_loader_accepts() {
        let with = |rule: &str| {
            doc(
                &parse(&format!(
                    r#"{{"title":"t","summary":"s","root_cause":"r","prevention":{rule}}}"#
                )),
                T,
            )
        };
        let good = with(
            r#"{"name":"checkout-5xx","over":"5m","for":"2m","when":"count > 20",
                "query":{"signal":"logs","where":[{"attr":"service.name","eq":"checkout"},
                                                  {"field":"severity_number","gte":17}]}}"#,
        )
        .unwrap();
        let md = {
            let mut g = good;
            g.evidence.clear();
            g.render()
        };
        assert!(md.contains("```yaml\nrules:\n  - \"name\""), "{md}");
        // Quoted keys and quoted strings, and 17 still a number: quoting every
        // scalar alike would turn the severity comparison into a string
        // comparison.
        assert!(md.contains(r#""name": "checkout-5xx""#), "{md}");
        assert!(md.contains(r#""gte":17"#), "{md}");

        // A window in the rule's query is the loader's error, not this file's,
        // and it arrives at render time rather than at 3am.
        let windowed =
            with(r#"{"name":"r","when":"count > 1","query":{"signal":"logs","from":"-1h"}}"#)
                .unwrap_err();
        assert!(windowed.contains("prevention:"), "{windowed}");
        assert!(windowed.contains("over"), "{windowed}");

        let typo =
            with(r#"{"name":"r","when":"count > 1","query":{},"notify":["oncall"]}"#).unwrap_err();
        assert!(typo.contains("prevention:"), "{typo}");
    }

    /// The document itself: UTC because it leaves this machine, the timeline
    /// sorted because a reader believes the order, and the remediation section
    /// carrying the sentence that says none of it has happened.
    #[test]
    fn the_rendered_document_is_timezone_free_ordered_and_honest_about_what_it_did() {
        let mut r = doc(
            &parse(
                r#"{"title":"Checkout 5xx","summary":"Checkout returned 502 for 31 minutes.",
                    "from":1789812000000000000,"to":1789813860000000000,
                    "impact":["4,102 requests failed"],
                    "timeline":[{"at":1789813800000000000,"what":"rollback | complete"},
                                {"at":1789812060000000000,"what":"first 502"}],
                    "root_cause":"The 1.4.0 image raised the pool ceiling.",
                    "evidence":[{"claim":"the pool was exhausted",
                                 "query":{"where":[{"attr":"service.name","eq":"checkout"}]}}],
                    "ruled_out":["Not the database: no slow queries."],
                    "contributing":["No alert on pool saturation."],
                    "remediation":["Roll back to 1.3.9."],
                    "verification":["502 rate back to zero."]}"#,
            ),
            T,
        )
        .unwrap();
        r.evidence[0].seen(Some(4_102));
        let md = r.render();

        assert!(md.starts_with("# Checkout 5xx\n"), "{md}");
        assert!(
            md.contains("2026-09-19T10:00:00Z → 2026-09-19T10:31:00Z (31m00s)"),
            "{md}"
        );
        // Sorted, and the pipe inside a cell escaped rather than ending it.
        let first = md.find("first 502").unwrap();
        let second = md.find("rollback").unwrap();
        assert!(first < second, "timeline out of order:\n{md}");
        assert!(md.contains(r"rollback \| complete"), "{md}");
        // Every citation carries what it found, which is the difference between
        // a write-up and a template.
        assert!(md.contains(", 4102 records"), "{md}");
        assert!(
            md.contains("> Mira did not apply any of this and cannot"),
            "{md}"
        );
        // Sections with nothing in them are absent rather than empty: an RCA
        // with a bare "## Prevention" invites the reader to assume there was
        // none to propose.
        assert!(!md.contains("## Prevention"), "{md}");

        // The same document as an OTLP export: one log record, INFO, filed
        // under this node rather than under the service it is about.
        let req = r.export(&md, T);
        let rl = &req.resource_logs[0];
        let rec = &rl.scope_logs[0].log_records[0];
        assert_eq!(rec.severity_number, 9);
        assert_eq!(rec.event_name, "rca");
        assert_eq!(rec.time_unix_nano, T as u64);
        assert_eq!(
            rl.resource.as_ref().unwrap().attributes[0].key,
            "service.name"
        );
        assert!(rec.attributes.iter().any(|a| a.key == "rca.title"));
    }

    /// Everything a model can get wrong in the argument document, refused with
    /// the key named. A dropped section is worse here than in a query: nobody
    /// re-reads an RCA to check that the timeline they wrote is in it.
    #[test]
    fn a_misspelled_section_is_refused_rather_than_dropped() {
        let bad = [
            (r#"{"summary":"s","root_cause":"r"}"#, "`title`"),
            (r#"{"title":"t","root_cause":"r"}"#, "`summary`"),
            (r#"{"title":"t","summary":"s"}"#, "`root_cause`"),
            (
                r#"{"title":"t","summary":"s","root_cause":"r","timelines":[]}"#,
                "unknown query key",
            ),
            (
                r#"{"title":"t","summary":"s","root_cause":"r","impact":"one thing"}"#,
                "`impact` must be a list",
            ),
            (
                r#"{"title":"t","summary":"s","root_cause":"r","impact":[7]}"#,
                "quoted string",
            ),
            (
                r#"{"title":"t","summary":"s","root_cause":"r","timeline":[{"what":"x"}]}"#,
                "needs `at`",
            ),
            (
                r#"{"title":"t","summary":"s","root_cause":"r","timeline":[{"at":"-5m"}]}"#,
                "`what`",
            ),
            (
                r#"{"title":"t","summary":"s","root_cause":"r","emit":"yes"}"#,
                "emit",
            ),
            (
                r#"{"title":"t","summary":"s","root_cause":"r","from":"-1h","to":"-2h"}"#,
                "is after",
            ),
        ];
        for (args, want) in bad {
            let e = doc(&parse(args), T).unwrap_err();
            assert!(e.contains(want), "{args}\n  wanted {want:?}, got {e:?}");
        }

        // And the minimum that is accepted: three sentences and no citations.
        // An RCA with nothing to cite is a short RCA, not an error.
        let r = doc(&parse(r#"{"title":"t","summary":"s","root_cause":"r"}"#), T).unwrap();
        assert!(r.verified());
        assert!(r.dead().is_none());
        assert!(!r.emit);
        assert!(r.render().contains("## Root cause"));
    }

    /// The rule fence is written by hand, so the writer gets its own test: it
    /// is the one place a value's *type* survives into the document, and
    /// `{"gte": 17}` quoted like a string is a severity comparison that no
    /// longer compares severities.
    #[test]
    fn the_rule_fence_keeps_a_number_a_number_and_one_key_per_line() {
        let y = parse(
            r#"{"name":"x","when":"count >= 1","query":{"where":[{"gte":17},{"on":true},
               {"ratio":0.5},{"nil":null}]}}"#,
        );
        let out = kyaml(&y);
        let lines: Vec<_> = out.lines().collect();
        assert_eq!(lines[0], r#""name": "x""#, "{out}");
        assert_eq!(lines.len(), 3, "one top-level key per line:\n{out}");
        // Nested values stay compact, and every scalar keeps its own type.
        assert!(out.contains(r#"{"gte":17}"#), "{out}");
        assert!(out.contains(r#"{"on":true}"#), "{out}");
        assert!(out.contains(r#"{"ratio":0.5}"#), "{out}");
        assert!(out.contains(r#"{"nil":null}"#), "{out}");
        // A fragment that is not a mapping is written as the one value it is,
        // rather than losing itself in a key-per-line loop that has no keys.
        assert_eq!(kyaml(&parse("[1, 2]")), "[1,2]");
    }
}
