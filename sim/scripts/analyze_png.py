#!/usr/bin/env python3
"""Analyze a PNG produced by jagemu: size, non-black %, distinct colors.
Pure-stdlib (parses our stored-zlib PNGs). Usage: analyze_png.py <file.png>"""
import struct, zlib, sys
from collections import Counter


def load(path):
    data = open(path, "rb").read()
    assert data[:8] == b"\x89PNG\r\n\x1a\n", "not a PNG"
    off, W, H, idat = 8, 0, 0, b""
    while off < len(data):
        ln = struct.unpack(">I", data[off:off + 4])[0]
        typ = data[off + 4:off + 8]
        chunk = data[off + 8:off + 8 + ln]
        off += 12 + ln
        if typ == b"IHDR":
            W, H = struct.unpack(">II", chunk[:8])
        elif typ == b"IDAT":
            idat += chunk
    raw = zlib.decompress(idat)
    stride = W * 4
    px = bytearray()
    p = 0
    for _ in range(H):
        p += 1  # filter byte (always 0 from our encoder)
        px += raw[p:p + stride]
        p += stride
    return W, H, px


def main():
    W, H, px = load(sys.argv[1])
    colors = Counter()
    nonblack = 0
    for i in range(0, len(px), 4):
        c = (px[i], px[i + 1], px[i + 2])
        colors[c] += 1
        if c != (0, 0, 0):
            nonblack += 1
    total = W * H
    print(f"size: {W}x{H}  non-black: {nonblack} ({100*nonblack/total:.1f}%)  distinct colors: {len(colors)}")
    for c, n in colors.most_common(6):
        print(f"  {c[0]:02X}{c[1]:02X}{c[2]:02X} : {n} ({100*n/total:.1f}%)")


if __name__ == "__main__":
    main()
