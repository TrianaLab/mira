<img src="docs/assets/mira-wordmark.svg" alt="Mira" height="64">

[![CI](https://github.com/TrianaLab/mira/actions/workflows/ci.yml/badge.svg)](https://github.com/TrianaLab/mira/actions/workflows/ci.yml)
[![Release](https://img.shields.io/github/v/release/TrianaLab/mira?label=release&color=2D4857)](https://github.com/TrianaLab/mira/releases/latest)
[![crates.io](https://img.shields.io/crates/v/miradb?label=crates.io&color=2D4857)](https://crates.io/crates/miradb)
[![Artifact Hub](https://img.shields.io/endpoint?url=https://artifacthub.io/badge/repository/mira)](https://artifacthub.io/packages/search?repo=mira)
[![Coverage](https://img.shields.io/badge/dynamic/json?url=https%3A%2F%2Fmiradb.dev%2Fcoverage.json&query=%24.line&suffix=%25&label=coverage&color=brightgreen)](docs/internals/testing.md)
[![Rust](https://img.shields.io/badge/rust-1.88%2B-orange)](https://github.com/TrianaLab/mira/blob/main/Cargo.toml)
[![Licence](https://img.shields.io/badge/licence-Apache--2.0-blue)](LICENSE)

**Store your logs, traces and metrics in one small binary. Read them back in a
browser, in your terminal, or straight from an AI agent. No cluster, no
sidecar, no database beside it.**

```sh
curl -fsSL https://miradb.dev/install.sh | bash
mira --data-dir ./data
```

Send data with any OpenTelemetry exporter — gRPC on `4317`, HTTP on `4318`.
Then open `http://localhost:4318/`, or point an agent at `POST /mcp`. The only
thing Mira keeps is the directory of files you gave it.

<img src="docs/assets/tui/investigation.gif" alt="A recording of Mira's terminal UI. The log list over the last hour; a filter typed live, severity_text=ERROR, narrowing 14,371 records to 824 in 7.3ms; the service map, where errors propagate frontend to checkout to payments while inventory stays clean; then the trace under the failure — eight spans over 76.08ms, with a retry and an exception marked on the timeline.">

`mira mira`, recorded against a running node. Not a mock-up: the footers are
that run's own query plans and wall clocks.

## The numbers

| | |
| --- | --- |
| **1,537,875 records/s** | ingest, on **2.23 of 12 cores** — 886k/s/core at one connection |
| **2.6 ms** | to prove a value is in **none** of 137 blocks, zero blocks opened |
| **4.7 ms** | every span of one trace, out of 27.1M spans on disk |
| **7 µs** | the durable log append inside an acknowledgement |
| **0.14** | bytes on disk per byte on the wire, once compacted |
| **6.20 MiB stripped, 149 crates** | `zstd-sys` is the only C dependency, and it vendors its source |

One process on an Apple M3 Pro, measured with the load harness in this
repository. What each number counts and what it leaves out is
**[the measurement contract](docs/internals/measurement.md)**; how they compare
to other tools, including where Mira loses, is **[docs/market.md](docs/market.md)**.

## Four ways to read it

Same data, same filters, same code underneath.

| | | |
| --- | --- | --- |
| **Browser** | `http://localhost:4318/` | Baked into the binary. The whole view is in the URL, so an alert webhook can link you straight back to what fired. |
| **Terminal** | `mira mira` | The same views against a running copy — or against a directory of files **with nothing running at all**. |
| **MCP** | `POST /mcp` | Nine tools for agents. No session to set up, so any copy can answer any call. The ninth writes the incident report. |
| **HTTP** | `/api/v1/…` | `query`, `correlate`, `map`, `entities`, `metrics`, `alerts`. |

The ninth tool is the interesting one. An agent hands over what it found and
gets back a written root-cause report — but first `render_rca` re-runs every
piece of cited evidence against the stored data. A citation it cannot reproduce
fails the call by name, and nothing gets written:

<img src="docs/assets/tui/write-up.gif" alt="A recorded terminal session. An agent's three claims are listed, then render_rca refuses them: nothing was rendered and nothing was stored, 1 of 3 citations do not hold against this store, naming the claim and the filter that matched no records. The claim is corrected and the call returns a 59-line document whose evidence section carries the record count beside every claim — 229, 229, and 8 for the trace. A last query finds the write-up itself stored as a log record.">

The report is written back as an ordinary log record, so you search for it the
same way you search for anything else. Worked example:
**[docs/agents.md](docs/agents.md)**.

## See it

**[miradb.dev/demo](https://miradb.dev/demo/)** — one command, then six screens
of both UIs, from data you generated a minute earlier.

<img src="docs/assets/ui/trace.png" alt="The same trace in Mira's web UI: eight nested spans over 76.08ms across frontend, inventory, checkout and payments, the five failed ones in red, with the query cost in the header — 8 matched of 30,720 scanned, 1 block, 47.5ms.">

```sh
docker run -p 4317:4317 -p 4318:4318 -v mira-data:/data ghcr.io/trianalab/mira:latest
helm install mira-operator oci://ghcr.io/trianalab/charts/mira-operator \
  --namespace mira-system --create-namespace   # then: kubectl apply a MiraCluster
```

Linux glibc >= 2.34 and macOS, x86_64 and arm64. `--version v0.4.3` pins the
installer to a release; every one ships an SBOM, `SHA256SUMS`, a cosign
signature and a SLSA provenance attestation.

## Where to go next

| | |
| --- | --- |
| Install it properly, on Kubernetes or otherwise | **[docs/install.md](docs/install.md)** |
| First query, and what the `stats` object tells you | **[docs/quickstart.md](docs/quickstart.md)** |
| Connect an agent, and a worked investigation | **[docs/agents.md](docs/agents.md)** |
| Every flag and config key | **[docs/config.md](docs/config.md)** · **[CLI](docs/reference/cli.md)** |
| How it works, and why it is shaped this way | **[docs/architecture/index.md](docs/architecture/index.md)** |
| Why it exists at all | **[MANIFESTO.md](MANIFESTO.md)** |

## Scope

Mira is pre-1.0. One process only ever answers from its own files — to read
across several, put `mira proxy` in front, and it merges record search but not
correlate, map, metrics or entities. There is no entity filter and no block
cache yet. The full list is
[architecture section 0.1](docs/architecture/corrections.md#01-what-is-not-true-yet).

## Contributing

Every gate CI runs is a target in the [`Makefile`](Makefile), and CI calls
nothing else:

```sh
make            # the target list
make check      # fmt, clippy, tests, rustdoc, UI, supply chain, drift, docs, coverage
```

[CONTRIBUTING.md](CONTRIBUTING.md) is the walkthrough. If the change is
structural, read [architecture section 0](docs/architecture/corrections.md)
first — it lists the mechanisms that do not survive contact with the formats.
Vulnerabilities go to [SECURITY.md](SECURITY.md), not to an issue.

## Licence

Apache-2.0.
