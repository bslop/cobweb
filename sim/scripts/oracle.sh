#!/usr/bin/env bash
# oracle.sh <rom> [frames] — accuracy diff: the seed emulator vs BigPEmu (ground
# truth) at frame N. Runs BigPEmu headless with the oracle.c CVM script (dumps
# 68k/GPU/DSP state + DRAM chunk hashes), runs jagemu oracle-dump in the same
# format, then jagemu oracle-diff to pinpoint divergence.
#
# NOTE: oracle.c hardcodes DUMP_FRAME=200; pass that same frame here (default 200).
set -u

ROM="${1:-}"
FRAMES="${2:-200}"
[ -z "$ROM" ] && { echo "usage: oracle.sh <rom> [frames=200]"; exit 1; }
[ -f "$ROM" ] || { echo "rom not found: $ROM"; exit 1; }

HERE="$(cd "$(dirname "$0")" && pwd)"
ROOT="$(cd "$HERE/.." && pwd)"
BIN="$ROOT/target/release/jagemu"
GITROOT="$(cd "$ROOT/.." && pwd)"
BPRUN="$GITROOT/.bigpemu/bigpemu-run"
BPKILL="$GITROOT/.bigpemu/bigpemu-kill"
SCRIPTS="/opt/BigPEmu/Scripts"
WINE_PREFIX="$HOME/.wine-bigpemu-dev"
USERDATA="$WINE_PREFIX/drive_c/users/$(id -un)/AppData/Roaming/BigPEmu"
OUT="/tmp/jagoracle"
mkdir -p "$OUT"

[ -x "$BIN" ] || { echo "building jagemu..."; ( cd "$ROOT" && cargo build --release -q ); }

# 1. jagemu side (fast, deterministic).
echo "[oracle] the seed emulator → frame $FRAMES"
"$BIN" oracle-dump "$ROM" --frames "$FRAMES" -o "$OUT/jagemu.bin" >/dev/null || exit 1

# 2. BigPEmu side: install + enable oracle.c, force a recompile, run until
#    oracle.bin lands. The CVM script is versioned in the repo at
#    scripts/bigpemu/oracle.c so the harness is reproducible after a clone.
echo "[oracle] BigPEmu (ground truth) → frame 200 (this takes ~10-40s under Wine)"
[ -f "$HERE/bigpemu/oracle.c" ] && cp "$HERE/bigpemu/oracle.c" "$SCRIPTS/oracle.c"
rm -f "$SCRIPTS/oracle.bigpcvm"
find "$USERDATA" -name oracle.bin -delete 2>/dev/null || true

# Enable "oracle" in ScriptsEnabled (bigpemu-run preserves existing entries).
for CFG in "$USERDATA/BigPEmuConfig.bigpcfg" "$HOME/.bigpemu_userdata/BigPEmuConfig.bigpcfg"; do
    [ -f "$CFG" ] || continue
    python3 - "$CFG" <<'PY'
import json, sys
p = sys.argv[1]
try:
    j = json.load(open(p))
    cfg = j.get("BigPEmuConfig", j)
    se = cfg.get("ScriptsEnabled")
    if isinstance(se, list) and "oracle" not in se:
        se.append("oracle")
        json.dump(j, open(p, "w"), indent=4)
except Exception as e:
    pass
PY
done

"$BPKILL" 2>/dev/null || true
setsid env BIGPEMU_HEADLESS=1 BIGPEMU_PROJECT=oracle "$BPRUN" "$ROM" >"$OUT/emu.log" 2>&1 &
DUMP=""
for i in $(seq 1 60); do
    DUMP="$(find "$USERDATA" -name oracle.bin 2>/dev/null | head -1)"
    [ -n "$DUMP" ] && [ -s "$DUMP" ] && break
    sleep 1
done
"$BPKILL" 2>/dev/null || true; pkill -f BigPEmuDev 2>/dev/null || true

if [ -z "$DUMP" ] || [ ! -s "$DUMP" ]; then
    echo "[oracle] FAIL: BigPEmu never produced oracle.bin (check $OUT/emu.log; did oracle.c compile? ls $SCRIPTS/oracle.bigpcvm)"
    exit 1
fi
cp "$DUMP" "$OUT/bigpemu.bin"

# 3. Diff.
echo "[oracle] diffing..."
"$BIN" oracle-diff "$OUT/bigpemu.bin" "$OUT/jagemu.bin"
