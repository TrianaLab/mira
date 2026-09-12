# Telemetry is the Short-Term Memory of Autonomous Systems

Observability was built for a human on call. Every assumption in the stack follows from
that one: dashboards refresh on a cadence a person can read, retention is priced for a
post-mortem written the next morning, and the query languages are dialects a person
learns once and keeps. The loop was **observe, page a human, wait**.

That loop is being closed by software. An agent watching a service does not read a
dashboard; it asks a question, gets a structured answer, asks the next question, and
acts. It runs that cycle continuously, not once per incident. Its working set is the
last few minutes of what the system actually did — which is exactly what telemetry is.

**Telemetry is not the audit log of an autonomous system. It is its short-term memory.**

That reframing has consequences, and they are architectural rather than rhetorical.

---

## The loop

An agent operating infrastructure runs the same four steps a human on call runs, at a
different clock rate.

| | the human version | the agent version | what it needs from storage |
|---|---|---|---|
| **Observe** | a dashboard refreshes | a tool call returns | an answer in milliseconds, not seconds |
| **Orient** | "what else broke at 14:32?" | one call that returns the frame | correlation as a primitive, not three round trips |
| **Decide** | judgement, experience | a model, in context | structured output that fits in a context window |
| **Act** | a runbook | an API call, a reroute, a rollback | nothing — this is not storage's job |

Three of those four are storage problems. The fourth is not, and Mira does not pretend
otherwise: it has no actuator, no remediation engine and no opinion about your control
plane. It is the memory the loop reads from, and it is built to be read from at that
rate.

---

## Why the existing stacks fail this workload

Not because they are badly built. Because they were built for the other loop.

**Latency is a per-iteration tax, not a per-incident one.** A human tolerates a
two-second dashboard because they look at it four times an hour. An agent reasoning
towards a conclusion issues twenty queries to get there. At two seconds each that is
most of a minute before the first hypothesis, per attempt — and a loop that slow gets
replaced by a static alert rule. The unit that matters is not "fast enough to watch",
it is "fast enough to iterate on".

**Proving something is *absent* is the operation nobody optimises, and half of an
agent's queries are hypotheses it is about to discard.** Ruling things out is how
reasoning narrows. A store that has to read everything to establish a negative makes
elimination the most expensive step in the loop, which is exactly backwards. Answering
"no" should be cheaper than answering "yes", not dearer.

**Cost decides the retention window, and the retention window decides whether the memory
exists at all.** Per-gigabyte-ingested pricing turns "keep everything at full fidelity
for a few hours so an agent can look" into a line item somebody eventually cuts. Sampling
is the usual answer, and sampling is amnesia: the record an agent needed is the one that
was dropped, and it has no way to know it is reasoning over a subset.

**Every query dialect is a place a model is wrong.** SQL, PromQL, TraceQL, LogQL — four
languages, four sets of failure modes, and a generated query that is syntactically valid
and semantically wrong returns a confident answer to a question nobody asked. The
interface an agent uses should be small enough to get right on the first try, and it
should refuse what it does not understand instead of guessing. A model that invents a
field name needs to be *told*; handing it plausible-looking rows teaches it the wrong
thing.

**A network hop is a design decision, and for a local agent it is the wrong one.** The
managed stacks have no in-process mode because they cannot have one. An agent that lives
next to the workload should be able to read the workload's memory without a service, a
port, or a token in between.

---

## What that asks of the storage engine

Four properties, each of which exists because of the loop above.

### The agent is a first-class reader, not an export

Mira speaks the Model Context Protocol natively, over the same code path the UI uses —
because a second surface with its own query path is a second surface with its own bugs.
The tools are written to be read by a model rather than by someone who already knows the
schema: each answer says what to feed it into next, and a question with no answer comes
back as an empty result rather than a transport error, because "no rows for that service"
and "malformed request" mean different things and a model that sees both as a failure
learns nothing from either.

### Reading is cheap enough to do in a loop

Recent data is held in a form a query can read directly, with no decoding step between
the bytes on disk and the answer. That is what makes the difference between an
interactive latency and a conversational one, and it is the reason a hypothesis is
cheap to discard: eliminating a possibility usually means opening nothing at all.

### Correlation is one call, not a plan the agent has to make

The interesting question is never "show me these logs". It is "what else was happening".
Mira answers that as a primitive: give it something that matched, get back the frame
around it — the real time window, the traces involved, the services that took part.

This removes three round trips, and more importantly it removes the step where the agent
has to *know* to make them. It also says when the frame is a sample rather than the whole
of it, so the model is told to narrow rather than left to conclude from a partial set.

### Memory should sit next to the thing remembering

One binary, a few megabytes, one directory. No cluster to join, no coordination state, no
database beside it. For an agent this is not an ops convenience — it is what makes the
memory *co-located*. The thing you can run in the same pod as the agent, on an edge node,
or inside the sandbox the agent is reasoning in, is a different primitive from the thing
you query over the network with a token.

---

## What the loop looks like in practice

Concretely, with an agent connected: one line of client configuration, and it can run an
investigation a human would recognise.

1. **Something is wrong.** The agent asks what is firing and gets back the rule, its
   state, the numbers behind it, and — this is the part that matters — *the search the
   threshold was counting*. It does not have to reconstruct the question.
2. **How far does it reach?** It hands that same search back and asks for the frame: the
   true time window, the traces involved, every service that took part. One call, and the
   answer says whether it is complete or a sample.
3. **Which hop failed?** Any trace from the frame, expanded in full, with the parent
   links intact — so the difference between where an error was *created* and where it was
   merely reported is visible rather than inferred.
4. **What did the service itself say?** Logs filtered on the same attributes as the
   spans, because they are in the same store, indexed the same way. The pod name and
   instance id come back attached, which is what the Act step needs.

Four questions, well under a second of query time between them, and each answer is
already the input to the next. That is the whole of the difference: not that an agent
*can* read telemetry — it always could — but that the reading is cheap enough, and the
answers composable enough, to do it in a loop rather than once.

The wiring, the tool list and a worked investigation from a firing alert to the failing
dependency: [Connect an agent](https://miradb.dev/agents/).

---

## What this does not claim

A manifesto that only lists strengths is marketing.

- **Mira is not long-term memory.** It is the working set: minutes to days, at full
  fidelity. Nothing in it is a vector store, an embedding index or a summarisation
  layer, and pointing an agent at a year of history through it is the wrong tool.
- **Mira does not act.** No remediation, no traffic control, no rollback. The loop's
  Act step is yours.
- **Mira does not reason.** It returns records and frames. The model does the orienting;
  Mira makes the orienting cheap enough to do in a loop.
- **The agentic framing does not buy an exemption from being a good database.** Every
  claim above is a measurement somewhere else in this repository, taken on one machine
  and reproducible with the load harness here. If one stops being true it comes out of
  this document.

---

The bet is simple: the systems being built now generate telemetry that no human will
ever read, consumed by software that reads it within seconds of it being written. That
consumer needs a store that answers in milliseconds, costs little enough that the window
stays open, speaks a surface a model gets right on the first try, and runs next to the
thing asking.

That store did not exist. Mira is an attempt at it.

<!-- Absolute URLs, not repository-relative ones: this file is read both on GitHub
     and published at /manifesto/ on the docs site, and no relative path is correct
     in both places at once. -->

*How it is built, and why each choice:
[the architecture document](https://miradb.dev/architecture/). The
numbers behind every claim here, with the machine they were taken on and what
every other engine publishes beside them:
[where Mira sits in the market](https://miradb.dev/market/).*
