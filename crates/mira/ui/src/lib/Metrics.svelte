<script>
  import { api, bounds, parseFilter, COLORS } from './api.js'
  import Chart from './Chart.svelte'

  let { params, nonce, onstats, onnames } = $props()

  let series = $state([])
  let loading = $state(true)
  let error = $state('')

  $effect(() => {
    const q = params.q || ''
    const range = params.range || '-1h'
    const wanted = params.name || ''
    void nonce

    let live = true
    loading = true
    error = ''
    ;(async () => {
      try {
        const win = bounds(range)
        // The name list is its own endpoint because the descriptor table is tens
        // of rows per block while the points it describes are hundreds of
        // thousands. Populating a dropdown should not read the points.
        const { names } = await api('/api/v1/metrics/names', win)
        if (!live) return
        const name = names.some((n) => n.name === wanted) ? wanted : names[0]?.name
        onnames(names, name)
        if (!name) {
          series = []
          onstats(null)
          return
        }
        const r = await api('/api/v1/metrics/query', {
          name,
          ...win,
          where: parseFilter(q, 'metrics'),
        })
        if (!live) return
        series = r.series
        onstats(r.stats)
      } catch (e) {
        if (live) { error = String(e.message || e); series = [] }
      } finally {
        if (live) loading = false
      }
    })()
    return () => { live = false }
  })

  const label = (s) =>
    Object.entries(s.attributes || {})
      .map(([k, v]) => `${k}=${v}`)
      .join(' ') || s.name
</script>

{#if error}
  <div class="err">{error}</div>
{:else if loading}
  <div class="empty">Loading…</div>
{:else if !series.length}
  <div class="empty">No series in this window.</div>
{:else}
  <Chart {series} />
  <div class="legend">
    {#each series as s, i (s.name + label(s))}
      <span>
        <i style="background:{COLORS[i % COLORS.length]}"></i>
        {label(s)}
        {#if s.dropped_points}
          <em>+{s.dropped_points} dropped</em>
        {/if}
      </span>
    {/each}
  </div>
{/if}

<style>
  .legend { display: flex; flex-wrap: wrap; gap: 6px 18px; padding: 10px 0; }
  .legend span { display: flex; align-items: center; gap: 6px; color: var(--dim); font-size: 11px; }
  i { width: 10px; height: 3px; border-radius: 2px; }
  /* Truncation is surfaced, never hidden: a chart missing its spike because the
     engine capped the series is the failure this label exists to prevent. */
  em { color: var(--warn); font-style: normal; }
</style>
