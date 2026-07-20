#!/usr/bin/env python3
"""mkfixture.py — generate a jopt certificate fixture by snapshotting live state.

A jopt certificate needs the kernel to run to a deterministic, observable end.
Without input state, a kernel like `gpu_geotex` loops on zeroed memory, never
halts, and the equivalence check compares a meaningless budget-cutoff snapshot —
which is the vacuous-capture failure mode jopt now rejects outright.

Rather than hand-reconstruct a param block, geometry and camera (which bakes in
my understanding of someone else's data formats and rots the moment they change
one), this SNAPSHOTS the real thing: boot the ROM in jagemu, let it reach a
frame where the kernel is live, and dump the state it was actually given.

The generator is the durable artifact. The blobs it emits are reproducible
build output and are deliberately NOT committed — an earlier fixture lived only
in a scratch directory and was lost to a reboot, which is what prompted this.

PROVEN END TO END (2026-07-19): snapshot -> fixture -> non-vacuous certificate
-> 34 accepted delay-slot fills on the production gpu_geotex kernel (3526 ->
3458 bytes), with the optimized kernel's rendered output BYTE-IDENTICAL to the
baseline's (verified independently via the jtest `fxrun` example, not just by
jopt's own certificate).

Two traps this generator now handles, found the hard way:
  - The snapshot's DRAM contains the frame the kernel already rendered, and a
    deterministic kernel re-renders the same bytes over it — after == before,
    so the certificate sees "never written" and rejects as vacuous. The blob's
    overlap with the capture region is therefore ZEROED.
  - The capture region must be where the kernel actually writes. It is
    params[3] (confirmed in the 68k kick: `G_PARAMS + 12 = fb`); a full-DRAM
    before/after diff in `fxrun` is the reliable way to find it when in doubt.

Usage:
  ./mkfixture.py <rom.cof> --out fixtures/geotex [--frames N] [--jagemu PATH]
  ./mkfixture.py <rom.cof> --out fixtures/geotex --verify path/to/kernel.s

Emitting:
  <out>/geotex.fx        fixture file for `jopt --fixture`
  <out>/gpu_state.bin    GPU SRAM state area (params + iterator state)
  <out>/dram_<a>.bin     DRAM windows the kernel reads

IMPORTANT: the snapshot deliberately EXCLUDES the kernel code region
($F03000..$F03DFF). Presetting that would overwrite the very code jopt is
certifying, so every candidate would run the baseline's instructions and the
certificate would pass everything.
"""

import argparse
import os
import subprocess
import sys

# GPU SRAM layout (gpu_geotex.gas): code at $F03000, state/params from $F03E00.
GPU_STATE = 0xF03E00
GPU_STATE_LEN = 0x200          # $F03E00..$F03FFF: buffers, camera, PARAMS
KERNEL_CODE = (0xF03000, 0xE00)  # never preset — see module docstring

# The kernel's observable output, and therefore the capture region. NOT a fixed
# address: it is params[3] in the snapshot — confirmed against the 68k-side
# kick (gpu.c: `G_PARAMS + 12 = (uint32_t)fb`) after two wrong guesses in a
# row (an assumed 0x100000, then params[5], which is the texture atlas; a
# full-DRAM diff in the `fxrun` example showed the render spans landing
# relative to params[3]).
FB_ADDR_PLACEHOLDER = 0  # rewritten from the snapshot
FB_PARAM_INDEX = 3
FB_LEN = 320 * 240
PARAMS_OFF = 0x100  # $F03F00 - $F03E00
DRAM_LO, DRAM_HI = 0x1000, 0x200000


def run(cmd):
    p = subprocess.run(cmd, capture_output=True, text=True)
    if p.returncode != 0:
        sys.exit("failed: %s\n%s" % (" ".join(cmd), p.stderr.strip()[:2000]))
    return p


def dump(jagemu, rom, addr, length, frames, out):
    run([jagemu, "dump", rom, "--at", hex(addr), "--len", str(length),
         "--frames", str(frames), "--fidelity", "silicon", "-o", out])
    n = os.path.getsize(out)
    if n != length:
        sys.exit("dump of 0x%X gave %d bytes, expected %d" % (addr, n, length))
    return n


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("rom")
    ap.add_argument("--out", required=True)
    ap.add_argument("--frames", type=int, default=620,
                    help="frame to snapshot at (default 620: past AUTOSTART into a live scene)")
    ap.add_argument("--jagemu", default="jagemu")
    ap.add_argument("--dram", action="append", default=[],
                    help="extra DRAM window as ADDR:LEN (repeatable), e.g. 0x140000:0x40000")
    ap.add_argument("--budget", type=int, default=20_000_000)
    ap.add_argument("--verify", metavar="KERNEL_S",
                    help="after generating, run jopt against this kernel and fail "
                         "if the capture is vacuous")
    ap.add_argument("--jopt", default="jopt")
    ap.add_argument("--kdefine", action="append", default=[],
                    help="kernel build define forwarded to jopt as -d (repeatable); "
                         "gpu_geotex needs its full Makefile set or it will not assemble")
    args = ap.parse_args()

    os.makedirs(args.out, exist_ok=True)
    lines = [
        "# generated by calib/mkfixture.py — DO NOT EDIT BY HAND, REGENERATE.",
        "# snapshot of %s at frame %d" % (os.path.basename(args.rom), args.frames),
        "#",
        "# The kernel code region $%06X..$%06X is deliberately NOT preset: it is"
        % (KERNEL_CODE[0], KERNEL_CODE[0] + KERNEL_CODE[1] - 1),
        "# what jopt is certifying, and presetting it would make every candidate",
        "# run the baseline's instructions and certify anything.",
        "",
        "budget %d" % args.budget,
        "capture 0x%X %d" % (FB_ADDR_PLACEHOLDER, FB_LEN),
        "",
    ]

    st = os.path.join(args.out, "gpu_state.bin")
    dump(args.jagemu, args.rom, GPU_STATE, GPU_STATE_LEN, args.frames, st)
    lines.append("blob 0x%X %s" % (GPU_STATE, os.path.basename(st)))

    # Derive the capture region and the DRAM span from the snapshot itself
    # rather than hardcoding addresses that go stale the moment the game's
    # memory map moves.
    import struct
    blob = open(st, "rb").read()
    params = [struct.unpack(">I", blob[PARAMS_OFF + i * 4:PARAMS_OFF + i * 4 + 4])[0]
              for i in range(12)]
    ptrs = [v for v in params if DRAM_LO <= v < DRAM_HI]
    fb = params[FB_PARAM_INDEX]
    if not (DRAM_LO <= fb < DRAM_HI):
        sys.exit("params[%d] = 0x%08X is not a plausible framebuffer pointer"
                 % (FB_PARAM_INDEX, fb))
    lines[lines.index("capture 0x%X %d" % (FB_ADDR_PLACEHOLDER, FB_LEN))] = (
        "capture 0x%X %d" % (fb, FB_LEN))
    if ptrs and not args.dram:
        lo = (min(ptrs) & ~0xFFF)
        hi = ((max(ptrs) + 0x4000) + 0xFFF) & ~0xFFF
        args.dram = ["0x%X:0x%X" % (lo, hi - lo)]
        print("derived DRAM span 0x%06X..0x%06X from params %s"
              % (lo, hi, [hex(v) for v in ptrs]))

    for spec in args.dram:
        a, _, l = spec.partition(":")
        addr, length = int(a, 0), int(l, 0)
        f = os.path.join(args.out, "dram_%06X.bin" % addr)
        dump(args.jagemu, args.rom, addr, length, args.frames, f)
        # ZERO the capture region wherever a blob overlaps it. The snapshot
        # necessarily contains the frame the kernel already rendered, and the
        # certificate detects vacuity by comparing the capture region before vs
        # after the run — a deterministic kernel re-rendering the same frame
        # over its own prior output produces after == before, so the fixture
        # masks the very writes it exists to observe. (Found the hard way:
        # fxrun showed 20k non-zero framebuffer bytes and MAGIC_DONE in the
        # mailbox while jopt reported "fixture never wrote the capture region"
        # — both were telling the truth.)
        lo = max(addr, fb)
        hi = min(addr + length, fb + FB_LEN)
        if lo < hi:
            with open(f, "r+b") as fh:
                fh.seek(lo - addr)
                fh.write(b"\0" * (hi - lo))
            print("zeroed capture overlap 0x%06X..0x%06X in %s"
                  % (lo, hi, os.path.basename(f)))
        lines.append("blob 0x%X %s" % (addr, os.path.basename(f)))

    fx = os.path.join(args.out, "geotex.fx")
    with open(fx, "w") as f:
        f.write("\n".join(lines) + "\n")
    print("wrote %s" % fx)

    # A fixture that captures nothing is indistinguishable from a passing one —
    # exactly what let 56 broken transforms through before jopt failed closed.
    # So prove the snapshot is non-vacuous rather than assuming it.
    blank = open(st, "rb").read()
    if not any(blank):
        sys.exit("gpu_state.bin is all zeros — the kernel was not live at frame %d; "
                 "try a different --frames" % args.frames)
    print("gpu_state.bin: %d non-zero bytes of %d" % (sum(1 for b in blank if b), len(blank)))

    if args.verify:
        cmd = [args.jopt, args.verify, "--fixture", fx, "--allow-input-hazards"]
        for d in args.kdefine:
            cmd += ["-d", d]
        p = subprocess.run(cmd, capture_output=True, text=True)
        out = (p.stdout + p.stderr).strip()
        print(out[-3000:])
        if "vacuous" in out.lower() or p.returncode != 0:
            sys.exit("VERIFY FAILED: fixture does not produce a usable certificate")
        print("verify: fixture produces a non-vacuous certificate")


if __name__ == "__main__":
    main()
