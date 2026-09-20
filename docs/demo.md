---
description: One command, then real output from both surfaces — an error log, the OOM kill behind it, the trace, the service map, a metric with its exemplars, a firing alert, and what the whole thing cost in memory and disk.
---

# See it work

**For:** anyone deciding whether this is worth installing. One command, about a
minute, no Docker and no second terminal.

```sh
make demo
```

It builds the binary, starts it on a scratch directory, generates 45 minutes of
backdated telemetry for a four-service shop — 16,005 logs, 30,720 spans, 4,608
metric points — and waits for the first block of each signal to seal. Ctrl-C
stops it; `make demo-clean` deletes the directory, which is the entire uninstall.

Everything below is that run — most of it twice: the terminal capture from
`mira mira --addr localhost:4318`, then the browser at `http://localhost:4318/`.
One binary, one set of blocks, one query path, read by both rather than copied
into each.

!!! tip "Or click through it now, without installing anything"

    [**Open the recorded UI**](https://miradb.dev/play/) — the same bundle that
    ships inside the binary, answering from responses a real Mira gave over this
    generator. Every part of the interface is live: tables, waterfall, service
    map, chart, alerts, paging, routing. What is recorded is the *query* —
    typing a filter re-renders the captured rows rather than reading blocks,
    because there are no blocks in a browser tab, and a banner says so. The
    timings are the ones the real instance measured at snapshot time.

## 1. Find the errors

`/` opens the filter. The footer is the query plan and the wall clock.

```text
 mira   1 logs    2 traces    3 metrics                                                           http localhost:4318
 filter  severity_text=ERROR                                                                       last 1h  limit 200
 12:32:18.378 ERROR  frontend         POST /checkout failed: upstream returned 503 after 76ms                        █
 12:32:18.374 ERROR  checkout         POST /pay failed: upstream returned 503 after 49ms                             ║
 12:32:18.371 ERROR  payments         POST /authorize failed: upstream returned 503 after 31ms                       ║
 12:32:17.502 ERROR  -                container payments last terminated: exit code 137, signal 9                    ║
 12:32:09.255 ERROR  frontend         POST /checkout failed: upstream returned 503 after 93ms                        ║
 12:32:09.250 ERROR  checkout         POST /pay failed: upstream returned 503 after 60ms                             ║
 12:32:09.246 ERROR  payments         POST /authorize failed: upstream returned 503 after 38ms                       ║
 12:32:00.132 ERROR  frontend         POST /checkout failed: upstream returned 503 after 110ms                       ║
 12:32:00.126 ERROR  checkout         POST /pay failed: upstream returned 503 after 72ms                             ║
 12:32:00.121 ERROR  payments         POST /authorize failed: upstream returned 503 after 45ms                       ║
 12:31:50.953 ERROR  frontend         POST /checkout failed: upstream returned 503 after 72ms                        ║
 12:31:50.949 ERROR  checkout         POST /pay failed: upstream returned 503 after 47ms                             ║
 12:31:50.946 ERROR  payments         POST /authorize failed: upstream returned 503 after 29ms                       ║
── record 1 of 200 ───────────────────────────────────────────────────────────────────────────────────────────────────
  time_unix_nano               2026-09-20 12:32:18.378
  observed_time_unix_nano      2026-09-20 12:32:18.380
  severity_number              17
  severity_text                ERROR
  event_name                   http.server.request
  body                         POST /checkout failed: upstream returned 503 after 76ms
 1/1 blocks · 16005 rows scanned · 920 matched · 20.4ms
 ↑↓ move  enter detail  t trace  c frame  ·  m map  a alerts  d node  ·  / filter  f follow  ? help  q quit
```

16,005 rows scanned to 920 matches in **20.4 ms**, over a block the process never
copied — the Arrow buffers are read where `mmap` put them.

![The same filter in the browser: severity_text=ERROR in the query box, twenty
ERROR rows from frontend, checkout and payments, and a header reading 920
matched / 16005 scanned, 1 of 1 blocks, 6.7 ms.](assets/ui/logs.png)

Same 920, from the same block. The header is the query plan: rows scanned, rows
matched, blocks touched, wall clock — on every screen, never behind a toggle.

## 2. The row with no service

Nothing exported that fourth line. The generator also lays down what the
operator's [cluster-event export](install.md#cluster-context) ships from a real
cluster — Kubernetes Events and container state, as OTLP logs — so the kill
behind the 503s is in the same block, found by the same filter.

```text
 mira   1 logs    2 traces    3 metrics                                                           http localhost:4318
 filter  k8s.event.reason=OOMKilled                                                                last 1h  limit 200
  time_unix_nano               2026-09-20 12:32:17.502
  observed_time_unix_nano      2026-09-20 12:32:19.502
  severity_number              17
  severity_text                ERROR
  event_name                   OOMKilled
  body                         container payments last terminated: exit code 137, signal 9
  flags                        0
  dropped_attributes_count     0
── attributes ────────────────────────────────────────────────────────────────────────────────────────────────────────
  container.image.name         ghcr.io/shop/payments:2.7.0
  k8s.container.name           payments
  k8s.container.restart_count  32
  k8s.event.reason             OOMKilled
  k8s.namespace.name           shop
  k8s.object.kind              Pod
  k8s.pod.host_ip              10.0.3.14
  k8s.pod.name                 payments-5d9f7c-0
  k8s.pod.uid                  e17ac568-5d9f-4c2a-b1e7-c5686623db26
  otel.scope.name              mira-operator
  otel.scope.version           0.3.0
 1/1 blocks · 16005 rows scanned · 32 matched · 10.2ms
```

`k8s.pod.uid` is the whole join, and it is the attribute the k8sattributes
processor already puts on application telemetry: one predicate on that value
returns 1,536 log records and 3,840 spans for `payments-5d9f7c-0`, both halves
of the story on one timeline. Nothing was correlated at write time.

## 3. Follow one to its trace

`t` on one of those 503s. No trace-id copy-paste, no second tab.

```text
 trace 0000000000000efb5555555555555bae  ·  8 spans  ·  76.08ms
──────────────────────────────────────────────────────────────────────────────────────────────────────────────────────
 POST /checkout                         frontend          █████████████████████████████████████████████████   76.08ms
  ↗ trace 0000000000000efa5555555555555baf
  GET /items                            frontend           ███████████                                        17.37ms
   GET /items                           inventory          █████████                                          14.89ms
  POST /pay                             frontend                       █████████████████████████████████      51.27ms
   POST /pay                            checkout                        ███████████████████████████████       49.62ms
    SELECT orders                       checkout                         ██████                                9.92ms
    POST /authorize                     checkout                                 █████████████████████        33.08ms
     ● retry  +47.41ms
     POST /authorize                    payments                                 ████████████████████         31.43ms
      ● exception  +65.50ms
 1/1 blocks · 30720 rows scanned · 8 matched · 5.8ms
 esc → logs  ↑↓ span  enter detail
```

Span events (`● retry`, `● exception`) sit inline at their offset, and `↗` is a
span link to the trace that caused this one. Both are OTLP fields Mira stores
rather than flattens.

![The same trace as a browser waterfall: eight nested bars over 76.08ms, the
five failed spans in red and the three successful ones in blue, with amber event
ticks on both /authorize spans.](assets/ui/trace.png)

Red is `status_code=2`; the amber ticks are those same span events, at the
offset they happened.

## 4. See the shape of the system

`m`. The service map is computed from the spans at read time — there is no
pre-aggregation job, and nothing to be stale.

```text
 map 4 services
──────────────────────────────────────────────────────────────────────────────────────────────────────────────────────
 entry
   frontend                                 11520 spans    592 errors    58.29ms avg
     checkout                               11520 spans    592 errors    37.31ms avg
       payments                              3840 spans    296 errors    37.97ms avg
     inventory                               3840 spans      -           17.99ms avg
 1/1 blocks · 30720 rows scanned · 4 matched · 20.2ms
 esc → logs  ↑↓ move  enter filter on this service
```

![The same graph in the browser: entry feeding frontend, frontend feeding
checkout and inventory, checkout feeding payments, with per-service span and
error counts and red edges where errors flow.](assets/ui/map.png)

Clicking a service takes you to its logs: from "checkout is red" to the lines
that say why, with no second query to compose.

## 5. A metric, and the traces underneath it

`http.server.request.duration` is a histogram, so it arrives as two derived
series, `.count` and `.sum`. A sum thirty times larger than its count flattens
the count onto a shared axis, so each gets its own.

![The metrics view: two stacked charts, http.server.request.duration.count
ranging 179 to 716 and .sum ranging 5.8k to 23.4k, twelve pod series each, with
a rug of coloured exemplar diamonds along both baselines.](assets/ui/metrics.png)

The rug along each baseline is exemplars — one diamond per request the SDK
sampled and stamped with a trace id. Clicking one opens that trace: the
metric-to-trace edge OTLP defines, in one click. It sits on the baseline and not
at its value because an exemplar is one 50ms request and the line above is a
count of seven hundred: they share a time axis and nothing else.

## 6. Alerting, evaluated in-process

`a`. Rules are a KYAML file (`--alerts docs/e2e/alerts.kyaml`); each is the same
filter language the query API takes. `enter` on a firing rule opens the records
that fired it.

```text
 alerts 4 rules  ·  1 firing
──────────────────────────────────────────────────────────────────────────────────────────────────────────────────────
  ○      ok      shop-error-rate             0.00% > 2.00%   0 of 0
          field:status_code=2  ·  over 1m00s
  ○      ok      checkout-p95-latency        0.00% > 5.00%   0 of 834
          attr:service.name=checkout field:duration_nano>250000000  ·  over 5m00s
  ●      firing  card-declines               22 > 20   22 records
          attr:exception.type=payments.CardDeclined  ·  over 5m00s
  ○      ok      inventory-outage            0 >= 1   0 records
          attr:service.name=inventory field:severity_number>=21  ·  over 1m00s
 2.2ms
 esc → logs  ↑↓ move  enter show the records that fired  a reload
```

![The alerts view in the browser: four rules in a table with value, threshold,
matched, window and filter columns; card-declines firing at 21 over a threshold
of 20, the other three green.](assets/ui/alerts.png)

No Alertmanager, no rule-evaluation sidecar, no second store: the rule is the
query, run on the same blocks the screen above reads.

## 7. What it cost

`d`. Everything a node knows about itself — no exporter, no sidecar, no
`/metrics` scrape.

```text
 node up 2m 42s  ·  peak rss 71.8 MiB  ·  disk 14% free
── queries ───────────────────────────────────────────────────────────────────────────────────────────────────────────
  served          19
  mean            11.49 ms
  max             89.54 ms
── logs ──────────────────────────────────────────────────────────────────────────────────────────────────────────────
  rows written    16.0k
  blocks          1 on disk  ·  1 published
  bytes on disk   4.7 MiB  ·  305 B/row
  rejected        0 shed  ·  0 failed  ·  0 refused
── traces ────────────────────────────────────────────────────────────────────────────────────────────────────────────
  rows written    30.7k
  blocks          1 on disk  ·  1 published
  bytes on disk   9.1 MiB  ·  310 B/row
  rejected        0 shed  ·  0 failed  ·  0 refused
── metrics ───────────────────────────────────────────────────────────────────────────────────────────────────────────
  rows written    4608
  blocks          1 on disk  ·  1 published
  bytes on disk   896.4 KiB  ·  199 B/row
  rejected        0 shed  ·  0 failed  ·  0 refused
```

**72 MiB peak resident** for 51,333 records ingested and 19 queries served, in a
6.20 MiB binary that also contains the web UI, the terminal UI and the MCP
server. That peak is a transient: `--demo` delivers the whole 45-minute window
in one burst at over a million records a second, and the process settles back
under 9 MiB once the blocks are sealed. A dozen runs spanned 53.7 to 71.8 MiB —
read it as "tens of megabytes", not a number that reproduces to a decimal place.
The 90 ms maximum is a cold read: the query that first touches a block pays the
page faults, and the ones behind it come from the page cache, at the 11 ms
mean.

## 8. Hand it to an agent

One line, and the blocks above answer an LLM instead of you:

```sh
claude mcp add --transport http mira http://localhost:4318/mcp
```

Asked why checkout is returning 503s, Claude starts at the alert that is firing
in section 6, walks the service map to `payments`, opens the trace and names the
exception behind it — seven calls, 136 ms of query time. The prompt, every call
and the answer: [Connect an agent](agents.md).

The same read path over JSON-RPC. No exporter, no API key, no session id — so
any replica can answer any call.

```sh
curl -s localhost:4318/mcp -H 'content-type: application/json' -d '{
  "jsonrpc": "2.0", "id": 1, "method": "tools/call",
  "params": { "name": "query_records", "arguments": {
    "signal": "traces",
    "where": [ { "attr": "exception.type", "eq": "payments.CardDeclined" } ],
    "limit": 1 } } }' | jq -r '.result.content[0].text'
```

MCP is JSON-RPC, so the 200 body is
`{"jsonrpc":"2.0","id":1,"result":{"content":[{"type":"text","text":"…"}],"isError":false}}`
and the answer is the JSON string in `text` — which is why the `jq` is there.
The same object comes back unwrapped from `POST /api/v1/query`.

```json
{
  "rows": [
    {
      "trace_id": "0000000000000efb5555555555555bae",
      "span_id": "e44c317088164e03",
      "name": "POST /authorize",
      "duration_nano": "31426000",
      "status_code": 2,
      "status_message": "authorization upstream returned 503",
      "attributes": {
        "service.name": "payments",
        "k8s.pod.name": "payments-5d9f7c-0",
        "http.response.status_code": "503"
      },
      "events": [
        {
          "name": "exception",
          "attributes": {
            "exception.type": "payments.CardDeclined",
            "exception.message": "issuer declined authorization: insufficient_funds",
            "exception.stacktrace": "payments/authorize.go:118 Authorize\npayments/handler.go:64  (*Server).Pay\n..."
          }
        }
      ]
    }
  ],
  "stats": {
    "blocks_total": 1, "blocks_scanned": 1,
    "rows_scanned": 30720, "rows_matched": 296, "elapsed_us": 2063
  }
}
```

That is the trace the waterfall opened, from the other direction, and `stats`
tells the model what the answer cost, so it can widen or narrow the next question
instead of guessing. An unknown argument is refused by name, so a model that
invents a field is told so instead of handed plausible rows. Wiring, the other
eight tools and a worked investigation: [Connect an agent](agents.md).

## Next

- [See it on Kubernetes](demo-cluster.md) — the same run with the operator,
  a proxy and an ingress on the path.
- [Install](install.md) — one script, or a container, or `cargo install`.
- [Quickstart](quickstart.md) — the manual path, and the query API.
- [Architecture](architecture/index.md) — why it is shaped like this.
