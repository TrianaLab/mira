# Changelog

Notable changes to Mira, in the format of
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), versioned by
[Semantic Versioning](https://semver.org/spec/v2.0.0.html).

Being pre-1.0, anything here may change: the block format, the query document,
the config keys, the `/mcp` tool set.

## [Unreleased]

## [0.0.2] - 2026-09-12

0.0.1 shipped the binary, the image and the chart. It did not ship the crates,
because the publish job did not exist yet — so this release exists mostly to
make `cargo install --locked miradb` true.

### Added

- **Published on crates.io**: `cargo install --locked miradb` installs the
  `mira` binary, and [`miradb-core`](https://docs.rs/miradb-core) and
  [`miradb-proto`](https://docs.rs/miradb-proto) are there for embedding the
  engine. The release workflow publishes them after the GitHub Release exists
  and `verify-release` resolves all three out of the sparse index.
- **`make ci` runs the whole pipeline on your machine.** Every leg is a
  `make ci-*` target in `ci.mk`, `.github/workflows/ci.yml` is a dispatcher
  over that file, and `scripts/check_ci.py` fails the build on any `run:` step
  that is not one — so a red leg is reproducible with one command rather than
  by pushing again. `make ci-changes` says which legs a diff needs.
- **The chart reference, contributing guide, security policy and code of
  conduct are pages on the site**, not files on a code host. Nothing in the
  documentation's body content links out to GitHub any more.

### Changed

- The three packages are named `miradb`, `miradb-core` and `miradb-proto`
  rather than `mira`, `mira-core` and `mira-proto` — `mira` on crates.io is an
  unrelated crate from 2024. Nothing else moved: the binary is still `mira`,
  the dependency keys and `use` paths are still `mira_core` / `mira_proto`, and
  no source file changed. `cargo test -p mira` is now `-p miradb`.
- **The README's coverage badge is the measured figure, not the ratchet.** It
  reads [miradb.dev/coverage.json](https://miradb.dev/coverage.json), which the
  deploy that publishes the site writes from its own `cargo llvm-cov` run, and
  the file records the commit it measured. `COVERAGE_MIN` is unchanged and
  still enforced on every code PR; it was always a floor rather than a
  measurement, and the badge no longer pretends otherwise.
- The README states the value rather than the design, and every badge on it is
  now something a reader can check.

### Fixed

- **The first release of a `ghcr.io` package is private, and nothing in a
  workflow can change that** — so 0.0.1 published an image and a chart that
  answered `DENIED` to everyone, in a run that was green because every step had
  logged in first. `verify-release` now asks for an anonymous pull token before
  it authenticates, for both coordinates, and fails the release if either is
  refused. Both 0.0.1 coordinates are public now.
- `mira update` names the tool it could not find when there is no shell to run
  the installer with, instead of failing with the installer's own error about
  something else.

First tagged release. Everything below is the initial cut rather than a
delta — there is no previous version to have changed from.

### Added

- **OTLP ingest for logs, traces and metrics** over OTLP/gRPC `4317` and
  OTLP/HTTP `4318`, protobuf or proto3-JSON, plain or gzipped — which is what a
  stock OpenTelemetry Collector sends, since both its OTLP exporters compress
  by default.
- **Storage as the OTAP star schema in Apache Arrow IPC.** Blocks are
  immutable, sealed on size or age, published by rename, and read back out of
  `mmap` with no buffer copies on the hot tier — asserted by a test that walks
  every buffer of every column and requires all of them to point inside the
  mapping.
- **Durable acknowledgement, on a write-ahead log.** An export is acked once its
  bytes are in the log's page cache — p50 7 µs — and the log is fsynced on a
  timer rather than per batch. With `ingest.wal` off the ack waits for the block
  itself to be fsynced and renamed, which is durable on the same terms and two
  orders of magnitude slower (p50 657 ms). Read-your-writes holds either way,
  because a query reads the open block as well as the published ones.
- **Block pruning.** The directory name is the time index, and a Bloom sidecar
  per block answers "could this hold that trace id, that attribute value"
  before anything is opened. A damaged or missing sidecar reads as "scan me".
- **A cold tier in the same format.** The retention sweep rewrites aged blocks
  ZSTD-compressed in place, to about 0.12 of their size. The codec is per-batch
  IPC metadata, so no reader is told which tier it is on, and an interrupted
  rewrite leaves a directory that still reads correctly.
- **Retention by unlink**, dropping whole block directories; in-flight readers
  keep working, guaranteed by POSIX.
- **A query API** on `POST /api/v1/query`: attribute predicates, time bounds,
  keyset pagination via `next`/`after`, and a `stats` object per response that
  makes the pruning visible. An unknown top-level key is refused by name rather
  than answered over the unfiltered window.
- **MCP on `/mcp`** — eight tools over JSON-RPC 2.0 (`query_records`,
  `get_trace`, `query_metric`, `list_metrics`, `correlate`, `service_map`,
  `list_services`, `list_alerts`), no session id, so any replica can answer any
  call, through the same read path the UI uses. Wiring and a worked
  investigation: [`docs/agents.md`](docs/agents.md).
- **The frame algebra**, `POST /api/v1/correlate`: a filter in, and the region
  of telemetry around it out — the real time extent, the traces those records
  belong to, and the entities that took part. Every operation on a frame returns
  a frame, so an investigation is a walk with no illegal state in it.
- **A service map computed on read**, `POST /api/v1/map`, out of the same join.
  No metrics generator, no second write path, nothing to deploy beside it.
- **Alerting**, KYAML rules that embed a query document verbatim and threshold
  it as a count or as a ratio of two counts — which covers percentiles exactly
  rather than approximately, since `p95(d) > 250ms` *is*
  `|{d > 250ms}| / |d| > 5%`. Webhook dispatch, with TLS behind a Cargo feature
  so the default build carries no rustls.
- **A browser UI served from the binary** — records, trace waterfalls and
  metric charts out of `include_bytes!`, with the built bundle committed under
  `crates/mira/ui/dist` so there is no Node toolchain in the build.
- **A terminal UI**, `mira mira`: the same tabs, filter grammar and trace
  waterfall over `termios` raw mode and ANSI, with no TUI framework and zero
  crates added — plus the frame, the service map, the alert rules and a node
  view (uptime, peak RSS, disk headroom, bytes on disk per row per signal). It
  reads a running replica over `--addr` or a block directory in-process over
  `--data-dir`, so a detached volume is still readable with nothing running.
- **Correlation edges on the read path** — a span comes back with its events
  and links, a metric series with the exemplars naming the traces behind it.
- **Nested attributes and structured log bodies** decoded from OTLP `AnyValue`
  and returned as nested JSON by the query API and MCP, printed inline by both
  UIs.
- **KYAML configuration** with `${env:VAR,default}` interpolation, every flag
  also settable from the file — [`docs/config.md`](docs/config.md) is the whole
  surface.
- **SIGTERM drains**: stop accepting, let in-flight exports reach their ack,
  seal and publish the open blocks, exit.
- **`/health` and `/readyz`**, the same 200 and the same JSON body of per-signal
  `shed` and `failed` counts, neither of which opens the block directory. A path
  the binary does not serve is a 404.
- **Two active replicas on one host** sharing a data directory with no
  coordination: the block name carries a node id derived from `--node`, so
  publishes are independent renames and either replica's scan sees both.
- **A startup refusal on network filesystems.** `mmap` over NFS, SMB or CephFS
  turns a server hiccup into `SIGBUS`; a `statfs` at startup names the
  filesystem and says what to point `--data-dir` at instead. FUSE warns rather
  than refuses.
- **A load harness**, `cargo run --release --example loadgen`, that scores all
  four performance axes in one run: ingest throughput, ack latency, six classes
  of query latency to p999, peak resident set and bytes on disk per record.
- **A differential test for the query engine** — random stores, random queries
  and a reference model sharing no code with the engine, seeded so a failure
  replays exactly.
- **A distroless Docker image**, one binary and one volume, on
  `base-nossl-debian12` plus the one library the binary actually needs.
- **A Helm chart**, `charts/mira`, published as a signed OCI artifact to
  `oci://ghcr.io/trianalab/charts/mira` and listed on
  [Artifact Hub](https://artifacthub.io/packages/helm/mira/mira), with a closed
  `values.schema.json` and the ownership metadata pushed alongside it.
- **An install script**, `curl -fsSL https://miradb.dev/install.sh | bash`, which picks the
  target triple, checks `SHA256SUMS` and verifies the SLSA provenance
  attestation when the GitHub CLI is on `PATH`.

### Security

- `cargo-deny` gates advisories, licences, bans and sources in CI; `Cargo.lock`
  is committed and the documented install path is `cargo install --locked`.
- A pull request that can change the image builds it and scans it with Trivy at
  HIGH and CRITICAL, fixable only — the same image the release publishes, from
  the same prebuilt binary.
- The image and the chart each carry a keyless cosign signature; the tarballs,
  the CycloneDX SBOM and `SHA256SUMS` are covered by one SLSA provenance
  attestation over the release. The install script checks `SHA256SUMS` and, when
  the GitHub CLI is present, verifies that attestation before it moves anything
  into place.
- Reporting is GitHub private vulnerability reporting — see
  [`SECURITY.md`](SECURITY.md).

### Known limitations

Tracked here because they are the difference between what the README promises
and what a reader might assume;
[`docs/architecture.md`](docs/architecture.md) section 0.1 is the authoritative
list.

- Ingestion is allocation-lean, not zero-copy: `prost` memcpies every string.
- No block cache — every query re-opens and re-CRCs the blocks it touches.
- No cross-replica query fan-out, and no peer list to configure.
- No entity selector: every block stores each resource's entity key and no read
  surface reads it yet.
- No `F_FULLFSYNC` fallback for volumes that answer `EINVAL`/`ENOTSUP`.
- No authentication, authorisation or TLS. Mira expects to sit behind something
  that has them.

[Unreleased]: https://github.com/TrianaLab/mira/compare/v0.0.1...HEAD
[0.0.1]: https://github.com/TrianaLab/mira/releases/tag/v0.0.1
