#!/bin/sh
# `mira proxy` in front of N nodes, against one node on its own.
#
#   make build && scripts/measure/proxy-ab.sh
#
# Both arms are the *same binary*, which is the one way this differs from
# wal-split-ab.sh: the proxy is a subcommand, not a rebuild, so there is no
# preserved-binary dance and no chance of a version skew between the arms. Arm A
# is one `mira` with the generator pointed straight at it — the shape every
# number in docs/architecture/performance.md section 11 was taken in. Arm B is REPLICAS nodes and
# one `mira proxy`, with the generator pointed at the proxy.
#
# What the ratio means, and what it does not. B/A above 1 is not a speedup of
# anything: arm B is running REPLICAS+1 server processes and the generator on
# one twelve-core laptop, so both arms are contending for the same cores and the
# same disk and arm B is paying an extra process to do it. It is the *shape* of
# the answer that is worth having — whether splitting an export across nodes
# buys anything at all when the nodes are not on separate hardware, and how much
# the extra hop costs a read. Neither question is settled by this script on this
# box, and section 12.2.4 says so rather than quoting a number it cannot defend.
#
# Paired and alternating to section 3 of docs/internals/measurement.md: both arms
# run inside each pass, B before A, and the report is the median of the per-pass
# ratios rather than the ratio of the pooled medians. Every pass's sign is
# printed, because a median near 1.00 whose passes all point the same way is a
# real effect being called noise, and a median far from 1.00 whose passes
# disagree is noise being quoted as an effect.
#
# `--records`, not `--for`, so both arms send exactly the same bytes and the
# corpus the read leg then queries is the same size in both. The read leg is a
# wide unfiltered scan with a small `limit`, which is the shape the proxy is
# worst at: the merge cost is per row returned, the fan-out cost is per replica,
# and a query that prunes to nothing would hide both.
#
# Two controls, asserted rather than eyeballed:
#   * `0 shed` in both arms, for the reason in wal-volume.sh's header.
#   * The paged read through the proxy returns no row twice. A merge that
#     silently degraded under load — losing the cursor order, double-counting a
#     boundary row — would otherwise show up only as a throughput number that
#     looked fine.
set -e
BIN=${BIN:-./target/release}
ROOT=${ROOT:-/tmp/mira-proxy}
REPLICAS=${REPLICAS:-2}
# Arm A's node and arm B's proxy both bind $HTTP, one at a time. Arm B's
# replicas count up from $BASE, so nothing collides with a sweep on 4339.
HTTP=${HTTP:-127.0.0.1:4349}
GRPC=${GRPC:-127.0.0.1:4346}
BASE=${BASE:-4360}
SHAPES=${SHAPES:-"4 32 96"}
PASSES=${PASSES:-3}
RECORDS=${RECORDS:-4000000}
PAGE=${PAGE:-200}

if [ ! -x "$BIN/mira" ] || [ ! -x "$BIN/examples/loadgen" ]; then
  echo "proxy-ab: $BIN needs both mira and examples/loadgen" >&2
  exit 1
fi
mkdir -p "$ROOT"
PIDS=""

node() { # dir port grpc name
  "$BIN/mira" --data-dir "$1" --http "$2" --grpc "$3" --node "$4" \
              --retention 999d >>"$ROOT/run.log" 2>&1 &
  PIDS="$PIDS $!"
}

stop() {
  for P in $PIDS; do kill "$P" 2>/dev/null || true; done
  for P in $PIDS; do wait "$P" 2>/dev/null || true; done
  PIDS=""
}
trap stop EXIT INT

# The wide read, through whatever is on $HTTP. Answered in microseconds by the
# server's own timing layer, so this measures the query and not curl.
read_us() {
  # Through a variable, not straight down the pipe: the body has no trailing
  # newline, so `sed` emits none either, and three calls in a row would run into
  # one line that `sort | sed -n 2p` then reads as a single sample.
  V=$(curl -s -X POST "http://$HTTP/api/v1/query" -H 'content-type: application/json' \
        -d '{"signal":"logs","from":"-1h","limit":'"$PAGE"'}' |
      sed 's/.*"elapsed_us"://; s/[^0-9].*//')
  echo "$V"
}

# Page the whole answer and count rows against distinct rows. Bounded at 25
# pages: this is a control, not a census, and an unbounded loop over a corpus
# this size is most of the pass.
#
# Counted on the cursor rather than on any field of the row. The cursor is
# `(ts, node, seq, row)`, unique by construction, so a repeat is a paging fault
# and never a coincidence in the data — and asking for it is what makes the
# proxy splice its `cursors` array, so the control covers that too.
dupes() {
  AFTER=""
  : >"$ROOT/page"
  I=0
  while [ "$I" -lt 25 ]; do
    R=$(curl -s -X POST "http://$HTTP/api/v1/query" -H 'content-type: application/json' \
          -d '{"signal":"logs","from":"-1h","cursors":"true","limit":'"$PAGE$AFTER"'}')
    # `sed -n …p` and not `grep`: a page with no rows carries no `cursors` key,
    # and grep would exit 1 there and take this whole subshell down with it
    # under `set -e` — a control that dies silently is worse than no control.
    echo "$R" | sed -n 's/.*"cursors":\[\([^]]*\)\].*/\1/p' | tr ',' '\n' >>"$ROOT/page"
    N=$(echo "$R" | grep -o '"next":"[0-9.]*"' | sed 's/"next":"//; s/"//')
    [ -n "$N" ] || break
    AFTER=',"after":"'$N'"'
    I=$((I + 1))
  done
  ROWS=$(wc -l <"$ROOT/page" | tr -d ' ')
  UNIQ=$(sort -u "$ROOT/page" | wc -l | tr -d ' ')
  # Zero rows would satisfy "no row twice" without reading anything, which is
  # how a control quietly stops being one.
  [ "$ROWS" -gt 0 ] ||
    { echo "proxy-ab: the paged read returned nothing at all" >&2; exit 1; }
  [ "$ROWS" = "$UNIQ" ] ||
    { echo "proxy-ab: the paged read returned $ROWS rows and $UNIQ distinct ones" >&2
      exit 1; }
  echo "$ROWS"
}

# "records_per_s ack_p50 read_us paged_rows"
load() { # conns
  if ! "$BIN/examples/loadgen" --addr "$HTTP" --records "$RECORDS" --conns "$1" \
       --batch 8192 >"$ROOT/run.load" 2>&1; then
    echo "proxy-ab: the generator failed; server said:" >&2
    tail -5 "$ROOT/run.log" >&2
    exit 1
  fi
  RATE=$(awk '/records\/s/{print $2}' "$ROOT/run.load")
  P50=$(awk '/ack p50/{gsub("ms","",$3); print $3}' "$ROOT/run.load")
  SHED=$(awk '/records\/s/{for(i=1;i<=NF;i++) if($(i+1)=="shed") print $i}' "$ROOT/run.load")
  [ "${SHED:-0}" = "0" ] ||
    { echo "proxy-ab: shed $SHED exports at $1 conns; df:" >&2; df -h "$ROOT" >&2; exit 1; }
  # Three reads, median, because the first one after a load pays for whatever
  # the flushers are still doing and a single sample reads that as the query.
  U=$(for _ in 1 2 3; do read_us; done | sort -n | sed -n 2p)
  echo "${RATE:-0} ${P50:-0} ${U:-0} $(dupes)"
}

arm_a() { # conns
  rm -rf "$ROOT/a"
  mkdir -p "$ROOT/a"
  node "$ROOT/a" "$HTTP" "$GRPC" direct
  sleep 2
  load "$1"
  stop
}

arm_b() { # conns
  rm -rf "$ROOT/b"
  R=""
  I=0
  while [ "$I" -lt "$REPLICAS" ]; do
    mkdir -p "$ROOT/b/$I"
    node "$ROOT/b/$I" "127.0.0.1:$((BASE + I * 2))" "127.0.0.1:$((BASE + I * 2 + 1))" "px-$I"
    R="$R --replica http://127.0.0.1:$((BASE + I * 2))"
    I=$((I + 1))
  done
  # shellcheck disable=SC2086
  "$BIN/mira" proxy --http "$HTTP" $R >>"$ROOT/run.log" 2>&1 &
  PIDS="$PIDS $!"
  sleep 2
  load "$1"
  stop
}

echo "$REPLICAS replicas behind the proxy; $RECORDS records an arm"
for C in $SHAPES; do
  echo "=================== $C connections, $PASSES passes"
  printf '%-5s %11s %11s %7s %9s %9s %7s\n' \
    pass 'A rec/s' 'B rec/s' 'B/A' 'A read us' 'B read us' 'B/A'
  RATIOS=""
  N=1
  while [ "$N" -le "$PASSES" ]; do
    # B first: the arm that runs second has the warmer page cache, and B is the
    # arm making the claim.
    # shellcheck disable=SC2046 # the fields are separate words on purpose.
    set -- $(arm_b "$C"); BR=$1; BU=$3; BP=$4
    # shellcheck disable=SC2046 # the fields are separate words on purpose.
    set -- $(arm_a "$C"); AR=$1; AU=$3; AP=$4
    RR=$(awk -v a="$AR" -v b="$BR" 'BEGIN{printf "%.3f", (a>0)? b/a : 0}')
    RU=$(awk -v a="$AU" -v b="$BU" 'BEGIN{printf "%.3f", (a>0)? b/a : 0}')
    printf '%-5s %11s %11s %7s %9s %9s %7s   paged %s/%s\n' \
      "$N" "$AR" "$BR" "$RR" "$AU" "$BU" "$RU" "$BP" "$AP"
    RATIOS="$RATIOS $RR"
    N=$((N + 1))
  done
  echo "$RATIOS" | tr ' ' '\n' | grep . | sort -n | awk '
    {v[NR]=$1}
    END{
      m = (NR%2) ? v[(NR+1)/2] : v[NR/2+1]
      up=0; for(i=1;i<=NR;i++) if(v[i]>1) up++
      printf "  median B/A rec/s = %.3f over %d passes; %d of %d favoured the proxy\n", m, NR, up, NR
      if (up==NR || up==0)
        printf "  every pass agrees in sign, so the direction is the reading\n"
      else
        printf "  passes disagree in sign -- noise, do not quote this median\n"
    }'
done
