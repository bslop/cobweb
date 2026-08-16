#!/usr/bin/env python3
"""Count emitted 68000 instructions per function, for tracking jcc68k codegen quality.

Usage:  measure.py <jcc68k-binary> [--json] [--baseline FILE]
Prints a per-function instruction count and a total; with --baseline, the delta.
"""
import json
import os
import subprocess
import sys

HERE = os.path.dirname(os.path.abspath(__file__))
SOURCES = ["vec.c", "scalar.c"]

# Lines that are directives/labels, not instructions.
def is_insn(line):
    t = line.strip()
    if not t or t.startswith(("*", ";", "|")):
        return False
    if t.startswith("."):          # .text .globl .long .68000 ...
        return False
    if t.endswith(":"):            # label
        return False
    return True


def count(jcc, src):
    path = os.path.join(HERE, src)
    r = subprocess.run([jcc, path, "-o", "/dev/stdout"],
                       capture_output=True, text=True)
    if r.returncode != 0:
        sys.exit(f"{src}: jcc68k failed:\n{r.stderr}")
    per, calls, cur = {}, {}, None
    for line in r.stdout.splitlines():
        t = line.strip()
        if t.endswith(":") and not t.startswith(".") and " " not in t:
            cur = t[:-1]
            per.setdefault(cur, 0)
            calls.setdefault(cur, 0)
        elif cur and is_insn(line):
            per[cur] += 1
            # A call into the software-arithmetic runtime (__mulsi3 and friends)
            # costs orders of magnitude more than the one instruction it looks
            # like, so count these separately — instruction totals alone hide
            # the single largest win available on this chip.
            if t.startswith("jsr __"):
                calls[cur] += 1
    return per, calls


def main():
    argv = sys.argv[1:]
    if not argv:
        sys.exit(__doc__)
    jcc = argv[0]
    as_json = "--json" in argv
    baseline = None
    if "--baseline" in argv:
        bp = argv[argv.index("--baseline") + 1]
        if os.path.exists(bp):
            baseline = json.load(open(bp))

    totals, helper = {}, {}
    for src in SOURCES:
        per, calls = count(jcc, src)
        totals.update(per)
        helper.update(calls)
    grand = sum(totals.values())
    grand_calls = sum(helper.values())

    if as_json:
        print(json.dumps(
            {"functions": totals, "total": grand,
             "helper_calls": helper, "total_helper_calls": grand_calls},
            indent=2, sort_keys=True))
        return

    w = max(len(f) for f in totals)
    width = w + 17 + (10 if baseline else 0)
    print(f"{'function':<{w}}  insns" + ("   vs base" if baseline else "") + "   helper calls")
    print("-" * width)
    for fn in sorted(totals):
        row = f"{fn:<{w}}  {totals[fn]:>5}"
        if baseline:
            b = baseline["functions"].get(fn)
            if b is None:
                row += "      new"
            else:
                d = totals[fn] - b
                pct = (100.0 * d / b) if b else 0.0
                row += f"  {d:+4d} {pct:+5.0f}%"
        hb = (baseline or {}).get("helper_calls", {}).get(fn)
        row += f"   {helper.get(fn, 0):>5}"
        if hb is not None:
            row += f" (was {hb})"
        print(row)
    print("-" * width)
    row = f"{'TOTAL':<{w}}  {grand:>5}"
    if baseline:
        b = baseline["total"]
        d = grand - b
        row += f"  {d:+4d} {100.0*d/b:+5.0f}%"
    row += f"   {grand_calls:>5}"
    if baseline and "total_helper_calls" in baseline:
        row += f" (was {baseline['total_helper_calls']})"
    print(row)


if __name__ == "__main__":
    main()
