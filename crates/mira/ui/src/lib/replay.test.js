// The recorded-snapshot data source, tested against the committed fixtures.
//
// Own file rather than a block in `api.test.js`, because importing this pulls
// 2.3 MB of JSON into the runner and the other tests have no reason to pay it.
//
// One rule again: the ways this can be wrong *silently*. A key that stops
// matching what the capture wrote produces an empty table, not an error, and
// an empty table on a demo page reads as "the product does not work". So the
// keys are asserted against the real file, both fallbacks are asserted to
// resolve, and the last page is asserted to have no cursor -- without that the
// "Load more" button pages onto itself forever.

import test from 'node:test'
import assert from 'node:assert/strict'

import fixtures from './fixtures.json' with { type: 'json' }
import { key, replay } from './replay.js'

const WIN = { from: '-1h', to: 'now' }

test('every key the UI derives is a key the capture recorded', () => {
  // Exactly the requests App.svelte issues on load and on the obvious clicks.
  const asked = [
    key('/api/v1/alerts', {}),
    key('/api/v1/entities', WIN),
    key('/api/v1/map', WIN),
    key('/api/v1/metrics/names', WIN),
    key('/api/v1/query', { signal: 'logs', ...WIN, where: [] }),
    key('/api/v1/query', { signal: 'traces', ...WIN, where: [] }),
    key('/api/v1/query', { signal: 'logs', ...WIN, where: [], after: 'cursor' }),
    key('/api/v1/query', { signal: 'traces', ...WIN, where: [], after: 'cursor' }),
    key('/api/v1/correlate', { signal: 'logs', ...WIN }),
    key('/api/v1/correlate', { signal: 'traces', ...WIN }),
  ]
  for (const k of asked) assert.ok(fixtures[k], `no fixture for ${k}`)
})

test('a trace the capture never saw still opens a waterfall', () => {
  // The traces table links to more trace ids than the capture records, and a
  // row that opens an empty pane is the worst version of this page: it looks
  // like a bug in the waterfall rather than a gap in a recording.
  const unseen = { signal: 'traces', where: [{ field: 'trace_id', eq: 'ff'.repeat(16) }] }
  assert.equal(key('/api/v1/query', unseen), `query:trace:${'ff'.repeat(16)}`)
  assert.ok(replay('/api/v1/query', unseen).rows.length > 0)
})

test('a metric the capture never saw still draws a chart', () => {
  assert.ok(replay('/api/v1/metrics/query', { name: 'no.such.metric' }).series.length > 0)
})

test('the last page carries no cursor', () => {
  // `Records.svelte` shows "Load more" exactly when the response has a `next`.
  // The capture blanks it on the last page it recorded; if that ever stops
  // happening the button replays page two until the reader gives up.
  for (const sig of ['logs', 'traces']) {
    const last = replay('/api/v1/query', { signal: sig, where: [], after: 'cursor' })
    assert.equal(last.next, '', `query:${sig}:2 still has a cursor`)
    assert.ok(last.rows.length > 0)
  }
})

test('a request nobody recorded answers empty rather than failing', () => {
  const empty = replay('/api/v1/nothing-like-this', {})
  assert.deepEqual(empty.rows, [])
  assert.equal(empty.stats.elapsed_us, 0)
})

test('a replayed response is a copy, because components mutate what they get', () => {
  // `Records.svelte` appends the next page onto `rows`. Handing out the
  // fixture itself would grow it on every visit, which shows up as a table
  // that gets longer each time the reader switches tabs.
  const first = replay('/api/v1/query', { signal: 'logs', where: [] })
  const n = first.rows.length
  first.rows.push({ body: 'not from the capture' })
  assert.equal(replay('/api/v1/query', { signal: 'logs', where: [] }).rows.length, n)
})
