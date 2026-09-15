//! The two version lines, and the changeset units that declare them.
//!
//! `changeset version` can write exactly one kind of file: a `package.json`.
//! The two under `release/units/` are that file and nothing else — one number
//! each, no dependencies, never published. This module is what turns those two
//! numbers into the twenty-odd places Mira actually states a version.
//!
//! The engine line needs almost nothing here: its unit is a row in
//! [`crate::drift`]'s table like any other site, so `make drift` already checks
//! it and `make bump` already writes it, and [`apply`] only has to hand the new
//! number to [`crate::drift::bump`], which promotes the changelog on the way
//! past.
//!
//! The operator line needs [`operator_sites`], which is the same idea on its own
//! version: one table of anchored patterns, read forwards by `make drift` and
//! backwards by `make version`. Before this existed,
//! `charts/mira-operator/Chart.yaml` had no local gate at all — `operator-meta`
//! in release.yml read it *after* the tag, which is the wrong side of the merge
//! to find out that `appVersion` and the Artifact Hub image annotation disagree.

use regex::Regex;

use crate::drift::{bump as bump_engine, rewrite_group1, workspace_version};
use crate::util::{Failures, line_of, re, read_or_exit, root};

const ENGINE_UNIT: &str = "release/units/mira-engine/package.json";
const OPERATOR_UNIT: &str = "release/units/mira-operator/package.json";
const CHART: &str = "charts/mira-operator/Chart.yaml";

/// Every place the *operator's* number is written, as anchored patterns.
///
/// The engine's equivalent is `version_sites()` in [`crate::drift`], and the two
/// tables are deliberately separate rather than one table with a column: they
/// answer to different sources, and the single most expensive mistake available
/// here is a change that makes one number follow the other.
///
/// A pattern matching *nothing* is a failure, not a pass, for the same reason it
/// is over there: a gate for a line that has moved is a gate that is off.
fn operator_sites() -> Vec<(&'static str, Regex)> {
    vec![
        // The chart's own two, plus the Artifact Hub annotation that restates
        // appVersion as an image tag. All three were hand-edited and nothing
        // local read them.
        (CHART, re(r"(?m)^version: (\d+\.\d+\.\d+)$")),
        (CHART, re(r#"(?m)^appVersion: "(\d+\.\d+\.\d+)"$"#)),
        (
            CHART,
            re(r"ghcr\.io/trianalab/mira-operator:(\d+\.\d+\.\d+)"),
        ),
        // The controller crate. It sat at 0.0.0 because nothing read it, which
        // was never quite true: the binary prints `CARGO_PKG_VERSION` in
        // `--version`, so 0.0.0 was a lie available to anyone debugging a
        // cluster.
        (
            "integrations/kubernetes/Cargo.toml",
            re(r#"(?m)^version = "(\d+\.\d+\.\d+)"$"#),
        ),
        // The second workspace's lockfile. Generated — `make version`
        // regenerates it — but checked here anyway, because `--locked` on the
        // operator build is what turns a stale entry into a red release rather
        // than a surprise.
        (
            "integrations/kubernetes/Cargo.lock",
            re(r#"(?m)^name = "mira-operator"\nversion = "(\d+\.\d+\.\d+)"$"#),
        ),
        // The two copy-pasteable commands on the install page. Ungated until
        // now, on a page whose whole job is to be copied.
        (
            "docs/install.md",
            re(r"--version (\d+\.\d+\.\d+) --namespace mira-system"),
        ),
        (
            "docs/install.md",
            re(r"ghcr\.io/trianalab/charts/mira-operator:(\d+\.\d+\.\d+)"),
        ),
        // charts/mira-operator/README.md is deliberately absent: every version
        // in it comes from `{{ template "chart.version" . }}`, so helm-docs
        // writes it and `make helm-docs-check` gates it. Two gates on one
        // generated file is one gate too many.
    ]
}

/// The `version` field of a changeset unit.
fn unit_version(rel: &str) -> String {
    let text = read_or_exit(rel);
    // `\s*` rather than two literal spaces: `changeset version` rewrites the
    // file with whatever indentation it detects, and a gate that depends on the
    // formatter agreeing with us is a gate that fires on the bot.
    match re(r#"(?m)^\s*"version": "(\d+\.\d+\.\d+)""#).captures(&text) {
        Some(c) => c[1].to_string(),
        None => {
            eprintln!("error: {rel} has no X.Y.Z \"version\" field.");
            std::process::exit(1);
        }
    }
}

/// What `charts/mira-operator/Chart.yaml` says today.
fn chart_version() -> String {
    let text = read_or_exit(CHART);
    match re(r"(?m)^version: (\d+\.\d+\.\d+)$").captures(&text) {
        Some(c) => c[1].to_string(),
        None => {
            eprintln!("error: {CHART} has no `version: X.Y.Z` line.");
            std::process::exit(1);
        }
    }
}

/// Every operator site states the version `release/units/mira-operator` does.
///
/// Called from `drift::check`, so this is part of `make drift` rather than a
/// second command nobody runs.
pub fn check_sites(f: &mut Failures) {
    let expected = unit_version(OPERATOR_UNIT);
    println!("operator:    {expected} in {CHART}");
    for (rel, pattern) in operator_sites() {
        let text = read_or_exit(rel);
        let mut found = 0;
        for c in pattern.captures_iter(&text) {
            let g = c.get(1).expect("group 1 is the version");
            found += 1;
            if g.as_str() != expected {
                f.fail(format!(
                    "{rel}:{} says {}, but {OPERATOR_UNIT} says {expected}.\n    \
                     The operator is on its own version line — `make version` moves \
                     it from an `\"@mira/operator\"` changeset, and `make bump` \
                     deliberately does not touch it.",
                    line_of(&text, g.start()),
                    g.as_str()
                ));
            }
        }
        if found == 0 {
            f.fail(format!(
                "{rel} has no line matching `{}`, so the operator's version is no longer \
                 gated there. Restore the line, or teach operator_sites() in \
                 crates/xtask/src/release.rs the new shape.",
                pattern.as_str()
            ));
        }
    }
}

/// Write the operator's sites. The engine's are [`crate::drift::bump`]'s job.
fn write_operator(to: &str) -> bool {
    let mut pending: Vec<(&str, String, usize)> = Vec::new();
    for (rel, pattern) in operator_sites() {
        let at = pending.iter().position(|(r, _, _)| *r == rel);
        let text = match at {
            Some(i) => pending[i].1.clone(),
            None => read_or_exit(rel),
        };
        let (text, n) = rewrite_group1(&pattern, &text, to);
        if n == 0 {
            eprintln!(
                "error: {rel} has no line matching `{}`, so this bump would leave it \
                 behind. Nothing has been written.",
                pattern.as_str()
            );
            return false;
        }
        match at {
            Some(i) => {
                pending[i].1 = text;
                pending[i].2 += n;
            }
            None => pending.push((rel, text, n)),
        }
    }
    for (rel, text, n) in &pending {
        if let Err(e) = std::fs::write(root().join(rel), text) {
            eprintln!("error: {rel}: {e}");
            return false;
        }
        println!("  {rel}: {n} site(s)");
    }
    true
}

/// Copy what `changeset version` wrote into the tree that ships.
///
/// Run by `make version`, straight after `changeset version`, and by nothing
/// else. Idempotent on a line that did not move: a batch of changesets naming
/// only the operator must not promote the engine's changelog section.
pub fn apply() -> bool {
    let engine = unit_version(ENGINE_UNIT);
    let operator = unit_version(OPERATOR_UNIT);
    let mut moved = false;

    let workspace = workspace_version();
    if engine == workspace {
        println!("engine:   still {engine} — no engine changeset in this batch.");
    } else {
        // Not a separate writer: this is `make bump`, which also promotes
        // `## [Unreleased]` and refuses if it is empty. That refusal is why
        // `make ci-changeset` checks the same thing on the pull request —
        // finding out here means finding out on main.
        println!("engine:   {workspace} -> {engine}");
        if !bump_engine(&engine) {
            return false;
        }
        moved = true;
    }

    let chart = chart_version();
    if operator == chart {
        println!("operator: still {operator} — no operator changeset in this batch.");
    } else {
        println!("operator: {chart} -> {operator}");
        if !write_operator(&operator) {
            return false;
        }
        moved = true;
    }

    if !moved {
        eprintln!(
            "error: neither line moved, so `changeset version` had nothing to apply. \
             Either there were no changesets, or `privatePackages.version` in \
             .changeset/config.json has been turned off — config v4 defaults it to \
             false, and a silent no-op bump is exactly what that looks like."
        );
        return false;
    }
    println!(
        "\nBoth Cargo.locks and the chart README are generated — `make version` \
         regenerates all three. Then `make drift` to verify."
    );
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The table read forwards, over the real tree. A pattern that has stopped
    /// matching fails `cargo test --workspace` and not only `make drift`, which
    /// is the difference between noticing on the branch and noticing on the
    /// release.
    #[test]
    fn the_operator_version_sites_agree() {
        let mut f = Failures::default();
        check_sites(&mut f);
        assert!(
            f.is_empty(),
            "operator version sites disagree — run `make drift` for the list"
        );
    }

    /// The two lines are separate, and this is the assertion that says so.
    ///
    /// Nothing stops a later edit from adding the chart to `version_sites()` or
    /// the workspace to `operator_sites()`, and either one silently couples the
    /// two numbers — which is the one thing `### And two that do not` in
    /// releases.md spends four paragraphs refusing. A crossed table is only
    /// visible on the release where one of them moves alone.
    #[test]
    fn neither_table_reaches_into_the_others_source() {
        for (rel, _) in operator_sites() {
            assert_ne!(
                rel, "Cargo.toml",
                "the operator table names the engine's source"
            );
            assert_ne!(
                rel, ENGINE_UNIT,
                "the operator table names the engine's unit"
            );
        }
    }
}
