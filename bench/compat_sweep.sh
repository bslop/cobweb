#!/bin/bash
# Full-corpus compatibility sweep: boot every cart image, record health + render.
# Usage: bench/compat_sweep.sh /path/to/cart/images   (see docs/COMPATIBILITY.md)
J=${JAGEMU:-sim/target/release/jagemu}
AN=${ANALYZE:-sim/scripts/analyze_png.py}
OUT=${SWEEP_OUT:-/tmp/cobweb_sweep}
mkdir -p "$OUT"
FRAMES=${FRAMES:-400}

one() {
  rom="$1"
  name=$(basename "$rom")
  safe=$(echo "$name" | tr -c 'A-Za-z0-9._-' '_')
  state=$("$J" run "$rom" --frames "$FRAMES" 2>/dev/null)
  "$J" screenshot "$rom" --frames "$FRAMES" -o "$OUT/$safe.png" >/dev/null 2>&1
  shot=$(python3 "$AN" "$OUT/$safe.png" 2>/dev/null | tr '\n' ' ')
  fmt=$("$J" info "$rom" 2>/dev/null | python3 -c "import json,sys; print(json.load(sys.stdin).get('format','?'))" 2>/dev/null)
  python3 - "$name" "$fmt" "$shot" <<PYEOF
import json, sys
name, fmt, shot = sys.argv[1], sys.argv[2], sys.argv[3]
try:
    st = json.loads('''$state''')["state"]
    rec = dict(rom=name, format=fmt, ok=True,
               illegal=st["illegal"], cpu_instret=st["instret"],
               gpu_instret=st["gpu"]["instret"], dsp_instret=st["dsp"]["instret"],
               shot=shot.strip())
except Exception as e:
    rec = dict(rom=name, format=fmt, ok=False, error=str(e)[:120])
print(json.dumps(rec))
PYEOF
}
export -f one
export J AN OUT FRAMES

LIST=${LIST:-}
if [ -n "$LIST" ]; then
  sed "s|^|$ROMDIR/|" "$LIST" | xargs -d'\n' -P 8 -I{} bash -c 'one "{}"' > "$OUT/results_retry.jsonl"
  echo "RETRY DONE: $(wc -l < "$OUT/results_retry.jsonl") roms"
else
  ROMDIR="${1:?usage: compat_sweep.sh /path/to/carts}"
  find "$ROMDIR" -name "*.jag" -o -name "*.j64" -o -name "*.rom" \
    | sort | xargs -d'\n' -P 8 -I{} bash -c 'one "{}"' > "$OUT/results.jsonl"
  echo "SWEEP DONE: $(wc -l < "$OUT/results.jsonl") roms"
fi
