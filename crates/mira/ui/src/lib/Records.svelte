<script>
  import { api, bounds, parseFilter, fmtTime, fmtDur, sevName, svc, STATUS } from './api.js'
  import Detail from './Detail.svelte'

  let { signal, params, nonce, onstats } = $props()

  const PAGE = 200

  let rows = $state([])
  // The cursor the server returned with the last page, or '' for the end of the
  // result. Absent rather than null on the last page, so this is the whole of
  // the paging logic.
  let next = $state('')
  let loading = $state(true)
  let error = $state('')
  let open = $state(new Set())

  // What the visible rows were fetched with. "Load more" pages exactly this, not
  // whatever has been typed into the boxes at the top since without pressing Run.
  let sent = null
  // A slow query that the user has already navigated away from must not
  // overwrite the results of the one they are looking at now, nor page into
  // them. Identity, not a boolean, so a page in flight knows which query it is.
  let live = { ok: true }

  async function page(after) {
    const gen = live
    loading = true
    error = ''
    try {
      const doc = {
        signal: sent.sig,
        ...bounds(sent.range),
        where: parseFilter(sent.q, sent.sig),
        limit: PAGE,
      }
      if (after) doc.after = after
      const r = await api('/api/v1/query', doc)
      if (!gen.ok) return
      rows = after ? [...rows, ...r.rows] : r.rows
      next = r.next || ''
      onstats(r.stats)
    } catch (e) {
      if (!gen.ok) return
      error = String(e.message || e)
      rows = []
      next = ''
      onstats(null)
    } finally {
      if (gen.ok) loading = false
    }
  }

  $effect(() => {
    // Read every dependency up front so the effect re-runs on any of them, and
    // so nothing `page` touches after its first await is tracked by accident.
    sent = { q: params.q || '', range: params.range || '-1h', sig: signal }
    void nonce

    const gen = { ok: true }
    live = gen
    rows = []
    next = ''
    open = new Set()
    page()
    return () => { gen.ok = false }
  })

  function toggle(i) {
    const next = new Set(open)
    next.has(i) ? next.delete(i) : next.add(i)
    open = next
  }

  const key = (r, i) => (signal === 'logs' ? r.time_unix_nano : r.span_id) + ':' + i
</script>

{#if error}
  <div class="err">{error}</div>
{:else if loading && !rows.length}
  <div class="empty">Loading…</div>
{:else if !rows.length}
  <div class="empty">No matching records.</div>
{:else}
  <table>
    <thead>
      <tr>
        {#if signal === 'logs'}
          <th>Time</th><th>Severity</th><th>Service</th><th>Body</th>
        {:else}
          <th>Time</th><th>Duration</th><th>Service</th><th>Span</th><th>Status</th>
        {/if}
      </tr>
    </thead>
    <tbody>
      {#each rows as row, i (key(row, i))}
        <tr onclick={() => toggle(i)}>
          {#if signal === 'logs'}
            <td class="t">{fmtTime(row.time_unix_nano)}</td>
            <td class="sev-{sevName(row)}">{sevName(row).toUpperCase()}</td>
            <td>{svc(row)}</td>
            <td class="msg">{row.body ?? ''}</td>
          {:else}
            <td class="t">{fmtTime(row.start_time_unix_nano)}</td>
            <td class="num">{fmtDur(row.duration_nano)}</td>
            <td>{svc(row)}</td>
            <td class="msg">{row.name ?? ''}</td>
            <td class={row.status_code === 2 ? 'sev-error' : ''}>
              {STATUS[row.status_code] || ''}
            </td>
          {/if}
        </tr>
        {#if open.has(i)}
          <tr class="detail">
            <td colspan={signal === 'logs' ? 4 : 5}><Detail {row} /></td>
          </tr>
        {/if}
      {/each}
    </tbody>
  </table>
  <div class="more">
    <span>{rows.length} rows</span>
    {#if next}
      <button onclick={() => page(next)} disabled={loading}>
        {loading ? 'Loading…' : `Load ${PAGE} more`}
      </button>
    {:else}
      <span>end of results</span>
    {/if}
  </div>
{/if}

<style>
  .more { display: flex; align-items: center; gap: 14px; padding: 14px 0; color: var(--dim); }
  .more button {
    font: inherit;
    color: var(--fg);
    background: var(--panel);
    border: 1px solid var(--line);
    border-radius: 6px;
    padding: 5px 12px;
    cursor: pointer;
  }
  .more button:disabled { color: var(--dim); cursor: default; }
</style>
