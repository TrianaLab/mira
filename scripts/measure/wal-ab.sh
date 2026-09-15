#!/bin/sh
# What the write-ahead log is worth, measured by taking it away.
#
# **This script was written to answer a different question and could not.** The
# intent was an upper bound on any log fix: section 11 names two — group commit
# and one log per signal — and neither had been priced, so `ingest.wal: false`
# was going to remove the mutex, the encode and the write from `submit` and
# leave arm B as the ceiling a perfect fix asymptotes to. It does not measure
# that, and the first run is what showed it. Arm B does not merely lose the log;
# it moves the acknowledgement back behind the block seal. The load harness is
# closed-loop, so each connection holds one export in flight and the seal
# latency becomes the throughput — 10.4k records/s at four connections against
# 2.0M with the log on. That is a measurement of `max_block_age` and the
# connection count, not of the log path's cost, and no arithmetic recovers the
# quantity that was wanted from it.
#
# What it does measure is worth having, because it is the opposite of the
# intuition the ceiling work starts from: **the log is not overhead on the
# ingest path, it is most of the throughput.** 5x at ninety-six connections and
# 190x at four. Any change to it is being made to the component holding the
# number up, which is the reason the bar for one is section 11's and not a
# micro-benchmark's.
#
# The upper bound the first paragraph wanted needs an arm that keeps
# ack-after-write and changes only the serialised section — two logs, or a
# batched one. That is a code change and not a config, so it is measured by
# building the arm, not by this script.
#
#   make build && scripts/measure/wal-ab.sh
#
# Paired and alternating, to section 3 of docs/internals/measurement.md: both
# arms run inside each pass, B before A, and what is reported is the median of
# the per-pass deltas rather than the delta of the pooled medians. The box
# drifts across five minutes; a per-pass delta cancels the drift and a pooled
# one measures it. Every pass's sign is printed, because a median near zero
# whose passes all point one way is a real effect being called noise.
set -e
BIN=${BIN:-./target/release}
ROOT=${ROOT:-/tmp/mira-walab}
HTTP=${HTTP:-127.0.0.1:4339}
GRPC=${GRPC:-127.0.0.1:4336}
SHAPES=${SHAPES:-"4 96"}
PASSES=${PASSES:-3}
FOR=${FOR:-20s}

mkdir -p "$ROOT"
# Arm B. `ingest.wal: false` is the only key that changes what an ack means, so
# it is the only one this file sets — everything else has to stay the default or
# the two arms differ by more than the log.
# Quoted, because KYAML quotes every string and every value here is one — an
# unquoted `false` is rejected by name rather than read as a boolean.
cat >"$ROOT/nowal.kyaml" <<'EOF'
ingest:
  wal: "false"
EOF

# conns, config-or-empty -> "records_per_s ack_p50 ack_p99 held_s lock_wait_s"
run() {
  rm -rf "$ROOT/p"
  mkdir -p "$ROOT/p"
  # shellcheck disable=SC2086
  env RUST_LOG=mira=info,mira_core=info,mira::probe=debug \
    "$BIN/mira" --data-dir "$ROOT/p" --http "$HTTP" --grpc "$GRPC" \
                --retention 999d ${2:+--config $2} >"$ROOT/run.log" 2>&1 &
  P=$!
  sleep 2
  # Not `|| true`: a server that refused to start makes the generator fail, and
  # under `set -e` that aborted this function's subshell silently — the arm came
  # back as an empty column and the ratio as 0.000, which reads like a
  # measurement. An arm that did not run has to say so.
  if ! "$BIN/examples/loadgen" --addr "$HTTP" --for "$FOR" --conns "$1" \
       --batch 8192 >"$ROOT/run.load" 2>&1; then
    kill $P 2>/dev/null || true
    echo "wal-ab: arm failed (config='${2:-default}'); server said:" >&2
    tail -3 "$ROOT/run.log" >&2
    exit 1
  fi
  sleep 1
  kill $P 2>/dev/null || true
  wait $P 2>/dev/null || true

  RATE=$(awk '/records\/s/{print $2}' "$ROOT/run.load")
  P50=$(awk '/ack p50/{gsub("ms","",$3); print $3}' "$ROOT/run.load")
  P99=$(awk '/ack p50/{gsub("ms","",$5); print $5}' "$ROOT/run.load")
  # The last dump, not the last nine lines: the drain logs a publish per open
  # block after it. Absent entirely in arm B, where nothing appends.
  D=$(grep -A8 'submit.total' "$ROOT/run.log" | tail -9)
  HELD=$(echo "$D" | awk '/wal.held/{gsub("s","",$4); print $4+0}')
  WAIT=$(echo "$D" | awk '/wal.lock_wait/{gsub("s","",$4); print $4+0}')
  echo "${RATE:-0} ${P50:-0} ${P99:-0} ${HELD:-0} ${WAIT:-0}"
}

for C in $SHAPES; do
  echo "=================== $C connections, $PASSES passes"
  printf '%-6s %12s %12s %9s %10s %10s\n' pass 'A rec/s' 'B rec/s' 'B/A' 'A held s' 'A wait s'
  DELTAS=""
  N=1
  while [ "$N" -le "$PASSES" ]; do
    # B first, then A. The arm that runs second is the one the box has had time
    # to warm for, so fixing the order would hand A a systematic advantage.
    # shellcheck disable=SC2046 # the fields are separate words on purpose.
    set -- $(run "$C" "$ROOT/nowal.kyaml"); BR=$1
    # shellcheck disable=SC2046 # the fields are separate words on purpose.
    set -- $(run "$C" ""); AR=$1; AHELD=$4; AWAIT=$5
    R=$(awk -v a="$AR" -v b="$BR" 'BEGIN{printf "%.3f", (a>0)? b/a : 0}')
    printf '%-6s %12s %12s %9s %10s %10s\n' "$N" "$AR" "$BR" "$R" "$AHELD" "$AWAIT"
    DELTAS="$DELTAS $R"
    N=$((N + 1))
  done
  echo "$DELTAS" | tr ' ' '\n' | grep . | sort -n | awk '
    {v[NR]=$1}
    END{
      m = (NR%2) ? v[(NR+1)/2] : v[NR/2+1]
      up=0; for(i=1;i<=NR;i++) if(v[i]>1) up++
      printf "  median B/A = %.3f over %d passes; %d of %d passes had B faster\n", m, NR, up, NR
      printf "  -> the log is worth %.0fx at this shape (B acks after the seal,\n", 1/m
      printf "     so this is the log against block latency, not against nothing)\n"
    }'
done
