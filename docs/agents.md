---
# `4.7 ms` and `27.1M` in "The failing hop" are pinned in measurements.kyaml —
# grep this path there before touching them.
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

=== "Any client"

    Streamable HTTP, protocol revision `2025-06-18`, JSON-RPC 2.0. One endpoint,
    `POST /mcp`, `content-type: application/json`.

    ```sh
    curl -s -X POST localhost:4318/mcp -H 'content-type: application/json' \
      -d '{"jsonrpc":"2.0","id":1,"method":"tools/list"}'
    ```

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

None of them changes anything outside Mira, and there is no tenth that restarts
a pod or edits a cluster:
[architecture section 14.6](architecture/kubernetes-context.md#146-read-only-and-not-by-omission).

Three things matter more than the list:

| | |
| --- | --- |
| **A wrong argument is refused by name** | Send a key a tool does not support and the error names it. |
| **"Nothing matched" and "bad request" stay apart** | A tool that cannot answer says so with `isError` in the body of a `200`. |
| **Every field you get back is an input to the next call** | Trace ids go to `get_trace`, a service name goes to `query_records` as `{attr: service.name, eq: …}`, and `from`/`to` go anywhere. `truncated: true` means you have a sample, so narrow rather than conclude. |

Every answer that reads files also carries `stats`: files on disk
(`blocks_total`), files opened (`blocks_scanned`), rows read (`rows_scanned`),
rows matched (`rows_matched`), microseconds (`elapsed_us`). `list_alerts` is
the exception — it reads rule state from memory and opens nothing.

## A root-cause investigation, end to end

The wiring above, a Mira holding `make demo`, and one prompt.

```console
$ claude -p "Checkout is throwing 503s. Use the mira MCP server to find the root
  cause: start from whatever it is alerting on, then dig until you can name the
  failing operation and the exception behind it. Show me the trace id you used."
```

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

`state` is `ok`, `pending` (over the line, but not for long enough yet) or
`firing`. `filter` is the rule's own search — the one the threshold was
counting. `link` opens the same records in the UI.

### 2. Two questions in one turn

```json
{"name":"query_records","arguments":{"signal":"traces","from":"-15m",
  "where":[{"field":"status_code","eq":2}],"limit":40}}
{"name":"service_map","arguments":{"from":"-15m"}}
```

The errors run down one branch and stop: `frontend` to `checkout` to
`payments`, while `inventory` hangs off the root and stays clean.

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

Eight spans, each naming its parent, so they arrive as a tree. Nothing in the
prompt named a service, a tool or a field. `get_trace` rather than a trace-id
filter on `query_records`: every file carries a trace-id index, and using it is
4.7 ms against 27.1M spans in [the query benchmark](market.md#query), because
almost every file is skipped without being opened.

## The write-up

`render_rca` turns the findings
into a document, and first re-runs every query in `evidence` at `limit: 0` —
counting the rows without reading any. A claim that no longer holds fails the
call by name. Only the citations are re-run —
anything you cannot back with a query belongs in `root_cause` or
`contributing`, which Mira takes as written.

![A recorded terminal session: three claims go in, one does not hold, and the call
fails naming it. Corrected, it renders a 59-line document carrying the record count
beside every claim.](assets/tui/write-up.gif)

```json
{"name":"render_rca","arguments":{
  "title":"Checkout 503s: a card decline mapped onto a retryable status",
  "from":"-45m",
  "summary":"payments returns 503 on card declines, and both retry layers replay them.",
  "root_cause":"payments returns 503 for payments.CardDeclined. A decline is a business outcome, not a server fault, and 503 propagates unchanged to the frontend, so both retry layers replay a call that can never succeed.",
  "evidence":[
    {"claim":"every failing authorize carries the same exception type",
     "query":{"signal":"traces","where":[{"attr":"exception.type","eq":"payments.CardDeclined"}]}},
    {"claim":"a failing checkout, end to end",
     "trace_id":"0000000000000efb5555555555555bae"}],
  "ruled_out":["inventory, off the failing branch: 1,223 spans, zero errors"],
  "remediation":["Return 402 for CardDeclined in payments' error mapping."],
  "emit":true}}
```

`title`, `summary` and `root_cause` are required; `impact`, `timeline` and
`verification` take the rest of it. Back comes markdown — the window, the
summary and the root cause first, then the record count beside every claim:

```markdown
… (window, ## Summary, ## Root cause)

## Evidence

- every failing authorize carries the same exception type — traces where
  `attr:exception.type=payments.CardDeclined`, 293 records
- a failing checkout, end to end — trace `0000000000000efb5555555555555bae`, 8 records

## Ruled out

- inventory, off the failing branch: 1,223 spans, zero errors

## Remediation

- Return 402 for CardDeclined in payments' error mapping.

> Mira did not apply any of this and cannot: it has no verb that changes a cluster.
```

The counts and that last line are Mira's, not the model's.

`"emit": true` stores the document as an ordinary log record — `event_name:
rca`, `service.name: mira`. You search for it with `{field: body, contains: …}`
like anything else, and it expires with the telemetry it describes.

## Waking the loop from an alert

A `json` target ([Configuration](config.md)) POSTs the rule's whole state, which
is enough for an agent to start without a first query:

```yaml title="alerts.kyaml"
notify: [
  { name: agent, url: "http://127.0.0.1:9000/incident", format: json },
]
```

`state` is `firing` or `resolved`, so the same endpoint closes the loop it
opened. There is no retry.

## No server at all

An agent sitting beside the data does not need the protocol
([architecture section 8.4](architecture/read-surfaces.md#84-the-in-process-read-path-and-why-a-local-agent-gets-it-for-free)):

```sh
mira mira --data-dir ./data     # the same views, no server, no port, no serialisation
```
