#!/bin/sh
# Is the log's `write(2)` serialised on the mutex, or throttled by the kernel's
# writeback? The control that decides whether either proposed log fix is worth
# building.
#
# **It does not work on this box, and the numbers it produced are withdrawn.**
# Four RAM-disk arms were run and reported — 0.291x and 0.296x at 4 GiB/20s,
# 0.117x (HFS+) and 0.127x (APFS) at 2 GiB/8s. Every one of them is ENOSPC. The
# log is truncated only when `wal_sweep` is called with `truncating`, which
# `wal_maintenance` does on `ticks % 240` at a 250 ms period — once every 60 s —
# so no run here ever reclaims a byte and the log grows at the full ingest rate
# (3.82 GiB measured in 20 s at 96 connections). Arm B fills its disk, every
# later `write_all` returns `os error 28`, and the failures are counted as shed
# exports: 51,113 of 53,969, which reads as a throughput collapse and is a disk
# full. `wal.lock_wait n=53969` against `wal.held n=2856` in the same dump is the
# tell — both probes are unconditional on one path, so the gap is the `?`.
#
# The instrument is self-defeating rather than merely mis-sized. Arm B fills the
# disk *because* it appends faster, so the speedup it exists to detect is what
# guarantees the failure that hides it; sizing the disk for the win means sizing
# it for a win not yet measured, and 20 s of headroom is 21% of this machine's
# memory. Two controls below it are sound and are what the run is now good for:
# the null arm (a second directory on the same APFS volume) came back 0.950 /
# 1.100 / 1.055 with signs split, so the symlink costs nothing; and a 2 GiB RAM
# disk allocated but left unused came back 1,966,523 records/s, so the memory
# tax is not a confound either. It is the disk filling, and nothing else.
#
# Answering the writeback question needs a bounded log — a sweep period short
# enough to truncate inside a run — which is a code change, not this script.
#
# section 11 diagnoses the ingest plateau as one mutex held across a `write(2)`,
# and both named fixes — group commit, one log per signal — assume the cost
# inside that section is the log's own. There is a third possibility neither
# addresses. `wal.write` moves ~0.79 MiB in 1.8-1.9 ms, about 430 MiB/s, while
# the same append in isolation on this volume runs near 1,000 MiB/s; and the
# engine dirties roughly as many bytes again sealing blocks. If `write(2)` is
# blocking on the volume's dirty-page limit, the serialisation is in the kernel
# and three mutexes buy nothing, because the bytes and the device do not change.
#
# So: move the log off the volume the blocks are on, and change nothing else.
# Arm B symlinks `<data-dir>/.wal` at a RAM disk, which has no writeback path at
# all. No code change and no config — the log's directory is the only variable.
#
#   make build && scripts/measure/wal-volume.sh
#
#   B >> A   the write is writeback-bound. Neither log fix addresses it and the
#            ceiling work belongs on the bytes or the device, not the mutex.
#   B ~= A   the write is not the volume's fault, the serialised section is real,
#            and shortening or splitting it is worth building.
#
# Paired and alternating to docs/internals/measurement.md section 3: both arms
# run in each pass, B first, and the report is the median of per-pass ratios
# with every pass's sign shown.
set -e
BIN=${BIN:-./target/release}
ROOT=${ROOT:-/tmp/mira-walvol}
HTTP=${HTTP:-127.0.0.1:4339}
GRPC=${GRPC:-127.0.0.1:4336}
SHAPES=${SHAPES:-"32 96"}
PASSES=${PASSES:-3}
FOR=${FOR:-20s}
# 4 GiB in 512-byte sectors. The log is truncated as blocks publish, so it does
# not hold a whole run — this is headroom over the largest backlog seen, not the
# run's byte total. Too small and arm B measures ENOSPC instead of writeback.
SECTORS=${SECTORS:-8388608}

# Set VOL to run arm B against a directory instead of a fresh RAM disk. The
# null control — a second directory on the *same* APFS volume — is what says
# whether this script measures the device or measures the symlink, and it has to
# be run before any RAM disk number is read as a volume effect.
VOL=${VOL:-}
cleanup() {
  [ -n "$DEV" ] && { diskutil eject "$DEV" >/dev/null 2>&1 || hdiutil detach "$DEV" >/dev/null 2>&1 || true; }
}
trap cleanup EXIT INT TERM

if [ -n "$VOL" ]; then
  mkdir -p "$VOL"
  echo "arm B log volume: $VOL (supplied, no RAM disk)"
else
  VOL=/Volumes/MIRAWALRD
  # `hdiutil` pads the device with both spaces and tabs, so take the first field
  # rather than stripping blanks — a name with a trailing tab reaches the
  # formatter as a path that does not exist, and the failure names /dev/rdisk<n>
  # rather than what was actually passed.
  DEV=$(hdiutil attach -nomount "ram://$SECTORS" | awk '{print $1; exit}')
  # APFS, not HFS+. The data volume is APFS, and the first run of this script
  # used `newfs_hfs` — which made the filesystem a second variable alongside the
  # device, so the 0.117x it reported was not attributable to the volume. HFS+
  # serialises an extending write on a volume-wide catalogue lock; matching the
  # data volume's filesystem is the only way the arms differ by the device alone.
  diskutil apfs create "$DEV" MIRAWALRD >/dev/null
  [ -d "$VOL" ] || { echo "wal-volume: $DEV did not mount at $VOL" >&2; exit 1; }
  echo "arm B log volume: $DEV at $VOL (APFS)"
fi

# conns, ramdisk("" for same-volume) -> "records_per_s held_s write_s lock_wait_s"
run() {
  rm -rf "$ROOT/p"
  mkdir -p "$ROOT/p"
  if [ -n "$2" ]; then
    rm -rf "${VOL:?}"/* 2>/dev/null || true
    # The log's directory is fixed at <data-dir>/.wal, so the RAM disk is put
    # there rather than configured: `create_dir_all` is happy with a symlink
    # that already resolves to a directory, and nothing else in the engine
    # cares which device answers.
    ln -s "$VOL" "$ROOT/p/.wal"
  fi
  env RUST_LOG=mira=info,mira_core=info,mira::probe=debug \
    "$BIN/mira" --data-dir "$ROOT/p" --http "$HTTP" --grpc "$GRPC" \
                --retention 999d >"$ROOT/run.log" 2>&1 &
  P=$!
  sleep 2
  if ! "$BIN/examples/loadgen" --addr "$HTTP" --for "$FOR" --conns "$1" \
       --batch 8192 >"$ROOT/run.load" 2>&1; then
    kill $P 2>/dev/null || true
    echo "wal-volume: arm failed (ramdisk='${2:-no}'); server said:" >&2
    tail -3 "$ROOT/run.log" >&2
    exit 1
  fi
  sleep 1
  kill $P 2>/dev/null || true
  wait $P 2>/dev/null || true

  RATE=$(awk '/records\/s/{print $2}' "$ROOT/run.load")
  # The assertion the four withdrawn figures above did not have. A full disk
  # sheds rather than fails, so the arm returns a number and the number is
  # ENOSPC. Nothing here is readable without this check passing first.
  SHED=$(awk '/records\/s/{for(i=1;i<=NF;i++) if($(i+1)=="shed") print $i}' "$ROOT/run.load")
  [ "${SHED:-0}" = "0" ] ||
    { echo "wal-volume: arm (ramdisk='${2:-no}') shed $SHED exports -- disk full?" >&2
      df -h "$VOL" "$ROOT" >&2; exit 1; }
  D=$(grep -A8 'submit.total' "$ROOT/run.log" | tail -9)
  HELD=$(echo "$D" | awk '/wal.held/{gsub("ms","",$6); print $6+0}')
  WRITE=$(echo "$D" | awk '/wal.write/{gsub("ms","",$6); print $6+0}')
  WAIT=$(echo "$D" | awk '/wal.lock_wait/{gsub("ms","",$6); print $6+0}')
  echo "${RATE:-0} ${HELD:-0} ${WRITE:-0} ${WAIT:-0}"
}

for C in $SHAPES; do
  echo "=================== $C connections, $PASSES passes"
  printf '%-5s %11s %11s %7s %19s %19s\n' \
    pass 'A rec/s' 'B rec/s' 'B/A' 'A write/held/wait' 'B write/held/wait'
  RATIOS=""
  N=1
  while [ "$N" -le "$PASSES" ]; do
    # shellcheck disable=SC2046 # the fields are separate words on purpose.
    set -- $(run "$C" ram);  BR=$1; BH=$2; BW=$3; BQ=$4
    # shellcheck disable=SC2046 # the fields are separate words on purpose.
    set -- $(run "$C" "");   AR=$1; AH=$2; AW=$3; AQ=$4
    R=$(awk -v a="$AR" -v b="$BR" 'BEGIN{printf "%.3f", (a>0)? b/a : 0}')
    printf '%-5s %11s %11s %7s %19s %19s\n' "$N" "$AR" "$BR" "$R" \
      "$AW/$AH/$AQ" "$BW/$BH/$BQ"
    RATIOS="$RATIOS $R"
    N=$((N + 1))
  done
  echo "$RATIOS" | tr ' ' '\n' | grep . | sort -n | awk '
    {v[NR]=$1}
    END{
      m = (NR%2) ? v[(NR+1)/2] : v[NR/2+1]
      up=0; for(i=1;i<=NR;i++) if(v[i]>1) up++
      printf "  median B/A = %.3f; %d of %d passes had the RAM disk faster\n", m, up, NR
      if (up==NR || up==0)
        printf "  every pass agrees in sign, so the direction is the reading\n"
      else
        printf "  passes disagree in sign -- this is noise, do not quote the median\n"
    }'
done
