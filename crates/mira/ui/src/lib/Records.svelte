<script>
  import { api, bounds, parseFilter, fmtTime, fmtDur, sevName, svc, STATUS } from './api.js'
  import Detail from './Detail.svelte'

  let { signal, params, nonce, onstats } = $props()

  let rows = $state([])
  let loading = $state(true)
  let error = $state('')
  let open = $state(new Set())

  $effect(() => {
    // Read every dependency up front so the effect re-runs on any of them, and
    // so the async body below cannot accidentally track something later.
    const q = params.q || ''
    const range = params.range || '-1h'
    const sig = signal
    void nonce

    let live = true
    loading = true
    error = ''
    open = new Set()
    ;(async () => {
      try {
        const r = await api('/api/v1/query', {
          signal: sig,
          ...bounds(range),
          where: parseFilter(q, sig),
          limit: 200,
        })
        if (!live) return
        rows = r.rows
        onstats(r.stats)
      } catch (e) {
        if (live) {
          error = String(e.message || e)
          rows = []
          onstats(null)
        }
      } finally {
        if (live) loading = false
      }
    })()
    // A slow query that the user has already navigated away from must not
    // overwrite the results of the one they are looking at now.
    return () => { live = false }
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
{:else if loading}
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
{/if}
