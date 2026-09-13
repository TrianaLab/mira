#!/bin/sh
# ingest -> offload -> list -> restore -> read back. Produces the numbers in
# architecture.md section 11 under "`--offload` costs the retention sweep and
# nothing else".
#
# The corpus is settled *and drained* before anything is measured. Settling is
# not enough on its own: a shutdown leaves frames above the slowest shard's
# pin, the next boot replays them, and the corpus grows on the first restart
# after an ingest even with no client attached. A2 and D are two different
# processes, so a baseline taken before that has finished is compared against a
# corpus that absorbed it in between, and the diff then reports the replay
# rather than the copy. Phase A1b boots until there is nothing left to replay.
#
#   make build && scripts/measure/offload-cycle.sh
set -e
BIN=${BIN:-./target/release}
ROOT=${ROOT:-/tmp/mira-measure}
HTTP=${HTTP:-127.0.0.1:4319}
GRPC=${GRPC:-127.0.0.1:4316}

rm -rf "$ROOT/data" "$ROOT/cold" "$ROOT/restored"
mkdir -p "$ROOT/data" "$ROOT/cold" "$ROOT/restored"

# Two queries: page one of the newest logs (row content), and a predicate the
# block filters cannot prune (every block read, rows_matched over all of them).
PAGE='{"signal":"logs","from":"-24h","to":"now","limit":100000}'
SCAN='{"signal":"logs","from":"-24h","to":"now","where":[{"field":"severity_number","gte":17}],"limit":5}'
STATS='"blocks_total":[0-9]*,"blocks_scanned":[0-9]*,"rows_scanned":[0-9]*,"rows_matched":[0-9]*'

# `elapsed_us` is wall-clock and is the only field in the envelope that cannot
# be equal between two processes. Everything else is content, including
# blocks_total / blocks_scanned / rows_scanned / rows_matched and the cursor.
snap() {
  for d in "$PAGE" "$SCAN"; do
    curl -s "http://$HTTP/api/v1/query" -H 'content-type: application/json' -d "$d" \
      | sed 's/"elapsed_us":[0-9]*/"elapsed_us":-/'
    echo
  done > "$1"
}

serve() { # data dir, log, extra args
  d=$1; l=$2; shift 2
  "$BIN/mira" --data-dir "$d" --http "$HTTP" --grpc "$GRPC" "$@" >"$l" 2>&1 &
  SERVER=$!
}

stop() { kill $SERVER; wait $SERVER 2>/dev/null || true; }

# The reclaimer drops unexpired blocks once free space falls under `MIN_FREE`.
# On a volume near the floor that silently deletes most of the corpus, and
# every comparison below then reports a difference that is the volume's and not
# the copy's. It is loud in the log, so read it.
reclaimed() { # log
  r_n=$(grep -c 'nearly full' "$1" || true)
  [ "$r_n" = 0 ] || {
    echo "!! the free-space reclaimer dropped blocks $r_n times ($1)."
    echo "!! free space and re-run; this run measures the volume, not the code."
    exit 1
  }
}

blocks() { find "$1" -mindepth 3 -maxdepth 3 -type d 2>/dev/null | wc -l | tr -d ' '; }

# Wait until the block count has stopped moving. A1 is killed with frames still
# in the log, so A2 boots into a replay and publishes for a while: a baseline
# taken before that finishes is a baseline over a corpus that is still growing,
# and the after-restore comparison then fails for a reason that has nothing to
# do with the copy. Ask the directory, not the clock.
#
# `sh` has no local variables, so every name in here is prefixed: an earlier
# draft used `i` and silently ate the drain loop's counter.
settle() {
  s_n=-1
  s_same=0
  s_i=0
  while [ $s_same -lt 5 ] && [ $s_i -lt 180 ]; do
    s_m=$(blocks "$1")
    if [ "$s_m" = "$s_n" ]; then s_same=$((s_same + 1)); else s_same=0; fi
    s_n=$s_m
    sleep 1
    s_i=$((s_i + 1))
  done
}

echo "### A1: ingest, no offload, retention 999d"
serve "$ROOT/data" "$ROOT/a1.log" --retention 999d
sleep 2
"$BIN/examples/loadgen" --addr "$HTTP" --for 12s --conns 8 --batch 2000 >"$ROOT/load.log" 2>&1
tail -3 "$ROOT/load.log"
settle "$ROOT/data"
stop
reclaimed "$ROOT/a1.log"

echo
echo "### A1b: drain the log, because a restart is not free"
# A boot replays every frame above the slowest shard's pin, and some of those
# frames are in blocks that are already published — `Wal::watermark_for` says
# so and `scripts/measure/restart-replay.sh` measures it. So the first boot
# after an ingest *grows* the corpus. A2 and D are two different processes, so
# a baseline taken before that growth has stopped is compared against a corpus
# that has since absorbed it, and the diff reports the replay rather than the
# copy. Boot until there is nothing left to replay; then the corpus is fixed
# and the two snapshots are of the same thing.
dn=0
while [ "$dn" -lt 4 ]; do
  dn=$((dn + 1))
  serve "$ROOT/data" "$ROOT/drain$dn.log" --retention 999d
  settle "$ROOT/data"
  stop
  reclaimed "$ROOT/drain$dn.log"
  sed -n 's/.*recovered from the write-ahead log \(.*\)/-- drain '"$dn"': \1/p' "$ROOT/drain$dn.log"
  grep -q 'replayed=0 ' "$ROOT/drain$dn.log" && break
done

echo
echo "### A2: baseline over the settled corpus"
serve "$ROOT/data" "$ROOT/a2.log" --retention 999d
settle "$ROOT/data"
snap "$ROOT/before.json"
echo "-- local blocks: $(blocks "$ROOT/data")   $(du -sh "$ROOT/data" | cut -f1)"
grep -o "$STATS" "$ROOT/before.json"
stop
reclaimed "$ROOT/a2.log"

echo
echo "### A3: retention sweep with --offload"
serve "$ROOT/data" "$ROOT/a3.log" --retention 1s --offload "file://$ROOT/cold"
i=0
while [ "$(blocks "$ROOT/data")" != "0" ] && [ $i -lt 120 ]; do sleep 1; i=$((i + 1)); done
sleep 1
stop
reclaimed "$ROOT/a3.log"
echo "-- local blocks after: $(blocks "$ROOT/data")"
echo "-- offloaded blocks:   $(blocks "$ROOT/cold")   $(du -sh "$ROOT/cold" | cut -f1)"
echo "-- sweep window (server start -> last 'retention dropped'):"
grep -E 'mira listening|retention dropped blocks' "$ROOT/a3.log" \
  | sed 's/ INFO mira::pipeline: retention dropped blocks signal=/ /;s/ INFO mira: mira listening.*/ START/'

echo
echo "### B: list"
# Timed without the pipe, so what is measured is the command and not `tail`.
time "$BIN/mira" offload list --offload "file://$ROOT/cold" --data-dir "$ROOT/restored" >"$ROOT/list.txt"
tail -1 "$ROOT/list.txt"

echo
echo "### C: restore into a fresh data dir"
time "$BIN/mira" offload restore --offload "file://$ROOT/cold" --data-dir "$ROOT/restored" >"$ROOT/restore.txt"
tail -1 "$ROOT/restore.txt"
echo "-- restored blocks: $(blocks "$ROOT/restored")"
echo "-- re-run must restore nothing:"
"$BIN/mira" offload restore --offload "file://$ROOT/cold" --data-dir "$ROOT/restored" \
  | grep -c 'restored$' || true

echo
echo "### D: the same two queries over the restored corpus"
serve "$ROOT/restored" "$ROOT/d.log" --retention 999d
settle "$ROOT/restored"
snap "$ROOT/after.json"
grep -o "$STATS" "$ROOT/after.json"
stop
reclaimed "$ROOT/d.log"

echo
if cmp -s "$ROOT/before.json" "$ROOT/after.json"; then
  echo "QUERY IDENTICAL"
else
  echo "QUERY DIFFERENT"
  # One response is one line and a page of logs is megabytes of it, so cut
  # before printing rather than after.
  diff "$ROOT/before.json" "$ROOT/after.json" | cut -c1-200 | head -5
fi
# Stronger than any query: the bytes themselves. .wal is the server's, not a block.
if diff -r -x .wal "$ROOT/cold" "$ROOT/restored" >"$ROOT/bytes.txt" 2>&1; then
  echo "BYTES IDENTICAL ($(blocks "$ROOT/cold") blocks)"
else
  echo "BYTES DIFFER"
  head -5 "$ROOT/bytes.txt"
fi

echo
echo "### E: the same sweep with no --offload, which is what A3 is measured against"
# A3's number on its own says nothing: "the sweep took 15 s" is only a cost if
# the sweep it replaces is known. This is that sweep -- same blocks, same
# bytes, same box, unlink and nothing else.
#
# Last, because it destroys $ROOT/restored. By here D has compared that
# directory against $ROOT/cold byte for byte, and $ROOT/cold is still the
# master copy, so nothing measured above depends on it any more.
serve "$ROOT/restored" "$ROOT/e.log" --retention 1s
i=0
while [ "$(blocks "$ROOT/restored")" != "0" ] && [ $i -lt 120 ]; do sleep 1; i=$((i + 1)); done
sleep 1
stop
reclaimed "$ROOT/e.log"
echo "-- local blocks after: $(blocks "$ROOT/restored")"
echo "-- sweep window (server start -> last 'retention dropped'):"
grep -E 'mira listening|retention dropped blocks' "$ROOT/e.log" \
  | sed 's/ INFO mira::pipeline: retention dropped blocks signal=/ /;s/ INFO mira: mira listening.*/ START/'
