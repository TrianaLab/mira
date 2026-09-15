#!/bin/sh
# One log per signal against the single log it replaces, as two binaries.
#
# The fix cannot be a config flag — it is a directory layout and three mutexes —
# so the arms are two builds rather than two configs, and the only way the
# comparison means anything is that arm A is a *preserved binary* of the parent
# commit rather than a rebuild from a dirty tree. Build it once, keep it, and
# point A_BIN at it:
#
#   git stash && make build && mkdir -p /tmp/mira-ab/A/examples \
#     && cp target/release/mira /tmp/mira-ab/A/ \
#     && cp target/release/examples/loadgen /tmp/mira-ab/A/examples/ && git stash pop
#   make build && scripts/measure/wal-split-ab.sh
#
# Each arm runs its own `loadgen`, not a shared one. The generator is closed
# loop, so a version skew between the two would show up as a throughput
# difference that has nothing to do with the server.
#
# Paired and alternating to section 3 of docs/internals/measurement.md: both arms
# run inside each pass, B before A, and the report is the median of the per-pass
# ratios rather than the ratio of the pooled medians — the box drifts across the
# sitting and a per-pass ratio cancels the drift where a pooled one measures it.
# Every pass's sign is printed, because a median near 1.00 whose passes all point
# the same way is a real effect being called noise, and a median far from 1.00
# whose passes disagree is noise being quoted as an effect.
#
# `wal.lock_wait` and `wal.held` are printed for both arms and are the direct
# read on whether the split did what it was built to do. The rate alone cannot
# say: if the mutex clears and the rate does not move, the constraint was never
# the mutex, and that is the answer the phase needs rather than a failure.
set -e
A_BIN=${A_BIN:-/tmp/mira-ab/A}
B_BIN=${B_BIN:-./target/release}
ROOT=${ROOT:-/tmp/mira-split}
HTTP=${HTTP:-127.0.0.1:4339}
GRPC=${GRPC:-127.0.0.1:4336}
SHAPES=${SHAPES:-"4 32 96"}
PASSES=${PASSES:-3}
FOR=${FOR:-20s}

for B in "$A_BIN" "$B_BIN"; do
  [ -x "$B/mira" ] && [ -x "$B/examples/loadgen" ] ||
    { echo "wal-split-ab: $B needs both mira and examples/loadgen" >&2; exit 1; }
done
mkdir -p "$ROOT"

# bin -> "records_per_s ack_p50 held_ms write_ms wait_ms"
run() {
  rm -rf "$ROOT/p"
  mkdir -p "$ROOT/p"
  env RUST_LOG=mira=info,mira_core=info,mira::probe=debug \
    "$1/mira" --data-dir "$ROOT/p" --http "$HTTP" --grpc "$GRPC" \
              --retention 999d >"$ROOT/run.log" 2>&1 &
  P=$!
  sleep 2
  if ! "$1/examples/loadgen" --addr "$HTTP" --for "$FOR" --conns "$2" \
       --batch 8192 >"$ROOT/run.load" 2>&1; then
    kill $P 2>/dev/null || true
    echo "wal-split-ab: arm failed ($1); server said:" >&2
    tail -3 "$ROOT/run.log" >&2
    exit 1
  fi
  sleep 1
  kill $P 2>/dev/null || true
  wait $P 2>/dev/null || true

  RATE=$(awk '/records\/s/{print $2}' "$ROOT/run.load")
  P50=$(awk '/ack p50/{gsub("ms","",$3); print $3}' "$ROOT/run.load")
  # A shedding server spins the closed-loop generator, and a full disk sheds:
  # `write_all` returning `os error 28` is counted as a failed export, so the
  # arm comes back as a throughput collapse that reads like a regression. That
  # is how four RAM-disk figures were published and withdrawn — see the header
  # of wal-volume.sh. Assert it rather than eyeballing the log afterwards.
  SHED=$(awk '/records\/s/{for(i=1;i<=NF;i++) if($(i+1)=="shed") print $i}' "$ROOT/run.load")
  [ "${SHED:-0}" = "0" ] ||
    { echo "wal-split-ab: $1 at $2 conns shed $SHED exports; df:" >&2
      df -h "$ROOT" >&2; exit 1; }
  # The last dump, not the last nine lines of the file: the drain logs a publish
  # per open block after it, so `tail -9` reads the shutdown.
  D=$(grep -A8 'submit.total' "$ROOT/run.log" | tail -9)
  HELD=$(echo "$D" | awk '/wal.held/{gsub("ms","",$6); print $6+0}')
  WRITE=$(echo "$D" | awk '/wal.write/{gsub("ms","",$6); print $6+0}')
  WAIT=$(echo "$D" | awk '/wal.lock_wait/{gsub("ms","",$6); print $6+0}')
  echo "${RATE:-0} ${P50:-0} ${HELD:-0} ${WRITE:-0} ${WAIT:-0}"
}

for C in $SHAPES; do
  echo "=================== $C connections, $PASSES passes, $FOR each arm"
  printf '%-5s %11s %11s %7s %22s %22s\n' \
    pass 'A rec/s' 'B rec/s' 'B/A' 'A wait/held/write ms' 'B wait/held/write ms'
  RATIOS=""
  N=1
  while [ "$N" -le "$PASSES" ]; do
    # B first. The arm that runs second has the warmer page cache, and B is the
    # arm making the claim — give the advantage to the one it is claimed against.
    # shellcheck disable=SC2046 # the fields are separate words on purpose.
    set -- $(run "$B_BIN" "$C"); BR=$1; BH=$3; BW=$4; BQ=$5
    # shellcheck disable=SC2046 # the fields are separate words on purpose.
    set -- $(run "$A_BIN" "$C"); AR=$1; AH=$3; AW=$4; AQ=$5
    R=$(awk -v a="$AR" -v b="$BR" 'BEGIN{printf "%.3f", (a>0)? b/a : 0}')
    printf '%-5s %11s %11s %7s %22s %22s\n' "$N" "$AR" "$BR" "$R" \
      "$AQ/$AH/$AW" "$BQ/$BH/$BW"
    RATIOS="$RATIOS $R"
    N=$((N + 1))
  done
  echo "$RATIOS" | tr ' ' '\n' | grep . | sort -n | awk '
    {v[NR]=$1}
    END{
      m = (NR%2) ? v[(NR+1)/2] : v[NR/2+1]
      up=0; for(i=1;i<=NR;i++) if(v[i]>1) up++
      printf "  median B/A = %.3f over %d passes; %d of %d had the split faster\n", m, NR, up, NR
      if (up==NR || up==0)
        printf "  every pass agrees in sign, so the direction is the reading\n"
      else
        printf "  passes disagree in sign -- noise, do not quote this median\n"
    }'
done
