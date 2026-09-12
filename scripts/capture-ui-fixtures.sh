#!/usr/bin/env bash
# Record the responses the documentation site's UI snapshot replays.
#
#   make ui-fixtures
#
# Starts a Mira on a scratch directory, fills it with the demo generator, asks
# it every question the UI asks, and writes the answers to
# crates/mira/ui/src/lib/fixtures.json. The keys are the ones `lib/replay.js`
# derives from a request; see that file for why they are coarser than the
# requests themselves.
#
# Committed output, not a build step. The capture needs a release binary and
# thirty seconds of generated telemetry, and neither belongs in `npm run
# build` — nor in CI, which would then re-record the snapshot on every merge
# and put a different set of trace ids in the diff each time.
set -euo pipefail

root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
out=$root/crates/mira/ui/src/lib/fixtures.json
bin=$root/target/release/mira
gen=$root/target/release/examples/loadgen
data=${TMPDIR:-/tmp}/mira-fixtures.$$
port=17399

for f in "$bin" "$gen"; do
  [ -x "$f" ] || { echo "missing $f — run: cargo build --release -p miradb --examples" >&2; exit 1; }
done

mkdir -p "$data"
"$bin" --data-dir "$data" --grpc 127.0.0.1:17398 --http 127.0.0.1:$port \
  --alerts "$root/docs/e2e/alerts.kyaml" >"$data/server.log" 2>&1 &
server=$!
cleanup() { kill $server 2>/dev/null || true; wait $server 2>/dev/null || true; rm -rf "$data"; }
trap cleanup EXIT

for _ in $(seq 40); do
  curl -fsS "http://127.0.0.1:$port/healthz" >/dev/null 2>&1 && break
  sleep 0.25
done

# The realistic generator, not the throughput harness: the snapshot is what a
# reader looks at, so it wants a service map with edges, spans with errors in
# them and metrics that move. `--for 1h` backdates an hour of history and
# returns in seconds.
"$gen" --addr 127.0.0.1:$port --demo --for 1h >/dev/null

# Give the alert evaluator a tick to see the data, and the flusher a moment to
# publish. Nothing here reads an open block through a query the way the server
# does; it is the alert rules that need a completed window.
sleep 12

ask() { # ask <key> <path> <body>
  printf '%s\t' "$1"
  curl -fsS -X POST "http://127.0.0.1:$port$2" -H 'content-type: application/json' -d "$3"
  printf '\n'
}
get() { printf '%s\t' "$1"; curl -fsS "http://127.0.0.1:$port$1"; printf '\n'; }

win='"from":"-1h","to":"now"'
# The same limit `Records.svelte` pages with. Two pages, so "Load more" does
# something; the second is rewritten below to have no cursor, or the button
# would page onto itself forever.
lim=200

tmp=$data/rows.tsv
: >"$tmp"
{
  get /api/v1/alerts
  ask /api/v1/entities /api/v1/entities "{$win}"
  ask /api/v1/map /api/v1/map "{$win}"
  ask 'query:logs:1'   /api/v1/query "{\"signal\":\"logs\",$win,\"where\":[],\"limit\":$lim}"
  ask 'query:traces:1' /api/v1/query "{\"signal\":\"traces\",$win,\"where\":[],\"limit\":$lim}"
  ask /api/v1/metrics/names /api/v1/metrics/names "{$win}"
  for sig in logs traces; do
    ask "correlate:$sig" /api/v1/correlate \
      "{\"signal\":\"$sig\",$win,\"where\":[],\"expand\":[\"traces\",\"peers\"]}"
  done
} >>"$tmp"

# The second pages need the first page's cursor, and the per-trace waterfalls
# need trace ids off the traces page, so both are a second round.
next() { python3 -c 'import json,sys; print(json.loads(sys.argv[1]).get("next") or "")' "$1"; }

logs1=$(awk -F'\t' '$1=="query:logs:1"{print $2}' "$tmp")
traces1=$(awk -F'\t' '$1=="query:traces:1"{print $2}' "$tmp")
{
  c=$(next "$logs1")
  [ -n "$c" ] && ask 'query:logs:2' /api/v1/query \
    "{\"signal\":\"logs\",$win,\"where\":[],\"limit\":$lim,\"after\":\"$c\"}"
  c=$(next "$traces1")
  [ -n "$c" ] && ask 'query:traces:2' /api/v1/query \
    "{\"signal\":\"traces\",$win,\"where\":[],\"limit\":$lim,\"after\":\"$c\"}"
  # Every distinct trace on the first page, capped: the waterfall is the part
  # of this UI worth clicking into, and a snapshot where only one row opens is
  # a snapshot of a table.
  for id in $(python3 -c '
import json,sys
seen=[]
for r in json.loads(sys.argv[1])["rows"]:
    t=r.get("trace_id")
    if t and t not in seen: seen.append(t)
print(" ".join(seen[:24]))' "$traces1"); do
    ask "query:trace:$id" /api/v1/query \
      "{\"signal\":\"traces\",\"from\":0,\"to\":\"now\",\"where\":[{\"field\":\"trace_id\",\"eq\":\"$id\"}],\"limit\":2000}"
  done
} >>"$tmp"

# Metric names come back with the descriptors; every one of them gets a series,
# because the dropdown lists all of them and a name that draws nothing reads as
# a broken chart rather than as a gap in a recording.
names=$(awk -F'\t' '$1=="/api/v1/metrics/names"{print $2}' "$tmp")
{
  for n in $(python3 -c '
import json,sys
print(" ".join(m["name"] for m in json.loads(sys.argv[1])["names"]))' "$names"); do
    ask "metrics:$n" /api/v1/metrics/query "{\"name\":\"$n\",$win,\"where\":[]}"
  done
} >>"$tmp"

python3 - "$tmp" "$out" <<'PY'
import json, os, sys

src, dst = sys.argv[1], sys.argv[2]
out = {}
for line in open(src):
    line = line.rstrip("\n")
    if not line:
        continue
    key, body = line.split("\t", 1)
    out[key] = json.loads(body)

# The last recorded page has no cursor. `Records.svelte` shows "Load more"
# exactly when the response carries one, and a third page would replay the
# second forever.
for sig in ("logs", "traces"):
    last = out.get(f"query:{sig}:2") or out.get(f"query:{sig}:1")
    if last:
        last["next"] = ""

# The fallbacks `replay.js` reaches for when the reader opens a trace or picks
# a metric the capture does not have. Aliases of a recorded answer, not extra
# recordings: `structuredClone` on the way out means sharing the object here is
# safe, and json.dump writes it twice either way.
for prefix, alias in (("query:trace:", "query:trace"), ("metrics:", "metrics")):
    first = next((v for k, v in out.items() if k.startswith(prefix)), None)
    if first is not None:
        out[alias] = first

# Sorted and indented: this file is committed, and a diff that reorders every
# key on each capture tells a reviewer nothing.
with open(dst, "w") as f:
    json.dump(out, f, indent=1, sort_keys=True)
    f.write("\n")

rows = sum(len(v.get("rows", [])) for v in out.values())
print(f"{dst}: {len(out)} responses, {rows} rows, {os.path.getsize(dst) / 1024:.0f} KiB")
PY
