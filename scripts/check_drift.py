#!/usr/bin/env python3
"""Check that the numbers Mira promises are still true of the tree that ships.

Every number in the README is a promise, and CLAUDE.md makes two of them
load-bearing: the crate count and the stripped binary size. Both
are quoted in six files between them. The failure mode is not that someone lies
— it is that someone adds a dependency, CI tells them the count moved, they fix
the README, and the other five sites keep saying 117 forever.

So this script does three things:

  1. measures the crate count and the binary size,
  2. reads what the README declares and fails if reality has moved,
  3. fails if any other site that quotes those numbers still quotes the old one.

Step 3 is the one that matters. Steps 1 and 2 catch a lie; step 3 catches the
half-fix, which is the thing that actually happens.

The Helm chart's version is the same class of problem and so it lives here too:
`charts/mira/Chart.yaml` restates the workspace version three times over
(`version`, `appVersion`, the image tag Artifact Hub scans for CVEs), and a
release that bumps Cargo.toml alone ships a chart pointing at the previous
image. Same shape, same script.

Run it with `make drift` — that target builds the release binary first, because
you cannot check a binary's size without the binary.
"""

from __future__ import annotations

import re
import subprocess
import sys
from datetime import date
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent

# The three workspace members show up in `cargo tree -p miradb`, so the measured
# figure is three above the number the README states. CLAUDE.md says the same
# thing; if a fourth member is ever added, this constant and that sentence move
# together.
WORKSPACE_MEMBERS = 3

# docs/architecture.md section 11 scores binary size as one of the four axes
# and sets the target at "<= 20 MB stripped with UI + query + MCP". That is the
# contract; the size the README declares is merely where we are against it.
SIZE_CEILING_BYTES = 20 * 1000 * 1000

# `zstd-sys` is the only C dependency in the tree and CLAUDE.md calls that a
# stated property. A property nobody checks is a property that expires quietly:
# `flate2` and `tonic` both have C-backed feature paths one careless
# `default-features` away.
#
# Two checks, because "C dependency" has two edges and the cheap one misses.
# `-sys` naming is a convention, so it catches crates by their name; the `cc`
# reverse-dependency below catches crates by what they actually do. Something
# like `ring` or `blake3` compiles C without a `-sys` suffix and only the
# second finds it.
C_TOOLCHAIN_CRATES = ("cc", "cmake", "bindgen")
ALLOWED_CC_DEPENDENTS = {"zstd-sys"}

# `core-foundation-sys` is a `-sys` crate that compiles no C: it is extern
# declarations against a framework already present on every Mac, and it is
# macOS-only. It arrives via arrow-array -> chrono -> iana-time-zone, so it is
# not a choice Mira gets to make either. It is on this list rather than
# excluded by a rule because "it looks like a C dependency and is not" is
# exactly the kind of thing that deserves a name and a sentence.
ALLOWED_SYS_CRATES = {"zstd-sys", "core-foundation-sys"}

# Every file that quotes the crate count, and every file that quotes the binary
# size. Adding a new mention somewhere is free; *removing* the last mention from
# a listed file fails here, which is the point — you then either restore it or
# delete the line below, and either way a reviewer sees the decision.
CRATE_COUNT_SITES = [
    "README.md",
    "docs/architecture.md",
    "docs/market.md",
    "docs/index.md",
    "crates/mira/Cargo.toml",
    "crates/mira/src/term.rs",
]
BINARY_SIZE_SITES = [
    "README.md",
    "docs/architecture.md",
    "docs/market.md",
    "docs/index.md",
    "crates/mira/src/term.rs",
]

# The README numbers were measured on an Apple M3 Pro (docs/architecture.md
# section 11 says so). A GitHub Linux runner links a measurably different
# binary, so the exact-size gate only runs where the comparison means something.
# Everywhere else the ceiling still applies, and the measured size is printed so
# a jump is visible in the log even when it is not fatal.
REFERENCE_PLATFORM = "darwin"

# The crate count needs no such escape hatch, because `cargo tree --target` will
# resolve any triple's graph from any host — it only evaluates `cfg`, so the
# target's standard library need not be installed. Without it the count is
# host-dependent (`core-foundation-sys` is macOS-only, so a Linux runner counts
# 116 where this Mac counts 117) and the gate fires on the runner rather than on
# a real change. Same triple as REFERENCE_PLATFORM, so the declared number stays
# the one that was measured.
REFERENCE_TARGET = "aarch64-apple-darwin"

# Two percent is ~115 KiB at the size the binary is now. A new dependency costs
# hundreds of KiB, so this catches the regression it exists to catch; a rustc
# point release moves the number by a few KiB, which it deliberately does not.
SIZE_TOLERANCE = 0.02

MIB = 1024 * 1024

# One binary, one chart, one number. The chart's README pins are generated by
# helm-docs from `chart.version`, so `make helm-docs-check` already catches those
# — what nothing else catches is Chart.yaml disagreeing with the crate it
# installs.
CHART_YAML = "charts/mira/Chart.yaml"

# Every site that restates the workspace version, as (file, pattern) with the
# version itself as group 1. One table, read by two things: `check_version_sites`
# asserts every match equals `[workspace.package] version`, and `--bump` rewrites
# every match to a new one. That is the point of the shared table — a writer and
# a gate maintained separately drift, and the direction they drift in is the bad
# one, because the writer is what people actually run.
#
# Each pattern is anchored on the *syntax around* the version rather than on a
# bare `X.Y.Z`, so that naming some other project's version in prose — a Rust
# release, a Helm version — is not a build failure, and so that a paragraph that
# deliberately recounts a past release survives a bump. `docs/internals/
# releases.md` is full of those sentences and must never be rewritten here.
#
# Every match in a listed file must equal the current version: a file carrying a
# second, stale one is the failure mode this exists for. `v0.1.0` sat in three
# copy-pasteable blocks while the workspace was at 0.0.1, and each was a 404 on a
# stranger's first contact with Mira.
VERSION_SITES: list[tuple[str, re.Pattern[str]]] = [
    # The source itself, and the two path-dep pins beside it. The pins are the
    # classic miss: `cargo publish` requires a `version` next to every `path`,
    # and no local build ever reads it, so a stale pin is invisible right up
    # until the release job uploads a crate depending on a sibling version that
    # does not exist.
    ("Cargo.toml", re.compile(r'^version = "(\d+\.\d+\.\d+)"$', re.M)),
    (
        "Cargo.toml",
        re.compile(r'^mira-(?:core|proto) = \{.*?version = "(\d+\.\d+\.\d+)"', re.M),
    ),
    # Three lines in one file, which is three chances to bump two of them. The
    # image tag is the expensive one: Artifact Hub reads `artifacthub.io/images`
    # to attach a security report, so a stale tag shows the previous release's
    # CVEs against this release's listing.
    (CHART_YAML, re.compile(r"^version: (\d+\.\d+\.\d+)$", re.M)),
    (CHART_YAML, re.compile(r'^appVersion: "(\d+\.\d+\.\d+)"$', re.M)),
    (
        CHART_YAML,
        re.compile(r"^\s*image: ghcr\.io/trianalab/mira:(\d+\.\d+\.\d+)$", re.M),
    ),
    (
        "charts/mira/tests/statefulset_test.yaml",
        re.compile(r"ghcr\.io/trianalab/mira:(\d+\.\d+\.\d+)"),
    ),
    # The commands a reader copies rather than reads. A stale version here is a
    # 404 rather than a typo. `--version v?` covers both spellings: the
    # installer takes a tag (`v0.0.2`), helm takes a chart version (`0.0.2`).
    ("README.md", re.compile(r"--version v(\d+\.\d+\.\d+)", re.M)),
    ("docs/install.md", re.compile(r"--version v?(\d+\.\d+\.\d+)", re.M)),
    ("docs/install.md", re.compile(r"^V=(\d+\.\d+\.\d+)", re.M)),
    (
        "docs/install.md",
        re.compile(r"ghcr\.io/trianalab/charts/mira:(\d+\.\d+\.\d+)"),
    ),
    # The two that used to be ungated prose, and rotted exactly as predicted:
    # the 0.0.1 cut left SECURITY.md claiming there was no tagged release, on
    # the day after there was one.
    ("SECURITY.md", re.compile(r"^Mira is pre-1\.0 — `(\d+\.\d+\.\d+)`", re.M)),
    (
        ".github/ISSUE_TEMPLATE/bug_report.yml",
        re.compile(r"^\s*placeholder: mira (\d+\.\d+\.\d+)$", re.M),
    ),
]

# The Artifact Hub ownership proof, pushed to the chart repository under a
# reserved tag by release.yml. Artifact Hub does not report a wrong or missing
# `repositoryID` as an error: it just leaves the repository unverified, quietly,
# forever. So the two ways to get there — the file gone, or the ID still the
# placeholder someone pasted before the repository existed — are checked here
# instead.
ARTIFACTHUB_REPO_YML = "artifacthub-repo.yml"
PLACEHOLDER_REPOSITORY_ID = "00000000-0000-0000-0000-000000000000"

failures: list[str] = []
notes: list[str] = []


def fail(msg: str) -> None:
    failures.append(msg)


def cargo(*args: str) -> str:
    return subprocess.run(
        ["cargo", *args], cwd=ROOT, check=True, capture_output=True, text=True
    ).stdout


def declared_from_readme() -> tuple[int, float]:
    """Parse the README bullet that both numbers hang off.

    The README is the source of truth on purpose: CLAUDE.md calls that section a
    promise, so the promise is what everything else is checked against, rather
    than a constant in this file that nobody reads.
    """
    line = (ROOT / "README.md").read_text()
    m = re.search(r"(\d+\.\d+) MiB stripped, (\d+) crates", line)
    if not m:
        sys.exit(
            "error: README.md no longer has a '<N.NN> MiB stripped, <N> crates' "
            "bullet. That bullet is what every other site is checked against — "
            "restore it, or teach scripts/check_drift.py where the numbers live."
        )
    return int(m.group(2)), float(m.group(1))


def check_crate_count(declared: int) -> None:
    # The pipeline CLAUDE.md documents, done in Python: one entry per
    # name+version pair in mira's normal (non-dev, non-build) dependency tree.
    tree = cargo(
        "tree", "-p", "miradb", "--edges", "normal", "--prefix", "none",
        "--target", REFERENCE_TARGET,
    )
    pairs = {
        " ".join(line.split()[:2]) for line in tree.splitlines() if line.strip()
    }
    measured = len(pairs) - WORKSPACE_MEMBERS
    print(f"crates:      {measured} measured, {declared} declared")
    if measured != declared:
        direction = "grew" if measured > declared else "shrank"
        fail(
            f"the dependency tree {direction} to {measured} crates but the docs "
            f"still say {declared}.\n"
            f"    The count is a product property (README, docs/architecture.md "
            f"section 11). Update every site, not just the README:\n"
            + "".join(f"      {s}\n" for s in CRATE_COUNT_SITES)
        )

    sys_crates = {n.split()[0] for n in pairs if n.split()[0].endswith("-sys")}
    unexpected = sys_crates - ALLOWED_SYS_CRATES
    if unexpected:
        fail(
            f"new `-sys` crate(s) in the tree: {', '.join(sorted(unexpected))}.\n"
            "    zstd-sys is the only C dependency Mira has, and that is a stated "
            "property (CLAUDE.md). Keep new crates on pure-Rust backends, or, if "
            "this one compiles no C, add it to ALLOWED_SYS_CRATES with the "
            "sentence explaining why."
        )


def check_c_toolchain() -> None:
    """Whoever build-depends on `cc` is whoever compiles C. There is one."""
    for tool in C_TOOLCHAIN_CRATES:
        out = subprocess.run(
            ["cargo", "tree", "-p", "miradb", "--edges", "normal,build",
             "-i", tool, "--prefix", "depth", "--format", "{p}",
             "--target", REFERENCE_TARGET],
            cwd=ROOT, capture_output=True, text=True,
        )
        if out.returncode != 0:
            continue  # `cargo tree -i` errors out when the crate is not present
        # `--prefix depth` writes the depth with no separator, so depth 1 is
        # every line starting with "1" — the crates that pull the tool in
        # directly. Anything deeper is just their dependents.
        dependents = {
            line[1:].split()[0]
            for line in out.stdout.splitlines()
            if line.startswith("1")
        }
        unexpected = dependents - ALLOWED_CC_DEPENDENTS
        if unexpected:
            fail(
                f"{', '.join(sorted(unexpected))} build-depends on `{tool}`, so "
                "it compiles C at build time.\n"
                "    That makes it a second C dependency, and 'zstd-sys is the "
                "only one' is a stated property of the product (CLAUDE.md, "
                "docs/architecture.md section 11). It also breaks the musl and "
                "cross-compilation story in .github/workflows/release.yml."
            )


def check_binary_size(declared_mib: float) -> None:
    binary = ROOT / "target" / "release" / "mira"
    if not binary.exists():
        sys.exit("error: target/release/mira is missing. Run `make build` first.")

    measured = binary.stat().st_size
    measured_mib = measured / MIB
    print(
        f"binary:      {measured:,} B ({measured_mib:.2f} MiB), "
        f"{declared_mib:.2f} MiB declared, ceiling {SIZE_CEILING_BYTES:,} B"
    )

    if measured > SIZE_CEILING_BYTES:
        fail(
            f"the binary is {measured_mib:.2f} MiB, over the {SIZE_CEILING_BYTES / 1e6:.0f} MB "
            "target in docs/architecture.md section 11."
        )

    if sys.platform != REFERENCE_PLATFORM:
        notes.append(
            f"exact size not checked on {sys.platform}: the declared figure was "
            f"measured on {REFERENCE_PLATFORM} and a different linker moves it. "
            "The ceiling above still applied."
        )
        return

    drift = abs(measured_mib - declared_mib) / declared_mib
    if drift > SIZE_TOLERANCE:
        fail(
            f"the binary is {measured_mib:.2f} MiB but the docs say "
            f"{declared_mib:.2f} MiB ({drift:.1%} off).\n"
            "    Re-measure and update every site:\n"
            + "".join(f"      {s}\n" for s in BINARY_SIZE_SITES)
        )


def check_sites(declared_crates: int, declared_mib: float) -> None:
    crate_pat = re.compile(rf"\b{declared_crates}\b")
    size_pat = re.compile(re.escape(f"{declared_mib:.2f}"))
    for rel in CRATE_COUNT_SITES:
        text = (ROOT / rel).read_text()
        if not crate_pat.search(text):
            fail(
                f"{rel} does not mention the current crate count "
                f"({declared_crates}). It quoted the old one — half-updating the "
                "docs is the drift this check exists for."
            )
    for rel in BINARY_SIZE_SITES:
        text = (ROOT / rel).read_text()
        if not size_pat.search(text):
            fail(
                f"{rel} does not mention the current binary size "
                f"({declared_mib:.2f} MiB). See above."
            )


def workspace_version() -> str:
    """The `version` under `[workspace.package]` in the root Cargo.toml.

    Read with a regex rather than a TOML parser because the stdlib's `tomllib`
    is 3.11+ and this script is the one thing in the tree that has to run on
    whatever Python a contributor already has.
    """
    text = (ROOT / "Cargo.toml").read_text()
    section = re.search(
        r"^\[workspace\.package\]\s*$(.*?)(?=^\[)", text, re.S | re.M
    )
    m = (
        re.search(r'^version\s*=\s*"([^"]+)"', section.group(1), re.M)
        if section
        else None
    )
    if not m:
        sys.exit(
            "error: Cargo.toml has no `version` under `[workspace.package]`. "
            "That is where the whole workspace — and the Helm chart — takes its "
            "version from."
        )
    return m.group(1)


def check_version_sites(crate_version: str) -> None:
    """Every site in VERSION_SITES restates `[workspace.package] version`.

    Two ways to fail, and the second is the one that happens. A pattern that
    matches *nothing* means the line it was written for has moved or gone, so
    the gate has silently stopped gating — that is a failure here rather than a
    pass, because a check for something that is no longer there passes forever.
    A pattern that matches a *different* version is the ordinary half-bump.
    """
    print(f"chart:       {crate_version} in Cargo.toml")
    for rel, pattern in VERSION_SITES:
        text = (ROOT / rel).read_text()
        found = [(m.start(), m.group(1)) for m in pattern.finditer(text)]
        if not found:
            fail(
                f"{rel} has no line matching `{pattern.pattern}`.\n"
                "    That pattern is how the version in this file is both "
                "checked and rewritten by `make bump`, so a shape change here "
                "turns the gate off rather than tripping it. Restore the line, "
                "or teach VERSION_SITES in scripts/check_drift.py the new shape."
            )
            continue
        for offset, version in found:
            if version != crate_version:
                lineno = text.count("\n", 0, offset) + 1
                fail(
                    f"{rel}:{lineno} says {version}, but the workspace is at "
                    f"{crate_version}.\n"
                    "    One binary, one chart, one number — a second is only "
                    f"ever a question with no answer. `make bump VERSION="
                    f"{crate_version}` rewrites every site at once."
                )


def check_artifacthub_repo() -> None:
    repo_yml = ROOT / ARTIFACTHUB_REPO_YML
    if not repo_yml.exists():
        fail(
            f"{ARTIFACTHUB_REPO_YML} is gone. release.yml pushes it to "
            "`ghcr.io/trianalab/charts/mira:artifacthub.io`, and without it the "
            "chart repository loses its Verified Publisher badge at the next "
            "release — silently, because Artifact Hub reports a missing owner "
            "proof as 'unverified' rather than as an error."
        )
    elif PLACEHOLDER_REPOSITORY_ID in repo_yml.read_text():
        fail(
            f"{ARTIFACTHUB_REPO_YML} still carries the placeholder "
            f"`repositoryID: {PLACEHOLDER_REPOSITORY_ID}`. Artifact Hub matches "
            "the ID it issued against the one it finds; a mismatch is the same "
            "silent 'unverified' as no file at all. Copy the real ID from the "
            "repository's Artifact Hub control panel."
        )


# A comment saying "section 7.3" is a link with no href: nothing resolves it,
# nothing breaks when the section is renumbered, and the reader is left looking
# for a heading that no longer exists. There are ~90 of them in the tree, which
# is too many to re-check by hand every time architecture.md is edited — and
# editing it is precisely when they rot.
SECTION_CITE = re.compile(r"\bsection ([0-9]+(?:\.[0-9]+)*)\b")
SECTION_HEADING = re.compile(r"^#{2,4} (?:Annex )?([0-9A-Z][0-9.]*)\.? ", re.M)


def sections_of(rel: str) -> set[str]:
    """The section numbers `rel` declares, closed over prefixes.

    `## 7. Correlation` declares 7 and `### 7.3 The frame algebra` declares 7.3;
    citing the parent is fine wherever a child exists, so 7.3 implies 7.
    """
    out = {h.rstrip(".") for h in SECTION_HEADING.findall((ROOT / rel).read_text())}
    for h in list(out):
        parts = h.split(".")
        for i in range(1, len(parts)):
            out.add(".".join(parts[:i]))
    return out


def check_section_refs() -> None:
    arch = sections_of("docs/architecture.md")

    dangling: dict[str, list[str]] = {}
    # `docs/**` rather than `docs/*`: the contributor pages moved into
    # docs/internals/ and cite architecture sections as heavily as anything at
    # the top level, and a gate that stops covering a file the moment it is
    # filed somewhere tidier is a gate that rots by reorganisation.
    for path in sorted(ROOT.glob("crates/*/src/*.rs")) + sorted(ROOT.glob("docs/**/*.md")):
        if path.name == "architecture.md":
            continue
        for cite in set(SECTION_CITE.findall(path.read_text())):
            if cite.rstrip(".") in arch:
                continue
            dangling.setdefault(cite, []).append(path.name)

    for cite, where in sorted(dangling.items()):
        fail(
            f'"section {cite}" is cited in {", ".join(sorted(set(where)))} and '
            "docs/architecture.md has no such heading.\n"
            "    Either the section moved and the citation did not, or the "
            "citation is a typo. A cross-reference into a document is a "
            "promise about that document."
        )


# The README's coverage badge. A number baked into an image URL is the one kind
# of number nobody re-reads — it renders the same whether or not it is still
# true — so this badge holds no number at all: it is shields' `dynamic/json`
# reader pointed at a file `make coverage-json` writes into the published site,
# from the measurement the deploy itself took.
#
# Which leaves exactly one way for it to rot, and it is silent: the badge points
# at a URL and nothing publishes the file, so it renders grey "resource not
# found" forever on a page whose whole job is to be a promise. Both halves are
# checked here, together, because either alone passes while the pair is broken.
COVERAGE_BADGE_URL = "https://miradb.dev/coverage.json"
COVERAGE_BADGE = re.compile(
    r"img\.shields\.io/badge/dynamic/json\?[^)\s]*"
    r"url=https(?::|%3A)(?://|%2F%2F)miradb\.dev(?:/|%2F)coverage\.json"
)
COVERAGE_WRITER = re.compile(r"^COVERAGE_JSON := site/coverage\.json$", re.M)


def check_coverage_badge() -> None:
    if not COVERAGE_BADGE.search((ROOT / "README.md").read_text()):
        fail(
            "README.md no longer carries a live coverage badge pointing at "
            f"{COVERAGE_BADGE_URL}.\n"
            "    A hard-coded percentage is not a substitute: it is true on "
            "the day it is written and unfalsifiable afterwards. Restore the "
            "shields dynamic/json badge, or delete check_coverage_badge and "
            "`make coverage-json` together — a gate for a thing that is gone "
            "is a gate that passes forever."
        )
        return

    if not COVERAGE_WRITER.search((ROOT / "Makefile").read_text()):
        fail(
            "the README's coverage badge reads "
            f"{COVERAGE_BADGE_URL}, and the Makefile no longer declares "
            "'COVERAGE_JSON := site/coverage.json' to write it.\n"
            "    Nothing would publish the file, so the badge would render "
            "grey on every view of the README and no build would go red."
        )

    # A writer nothing calls is the same outage as no writer. `scripts/check_ci.py`
    # cannot catch this: its make-dispatch rule covers ci.yml only, so a docs.yml
    # that never runs the target is valid to it.
    docs_yml = (ROOT / ".github/workflows/docs.yml").read_text()
    if "make ci-coverage-json" not in docs_yml or not re.search(
        r"^ci-coverage-json:", (ROOT / "ci.mk").read_text(), re.M
    ):
        fail(
            "nothing publishes site/coverage.json: the deploy in "
            ".github/workflows/docs.yml must run 'make ci-coverage-json' and "
            "ci.mk must define that target.\n"
            "    It has to be the deploy, after `make site` — mkdocs empties "
            "the output directory, and ci.yml's coverage leg is conditional on "
            "a code change, so a docs-only push would produce no file at all."
        )


SEMVER = re.compile(r"^\d+\.\d+\.\d+$")
CHANGELOG = "CHANGELOG.md"


def _rewrite_group1(pattern: re.Pattern[str], text: str, new: str) -> tuple[str, int]:
    """Replace group 1 of every match with `new`, leaving the rest untouched.

    `re.sub` cannot do this: a template replaces the whole match, so putting the
    surrounding syntax back means re-spelling every pattern twice, once to find
    and once to restore. Splicing by group span keeps one pattern per site, and
    one pattern is what lets the gate and the writer share a table.
    """
    out: list[str] = []
    last = 0
    count = 0
    for m in pattern.finditer(text):
        out.append(text[last : m.start(1)])
        out.append(new)
        last = m.end(1)
        count += 1
    out.append(text[last:])
    return "".join(out), count


def bumped_changelog(old: str, new: str, today: str) -> str:
    """`## [Unreleased]` promoted to `## [new]`, a fresh one opened, links moved.

    Refuses on an empty `[Unreleased]`, which is not pedantry: release.yml
    extracts that section verbatim as the GitHub Release notes and falls back to
    `--generate-notes` when it comes out empty. Mira does not use Conventional
    Commits, so the fallback is a list of imperative prose subjects — strictly
    worse than the section nobody wrote. Better to refuse than to publish it.
    """
    text = (ROOT / CHANGELOG).read_text()

    body = re.search(r"^## \[Unreleased\]\n(.*?)(?=^## \[)", text, re.S | re.M)
    if not body:
        sys.exit(
            f"error: {CHANGELOG} has no `## [Unreleased]` section followed by a "
            "released one. That is the section this promotes; see "
            "docs/internals/releases.md."
        )
    if not body.group(1).strip():
        sys.exit(
            f"error: {CHANGELOG}'s `## [Unreleased]` section is empty. Write the "
            f"{new} notes into it first — release.yml publishes that section "
            "verbatim as the Release notes, and an empty one degrades to "
            "generated notes, which for this tree means a list of imperative "
            "commit subjects."
        )

    text = text.replace(
        "## [Unreleased]\n", f"## [Unreleased]\n\n## [{new}] - {today}\n", 1
    )
    text, n = _rewrite_group1(
        re.compile(r"^\[Unreleased\]: \S+/compare/v(\d+\.\d+\.\d+)\.\.\.HEAD$", re.M),
        text,
        new,
    )
    if n != 1:
        sys.exit(
            f"error: {CHANGELOG} has no `[Unreleased]: …/compare/vX.Y.Z...HEAD` "
            "link definition to move. Restore it, or drop the link definitions "
            "and this block together."
        )
    return re.sub(
        r"^(\[Unreleased\]: (\S+)/compare/\S+$)",
        rf"\1\n[{new}]: \g<2>/compare/v{old}...v{new}",
        text,
        count=1,
        flags=re.M,
    )


def bump(new: str) -> int:
    """Write `new` to every site in VERSION_SITES, and promote the changelog.

    Every file is rewritten in memory and validated before *any* of them is
    written, because a refusal half way through is the worst outcome available:
    `make drift` then reports the sites it did reach as the wrong ones, and
    whoever is mid-release has to work out by hand which half happened. The
    first version of this wrote as it went and tripped over exactly that on its
    own empty-changelog guard.

    Deliberately does *not* touch Cargo.lock or charts/mira/README.md: both are
    generated, and `make bump` regenerates them straight after. Nor does it
    touch docs/internals/releases.md, which recounts past releases by number on
    purpose — the whole reason the sites are a table of anchored patterns rather
    than a find-and-replace.
    """
    if not SEMVER.match(new):
        sys.exit(f"error: {new!r} is not an X.Y.Z version.")
    old = workspace_version()
    if old == new:
        sys.exit(f"error: the workspace is already at {new}. Nothing to bump.")

    pending: dict[str, tuple[str, int]] = {}
    for rel, pattern in VERSION_SITES:
        text = pending[rel][0] if rel in pending else (ROOT / rel).read_text()
        text, n = _rewrite_group1(pattern, text, new)
        if not n:
            sys.exit(
                f"error: {rel} has no line matching `{pattern.pattern}`, so this "
                "bump would leave it behind. Nothing has been written. Fix the "
                "file, or teach VERSION_SITES in scripts/check_drift.py the new "
                "shape."
            )
        pending[rel] = (text, pending.get(rel, ("", 0))[1] + n)
    changelog = bumped_changelog(old, new, date.today().isoformat())

    print(f"bump: {old} -> {new}")
    for rel, (text, n) in pending.items():
        (ROOT / rel).write_text(text)
        print(f"  {rel}: {n} site(s)")
    (ROOT / CHANGELOG).write_text(changelog)
    print(f"  {CHANGELOG}: [Unreleased] promoted, link definitions moved")
    print(
        "\nCargo.lock and charts/mira/README.md are generated — `make bump` "
        "regenerates them.\nThen `make drift` to verify, and open a PR: "
        "docs/internals/releases.md step 3."
    )
    return 0


def main() -> int:
    if len(sys.argv) > 1:
        if sys.argv[1] != "--bump" or len(sys.argv) != 3:
            sys.exit("usage: check_drift.py [--bump X.Y.Z]")
        return bump(sys.argv[2])

    declared_crates, declared_mib = declared_from_readme()
    check_crate_count(declared_crates)
    check_c_toolchain()
    check_binary_size(declared_mib)
    check_sites(declared_crates, declared_mib)
    check_version_sites(workspace_version())
    check_artifacthub_repo()
    check_coverage_badge()
    check_section_refs()

    for note in notes:
        print(f"note: {note}")
    if failures:
        print(f"\n{len(failures)} drift check(s) failed:\n", file=sys.stderr)
        for f in failures:
            print(f"  - {f}", file=sys.stderr)
        return 1
    print("no drift.")
    return 0


if __name__ == "__main__":
    sys.exit(main())
