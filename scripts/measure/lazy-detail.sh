#!/bin/sh
# Paired A/B: what does *not* CRC32ing the attribute tables buy a query that
# does not read them? Produces the numbers in architecture.md section 11 under
# "a query verifies the tables it reads".
#
# Two binaries, one corpus, alternating on the same box. Four cases, chosen so
# that two of them must move and two of them must not:
#
#   scan-miss-logs    every block scanned, no attribute predicate, no row
#                     emitted. Nothing in the query needs an attribute table.
#   scan-miss-traces  the same on traces, where the attribute tables are a
#                     larger share of the block.
#   scan-attr         *identical shape* to scan-miss-logs -- same corpus, same
#                     op, same zero matches -- except that the predicate names
#                     an attribute instead of a field. The attribute tables are
#                     read in both builds, so this is the control: if it moves,
#                     the run measured something other than the change.
#   page-100          the commonest query there is. One block, every row of the
#                     page rendered, so its attributes are read in both builds.
#
# `contains` is what makes the two scans honest: it has no zone summary and no
# bloom probe, so no block prunes and no scan short-circuits. `blocks_scanned`
# is printed beside every median, and it has to equal `blocks_total`.
#
# The number timed is the server's own `elapsed_us`, which is what the API
# reports and what a client sees less the socket. Medians over PASSES x REPS
# samples per case per build.
#
# The process boundary is one server per build per pass: 4 cases x (WARM + REPS)
# queries inside it, then killed. That is deliberate for the A/B here -- neither
# binary carries per-process state about a block, so a long-lived process only
# warms the page cache, which is what the warm-ups are for. It stops being
# neutral the moment a build under test *does* carry such state: a verification
# cache, say, is empty at exec and full from the second pass onward, so it would
# be measured warm here and cold by a client that reconnects to a fresh server.
# Anyone A/B-ing such a build should run it both ways and say which row is which.
#
#   make release && A=./target/release/mira B=/path/to/0.0.3/mira \
#     scripts/measure/lazy-detail.sh
set -e
A=${A:-./target/release/mira}
B=${B:-/tmp/mira-0.0.3-target/release/mira}
CORPUS=${CORPUS:-/tmp/mira-measure/restored}
OUT=${OUT:-/tmp/mira-lazy}
HTTP=${HTTP:-127.0.0.1:4339}
GRPC=${GRPC:-127.0.0.1:4336}
PASSES=${PASSES:-7}
REPS=${REPS:-5}
WARM=${WARM:-2}
MISS=zqxjw-no-such-token

[ -d "$CORPUS" ] || { echo "no corpus at $CORPUS"; exit 1; }
rm -rf "$OUT"
mkdir -p "$OUT"

CASES="scan-miss-logs scan-miss-traces scan-attr page-100"
body() { # case
  case $1 in
    scan-miss-logs)
      echo "{\"signal\":\"logs\",\"from\":\"-24h\",\"to\":\"now\",\
\"where\":[{\"field\":\"body\",\"contains\":\"$MISS\"}],\"limit\":1}" ;;
    scan-miss-traces)
      echo "{\"signal\":\"traces\",\"from\":\"-24h\",\"to\":\"now\",\
\"where\":[{\"field\":\"name\",\"contains\":\"$MISS\"}],\"limit\":1}" ;;
    scan-attr)
      echo "{\"signal\":\"logs\",\"from\":\"-24h\",\"to\":\"now\",\
\"where\":[{\"attr\":\"http.route\",\"contains\":\"$MISS\"}],\"limit\":1}" ;;
    page-100)
      echo "{\"signal\":\"logs\",\"from\":\"-24h\",\"to\":\"now\",\"limit\":100}" ;;
  esac
}

ask() { # case -> "<elapsed_us> <blocks_scanned> <blocks_total> <rows_scanned>"
  # The `awk` is not decoration. A response body has no trailing newline and
  # `sed` faithfully keeps it that way, so without it every sample of a run
  # lands on one line, `sort -n` has one line to sort, and the "median" is
  # silently the first sample of pass 1 -- the coldest one. The sample-count
  # check in the report is the guard that caught it.
  curl -s "http://$HTTP/api/v1/query" -H 'content-type: application/json' \
       -d "$(body "$1")" \
    | sed -n 's/.*"blocks_total":\([0-9]*\),"blocks_scanned":\([0-9]*\),"rows_scanned":\([0-9]*\),"rows_matched":[0-9]*,"elapsed_us":\([0-9]*\).*/\4 \2 \1 \3/p' \
    | awk '{print}'
}

# The reclaimer drops unexpired blocks once free space falls under `MIN_FREE`.
# Half a corpus is a faster scan, and the A/B would then report the volume.
reclaimed() { # log
  r_n=$(grep -c 'nearly full' "$1" || true)
  [ "$r_n" = 0 ] || {
    echo "!! the free-space reclaimer dropped blocks $r_n times ($1)."
    echo "!! free space and re-run; this run measures the volume, not the code."
    exit 1
  }
}

run() { # tag, binary, pass
  "$2" --data-dir "$CORPUS" --http "$HTTP" --grpc "$GRPC" --retention 999d \
    >"$OUT/$1-$3.log" 2>&1 &
  r_pid=$!
  r_i=0
  until curl -s -o /dev/null "http://$HTTP/api/v1/query" -d '{"signal":"logs","limit":1}' \
        || [ $r_i -ge 60 ]; do sleep 1; r_i=$((r_i + 1)); done
  # The page cache is shared between the two processes and is the state this
  # cannot control, so warm it before every timed set and alternate the builds
  # pass by pass. What is left is paired.
  for r_c in $CASES; do
    r_w=0
    while [ $r_w -lt "$WARM" ]; do ask "$r_c" >/dev/null; r_w=$((r_w + 1)); done
    r_w=0
    while [ $r_w -lt "$REPS" ]; do
      ask "$r_c" >> "$OUT/$1.$r_c"
      r_w=$((r_w + 1))
    done
    # An envelope that `ask` cannot parse is a silent zero everywhere
    # downstream, so say so here instead.
    [ -s "$OUT/$1.$r_c" ] || {
      echo "!! no samples for $r_c: see $OUT/$1-$3.log, and check ask()'s grep"
      kill $r_pid
      exit 1
    }
  done
  kill $r_pid
  wait $r_pid 2>/dev/null || true
  reclaimed "$OUT/$1-$3.log"
}

echo "corpus: $CORPUS  $(find "$CORPUS" -mindepth 3 -maxdepth 3 -type d | wc -l | tr -d ' ') blocks  $(du -sh "$CORPUS" | cut -f1)"
echo "a: $A"
echo "b: $B"
echo "$PASSES passes x $REPS reps, $WARM warm-up per case per pass"

p=1
while [ "$p" -le "$PASSES" ]; do
  printf 'pass %s ' "$p"
  run b "$B" "$p"
  printf 'b '
  run a "$A" "$p"
  echo a
  p=$((p + 1))
done

echo
# Median of the first column. `sort -n` rather than an awk sort, because one
# of the two awks on this box has no arrays of arrays and the bug is silent.
med() { sort -n "$1" | awk '{v[NR]=$1} END{h=int(NR/2); print NR%2 ? v[h+1] : (v[h]+v[h+1])/2}'; }

want=$((PASSES * REPS))
printf '%-18s %10s %10s %9s  %s\n' case 0.0.3_us branch_us delta scanned
for c in $CASES; do
  for t in a b; do
    n=$(wc -l < "$OUT/$t.$c" | tr -d ' ')
    [ "$n" = "$want" ] || { echo "!! $t.$c has $n samples, not $want"; exit 1; }
  done
  mb=$(med "$OUT/b.$c")
  ma=$(med "$OUT/a.$c")
  # Every sample of a case reads the same corpus, so the last line speaks for
  # all of them -- and blocks_scanned/blocks_total is the check that it did.
  s=$(tail -1 "$OUT/a.$c")
  printf '%-18s %10s %10s %8.1f%%  %s of %s blocks, %s rows\n' \
    "$c" "$mb" "$ma" \
    "$(awk -v a="$ma" -v b="$mb" 'BEGIN{print 100*(a-b)/b}')" \
    "$(echo "$s" | cut -d' ' -f2)" "$(echo "$s" | cut -d' ' -f3)" \
    "$(echo "$s" | cut -d' ' -f4)"
done
echo
echo "0.0.3  = $B"
echo "branch = $A"
echo "raw samples in $OUT/{a,b}.<case>, $want per case per build"
