<script>
  import { api, bounds, serviceOf, withService, REPLAY } from './lib/api.js'
  import { route, go } from './lib/route.svelte.js'
  import Records from './lib/Records.svelte'
  import Trace from './lib/Trace.svelte'
  import Metrics from './lib/Metrics.svelte'
  import ServiceMap from './lib/Map.svelte'
  import Frame from './lib/Frame.svelte'
  import Alerts from './lib/Alerts.svelte'

  let stats = $state(null)
  let names = $state([])
  let metric = $state('')

  let view = $derived(route.path.split('/')[1] || 'logs')
  let traceId = $derived(route.path.split('/')[2] || '')
  // The frame panel is a property of the query, not of the session: it lives in
  // the URL so a link to an investigation carries the correlation with it.
  let framed = $derived(route.params.frame === '1' && (view === 'logs' || view === 'traces'))

  // The inputs are local state seeded from the URL, not bound to it: typing in
  // the box must not navigate on every keystroke.
  let q = $state('')
  let range = $state('-1h')
  $effect(() => {
    q = route.params.q || ''
    range = route.params.range || '-1h'
  })

  const RANGES = ['-5m', '-15m', '-1h', '-6h', '-24h', '-7d', 'all']

  // The service list, from the entity facet (section 7.2). Distinct *names*: an entity
  // is an instance, and three replicas are three entities with one name — which
  // matters to the engine and not to someone picking a service out of a list.
  //
  // A facet read of the resource tables, so it costs tens of rows a block and
  // not a scan; cheap enough to refresh with the window rather than caching a
  // list that goes stale the moment a service is deployed.
  let services = $state([])
  $effect(() => {
    const r = range
    let live = true
    api('/api/v1/entities', bounds(r))
      .then((d) => {
        if (live) services = [...new Set(d.entities.map((e) => e.name))].sort()
      })
      // A picker that cannot be populated is a picker that is not shown. The
      // filter box does the same job by hand, and an error here would be the
      // second one on screen for the same dead server.
      .catch(() => { if (live) services = [] })
    return () => { live = false }
  })

  function pick(name) {
    q = withService(q, name)
    run()
  }
  let picked = $derived(serviceOf(q))

  // Follow mode. A refetch on a timer rather than a stream: every query on this
  // page is already a millisecond-scale read over mmap'd blocks, and a
  // WebSocket would be a second protocol, a second server path and a
  // reconnection state machine to buy back a few hundred milliseconds.
  let tail = $state(false)
  const TAIL_MS = 3000
  $effect(() => {
    if (!tail) return
    const t = setInterval(() => route.nonce++, TAIL_MS)
    return () => clearInterval(t)
  })
  // Following a range that ends an hour ago follows nothing. Absolute windows
  // are not expressible here yet, so this is only about leaving the mode on
  // across a navigation to a view that cannot tail.
  $effect(() => {
    if (view === 'trace' || view === 'metrics') tail = false
  })

  function run(e) {
    e?.preventDefault()
    const params = { q: q.trim(), range }
    if (view === 'metrics' && metric) params.name = metric
    if (route.params.frame === '1') params.frame = '1'
    go(view === 'trace' ? route.path : '/' + view, params)
  }

  const toggleFrame = () =>
    go(route.path, { ...route.params, q: q.trim(), range, frame: framed ? '' : '1' })

  function onnames(list, chosen) {
    names = list
    metric = chosen || ''
  }
</script>

{#if REPLAY}
  <!-- Only in the documentation site's build. Says what is real before the
       reader forms an opinion from a number on screen: every timing in the
       header below was measured by a real Mira when the snapshot was taken,
       and none of it is being measured now. -->
  <p class="replay">
    Recorded snapshot. The UI is the real one; the data is a capture of a
    running Mira, so the query timings are the ones it measured then, not now,
    and typed filters replay the nearest recorded answer.
    <a href="https://miradb.dev/quickstart/">Run it for real</a>
  </p>
{/if}

<header>
  <a class="brand" href="#/logs">mira</a>
  <nav>
    <!-- The range comes along; the filter does not. Terms are signal-scoped by
         FIELDS, so `severity_number>=17` carried from logs to traces turns from
         a column predicate into an attribute one and matches nothing. -->
    {#each ['logs', 'traces', 'metrics', 'map', 'alerts'] as v (v)}
      <a
        href="#/{v}?range={encodeURIComponent(range)}"
        class:on={v === (view === 'trace' ? 'traces' : view)}
      >
        {v[0].toUpperCase() + v.slice(1)}
      </a>
    {/each}
  </nav>
  <span class="grow"></span>
  {#if stats}
    <span class="stats">
      {stats.rows_matched} matched / {stats.rows_scanned} scanned ·
      {stats.blocks_scanned} of {stats.blocks_total} blocks
      {#if stats.elapsed_us != null}
        · <b>{(stats.elapsed_us / 1000).toFixed(1)} ms</b>
      {/if}
    </span>
  {/if}
</header>

<form onsubmit={run} autocomplete="off">
  {#if view !== 'map' && view !== 'alerts'}
    <input
      bind:value={q}
      aria-label="Filter"
      spellcheck="false"
      placeholder={view === 'metrics'
        ? 'http.method=GET'
        : 'service.name=checkout severity_number>=17 body~timeout'}
    />
  {/if}
  {#if (view === 'logs' || view === 'traces') && services.length}
    <select
      aria-label="Service"
      value={picked}
      onchange={(e) => pick(e.currentTarget.value)}
    >
      <option value="">all services</option>
      {#each services as s (s)}
        <option value={s}>{s}</option>
      {/each}
    </select>
  {/if}
  {#if view === 'metrics'}
    <select bind:value={metric} aria-label="Metric" onchange={run}>
      {#each names as n (n.name)}
        <option value={n.name}>{n.name}{n.unit ? ` (${n.unit})` : ''}</option>
      {/each}
    </select>
  {/if}
  <select bind:value={range} aria-label="Time range" onchange={run}>
    {#each RANGES as r (r)}
      <option value={r}>{r === 'all' ? 'all' : r.slice(1)}</option>
    {/each}
  </select>
  {#if view === 'logs' || view === 'traces'}
    <!-- One click from "these rows" to "everything around these rows". -->
    <button type="button" class="ghost" class:on={framed} onclick={toggleFrame}>
      Frame
    </button>
  {/if}
  {#if view !== 'trace' && view !== 'metrics'}
    <button
      type="button"
      class="ghost"
      class:on={tail}
      aria-pressed={tail}
      onclick={() => (tail = !tail)}
    >
      {tail ? '● Live' : 'Live'}
    </button>
  {/if}
  <button type="submit">Run</button>
</form>

<main>
  {#if view === 'trace'}
    <Trace {traceId} nonce={route.nonce} onstats={(s) => (stats = s)} />
  {:else if view === 'metrics'}
    <Metrics
      params={route.params}
      nonce={route.nonce}
      onstats={(s) => (stats = s)}
      {onnames}
    />
  {:else if view === 'map'}
    <ServiceMap params={route.params} nonce={route.nonce} onstats={(s) => (stats = s)} />
  {:else if view === 'alerts'}
    <Alerts params={route.params} nonce={route.nonce} onstats={(s) => (stats = s)} />
  {:else}
    {#if framed}
      <Frame
        signal={view === 'traces' ? 'traces' : 'logs'}
        params={route.params}
        nonce={route.nonce}
      />
    {/if}
    <Records
      signal={view === 'traces' ? 'traces' : 'logs'}
      params={route.params}
      nonce={route.nonce}
      onstats={(s) => (stats = s)}
    />
  {/if}
</main>

<style>
  header {
    display: flex;
    align-items: center;
    gap: 18px;
    padding: 10px 16px;
    border-bottom: 1px solid var(--line);
    background: var(--panel);
    position: sticky;
    top: 0;
    z-index: 2;
  }
  .brand { font-weight: 700; letter-spacing: 0.08em; color: var(--fg); }
  nav { display: flex; gap: 14px; }
  nav a { color: var(--dim); }
  nav a.on { color: var(--fg); border-bottom: 2px solid var(--accent); }
  .grow { flex: 1; }
  .stats { color: var(--dim); font-size: 11px; white-space: nowrap; }
  .stats b { color: var(--fg); font-weight: 500; }

  form { display: flex; gap: 8px; padding: 10px 16px; border-bottom: 1px solid var(--line); }
  input, select, button {
    font: inherit;
    background: var(--bg);
    color: var(--fg);
    border: 1px solid var(--line);
    border-radius: 6px;
    padding: 6px 9px;
  }
  input { flex: 1; }
  input:focus, select:focus { outline: 1px solid var(--accent); }
  button { background: var(--accent); color: #fff; border-color: transparent; cursor: pointer; }
  .ghost { background: var(--bg); color: var(--dim); border-color: var(--line); }
  .ghost.on { color: var(--fg); border-color: var(--accent); }

  main { padding: 0 16px 60px; }
</style>
