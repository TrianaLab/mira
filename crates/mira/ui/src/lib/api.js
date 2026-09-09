// The API client, the filter mini-language, and formatting. No Svelte in here
// on purpose: this is the part with logic worth testing without a DOM.

export async function api(path, body) {
  const r = await fetch(path, {
    method: 'POST',
    headers: { 'content-type': 'application/json' },
    // JSON.stringify output is valid KYAML, which is why one parser on the
    // server serves both this and the config file.
    body: JSON.stringify(body),
  })
  const text = await r.text()
  let j
  try {
    j = JSON.parse(text)
  } catch {
    throw new Error(text || r.statusText)
  }
  if (!r.ok) throw new Error(j.error || r.statusText)
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

// ---------------------------------------------------------------- formatting

export function fmtTime(ns) {
  const d = new Date(Number(ns) / 1e6)
  const p = (n, w = 2) => String(n).padStart(w, '0')
  return `${p(d.getMonth() + 1)}-${p(d.getDate())} ${p(d.getHours())}:` +
    `${p(d.getMinutes())}:${p(d.getSeconds())}.${p(d.getMilliseconds(), 3)}`
}

export function fmtDur(ns) {
  if (ns < 1e3) return `${ns}ns`
  if (ns < 1e6) return `${(ns / 1e3).toFixed(1)}µs`
  if (ns < 1e9) return `${(ns / 1e6).toFixed(2)}ms`
  return `${(ns / 1e9).toFixed(3)}s`
}

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
