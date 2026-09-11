<script>
  import { addTerm, fmtTime, fmtValue, term } from './api.js'
  import { go, route } from './route.svelte.js'

  // The raw row, everything the engine returned. That is the point: a field
  // missing from this dump is missing from storage, not from the renderer.
  let { row } = $props()
  let entries = $derived(
    Object.entries({ ...omit(row), ...(row.attributes || {}) })
      .filter(([, v]) => v !== null && v !== '' && v !== undefined),
  )
  // Events and links are arrays of objects, so they get their own rendering
  // below rather than being stringified into `[object Object]`.
  function omit({ attributes, events, links, ...rest }) {
    return rest
  }
  const kvs = (a) =>
    Object.entries(a || {})
      .map(([k, v]) => `${k}=${fmtValue(v)}`)
      .join(' ')

  // Every key here is either a root column or an attribute, and `parseFilter`
  // decides which from the same FIELDS table -- so a value clicked out of this
  // pane round-trips into the filter box as the term that finds it again. That
  // is the whole feature: nobody remembers the spelling of
  // `dropped_events_count`, and nobody should have to.
  //
  // Not offered for arrays and kvlists: an OTLP AnyValue of those shapes has no
  // scalar spelling, and a term that cannot match is worse than no term.
  const filterable = (v) => v === null || typeof v !== 'object'

  function filter(k, v) {
    // A waterfall has no filter box of its own; the terms it produces belong to
    // the span list it was opened from.
    const path = route.path.startsWith('/trace/') ? '/traces' : route.path
    go(path, { ...route.params, q: addTerm(route.params.q, term(k, v)) })
  }
</script>

<div class="detail-box">
  {#if row.trace_id}
    <a href="#/trace/{row.trace_id}">→ view trace {row.trace_id}</a>
  {/if}
  {#each entries as [k, v] (k)}
    <pre><b>{k}:</b> {#if filterable(v)}<button
          class="val"
          title="filter on {k}"
          onclick={() => filter(k, v)}>{fmtValue(v)}</button>{:else}{fmtValue(v)}{/if}</pre>
  {/each}
  {#each row.events || [] as e, i (i)}
    <pre class="ev"><b>event {fmtTime(e.time_unix_nano)}</b> {e.name || ''} {kvs(e.attributes)}</pre>
  {/each}
  <!-- A link points out of this trace, which is the whole reason to store it:
       it is the only edge that survives an async boundary. -->
  {#each row.links || [] as l, i (i)}
    <div><a href="#/trace/{l.trace_id}">→ linked trace {l.trace_id}</a> {kvs(l.attributes)}</div>
  {/each}
</div>

<style>
  .detail-box { padding: 6px 0; }
  pre {
    margin: 0;
    white-space: pre-wrap;
    word-break: break-all;
    color: var(--dim);
    font: inherit;
  }
  b { color: var(--fg); font-weight: 500; }
  .ev b { color: var(--warn); }
  /* A button that reads as text until it is worth clicking. Underline on hover
     rather than a border, because a border per value turns the dump into a
     grid and the dump is the thing people came here to read. */
  .val {
    font: inherit;
    color: inherit;
    background: none;
    border: none;
    padding: 0;
    text-align: left;
    cursor: pointer;
  }
  .val:hover, .val:focus-visible { color: var(--accent); text-decoration: underline; }
</style>
