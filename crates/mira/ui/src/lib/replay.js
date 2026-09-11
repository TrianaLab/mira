// The recorded-snapshot build of this UI: the same components, the same
// rendering, answers served from a file instead of from a server.
//
// It exists so the documentation site can host a version someone can click
// through without installing anything. It is *not* a wasm build of the engine
// and does not pretend to be one: the responses in `fixtures.json` were
// recorded from a real Mira over the demo generator by
// `scripts/capture-ui-fixtures.sh`, and this module hands them back. What is
// real is every byte of the UI — the tables, the waterfall, the service map,
// the chart, the detail panes, the paging, the routing. What is not real is
// the query: typing a filter re-renders from the same recorded rows rather
// than re-reading blocks, because there are no blocks in a browser tab.
//
// Compiling the engine itself to wasm would make the query real, and it is not
// out of reach — `mira-core` reads through `mmap` and a browser has no `mmap`,
// so the block reader would need a second backend over an `ArrayBuffer`. That
// is a real piece of work with a real payoff and it is not this. The banner
// says which of the two the reader is looking at, because a demo that implies
// it is measuring something it is not is worse than no demo.
//
// This whole module is tree-shaken out of the ordinary build: `api.js` reaches
// it behind `__REPLAY__`, a literal substituted by `vite.config.js`, so the
// binary's UI never carries the fixtures. Verified rather than assumed —
// `make ui` produces a 76 KB bundle and `make ui-demo` a 1.77 MB one.

// The import attribute is not decoration: Vite infers the type from the
// extension, but `node --test` refuses a bare JSON import, and this module has
// tests. One clause keeps both loaders happy.
import fixtures from './fixtures.json' with { type: 'json' }

/// Which recorded response answers this request.
///
/// Coarser than the request, deliberately. The UI can generate more distinct
/// bodies than anyone would record — seven ranges times three signals times
/// any filter that can be typed — so the key drops everything the recording
/// cannot honour and keeps the two things that change what is on screen: which
/// signal, and which page of it.
export function key(path, body) {
  if (path === '/api/v1/query') {
    // A trace id is the one filter that is honoured, because the waterfall is
    // unreadable with another trace's spans in it. The capture records the
    // traces its own first page links to; anything else falls back below.
    const trace = (body.where || []).find((t) => t.field === 'trace_id' && t.eq)
    if (trace) return `query:trace:${trace.eq}`
    return `query:${body.signal}:${body.after ? 2 : 1}`
  }
  if (path === '/api/v1/metrics/query') return `metrics:${body.name}`
  // The frame panel is per signal — it correlates from the rows the current
  // view is showing — so logs and traces are two different answers.
  if (path === '/api/v1/correlate') return `correlate:${body.signal}`
  return path
}

/// The recorded answer, or the nearest one. Never a rejection: a missing
/// fixture is a gap in the recording, not a server that fell over, and an
/// error banner would send the reader looking for a cause that is not there.
export function replay(path, body) {
  const k = key(path, body)
  const hit =
    fixtures[k] ||
    // A trace nobody recorded shows the trace that was recorded, so clicking
    // any row in the traces table opens a waterfall rather than an empty pane.
    (k.startsWith('query:trace:') && fixtures['query:trace']) ||
    // A metric nobody recorded is a name the capture truncated at; show the
    // first series it did record rather than an empty chart.
    (k.startsWith('metrics:') && fixtures['metrics']) ||
    null
  if (!hit) return { rows: [], series: [], entities: [], names: [], stats: STATS }
  // Cloned, because components mutate what they are handed — `Records` appends
  // the next page onto `rows` — and a fixture is read more than once.
  return structuredClone(hit)
}

/// What the UI puts in the header when a response carries no stats of its own.
/// Zeroed rather than invented: the recording has real numbers for every
/// endpoint that returns them, and this is only reached on a fallback.
const STATS = { blocks_total: 0, blocks_read: 0, rows_matched: 0, elapsed_us: 0 }
