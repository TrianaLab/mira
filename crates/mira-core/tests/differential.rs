//! Differential verification: the engine against a reference model.
//!
//! A fixture test asserts that one query returns one answer, which proves the
//! path it happens to walk. This builds a random store, computes the answer with
//! forty lines of `Vec::filter` that share no code with the engine, and asserts
//! they agree — for a few thousand random queries. What it actually pins down is
//! everything the model does *not* reimplement:
//!
//! * block pruning by time range never drops a block holding a match;
//! * the attribute Bloom sidecar never prunes a block holding a match, which is
//!   the one failure mode an index is not allowed to have (§7.4);
//! * the merge across blocks is ordered, and `limit` cuts the right end of it;
//! * paging with a cursor visits every row exactly once — no duplicate, no gap;
//! * `rows_matched` is the size of the match set, not of the page.
//!
//! No proptest. The dependency budget is a stated product property (§11) and
//! shrinking is the only thing it would add here; a failure prints its seed and
//! `MIRA_DIFF_SEED=<n>` replays it exactly.

use std::collections::BTreeMap;

use mira_core::query::{self, Op, Search, Signal, Target, Term, Value};
use mira_core::{block, logs};
use mira_proto::collector::logs::v1::ExportLogsServiceRequest;
use mira_proto::common::v1::{AnyValue, InstrumentationScope, KeyValue, any_value};
use mira_proto::logs::v1::{LogRecord, ResourceLogs, ScopeLogs};
use mira_proto::resource::v1::Resource;

/// xorshift64*. Deterministic, seedable, eight lines — a generator good enough
/// to shake out a query planner does not need to be good enough for a casino.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_f491_4f6c_dd1d)
    }

    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }

    fn pick<'a, T>(&mut self, xs: &'a [T]) -> &'a T {
        &xs[self.below(xs.len() as u64) as usize]
    }
}

/// Resource and scope attributes are a pure function of the entity's name.
///
/// Not laziness: the ingest path interns a resource by identity, so two resource
/// rows with the same `service.name` and different attributes land on *one*
/// entity whose attribute set is the union — and then a filter matches records
/// that were exported before that key existed. Real, intended, and a rule the
/// model would have to grow a second interning table to predict. Keeping the map
/// injective takes it off the table so the failures this test reports are query
/// failures.
const RESOURCES: [(&str, &[(&str, &str)]); 3] = [
    (
        "checkout",
        &[("svc", "alpha"), ("env", "prod"), ("region", "eu")],
    ),
    ("payments", &[("svc", "beta"), ("env", "prod")]),
    ("search", &[("svc", "gamma"), ("tier", "gold")]),
];

const SCOPES: [(&str, &[(&str, &str)]); 3] = [
    ("http", &[("tier", "silver")]),
    ("db", &[]),
    ("cache", &[("region", "us"), ("env", "canary")]),
];

const SEV: [(i32, &str); 5] = [
    (1, "TRACE"),
    (5, "DEBUG"),
    (9, "INFO"),
    (13, "WARN"),
    (17, "ERROR"),
];

/// Deliberately overlapping with the resource and scope sets above: `env` and
/// `tier` and `region` each live at two levels, so a row can match a term at one
/// level, the other, or both. That union is the part of `attr_rows` a
/// single-level fixture cannot reach.
const REC_KEYS: [&str; 4] = ["env", "tier", "region", "user"];
const VALS: [&str; 4] = ["alpha", "beta", "prod", "canary"];
/// `nope` is in the query pool and in no record: the dictionary miss.
const ATTR_KEYS: [&str; 6] = ["svc", "env", "region", "tier", "user", "nope"];

/// A stored attribute value. Strings and ints only — the two the generator
/// emits, and the two `attr_matches` dispatches differently.
#[derive(Clone, Copy)]
enum Av {
    Str(&'static str),
    Int(i64),
}

/// One log record, as the model holds it.
struct Row {
    ts: i64,
    /// Unique across the whole store, so a returned row can be named by it and
    /// the model's sort is total on `ts` alone.
    body: String,
    sev: usize,
    res: usize,
    scope: usize,
    rec: BTreeMap<&'static str, Av>,
}

impl Row {
    /// Does this record carry `key op value` at *any* of the three levels?
    ///
    /// Union, not override. `emit_attrs` merges with the record level winning,
    /// which is a different question and a different function; conflating them
    /// is the obvious way to write a model that disagrees for the wrong reason.
    fn attr(&self, key: &str, op: Op, v: &Value) -> bool {
        let level = |pairs: &'static [(&'static str, &'static str)]| {
            pairs
                .iter()
                .any(|&(k, val)| k == key && av_matches(Av::Str(val), op, v))
        };
        level(RESOURCES[self.res].1)
            || level(SCOPES[self.scope].1)
            || self.rec.get(key).is_some_and(|&a| av_matches(a, op, v))
    }
}

// ---------------------------------------------------------------------------
// The model. Every function here is written from the documented semantics in
// `query.rs`, not from its code.
// ---------------------------------------------------------------------------

fn holds(op: Op, ord: std::cmp::Ordering) -> bool {
    match op {
        Op::Eq => ord.is_eq(),
        Op::Ne => ord.is_ne(),
        Op::Lt => ord.is_lt(),
        Op::Lte => ord.is_le(),
        Op::Gt => ord.is_gt(),
        Op::Gte => ord.is_ge(),
        // A substring test on anything that is not a string matches nothing.
        Op::Contains => false,
    }
}

/// `query::canon`: the text an attribute of this value is indexed under.
fn text(v: &Value) -> String {
    match v {
        Value::Str(s) => s.clone(),
        Value::Int(i) => i.to_string(),
        Value::Double(_) | Value::Bool(_) => unreachable!("not generated"),
    }
}

fn int(v: &Value) -> Option<i64> {
    match v {
        Value::Int(i) => Some(*i),
        Value::Str(s) => s.parse().ok(),
        Value::Double(_) | Value::Bool(_) => unreachable!("not generated"),
    }
}

fn av_matches(a: Av, op: Op, v: &Value) -> bool {
    match a {
        Av::Str(s) => match op {
            Op::Contains => s.contains(&text(v)),
            // Equality against a stored string is equality with the indexed
            // text, because the Bloom filter holds exactly that.
            Op::Eq | Op::Ne => holds(op, s.cmp(text(v).as_str())),
            // Ordering is not in the index, so a number written as a string can
            // be ordered as the number it is — unless the query quoted it, which
            // asks for a text comparison.
            _ => match (s.parse::<f64>(), int(v)) {
                (Ok(x), Some(y)) if !matches!(v, Value::Str(_)) => {
                    x.partial_cmp(&(y as f64)).is_some_and(|o| holds(op, o))
                }
                _ => holds(op, s.cmp(text(v).as_str())),
            },
        },
        Av::Int(x) => int(v).is_some_and(|y| holds(op, x.cmp(&y))),
    }
}

fn matches(r: &Row, t: &Term) -> bool {
    match &t.target {
        Target::Field(f) if f == "severity_number" => {
            int(&t.value).is_some_and(|y| holds(t.op, (SEV[r.sev].0 as i64).cmp(&y)))
        }
        // `body` is Utf8 and `severity_text` a dictionary of Utf8; both need a
        // string on the query side and compare the same way once they have one.
        Target::Field(f) => {
            let s = if f == "body" {
                r.body.as_str()
            } else {
                SEV[r.sev].1
            };
            let Value::Str(t2) = &t.value else {
                return false;
            };
            match t.op {
                Op::Contains => s.contains(t2.as_str()),
                _ => holds(t.op, s.cmp(t2.as_str())),
            }
        }
        Target::Attr(k) => r.attr(k, t.op, &t.value),
    }
}

/// Everything the store should return for `q`, newest first, before `limit`.
fn expect<'a>(rows: &'a [Row], q: &Search) -> Vec<&'a str> {
    let mut hits: Vec<&Row> = rows
        .iter()
        .filter(|r| (q.from..=q.to).contains(&r.ts))
        .filter(|r| q.terms.iter().all(|t| matches(r, t)))
        .collect();
    hits.sort_by_key(|r| std::cmp::Reverse(r.ts));
    hits.iter().map(|r| r.body.as_str()).collect()
}

// ---------------------------------------------------------------------------
// Generation.
// ---------------------------------------------------------------------------

fn kv(k: &str, v: any_value::Value) -> KeyValue {
    KeyValue {
        key: k.into(),
        value: Some(AnyValue { value: Some(v) }),
    }
}

fn str_kvs(pairs: &[(&str, &str)]) -> Vec<KeyValue> {
    pairs
        .iter()
        .map(|&(k, v)| kv(k, any_value::Value::StringValue(v.into())))
        .collect()
}

fn rec_attrs(rng: &mut Rng) -> BTreeMap<&'static str, Av> {
    let mut out = BTreeMap::new();
    for _ in 0..rng.below(3) {
        out.insert(*rng.pick(&REC_KEYS), Av::Str(rng.pick(&VALS)));
    }
    // An int-valued attribute, so the type dispatch in `attr_matches` and the
    // `canon` contract with the Bloom index both carry weight.
    if rng.below(2) == 0 {
        out.insert("code", Av::Int(rng.below(5) as i64));
    }
    out
}

/// One export, appending the records it carries to the model as it builds them.
fn request(rng: &mut Rng, base: i64, rows: &mut Vec<Row>) -> ExportLogsServiceRequest {
    let resource_logs = (0..1 + rng.below(2))
        .map(|_| {
            let res = rng.below(RESOURCES.len() as u64) as usize;
            let scope_logs = (0..1 + rng.below(2))
                .map(|_| {
                    let scope = rng.below(SCOPES.len() as u64) as usize;
                    let log_records = (0..1 + rng.below(12))
                        .map(|_| {
                            let n = rows.len();
                            let row = Row {
                                // Scattered over ~2 hours so a store spans three
                                // hour partitions, and unique because the spacing
                                // between two draws dwarfs the counter.
                                ts: base + rng.below(40_000) as i64 * 200_000_000 + n as i64,
                                body: format!("r{n}"),
                                sev: rng.below(SEV.len() as u64) as usize,
                                res,
                                scope,
                                rec: rec_attrs(rng),
                            };
                            let rec = LogRecord {
                                time_unix_nano: row.ts as u64,
                                severity_number: SEV[row.sev].0,
                                severity_text: SEV[row.sev].1.into(),
                                body: Some(AnyValue {
                                    value: Some(any_value::Value::StringValue(row.body.clone())),
                                }),
                                attributes: row
                                    .rec
                                    .iter()
                                    .map(|(&k, v)| match *v {
                                        Av::Str(s) => {
                                            kv(k, any_value::Value::StringValue(s.into()))
                                        }
                                        Av::Int(i) => kv(k, any_value::Value::IntValue(i)),
                                    })
                                    .collect(),
                                ..Default::default()
                            };
                            rows.push(row);
                            rec
                        })
                        .collect();
                    ScopeLogs {
                        scope: Some(InstrumentationScope {
                            name: SCOPES[scope].0.into(),
                            attributes: str_kvs(SCOPES[scope].1),
                            ..Default::default()
                        }),
                        log_records,
                        ..Default::default()
                    }
                })
                .collect();
            let mut attributes = vec![kv(
                "service.name",
                any_value::Value::StringValue(RESOURCES[res].0.into()),
            )];
            attributes.extend(str_kvs(RESOURCES[res].1));
            ResourceLogs {
                resource: Some(Resource {
                    attributes,
                    ..Default::default()
                }),
                scope_logs,
                ..Default::default()
            }
        })
        .collect();
    ExportLogsServiceRequest { resource_logs }
}

/// A store of one to three blocks, published exactly as the ingest pipeline
/// publishes them, plus the model of what is in it.
fn store(rng: &mut Rng, root: &std::path::Path, base: i64) -> Vec<Row> {
    let mut rows = Vec::new();
    let node = block::node_id("diff");
    for seq in 1..=1 + rng.below(4) {
        let mut b = logs::LogsBuilder::new();
        for _ in 0..1 + rng.below(3) {
            b.append_request(&request(rng, base, &mut rows)).unwrap();
        }
        let sealed = b.finish().unwrap();
        block::publish(root, "logs", node, seq, &sealed).unwrap();
    }
    rows
}

/// A random conjunct over a column or an attribute.
fn term(rng: &mut Rng) -> Term {
    const OPS: [Op; 7] = [
        Op::Eq,
        Op::Ne,
        Op::Lt,
        Op::Lte,
        Op::Gt,
        Op::Gte,
        Op::Contains,
    ];
    let op = *rng.pick(&OPS);
    // A quarter of the numbers arrive quoted, because that is how a browser and
    // an LLM both write them and the coercion is load-bearing either way.
    let number = |rng: &mut Rng, n: i64| match rng.below(4) {
        0 => Value::Str(n.to_string()),
        _ => Value::Int(n),
    };
    let (target, value) = match rng.below(5) {
        // Int32 column.
        0 => {
            let n = rng.pick(&SEV).0 as i64;
            (Target::Field("severity_number".into()), number(rng, n))
        }
        // Utf8 column. `contains: "r1"` is a whole family of rows, not one.
        1 => (
            Target::Field("body".into()),
            Value::Str(format!("r{}", rng.below(30))),
        ),
        // Dictionary column: the predicate is evaluated against the dictionary
        // and the scan is a lookup table, so it is a different code path.
        2 => (
            Target::Field("severity_text".into()),
            Value::Str(rng.pick(&SEV).1.into()),
        ),
        // A key that lives at a different level in different rows.
        3 => (
            Target::Attr((*rng.pick(&ATTR_KEYS)).into()),
            Value::Str((*rng.pick(&VALS)).into()),
        ),
        _ => {
            let n = rng.below(5) as i64;
            (Target::Attr("code".into()), number(rng, n))
        }
    };
    Term { target, op, value }
}

fn search(rng: &mut Rng, base: i64) -> Search {
    // A quarter of the bounds are open, which is what a UI's default is, and an
    // open bound is what makes the sidecar filters the only thing pruning.
    let bound = |rng: &mut Rng| match rng.below(4) {
        0 => 0,
        1 => i64::MAX,
        _ => base + rng.below(9_000_000_000_000) as i64,
    };
    let (a, b) = (bound(rng), bound(rng));
    Search {
        signal: Signal::Logs,
        from: a.min(b),
        to: a.max(b),
        terms: (0..rng.below(3)).map(|_| term(rng)).collect(),
        limit: 1 + rng.below(20) as usize,
        after: None,
    }
}

/// The rows in a response, named by their unique bodies. A response is JSON and
/// the point of the assertion is which rows came back, in which order — parsing
/// it properly would mean a JSON parser, and there is deliberately none in the
/// tree (principle 5: KYAML in, and JSON is a subset of it).
fn bodies(json: &str) -> Vec<&str> {
    json.split("\"body\":\"")
        .skip(1)
        .map(|s| s.split('"').next().expect("a closed string"))
        .collect()
}

/// The whole thing: random stores, random queries, the model as the oracle.
#[test]
fn the_engine_agrees_with_a_reference_model_on_random_stores() {
    const BASE: i64 = 1_700_000_000_000_000_000;
    let seed0: u64 = std::env::var("MIRA_DIFF_SEED")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(0x5eed_1234_abcd_0001);
    let dir = std::env::temp_dir().join(format!("mira-diff-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    // How much work the generator actually produced, checked at the end. A
    // differential test whose queries all return nothing passes forever and
    // proves nothing, and the way it gets there — a filter pool that drifts out
    // of step with the data pool — leaves no other trace.
    let (mut matched, mut paged) = (0usize, 0usize);

    for case in 0..24u64 {
        let seed = seed0.wrapping_add(case.wrapping_mul(0x9e37_79b9_7f4a_7c15));
        let mut rng = Rng(seed | 1);
        let root = dir.join(format!("case-{case}"));
        let rows = store(&mut rng, &root, BASE);
        let ctx = |q: &Search| format!("MIRA_DIFF_SEED={seed} case {case}\nquery {q:?}");

        for i in 0..16 {
            let q = search(&mut rng, BASE);
            let want = expect(&rows, &q);
            matched += usize::from(!want.is_empty());
            paged += usize::from(want.len() > q.limit);

            // 1. Unbounded. Every block that could hold a match is scanned, so
            //    `rows_matched` is the exact size of the match set — which is
            //    the assertion that a Bloom filter pruning a block it should not
            //    have cannot survive.
            let all = Search {
                limit: rows.len() + 1,
                ..q.clone()
            };
            let got = query::search(&root, &all).unwrap();
            assert_eq!(bodies(&got.json), want, "{}", ctx(&all));
            assert_eq!(got.stats.rows_matched, want.len(), "{}", ctx(&all));
            assert!(got.next.is_none(), "{}", ctx(&all));
            assert!(got.stats.blocks_scanned <= got.stats.blocks_total);

            // 2. Limited: the same prefix, and `next` set exactly when there is
            //    reason to believe another page exists.
            let got = query::search(&root, &q).unwrap();
            let head = &want[..want.len().min(q.limit)];
            assert_eq!(bodies(&got.json), head, "{}", ctx(&q));
            assert_eq!(got.next.is_some(), want.len() >= q.limit, "{}", ctx(&q));

            // 3. Paging. Every row exactly once, in order, over as many pages as
            //    it takes — no duplicate across a page boundary, no gap, and a
            //    last page that says it is the last one. Run on a third of the
            //    queries: it costs one search per page and proves nothing new
            //    about the predicate.
            if i % 3 != 0 {
                continue;
            }
            let mut after = None;
            let mut seen: Vec<String> = Vec::new();
            let mut ended = false;
            for _ in 0..=rows.len() {
                let page = query::search(&root, &Search { after, ..q.clone() }).unwrap();
                seen.extend(bodies(&page.json).into_iter().map(str::to_owned));
                after = page.next;
                if after.is_none() {
                    ended = true;
                    break;
                }
            }
            assert!(ended, "paging never terminated: {}", ctx(&q));
            assert_eq!(seen, want, "{}", ctx(&q));
        }
    }
    assert!(matched > 100, "only {matched} queries matched anything");
    assert!(paged > 20, "only {paged} queries needed a second page");
    let _ = std::fs::remove_dir_all(&dir);
}
