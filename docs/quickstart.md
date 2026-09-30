---
description: Get data into Mira and read it back. One command for a demo with realistic data, then the manual path — run it, fill it, and query it from curl, a browser, your terminal or an AI agent.
---

# Quickstart

**For:** anyone who has Mira and wants data in it. If you have not seen it yet,
[See it work](demo.md) is the shorter route.

## One command

```sh
make demo
```

It builds Mira and the data generator, starts Mira on a scratch directory,
writes 45 minutes of backdated telemetry for a shop with four services, waits
for the first block of each signal — logs, traces and metrics — to seal, and
prints where to look. Mira stores data as blocks: sealed files it never
rewrites. Ctrl-C stops it; `make demo-clean` deletes the directory.
You need a Rust toolchain and nothing else — no Docker, no second terminal.
[See it work](demo.md) is what comes out.

The rest of this page is the manual path: your own data, and the query API.

## Run it

```sh
mira --data-dir ./data
```

That is the whole configuration. It accepts OpenTelemetry on `4317` over gRPC
and on `4318` over HTTP, and `4318` also serves the query API, the browser UI
and the agent tools. Point any OpenTelemetry exporter at it — protobuf or JSON,
plain or gzipped. There is nothing Mira-specific to add to your collector.

```text
--data-dir PATH            where the blocks go        (./mira-data)
--grpc ADDR --http ADDR    listen addresses           (0.0.0.0:4317 / :4318)
--retention DURATION       how long to keep data      (7d; 12h, 30m, 500ms)
--node NAME                this copy's identity       (mira)
--alerts FILE              alert rules; off if unset
--config FILE              KYAML config; flags override it
```

[Configuration](config.md) is every key.

### Fill it

`cargo build --release` skips examples, so name the one you want:

```sh
cargo build --release --bin mira --example loadgen
./target/release/examples/loadgen --demo --for 45m
```

`--demo` writes the four-service shop, backdated across the window `--for`
names. Without it, `loadgen` sends the same deterministic record shape as fast
as it can, to measure how much the write path can take — [what section 3 of the
testing guide measures](internals/e2e.md#3-the-load-harness).

## Read it back

```sh
curl -s localhost:4318/api/v1/query -H 'content-type: application/json' \
  -d '{"signal":"logs","where":[{"attr":"service.name","eq":"checkout"}],"limit":5}'
```

Request bodies are KYAML. JSON is a subset of KYAML, so a JSON client works
unchanged:

```yaml
{
  "signal": "logs",                    # or "traces" / "spans"
  "from": "-15m",
  "to": "now",
  "where": [
    { "attr": "service.name", "eq": "checkout" },
    { "field": "severity_number", "gte": 17 },
    { "attr": "http.route", "contains": "/api" },
  ],
  "limit": 100,
}
```

You do not have to learn the whole shape first. A top-level key the endpoint
does not implement is refused by name, and the error lists the keys that do
exist — a `400` on the query API, a `200` carrying `isError: true` on `/mcp`.

Every response also carries a `stats` object saying what the query cost:
`{"blocks_total":1,"blocks_scanned":1,"rows_scanned":16005,"rows_matched":3840,"elapsed_us":31255}`.
`blocks_scanned` counts the blocks the scan read. A response that filled
`limit` carries `next`; send it back as `after` for the following page.

| endpoint | what it answers |
| --- | --- |
| `POST /api/v1/query` | log records and spans, with events and links |
| `POST /api/v1/metrics/query` | metric series, with the exemplars naming the traces behind them |
| `POST /api/v1/metrics/names` | which metric names exist; an empty body means "right now" |
| `POST /api/v1/correlate` | the frame around a filter — time extent, traces, services |
| `POST /api/v1/map` | the service map, computed on read |
| `POST /api/v1/entities` | the entity keys a window holds |
| `GET /api/v1/alerts` | rule states; an empty list means alerting is off here |
| `GET /api/v1/stats` | uptime, peak RSS, disk headroom, per-signal rows and bytes |
| `GET /health` | liveness — a constant 200, with per-signal `shed` and `failed` |
| `GET /readyz` | readiness — 200 while exports can be made durable, 503 once they cannot |

Every one of those, with its request and response shape in full, is the
[HTTP API reference](reference/http.md) — generated from the source, so it
cannot drift from what the binary serves.

An `attr` filter matches an attribute on the span itself *or* on one of its
events or links. So the demo's exceptions are one query away, and come back
with the event attached:

```sh
curl -s localhost:4318/api/v1/query -H 'content-type: application/json' \
  -d '{"signal":"traces","where":[{"attr":"exception.type","eq":"payments.CardDeclined"}],"limit":3}'
```

## The other three surfaces

=== "Browser UI"

    ```sh
    open http://localhost:4318/
    ```

    Records, trace waterfalls, metric charts, the service map, alert rules and
    a live tail. The whole UI is inside the binary; there is nothing to serve
    separately.

=== "Terminal UI"

    ```sh
    mira mira --data-dir ./data        # read a directory of blocks directly
    mira mira --addr localhost:4318    # or a running copy
    ```

    The same views, drawn straight to the terminal with ratatui. The
    `--data-dir` form needs no server: a detached volume, or the disk of a pod
    that has died, is still readable with nothing running.

=== "MCP"

    ```sh
    curl -s -X POST localhost:4318/mcp -H 'content-type: application/json' \
      -d '{"jsonrpc":"2.0","id":1,"method":"tools/list"}'
    ```

    Nine tools for an agent, over the Model Context Protocol (MCP), which
    speaks JSON-RPC — `query_records`, `get_trace`, `query_metric`,
    `list_metrics`, `correlate`, `service_map`, `list_services`, `list_alerts`,
    and `render_rca` for the write-up at the end. There is no session to set
    up, so any copy can answer any call.

## Next

- [Connect an agent](agents.md) — the MCP wiring, the nine tools, and one
  investigation worked end to end, up to the RCA it writes.
- [Configuration](config.md) — the fourteen keys, and how much machine to give it.
- [End-to-end testing](internals/e2e.md) — a live binary, `telemetrygen`, and a stock
  Collector in front of it in Docker.
- [The load harness](internals/e2e.md#3-the-load-harness) — `loadgen` without
  `--demo`, scoring ingest throughput, query latency, resident set and bytes per
  record in one run.
