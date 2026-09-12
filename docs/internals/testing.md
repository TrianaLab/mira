# Testing architecture

**For:** contributors deciding where a new test belongs, and reviewers deciding
whether a PR has enough of them. For *reproducing the published numbers* against
a live binary, see [End-to-end testing](e2e.md) — that page is a transcript, this
one is the map.

Mira has 316 cargo tests, 22 UI tests and 36 chart tests, and every one of them
runs from a `make` target that CI also calls. There is no CI-only test step. If
`make check` is green on your machine, the only things left that can turn CI red
are the four gates that need something a pre-push check should not assume (a
second toolchain, a Docker daemon, a Trivy database, minutes rather than
seconds) — and those are listed in
[CONTRIBUTING](https://github.com/TrianaLab/mira/blob/main/CONTRIBUTING.md).

## The levels

Eight of them, and the ordering is by how much of the real system each one
holds, not by how they are usually named. "Integration test" is the label with
the least agreement in the industry, so it does not appear here.

| # | Level | Count | Lives in | Runs from |
|---|---|---|---|---|
| 1 | Unit, in-source | 124 core + 155 bin | `#[cfg(test)]` in the module under test | `make test` |
| 2 | Differential vs a reference model | 1 test, thousands of queries | `crates/mira-core/tests/differential.rs` | `make test` |
| 3 | In-process end-to-end | 34 | `crates/mira/src/e2e.rs` | `make test` |
| 4 | Subprocess CLI | 2 | `crates/mira/tests/cli.rs` | `make test` |
| 5 | Generator self-check | 1 binary flag | `crates/mira/examples/loadgen.rs` | `make test` |
| 6 | Browser-free UI | 22 | `crates/mira/ui/src/lib/*.test.js` | `make ui-check` |
| 7 | Chart rendering | 36 in 5 suites | `charts/mira/tests/*_test.yaml` | `make helm-unittest` |
| 8 | Live, over real sockets | asserted, not counted | `docs/e2e/compose.yaml` | `make e2e` |

Levels 1–5 are one `cargo test --workspace`. That is deliberate: the loop a
contributor stays in has to be one command and a few seconds, or it stops being
the loop they stay in.

### 1. Unit, and why they are in-source

Rust's convention, a `#[cfg(test)] mod tests` at the bottom of the file it
tests, is followed here for the reason the convention exists: a unit test that
can see private state can assert the invariant rather than a proxy for it. The
zero-copy read path is the sharpest example — the test walks every buffer of
every column of a decoded block and requires each pointer to fall inside the
`mmap`, which is not a thing a public API can be asked.

### 2. Differential, and why there is no proptest

A fixture test proves the path it happens to walk. `differential.rs` builds a
random store, computes the answer with forty lines of `Vec::filter` that share
no code with the engine, and asserts the two agree over a few thousand random
queries. What that pins down is everything the model does *not* reimplement:
that time-range pruning never drops a block holding a match, that the Bloom
sidecar never prunes one either — the single failure mode an index is not
allowed to have — that the cross-block merge is ordered, that a cursor visits
every row exactly once, and that `rows_matched` counts the match set rather
than the page.

No `proptest`. The dependency budget is a stated product property
([architecture section 11](../architecture.md)), shrinking is the only thing it
would add, and a failure here prints its seed: `MIRA_DIFF_SEED=<n>` replays the
run exactly.

### 3. In-process end-to-end, the one that catches wiring

`e2e.rs` drives the real `Router` — the same value `main` hands to
`axum::serve` — with OTLP protobuf in one end and query JSON out the other. It
is the only test in the tree that would catch a receiver wired to the wrong
flusher, a block written where the reader does not look, a query that parses but
never matches, or an acknowledgement returned before the data is findable. The
only thing it does not exercise is the TCP socket.

**Nothing in it sleeps.** A 200 on `/v1/logs` is a read-your-writes promise, so
the next query can already see the data; a test that sleeps to make that true is
a test that has stopped asserting the promise. Under the shipped default the
mechanism is the open-block read path ([architecture section
4](../architecture.md)) — the export is a frame in the WAL and a row in a
builder, and the query reaches into the builder. `boot_sealing` is the other
contract, for the handful of tests that are about the block directory itself.

This is the level most new behaviour should land at. It is fast, it needs no
sockets and no cleanup, and it is the closest thing to the real thing a unit
test can be.

### 4. Subprocess CLI, for what only a process has

`main`, `run`, `load` and `shutdown` are reachable only by exec'ing the binary:
a unit test inside the bin crate never calls its own `main`, `-h` and `-V` end
the process rather than returning a value, and a signal handler needs a process
to signal. Two tests, argv in and exit code out, with a SIGTERM in the middle.
Coverage still counts them — the child inherits `LLVM_PROFILE_FILE` and writes a
profraw that gets merged.

### 5. The generator's own invariants

`cargo test` compiles examples but does not run their `#[test]`s; an example
target defaults to `test = false`. loadgen's invariants — a trace that crosses a
service boundary, histogram buckets that sum to their count, exemplars naming
traces that exist — therefore ride behind `--selftest` rather than in a test
module, and `make test` invokes it as a second command. If the generator is
wrong, every number measured with it is wrong, so it is not optional.

### 6. UI, without a browser

`api.test.js` and `replay.test.js` are vitest over the two modules with logic in
them: the query-document builder and the recorded-snapshot replay the docs site
serves at `/play`. There is no jsdom, no component renderer and no headless
Chrome, because the components are thin and a browser runner is a toolchain the
build does not otherwise need. The gate that actually protects the UI is a
different one — `make ui-check` rebuilds `crates/mira/ui/dist` and fails on
`git diff --exit-code`, because that directory is `include_bytes!`d into the
binary and a fix that is not in `dist` is a fix nobody gets.

### 7. The chart, rendered

`helm-unittest` over five suites, one per template. It asserts the rendering
decisions that are cheap to break and invisible until something is deployed:
that the image tag defaults to the chart's `appVersion`, that the headless
Service stays on the container ports however the public Service's move, that a
config change rolls the pods, that SIGTERM gets long enough to seal the open
blocks, and that alert rules are refused on more than one replica. Four
more chart gates sit beside it — `helm-lint`, `helm-template` across the
permutations that change the chart's shape, `helm-schema` (the defaults are
admitted and a typo is refused), and `helm-docs-check`. `make chart` is all
five.

### 8. Live, over real sockets

`make e2e` stands up a stock OpenTelemetry Collector in front of a real Mira
container and asserts all three signals made the full trip. It is the only level
with a network, a container runtime and someone else's binary in it, which is
exactly why it is the last one and why it does not run on every PR — see
[End-to-end testing](e2e.md) for the manual version, with `loadgen`,
`telemetrygen` and the numbers.

## Gates that test the repository, not the code

These are the other half of `make check`, and they fail more first PRs than the
tests do. Each exists because something got through.

| Target | What it refuses |
|---|---|
| `section` | A U+00A7 section sign anywhere authored — including the PR title and body |
| `fmt-check` | Anything unformatted |
| `lint` | A clippy warning anywhere in the workspace, over all targets |
| `features` | `webhook-tls`, which nothing else compiles, failing to lint |
| `doc` | A rustdoc warning, private items included — a dead intra-doc link is a dead link |
| `reference-check` | A generated reference page (`docs/reference/`, `docs/config.md`) that the code has moved past |
| `ui-check` | A `.svelte` change whose rebuilt bundle was not committed |
| `ui-demo` | The `/play` snapshot bundle going stale the same way |
| `deps` | An advisory, licence, ban or source `cargo-deny` refuses, or a dependency nothing imports |
| `drift` | The README's crate count or binary size no longer matching the tree that builds |
| `workflows` | A CI job that cannot block a merge, an unpinned action, a missing `permissions:` |
| `install-script` | The published one-liner no longer parsing, linting or running |
| `docs` | A dead link, a dead anchor, a page outside the nav, or a route with a capital letter in it |

Listed in `make check`'s own order, which is the order they fail fastest. It
runs `test`, `chart` and `coverage` alongside these — the levels above and the
ratchet below — and leaves out the four the top of this page counts: `msrv`
refuses a construct newer than the declared minimum Rust, `scan-image` a
fixable HIGH or CRITICAL in the release image, `dist` a Linux binary that will
not start on the glibc the README promises (through `glibc-floor`), and `e2e`
is level 8.

`section` has a `--selftest`, because every way that gate can break makes it
pass. `workflows` runs two things that do not subsume each other: actionlint
will not notice that a job nothing depends on cannot fail a PR, and
`check_ci.py` will not notice a typo in a `${{ }}` expression.

## Coverage is a ratchet

`COVERAGE_MIN` in the Makefile is line coverage, and it is the coverage that
existed when that line was last edited. It may only go up. Raise it in the same
diff that raises coverage — a one-line change a reviewer can see — and never
lower it to make a red build green. If a change genuinely cannot be covered, say
so in the PR; the number is a floor under a conversation, not the conversation.

Coverage runs take the same target-dir lock as a normal build, so give them
their own:

```sh
CARGO_TARGET_DIR=/tmp/mira-cov make coverage
make coverage-report   # per-file, worst first — what to write next
```

## What CI adds

`ci.yml` computes a `changes` matrix first and every job is conditional on it,
so a docs-only PR does not build the workspace. Two aggregate jobs — `required`
and `security-required` — sit downstream of everything and are the contexts the
branch ruleset requires; `check_ci.py` enforces that no job can escape their
`needs` closure, which is how a silently-skipped gate is caught statically at
PR time rather than noticed later.

The four gates that run only in CI are `msrv`, `scan-image`, `e2e` and `dist`
(as `release-dry-run`, which runs the real tarball, SBOM and checksum targets on
every code PR). Run them by hand when you have touched what they cover.

## Choosing a home for a new test

Work down; stop at the first level that can fail for the reason you care about.

1. **Can it be an assertion inside the module?** Then it is a unit test, in
   that file, and you are done.
2. **Is it "the engine agrees with what the answer obviously is"?** Extend the
   reference model in `differential.rs` rather than adding a fixture. A fixture
   proves one query; the model proves the class.
3. **Does it cross a layer — receiver to storage, storage to query, query to
   MCP?** `e2e.rs`. This is where a bug fix belongs when the bug was a wiring
   mistake, which most of them are.
4. **Does it need a process — argv, an exit code, a signal?** `cli.rs`, and
   expect it to be slower than everything above it.
5. **Does it need a socket, a container, or someone else's binary?** `make e2e`,
   and consider whether the thing you are testing is really Mira's behaviour or
   the Collector's.

A bug fix arrives with the test that would have caught it, at the level where it
would have caught it. A fix at level 3 for a bug that a level 1 assertion would
have caught is a test that will not be maintained.
