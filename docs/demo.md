---
description: One command, about a minute, and you are reading real data — the errors, the crash behind them, the trace, the service map, a chart, a firing alert, and what the run cost in memory and disk.
---

# See it work

**For:** anyone deciding whether this is worth installing. One command, about a
minute, no Docker and no second terminal.

```sh
make demo
```

It builds the binary, starts it on a scratch directory, invents 45 minutes of
traffic for a four-service shop — 16,005 logs, 30,720 spans, 4,608 metric
points — and waits until the first file of each kind is written. Ctrl-C stops
it; `make demo-clean` deletes the directory, which is the whole uninstall.

Everything below is that run, most of it twice: in the terminal
(`mira mira --addr localhost:4318`) and in the browser
(`http://localhost:4318/`). Both read the same files.

!!! tip "Or click through it now, without installing anything"

    [**Open the recorded UI**](https://miradb.dev/play/) — the same interface
    that ships inside the binary. The UI is live; the data behind it is a
    capture of a running Mira, so typing a filter re-renders captured rows
    instead of reading files. A banner says so.

## 1. Find the errors

`/` opens the filter. The footer says what the query did and how long it took.

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
 1/1 blocks · 16005 rows scanned · 920 matched · 20.4ms
 ↑↓ move  enter detail  t trace  c frame  ·  m map  a alerts  d node  ·  / filter  f follow  ? help  q quit
```

16,005 rows read, 920 matched, **20.4 ms** — read straight out of the file as
it sits on disk. Recent files are stored uncompressed, so nothing is unpacked
to answer the query; older files are compressed to save space.

![The same filter in the browser: severity_text=ERROR in the query box, twenty
ERROR rows from frontend, checkout and payments, and a header reading 920
matched / 16005 scanned, 1 of 1 blocks, 6.7 ms.](assets/ui/logs.png)

Every screen that reads the files carries that line — rows read, rows matched,
files touched, milliseconds — never behind a toggle.

## 2. The row with no service

Nothing in the shop wrote that fourth line. The demo also lays down what the
[cluster-event export](install.md#cluster-context) sends from a real Kubernetes
cluster — pod events and container state, as ordinary log records.

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

One field does all the joining: `k8s.pod.uid`, which the OpenTelemetry
Collector's `k8sattributes` processor stamps on application telemetry once you
add it to the pipeline.

## 3. Follow one to its trace

`t` on one of those 503s. No copying an id, no second tab.

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

Each bar is one call — a *span*, in OpenTelemetry's vocabulary. Things that
happened part-way through one (`● retry`, `● exception`) sit inline where they
happened, and `↗` points at the trace that caused this one.

![The same trace as a browser waterfall: eight nested bars over 76.08ms, the
five failed spans in red and the three successful ones in blue, with amber event
ticks on both /authorize spans.](assets/ui/trace.png)

## 4. See the shape of the system

`m`. The map is worked out from the stored spans when you ask for it.

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

Click a service and you land on its logs.

## 5. A chart, and the requests underneath it

`http.server.request.duration` records a spread of values, so it arrives as two
series: how many requests there were, and how long they took in total. Each
gets its own chart.

![The metrics view: two stacked charts, http.server.request.duration.count
ranging 179 to 716 and .sum ranging 5.8k to 23.4k, twelve pod series each, with
a rug of small coloured exemplar diamonds along both
baselines.](assets/ui/metrics.png)

Each diamond along the baseline is an *exemplar* — one real request the sending
library sampled and tagged with a trace id. Click an exemplar and
you get that trace: from a chart straight to one request behind it.

## 6. Alerting, with nothing else to run

`a`. Rules live in a KYAML file (`--alerts docs/e2e/alerts.kyaml`), and each one
is written in the same filter language the query API takes. `enter` on a firing
rule opens the records that fired it.

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

A rule is a query, run against the same files the screens above read.

## 7. What it cost

`d`. Everything the process knows about itself — nothing to export, nothing to
scrape.

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
```

**72 MiB of memory at peak**, for 51,333 records ingested and 19 queries
served, from a 6.20 MiB binary that also holds the web UI, the terminal UI and
the tools agents call. A dozen runs ranged 53.7 to 71.8 MiB, so read it as
"tens of megabytes" and not a figure that repeats to the decimal.

## 8. Hand it to an agent

One line, and an agent can ask all of the above instead of you:

```sh
claude mcp add --transport http mira http://localhost:4318/mcp
```

Asked why checkout is returning 503s, Claude starts at the alert firing in
section 6, walks the service map to `payments`, opens the trace and names the
exception behind it. The prompt, every call
and the answer: [Connect an agent](agents.md).

The same data over plain HTTP, at `POST /api/v1/query`. No exporter, no API
key, no session to set up. An unknown argument is refused by name, so a model
that invents a field is told so rather than handed plausible rows.

## Next

| | |
| --- | --- |
| [See it on Kubernetes](demo-cluster.md) | the same run, with the operator, a proxy and an ingress on the path |
| [Install](install.md) | one script, a container, or `cargo install` |
| [Quickstart](quickstart.md) | the manual path, and the query API |
| [Connect an agent](agents.md) | the wiring, the nine tools, a worked investigation |
| [Architecture](architecture/index.md) | why it is shaped like this |
