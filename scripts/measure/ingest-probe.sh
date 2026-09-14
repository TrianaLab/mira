#!/bin/sh
# Where an export's milliseconds go, at three points of the connection sweep,
# plus the worker-count A/B. Produces the numbers in architecture.md section 11
# under "the ceiling is one mutex" and "each obvious suspect is ruled out".
#
# `mira::probe` at DEBUG is what turns the two diagnostic tasks on; the dump is
# cumulative from process start, so the last one in the log is the whole run.
# Each shape gets a fresh store, because a sweep that starts with blocks on
# disk starts with a different amount of work to do.
#
#   make build && scripts/measure/ingest-probe.sh
#
# BIN defaults to the release tree, ROOT to a scratch directory. The ports are
# not the defaults so this can run beside a real node.
set -e
BIN=${BIN:-./target/release}
ROOT=${ROOT:-/tmp/mira-measure}
HTTP=${HTTP:-127.0.0.1:4339}
GRPC=${GRPC:-127.0.0.1:4336}

probe() { # conns, workers ("" for the default)
  rm -rf "$ROOT/p"
  mkdir -p "$ROOT/p"
  # `${2:+...}` and not `TOKIO_WORKER_THREADS=$2`, because an empty value is not
  # an absent one: tokio parses the variable whenever it is set and panics on
  # "cannot parse integer from empty string", so the three default-worker shapes
  # aborted the server before it bound a port.
  env ${2:+TOKIO_WORKER_THREADS=$2} \
  RUST_LOG=mira=info,mira_core=info,mira::probe=debug \
  "$BIN/mira" --data-dir "$ROOT/p" --http "$HTTP" --grpc "$GRPC" \
              --retention 999d >"$ROOT/probe.log" 2>&1 &
  P=$!
  sleep 2
  "$BIN/examples/loadgen" --addr "$HTTP" --for 20s --conns "$1" --batch 8192 \
    >"$ROOT/probe.load" 2>&1
  sleep 1
  kill $P
  wait $P 2>/dev/null || true
  grep -E 'records/s|ack p50' "$ROOT/probe.load"
  # The last *dump*, not the last nine lines of the file. A dump is the nine
  # contiguous lines `diag::dump` writes starting at submit.total, and the
  # drain logs a block publish per open block after the final one — so
  # `tail -9` over everything printed the shutdown instead of the measurement.
  grep -A8 'submit.total' "$ROOT/probe.log" | tail -9
}

for C in 4 32 96; do
  echo "=================== $C connections"
  probe "$C" ""
done

# Not "more workers is the fix": four times the workers is four times the queue
# in front of the same serialised section. Back to back, same binary, same box.
for W in 12 48; do
  echo "=================== 96 connections, TOKIO_WORKER_THREADS=$W"
  probe 96 "$W"
done
