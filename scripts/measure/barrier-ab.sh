#!/bin/sh
# What is `block::publish`'s device barrier worth?
#
# A published block costs **eleven `F_FULLFSYNC`** — one per table
# (`block.rs:280`, five of them), one per sidecar (`:626`, three), and the three
# directories (`:537`, `:544`, `:628`). On macOS `File::sync_all` is not a
# page-cache flush: it is `fcntl(F_FULLFSYNC)`, a device-wide cache barrier, and
# section 10 publishes 4,230 us for one. A plain `fsync(2)` on the same volume
# is not in the same class, which is the whole reason this is worth pricing.
# performance-ingest.md leaves two candidates open for why the flusher is slow
# and this prices the second of them: the volume, in the one form the engine can
# actually stop paying for.
#
# Arm B is the same tree built `--features weak-sync-ab`, which replaces every
# barrier with `fsync(2)` and nothing else. That trades the power-loss guarantee
# `sync_all` documents, so it is a compile-time feature rather than a config key
# -- a released binary has no code path that reaches it. Build both arms first:
#
#   cargo build --release --locked && cp target/release/mira /tmp/ab/strong
#   cargo build --release --locked --features weak-sync-ab \
#     && cp target/release/mira /tmp/ab/weak
#   ROUNDS=8 scripts/measure/barrier-ab.sh
#
# ABBA-interleaved: strong first on even rounds, weak first on odd, so this
# box's 23% day-to-day drift cancels *within* a round instead of loading onto
# whichever arm ran second. Eight rounds because five is not enough here -- two
# earlier five- and thirteen-pass runs of this exact intervention returned 1.40x
# and 0.991x, and the disagreement was the sample, not the mechanism.
#
# RECORDS is 12M and not more on purpose. conn-sweep.sh exits non-zero when an
# export sheds and such a reading is not comparable; 24M sheds on this box and
# 12M does not. The shed count is carried into the table anyway rather than
# being trusted to stay zero.
set -e
ROUNDS=${ROUNDS:-8}
RECORDS=${RECORDS:-12000000}
CONNS=${CONNS:-32}
DIR=${DIR:-/tmp/ab}
SETTLE=${SETTLE:-20}
OUT="$DIR/results.tsv"

mkdir -p "$DIR"
for arm in strong weak; do
  [ -x "$DIR/$arm" ] || { echo "barrier-ab: build $DIR/$arm first (see header)" >&2; exit 1; }
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
  if [ $((i % 2)) -eq 0 ]; then first=strong; second=weak; else first=weak; second=strong; fi
  a=$(one $first);  sleep "$SETTLE"
  b=$(one $second); sleep "$SETTLE"
  printf '%s\t%s\t%s\n' "$first"  "$i" "$a" >>"$OUT"
  printf '%s\t%s\t%s\n' "$second" "$i" "$b" >>"$OUT"
  echo "round $i done" >&2
  i=$((i + 1))
done

# Per-round paired ratios, oriented so >1 always means "the barrier costs
# something": rates and cores weak/strong, latencies strong/weak. The sign count
# is the reading -- docs/internals/measurement.md section 3 -- and the per-round
# column beside it is there because this effect is not stationary.
python3 - "$OUT" <<'PY'
import statistics as st, sys
rows = {}
for line in open(sys.argv[1]):
    f = line.split()
    if "ERR" in f: continue
    rows.setdefault(int(f[1]), {})[f[0]] = [float(x) for x in f[2:8]]
names = ["records/s", "per-core", "cores", "ack p50", "ack p99", "rss"]
higher_is_weak_win = {0, 1, 2}
full = sorted(r for r in rows if len(rows[r]) == 2)
print(f"\n{len(full)} complete rounds\n")
for i, n in enumerate(names):
    rs = [(rows[r]["weak"][i] / rows[r]["strong"][i]) if i in higher_is_weak_win
          else (rows[r]["strong"][i] / rows[r]["weak"][i]) for r in full]
    fav = sum(1 for x in rs if x > 1)
    verdict = "agreed" if fav in (0, len(rs)) else "SIGNS SPLIT, not quotable"
    print(f"{n:10s} median {st.median(rs):6.3f}  {fav}/{len(rs)} favourable  "
          f"[{min(rs):.3f}, {max(rs):.3f}]  {verdict}")
    print(f"{'':10s} per round: " + " ".join(f"{x:.3f}" for x in rs))
PY
