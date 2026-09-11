<script>
  import { api, bounds, parseFilter, fmtTime, fmtDur, fmtValue, sevName, svc, STATUS } from './api.js'
  import Detail from './Detail.svelte'
  import Empty from './Empty.svelte'

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
  // `blocks_total` from the last successful response — how many blocks of this
  // signal exist before any predicate runs. Null until one arrives, so the
  // empty state never has to guess while a query is still in flight.
  let blocks = $state(null)

  // What the visible rows were fetched with. "Load more" pages exactly this, not
  // whatever has been typed into the boxes at the top since without pressing Run.
  let sent = null
  // A slow query that the user has already navigated away from must not
  // overwrite the results of the one they are looking at now, nor page into
  // them. Identity, not a boolean, so a page in flight knows which query it is.
  let live = { ok: true }
  // What is currently on screen, as a string. A re-run of the *same* query --
  // pressing Run, or a tick of follow mode -- has to leave the rows up until
  // the replacement arrives, or the table blanks for the length of a round trip
  // every few seconds. A different query does clear them: they are answers to a
  // question nobody is asking any more.
  let shown = null

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
      // Detail rows are open by index, and a first page renumbers every row
      // under them, so a refresh that kept them open would expand a different
      // record than the one the reader clicked.
      if (!after) open = new Set()
      next = r.next || ''
      blocks = r.stats.blocks_total
      onstats(r.stats)
    } catch (e) {
      if (!gen.ok) return
      error = String(e.message || e)
      // A failed page keeps everything already on screen. `next` is unchanged,
      // so the button below is still pointed at the page that failed and one
      // click retries it; clearing the rows instead would throw away three
      // pages the user waited for because the fourth timed out, and the only
      // way back is re-running the whole query. Only a first page has nothing
      // to keep.
      if (!after) {
        rows = []
        next = ''
        blocks = null
        onstats(null)
      }
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
    const id = `${sent.sig}\u0000${sent.q}\u0000${sent.range}`
    if (id !== shown) {
      rows = []
      next = ''
      blocks = null
      open = new Set()
    }
    shown = id
    page()
    return () => { gen.ok = false }
  })

  function toggle(i) {
    const next = new Set(open)
    next.has(i) ? next.delete(i) : next.add(i)
    open = next
  }

  const key = (r, i) => (signal === 'logs' ? r.time_unix_nano : r.span_id) + ':' + i

  // A row is a disclosure control, so it answers to what a button answers to.
  // Space is claimed rather than left alone because its default is to scroll
  // the page, which moves the row the user was about to open out from under
  // them.
  function activate(e, i) {
    if (e.key !== 'Enter' && e.key !== ' ') return
    e.preventDefault()
    toggle(i)
  }
</script>

<!-- The error sits above the rows rather than replacing them: a page that
     failed must not take the pages that succeeded with it. -->
{#if error}
  <div class="err">{error}</div>
{/if}

{#if !rows.length}
  {#if loading}
    <div class="empty">Loading…</div>
  {:else if !error}
    <Empty {signal} {blocks} q={params.q || ''} range={params.range || '-1h'} />
  {/if}
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
        <!-- `tabindex` and `aria-expanded` on the row itself, not a `role`
             swapped onto it: `row` is one of the few roles that already
             supports `aria-expanded`, so this stays a row inside a table for a
             screen reader while becoming reachable by Tab. Opening it is also
             the only way to reach the "view trace" link it contains, which is
             what made the mouse-only version a dead end rather than an
             inconvenience. -->
        <tr
          tabindex="0"
          aria-expanded={open.has(i)}
          onclick={() => toggle(i)}
          onkeydown={(e) => activate(e, i)}
        >
          {#if signal === 'logs'}
            <td class="t">{fmtTime(row.time_unix_nano)}</td>
            <td class="sev-{sevName(row)}">{sevName(row).toUpperCase()}</td>
            <td>{svc(row)}</td>
            <!-- A body that is not a string is stored and returned as
                 `body_ser`, so reading only `body` blanks the column for
                 exactly the records whose body carries the most. -->
            <td class="msg">{fmtValue(row.body ?? row.body_ser)}</td>
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
      <!-- The same cursor, so after a failure this button is a retry of the
           page that failed rather than a skip past it. Saying so matters: a
           button reading "Load 200 more" under an error looks like the way
           forward, not the way to recover. -->
      <button onclick={() => page(next)} disabled={loading}>
        {loading ? 'Loading…' : error ? 'Retry' : `Load ${PAGE} more`}
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
