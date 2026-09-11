// `node --test`. Node's own runner, so the browser UI gets tests without the
// repo gaining a test framework, a config file and a lockfile entry for one
// pure function.
//
// Coverage follows one rule: the pieces of this bundle that can be wrong
// *silently*. `parseFilter` turns a real column into a predicate that matches
// nothing; `fmtValue` turns a nested attribute into `[object Object]`; `num`
// and `fmtDur` are what stand between a response full of int64-as-string and a
// screen full of `NaN`; and `api`'s two failure paths are the difference
// between a diagnosis and "Failed to fetch". Rendering is not covered — there
// is no DOM here on purpose, since one costs a devDependency and a config file
// for a component tree this small.

import test from 'node:test'
import assert from 'node:assert/strict'

import {
  addTerm, api, fmtAlert, fmtDur, fmtTime, fmtValue, fmtWindow, get, layer, num, parseFilter,
  serviceOf,
  term, withService,
} from './api.js'

test('operators split longest-first and route to field or attr', () => {
  assert.deepEqual(
    parseFilter('service.name=checkout severity_number>=17 http.route~/api', 'logs'),
    [
      { attr: 'service.name', eq: 'checkout' },
      { field: 'severity_number', gte: 17 },
      { attr: 'http.route', contains: '/api' },
    ],
  )
  // `!=` must not read as `=` with a key ending in `!`.
  assert.deepEqual(parseFilter('k!=v', 'logs'), [{ attr: 'k', ne: 'v' }])
  // The same key is a column on one signal and an attribute on another.
  assert.deepEqual(parseFilter('name=checkout', 'traces'), [{ field: 'name', eq: 'checkout' }])
  assert.deepEqual(parseFilter('name=checkout', 'logs'), [{ attr: 'name', eq: 'checkout' }])
  // An explicit prefix overrides the guess.
  assert.deepEqual(parseFilter('field:name=x', 'logs'), [{ field: 'name', eq: 'x' }])
})

test('a word with no operator searches the signal message column', () => {
  assert.deepEqual(parseFilter('refused', 'logs'), [{ field: 'body', contains: 'refused' }])
  assert.deepEqual(parseFilter('checkout', 'traces'), [{ field: 'name', contains: 'checkout' }])
  assert.deepEqual(
    parseFilter('"connection refused" a=b', 'logs'),
    [{ field: 'body', contains: 'connection refused' }, { attr: 'a', eq: 'b' }],
  )
  // Text stays text. `body` is a string column and an integer there would
  // match nothing, silently — the one outcome this box must never produce.
  assert.deepEqual(parseFilter('500', 'logs'), [{ field: 'body', contains: '500' }])
})

test('ids stay strings however numeric they look', () => {
  // An all-decimal span id is the case that breaks silently: the engine unhexes
  // a string and an integer clears the selection instead of erroring.
  assert.deepEqual(
    parseFilter('span_id=0000000000001234', 'logs'),
    [{ field: 'span_id', eq: '0000000000001234' }],
  )
  // 32 digits is past 2^53, so coercing would corrupt the value as well.
  assert.deepEqual(
    parseFilter('trace_id=00000000000002705555555555555725', 'traces'),
    [{ field: 'trace_id', eq: '00000000000002705555555555555725' }],
  )
  // The exemption is on the key, not the column: attributes ending in `.id`
  // hold ids too, and the prefix must not hide the suffix.
  assert.deepEqual(parseFilter('peer.id=42', 'logs'), [{ attr: 'peer.id', eq: '42' }])
  assert.deepEqual(parseFilter('attr:span_id=42', 'traces'), [{ attr: 'span_id', eq: '42' }])
  // Everything else still gets typed.
  assert.deepEqual(parseFilter('http.status_code=500', 'logs'), [
    { attr: 'http.status_code', eq: 500 },
  ])
})

test('event_name and the dropped counts are columns, not attributes', () => {
  // Read as an attribute this is worse than wrong: the block gets Bloom-pruned
  // on a key no attribute table holds, so it is a confidently empty result.
  assert.deepEqual(parseFilter('event_name=user.login', 'logs'), [
    { field: 'event_name', eq: 'user.login' },
  ])
  assert.deepEqual(parseFilter('dropped_events_count>0', 'traces'), [
    { field: 'dropped_events_count', gt: 0 },
  ])
})

test('a nested AnyValue survives rendering', () => {
  // What the engine decodes out of a kvlist and an array attribute. `String()`
  // gave `[object Object]` and `mira,--data-dir` respectively.
  assert.equal(fmtValue({ role: 'user', content: 'hello' }), '{"role":"user","content":"hello"}')
  assert.equal(fmtValue(['mira', '--data-dir']), '["mira","--data-dir"]')
  // Scalars stay bare, and a missing body renders as an empty cell.
  assert.equal(fmtValue('connection refused'), 'connection refused')
  assert.equal(fmtValue(0), '0')
  assert.equal(fmtValue(false), 'false')
  assert.equal(fmtValue(undefined), '')
  assert.equal(fmtValue(null), '')
})

test('what cannot be placed is an error, never a dropped term', () => {
  // Metrics has no message column.
  assert.throws(() => parseFilter('refused', 'metrics'), /needs an operator/)
  // A half-written term: the operator is there, the key is not.
  assert.throws(() => parseFilter('=v', 'logs'), /needs an operator/)
})

test('64-bit integers arrive as strings and still render', () => {
  // Every one of these is a real response value: the engine emits an int64 as
  // a JSON string, because 1.7e18 does not survive a double. Formatting has to
  // take the string form, or the whole screen reads `NaN` and `Invalid Date`.
  const ns = '1757462096123456789'
  assert.equal(fmtTime(ns), fmtTime(Number(ns)))
  assert.match(fmtTime(ns), /^\d\d-\d\d \d\d:\d\d:\d\d\.\d\d\d$/)

  // `"500" < 1e3` happens to coerce, so the bug this pins is the *branch* body:
  // template interpolation of the raw string, and `.toFixed` on it.
  assert.equal(fmtDur('500'), '500ns')
  assert.equal(fmtDur('1500'), '1.5µs')
  assert.equal(fmtDur('2500000'), '2.50ms')
  assert.equal(fmtDur('3000000000'), '3.000s')
  // A column the engine left out must not paint the string "NaNns".
  assert.equal(fmtDur(undefined), '')
  assert.equal(fmtDur(null), '')

  // `num` keeps null distinguishable from zero: a metric point of `null` is a
  // gap the engine refused to interpolate across, and `Number(null)` is 0,
  // which would draw a line down to the axis and back.
  assert.equal(num('42'), 42)
  assert.equal(num(0), 0)
  assert.equal(num(null), null)
  assert.equal(num(undefined), null)
})

test('a value clicked out of a detail pane round-trips into the filter', () => {
  // The contract: whatever `term` writes, `parseFilter` reads back as the
  // predicate that finds the row it came from. Column-versus-attribute is
  // decided by the same FIELDS table on both sides, so neither has to be told.
  const round = (k, v, signal) => parseFilter(term(k, v), signal)
  assert.deepEqual(round('service.name', 'checkout', 'logs'), [
    { attr: 'service.name', eq: 'checkout' },
  ])
  // Unquoted, so the zone map can still prune on it (see `term`).
  assert.deepEqual(round('severity_number', 17, 'logs'), [{ field: 'severity_number', eq: 17 }])
  assert.deepEqual(round('flags', 0, 'logs'), [{ field: 'flags', eq: 0 }])
  assert.deepEqual(round('feature.on', true, 'logs'), [{ attr: 'feature.on', eq: true }])
  // A body with spaces in it is why the quotes are there at all.
  assert.deepEqual(round('body', 'connection refused', 'logs'), [
    { field: 'body', eq: 'connection refused' },
  ])
  // An id stays a string: 16 decimal digits is past 2^53, and the engine's
  // unhex path only takes the string form.
  assert.deepEqual(round('span_id', '0000000000001234', 'traces'), [
    { field: 'span_id', eq: '0000000000001234' },
  ])
  // A value carrying the delimiter has no equality spelling in this
  // mini-language, so it degrades to a prefix match rather than to a term that
  // throws — this runs in a click handler.
  assert.deepEqual(round('body', 'said "no" twice', 'logs'), [{ field: 'body', contains: 'said ' }])
})

test('adding a term never rewrites what is already in the box', () => {
  assert.equal(addTerm('', 'a="b"'), 'a="b"')
  assert.equal(addTerm(undefined, 'a="b"'), 'a="b"')
  assert.equal(addTerm('x=1', 'a="b"'), 'x=1 a="b"')
  // Clicking the same value twice is a double-click, not `a="b" a="b"`.
  assert.equal(addTerm('x=1 a="b"', 'a="b"'), 'x=1 a="b"')
})

test('the service picker replaces its own term rather than anding a second', () => {
  // Two `service.name=` equalities match nothing, which reads as "this service
  // is silent" — the one wrong answer a picker must never produce.
  assert.equal(withService('service.name="a"', 'b'), 'service.name="b"')
  assert.equal(withService('body~timeout service.name="a"', 'b'), 'body~timeout service.name="b"')
  assert.equal(withService('body~timeout', 'b'), 'body~timeout service.name="b"')
  assert.equal(withService('', 'b'), 'service.name="b"')
  // "all services" drops it and leaves the rest of the filter alone.
  assert.equal(withService('body~timeout service.name="a"', ''), 'body~timeout')
  // What the <select> shows as selected has to be what the box says, quoted or
  // not, or picking a service appears to do nothing.
  assert.equal(serviceOf('body~x service.name="checkout"'), 'checkout')
  assert.equal(serviceOf('service.name=checkout body~x'), 'checkout')
  assert.equal(serviceOf('body~x'), '')
  assert.equal(serviceOf(withService('a=1', 'gw')), 'gw')
})

test('the service map lays out cycles instead of hanging on them', () => {
  const n = (...keys) => keys.map((key) => ({ key }))
  const e = (...pairs) => pairs.map(([from, to]) => ({ from, to }))

  // The ordinary shape: one column per hop.
  const chain = layer(n('entry', 'a', 'b', 'c'), e(['entry', 'a'], ['a', 'b'], ['b', 'c']))
  assert.deepEqual([...chain.values()], [0, 1, 2, 3])

  // Longest path, not shortest: `c` is called both directly by `a` and through
  // `b`, and drawing it in `b`'s column runs an edge straight through a box.
  const diamond = layer(
    n('entry', 'a', 'b', 'c'),
    e(['entry', 'a'], ['a', 'b'], ['a', 'c'], ['b', 'c']),
  )
  assert.equal(diamond.get('c'), 3)

  // A retry loop. Relaxing to a fixed point never terminates on this; the
  // node-count bound is what keeps it a render path rather than a frozen tab.
  const cyc = layer(n('entry', 'a', 'b'), e(['entry', 'a'], ['a', 'b'], ['b', 'a']))
  assert.equal(cyc.size, 3)
  for (const d of cyc.values()) assert.ok(Number.isFinite(d))

  // The map's span budget can cut a sample mid-trace, so an edge may name a
  // node that never made it into the node list.
  const partial = layer(n('entry', 'a'), e(['entry', 'a'], ['a', 'ghost'], ['ghost', 'a']))
  assert.deepEqual([...partial.keys()], ['entry', 'a'])
})

test('a failed request says which kind of failure it was', async () => {
  const origin = 'http://127.0.0.1:4318'
  globalThis.location = { origin }
  const stub = (fn) => { globalThis.fetch = fn }

  // The server is not running. The browser's own wording for this names
  // neither the cause nor the fix, and it is the first thing a new user hits.
  stub(() => Promise.reject(new TypeError('Failed to fetch')))
  await assert.rejects(api('/api/v1/query', {}), (e) => {
    assert.match(e.message, /Cannot reach http:\/\/127\.0\.0\.1:4318/)
    assert.match(e.message, /server has stopped/)
    return true
  })

  // A rejected query. The engine's own message is the useful part, and the
  // status is what says whose fault it is.
  stub(() => Promise.resolve(new Response('{"error":"unknown field \\"svc\\""}', { status: 400 })))
  await assert.rejects(api('/api/v1/query', {}), /^Error: HTTP 400: unknown field "svc"$/)

  // A 500 comes back as text/plain, not JSON. Losing that body to a parse
  // error would report the wrong problem entirely.
  stub(() => Promise.resolve(new Response('query task panicked', { status: 500 })))
  await assert.rejects(api('/api/v1/query', {}), /query task panicked/)

  stub(() => Promise.resolve(new Response('{"rows":[]}', { status: 200 })))
  assert.deepEqual(await api('/api/v1/query', {}), { rows: [] })
})

test('a rule window reads back as the duration the rules file spelled', () => {
  // The whole point of not using `fmtDur`: an alert's `over: 1m` has to render
  // as the operator wrote it, not as `60.000s`.
  assert.equal(fmtWindow('60000000000'), '1m')
  assert.equal(fmtWindow('300000000000'), '5m')
  assert.equal(fmtWindow('3600000000000'), '1h')
  assert.equal(fmtWindow('7200000000000'), '2h')
  // Not a whole minute, and not a whole hour: seconds and minutes respectively,
  // never a rounded lie about which window the rule actually used.
  assert.equal(fmtWindow('30000000000'), '30s')
  assert.equal(fmtWindow('90000000000'), '90s')
  assert.equal(fmtWindow('5400000000000'), '90m')
  assert.equal(fmtWindow('0'), '0s')
})

test('a GET carries no body and fails the same way a query does', async () => {
  globalThis.location = { origin: 'http://127.0.0.1:4318' }
  let seen = null
  globalThis.fetch = (path, init) => {
    seen = { path, init }
    return Promise.resolve(new Response('{"alerts":[]}', { status: 200 }))
  }
  assert.deepEqual(await get('/api/v1/alerts'), { alerts: [] })
  assert.equal(seen.path, '/api/v1/alerts')
  // No method and no body: a `POST {}` to this route is a 405, and a GET with a
  // `content-type` header is a preflight nobody needs.
  assert.deepEqual(seen.init, {})

  // The one shared failure path, reached from the other entry point too.
  globalThis.fetch = () => Promise.reject(new TypeError('Load failed'))
  await assert.rejects(get('/api/v1/alerts'), /server has stopped/)
})

test('the filter an alert reports parses back into the terms it counted', () => {
  // Pinned against `alert::filter_of`, which has the mirror of this list in
  // `the_shipped_example_rules_file_parses`. A rule's threshold and the rows a
  // click on it opens have to be the same predicate, and the two spellings are
  // produced by different code in different languages -- so drift here is
  // silent: the page opens, and shows the wrong rows.
  assert.deepEqual(parseFilter('field:status_code=2', 'traces'), [
    { field: 'status_code', eq: 2 },
  ])
  assert.deepEqual(
    parseFilter('attr:service.name=checkout field:duration_nano>250000000', 'traces'),
    [{ attr: 'service.name', eq: 'checkout' }, { field: 'duration_nano', gt: 250000000 }],
  )
  assert.deepEqual(parseFilter('attr:exception.type=payments.CardDeclined', 'traces'), [
    { attr: 'exception.type', eq: 'payments.CardDeclined' },
  ])
  assert.deepEqual(
    parseFilter('attr:service.name=inventory field:severity_number>=21', 'logs'),
    [{ attr: 'service.name', eq: 'inventory' }, { field: 'severity_number', gte: 21 }],
  )
  // A value with a space is quoted on the way out, and has to survive the trip
  // back -- unquoted it would tokenise as two terms, the second of them junk.
  assert.deepEqual(parseFilter('field:name="GET /health"', 'traces'), [
    { field: 'name', eq: 'GET /health' },
  ])
})

test('a ratio rule reads as a percentage and a count rule does not', () => {
  // The bug this exists to catch is a silent one: both are JSON numbers, and
  // 0.0481 next to a threshold of 0.02 reads as two small numbers rather than
  // as "4.81% against a 2% line".
  assert.equal(fmtAlert('ratio', 0.0481), '4.81%')
  assert.equal(fmtAlert('ratio', 0.02), '2.00%')
  assert.equal(fmtAlert('ratio', 0), '0.00%')
  assert.equal(fmtAlert('ratio', 1), '100.00%')
  // A count is a count. `20.0` and `20` are the same JSON number, and neither
  // may come out as `2000.00%`.
  assert.equal(fmtAlert('count', 20), '20')
  assert.equal(fmtAlert('count', 0), '0')
})
