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
//! message saying to quote it.
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
//!   policy is, what this replica is called, where its peers are. That is not
//!   tuning. Refusing to accept it does not make an engine self-driving, it makes
//!   it unusable.
//!
//! The boundary is structural rather than documentary: this struct has no field
//! that affects how the engine performs, only where it runs.
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

#[derive(Debug, Clone)]
pub struct Config {
    /// This replica's name. Hashed into the block directory name so that
    /// replicas sharing a volume cannot collide (see `mira_core::block`).
    pub node: String,
    pub grpc: SocketAddr,
    pub http: SocketAddr,
    pub data_dir: PathBuf,
    pub retention: Duration,
    /// Peer addresses for scatter-gather queries. Empty means single-node.
    pub peers: Vec<String>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            node: "mira".into(),
            grpc: "0.0.0.0:4317".parse().unwrap(),
            http: "0.0.0.0:4318".parse().unwrap(),
            data_dir: PathBuf::from("./mira-data"),
            retention: Duration::from_secs(7 * 24 * 3600),
            peers: Vec::new(),
        }
    }
}

impl Config {
    pub fn load(path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
        Self::parse(&text).map_err(|e| format!("{}: {e}", path.display()))
    }

    pub fn parse(text: &str) -> Result<Self> {
        let docs = YamlLoader::load_from_str(text).map_err(|e| e.to_string())?;
        let root = docs.into_iter().next().unwrap_or(Yaml::Null);
        let mut cfg = Config::default();

        if let Some(v) = get(&root, "node")? {
            cfg.node = v;
        }
        if let Some(v) = get(&root, "listen.grpc")? {
            cfg.grpc = v.parse().map_err(|e| format!("listen.grpc: {e}"))?;
        }
        if let Some(v) = get(&root, "listen.http")? {
            cfg.http = v.parse().map_err(|e| format!("listen.http: {e}"))?;
        }
        if let Some(v) = get(&root, "storage.dir")? {
            cfg.data_dir = PathBuf::from(v);
        }
        if let Some(v) = get(&root, "storage.retention")? {
            cfg.retention = duration(&v).map_err(|e| format!("storage.retention: {e}"))?;
        }
        // Peers may be a YAML list or one comma-separated string, because both
        // are natural: a list in a hand-written file, a string from a `${env:}`.
        cfg.peers = match lookup(&root, "cluster.peers") {
            Some(Yaml::Array(items)) => {
                let mut out = Vec::new();
                for it in items {
                    let s = scalar(it)
                        .map_err(|e| format!("cluster.peers: {e}"))?
                        .ok_or("cluster.peers: non-scalar list entry")?;
                    out.push(resolve(&root, &s, &mut Vec::new())?);
                }
                out
            }
            _ => get(&root, "cluster.peers")?
                .unwrap_or_default()
                .split(',')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_owned)
                .collect(),
        };
        Ok(cfg)
    }
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
/// `Ok(None)` means "not a scalar at all": a missing key, a map, a list. `Err`
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

fn get(root: &Yaml, path: &str) -> Result<Option<String>> {
    let found = match lookup(root, path) {
        Some(y) => scalar(y).map_err(|e| format!("{path}: {e}"))?,
        None => None,
    };
    match found {
        None => Ok(None),
        Some(raw) => resolve(root, &raw, &mut vec![path.to_owned()]).map(Some),
    }
}

/// Expand every `${...}` in `raw`. `stack` carries the config paths currently
/// being resolved, so a reference cycle is reported rather than overflowing.
fn resolve(root: &Yaml, raw: &str, stack: &mut Vec<String>) -> Result<String> {
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
        let end = rest
            .find('}')
            .ok_or_else(|| format!("unterminated `${{` in {raw:?}"))?;
        out.push_str(&expand(root, &rest[..end], stack)?);
        i += 2 + end + 1;
    }
    Ok(out)
}

fn expand(root: &Yaml, expr: &str, stack: &mut Vec<String>) -> Result<String> {
    if let Some(rest) = expr.strip_prefix("env:") {
        let (name, default) = match rest.split_once(',') {
            Some((n, d)) => (n.trim(), Some(d)),
            None => (rest.trim(), None),
        };
        return match (std::env::var(name), default) {
            (Ok(v), _) => Ok(v),
            (Err(_), Some(d)) => resolve(root, d, stack),
            (Err(_), None) => Err(format!(
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
    let v = resolve(root, &raw, stack)?;
    stack.pop();
    Ok(v)
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

#[cfg(test)]
mod tests {
    use super::*;

    // SAFETY: set_var is unsafe in edition 2024 because it races other threads
    // reading the environment. These tests are the only reader here and cargo
    // runs each test binary's threads against a fresh process.
    fn set(k: &str, v: &str) {
        unsafe { std::env::set_var(k, v) }
    }

    /// Written in KYAML, and deliberately so: this doubles as the check that
    /// `yaml-rust2` really accepts the house format — explicit `{}` and `[]`,
    /// every string quoted, trailing commas, indentation that means nothing.
    /// The trailing commas are the part worth verifying rather than assuming;
    /// YAML 1.2 permits them in flow collections but plenty of parsers do not.
    #[test]
    fn interpolation_covers_env_reference_default_and_escape() {
        set("MIRA_TEST_HOST", "node-7");
        let cfg = Config::parse(
            r#"{
  "node": "${env:MIRA_TEST_HOST}",
      "listen": { "grpc": "0.0.0.0:5317", },
  "storage": {
    "dir": "/var/lib/${node}/${env:MIRA_TEST_MISSING,fallback}",
    "retention": "36h",
  },
  "cluster": { "peers": ["a:1", "b:2",], },
}"#,
        )
        .unwrap();

        assert_eq!(cfg.node, "node-7");
        assert_eq!(cfg.grpc.port(), 5317);
        assert_eq!(cfg.data_dir, PathBuf::from("/var/lib/node-7/fallback"));
        assert_eq!(cfg.retention, Duration::from_secs(36 * 3600));
        assert_eq!(cfg.peers, vec!["a:1", "b:2"]);
        // Unset keys keep their defaults rather than becoming empty.
        assert_eq!(cfg.http.port(), 4318);

        let esc = Config::parse(r#"{ "node": "$${env:NOPE}" }"#).unwrap();
        assert_eq!(esc.node, "${env:NOPE}");
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
        let e = Config::parse("node: ${env:MIRA_DEFINITELY_UNSET_XYZ}").unwrap_err();
        assert!(e.contains("is not set"), "{e}");

        let e = Config::parse("node: ${a}\na: ${node}").unwrap_err();
        assert!(e.contains("cycle"), "{e}");

        let e = Config::parse("storage:\n  retention: 7 fortnights").unwrap_err();
        assert!(e.contains("duration"), "{e}");

        assert!(
            Config::parse("node: ${env:X")
                .unwrap_err()
                .contains("unterminated")
        );
    }

    #[test]
    fn peers_accept_a_list_as_well_as_a_string() {
        let cfg = Config::parse("cluster:\n  peers:\n    - a:1\n    - b:2\n").unwrap();
        assert_eq!(cfg.peers, vec!["a:1", "b:2"]);
        assert!(Config::default().peers.is_empty());
    }
}
