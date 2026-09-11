<script>
  import { api, bounds, parseFilter, fmtValue, COLORS } from './api.js'
  import Chart from './Chart.svelte'
  import Empty from './Empty.svelte'

  let { params, nonce, onstats, onnames } = $props()

  let series = $state([])
  let loading = $state(true)
  let error = $state('')
  // Metrics blocks on disk, from the names response — which is fetched
  // unconditionally, so this is known even when there is no metric to chart.
  // Zero means nothing has ever sent metrics here, which is a different
  // problem from a window with no points in it.
  let blocks = $state(null)

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
        const { names, stats } = await api('/api/v1/metrics/names', win)
        if (!live) return
        blocks = stats.blocks_total
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
        if (live) { error = String(e.message || e); series = []; blocks = null }
      } finally {
        if (live) loading = false
      }
    })()
    return () => { live = false }
  })

  // A histogram comes back as `<name>.count` and `<name>.sum` over the *same*
  // attributes, and those are not the same quantity: a count of 700 and a sum of
  // 23,000 milliseconds share one linear axis only by flattening the count onto
  // the x-axis — along with every exemplar, which is a duration and so lands
  // there too, unclickable. One chart per series name, and the axis is back to
  // one unit. A gauge or a sum yields one group, which is the same screen as
  // before.
  let groups = $derived.by(() => {
    const by = new Map()
    for (const s of series) {
      if (!by.has(s.name)) by.set(s.name, [])
      by.get(s.name).push(s)
    }
    return [...by].map(([name, list]) => ({ name, list }))
  })

  // Series off one metric share nearly all of their attributes — the demo's
  // twelve pods differ in `service.instance.id` and in nothing else — so a
  // legend that prints the whole set is twelve entries of three wrapped lines,
  // identical but for one token, and the one token is buried. Only what differs
  // between the lines can identify a line, so that is what each entry carries.
  let common = $derived.by(() => {
    const [first, ...rest] = series
    if (!first || !rest.length) return new Set()
    return new Set(
      Object.entries(first.attributes || {})
        .filter(([k, v]) =>
          rest.every((s) => k in (s.attributes || {}) && fmtValue(s.attributes[k]) === fmtValue(v)),
        )
        .map(([k]) => k),
    )
  })

  const pairs = (s, keys) =>
    Object.entries(s.attributes || {})
      .filter(([k]) => keys(k))
      .map(([k, v]) => `${k}=${fmtValue(v)}`)
      .join(' ')

  const attrs = (s) => pairs(s, (k) => !common.has(k))

  // Elided, not dropped: everything the entries no longer say is said once,
  // above them. Hiding an attribute in a telemetry UI is how a reader ends up
  // certain they are looking at prod when they are looking at staging.
  let shared = $derived(common.size ? pairs(series[0], (k) => common.has(k)) : '')

  const label = (s) => attrs(s) || s.name
</script>

{#if error}
  <div class="err">{error}</div>
{:else if loading}
  <div class="empty">Loading…</div>
{:else if !series.length}
  <Empty signal="metrics" {blocks} q={params.q || ''} range={params.range || '-1h'} />
{:else}
  {#if shared}<p class="shared">every series: {shared}</p>{/if}
  {#each groups as g (g.name)}
    {#if groups.length > 1}<h2>{g.name}</h2>{/if}
    <Chart series={g.list} />
    <div class="legend">
      {#each g.list as s, i (label(s))}
        <span>
          <i style="background:{COLORS[i % COLORS.length]}"></i>
          {label(s)}
          {#if s.dropped_points}
            <em>+{s.dropped_points} dropped</em>
          {/if}
        </span>
      {/each}
    </div>
  {/each}
{/if}

<style>
  .shared { margin: 10px 0 0; color: var(--dim); font-size: 11px; }
  h2 { margin: 18px 0 0; font-size: 12px; font-weight: 500; }
  .legend { display: flex; flex-wrap: wrap; gap: 6px 18px; padding: 10px 0; }
  .legend span { display: flex; align-items: center; gap: 6px; color: var(--dim); font-size: 11px; }
  i { width: 10px; height: 3px; border-radius: 2px; }
  /* Truncation is surfaced, never hidden: a chart missing its spike because the
     engine capped the series is the failure this label exists to prevent. */
  em { color: var(--warn); font-style: normal; }
</style>
