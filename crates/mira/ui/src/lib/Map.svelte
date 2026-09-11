<script>
  import { api, bounds, fmtDur, layer, num } from './api.js'
  import { go } from './route.svelte.js'

  let { params, nonce, onstats } = $props()

  let graph = $state(null)
  let loading = $state(true)
  let error = $state('')

  $effect(() => {
    const range = params.range || '-1h'
    void nonce
    let live = true
    loading = true
    error = ''
    ;(async () => {
      try {
        const r = await api('/api/v1/map', bounds(range))
        if (!live) return
        graph = r.map
        onstats(r.stats)
      } catch (e) {
        if (live) { error = String(e.message || e); graph = null; onstats(null) }
      } finally {
        if (live) loading = false
      }
    })()
    return () => { live = false }
  })

  // Columns from `layer`, rows by arrival. Deliberately not force-directed:
  // those move every node when one edge changes, and this page refreshes on a
  // timer — a graph that reshuffles itself every three seconds cannot be read.
  const ROW = 62
  const H = 34
  const GAP = 68

  const sub = (n) => `${n.spans} spans · ${n.errors} err · ${fmtDur(n.avg_nano)}`

  // The box is sized from its longest line rather than fixed, because SVG text
  // neither wraps nor clips: a box too narrow for its content does not truncate
  // it, it draws it over the edge leaving the node and over the node it arrives
  // at. Monospace is what makes the width knowable without measuring — 12px
  // `ui-monospace` advances 0.6em — and the 18 is the two 9px gutters.
  const CH = 7.3
  let W = $derived(
    Math.max(
      132,
      ...(graph?.nodes || []).map((n) => 18 + CH * Math.max(n.name.length, sub(n).length)),
    ),
  )
  let COL = $derived(W + GAP)

  let laid = $derived.by(() => {
    if (!graph?.nodes.length) return null
    // `entry` is a synthetic node (frame.rs) and belongs on screen: a map that
    // hides where traffic arrives cannot be read left to right.
    const nodes = [{ key: 'entry', name: 'entry', spans: 0, errors: 0 }, ...graph.nodes]
    const depth = layer(nodes, graph.edges)
    const cols = new Map()
    for (const n of nodes) {
      const d = depth.get(n.key)
      if (!cols.has(d)) cols.set(d, [])
      cols.get(d).push(n)
    }
    const at = new Map()
    for (const [d, list] of cols) {
      list.forEach((n, i) => at.set(n.key, { x: 20 + d * COL, y: 20 + i * ROW, n }))
    }
    const rows = Math.max(...[...cols.values()].map((l) => l.length))
    return {
      at,
      placed: [...at.values()],
      width: 40 + (cols.size - 1) * COL + W,
      height: 40 + (rows - 1) * ROW + H,
    }
  })

  // Thickness by call volume on a log scale: a 10,000-call edge next to a
  // 3-call one is unreadable linearly, and the question the width answers is
  // "which way does the traffic mostly go", not "how much exactly".
  const busiest = $derived(Math.max(1, ...(graph?.edges || []).map((e) => e.calls)))
  const width = (e) => 1 + 4 * (Math.log1p(e.calls) / Math.log1p(busiest))

  function path(e) {
    const a = laid.at.get(e.from)
    const b = laid.at.get(e.to)
    if (!a || !b) return ''
    const [x0, y0] = [a.x + W, a.y + H / 2]
    const [x1, y1] = [b.x, b.y + H / 2]
    // A backward edge would otherwise be drawn straight through the boxes
    // between its endpoints; bowing it out below is enough to follow by eye.
    const bow = x1 <= x0 ? Math.max(30, (y1 - y0) / 2 + 40) : 0
    const mx = (x0 + x1) / 2
    return `M${x0},${y0} C${mx + bow},${y0 + bow} ${mx - bow},${y1 + bow} ${x1},${y1}`
  }

  const rate = (e) => (e.calls ? e.errors / e.calls : 0)
  const pct = (v) => (v * 100).toFixed(v >= 0.1 ? 0 : 1) + '%'

  // Clicking a service is the point of the map: it is how you get from "the
  // graph says checkout is red" to the log lines that say why.
  const drill = (n) =>
    go('/logs', { q: `service.name="${n.name}"`, range: params.range || '-1h' })
</script>

{#if error}
  <div class="err">{error}</div>
{:else if loading && !graph}
  <div class="empty">Loading…</div>
{:else if !laid}
  <div class="empty">
    No spans in this window. The map is built from <code>parent_span_id</code> at
    read time, so it needs traces — send some and it appears.
  </div>
{:else}
  <div class="bar">
    {graph.nodes.length} services · {graph.edges.length} edges
    {#if num(graph.unresolved) > 0}
      <!-- An unresolved span is one whose parent was not in the sample, so
           every edge on screen is a lower bound. Saying how many is the only
           way a reader can tell a thin dependency from a truncated read. -->
      <span class="warn">
        · {graph.unresolved} spans with a parent outside the sample
      </span>
    {/if}
  </div>
  <svg viewBox="0 0 {laid.width} {laid.height}" style="height:{laid.height}px">
    <defs>
      <marker id="a" viewBox="0 0 8 8" refX="7" refY="4" markerWidth="6"
              markerHeight="6" orient="auto-start-reverse">
        <path d="M0,0 L8,4 L0,8 z" fill="var(--dim)" />
      </marker>
    </defs>
    {#each graph.edges as e (e.from + '>' + e.to)}
      <path
        d={path(e)}
        class="edge"
        class:bad={rate(e) > 0}
        stroke-width={width(e)}
        marker-end="url(#a)"
      >
        <title>{e.calls} calls · {e.errors} errors ({pct(rate(e))}) · avg {fmtDur(e.avg_nano)} · max {fmtDur(e.max_nano)}</title>
      </path>
    {/each}
    {#each laid.placed as { x, y, n } (n.key)}
      <g
        class="node"
        class:entry={n.key === 'entry'}
        transform="translate({x},{y})"
        role="button"
        tabindex="0"
        onclick={() => n.key !== 'entry' && drill(n)}
        onkeydown={(ev) => {
          if (ev.key === 'Enter' && n.key !== 'entry') drill(n)
        }}
      >
        <rect width={W} height={H} rx="6" />
        <text x="9" y="14">{n.name}</text>
        {#if n.key !== 'entry'}
          <text x="9" y="27" class="sub" class:bad={n.errors > 0}>{sub(n)}</text>
          <title>{n.name} — click to see its logs</title>
        {/if}
      </g>
    {/each}
  </svg>
{/if}

<style>
  .bar { padding: 12px 0 4px; color: var(--dim); }
  .warn { color: var(--warn); }
  svg { width: 100%; display: block; }
  .edge { fill: none; stroke: var(--dim); opacity: 0.7; }
  .edge.bad { stroke: var(--err); opacity: 1; }
  .node rect { fill: var(--panel); stroke: var(--line); }
  .node { cursor: pointer; }
  .node.entry { cursor: default; }
  .node.entry rect { fill: none; stroke-dasharray: 3 3; }
  .node:not(.entry):hover rect { stroke: var(--accent); }
  .node:focus-visible { outline: none; }
  .node:focus-visible rect { stroke: var(--accent); stroke-width: 2; }
  text { fill: var(--fg); font: 12px ui-monospace, monospace; }
  .sub { fill: var(--dim); font-size: 10px; }
  .sub.bad { fill: var(--err); }
</style>
