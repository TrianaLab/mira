<script>
  import { api, fmtDur, svc } from './api.js'
  import Detail from './Detail.svelte'

  let { traceId, nonce, onstats } = $props()

  let rows = $state([])
  let loading = $state(true)
  let error = $state('')
  let open = $state(new Set())

  $effect(() => {
    const id = traceId
    void nonce
    let live = true
    loading = true
    error = ''
    open = new Set()
    ;(async () => {
      try {
        // Every span of a trace is one ordinary search. The waterfall is a
        // rendering of that result, not a second endpoint: `parent_span_id` is
        // a root column, so the tree is already in the rows.
        const r = await api('/api/v1/query', {
          signal: 'traces',
          from: 0,
          to: 'now',
          where: [{ field: 'trace_id', eq: id }],
          limit: 2000,
        })
        if (!live) return
        rows = r.rows
        onstats(r.stats)
      } catch (e) {
        if (live) { error = String(e.message || e); rows = [] }
      } finally {
        if (live) loading = false
      }
    })()
    return () => { live = false }
  })

  const start = (r) => Number(r.start_time_unix_nano)
  const end = (r) => start(r) + Number(r.duration_nano)

  // Flatten the tree once, depth-first, into the order the rows render in.
  let laid = $derived.by(() => {
    if (!rows.length) return { list: [], t0: 0, span: 1 }
    const t0 = Math.min(...rows.map(start))
    const span = Math.max(1, Math.max(...rows.map(end)) - t0)

    // A span whose parent is absent -- a partial trace, which is normal
    // mid-flight -- is treated as a root so it still renders.
    const byId = new Set(rows.map((r) => r.span_id))
    const kids = new Map()
    for (const r of rows) {
      const k = r.parent_span_id && byId.has(r.parent_span_id) ? r.parent_span_id : '\0'
      if (!kids.has(k)) kids.set(k, [])
      kids.get(k).push(r)
    }
    for (const l of kids.values()) l.sort((a, b) => start(a) - start(b))

    const list = []
    const seen = new Set()
    const walk = (parent, depth) => {
      for (const r of kids.get(parent) || []) {
        // Guard against a cycle in hostile or corrupted span ids; without it a
        // self-parenting span hangs the tab.
        if (seen.has(r.span_id)) continue
        seen.add(r.span_id)
        list.push({ r, depth })
        walk(r.span_id, depth + 1)
      }
    }
    walk('\0', 0)
    return { list, t0, span }
  })

  function toggle(id) {
    const next = new Set(open)
    next.has(id) ? next.delete(id) : next.add(id)
    open = next
  }
</script>

{#if error}
  <div class="err">{error}</div>
{:else if loading}
  <div class="empty">Loading…</div>
{:else if !rows.length}
  <div class="empty">Trace not found in retention.</div>
{:else}
  <div class="crumb">
    <a href="#/traces">← spans</a>
    · {traceId} · {rows.length} spans · {fmtDur(laid.span)}
  </div>
  <div class="wf">
    {#each laid.list as { r, depth } (r.span_id)}
      <div
        class="row"
        role="button"
        tabindex="0"
        onclick={() => toggle(r.span_id)}
        onkeydown={(e) => e.key === 'Enter' && toggle(r.span_id)}
      >
        <div class="lbl" title="{svc(r)} {r.name}" style="padding-left:{depth * 14}px">
          <span class="t">{svc(r)}</span>
          {r.name || ''}
        </div>
        <div class="track">
          <div
            class="bar"
            class:err={r.status_code === 2}
            style="left:{((start(r) - laid.t0) / laid.span) * 100}%;
                   width:{Math.max((Number(r.duration_nano) / laid.span) * 100, 0.2)}%"
          ></div>
        </div>
        <div class="dur">{fmtDur(Number(r.duration_nano))}</div>
      </div>
      {#if open.has(r.span_id)}
        <div class="detail"><Detail row={r} /></div>
      {/if}
    {/each}
  </div>
{/if}

<style>
  /* The bar is positioned in percent of the trace's own duration, so the layout
     needs no measurement pass and survives a window resize for free. */
  .wf { margin-top: 12px; }
  .row { display: flex; align-items: center; gap: 10px; padding: 2px 0; cursor: pointer; }
  .row:hover { background: var(--panel); }
  .lbl { width: 42%; white-space: nowrap; overflow: hidden; text-overflow: ellipsis; }
  .track { flex: 1; height: 15px; background: var(--panel); border-radius: 3px; position: relative; }
  .bar { position: absolute; top: 0; bottom: 0; background: var(--accent); border-radius: 3px; }
  .bar.err { background: var(--err); }
  .dur { width: 84px; text-align: right; color: var(--dim); font-variant-numeric: tabular-nums; }
  .detail { padding-left: 14px; background: var(--panel); }
  .crumb { padding: 12px 0; color: var(--dim); }
</style>
