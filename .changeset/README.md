# Changesets

A changeset is a file that says which version line your change moves, and why.
It is five lines of markdown; `make changeset` writes one interactively, and
writing it by hand is equally fine — no Node needed for that.

````markdown
---
"@mira/engine": patch
---

Blocks written during a scale-in were restored from the pod name rather than
the node.
````

Two names, and only two, because Mira publishes two things on two version
lines:

| Name | Moves | Source of truth |
|---|---|---|
| `@mira/engine` | the binary, the image, the three crates | `Cargo.toml`'s `[workspace.package] version` |
| `@mira/operator` | the controller image and the Helm chart | `charts/mira-operator/Chart.yaml` |

They are **not** a `fixed` group in `config.json` and must never become one. A
controller at `0.3.1` beside an engine at `0.3.1` is a claim that they move
together, and they do not: a controller bug fix should not require a Mira
release, and a Mira release should not republish a controller that did not
change. See [Release architecture](https://miradb.dev/internals/releases/).

Three settings in `config.json` are the whole port, and JSON cannot hold a
comment:

- **`"changelog": false`** — no per-unit `CHANGELOG.md`. Mira's changelog is
  prose a human writes under `## [Unreleased]`, `release.yml` extracts that
  section verbatim, and `xtask drift --bump` refuses an empty one. A generated
  changelog would fight all three.
- **`"fixed": []`** — see above. This is the setting that would couple the two
  lines, so it is empty on purpose rather than by default.
- **`"privatePackages": {"version": true}`** — config v4 flipped `version` to
  `false` for private packages. Without this line every bump is a silent no-op
  and the Version PR comes out empty.

The files under `release/units/` are the only thing `changeset version` knows
how to write. `make version` then copies the two numbers it wrote into the
places that actually matter, and `make drift` fails if any of them disagree —
so a unit file is never a second source of truth for longer than one command.

An `@mira/engine` changeset also needs a `## [Unreleased]` entry in
`CHANGELOG.md`, and `make ci-changeset` fails without one. The changeset says
*which number moves*; the changelog is what a stranger reads on the release
page, and nothing can generate that.
