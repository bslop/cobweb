#!/usr/bin/env python3
"""Verify the jcc68k graphical smoke test by MEASURING the frame, not looking at it.

    jagemu screenshot drawtest.bin --frames 200 -o shot.png
    checkshot.py shot.png

`drawtest.c` is the smallest program that proves jcc68k emits working graphical
code: it fills a framebuffer, builds an Object Processor list by hand, programs
the video registers, and halts. Every pixel is computed by compiled C — no GPU,
no Blitter, no borrowed port code — so if the frame is right, the compiler
produced correct MMIO stores, bitfield packing, loops and arithmetic.

Why measure rather than eyeball. Two failures this checker exists to catch are
invisible by inspection:

  * A 1-pixel border is invisible in any downscaled view, so "the border is
    missing" is a conclusion you can reach about a frame that is perfectly
    correct (jag_viewpoint, 2026-08-17).
  * Jaguar RGB16 is R<<11 | B<<6 | G<<1 — green is the odd one out. A packing
    that swaps the pairs still yields three coloured bands and three rising
    ramps; it just renders the red band blue. Per-band CHANNEL ISOLATION is the
    only check that actually pins the layout, and it is the reason this file
    exists rather than a size assertion.

Exit status is 0 only if every check passes, so it can gate a build.
"""
import sys

try:
    from PIL import Image
except ImportError:
    sys.exit("checkshot: needs Pillow (pip install pillow)")

W, H = 320, 120          # must match drawtest.c
BORDER = 2
SQUARE = (40, 20, 70, 50)   # x0, y0, x1, y1 of the magenta block


def main():
    if len(sys.argv) != 2:
        sys.exit(__doc__)
    im = Image.open(sys.argv[1]).convert("RGB")
    px = im.load()
    fails = []

    def chk(name, ok, detail=""):
        print(f"  {'PASS' if ok else 'FAIL'}  {name}" + ("" if ok else f"   {detail}"))
        if not ok:
            fails.append(name)

    chk("geometry", im.size == (W, H), f"expected {W}x{H}, got {im.size[0]}x{im.size[1]}")
    if im.size != (W, H):
        # every later check indexes by absolute coordinate
        print("\n  1 failure (geometry) — later checks skipped")
        return 1

    def white(p):
        return p[0] > 200 and p[1] > 200 and p[2] > 200

    chk("border top", all(white(px[x, 0]) for x in range(W)))
    chk("border bottom", all(white(px[x, H - 1]) for x in range(W)))
    chk("border left", all(white(px[0, y]) for y in range(H)))
    chk("border right", all(white(px[W - 1, y]) for y in range(H)))

    def band_mean(y0, y1):
        """Mean RGB over a band, skipping the diagonal and the magenta block."""
        r = g = b = n = 0
        for y in range(y0, y1):
            for x in range(200, 300):          # right half: clear of the square
                if x == (y * W) // H:          # clear of the diagonal
                    continue
                c = px[x, y]
                r += c[0]; g += c[1]; b += c[2]; n += 1
        return r / n, g / n, b / n

    bands = (("red", (BORDER + 3, H // 3), 0),
             ("green", (H // 3 + 2, 2 * H // 3), 1),
             ("blue", (2 * H // 3 + 2, H - BORDER - 3), 2))
    for name, (y0, y1), dom in bands:
        m = band_mean(y0, y1)
        others = [m[i] for i in range(3) if i != dom]
        chk(f"band {name} channel-isolated",
            m[dom] > 40 and all(o < 12 for o in others),
            f"mean rgb {tuple(round(v, 1) for v in m)} — a swapped RGB16 layout looks like this")

    def ramps(y, ch):
        lo = sum(px[x, y][ch] for x in range(10, 40)) / 30
        hi = sum(px[x, y][ch] for x in range(280, 310)) / 30
        return hi > lo + 60, (round(lo, 1), round(hi, 1))

    for name, y, ch in (("red", 20, 0), ("green", 60, 1), ("blue", 100, 2)):
        ok, v = ramps(y, ch)
        chk(f"ramp {name} rises left-to-right", ok, f"left/right means {v}")

    # PRECISION: does green actually carry six bits?
    #
    # Channel isolation proves WHICH channel a value lands in; it cannot prove
    # how many bits that channel has. A layout packing 5-bit green into the
    # 6-bit field passes isolation and ramp assertions while silently halving
    # green resolution. The discriminator is that distinct inputs must produce
    # distinct outputs: if the low bit is dropped, consecutive values collide
    # in pairs. drawtest.c drives green 0..63 (red/blue stay 0..31) precisely
    # so this is observable — an assertion cannot catch a failure the input
    # never provokes.
    gy = 60                                  # inside the green band
    seen = {}
    for x in range(BORDER + 4, W - BORDER - 4):
        if x == (gy * W) // H:               # skip the diagonal
            continue
        seen.setdefault(px[x, gy][1], 0)
        seen[px[x, gy][1]] += 1
    distinct = len(seen)
    chk("green carries 6 bits (precision)", distinct >= 40,
        f"{distinct} distinct green levels across the ramp — 5-bit green in a "
        f"6-bit field roughly halves this")

    x0, y0, x1, y1 = SQUARE
    c = px[(x0 + x1) // 2, (y0 + y1) // 2]
    chk("magenta block (R+B, no G)", c[0] > 200 and c[2] > 200 and c[1] < 40, f"got {c}")

    print(f"\n  {len(fails)} failure(s)" if fails else f"\n  {13} checks passed")
    return 1 if fails else 0


if __name__ == "__main__":
    sys.exit(main())
