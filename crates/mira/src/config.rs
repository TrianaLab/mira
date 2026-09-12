//! Deployment configuration: KYAML with OmegaConf-style interpolation.
//!
//! # Why KYAML
//!
//! Principle 5 is KYAML-first, and this file is where it starts. KYAML is a
//! strict subset of YAML 1.2 — explicit `{}` and `[]`, every string
//! double-quoted, indentation carrying no meaning — so any YAML parser reads it
//! and no YAML parser can guess wrong about it.
//!
//! The guessing is the point. An unquoted scalar is resolved by pattern-match
//! against a table, and *which* table depends on the YAML version and the
//! implementation. The famous case — a replica honestly named `no` becoming the
//! boolean false — is YAML 1.1, and does not happen with the parser below.
//! Measured against that parser, these do:
//!
//! * `node: False` → `Boolean(false)` → the string `"false"`, case flipped.
//! * `node: 0x1f` → `Integer(31)` → the string `"31"`. The text changed.
//! * `node: null` → `Null`, which reads as "key absent", so the default is used
//!   and nothing is reported.
//!
//! A different parser, or the same one a major version later, has a different
//! list. That is the real argument: the correct reading of an unquoted scalar is
//! not a property of the document. Quoting makes it one, for four characters,
//! and a generating model gets the same guarantee a careful human would — which
//! is why the principle exists.
//!
//! Enforcement is [`scalar`]: every value Mira reads out of this file is a
//! string, so anything that arrived as another type is refused at boot with a
//! message saying to quote it. A list or a map never reaches a value at all, so
//! [`check_keys`] refuses those, by path — quoting is not the fix for a shape.
//!
//! # Why there is a config file at all
//!
//! Principle 2c is "self-driving, no tuning knobs", which this appears to
//! violate and does not. The distinction that matters:
//!
//! * A **knob** is a number the engine could work out for itself and instead
//!   asks a human to guess — block size, buffer depth, flush interval, cache
//!   sizes, compaction thresholds. None of those are here, and none of them will
//!   be. They live in `pipeline::Config`, derived, with no path from this file.
//! * **Deployment description** is what the engine cannot know: which addresses
//!   to listen on, which directory is the data directory, how long the retention
//!   policy is, what this replica is called. That is not tuning. Refusing to
//!   accept it does not make an engine self-driving, it makes it unusable.
//!
//! The boundary is structural rather than documentary: this struct has no field
//! that affects how the engine performs, only where it runs — with one
//! deliberate exception, `ingest.wal`, which is not a number to guess but a
//! choice between two correct durability promises that no measurement can make
//! for the operator. Its own doc comment argues that.
//!
//! # Interpolation
//!
//! ```yaml
//! {
//!   "node": "${env:HOSTNAME,mira-0}",  # env var, default after the comma
//!   "storage": {
//!     "dir": "/var/lib/${node}",       # another key, by dotted path
//!     "retention": "7d",
//!   },
//! }
//! ```
//!
//! * `${env:NAME}` — required; a missing variable is a startup error, not an
//!   empty string. Silently defaulting is how a staging cluster ends up writing
//!   to a production bucket.
//! * `${env:NAME,default}` — everything after the first comma is the default,
//!   verbatim, including further `${...}`.
//! * `${dotted.path}` — another key in this file. Resolved recursively; a cycle
//!   is a startup error naming the cycle.
//! * `$${` — a literal `${`.
//!
//! Resolution is lazy: only keys actually read are expanded, so an unused key
//! with a broken reference cannot stop the process from booting.
//!
//! There is deliberately no second `MIRA_*` environment-override mechanism.
//! `${env:...}` already covers every case, explicitly and visibly in one file,
//! and two ways to set the same value is exactly the complexity this is meant to
//! avoid.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::time::Duration;

use yaml_rust2::{Yaml, YamlLoader};

type Error = String;
type Result<T> = std::result::Result<T, Error>;

/// How `${env:NAME}` is resolved, handed to the parser rather than read out of
/// the process.
///
/// Not an abstraction for its own sake — it is what lets the tests supply an
/// environment without writing one. `std::env::set_var` is `unsafe` in edition
/// 2024 because it can reallocate `environ` under a concurrent `getenv`, in any
/// thread, including one inside libc; `cargo test` runs this binary's tests as
/// threads of a single process, and at least two of them read: `term.rs` looks
/// up `MIRA_PTY_CHILD`, and every `tui.rs` test that formats a timestamp reaches
/// `localtime_r`, which reads `TZ`. No lock closes that, because the libc reader
/// will not take it. Not writing does.
type Env<'a> = &'a dyn Fn(&str) -> Option<String>;

#[derive(Debug, Clone)]
pub struct Config {
    /// This replica's name. Hashed into the block directory name so that
    /// replicas sharing a volume cannot collide (see `mira_core::block`).
    pub node: String,
    /// Where OTLP/gRPC listens.
    pub grpc: SocketAddr,
    /// Where OTLP/HTTP, the query API, the MCP endpoint and the web UI listen —
    /// one port, because they are one surface over one set of blocks.
    pub http: SocketAddr,
    /// The block directory. It is the whole manifest: no catalogue, no index
    /// file, nothing outside it to keep in sync.
    pub data_dir: PathBuf,
    /// How long a block is kept. Retention is a delete of whole blocks, so the
    /// oldest data disappears in block-sized steps rather than row by row.
    pub retention: Duration,
    /// The largest export either listener will decode. See
    /// `receiver::Receivers::max_request_bytes` for why it is one number.
    pub max_request_bytes: usize,
    /// How many exports may be queued for one signal's flusher before the next
    /// one has to wait for a slot — and is shed with a 503 only if none frees
    /// up within `pipeline::ADMIT_WAIT`.
    ///
    /// The concurrency limit Mira did not used to have. It was a fixed 128 and
    /// a full queue meant an immediate 503, so a wide collector fleet spent
    /// most of its time being told to retry: that sweep shed 93% of exports at
    /// 96 connections and landed at a third of the two-connection rate. Waiting
    /// briefly for a slot instead (`ADMIT_WAIT`, `pipeline.rs`) took the same
    /// row to nothing shed and double the throughput, and twenty-one
    /// consecutive runs of the whole sweep have refused nothing since. This
    /// knob is the other half — an operator whose fleet is wide can buy queue
    /// depth with memory they have spare.
    ///
    /// It buys queueing, not throughput: the flusher drains at the rate it
    /// drains, and a queue deep enough to hide a permanently overloaded node
    /// just moves the shed into a latency tail. Size it to absorb a burst, not
    /// to avoid a 503. Each slot can hold a decoded export, so the worst case is
    /// this times [`Config::max_request_bytes`] times three signals resident.
    pub queue: usize,
    /// How many flushers a signal runs, or 0 for "one per two cores".
    ///
    /// One shard per core is the sanctioned unit (architecture.md section 4);
    /// this is only here so the number can be pinned when the machine lies
    /// about its core count. `available_parallelism` honours cgroup v1 and v2
    /// CPU quotas, so a container with a quota set needs no help here — but
    /// `cpu.shares`/`cpu.weight` is a relative weight rather than a quota and
    /// reads as the whole machine, a shared host often sets no quota at all, a
    /// non-Linux container runtime leaves nothing to read, and hyperthreads
    /// count as cores. A 96-core host running Mira on two cores' worth of any
    /// of those would otherwise start 48 flushers per signal and publish 48
    /// files per seal window. Set it to the cores the process actually gets,
    /// or to 1 to get the pre-0.0.2 behaviour.
    ///
    /// Shards split `queue`, they do not multiply it: the resident worst case
    /// is the same whatever this is. Capped at `pipeline::MAX_SHARDS`.
    pub shards: usize,
    /// Acknowledge an export once it is a frame in the write-ahead log, rather
    /// than once the block holding it has been published.
    ///
    /// The one durability decision Mira does not make for the operator, and it
    /// is not the tuning knob the module docs above rule out: both settings are
    /// correct, they promise different things, and nothing the engine can
    /// measure says which promise a deployment wants. On, the default, is the
    /// log's: the export survives the process dying, `panic = "abort"`, SIGKILL
    /// and the OOM killer, but not power loss in the last [`WAL_SYNC_PERIOD`],
    /// at a p99 in the microseconds. Off is the block's — acknowledged means
    /// fsynced and renamed, which survives power loss too, at a p99 of 2.6 s
    /// because that is how long a lightly-loaded block takes to fill.
    ///
    /// Read-your-writes holds either way: the open block is queryable (section 4),
    /// so a record is visible from the acknowledgement whether or not it has
    /// been published yet.
    ///
    /// [`WAL_SYNC_PERIOD`]: crate::pipeline::WAL_SYNC_PERIOD
    pub wal: bool,
    /// Store this node's own telemetry in this node, as ordinary metrics.
    ///
    /// Mira already knows everything in `/api/v1/stats`; what it does not do by
    /// default is remember it. On, a task samples those counters every
    /// [`Config::telemetry_interval`] and submits them through the metrics
    /// ingest path like any other exporter would — so `mira.ingest.rows`,
    /// `mira.query.latency_ms` and the rest become series a chart, an alert rule
    /// or an agent can read with no exporter, no scrape target and no second
    /// system to stand up.
    ///
    /// Off by default, because it is not free and the operator should choose to
    /// spend it: the samples are rows, they are subject to
    /// [`Config::retention`] like everything else, and a node storing its own
    /// telemetry is a node whose disk usage no longer goes to zero when nothing
    /// is being sent to it.
    ///
    /// Self-import is the only destination. Shipping these somewhere else is
    /// what an OTLP exporter is for, and Mira is not going to grow a second one
    /// pointed at itself.
    pub self_telemetry: bool,
    /// How often [`Config::self_telemetry`] samples this node's counters.
    ///
    /// A sample is one point per series, so this is the resolution of every
    /// chart drawn from it and also its cost. The default matches what a
    /// collector's own scrape interval usually is; below a second it is
    /// measuring the sampler.
    pub telemetry_interval: Duration,
    /// A KYAML file of alerting rules ([`crate::alert`]), or none.
    ///
    /// Deployment description rather than a knob, and the same argument as the
    /// data directory: what to page on is a thing the engine cannot know. It is
    /// a path rather than an inline section for two reasons — a rules list is a
    /// list of maps, which this file's closed-scalar shape refuses on purpose,
    /// and rules change on a different cadence to addresses, so they belong in
    /// a different file and a different review.
    ///
    /// Absent means alerting is off, which is also the coordination mechanism:
    /// N replicas over one block directory would each page, so exactly one
    /// replica gets this key. See [`crate::alert`].
    pub alerts: Option<PathBuf>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            node: "mira".into(),
            grpc: "0.0.0.0:4317".parse().unwrap(),
            http: "0.0.0.0:4318".parse().unwrap(),
            data_dir: PathBuf::from("./mira-data"),
            retention: Duration::from_secs(7 * 24 * 3600),
            // Eight times axum's default and four times tonic's. A stock
            // collector batches 8192 records, which is already past 2 MiB of
            // spans, and an exporter reads 413 as permanent — so the cost of
            // this being too small is dropped data, while the cost of it being
            // too large is bounded resident bytes per in-flight request.
            max_request_bytes: 16 << 20,
            // What it has always been, kept as the default so that raising it
            // is a decision an operator makes with the sweep in front of them
            // rather than a number that quietly moved under everyone.
            queue: 128,
            // Auto: `pipeline::shard_count` reads the core count at startup.
            shards: 0,
            // On, now that the open block is queryable (section 4). The reason it
            // was off was that acking before the seal let a query miss data the
            // sender had been told was stored; the snapshot closes that, so
            // what is left is a three-orders-of-magnitude better ack latency
            // against a strictly weaker crash promise. That is the trade the
            // overwhelming majority of collectors already assume they have.
            wal: true,
            self_telemetry: false,
            telemetry_interval: Duration::from_secs(15),
            alerts: None,
        }
    }
}

impl Config {
    pub fn load(path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
        Self::parse(&text).map_err(|e| format!("{}: {e}", path.display()))
    }

    pub fn parse(text: &str) -> Result<Self> {
        Self::parse_with(text, &|k| std::env::var(k).ok())
    }

    /// [`Config::parse`] against a supplied environment. See [`Env`].
    fn parse_with(text: &str, env: Env) -> Result<Self> {
        let docs = YamlLoader::load_from_str(text).map_err(|e| e.to_string())?;
        let root = docs.into_iter().next().unwrap_or(Yaml::Null);
        let mut cfg = Config::default();

        if let Some(v) = get(&root, "node", env)? {
            cfg.node = v;
        }
        if let Some(v) = get(&root, "listen.grpc", env)? {
            cfg.grpc = v.parse().map_err(|e| format!("listen.grpc: {e}"))?;
        }
        if let Some(v) = get(&root, "listen.http", env)? {
            cfg.http = v.parse().map_err(|e| format!("listen.http: {e}"))?;
        }
        if let Some(v) = get(&root, "storage.dir", env)? {
            cfg.data_dir = PathBuf::from(v);
        }
        if let Some(v) = get(&root, "storage.retention", env)? {
            cfg.retention = duration(&v).map_err(|e| format!("storage.retention: {e}"))?;
        }
        if let Some(v) = get(&root, "ingest.max_request_bytes", env)? {
            cfg.max_request_bytes =
                bytes(&v).map_err(|e| format!("ingest.max_request_bytes: {e}"))?;
        }
        if let Some(v) = get(&root, "ingest.queue", env)? {
            cfg.queue = positive(&v).map_err(|e| format!("ingest.queue: {e}"))?;
        }
        if let Some(v) = get(&root, "ingest.shards", env)? {
            cfg.shards = whole(&v).map_err(|e| format!("ingest.shards: {e}"))?;
        }
        if let Some(v) = get(&root, "ingest.wal", env)? {
            cfg.wal = boolean(&v).map_err(|e| format!("ingest.wal: {e}"))?;
        }
        if let Some(v) = get(&root, "telemetry.self", env)? {
            cfg.self_telemetry = boolean(&v).map_err(|e| format!("telemetry.self: {e}"))?;
        }
        if let Some(v) = get(&root, "telemetry.interval", env)? {
            cfg.telemetry_interval =
                duration(&v).map_err(|e| format!("telemetry.interval: {e}"))?;
        }
        if let Some(v) = get(&root, "alerts.rules", env)? {
            cfg.alerts = Some(PathBuf::from(v));
        }
        check_keys(&root, "")?;
        Ok(cfg)
    }
}

/// Every path this file may contain, in the order [`Config::parse`] reads them.
const KNOWN: [&str; 12] = [
    "node",
    "listen.grpc",
    "listen.http",
    "storage.dir",
    "storage.retention",
    "ingest.max_request_bytes",
    "ingest.queue",
    "ingest.shards",
    "ingest.wal",
    "telemetry.self",
    "telemetry.interval",
    "alerts.rules",
];

/// Refuse a key Mira does not read, or a shape it cannot read.
///
/// The `get`s above are silent about everything they do not name, so
/// `storage.retension` and a `retention` nested one level too deep both boot
/// happily on the 7-day default and surface a week later as a full disk. A flag
/// Mira does not know is already `unknown flag --nope`; there is no reason a file
/// should be the lenient half of the same interface.
///
/// `lookup` is just as silent about a value of the wrong *shape* — a list where
/// a string belongs, a string where a section belongs — and the outcome is
/// identical: the default, in silence. So the structure is checked here too,
/// where the full path is in hand to name.
///
/// The closed set is also what makes deleting a setting safe: a key that no
/// longer exists becomes a startup error naming it, rather than a value the
/// operator still believes is in effect.
fn check_keys(node: &Yaml, prefix: &str) -> Result<()> {
    let Yaml::Hash(h) = node else { return Ok(()) };
    for (k, v) in h {
        // Keys obey the same rule as values, and for the same reason: `2: x` is
        // a key nobody typed. `scalar` says it with the message that teaches it.
        let path = match (prefix, scalar(k)?.unwrap_or_default()) {
            ("", name) => name,
            (p, name) => format!("{p}.{name}"),
        };
        // A name is acceptable as a whole key or as the *prefix* of one —
        // `listen` is a section only because `listen.grpc` exists. Judging the
        // name rather than waiting for a leaf under it is what makes the set
        // closed: `{ "cluster": {} }` is exactly how an operator writes a
        // section they are about to fill in, and testing leaves only would let
        // it boot in silence and look accepted.
        let known = KNOWN.iter().any(|k| {
            *k == path
                || k.strip_prefix(path.as_str())
                    .is_some_and(|r| r.starts_with('.'))
        });
        if !known {
            return Err(format!(
                "unknown key {path:?}. Mira reads exactly {}; see https://miradb.dev/config/",
                KNOWN.join(", ")
            ));
        }
        // Shape has to match the name: a section holds a map, a setting holds a
        // scalar. `scalar` judges the scalar itself at read time; the two
        // collection types never reach it, because `lookup` walks past a list
        // and stops inside a map, returning "absent" for both.
        let leaf = KNOWN.contains(&path.as_str());
        match v {
            Yaml::Hash(_) => {
                // No known key is a prefix of another, so a map under a setting
                // holds nothing but keys nested a level too deep, and the
                // recursion is what names them. An empty one has nothing to
                // name, which is why the check below is not unreachable.
                check_keys(v, &path)?;
                if leaf {
                    return Err(format!("{path}: expected a string, found a map"));
                }
            }
            Yaml::Array(_) => return Err(format!("{path}: expected a string, found a list")),
            // `null` is "absent, use the default" at every level, for the reason
            // `scalar` gives: it is how a templating layer writes "not set".
            Yaml::Null => {}
            // A scalar where a section belongs: `{ "listen": "0.0.0.0:4317" }`
            // reads as neither `listen.grpc` nor `listen.http`.
            _ if !leaf => {
                return Err(format!(
                    "{path}: expected a map of settings, found a value; see https://miradb.dev/config/"
                ));
            }
            _ => {}
        }
    }
    Ok(())
}

fn lookup<'a>(root: &'a Yaml, path: &str) -> Option<&'a Yaml> {
    let mut node = root;
    for segment in path.split('.') {
        node = match node {
            Yaml::Hash(h) => h.get(&Yaml::String(segment.to_owned()))?,
            _ => return None,
        };
    }
    Some(node)
}

/// Every value Mira reads from the config file is a string — an address, a
/// path, a name, a duration. So the rule is simply that it must have arrived as
/// one.
///
/// `Ok(None)` means "no scalar here": a missing key, an explicit `null`, or a
/// collection — and a collection at a key Mira reads is refused by
/// [`check_keys`], which knows the path to name it by. `Err`
/// means the key is present and the parser resolved it to some other type,
/// which is the ambiguity KYAML exists to remove. Coercing back with
/// `to_string()` is what makes `0x1f` silently become `31`; refusing costs the
/// author two quote characters and cannot be wrong.
fn scalar(y: &Yaml) -> Result<Option<String>> {
    match y {
        Yaml::String(s) => Ok(Some(s.clone())),
        // The source text is already gone by the time we see this — `0x1f` and
        // `31` are the same `Integer(31)` — so the message teaches the rule
        // instead of quoting text we no longer have.
        Yaml::Integer(_) | Yaml::Real(_) | Yaml::Boolean(_) => Err(format!(
            "YAML read this as a {}, not a string; quote it \
             (KYAML quotes every string, and every value here is one)",
            match y {
                Yaml::Integer(_) => "number",
                Yaml::Real(_) => "float",
                _ => "boolean",
            }
        )),
        _ => Ok(None),
    }
}

fn get(root: &Yaml, path: &str, env: Env) -> Result<Option<String>> {
    let found = match lookup(root, path) {
        Some(y) => scalar(y).map_err(|e| format!("{path}: {e}"))?,
        None => None,
    };
    match found {
        None => Ok(None),
        Some(raw) => resolve(root, &raw, &mut vec![path.to_owned()], env).map(Some),
    }
}

/// Expand every `${...}` in `raw`. `stack` carries the config paths currently
/// being resolved, so a reference cycle is reported rather than overflowing.
fn resolve(root: &Yaml, raw: &str, stack: &mut Vec<String>, env: Env) -> Result<String> {
    let mut out = String::with_capacity(raw.len());
    let bytes = raw.as_bytes();
    let mut i = 0;

    while i < bytes.len() {
        // `$${` is an escaped literal `${`.
        if raw[i..].starts_with("$${") {
            out.push_str("${");
            i += 3;
            continue;
        }
        if !raw[i..].starts_with("${") {
            let ch = raw[i..].chars().next().unwrap();
            out.push(ch);
            i += ch.len_utf8();
            continue;
        }
        let rest = &raw[i + 2..];
        let end = closing_brace(rest).ok_or_else(|| format!("unterminated `${{` in {raw:?}"))?;
        out.push_str(&expand(root, &rest[..end], stack, env)?);
        i += 2 + end + 1;
    }
    Ok(out)
}

/// Offset of the `}` that closes a `${` already consumed.
///
/// The matching brace, not the first one. `${env:A,${env:B,fallback}}` is a
/// documented shape — a default that is itself an expression — and taking
/// `find('}')` splits it in the middle: with `A` unset it complains about a `${`
/// the author did terminate, and with `A` set it yields the value with a stray
/// `}` welded on. That second one is the failure that matters, because `node`
/// ends up in every block directory name.
fn closing_brace(s: &str) -> Option<usize> {
    let b = s.as_bytes();
    let (mut depth, mut i) = (0usize, 0);
    while i < b.len() {
        // A multi-byte character's continuation bytes are all ≥ 0x80, so
        // scanning bytes for these three ASCII ones cannot land inside one.
        match b[i] {
            b'$' if b.get(i + 1) == Some(&b'{') => {
                depth += 1;
                i += 1;
            }
            b'}' if depth == 0 => return Some(i),
            b'}' => depth -= 1,
            _ => {}
        }
        i += 1;
    }
    None
}

fn expand(root: &Yaml, expr: &str, stack: &mut Vec<String>, env: Env) -> Result<String> {
    if let Some(rest) = expr.strip_prefix("env:") {
        let (name, default) = match rest.split_once(',') {
            Some((n, d)) => (n.trim(), Some(d)),
            None => (rest.trim(), None),
        };
        return match (env(name), default) {
            (Some(v), _) => Ok(v),
            (None, Some(d)) => resolve(root, d, stack, env),
            (None, None) => Err(format!(
                "${{env:{name}}} is not set and has no default (write `${{env:{name},<default>}}`)"
            )),
        };
    }

    let path = expr.trim();
    if stack.iter().any(|p| p == path) {
        stack.push(path.to_owned());
        return Err(format!("reference cycle: {}", stack.join(" -> ")));
    }
    let raw = match lookup(root, path) {
        Some(y) => scalar(y).map_err(|e| format!("${{{path}}}: {e}"))?,
        None => None,
    }
    .ok_or_else(|| format!("${{{path}}} does not name a scalar key"))?;

    stack.push(path.to_owned());
    let v = resolve(root, &raw, stack, env)?;
    stack.pop();
    Ok(v)
}

/// `true` or `false`, and nothing else.
///
/// Not YAML 1.1's dozen spellings. `on`, `yes` and `y` are why a Norwegian
/// country code parses as `false`, and a config file that accepts eleven ways
/// to say the same thing has ten ways to typo it into the other one.
pub fn boolean(s: &str) -> Result<bool> {
    match s.trim() {
        "true" => Ok(true),
        "false" => Ok(false),
        other => Err(format!("{other:?} is not `true` or `false`")),
    }
}

/// A count of things, which must be at least one.
///
/// Not [`bytes()`]: a queue depth of `4k` would read as 4,096 there and mean 4,000
/// to whoever typed it, and a slot is not a byte. Zero is refused rather than
/// silently meaning "rendezvous channel", which is what `mpsc::channel(0)` would
/// panic on and what an operator typing it would never intend.
pub fn positive(s: &str) -> Result<usize> {
    match whole(s)? {
        0 => Err("must be at least 1".into()),
        n => Ok(n),
    }
}

/// A count of things where zero is an answer rather than a mistake — see
/// [`Config::shards`], where it means "ask the machine".
pub fn whole(s: &str) -> Result<usize> {
    s.trim()
        .parse::<usize>()
        .map_err(|_| format!("{s:?} is not a whole number"))
}

/// `500ms`, `30s`, `5m`, `2h`, `7d`. A bare number is seconds.
pub fn duration(s: &str) -> Result<Duration> {
    let s = s.trim();
    let split = s.len()
        - s.chars()
            .rev()
            .take_while(|c| c.is_ascii_alphabetic())
            .count();
    let (n, unit) = s.split_at(split);
    let n: u64 = n
        .trim()
        .parse()
        .map_err(|_| format!("{s:?} is not a duration like `7d` or `500ms`"))?;
    let scale = match unit {
        "ms" => return Ok(Duration::from_millis(n)),
        "" | "s" => 1,
        "m" => 60,
        "h" => 3600,
        "d" => 86_400,
        other => return Err(format!("unknown duration unit {other:?} in {s:?}")),
    };
    Ok(Duration::from_secs(n * scale))
}

/// `4MiB`, `512k`, `1048576`. Binary units, because every other size in this
/// system — page, block, mmap — is binary and a `MB` that meant 10^6 next to a
/// block size that meant 2^20 would be a trap.
pub fn bytes(s: &str) -> Result<usize> {
    let s = s.trim();
    let split = s.len()
        - s.chars()
            .rev()
            .take_while(|c| c.is_ascii_alphabetic())
            .count();
    let (n, unit) = s.split_at(split);
    let n: usize = n
        .trim()
        .parse()
        .map_err(|_| format!("{s:?} is not a size like `4MiB` or `1048576`"))?;
    let shift = match unit.to_ascii_lowercase().as_str() {
        "" | "b" => 0,
        "k" | "kb" | "kib" => 10,
        "m" | "mb" | "mib" => 20,
        "g" | "gb" | "gib" => 30,
        other => return Err(format!("unknown size unit {other:?} in {s:?}")),
    };
    n.checked_shl(shift)
        .filter(|v| v >> shift == n)
        .ok_or_else(|| format!("{s:?} overflows a usize"))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The environment these tests parse against. A literal, not the process's:
    /// see [`Env`] for why writing the real one is not an option.
    fn env(k: &str) -> Option<String> {
        match k {
            "MIRA_TEST_HOST" => Some("node-7".to_owned()),
            _ => None,
        }
    }

    /// Written in KYAML, and deliberately so: this doubles as the check that
    /// `yaml-rust2` really accepts the house format — explicit `{}` and `[]`,
    /// every string quoted, trailing commas, indentation that means nothing.
    /// The trailing commas are the part worth verifying rather than assuming;
    /// YAML 1.2 permits them in flow collections but plenty of parsers do not.
    #[test]
    fn interpolation_covers_env_reference_default_and_escape() {
        let cfg = Config::parse_with(
            r#"{
  "node": "${env:MIRA_TEST_HOST}",
      "listen": { "grpc": "0.0.0.0:5317", },
  "storage": {
    "dir": "/var/lib/${node}/${env:MIRA_TEST_MISSING,fallback}",
    "retention": "36h",
  },
  "alerts": { "rules": "/etc/${node}/rules.yaml" },
}"#,
            &env,
        )
        .unwrap();

        assert_eq!(cfg.node, "node-7");
        assert_eq!(cfg.grpc.port(), 5317);
        assert_eq!(cfg.data_dir, PathBuf::from("/var/lib/node-7/fallback"));
        assert_eq!(cfg.retention, Duration::from_secs(36 * 3600));
        // Every path in the file interpolates, including the one that is read
        // last: a rules file under `/etc/${node}` is how one image serves a
        // fleet, and a literal `${node}` there is a boot that finds no rules.
        assert_eq!(
            cfg.alerts,
            Some(PathBuf::from("/etc/node-7/rules.yaml")),
            "alerts.rules is read and interpolated"
        );
        assert_eq!(Config::default().alerts, None, "and is off by default");
        // Unset keys keep their defaults rather than becoming empty.
        assert_eq!(cfg.http.port(), 4318);

        let esc = Config::parse_with(r#"{ "node": "$${env:NOPE}" }"#, &env).unwrap();
        assert_eq!(esc.node, "${env:NOPE}");

        // Defaults nest, which only works if the scan finds the *matching*
        // brace. Taking the first one used to boot the node called `alpha}` —
        // a stray character in every block directory this replica writes.
        let chain = r#"{ "node": "${env:MIRA_TEST_MISSING,${env:MIRA_TEST_HOST,last}}" }"#;
        assert_eq!(Config::parse_with(chain, &env).unwrap().node, "node-7");
        let all_unset =
            r#"{ "node": "${env:MIRA_TEST_MISSING,${env:MIRA_TEST_ALSO_MISSING,last}}" }"#;
        assert_eq!(Config::parse_with(all_unset, &env).unwrap().node, "last");
        // And the outer value still wins without picking up the inner brace.
        let outer = r#"{ "node": "${env:MIRA_TEST_HOST,${env:MIRA_TEST_MISSING,last}}" }"#;
        assert_eq!(Config::parse_with(outer, &env).unwrap().node, "node-7");
    }

    /// `ingest.wal` is the only setting that changes what an acknowledgement
    /// promises, so it is the last one that should accept a fuzzy spelling.
    /// KYAML has already made the value a quoted string by the time it gets
    /// here; what is refused is YAML 1.1's other ten ways to write a boolean,
    /// each of which is a way to typo the durability contract into its
    /// opposite.
    #[test]
    fn the_log_is_on_unless_it_is_spelled_false() {
        assert!(Config::default().wal);
        assert!(
            Config::parse_with(r#"{ "ingest": { "wal": "true" } }"#, &env)
                .unwrap()
                .wal
        );
        assert!(
            !Config::parse_with(r#"{ "ingest": { "wal": "false" } }"#, &env)
                .unwrap()
                .wal
        );
        for fuzzy in [r#""yes""#, r#""on""#, r#""1""#, r#""True""#] {
            let doc = format!(r#"{{ "ingest": {{ "wal": {fuzzy} }} }}"#);
            let e = Config::parse_with(&doc, &env).unwrap_err();
            assert!(e.contains("ingest.wal"), "{fuzzy} was accepted: {e}");
        }
    }

    /// The coercions KYAML exists to kill. Each of these used to be accepted,
    /// stringified back into a *different* word, and used as the node name and
    /// therefore as part of the block directory path.
    #[test]
    fn scalars_yaml_guessed_at_are_refused_rather_than_stringified_back() {
        // `0x1f` is the worst of them: it survives as the string "31".
        let e = Config::parse("node: 0x1f").unwrap_err();
        assert!(e.contains("node:") && e.contains("quote"), "{e}");

        // `False` comes back as "false", with the case quietly changed.
        let e = Config::parse("node: False").unwrap_err();
        assert!(e.contains("boolean") && e.contains("quote"), "{e}");

        let e = Config::parse("storage:\n  dir: 1.10").unwrap_err();
        assert!(e.contains("storage.dir:") && e.contains("quote"), "{e}");

        // Quoted, each means exactly what it says.
        assert_eq!(Config::parse(r#"{ "node": "0x1f" }"#).unwrap().node, "0x1f");

        // This parser is YAML 1.2, so `no` was never the boolean anyway. Worth
        // pinning: the reason to quote is that the rule varies by parser, not
        // that this particular one gets `no` wrong.
        assert_eq!(Config::parse("node: no").unwrap().node, "no");
    }

    #[test]
    fn bad_config_fails_at_boot_rather_than_silently() {
        // A missing env var with no default must not become "".
        let e = Config::parse_with("node: ${env:MIRA_MISSING}", &env).unwrap_err();
        assert!(e.contains("is not set"), "{e}");

        let e = Config::parse("node: ${a}\na: ${node}").unwrap_err();
        assert!(e.contains("cycle"), "{e}");

        // A reference to a key that is not in the file, and one to a key that
        // is a map rather than a scalar. Both used to interpolate to nothing,
        // so `${storage.dr}` for `${storage.dir}` booted a node whose data
        // directory was the empty string — the process's cwd.
        for text in ["node: ${nope}", "node: ${storage}\nstorage:\n  dir: /x"] {
            let e = Config::parse(text).unwrap_err();
            assert!(e.contains("does not name a scalar key"), "{text}: {e}");
        }

        let e = Config::parse("storage:\n  retention: 7 fortnights").unwrap_err();
        assert!(e.contains("duration"), "{e}");

        assert!(
            Config::parse("node: ${env:X")
                .unwrap_err()
                .contains("unterminated")
        );
        // A nested default that never closes is still unterminated, rather than
        // swallowing the rest of the file quietly.
        assert!(
            Config::parse("node: ${env:X,${env:Y,z}")
                .unwrap_err()
                .contains("unterminated")
        );
    }

    /// A key Mira does not read is a startup error, not a shrug.
    ///
    /// The failure this prevents is the quietest one in the system: `retension`
    /// for `retention` keeps the 7-day default, says nothing, and is diagnosed
    /// weeks later as a disk that will not stop growing.
    #[test]
    fn a_key_mira_does_not_read_refuses_to_start() {
        let e = Config::parse(r#"{ "storage": { "retension": "30d" } }"#).unwrap_err();
        assert!(e.contains("storage.retension"), "{e}");

        // Right name, wrong depth. Reported by its full path, because that is
        // the thing that is wrong about it.
        let e = Config::parse(r#"{ "retention": "30d" }"#).unwrap_err();
        assert!(e.contains("unknown key \"retention\""), "{e}");
        let e = Config::parse(r#"{ "storage": { "dir": { "path": "/x" } } }"#).unwrap_err();
        assert!(e.contains("storage.dir.path"), "{e}");

        // A setting that was deleted becomes loud for free — no special case.
        // The section is what no longer exists, so that is what the error names.
        let e = Config::parse(r#"{ "cluster": { "peers": "a:1" } }"#).unwrap_err();
        assert!(e.contains("unknown key \"cluster\""), "{e}");

        // And with nothing in it yet, which is how an operator writes a section
        // they are about to fill in — so it is the reading most likely to be
        // believed, and it used to be the one that booted.
        let e = Config::parse(r#"{ "cluster": {} }"#).unwrap_err();
        assert!(e.contains("unknown key \"cluster\""), "{e}");

        // Keys are held to the same quoting rule as values.
        let e = Config::parse("2: x").unwrap_err();
        assert!(e.contains("quote"), "{e}");

        // And the shipped shape passes, including sections with nothing in them.
        Config::parse(r#"{ "node": "a", "listen": {}, "ingest": { "max_request_bytes": "1k" } }"#)
            .unwrap();
    }

    /// A list or a map where a value belongs is refused, not ignored.
    ///
    /// `lookup` returns "absent" for both — it walks past a list and stops
    /// inside a map — so each of these used to boot on the default: the wrong
    /// listen address, or worse, the wrong data directory, with nothing said.
    #[test]
    fn a_value_of_the_wrong_shape_refuses_to_start() {
        let e = Config::parse(r#"{ "node": ["a"] }"#).unwrap_err();
        assert!(e.contains("node: expected a string, found a list"), "{e}");

        // The empty map is the one the leaf-only check missed.
        let e = Config::parse(r#"{ "storage": { "dir": {} } }"#).unwrap_err();
        assert!(
            e.contains("storage.dir: expected a string, found a map"),
            "{e}"
        );

        // A section given a value instead of its settings.
        let e = Config::parse(r#"{ "listen": "0.0.0.0:4317" }"#).unwrap_err();
        assert!(e.contains("listen: expected a map of settings"), "{e}");

        // `null` still means "absent, use the default" — a key present-but-null
        // is how a templating layer says "not set", at either level.
        let cfg = Config::parse(r#"{ "node": null, "listen": null }"#).unwrap();
        assert_eq!(cfg.node, "mira");
        assert_eq!(cfg.http.port(), 4318);
    }

    /// Binary units throughout, and no silent second meaning for `MB`.
    #[test]
    fn sizes_parse_in_binary_units_or_not_at_all() {
        assert_eq!(bytes("1048576"), Ok(1 << 20));
        assert_eq!(bytes(" 512k "), Ok(512 << 10));
        assert_eq!(bytes("4MiB"), Ok(4 << 20));
        // `MB` is the same as `MiB` here rather than 10^6, because a config
        // where `block: 4MiB` and `request: 4MB` differed by 5% would be read
        // as equal by everyone.
        assert_eq!(bytes("4MB"), bytes("4MiB"));
        assert_eq!(bytes("2g"), Ok(2 << 30));

        assert!(bytes("4 fortnights").unwrap_err().contains("unit"));
        assert!(bytes("MiB").unwrap_err().contains("size"));
        assert!(bytes("-1").unwrap_err().contains("size"));
        // The shift is checked, so a plausible typo is an error and not a wrap
        // to some small number that then silently truncates every export.
        assert!(bytes("99999999999g").unwrap_err().contains("overflow"));

        let cfg = Config::parse(r#"{ "ingest": { "max_request_bytes": "32MiB" } }"#).unwrap();
        assert_eq!(cfg.max_request_bytes, 32 << 20);
        let e = Config::parse(r#"{ "ingest": { "max_request_bytes": "big" } }"#).unwrap_err();
        assert!(e.contains("ingest.max_request_bytes"), "{e}");
    }

    /// A queue depth is a count of slots, and the two ways of writing a number
    /// that [`bytes`] accepts are both wrong for it: `4k` would be 4,096 slots
    /// to the parser and 4,000 to whoever typed it, and zero would be a
    /// rendezvous channel nobody asks for on purpose.
    #[test]
    fn a_queue_depth_is_a_count_of_slots_and_not_a_size() {
        assert_eq!(positive("2048"), Ok(2048));
        assert_eq!(positive("  1  "), Ok(1));
        assert!(positive("0").unwrap_err().contains("at least 1"));
        assert!(positive("-1").unwrap_err().contains("whole number"));
        assert!(positive("4k").unwrap_err().contains("whole number"));

        let cfg = Config::parse(r#"{ "ingest": { "queue": "512" } }"#).unwrap();
        assert_eq!(cfg.queue, 512);
        assert_eq!(Config::default().queue, 128);
        let e = Config::parse(r#"{ "ingest": { "queue": "0" } }"#).unwrap_err();
        assert!(e.contains("ingest.queue"), "{e}");
    }

    /// Off by default: a node that stores its own telemetry is writing to the
    /// disk it is being measured on, and that is a decision, not a default.
    #[test]
    fn self_telemetry_is_off_until_it_is_turned_on() {
        let d = Config::default();
        assert!(!d.self_telemetry);
        assert_eq!(d.telemetry_interval, Duration::from_secs(15));

        let cfg =
            Config::parse(r#"{ "telemetry": { "self": "true", "interval": "1m" } }"#).unwrap();
        assert!(cfg.self_telemetry);
        assert_eq!(cfg.telemetry_interval, Duration::from_secs(60));

        // And `false` in a file has to be able to turn off what an inherited
        // file turned on, which is the reason it is a key and not only a flag.
        let cfg = Config::parse(r#"{ "telemetry": { "self": "false" } }"#).unwrap();
        assert!(!cfg.self_telemetry);

        let e = Config::parse(r#"{ "telemetry": { "self": "yes please" } }"#).unwrap_err();
        assert!(e.contains("telemetry.self"), "{e}");
        let e = Config::parse(r#"{ "telemetry": { "interval": "soon" } }"#).unwrap_err();
        assert!(e.contains("telemetry.interval"), "{e}");
    }
}
