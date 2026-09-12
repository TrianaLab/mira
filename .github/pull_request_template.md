## What this changes

<!-- One paragraph. What it does and why, not which files moved. -->

## Why this way

<!--
Only if the choice was not obvious — a mechanism you rejected, a constraint
from docs/architecture.md section 0, a trade between two of the four performance
axes. If the reasoning is worth keeping, it belongs in architecture.md in this
diff and this box can just point at the section.
-->

## Numbers

<!--
Required for any performance claim, and for anything touching ingest, the
block format or the query path. Before and after, on the same machine, with
the command you ran:

    cargo run --release --example loadgen -- --for 30s --conns 64 --readers 8

"No measurable change" is a fine answer. "Should be faster" is not.
-->

## Checklist

- [ ] `make check` is green locally.
- [ ] Tests cover the new behaviour — or, for a fix, the test fails without it.
- [ ] Coverage went up, or held. If the ratchet moved, it moved **up**.
- [ ] No new dependency. If there is one: `make deps` and `make drift` are
      green, and the README bullet and `docs/architecture.md` section 11 are updated
      in this diff.
- [ ] UI touched? `make ui` was run and `crates/mira/ui/dist` is committed here.
- [ ] The README is still true, with every number re-measured if this change
      moved one, and "Scope" still lists every boundary this leaves standing.
- [ ] Deliberate simplifications carry a `ponytail:` comment naming the ceiling.

## Related

<!-- Closes #123, or the issue this discussion started in. -->
