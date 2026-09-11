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
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent

# The three workspace members show up in `cargo tree -p mira`, so the measured
# figure is three above the number the README states. CLAUDE.md says the same
# thing; if a fourth member is ever added, this constant and that sentence move
# together.
WORKSPACE_MEMBERS = 3

# docs/ARCHITECTURE.md section 11 scores binary size as one of the four axes
# and sets the target at "<= 20 MB stripped with UI + query + MCP". That is the
# contract; the 5.62 MiB below is merely where we are against it.
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
    "docs/ARCHITECTURE.md",
    "docs/MARKET.md",
    "docs/index.md",
    "crates/mira/Cargo.toml",
    "crates/mira/src/term.rs",
]
BINARY_SIZE_SITES = [
    "README.md",
    "docs/ARCHITECTURE.md",
    "docs/MARKET.md",
    "docs/index.md",
    "crates/mira/src/term.rs",
]

# The README numbers were measured on an Apple M3 Pro (docs/ARCHITECTURE.md
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

# Two percent of 5.62 MiB is ~115 KiB. A new dependency costs hundreds of KiB, so
# this catches the regression it exists to catch; a rustc point release moves
# the number by a few KiB, which it deliberately does not.
SIZE_TOLERANCE = 0.02

MIB = 1024 * 1024

# One binary, one chart, one number. The chart's README pins are generated by
# helm-docs from `chart.version`, so `make helm-docs-check` already catches those
# — what nothing else catches is Chart.yaml disagreeing with the crate it
# installs.
CHART_YAML = "charts/mira/Chart.yaml"

# Prose that pins the chart version in a copy-pasteable command. Same half-fix
# problem as the crate count: whoever bumps Chart.yaml has no reason to think
# about a docs page, and a `--version` the registry does not have is an install
# that fails for a reader who did exactly what the page said.
CHART_VERSION_SITES = ["docs/install.md"]

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
        "tree", "-p", "mira", "--edges", "normal", "--prefix", "none",
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
            f"    The count is a product property (README, docs/ARCHITECTURE.md "
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
            ["cargo", "tree", "-p", "mira", "--edges", "normal,build",
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
                "docs/ARCHITECTURE.md section 11). It also breaks the musl and "
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
            "target in docs/ARCHITECTURE.md section 11."
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


def check_chart_version(crate_version: str) -> None:
    """Chart version, appVersion and the scanned image tag all equal the crate's.

    Three separate lines in one file, which is three chances to bump two of
    them. The image tag is the expensive one to get wrong: Artifact Hub reads
    `artifacthub.io/images` to attach a security report, so a stale tag shows
    the previous release's CVEs against this release's listing.
    """
    text = (ROOT / CHART_YAML).read_text()
    found = {
        "version": re.search(r"^version:\s*(\S+)\s*$", text, re.M),
        "appVersion": re.search(r'^appVersion:\s*"?([^"\s]+)"?\s*$', text, re.M),
        "artifacthub.io/images tag": re.search(
            r"^\s*image:\s*ghcr\.io/trianalab/mira:(\S+)\s*$", text, re.M
        ),
    }
    print(f"chart:       {crate_version} in Cargo.toml")
    for field, m in found.items():
        if not m:
            fail(
                f"{CHART_YAML} has no `{field}` this script can read.\n"
                f"    It is checked against the crate version ({crate_version}); "
                "restore the line, or teach scripts/check_drift.py the new shape."
            )
        elif m.group(1) != crate_version:
            fail(
                f"{CHART_YAML} `{field}` is {m.group(1)} but the workspace is at "
                f"{crate_version}.\n"
                "    Chart version, appVersion and the image tag are the crate "
                "version — there is one binary and one chart, so a second number "
                "would only ever be a question with no answer. Bump all three, "
                "then `make helm-docs` to regenerate the README's pins."
            )

    pinned = re.compile(rf"\b{re.escape(crate_version)}\b")
    for rel in CHART_VERSION_SITES:
        if not pinned.search((ROOT / rel).read_text()):
            fail(
                f"{rel} does not mention the current chart version "
                f"({crate_version}). It pins `helm install --version` at the old "
                "one, which is an install that fails for whoever copies it."
            )



# A comment saying "section 7.3" is a link with no href: nothing resolves it,
# nothing breaks when the section is renumbered, and the reader is left looking
# for a heading that no longer exists. There are ~90 of them in the tree, which
# is too many to re-check by hand every time ARCHITECTURE.md is edited — and
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
    arch = sections_of("docs/ARCHITECTURE.md")

    dangling: dict[str, list[str]] = {}
    for path in sorted(ROOT.glob("crates/*/src/*.rs")) + sorted(ROOT.glob("docs/*.md")):
        if path.name == "ARCHITECTURE.md":
            continue
        for cite in set(SECTION_CITE.findall(path.read_text())):
            if cite.rstrip(".") in arch:
                continue
            dangling.setdefault(cite, []).append(path.name)

    for cite, where in sorted(dangling.items()):
        fail(
            f'"section {cite}" is cited in {", ".join(sorted(set(where)))} and '
            "docs/ARCHITECTURE.md has no such heading.\n"
            "    Either the section moved and the citation did not, or the "
            "citation is a typo. A cross-reference into a document is a "
            "promise about that document."
        )


def main() -> int:
    declared_crates, declared_mib = declared_from_readme()
    check_crate_count(declared_crates)
    check_c_toolchain()
    check_binary_size(declared_mib)
    check_sites(declared_crates, declared_mib)
    check_chart_version(workspace_version())
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
