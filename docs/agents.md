---
# `4.7 ms` and `27.1M` in "The failing hop" are pinned in measurements.kyaml —
# grep this path there before touching them or the words around them.
description: Point an AI agent at your logs, traces and metrics. How to connect one, the nine things it can ask for, and a worked investigation that ends in a written root-cause report with every piece of cited evidence re-run.
---

# Connect an agent

**For:** anyone pointing an LLM agent at their telemetry — Claude Code, Cursor,
or their own.

Give an agent one URL and it can search your logs and spans, pull a whole trace,
read a metric, see which service calls which, and check what your alerts are
doing. At the end it can write the incident up, and Mira re-runs every piece of
cited evidence in that write-up against the stored data before it hands it back.

Mira serves these as [Model Context Protocol](https://modelcontextprotocol.io)
tools on `POST /mcp`, the same port as the UI and the query API. Nothing else to
install or run.

Everything below assumes a Mira with data in it. `make demo` gives you one in a
single command: 45 minutes of a four-service shop, one of which is failing.

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

**Nothing to set up first.** Streamable HTTP lets a server issue an
`Mcp-Session-Id` and require it on every later request; Mira issues none. Every
request carries all it needs, so any copy of Mira can answer any call, a load
balancer needs no stickiness, and killing one mid-conversation loses nothing.

**No authentication** — see the [security policy](security.md). Bind it to
localhost for a local agent and put a proxy or a network policy in front of
anything else. `--http 127.0.0.1:4318` is the whole of the local case.

## The nine tools

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
| `render_rca` | the findings, as a root-cause analysis in markdown ([section 15](architecture/rca.md)) |

Eight of them are the questions the browser UI asks, over the same code. The
ninth writes the report.

None of them changes anything outside Mira, and there is no tenth that restarts
a pod or edits a cluster. That is a decision, not a gap:
[architecture section 14.6](architecture/kubernetes-context.md#146-read-only-and-not-by-omission).

With the operator's [cluster-event export](install.md#cluster-context) on, an
`OOMKilled` arrives as an ordinary log record carrying the dead pod's
`k8s.pod.uid` — so `query_records` finds it, beside the spans that stopped.

Three things matter more than the list:

| | |
| --- | --- |
| **A wrong argument is refused by name** | Send a key a tool does not support and the error names it. You never get a confident answer over the wrong window instead. |
| **"Nothing matched" and "bad request" stay apart** | A tool that cannot answer says so with `isError` in the body of a `200`. A model that saw both as a network failure would learn nothing from either. |
| **Every field you get back is an input to the next call** | Trace ids go to `get_trace`, a service name goes to `query_records` as `{attr: service.name, eq: …}`, and `from`/`to` go anywhere. `truncated: true` means you have a sample, so narrow rather than conclude. |

Every answer that reads files also carries `stats`: files on disk
(`blocks_total`), files opened (`blocks_scanned`), rows read (`rows_scanned`),
rows matched (`rows_matched`), microseconds (`elapsed_us`). That is the agent's own cost signal, and
how it knows it is on the first page of something much bigger. `list_alerts` is
the exception — it reads rule state from memory and opens nothing.

## A root-cause investigation, end to end

The wiring above, a Mira holding `make demo`, and one prompt. No system prompt,
no tool list in the question, nothing else in the project directory.

```console
$ claude -p "Checkout is throwing 503s. Use the mira MCP server to find the root
  cause: start from whatever it is alerting on, then dig until you can name the
  failing operation and the exception behind it. Show me the trace id you used."
```

Seven calls and 136 ms of query time, inside 40 s of wall clock — the rest is
the model reading. Here is that session, cut down to the calls.

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

`state` is `ok`, `pending` (over the line, but not for long enough yet) or
`firing`. `filter` is the rule's own search — the one the threshold was
counting, so the model copies it instead of inventing one. `link` opens the same
records in the UI, which is what to hand a human at the end.

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

Both at once, because neither call depends on the other. `field` is a column of
the record — `status_code`, `severity_number`, `duration_nano`. `attr` searches
every level at once, so the model need not know which level `service.name` was
set at. The errors run down one path and stop: `inventory` takes the
same 1,223 calls with none.

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

Eight spans in 6.4 ms, each naming its parent, so they arrive as a tree.
`get_trace` rather than a trace-id filter on `query_records`: every file carries
a trace-id index and this is the call that uses it —
4.7 ms against 27.1M spans in [the query benchmark](market.md#query), because
almost every file can be skipped without opening it.

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

That asked for every payments log, not the failing ones: 17,214 rows and a
thousand traces around them. `truncated: true` said it was a sample rather than
the answer, and the model narrowed on it. That is the whole reason the field is
there.

### 5. The service's own words, then the falsification

```json
{"name":"query_records","arguments":{"signal":"logs","from":"-15m","limit":5,
  "where":[{"attr":"service.name","eq":"payments"},{"field":"severity_number","gte":17}]}}

{"name":"query_records","arguments":{"signal":"traces","from":"-15m","limit":5,
  "where":[{"attr":"service.name","eq":"payments"},{"field":"status_code","eq":2},
           {"attr":"exception.type","ne":"payments.CardDeclined"}]}}
```

93 error logs in 1.0 ms, then zero rows in 3.2 ms: no payments error carries any
other exception type. One failure mode, settled by asking for the
counter-example instead of reading five rows that agreed.

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

Nothing in the prompt named a service, a tool or a field. The retry event and
the span link survived because Mira stores what your service sent, whole, rather than
flattening it into rows.

## The write-up

So far the findings only exist in the model's context. `render_rca` turns them
into a document, and first re-runs every query in `evidence` at `limit: 0` —
counting the rows without reading any, so writing the report never re-reads the
corpus. A claim that no longer holds fails the call by name rather than
shipping a count that aged out of retention. Only the citations are re-run —
anything you cannot back with a query belongs in `root_cause` or
`contributing`, which Mira takes as written.

![A recorded terminal session: three claims go in, one does not hold, and the call
fails naming it. Corrected, it renders a 59-line document carrying the record count
beside every claim.](assets/tui/write-up.gif)

```json
{"name":"render_rca","arguments":{
  "title":"Checkout 503s: a card decline mapped onto a retryable status",
  "from":"-45m",
  "root_cause":"payments returns 503 for payments.CardDeclined. A decline is a business outcome, not a server fault, and 503 propagates unchanged to the frontend, so both retry layers replay a call that can never succeed.",
  "evidence":[
    {"claim":"every failing authorize carries the same exception type",
     "query":{"signal":"traces","where":[{"attr":"exception.type","eq":"payments.CardDeclined"}]}},
    {"claim":"payments logged errors throughout the window",
     "query":{"signal":"logs","where":[{"attr":"service.name","eq":"payments"},
                                       {"field":"severity_number","gte":17}]}},
    {"claim":"a failing checkout, end to end",
     "trace_id":"0000000000000efb5555555555555bae"}],
  "ruled_out":["inventory, on the same call path: 1,223 spans, zero errors"],
  "remediation":["Return 402 for CardDeclined in payments' error mapping."],
  "prevention":{"name":"card-declines","over":"5m","when":"count > 20","severity":"warning",
    "query":{"signal":"traces","where":[{"attr":"exception.type","eq":"payments.CardDeclined"}]}},
  "emit":true}}
```

`summary`, `impact`, `timeline` and `verification` take the rest of it. Back
comes markdown, with the record count beside every claim:

```markdown
*2026-09-20T08:50:08Z → 2026-09-20T09:35:08Z (45m00s). Written by an agent against
Mira; every citation below was re-run at render time and returned the record count
beside it.*

## Evidence

- every failing authorize carries the same exception type — traces where
  `attr:exception.type=payments.CardDeclined`, 293 records
- payments logged errors throughout the window — logs where
  `attr:service.name=payments field:severity_number>=17`, 293 records
- a failing checkout, end to end — trace `0000000000000efb5555555555555bae`, 8 records

## Remediation

- Return 402 for CardDeclined in payments' error mapping.

> Mira did not apply any of this and cannot: it has no verb that changes a cluster.

## Prevention

The rule that would have caught this, ready for `alerts.kyaml` — add your own
`notify` targets:

    rules:
      - "name": "card-declines"
        "query": {"signal":"traces","where":[{"attr":"exception.type","eq":"payments.CardDeclined"}]}
        "over": "5m"
        "when": "count > 20"
        "severity": "warning"
```

The counts, the UTC window and that last line are Mira's, not the model's. The
alert rule went through the same parser that reads `alerts.kyaml` at boot, so it
is a rule that will start.

`"emit": true` stores the document as an ordinary log record — `event_name:
rca`, `service.name: mira`. You search for it with `{field: body, contains: …}`
like anything else, and it expires with the telemetry it describes. There is no
incident database to run.

## Waking the loop from an alert

Mira evaluates static alert rules itself, from KYAML
([Configuration](config.md)). A `json` target POSTs the rule's whole state, which
is enough for an agent to start without a first query:

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

`state` is `firing` or `resolved`, so the same endpoint closes the loop it
opened. There is no retry: a webhook that is down stays down for longer than the
evaluation period, and the next state change pages again anyway.

## No server at all

An agent sitting beside the data does not need the protocol. Mira keeps nothing
but the directory of files you gave it, and the files it queries are never
edited once written — so a second process can map the same directory read-only
while the first keeps writing to it:

```sh
mira mira --data-dir ./data     # the same views, no server, no port, no serialisation
```

Sandbox, edge node, or the pod next door: if the agent can see the directory, it
can read the data. That is
[architecture section 8.4](architecture/read-surfaces.md#84-the-in-process-read-path-and-why-a-local-agent-gets-it-for-free).
