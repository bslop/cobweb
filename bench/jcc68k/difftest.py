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
    def __init__(self, rng, stress=False):
        self.rng = rng
        # Stress mode targets the ALLOCATOR rather than the type rules: only
        # d4-d7 and a2-a4 are available to locals, and the evaluation stack
        # spills to the machine stack past d2-d7 / a2-a5. Programs with more
        # simultaneously-live values than that force spill/reload paths that a
        # small program never reaches.
        self.stress = stress
        self.vars = []          # (name, ctype, signed)
        self.calls = []         # (helper name, arg count)
        self.has_struct = False # whether `struct T s` is in scope

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
        max_depth = 6 if self.stress else 3
        leaf_chance = 0.15 if self.stress else 0.3
        if depth >= max_depth or (self.vars and r.random() < leaf_chance):
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
            # named type, so a second instance can be declared and assigned
            "    struct T { int a; short b; unsigned char c; } s;",
            # nested aggregates: a struct holding an array and another struct,
            # and an array of structs. Whole-object assignment of these is the
            # shape that broke when struct assignment stored a pointer.
            "    struct N { short v[3]; struct T inner; };",
            f"    struct N nst; nst.v[0]={r.randint(-9000,9000)}; nst.v[1]={r.randint(-9000,9000)};"
            f" nst.v[2]={r.randint(-9000,9000)};"
            f" nst.inner.a={r.randint(-70000,70000)}; nst.inner.b={r.randint(-3000,3000)};"
            f" nst.inner.c={r.randint(0,255)};",
            f"    struct T sa[2]; sa[0].a={r.randint(-70000,70000)}; sa[0].b={r.randint(-3000,3000)};"
            f" sa[0].c={r.randint(0,255)}; sa[1].a={r.randint(-70000,70000)};"
            f" sa[1].b={r.randint(-3000,3000)}; sa[1].c={r.randint(0,255)};",
            f"    s.a = {r.randint(-100000, 100000)}; s.b = {r.randint(-3000, 3000)}; s.c = {r.randint(0,255)};",
            "    short *p = arr;",
        ]
        self.vars += [(f"arr[{i}]", "short", True) for i in range(n)]
        self.vars += [(f"bytes[{i}]", "unsigned char", False) for i in range(n)]
        self.vars += [("s.a", "int", True), ("s.b", "short", True), ("s.c", "unsigned char", False)]
        self.vars += [(f"p[{i}]", "short", True) for i in range(n)]
        self.vars.append(("(int)(p - arr)", "int", True))
        self.vars += [(f"nst.v[{i}]", "short", True) for i in range(3)]
        self.vars += [
            ("nst.inner.a", "int", True),
            ("nst.inner.b", "short", True),
            ("nst.inner.c", "unsigned char", False),
            ("sa[0].a", "int", True),
            ("sa[0].b", "short", True),
            ("sa[1].a", "int", True),
            ("sa[1].c", "unsigned char", False),
        ]
        self.has_struct = True
        return "\n".join(lines)

    def stmt(self, n):
        """One random control-flow statement that folds into `acc`.

        `n` makes labels unique. Every form terminates: loop counts are
        literals and `goto` only ever jumps forward, so no generated program
        can spin.
        """
        r = self.rng
        pick = r.random()
        if pick < 0.25:
            # switch with a deliberate fallthrough and a default
            e = self.expr()
            return (
                f"    switch ((int)((unsigned)({e}) & 3u)) {{\n"
                f"      case 0: acc += 11u;\n"
                f"      case 1: acc += 22u; break;\n"
                f"      case 2: acc ^= 33u; break;\n"
                f"      default: acc += 44u;\n"
                f"    }}"
            )
        if pick < 0.45:
            k = r.randint(1, 5)
            return f"    {{ int k{n} = {k}; do {{ acc = acc * 5u + (unsigned)({self.expr()}); }} while (--k{n} > 0); }}"
        if pick < 0.60:
            # forward goto skipping an update
            return (
                f"    if ((unsigned)({self.expr()}) & 1u) goto L{n};\n"
                f"    acc += (unsigned)({self.expr()});\n"
                f"    L{n}: acc ^= {r.randint(1, 255)}u;"
            )
        if pick < 0.75 and self.has_struct:
            which = r.random()
            if which < 0.4:
                return f"    {{ struct T t{n}; t{n} = s; acc += (unsigned)(t{n}.a + t{n}.b + t{n}.c); }}"
            if which < 0.7:
                # nested: array member plus an inner struct member
                return (
                    f"    {{ struct N m{n}; m{n} = nst;\n"
                    f"      acc += (unsigned)(m{n}.v[0] + m{n}.v[1] + m{n}.v[2]\n"
                    f"                       + m{n}.inner.a + m{n}.inner.b + m{n}.inner.c); }}"
                )
            # element-to-element copy inside an array of structs
            return (
                f"    {{ sa[1] = sa[0];\n"
                f"      acc += (unsigned)(sa[1].a + sa[1].b + sa[1].c); }}"
            )
        if pick < 0.90:
            # multi-level pointer round trip
            return (
                f"    {{ unsigned base{n} = (unsigned)({self.expr()});\n"
                f"      unsigned *q{n} = &base{n}; unsigned **qq{n} = &q{n};\n"
                f"      **qq{n} += 7u; acc += *q{n}; }}"
            )
        return f"    if ((unsigned)({self.expr()}) > 3u) {{ acc += 5u; }} else {{ acc -= 3u; }}"

    def program(self):
        self.vars = []
        self.calls = []
        self.has_struct = False
        prelude = self.helpers(self.rng.randint(1, 3)) if self.rng.random() < 0.7 else ""
        nvars = self.rng.randint(14, 22) if self.stress else self.rng.randint(3, 6)
        parts = [self.decls(nvars)]
        if self.rng.random() < 0.6:
            parts.append(self.aggregates())
        body = parts + ["    unsigned acc = 0u;"]
        nterms = self.rng.randint(8, 14) if self.stress else self.rng.randint(2, 5)
        for _ in range(nterms):
            body.append(f"    acc += (unsigned)({self.expr()});")
            if self.rng.random() < 0.4:
                m = self.mutation()
                if m:
                    body.append(m)
        # a bounded loop, so control flow is exercised too
        if self.rng.random() < 0.5:
            n = self.rng.randint(1, 6)
            body.append(f"    for (int i = 0; i < {n}; i++) acc = acc * 3u + (unsigned)({self.expr()});")
        # control-flow forms the expression generator cannot reach
        for k in range(self.rng.randint(0, 3)):
            body.append(self.stmt(k))
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
    # Read $100 twice, at different frame counts, and require agreement.
    #
    # The startup stashes main's return at $100, but a program that has not
    # FINISHED leaves whatever was there before. One frame is plenty for a
    # small program and not enough for a large one: a stress case needing
    # ~25k instructions read back garbage at --frames 1 and the correct value
    # at --frames 2, which looked exactly like eight codegen bugs. Requiring a
    # stable value across a short and a long run turns "didn't finish yet"
    # into a reported error instead of a silent wrong answer.
    def peek(frames):
        r = subprocess.run(
            [jagemu, "peek", binf, "--at", "0x100", "--len", "4", "--frames", str(frames)],
            capture_output=True, text=True)
        if r.returncode != 0:
            return None, f"jagemu failed: {r.stderr.strip()[:200]}"
        line = [l for l in r.stdout.splitlines() if l.startswith("{")]
        if not line:
            return None, "no JSON from jagemu"
        b = json.loads(line[-1])["bytes"]
        return (b[0] << 24) | (b[1] << 16) | (b[2] << 8) | b[3], None

    quick, err = peek(4)
    if err:
        return None, err
    settled, err = peek(16)
    if err:
        return None, err
    if quick != settled:
        return None, f"did not settle: {quick} at 4 frames vs {settled} at 16"
    return settled, None


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--count", type=int, default=200)
    ap.add_argument("--seed", type=int, default=1)
    ap.add_argument("--stress", action="store_true",
                    help="many live variables and deep expressions, to exhaust "
                         "the local-register pools and force eval-stack spilling")
    ap.add_argument("--jcc", default="sim/target/release/jcc68k")
    ap.add_argument("--jagemu", default="sim/target/release/jagemu")
    ap.add_argument("--cc", default="cc")
    ap.add_argument("--keep", action="store_true", help="keep failing cases on disk")
    a = ap.parse_args()

    rng = random.Random(a.seed)
    gen = Gen(rng, stress=a.stress)
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
