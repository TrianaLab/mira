# Changesets

A changeset says which version line your change moves, and why. `make changeset`
writes one interactively; writing it by hand is equally fine — no Node needed.

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
| --- | --- | --- |
| `@mira/engine` | the binary, the image, the three crates | `Cargo.toml`'s `[workspace.package] version` |
| `@mira/operator` | the controller image and the Helm chart | `charts/mira-operator/Chart.yaml` |

They are **not** a `fixed` group in `config.json` and must never become one: a
controller bug fix should not require a Mira release, nor a Mira release
republish an unchanged controller. See
[Release architecture](https://miradb.dev/internals/releases/).

Three settings in `config.json` are the whole port, and JSON cannot hold a
comment:

- **`"changelog": false`** — no per-unit `CHANGELOG.md`. Mira's changelog is
  prose a human writes under `## [Unreleased]`, `release.yml` extracts it
  verbatim, `xtask drift --bump` refuses an empty one — a generated changelog
  would fight all three.
- **`"fixed": []`** — the setting that would couple the two lines, empty on
  purpose rather than by default.
- **`"privatePackages": {"version": true}`** — config v4 flipped `version` to
  `false` for private packages; without this line every bump is a silent no-op
  and the Version PR comes out empty.

`changeset version` only writes the files under `release/units/`. `make version`
copies those numbers into the places that matter, `make drift` fails if any
disagree — so a unit file is never a second source of truth for longer than one
command.

An `@mira/engine` changeset needs a `## [Unreleased]` entry in `CHANGELOG.md`;
`make ci-changeset` fails without one. The changeset says *which number moves*;
the changelog is what a stranger reads on the release page.
