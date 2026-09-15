---
"@mira/engine": patch
---

`cargo run -p xtask -- reference` now also writes `docs/reference/crd.md` from
the shipped `MiraCluster` CRD, so the field list a user writes a custom resource
against cannot drift from the schema the API server prunes against. Nothing in
the binary changed: patch because the generator lives in the engine's workspace
and the page is published from it.
