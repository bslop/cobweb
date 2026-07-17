#!/usr/bin/env python3
"""Debug: walk a running instance's OP object graph, find every BITMAP, and
render the one whose data pointer holds real image content (most distinct
values) directly to PNG — bypassing the OP's buffer selection. Diagnostic for
'game runs but OP shows the wrong buffer' cases (Doom, a reference homebrew title)."""
import subprocess, json, sys, struct, zlib

BIN, INST = sys.argv[1], sys.argv[2]
OUT = sys.argv[3] if len(sys.argv) > 3 else "/tmp/realfb.png"


def peek(addr, n):
    out = subprocess.run([BIN, "ctl", INST, "peek", hex(addr), "--len", str(n)],
                         capture_output=True, text=True)
    return bytes(json.loads(out.stdout)["bytes"])


def l32(b, o): return (b[o] << 24) | (b[o + 1] << 16) | (b[o + 2] << 8) | b[o + 3]


# VMODE (pixel format) + OLP (object list, word-swapped)
v = peek(0xF00020, 0x2A)
vmode = (v[8] << 8) | v[9]
olp = (((v[2] << 8 | v[3]) << 16) | (v[0] << 8 | v[1]))
mode = vmode & 6
fmt = {0: "CRY16", 2: "RGB24", 4: "DIRECT16", 6: "RGB16"}[mode]
print(f"VMODE=0x{vmode:04X} ({fmt})  OLP=0x{olp:06X}")

# DFS the object graph collecting BITMAPs.
seen, stack, bitmaps = set(), [olp], []
while stack:
    a = stack.pop() & ~7
    if a in seen or len(seen) > 4096:
        continue
    seen.add(a)
    p = peek(a, 16)
    hi, lo, hi2, lo2 = l32(p, 0), l32(p, 4), l32(p, 8), l32(p, 12)
    t = lo & 7
    link = (((hi & 0x7FF) << 8) | ((lo >> 24) & 0xFF)) << 3
    if t == 0:  # BITMAP
        data = (hi >> 11) << 3
        depth = (lo2 >> 12) & 7
        dwidth = (lo2 >> 18) & 0x3FF
        iwidth = (((hi2 & 0x3F) << 4) | ((lo2 >> 28) & 0xF)) & 0x3FF
        height = (lo >> 14) & 0x3FF
        bitmaps.append((a, data, 1 << depth, iwidth, dwidth, height))
        if link:
            stack.append(link)
    elif t == 3:
        if link:
            stack.append(link)
        stack.append(a + 8)
    elif t != 4:
        stack.append(a + 8)

print(f"found {len(bitmaps)} BITMAP object(s):")
best = None
for (a, data, bpp, iw, dw, h) in bitmaps:
    pps = max(1, 64 // bpp)
    w = iw * pps
    indram = data < 0x200000
    nd = 0
    if indram and w and h:
        buf = peek(data, min(8192, w * 2))
        words = [(buf[i] << 8) | buf[i + 1] for i in range(0, len(buf) - 1, 2)]
        nd = len(set(words))
    print(f"  obj@0x{a:06X} data=0x{data:06X} {bpp}bpp {w}x{h} dwidth={dw} "
          f"{'DRAM' if indram else 'UNMAPPED'} distinct={nd}")
    if indram and nd > 16 and (best is None or nd > best[6]):
        best = (a, data, bpp, w, dw, h, nd)

if not best:
    print("no image-bearing bitmap found")
    sys.exit(1)

_, data, bpp, w, dw, h, nd = best
w = min(w, 384)
h = min(h, 288)
print(f"rendering real framebuffer: data=0x{data:06X} {bpp}bpp {w}x{h} ({fmt})")

# Read the framebuffer and decode to RGB.
stride = dw * 8 if dw else w * (bpp // 8 if bpp >= 8 else 1)
fbsize = stride * h + 16
raw = peek(data, min(fbsize, 0x40000))


def cry(px):  # rough CRY→RGB (intensity-scaled); good enough to recognize a scene
    cr = (px >> 12) & 0xF
    cyn = (px >> 8) & 0xF
    y = px & 0xFF
    r = (cr * 17 * y) // 255
    b = ((15 - cyn) * 17 * y) // 255
    g = (((cr + (15 - cyn)) // 2) * 17 * y) // 255
    return min(r, 255), min(g, 255), min(b, 255)


def rgb16(px):
    r5, b5, g6 = (px >> 11) & 0x1F, (px >> 6) & 0x1F, px & 0x3F
    return (r5 << 3) | (r5 >> 2), (g6 << 2) | (g6 >> 4), (b5 << 3) | (b5 >> 2)


dec = cry if fmt == "CRY16" else rgb16
rows = bytearray()
for y in range(h):
    rows.append(0)
    base = y * stride
    for x in range(w):
        o = base + x * 2
        px = (raw[o] << 8) | raw[o + 1] if o + 1 < len(raw) else 0
        rows += bytes(dec(px))


def chunk(typ, d):
    return struct.pack(">I", len(d)) + typ + d + struct.pack(">I", zlib.crc32(typ + d) & 0xffffffff)


png = b"\x89PNG\r\n\x1a\n"
png += chunk(b"IHDR", struct.pack(">IIBBBBB", w, h, 8, 2, 0, 0, 0))
png += chunk(b"IDAT", zlib.compress(bytes(rows), 6))
png += chunk(b"IEND", b"")
open(OUT, "wb").write(png)
print(f"wrote {OUT} ({w}x{h})")
