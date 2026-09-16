//! Check the CI configuration the way CI checks everything else.
//!
//! Branch protection requires a small, fixed set of status contexts — for Mira,
//! two: `required` and `security-required`. Every other job earns its authority
//! by being in one of those jobs' transitive `needs:` closure. That design has
//! one failure mode, and it is silent: add a job, forget to wire it into a gate,
//! and it now runs, goes red, and merges anyway. Nobody notices for months.
//!
//! The release has the same shape and the same failure mode wearing different
//! clothes. A tag push has no status context to require, so the thing that holds
//! it together is the terminal `verify-release` job: every publisher must be in
//! its `needs:` closure, or a publisher can fail — or worse, be skipped — with
//! the run still reported green, which is how a release ships nothing and says
//! it worked.
//!
//! So this reads the workflow files and asserts the graph is what everyone
//! assumes it is:
//!
//!   * every job in a PR-triggered workflow is reachable from a required
//!     context, or is on the allowlist below with a written reason;
//!   * the allowlist is bidirectional — an entry that is stale, or that names a
//!     job which is in fact reachable, fails just as loudly as a missing one;
//!   * every job in a publishing workflow is reachable from its terminal job;
//!   * a job downstream of an `if: always()` gate names a status function too,
//!     because the skip that gate absorbs keeps travelling down the chain;
//!   * the gate job itself is `if: always()` and actually inspects every leg,
//!     rather than passing because its dependencies were skipped;
//!   * no required workflow carries a trigger-level `paths:` filter, because a
//!     filtered-out required check never starts, never reports, and strict
//!     branch protection waits for it forever;
//!   * every `run:` in `ci.yml` is a single `make ci-*` call into a target that
//!     exists in `ci.mk`, so every leg is one command on a laptop and nobody has
//!     to debug shell by pushing commits;
//!   * every third-party action is pinned to a full commit SHA with a version
//!     comment, and every workflow declares `permissions:` and `concurrency:`.
//!
//! Run it with `make workflows`.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use yaml_rust2::Yaml;

use crate::util::{Failures, re, root};

/// The status contexts configured as required in branch protection for `main`.
///
/// This list and that setting are the same fact stored twice; there is no API
/// call that can reconcile them without a token, so the convention is: change
/// one, change the other, in the same PR.
///
/// Two rather than one because the contexts are the API between this repository
/// and a setting nobody in a pull request can edit. Security legs land at their
/// own rate — a scanner, a SARIF sink — and `security-required` lets them join
/// without a settings change and without diluting what `required` means.
const REQUIRED_CONTEXTS: [&str; 2] = ["required", "security-required"];

/// Workflows that publish rather than gate, and the job that must be able to
/// see every other job in them.
///
/// There is no status context to require on a tag push, so this is the same
/// reachability invariant in the only form the event allows: if a publisher is
/// not in the terminal job's closure, it can fail or be skipped and the run
/// still reports green.
const TERMINAL_JOBS: [(&str, &str); 1] = [("release.yml", "verify-release")];

/// Workflows that must be pure dispatchers over `ci.mk`: every `run:` in them
/// is one `make ci-*` call, so a red leg is a leg you can reproduce with one
/// command.
///
/// The alternative is what every repository drifts into — shell that exists
/// only inside YAML, debugged by pushing commits and waiting six minutes, and
/// slowly diverging from whatever `make` does locally.
///
/// The jobs exempted from the rule are [`REQUIRED_CONTEXTS`], by job *name*,
/// because their shell reads `join(needs.*.result, ' ')` — workflow state that
/// exists nowhere but there. There is nothing for a local target to reproduce,
/// so hiding it behind `make` would buy an indirection and lose the only place
/// a reader can see what the gate actually does.
const MAKE_DISPATCHED: [&str; 1] = ["ci.yml"];

/// Jobs that are deliberately NOT reachable from a required context.
///
/// Each needs a reason, and the reason is read by a human, so write it for one.
/// Empty is very nearly the right state — an entry here is usually an admission
/// that something runs on pull requests without being able to block a merge.
///
/// The exception is a job *downstream* of a gate. That is the one shape where
/// unreachable is stronger than reachable rather than weaker: it cannot start
/// until the gate has passed, and the check below walks `needs:` forwards, so it
/// sees the edge pointing the wrong way and cannot tell the two apart. Widening
/// the walk to accept "needs a gate" would also accept `needs: [required]` plus
/// `if: always()`, which runs on a red main — so the exemption is written down
/// one job at a time instead.
const UNREACHABLE_ALLOWLIST: [(&str, &str); 1] = [(
    "tag",
    "downstream of both gates, not upstream: it `needs: [required, \
     security-required]` and runs only on a push to main, so there is no pull \
     request for it to fail to block",
)];

/// Actions from these owners still have to be SHA-pinned; nobody is exempt.
///
/// The set exists only so a first-party action's missing version comment reads
/// as the nit it is rather than as a supply-chain finding.
const FIRST_PARTY: [&str; 2] = ["actions", "github"];

pub fn run() -> bool {
    let dir = root().join(".github/workflows");
    let mut files: Vec<_> = std::fs::read_dir(&dir)
        .unwrap_or_else(|e| {
            eprintln!("error: {}: {e}", dir.display());
            std::process::exit(1);
        })
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| matches!(p.extension().and_then(|e| e.to_str()), Some("yml" | "yaml")))
        .collect();
    files.sort();
    if files.is_empty() {
        eprintln!("error: no workflows found under {}", dir.display());
        std::process::exit(1);
    }

    let mut f = Failures::default();
    for path in &files {
        check_workflow(path, &mut f);
    }

    let names: Vec<_> = files
        .iter()
        .filter_map(|p| p.file_name()?.to_str())
        .collect();
    println!("checked {} workflow(s): {}", files.len(), names.join(", "));
    if !f.report("problem(s)") {
        return false;
    }
    println!(
        "required context(s) {} cover every PR job.",
        REQUIRED_CONTEXTS.join(", ")
    );
    for (wf, terminal) in TERMINAL_JOBS {
        println!("{wf}: `{terminal}` covers every publishing job.");
    }
    for wf in MAKE_DISPATCHED {
        println!("{wf}: every step is a `make ci-*` target in ci.mk.");
    }
    true
}

fn check_workflow(path: &Path, f: &mut Failures) {
    let text = std::fs::read_to_string(path).unwrap_or_default();
    let name = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or_default();
    let doc = match crate::util::parse_yaml(&text) {
        Ok(doc) => doc,
        Err(e) => {
            f.fail(format!("{name}: is not valid YAML: {e}"));
            return;
        }
    };
    let jobs = jobs_of(&doc);

    if doc["permissions"].is_badvalue() {
        f.fail(format!(
            "{name}: no top-level `permissions:`. Without it the job gets \
             whatever the repository default is, which for many repos is write \
             to everything."
        ));
    }
    if doc["concurrency"].is_badvalue() {
        f.fail(format!(
            "{name}: no `concurrency:` group; superseded runs will pile up."
        ));
    }

    check_pins(name, &text, f);
    check_terminal(name, &jobs, f);
    check_skip_propagation(name, &jobs, f);
    check_make_dispatch(name, &jobs, f);

    let on = triggers(&doc);
    if !on.contains_key("pull_request") {
        return; // not a gating workflow; the checks below are about merges
    }
    check_gates(name, &jobs, &on, f);
}

/// The `on:` mapping, normalised to a set of event names.
///
/// The keys are kept alongside their bodies because the path-filter check needs
/// the body. A bare string or a list of strings is the same trigger with no
/// options, so both flatten to entries with an empty body.
///
/// YAML 1.1 read the bare word `on` as a boolean, and a 1.1 parser hands back
/// the key `true` — the single most common reason a workflow-linting script
/// silently checks nothing. This is a 1.2 parser, where `on` is a plain string,
/// so the trap is gone; the `true` arm below is kept because the cost of being
/// wrong about that is a checker that passes everything.
fn triggers(doc: &Yaml) -> BTreeMap<String, Yaml> {
    let Some(hash) = doc.as_hash() else {
        return BTreeMap::new();
    };
    for key in [Yaml::String("on".into()), Yaml::Boolean(true)] {
        let Some(value) = hash.get(&key) else {
            continue;
        };
        return match value {
            Yaml::String(s) => BTreeMap::from([(s.clone(), Yaml::BadValue)]),
            Yaml::Array(items) => items
                .iter()
                .filter_map(|i| Some((i.as_str()?.to_string(), Yaml::BadValue)))
                .collect(),
            Yaml::Hash(h) => h
                .iter()
                .filter_map(|(k, v)| Some((k.as_str()?.to_string(), v.clone())))
                .collect(),
            _ => continue,
        };
    }
    BTreeMap::new()
}

fn jobs_of(doc: &Yaml) -> BTreeMap<String, Yaml> {
    doc["jobs"]
        .as_hash()
        .map(|h| {
            h.iter()
                .filter_map(|(k, v)| Some((k.as_str()?.to_string(), v.clone())))
                .collect()
        })
        .unwrap_or_default()
}

/// A job's display name, which is what branch protection matches on — falling
/// back to its id, which is what GitHub falls back to.
fn display_name<'a>(id: &'a str, job: &'a Yaml) -> &'a str {
    job["name"].as_str().unwrap_or(id)
}

/// Transitive `needs:` closure, roots included.
fn closure(jobs: &BTreeMap<String, Yaml>, roots: &BTreeSet<String>) -> BTreeSet<String> {
    let mut seen = BTreeSet::new();
    let mut stack: Vec<String> = roots.iter().cloned().collect();
    while let Some(id) = stack.pop() {
        let Some(job) = jobs.get(&id) else { continue };
        if !seen.insert(id) {
            continue;
        }
        match &job["needs"] {
            Yaml::String(s) => stack.push(s.clone()),
            Yaml::Array(items) => {
                stack.extend(items.iter().filter_map(|i| Some(i.as_str()?.into())));
            }
            _ => {}
        }
    }
    seen
}

/// Pinning is checked on the raw text, because YAML throws comments away.
fn check_pins(name: &str, text: &str, f: &mut Failures) {
    let uses = re(r"(?m)^\s*-?\s*uses:\s*(\S+)\s*(#.*)?$");
    let sha = re("^[0-9a-f]{40}$");
    let versionish = re(r"^v?\d");
    for (lineno, line) in text.lines().enumerate() {
        let Some(m) = uses.captures(line) else {
            continue;
        };
        let r = &m[1];
        if r.starts_with("./") || r.starts_with("docker://") {
            continue;
        }
        let where_ = format!("{name}:{}", lineno + 1);
        let Some((action, version)) = r.rsplit_once('@') else {
            f.fail(format!("{where_}: `uses: {r}` has no version at all"));
            continue;
        };
        if !sha.is_match(version) {
            f.fail(format!(
                "{where_}: `{action}` is pinned to `{version}`, a tag. Tags move, \
                 and a moving tag is arbitrary code execution with our token. Pin \
                 the 40-character commit SHA and put the tag in a trailing comment."
            ));
            continue;
        }
        let comment = m
            .get(2)
            .map_or("", |c| c.as_str())
            .trim_matches(|c| c == '#' || c == ' ');
        if !versionish.is_match(comment) {
            let owner = action.split('/').next().unwrap_or_default();
            let severity = if FIRST_PARTY.contains(&owner) {
                "nit"
            } else {
                "unreviewable"
            };
            f.fail(format!(
                "{where_}: `{action}` is SHA-pinned but has no `# vX.Y.Z` comment \
                 ({severity}). Without it nobody can tell what version this is, and \
                 Dependabot has nothing to bump."
            ));
        }
    }
}

/// A publishing workflow's terminal job must be able to see every other job.
///
/// Same [`closure`] as the PR side, rooted differently. The bug it catches is
/// the one nobody sees: a publisher whose result nothing reads, so the run is
/// green whether it pushed, failed, or never ran at all.
fn check_terminal(name: &str, jobs: &BTreeMap<String, Yaml>, f: &mut Failures) {
    let Some((_, terminal)) = TERMINAL_JOBS.iter().find(|(wf, _)| *wf == name) else {
        return;
    };
    if !jobs.contains_key(*terminal) {
        f.fail(format!(
            "{name}: declares no `{terminal}` job. This workflow publishes, so the \
             only thing that can prove it published is a terminal job that re-reads \
             what came out. Add it, or drop the entry from TERMINAL_JOBS in this file."
        ));
        return;
    }
    let reachable = closure(jobs, &BTreeSet::from([(*terminal).to_string()]));
    for id in jobs.keys().filter(|id| !reachable.contains(*id)) {
        f.fail(format!(
            "{name}:{id}: is not in the `needs:` closure of `{terminal}`. Nothing \
             reads its result, so it can go red — or be skipped by an `if:` nobody \
             re-read — and the release still reports success. Add it to \
             `{terminal}`'s `needs:`, directly or through a job that is already there."
        ));
    }
}

/// A skip runs the length of the chain, not one edge of it.
///
/// An aggregate gate carries `if: always()` precisely because legs upstream of
/// it are skipped — but that only rescues the gate. GitHub keeps propagating the
/// skip past it, so the job *after* the gate inherits it and never runs. Naming
/// a status function is the only thing that stops the propagation, and doing so
/// also turns off the implicit `success()`, which is why the results then have
/// to be spelled out by hand.
///
/// `ci.yml`'s `tag` job was skipped on every push to main from the day it was
/// written: green run, green gates, four releases tagged by hand.
fn check_skip_propagation(name: &str, jobs: &BTreeMap<String, Yaml>, f: &mut Failures) {
    let status_fn = re(r"\b(always|cancelled|failure|success)\(\)");
    let survivors: BTreeSet<String> = jobs
        .iter()
        .filter(|(_, job)| status_fn.is_match(job["if"].as_str().unwrap_or_default()))
        .map(|(id, _)| id.clone())
        .collect();
    for id in jobs.keys().filter(|id| !survivors.contains(*id)) {
        let mut ancestors = closure(jobs, &BTreeSet::from([id.clone()]));
        ancestors.remove(id);
        let Some(gate) = ancestors.iter().find(|a| survivors.contains(*a)) else {
            continue;
        };
        f.fail(format!(
            "{name}:{id}: `needs:` reaches `{gate}`, which survives a skipped \
             dependency by naming a status function. `{id}` does not, so it inherits \
             the skip the gate was written to absorb and never runs — a green run \
             with a job that silently did nothing. Name a status function in its \
             `if:` too — `!cancelled()`, not `always()`, because an `always()` gate \
             reports success on a cancelled run — and spell out the `needs.*.result` \
             values the implicit `success()` used to check."
        ));
    }
}

/// Target names declared in `ci.mk`.
///
/// A regex rather than `make -pn`: parsing the database means running make, and
/// make runs `$(shell ...)` in the Makefile's variable assignments to do it.
/// This check has to be able to say "that target does not exist" on a machine
/// where cargo is missing.
fn ci_mk_targets(f: &mut Failures) -> BTreeSet<String> {
    let Ok(text) = crate::util::read("ci.mk") else {
        f.fail("ci.mk: does not exist, but ci.yml is declared to dispatch to it.");
        return BTreeSet::new();
    };
    // `:` and not `:=`, which is an assignment rather than a target.
    let target = re("^([A-Za-z0-9_.-]+):($|[^=])");
    text.lines()
        .filter_map(|l| Some(target.captures(l)?[1].to_string()))
        .collect()
}

/// Every `run:` is a `make ci-*` call into a target that exists.
fn check_make_dispatch(name: &str, jobs: &BTreeMap<String, Yaml>, f: &mut Failures) {
    if !MAKE_DISPATCHED.contains(&name) {
        return;
    }
    let targets = ci_mk_targets(f);
    // `make ci-foo VAR=value` — the target is the first word after `make`.
    let make_run = re(r"^make\s+([A-Za-z0-9_.-]+)(\s|$)");
    for (id, job) in jobs {
        if REQUIRED_CONTEXTS.contains(&display_name(id, job)) {
            continue;
        }
        let steps = job["steps"].as_vec().map(Vec::as_slice).unwrap_or_default();
        for (index, step) in steps.iter().enumerate() {
            let Some(run) = step["run"].as_str() else {
                continue;
            };
            let label = step["name"]
                .as_str()
                .map_or_else(|| format!("step {index}"), str::to_string);
            let where_ = format!("{name}:{id}:{label}");
            let body = run.trim();
            let single = body.lines().count() == 1;
            let Some(m) = single.then(|| make_run.captures(body)).flatten() else {
                f.fail(format!(
                    "{where_}: is shell in a workflow file. Every step here must be \
                     one `make ci-*` call, so the leg can be run on a laptop and so \
                     `make ci` means the same thing as a green pull request. Move the \
                     body into a target in ci.mk."
                ));
                continue;
            };
            let target = &m[1];
            if !target.starts_with("ci-") {
                f.fail(format!(
                    "{where_}: calls `make {target}`, not a `ci-` target. The `ci-` \
                     prefix is what makes ci.mk readable against the job list: one \
                     target per leg, named after it. Wrap it — `ci-<leg>: {target}` — \
                     and call that."
                ));
            } else if !targets.contains(target) {
                f.fail(format!(
                    "{where_}: calls `make {target}`, which ci.mk does not define. \
                     Either the target was renamed and this was not, or it lives in \
                     the Makefile and belongs here."
                ));
            }
        }
    }
}

fn check_gates(
    name: &str,
    jobs: &BTreeMap<String, Yaml>,
    on: &BTreeMap<String, Yaml>,
    f: &mut Failures,
) {
    for event in ["pull_request", "push"] {
        let Some(spec) = on.get(event) else { continue };
        if !spec["paths"].is_badvalue() || !spec["paths-ignore"].is_badvalue() {
            f.fail(format!(
                "{name}: `on.{event}` has a path filter. This workflow publishes a \
                 required status context, and a filtered-out required check never \
                 starts, never reports, and blocks the PR forever. Filter in a \
                 `changes` job instead and let the gate treat `skipped` as a pass."
            ));
        }
    }

    let gates: BTreeSet<String> = jobs
        .iter()
        .filter(|(id, job)| REQUIRED_CONTEXTS.contains(&display_name(id, job)))
        .map(|(id, _)| id.clone())
        .collect();
    if gates.is_empty() {
        f.fail(format!(
            "{name}: is triggered on pull_request but defines no job named {}. \
             Nothing here can gate a merge — either add the aggregate gate or stop \
             running it on PRs.",
            REQUIRED_CONTEXTS.join(" or ")
        ));
        return;
    }

    for id in &gates {
        let gate = &jobs[id];
        if gate["if"].as_str().unwrap_or_default().trim() != "always()" {
            f.fail(format!(
                "{name}:{id}: the aggregate gate must be `if: always()`. Without it \
                 the gate is itself skipped the moment any leg fails, and a skipped \
                 required check is reported as neutral — the PR goes green."
            ));
        }
        // Dumped rather than walked: the expression can be in any of the gate's
        // steps, in an `env:` value or in an `if:`, and what matters is that it
        // is in there somewhere.
        let mut body = String::new();
        let mut emitter = yaml_rust2::YamlEmitter::new(&mut body);
        let _ = emitter.dump(gate);
        if !body.contains("join(needs.*.result") {
            f.fail(format!(
                "{name}:{id}: does not inspect `join(needs.*.result, ' ')`. A gate \
                 that only depends on its legs passes when they are skipped *or* \
                 failed, because a failed dependency skips the gate and `always()` \
                 then runs it with nothing checked."
            ));
        }
    }

    let reachable = closure(jobs, &gates);
    for id in jobs.keys() {
        if reachable.contains(id) || UNREACHABLE_ALLOWLIST.iter().any(|(j, _)| j == id) {
            continue;
        }
        f.fail(format!(
            "{name}:{id}: runs on pull requests but is not in the `needs:` closure \
             of any required context ({}). It can go red and the PR will still \
             merge. Add it to the gate's `needs:`, or add it to \
             UNREACHABLE_ALLOWLIST in this file with a reason.",
            REQUIRED_CONTEXTS.join(", ")
        ));
    }
    for (id, reason) in UNREACHABLE_ALLOWLIST {
        if !jobs.contains_key(id) {
            f.fail(format!(
                "{name}: UNREACHABLE_ALLOWLIST names `{id}` ({reason}) but no such \
                 job exists. A stale exemption is how the next real one gets waved \
                 through — delete it."
            ));
        } else if reachable.contains(id) {
            f.fail(format!(
                "{name}: UNREACHABLE_ALLOWLIST names `{id}` ({reason}) but it *is* \
                 reachable from the gate. Delete the entry; the exemption is a lie \
                 about how the graph works."
            ));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn yaml(text: &str) -> Yaml {
        crate::util::parse_yaml(text).unwrap()
    }

    /// The trap this file's [`triggers`] doc comment is about. A YAML 1.1
    /// parser answers `true` here and a checker that only looks for the string
    /// `"on"` then checks nothing at all, silently, forever.
    #[test]
    fn the_on_key_is_a_string_and_not_a_boolean() {
        let on = triggers(&yaml("on:\n  pull_request:\n    branches: [main]\n"));
        assert!(on.contains_key("pull_request"), "{:?}", on.keys());
    }

    #[test]
    fn a_trigger_reads_the_same_as_a_string_a_list_or_a_map() {
        for text in ["on: push", "on: [push]", "on:\n  push:\n"] {
            assert!(triggers(&yaml(text)).contains_key("push"), "{text}");
        }
    }

    #[test]
    fn the_needs_closure_is_transitive_and_survives_a_dangling_edge() {
        let jobs = jobs_of(&yaml(
            "jobs:\n  a: {}\n  b:\n    needs: a\n  c:\n    needs: [b, gone]\n  loose: {}\n",
        ));
        let from_c = closure(&jobs, &BTreeSet::from(["c".to_string()]));
        assert_eq!(from_c, BTreeSet::from(["a".into(), "b".into(), "c".into()]));
        assert!(!from_c.contains("loose"));
    }

    /// The pin rules, one line each, because the message a contributor gets is
    /// the whole value of this check.
    #[test]
    fn a_pin_must_be_a_sha_and_must_say_what_version_it_is() {
        let sha = "3d3c42e5aac5ba805825da76410c181273ba90b1";
        let cases = [
            (
                format!("      - uses: actions/checkout@{sha} # v7.0.1"),
                None,
            ),
            ("      - uses: ./.github/actions/x".into(), None),
            (
                "      - uses: actions/checkout@v7".into(),
                Some("a tag. Tags move"),
            ),
            (
                "      - uses: actions/checkout".into(),
                Some("no version at all"),
            ),
            (
                format!("      - uses: actions/checkout@{sha}"),
                Some("(nit)"),
            ),
            (
                format!("      - uses: rando/thing@{sha}"),
                Some("(unreviewable)"),
            ),
        ];
        for (line, want) in cases {
            let mut f = Failures::default();
            check_pins("t.yml", &line, &mut f);
            match want {
                None => assert!(f.is_empty(), "{line} should pass"),
                Some(needle) => assert!(
                    f.0.iter().any(|m| m.contains(needle)),
                    "{line} should say {needle:?}, said {:?}",
                    f.0
                ),
            }
        }
    }

    /// The whole point of the file, reduced: a job nothing depends on runs on
    /// every pull request and can never block one — and the allowlist is the
    /// only thing that excuses it, one named job at a time.
    #[test]
    fn a_job_outside_the_gates_closure_is_a_failure() {
        // Every allowlisted job, so the bidirectional half does not fire on a
        // fixture too small to contain them. That is also the assertion: these
        // are as orphaned as `orphan` and only the allowlist tells them apart.
        let excused: String = UNREACHABLE_ALLOWLIST
            .iter()
            .map(|(id, _)| format!("  {id}: {{}}\n"))
            .collect();
        let doc = yaml(&format!(
            "on:\n  pull_request:\n\
             jobs:\n\
             \x20 lint: {{}}\n\
             \x20 orphan: {{}}\n\
             {excused}\
             \x20 gate:\n\
             \x20   name: required\n\
             \x20   if: always()\n\
             \x20   needs: [lint]\n\
             \x20   steps:\n\
             \x20     - run: echo \"${{{{ join(needs.*.result, ' ') }}}}\"\n"
        ));
        let jobs = jobs_of(&doc);
        let mut f = Failures::default();
        check_gates("t.yml", &jobs, &triggers(&doc), &mut f);
        assert_eq!(f.0.len(), 1, "{:?}", f.0);
        assert!(f.0[0].contains("t.yml:orphan"), "{:?}", f.0);
    }

    /// Both halves of the gate's own contract, which is the one nobody can see
    /// by reading the job list.
    #[test]
    fn a_gate_that_cannot_fail_is_a_failure() {
        let doc = yaml(
            "on:\n  pull_request:\n\
             jobs:\n\
             \x20 gate:\n\
             \x20   name: required\n\
             \x20   needs: []\n\
             \x20   steps:\n\
             \x20     - run: 'true'\n",
        );
        let mut f = Failures::default();
        check_gates("t.yml", &jobs_of(&doc), &triggers(&doc), &mut f);
        assert!(f.0.iter().any(|m| m.contains("always()")), "{:?}", f.0);
        assert!(
            f.0.iter().any(|m| m.contains("join(needs.*.result")),
            "{:?}",
            f.0
        );
    }

    /// A trigger-level path filter on a required workflow is the outage that
    /// looks like a hang: the check never starts, so strict protection waits
    /// for a report that is never coming.
    #[test]
    fn a_path_filter_on_a_required_workflow_is_a_failure() {
        let doc = yaml(
            "on:\n  pull_request:\n    paths: ['src/**']\n\
             jobs:\n\
             \x20 gate:\n\
             \x20   name: required\n\
             \x20   if: always()\n\
             \x20   steps:\n\
             \x20     - run: echo \"${{ join(needs.*.result, ' ') }}\"\n",
        );
        let mut f = Failures::default();
        check_gates("t.yml", &jobs_of(&doc), &triggers(&doc), &mut f);
        assert!(f.0.iter().any(|m| m.contains("path filter")), "{:?}", f.0);
    }

    /// The shape of the `tag` outage, reduced: a leg that skips, a gate that
    /// survives it, and a job after the gate. Run on GitHub, `after` is skipped
    /// and `after_ok` is not — so the gate's `always()` has to be repeated.
    #[test]
    fn a_job_after_an_always_gate_must_say_always_itself() {
        let doc = yaml(
            "jobs:\n\
             \x20 leg:\n\
             \x20   if: needs.changes.outputs.code == 'true'\n\
             \x20 gate:\n\
             \x20   if: always()\n\
             \x20   needs: [leg]\n\
             \x20 after:\n\
             \x20   if: github.ref == 'refs/heads/main'\n\
             \x20   needs: [gate]\n\
             \x20 after_ok:\n\
             \x20   if: always() && needs.gate.result == 'success'\n\
             \x20   needs: [gate]\n",
        );
        let mut f = Failures::default();
        check_skip_propagation("t.yml", &jobs_of(&doc), &mut f);
        assert_eq!(f.0.len(), 1, "{:?}", f.0);
        assert!(f.0[0].contains("t.yml:after:"), "{:?}", f.0);
    }

    /// The tree's own workflows, checked by the checker. If this fails, `make
    /// workflows` was going to fail too — but here it fails in `cargo test`,
    /// which is the run somebody is already watching.
    #[test]
    fn the_workflows_in_this_repository_pass() {
        assert!(run());
    }
}
