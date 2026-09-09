<script>
  import { route, go } from './lib/route.svelte.js'
  import Records from './lib/Records.svelte'
  import Trace from './lib/Trace.svelte'
  import Metrics from './lib/Metrics.svelte'

  let stats = $state(null)
  let names = $state([])
  let metric = $state('')

  let view = $derived(route.path.split('/')[1] || 'logs')
  let traceId = $derived(route.path.split('/')[2] || '')

  // The inputs are local state seeded from the URL, not bound to it: typing in
  // the box must not navigate on every keystroke.
  let q = $state('')
  let range = $state('-1h')
  $effect(() => {
    q = route.params.q || ''
    range = route.params.range || '-1h'
  })

  const RANGES = ['-5m', '-15m', '-1h', '-6h', '-24h', '-7d', 'all']

  function run(e) {
    e?.preventDefault()
    const params = { q: q.trim(), range }
    if (view === 'metrics' && metric) params.name = metric
    go(view === 'trace' ? route.path : '/' + view, params)
  }

  function onnames(list, chosen) {
    names = list
    metric = chosen || ''
  }
</script>

<header>
  <a class="brand" href="#/logs">mira</a>
  <nav>
    <!-- The range comes along; the filter does not. Terms are signal-scoped by
         FIELDS, so `severity_number>=17` carried from logs to traces turns from
         a column predicate into an attribute one and matches nothing. -->
    {#each ['logs', 'traces', 'metrics'] as v (v)}
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
    </span>
  {/if}
</header>

<form onsubmit={run} autocomplete="off">
  <input
    bind:value={q}
    aria-label="Filter"
    spellcheck="false"
    placeholder={view === 'metrics'
      ? 'http.method=GET'
      : 'service.name=checkout severity_number>=17 body~timeout'}
  />
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
  {:else}
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

  main { padding: 0 16px 60px; }
</style>
