---
"@mira/engine": patch
---

The week's dependency bumps, landed as one: arrow 59.3 → 60.0 across the
workspace pin, tonic and tonic-prost 0.14.5 → 0.14.6, crc32fast 1.5.1 → 1.5.2,
and the pinned action SHAs in CI, docs and release.

Arrow moves as a set, and `arrow-select` is part of the set — Dependabot's own
PR left it on 59.3 while the other four went to 60, which is the exact
two-majors-of-`RecordBatch` breakage the pin exists to prevent. It goes to 60.0
here with the rest. The 60.0 tree brings `arrow-cmp` and pulls zstd 0.13 → 0.14;
`supply-chain/config.toml` carries the exemptions forward for all of it, with
the imported audits left alone.
