---
description: One command from a fresh clone to a UI full of realistic telemetry, then the manual path — run Mira, fill it, and read it back over the query API, the browser UI, the terminal UI and MCP.
---

# Quickstart

**For:** anyone who has Mira and wants data in it. If you have not seen it yet,
[See it work](demo.md) is the shorter route.

## One command

```sh
make demo
```

Builds the binary and the generator, starts Mira on a scratch directory, lays
down 45 minutes of backdated telemetry for a four-service shop, waits for the
first block of each signal to seal, and prints where to look. Ctrl-C stops it;
`make demo-clean` deletes the directory. No Docker, no second terminal, nothing
to install beyond a Rust toolchain. [See it work](demo.md) is what comes out.

The rest of this page is the manual path — your own data, and the query API.

## Run it

```sh
mira --data-dir ./data
```

That is the whole configuration. OTLP/gRPC on `4317`, OTLP/HTTP on `4318`, and
`4318` also serves the query API, MCP and the UI. Point any OTLP exporter at it
— protobuf or JSON, plain or gzipped. There is no Mira-specific collector
component.

```
--data-dir PATH            where blocks go            (./mira-data)
--grpc ADDR --http ADDR    listen addresses           (0.0.0.0:4317 / :4318)
--retention DURATION       TTL: 7d, 12h, 30m, 500ms   (7d)
--node NAME                this replica's identity    (mira)
--alerts FILE              alert rules; off if unset
--config FILE              KYAML; flags override it
```

[Configuration](config.md) is every key.

### Fill it

`cargo build --release` does not build examples, so name it:

```sh
cargo build --release --bin mira --example loadgen
./target/release/examples/loadgen --demo --for 45m
```

`--demo` is the four-service shop, backdated over the window `--for` names.
Without it `loadgen` is the load harness — a flat deterministic firehose aimed
at the ingest path, which is [what section 3 of the testing guide
measures](testing.md#3-the-load-harness).

## Read it back

```sh
curl -s localhost:4318/api/v1/query -H 'content-type: application/json' \
  -d '{"signal":"logs","where":[{"attr":"service.name","eq":"checkout"}],"limit":5}'
```

Bodies are KYAML, and JSON is a subset of it, so a JSON client works unchanged:

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

A top-level key the endpoint does not implement is refused by name with the keys
that do exist listed — a `400` on the query API, a `200` carrying
`isError: true` on `/mcp`. Every response carries a `stats` object
(`{"blocks_total":1,"blocks_scanned":1,"rows_scanned":15909,"rows_matched":3840,"elapsed_us":102021}`),
which is the sidecar pruning made visible. A response that filled `limit` also
carries `next`; pass it back as `after` for the following page.

| endpoint | what it answers |
|---|---|
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

An `attr` term matches a span's own attributes *or* an attribute on one of its
events or links, so the demo's exceptions are one query away and come back with
the event attached:

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
    a live tail, served out of the binary by `include_bytes!`.

=== "Terminal UI"

    ```sh
    mira mira --data-dir ./data        # read a block directory in-process
    mira mira --addr localhost:4318    # or a running replica
    ```

    The same views over `termios` raw mode and ANSI — no TUI framework, zero
    crates added. The `--data-dir` form needs no server: a detached PVC or a
    dead pod's volume is still readable with nothing running.

=== "MCP"

    ```sh
    curl -s -X POST localhost:4318/mcp -H 'content-type: application/json' \
      -d '{"jsonrpc":"2.0","id":1,"method":"tools/list"}'
    ```

    Eight tools over JSON-RPC — `query_records`, `get_trace`, `query_metric`,
    `list_metrics`, `correlate`, `service_map`, `list_services`, `list_alerts`
    — with no session id, so any replica can answer any call.

## Next

- [Connect an agent](agents.md) — the MCP wiring, the eight tools, and one
  investigation worked end to end.
- [Configuration](config.md) — the eleven keys, and how much machine to give it.
- [End-to-end testing](testing.md) — a live binary, `telemetrygen`, and a stock
  Collector in front of it in Docker.
- [The load harness](testing.md#3-the-load-harness) — `loadgen` without
  `--demo`, scoring ingest throughput, query latency, resident set and bytes per
  record in one run.
