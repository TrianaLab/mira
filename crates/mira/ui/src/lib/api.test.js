// `node --test`. Node's own runner, so the browser UI gets tests without the
// repo gaining a test framework, a config file and a lockfile entry for one
// pure function.
//
// Only `parseFilter` is covered, deliberately: it is the one piece of this
// bundle that can be wrong *silently*. Everything else here either renders
// visibly or throws.

import test from 'node:test'
import assert from 'node:assert/strict'

import { parseFilter } from './api.js'

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

test('what cannot be placed is an error, never a dropped term', () => {
  // Metrics has no message column.
  assert.throws(() => parseFilter('refused', 'metrics'), /needs an operator/)
  // A half-written term: the operator is there, the key is not.
  assert.throws(() => parseFilter('=v', 'logs'), /needs an operator/)
})
