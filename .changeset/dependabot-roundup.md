---
"@mira/engine": patch
---

The week's dependency bumps, landed as one: tokio 1.53.1 → 1.53.3, hyper
1.11.1 → 1.12.0 (pulling want 0.3.1 → 0.3.2), libc 0.2.189 → 0.2.190, svelte
5.57.1 → 5.57.2, vite 8.3.1 → 8.3.4 and devalue 5.9.2 → 5.9.4 in the UI,
shell-quote 1.10.0 → 1.12.0 in the release tooling, and the pinned action SHAs.

The UI bundle in `crates/mira/ui/dist` is rebuilt against the new toolchain,
since it's what gets `include_bytes!`'d, and `supply-chain/config.toml` moves
the four Rust exemptions forward, leaving the imported audits alone.

protox 0.9 → 0.10 comes along too. It shipped after main last went green, so
`make outdated` would have failed every PR from now on, and it dedupes logos
down to 0.16, which takes five exemptions out with it.
