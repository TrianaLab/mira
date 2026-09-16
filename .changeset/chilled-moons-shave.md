---
"@mira/engine": minor
---

The terminal UI is drawn by ratatui. Every pane — list, detail, waterfall,
service map, alerts, node and help — renders through a widget and a layout
solver instead of the hand-rolled row writer, which clipped columns at `max` and
so lost content while its unit tests stayed green. No backend is linked:
`term.rs` keeps the pty, the raw mode, the resize handling and the key decoding.

**This raises the minimum supported Rust version from 1.85 to 1.88**, which
ratatui 0.30 requires. The binary grows 145 KiB (6.06 MiB to 6.18 MiB) and the
dependency tree 26 crates (122 to 148).
