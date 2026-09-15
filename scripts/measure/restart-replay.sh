#!/bin/sh
# Does a restart re-ingest rows that are already published? Reproducer for the
# finding in docs/architecture/performance.md section 11 under "a restart replays past the
# slowest shard".
#
# Boot, ingest, stop. Then boot and stop again N times with **no ingest at
# all**. The load generator reports what it acknowledged, so there is an exact
# expected row count, and a restart that re-ingests nothing must land on it.
#
# Counting is a query, not instrumentation: `contains` has no zone summary, so
# a substring no row holds prunes no block and short-circuits nowhere. Every
# block is opened and every row compared, which makes `rows_scanned` the exact
# size of the corpus — and `blocks_scanned == blocks_total` is the check that
# it really was exact.
#
# `duplicated` counts block directories whose contents are byte-identical.
# Names carry min_ts and max_ts, so identical bytes are identical rows. It is a
# lower bound: a replay that lands on a different shard boundary writes the
# same rows into differently-cut blocks, and those bytes do not match.
#
# Each boot's `replayed=` is printed beside the counts, because that is the
# number that explains them.
#
#   make build && scripts/measure/restart-replay.sh
#   MIRA=/tmp/mira-0.0.3-target/release/mira ROOT=/tmp/mira-restart-base \
#     scripts/measure/restart-replay.sh
#
# MIRA is the server under test and LOADGEN the client, named separately so a
# baseline binary can be driven by the same generator it is compared against.
set -e
BIN=${BIN:-./target/release}
MIRA=${MIRA:-$BIN/mira}
LOADGEN=${LOADGEN:-$BIN/examples/loadgen}
ROOT=${ROOT:-/tmp/mira-restart}
HTTP=${HTTP:-127.0.0.1:4329}
GRPC=${GRPC:-127.0.0.1:4326}
BOOTS=${BOOTS:-4}
MISS=zqxjw-no-such-token

rm -rf "$ROOT"
mkdir -p "$ROOT/data"

serve() { # log
  "$MIRA" --data-dir "$ROOT/data" --http "$HTTP" --grpc "$GRPC" \
          --retention 999d >"$1" 2>&1 &
  SERVER=$!
}
stop() { kill $SERVER; wait $SERVER 2>/dev/null || true; }

# The reclaimer drops unexpired blocks once free space falls under `MIN_FREE`,
# which on a volume near the floor silently deletes most of the corpus and
# turns every count below into noise. It is loud in the log, so read it: a run
# that trips this is an environment result, not a measurement.
reclaimed() { # log
  r_n=$(grep -c 'nearly full' "$1" || true)
  [ "$r_n" = 0 ] || {
    echo "!! the free-space reclaimer dropped blocks $r_n times during this boot."
    echo "!! free space and re-run; these counts measure the volume, not the code."
    exit 1
  }
}

blocks() { find "$ROOT/data" -mindepth 3 -maxdepth 3 -type d 2>/dev/null; }
# Files inside a block directory only: the log is at depth 2 and is not corpus.
kbytes() { blocks | xargs du -sk | awk '{s+=$1} END{print s+0}'; }

# Wait until the block count stops moving: a boot that replays publishes for a
# while, and counting before it finishes counts a corpus mid-flight. `sh` has
# no local variables, so these are named not to collide with the caller's `i`.
settle() {
  s_n=-1
  s_same=0
  s_i=0
  while [ $s_same -lt 5 ] && [ $s_i -lt 120 ]; do
    s_m=$(blocks | wc -l)
    if [ "$s_m" = "$s_n" ]; then s_same=$((s_same + 1)); else s_same=0; fi
    s_n=$s_m
    sleep 1
    s_i=$((s_i + 1))
  done
}

# "<blocks_total> <blocks_scanned> <rows_scanned>" for a whole-corpus scan.
scan() { # signal, text field
  curl -s "http://$HTTP/api/v1/query" -H 'content-type: application/json' \
    -d "{\"signal\":\"$1\",\"from\":\"-24h\",\"to\":\"now\",\
         \"where\":[{\"field\":\"$2\",\"contains\":\"$MISS\"}],\"limit\":1}" \
    | sed -n 's/.*"blocks_total":\([0-9]*\),"blocks_scanned":\([0-9]*\),"rows_scanned":\([0-9]*\).*/\1 \2 \3/p'
}

count() { # label -- run with the server up
  c_label=$1
  c_out=""
  for c_sig in "logs body" "traces name"; do
    # shellcheck disable=SC2086 # the pair is two words on purpose.
    set -- $c_sig
    c_r=$(scan "$1" "$2")
    c_tot=$(echo "$c_r" | cut -d' ' -f1)
    c_scn=$(echo "$c_r" | cut -d' ' -f2)
    c_rows=$(echo "$c_r" | cut -d' ' -f3)
    [ "$c_tot" = "$c_scn" ] || echo "!! $1: scanned $c_scn of $c_tot blocks, count is partial"
    c_exp=$(eval echo "\$ACKED_$1")
    c_out="$c_out  $1 $c_rows (+$((c_rows - c_exp)))"
  done
  printf '%-16s%s\n' "$c_label" "$c_out"
}

report() { # label
  printf '%-16s  blocks=%-5s %5s MiB\n' \
    "$1" "$(blocks | wc -l | tr -d ' ')" "$(($(kbytes) / 1024))"
}

# Once, at the end: hashing the corpus costs a full read of it, and the row
# counts above are the metric this answers to.
fingerprint() {
  blocks | while read -r d; do
    printf '%s %s\n' \
      "$(find "$d" -type f | sort | xargs cksum | cksum | cut -d' ' -f1)" \
      "$(basename "$d")"
  done | sort > "$ROOT/fp.txt"
  f_total=$(wc -l < "$ROOT/fp.txt" | tr -d ' ')
  f_uniq=$(cut -d' ' -f1 "$ROOT/fp.txt" | sort -u | wc -l | tr -d ' ')
  echo "### $((f_total - f_uniq)) of $f_total blocks are byte-identical to another"
  cut -d' ' -f1 "$ROOT/fp.txt" | uniq -d | while read -r h; do
    grep "^$h " "$ROOT/fp.txt" | awk '{print "   ", $2}'
  done | head -30
}

echo "### boot 1: ingest 12s, then stop"
serve "$ROOT/b1.log"
sleep 2
"$LOADGEN" --addr "$HTTP" --for 12s --conns 8 --batch 2000 >"$ROOT/load.log" 2>&1
cat "$ROOT/load.log"
ACKED_logs=$(awk '/ logs \+ /{print $1}' "$ROOT/load.log")
ACKED_traces=$(awk '/ logs \+ /{print $4}' "$ROOT/load.log")
# Nothing downstream means anything without this, and the usual cause is a
# server that never bound because the port was already taken.
[ -n "$ACKED_logs" ] || { echo "the load generator acknowledged nothing; see $ROOT/b1.log"; exit 1; }
echo "acknowledged: logs=$ACKED_logs traces=$ACKED_traces -- the corpus should hold exactly these"
echo
settle
count "after ingest"
stop
reclaimed "$ROOT/b1.log"
report "after ingest"

i=2
while [ "$i" -le "$BOOTS" ]; do
  serve "$ROOT/b$i.log"
  settle
  count "after boot $i"
  stop
  reclaimed "$ROOT/b$i.log"
  report "after boot $i"
  sed -n 's/.*recovered from the write-ahead log \(.*\)/                  \1/p' "$ROOT/b$i.log"
  i=$((i + 1))
done

echo
fingerprint
