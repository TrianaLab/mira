// All view state lives in the URL hash.
//
// That is the answer to "saved views" without a saved-view store: the URL *is*
// the saved view. It pastes into a chat message and into a ticket, the back
// button works, and the server stays stateless -- which is principle 4, and the
// reason this is 30 lines instead of a table.

function parse() {
  const [path, qs] = (location.hash.slice(1) || '/logs').split('?')
  return { path, params: Object.fromEntries(new URLSearchParams(qs || '')) }
}

/// `nonce` is what makes "Run" re-run. Views key their fetch on it, so pressing
/// the button with the query unchanged still refetches -- which is the whole
/// point of pressing it on live telemetry.
export const route = $state({ ...parse(), nonce: 0 })

function sync() {
  const next = parse()
  route.path = next.path
  route.params = next.params
  route.nonce++
}

addEventListener('hashchange', sync)

/// Navigate. Empty params are dropped so the URL stays short enough to read.
export function go(path, params = {}) {
  const qs = new URLSearchParams(
    Object.entries(params).filter(([, v]) => v !== '' && v != null),
  ).toString()
  const next = '#' + path + (qs ? '?' + qs : '')
  // Assigning an unchanged hash fires no event, so drive the state directly.
  if (location.hash === next || (!location.hash && next === '#/logs')) sync0(path, params)
  else location.hash = next
}

function sync0(path, params) {
  route.path = path
  route.params = { ...params }
  route.nonce++
}
