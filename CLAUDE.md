# Working on Mira

Mira is an OTLP-native telemetry storage engine in a single binary: OTLP in,
immutable Arrow IPC blocks out, queried straight from `mmap`. Read
[docs/architecture.md](docs/architecture.md) section 0 and section 1 before changing anything
structural — section 0 lists the mechanisms from the original brief that do not survive
contact with the formats, and re-proposing one of them is the most common way to
waste a session.

## The five principles

They are constraints, not aspirations, and each has a mechanism (section 1):

1. **Performance is the product** — four axes at once: ingest throughput per
   core, resident footprint, query p99, cost per GB. Writing a custom library is
   explicitly authorised when a crate costs one of them.
2. **Agentic** in all four readings — LLM-queryable surface, telemetry *for* AI
   workloads, agent-operated, self-tuning.
3. **OTLP-first** — the storage layout *is* Resource-Scope-Signal. If a change
   makes the on-disk shape diverge from the spec, it is the wrong change.
4. **Single binary, no operational overhead, stateless** — stateless means *no
   coordination state*. No Raft, no membership, no external metadata store. The
   block directory is the manifest.
5. **KYAML-first** — config, query documents and API bodies. JSON is a subset,
   so JSON clients work for free; there is no JSON parser in the tree.

## Commands

`cargo` is not on `PATH` in a non-login shell here:

```sh
export PATH="$HOME/.cargo/bin:$PATH"
```

```sh
cargo test --workspace                                   # unit + in-process e2e
cargo test -p miradb --bin mira <filter>                 # NOT --lib; miradb has no lib target
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all
CARGO_TARGET_DIR=/tmp/mira-cov cargo llvm-cov --workspace --summary-only
```

Use a separate `CARGO_TARGET_DIR` for coverage — it takes the same target-dir
lock as a normal build, so it will block anything running in parallel.

The package names are `miradb`, `miradb-core` and `miradb-proto`, because
`mira` on crates.io is an unrelated crate from 2024. Nothing else moved: the
binary is `mira`, the dependency keys are `mira-core`/`mira-proto`, `[lib] name`
keeps the `use mira_core::…` spelling, and no source file mentions the registry.
Only `-p` on a cargo command wants the published name.

CI (`.github/workflows/ci.yml`) runs fmt, clippy, tests, the UI's `npm test` and
build — `git diff --exit-code dist`, so a `.svelte` change that is not rebuilt is
a red build — and a **coverage ratchet**. The ratchet is the coverage that
existed when the line was last edited; raise it when you raise coverage, never
lower it.

`ci.yml` is a dispatcher: every `run:` in it is a `make ci-*` target in `ci.mk`,
and `scripts/check_ci.py` fails the build if one is not — so never write shell
into that file. `make ci` runs every leg here, `make ci-<leg>` runs one, and
`make ci-changes` says which legs a diff needs. Gates go in the `Makefile`; only
leg grouping, the path filter and pinned runner tool versions go in `ci.mk`.

The contributor-facing internals live under `docs/internals/`:
[testing.md](docs/internals/testing.md) is the map of the eight test levels and
which one a new test belongs at, [e2e.md](docs/internals/e2e.md) is end-to-end
testing against a live instance with synthetic data, and
[releases.md](docs/internals/releases.md) is how a release is cut and what is
signed. Those three plus `architecture.md` are the site's "Project internals"
tab.

## The dependency budget is a product property

The README states the crate count and binary size, and section 11 scores binary size as
an axis. Before and after adding any dependency:

```sh
cargo tree -p miradb --edges normal --prefix none --target aarch64-apple-darwin \
  | awk '{print $1" "$2}' | sort -u | wc -l
ls -l target/release/mira
```

`--target` is not optional: the graph is host-dependent (`core-foundation-sys`
is macOS-only), so without it a Linux runner counts one crate fewer than this
Mac and the drift gate fires on the runner rather than on a real change.
`scripts/check_drift.py` pins the same triple.

That count includes the three workspace members, so it is three above the number
the README states. If it moves, update the README bullet, `docs/architecture.md`
section 11's table, and the "against a tree of N" comments. `zstd-sys` is the only C
dependency and that is a stated property — keep new crates on pure-Rust
backends.

## Documentation discipline

- **The README** is a promise. Every bullet must be true of the committed code,
  and every number must have been measured on this machine. Anything that is not
  yet true belongs in `docs/architecture.md` section 0.1, summarised under the
  README's "Scope".
- **docs/architecture.md** is the reasoning, not a plan. When you make a
  non-obvious choice, the section explaining *why* is part of the diff.
- Comments explain **why**, not what. A deliberate simplification with a known
  ceiling gets a `ponytail:` comment naming the ceiling and the upgrade path.

## Driving the TUI headlessly

`mira mira` (alias: `mira tui`) needs a pty, and stdin EOF does **not** close it.
`q` quits from the list and backs out one mode anywhere else, so the key string
needs one `q` per pane it opened or the process hangs forever — `2t\r` opens the
waterfall and then a span detail, so it takes `qqq`:

```sh
printf ']q' | script -q /dev/null ./target/release/mira mira --data-dir ./data 2>&1 \
  | tr -d '\r' | sed -e 's/\x1b\[[0-9;?]*[a-zA-Z]//g'
```

Force a size with `sh -c 'stty rows 50 cols 200; ...'`. The layout unit tests
pass even when content is being *lost*, because `Row` clips at `max` rather than
overflowing — so a layout change is not verified until you have looked at it.

## Pushing

`gh` is set to `edu-diaz` permanently — it is the only account that can see
this repo, and there is no switch-back dance any more. If a `gh` call 404s on a
repository that exists, check `gh auth status` before assuming anything else.

`main` is protected by a ruleset with an empty `bypass_actors`, so every change
lands through a PR — including a version bump. See
[docs/internals/releases.md](docs/internals/releases.md) for what a tag then
triggers.
