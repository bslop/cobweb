#!/bin/sh
# run_bench.sh — one Skunkboard capture, safely.
#
# Wraps the two mistakes that cost the 2026-07-27 window:
#
#   1. A short timeout killed the console mid-suite. The board kept running the
#      ROM, held the console, and jcp could not re-handshake afterwards — one
#      interrupted capture costs a physical power-cycle. The timeout here is
#      sized for the FULL suite in both modes, with margin.
#   2. The retry overwrote the log, destroying the partial capture that the
#      first (successful) flash had produced. Logs are timestamped now; nothing
#      is ever written twice.
#
# Usage:  ./run_bench.sh <rom.cof> [tag] [timeout_seconds]
# e.g.    ./run_bench.sh build/calib_skunk.cof full
#         ./run_bench.sh build/calibdsphw_skunk.cof dsphw 180
set -eu

ROM=${1:?usage: run_bench.sh <rom.cof> [tag] [timeout_seconds]}
TAG=${2:-$(basename "$ROM" .cof)}
LIMIT=${3:-600}
PATH=~/jaguar-tools/bin:$PATH
export PATH

[ -f "$ROM" ] || { echo "run_bench: no such ROM: $ROM" >&2; exit 2; }

STAMP=$(date +%Y%m%d_%H%M%S)
LOG="bench_${TAG}_${STAMP}.log"

echo "run_bench: $ROM -> $LOG (limit ${LIMIT}s)"
# `script` keeps jcp's console output line-buffered into the log even though
# stdout is not a terminal. Never reuse a log name.
timeout "$LIMIT" script -qefc "jcp -c $ROM" "$LOG" >/dev/null 2>&1 || true

if grep -aq "can't connect with skunkboard" "$LOG"; then
    echo "run_bench: BOARD DID NOT CONNECT — power-cycle the Jaguar and retry." >&2
    echo "run_bench: (log kept at $LOG)" >&2
    exit 3
fi

ROWS=$(grep -ac "^CAL " "$LOG" || true)
echo "run_bench: captured $ROWS CAL rows"
if grep -aq "CAL DONE" "$LOG"; then
    echo "run_bench: suite COMPLETE"
elif grep -aq "WEDGED" "$LOG"; then
    echo "run_bench: probe WEDGED the GPU — power-cycle before the next flash." >&2
else
    echo "run_bench: capture ended early (no DONE, no WEDGED) — likely a USB drop;" >&2
    echo "run_bench: the board may still be running. Power-cycle before retrying." >&2
fi

echo "run_bench: log = $LOG"
python3 parse_results.py --console "$LOG" || true
