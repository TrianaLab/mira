---
"@mira/engine": minor
---

`mira proxy` fans out OTLP and queries across N storage nodes with no
coordination state, hash-based ingest routing ships alongside it, and the WAL's
mutex stopped being the ingest ceiling. Minor rather than patch: the binary
grew a subcommand.

No `"@mira/operator"` entry on purpose. The operator's line is *established* at
0.1.0 by this PR rather than moved by it — `integrations/kubernetes/Cargo.toml`
went from the `0.0.0` placeholder to the number `charts/mira-operator/Chart.yaml`
already carried — and a changeset here would make its first release 0.1.1.

That leaves both lines reading 0.1.0 for exactly one release. It is a
coincidence and not a claim: see `### And two that do not` in
docs/internals/releases.md for why the two numbers are not allowed to mean the
same thing.
