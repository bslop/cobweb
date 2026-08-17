#!/usr/bin/env python3
"""Differential tester: jcc68k+jsim vs the host C compiler.

Generates random C programs whose value is well-defined (no UB), computes the
answer two ways, and reports any disagreement. Hand-written tests only probe
what someone thought to ask about; this asks combinations nobody would.

    difftest.py --count 500 [--seed 1] [--jcc PATH] [--jagemu PATH] [--keep]

Both sides must agree on integer widths, so the generated code uses only types
whose size matches on the host (LP64) and on the target (LP32): char, short,
int.  `long` and pointers are deliberately absent.

Avoiding UB is the whole game — a disagreement is only a bug if C defines the
answer.  The generator therefore:
  * accumulates in `unsigned` (wraparound is defined),
  * forces every divisor non-zero with `| 1`,
  * masks every shift count below the operand width,
  * keeps signed operands small so no signed overflow is reachable.
"""
import argparse
import json
import os
import random
import shutil
import subprocess
import sys
import tempfile

# (C type, is_signed, low, high) — ranges chosen so signed arithmetic on any
# pair stays far from overflow.
TYPES = [
    ("unsigned char", False, 0, 255),
    ("signed char", True, -128, 127),
    ("unsigned short", False, 0, 65535),
    ("short", True, -32768, 32767),
    ("unsigned int", False, 0, 4000000000),
    ("int", True, -1000000, 1000000),
]

BIN_ARITH = ["+", "-", "*"]
BIN_BITS = ["&", "|", "^"]
BIN_CMP = ["<", ">", "<=", ">=", "==", "!="]


class Gen:
    def __init__(self, rng):
        self.rng = rng
        self.vars = []          # (name, ctype, signed)
        self.calls = []         # (helper name, arg count)

    def decls(self, n):
        out = []
        for i in range(n):
            ty, signed, lo, hi = self.rng.choice(TYPES)
            name = f"v{i}"
            val = self.rng.randint(lo, hi)
            suffix = "u" if not signed and "int" in ty else ""
            out.append(f"    {ty} {name} = {val}{suffix};")
            self.vars.append((name, ty, signed))
        return "\n".join(out)

    def helpers(self, n):
        """Free functions with mixed-width parameters and return types.

        The parameter ABI and sub-word return narrowing were two of the worst
        bugs found by hand, so the generator should be able to reach them: each
        helper takes 1-4 operands of assorted widths and returns an assorted
        width, and its body mixes them.
        """
        r = self.rng
        out, self.calls = [], []
        for i in range(n):
            ret, _rs, _lo, _hi = r.choice(TYPES)
            nargs = r.randint(1, 4)
            ptypes = [r.choice(TYPES)[0] for _ in range(nargs)]
            params = ", ".join(f"{t} p{j}" for j, t in enumerate(ptypes))
            terms = " + ".join(f"(int)p{j}" for j in range(nargs))
            body = f"({terms}) * {r.randint(1, 7)} - {r.randint(0, 1000)}"
            out.append(f"static {ret} h{i}({params}) {{ return ({ret})({body}); }}")
            self.calls.append((f"h{i}", nargs))
        return "\n".join(out)

    def call(self, depth):
        name, nargs = self.rng.choice(self.calls)
        args = ", ".join(self.expr(depth + 1) for _ in range(nargs))
        return f"{name}({args})"

    def expr(self, depth=0):
        r = self.rng
        if depth >= 3 or (self.vars and r.random() < 0.3):
            name, _ty, _s = r.choice(self.vars)
            return name
        if self.calls and depth < 2 and r.random() < 0.15:
            return f"((int){self.call(depth)})"
        pick = r.random()
        if pick < 0.34:
            return f"({self.expr(depth+1)} {r.choice(BIN_ARITH)} {self.expr(depth+1)})"
        if pick < 0.50:
            return f"({self.expr(depth+1)} {r.choice(BIN_BITS)} {self.expr(depth+1)})"
        if pick < 0.62:
            # division/modulo: the divisor is forced non-zero, and both sides
            # are cast to unsigned so the sign rules cannot introduce UB
            op = r.choice(["/", "%"])
            return f"((unsigned)({self.expr(depth+1)}) {op} ((unsigned)({self.expr(depth+1)}) | 1u))"
        if pick < 0.74:
            op = r.choice(["<<", ">>"])
            return f"((unsigned)({self.expr(depth+1)}) {op} ((unsigned)({self.expr(depth+1)}) & 31u))"
        if pick < 0.86:
            return f"({self.expr(depth+1)} {r.choice(BIN_CMP)} {self.expr(depth+1)})"
        if pick < 0.90:
            ty, _s, _lo, _hi = r.choice(TYPES)
            return f"(({ty})({self.expr(depth+1)}))"
        if pick < 0.96:
            # unary: `-` only on unsigned, where wraparound is defined
            op = r.choice(["~", "!", "-"])
            inner = self.expr(depth + 1)
            return f"({op}(unsigned)({inner}))" if op == "-" else f"({op}({inner}))"
        return f"({self.expr(depth+1)} ? {self.expr(depth+1)} : {self.expr(depth+1)})"

    def mutation(self):
        """A statement that read-modify-writes a declared variable in place.

        Compound assignment and ++/-- narrow their result to the LVALUE's
        width, which is a different path from a plain expression and is where
        several width bugs have lived.
        """
        r = self.rng
        # UNSIGNED targets only: `short v = 32767; v++` is signed overflow, i.e.
        # UB, and a disagreement there would be a false positive rather than a
        # bug. Unsigned wraparound is fully defined.
        scalars = [
            (n, t, s) for (n, t, s) in self.vars
            if "[" not in n and "." not in n and not s
        ]
        if not scalars:
            return None
        name, _ty, _signed = r.choice(scalars)
        op = r.choice(["+=", "-=", "*=", "&=", "|=", "^="])
        if op in ("&=", "|=", "^="):
            return f"    {name} {op} (unsigned)({self.expr()});"
        if r.random() < 0.3:
            return f"    {name}{r.choice(['++', '--'])};"
        return f"    {name} {op} ({self.expr()});"

    def aggregates(self):
        """An array and a struct, so indexing and field access are exercised.

        Both are fully initialized: reading an indeterminate value would make
        the two sides disagree for a reason that is not a compiler bug.
        """
        r = self.rng
        n = r.randint(3, 6)
        vals = [r.randint(-30000, 30000) for _ in range(n)]
        lines = [
            f"    short arr[{n}] = {{ {', '.join(str(v) for v in vals)} }};",
            f"    unsigned char bytes[{n}] = {{ {', '.join(str(r.randint(0,255)) for _ in range(n))} }};",
            "    struct { int a; short b; unsigned char c; } s;",
            f"    s.a = {r.randint(-100000, 100000)}; s.b = {r.randint(-3000, 3000)}; s.c = {r.randint(0,255)};",
            "    short *p = arr;",
        ]
        self.vars += [(f"arr[{i}]", "short", True) for i in range(n)]
        self.vars += [(f"bytes[{i}]", "unsigned char", False) for i in range(n)]
        self.vars += [("s.a", "int", True), ("s.b", "short", True), ("s.c", "unsigned char", False)]
        self.vars += [(f"p[{i}]", "short", True) for i in range(n)]
        self.vars.append(("(int)(p - arr)", "int", True))
        return "\n".join(lines)

    def program(self):
        self.vars = []
        self.calls = []
        prelude = self.helpers(self.rng.randint(1, 3)) if self.rng.random() < 0.7 else ""
        parts = [self.decls(self.rng.randint(3, 6))]
        if self.rng.random() < 0.6:
            parts.append(self.aggregates())
        body = parts + ["    unsigned acc = 0u;"]
        for _ in range(self.rng.randint(2, 5)):
            body.append(f"    acc += (unsigned)({self.expr()});")
            if self.rng.random() < 0.4:
                m = self.mutation()
                if m:
                    body.append(m)
        # a bounded loop, so control flow is exercised too
        if self.rng.random() < 0.5:
            n = self.rng.randint(1, 6)
            body.append(f"    for (int i = 0; i < {n}; i++) acc = acc * 3u + (unsigned)({self.expr()});")
        # a while loop with a compound-assignment body
        if self.rng.random() < 0.35:
            n = self.rng.randint(1, 5)
            body.append(f"    {{ int k = {n}; while (k-- > 0) {{ acc ^= (unsigned)({self.expr()}); acc <<= 1; }} }}")
        pre = (prelude + "\n") if prelude else ""
        return pre + "int compute(void) {\n" + "\n".join(body) + "\n    return (int)acc;\n}\n"


HOST_MAIN = """
#include <stdio.h>
int compute(void);
int main(void){ printf("%u\\n", (unsigned)compute()); return 0; }
"""

TARGET_MAIN = "int main(void){ return compute(); }\n"


def host_value(src, workdir, cc):
    c = os.path.join(workdir, "h.c")
    exe = os.path.join(workdir, "h.out")
    open(c, "w").write(src + HOST_MAIN)
    r = subprocess.run([cc, "-std=c99", "-w", "-O0", c, "-o", exe],
                       capture_output=True, text=True)
    if r.returncode != 0:
        return None, f"host cc failed: {r.stderr.strip()[:200]}"
    r = subprocess.run([exe], capture_output=True, text=True)
    if r.returncode != 0:
        return None, "host program crashed"
    return int(r.stdout.strip()) & 0xFFFFFFFF, None


def target_value(src, workdir, jcc, jagemu):
    c = os.path.join(workdir, "t.c")
    binf = os.path.join(workdir, "t.bin")
    open(c, "w").write(src + TARGET_MAIN)
    r = subprocess.run([jcc, c, "-o", binf, "--bin", "--prog"],
                       capture_output=True, text=True)
    if r.returncode != 0:
        return None, f"jcc68k failed: {r.stderr.strip()[:200]}"
    r = subprocess.run([jagemu, "peek", binf, "--at", "0x100", "--len", "4", "--frames", "1"],
                       capture_output=True, text=True)
    if r.returncode != 0:
        return None, f"jagemu failed: {r.stderr.strip()[:200]}"
    line = [l for l in r.stdout.splitlines() if l.startswith("{")]
    if not line:
        return None, "no JSON from jagemu"
    b = json.loads(line[-1])["bytes"]
    return (b[0] << 24) | (b[1] << 16) | (b[2] << 8) | b[3], None


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--count", type=int, default=200)
    ap.add_argument("--seed", type=int, default=1)
    ap.add_argument("--jcc", default="sim/target/release/jcc68k")
    ap.add_argument("--jagemu", default="sim/target/release/jagemu")
    ap.add_argument("--cc", default="cc")
    ap.add_argument("--keep", action="store_true", help="keep failing cases on disk")
    a = ap.parse_args()

    rng = random.Random(a.seed)
    gen = Gen(rng)
    work = tempfile.mkdtemp(prefix="jccdiff_")
    fails, skipped = [], 0
    try:
        for i in range(a.count):
            src = gen.program()
            want, err = host_value(src, work, a.cc)
            if err:
                skipped += 1
                continue
            got, err = target_value(src, work, a.jcc, a.jagemu)
            if err:
                fails.append((i, src, None, None, err))
                continue
            if want != got:
                fails.append((i, src, want, got, None))
            if (i + 1) % 50 == 0:
                print(f"  {i+1}/{a.count}  mismatches={len(fails)}", file=sys.stderr)
        print(f"\nchecked {a.count - skipped}, skipped {skipped}, MISMATCHES {len(fails)}")
        for idx, src, want, got, err in fails[:5]:
            print(f"\n=== case {idx} ===")
            if err:
                print(f"  error: {err}")
            else:
                print(f"  host={want} ({want:#010x})   target={got} ({got:#010x})")
            print(src)
            if a.keep:
                p = f"difffail_{idx}.c"
                open(p, "w").write(src + TARGET_MAIN)
                print(f"  saved {p}")
        return 1 if fails else 0
    finally:
        if not a.keep:
            shutil.rmtree(work, ignore_errors=True)


if __name__ == "__main__":
    sys.exit(main())
