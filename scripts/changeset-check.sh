#!/bin/sh
# Every diff that changes something a stranger installs declares which version
# line it moves. The declaration is a file under .changeset/; see
# .changeset/README.md for the shape.
#
# This is the half of changesets that is not the bot. The bot can only version
# what it was told about, so "somebody forgot the changeset" is otherwise
# discovered at release time, on main, by nobody — which is why it is a pull
# request gate and not a step in the release.
#
# Usage: scripts/changeset-check.sh [<base-ref>]
# The base defaults to the merge-base with origin/main. CI passes the pull
# request event's base sha.
set -eu

base="${1:-}"
if [ -z "${base}" ]; then
  base=$(git merge-base HEAD origin/main 2>/dev/null || true)
fi

if [ -z "${base}" ] || ! git cat-file -e "${base}^{commit}" 2>/dev/null; then
  # Unlike scripts/ci-changes.sh, "unknown" here means pass rather than "run
  # everything": there is no diff to read, so there is no claim to make. A gate
  # that fails on a base it cannot see is a gate that blocks the first push to
  # every branch.
  echo "base ref '${base}' unusable — cannot tell whether this diff needs a changeset" >&2
  exit 0
fi

# A version bump is not a change that needs declaring: it is the declaration
# being applied. This exempts both the bot's Version PR (which deletes
# changesets rather than adding them) and the hand-run `make bump` fallback,
# with one mechanism rather than two.
#
# Both halves are required, and that is the whole point: a bump *replaces* a
# version line, so it shows up as a `-` and a `+`. Matching the `+` alone let a
# diff that merely introduces the file — a new chart, a new manifest — exempt
# itself along with everything else it shipped.
vdiff=$(git diff "${base}" HEAD -- Cargo.toml charts/mira-operator/Chart.yaml)
if printf '%s\n' "${vdiff}" | grep -qE '^\+(version = "|version: )' \
  && printf '%s\n' "${vdiff}" | grep -qE '^-(version = "|version: )'; then
  echo "this diff moves a version line — it is the release, not a change to declare" >&2
  exit 0
fi

files=$(git diff --name-only --diff-filter=d "${base}" HEAD -- '.changeset/*.md' \
  | grep -v '/README\.md$' || true)

if [ -z "${files}" ]; then
  cat >&2 <<'EOF'
error: this diff changes something that ships, and declares no version line.

  Add a changeset: `make changeset`, or write the file by hand —

      .changeset/<any-name>.md
      ---
      "@mira/engine": patch
      ---

      One sentence, in the past tense, about what changed.

  "@mira/engine" is the binary, the image and the crates; "@mira/operator" is
  the controller image and the Helm chart. They are separate version lines and
  a change can name either, or both. See .changeset/README.md.
EOF
  exit 1
fi

# The frontmatter lines, across every changeset in the diff. Read through a
# `while read` rather than an unquoted glob so a path with a space in it is not
# a silently skipped declaration.
decls=$(printf '%s\n' "${files}" \
  | while IFS= read -r f; do cat "${f}"; done \
  | grep -E '^"' || true)

bad=$(printf '%s\n' "${decls}" \
  | grep -vE '^"@mira/(engine|operator)": (major|minor|patch)$' \
  | grep -v '^$' || true)
if [ -n "${bad}" ]; then
  echo "error: a changeset names something that is not a version line:" >&2
  printf '%s\n' "${bad}" | sed 's/^/  /' >&2
  echo "  Only \"@mira/engine\" and \"@mira/operator\" exist, and only major|minor|patch." >&2
  exit 1
fi

if ! printf '%s\n' "${decls}" | grep -qE '^"@mira/(engine|operator)":'; then
  echo "error: a changeset was added but declares no package. The frontmatter between" >&2
  echo "  the two \`---\` lines is what the bot reads; prose alone bumps nothing." >&2
  exit 1
fi

# An engine changeset promotes CHANGELOG.md's [Unreleased] to a released
# heading, and `xtask drift --bump` refuses to do that to an empty section — so
# without this check the failure lands in the bot's job on main, where the
# person who could fix it is not looking.
if printf '%s\n' "${decls}" | grep -q '^"@mira/engine":'; then
  notes=$(awk '/^## \[Unreleased\]/{f=1;next} /^## \[/{f=0} f' CHANGELOG.md | tr -d '[:space:]')
  if [ -z "${notes}" ]; then
    echo "error: an @mira/engine changeset with an empty \`## [Unreleased]\` in CHANGELOG.md." >&2
    echo "  release.yml publishes that section verbatim as the release notes, and an" >&2
    echo "  empty one degrades to --generate-notes. The changeset says which number" >&2
    echo "  moves; the changelog is what a stranger reads." >&2
    exit 1
  fi
fi

printf '%s\n' "${files}" | sed 's/^/changeset: /'
echo "this diff declares its version line."
