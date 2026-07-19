#!/usr/bin/env bash
# skunkflash.sh — reset the Skunkboard over USB, then flash + capture a ROM.
#
# The board needs a reset between runs (a running ROM holds the console, so the
# next `jcp -c` gets "can't connect"). That reset is just a USB port reset, which
# we can issue ourselves via USBDEVFS_RESET — no physical power-cycle needed.
#
# Also forces UNBUFFERED jcp output: jcp's stdout is a pipe here, so libc would
# full-buffer it and a timeout SIGTERM would discard the whole capture (this bit
# us repeatedly — successful runs looked like silent failures).
#
#   usage: ./skunkflash.sh <rom.cof> [seconds] [outfile]
set -u
ROM=${1:?usage: skunkflash.sh <rom.cof> [seconds] [outfile]}
SECS=${2:-75}
OUT=${3:-/tmp/skunkflash.log}
VID_PID=04b4:7200          # Skunkboard = Cypress EZ-USB FX2

find_node() {
    lsusb -d "$VID_PID" 2>/dev/null |
        sed -E 's|Bus ([0-9]+) Device ([0-9]+).*|/dev/bus/usb/\1/\2|' | head -1
}

usb_reset() {
    local node
    node=$(find_node)
    [ -n "$node" ] || { echo "skunkflash: board $VID_PID not on the USB bus"; return 1; }
    python3 - "$node" <<'PY'
import fcntl, sys
USBDEVFS_RESET = 0x5514          # _IO('U', 20)
with open(sys.argv[1], 'wb') as f:
    fcntl.ioctl(f, USBDEVFS_RESET, 0)
print(f"skunkflash: USB reset {sys.argv[1]}")
PY
}

# `jcp -r` resets the board itself — this is the software equivalent of the
# physical "bounce", and is what actually clears a running ROM off the console.
# (A USB port reset alone is NOT enough: the board is powered from the cart slot,
# so resetting the USB link leaves the board's logic and the running ROM intact.)
echo "skunkflash: resetting board (jcp -r)"
timeout 30 stdbuf -o0 -e0 jcp -r >/dev/null 2>&1
sleep 2
# re-find the node in case the reset re-enumerated it
for _ in $(seq 1 20); do
    [ -n "$(find_node)" ] && break
    sleep 0.5
done
node=$(find_node)
[ -n "$node" ] || { echo "skunkflash: board not on USB after reset"; exit 1; }
echo "skunkflash: board at $node — flashing $ROM"

timeout "$SECS" stdbuf -o0 -e0 jcp -c "$ROM" > "$OUT" 2>&1
echo "skunkflash: jcp exit=$? capture=$(wc -c <"$OUT") bytes -> $OUT"
