# What Cobweb solves that the current Jaguar toolchain doesn't

The Atari Jaguar homebrew ecosystem in 2026: rmac/rln (assembler/linker,
MadMac lineage), vasm/vlink, the vbcc `jrisc` C compiler preview (2026),
Atari's recovered 1995 Brainstorm GCC, BigPEmu / Virtual-Jaguar-Rx
(emulators), skunklib + jcp (hardware I/O), and closed engines
(Raptor/JagStudio, U-235). This document lists, concretely, what Cobweb
solves that none of them do — split honestly into **solved today** and
**designed, in build order**.

## Solved today

### 1. Nobody could tell you how many cycles JRISC code takes. Now something can.
BigPEmu runs the GPU on a host thread with approximated sync (its own
community calls GPU timing untrustworthy for measurement); Virtual-Jaguar-Rx
is not cycle-accurate; MAME's Jaguar core is explicitly not cycle-accurate.
**jsim's silicon profile is calibrated against a real console**: 32/32
timing probes match hardware, mean error 0.059 cycles/instruction, max 0.27
— across both a quiet bus (68k STOPped) and a busy one (68k running).
Every constant carries its provenance: which probe, which bench log.

### 2. Nobody could tell you WHY code is slow. Stall attribution can.
No existing tool answers "is this loop issue-bound, latency-bound, or
bus-bound?" jsim attributes every stalled cycle to its cause: ALU bubble,
load latency, DIV shadow, flag latency, taken-jump refill, external fetch,
bus contention. First result of pointing it at real code: **vbcc -O3 GPU
code spends 26% of its time in taken-jump refill** — a number nobody has
ever been able to see, and exactly what a scheduler must fix.

### 3. Hazards were folklore enforced by nothing. Now they're counted by the oracle.
The JRISC's silicon traps — the bug-13 write-after-write landing order, the
indexed-store unprotected-DATA erratum, MOVEI/jump in a delay slot — are
assembled silently by every existing assembler (rmac even mis-assembles
forward `jr`). jsim *models* the first two (your code misbehaves in the
simulator exactly as on silicon) and *counts* all of them as lints, plus a
BigPEmu-divergence counter for the class of code that is hardware-correct
but emulator-wrong.

### 4. The community's performance folklore is now measured fact.
The calibration bench settled, with reproducible numbers:
- Local issue rate is **1.00 cyc/instr** (the U-235 "2 ticks/MOVE" figure
  was a timer artifact).
- GPU-in-main costs **6.24x** local on a truly quiet bus and **13.5x**
  under a busy 68k — the famous 8.5x was measured with a 68k that idled
  but was never STOPped.
- 68k bus contention costs DRAM-bound GPU work **2.1x** (row thrash), and
  local-SRAM GPU code **under 1%** — "lay off the 68k," quantified.
- Internal loads are ready a cycle earlier than the TRM implies; taken
  jumps cost 4 (1 + 3 refill); consumed DRAM loads take ~16 cycles;
  stores never pay contention; the VC modulus is 524.
Anyone with a Skunkboard can re-run the whole suite (`calib/`) and verify
every number on their own console.

### 5. Headless, deterministic, parallel emulation with an honest debug API.
BigPEmu is closed source, Windows-under-Wine, serializes all users through
a global lock, overwrites its own config, and its headless screenshots read
the DRAM the 68k wrote — not what the OP actually displays. jagemu is
native, open, deterministic (same ROM + same inputs = identical frames),
true multi-instance, JSON in/out, with breakpoints/watchpoints/peek/poke/
disassembly/audio capture — and its screenshots are the **true Object
Processor scan-out**.

### 6. Compiler output can finally be benchmarked and regressed.
No compiled-vs-hand-asm numbers have ever been published for this platform.
Cobweb's shootout pipeline compiles with the public toolchains (vbcc today;
Brainstorm GCC next), runs the output under the calibrated oracle, and
reports cycles + attribution — deterministic, so it doubles as a CI
regression gate. The first public report is being generated now.

### 7. Fully open, no strings.
vbcc and vasm prohibit commercial use without written consent; BigPEmu,
Raptor/JagStudio, and U-235 are closed. Cobweb is being released openly on
a platform Hasbro placed in the public domain in 1999 — nothing in the tree
carries a restrictive license (provenance audit in `OSS_RELEASE.md`).

## Designed and in build order (the rest of the suite)

8. **jas** — an assembler that refuses to assemble lies. **v1 shipped**
   (`sim/crates/jas/`): the full GPU+DSP instruction set with rmac-compatible
   syntax, and a hazard pass that errors — with fix-its — on bug-13
   write-after-write, the indexed-store erratum, JUMP/MOVEI in a delay slot,
   and out-of-range branches. Its encoding is proven by assembling programs
   and running them in jsim, so assembler and simulator cannot disagree.
   Alongside rmac, not against it. Still to come: a section/relocation model
   with SRAM overlay groups as first-class objects, and macro/include support.
9. **jtest** — verification as a product. **v1 shipped** (`sim/crates/jtest/`):
   runs a program (flat `.bin` or jas-assembled source) in jsim and compares a
   captured memory region and the register file. `jtest diff` is the shadow
   harness (candidate vs reference); `jtest profiles` runs the same code under
   `silicon` and `bigpemu` and reports divergence — catching code that is
   hardware-correct but emulator-wrong *before* a hardware session,
   deterministically. `jtest golden` is a one-command regression gate. Still to
   come: on-device dual-compute runs and BigPEmu/hardware differential.
10. **jopt** — the scheduler that cannot ship a wrong answer. **v1 shipped**
    (`sim/crates/jopt/`): it fills wasted delay slots, and every transform is
    re-assembled through jas (so hazards and `jr` ranges re-validate) and run
    in jsim against the original — kept only if the captured memory and
    registers are byte-identical. That equivalence certificate is the whole
    point: jopt can try aggressively because jsim catches any mistake. On its
    first real input it found a redundant-compare + slot-fill and proved it
    safe. Still to come: DIV-shadow packing, software-pipelined back edges, and
    a cycle-cost objective under the calibrated bus model.
11. **jcc** — the compiler. **v1 shipped** (`sim/crates/jcc/`): a restricted,
    statically allocated systems language (int variables, arithmetic, if/else,
    while, store) compiling to JRISC that is *auditable by construction* — jcc
    feeds its own output back through jas, so any hazard it emits is a compile
    error, not silent wrong silicon. It reports a whole-program SRAM budget
    ledger against the 4 KB local RAM, and its output composes with jopt and
    runs in jsim. Still to come — the headline features: **automatic overlay
    streaming past the 4 KB/8 KB ceiling** (the compiler Atari promised in 1995
    and never shipped), bit-exact fixed-point intrinsics, explicit SRAM/DRAM
    placement, and a 68000-strict backend that makes the known m68k-gcc booby
    traps impossible by construction.
12. **jdbg** — one debug frontend over Skunkboard/GameDrive *and* the
    emulator: crash forensics with source lines, on both RISC chips. The
    current state of the art for a hardware crash is pointing a camera at
    the TV; that ends.
13. **jprof** — see the frame: 68k/GPU/Blitter/OP occupancy timelines,
    unbiased flip-interval histograms, deterministic replay walks.
14. **Documentation as a product** — every hazard, every timing fact, every
    workflow, written for developers, with hardware provenance tags, kept
    honest by CI against the simulator. Nothing untouched.

## A note on tone

None of this is a knock on the people who built the existing tools — rmac,
vasm, vbcc, BigPEmu, and the rest are labors of love that kept this
platform alive for decades, and Cobweb builds directly on what they proved
possible. This suite exists because the platform needed *more* — more
measurement, more verification, more openness — not because what came
before was wrong to build. And it is for **everyone**: whether you
hand-schedule delay slots yourself or direct an AI to do it, the machine
charges the same cycles and the tools measure them the same way.

## The thesis

The Jaguar was never limited by its silicon; it was limited by iteration
cost. Every tool above exists to make the three things that actually
determine performance — which chip does the work, SRAM bytes, bus behavior
— cheap to see and cheap to change. That is the suite Atari should have
shipped, and it is being built in public.
