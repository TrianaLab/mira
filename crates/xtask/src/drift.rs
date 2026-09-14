//! Check that the numbers Mira promises are still true of the tree that ships.
//!
//! Every number in the README is a promise, and CLAUDE.md makes two of them
//! load-bearing: the crate count and the stripped binary size. Both are quoted
//! in six files between them. The failure mode is not that someone lies — it is
//! that someone adds a dependency, CI tells them the count moved, they fix the
//! README, and the other five sites keep saying 117 forever.
//!
//! So this does three things:
//!
//!   1. measures the crate count and the binary size,
//!   2. reads what the README declares and fails if reality has moved,
//!   3. fails if any other site that quotes those numbers still quotes the old
//!      one.
//!
//! Step 3 is the one that matters. Steps 1 and 2 catch a lie; step 3 catches the
//! half-fix, which is the thing that actually happens.
//!
//! The Helm chart is **not** one of the sites, and that is a change rather than
//! an oversight: `charts/mira-operator/Chart.yaml` is the operator's own source
//! of truth on its own version line, because the controller and the engine are
//! not released together in any meaningful sense. `operator-meta` in release.yml
//! is what gates it. See docs/internals/releases.md.
//!
//! Run it with `make drift` — that target builds the release binary first,
//! because you cannot check a binary's size without the binary.

use std::collections::BTreeMap;
use std::process::Command;

use regex::Regex;

use crate::util::{Failures, glob, line_of, re, read, read_or_exit, root};

/// The three workspace members that show up in `cargo tree -p miradb`, so the
/// measured figure is three above the number the README states.
///
/// `xtask` is a fourth workspace member and is deliberately not counted: it is
/// not a dependency of `mira`, so it is not in that tree at all. CLAUDE.md says
/// the same thing; if a member is ever added *to `mira`'s graph*, this constant
/// and that sentence move together.
const WORKSPACE_MEMBERS: usize = 3;

/// docs/architecture.md section 11 scores binary size as one of the four axes
/// and sets the target at "<= 20 MB stripped with UI + query + MCP". That is the
/// contract; the size the README declares is merely where we are against it.
const SIZE_CEILING_BYTES: u64 = 20 * 1000 * 1000;

/// `zstd-sys` is the only C dependency in the tree and CLAUDE.md calls that a
/// stated property. A property nobody checks is a property that expires
/// quietly: `flate2` and `tonic` both have C-backed feature paths one careless
/// `default-features` away.
///
/// Two checks, because "C dependency" has two edges and the cheap one misses.
/// `-sys` naming is a convention, so [`ALLOWED_SYS_CRATES`] catches crates by
/// their name; the `cc` reverse-dependency walk catches crates by what they
/// actually do. Something like `ring` or `blake3` compiles C without a `-sys`
/// suffix and only the second finds it.
const C_TOOLCHAIN_CRATES: [&str; 3] = ["cc", "cmake", "bindgen"];
const ALLOWED_CC_DEPENDENTS: [&str; 1] = ["zstd-sys"];

/// `core-foundation-sys` is a `-sys` crate that compiles no C: it is extern
/// declarations against a framework already present on every Mac, and it is
/// macOS-only. It arrives via arrow-array -> chrono -> iana-time-zone, so it is
/// not a choice Mira gets to make either. It is on this list rather than
/// excluded by a rule because "it looks like a C dependency and is not" is
/// exactly the kind of thing that deserves a name and a sentence.
const ALLOWED_SYS_CRATES: [&str; 2] = ["zstd-sys", "core-foundation-sys"];

/// Every file that quotes the crate count.
///
/// Adding a new mention somewhere is free; *removing* the last mention from a
/// listed file fails here, which is the point — you then either restore it or
/// delete the line below, and either way a reviewer sees the decision.
const CRATE_COUNT_SITES: [&str; 6] = [
    "README.md",
    "docs/architecture.md",
    "docs/market.md",
    "docs/index.md",
    "crates/mira/Cargo.toml",
    "crates/mira/src/term.rs",
];

/// Every file that quotes the binary size. See [`CRATE_COUNT_SITES`].
const BINARY_SIZE_SITES: [&str; 5] = [
    "README.md",
    "docs/architecture.md",
    "docs/market.md",
    "docs/index.md",
    "crates/mira/src/term.rs",
];

/// The README numbers were measured on an Apple M3 Pro (docs/architecture.md
/// section 11 says so). A GitHub Linux runner links a measurably different
/// binary, so the exact-size gate only runs where the comparison means
/// something. Everywhere else the ceiling still applies, and the measured size
/// is printed so a jump is visible in the log even when it is not fatal.
const REFERENCE_PLATFORM: &str = "macos";

/// The crate count needs no such escape hatch, because `cargo tree --target`
/// will resolve any triple's graph from any host — it only evaluates `cfg`, so
/// the target's standard library need not be installed. Without it the count is
/// host-dependent (`core-foundation-sys` is macOS-only, so a Linux runner counts
/// 116 where this Mac counts 117) and the gate fires on the runner rather than
/// on a real change. Same triple as [`REFERENCE_PLATFORM`], so the declared
/// number stays the one that was measured.
const REFERENCE_TARGET: &str = "aarch64-apple-darwin";

/// Two percent is ~115 KiB at the size the binary is now. A new dependency costs
/// hundreds of KiB, so this catches the regression it exists to catch; a rustc
/// point release moves the number by a few KiB, which it deliberately does not.
const SIZE_TOLERANCE: f64 = 0.02;

const MIB: f64 = 1024.0 * 1024.0;

/// The Artifact Hub ownership proof, pushed to the chart repository under a
/// reserved tag by release.yml.
///
/// Artifact Hub does not report a wrong or missing `repositoryID` as an error:
/// it just leaves the repository unverified, quietly, forever. So the two ways
/// to get there — the file gone, or the ID still the placeholder someone pasted
/// before the repository existed — are checked here instead.
const ARTIFACTHUB_REPO_YML: &str = "artifacthub-repo.yml";
const PLACEHOLDER_REPOSITORY_ID: &str = "00000000-0000-0000-0000-000000000000";

const CHANGELOG: &str = "CHANGELOG.md";

/// Every site that restates the workspace version, as (file, pattern) with the
/// version itself as group 1.
///
/// One table, read by two things: [`check_version_sites`] asserts every match
/// equals `[workspace.package] version`, and [`bump`] rewrites every match to a
/// new one. That is the point of the shared table — a writer and a gate
/// maintained separately drift, and the direction they drift in is the bad one,
/// because the writer is what people actually run.
///
/// Each pattern is anchored on the *syntax around* the version rather than on a
/// bare `X.Y.Z`, so that naming some other project's version in prose — a Rust
/// release, a Helm version — is not a build failure, and so that a paragraph
/// that deliberately recounts a past release survives a bump.
/// `docs/internals/releases.md` is full of those sentences and must never be
/// rewritten here.
///
/// Every match in a listed file must equal the current version: a file carrying
/// a second, stale one is the failure mode this exists for. `v0.1.0` sat in
/// three copy-pasteable blocks while the workspace was at 0.0.1, and each was a
/// 404 on a stranger's first contact with Mira.
fn version_sites() -> Vec<(&'static str, Regex)> {
    vec![
        // The source itself, and the two path-dep pins beside it. The pins are
        // the classic miss: `cargo publish` requires a `version` next to every
        // `path`, and no local build ever reads it, so a stale pin is invisible
        // right up until the release job uploads a crate depending on a sibling
        // version that does not exist.
        ("Cargo.toml", re(r#"(?m)^version = "(\d+\.\d+\.\d+)"$"#)),
        (
            "Cargo.toml",
            re(r#"(?m)^mira-(?:core|proto) = \{.*?version = "(\d+\.\d+\.\d+)""#),
        ),
        // No chart row. `charts/mira-operator/Chart.yaml` is the operator's own
        // source of truth on its own version line, deliberately not this one —
        // see docs/internals/releases.md. `operator-meta` in release.yml is
        // what gates it, and `make bump` must leave it alone.
        //
        // The commands a reader copies rather than reads. A stale version here
        // is a 404 rather than a typo. The `v` is required rather than
        // optional, and that is load-bearing now: `helm install
        // mira-operator … --version 0.1.0` is in the same file, on the
        // operator's own version line, and a `v?` would demand it equal the
        // engine's.
        ("README.md", re(r"--version v(\d+\.\d+\.\d+)")),
        ("docs/install.md", re(r"--version v(\d+\.\d+\.\d+)")),
        ("docs/install.md", re(r"(?m)^V=(\d+\.\d+\.\d+)")),
        // `spec.image` in the copy-pasteable MiraCluster. This *is* the
        // engine's version even though it sits in the operator's chart, which
        // is why it is here and the chart's own `version` is not. The template
        // and its helm-docs output are both listed: `make bump` rewrites the
        // template, and the `helm-docs` it runs straight after propagates it —
        // but `make drift` has to fail on either one alone.
        (
            "docs/install.md",
            re(r"ghcr\.io/trianalab/mira:(\d+\.\d+\.\d+)"),
        ),
        (
            "charts/mira-operator/README.md.gotmpl",
            re(r"ghcr\.io/trianalab/mira:(\d+\.\d+\.\d+)"),
        ),
        (
            "charts/mira-operator/README.md",
            re(r"ghcr\.io/trianalab/mira:(\d+\.\d+\.\d+)"),
        ),
        // The two that used to be ungated prose, and rotted exactly as
        // predicted: the 0.0.1 cut left SECURITY.md claiming there was no tagged
        // release, on the day after there was one.
        (
            "SECURITY.md",
            re(r"(?m)^Mira is pre-1\.0 — `(\d+\.\d+\.\d+)`"),
        ),
        (
            ".github/ISSUE_TEMPLATE/bug_report.yml",
            re(r"(?m)^\s*placeholder: mira (\d+\.\d+\.\d+)$"),
        ),
    ]
}

fn cargo(args: &[&str]) -> Result<String, String> {
    let out = Command::new("cargo")
        .args(args)
        .current_dir(root())
        .output()
        .map_err(|e| format!("cargo {}: {e}", args.join(" ")))?;
    if !out.status.success() {
        return Err(String::from_utf8_lossy(&out.stderr).trim().to_string());
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

pub fn check() -> bool {
    let mut f = Failures::default();
    let mut notes: Vec<String> = Vec::new();

    let (declared_crates, declared_mib) = declared_from_readme();
    check_crate_count(declared_crates, &mut f);
    check_c_toolchain(&mut f);
    check_binary_size(declared_mib, &mut f, &mut notes);
    check_sites(declared_crates, declared_mib, &mut f);
    check_version_sites(&workspace_version(), &mut f);
    check_artifacthub_repo(&mut f);
    check_coverage_badge(&mut f);
    check_section_refs(&mut f);

    for note in &notes {
        println!("note: {note}");
    }
    if !f.report("drift check(s) failed") {
        return false;
    }
    println!("no drift.");
    true
}

/// Parse the README bullet that both numbers hang off.
///
/// The README is the source of truth on purpose: CLAUDE.md calls that section a
/// promise, so the promise is what everything else is checked against, rather
/// than a constant in this file that nobody reads.
fn declared_from_readme() -> (usize, f64) {
    let text = read_or_exit("README.md");
    let Some(m) = re(r"(\d+\.\d+) MiB stripped, (\d+) crates").captures(&text) else {
        eprintln!(
            "error: README.md no longer has a '<N.NN> MiB stripped, <N> crates' \
             bullet. That bullet is what every other site is checked against — \
             restore it, or teach crates/xtask/src/drift.rs where the numbers live."
        );
        std::process::exit(1);
    };
    (
        m[2].parse().unwrap_or_default(),
        m[1].parse().unwrap_or_default(),
    )
}

fn check_crate_count(declared: usize, f: &mut Failures) {
    // The pipeline CLAUDE.md documents: one entry per name+version pair in
    // mira's normal (non-dev, non-build) dependency tree.
    let tree = match cargo(&[
        "tree",
        "-p",
        "miradb",
        "--edges",
        "normal",
        "--prefix",
        "none",
        "--target",
        REFERENCE_TARGET,
    ]) {
        Ok(tree) => tree,
        Err(e) => {
            f.fail(format!("cannot read the dependency tree: {e}"));
            return;
        }
    };
    let pairs: std::collections::BTreeSet<String> = tree
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| l.split_whitespace().take(2).collect::<Vec<_>>().join(" "))
        .collect();
    let measured = pairs.len() - WORKSPACE_MEMBERS;
    println!("crates:      {measured} measured, {declared} declared");
    if measured != declared {
        let direction = if measured > declared {
            "grew"
        } else {
            "shrank"
        };
        f.fail(format!(
            "the dependency tree {direction} to {measured} crates but the docs still \
             say {declared}.\n    The count is a product property (README, \
             docs/architecture.md section 11). Update every site, not just the \
             README:\n{}",
            CRATE_COUNT_SITES.map(|s| format!("      {s}\n")).concat()
        ));
    }

    let unexpected: Vec<&str> = pairs
        .iter()
        .filter_map(|p| p.split_whitespace().next())
        .filter(|n| n.ends_with("-sys") && !ALLOWED_SYS_CRATES.contains(n))
        .collect();
    if !unexpected.is_empty() {
        f.fail(format!(
            "new `-sys` crate(s) in the tree: {}.\n    zstd-sys is the only C \
             dependency Mira has, and that is a stated property (CLAUDE.md). Keep \
             new crates on pure-Rust backends, or, if this one compiles no C, add it \
             to ALLOWED_SYS_CRATES with the sentence explaining why.",
            unexpected.join(", ")
        ));
    }
}

/// Whoever build-depends on `cc` is whoever compiles C. There is one.
fn check_c_toolchain(f: &mut Failures) {
    for tool in C_TOOLCHAIN_CRATES {
        // An error is the ordinary answer: `cargo tree -i` exits non-zero when
        // the crate is not in the graph at all, which is the state this wants.
        let Ok(out) = cargo(&[
            "tree",
            "-p",
            "miradb",
            "--edges",
            "normal,build",
            "-i",
            tool,
            "--prefix",
            "depth",
            "--format",
            "{p}",
            "--target",
            REFERENCE_TARGET,
        ]) else {
            continue;
        };
        // `--prefix depth` writes the depth with no separator, so depth 1 is
        // every line starting with "1" — the crates that pull the tool in
        // directly. Anything deeper is just their dependents.
        let unexpected: std::collections::BTreeSet<&str> = out
            .lines()
            .filter_map(|l| l.strip_prefix('1'))
            .filter_map(|l| l.split_whitespace().next())
            .filter(|n| !ALLOWED_CC_DEPENDENTS.contains(n))
            .collect();
        if !unexpected.is_empty() {
            f.fail(format!(
                "{} build-depends on `{tool}`, so it compiles C at build time.\n    \
                 That makes it a second C dependency, and 'zstd-sys is the only one' \
                 is a stated property of the product (CLAUDE.md, docs/architecture.md \
                 section 11). It also breaks the musl and cross-compilation story in \
                 .github/workflows/release.yml.",
                unexpected.into_iter().collect::<Vec<_>>().join(", ")
            ));
        }
    }
}

/// `5905712` as `5,905,712`. Only ever shown next to a ceiling, and a reader
/// comparing two seven-digit numbers by eye needs the separators to do it.
fn grouped(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(c);
    }
    out
}

fn check_binary_size(declared_mib: f64, f: &mut Failures, notes: &mut Vec<String>) {
    let binary = root().join("target/release/mira");
    let Ok(meta) = std::fs::metadata(&binary) else {
        eprintln!("error: target/release/mira is missing. Run `make build` first.");
        std::process::exit(1);
    };
    let measured = meta.len();
    let measured_mib = measured as f64 / MIB;
    println!(
        "binary:      {} B ({measured_mib:.2} MiB), {declared_mib:.2} MiB declared, \
         ceiling {} B",
        grouped(measured),
        grouped(SIZE_CEILING_BYTES)
    );

    if measured > SIZE_CEILING_BYTES {
        f.fail(format!(
            "the binary is {measured_mib:.2} MiB, over the {} MB target in \
             docs/architecture.md section 11.",
            SIZE_CEILING_BYTES / 1_000_000
        ));
    }

    if std::env::consts::OS != REFERENCE_PLATFORM {
        notes.push(format!(
            "exact size not checked on {}: the declared figure was measured on \
             {REFERENCE_PLATFORM} and a different linker moves it. The ceiling above \
             still applied.",
            std::env::consts::OS
        ));
        return;
    }

    let drift = (measured_mib - declared_mib).abs() / declared_mib;
    if drift > SIZE_TOLERANCE {
        f.fail(format!(
            "the binary is {measured_mib:.2} MiB but the docs say {declared_mib:.2} \
             MiB ({:.1}% off).\n    Re-measure and update every site:\n{}",
            drift * 100.0,
            BINARY_SIZE_SITES.map(|s| format!("      {s}\n")).concat()
        ));
    }
}

fn check_sites(declared_crates: usize, declared_mib: f64, f: &mut Failures) {
    let crate_pat = re(&format!(r"\b{declared_crates}\b"));
    let size = format!("{declared_mib:.2}");
    for rel in CRATE_COUNT_SITES {
        if !crate_pat.is_match(&read_or_exit(rel)) {
            f.fail(format!(
                "{rel} does not mention the current crate count ({declared_crates}). \
                 It quoted the old one — half-updating the docs is the drift this \
                 check exists for."
            ));
        }
    }
    for rel in BINARY_SIZE_SITES {
        if !read_or_exit(rel).contains(&size) {
            f.fail(format!(
                "{rel} does not mention the current binary size ({size} MiB). See \
                 above."
            ));
        }
    }
}

/// The `version` under `[workspace.package]` in the root Cargo.toml.
///
/// Read with a regex rather than a TOML parser because that is one dependency
/// for one line, and the shape of that line is already asserted by
/// [`version_sites`] — the first entry there matches the same text.
fn workspace_version() -> String {
    let text = read_or_exit("Cargo.toml");
    // The lines between the header and the next table. By hand rather than by
    // regex: "up to the next `[`" wants a lookahead, which this engine does not
    // have, and `take_while` says it more plainly than the pattern would.
    let section = text
        .lines()
        .skip_while(|l| l.trim_end() != "[workspace.package]")
        .skip(1)
        .take_while(|l| !l.starts_with('['))
        .collect::<Vec<_>>()
        .join("\n");
    let found = re(r#"(?m)^version\s*=\s*"([^"]+)""#)
        .captures(&section)
        .map(|m| m[1].to_string());
    found.unwrap_or_else(|| {
        eprintln!(
            "error: Cargo.toml has no `version` under `[workspace.package]`. That is \
             where the whole workspace takes its version from."
        );
        std::process::exit(1);
    })
}

/// Every site in [`version_sites`] restates `[workspace.package] version`.
///
/// Two ways to fail, and the second is the one that happens. A pattern that
/// matches *nothing* means the line it was written for has moved or gone, so the
/// gate has silently stopped gating — that is a failure here rather than a pass,
/// because a check for something that is no longer there passes forever. A
/// pattern that matches a *different* version is the ordinary half-bump.
fn check_version_sites(crate_version: &str, f: &mut Failures) {
    println!("version:     {crate_version} in Cargo.toml");
    for (rel, pattern) in version_sites() {
        let text = read_or_exit(rel);
        let mut any = false;
        for m in pattern.captures_iter(&text) {
            any = true;
            let g = m.get(1).expect("group 1 is the version");
            if g.as_str() != crate_version {
                f.fail(format!(
                    "{rel}:{} says {}, but the workspace is at {crate_version}.\n    \
                     One binary, one number — a second is only ever a \
                     question with no answer. `make bump TO={crate_version}` rewrites \
                     every site at once.",
                    line_of(&text, g.start()),
                    g.as_str()
                ));
            }
        }
        if !any {
            f.fail(format!(
                "{rel} has no line matching `{}`.\n    That pattern is how the version \
                 in this file is both checked and rewritten by `make bump`, so a shape \
                 change here turns the gate off rather than tripping it. Restore the \
                 line, or teach version_sites() in crates/xtask/src/drift.rs the new \
                 shape.",
                pattern.as_str()
            ));
        }
    }
}

fn check_artifacthub_repo(f: &mut Failures) {
    match read(ARTIFACTHUB_REPO_YML) {
        Err(_) => f.fail(format!(
            "{ARTIFACTHUB_REPO_YML} is gone. release.yml pushes it to \
             `ghcr.io/trianalab/charts/mira-operator:artifacthub.io`, and without it the chart \
             repository loses its Verified Publisher badge at the next release — \
             silently, because Artifact Hub reports a missing owner proof as \
             'unverified' rather than as an error."
        )),
        Ok(text) if text.contains(PLACEHOLDER_REPOSITORY_ID) => f.fail(format!(
            "{ARTIFACTHUB_REPO_YML} still carries the placeholder `repositoryID: \
             {PLACEHOLDER_REPOSITORY_ID}`. Artifact Hub matches the ID it issued \
             against the one it finds; a mismatch is the same silent 'unverified' as \
             no file at all. Copy the real ID from the repository's Artifact Hub \
             control panel."
        )),
        Ok(_) => {}
    }
}

/// The README's coverage badge.
///
/// A number baked into an image URL is the one kind of number nobody re-reads —
/// it renders the same whether or not it is still true — so this badge holds no
/// number at all: it is shields' `dynamic/json` reader pointed at a file
/// `make coverage-json` writes into the published site, from the measurement the
/// deploy itself took.
///
/// Which leaves exactly one way for it to rot, and it is silent: the badge
/// points at a URL and nothing publishes the file, so it renders grey "resource
/// not found" forever on a page whose whole job is to be a promise. Both halves
/// are checked here, together, because either alone passes while the pair is
/// broken.
fn check_coverage_badge(f: &mut Failures) {
    const URL: &str = "https://miradb.dev/coverage.json";
    let badge = re(concat!(
        r"img\.shields\.io/badge/dynamic/json\?[^)\s]*",
        r"url=https(?::|%3A)(?://|%2F%2F)miradb\.dev(?:/|%2F)coverage\.json",
    ));
    if !badge.is_match(&read_or_exit("README.md")) {
        f.fail(format!(
            "README.md no longer carries a live coverage badge pointing at {URL}.\n    \
             A hard-coded percentage is not a substitute: it is true on the day it is \
             written and unfalsifiable afterwards. Restore the shields dynamic/json \
             badge, or delete check_coverage_badge and `make coverage-json` together — \
             a gate for a thing that is gone is a gate that passes forever."
        ));
        return;
    }

    let makefile = read_or_exit("Makefile");
    if !re(r"(?m)^COVERAGE_JSON := site/coverage\.json$").is_match(&makefile) {
        f.fail(format!(
            "the README's coverage badge reads {URL}, and the Makefile no longer \
             declares 'COVERAGE_JSON := site/coverage.json' to write it.\n    Nothing \
             would publish the file, so the badge would render grey on every view of \
             the README and no build would go red."
        ));
    }

    // A writer nothing calls is the same outage as no writer. The `ci` check
    // cannot catch this: its make-dispatch rule covers ci.yml only, so a
    // docs.yml that never runs the target is valid to it.
    let docs_yml = read_or_exit(".github/workflows/docs.yml");
    let ci_mk = read_or_exit("ci.mk");
    if !docs_yml.contains("make ci-coverage-json")
        || !re(r"(?m)^ci-coverage-json:").is_match(&ci_mk)
    {
        f.fail(
            "nothing publishes site/coverage.json: the deploy in \
             .github/workflows/docs.yml must run 'make ci-coverage-json' and ci.mk \
             must define that target.\n    It has to be the deploy, after `make site` \
             — mkdocs empties the output directory, and ci.yml's coverage leg is \
             conditional on a code change, so a docs-only push would produce no file \
             at all.",
        );
    }
}

/// The section numbers `rel` declares, closed over prefixes.
///
/// `## 7. Correlation` declares 7 and `### 7.3 The frame algebra` declares 7.3;
/// citing the parent is fine wherever a child exists, so 7.3 implies 7.
fn sections_of(rel: &str) -> std::collections::BTreeSet<String> {
    let text = read_or_exit(rel);
    let heading = re(r"(?m)^#{2,4} (?:Annex )?([0-9A-Z][0-9.]*)\.? ");
    let mut out: std::collections::BTreeSet<String> = heading
        .captures_iter(&text)
        .map(|m| m[1].trim_end_matches('.').to_string())
        .collect();
    for h in out.clone() {
        let parts: Vec<&str> = h.split('.').collect();
        for i in 1..parts.len() {
            out.insert(parts[..i].join("."));
        }
    }
    out
}

/// A comment saying "section 7.3" is a link with no href: nothing resolves it,
/// nothing breaks when the section is renumbered, and the reader is left looking
/// for a heading that no longer exists. There are ~90 of them in the tree, which
/// is too many to re-check by hand every time architecture.md is edited — and
/// editing it is precisely when they rot.
fn check_section_refs(f: &mut Failures) {
    let arch = sections_of("docs/architecture.md");
    let cite = re(r"\bsection ([0-9]+(?:\.[0-9]+)*)\b");

    // `docs/**` rather than `docs/*`: the contributor pages moved into
    // docs/internals/ and cite architecture sections as heavily as anything at
    // the top level, and a gate that stops covering a file the moment it is
    // filed somewhere tidier is a gate that rots by reorganisation.
    let mut files = glob("crates/*/src/*.rs");
    files.extend(glob("docs/**/*.md"));
    files.sort();

    let mut dangling: BTreeMap<String, std::collections::BTreeSet<String>> = BTreeMap::new();
    for path in files {
        let name = path.rsplit('/').next().unwrap_or(&path).to_string();
        if name == "architecture.md" {
            continue;
        }
        let text = read_or_exit(&path);
        for m in cite.captures_iter(&text) {
            if arch.contains(m[1].trim_end_matches('.')) {
                continue;
            }
            dangling
                .entry(m[1].to_string())
                .or_default()
                .insert(name.clone());
        }
    }

    for (cite, where_) in dangling {
        f.fail(format!(
            "\"section {cite}\" is cited in {} and docs/architecture.md has no such \
             heading.\n    Either the section moved and the citation did not, or the \
             citation is a typo. A cross-reference into a document is a promise about \
             that document.",
            where_.into_iter().collect::<Vec<_>>().join(", ")
        ));
    }
}

// ---------------------------------------------------------------------------
// Writing
// ---------------------------------------------------------------------------

/// Replace group 1 of every match with `new`, leaving the rest untouched.
///
/// `Regex::replace_all` cannot do this: a template replaces the whole match, so
/// putting the surrounding syntax back means re-spelling every pattern twice,
/// once to find and once to restore. Splicing by group span keeps one pattern
/// per site, and one pattern is what lets the gate and the writer share a table.
fn rewrite_group1(pattern: &Regex, text: &str, new: &str) -> (String, usize) {
    let mut out = String::with_capacity(text.len());
    let mut last = 0;
    let mut count = 0;
    for m in pattern.captures_iter(text) {
        let g = m.get(1).expect("group 1 is the version");
        out.push_str(&text[last..g.start()]);
        out.push_str(new);
        last = g.end();
        count += 1;
    }
    out.push_str(&text[last..]);
    (out, count)
}

/// What is written under `## [Unreleased]`, up to the next released section.
///
/// The pattern *consumes* the header that ends the section rather than looking
/// ahead at it, and the difference is the whole reason this is a function with
/// a test rather than one line inside [`bumped_changelog`]. `regex` has no
/// look-around, `re` panics on a pattern it cannot compile, and the version
/// that wrote `(?=^## \[)` therefore aborted every single invocation of
/// `make bump` before it read a byte — an unreachable release path that no gate
/// could see, because nothing called it. Consuming the header is free here:
/// only group 1 is read, and the promotion below rewrites `text`, not the match.
fn unreleased_body(text: &str) -> Option<&str> {
    re(r"(?ms)^## \[Unreleased\]\n(.*?)^## \[")
        .captures(text)
        .map(|c| c.get(1).expect("group 1 is not optional").as_str())
}

/// `## [Unreleased]` promoted to `## [new]`, a fresh one opened, links moved.
///
/// Refuses on an empty `[Unreleased]`, which is not pedantry: release.yml
/// extracts that section verbatim as the GitHub Release notes and falls back to
/// `--generate-notes` when it comes out empty. Mira does not use Conventional
/// Commits, so the fallback is a list of imperative prose subjects — strictly
/// worse than the section nobody wrote. Better to refuse than to publish it.
fn bumped_changelog(old: &str, new: &str, today: &str) -> String {
    let text = read_or_exit(CHANGELOG);

    let Some(body) = unreleased_body(&text) else {
        eprintln!(
            "error: {CHANGELOG} has no `## [Unreleased]` section followed by a \
             released one. That is the section this promotes; see \
             docs/internals/releases.md."
        );
        std::process::exit(1);
    };
    if body.trim().is_empty() {
        eprintln!(
            "error: {CHANGELOG}'s `## [Unreleased]` section is empty. Write the {new} \
             notes into it first — release.yml publishes that section verbatim as the \
             Release notes, and an empty one degrades to generated notes, which for \
             this tree means a list of imperative commit subjects."
        );
        std::process::exit(1);
    }

    let text = text.replacen(
        "## [Unreleased]\n",
        &format!("## [Unreleased]\n\n## [{new}] - {today}\n"),
        1,
    );
    let (text, n) = rewrite_group1(
        &re(r"(?m)^\[Unreleased\]: \S+/compare/v(\d+\.\d+\.\d+)\.\.\.HEAD$"),
        &text,
        new,
    );
    if n != 1 {
        eprintln!(
            "error: {CHANGELOG} has no `[Unreleased]: …/compare/vX.Y.Z...HEAD` link \
             definition to move. Restore it, or drop the link definitions and this \
             block together."
        );
        std::process::exit(1);
    }
    re(r"(?m)^(\[Unreleased\]: (\S+)/compare/\S+$)")
        .replace(
            &text,
            format!("${{1}}\n[{new}]: ${{2}}/compare/v{old}...v{new}"),
        )
        .into_owned()
}

/// Write `to` to every site in [`version_sites`], and promote the changelog.
///
/// Every file is rewritten in memory and validated before *any* of them is
/// written, because a refusal half way through is the worst outcome available:
/// `make drift` then reports the sites it did reach as the wrong ones, and
/// whoever is mid-release has to work out by hand which half happened. The first
/// version of this wrote as it went and tripped over exactly that on its own
/// empty-changelog guard.
///
/// Deliberately does *not* touch Cargo.lock, which is generated and which
/// `make bump` regenerates straight after. Nor does it touch
/// docs/internals/releases.md, which recounts past releases by number on purpose
/// — the whole reason the sites are a table of anchored patterns rather than a
/// find-and-replace.
pub fn bump(to: &str) -> bool {
    if !re(r"^\d+\.\d+\.\d+$").is_match(to) {
        eprintln!("error: {to:?} is not an X.Y.Z version.");
        return false;
    }
    let old = workspace_version();
    if old == to {
        eprintln!("error: the workspace is already at {to}. Nothing to bump.");
        return false;
    }

    // Ordered, because the report below is read as a checklist against the diff.
    let mut pending: Vec<(&str, String, usize)> = Vec::new();
    for (rel, pattern) in version_sites() {
        let at = pending.iter().position(|(r, _, _)| *r == rel);
        let text = match at {
            Some(i) => pending[i].1.clone(),
            None => read_or_exit(rel),
        };
        let (text, n) = rewrite_group1(&pattern, &text, to);
        if n == 0 {
            eprintln!(
                "error: {rel} has no line matching `{}`, so this bump would leave it \
                 behind. Nothing has been written. Fix the file, or teach \
                 version_sites() in crates/xtask/src/drift.rs the new shape.",
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
    let changelog = bumped_changelog(&old, to, &today());

    println!("bump: {old} -> {to}");
    for (rel, text, n) in &pending {
        if let Err(e) = std::fs::write(root().join(rel), text) {
            eprintln!("error: {rel}: {e}");
            return false;
        }
        println!("  {rel}: {n} site(s)");
    }
    if let Err(e) = std::fs::write(root().join(CHANGELOG), changelog) {
        eprintln!("error: {CHANGELOG}: {e}");
        return false;
    }
    println!("  {CHANGELOG}: [Unreleased] promoted, link definitions moved");
    println!(
        "\nCargo.lock is generated — `make bump` regenerates it.\nThen `make drift` to verify, and open a PR: \
         docs/internals/releases.md step 3."
    );
    true
}

/// Today, as `YYYY-MM-DD`, for one changelog heading.
///
/// `date(1)` rather than a civil-calendar conversion off `SystemTime`: that is
/// twenty lines of leap-year arithmetic, and `chrono` is a dependency, for a
/// string that appears once per release. POSIX guarantees the format.
fn today() -> String {
    Command::new("date")
        .arg("+%Y-%m-%d")
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .filter(|d| !d.is_empty())
        .unwrap_or_else(|| {
            eprintln!("error: `date +%Y-%m-%d` did not answer with a date.");
            std::process::exit(1);
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The splice, which is the half of the shared table that writes. Getting
    /// this wrong rewrites the syntax around the version rather than the
    /// version, and the failure is a corrupted Cargo.toml mid-release.
    #[test]
    fn rewriting_replaces_the_version_and_nothing_around_it() {
        let pattern = re(r#"(?m)^version = "(\d+\.\d+\.\d+)"$"#);
        let (out, n) = rewrite_group1(&pattern, "version = \"0.0.3\"\nother = 1\n", "9.9.9");
        assert_eq!((out.as_str(), n), ("version = \"9.9.9\"\nother = 1\n", 1));

        // No match is 0 rather than a panic: `bump` turns that into the "this
        // would leave the file behind" refusal, before anything is written.
        let (out, n) = rewrite_group1(&pattern, "nothing here\n", "9.9.9");
        assert_eq!((out.as_str(), n), ("nothing here\n", 0));
    }

    /// The release path's first read, which had never once been executed.
    ///
    /// Three separate ways to be wrong, and each fails a line here: the pattern
    /// not compiling at all (look-around), the body running past the section it
    /// belongs to (a greedy `.*`), and an unwritten section being promoted
    /// anyway (the emptiness guard's input).
    #[test]
    fn the_unreleased_body_is_this_release_and_stops_at_the_last_one() {
        // Two released sections, not one: with a single one below it a greedy
        // `.*` and a lazy `.*?` stop at the same place, and the test passes
        // while the reader swallows the entire history.
        let text = "# Changelog\n\n## [Unreleased]\n\n### Added\n\n- a thing\n\n\
                    ## [0.0.3] - 2026-09-12\n\n- an older thing\n\n\
                    ## [0.0.2] - 2026-09-12\n\n- an older thing still\n";
        assert_eq!(
            unreleased_body(text),
            Some("\n### Added\n\n- a thing\n\n"),
            "the body is what is under Unreleased, not everything after it"
        );

        assert_eq!(
            unreleased_body("# Changelog\n\n## [Unreleased]\n\n## [0.0.3] - x\n").map(str::trim),
            Some(""),
            "an unwritten section reads empty, which is what the refusal tests"
        );

        // A tree with no released section yet cannot be promoted, and says so
        // rather than promoting the whole file.
        assert_eq!(unreleased_body("## [Unreleased]\n\n- a thing\n"), None);
    }

    /// And the committed changelog is still shaped the way the reader expects.
    ///
    /// The unit test above proves the reader; this proves the file it reads.
    /// Both matter, because the failure mode being closed is a release that
    /// stops at the first command of the runbook.
    ///
    /// Deliberately structural and not "has notes in it": the section is empty
    /// on purpose for the whole window between a release and the next change,
    /// starting with the commit that cut this one. Asserting otherwise here
    /// would turn every release into a red build, which is a worse gate than
    /// none — and `bumped_changelog` refuses an empty section at the moment it
    /// actually matters, which is when somebody types `make bump`.
    #[test]
    fn the_committed_changelog_still_has_a_section_to_promote() {
        let text = read_or_exit(CHANGELOG);
        assert!(
            unreleased_body(&text).is_some(),
            "{CHANGELOG} has no `## [Unreleased]` above a released section, so \
             `make bump` would refuse whatever is written in it"
        );
    }

    /// Every pattern in the shared table still matches the file it names.
    ///
    /// This is the check that cannot be written as a comment: a pattern that
    /// matches nothing does not fail `make drift` loudly enough to be noticed in
    /// a release, it just stops gating. Here it is a red `cargo test`.
    #[test]
    fn every_version_site_pattern_still_matches_its_file() {
        let version = workspace_version();
        for (rel, pattern) in version_sites() {
            let text = read_or_exit(rel);
            let found: Vec<&str> = pattern
                .captures_iter(&text)
                .filter_map(|m| Some(m.get(1)?.as_str()))
                .collect();
            assert!(
                !found.is_empty(),
                "{rel}: nothing matches `{}`",
                pattern.as_str()
            );
            assert!(
                found.iter().all(|v| *v == version),
                "{rel}: {found:?} against a workspace at {version}"
            );
        }
    }

    /// The two shapes [`glob`] is allowed to be asked for, against a tree whose
    /// answer is known: this crate's own source is under `crates/*/src/*.rs` and
    /// the architecture page is under `docs/**/*.md`.
    #[test]
    fn the_two_globs_find_what_the_section_check_walks() {
        let rs = glob("crates/*/src/*.rs");
        assert!(
            rs.contains(&"crates/xtask/src/drift.rs".to_string()),
            "{rs:?}"
        );
        assert!(rs.iter().all(|p| p.ends_with(".rs")), "{rs:?}");

        let md = glob("docs/**/*.md");
        assert!(md.contains(&"docs/architecture.md".to_string()), "{md:?}");
        assert!(
            md.contains(&"docs/internals/releases.md".to_string()),
            "the `**` did not recurse: {md:?}"
        );
    }

    /// Section citations close over their prefixes, so citing `7` is fine
    /// wherever only `7.3` is declared.
    #[test]
    fn a_cited_parent_section_resolves_through_its_children() {
        let arch = sections_of("docs/architecture.md");
        assert!(arch.contains("11"), "{arch:?}");
        assert!(!arch.contains("99"));
    }
}
