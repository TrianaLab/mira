# Security policy

## Supported versions

Mira is pre-1.0 — `0.3.0`. Two things are supported: the latest release and the
tip of `main`. Fixes land on `main` and ship in the next release; there are no
backport branches, and this table will not gain a third row.

| Version | Supported |
| --- | --- |
| `main` | yes |
| latest release | yes |
| everything else | no |

## Reporting a vulnerability

Use GitHub private vulnerability reporting:
**<https://github.com/TrianaLab/mira/security/advisories/new>**

Do not open a public issue, and do not post a reproducer anywhere public. There
is no email channel and no PGP key: the form is private to the maintainers and
it is the whole process.

Include the `mira --version` output (or the commit), how the process was reached
(OTLP/gRPC `4317`, OTLP/HTTP `4318`, the query API, `/mcp`, the UI, or the CLI),
and the smallest input that triggers it. A raw request body as a hex dump or a
base64 blob is worth more than a description of one.

What to expect:

- **Acknowledgement within 72 hours.**
- An assessment — in scope or not, and a severity — within **7 days**.
- A fix on `main` for anything we rate high or critical within **30 days**, or,
  if it will take longer, a written reason and a date.
- Credit in the advisory unless you ask us not to. Coordinated disclosure: we
  publish when the fix is on `main`.

## Scope

The trust boundary is the network, and everything that crosses it is in scope:

- **OTLP parser crashes.** A protobuf `ExportLogsServiceRequest`, or its
  proto3-JSON equivalent on `/v1/logs`, `/v1/traces`, `/v1/metrics`, that panics,
  aborts, hangs, or allocates without bound. `panic = "abort"` is set in the
  release profile, so a panic in a request handler is not a 500 but the process
  gone: a denial of service.
- **The gzip path.** Every OTLP transport accepts `content-encoding: gzip`. A
  decompression bomb that gets past `ingest.max_request_bytes` (16 MiB by
  default, the ceiling on what arrives *and* on what a gzip body may inflate to)
  is in scope.
- **The KYAML parser.** `--config` is operator-supplied and therefore trusted,
  but the same parser reads query documents and API bodies off the network, and
  those are not. A crash or unbounded allocation from one is in scope.
- **Unsoundness in the `unsafe` blocks.** Mira has `unsafe` in the block reader,
  the `statfs`/`madvise` calls and the terminal's `termios` handling. Anything
  that turns a malformed *block* into a read out of bounds, a use after free, or
  a data race is in scope, and is the highest-severity class here.
- **The read surfaces.** The query API, `/mcp` and the UI: a query that reads
  records the caller's filters exclude, a response that leaks a path, a stored
  XSS through an attribute value.
- **The supply chain.** A dependency advisory that `make audit` should have
  caught and did not, or a released artefact that does not match this tree.

## Not in scope

Documented properties rather than defects — a report about one of these gets a
link back to this section:

- **Mira has no authentication, authorisation or TLS.** No token, no mTLS, no
  per-tenant isolation. It is designed to sit behind something that has those: a
  collector, a service mesh, a network policy. Getting past a *stated* limit —
  `max_request_bytes`, the retention TTL, `limit`/`max_points` on a query — is a
  different matter, and it is in scope.
- **A hostile data directory.** Mira `mmap`s its blocks; a filesystem that lies
  about a mapping delivers `SIGBUS`, a signal with nothing to catch — which is
  why the binary refuses to start on a network filesystem and warns on FUSE.
  Corrupting blocks under a running process, or pointing `--data-dir` at a
  filesystem you control and breaking it, is a machine you already own. A
  malformed block that reads *out of its own mapping* is the unsoundness bullet
  above.
- **Resource exhaustion the operator asked for.** An unfiltered query over the
  whole of retention is slow by construction; the README says how slow. Tune
  retention, or put a proxy in front.
- **Upstream advisories.** Report them upstream. Tell us too, so `make audit`
  and the pin move.

## What is already gated

- `make audit` runs `cargo deny check` — advisories, licences, bans, sources —
  and CI runs the same target, so a `RUSTSEC` advisory against anything in the
  tree fails the build.
- `make scan-image` runs `trivy image --severity HIGH,CRITICAL --ignore-unfixed
  --exit-code 1` against the container image, on every pull request that touches
  the binary or the `Dockerfile`. `cargo deny` covers the crates we chose and
  Trivy the base image underneath — neither sees the other's half.
- Both hang off a **`security-required`** status context separate from
  `required`: the two answer different questions, "does it work" and "is it safe
  to ship".
- **Nothing runs on a schedule alone.** A weekly `cron` re-runs the same gates,
  so an advisory published against an unchanged tree surfaces anyway.
- `Cargo.lock` is committed and the install path is `cargo install --locked`,
  so the dependency set that was audited is the dependency set that builds.
- Every third-party GitHub Action is pinned to a full commit SHA, checked by
  `make workflows` in CI. A tag is a mutable pointer, and a mutable pointer
  in `uses:` is arbitrary code execution holding a token with `packages: write`.
- The release path is rehearsed on every pull request: `release-dry-run` runs the
  same `make dist` that a tag runs.
- The dependency budget is a stated product property, and a security property by
  accident: the crate count in the README is also the number of crates whose
  advisories we inherit, and `zstd-sys` is the only C in the tree.

## Verifying a release

Everything signed is signed over the **digest**, never the tag: a tag is a name
and names can be repointed. The release workflow refuses to publish over an
existing coordinate, so a re-run of a released version fails.

The image and the chart each carry a cosign signature over their digest; the
tarballs and the SBOM carry `SHA256SUMS`, which itself carries one SLSA
provenance attestation. **The three crates.io crates and the `:artifacthub.io`
metadata tag carry neither** — `cargo install --locked miradb` is verified by the
registry's own `.crate` checksum. The per-artifact table is in
[Release architecture](https://miradb.dev/internals/releases/#what-is-signed-and-what-is-not).

Every command below has a subject from `v0.0.1` on, and the workflow's own
`verify-release` job runs them against every publication: it re-downloads what a
stranger downloads and checks it this way.

Container image, keyless — the identity is the workflow that signed it:

```sh
cosign verify \
  --new-bundle-format=false \
  --certificate-oidc-issuer=https://token.actions.githubusercontent.com \
  --certificate-identity-regexp='^https://github\.com/TrianaLab/mira/\.github/workflows/release\.yml@refs/tags/' \
  ghcr.io/trianalab/mira@sha256:<digest>
```

`--new-bundle-format=false` is not optional: cosign v3 defaults to the new
Sigstore bundle, and a verifier on the default looks in a place the signature is
not and reports a signed artefact as unsigned. We sign in the legacy format
because that is the one Artifact Hub's indexer reads.

The operator image and the Helm chart are signed the same way, at
`ghcr.io/trianalab/mira-operator` and `ghcr.io/trianalab/charts/mira-operator`.
They are on their own version line, so the tag is the operator's.

Tarballs: check the sums, then the provenance.

```sh
gh release download vX.Y.Z --dir mira-release
cd mira-release && sha256sum -c SHA256SUMS
gh attestation verify mira-X.Y.Z-x86_64-unknown-linux-gnu.tar.gz --repo TrianaLab/mira
```

A signature that does not verify, or an artefact whose digest is not the one the
release recorded, is the supply-chain bullet in Scope above. Report it.
