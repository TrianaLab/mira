# Changelog

Notable changes to Mira, in the format of
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), versioned by
[Semantic Versioning](https://semver.org/spec/v2.0.0.html).

Being pre-1.0, anything here may change: the block format, the query document,
the config keys, the `/mcp` tool set.

## [Unreleased]

### Added

- **Every published performance number is a variable.** `measurements.kyaml`
  holds all 35 of them once, with the 95 sites that quote each one listed
  beside it, and `make measurements-check` — now part of `ci-drift` — fails the
  build when a site and the registry disagree. `loadgen` grew `--records N`,
  which divides the rounds across the connections before the first byte goes
  out so the same command produces the same corpus on a slower box, and
  `--emit PATH`, which appends one JSON object per run keyed the way the
  registry is keyed. `scripts/measure/conn-sweep.sh` runs four connection
  counts three times each, and `make measurements-ingest RUN=…` takes the
  median per key across the passes, reports every site that now lies, and with
  `WRITE=1` updates the values — never the prose, because the sentence around a
  number is a claim a new number can invalidate. 12 of the 35 are marked
  `unscripted` rather than quietly presented as reproducible.
- **[A measurement contract](https://miradb.dev/internals/measurement/)**
  defining each of the four axes by numerator, denominator and what is inside
  the measurement, plus the five reasons a competitor's published figure is not
  the same quantity.

### Changed

- **A block's checksum is verified once per process, not once per open.** A
  published block never changes, so a scan that reopens the same corpus
  re-hashes bytes this process already hashed. `open_table` now consults a
  process-scoped map of `path -> (len, mtime, ino)` — the path alone is not
  enough, because `compact` renames a new table over an existing name, and the
  inode is what catches a restore that preserves both other fields — and an
  entry is only recorded once the file has been untouched for longer than any
  filesystem's mtime granularity, so a corruption that preserves the length
  cannot slip in inside one mtime tick. Worth between a quarter and two fifths
  of a single block's read path and 1.47x on a trace lookup. **The guarantee is
  unchanged**: a corrupt block is still refused rather than served, and a
  restart re-verifies everything.
- **The path the "query at scale" gap named was taken, and the row stays
  open.** `docs/market.md` inferred that an unpruned scan was
  integrity-check-bound because 4,380 MiB in 885 ms is ~5 GB/s "which is what
  `crc32fast` does on this machine". Warm, it is nearer 27 GB/s — the agreement
  was a coincidence published as a mechanism. Measured on both sides of one run
  instead, re-verification is between a quarter and two fifths of a single
  block's read path and about 1.1x of a full-corpus scan. What that scan is
  bound by is whether the corpus fits in page cache: a paired A/B changing only
  the corpus size puts it at 27-64 ns/row over 9.6 GiB — where one binary ranges
  2.6x against itself and the two arms are not separable — and **8.9 ns/row over
  5.01 GiB**, same binary, same predicate, ~187 K rows per block either way. The
  change that did move the unpruned scan is the one below it, reading fewer
  bytes rather than hashing them faster: opening the root table alone is
  **1.45x on logs and 1.88x on traces** over nine paired passes, or **1.8x and
  2.6x** for both changes together against 0.0.3. The row is still listed as a
  gap, because a page-cache-bound scan is not something either change fixes.
  `docs/architecture.md` section 11 now carries a second, named sitting rather
  than folding the new numbers into the old table; `scan_cost_per_row` prints
  the checksum's share as a column of its own run so the next such claim is a
  measurement.
- **Merging a version bump is the release.** `make bump TO=X.Y.Z`, a pull
  request, merge — and that is all of it. `ci.yml`'s `tag` job runs downstream
  of both required contexts on a push to `main`, reads the version out of
  `Cargo.toml`, and pushes `vX.Y.Z` if that version is not tagged yet. The tag
  step that used to follow the merge had no decision left in it, which is
  exactly the kind of step that gets forgotten and leaves a merged release that
  never shipped. Nothing downstream of the tag moved: the tag is *dispatched*
  at `release.yml` rather than relied on to trigger it — a tag pushed with
  `GITHUB_TOKEN` starts no workflow, and `workflow_dispatch` is one of the two
  events exempt from that — so `publish=true`, the tag-versus-`Cargo.toml`
  assertion and the `refs/tags/` cosign identity pin are all unchanged, and
  there is no PAT, deploy key or app token to rotate. A hand-pushed tag still
  works and is now the recovery path.
- **The build's own checks are Rust.** `scripts/check_ci.py`,
  `scripts/check_drift.py` and `scripts/gen_reference.py` are `crates/xtask`,
  a workspace member that nothing in `mira`'s dependency graph can see — so the
  crate count and the binary are untouched, while `cargo fmt`, `cargo clippy
  -D warnings`, `cargo doc -D warnings` and `cargo test` now cover the checker
  that gates every merge. It was the one part of the build nothing checked, and
  a regex that quietly stops matching passes forever. `ci` and `drift` were
  verified byte-identical to the scripts they replace before those were
  deleted. The port fixed a real bug on the way: the gate graph check panicked
  on a workflow without a `push:` trigger.
- **`make build` builds the binary and the example separately.** In one cargo
  invocation the example's dev-dependencies unify into the normal graph, so the
  release binary carried `tokio/test-util` and tower middleware nothing serves:
  5.99 MiB against 5.63. The shipped size is the declared one again.
- **vite 8 in the UI build, and the two packages that had to move with it.**
  `vite` 7.3.6 to 8.3.0 could not install on its own:
  `@sveltejs/vite-plugin-svelte@6.2.4` peers with `vite ^6.3.0 || ^7.0.0`, so
  `npm ci` failed with `ERESOLVE` before a test ran. The plugin goes to 7.3.0,
  which accepts vite 8 and in turn peers with `svelte ^5.46.4`. On the way,
  `rollupOptions.output.inlineDynamicImports` is deprecated under vite 8 and is
  now `codeSplitting: false` — the same instruction, and a load-bearing one,
  because `src/ui.rs` names the built files in a const table and a second chunk
  would be a 404 rather than a slower page. The bundle is still one `app.js`
  (78.00 kB), one `app.css` (8.39 kB) and one `index.html`, and the 22 UI tests
  pass. `taiki-e/install-action` moves 2.87.9 to 2.87.10 in the same release.

### Removed

- **Python.** The three scripts above, `scripts/requirements.txt`, and the
  `python3 -c` one-liners in the `Makefile` — JSON parsing, the coverage badge's
  figure and two "wait until the data arrives" loops, now `xtask parse-json`,
  `xtask coverage-json` and `scripts/wait-for-signals.sh`. `mkdocs` is a Python
  program and still is; nothing else in the tree needs an interpreter.

### Fixed

- **`make bump` aborted on every invocation, which is to say the release path
  did not run.** Porting `bumped_changelog` from Python transliterated its
  pattern character for character, and `(?=^## \[)` is the one character
  sequence that does not survive the move: Python's `re` has look-ahead, the
  `regex` crate has none, and `xtask` panics on a pattern it cannot compile. So
  the first command of the release runbook died before reading a byte of
  `CHANGELOG.md`. It went unnoticed because 0.0.3 was cut by the Python script
  that the same release deleted, and nothing called the Rust one until now. The
  match is lazy up to the next header and consumes it instead of looking at it,
  and the extraction is a `unreleased_body` function with a test — the thing
  actually missing, since no test executed this path either.

## [0.0.3] - 2026-09-12

Two performance gaps that [the comparison page](docs/market.md) listed as having
a cause inside this repository and a path to closing it. Both paths were taken.
No format change: a 0.0.2 block directory is read by this binary and a block it
writes is read by 0.0.2.

### Added

- **`ingest.shards`** — the number of flusher tasks per signal, each owning its
  own block sequence. Defaults to `0`, meaning `(cores / 2).clamp(1, 16)`; six on
  a twelve-core machine. The block directory is the manifest and a sequence is
  just a filename, so nothing above the flusher had to learn that there is more
  than one of them.
- **`make bump TO=X.Y.Z`** writes every version site the release needs, and
  `make drift` reads the same `VERSION_SITES` table to check them. A gate kept
  separately from a writer drifts towards the writer, because the writer is what
  people run; three sites that were ungated prose — both `Cargo.toml` path-dep
  pins, `SECURITY.md` and the issue template's placeholder — became gated by
  becoming writable. A pattern that matches nothing is a failure, because a gate
  for a line that has moved is a gate that is off.

### Changed

- **Ingest throughput no longer falls away under connection count.** One flusher
  per signal meant every connection past the point where that consumer saturated
  bought contention rather than work, and the curve peaked at four connections
  and declined. It now climbs to a plateau at sixteen to thirty-two. Paired A/B
  on the same box, three passes a row: 1.19× at eight connections, 1.24× at
  sixteen, 1.41× at thirty-two and **1.55× at ninety-six**, where 734,142
  records/s became 1,136,941. Four connections and below are unchanged by
  design — dispatch is first fit from shard 0, so a node that never saturates one
  flusher never starts a second and goes on producing one block per seal window
  instead of six nearly-empty ones.
- **Peak resident memory fell with it**, which was not the goal: 689 MiB at four
  connections against 1,366 MiB. Six open blocks per signal is more block state
  than one, so the saving is the queue — the exports that used to sit in it are
  never resident at all.
- **`ingest.queue` is now a per-signal total rather than a per-shard depth.**
  Each shard gets `queue.div_ceil(shards)`, so raising the shard count does not
  multiply the worst-case resident cost. An operator who set `--queue` explicitly
  keeps the same memory bound they had.
- **The attribute scan is vectorised.** Per-row `attr_matches` is replaced by a
  predicate evaluated once per contiguous, binary-searched parent run, scattering
  into a `Vec<bool>` over root rows — no hash set anywhere. Measured against
  0.0.1 on one corpus, restart between: **6.7×** on a matching attribute value,
  **5.3×** on an unfiltered `limit 100`, **3.5×** on a substring that fills its
  limit, and on an eight-reader read mix 5.1× on the `attr` class p50 and 4.6× on
  `errors`, taking the whole mix from 40 to 50 queries/s. The metrics `series`
  class read slower in the mix and does not share this path at all — see
  **Fixed** below and
  [architecture section 11](docs/architecture.md#11-performance-model).
- **The WAL watermark is a set, not a high-water mark.** Shards seal out of
  order, so the highest sequence in a block says nothing about the ones below it.
  A block now claims the oldest sequence of its signal that nobody has published
  and that the block does not itself hold. Getting this wrong in the unsafe
  direction loses data silently, so section 9 states which way it is allowed to
  be wrong and why.

### Fixed

- **The published 175 ms figure for an unpruned full scan does not reproduce and
  has been withdrawn.** Re-measured across four predicates on both binaries, the
  row is 885 ms steady over 27.1 M rows and 137 blocks — better than 0.0.1's
  1,163 ms, and much worse than the number that had been printed. The
  re-measurement also changed the diagnosis: `scan_cost_per_row` prices predicate
  evaluation at 0.047–5.586 ns/row against 24–25 ns/row for the same block
  through the whole read path, so the unpruned scan is bound by the CRC32 every
  `Block::open` runs over the whole body, not by the scan.
- **The metrics attribute join was quadratic in a block's point count.**
  `collect_attrs` scanned the whole attribute table per parent and runs once per
  matched data point, so the metrics path kept precisely the shape the
  vectorisation above removed from the log path — missed because `series_open`
  loads its tables directly rather than through `query::Block::open`, and so
  never met `Attrs`. It uses it now. The new `series_cost_per_point` prices a
  point at a flat 0.9–1.1 µs where it used to rise with the point count: at
  50,000 points in a block, 1,582 ms became 53.9.
- **The `series` figure in the read mix is not a regression, and the note saying
  it was has been replaced with the measurement.** `series.rs` is byte-identical
  across this release and the only vectorised function it can reach is called
  from inside `q.terms.iter()`, which is empty for a query with no `where`. Two
  binaries differing only in the read path measure 449.7 ms against 445.1 and
  524.5 against 608.7 — both directions, all inside one binary's spread against
  itself. Mira does not keep regressions as loose notes, and it does not keep
  phantom ones either.
- The `ingest.shards` justification no longer claims `available_parallelism`
  cannot see a cgroup CPU quota. It can. The cases it genuinely cannot see —
  `cpu.shares`/`cpu.weight`, a pod with no quota on a large node, hyperthreads
  counted as cores — are what the knob is for.

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

## [0.0.1] - 2026-09-12

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
- No entity *predicate* in the query document. Every block stores each
  resource's entity key and `/api/v1/entities` lists them, but no filter
  selects on one.
- No `F_FULLFSYNC` fallback for volumes that answer `EINVAL`/`ENOTSUP`.
- No authentication, authorisation or TLS. Mira expects to sit behind something
  that has them.

[Unreleased]: https://github.com/TrianaLab/mira/compare/v0.0.3...HEAD
[0.0.3]: https://github.com/TrianaLab/mira/compare/v0.0.2...v0.0.3
[0.0.2]: https://github.com/TrianaLab/mira/compare/v0.0.1...v0.0.2
[0.0.1]: https://github.com/TrianaLab/mira/releases/tag/v0.0.1
