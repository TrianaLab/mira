//! The test counts [`docs/internals/testing.md`] states, against the tests that
//! exist.
//!
//! That page opens by counting the suite, and every level in its table carries
//! a number. Nothing read any of them. This pull request found the page
//! claiming 422 cargo tests against 425, 20 chart tests against 26 and 52
//! operator tests against 56 — the page a contributor reads to decide where a
//! new test belongs, wrong about how many there are, in three places at once.
//!
//! The counts come from `--list` on the built test binaries rather than from a
//! `#[test]` grep, because the grep is wrong: `block.rs` declares
//! `every_mount_the_check_refuses_is_one_the_table_names` twice, once behind
//! `#[cfg(target_os = "macos")]` and once behind its negation, so a count of
//! attributes is one high on every platform and a count of *tests* is right on
//! all of them. Same reason `cargo tree` takes a `--target` in [`crate::drift`].
//!
//! Two invocations, split the way the workspaces are: `make test` has the
//! engine's binaries built and `make operator-test` has the operator's, and
//! neither leg builds the other — `ci-operator` exists so an engine diff does
//! not compile 220 crates of kube-rs.
//!
//! ```console
//! $ cargo run -p xtask -- testcounts              # engine, UI and chart
//! $ cargo run -p xtask -- testcounts --operator   # the second workspace
//! ```
//!
//! [`docs/internals/testing.md`]: https://miradb.dev/internals/testing/

use crate::util::{Failures, glob, re, read_or_exit, root};

const PAGE: &str = "docs/internals/testing.md";

/// How many tests a `cargo test` target holds, counting only those under
/// `prefix` — `""` for all of them.
///
/// `--list` prints one `name: test` line each and a summary after them, so the
/// suffix is the whole parser. It builds the binary if it is not built, which
/// is why the callers are the targets that have just built it.
fn listed(args: &[&str], prefix: &str) -> Result<usize, String> {
    let mut all: Vec<&str> = args.to_vec();
    all.extend(["--", "--list"]);
    let out = std::process::Command::new("cargo")
        .args(&all)
        .current_dir(root())
        .output()
        .map_err(|e| format!("cargo {}: {e}", all.join(" ")))?;
    if !out.status.success() {
        return Err(format!(
            "cargo {} failed:\n{}",
            all.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    Ok(String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter(|l| l.ends_with(": test") && l.starts_with(prefix))
        .count())
}

/// Lines in `paths` matching `pattern`, summed.
///
/// The UI and the chart have no `--list`: a `node --test` file declares one test
/// per `test(` at column zero and a helm-unittest suite one per `- it:`. Both
/// counts are exact, and both were checked against the runners' own totals when
/// this was written.
fn grepped(paths: &[String], pattern: &str) -> usize {
    let pattern = re(pattern);
    paths
        .iter()
        .map(|p| pattern.find_iter(&read_or_exit(p)).count())
        .sum()
}

/// Assert the page states `n` where `pattern` matches, group 1 being the number.
fn states(page: &str, n: usize, what: &str, pattern: &str, f: &mut Failures) {
    let Some(m) = re(pattern).captures(page) else {
        f.fail(format!(
            "{PAGE} has no line matching `{pattern}`, so the {what} count is no \
             longer gated there. Restore the sentence, or teach \
             crates/xtask/src/testcounts.rs where the page states it now."
        ));
        return;
    };
    let said: usize = m[1].parse().unwrap_or_default();
    if said != n {
        f.fail(format!(
            "{PAGE} says {said} {what}, and there are {n}. The page a \
             contributor reads to decide where a test belongs is the one page \
             that has to know."
        ));
    }
}

/// The engine's counts, plus the UI's and the chart's. Run by `make test`.
pub fn check() -> bool {
    let mut f = Failures::default();
    let counts = [
        listed(&["test", "-q", "-p", "miradb-core", "--lib"], ""),
        listed(
            &["test", "-q", "-p", "miradb-core", "--test", "differential"],
            "",
        ),
        listed(&["test", "-q", "-p", "miradb", "--bin", "mira"], ""),
        listed(&["test", "-q", "-p", "miradb", "--bin", "mira"], "e2e::"),
        listed(&["test", "-q", "-p", "miradb", "--test", "cli"], ""),
        listed(&["test", "-q", "-p", "xtask"], ""),
    ];
    let mut n = Vec::new();
    for c in counts {
        match c {
            Ok(v) => n.push(v),
            Err(e) => {
                eprintln!("error: {e}");
                return false;
            }
        }
    }
    let (core, differential, bin_target, e2e, cli, xtask) = (n[0], n[1], n[2], n[3], n[4], n[5]);
    // `e2e.rs` is a module of the binary, so its tests are in the bin target's
    // list. Level 1 is what is left after taking them out.
    let bin = bin_target - e2e;
    let levels = core + differential + bin_target + cli;
    let ui = grepped(&glob("crates/mira/ui/src/lib/*.test.js"), r"(?m)^test\(");
    let suites = glob("charts/mira-operator/tests/*_test.yaml");
    let chart = grepped(&suites, r"(?m)^\s*- it:");

    let page = read_or_exit(PAGE);
    states(
        &page,
        levels + xtask,
        "cargo tests",
        r"Mira has (\d+) cargo tests",
        &mut f,
    );
    states(
        &page,
        levels,
        "tests across levels 1–5",
        r"(\d+) across levels 1–5",
        &mut f,
    );
    states(
        &page,
        xtask,
        "xtask tests",
        r"plus (\d+) in `xtask`",
        &mut f,
    );
    states(&page, ui, "UI tests", r"(\d+) UI tests", &mut f);
    states(&page, chart, "chart tests", r"(\d+) chart tests", &mut f);
    states(
        &page,
        core,
        "core unit tests",
        r"\| (\d+) core \+ \d+ bin \|",
        &mut f,
    );
    states(
        &page,
        bin,
        "bin unit tests",
        r"\| \d+ core \+ (\d+) bin \|",
        &mut f,
    );
    states(
        &page,
        e2e,
        "in-process end-to-end tests",
        r"\| In-process end-to-end \| (\d+) \|",
        &mut f,
    );
    states(
        &page,
        cli,
        "subprocess CLI tests",
        r"\| Subprocess CLI \| (\d+) \|",
        &mut f,
    );
    states(
        &page,
        ui,
        "browser-free UI tests",
        r"\| Browser-free UI \| (\d+) \|",
        &mut f,
    );
    states(
        &page,
        chart,
        "rendered chart tests",
        r"\| Chart rendering \| (\d+) in \d+ suites \|",
        &mut f,
    );
    states(
        &page,
        suites.len(),
        "chart suites",
        r"\| Chart rendering \| \d+ in (\d+) suites \|",
        &mut f,
    );

    if f.is_empty() {
        println!(
            "test counts: {} cargo, {ui} UI, {chart} chart — as stated.",
            levels + xtask
        );
    }
    f.report("test count(s) the page states wrongly")
}

/// The second workspace's counts. Run by `make operator-test`.
pub fn check_operator() -> bool {
    let mut f = Failures::default();
    let manifest = "integrations/kubernetes/Cargo.toml";
    let all = listed(&["test", "-q", "--manifest-path", manifest], "");
    let apiserver = listed(
        &[
            "test",
            "-q",
            "--manifest-path",
            manifest,
            "--test",
            "apiserver",
        ],
        "",
    );
    let (Ok(all), Ok(apiserver)) = (all, apiserver) else {
        eprintln!("error: could not list the operator's tests");
        return false;
    };

    let page = read_or_exit(PAGE);
    states(&page, all, "operator tests", r"and (\d+) more in", &mut f);
    states(
        &page,
        apiserver,
        "API-server tests",
        r"\| Against a real API server \| (\d+) \|",
        &mut f,
    );
    // The gate runs the whole manifest, so the apiserver target is *in* that
    // 56 — it just returns immediately without `MIRA_OPERATOR_APISERVER`. The
    // number worth stating twice on the page is the one that can actually
    // fail on a laptop with no cluster.
    for pattern in [r"fmt, clippy, (\d+) unit tests", r"Its (\d+) unit tests"] {
        states(
            &page,
            all - apiserver,
            "operator unit tests",
            pattern,
            &mut f,
        );
    }

    if f.is_empty() {
        println!("test counts: {all} in the operator's workspace — as stated.");
    }
    f.report("test count(s) the page states wrongly")
}
