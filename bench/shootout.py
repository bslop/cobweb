#!/usr/bin/env python3
"""Cobweb compiler shootout driver: measure compiler-generated Jaguar binaries
under calibrated jsim (the public performance-comparison harness).

Usage: JAGEMU=... SHOOTOUT_BINS=... python3 bench/shootout.py
Benchmarks are vbcc `+jriscbin` RAM binaries (see docs/COMPARISON.md #6). Completion = last framebuffer row fully written
(mandelbrot renders row-major; the bottom row is all-exterior => nonzero)."""
import json, os, subprocess, sys

# jagemu binary and the directory of compiler-built .bin benchmarks.
J = os.environ.get("JAGEMU", "sim/target/release/jagemu")
S = os.environ.get("SHOOTOUT_BINS", "bench/bins")
OUT = os.environ.get("SHOOTOUT_OUT", "bench/shootout_results.json")
FIELD_HZ = 59.94

def run_json(args):
    out = subprocess.run([J] + args, capture_output=True, text=True).stdout
    return json.loads(out)

def fb_base(bin_path, frames):
    d = run_json(["objects", bin_path, "--frames", str(frames), "--fidelity", "silicon"])
    for o in d.get("objects", []):
        if o["type"] == "BITMAP" and o["height"] >= 190:
            return int(o["data"], 16)
    return None

def last_row(bin_path, frames, base):
    addr = base + 199 * 320
    d = run_json(["peek", bin_path, "--at", hex(addr), "--len", "64",
                  "--frames", str(frames), "--fidelity", "silicon"])
    return bytes(d["bytes"])

def state(bin_path, frames):
    d = run_json(["run", bin_path, "--frames", str(frames), "--fidelity", "silicon"])
    return d["state"]

def measure(name, bin_path, max_frames=30000):
    base = fb_base(bin_path, 240)
    if base is None:
        print(f"{name}: no bitmap object found", file=sys.stderr)
        return None
    # Baseline AFTER the runtime's screen clear (frame 60) — the bottom row
    # then holds the cleared value until the row-major render reaches it last.
    initial = last_row(bin_path, 60, base)
    def done(f):
        r = last_row(bin_path, f, base)
        return r != initial and r == last_row(bin_path, f + 30, base)
    # scan for completion
    lo, hi, step = 60, None, 480
    f = 60 + step
    while f <= max_frames:
        if done(f):
            hi = f
            lo = f - step
            break
        f += step
    if hi is None:
        print(f"{name}: not complete by {max_frames} frames", file=sys.stderr)
        return None
    # bisect to +-15 frames
    while hi - lo > 15:
        mid = (lo + hi) // 2
        if done(mid):
            hi = mid
        else:
            lo = mid
    if hi <= 200:
        print(f"{name}: completion at {hi} frames is implausible (detector fault?)",
              file=sys.stderr)
    st = state(bin_path, hi)
    g = st["gpu"]
    res = {
        "name": name,
        "frames": hi,
        "seconds": round(hi / FIELD_HZ, 2),
        "gpu_instret": g["instret"],
        "gpu_cycles": g["cycles"],
        "cpu_instret": st["instret"],
        "timing": {k: v for k, v in g["timing"].items() if v},
    }
    print(json.dumps(res))
    return res

if __name__ == "__main__":
    bins = [
        ("vbcc GPU local (-gpulocal -O3)", f"{S}/mbrotl.bin"),
        ("vbcc GPU main, all workarounds (-O3)", f"{S}/mbrotm.bin"),
        ("vbcc GPU main, workaround=1 (-O3)", f"{S}/mbrotm1.bin"),
        ("vbcc 68000 (-l68kmain -O3)", f"{S}/mbrot68k.bin"),
    ]
    results = [measure(n, p) for n, p in bins]
    with open(OUT, "w") as fh:
        json.dump([r for r in results if r], fh, indent=2)
