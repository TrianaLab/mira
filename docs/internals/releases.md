# Release architecture

**For:** whoever is cutting the next release, and anyone reviewing a change to
`.github/workflows/release.yml`. If you want to *install* Mira rather than
publish it, you want [Install](../install.md).

## One binary, one chart, one number

Mira publishes four coordinates and they all carry the same version string:

| Coordinate | Where |
|---|---|
| Tarballs, SBOM, `SHA256SUMS` | GitHub Release assets on the `vX.Y.Z` tag |
| Multi-arch image | `ghcr.io/trianalab/mira:X.Y.Z` (and `:latest`) |
| Helm chart | `ghcr.io/trianalab/charts/mira:X.Y.Z` |
| Three crates | `miradb`, `miradb-core`, `miradb-proto` on crates.io |

One number across all four, so there is nothing to compute: bumping is an edit
to `Cargo.toml` and a `make drift` run.

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
3. **Open a normal PR.** The point is to make CI run `drift`, `chart` and
   `docs` against the bumped tree before a tag can be cut from it.
4. **Tag the merged commit.**
   ```sh
   git tag vX.Y.Z && git push origin vX.Y.Z
   ```
   `meta` re-checks the tag against `Cargo.toml` and fails the whole run before
   anything builds if they disagree, because `mira --version` prints
   `CARGO_PKG_VERSION` and nothing downstream can fix that afterwards.
5. **Watch `verify-release`.** It is the terminal job and the only one whose
   success means anything to a stranger.

A tag with a suffix — `v0.2.0-rc.1` — is marked a GitHub prerelease, and a plain
`vX.Y.Z` is not, including while Mira is pre-1.0. That is deliberate:
`releases/latest` excludes prereleases, and that endpoint is what
`scripts/get-mira.sh` resolves when no `--version` is given, so flagging `0.x`
would turn the install one-liner off. The pre-1.0 warning lives in `CHANGELOG.md`
and `SECURITY.md` instead.

To rehearse without publishing, run the workflow from the Actions tab:
`workflow_dispatch` sets `publish=false` and `version=<crate>-dev.<sha7>`, and
every push, sign and upload step is skipped.

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
`scripts/check_ci.py` enforces that statically, which is how a publisher that
silently skips is caught at PR time rather than discovered in a green run that
published nothing.

```
meta ──> build (×4 targets) ──┬─> package ──────────┐
                              └─> image ──> chart ──┴─> release ──> crates ──> verify-release
```

`chart` is downstream of `image`, not a sibling of it: the chart advertises an
image coordinate, and publishing a chart that points at an image which failed to
push is the one ordering mistake that produces a green run and a broken
`helm install`.

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

**`chart`** passes `--version/--app-version "$VERSION"` to `helm package`, which
overrides `Chart.yaml` without editing the tree. It also pushes a
`:artifacthub.io` OCI artifact — the repository-ownership proof Artifact Hub
looks for.

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
ghcr.io/trianalab/mira` and `helm install oci://ghcr.io/trianalab/charts/mira` —
the two lines the README hands a reader — answered `DENIED`, Artifact Hub's
first tracking pass failed with the same error, and the run was green, because
every step that touched the registry had logged in first.

`verify-release` now asks for an anonymous pull token before it authenticates,
for both coordinates, and fails the release if either is refused. That failure
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
| Chart (`charts/mira:X.Y.Z`) | digest | yes, over the digest | — |
| `:artifacthub.io` metadata | — | — | — |
| crates (`miradb*`) | registry `.crate` checksum | — | — |

**Everything signed is signed over its digest, never over a tag.** A tag is a
mutable pointer; a signature over one says nothing about the bytes that came
back.

The image signature is written in the legacy `.sig`-tag layout
(`--new-bundle-format=false --use-signing-config=false`) rather than as an OCI
referrer. That is not a preference — Artifact Hub does not read referrers, and an
unverifiable badge on the page people land on is worse than an old-format
signature.

`verify-release` re-runs `gh attestation verify` and both `cosign verify` calls
with the certificate identity pinned to `release.yml@refs/tags/`, so a signature
raised by any other workflow, or on any other ref, fails the release.

## Where the version lives

`Cargo.toml`'s `[workspace.package] version` is the source; everything below
restates it, and the right-hand column is what stops it rotting.

| Site | Written by | Gate |
|---|---|---|
| `Cargo.toml` `[workspace.package]` | `make bump` | the source |
| `Cargo.toml` `miradb-core` / `miradb-proto` path-dep pins | `make bump` | `make drift` |
| `charts/mira/Chart.yaml` — `version`, `appVersion`, the scanned image tag | `make bump` | `make drift` |
| `charts/mira/tests/statefulset_test.yaml` | `make bump` | `make drift`, and the chart suite |
| `docs/install.md` — `--version v`, `V=`, `helm install --version`, the chart coordinate | `make bump` | `make drift` |
| `README.md` — `--version v` | `make bump` | `make drift` |
| `SECURITY.md` — the supported-versions line | `make bump` | `make drift` |
| `.github/ISSUE_TEMPLATE/bug_report.yml` — the `mira X.Y.Z` placeholder | `make bump` | `make drift` |
| `CHANGELOG.md` — the heading and the link definitions | `make bump` | **none** (prose) |
| `Cargo.lock` | `cargo update --workspace` | `--locked` fails the build |
| `charts/mira/README.md` | `helm-docs` | `make helm-docs-check` |

`make bump TO=X.Y.Z` writes every row above and then regenerates the last two,
so step 1 is one command and `make drift` is how you check it did. The middle
column exists because the writer and the gate are deliberately the same thing:
`VERSION_SITES` in `scripts/check_drift.py` is one table of anchored patterns,
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

This is survivable because nothing reaches `main` un-gated and tags are cut from
already-green commits. The residual hole is a tag cut from a *stale* `main`
commit: `helm package --version/--app-version` overrides two of `Chart.yaml`'s
three version fields, but not the `artifacthub.io/images` annotation, so a chart
could ship advertising an image tag that is not the one being released.

## Rehearsal, and what has never run

`ci.yml`'s `release-dry-run` leg runs `make dist` and `make publish-dry` — the
real tarball, SBOM and checksum targets, then a full `cargo publish --workspace`
that packages all three crates, resolves each against the one before it out of a
temporary registry and compiles them, stopping at the upload — on every code PR.
`workflow_dispatch` runs the whole DAG with `publish=false`. Both have passed.

`publish=false` skips every network-publishing step, so `cosign sign`,
`helm push`, the `Digest:` scrape off `helm push`'s stderr and the Artifact Hub
`oras push` were all executing for the first time on v0.0.1. All four worked.

What is still unexercised is the `crates` job: v0.0.1 was tagged before the job
existed, so 0.0.2 is the first release whose `make publish` actually uploads.
That step is unrehearsable by construction — `publish-dry` does everything
except the one irreversible thing — and the anonymous-pull check added *after*
v0.0.1 is likewise running for the first time on a coordinate it has not seen.
`verify-release` fails loudly if either is wrong, and recovery is a bump to the
next patch.
