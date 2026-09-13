#!/bin/sh
# Tag the version Cargo.toml already says, and let that tag cut the release.
#
# A version bump is a pull request — main is protected, so it has to be — and by
# the time it merges the number has been reviewed and `make drift` has checked
# that all fifteen places restating it agree. Asking someone to then remember
# `git tag && git push` is asking them to remember a step with no decision in
# it, and a step with no decision is a step that gets forgotten. Forgotten here
# means a merged release that never shipped, which is exactly what happened.
#
# So: main is green, Cargo.toml says X.Y.Z, no vX.Y.Z exists yet -> tag it.
# Merging the bump *is* the release now. Everything after the tag is unchanged.
#
# The dispatch at the end is not belt-and-braces, it is the whole trick. A tag
# pushed with GITHUB_TOKEN does not start a workflow — GitHub's loop protection
# — and the two events exempt from that rule are `workflow_dispatch` and
# `repository_dispatch`. Dispatching release.yml *at the tag* therefore needs no
# PAT, no deploy key and no GitHub App: `meta` branches on GITHUB_REF_TYPE, so
# the run is a tag run, and the OIDC subject is still `refs/tags/vX.Y.Z`, so
# `verify-release`'s `release.yml@refs/tags/` cosign identity pin still matches.
#
# The obvious alternative — have release.yml trigger on the push to main — moves
# that subject to `refs/heads/main`, and every signature Mira has published
# verifies against the tag form. That is not a pin you get to change quietly.

set -eu

version=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)
if [ -z "${version}" ]; then
	echo "error: no [workspace.package] version in Cargo.toml" >&2
	exit 1
fi
tag="v${version}"

# The remote, not `git tag -l`: a runner's checkout has no tags at all, so a
# local lookup would answer "not tagged" every single time and re-cut every
# release on every push.
if git ls-remote --exit-code --tags origin "refs/tags/${tag}" >/dev/null 2>&1; then
	echo "tag: ${tag} exists already - this push released nothing."
	exit 0
fi

echo "tag: Cargo.toml says ${version} and ${tag} does not exist - cutting it."

# Lightweight, like the tag a human used to push by hand. An annotated tag wants
# a tagger identity, and `git config user.email` on a runner is nobody.
git tag "${tag}"
git push origin "refs/tags/${tag}"

if ! gh workflow run release.yml --ref "${tag}"; then
	echo "::error::${tag} is pushed but release.yml was not dispatched. Nothing is" >&2
	echo "::error::building. Run release.yml from the Actions tab at ref ${tag}." >&2
	exit 1
fi

echo "tag: ${tag} pushed and release.yml dispatched at it."
