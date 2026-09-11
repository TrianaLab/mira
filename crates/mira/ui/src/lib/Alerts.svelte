<script>
  import { fmtAlert, fmtTime, fmtWindow, get, num } from './api.js'
  import { go } from './route.svelte.js'

  let { params, nonce, onstats } = $props()

  let alerts = $state(null)
  let error = $state('')

  $effect(() => {
    void nonce
    let live = true
    ;(async () => {
      try {
        const r = await get('/api/v1/alerts')
        if (!live) return
        alerts = r.alerts
        error = ''
      } catch (e) {
        if (live) { error = String(e.message || e); alerts = null }
      }
    })()
    // Nothing here scans a block, so the stats bar would still be showing the
    // last query's counters as if they were this page's.
    onstats(null)
    return () => { live = false }
  })

  const firing = $derived((alerts || []).filter((a) => a.state === 'firing').length)
  const broken = $derived((alerts || []).filter((a) => a.error).length)

  // The rule's own terms, already spelled in this box's grammar by the server
  // (`alert::filter_of`), so there is one spelling of a predicate and the rows
  // this opens are the rows that were counted.
  const drill = (a) =>
    go('/' + a.signal, { q: a.filter, range: params.range || '-1h' })
</script>

{#if error}
  <div class="err">{error}</div>
{:else if !alerts}
  <div class="empty">Loading…</div>
{:else if !alerts.length}
  <!-- An empty list means alerting is off, not that everything is fine. The
       difference is the whole reason this page says anything at all. -->
  <div class="first">
    <h2>No alert rules loaded</h2>
    <p>
      This node is evaluating nothing and would page nobody. Rules live in their
      own KYAML file, named by <code>alerts.rules</code> in the config or by a flag:
    </p>
    <pre>mira --data-dir ./data --alerts ./alerts.kyaml</pre>
    <p>A rule is a query you already know how to write, plus a threshold:</p>
    <pre>{`{ every: 15s, rules: [
  { name: shop-error-rate,
    query: { signal: traces, where: [ { field: status_code, eq: 2 } ] },
    of:    { signal: traces },
    over: 1m, when: "ratio > 2%", for: 30s, severity: critical },
] }`}</pre>
    <p class="dim">
      <code>docs/e2e/alerts.kyaml</code> is the worked example, and
      <code>make demo</code> starts a node with it loaded.
    </p>
  </div>
{:else}
  <div class="bar">
    {alerts.length} {alerts.length === 1 ? 'rule' : 'rules'} ·
    {#if firing}<span class="firing">{firing} firing</span>{:else}none firing{/if}
    {#if broken}<span class="firing"> · {broken} could not be evaluated</span>{/if}
  </div>
  <table>
    <thead>
      <tr>
        <th></th>
        <th>rule</th>
        <th>value</th>
        <th class="num">matched</th>
        <th class="num">of</th>
        <th>over</th>
        <th>for</th>
        <th>since</th>
        <th class="msg">filter</th>
      </tr>
    </thead>
    <tbody>
      {#each alerts as a (a.name)}
        <tr
          tabindex="0"
          onclick={() => drill(a)}
          onkeydown={(e) => e.key === 'Enter' && drill(a)}
        >
          <td class="dot {a.state}" title={a.state}>
            {a.state === 'firing' ? '●' : a.state === 'pending' ? '◐' : '○'}
          </td>
          <td>
            {a.name}
            <span class="sev-{a.severity}">{a.severity}</span>
          </td>
          <td class:bad={a.state === 'firing'}>
            {fmtAlert(a.metric, a.value)} <span class="dim">{a.op} {fmtAlert(a.metric, a.threshold)}</span>
          </td>
          <td class="num">{a.matched}</td>
          <!-- Null for a count rule: there is no denominator, and a dash says
               that where a 0 would read as "nothing was there to match". -->
          <td class="num dim">{a.total ?? '—'}</td>
          <td class="dim">{fmtWindow(a.over_nano)}</td>
          <td class="dim">{num(a.for_nano) ? fmtWindow(a.for_nano) : ''}</td>
          <!-- When the current state started, not when it was last evaluated:
               "firing since 09:14" is the line that goes in the incident. -->
          <td class="dim">{a.since ? fmtTime(a.since) : ''}</td>
          <td class="msg">{a.filter || 'every record'}</td>
        </tr>
        {#if a.error}
          <tr class="detail">
            <td></td>
            <td colspan="8" class="bad">{a.error}</td>
          </tr>
        {/if}
      {/each}
    </tbody>
  </table>
  <!-- Every rule is evaluated in the same tick, so one timestamp covers them
       all -- and it is the one that says whether the evaluator is still alive. -->
  <p class="dim foot">
    Click a rule to see the records it counted. Last evaluated
    {fmtTime(alerts[0].evaluated_at)}.
  </p>
{/if}

<style>
  .bar { padding: 12px 0 4px; color: var(--dim); }
  .firing { color: var(--err); }
  .dot { width: 1%; }
  .dot.firing { color: var(--err); }
  .dot.pending { color: var(--warn); }
  .dot.ok { color: var(--ok); }
  .dim { color: var(--dim); }
  .bad { color: var(--err); }
  /* Severity is the operator's own word, not one of ours, so anything that is
     not a level we colour simply stays dim rather than being rejected. */
  .sev-critical { color: var(--err); }
  .sev-warning { color: var(--warn); }
  td span { color: var(--dim); margin-left: 8px; }
  .foot { margin: 14px 0 0; }
  .first { padding: 32px 0; max-width: 76ch; }
  h2 { font-size: 13px; font-weight: 500; margin: 0 0 12px; }
  p { color: var(--dim); margin: 12px 0 6px; }
  pre {
    margin: 0;
    padding: 10px 12px;
    background: var(--panel);
    border: 1px solid var(--line);
    border-radius: 6px;
    white-space: pre-wrap;
    overflow-wrap: anywhere;
    font: inherit;
  }
  code { color: var(--fg); }
</style>
