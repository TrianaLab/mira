# Testing architecture

**For:** contributors deciding where a new test belongs, and reviewers deciding
whether a PR has enough of them. For *reproducing the published numbers* against
a live binary, see [End-to-end testing](e2e.md) — that page is a transcript, this
one is the map.

Mira has 386 cargo tests — 360 across levels 1–5 below, plus 26 in `xtask` that
test the gates rather than the engine — 22 UI tests, 20 chart tests, and 37 more
in the operator's [second workspace](#the-operator-in-a-workspace-of-its-own),
where levels 8 and 9 live too. Every one runs from a `make` target CI also calls;
there is no CI-only step. If `make check` is green locally, the only things that
can still turn CI red are the four gates needing what a pre-push check should not
assume — a second toolchain, a Docker daemon, a Trivy database, minutes rather
than seconds — and `make ci` runs those too, on this host. Both are in
[Contributing](../contributing.md).

## The levels

Nine, ordered by how much of the real system each holds rather than by the usual
names. "Integration test" is the industry's least agreed label, so it is absent.

| # | Level | Count | Lives in | Runs from |
|---|---|---|---|---|
| 1 | Unit, in-source | 139 core + 180 bin | `#[cfg(test)]` in the module under test | `make test` |
| 2 | Differential vs a reference model | 1 test, thousands of queries | `crates/mira-core/tests/differential.rs` | `make test` |
| 3 | In-process end-to-end | 37 | `crates/mira/src/e2e.rs` | `make test` |
| 4 | Subprocess CLI | 3 | `crates/mira/tests/cli.rs` | `make test` |
| 5 | Generator self-check | 1 binary flag | `crates/mira/examples/loadgen.rs` | `make test` |
| 6 | Browser-free UI | 22 | `crates/mira/ui/src/lib/*.test.js` | `make ui-check` |
| 7 | Chart rendering | 20 in 2 suites | `charts/mira-operator/tests/*_test.yaml` | `make helm-unittest` |
| 8 | Against a real API server | 4 | `integrations/kubernetes/tests/apiserver.rs` | `make operator-apiserver` |
| 9 | Live, on a real cluster | asserted, not counted | `integrations/kubernetes/e2e/run.sh` | `make operator-e2e` |

Levels 1–5 are one `cargo test --workspace`: the contributor's loop has to be one
command and a few seconds, or they stop staying in it.

### 1. Unit, and why they are in-source

`#[cfg(test)] mod tests` at the bottom of the file under test, for the reason the
convention exists: a test that sees private state asserts the invariant, not a
proxy for it. Sharpest example is the zero-copy read path — it walks every buffer
of every column of a decoded block and requires each pointer to fall inside the
`mmap`, which no public API can be asked.

### 2. Differential, and why there is no proptest

A fixture test proves the path it happens to walk. `differential.rs` builds a
random store, computes the answer with forty lines of `Vec::filter` sharing no
code with the engine, and asserts the two agree over a few thousand random
queries. That pins everything the model does *not* reimplement: time-range
pruning never drops a block holding a match, the Bloom sidecar never prunes one
either — the single failure mode an index is not allowed to have — the
cross-block merge is ordered, a cursor visits every row exactly once, and
`rows_matched` counts the match set rather than the page.

No `proptest`: the dependency budget is a stated product property ([architecture
section 11](../architecture.md)), shrinking is all it would add, and a failure
prints its seed — `MIRA_DIFF_SEED=<n>` replays the run exactly.

### 3. In-process end-to-end, the one that catches wiring

`e2e.rs` drives the real `Router` — the value `main` hands `axum::serve` — with
OTLP protobuf in one end and query JSON out the other. Only this test catches a
receiver wired to the wrong flusher, a block written where the reader does not
look, a query that parses but never matches, an acknowledgement returned before
the data is findable. Almost all of it reaches the router through `oneshot`, so
the one thing untested is the TCP socket.

Two tests need a real one and say so at the call site. The TUI's client is a
blocking `std::net::TcpStream` by design, so asserting it parses what the API
emits means making the API emit it over TCP. `mira proxy` reaches its replicas
over HTTP because in a deployment they are separate processes, so the proxy test
boots **two** storage nodes — own directory, own `--node` identity, each on
`127.0.0.1:0` — and drives the proxy's router through `oneshot` in front of them.
Those nodes are never stopped: `axum::serve` holds a router clone for the life of
the process, so `Node::stop`'s contract — every `Ingest` dropped — can never be
met, and the test ends with `forget_open_blocks` instead.

**Nothing in it sleeps.** A 200 on `/v1/logs` is a read-your-writes promise, so
the next query already sees the data, and a test that sleeps to make that true
has stopped asserting the promise. Under the shipped default the mechanism is the
open-block read path ([architecture section 4](../architecture.md)): the export
is a frame in the WAL and a row in a builder, and the query reaches into the
builder. `boot_sealing` is the other contract, for the few tests about the block
directory itself.

This is the level most new behaviour should land at: fast, no sockets, no
cleanup, the closest thing to the real thing a unit test can be.

### 4. Subprocess CLI, for what only a process has

`main`, `run`, `load` and `shutdown` are reachable only by exec'ing the binary: a
unit test inside the bin crate never calls its own `main`, `-h` and `-V` end the
process rather than returning a value, and a signal handler needs a process to
signal. Three tests, argv in and exit code out, with a SIGTERM in the middle. The
third is here for the same reason — `mira proxy` and `mira run` are two modes of
one binary that must refuse each other's flags — and it then puts a real proxy in
front of a real node, two processes, the one thing level 3's `oneshot` harness
cannot be. Coverage still counts them: the child inherits `LLVM_PROFILE_FILE` and
writes a profraw that gets merged.

### 5. The generator's own invariants

`cargo test` compiles examples but does not run their `#[test]`s; an example
target defaults to `test = false`. loadgen's invariants — a trace that crosses a
service boundary, histogram buckets that sum to their count, exemplars naming
traces that exist — therefore ride behind `--selftest` rather than a test module,
and `make test` invokes it as a second command. If the generator is wrong, every
number measured with it is wrong, so it is not optional.

### 6. UI, without a browser

`api.test.js` and `replay.test.js` are vitest over the two modules with logic in
them: the query-document builder and the recorded-snapshot replay the docs site
serves at `/play`. No jsdom, no component renderer, no headless Chrome — the
components are thin and a browser runner is a toolchain the build does not
otherwise need. The gate that actually protects the UI is `make ui-check`, which
rebuilds `crates/mira/ui/dist` and fails on `git diff --exit-code`: that
directory is `include_bytes!`d into the binary, so a fix that is not in `dist` is
a fix nobody gets.

### 7. The chart, rendered

`helm-unittest` over two suites, and one chart to run them against:
`charts/mira-operator`. The chart that installed a StatefulSet directly was
removed — a tier is a `MiraCluster` now — taking five suites and 36 tests with
it, which is why the count in this page's opening line went *down* in the release
that added an operator. The two that remain assert rendering decisions that are
cheap to break and invisible until something is deployed: the image tag defaults
to the chart's `appVersion`, `replicaCount` cannot be raised past one,
`rbac.namespaces` turns one `ClusterRole` into a `Role` per namespace, and the
rule list is exactly the rule list. Four more chart gates sit beside it —
`helm-lint`, `helm-template` across the permutations that change the chart's
shape, `helm-schema` (defaults admitted, six bad values refused), and
`helm-docs-check`. `make chart` is all five.

The `rbac` suite is the one worth copying, because the first version was
**vacuous**. It asserted the absence of a wildcard with `notMatchRegexRaw`, which
reads the rendered document as text — and a mutation injecting `verbs: ["*"]`
into `templates/rbac.yaml` passed all thirteen tests. A permissions test that
cannot fail on over-permission is a green check beside a `cluster-admin`. It now
ends in `matchSnapshot: path: rules`, so the assertion is the whole rule list
rather than a pattern somebody guessed, and the same mutation fails it.

Check any assertion here by breaking the thing it claims to protect.
helm-unittest 1.0.3 in particular accepts assertions it does not implement: an
assertion-level `documentIndex` is silently ignored (it has to be at test level),
and `containsDocument` requires *every* document to match rather than any. Both
fail open.

### 8. Against a real API server

`make operator-apiserver` runs `integrations/kubernetes/tests/apiserver.rs`
against whatever cluster the current kubeconfig context points at. The target
sets `MIRA_OPERATOR_APISERVER` itself; a bare `cargo test` does not, and every
test in the file returns immediately without it — so `make operator` stays a gate
a laptop with no cluster can pass.

kube-rs has no `envtest`. Go operators get a real `kube-apiserver` and `etcd`
pair downloaded and started by `controller-runtime`; no Rust crate does that, so
"a real API server" here means one you already have. `make operator-e2e` runs
this leg first, against the Kind cluster it just created, because these are the
failures a fake client cannot see:

| | |
|---|---|
| **apiextensions prunes.** | A field the CRD's schema does not describe is dropped on write with no error anywhere — a stale CRD is silent data loss. One test writes every field of the spec and compares the object it reads back whole; the fixture spells out every field rather than using `..Default::default()`, so adding a spec field breaks the build here. |
| **Server-side apply and ownership.** | The field manager, the owner reference's uid and kind, `clusterIP: None` surviving on the headless Service. |
| **Write loops.** | Two reconciles, and the operator's own `managedFields` entry must not move on the second. Explicitly *not* `resourceVersion`: on a real cluster kube-controller-manager writes the StatefulSet's `.status` within milliseconds of the create, bumping the version with no second write from the operator at all — it passed on a warm cluster and failed on the freshly created one in the Kind suite. A fake client cannot fail this either way, because nothing in it has another writer. |
| **The status subresource.** | An unsatisfiable spec has to land on `.status` rather than in a log line, and build nothing. |

### 9. Live, on a real cluster

`make operator-e2e` builds a Kind cluster, installs the chart as published, and
asserts five things end to end — including that the tier keeps serving after the
operator is uninstalled, the claim principle 4 rests on. It is the only level
with a network, a container runtime, a Kubernetes API and someone else's binary
in it, which is why it is last and its own CI leg. [End-to-end
testing](e2e.md) section 6 maps what it asserts and how to debug one; sections
2–5 and 7 there are the manual versions, with `loadgen`, `telemetrygen` and the
OpenTelemetry Demo.

It replaced a docker compose file that stood one Mira up behind one stock
collector. The three signals still make the full trip from a stock collector, but
through a tier the operator built — so the reconciler, the chart, the CRD and the
RBAC are on that path rather than beside it, and there is one end-to-end suite
rather than two asserting the OTLP surface twice.

## The operator, in a workspace of its own

`integrations/kubernetes` is a second Cargo workspace with its own `Cargo.lock`,
so `cargo test --workspace` in the root cannot reach it and `make test` does not
try. `make operator` is its whole gate — fmt, clippy, 33 tests, a coverage floor
and the CRD drift check — and `ci-operator` is its own CI leg, skipped entirely
by `scripts/ci-changes.sh` on a diff that does not touch it. The two levels that
need a cluster, 8 and 9, are deliberately *not* in it: `make operator` has to
pass on a laptop with no kubeconfig.

The separation is not about testing. kube-rs declares Rust 1.89 against the
engine's 1.85 floor, brings ~160 crates and a TLS stack through a `deny.toml`
that sets `multiple-versions = "deny"`, and the README's crate count is a
published product property. A nested workspace keeps all of that pinned to the
engine while this tree resolves whatever the Kubernetes API needs;
`integrations/kubernetes/Cargo.toml` opens with the argument.

Its 33 in-workspace tests are level 1 in shape — in-source, private state — and
almost all about **arithmetic that decides to delete a volume**. A controller's
own behaviour needs an API server, which is level 8 in cost, so every decision
lives in a pure function tested here and level 8 is reserved for the four things
only a real server can answer. `stats::decide` takes a slice of readings and
returns `Up`/`Down`/`Hold`, so "one full replica outvotes nine empty ones" and
"an unreachable replica stops every decision" are unit tests rather than a
cluster. `resources::*` build the objects, and the tests assert the fields a typo
drops silently — the owner reference, the immutable selector, the claim the drain
Job mounts. `crd::*` assert the validations that refuse a spec whose thresholds
would oscillate.

`main.rs` reaches the modules through the library rather than re-declaring them
with `mod`. A bin that redeclares a `[lib]`'s modules compiles the crate twice
and runs every test twice under two target names, which is also how you end up
reporting 36.

**`operator-crd-check` is the gate that matters most here** and it is not a test.
`make operator-crd` regenerates `charts/mira-operator/crds/miraclusters.yaml`
from the Rust types and `git diff --exit-code`s it, because the API server
*prunes* any field its stored schema does not name. A struct field added without
regenerating does not error — the value is silently dropped on the way in and the
controller reads the default. That is the one failure mode in this tree with no
symptom.

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
| `workflows` | A CI job that cannot block a merge, an unpinned action, a missing `permissions:`, a `run:` step that is not a `make ci-*` call |
| `install-script` | The published one-liner no longer parsing, linting or running |
| `operator-crd-check` | A `MiraCluster` field the shipped CRD does not name, which the API server would prune rather than reject |
| `docs` | A dead link, a dead anchor, a page outside the nav, or a route with a capital letter in it |

Listed in `make check`'s own order, which is the order they fail fastest. It also
runs `test`, `chart` and `coverage` — the levels above and the ratchet below —
plus `operator`, which is that row and the second workspace's own fmt, clippy and
tests. It leaves out the four `make ci` picks up: `msrv` refuses a construct
newer than the declared minimum Rust, `scan-image` a fixable HIGH or CRITICAL in
the release image, `dist` a Linux binary that will not start on the glibc the
README promises (through `glibc-floor`), and `operator-e2e` is level 9.

`section` has a `--selftest`, because every way that gate can break makes it
pass. `workflows` runs two things that do not subsume each other: actionlint will
not notice that a job nothing depends on cannot fail a PR, and `xtask ci` will
not notice a typo in a `${{ }}` expression.

## Coverage is a ratchet

`COVERAGE_MIN` in the Makefile is line coverage, and it is the coverage that
existed when that line was last edited. It may only go up: raise it in the same
diff that raises coverage — a one-line change a reviewer can see — and never
lower it to make a red build green. If a change genuinely cannot be covered, say
so in the PR; the number is a floor under a conversation, not the conversation.

Coverage runs take the same target-dir lock as a normal build, so give them their
own:

```sh
CARGO_TARGET_DIR=/tmp/mira-cov make coverage
make coverage-report   # per-file, worst first — what to write next
```

The ratchet is a floor and the README's badge does not show it. `make
coverage-json` writes the *measured* figure to
[miradb.dev/coverage.json](https://miradb.dev/coverage.json) as part of the
deploy that publishes this page, and the badge is shields reading that file at
render time — the coverage of the commit the site was built from, never a number
someone remembered to edit. It moves by a hundredth between runs because
`differential.rs` picks a fresh seed each time; that is what the measurement
does.

## What CI adds

Three things, all in `ci.mk`: which legs a diff needs, the tool versions everyone
has to agree on, and the grouping of gates into legs. `ci.yml` is a dispatcher
over that file — every `run:` in it is a `make ci-*` target, and `xtask ci` fails
the build if one is not — so `make ci` runs the whole pipeline on one host and a
red leg is reproducible with one command.

`ci.yml` computes a `changes` matrix first and every job is conditional on it, so
a docs-only PR does not build the workspace and an engine-only PR does not
compile 160 crates of kube-rs for the `operator` leg. `scripts/ci-changes.sh`
holds those filters and **fails open**: an unusable base ref runs every leg,
because a filter guessing wrong in that direction costs runner minutes and one
guessing wrong in the other ships the bug. Two aggregate jobs — `required` and
`security-required` — sit downstream of everything and are the contexts the
branch ruleset requires; `xtask ci` enforces that no job escapes their `needs`
closure, which catches a silently-skipped gate statically at PR time.

The four gates `make check` leaves to `make ci` are `msrv`, `scan-image`,
`operator-e2e` and `dist` (as `release-dry-run`, which runs the real tarball,
SBOM and checksum targets on every code PR). Two need tools a laptop may not have
— `ci-image` a Docker daemon and a Trivy database, `ci-operator-e2e` Docker plus
`kind`, `kubectl` and `helm` — and they say so rather than passing quietly.

## Choosing a home for a new test

Work down; stop at the first level that can fail for the reason you care about.

| | |
|---|---|
| 1 | **Can it be an assertion inside the module?** Then it is a unit test, in that file, and you are done. |
| 2 | **Is it "the engine agrees with what the answer obviously is"?** Extend the reference model in `differential.rs` rather than adding a fixture. A fixture proves one query; the model proves the class. |
| 3 | **Does it cross a layer — receiver to storage, storage to query, query to MCP?** `e2e.rs`. This is where a bug fix belongs when the bug was a wiring mistake, which most of them are. |
| 4 | **Does it need a process — argv, an exit code, a signal?** `cli.rs`, and expect it to be slower than everything above it. |
| 5 | **Does it need a socket, a container, or someone else's binary?** The Kind e2e, and consider whether what you are testing is really Mira's behaviour or the Collector's. If it is about what the API server does to an object — pruning, ownership, a write loop — it is level 8, cheaper and failing in seconds. |

A bug fix arrives with the test that would have caught it, at the level where it
would have caught it. A fix at level 3 for a bug a level 1 assertion would have
caught is a test that will not be maintained.

Changing the operator is a different tree and a different question: there is no
level 3 there, so the answer is "make the decision a pure function and test that"
— see [above](#the-operator-in-a-workspace-of-its-own).
