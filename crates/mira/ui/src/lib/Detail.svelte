<script>
  // The raw row, everything the engine returned. That is the point: a field
  // missing from this dump is missing from storage, not from the renderer.
  let { row } = $props()
  let entries = $derived(
    Object.entries({ ...omit(row), ...(row.attributes || {}) })
      .filter(([, v]) => v !== null && v !== '' && v !== undefined),
  )
  function omit({ attributes, ...rest }) {
    return rest
  }
</script>

<div class="detail-box">
  {#if row.trace_id}
    <a href="#/trace/{row.trace_id}">→ view trace {row.trace_id}</a>
  {/if}
  {#each entries as [k, v] (k)}
    <pre><b>{k}:</b> {String(v)}</pre>
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
</style>
