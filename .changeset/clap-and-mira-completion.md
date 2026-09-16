---
"@mira/engine": minor
---

`mira` parses its command line with clap, which adds `mira completion <shell>`
for bash, zsh, fish, PowerShell and elvish. Two behaviour changes come with it:
`mira <subcommand> -V` no longer prints the version — `-V` is the root's, and
`mira update --version <TAG>` needs that spelling for the tag — and `--replica`
outside `mira proxy` is refused by the parser rather than by the server it was
about to start. The binary grows 291 KiB (5.77 → 6.06 MiB) and the tree grows
from 117 crates to 122.
