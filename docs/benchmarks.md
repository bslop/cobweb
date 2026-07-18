# Compiler benchmark report

The first published measurement of Atari Jaguar compiler output at the cycle
level. It profiles the **vbcc `jrisc` C compiler** (the current public JRISC
compiler) rendering the same mandelbrot at `-O3` across the placements vbcc
supports, under Cobweb's **hardware-calibrated** timing model (jsim silicon
profile — 0.059 cyc/instr error vs. a real console).

Every number here is reproducible; the method is at the bottom.

## Method: instruction-normalized efficiency

We do **not** report "time to render." Two of the builds defeat frame-based
completion detection — the vbcc runtime's 8bpp CLUT display path renders black
under the true-OP scan-out (a documented jsim gap), and the builds manage their
framebuffers differently — so a completion frame can't be auto-detected
robustly across them. More importantly, wall-clock render time conflates the
program with the compiler; the honest measure of **compiler code quality** is
how efficiently the emitted code runs.

So each build runs a fixed 400-field window under the silicon model, and we
report **IPC** (instructions per cycle) and the **stall attribution** — the
share of cycles that were productive issue vs. each recoverable stall class.
This is exactly what jsim's per-cause attribution is for, and it needs no
completion guessing.

## Results — vbcc `-O3`, mandelbrot, per placement

| placement | IPC | issue % | jump-refill % | ext-fetch % | alu-stall % |
|---|---|---|---|---|---|
| **GPU local** (`-gpulocal`) | **0.349** | 69.5 | 25.9 | 4.3 | 0.3 |
| GPU main (all workarounds) | 0.169 | 27.3 | 9.4 | 56.7 | 6.6 |
| GPU main (`-workaround=1`) | 0.169 | 27.3 | 9.4 | 56.7 | 6.6 |
| 68000 (`-l68kmain`) † | 0.400 | 40.0 | 60.0 | 0.0 | 0.0 |

† The 68000 row is **not comparable** to the GPU rows: in the 68k build the
render runs on the 68000 CPU while the GPU sits in a wait loop. Its "GPU"
numbers describe that idle jump-heavy spinner (hence 60% refill), not compiled
render code. It's shown for completeness, not as a peer.

## What it shows

Three findings, all first-of-their-kind for this platform:

1. **GPU-local code is ~2× more efficient than GPU-in-main** (IPC 0.349 vs
   0.169). The gap is almost entirely the **external-fetch tax**: 56.7 % of the
   GPU-main build's cycles are spent fetching instructions from DRAM, versus
   4.3 % for the local build. This is the "GPU in main" penalty, quantified on
   real compiled code — and it is exactly what jcc's planned **automatic
   overlay streaming** (running hot code from local SRAM) is designed to
   eliminate.

2. **A quarter of the best-case GPU time (25.9 %) is lost to taken-jump
   refill.** vbcc's `-O3` output is branchy with unfilled delay slots — the
   JRISC slot always executes, so every unfilled one is wasted. This is
   precisely the class of stall **jopt** targets (delay-slot filling,
   software-pipelined back edges). It is not a knock on vbcc — it is the first
   time anyone could *see* the number, and it quantifies the headroom a
   hazard-aware scheduler has to work with.

3. The two workaround levels are **identical** in steady state — the JUMP/JR
   main-RAM bug workarounds don't change the per-cycle cost of this loop.

## Why this matters for Cobweb

The two dominant recoverable costs this report surfaces — 56.7 % external
fetch (GPU-in-main) and 25.9 % jump-refill (unfilled slots) — are the exact two
things Cobweb's compiler and scheduler are built to attack: **jcc**'s overlay
streaming for the first, **jopt**'s slot filling for the second. This report is
therefore also the **baseline Cobweb's own output must beat**, publicly, on the
same calibrated oracle.

## Reproduce it

Build the benchmarks with vbcc's bundled toolchain (`vc +jriscbin -O3
mandelbrot.c -gpulocal` etc.; see the vbcc `samples/build` script), then, from
`sim/`:

```sh
# for each build, run a fixed window under the silicon model and read gpu.timing
./target/release/jagemu run <build>.bin --frames 400 --fidelity silicon
```

`gpu.cycles`, `gpu.instret`, and the `gpu.timing` attribution block give every
number in the table. The silicon model's constants are themselves reproducible
on your own console (see `calib/`).

## Honest limits

- The vbcc bins are `+jriscbin` RAM builds of the stock samples; the `.j64`
  ROM images use a boot path jsim doesn't fully replicate yet.
- The 68000 row measures an idle GPU, as noted.
- This is one workload (mandelbrot). A broader suite (dhrystone, hennessy,
  hand-written kernels, and — when it lands — jcc's own output) is the next
  step; the harness (`bench/`) already generalizes to more binaries.
