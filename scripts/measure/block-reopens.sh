#!/bin/sh
# Does one process open the same block more than once? The access-pattern
# measurement docs/architecture/data-layout.md section 3.3 links: it is the gate on whether a
# per-process verification cache can save anything at all.
#
# No instrumentation needed. Every response already carries `blocks_scanned`,
# and one scanned block is one `open_table` — one mmap and one CRC32 over the
# body. So the sum of `blocks_scanned` over a session is the number of
# verifications the process performed, and `blocks_total` bounds the number of
# distinct blocks it could possibly have touched. The ratio is the ceiling on
# what a cache could ever remove, not an estimate of what it would.
#
# Run it against a corpus that already exists — `offload-cycle.sh` leaves one
# in $ROOT/restored:
#
#   DATA=/tmp/mira-measure/restored scripts/measure/block-reopens.sh
set -e
BIN=${BIN:-./target/release}
ROOT=${ROOT:-/tmp/mira-measure}
DATA=${DATA:-$ROOT/restored}
HTTP=${HTTP:-127.0.0.1:4319}
GRPC=${GRPC:-127.0.0.1:4316}

"$BIN/mira" --data-dir "$DATA" --http "$HTTP" --grpc "$GRPC" \
            --retention 999d >"$ROOT/reopen.log" 2>&1 &
P=$!
trap 'kill $P 2>/dev/null' EXIT
sleep 3

Q() { curl -s "http://$HTTP/api/v1/query" -H 'content-type: application/json' -d "$1"; }
W='"from":"-24h","to":"now"'

# One agentic session: look, narrow, filter, widen, page. The shape an LLM
# actually produces — every step re-asks over a window it has already read.
: > "$ROOT/reopen.txt"
for _ in 1 2 3; do
  for q in \
    "{\"signal\":\"logs\",$W,\"limit\":50}" \
    "{\"signal\":\"logs\",$W,\"where\":[{\"field\":\"severity_number\",\"gte\":17}],\"limit\":50}" \
    "{\"signal\":\"logs\",$W,\"where\":[{\"field\":\"body\",\"contains\":\"timeout\"}],\"limit\":50}" \
    "{\"signal\":\"logs\",$W,\"where\":[{\"attr\":\"service.name\",\"eq\":\"checkout\"}],\"limit\":50}" \
    "{\"signal\":\"traces\",$W,\"limit\":50}" \
    "{\"signal\":\"traces\",$W,\"where\":[{\"field\":\"duration_ns\",\"gte\":1000000}],\"limit\":50}"
  do
    Q "$q" >> "$ROOT/reopen.txt"
    echo >> "$ROOT/reopen.txt"
  done
done

python3 - "$ROOT/reopen.txt" <<'PY'
import json, sys

rows = []
for line in open(sys.argv[1]):
    line = line.strip()
    if not line:
        continue
    s = json.loads(line)["stats"]
    rows.append((s["blocks_total"], s["blocks_scanned"], s["rows_scanned"], s["elapsed_us"]))

print(f"{'blocks_total':>12} {'scanned':>9} {'rows':>10} {'us':>9}")
for r in rows:
    print("%12d %9d %10d %9d" % r)

# One `blocks_total` per signal, so summing the distinct values is the size of
# the union the session could have touched. It is an upper bound on "distinct",
# which makes the reopen ratio a lower bound.
opens = sum(r[1] for r in rows)
distinct = sum({r[0] for r in rows})
print(f"\n{len(rows)} queries, {opens} block opens, at most {distinct} distinct "
      f"blocks ({opens / distinct:.1f}x reopen)")
PY
