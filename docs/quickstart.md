---
description: Run Mira, fill it with the built-in load generator, and read it back over the query API, the browser UI, the terminal UI and MCP.
---

# Quickstart

```sh
mira --data-dir ./data
```

That is the whole configuration. It listens on OTLP/gRPC `4317` and OTLP/HTTP
`4318`, and `4318` also serves the query API, MCP and the UI. Point any OTLP
exporter at it — protobuf or JSON, plain or gzipped. There is no Mira-specific
collector component, and none is needed.

```
--data-dir PATH            where blocks go            (./mira-data)
--grpc ADDR --http ADDR    listen addresses           (0.0.0.0:4317 / :4318)
--retention DURATION       TTL: 7d, 12h, 30m, 500ms   (7d)
--node NAME                this replica's identity    (mira)
--config FILE              KYAML; flags override it
```

Everything a flag sets, the config file sets too, with `${env:VAR,default}`
interpolation — [Configuration](CONFIG.md) is the whole surface.

## Fill it

The load generator is an example in the workspace, so there is nothing else to
install:

```sh
cargo run --release --example loadgen -- --for 10s --conns 8
```

Blocks seal on size or age, so wait a couple of seconds after the last export.
The server logs `block published` when one lands, and an export is only
acknowledged once its block is fsynced and renamed into place — so once you have
an ack, the data is queryable.

## Read it back

```sh
curl -s localhost:4318/api/v1/query -H 'content-type: application/json' \
  -d '{"signal":"logs","where":[{"attr":"service.name","eq":"checkout"}],"limit":5}'
```

A query document is **checked, not guessed**. A top-level key the endpoint does
not implement is refused by name, with the keys that do exist listed, so
`{"signal":"logs","filters":[…]}` never comes back as an answer over the
unfiltered window. On the query API that refusal is a `400`; on `/mcp` it is a
`200` carrying `isError: true`, which is how the protocol says a tool fails.

The bodies are KYAML, and JSON is a subset of it, so a JSON client works
unchanged:

```yaml
{
  "signal": "logs",                    # or "traces"
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

Every response carries a `stats` object —
`{"blocks_total":2,"blocks_scanned":1,…}` — which is the sidecar pruning, made
visible. A response that filled `limit` also carries `next`; pass it back as
`after` for the following page, and paging visits every row exactly once.

| endpoint | what it answers |
|---|---|
| `POST /api/v1/query` | log records and spans, with events and links |
| `POST /api/v1/metrics/query` | metric series, with the exemplars naming the traces behind them |
| `POST /api/v1/metrics/names` | which metric names exist; an empty body means "right now" |
| `GET /health`, `GET /readyz` | the same 200 and the same JSON body, per-signal `shed` and `failed` counts |

## The other three surfaces

Same read path, same questions.

=== "Browser UI"

    ```sh
    open http://localhost:4318/
    ```

    Records, trace waterfalls and metric charts, served out of the binary by
    `include_bytes!`. Nothing to deploy beside it.

=== "Terminal UI"

    ```sh
    mira mira --data-dir ./data        # read a block directory in-process
    mira mira --addr localhost:4318    # or a running replica
    ```

    The same three tabs, the same filter grammar and the same trace waterfall,
    over `termios` raw mode and ANSI — no TUI framework, zero crates added. The
    `--data-dir` form is the point: a detached PVC or a dead pod's volume is
    still readable with nothing running.

=== "MCP"

    ```sh
    curl -s -X POST localhost:4318/mcp -H 'content-type: application/json' \
      -d '{"jsonrpc":"2.0","id":1,"method":"tools/list"}'
    ```

    Four tools over JSON-RPC — `query_records`, `get_trace`, `query_metric`,
    `list_metrics` — with no session id, so any replica can answer any call.

## Next

- [Configuration](CONFIG.md) — the six keys, and why there is no block size among
  them.
- [End-to-end testing](TESTING.md) — a live binary, `telemetrygen`, and a stock
  Collector in front of it in Docker, which is the only test where the client is
  not ours.
- [The load harness](TESTING.md#3-the-load-harness-all-four-axes-one-command) —
  `loadgen` again, scoring ingest throughput, query latency, resident set and
  bytes per record in one run, because any one of the four is easy to win alone.
