# Contributing to Mira

Mira is an OTLP-native telemetry storage engine in a single binary. Before a
change that is structural — a new dependency, a new on-disk shape, a new
mechanism — read [`docs/architecture.md`](docs/architecture.md) section 0 and section 1.
section 0 lists the mechanisms that did not survive contact with the formats, and
re-proposing one of them is the most common way to waste an afternoon.
[`CLAUDE.md`](CLAUDE.md) is the short version of everything below, written for
someone already inside the tree; this file is the same rules for someone
arriving.

The five principles are constraints, not aspirations. A PR is judged against
them: performance is the product (ingest throughput per core, resident
footprint, query p99, cost per GB — all four at once); agentic; OTLP-first, so
the on-disk layout *is* Resource-Scope-Signal; single binary, stateless, with
no coordination state; and KYAML-first, which is why there is no JSON parser in
the tree.

## Setting up

Rust **1.85** or newer — that is the `rust-version` in `Cargo.toml`, and CI
holds it. A working `cc` for `zstd-sys`, which vendors its own source. No
`protoc`: the OTLP protos are compiled by `protox` in a build script. No Node,
unless you are changing the UI.

`cargo` may not be on `PATH` in a non-login shell:

```sh
export PATH="$HOME/.cargo/bin:$PATH"
```

## The Makefile is the source of truth

Every gate CI runs, it runs by calling `make`. Run the same target locally and
you get the same verdict — there is no CI-only step to be surprised by.

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

**Before you open a PR: `make check`.** That is the checklist. Four gates run
only in CI, because each wants something a pre-push check should not assume —
`make msrv` (a second toolchain), `make scan-image` (a Docker daemon and a
Trivy database), `make e2e` and `make dist` (minutes, not seconds). Run those
by hand when you have touched what they cover. Anything else that is green
locally and red in CI is a bug in the Makefile worth reporting on its own.

## Four traps

They are the reason most first PRs go red, and none of them is your fault.

**The UI's `dist/` is committed.** `crates/mira/ui/dist` is `include_bytes!`d
into the binary, so the bundle in git is the bundle that ships. Edit a
`.svelte` file, forget to rebuild, and CI fails on `git diff --exit-code dist`
— correctly, because a fix that is not in `dist` is a fix nobody gets. `make
ui` rebuilds it and `make ui-check` is the gate; commit the rebuilt bundle
alongside the source change, in the same commit.

**Coverage is a ratchet, not a target.** The number in the gate is the coverage
that existed when that line was last edited. It may only go up. Raise it when
you raise coverage — a one-line diff a reviewer can see — and never lower it to
make a red build green. If your change genuinely cannot be covered, say so in
the PR and we will look at it together rather than at the number.

**The dependency budget is a product property.** The README states a crate
count and a stripped binary size, and `docs/architecture.md` section 11 scores binary
size as an axis. Adding a crate is allowed; adding one silently is not — `make
drift` compares those numbers against the tree that actually builds and fails
when they disagree, so a PR that moves the count also updates the README bullet
and the section 11 table in the same diff. `make deps` is the other half: `cargo deny`
for advisories and licences, `cargo machete` for a crate you declared and never
used. Prefer a pure-Rust backend — `zstd-sys` is the only C dependency in the
tree and keeping it that way is a stated property.
Writing forty lines here instead of taking a crate is explicitly authorised
when the crate costs one of the four performance axes — that is principle 1,
not a code-golf preference.

**`mira` has no lib target.** Filtering tests in the binary crate is
`cargo test -p mira --bin mira <filter>`, not `--lib`. `--lib` silently matches
nothing and looks like a pass.

## Tests

`make test` runs everything, including the in-process end-to-end suite in
`crates/mira/src/e2e.rs`, which drives the real router over OTLP/HTTP,
OTLP/gRPC, the query API, MCP and the UI with no sockets and nothing to clean
up. That is the loop to stay in: it is fast, and it is the closest thing to
the real thing that a unit test can be.

New behaviour arrives with a test in the same PR. A bug fix arrives with the
test that would have caught it.

For what a unit test cannot reach — a real socket, a real exporter, real volume
— [`docs/testing.md`](docs/testing.md) is a transcript rather than a plan: a
live binary fed by the built-in `loadgen`, then `telemetrygen`, then a stock
OpenTelemetry Collector in front of it in Docker. Run it for anything touching
the receivers, the wire formats or the ingest pipeline.

## Code

- **Comments explain why, not what.** The code says what. A comment that
  restates it is a second thing to keep in sync and it will not be.
- **A deliberate simplification gets a `ponytail:` comment** naming the ceiling
  and the upgrade path — `// ponytail: linear scan, index it if the block count
  passes ~1k`. It marks the shortcut as a decision rather than an oversight,
  and it tells the next person the condition under which to revisit it. A
  shortcut with no known ceiling is not simple, it is unfinished.
- **A non-obvious choice gets its reasoning in `docs/architecture.md`**, in the
  same diff. That file is the record of why the tree looks like this, not a
  plan for what it might become.
- **New `unsafe` needs a `// SAFETY:` comment** stating the invariant that
  makes it sound, and a test that would fail if the invariant broke. The
  zero-copy read path is asserted rather than assumed today, and it stays that
  way.

## Commits and pull requests

The subject line is imperative and says what the change *does*, not which files
it touches: "Refuse an unwritable data directory, and print startup errors
readably". Short enough that `git log --oneline` stays readable. A lowercase
area prefix is fine when the change really is confined to one — `tui:`,
`query:`, `docs:`. Conventional Commits are not used here; do not add them.

The body is for why, and for what you measured. A performance claim without a
number is a hope. Every number in the README must have been measured on the
machine that ran it, so if your change moves one, re-measure it rather than
adjusting it.

Keep a PR to one idea. A refactor and a behaviour change in one diff is two
reviews wearing a trenchcoat, and the second one never happens properly. Fill
in the PR template — the checklist is short because `make check` covers the
rest.

Open an issue before a large change. Not for process: because section 0 may already
have an answer, and finding that out after the work is the expensive order.

## Licence

Apache-2.0, Copyright 2026 TrianaLab. Contributions are accepted under the same
licence — Apache-2.0 section 5, inbound equals outbound. There is no CLA and no
sign-off to remember.

## Conduct

[`CODE_OF_CONDUCT.md`](CODE_OF_CONDUCT.md) — Contributor Covenant 2.1. It
applies here and to every surface with the project's name on it.
