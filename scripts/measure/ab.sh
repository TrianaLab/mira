#!/bin/sh
# Price one ingest change against the build without it.
#
# Two binaries in, a signed verdict per axis out. The arms are builds and not a
# runtime switch on purpose: an interesting intervention is usually one no
# released binary should be able to reach, and `--features` is visible in the
# build line of whoever measured.
#
# An arm is a **directory** laid out the way conn-sweep.sh reads a build — the
# server at `mira` and the client at `examples/loadgen` — because that is the
# script doing the actual run and it takes a build directory, not a binary:
#
#   arm() { mkdir -p "$1/examples"
#           cp target/release/mira "$1/" && cp target/release/examples/loadgen "$1/examples/"; }
#   cargo build --release --locked --examples && arm /tmp/ab/a
#   git stash && cargo build --release --locked --examples && arm /tmp/ab/b
#   A=a B=b ROUNDS=8 scripts/measure/ab.sh
#
# B is the arm under test and A the one it has to beat: every ratio printed is
# oriented so **above 1 means B wins**.
#
# ABBA-interleaved: A first on even rounds, B first on odd, so this box's 23%
# day-to-day drift cancels *within* a round instead of loading onto whichever
# arm ran second. Eight rounds because five is not enough here -- two five- and
# thirteen-pass runs of the *same* barrier intervention returned 1.40x and
# 0.991x, and the disagreement was the sample, not the mechanism. The verdict
# line is the sign count, per docs/internals/measurement.md section 3, and the
# per-round series beside it is there because an effect here need not be
# stationary.
#
# RECORDS is 12M and not more on purpose. conn-sweep.sh exits non-zero when an
# export sheds and such a reading is not comparable; 24M sheds on this box and
# 12M does not. The shed count is carried into the table anyway rather than
# being trusted to stay zero.
#
# Two runs of this are written up: performance-barrier.md (the device barrier,
# `--features weak-sync-ab`) and performance-overlap.md (the flusher's publish
# overlapped with the next block's encode).
set -e
A=${A:-strong}
B=${B:-weak}
ROUNDS=${ROUNDS:-8}
RECORDS=${RECORDS:-12000000}
CONNS=${CONNS:-32}
DIR=${DIR:-/tmp/ab}
SETTLE=${SETTLE:-20}
OUT="$DIR/results.tsv"

mkdir -p "$DIR"
for arm in "$A" "$B"; do
  for exe in mira examples/loadgen; do
    [ -x "$DIR/$arm/$exe" ] ||
      { echo "ab: $DIR/$arm/$exe missing -- lay the arm out first (see header)" >&2; exit 1; }
  done
done
: >"$OUT"

# arm -> one tab-separated row of the six keys conn-sweep emits, plus shed.
one() {
  arm=$1
  rm -f "$DIR/$arm.json"
  BIN=$DIR/$arm ROOT=$DIR/d-$arm RUN=$DIR/$arm.json \
  SHAPES=$CONNS PASSES=1 RECORDS=$RECORDS HTTP=127.0.0.1:4439 GRPC=127.0.0.1:4436 \
    scripts/measure/conn-sweep.sh >"$DIR/$arm.out" 2>&1 || true
  shed=$(grep -o '[0-9]* shed' "$DIR/$arm.out" | head -1 | cut -d' ' -f1)
  CONNS=$CONNS python3 - "$DIR/$arm.json" "${shed:-NA}" <<'PY'
import json, os, sys
c = os.environ["CONNS"]
keys = [f"{k}.conns{c}" for k in ("ingest.records_per_s", "ingest.per_core",
        "ingest.cores", "ingest.ack_p50_ms", "ingest.ack_p99_ms", "rss_mib")]
try:
    o = json.loads(open(sys.argv[1]).read().strip().splitlines()[-1])
except Exception:
    print("\t".join(["ERR"] * len(keys) + [sys.argv[2]])); raise SystemExit
print("\t".join([str(o.get(k, "")) for k in keys] + [sys.argv[2]]))
PY
}

i=0
while [ "$i" -lt "$ROUNDS" ]; do
  if [ $((i % 2)) -eq 0 ]; then first=$A; second=$B; else first=$B; second=$A; fi
  a=$(one "$first");  sleep "$SETTLE"
  b=$(one "$second"); sleep "$SETTLE"
  printf '%s\t%s\t%s\n' "$first"  "$i" "$a" >>"$OUT"
  printf '%s\t%s\t%s\n' "$second" "$i" "$b" >>"$OUT"
  echo "round $i done" >&2
  i=$((i + 1))
done

# Per-round paired ratios, oriented so >1 always means "B wins": rates and
# cores B/A, latencies and RSS A/B. The sign count is the reading --
# docs/internals/measurement.md section 3 -- and the per-round column beside it
# is there because an effect here need not be stationary.
A=$A B=$B python3 - "$OUT" <<'PY'
import os, statistics as st, sys
a, b = os.environ["A"], os.environ["B"]
rows = {}
for line in open(sys.argv[1]):
    f = line.split()
    if "ERR" in f: continue
    rows.setdefault(int(f[1]), {})[f[0]] = [float(x) for x in f[2:8]]
names = ["records/s", "per-core", "cores", "ack p50", "ack p99", "rss"]
bigger_is_better = {0, 1, 2}
full = sorted(r for r in rows if len(rows[r]) == 2)
# Loud, because a pass that fails outright still prints "round N done" above --
# conn-sweep's exit is swallowed so a shed round can be recorded rather than
# abort the run, and that swallows a broken invocation just as quietly. This
# script once shipped unable to run at all for exactly that reason.
if not full:
    print(f"\nNO COMPLETE ROUNDS -- every pass errored. Read {sys.argv[1]}'s siblings: *.out")
    raise SystemExit(1)
print(f"\n{len(full)} complete rounds, {b} (B) over {a} (A)\n")
for i, n in enumerate(names):
    rs = [(rows[r][b][i] / rows[r][a][i]) if i in bigger_is_better
          else (rows[r][a][i] / rows[r][b][i]) for r in full]
    fav = sum(1 for x in rs if x > 1)
    verdict = "agreed" if fav in (0, len(rs)) else "SIGNS SPLIT, not quotable"
    print(f"{n:10s} median {st.median(rs):6.3f}  {fav}/{len(rs)} favourable  "
          f"[{min(rs):.3f}, {max(rs):.3f}]  {verdict}")
    print(f"{'':10s} per round: " + " ".join(f"{x:.3f}" for x in rs))
PY
