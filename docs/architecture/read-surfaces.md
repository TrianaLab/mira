# 8. Read surfaces: agents and humans

Three of them — MCP, a browser UI, a terminal UI — and all three go through
`api.rs`'s parsers and `api::envelope`, so there is one query grammar to keep
correct rather than three that drift.

That grammar is closed at the top of a document as well as inside a term: an
unimplemented top-level key is a 400 naming it. A lenient parser answered 200 over
the whole window for a key it does not implement: a wrong answer that looks right.

## 8.1 Agentic surface

**Built**: `POST /mcp` on the same listener, JSON-RPC 2.0 over Streamable HTTP,
eight tools — `query_records`, `get_trace`, `query_metric` and `list_metrics` over
the query document, three more over the frame algebra (section 7.3), and
`list_alerts` over the alert evaluator (section 13). Hand-rolled rather than
`rmcp`: at this scope the protocol is a method dispatch over a JSON document,
which we already parse (section 1, KYAML), and the SDK's session model is the
thing we do not want.

No `Mcp-Session-Id` is issued: Streamable HTTP lets a server require one on every
later request, which makes the server a thing with memory that a load balancer
must route back to. Issuing none means any replica answers any request. The tools
take the UI's read path — the same parsers, the same `query`/`series` functions,
the same `envelope()` — because a second read path drifts.

`get_trace` is its own tool rather than a `trace_id` term because the default
window is one hour, and an agent handed yesterday's trace id would get an empty
result with nothing to explain it. It searches all of retention; section 7.4's
block filter makes that affordable.

Not built: **block-footer sketches** in `Schema.custom_metadata` — HyperLogLog for
cardinality, t-digest for latency quantiles, top-K for attribute values — so that
"what is unusual in this hour" is answered from footers rather than a scan.

## 8.2 The browser UI

Svelte, built to `crates/mira/ui/dist` and `include_bytes!`'d into the binary,
served from `/` on the same listener: nothing to deploy beside the binary, and no
CORS because it is the same origin as the API it calls. `dist/` is checked into
git so `cargo build` never needs a JavaScript toolchain to reproduce bytes that
did not change.

Owning the UI follows from owning the API: Mira's query shape is not PromQL, not
LogQL and not SQL — the query document of section 7.6 is the interface — so an
off-the-shelf frontend would have to be taught it, which means shipping and
versioning a second thing.

The frame algebra surfaces as a Frame panel above the records and a Map view
beside them, both built so that **every line leads back into a query**: a service
in the frame ands a `service.name` term into the filter box. A correlation view
that is only a picture is a dead end, and the algebra's closure makes the
alternative free. The panel's state lives in the URL (`?frame=1`), because a link
to an investigation has to carry the investigation. Live is a refetch on a
three-second timer, not a stream: every query is a millisecond-scale read over
mmap'd blocks, and a WebSocket would be a second protocol and a reconnection state
machine.

## 8.3 The terminal UI

`mira mira` renders the same three tabs, the same filter grammar and the same
trace waterfall in the terminal, plus the frame algebra on `c` and `m` and a live
tail on `f`.

### ratatui, and no backend

This section used to argue for no framework, on an unmeasured estimate of 35
crates. It was wrong: the hand-rolled renderer clipped at `max` rather than
overflowing, so layout bugs lost content while its unit tests stayed green.
Measured against the prior binary on this laptop, the port costs **+26 crates
and +145 KiB** (122 → 148 crates) and an MSRV of 1.88.

Most of that gap is the backend: `CrosstermBackend` would pull crossterm, mio,
signal-hook and parking_lot in to do what `term.rs` already does.
`Term::draw(&[String])` is the seam — `tui::rata`
flattens a rendered `Buffer` into those rows — so `term.rs` keeps the syscall
half: `termios`, `TIOCGWINSZ`, `poll(2)`, three `sigaction`s, on `libc`, already
in the tree for section 9's `statfs` guard. Its `Row` builder is deleted rather
than wrapped: 909 lines, from 1,127.

SIGWINCH's default disposition is to *discard* it, so without a handler a resize
repainted nothing; the handler itself does nothing, because the delivery is the
message — the `EINTR` that sends the loop round to re-read the size. SIGTERM and
SIGHUP hand the terminal back rather than leaving a shell in raw mode on the
alternate screen, so `restore()` is async-signal-safe `libc::write`. Unix only,
the same bet `mmap` already makes.

### Two transports, one code path

`--addr host:4318` POSTs to a running replica. `--data-dir` calls
`mira_core::query` **in-process, with no server anywhere**, which is why the TUI
is worth building rather than being a smaller browser UI: a detached PVC, or the
volume of a pod already killed, is still readable. Responses are parsed with the
KYAML loader, so the binary still has no JSON *parsing* dependency.

The whole thing is synchronous: no runtime, no task, no channel. A slow query
freezes the UI for its duration, which is why the frame is painted *before* the
query runs — the freeze always carries a "running" rather than a stale screen.

### The frame and the map are modes, not tabs

A fourth tab would need arms in eight places; a mode needs six and gets esc-back
for free, which is what these panes want: a detour from a list, the same shape `t`
and the waterfall already have. The map is drawn as a **tree**, not a graph,
because a terminal draws trees well and a tree makes every line something Enter
can act on.

### Follow is the key read timing out

The loop blocks on `poll(2)`, so a key that does not arrive within three seconds
*is* the tick: live tail is a timeout rather than a thread, a channel and a second
copy of the app state.

## 8.4 The in-process read path, and why a local agent gets it for free

There is a fourth read surface with no protocol at all: open the block directory
and map it. `mira mira --data-dir PATH` is the shipped consumer — the *same*
binary and query code as `--addr`, with the HTTP client swapped for a direct call.
No port is bound and no bytes are serialised; `main.rs` guards against the
filesystems where `mmap` can raise `SIGBUS` (section 9).

It falls out of three decisions made for other reasons, which is the argument for
having made them:

- The filesystem is the manifest (section 3.2), so there is nothing to ask a
  server for: a catalogue-based engine cannot offer this, because the catalogue
  lives in the process you are trying not to run.
- Blocks are immutable once renamed (section 5), so a reader needs no lock, no
  lease and no coordination with a writer; retention is `unlink`, and an
  already-mapped block survives it (section 6).
- `mira-core` is a library crate, so any Rust program can do the same; the CLI has
  no privileged path into the data.

An agent co-located with its telemetry pays three costs to read over HTTP: a
serialise/deserialise round trip per result, a port and its lifecycle, and a
process to keep alive between questions. All three are pure loss at that
distance.

It deliberately does not write: two writers against one directory is the
staging-path collision of section 12.6.

---
