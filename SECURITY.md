# Security policy

## Supported versions

Mira is pre-1.0 — `0.0.1`, with no published binaries. There is exactly one
supported version and it is the tip of `main`. Fixes land there; there are no
backport branches to ask about.

| Version | Supported |
| --- | --- |
| `main` | yes |
| everything else | no |

When the first tagged release exists this table gains one row: the latest
release. It will not gain a third.

## Reporting a vulnerability

Use GitHub private vulnerability reporting:
**<https://github.com/TrianaLab/mira/security/advisories/new>**

Do not open a public issue, and do not post a reproducer anywhere public. There
is no email channel and no PGP key — the advisory form is private to the
maintainers and it is the whole process.

Include the `mira --version` output (or the commit), how the process was
reached (OTLP/gRPC `4317`, OTLP/HTTP `4318`, the query API, `/mcp`, the UI, or
the CLI), and the smallest input that triggers it. A raw request body as a hex
dump or a base64 blob is worth more than a description of one.

What to expect. These are deliberately modest, because a small project that
promises 24 hours and takes a week has published a lie rather than a policy:

- **Acknowledgement within 72 hours.**
- An assessment — in scope or not, and a severity — within **7 days**.
- A fix on `main` for anything we rate high or critical within **30 days**, or,
  if it will take longer, a written reason and a date.
- Credit in the advisory unless you ask us not to. Coordinated disclosure: we
  publish when the fix is on `main`.

## Scope

Mira is a database that parses hostile bytes off a socket for a living. The
trust boundary is the network, and everything that crosses it is in scope:

- **OTLP parser crashes.** A protobuf `ExportLogsServiceRequest`, or its
  proto3-JSON equivalent on `/v1/logs`, `/v1/traces`, `/v1/metrics`, that
  panics, aborts, hangs, or allocates without bound. `panic = "abort"` is set in
  the release profile, so a panic in a request handler is not a 500 — it is the
  process gone, taking every other tenant's in-flight export with it. Treat a
  reachable panic as a denial of service, because it is one.
- **The gzip path.** Every OTLP transport accepts `content-encoding: gzip`. A
  decompression bomb that gets past `ingest.max_request_bytes` (16 MiB by
  default, and it is the ceiling on what a gzip body may *inflate* to, not on
  what arrives) is in scope.
- **The KYAML parser.** `--config` is operator-supplied and therefore trusted,
  but the same parser reads query documents and API bodies off the network, and
  those are not. A crash or an unbounded allocation from a query document is in
  scope.
- **Unsoundness in the `unsafe` blocks.** Mira reads blocks straight out of an
  `mmap` and has `unsafe` in the block reader, the `statfs`/`madvise` calls and
  the terminal's `termios` handling. Anything that turns a malformed *block* —
  as opposed to a malformed file — into a read out of bounds, a use after free,
  or a data race is in scope, and is the highest-severity class here.
- **The read surfaces.** The query API, `/mcp` and the UI served from the
  binary: a query that reads records the caller's filters exclude, a response
  that leaks a path, a stored XSS through an attribute value rendered in the UI.
- **The supply chain.** A dependency advisory that `make audit` should have
  caught and did not, or a released artefact that does not match this tree.

## Not in scope

Not because they do not matter, but because they are documented properties
rather than defects — a report about one of these gets a link back to this
section:

- **Mira has no authentication, authorisation or TLS.** No token, no mTLS, no
  per-tenant isolation. It is designed to sit behind something that has those:
  a collector, a service mesh, a network policy. "I sent an export to `4317`
  without credentials" and "I read another service's spans" are the intended
  behaviour of an open OTLP receiver, not a vulnerability. If you can get past
  a *stated* limit — `max_request_bytes`, the retention TTL, `limit`/`max_points`
  on a query — that is a different matter, and it is in scope.
- **A hostile data directory.** Mira `mmap`s its blocks; a filesystem that lies
  about a mapping delivers `SIGBUS`, which is a signal with nothing to catch.
  That is why the binary refuses to start on a network filesystem and warns on
  FUSE. Corrupting the blocks under a running process, or pointing `--data-dir`
  at a filesystem you control and then breaking it, is a machine you already
  own. A malformed block that reads *out of its own mapping*, however, is the
  unsoundness bullet above, and that we want.
- **Resource exhaustion the operator asked for.** An unfiltered query over the
  whole of retention is slow by construction; the README says how slow. Tune
  retention, or put a proxy in front.
- **Upstream advisories.** Report them upstream. Tell us too, so `make audit`
  and the pin move.

## What is already gated

- `make audit` runs `cargo deny check` — advisories, licences, bans, sources —
  and CI runs the same target, so a `RUSTSEC` advisory against anything in the
  tree fails the build rather than waiting for someone to notice.
- `make scan-image` runs `trivy image --severity HIGH,CRITICAL --ignore-unfixed
  --exit-code 1` against the container image, on every pull request that touches
  the binary or the `Dockerfile`. `--ignore-unfixed` because a finding with no
  upstream fix is not an action, it is a subscription to noise; a fixable HIGH is
  an action, so it is a red build. `cargo deny` covers the crates we chose and
  Trivy covers the base image and everything underneath it — neither sees the
  other's half.
- Both of those hang off a **`security-required`** status context that is
  separate from `required`. Separate because the two answer different questions —
  "does it work" and "is it safe to ship" — and a merge should have to satisfy
  both on their own terms rather than one aggregate somebody can argue was flaky.
- **Nothing runs on a schedule alone.** A weekly `cron` re-runs the same gates so
  an advisory published against an unchanged tree surfaces without waiting for
  somebody to open a pull request.
- `Cargo.lock` is committed and the install path is `cargo install --locked`,
  so the dependency set that was audited is the dependency set that builds.
- Every third-party GitHub Action is pinned to a full commit SHA, checked by
  `scripts/check_ci.py` in CI. A tag is a mutable pointer, and a mutable pointer
  in `uses:` is arbitrary code execution holding a token with `packages: write`.
- The release path is rehearsed on every pull request: `release-dry-run` runs the
  same `make dist` that a tag runs. The first time we build a release is not the
  day we publish one.
- The dependency budget is a stated product property, and a security property
  by accident: the crate count in the README is also the number of crates whose
  advisories we inherit, and `zstd-sys` is deliberately the only C in the tree.

## Verifying a release

Everything a tag publishes is signed, and the signature is over the **digest**,
never the tag — a tag is a name and names can be repointed. The release workflow
also refuses to publish over a coordinate that already exists, so a re-run of a
released version fails instead of quietly replacing what you verified yesterday.

There is no tagged release yet, so none of this has a subject to run against
today. The commands are here because they are the contract the workflow's own
`verify-release` job runs against every publication — if they stop working, that
job goes red before you find out.

Container image, keyless (no key to distribute, no key to leak — the identity is
the workflow that signed it):

```sh
cosign verify \
  --new-bundle-format=false \
  --certificate-oidc-issuer=https://token.actions.githubusercontent.com \
  --certificate-identity-regexp='^https://github\.com/TrianaLab/mira/\.github/workflows/release\.yml@refs/tags/' \
  ghcr.io/trianalab/mira@sha256:<digest>
```

`--new-bundle-format=false` is not optional and not cosmetic: cosign v3 defaults
to the new Sigstore bundle, and a verifier on the default looks in a place the
signature is not and reports a signed artefact as unsigned. We sign in the legacy
format because that is the one Artifact Hub's indexer reads.

The Helm chart is signed the same way, at `ghcr.io/trianalab/charts/mira`.

Tarballs: check the sums, then check the provenance. The sums prove the bytes are
the bytes; the attestation proves which workflow run, from which commit, produced
them.

```sh
gh release download vX.Y.Z --dir mira-release
cd mira-release && sha256sum -c SHA256SUMS
gh attestation verify mira-X.Y.Z-x86_64-unknown-linux-gnu.tar.gz --repo TrianaLab/mira
```

A signature that does not verify, or an artefact whose digest is not the one the
release recorded, is the supply-chain bullet in Scope above. Report it.
