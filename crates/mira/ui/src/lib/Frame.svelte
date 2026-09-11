<script>
  // The frame algebra on screen (section 7.3): the region of telemetry around whatever
  // the query above matched.
  //
  // It runs the *same* filter the table below is showing, so the two answers
  // are about one question. Everything it returns is a link back into an
  // ordinary query, which is what the algebra's closure buys: there is nothing
  // here a reader can click that lands them somewhere they cannot then filter.

  import { addTerm, api, bounds, parseFilter, fmtTime, fmtDur, num } from './api.js'
  import { go, route } from './route.svelte.js'

  let { signal, params, nonce } = $props()

  let frame = $state(null)
  let error = $state('')
  let loading = $state(false)

  $effect(() => {
    const q = params.q || ''
    const range = params.range || '-1h'
    void nonce
    let live = true
    loading = true
    error = ''
    ;(async () => {
      try {
        const r = await api('/api/v1/correlate', {
          signal,
          ...bounds(range),
          where: parseFilter(q, signal),
          // Ordered: `traces` measures the real extent of what was found, and
          // `peers` then reads the services inside it. Reversed, `peers` would
          // run against the narrow window and find only what is already here.
          expand: ['traces', 'peers'],
        })
        if (!live) return
        frame = r.frame
      } catch (e) {
        if (live) { error = String(e.message || e); frame = null }
      } finally {
        if (live) loading = false
      }
    })()
    return () => { live = false }
  })

  // Add a service to the filter rather than replacing it: the frame is a
  // narrowing step in an investigation, and the term already typed is the
  // reason this frame exists.
  const narrow = (name) =>
    go(route.path, { ...params, q: addTerm(params.q, `service.name="${name}"`) })

  const span = $derived(frame ? num(frame.to) - num(frame.from) : 0)

  // Grouped by name, because an entity is an *instance* (section 7.2) and three
  // replicas of one service are three entities. A strip reading
  // "frontend frontend frontend" says less than "frontend x3", and the filter
  // the chip writes is on the name anyway.
  let services = $derived.by(() => {
    const by = new Map()
    for (const e of frame?.entities || []) by.set(e.name, (by.get(e.name) || 0) + 1)
    return [...by].sort((a, b) => a[0].localeCompare(b[0]))
  })
</script>

{#if error}
  <div class="err">{error}</div>
{:else if frame}
  <div class="frame">
    <div class="hd">
      <b>Frame</b>
      <span class="win">
        {fmtTime(frame.from)} → {fmtTime(frame.to)} · {fmtDur(span)}
      </span>
      {#if loading}<span class="win">refreshing…</span>{/if}
      {#if frame.truncated}
        <!-- A capped frame is a sample, and a sample presented as a census is
             the one way this panel can be confidently wrong. -->
        <span class="trunc">sample — narrow the filter before concluding</span>
      {/if}
    </div>
    {#if !services.length && !frame.traces.length}
      <div class="none">Nothing matched, so there is no frame to widen.</div>
    {:else}
      {#if services.length}
        <div class="row">
          <span class="lbl">services</span>
          {#each services as [name, n] (name)}
            <button class="chip" onclick={() => narrow(name)} title="add service.name={name} to the filter">
              {name}{n > 1 ? ` \u00d7${n}` : ''}
            </button>
          {/each}
        </div>
      {/if}
      {#if frame.traces.length}
        <div class="row">
          <span class="lbl">traces</span>
          {#each frame.traces.slice(0, 24) as t (t)}
            <a class="chip" href="#/trace/{t}">{t.slice(0, 12)}…</a>
          {/each}
          {#if frame.traces.length > 24}
            <span class="lbl">+{frame.traces.length - 24} more</span>
          {/if}
        </div>
      {/if}
    {/if}
  </div>
{/if}

<style>
  .frame {
    border: 1px solid var(--line);
    border-radius: 6px;
    background: var(--panel);
    padding: 8px 12px;
    margin: 12px 0;
  }
  .hd { display: flex; gap: 12px; align-items: baseline; }
  .win { color: var(--dim); }
  .trunc { color: var(--warn); }
  .none { color: var(--dim); padding-top: 4px; }
  .row { display: flex; flex-wrap: wrap; gap: 6px; align-items: baseline; padding-top: 6px; }
  .lbl { color: var(--dim); width: 62px; flex: none; }
  .chip {
    font: inherit;
    color: var(--fg);
    background: var(--bg);
    border: 1px solid var(--line);
    border-radius: 10px;
    padding: 1px 9px;
    cursor: pointer;
    text-decoration: none;
  }
  .chip:hover { border-color: var(--accent); text-decoration: none; }
</style>
