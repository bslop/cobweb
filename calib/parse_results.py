#!/usr/bin/env python3
"""parse_results.py — decode Cobweb jsim calibration results.

Inputs (either):
  --peek FILE     JSON from `jagemu peek <rom> --at 0x100000 --len 1024 ...`
                  (also accepts '-' for stdin)
  --console FILE  Skunkboard console log captured from `jcp -c` ("CAL ..." lines)

Outputs a per-probe table (raw ticks -> cycles -> cycles/instruction) and the
derived calibration constants next to jsim's current model values, so a bench
session directly answers "which CAL knobs move, and to what".

The clock is self-calibrating: probe `vcmod` measures the VC wrap modulus on
the rig itself (folklore values have been wrong before — trust the hardware).
"""

import argparse
import json
import re
import struct
import sys

RISC_HZ = 26_590_906.0
FIELD_HZ = 59.94  # NTSC; pass --pal for 50.0

# name -> (instructions of interest per repetition, description)
PROBES = [
    ("vcmod",    0, "VC wrap modulus discovery (special)"),
    ("null",     0, "harness overhead per repetition"),
    ("nop",    512, "local issue rate baseline"),
    ("move",   512, "reg-to-reg MOVE stream (U-235 replication)"),
    ("moveq",  512, "fast-write stream"),
    ("adddep", 512, "dependent ALU chain (bubble per op)"),
    ("addind", 512, "interleaved ALU chains (no bubbles)"),
    ("ldsram", 512, "internal load + consume pairs"),
    ("ldidx",  512, "indexed internal load + consume pairs"),
    ("lddram", 512, "sequential DRAM load stream (page hits)"),
    ("lddramc", 384, "CONSUMED sequential DRAM loads (3 instr/unit)"),
    ("ldstride", 256, "2KB-strided DRAM loads (page misses)"),
    ("stdram", 512, "sequential DRAM store stream"),
    ("blitsm", 0, "Blitter SRCEN|DSTA2 copy, 8 px + launch + bwait"),
    ("blitbg", 0, "Blitter SRCEN|DSTA2 copy, 256 px + launch + bwait"),
    ("blit1", 0, "Blitter 1-px span + launch + bwait (launch-dominated)"),
    ("blit2", 0, "Blitter 2-px span + launch + bwait"),
    ("blit4", 0, "Blitter 4-px span + launch + bwait"),
    ("blittex1", 0, "TEXTURED XADDINC span, 256 px, du=1.0 (fresh texel/pixel)"),
    ("blittexq", 0, "TEXTURED XADDINC span, 256 px, du=0.25 (4 px per texel)"),
    ("blitrmw", 0, "DSTEN RMW OR-fill, 256 px + launch + bwait (dest-READ price)"),
    ("ldunderb", 256, "DRAM loads WHILE a 2048-px blit holds the bus (contention)"),
    ("dens2",  256, "DRAM load per 4 instr (dense) — mode-A regime sweep"),
    ("dens6",  512, "DRAM load per 8 instr — mode-A regime sweep"),
    ("dens14", 1024, "DRAM load per 16 instr — mode-A regime sweep"),
    ("dens30", 1024, "DRAM load per 32 instr (game-like sparse) — mode-A regime sweep"),
    ("ldcunder", 384, "CONSUMED DRAM loads WHILE a 128-px blit runs (geotex staging shape)"),
    ("fib", 824, "2nd B_CMD store fired INTO a running blit + 800 nops (held vs queued)"),
    ("divext", 576, "DIV + consumed staging loads interleaved (geotex per-face shape)"),
    ("divoff", 576, "divext body in DIV_OFFSET 16.16 mode (geotex perspective divide)"),
    ("mmultw", 256, "width-3 MMULT throughput (per-MMULT cost at MTXC=3)"),
    ("mmulta", 256, "MMULT + per-call MTXA write (mmulta-mmultw = control-write cost)"),
    ("face", 130, "synthetic per-face compute: 2 div + 16px DDA + edge branches"),
    ("facenb", 82, "per-face compute, NO edge branches (bisection)"),
    ("facebr", 226, "per-face compute, 3 branches/px (bisection)"),
    ("ovlap", 740, "launch + compute + bwait (overlap possible)"),
    ("serial", 740, "launch + bwait + compute (serialized)"),
    ("bcmdidle", 256, "B_CMD register-read poll stream, Blitter idle (bwait baseline)"),
    ("bcmdbusy", 256, "B_CMD polls WHILE a 2048-px blit runs (the bwait spin, priced)"),
    ("lddramop", 512, "DRAM load stream WHILE the OP scans a full screen (Tom<->OP contention)"),
    ("m68kbus", 0, "68000 throughput, BLOCKS done in 30 fields (higher=faster): A=OP idle, B=OP scanning"),
    ("m68kreg", 0, "68000 register-only dbra loop, BLOCKS in 30 fields (fetch-only bus traffic)"),
    ("m68kcpy", 0, "68000 bytewise copy (OpenLara's exact hot loop), BLOCKS of 64B in 30 fields"),
    ("divhot", 192, "DIV + immediate consume units (3 instr/unit)"),
    ("divsh",  640, "DIV + 17-instr shadow units (20 instr/unit)"),
    ("jr",    1537, "tight taken-JR loop (movei + 512x3)"),
    ("mainmov", 512, "GPU-in-main MOVE body (+call overhead)"),
    ("mainnop", 512, "GPU-in-main NOP body (+call overhead)"),
]
MAGIC = 0xC0DED04E

# jsim model expectations (crates/jag-core/src/risc/timing.rs), cycles/instr,
# used only for the comparison column. None = no direct single-number model.
MODEL = {  # post-calibration jsim claims (bench 2026-07-17)
    "nop": 1.0, "move": 1.0, "moveq": 1.0,
    "adddep": 2.0, "addind": 1.0,
    "ldsram": 1.5, "ldidx": 3.0,
    "lddram": 2.0, "lddramc": None, "ldstride": 2.5, "stdram": 2.0,  # quiet-bus (mode B)
    "divhot": 6.67, "divsh": 1.0,
    "jr": 2.33,
    "mainmov": 13.5, "mainnop": 13.5,  # mode-A (68k polling) calibration
}


def parse_peek(path):
    data = sys.stdin.read() if path == "-" else open(path).read()
    d = json.loads(data)
    b = bytes(d["bytes"])
    w = lambda o: struct.unpack(">I", b[o : o + 4])[0]
    if b[0:4] != b"CALB":
        sys.exit("no CALB header at start of peek block (peek --at 0x100000)")
    results = {}
    for i, (name, _, _) in enumerate(PROBES):
        for mode, mn in ((0, "A"), (1, "B")):
            base = 16 + i * 32 + mode * 16
            if base + 16 > len(b):
                continue
            if w(base + 12) == MAGIC:
                results[(name, mn)] = (w(base), w(base + 4), w(base + 8))
    return results, w(12) == MAGIC


LINE_RE = re.compile(
    r"CAL\s+(\w+)\s+([AB])\s+s=([0-9A-Fa-f]{8})\s+e=([0-9A-Fa-f]{8})"
    r"\s+w=([0-9A-Fa-f]{8})\s+r=([0-9A-Fa-f]{8})"
)


def parse_console(path):
    results = {}
    done = False
    for line in open(path):
        m = LINE_RE.search(line)
        if m:
            name, mode, s, e, wr, _ = m.groups()
            results[(name, mode)] = (int(s, 16), int(e, 16), int(wr, 16))
        if "CAL DONE" in line:
            done = True
    return results, done


def reps_of(name):
    return {
        "vcmod": 0x80000, "null": 8192, "nop": 1024, "move": 1024,
        "moveq": 1024, "adddep": 1024, "addind": 1024, "ldsram": 512,
        "ldidx": 512, "lddram": 512, "lddramc": 256, "ldstride": 256, "stdram": 512,
        "blitsm": 128, "blitbg": 128, "blit1": 128, "blit2": 128, "blit4": 128, "blittex1": 128, "blittexq": 128, "blitrmw": 128, "ldunderb": 128, "ldcunder": 128, "fib": 128, "divext": 128, "divoff": 128, "mmultw": 256, "mmulta": 256, "face": 128, "ovlap": 64, "serial": 64, "facenb": 128, "facebr": 128, "bcmdidle": 128, "bcmdbusy": 128, "dens2": 256, "dens6": 256, "dens14": 128, "dens30": 128, "m68kbus": 30, "m68kreg": 30, "m68kcpy": 30, "lddramj": 512, "lddramop": 512,
        "divhot": 512, "divsh": 512, "jr": 256, "mainmov": 128,
        "mainnop": 128,
    }[name]


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--peek")
    ap.add_argument("--console")
    ap.add_argument("--pal", action="store_true")
    ap.add_argument("--json", action="store_true", help="emit machine-readable JSON")
    args = ap.parse_args()
    if not (args.peek or args.console):
        ap.error("need --peek or --console")

    results, done = parse_peek(args.peek) if args.peek else parse_console(args.console)
    field_hz = 50.0 if args.pal else FIELD_HZ

    vc = results.get(("vcmod", "A"))
    modulus = (vc[0] + 1) if vc else 525
    cpt = RISC_HZ / (field_hz * modulus)  # RISC cycles per VC tick

    print(f"suite complete: {done}   VC modulus: {modulus}   "
          f"cycles/VC-tick: {cpt:.1f}")
    print(f"{'probe':10} {'mode':4} {'ticks':>8} {'cycles':>10} "
          f"{'cyc/instr':>9} {'model':>6}  note")

    out = {"modulus": modulus, "cycles_per_tick": cpt, "probes": {}}

    def ticks_of(key):
        s, e, wr = results[key]
        return wr * modulus + (e - s) if e >= s or wr else (e - s) % modulus

    null_a = ticks_of(("null", "A")) if ("null", "A") in results else 0

    for name, k, desc in PROBES:
        for mode in ("A", "B"):
            key = (name, mode)
            if key not in results:
                continue
            if name == "vcmod":
                print(f"{name:10} {mode:4} {'—':>8} {'—':>10} {'—':>9} "
                      f"{'—':>6}  modulus={modulus}")
                continue
            t = ticks_of(key)
            reps = reps_of(name)
            cyc = t * cpt
            per = None
            if k:
                base = results.get(("null", mode))
                nt = (ticks_of(("null", mode)) if base else null_a)
                overhead = nt * cpt * reps / reps_of("null")
                per = (cyc - overhead) / (reps * k)
            model = MODEL.get(name)
            per_s = f"{per:9.2f}" if per is not None else f"{'—':>9}"
            mod_s = f"{model:6.2f}" if model is not None else f"{'—':>6}"
            print(f"{name:10} {mode:4} {t:8d} {cyc:10.0f} {per_s} {mod_s}  {desc}")
            out["probes"][f"{name}.{mode}"] = {
                "ticks": t, "cycles": cyc,
                "cycles_per_instr": per,
            }

    # Derived CAL knobs (silicon-mode names from timing.rs)
    def per(name, mode="A"):
        p = out["probes"].get(f"{name}.{mode}")
        return p["cycles_per_instr"] if p else None

    knobs = {}
    if per("adddep") and per("addind"):
        knobs["Lat::ALU bubble (adddep - addind)"] = per("adddep") - per("addind")
    if per("mainmov") and per("move"):
        knobs["external fetch tax (mainmov / move)"] = per("mainmov") / per("move")
    if per("lddram"):
        knobs["DRAM seq load, cyc/load (2 instr/pair)"] = per("lddram") * 2
    if per("ldstride"):
        knobs["DRAM strided load, cyc/load"] = per("ldstride") * 2
    if per("nop", "A") and per("nop", "B"):
        knobs["68k bus noise (nop A/B)"] = per("nop", "A") / per("nop", "B")

    print("\nderived CAL knobs:")
    for k2, v in knobs.items():
        print(f"  {k2}: {v:.2f}")
    out["knobs"] = knobs

    if args.json:
        print(json.dumps(out, indent=2))


if __name__ == "__main__":
    main()
