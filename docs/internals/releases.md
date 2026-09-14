# Release architecture

**For:** whoever is cutting the next release, and anyone reviewing a change to
`.github/workflows/release.yml`. If you want to *install* Mira rather than
publish it, you want [Install](../install.md).

## One binary, one number

Mira publishes three coordinates and they all carry the same version string:

| Coordinate | Where |
|---|---|
| Tarballs, SBOM, `SHA256SUMS` | GitHub Release assets on the `vX.Y.Z` tag |
| Multi-arch image | `ghcr.io/trianalab/mira:X.Y.Z` (and `:latest`) |
| Three crates | `miradb`, `miradb-core`, `miradb-proto` on crates.io |

One number across all three, so there is nothing to compute: bumping is an edit
to `Cargo.toml` and a `make drift` run.

No chart is on that list, and that is a change rather than an omission. There
used to be a `charts/mira` stamped from the workspace version; it was removed
when the operator landed, because two charts is two answers to "how do I run
Mira on Kubernetes" and the one that cannot scale, cannot drain and cannot be
told a ceiling is the wrong default. The only chart Mira publishes now is the
operator's, and it is on the next table.

### And two that do not

The operator is a separate program in a separate workspace, and it publishes two
more coordinates on a **version of its own**:

| Coordinate | Where | Version from |
|---|---|---|
| Multi-arch image | `ghcr.io/trianalab/mira-operator:A.B.C` (no `:latest`) | `charts/mira-operator/Chart.yaml` |
| Helm chart | `ghcr.io/trianalab/charts/mira-operator:A.B.C` | the same file |

They are built and pushed by the same `release.yml`, on the same tag, and that is
the only thing they share with the four above. Three consequences follow, and
none of them is optional:

**The operator does not take the engine's number.** A controller at `0.3.1` and
an engine at `0.3.1` would be a claim that they move together, and they do not:
`spec.image` in a `MiraCluster` names whatever engine tag you want, including an
older one, which is the entire point of a controller that upgrades a tier. A
shared number would turn "which operator supports which engine" into a question
whose answer looks obvious and is wrong. `make bump` therefore does **not**
touch `charts/mira-operator/Chart.yaml`, and `make drift` does not check it.

**Its publishers skip rather than fail.** This is the exact inverse of the rule
in [Two irreversible facts](#two-irreversible-facts), and it is deliberate. The
engine's publishers refuse an existing coordinate because a tag that already
exists means something went wrong. The operator's already exist on almost every
release — its number only moves when *it* changes — so `operator-image` and
`operator-chart` each ask the registry first and exit with a `::notice::` if the
coordinate is there. Applying the engine's rule here would make every engine
release red.

**So `verify-release` checks them unconditionally.** A publisher that skipped and
a publisher that broke look identical from the outside, which is what makes the
paragraph above dangerous on its own. The terminal job therefore resolves both
operator coordinates by digest, cosign-verifies both against the
`release.yml@refs/tags/` identity, and asserts that `helm template` on the
published chart names the published image tag — every run, including the runs
where neither was pushed.

There is no `:latest` on the operator image. The chart pins a digestless tag it
gets from its own `appVersion`, and a floating tag on a controller that holds a
cluster's scaling logic is a silent in-place upgrade nobody asked for.

The ceiling on all of this: **the operator cannot ship a fix without an engine
release**, because there is no second tag to hang one on. The upgrade path is a
`mira-operator/vA.B.C` tag and a second `push: tags:` filter beside the existing
one, on the first day that costs somebody something.

### Why not changesets

[pacto](https://pacto.run) uses [changesets](https://github.com/changesets/changesets)
for exactly this shape — several artefacts, versions that move independently —
and it was the obvious thing to reach for here. It was not adopted, and the
reason is worth recording so the question does not get re-opened for free.

Changesets' product is the **Version Packages PR**: contributors drop a markdown
file saying "this is a patch to the operator", a bot accumulates them, and
merging its PR performs the bumps and writes the changelog. That is a real
feature and Mira is not currently using any part of it — the bump is `make bump`
and the changelog is prose a human writes, on purpose, because
[the notes are the point](#cutting-a-release).

What adopting it *today* would cost is a `package.json` at the root declaring an
npm workspace, a `.changeset/config.json` with a fixed group, and one
`package.json` per publishable unit whose only content is a version string that
`Chart.yaml` already holds — three files restating one number, plus a gate to
keep them agreeing, plus node on the release path. That is the machinery with
none of the benefit.

The trigger to revisit is a **third** independently-versioned artefact, or the
first outside contributor who has to be told by hand which version line their
change belongs to. Two artefacts and one maintainer is below the line where the
bot pays for itself; `charts/mira-operator/Chart.yaml` is the source of truth
until then, and `operator-meta` reads it with `sed`.

The crates are `miradb-*` because `mira` on crates.io is an unrelated crate
from 2024. Only the registry knows those names: the dependency keys, the `use`
paths, the `[lib] name`s and the installed binary are all still `mira`.
`Cargo.toml`'s `[workspace.dependencies]` block is where that is set up.

## Cutting a release

1. **Write the changelog section first**, under `## [Unreleased]` in
   `CHANGELOG.md`. This is not optional decoration: the release job extracts
   that section verbatim as the Release notes, and an empty one degrades to
   `--generate-notes`. Mira does not use Conventional Commits, so generated
   notes are a list of imperative prose subjects — worse than what you would
   have written. Step 2 refuses if you skip this.
2. **Bump the version.**
   ```sh
   make bump TO=X.Y.Z && make drift
   ```
   `Cargo.toml`'s `[workspace.package] version` is the source of truth and
   everything else restates it; `bump` writes all of them and promotes the
   changelog section you just wrote, `drift` checks it. The full list, and what
   guards each one, is under
   [Where the version lives](#where-the-version-lives).
3. **Open a normal PR, and merge it when it is green.** That is the release.
4. **Watch `verify-release`.** It is the terminal job and the only one whose
   success means anything to a stranger.

There is no tag step. `ci.yml`'s `tag` job runs on every push to `main`,
downstream of both required contexts, and `scripts/tag-release.sh` reads
`Cargo.toml`: if that version already has a tag it says so and exits, and if it
does not, it pushes `vX.Y.Z`. Almost every push to main releases nothing; the
one that merged a bump releases.

That used to be `git tag vX.Y.Z && git push origin vX.Y.Z`, typed by hand, and
it was a step with no decision in it — the number was chosen in step 2, reviewed
in step 3 and cross-checked by `drift` against fifteen files. A step with no
decision is a step that gets forgotten, and forgetting this one leaves a merged
release that never shipped. The bump PR is still the gate; it is just no longer
followed by a chore.

### Why the tag is dispatched and not just pushed

A tag pushed with `GITHUB_TOKEN` does **not** start a workflow. That is GitHub's
loop protection and there is no flag for it — the two events exempt from the
rule are `workflow_dispatch` and `repository_dispatch`. So `tag-release.sh`
pushes the tag and then runs `gh workflow run release.yml --ref vX.Y.Z`.

The point of dispatching *at the tag* rather than making `release.yml` trigger
on the push to `main` is the OIDC subject. `meta` branches on `GITHUB_REF_TYPE`,
so a dispatch at a tag ref is a tag run in every respect — same version, same
`publish=true`, same tag-versus-`Cargo.toml` assertion — and the certificate
identity is still `release.yml@refs/tags/vX.Y.Z`, which is what
`verify-release` pins and what every signature Mira has already published was
raised under. Triggering off the branch would move that subject to
`refs/heads/main`, and the pin is not something to change quietly.

It also means no credential. A PAT, a deploy key or a GitHub App token would all
have worked, and all three are a secret somebody has to rotate; `github.token`
with `contents: write` and `actions: write` on that one job is not.

Running `release.yml` from the Actions tab **at a branch** is still the
rehearsal: `publish=false`, `version=<crate>-dev.<sha7>`, and every push, sign
and upload step skipped. At a *tag* it is a real release, by the same
`GITHUB_REF_TYPE` branch — which is the behaviour that has always been there,
and now something relies on it.

`meta` re-checks the tag against `Cargo.toml` and fails the whole run before
anything builds if they disagree, because `mira --version` prints
`CARGO_PKG_VERSION` and nothing downstream can fix that afterwards. Nothing
automated can make them disagree any more — the tag is *derived* from
`Cargo.toml` — but the assertion stays, because a hand-pushed tag is still a
tag.

A tag with a suffix — `v0.2.0-rc.1` — is marked a GitHub prerelease, and a plain
`vX.Y.Z` is not, including while Mira is pre-1.0. That is deliberate:
`releases/latest` excludes prereleases, and that endpoint is what
`scripts/get-mira.sh` resolves when no `--version` is given, so flagging `0.x`
would turn the install one-liner off. The pre-1.0 warning lives in `CHANGELOG.md`
and `SECURITY.md` instead.

## Two irreversible facts

**A published coordinate is immutable.** Every publisher asks the registry
whether the coordinate already exists and refuses rather than overwriting it. So
a re-run of a tag that partly published is a *red run*, not a silent swap of
bytes somebody has already verified against `SHA256SUMS`. Recovery from a failed
publish is a version bump — never a retag, never a force-push of the tag.

**`:latest` moves on the image.** By the time an image push has happened,
`ghcr.io/trianalab/mira:latest` points at it whether or not the Release was ever
created. There is no unwinding that except by publishing a newer version.

Both are why `concurrency` here is deliberately **not** `cancel-in-progress`: a
cancelled release is a half-published one, and letting a superseded run finish is
strictly cheaper than reconciling that by hand.

## The job graph

Every job `needs: meta`, and every job is in `verify-release`'s `needs` closure —
`make workflows` enforces that statically, which is how a publisher that
silently skips is caught at PR time rather than discovered in a green run that
published nothing.

```
meta ──> build (×4 targets) ──┬─> package ──┐
                              └─> image ────┴─> release ──> crates ──┐
                                                                     │
meta ──> operator-meta ──> operator-build (×2) ──> operator-image ──┐│
                                                    operator-chart <┘│
                                                              └──────┴─> verify-release
```

Two chains, one terminal job. They share `meta` and nothing else — no operator
job blocks an engine job or the other way round, so a broken operator build does
not stop the engine from releasing, and `verify-release` is where that is
noticed rather than papered over.

`operator-chart` is downstream of `operator-image` rather than a sibling of it:
the chart advertises an image coordinate, and publishing a chart that points at
an image which failed to push is the one ordering mistake that produces a green
run and a broken `helm install`. That is the only reason — the two jobs have no
artefact to pass.

**`meta`** computes `version`, `publish` and `prerelease` once. Deriving them
per job is how a release ends up tagged `v0.2.0` with a binary that prints
`0.1.0`.

**`build`** is a four-way matrix: `{x86_64,aarch64}-unknown-linux-gnu` on
`ubuntu-22.04` / `ubuntu-22.04-arm`, `{x86_64,aarch64}-apple-darwin` on
`macos-15`. The Ubuntu images are pinned to 22.04 rather than `-latest` because
that is the GLIBC_2.34 floor the README promises; `make glibc-floor` asserts it
on the built binary. Each leg runs `make dist-tarball`; the x86_64 Linux leg also
runs `make dist-sbom`.

**`package`** merges the artifacts, runs `make dist-sums`, and raises one SLSA
provenance attestation over `dist/SHA256SUMS` — so every tarball and the SBOM
are subjects of it transitively, at the cost of one attestation instead of six.

**`image`** untars the two Linux binaries into `dist/linux/{amd64,arm64}/mira`
and builds with `BIN=prebuilt`, so neither stage runs a command and a
`linux/amd64,linux/arm64` build is buildx copying files the host already has —
no QEMU. It refuses an existing coordinate *before* pushing, because
`gh release create` would only refuse the duplicate after `:latest` had already
moved.

**`release`** calls `gh release create`. `gh` is preinstalled on the runner and
does exactly this, so there is no third-party release action holding a write
token.

**`crates`** runs `make publish` — `cargo publish --workspace`, which works the
order out of the dependency graph and waits for the index to serve each member
before building the next. It is last because it is the least reversible thing
the workflow does: a ghcr tag can be overwritten and a GitHub Release deleted,
but a crates.io version is consumed on upload and `cargo yank` only hides it.
It needs a `CARGO_REGISTRY_TOKEN` secret, and fails with a message naming it if
it is missing — everything else has already published by then, so the fix is to
re-run the job, not to bump.

**`operator-meta`** reads the three version fields out of
`charts/mira-operator/Chart.yaml` — `version`, `appVersion` and the tag in the
`artifacthub.io/images` annotation — and **fails if any disagrees**. The chart
installs a Deployment whose image tag defaults to `appVersion`, so a chart at
`0.2.0` carrying `appVersion: 0.1.0` ships a controller one version behind the
CRD schema it was installed with. The annotation is worse in a quieter way:
nothing renders it, so a stale one is Artifact Hub scanning the previous
image for CVEs and reporting them on this release's page. They are published as
one unit; they have to say one number.

**`operator-build`** is two native runners, `ubuntu-22.04` and
`ubuntu-22.04-arm`, pushing by digest — not one buildx pass over
`linux/amd64,linux/arm64`. `image` can do that because it copies prebuilt
binaries and never runs a command in the build; the operator's Dockerfile
*compiles*, so the same trick would emulate a 160-crate Rust build under QEMU.
A matrix job cannot write per-leg `outputs` (last writer wins), so each leg
uploads its digest as an artifact named after its arch, and `operator-image`
joins them with `docker buildx imagetools create` — which writes an index over
manifests already in the registry, uploading nothing.

A dry run still builds both architectures, with `outputs: type=cacheonly`. The
half of this that can be wrong is the compile.

**`operator-image`** and **`operator-chart`** each begin with a `free` step that
asks the registry whether the coordinate exists and **skips the rest of the job**
if it does. See [And two that do not](#and-two-that-do-not) for why that is the
opposite of every other publisher here, and why `verify-release` then has to
check both coordinates on every run.

`operator-chart` passes **no** `--version`/`--app-version` to `helm package`.
The chart it replaced took them from the command line, because the authority
there was `Cargo.toml` and `Chart.yaml` was a copy; here the numbers in
`Chart.yaml` *are* the release, and `operator-meta` has already asserted they
agree. It is also where the `:artifacthub.io` OCI artifact is pushed — the
repository-ownership proof Artifact Hub looks for — and that push is
deliberately *outside* the skip: the tag is mutable, one repository has one
proof, and re-pushing identical bytes is free.

**`verify-release`** throws away every artifact and output the run produced,
checks out nothing, re-downloads what a stranger would download, and verifies it
with the same commands [Install](../install.md) tells a stranger to run. Its
first step is the only one that does *not* authenticate, and it is there for the
trap below.

## A package's first push is private

GitHub creates a `ghcr.io` package private, and the release that publishes a
coordinate for the first time is the release that creates it. Nothing in the
workflow can change that — visibility is a setting on the package, not a field
in a manifest, and there is no API for it.

So v0.0.1 published an image and a chart that nobody could pull. `docker run
ghcr.io/trianalab/mira` and the chart install beside it — the two lines the
README hands a reader — answered `DENIED`, Artifact Hub's first tracking pass
failed with the same error, and the run was green, because every step that
touched the registry had logged in first.

The operator's first release walks into the same trap twice more, on
`mira-operator` and on `charts/mira-operator`, and neither package exists yet as
this is written.

`verify-release` now asks for an anonymous pull token before it authenticates,
for all three coordinates, and fails the release if any is refused. That failure
is not a broken release: the bytes are published and correct, and the fix is the
package's own settings page rather than a version bump. It is the only check
here whose remedy is a click.

The crates have the mirror-image problem and it is checked the same way: the
`crates` job knows the upload returned 200, which is not the same as a stranger
being able to resolve it. `verify-release` reads `index.crates.io` — the sparse
index Cargo itself resolves against, not the API — for all three names.

## What is signed, and what is not

| Artifact | Checksummed | Cosign | SLSA provenance |
|---|---|---|---|
| Release tarballs | `SHA256SUMS` | — | via `SHA256SUMS` |
| CycloneDX SBOM | `SHA256SUMS` | — | via `SHA256SUMS` |
| `SHA256SUMS` itself | — | — | yes, directly |
| Image (`mira:X.Y.Z`) | digest | yes, over the digest | yes, pushed to the registry |
| crates (`miradb*`) | registry `.crate` checksum | — | — |
| Image (`mira-operator:A.B.C`) | digest | yes, over the digest | yes, pushed to the registry |
| Chart (`charts/mira-operator:A.B.C`) | digest | yes, over the digest | — |
| `:artifacthub.io` metadata | — | — | — |

**Everything signed is signed over its digest, never over a tag.** A tag is a
mutable pointer; a signature over one says nothing about the bytes that came
back.

The image signature is written in the legacy `.sig`-tag layout
(`--new-bundle-format=false --use-signing-config=false`) rather than as an OCI
referrer. That is not a preference — Artifact Hub does not read referrers, and an
unverifiable badge on the page people land on is worse than an old-format
signature.

`verify-release` re-runs `gh attestation verify` and all three `cosign verify`
calls with the certificate identity pinned to `release.yml@refs/tags/`, so a
signature raised by any other workflow, or on any other ref, fails the release.
The operator's two are verified against the *same* identity as the engine's,
because they are raised by the same workflow on the same tag — a separate
operator tag, the upgrade path named above, would move that subject and is
therefore not a change to make quietly.

## Where the version lives

`Cargo.toml`'s `[workspace.package] version` is the source; everything below
restates it, and the right-hand column is what stops it rotting.
`charts/mira-operator/Chart.yaml` is deliberately **not** in this table — it is
its own source, for [the reasons above](#and-two-that-do-not), and `make bump`
leaving it alone is the behaviour rather than an omission. The gate on it is
`operator-meta`, at release time, not `make drift`.

| Site | Written by | Gate |
|---|---|---|
| `Cargo.toml` `[workspace.package]` | `make bump` | the source |
| `Cargo.toml` `miradb-core` / `miradb-proto` path-dep pins | `make bump` | `make drift` |
| `docs/install.md` — `--version v`, `V=`, the `spec.image` in the MiraCluster | `make bump` | `make drift` |
| `README.md` — `--version v` | `make bump` | `make drift` |
| `SECURITY.md` — the supported-versions line | `make bump` | `make drift` |
| `.github/ISSUE_TEMPLATE/bug_report.yml` — the `mira X.Y.Z` placeholder | `make bump` | `make drift` |
| `charts/mira-operator/README.md.gotmpl` — the `spec.image` in the MiraCluster | `make bump` | `make drift` |
| `CHANGELOG.md` — the heading and the link definitions | `make bump` | **none** (prose) |
| `Cargo.lock` | `cargo update --workspace` | `--locked` fails the build |
| `charts/mira-operator/README.md` | `make bump`, then `helm-docs` over it | `make helm-docs-check`, `make drift` |

`make bump TO=X.Y.Z` writes every row above and then regenerates the last two,
so step 1 is one command and `make drift` is how you check it did. The middle
column exists because the writer and the gate are deliberately the same thing:
`version_sites()` in `crates/xtask/src/drift.rs` is one table of anchored patterns,
read forwards to check and backwards to write. A gate maintained separately
from the writer drifts, and it drifts in the bad direction — the writer is what
people actually run.

Anchored patterns rather than a find-and-replace, because this page is full of
sentences that name a past release on purpose, and a bump must not rewrite one
of them. For the same reason a pattern matching *nothing* is a failure rather
than a pass: a gate for a line that has moved is a gate that is off.

`CHANGELOG.md` is the one site still ungated, and it is ungated because the
prose is the point — nothing can check that a human wrote the right notes. What
`make bump` does mechanically is promote `## [Unreleased]` to `## [X.Y.Z]`, open
a fresh empty one and move the link definitions. It **refuses** if
`## [Unreleased]` is empty, since `release.yml` publishes that section verbatim
and an empty one silently degrades to `--generate-notes`.

This existed as a forty-line stub of an idea through two releases before it was
written, and both paid for it: the 0.0.1 cut left `SECURITY.md` saying there was
no tagged release on the day there was one, and 0.0.2 hand-edited the same three
ungated sites again — missing the issue-template placeholder, which nothing had
ever checked.

## What the tag path does not re-run

`ci.yml` triggers on push-to-main, on pull requests and on a Monday cron. A tag
push triggers `release.yml` only — so no tests, no clippy and no `make drift`
run on the release path. The tag-versus-`Cargo.toml` assertion is the only thing
re-checked.

This is survivable because nothing reaches `main` un-gated, and because the
`tag` job `needs: [required, security-required]` — so the commit a tag names is
by construction the commit both gates just passed on, rather than a commit
somebody believed was green.

The residual hole is a hand-pushed tag, which is the one way to reach
`release.yml` from a commit no gate has seen. It used to be wider: the engine's
chart took its `version`/`appVersion` from `helm package` on the command line
but left the `artifacthub.io/images` annotation alone, so a stale tag shipped a
chart advertising an image that was not the one being released. That chart is
gone, and the operator's does not have the hole — its three version fields all
come from the tree rather than the command line, and `operator-meta` refuses to
run if any disagrees with the other two. `verify-release` then `helm template`s
the *published* chart and asserts the image tag it renders is the published one.

## Rehearsal, and what only the tag can run

`ci.yml`'s `release-dry-run` leg runs `make dist` and `make publish-dry` — the
real tarball, SBOM and checksum targets, then a full `cargo publish --workspace`
that packages all three crates, resolves each against the one before it out of a
temporary registry and compiles them, stopping at the upload — on every code PR.
`workflow_dispatch` **at a branch** runs the whole DAG with `publish=false`.
Both have passed.

`publish=false` skips every network-publishing step, so `cosign sign`,
`helm push`, the `Digest:` scrape off `helm push`'s stderr and the Artifact Hub
`oras push` were all executing for the first time on v0.0.1. All four worked.

The operator's four jobs are in the weakest position of anything here, and it is
worth being explicit rather than discovering it on a tag. `operator-build`
compiles both architectures on a dry run, so the Dockerfile is rehearsed.
`operator-meta` runs unconditionally, so the version assertion is rehearsed.
`operator-image` and `operator-chart` are gated on `publish`, so the `free`
check, `imagetools create`, both `cosign sign` calls and the `helm push` digest
scrape have **never executed** — and unlike the engine's equivalents, they have
no `release-dry-run` leg on pull requests either, because `ci.yml`'s rehearsal
covers `make dist` and `make publish-dry` and neither touches this tree. The
first tag after this lands is their first run. `verify-release` is what will say
so.

The `crates` job and the anonymous-pull check both ran for the first time on
v0.0.2, which is also the first release whose `make publish` actually uploaded.
Both worked. That upload is unrehearsable by construction — `publish-dry` does
everything except the one irreversible thing — so every release after it is
still trusting a step that only the tag can run. `verify-release` fails loudly
when it is wrong, and recovery is a bump to the next patch.
