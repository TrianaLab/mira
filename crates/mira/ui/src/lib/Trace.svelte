<script>
  import { api, fmtDur, num, svc } from './api.js'
  import Detail from './Detail.svelte'

  let { traceId, nonce, onstats } = $props()

  // One page. A trace bigger than this exists — a fan-out job can emit tens of
  // thousands of spans — and the waterfall is not the tool for it, but the
  // response says so and the reader has to be told.
  const LIMIT = 2000

  let rows = $state([])
  let loading = $state(true)
  let error = $state('')
  let open = $state(new Set())
  let truncated = $state(false)

  $effect(() => {
    const id = traceId
    void nonce
    let live = true
    loading = true
    error = ''
    truncated = false
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
          limit: LIMIT,
        })
        if (!live) return
        rows = r.rows
        // `next` is present exactly when the result filled the limit, so this
        // is the server's own answer rather than `rows.length === LIMIT`
        // guessing — which is wrong for the trace that has exactly 2000 spans.
        truncated = !!r.next
        onstats(r.stats)
      } catch (e) {
        if (live) { error = String(e.message || e); rows = [] }
      } finally {
        if (live) loading = false
      }
    })()
    return () => { live = false }
  })

  // `num`, because these arrive as strings: see api.js. Nanoseconds past 2^53
  // round here, by about 128ns on a present-day timestamp — four orders of
  // magnitude below one pixel of a one-second trace, and both ends round the
  // same way, so a duration is unaffected.
  const start = (r) => num(r.start_time_unix_nano)
  const end = (r) => start(r) + num(r.duration_nano)

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

  // Space as well as Enter, because the row advertises `role="button"` and a
  // button takes both. Its default is to scroll, so it has to be claimed.
  function activate(e, id) {
    if (e.key !== 'Enter' && e.key !== ' ') return
    e.preventDefault()
    toggle(id)
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
  <!-- A partial trace drawn as if it were whole is the one output of this
       screen that can be confidently wrong: the missing spans are the ones the
       reader would have concluded do not exist. -->
  {#if truncated}
    <div class="trunc">
      Truncated: this trace has more than {LIMIT} spans and only the first {rows.length}
      were read. The tree below is incomplete, and the total duration is a lower bound.
    </div>
  {/if}
  <div class="wf">
    {#each laid.list as { r, depth } (r.span_id)}
      <div
        class="row"
        role="button"
        tabindex="0"
        onclick={() => toggle(r.span_id)}
        onkeydown={(e) => activate(e, r.span_id)}
      >
        <div class="lbl" title="{svc(r)} {r.name}" style="padding-left:{depth * 14}px">
          <span class="t">{svc(r)}</span>
          {r.name || ''}
        </div>
        <div class="track">
          <div
            class="bar"
            class:bad={r.status_code === 2}
            style="left:{((start(r) - laid.t0) / laid.span) * 100}%;
                   width:{Math.max((num(r.duration_nano) / laid.span) * 100, 0.2)}%"
          ></div>
          <!-- Events are where a span says what happened inside it — an
               exception, a retry, a cache miss — so they belong on the bar, not
               only in the expanded detail. -->
          {#each r.events || [] as e, i (i)}
            <div
              class="ev"
              title="{e.name || 'event'}"
              style="left:{((num(e.time_unix_nano) - laid.t0) / laid.span) * 100}%"
            ></div>
          {/each}
        </div>
        <div class="dur">{fmtDur(r.duration_nano)}</div>
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
  .row:focus-visible { outline: 2px solid var(--accent); outline-offset: -2px; background: var(--panel); }
  .lbl { width: 42%; white-space: nowrap; overflow: hidden; text-overflow: ellipsis; }
  .track { flex: 1; height: 15px; background: var(--panel); border-radius: 3px; position: relative; }
  .bar { position: absolute; top: 0; bottom: 0; background: var(--accent); border-radius: 3px; }
  /* `bad`, not `err`: `.err` is a global block-level message box carrying
     `padding: 16px 0`, and a bar wearing it grew 32px past its 15px track and
     drew over the span below. `bad` is what the rest of the UI already calls
     this state. */
  .bar.bad { background: var(--err); }
  .ev { position: absolute; top: -2px; bottom: -2px; width: 2px; background: var(--warn); }
  .dur { width: 84px; text-align: right; color: var(--dim); font-variant-numeric: tabular-nums; }
  .detail { padding-left: 14px; background: var(--panel); }
  .crumb { padding: 12px 0; color: var(--dim); }
  /* Same colour as the metrics legend's dropped-points badge, and for the same
     reason: both say "what you are looking at is not all of it". */
  .trunc {
    color: var(--warn);
    border: 1px solid var(--warn);
    border-radius: 6px;
    padding: 8px 12px;
  }
</style>
