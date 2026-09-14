#!/usr/bin/env bash
#
# Wait until every signal answers with a row, or say which one never did.
#
# "The first block has sealed" is not a sleep. Exports are acked after
# durability, so the honest test is the one the user is about to run: ask each
# signal for a row and stop when all three have one. A sleep is right on this
# machine and wrong on a slower one, which is how a demo that "sometimes opens
# empty" happens.
#
# Two callers, one loop, because they were the same loop with two timeouts:
#
#   wait-for-signals.sh 60 warn      `make demo`. A slow laptop is not a bug, so
#                                    a timeout prints a warning and exits 0.
#   wait-for-signals.sh 240 assert   `make e2e`. The client on the wire there is
#                                    the stock collector, so a timeout is a
#                                    failed gate.
#
# curl rather than a language runtime: this is three HTTP requests in a loop,
# and the alternative was the only reason two of Mira's gates needed Python.

set -euo pipefail

usage() {
	echo "usage: wait-for-signals.sh <seconds> <warn|assert>" >&2
	exit 2
}

[ $# -eq 2 ] || usage
timeout=$1
mode=$2
case "$mode" in
warn | assert) ;;
*) usage ;;
esac

# `make demo` runs the engine on the default port; the Kind e2e reaches it
# through a port-forward, which cannot bind 4318 if a demo is already up.
base=${MIRA_URL:-http://127.0.0.1:4318}

# name | path | request body | the answer that means it is not there yet
checks=(
	'traces|/api/v1/query|{"signal":"traces","from":"-24h","to":"now","limit":1}|"rows":[]'
	'logs|/api/v1/query|{"signal":"logs","from":"-24h","to":"now","limit":1}|"rows":[]'
	'metrics|/api/v1/metrics/names|{}|"names":[]'
)

# Every second for the gate, three times a second for the demo: the demo is
# somebody watching a terminal, and four extra requests are cheaper than four
# extra seconds of a blank screen.
if [ "$mode" = assert ]; then interval=1; else interval=0.3; fi

left=$(printf '%s\n' "${checks[@]}")
SECONDS=0
while [ -n "$left" ] && [ "$SECONDS" -lt "$timeout" ]; do
	still=
	while IFS='|' read -r name path body empty; do
		# A connection refused, a 5xx and an empty result are the same
		# answer here — not yet — so they take the same branch.
		if out=$(curl -fs --max-time 5 -H 'content-type: application/yaml' \
			--data "$body" "$base$path" 2>/dev/null) &&
			[[ $out != *"$empty"* ]]; then
			[ "$mode" = warn ] || echo "  $name: came back out of Mira"
		else
			still="$still$name|$path|$body|$empty"$'\n'
		fi
	done <<<"$left"
	left=${still%$'\n'}
	[ -z "$left" ] || sleep "$interval"
done

if [ -z "$left" ]; then
	[ "$mode" = warn ] || echo "e2e: all three signals made the full trip"
	exit 0
fi

missing=$(cut -d'|' -f1 <<<"$left" | paste -sd, - | sed 's/,/, /g')
if [ "$mode" = assert ]; then
	echo "error: $missing never arrived - generator -> collector -> mira -> query API is broken" >&2
	exit 1
fi
echo "warning: $missing still has no sealed block after ${timeout}s."
echo "  the server is up; look at the log named above before filing anything."
