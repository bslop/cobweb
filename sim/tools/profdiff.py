#!/usr/bin/env python3
"""profdiff — price a work move between two jagemu profiles.

    jagemu run a.cof --frames 620 --fidelity silicon --pc-histogram \
        --core all --prof-json a.json
    jagemu run b.cof --frames 620 --fidelity silicon --pc-histogram \
        --core all --prof-json b.json
    python3 sim/tools/profdiff.py a.json b.json

A top-K table cannot answer "did moving this work to the DSP help?". The
routine that appeared is rarely near the top, and the routine that vanished
leaves no row behind at all — you see two similar-looking tables and no
signal. The diff shows both sides: what each core gained, what it lost, and
whether the total moved or just relocated.

Read the per-core `total` line first. A work move that changes *where* cycles
are spent without changing the total is the case that has burned this project
before: a resident kernel that spins when idle absorbs new work into its spin,
so its cycle count is invariant and the move looks free. The per-PC rows below
the total are what show that — the poll loop shrinks by exactly what the new
routine grew.
"""
import json
import sys


def load(path):
    with open(path) as f:
        return json.load(f)


def rows(core):
    """{pc: {column: value}} for one core's profile block."""
    cols = core["columns"]
    return {r[0]: dict(zip(cols[1:], r[1:])) for r in core["pcs"]}


def symtab(core):
    return sorted((a, n) for a, n in core.get("symbols", []))


def sym_for(syms, addr):
    """Nearest preceding symbol as `name+0xNN`, or '' if none applies."""
    lo, hi = 0, len(syms)
    while lo < hi:
        mid = (lo + hi) // 2
        if syms[mid][0] <= addr:
            lo = mid + 1
        else:
            hi = mid
    if lo == 0:
        return ""
    a, n = syms[lo - 1]
    if addr == a:
        return n
    # Beyond 8 KB the "nearest preceding" symbol is almost certainly unrelated
    # code, and naming a hot spot after it is worse than leaving it blank.
    return f"{n}+0x{addr - a:X}" if addr - a <= 0x2000 else ""


def diff_core(name, a, b, top, threshold):
    if name not in a and name not in b:
        return
    if name not in a or name not in b:
        print(f"\n=== {name} — only profiled in one run, cannot diff ===")
        return
    ca, cb = a[name], b[name]
    ra, rb = rows(ca), rows(cb)
    syms = symtab(cb) or symtab(ca)

    # For the 68000, `total_cycles` counts STOP-sleeping cycles too, which makes
    # it a proxy for wall clock rather than for work: it is near-identical in any
    # two runs of the same length and diffs to ~zero no matter what changed. The
    # comparable quantity is AWAKE cycles.
    if name == "m68k":
        ta = ca.get("main_cycles", 0) + ca.get("isr_cycles", 0)
        tb = cb.get("main_cycles", 0) + cb.get("isr_cycles", 0)
        label = "awake cycles"
    else:
        ta, tb = ca.get("total_cycles", 0), cb.get("total_cycles", 0)
        label = "total cycles"
    d = tb - ta
    pct = (100.0 * d / ta) if ta else 0.0
    print(f"\n=== {name} ===")
    print(f"  {label:<13} {ta:>14} -> {tb:>14}   {d:+14}  ({pct:+.2f}%)")
    if name == "m68k":
        sa, sb = ca.get("stopped_cycles", 0), cb.get("stopped_cycles", 0)
        print(f"  {'asleep in STOP':<13} {sa:>14} -> {sb:>14}   {sb - sa:+14}")

    moved = sorted(
        ((pc, rb.get(pc, {}).get("cycles", 0) - ra.get(pc, {}).get("cycles", 0))
         for pc in set(ra) | set(rb)),
        key=lambda t: -abs(t[1]),
    )
    shown = [(pc, dc) for pc, dc in moved if abs(dc) >= threshold][:top]
    if not shown:
        print("  (no per-PC change above the threshold)")
        return

    grew = sum(dc for _, dc in moved if dc > 0)
    shrank = sum(dc for _, dc in moved if dc < 0)
    print(f"  cycles gained {grew:>14}   cycles lost {shrank:>14}")
    if ta and abs(d) < 0.01 * ta and grew > 0.02 * ta:
        print("  NOTE: the total barely moved but large per-PC cycles did — this"
              " core absorbed\n        the change into existing slack (a spin"
              " loop), so the move looks free here.")

    print(f"\n  {'pc':<10} {'delta cycles':>14} {'a':>13} {'b':>13}  symbol")
    for pc, dc in shown:
        print(f"  0x{pc:06X}   {dc:>+14} {ra.get(pc, {}).get('cycles', 0):>13}"
              f" {rb.get(pc, {}).get('cycles', 0):>13}  {sym_for(syms, pc)}")


def main():
    args = [x for x in sys.argv[1:] if not x.startswith("--")]
    if len(args) != 2:
        print(__doc__)
        return 2
    top = 20
    threshold = 0
    for x in sys.argv[1:]:
        if x.startswith("--top="):
            top = int(x.split("=", 1)[1])
        elif x.startswith("--threshold="):
            threshold = int(x.split("=", 1)[1])

    a, b = load(args[0]), load(args[1])
    if a.get("frames") != b.get("frames"):
        print(f"WARNING: different frame counts ({a.get('frames')} vs "
              f"{b.get('frames')}) — cycle totals are not comparable.\n")
    print(f"A = {args[0]}\nB = {args[1]}   ({a.get('frames')} frames)")
    for core in ("m68k", "gpu", "dsp"):
        diff_core(core, a, b, top, threshold)
    return 0


if __name__ == "__main__":
    sys.exit(main())
