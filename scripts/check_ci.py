#!/usr/bin/env python3
"""Check the CI configuration the way CI checks everything else.

Branch protection requires a small, fixed set of status contexts — for Mira,
two: `required` and `security-required`. Every other job earns its authority by
being in one of those jobs' transitive `needs:` closure. That design has one
failure mode, and it is silent: add a job, forget to wire it into a gate, and it
now runs, goes red, and merges anyway. Nobody notices for months.

The release has the same shape and the same failure mode wearing different
clothes. A tag push has no status context to require, so the thing that holds it
together is the terminal `verify-release` job: every publisher must be in its
`needs:` closure, or a publisher can fail — or worse, be skipped — with the run
still reported green, which is how a release ships nothing and says it worked.

So this script reads the workflow files and asserts the graph is what everyone
assumes it is:

  * every job in a PR-triggered workflow is reachable from a required context,
    or is on the allowlist below with a written reason;
  * the allowlist is bidirectional — an entry that is stale, or that names a
    job which is in fact reachable, fails just as loudly as a missing one;
  * every job in a publishing workflow is reachable from its terminal job;
  * the gate job itself is `if: always()` and actually inspects every leg,
    rather than passing because its dependencies were skipped;
  * no required workflow carries a trigger-level `paths:` filter, because a
    filtered-out required check never starts, never reports, and strict branch
    protection waits for it forever;
  * every `run:` in `ci.yml` is a single `make ci-*` call into a target that
    exists in `ci.mk`, so every leg is one command on a laptop and nobody has
    to debug shell by pushing commits;
  * every third-party action is pinned to a full commit SHA with a version
    comment, and every workflow declares `permissions:` and `concurrency:`.

Run it with `make workflows`.
"""

from __future__ import annotations

import re
import sys
from pathlib import Path

try:
    import yaml
except ModuleNotFoundError:  # pragma: no cover - environment problem, not logic
    sys.exit(
        "error: PyYAML is not installed.\n"
        "  run: pip install -r scripts/requirements.txt"
    )

ROOT = Path(__file__).resolve().parent.parent
WORKFLOWS = ROOT / ".github" / "workflows"
CI_MK = ROOT / "ci.mk"

# The status contexts configured as required in branch protection for `main`.
# This list and that setting are the same fact stored twice; there is no API
# call that can reconcile them without a token, so the convention is: change
# one, change the other, in the same PR.
#
# Two rather than one because the contexts are the API between this repository
# and a setting nobody in a pull request can edit. Security legs land at their
# own rate — a scanner, a SARIF sink — and `security-required` lets them join
# without a settings change and without diluting what `required` means.
REQUIRED_CONTEXTS = {"required", "security-required"}

# Workflows that publish rather than gate, and the job that must be able to see
# every other job in them. There is no status context to require on a tag push,
# so this is the same reachability invariant in the only form the event allows:
# if a publisher is not in the terminal job's closure, it can fail or be skipped
# and the run still reports green.
TERMINAL_JOBS = {"release.yml": "verify-release"}

# Workflows that must be pure dispatchers over `ci.mk`: every `run:` in them is
# one `make ci-*` call, so a red leg is a leg you can reproduce with one
# command. The alternative is what every repository drifts into — shell that
# exists only inside YAML, debugged by pushing commits and waiting six minutes,
# and slowly diverging from whatever `make` does locally.
#
# The value maps a workflow to the jobs exempted from the rule, by job *name*.
# `required` and `security-required` are exempt because their shell reads
# `join(needs.*.result, ' ')` — workflow state that exists nowhere but here.
# There is nothing for a local target to reproduce, so hiding it behind `make`
# would buy an indirection and lose the only place a reader can see what the
# gate actually does.
MAKE_DISPATCHED = {"ci.yml": REQUIRED_CONTEXTS}

# `make ci-foo VAR=value` — the target is the first word after `make`.
MAKE_RUN = re.compile(r"^make\s+(?P<target>[A-Za-z0-9_.-]+)(\s|$)")
MAKE_TARGET = re.compile(r"^(?P<target>[A-Za-z0-9_.-]+):(?!=)")

# Jobs that are deliberately NOT reachable from a required context. Each needs a
# reason, and the reason is read by a human, so write it for one. Empty is the
# right state — an entry here is an admission that something runs on PRs without
# being able to block a merge.
UNREACHABLE_ALLOWLIST: dict[str, str] = {}

# Actions from these owners still have to be SHA-pinned; nobody is exempt. The
# set exists only so a first-party action's missing version comment reads as the
# nit it is rather than as a supply-chain finding.
FIRST_PARTY = {"actions", "github"}

SHA_PIN = re.compile(r"^[0-9a-f]{40}$")
USES_LINE = re.compile(r"^\s*-?\s*uses:\s*(?P<ref>\S+)\s*(?P<comment>#.*)?$")

failures: list[str] = []


def fail(where: str, msg: str) -> None:
    failures.append(f"{where}: {msg}")


def triggers(doc: dict) -> dict:
    """Return the `on:` mapping.

    YAML 1.1 says the bare word `on` is a boolean, and PyYAML agrees, so the
    key that every GitHub workflow in the world uses arrives as `True`. This
    is the single most common reason a workflow-linting script silently checks
    nothing.
    """
    for key in ("on", True):
        if key in doc:
            value = doc[key]
            if isinstance(value, str):
                return {value: None}
            if isinstance(value, list):
                return dict.fromkeys(value)
            if isinstance(value, dict):
                return value
    return {}


def closure(jobs: dict, roots: set[str]) -> set[str]:
    """Transitive `needs:` closure, roots included."""
    seen: set[str] = set()
    stack = list(roots)
    while stack:
        job_id = stack.pop()
        if job_id in seen or job_id not in jobs:
            continue
        seen.add(job_id)
        needs = (jobs[job_id] or {}).get("needs") or []
        if isinstance(needs, str):
            needs = [needs]
        stack.extend(needs)
    return seen


def check_pins(path: Path) -> None:
    """Pinning is checked on the raw text, because YAML throws comments away."""
    for lineno, line in enumerate(path.read_text().splitlines(), 1):
        m = USES_LINE.match(line)
        if not m:
            continue
        ref = m.group("ref")
        if ref.startswith(("./", "docker://")):
            continue
        where = f"{path.name}:{lineno}"
        if "@" not in ref:
            fail(where, f"`uses: {ref}` has no version at all")
            continue
        action, version = ref.rsplit("@", 1)
        if not SHA_PIN.match(version):
            fail(
                where,
                f"`{action}` is pinned to `{version}`, a tag. Tags move, and a "
                "moving tag is arbitrary code execution with our token. Pin the "
                "40-character commit SHA and put the tag in a trailing comment.",
            )
            continue
        comment = (m.group("comment") or "").strip("# ").strip()
        if not re.match(r"^v?\d", comment):
            owner = action.split("/")[0]
            severity = "nit" if owner in FIRST_PARTY else "unreviewable"
            fail(
                where,
                f"`{action}` is SHA-pinned but has no `# vX.Y.Z` comment "
                f"({severity}). Without it nobody can tell what version this is, "
                "and Dependabot has nothing to bump.",
            )


def check_terminal(name: str, jobs: dict) -> None:
    """A publishing workflow's terminal job must be able to see every other job.

    Same `closure()` as the PR side, rooted differently. The bug it catches is
    the one nobody sees: a publisher whose result nothing reads, so the run is
    green whether it pushed, failed, or never ran at all.
    """
    terminal = TERMINAL_JOBS.get(name)
    if terminal is None:
        return
    if terminal not in jobs:
        fail(
            name,
            f"declares no `{terminal}` job. This workflow publishes, so the "
            "only thing that can prove it published is a terminal job that "
            "re-reads what came out. Add it, or drop the entry from "
            "TERMINAL_JOBS in this file.",
        )
        return
    for job_id in sorted(set(jobs) - closure(jobs, {terminal})):
        fail(
            f"{name}:{job_id}",
            f"is not in the `needs:` closure of `{terminal}`. Nothing reads its "
            "result, so it can go red — or be skipped by an `if:` nobody "
            "re-read — and the release still reports success. Add it to "
            f"`{terminal}`'s `needs:`, directly or through a job that is "
            "already there.",
        )


def ci_mk_targets() -> set[str]:
    """Target names declared in `ci.mk`.

    A regex rather than `make -pn`: parsing the database means running make,
    and make runs `$(shell ...)` in the Makefile's variable assignments to do
    it. This check has to be able to say "that target does not exist" on a
    machine where cargo is missing.
    """
    if not CI_MK.exists():
        fail("ci.mk", "does not exist, but ci.yml is declared to dispatch to it.")
        return set()
    return {
        m.group("target")
        for line in CI_MK.read_text().splitlines()
        if (m := MAKE_TARGET.match(line))
    }


def check_make_dispatch(name: str, jobs: dict) -> None:
    """Every `run:` is a `make ci-*` call into a target that exists."""
    exempt = MAKE_DISPATCHED.get(name)
    if exempt is None:
        return
    targets = ci_mk_targets()
    for job_id, job in sorted(jobs.items()):
        if ((job or {}).get("name") or job_id) in exempt:
            continue
        for index, step in enumerate((job or {}).get("steps") or []):
            run = (step or {}).get("run")
            if run is None:
                continue
            label = (step or {}).get("name") or f"step {index}"
            where = f"{name}:{job_id}:{label}"
            body = run.strip()
            single = len(body.splitlines()) == 1
            m = MAKE_RUN.match(body) if single else None
            if m is None:
                fail(
                    where,
                    "is shell in a workflow file. Every step here must be one "
                    "`make ci-*` call, so the leg can be run on a laptop and "
                    "so `make ci` means the same thing as a green pull "
                    "request. Move the body into a target in ci.mk.",
                )
                continue
            target = m.group("target")
            if not target.startswith("ci-"):
                fail(
                    where,
                    f"calls `make {target}`, not a `ci-` target. The `ci-` "
                    "prefix is what makes ci.mk readable against the job list: "
                    "one target per leg, named after it. Wrap it — "
                    f"`ci-<leg>: {target}` — and call that.",
                )
            elif target not in targets:
                fail(
                    where,
                    f"calls `make {target}`, which ci.mk does not define. Either "
                    "the target was renamed and this was not, or it lives in the "
                    "Makefile and belongs here.",
                )


def check_workflow(path: Path) -> None:
    doc = yaml.safe_load(path.read_text()) or {}
    name = path.name
    jobs = doc.get("jobs") or {}
    on = triggers(doc)

    if "permissions" not in doc:
        fail(
            name,
            "no top-level `permissions:`. Without it the job gets whatever the "
            "repository default is, which for many repos is write to everything.",
        )
    if "concurrency" not in doc:
        fail(name, "no `concurrency:` group; superseded runs will pile up.")

    check_pins(path)
    check_terminal(name, jobs)
    check_make_dispatch(name, jobs)

    if "pull_request" not in on:
        return  # not a gating workflow; the checks below are about merges

    for event in ("pull_request", "push"):
        spec = on.get(event) or {}
        if isinstance(spec, dict) and ({"paths", "paths-ignore"} & spec.keys()):
            fail(
                name,
                f"`on.{event}` has a path filter. This workflow publishes a "
                "required status context, and a filtered-out required check "
                "never starts, never reports, and blocks the PR forever. Filter "
                "in a `changes` job instead and let the gate treat `skipped` as "
                "a pass.",
            )

    gates = {
        job_id
        for job_id, job in jobs.items()
        if ((job or {}).get("name") or job_id) in REQUIRED_CONTEXTS
    }
    if not gates:
        fail(
            name,
            "is triggered on pull_request but defines no job named "
            f"{' or '.join(sorted(REQUIRED_CONTEXTS))}. Nothing here can gate a "
            "merge — either add the aggregate gate or stop running it on PRs.",
        )
        return

    for gate_id in gates:
        gate = jobs[gate_id] or {}
        if str(gate.get("if", "")).strip() != "always()":
            fail(
                f"{name}:{gate_id}",
                "the aggregate gate must be `if: always()`. Without it the gate "
                "is itself skipped the moment any leg fails, and a skipped "
                "required check is reported as neutral — the PR goes green.",
            )
        body = yaml.dump(gate)
        if "join(needs.*.result" not in body:
            fail(
                f"{name}:{gate_id}",
                "does not inspect `join(needs.*.result, ' ')`. A gate that only "
                "depends on its legs passes when they are skipped *or* failed, "
                "because a failed dependency skips the gate and `always()` then "
                "runs it with nothing checked.",
            )

    reachable = closure(jobs, gates)
    unreachable = set(jobs) - reachable

    for job_id in sorted(unreachable - UNREACHABLE_ALLOWLIST.keys()):
        fail(
            f"{name}:{job_id}",
            "runs on pull requests but is not in the `needs:` closure of any "
            f"required context ({', '.join(sorted(REQUIRED_CONTEXTS))}). It can "
            "go red and the PR will still merge. Add it to the gate's `needs:`, "
            "or add it to UNREACHABLE_ALLOWLIST in this file with a reason.",
        )
    for job_id, reason in UNREACHABLE_ALLOWLIST.items():
        if job_id not in jobs:
            fail(
                name,
                f"UNREACHABLE_ALLOWLIST names `{job_id}` ({reason}) but no such "
                "job exists. A stale exemption is how the next real one gets "
                "waved through — delete it.",
            )
        elif job_id in reachable:
            fail(
                name,
                f"UNREACHABLE_ALLOWLIST names `{job_id}` ({reason}) but it *is* "
                "reachable from the gate. Delete the entry; the exemption is a "
                "lie about how the graph works.",
            )


def main() -> int:
    files = sorted(
        p for p in WORKFLOWS.glob("*") if p.suffix in (".yml", ".yaml")
    )
    if not files:
        sys.exit(f"error: no workflows found under {WORKFLOWS}")

    for path in files:
        check_workflow(path)

    print(f"checked {len(files)} workflow(s): {', '.join(p.name for p in files)}")
    if failures:
        print(f"\n{len(failures)} problem(s):\n", file=sys.stderr)
        for f in failures:
            print(f"  - {f}\n", file=sys.stderr)
        return 1
    print(f"required context(s) {', '.join(sorted(REQUIRED_CONTEXTS))} cover every PR job.")
    for wf, terminal in sorted(TERMINAL_JOBS.items()):
        print(f"{wf}: `{terminal}` covers every publishing job.")
    for wf in sorted(MAKE_DISPATCHED):
        print(f"{wf}: every step is a `make ci-*` target in ci.mk.")
    return 0


if __name__ == "__main__":
    sys.exit(main())
