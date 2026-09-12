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
any replica can answer any call and a load balancer needs no affinity. Killing a replica
mid-conversation loses nothing.

**No authentication either** — see [SECURITY.md](https://github.com/TrianaLab/mira/blob/main/SECURITY.md).
Bind it to localhost for a local agent, and put a proxy or a network policy in front of
anything else. `--http 127.0.0.1:4318` is the whole of the local case.

## The eight tools

| tool | answers |
|---|---|
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

Every response also carries `stats` — `blocks_total`, `blocks_scanned`, `rows_scanned`,
`rows_matched`, `elapsed_us`. That is the agent's own cost signal: `rows_matched` above
`limit` means it is looking at a page, and `blocks_scanned` well below `blocks_total`
means the pruning did its job.

## A root-cause investigation, end to end

Real output from `make demo`, trimmed to the fields that carry the argument. Four calls,
57 ms of query time between them, and the answer is the last one.

**1. What is wrong?** The loop starts here whether it was woken by a schedule or by a
webhook.

```json
{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"list_alerts"}}
```

```json
{"name":"card-declines","state":"firing","severity":"warning",
 "metric":"count","op":">","threshold":20,"value":32,"matched":32,
 "signal":"traces","filter":"attr:exception.type=payments.CardDeclined",
 "link":"http://localhost:4318/#/traces?q=attr%3Aexception.type%3Dpayments.CardDeclined&range=-300s"}
```

`state` is one of `ok`, `pending` (breaching, but has not held for `for_nano` yet) and
`firing`. `filter` is the rule's own query, so the next call is a copy of it — the model
never has to invent the search that the threshold was counting. `link` opens the same
records in the UI, which is what to hand a human at the end.

**2. What was the blast radius?** `correlate` is the call that replaces "query, read a
trace id out of the result, query again".

```json
{"name":"correlate","arguments":{
  "signal":"traces","from":"-15m",
  "where":[{"attr":"exception.type","eq":"payments.CardDeclined"}],
  "expand":["traces","peers"]}}
```

```json
{"frame":{"from":"…404518849000","to":"…304518849000",
  "entities":[{"name":"payments"},{"name":"frontend"},{"name":"inventory"},{"name":"checkout"}],
  "traces":["0000000000000a4f5555555555555f1a", … 93 total],
  "truncated":false},
 "stats":{"blocks_scanned":3,"rows_scanned":92160,"rows_matched":1581,"elapsed_us":10247}}
```

`expand` is an ordered walk and the order is load-bearing. `traces` first, because a log
line is written *after* the request it describes, so the true window is wider than the
one the match fell in; `peers` then adds every service that appears in those traces.
`truncated: false` says the 93 traces are all of them, not a sample.

**3. Which hop actually failed?** Any trace from the frame, expanded in full.

```json
{"name":"get_trace","arguments":{"trace_id":"0000000000000a4f5555555555555f1a"}}
```

```json
{"name":"POST /authorize","kind":2,"status_code":2,
 "status_message":"authorization upstream returned 503",
 "duration_nano":"45714000","parent_span_id":"9349cdcc0910b02f", …}
```

Eight spans, `parent_span_id` linking them into the tree. The 503 originates at
`payments`' own `POST /authorize` and propagates up through `POST /pay` to the frontend —
so `payments` is where the error is *created*, not merely where it is reported. `get_trace`
rather than `query_records` with a `trace_id` filter: blocks carry a trace-id index, and
this is the call that uses it — 4.6 ms here, and 4.7 ms against 27.1M spans in
[the query benchmark](market.md#query), opening 2 blocks of 155, because the cost is
the blocks the Bloom sidecar could not rule out.

**4. What does the service itself say?** Logs are indexed on the same attributes, so this
is one call, not a jump to another system.

```json
{"name":"query_records","arguments":{
  "signal":"logs","from":"-15m",
  "where":[{"attr":"service.name","eq":"payments"},{"field":"severity_number","gte":17}],
  "limit":3}}
```

```json
{"severity_text":"ERROR","body":"POST /authorize failed: upstream returned 503 after 31ms",
 "trace_id":"0000000000000efb5555555555555bae",
 "attributes":{"http.response.status_code":"503","http.route":"/authorize",
   "k8s.pod.name":"payments-5d9f7c-1","service.instance.id":"payments-1", …}}
```

`attr` searches the record, resource and scope levels at once, so the model does not need
to know where the SDK put `service.name`. `field` is for columns of the record itself
(`severity_number`, `body`, `duration_nano`, `status_code`). The pod name and instance id
come back merged into `attributes`, which is the identifier an Act step needs.

**Conclusion, with the evidence attached**: `payments` is returning 503 from its
authorization upstream; 93 traces in 15 minutes, all four services touched, blast radius
contained to the checkout path. `service_map` is the fifth call if the loop needs the
edge that is degrading rather than the service that is erroring — it returns per-edge
call, error and latency counts computed from `parent_span_id` at read time.

!!! tip "Give the model the shape of the loop, not the schema"

    The tool descriptions already carry the schema, and they are written to be read by a
    model rather than by someone who knows it. What is worth putting in a system prompt
    is the sequence: **alerts or a symptom → `correlate` for the frame → `get_trace` for
    the failing hop → `query_records` for the service's own words**. Two sentences, and it
    saves the model rediscovering the order on every incident.

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

That is [architecture section 8.4](architecture.md#84-the-in-process-read-path-and-why-a-local-agent-gets-it-for-free),
and it is the reason the co-located
case is a different primitive from a managed backend rather than a cheaper one. Sandbox,
edge node or the pod next door: if the agent can see the directory, it can read the
memory.
