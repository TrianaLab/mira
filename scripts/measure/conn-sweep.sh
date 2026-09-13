#!/bin/sh
# The connection sweep behind architecture.md section 11: ingest rate, consumed
# CPU, per-core ceiling, ack latency, peak RSS and hot-tier cost, at four
# connection counts, three passes each. It is the script named as `provenance`
# by most of measurements.kyaml, and its output feeds straight back in:
#
#   make build && scripts/measure/conn-sweep.sh
#   make measurements-ingest RUN=/tmp/mira-sweep/run.json
#
# Every pass appends one JSON object to $RUN, so the file that comes out is the
# whole sweep rather than its last row, and `xtask measurements ingest` takes
# the **median per key across passes** — which is the standard the published
# figures were taken to. Three passes is the minimum that has a median; raise
# PASSES if a shape looks unstable, never lower it to one.
#
# Why it takes hours. RECORDS is the size of the published corpus and the run
# is what produces it, so this is not a smoke test with the numbers scaled down:
# 27.1M log records at ~1.3M records/s is twenty minutes per pass before the
# 96-connection shape, and there are twelve passes. Two knobs make it a smoke
# test instead, and neither produces a number fit to publish:
#
#   SHAPES="1 4" PASSES=1 RECORDS=2000000 scripts/measure/conn-sweep.sh
#
# A record count, not a clock. `--records N` divides the rounds evenly across
# the connections up front, so a pass on a slow box sends exactly the bytes a
# pass on a fast one did — which is what makes two passes comparable at all, and
# what lets the corpus census below be a property of the arguments rather than
# of how the laptop felt. See docs/internals/measurement.md section 1.
#
# Each shape gets a fresh store. A sweep that starts with blocks on disk starts
# with a different amount of work to do, and the cost-per-byte figure would be
# measuring the previous shape's leftovers.
set -e
BIN=${BIN:-./target/release}
ROOT=${ROOT:-/tmp/mira-sweep}
RUN=${RUN:-$ROOT/run.json}
HTTP=${HTTP:-127.0.0.1:4339}
GRPC=${GRPC:-127.0.0.1:4336}
SHAPES=${SHAPES:-"1 4 32 96"}
PASSES=${PASSES:-3}
BATCH=${BATCH:-8192}

# 826 rounds x 4 connections x (2 x 8192 + 4 x 3) records. Chosen so the
# 4-connection shape lands on exactly 27,066,368 logs, 27,066,368 spans and
# 39,648 points — the corpus every query row in section 11 divides by. The other
# shapes get the same offered load and a slightly different round split, because
# a whole number of rounds per connection is what makes the pass reproducible.
RECORDS=${RECORDS:-54172384}

# The shape whose store is the published corpus. Four, because it is the shape
# section 11 publishes the paired comparison at.
CORPUS_CONNS=${CORPUS_CONNS:-4}

mkdir -p "$ROOT"
: >"$RUN"

pass() { # conns
  rm -rf "$ROOT/d"
  mkdir -p "$ROOT/d"
  "$BIN/mira" --data-dir "$ROOT/d" --http "$HTTP" --grpc "$GRPC" \
              --retention 999d >"$ROOT/server.log" 2>&1 &
  P=$!
  sleep 2
  "$BIN/examples/loadgen" --addr "$HTTP" --records "$RECORDS" --conns "$1" \
    --batch "$BATCH" --pid "$P" --data-dir "$ROOT/d" --emit "$RUN" \
    | tee "$ROOT/pass.out"
  # Before the kill: the census is of a store the server still owns, which is
  # the only state a reader can reproduce. After a kill it is a store with an
  # unreplayed log beside it, and the block count is whatever the timing was.
  [ "$1" = "$CORPUS_CONNS" ] && census
  kill $P
  wait $P 2>/dev/null || true
}

# The corpus keys, which loadgen cannot emit because they are properties of the
# store rather than of the run: how many Arrow tables it left, how many bytes,
# how many log blocks, and the record counts the query table divides by. One
# more JSON line in the same file, so `ingest` reads it with everything else.
census() {
  # The block directories, not the data directory: `.wal` is not Arrow and the
  # registry's `corpus.gib` is the Arrow the queries read. A block sits at
  # <signal>/p=<partition>/<block>, which is depth three — the same expression
  # restart-replay.sh and offload-cycle.sh already use — and the `.arrow` files
  # inside one are its tables.
  awk -v gib="$(find "$ROOT/d" -mindepth 3 -maxdepth 3 -type d | xargs du -sk \
                 | awk '{s += $1} END {printf "%.2f", s / 1048576}')" \
      -v tables="$(find "$ROOT/d" -name '*.arrow' | wc -l | tr -d ' ')" \
      -v blocks="$(find "$ROOT/d/logs" -mindepth 2 -maxdepth 2 -type d | wc -l | tr -d ' ')" \
      '/logs \+/ {
         printf "{\"corpus.records.logs\":%d,\"corpus.records.points\":%d,", $1, $7
         printf "\"corpus.gib\":%s,\"corpus.tables\":%s,\"corpus.blocks.logs\":%s}\n", \
                gib, tables, blocks
       }' "$ROOT/pass.out" | tee -a "$RUN"
}

for C in $SHAPES; do
  N=1
  while [ "$N" -le "$PASSES" ]; do
    echo "=================== $C connections, pass $N of $PASSES"
    pass "$C"
    N=$((N + 1))
  done
done

echo
echo "$(grep -c . "$RUN") readings in $RUN. Fold them in with:"
echo "  make measurements-ingest RUN=$RUN"
