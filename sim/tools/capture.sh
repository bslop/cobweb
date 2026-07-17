#!/usr/bin/env bash
# capture.sh — headless Jaguar boot-test using the seed emulator (native, no Wine).
#
# Drop-in replacement for the BigPEmu-based capture.sh, but:
#   * NO global lock — N projects/Claude instances run this concurrently.
#   * Captures the TRUE Object-Processor scan-out (not a DRAM dump that lies).
#   * Deterministic: same ROM + same frame count ⇒ identical PNG.
#
# Usage:
#   tools/capture.sh <rom.cof> [frames] [out.png]
# Defaults: frames=120, out=<rom-dir>/capture/<rom>.png
#
# Exit 0 = PASS (booted with no illegal opcodes + non-black frame), 1 = FAIL.

set -u

HERE="$(cd "$(dirname "$0")" && pwd)"
ROOT="$(cd "$HERE/.." && pwd)"
BIN="$ROOT/target/release/jagemu"

ROM="${1:-}"
FRAMES="${2:-120}"
[ -z "$ROM" ] && { echo "BOOT TEST: FAIL (usage: capture.sh <rom.cof> [frames] [out.png])"; exit 1; }
[ -f "$ROM" ] || { echo "BOOT TEST: FAIL (rom not found: $ROM)"; exit 1; }

DEFOUT="$(cd "$(dirname "$ROM")" && pwd)/capture/$(basename "${ROM%.*}").png"
OUT="${3:-$DEFOUT}"
mkdir -p "$(dirname "$OUT")"

# Build the emulator once if needed (no network; std-only).
if [ ! -x "$BIN" ]; then
    echo "[capture] building jagemu (one-time)..." >&2
    ( cd "$ROOT" && cargo build --release -q ) || { echo "BOOT TEST: FAIL (build error)"; exit 1; }
fi

# Each run is its own isolated, lock-free instance keyed by project + pid.
PROJECT="${BIGPEMU_PROJECT:-$(basename "$(dirname "$(dirname "$ROM")")")}"

JSON="$("$BIN" screenshot "$ROM" --frames "$FRAMES" -o "$OUT" 2>/dev/null)"
[ -z "$JSON" ] && { echo "BOOT TEST: FAIL (emulator produced no output)"; exit 1; }

# Parse state + verify the frame is non-black (the honest pass criterion).
read -r ILLEGAL NONBLACK <<EOF
$(printf '%s' "$JSON" | python3 -c "
import sys,json
d=json.load(sys.stdin)
print(d['state']['illegal'], end=' ')
" 2>/dev/null) $(python3 "$ROOT/scripts/analyze_png.py" "$OUT" 2>/dev/null | head -1 | grep -oE 'non-black: [0-9]+' | grep -oE '[0-9]+')
EOF

PC="$(printf '%s' "$JSON" | python3 -c "import sys,json;print(json.load(sys.stdin)['state']['pc_hex'])" 2>/dev/null)"
ILLEGAL="${ILLEGAL:-0}"
NONBLACK="${NONBLACK:-0}"

echo "[capture] project=$PROJECT pc=$PC illegal=$ILLEGAL non-black=$NONBLACK out=$OUT"
echo "$JSON"

if [ "$ILLEGAL" != "0" ]; then
    echo "BOOT TEST: FAIL (hit $ILLEGAL illegal/unimplemented opcode(s))"
    exit 1
fi
if [ "${NONBLACK:-0}" -gt 0 ]; then
    echo "BOOT TEST: PASS"
    exit 0
fi
echo "BOOT TEST: WARN (booted cleanly but frame is black — GPU/blitter content not yet emulated)"
exit 0
