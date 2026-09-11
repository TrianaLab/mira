#!/usr/bin/env sh
# The blocking U+00A7 gate: the section-sign glyph must appear NOWHERE in authored
# repository content or PR metadata. Write "section" instead.
#
# Mira's docs cross-reference each other constantly, and for a while they did it
# with the sign. It renders fine in a Markdown viewer on a Mac and badly
# everywhere else that matters: a grep from a terminal whose locale is not UTF-8,
# a git log in a CI viewer, a plain-text alert webhook body, and a model reading
# the repository through a tool that normalises to ASCII. Plain "section 7.2"
# costs six characters and has none of those failure modes.
#
# Removing the character once fixes nothing on its own — the next document
# written in the same style puts it straight back — so it is a gate, the same way
# the crate count and the binary size are. Ported from TrianaLab/pacto, which
# runs the identical check; keeping the two scripts recognisably the same is
# deliberate.
#
# Modes:
#
#   (no args)                 every tracked authored file. Only genuinely
#                             non-authored paths are excluded (see below);
#                             binary files are skipped by grep -I.
#   FILE...                   exactly the given files (used by the fixture test).
#   --commits RANGE           the subject and body of every commit in the git
#                             RANGE (e.g. origin/main..HEAD). Reports the sha.
#   --text LABEL [FILE]       an arbitrary text (a PR title or body); FILE
#                             defaults to stdin ("-").
#   --selftest                prove the gate can still fail. `make section` runs
#                             this first.
#
# The self-test is the part that earns the gate its trust. Every failure mode
# here is silent: a mangled `printf` escape, a locale that folds the byte pair,
# a `grep` that stops honouring -F, an exclude that grew to cover the tree. Each
# one turns a green run into "found nothing" rather than into an error, and a
# gate that cannot fail is indistinguishable from no gate at all. pacto pays for
# this with a Go fixture test; here it is nine lines in the script itself,
# because a separate test target would be a second thing to keep wired up.
#
# EXCLUDES, narrow and documented: crates/mira/ui/dist/ is the Svelte bundle,
# generated from crates/mira/ui/src and committed so that building Mira needs no
# node toolchain. A hit there has to be fixed at the source, and checking both
# would report it twice with the second one pointing at a file nobody edits.
#
# Run it with `make section`.
set -eu

sign=$(printf '\302\247') # UTF-8 bytes for U+00A7

fail() {
	echo "check-section: U+00A7 (section sign) found; write 'section' instead:" >&2
	printf '%s\n' "$1" >&2
	exit 1
}

case "${1:-}" in
--selftest)
	tmp=$(mktemp -d)
	trap 'rm -rf "$tmp"' EXIT
	printf 'clean line\n' >"$tmp/clean"
	printf 'a %s7.2 reference\n' "$sign" >"$tmp/dirty"
	sh "$0" "$tmp/clean" >/dev/null || {
		echo "check-section selftest: a clean file was rejected" >&2
		exit 1
	}
	# Both halves matter: exit status *and* the file:line the author needs.
	if out=$(sh "$0" "$tmp/dirty" 2>&1); then
		echo "check-section selftest: a file containing U+00A7 passed" >&2
		exit 1
	fi
	case "$out" in
	*"$tmp/dirty:1:"*) ;;
	*)
		echo "check-section selftest: the failure did not report path:line" >&2
		printf '%s\n' "$out" >&2
		exit 1
		;;
	esac
	printf '%s' "a $sign in a title" | sh "$0" --text "selftest" >/dev/null 2>&1 && {
		echo "check-section selftest: --text passed a dirty string" >&2
		exit 1
	}
	echo "check-section: selftest ok (clean passes, dirty fails with path:line, --text fails)"
	;;
--commits)
	range="${2:?usage: check-section-sign.sh --commits <range>}"
	hits=""
	for c in $(git rev-list "$range"); do
		if git log -1 --format='%B' "$c" | grep -qF "$sign"; then
			hits="${hits}commit ${c}: $(git log -1 --format='%s' "$c")
"
		fi
	done
	[ -n "$hits" ] && fail "$hits"
	echo "check-section: zero U+00A7 in commit messages of $range"
	;;
--text)
	label="${2:?usage: check-section-sign.sh --text <label> [file]}"
	file="${3:--}"
	if [ "$file" = "-" ]; then
		content=$(cat)
	else
		content=$(cat "$file")
	fi
	hit=$(printf '%s\n' "$content" | grep -nF "$sign" | sed "s#^#${label}:#" || true)
	[ -n "$hit" ] && fail "$hit"
	echo "check-section: zero U+00A7 in $label"
	;;
*)
	if [ "$#" -gt 0 ]; then
		files=$(printf '%s\n' "$@")
	else
		files=$(git ls-files | grep -v '^crates/mira/ui/dist/')
	fi
	hits=$(printf '%s\n' "$files" | grep -v '^$' | xargs grep -IHnF "$sign" 2>/dev/null || true)
	[ -n "$hits" ] && fail "$hits"
	echo "check-section: zero U+00A7 in authored files (generated UI bundle excluded by path)"
	;;
esac
