# 15. The write-up

**Built**: `crates/mira/src/rca.rs`, and the ninth entry in `mcp.rs`'s tool
list. `render_rca` takes an investigation — the one [section 14](kubernetes-context.md)
made possible by putting the kill on the same timeline as the telemetry — and
returns it as markdown, having re-run every citation in it first. Zero new
crates: the query executor, the alerting rule parser and the ingest pipeline
were all already there.

## 15.1 A template would not have been worth a tool

`render_rca` is the only MCP tool that is not a question the UI also asks. It
takes the findings as fields — summary, impact, timeline, root cause, evidence,
what was ruled out, remediation, verification, prevention — and returns
markdown. The prompt could have carried a template. Two things make this a
tool instead.

### Every citation is re-run before the document renders

An evidence item gives a `trace_id` or a `query`, and Mira executes it with
`limit: 0` — the same free count
[section 13.2](alerting.md#132-counting-is-free-so-there-is-no-aggregation-engine)
uses — and puts the row count beside the claim. A citation matching nothing
fails the whole call, naming the claim; nothing is rendered and nothing is
stored.

This is the part that is not a formatting convenience. An RCA whose evidence
has aged out of retention reads exactly like one whose evidence was invented,
and a model reasoning for twenty minutes has usually accumulated one claim it
can no longer support. An item with no citation is refused at parse — judgement
belongs in `root_cause`, which is not a field anything can check. A citation
with no window of its own inherits the incident's, which is the difference
between a working tool and one where every write-up of this morning fails its
own verification.

### The proposed rule goes through the alerting loader

`prevention` is an `alerts.kyaml` rule, parsed by the same `alert::rule` that
reads the file at boot, so the fence in the document is a rule that will start
rather than one that looks like it would. That is the fourth reading of
principle 2 — self-tuning — reduced to its only honest mechanism: the incident
emits the rule that would have caught it, and a human decides whether to add
it.

Timestamps in the document are UTC. `tui::stamp` is local, which is right on a
terminal somebody is sitting at and wrong in a ticket read from another
timezone.

## 15.2 What comes back

The call, abbreviated:

```kyaml
title: "Checkout 502s: the connection pool never grew with the image"
from: 1789812000000000000
to:   1789813860000000000
root_cause: >-
  1.4.0 raised the pool ceiling to 200 without raising the memory limit, so
  the pod crossed its limit under normal load and the kubelet killed it.
evidence: [
  { claim: "checkout logged pool-exhaustion errors throughout the window",
    query: { where: [ { attr: service.name, eq: checkout },
                      { field: severity_number, gte: 17 } ] } },
  { claim: "the kubelet killed the container twice",
    query: { signal: logs,
             where: [ { attr: k8s.container.reason, eq: OOMKilled } ] } },
  { claim: "a failing request, end to end",
    trace_id: 4bf92f3577b34da6a3ce929d0e0e4736 },
]
prevention: {
  name:  checkout-oomkilled,
  query: { signal: logs,
           where: [ { attr: k8s.container.reason, eq: OOMKilled },
                    { attr: k8s.namespace.name, eq: shop } ] },
  over:  5m, when: "count >= 1", severity: critical,
}
emit: true
```

The second citation is the one [section 14](kubernetes-context.md) exists for:
`k8s.container.reason` is a field the kubelet holds and OTLP has never carried.
The parts of the reply that are not the agent's own prose:

```markdown
*2026-09-19T10:00:00Z → 2026-09-19T10:31:00Z (31m00s). Written by an agent
against Mira; every citation below was re-run at render time and returned the
record count beside it.*

## Evidence

- checkout logged pool-exhaustion errors throughout the window — logs where
  `attr:service.name=checkout field:severity_number>=17`, 4102 records
- the kubelet killed the container twice — logs where
  `attr:k8s.container.reason=OOMKilled`, 2 records
- a failing request, end to end — trace `4bf92f3577b34da6a3ce929d0e0e4736`,
  37 records

## Remediation

- Roll back to 1.3.9.
- Raise the memory limit to 1Gi before rolling 1.4.0 forward.

> Mira did not apply any of this and cannot: it has no verb that changes a
> cluster.
```

That last line is in the document, not only in this repository's docs
([section 14.6](kubernetes-context.md#146-read-only-and-not-by-omission)). The
reader of an RCA is the one who needs to know none of it has happened yet.

Sections with nothing in them are absent rather than empty: a bare
`## Prevention` invites the reader to assume there was none to propose.

## 15.3 `emit`, and why there is no incident store

With `emit`, the rendered document is also ingested — as a log record, through
the pipeline an OTLP export goes through, with `event_name: rca` and the title
and window as attributes.

A log record and not a new kind of object, and that is why it is affordable.
There is no incidents table, no index to maintain, nothing extra to back up and
no separate expiry: the write-up is searchable with `query_records`, framed by
`correlate`, and aged out by the retention that ages the telemetry it
describes. Principle 4 says the block directory is the manifest, and an
incident store would have been a second one.

`service.name` on the record is `mira`, not the service the RCA is about.
Naming the subject would file the document under that service's entity key and
put a page of markdown in the middle of its logs —
[section 14.4](kubernetes-context.md#144-k8spoduid-and-the-servicename-that-was-refused)
again, from the other direction.

The engine's ingest handle is the one place a read surface can write, and the
alert evaluator's copy of `Api` is deliberately built without it. That task
never returns, so a handle inside it is one the logs flusher never sees
dropped, and every shutdown on an alerting node would sit out the full drain
grace.
