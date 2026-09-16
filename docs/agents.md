---
description: Point an agent at Mira over MCP — client configuration, the eight tools, and a worked root-cause investigation from a firing alert to the failing dependency.
---

# Connect an agent

**For:** anyone wiring an LLM agent to their telemetry — Claude Code, Cursor, or
something they wrote themselves.

Mira speaks the [Model Context Protocol](https://modelcontextprotocol.io) natively on
`POST /mcp`, on the same port as the UI and the query API. There is no gateway to run, no
exporter to configure and no second read path: the eight tools are the eight questions
the browser UI asks, over the same code.

Everything below assumes a Mira with data in it. `make demo` gives you one in a single
command — 45 minutes of a four-service shop, one of which is failing.

## Wire it up

=== "Claude Code"

    ```sh
    claude mcp add --transport http mira http://localhost:4318/mcp
    ```

=== "`.mcp.json`"

    ```json
    {
      "mcpServers": {
        "mira": { "type": "http", "url": "http://localhost:4318/mcp" }
      }
    }
    ```

=== "Any client"

    Streamable HTTP, protocol revision `2025-06-18`, JSON-RPC 2.0. One endpoint,
    `POST /mcp`, `content-type: application/json`.

    ```sh
    curl -s -X POST localhost:4318/mcp -H 'content-type: application/json' \
      -d '{"jsonrpc":"2.0","id":1,"method":"tools/list"}'
    ```

**No session id.** Streamable HTTP lets a server issue an `Mcp-Session-Id` and require it
on every later request; Mira issues none. Every request carries everything it needs, so
any replica can answer any call, a load balancer needs no affinity, and killing a replica
mid-conversation loses nothing.

**No authentication either** — see the [security policy](security.md).
Bind it to localhost for a local agent, and put a proxy or a network policy in front of
anything else. `--http 127.0.0.1:4318` is the whole of the local case.

## The eight tools

| tool | answers |
| --- | --- |
| `query_records` | search logs or spans, newest first, attributes merged in |
| `get_trace` | every span of one trace id, over all of retention |
| `query_metric` | one metric as time series, grouped by attribute set |
| `list_metrics` | which metric names exist in a window, with unit and kind |
| `correlate` | the frame around a match: window, traces, services |
| `service_map` | who calls whom, with call, error and latency counts per edge |
| `list_services` | which services emitted anything, and their entity keys |
| `list_alerts` | every rule this node evaluates, and what it is doing now |

Three properties matter more than the list:

- **Arguments are validated, not guessed.** A top-level key the tool does not implement
  is refused *by name*. `{"signal":"logs","filters":[…]}` comes back as an error naming
  `filters`, not as a confident answer over the unfiltered window.
- **A tool that cannot answer returns `isError` inside a 200.** "No rows for that service"
  and "malformed request" are different facts, and a model that receives both as a
  transport failure learns nothing from either.
- **Every field of every answer is an input to another call.** A trace id goes to
  `get_trace`, a service name goes to `query_records` as `{attr: service.name, eq: …}`,
  and `from`/`to` go to anything. `truncated: true` means the answer is a sample and the
  model should narrow rather than conclude.

Every response that scans blocks also carries `stats` — `blocks_total`,
`blocks_scanned`, `rows_scanned`, `rows_matched`, `elapsed_us`. That is the agent's own
cost signal: `rows_matched` above `limit` means it is looking at a page, and
`blocks_scanned` well below `blocks_total` means the pruning did its job. `list_alerts`
is the one tool without it, because it reads in-memory rule state and scans nothing.

## A root-cause investigation, end to end

The wiring above, a Mira holding `make demo`, and one prompt. No system prompt, no tool
list in the question, nothing else in the project directory.

```console
$ claude -p "Checkout is throwing 503s. Use the mira MCP server to find the root
  cause: start from whatever it is alerting on, then dig until you can name the
  failing operation and the exception behind it. Show me the trace id you used."
```

Seven calls and 136 ms of query time, inside 40 s of wall clock — the rest is the model
reading. Below is that session, abridged to the calls and what it said between them.

### 1. Start where the alert is

```json
{"name":"list_alerts","arguments":{}}
```

```json
{"name":"card-declines","state":"firing","severity":"warning","metric":"count",
 "op":">","threshold":20,"value":30,"matched":30,"signal":"traces",
 "filter":"attr:exception.type=payments.CardDeclined",
 "link":"http://localhost:4318/#/traces?q=attr%3Aexception.type%3Dpayments.CardDeclined&range=-300s"}
```

> Two alerts of interest: `shop-error-rate` (pending, 5.4% error) and `card-declines`
> (firing). Digging into the error spans.

`state` is `ok`, `pending` (breaching, but not yet for `for_nano`) or `firing`, and
`filter` is the rule's own query — the search the threshold was counting, so the model
copies it instead of inventing one. `link` opens the same records in the UI, which is
what to hand a human at the end.

### 2. Two questions in one turn

```json
{"name":"query_records","arguments":{"signal":"traces","from":"-15m",
  "where":[{"field":"status_code","eq":2}],"limit":40}}
{"name":"service_map","arguments":{"from":"-15m"}}
```

```json
{"nodes":[{"name":"frontend","spans":3669,"errors":188},
          {"name":"checkout","spans":3669,"errors":188},
          {"name":"payments","spans":1223,"errors":94},
          {"name":"inventory","spans":1223,"errors":0}]}
```

Both at once, because no call depends on a session the other opened. `field` is a column
of the record — `status_code`, `severity_number`, `duration_nano`; `attr` searches the
record, resource and scope levels together, so the model need not know where the SDK put
`service.name`. Errors run down one path and stop: `inventory` takes the same 1,223 calls
with none.

### 3. The failing hop, and the exception under it

```json
{"name":"get_trace","arguments":{"trace_id":"0000000000000efb5555555555555bae"}}
```

```json
{"name":"POST /authorize","kind":2,"status_code":2,
 "status_message":"authorization upstream returned 503",
 "attributes":{"k8s.pod.name":"payments-5d9f7c-0","service.version":"2.7.0"},
 "events":[{"name":"exception","attributes":{
   "exception.type":"payments.CardDeclined","exception.escaped":true,
   "exception.message":"issuer declined authorization: insufficient_funds",
   "exception.stacktrace":"payments/authorize.go:118 Authorize\npayments/handler.go:64  (*Server).Pay"}}]}
```

Eight spans in 6.4 ms, `parent_span_id` linking them into a tree. `get_trace` rather than
`query_records` with a trace-id filter: blocks carry a trace-id index and this is the call
that uses it — 4.7 ms against 27.1M spans in [the query benchmark](market.md#query),
because the cost is the blocks the Bloom sidecar could not rule out.

### 4. A wrong turn, and the field that corrects it

```json
{"name":"correlate","arguments":{"signal":"logs","from":"-15m",
  "where":[{"attr":"service.name","eq":"payments"}],"expand":["traces","peers"]}}
```

```json
{"frame":{"entities":6,"traces":1000,"truncated":true},
 "stats":{"blocks_scanned":3,"rows_scanned":77349,"rows_matched":17214,"elapsed_us":108646}}
```

> Correlate was too broad. Narrowing to payments error logs.

Every payments log, not the failing ones: 17,214 rows and a frame of a thousand traces.
`truncated: true` says that is a sample rather than the answer, and the model narrowed on
it — which is the whole reason the field is there.

### 5. The service's own words, then the falsification

```json
{"name":"query_records","arguments":{"signal":"logs","from":"-15m","limit":5,
  "where":[{"attr":"service.name","eq":"payments"},{"field":"severity_number","gte":17}]}}

{"name":"query_records","arguments":{"signal":"traces","from":"-15m","limit":5,
  "where":[{"attr":"service.name","eq":"payments"},{"field":"status_code","eq":2},
           {"attr":"exception.type","ne":"payments.CardDeclined"}]}}
```

93 error logs in 1.0 ms, and then zero rows in 3.2 ms: no payments error carries any other
exception type. One failure mode, established by asking for the counter-example rather
than by reading five rows that agreed.

### The answer

```text
Trace id: 0000000000000efb5555555555555bae

Failing operation: POST /authorize, the server span in payments
(payments/authorize.go:118 Authorize).
Exception: payments.CardDeclined — "issuer declined authorization:
insufficient_funds", exception.escaped: true.

A card decline is a business outcome, not a server fault. payments maps it to
HTTP 503, and that status propagates unchanged all the way out. Two amplifiers
riding on that mislabel:

  - checkout's client span carries a retry event, retry.reason: "503" — it
    retries a decline that will never succeed.
  - The frontend /checkout span carries a link {link.kind: "retry_of"} to trace
    0000000000000efa5555555555555baf — the whole checkout gets replayed too.

Fix is in payments' error mapping, not in checkout: return 402 for CardDeclined
so the retry layers stop treating it as retryable.
```

Nothing in the prompt named a service, a tool or a field. The retry event and the span
link are OTLP structures Mira stores rather than flattens, which is why they were still
there to be read.

## Waking the loop from an alert

Mira's alerting is static KYAML rules evaluated in-process
([Configuration](config.md)). A `json` target POSTs the rule's whole state, which is
enough to start an investigation without a first query:

```json
{"rule":"card-declines","state":"firing","severity":"warning",
 "summary":"card-declines firing: count 32 > 20 over 5m",
 "value":32,"threshold":20,"matched":32,"total":null,
 "over_nano":"300000000000","at":"1789063264789204000",
 "link":"http://localhost:4318/#/traces?q=…&range=-300s"}
```

```yaml title="alerts.kyaml"
notify: [
  { name: agent, url: "http://127.0.0.1:9000/incident", format: json },
]
```

`state` is `firing` or `resolved`, so the same endpoint closes the loop it opened. There
is no retry: a webhook that is down stays down for longer than the evaluation period, and
the state machine re-pages on the next transition anyway.

## No server at all

An agent running beside the data does not need the protocol. The block directory is the
whole of Mira's state, blocks are immutable once published, and the reader is a library —
so a second process can map the same directory read-only while the writer keeps writing:

```sh
mira mira --data-dir ./data     # the same views, no server, no port, no serialisation
```

That is [architecture section 8.4](architecture/read-surfaces.md#84-the-in-process-read-path-and-why-a-local-agent-gets-it-for-free),
and it is why the co-located case is a different primitive from a managed backend rather
than a cheaper one. Sandbox, edge node or the pod next door: if the agent can see the
directory, it can read the memory.
