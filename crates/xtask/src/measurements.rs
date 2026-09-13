//! Keep every published performance number tied to the run that produced it.
//!
//! [`crate::drift`] already solved this shape for two numbers — the crate count
//! and the binary size — and the lesson it encodes is in its module docs: the
//! failure is never that someone lies, it is that someone re-measures, fixes the
//! README, and leaves the other four sites quoting the old figure forever.
//!
//! `measurements.kyaml` is the same idea for everything a load test produces.
//! One entry per quantity, every site that spells it listed beside it, and three
//! things that can be done with the file:
//!
//! | | |
//! |---|---|
//! | `measurements` | every site still contains the value it is supposed to, and the generated table in the contract page is current |
//! | `measurements render` | rewrite that table from the registry |
//! | `measurements ingest RUN.json [--write]` | compare a run's output against the registry, name every value that moved and every site that now lies, and optionally update the values |
//!
//! `ingest --write` updates values and stops there. It does **not** rewrite the
//! prose at the sites, and that is deliberate rather than unfinished: the
//! sentence around a number is usually a claim about it — "worth between a
//! quarter and two fifths", "a 26% fall" — and a formatter that swapped the digits
//! and left the claim would produce a document that passes this check and is
//! wrong. Naming the sites is the part a machine can do correctly.

use std::collections::BTreeMap;

use yaml_rust2::Yaml;

use crate::reference::inject;
use crate::util::{Failures, line_of, read, read_or_exit};

const REGISTRY: &str = "measurements.kyaml";

/// The page the generated table lands on. It is also the page every `measures`
/// string is written for, so the two move together.
const CONTRACT: &str = "docs/internals/measurement.md";

/// The marker name inside [`CONTRACT`]. Same idiom as `xtask reference`.
const MARKER: &str = "measurement-registry";

/// One published quantity.
struct Measurement {
    key: String,
    /// The value as the registry spells it, not as an `f64`. `2.23` and `2.230`
    /// are the same number and only one of them is the one four documents say,
    /// so the text is the contract and the parse is only a validation.
    literal: String,
    unit: String,
    measures: String,
    provenance: String,
    /// How prose spells the value, and where. Several renderings per value is
    /// normal — `886,147` in the sweep table, `886k` in the README — and every
    /// one of them is checked.
    quoted: Vec<(String, Vec<String>)>,
}

pub fn check() -> bool {
    let mut f = Failures::default();
    let all = load(&mut f);
    check_sites(&all, &mut f);
    check_rendered(&all, &mut f);

    if !f.report("measurement check(s) failed") {
        return false;
    }
    let sites: usize = all
        .iter()
        .flat_map(|m| &m.quoted)
        .map(|(_, s)| s.len())
        .sum();
    println!("{} measurements, {sites} sites, all current.", all.len());
    true
}

pub fn render() -> bool {
    let mut f = Failures::default();
    let all = load(&mut f);
    if !f.is_empty() {
        return f.report("measurement render failed");
    }
    inject(CONTRACT, MARKER, &table(&all), &mut f);
    f.report("measurement render failed")
}

/// Compare a run against the registry.
///
/// The run is JSON lines — one flat object of `key` to number per line, which is
/// what `loadgen --emit` appends. JSON is a YAML subset, so the same parser reads
/// each line. Keys the registry does not know are reported rather than ignored:
/// an emitter that renames a key would otherwise go quiet.
pub fn ingest(path: &str, write: bool) -> bool {
    let mut f = Failures::default();
    let mut all = load(&mut f);
    if !f.is_empty() {
        return f.report("measurement ingest failed");
    }

    let text = match read(path) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("error: {e}");
            return false;
        }
    };
    let Some(run) = passes(path, &text, &mut f) else {
        return f.report("measurement ingest failed");
    };

    let known: BTreeMap<&str, usize> = all
        .iter()
        .enumerate()
        .map(|(i, m)| (m.key.as_str(), i))
        .collect();
    // A sweep measures every shape; the documents publish some of them. So an
    // emitted `ingest.cores.conns96` with no entry is the normal case, and the
    // thing actually worth failing on is a key whose *stem* nobody knows — which
    // is what a renamed emitter produces.
    let stems: std::collections::BTreeSet<&str> = known.keys().map(|k| stem(k)).collect();
    let mut moved: Vec<(usize, f64)> = Vec::new();
    let mut unpublished: Vec<&str> = Vec::new();
    for (k, v) in &run {
        let v = median(v);
        match known.get(k.as_str()) {
            None if stems.contains(stem(k)) => unpublished.push(k),
            None => f.fail(format!(
                "{path} emits `{k}`, and nothing in {REGISTRY} measures \
                 `{}`. Either the emitter renamed a key or the registry is \
                 missing an entry — both are worth a line in the diff.",
                stem(k)
            )),
            // Exactly equal, not within a tolerance. A tolerance here would be a
            // second opinion about how precise the docs are, and the registry
            // already states that: the literal carries its own decimals.
            Some(&i) if format(v, &all[i].literal) != all[i].literal => moved.push((i, v)),
            Some(_) => {}
        }
    }
    if !unpublished.is_empty() {
        println!(
            "{} measured but not published: {}",
            unpublished.len(),
            unpublished.join(", ")
        );
    }
    if !f.is_empty() {
        return f.report("measurement ingest failed");
    }

    let passes = run.values().map(Vec::len).max().unwrap_or(0);
    if moved.is_empty() {
        println!(
            "{} keys over {passes} pass(es) in {path}, none moved.",
            run.len()
        );
        return true;
    }

    println!(
        "\n{} of {} measurements moved ({passes} pass(es) in {path}):\n",
        moved.len(),
        run.len()
    );
    for &(i, v) in &moved {
        let m = &all[i];
        let new = format(v, &m.literal);
        println!("  {} : {} -> {new} {}", m.key, m.literal, m.unit);
        for (as_, sites) in &m.quoted {
            for rel in sites {
                for line in lines_containing(rel, as_) {
                    println!("      {rel}:{line}  still says `{as_}`");
                }
            }
        }
        println!();
    }
    if !write {
        println!(
            "Nothing written. Re-run with --write to update {REGISTRY}; the sites \
             above are yours, because the sentence around a number is a claim \
             about it and this tool cannot read one."
        );
        return false;
    }
    for (i, v) in moved {
        let new = format(v, &all[i].literal);
        if !rewrite(&all[i].key, &new) {
            f.fail(format!("could not rewrite {} in {REGISTRY}", all[i].key));
        }
        all[i].literal = new;
    }
    if !f.is_empty() {
        return f.report("measurement ingest failed");
    }
    println!("{REGISTRY} updated. `make measurements-check` will now name the sites.");
    inject(CONTRACT, MARKER, &table(&all), &mut Failures::default());
    true
}

// ---------------------------------------------------------------------------

/// Every reading in a run file, grouped by key.
///
/// One JSON object per line, because a sweep is several passes over several
/// shapes and `loadgen --emit` appends rather than truncates — so the file that
/// answers "what did the published sweep measure" is the whole sweep, not its
/// last row. A single-line file is the degenerate case of the same thing.
fn passes(path: &str, text: &str, f: &mut Failures) -> Option<BTreeMap<String, Vec<f64>>> {
    let mut out: BTreeMap<String, Vec<f64>> = BTreeMap::new();
    for (n, line) in text.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let at = format!("{path}:{}", n + 1);
        let doc = match crate::util::parse_yaml(line) {
            Ok(d) => d,
            Err(e) => {
                f.fail(format!("{at}: {e}"));
                continue;
            }
        };
        let Yaml::Hash(h) = doc else {
            f.fail(format!("{at}: expected an object of key -> number"));
            continue;
        };
        for (k, v) in &h {
            let Some(k) = k.as_str() else { continue };
            match number(v) {
                Some(v) => out.entry(k.to_string()).or_default().push(v),
                None => f.fail(format!("{at}: {k} is not a number")),
            }
        }
    }
    if out.is_empty() {
        f.fail(format!("{path} holds no readings."));
    }
    f.is_empty().then_some(out)
}

/// A key without its shape suffix: `ingest.cores.conns96` is `ingest.cores`.
///
/// Only a `.conns<digits>` tail counts. `cost.hot_bytes_per_byte` has no shape
/// because what a byte of wire costs on disk is not a property of how many
/// sockets delivered it, and `ingest.ack_p50_ms.logoff` names a configuration
/// rather than a shape — neither should collapse into something else's stem.
fn stem(key: &str) -> &str {
    match key.rsplit_once(".conns") {
        Some((head, n)) if !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()) => head,
        _ => key,
    }
}

/// The median of a key's readings across the passes in the file.
///
/// Medians over passes is the standard the published figures were taken to, so
/// it is the standard the tool that replaces them applies. An even count takes
/// the upper of the two middles rather than averaging them: the average of two
/// passes is a rate no pass measured, and every number in the registry is meant
/// to be a reading somebody can point at.
fn median(xs: &[f64]) -> f64 {
    let mut v = xs.to_vec();
    v.sort_by(f64::total_cmp);
    v[v.len() / 2]
}

fn load(f: &mut Failures) -> Vec<Measurement> {
    let text = read_or_exit(REGISTRY);
    let doc = match crate::util::parse_yaml(&text) {
        Ok(d) => d,
        Err(e) => {
            eprintln!("error: {REGISTRY}: {e}");
            std::process::exit(1);
        }
    };
    let Yaml::Array(entries) = &doc["measurements"] else {
        eprintln!("error: {REGISTRY} has no `measurements:` list.");
        std::process::exit(1);
    };

    let mut out = Vec::new();
    let mut seen = std::collections::BTreeSet::new();
    for (n, e) in entries.iter().enumerate() {
        let at = format!("{REGISTRY} entry {}", n + 1);
        let Some(key) = e["key"].as_str() else {
            f.fail(format!("{at} has no `key`."));
            continue;
        };
        if !seen.insert(key.to_string()) {
            f.fail(format!("{REGISTRY}: `{key}` appears twice."));
        }
        if number(&e["value"]).is_none() {
            f.fail(format!("{key} has no numeric `value`."));
            continue;
        }
        let mut quoted = Vec::new();
        if let Yaml::Array(qs) = &e["quoted"] {
            for q in qs {
                let Some(as_) = q["as"].as_str() else {
                    f.fail(format!("{key}: a `quoted` entry has no `as`."));
                    continue;
                };
                let sites = match &q["sites"] {
                    Yaml::Array(s) => s.iter().filter_map(|s| Some(s.as_str()?.to_string())),
                    _ => {
                        f.fail(format!("{key}: `{as_}` has no `sites` list."));
                        continue;
                    }
                };
                quoted.push((as_.to_string(), sites.collect()));
            }
        }
        if quoted.is_empty() {
            f.fail(format!(
                "{key} is quoted nowhere. A measurement no document makes a \
                 claim out of does not need an entry here — delete it, or add \
                 the site that motivated it."
            ));
        }
        out.push(Measurement {
            key: key.to_string(),
            literal: literal(&e["value"]),
            unit: e["unit"].as_str().unwrap_or_default().to_string(),
            measures: squash(e["measures"].as_str().unwrap_or_default()),
            provenance: e["provenance"].as_str().unwrap_or("unscripted").to_string(),
            quoted,
        });
    }
    out
}

/// Every site listed must contain the literal it is listed under.
///
/// Containment, not a count: a figure quoted twice in one file is fine, and
/// asserting a count would fail the day somebody adds a sentence. What this
/// catches is the half-update — the figure gone from one of five files — which
/// is the thing that actually happens.
fn check_sites(all: &[Measurement], f: &mut Failures) {
    let mut cache: BTreeMap<&str, String> = BTreeMap::new();
    for m in all {
        for (as_, sites) in &m.quoted {
            for rel in sites {
                let text = match cache.get(rel.as_str()) {
                    Some(t) => t,
                    None => match read(rel) {
                        Ok(t) => cache.entry(rel).or_insert(t),
                        Err(e) => {
                            f.fail(format!("{}: {e}", m.key));
                            continue;
                        }
                    },
                };
                if !text.contains(as_) {
                    f.fail(format!(
                        "{rel} no longer says `{as_}` ({}, {} {}). Either it was \
                         re-measured and this site was missed, or the sentence \
                         moved and {REGISTRY} needs the new site — in both cases \
                         the fix is in the diff, not here.",
                        m.key, m.literal, m.unit
                    ));
                }
            }
        }
    }
}

fn check_rendered(all: &[Measurement], f: &mut Failures) {
    let want = table(all);
    let text = read_or_exit(CONTRACT);
    let begin = format!("<!-- BEGIN GENERATED: {MARKER} -->");
    let end = format!("<!-- END GENERATED: {MARKER} -->");
    let have = text
        .split_once(&begin)
        .and_then(|(_, rest)| rest.split_once(&end))
        .map(|(body, _)| body.trim());
    match have {
        None => f.fail(format!("{CONTRACT} has no `{begin}` / `{end}` markers.")),
        Some(have) if have != want.trim() => f.fail(format!(
            "the registry table in {CONTRACT} is stale. `cargo run -p xtask -- \
             measurements render` rewrites it."
        )),
        Some(_) => {}
    }
}

/// The table the contract page carries: every quantity, what it is, and the one
/// command that produces it again.
fn table(all: &[Measurement]) -> String {
    let mut out = String::from(
        "| Quantity | Measured | What the number is | Reproduce |\n|---|---|---|---|\n",
    );
    for m in all {
        let unit = if m.unit.is_empty() {
            String::new()
        } else {
            format!(" {}", m.unit)
        };
        let sites: usize = m.quoted.iter().map(|(_, s)| s.len()).sum();
        let how = match m.provenance.as_str() {
            "unscripted" => "**no script** — reproducible in method only".to_string(),
            p => format!("`{p}`"),
        };
        out.push_str(&format!(
            "| `{}` | **{}**{unit} | {} | {how}, quoted in {sites} place{} |\n",
            m.key,
            m.literal,
            m.measures,
            if sites == 1 { "" } else { "s" },
        ));
    }
    out
}

/// The line numbers in `rel` that contain `needle`.
fn lines_containing(rel: &str, needle: &str) -> Vec<usize> {
    let Ok(text) = read(rel) else {
        return Vec::new();
    };
    text.match_indices(needle)
        .map(|(at, _)| line_of(&text, at))
        .collect()
}

fn rewrite(key: &str, new: &str) -> bool {
    let path = crate::util::root().join(REGISTRY);
    match splice(&read_or_exit(REGISTRY), key, new) {
        Some(out) => std::fs::write(path, out).is_ok(),
        None => false,
    }
}

/// Replace one entry's `value:` line, leaving every comment in the file alone.
///
/// Line-based rather than a regex over the whole document, and rather than
/// re-emitting the parsed YAML: the registry is two thirds prose explaining what
/// each number means, and an emitter would drop all of it. `None` when the key
/// has no `value:` under it, which [`rewrite`] turns into a refusal rather than
/// a silently unchanged file.
fn splice(text: &str, key: &str, new: &str) -> Option<String> {
    let mut out = String::with_capacity(text.len());
    let mut inside = false;
    let mut done = false;
    for line in text.lines() {
        let trimmed = line.trim_start();
        if let Some(k) = trimmed.strip_prefix("- key:") {
            inside = k.trim() == key;
        }
        if inside && !done && trimmed.starts_with("value:") {
            let indent = &line[..line.len() - trimmed.len()];
            out.push_str(&std::format!("{indent}value: {new}\n"));
            done = true;
            continue;
        }
        out.push_str(line);
        out.push('\n');
    }
    done.then_some(out)
}

/// Format `v` the way `like` is formatted: same decimal places, or an integer if
/// `like` is one.
///
/// The registry's own spelling is the contract. A value written `1.20` stays two
/// decimals even when the new reading is 1.2 exactly, because `1.20 B/B` is what
/// four documents say and a silent `1.2` would fail every one of them for a
/// change that did not happen.
fn format(v: f64, like: &str) -> String {
    match like.split_once('.') {
        Some((_, frac)) => std::format!("{v:.*}", frac.len()),
        None => std::format!("{}", v.round() as i64),
    }
}

fn number(y: &Yaml) -> Option<f64> {
    match y {
        Yaml::Integer(n) => Some(*n as f64),
        Yaml::Real(s) => s.parse().ok(),
        _ => None,
    }
}

/// The value exactly as the registry spells it.
fn literal(y: &Yaml) -> String {
    match y {
        Yaml::Real(s) => s.clone(),
        Yaml::Integer(n) => n.to_string(),
        _ => String::new(),
    }
}

/// A folded YAML scalar arrives with its newlines; a markdown table cell cannot
/// hold one.
fn squash(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The registry's own spelling is the contract, so a new reading is printed
    /// to the decimals the old one had.
    ///
    /// The failure this prevents is silent and total: `1.20` re-measured to
    /// exactly 1.2 would be written `1.2`, and every one of the four documents
    /// saying "1.20x the wire" would then fail [`check_sites`] for a change that
    /// did not happen.
    #[test]
    fn a_new_reading_keeps_the_decimals_the_registry_publishes() {
        assert_eq!(format(1.2, "1.20"), "1.20");
        assert_eq!(format(2.2349, "2.23"), "2.23");
        assert_eq!(format(8.3351, "8.33"), "8.34");
        // An integer site stays an integer, and rounds rather than truncates:
        // 886,146.6 records/s/core published as 886146 is off by one for no
        // reason a reader could reconstruct.
        assert_eq!(format(886_146.6, "886147"), "886147");
        assert_eq!(format(137.0, "137"), "137");
    }

    /// Medians over passes, and never a number no pass measured.
    ///
    /// The tempting even-count implementation averages the two middles. That
    /// produces a rate nothing observed, which is the one property every figure
    /// in the registry is supposed to have.
    #[test]
    fn the_merge_is_a_median_and_always_an_observed_reading() {
        assert_eq!(median(&[3.0, 1.0, 2.0]), 2.0);
        assert_eq!(median(&[1.0]), 1.0);
        // Upper of the two middles, not their mean — 3.0, not 2.5.
        assert_eq!(median(&[1.0, 2.0, 3.0, 4.0]), 3.0);
        // An outlier pass moves the mean and not this.
        assert_eq!(median(&[1.0, 2.0, 99.0]), 2.0);
    }

    /// A shape the sweep measures but nobody publishes is normal; a renamed
    /// emitter is not. The two are told apart by the stem, so the stem has to
    /// strip exactly the shape suffix and nothing else.
    #[test]
    fn only_a_connection_count_is_stripped_from_a_key() {
        assert_eq!(stem("ingest.cores.conns96"), "ingest.cores");
        assert_eq!(stem("ingest.records_per_s.conns1"), "ingest.records_per_s");
        // `logoff` names a configuration, not a shape: collapsing it would let
        // an ack figure taken with the log off pass as one taken with it on.
        assert_eq!(stem("ingest.ack_p50_ms.logoff"), "ingest.ack_p50_ms.logoff");
        assert_eq!(stem("cost.hot_bytes_per_byte"), "cost.hot_bytes_per_byte");
        assert_eq!(stem("q.conns"), "q.conns");
        assert_eq!(stem("q.connsX"), "q.connsX");
    }

    /// The splice is the half of this module that writes, and getting it wrong
    /// corrupts the file every other check reads.
    #[test]
    fn the_splice_replaces_one_value_and_leaves_the_prose_alone() {
        let reg = "\
measurements:
  # a comment that an emitter would drop
  - key: a.b
    value: 232
    unit: MiB
  - key: c.d
    value: 1.20
";
        let out = splice(reg, "c.d", "1.31").unwrap();
        assert!(out.contains("value: 1.31"), "{out}");
        assert!(out.contains("value: 232"), "the wrong entry moved: {out}");
        assert!(out.contains("# a comment"), "the prose was dropped: {out}");
        assert_eq!(out.lines().count(), reg.lines().count());

        // A key with no `value:` is a refusal, not a file written unchanged —
        // otherwise `ingest --write` would report success and update nothing.
        assert!(splice(reg, "nope", "1").is_none());
    }

    /// A sweep is several passes appended to one file, so the parse has to be
    /// per line. A single whole-file parse reads the first object and silently
    /// ignores every pass after it.
    #[test]
    fn a_multi_pass_run_file_is_read_as_every_pass() {
        let mut f = Failures::default();
        let run = passes("run.json", "{\"a\": 1, \"b\": 2}\n\n{\"a\": 3}\n", &mut f)
            .expect("three lines, one blank");
        assert!(f.is_empty());
        assert_eq!(run["a"], vec![1.0, 3.0]);
        assert_eq!(run["b"], vec![2.0]);

        // A line that is not an object of numbers is named rather than skipped.
        let mut f = Failures::default();
        assert!(passes("run.json", "{\"a\": \"fast\"}\n", &mut f).is_none());
        let mut f = Failures::default();
        assert!(passes("run.json", "[1, 2]\n", &mut f).is_none());
        let mut f = Failures::default();
        assert!(passes("run.json", "\n\n", &mut f).is_none());
    }

    /// Every `provenance` that is not `unscripted` names a script that exists.
    ///
    /// This is the check a comment cannot be: the registry's promise is "one
    /// command reproduces this number", and a renamed or deleted script turns
    /// that promise into a path nobody notices is dead until they try it.
    #[test]
    fn every_provenance_command_is_a_script_in_the_tree() {
        let mut f = Failures::default();
        let all = load(&mut f);
        assert!(!all.is_empty());
        for m in &all {
            if m.provenance == "unscripted" {
                continue;
            }
            let path = crate::util::root().join(&m.provenance);
            assert!(
                path.is_file(),
                "{}: `provenance: {}` names nothing in the tree",
                m.key,
                m.provenance
            );
        }
    }

    /// The registry parses and every entry is complete, which is what the rest
    /// of this module assumes before it does anything useful.
    #[test]
    fn the_committed_registry_loads_without_a_complaint() {
        let mut f = Failures::default();
        let all = load(&mut f);
        assert!(f.is_empty(), "measurements.kyaml does not load cleanly");
        assert!(
            all.iter().all(|m| !m.measures.is_empty()),
            "a `measures` is empty"
        );
        assert!(all.iter().all(|m| !m.literal.is_empty()));
    }
}
