<script>
  import { fmtTime } from './api.js'

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
      .map(([k, v]) => `${k}=${v}`)
      .join(' ')
</script>

<div class="detail-box">
  {#if row.trace_id}
    <a href="#/trace/{row.trace_id}">→ view trace {row.trace_id}</a>
  {/if}
  {#each entries as [k, v] (k)}
    <pre><b>{k}:</b> {String(v)}</pre>
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
</style>
