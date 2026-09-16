# Testing architecture

**For:** contributors deciding where a new test belongs. For *reproducing the
published numbers*, see [End-to-end testing](e2e.md).

Mira has 431 cargo tests — 371 across levels 1–5, plus 60 in `xtask` that test
the gates rather than the engine — 22 UI tests, 26 chart tests, and 67 more in
the operator's [second workspace](#the-operator-in-a-workspace-of-its-own), where
levels 8 and 9 live too. Every one runs from a `make` target CI also calls, and
both targets are in [Contributing](../contributing.md).

## The levels

Nine, ordered by how much of the real system each holds. "Integration test" is
the industry's least agreed label, so it is absent.

| # | Level | Count | Lives in | Runs from |
| --- | --- | --- | --- | --- |
| 1 | Unit, in-source | 146 core + 184 bin | `#[cfg(test)]` in the module under test | `make test` |
| 2 | Differential vs a reference model | 1 test, thousands of queries | `crates/mira-core/tests/differential.rs` | `make test` |
| 3 | In-process end-to-end | 37 | `crates/mira/src/e2e.rs` | `make test` |
| 4 | Subprocess CLI | 3 | `crates/mira/tests/cli.rs` | `make test` |
| 5 | Generator self-check | 1 binary flag | `crates/mira/examples/loadgen.rs` | `make test` |
| 6 | Browser-free UI | 22 | `crates/mira/ui/src/lib/*.test.js` | `make ui-check` |
| 7 | Chart rendering | 26 in 2 suites | `charts/mira-operator/tests/*_test.yaml` | `make helm-unittest` |
| 8 | Against a real API server | 5 | `integrations/kubernetes/tests/apiserver.rs` | `make operator-apiserver` |
| 9 | Live, on a real cluster | asserted, not counted | `integrations/kubernetes/e2e/run.sh` | `make operator-e2e` |

Levels 1–5 are one `cargo test --workspace`.

### 1. Unit, and why they are in-source

`#[cfg(test)] mod tests` at the bottom of the file under test: a test that sees
private state asserts the invariant, not a proxy for it. The zero-copy read path
requires every pointer of a decoded block to fall inside the `mmap`, which no
public API can be asked.

### 2. Differential, and why there is no proptest

`differential.rs` builds a random store, computes the answer with forty lines of
`Vec::filter` sharing no code with the engine, and asserts the two agree over a
few thousand random queries. That pins what the model does *not* reimplement:
neither time-range pruning nor the Bloom sidecar ever drops a block holding a
match, the cross-block merge is ordered, a cursor visits every row once, and
`rows_matched` counts the match set rather than the page.

No `proptest`: the dependency budget is a stated product property ([architecture
section 11](../architecture/performance.md)), and `MIRA_DIFF_SEED=<n>` replays a failure
exactly:

```sh
MIRA_DIFF_SEED=12858170866899772564 cargo test -p miradb-core --test differential
```

### 3. In-process end-to-end, the one that catches wiring

`e2e.rs` drives the real `Router`, the value `main` hands `axum::serve`, with
OTLP protobuf in one end and query JSON out the other. Only this test catches a
receiver wired to the wrong flusher, or an acknowledgement returned before the
data is findable. Almost all of it reaches the router through `oneshot`; the two
tests that need a real socket — the TUI's blocking `std::net::TcpStream`, and
`mira proxy` in front of two storage nodes — end with `forget_open_blocks`,
because `axum::serve` holds a router clone for the life of the process.

**Nothing in it sleeps.** A 200 on `/v1/logs` is a read-your-writes promise, so
the next query already sees the data through the open-block read path
([architecture section 4](../architecture/ingest.md)). This is the level most
new behaviour should land at.

### 4. Subprocess CLI, for what only a process has

`main`, `run` and `shutdown` are reachable only by exec'ing the binary: a
unit test in the bin crate never calls its own `main`, `-h` and `-V` end the
process, and a signal handler needs a process to signal. Three tests, argv in and
exit code out, with a SIGTERM in the middle; the third puts a real proxy in front
of a real node.

### 5. The generator's own invariants

`cargo test` compiles examples but does not run their `#[test]`s; an example
target defaults to `test = false`. loadgen's invariants — a trace that crosses a
service boundary, histogram buckets that sum to their count, exemplars naming
traces that exist — therefore ride behind `--selftest`, which `make test` invokes
as a second command.

### 6. UI, without a browser

`api.test.js` and `replay.test.js` are `node --test` over the two modules with logic in
them: the query-document builder and the recorded-snapshot replay the docs site
serves at `/play`. `make ui-check` rebuilds `crates/mira/ui/dist` and fails on
`git diff --exit-code`, because that directory is `include_bytes!`d into the
binary.

### 7. The chart, rendered

`helm-unittest` over two suites, against `charts/mira-operator`. They assert
rendering decisions invisible until something is deployed: the image tag defaults
to the chart's `appVersion`, the operator learns its own pod name from the
downward API, `rbac.namespaces` turns one `ClusterRole` into a `Role` per
namespace, and the rule list is exactly the rule list. `helm-lint`, `helm-template`, `helm-schema`
and `helm-docs-check` sit beside it; `make chart` is all five.

The `rbac` suite's first version was **vacuous**: `notMatchRegexRaw` reads the
rendered document as text, and a mutation injecting `verbs: ["*"]` into
`templates/rbac.yaml` passed all thirteen tests. It now ends in `matchSnapshot:
path: rules`, and the same mutation fails it.

### 8. Against a real API server

`make operator-apiserver` runs `integrations/kubernetes/tests/apiserver.rs`
against whatever cluster the current kubeconfig context points at. The target sets
`MIRA_OPERATOR_APISERVER` itself and a bare `cargo test` does not, so every test
in the file returns immediately and `make operator` stays a gate a laptop with no
cluster can pass. `make operator-e2e` runs this leg first, against the Kind
cluster it just created, because a fake client cannot see:

| | |
| --- | --- |
| **apiextensions prunes.** | A field the CRD's schema does not describe is dropped on write with no error anywhere — a stale CRD is silent data loss. One test writes every field of the spec and compares the object it reads back whole; the fixture spells out every field rather than using `..Default::default()`, so adding a spec field breaks the build here. |
| **Server-side apply and ownership.** | The field manager, the owner reference's uid and kind, `clusterIP: None` surviving on the headless Service. |
| **Write loops.** | Two reconciles, and the operator's own `managedFields` entry must not move on the second. Explicitly *not* `resourceVersion`: on a real cluster kube-controller-manager writes the StatefulSet's `.status` within milliseconds of the create, bumping the version with no second write from the operator at all — it passed on a warm cluster and failed on the freshly created one in the Kind suite. A fake client cannot fail this either way, because nothing in it has another writer. |
| **The status subresource.** | An unsatisfiable spec has to land on `.status` rather than in a log line, and build nothing. |

### 9. Live, on a real cluster

`make operator-e2e` builds a Kind cluster, installs the chart as published, and
asserts five things end to end — including that the tier keeps serving after the
operator is uninstalled, the claim principle 4 rests on.
[End-to-end testing](e2e.md) section 5 maps what it asserts and how to debug
one.

## The operator, in a workspace of its own

`integrations/kubernetes` is a second Cargo workspace with its own `Cargo.lock`,
so `cargo test --workspace` in the root cannot reach it. `make operator` is its
whole gate — fmt, clippy, 62 unit tests, a coverage floor and the CRD drift
check — and has to pass on a laptop with no kubeconfig, so levels 8 and 9 are
deliberately not in it.

The separation is not about testing: kube-rs declares Rust 1.89 against the
engine's 1.85 floor and brings ~160 crates and a TLS stack, against a README
crate count that is a published product property.

Its 62 unit tests are level 1 in shape and almost all about **arithmetic that
decides to delete a volume**: `stats::decide` returns `Up`/`Down`/`Hold` from a slice of
readings, `resources::*` assert the fields a typo drops silently, `crd::*` refuse
a spec whose thresholds would oscillate.

**`operator-crd-check` is the gate that matters most here** and it is not a test.
`make operator-crd` regenerates `charts/mira-operator/crds/miraclusters.yaml`
from the Rust types and `git diff --exit-code`s it, because the API server
*prunes* any field its stored schema does not name.

## Gates that test the repository, not the code

These are the other half of `make check`.

| Target | What it refuses |
| --- | --- |
| `section` | A U+00A7 section sign anywhere authored — including the PR title and body |
| `fmt-check` | Anything unformatted |
| `lint` | A clippy warning anywhere in the workspace, over all targets |
| `features` | `webhook-tls`, which nothing else compiles, failing to lint |
| `doc` | A rustdoc warning, private items included — a dead intra-doc link is a dead link |
| `reference-check` | A generated reference page (`docs/reference/`, `docs/config.md`) that the code has moved past |
| `market-check` | The claim tally on `docs/market.md` no longer being what its own tables add up to |
| `measurements-check` | A published performance number disagreeing with `measurements.kyaml`, the registry that owns it |
| `docs-check` | Markup markdownlint refuses, a word Vale refuses, a page past a structural limit, or a cross-reference outside `docs/` resolving to nothing — `make docs` covers the ones inside it |
| `ui-check` | A `.svelte` change whose rebuilt bundle was not committed |
| `ui-demo` | The `/play` snapshot bundle going stale the same way |
| `deps` | An advisory, licence, ban or source `cargo-deny` refuses, or a dependency nothing imports |
| `drift` | The README's crate count or binary size no longer matching the tree that builds |
| `workflows` | A CI job that cannot block a merge, an unpinned action, a missing `permissions:`, a `run:` step that is not a `make ci-*` call |
| `install-script` | The published one-liner no longer parsing, linting or running |
| `operator-crd-check` | A `MiraCluster` field the shipped CRD does not name, which the API server would prune rather than reject |
| `docs` | A dead link, a dead anchor, a page outside the nav, a route with a capital letter in it, or an absolute `miradb.dev` URL — anywhere in the tree, including Rust doc comments — that the site just built does not answer |

Listed in `make check`'s own order. It also runs `test`, `chart`, `coverage` and
`operator`, and leaves `msrv`, `scan-image`, `dist`, `publish-dry` and `operator-e2e` to
`make ci`. `section` has a `--selftest`, because every way that gate can break makes it
pass.

## Coverage is a ratchet

`COVERAGE_MIN` in the Makefile is line coverage, and it is the coverage that
existed when that line was last edited. It may only go up: raise it in the same
diff that raises coverage, and never lower it to make a red build green.

Coverage runs take the same target-dir lock as a normal build, so give them
their own:

```sh
CARGO_TARGET_DIR=/tmp/mira-cov make coverage
make coverage-report   # per-file, worst first — what to write next
```

`make coverage-json` writes the measured figure to
[miradb.dev/coverage.json](https://miradb.dev/coverage.json), which the README's
badge reads.

## What CI adds

Three things, all in `ci.mk`: which legs a diff needs, the tool versions to agree
on, and the grouping of gates into legs. `ci.yml` dispatches over that file, so
`make ci` runs the whole pipeline on one host.

Every job is conditional on a `changes` matrix, so a docs-only PR does not build
the workspace. `scripts/ci-changes.sh` holds those filters and **fails open**: an
unusable base ref runs every leg.

## Choosing a home for a new test

Work down; stop at the first level that can fail for the reason you care about.

| | |
| --- | --- |
| 1 | **Can it be an assertion inside the module?** Then it is a unit test, in that file, and you are done. |
| 2 | **Is it "the engine agrees with an answer computed a simpler way"?** Extend the reference model in `differential.rs` rather than adding a fixture. A fixture proves one query; the model proves the class. |
| 3 | **Does it cross a layer — receiver to storage, storage to query, query to MCP?** `e2e.rs`. This is where a bug fix belongs when the bug was a wiring mistake, which most of them are. |
| 4 | **Does it need a process — argv, an exit code, a signal?** `cli.rs`, and expect it to be slower than everything above it. |
| 5 | **Does it need a socket, a container, or someone else's binary?** The Kind e2e, and consider whether what you are testing is Mira's behaviour or the Collector's. If it is about what the API server does to an object — pruning, ownership, a write loop — it is level 8, cheaper and failing in seconds. |

A bug fix arrives with the test that would have caught it, at the level where it
would have caught it. Changing the operator is a
different tree: there is no level 3 there, so the answer is "make the decision a
pure function and test that" — or, where the thing under test is a client, a
`std::net::TcpListener` on a thread.
