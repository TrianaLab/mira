# Contributing to Mira

Mira is an OTLP-native telemetry storage engine in a single binary. Before a
structural change — a new dependency, a new on-disk shape, a new mechanism —
read [Architecture](https://miradb.dev/architecture/) section 0 and section 1.
section 0 lists the mechanisms that did not survive contact with the formats,
and re-proposing one is the most common way to waste an afternoon. `CLAUDE.md`
at the repository root is the same rules for someone already inside the tree.

> Links here are absolute `miradb.dev` URLs: this file is read both in GitHub's
> pull-request sidebar and on [the site](https://miradb.dev/contributing/), which
> serves it by symlink, and a relative path is only correct in one of them.

The five principles are constraints, not aspirations, and a PR is judged against
them: performance is the product (ingest throughput per core, resident footprint,
query p99, cost per GB — all four at once); agentic; OTLP-first, so the on-disk
layout *is* Resource-Scope-Signal; single binary, stateless, no coordination
state; KYAML-first, which is why there is no JSON parser in the tree.

## Setting up

Rust **1.85** or newer — the `rust-version` in `Cargo.toml`, and CI holds it. A
working `cc` for `zstd-sys`, which vendors its own source. No `protoc`: the OTLP
protos are compiled by `protox` in a build script. Node only for a UI change or
`make changeset`, and that one writes five lines of markdown you can write by
hand.

`cargo` may not be on `PATH` in a non-login shell:

```sh
export PATH="$HOME/.cargo/bin:$PATH"
```

## The Makefile is the source of truth

Every gate CI runs, it runs by calling `make` — the same target locally, the
same verdict, and no CI-only step to be surprised by.

```sh
make help        # every target, with what it does
make tools       # install the cargo subcommands the gates need
make fmt         # rustfmt, in place
make lint        # clippy over the workspace, warnings are errors
make test        # unit tests plus the in-process end-to-end suite
make ui          # rebuild the committed Svelte bundle
make coverage    # line coverage against the ratchet
make audit       # cargo-deny: advisories, licences, bans, sources
make deps        # audit, plus cargo-machete for crates declared and never used
make drift       # the numbers the README promises, against the tree that ships
make docs        # the docs site, --strict, so a dead link is a failure
make msrv        # compile with exactly the declared MSRV
make build       # release binary
make check       # every gate, in the order they fail fastest
```

**Before you open a PR: `make check`.** That is the checklist.

## Running the pipeline locally

`.github/workflows/ci.yml` is a dispatcher and nothing else: every `run:` step is
a `make ci-*` call into `ci.mk`, and `cargo run -p xtask -- ci` fails the build if
one is not — so a red leg reproduces with one command, and there is no shell in
YAML to debug by pushing commits and waiting six minutes.

```sh
make ci          # every leg a pull request runs, on this host
make ci-rust     # one leg, exactly as its job runs it
make ci-changes  # which legs your diff against origin/main would run
```

`make ci` is `make check` plus the four it leaves out: `ci-msrv` (a second
toolchain), `ci-image` (a Docker daemon and a Trivy database), `ci-operator-e2e`
(Docker, `kind`, `kubectl`, `helm`, and fifteen minutes building a Kubernetes
cluster) and `ci-release-dry-run` (minutes, not seconds). A laptop without those
tools gets a leg that says so rather than passing quietly.

The Makefile holds the **gates** — what "correct" means, which must mean the same
on a laptop as on a runner. `ci.mk` holds what a **runner** adds: which legs a
diff needs, the tool versions everyone agrees on, the grouping into legs. A new
gate goes in the Makefile and joins a `ci-` leg; a new leg is a target in `ci.mk`
and a job in `ci.yml` calling it, wired into `required` or `security-required`.
Green locally and red in CI is a bug in one of those two files, worth reporting.

## Four traps

They are the reason most first PRs go red, and none of them is your fault.

**The UI's `dist/` is committed.** `crates/mira/ui/dist` is `include_bytes!`d
into the binary, so the bundle in git is the bundle that ships. Edit a `.svelte`
file, forget to rebuild, and CI fails on `git diff --exit-code dist` — correctly,
because a fix that is not in `dist` is a fix nobody gets. `make ui` rebuilds it,
`make ui-check` is the gate, and the bundle goes in the same commit as the source.

**Coverage is a ratchet, not a target.** The number in the gate is the coverage
that existed when that line was last edited, and it may only go up. Raise it
when you raise coverage; never lower it to make a red build green. If your
change genuinely cannot be covered, say so in the PR rather than moving it.

**The dependency budget is a product property.** The README states a crate count
and a stripped binary size, and `docs/architecture.md` section 11 scores binary
size as an axis. Adding a crate is allowed; adding one silently is not — `make
drift` compares those numbers against the tree that builds and fails when they
disagree, so a PR that moves the count updates the README bullet and the
section 11 table in the same diff, and `make deps` is the other half. Prefer a
pure-Rust backend; `zstd-sys` is the only C dependency in the tree and keeping
it that way is a stated property. Writing forty lines instead of taking a crate
is authorised when the crate costs one of the four performance axes — that is
principle 1, not a code-golf preference.

**`mira` has no lib target.** Filtering tests in the binary crate is
`cargo test -p miradb --bin mira <filter>`, not `--lib`. `--lib` silently matches
nothing and looks like a pass.

## Tests

`make test` runs everything, including the in-process end-to-end suite in
`crates/mira/src/e2e.rs`, which drives the real router over OTLP/HTTP, OTLP/gRPC,
the query API, MCP and the UI with no sockets and nothing to clean up. That is
the loop to stay in: fast, and the closest a unit test gets to the real thing.

New behaviour arrives with a test in the same PR. A bug fix arrives with the
test that would have caught it, at the level where it would have —
[Testing architecture](https://miradb.dev/internals/testing/) maps those levels
and ends with how to pick one.

For what a unit test cannot reach — a real socket, a real exporter, real volume —
[End-to-end testing](https://miradb.dev/internals/e2e/) is a transcript: a live
binary fed by the built-in `loadgen`, then `telemetrygen`, then a stock
OpenTelemetry Collector in Docker. Run it for anything touching the receivers,
the wire formats or the ingest pipeline.

## Code

- **Comments explain why, not what.** The code says what. A comment that
  restates it is a second thing to keep in sync and it will not be.
- **A deliberate simplification gets a `ponytail:` comment** naming the ceiling
  and the upgrade path — `// ponytail: linear scan, index it if the block count
  passes ~1k`. A shortcut with no known ceiling is unfinished, not simple.
- **A non-obvious choice gets its reasoning in `docs/architecture.md`**, in the
  same diff — the record of why the tree looks like this, not a plan for what it
  might become.
- **New `unsafe` needs a `// SAFETY:` comment** stating the invariant that makes
  it sound, and a test that would fail if the invariant broke. The zero-copy
  read path is asserted rather than assumed, and it stays that way.

## Commits and pull requests

The subject line is imperative and says what the change *does*, not which files
it touches: "Refuse an unwritable data directory, and print startup errors
readably". Short enough that `git log --oneline` stays readable. A lowercase area
prefix is fine when the change is confined to one — `tui:`, `query:`, `docs:`.
Conventional Commits are not used here; do not add them.

The body is for why, and for what you measured. A performance claim without a
number is a hope, and every README number must have been measured on the machine
that ran it — if your change moves one, re-measure rather than adjust.

Keep a PR to one idea. A refactor and a behaviour change in one diff is two
reviews wearing a trenchcoat, and the second never happens properly. Fill in the
PR template — the checklist is short because `make check` covers the rest.

Open an issue before a large change. Not for process: because section 0 may already
have an answer, and finding that out after the work is the expensive order.

## Releases

There are two version lines, the engine and the operator. They move
independently, and your PR says which one it moves:

```sh
make changeset
```

That writes a file under `.changeset/`. `make ci-changeset` fails a pull request
that changes shipped code without one; docs, tests and CI-only changes are
exempt. For an engine change, add the notes under `## [Unreleased]` in
`CHANGELOG.md` too — the release publishes that section verbatim.

Nothing else is yours to do. Merging your PR does not release; a bot opens a
**`chore: version packages`** PR accumulating changesets, and merging *that* is
the release.

[Release architecture](https://miradb.dev/internals/releases/) is the procedure
and the reasoning: two version lines across five coordinates, what a tag
triggers, what is signed, and the two facts that cannot be undone once a
coordinate is published. Read it before changing `.github/workflows/release.yml`.

## Licence

Apache-2.0, Copyright 2026 TrianaLab. Contributions are accepted under the same
licence — Apache-2.0 section 5, inbound equals outbound. No CLA, no sign-off.

## Conduct

[Code of conduct](https://miradb.dev/conduct/) — Contributor Covenant 2.1. It
applies here and to every surface with the project's name on it.
