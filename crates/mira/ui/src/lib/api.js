// The API client, the filter mini-language, and formatting. No Svelte in here
// on purpose: this is the part with logic worth testing without a DOM.

export const api = (path, body) =>
  send(path, {
    method: 'POST',
    headers: { 'content-type': 'application/json' },
    // JSON.stringify output is valid KYAML, which is why one parser on the
    // server serves both this and the config file.
    body: JSON.stringify(body),
  })

// The read-only documents that describe the *process* rather than the blocks:
// `/api/v1/alerts`, and anything else that answers without being asked a query.
// A GET because there is nothing to ask -- and separate from `api` only so the
// error handling below has one home.
export const get = (path) => send(path, {})

// The recorded-snapshot build the documentation site hosts. `__REPLAY__` is a
// literal substituted by `vite.config.js`, so the ordinary build folds this to
// `false`, drops the branch below, and never pulls `replay.js` — or the
// fixtures behind it — into the bundle that ships inside the binary. Guarded
// with `typeof` rather than read directly, because `node --test` runs this
// file with no substitution at all and a bare undeclared identifier is a
// `ReferenceError`. See `lib/replay.js` for what the snapshot claims to be.
export const REPLAY = typeof __REPLAY__ !== 'undefined' && __REPLAY__

async function send(path, init) {
  if (REPLAY) {
    const { replay } = await import('./replay.js')
    return replay(path, init.body ? JSON.parse(init.body) : {})
  }
  let r
  try {
    r = await fetch(path, init)
  } catch {
    // A rejected fetch is a transport failure, and every browser words it
    // differently and uselessly -- "Failed to fetch" in Chrome, "Load failed"
    // in Safari, "NetworkError" in Firefox. This page was served by the same
    // process it is now failing to reach, so there is really only one cause
    // worth naming, and it is one the reader can act on.
    throw new Error(
      `Cannot reach ${location.origin}. This page came from Mira, so the ` +
        `likely cause is that the server has stopped — restart it and reload.`,
    )
  }
  const text = await r.text()
  let j
  try {
    j = JSON.parse(text)
  } catch {
    throw new Error(text || `HTTP ${r.status} ${r.statusText}`)
  }
  // The status code stays in the message. A rejected query and a panicked one
  // read the same otherwise, and 4xx-versus-5xx is the difference between
  // "fix your filter" and "look at the server log".
  if (!r.ok) throw new Error(`HTTP ${r.status}: ${j.error || r.statusText}`)
  return j
}

// "all" is 1970 to now. There is no unbounded query: an open-ended scan is the
// one mistake that makes a fast engine look slow.
export const bounds = (range) =>
  range === 'all' ? { from: 0, to: 'now' } : { from: range, to: 'now' }

// ---------------------------------------------------------------- filters
//
// A mini syntax that maps one-to-one onto the API's `where` terms, because the
// alternative is asking a human to type KYAML into a text box.
//
//   service.name=checkout severity_number>=17 body~timeout name!="GET /health"
//
// A bare key is an attribute unless it is one of the signal's root columns, so
// the box teaches the schema as you use it.
//
// A whole word with no operator is free text over the signal's message column,
// which is what every log viewer does with one and what the terminal UI does
// with one. Metrics has no message column — its terms are attribute predicates
// on data points — so there a bare word is an error rather than a guess.

// Every root column of mira-core's LOGS and SPANS that is worth comparing
// against. The block-local ids (`id`, `resource_id`, `scope_id`) and `body_ser`
// are the omissions: the first three name a row inside one block and mean
// nothing to a person, and the last is protobuf bytes on disk, so no predicate
// the box can express would match it. Anything missing here is read as an
// attribute and matches nothing at all, which is why the list must track
// schema.rs.
export const FIELDS = {
  logs: ['time_unix_nano', 'observed_time_unix_nano', 'severity_number',
    'severity_text', 'event_name', 'body', 'trace_id', 'span_id', 'flags',
    'dropped_attributes_count'],
  traces: ['trace_id', 'span_id', 'parent_span_id', 'trace_state', 'flags',
    'name', 'kind', 'start_time_unix_nano', 'duration_nano', 'status_code',
    'status_message', 'dropped_attributes_count', 'dropped_events_count',
    'dropped_links_count'],
  metrics: [],
}

// Longest first at the same position: `>=` must win over `>`, `!=` over `=`.
const OPS = [['>=', 'gte'], ['<=', 'lte'], ['!=', 'ne'], ['~', 'contains'],
  ['>', 'gt'], ['<', 'lt'], ['=', 'eq']]

export const FREE_TEXT = { logs: 'body', traces: 'name' }

const unquote = (raw) =>
  raw.length > 1 && raw[0] === '"' && raw.endsWith('"') ? raw.slice(1, -1) : raw

// Ids stay strings whatever they look like, the same exemption the terminal
// filter box makes: a span id of 16 decimal digits reads as a number, the
// engine's unhex path only accepts a string, and an inapplicable term is
// defined to return no rows rather than an error -- so the mistake is silent.
// A 32-digit trace id, which this repo's own loadgen emits, loses its low bits
// to the float as well.
const isId = (key) => key.endsWith('_id') || key.endsWith('.id')

function coerce(raw, key) {
  if (raw !== unquote(raw)) return unquote(raw)
  if (raw === 'true') return true
  if (raw === 'false') return false
  if (isId(key)) return raw
  if (/^-?\d+$/.test(raw)) return Number(raw)
  if (/^-?\d*\.\d+$/.test(raw)) return Number(raw)
  return raw
}

export function parseFilter(text, signal) {
  const terms = []
  for (const tok of text.match(/(?:[^\s"]|"[^"]*")+/g) || []) {
    const hit = OPS.map(([sym, op]) => [tok.indexOf(sym), sym, op])
      .filter(([i]) => i > 0)
      .sort((a, b) => a[0] - b[0] || b[1].length - a[1].length)[0]
    if (!hit) {
      // An operator at position 0 (`=v`) is a half-written term, not a word:
      // the operator says what was meant and the key is missing.
      const free = OPS.some(([sym]) => tok.includes(sym)) ? null : FREE_TEXT[signal]
      if (!free) throw new Error(`\`${tok}\` needs an operator: = != ~ > >= < <=`)
      // `unquote`, not `coerce`: the message column is text, and `500` sent as
      // an integer would match nothing at all.
      terms.push({ field: free, contains: unquote(tok) })
      continue
    }
    const [i, sym, op] = hit
    let key = tok.slice(0, i)
    let target = 'attr'
    if (key.startsWith('field:') || key.startsWith('attr:')) {
      ;[target, key] = key.split(':')
    } else if ((FIELDS[signal] || []).includes(key)) {
      target = 'field'
    }
    // Typed after the prefix is stripped, so the exemption sees the real key.
    terms.push({ [target]: key, [op]: coerce(tok.slice(i + sym.length), key) })
  }
  return terms
}

// ------------------------------------------------------- editing a filter
//
// Three operations over the filter box's text, extracted from the components
// that call them because every one of them can be quietly wrong: a term that
// does not parse, a term that contradicts one already there, or a service name
// read back out of the wrong place. They are pure, so they are tested.

// The term that finds this value again, for a key clicked out of a detail
// pane. `parseFilter` decides field-versus-attribute from FIELDS, so a root
// column and an attribute both round-trip.
export function term(key, value) {
  // Unquoted for a value that arrived as a JSON number or bool, quoted for
  // everything else. Not cosmetic: the engine's zone maps only answer a term
  // whose scalar is unquoted (`range_probes`), because a quoted one is
  // lexicographic against a str column and arithmetic against an int one. Both
  // spellings return the same rows; the quoted one reads every block to do it.
  // A string stays quoted even when it looks numeric — an int term against a
  // str column matches nothing, silently.
  if (typeof value === 'number' || typeof value === 'boolean') return `${key}=${value}`
  const s = String(value)
  // A double quote is the delimiter and the mini-language has no escape for
  // it, so a value carrying one is matched on the part before it with
  // `contains` rather than on an equality that would fail to parse.
  return s.includes('"') ? `${key}~"${s.split('"')[0]}"` : `${key}="${s}"`
}

// And, skipping a term already present -- clicking the same value twice is a
// double-click, not a request for `a="b" a="b"`.
export const addTerm = (q, t) => {
  const cur = (q || '').trim()
  return !cur ? t : cur.split(/\s+/).includes(t) ? cur : `${cur} ${t}`
}

// The service currently selected, and how to change it. Replaces rather than
// ands: two `service.name=` equalities match nothing at all, which reads as
// "this service has no data" rather than "you picked twice".
export const serviceOf = (q) => ((q || '').match(/service\.name="?([^"\s]+)"?/) || [])[1] || ''

export function withService(q, name) {
  const rest = (q || '')
    .split(/\s+/)
    .filter((t) => t && !t.startsWith('service.name='))
    .join(' ')
  return (name ? `${rest} service.name="${name}"` : rest).trim()
}

// ------------------------------------------------------------ service map
//
// Longest-path layering: every node sits one column right of its furthest
// caller. A service map is a DAG in principle and a cycle in practice -- a
// retry, a callback, a consumer that calls back into its producer -- so the
// relaxation is bounded by the node count instead of run to a fixed point.
// Without that bound a two-service cycle is an infinite loop in a render path.
export function layer(nodes, edges) {
  const depth = new Map(nodes.map((n) => [n.key, n.key === 'entry' ? 0 : 1]))
  for (let i = 0; i < nodes.length; i++) {
    let moved = false
    for (const e of edges) {
      // An edge from or to a node that is not in `nodes` is not a layout
      // problem to solve: the map bounds its span budget, so an edge can name
      // a node the sample never reached.
      if (!depth.has(e.from) || !depth.has(e.to)) continue
      const d = depth.get(e.from) + 1
      if (d > depth.get(e.to)) { depth.set(e.to, d); moved = true }
    }
    if (!moved) break
  }
  return depth
}

// ------------------------------------------------------- 64-bit integers
//
// Every 64-bit integer in a response arrives as a JSON *string*: `time_unix_nano`
// and `observed_time_unix_nano` on a log, `start_time_unix_nano` and
// `duration_nano` on a span, `events[].time_unix_nano`, a metric point's
// timestamp and its value when the value is an integer, an exemplar's
// `time_unix_nano` and `int`, and every attribute whose OTLP type is
// `int_value`. That is OTLP/JSON's encoding of an int64 and it is not
// negotiable-per-field: a nanosecond timestamp is ~1.7e18, twenty times past
// `Number.MAX_SAFE_INTEGER`, so `JSON.parse` on a bare number would have
// rounded the low digits off before this file ever saw them.
//
// A string renders, sorts and keys correctly as it stands, so nothing converts
// by default. `num` exists for the two places that do arithmetic — the chart's
// pixel math and the waterfall's bar geometry — where the rounding is far below
// one pixel and being handed a string instead is fatal.
export const num = (v) => (v === null || v === undefined ? null : Number(v))

// ---------------------------------------------------------------- formatting

export function fmtTime(ns) {
  const d = new Date(Number(ns) / 1e6)
  const p = (n, w = 2) => String(n).padStart(w, '0')
  return `${p(d.getMonth() + 1)}-${p(d.getDate())} ${p(d.getHours())}:` +
    `${p(d.getMinutes())}:${p(d.getSeconds())}.${p(d.getMilliseconds(), 3)}`
}

export function fmtDur(v) {
  // Coerced up front, not relied on per comparison: `"1500" < 1e3` happens to
  // do the right thing because a relational operator coerces, but `${v}ns`
  // and `.toFixed` do not, so one branch would print the raw string. Via
  // `num`, so a column the engine omitted renders as an empty cell rather than
  // as `Number(null)` — which is 0, and "0ns" is a measurement, not a blank.
  const ns = num(v)
  if (ns === null || !Number.isFinite(ns)) return ''
  if (ns < 1e3) return `${ns}ns`
  if (ns < 1e6) return `${(ns / 1e3).toFixed(1)}µs`
  if (ns < 1e9) return `${(ns / 1e6).toFixed(2)}ms`
  return `${(ns / 1e9).toFixed(3)}s`
}

// A rule's window, written the way the rules file writes it. Not `fmtDur`:
// that one measures a span *inside* a request and would print a whole minute
// as `60.000s`, where the thing being named here is the operator's own `1m`.
export function fmtWindow(ns) {
  const s = Math.round(num(ns) / 1e9)
  if (s >= 3600 && s % 3600 === 0) return `${s / 3600}h`
  if (s >= 60 && s % 60 === 0) return `${s / 60}m`
  return `${s}s`
}

// A rule's value, or its threshold, in the units the rule is written in. The
// two metrics are not interchangeable and the numbers do not say which is
// which: a ratio is a fraction, so 0.0481 printed bare reads as "under 1" when
// it means 4.81% and the rule fires at 2%.
export const fmtAlert = (metric, v) =>
  metric === 'ratio' ? (num(v) * 100).toFixed(2) + '%' : String(num(v))

// OTLP severity numbers are banded: 1-4 TRACE, 5-8 DEBUG, 9-12 INFO,
// 13-16 WARN, 17-20 ERROR, 21-24 FATAL. `severity_text` is optional, so derive
// it when the exporter left it out.
const SEV = ['trace', 'debug', 'info', 'warn', 'error', 'fatal']
export const sevName = (row) =>
  (row.severity_text || SEV[Math.floor((row.severity_number - 1) / 4)] || '').toLowerCase()

// An OTLP AnyValue can be an array or a kvlist, and the engine goes to the
// trouble of decoding both back out of the stored protobuf so they arrive here
// intact. `String()` would undo that — `[object Object]` for a kvlist, a
// comma-joined run of values for an array — so anything non-scalar is shown as
// compact JSON, which is at least the shape it was sent in. Scalars stay bare:
// quoting every string in the detail pane is noise.
export const fmtValue = (v) =>
  v !== null && typeof v === 'object' ? JSON.stringify(v) : String(v ?? '')

export const svc = (row) => (row.attributes && row.attributes['service.name']) || ''

export const STATUS = ['', 'OK', 'ERROR']

export const COLORS = ['#58a6ff', '#3fb950', '#d29922', '#f85149', '#bc8cff',
  '#39c5cf', '#ff7b72', '#a5d6ff']
