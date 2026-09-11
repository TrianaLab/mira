<script>
  // Two states that render identically and mean opposite things: a store with
  // nothing in it, and a filter that excluded everything. Told they have "no
  // matching records", someone who has just started the binary goes and debugs
  // a filter that is fine, when what they need is an exporter pointed here.
  //
  // `stats.blocks_total` separates them with no extra request: it is the number
  // of blocks the query could have opened for this signal, counted before any
  // predicate runs, so zero means nothing has ever been stored. It counts the
  // open block too, so this stops being a lie in the seconds between the first
  // export being acknowledged and its block reaching the disk.
  let { signal, blocks, q = '', range = '' } = $props()

  // Only the levers that are actually still available. Telling someone whose
  // range is already `all` to widen it is worse than saying nothing: it is a
  // suggestion they will follow and that cannot work.
  const hints = $derived(
    [range !== 'all' && 'widening the time range', q && 'loosening the filter'].filter(Boolean),
  )

  // The page was served by the same listener that accepts OTLP/HTTP -- `/v1/*`
  // in, `/api/v1/*` out, one port -- so the endpoint to paste into an exporter
  // is derivable rather than guessable. gRPC is a second listener on a port
  // this page cannot see, hence "by default": 4317 is what config.rs ships and
  // may not be what this process was started with.
  const http = $derived(`${location.origin}/v1/${signal}`)
  const grpc = $derived(`${location.hostname}:4317`)
</script>

{#if blocks === 0}
  <div class="first">
    <h2>No {signal} stored yet</h2>
    <!-- Scoped to the signal, because `blocks_total` is: a store full of logs
         still has zero metrics blocks, and "nothing has been written here"
         would be a flat lie on that page. -->
    <p>Nothing has sent {signal} to this Mira. Point an OTLP exporter at it:</p>
    <pre>OTLP/HTTP   {http}
OTLP/gRPC   {grpc}</pre>
    <!-- Outside the block, so the block stays something you can select and
         paste. Hedged, because this page is served by the HTTP listener and
         genuinely cannot see which port the gRPC one took. -->
    <p class="dim">4317 is the default gRPC port; the "mira listening" log line has the real one.</p>
    <!-- Both spellings, because the second one is what the first one runs and
         is the only one that works from a release tarball with no Makefile. -->
    <p>Or generate a shop's worth of synthetic logs, spans and metrics:</p>
    <pre>make demo
cargo run --release --example loadgen -- --for 30s</pre>
    <p class="dim">This page refreshes when you press Run.</p>
  </div>
{:else}
  <div class="empty">
    {#if q}
      No {signal} matched <code>{q}</code> in this time range.
    {:else}
      No {signal} in this time range.
    {/if}
    <div class="dim">
      {blocks}
      {blocks === 1 ? 'block' : 'blocks'} of {signal} stored{hints.length
        ? ` — try ${hints.join(' or ')}.`
        : '.'}
    </div>
  </div>
{/if}

<style>
  /* Left-aligned and unpadded, unlike `.empty`: this one is instructions to
     follow, and centred instructions with a code block in them read as an
     error page. */
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
    /* `anywhere`, not `break-all`: the endpoint URL has to be allowed to break
       mid-token, but `break-all` also hyphenates ordinary prose mid-word. */
    overflow-wrap: anywhere;
    font: inherit;
  }
  code { color: var(--fg); }
  .dim { color: var(--dim); margin-top: 8px; }
</style>
