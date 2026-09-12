#!/bin/sh
# Which CI legs a diff needs. Prints `name=true|false` per leg, and appends the
# same lines to $GITHUB_OUTPUT when there is one.
#
# dorny/paths-filter is the usual answer. It is also one more third-party action
# to pin, review and trust, for something `git diff` already does — and this
# repository's whole argument is that a dependency has to earn its place.
#
# Usage: scripts/ci-changes.sh [<base-ref>]
# The base defaults to the merge-base with origin/main, which is what a local
# `make ci-changes` wants. CI passes the event's base sha.
set -eu

base="${1:-}"
if [ -z "${base}" ]; then
  base=$(git merge-base HEAD origin/main 2>/dev/null || true)
fi

if [ -z "${base}" ] || ! git cat-file -e "${base}^{commit}" 2>/dev/null; then
  # First push to a branch, a force-push past the old tip, a shallow base, or
  # no origin/main locally. Unknown means "everything", never "nothing": a
  # filter that fails open costs CI minutes, one that fails closed ships bugs.
  echo "base ref '${base}' unusable — running every leg" >&2
  code=true; docs=true; ui=true; image=true; chart=true
else
  files=$(git diff --name-only "${base}" HEAD)
  echo "changed files:" >&2
  printf '%s\n' "${files}" | sed 's/^/  /' >&2
  m() { printf '%s\n' "${files}" | grep -qE "$1" && echo true || echo false; }

  # A workflow change re-runs everything — the thing most likely to be wrong
  # about a CI edit is the leg you did not think it touched. `ci.mk` is in the
  # same clause for the same reason: it *is* the workflow now.
  W='|^\.github/workflows/|^ci\.mk$'
  code=$(m "^(crates/|Cargo\.(toml|lock)\$|rust-toolchain\.toml\$|deny\.toml\$|Makefile\$|scripts/)${W}")
  # `overrides/` is the mkdocs theme override directory — one file, the landing
  # hero — and a Jinja error in it fails `mkdocs build --strict` like any page
  # would.
  # `scripts/get-mira.sh` is here rather than only in `code` because
  # `docs/install.sh` is a symlink to it: the docs site *publishes* it, so it is
  # a documentation artefact that happens to be a script, and this is the leg
  # that checks it.
  docs=$(m "^(docs/|overrides/|mkdocs\.yml\$|scripts/get-mira\.sh\$|.*\.md\$)${W}")
  # `Makefile$` for the same reason it is in `code` and `image`: both of that
  # leg's steps are make targets, so an edit to `ui-check` or `ui-demo` is a
  # change to what the job asserts.
  ui=$(m "^(crates/mira/ui/|Makefile\$)${W}")
  # The image is the binary plus the base image, so anything that changes the
  # binary changes what Trivy is looking at. Dockerfile and .dockerignore are
  # here for the obvious reason; Cargo.lock is here because a dependency bump is
  # exactly the change that introduces the advisory this leg exists to catch.
  image=$(m "^(crates/|Cargo\.(toml|lock)\$|rust-toolchain\.toml\$|Dockerfile\$|\.dockerignore\$|Makefile\$)${W}")
  chart=$(m "^charts/${W}")
fi

out=$(printf 'code=%s\ndocs=%s\nui=%s\nimage=%s\nchart=%s\n' \
  "${code}" "${docs}" "${ui}" "${image}" "${chart}")
printf '%s\n' "${out}"
[ -z "${GITHUB_OUTPUT:-}" ] || printf '%s\n' "${out}" >> "${GITHUB_OUTPUT}"
