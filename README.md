# Cobweb

**A full compiler and tooling suite for the Atari Jaguar — named for the Jaguar II
development board that never saw the light of day.**

**Mission: open source, for everyone — the suite Atari should have shipped in
the first place.** Hasbro released the Jaguar into the public domain in 1999;
Cobweb finishes the job: fully open, painfully easy to use, exhaustively
documented, and every performance claim traceable to a bench log reproducible
on your own console. For everyone — hand coders and AI coders alike; the
machine charges the same cycles either way. Release plan:
[`OSS_RELEASE.md`](OSS_RELEASE.md).

**Authorship, plainly:** the code and documentation in this repository were
written by an AI (Claude, Anthropic) working under human direction. The
maintainer — who prefers to remain anonymous and claims no personal credit —
sets the goals, makes the decisions, runs the hardware bench, and reviews
what ships; the AI does the writing. Every timing claim is verified against
real silicon regardless of who typed it — the bench logs don't care who
wrote the code.

The Jaguar II's silicon (Oberon, Puck) and its Cobweb dev board were cancelled in
1996; the fairy name comes from *A Midsummer Night's Dream*, following Tom & Jerry's
successors. This project takes the name as a promise: build the toolchain the
platform was never given, so that anyone using it can push the Jaguar beyond
anything done today.

## Why this can leapfrog everything (the 30-second version)

In 30 years exactly two compilers have ever targeted the JRISC cores — a 1995 GCC
with no hardware-bug workarounds, and a 2026 non-open preview. Atari's 1995 FAQ
promised a compiler that would "transparently swap code through the cache… without
concern for the cache size limits." It never shipped, and nobody has built it
since. Meanwhile the local project corpus has measured where performance actually
lives: **(1) which chip does the work, (2) SRAM bytes, (3) bus behavior — in that
order** — and none of those three are addressed by any existing tool.

Full ecosystem/hardware survey: [`RESEARCH.md`](RESEARCH.md).

## Getting started

**`make help`** lists every one-command workflow. New here? Read
[`docs/quickstart.md`](docs/quickstart.md) (humans) or [`AGENTS.md`](AGENTS.md)
(AI agents) — zero to a running, measured Jaguar in two commands.

## The spec

The suite's requirements were distilled from a private corpus of shipped
Jaguar ports and hardware sessions — every requirement traces to a measured
cost or a documented failure. The load-bearing knowledge (the hazard
rulebook, timing facts, and their [HW]-provenance) is being vendored into
`docs/` so this repository stands alone; until then, the measured facts
live in `calib/` (bench logs) and `sim/crates/jag-core/src/risc/timing.rs`
(constants with provenance comments).

## Which tool do you want?

If you want to **play or test Jaguar software**, you want
[BigPEmu](https://www.richwhitehouse.com/jaguar/) — Rich Whitehouse's
emulator runs the whole library, in real time, with excellent fidelity, and
it has a scripting API. It is also what Cobweb itself trusts for functional
behavior: `jagemu oracle-diff` compares machine state against BigPEmu, and
when the two disagree on what a program *computes*, jsim treats itself as the
one that's probably wrong.

jsim is a different kind of tool: a **measurement instrument for code you are
writing** — deterministic, headless, machine-readable output, exact cycle
attribution, and timing calibrated against real hardware (Skunkboard probes)
with its error bars written down. It covers the narrow set of paths a
toolchain needs and runs well below real time. Think logic analyzer, not
television — most days a Jaguar developer wants both on the bench.

## Components## Components

| Tool | What | Status |
|---|---|---|
<!-- ☠️ jsim is the ROLE; the binary you actually run is `jagemu`. The table
     used the intended name only, so readers went looking for a jsim command
     that does not exist. Both names are given until the rename lands. -->
| **jsim** (CLI: `jagemu`) | Cycle-honest Tom/Jerry simulator + open full-system emulator: real scoreboard/pipeline semantics, calibrated bus model, `silicon` vs `bigpemu` fidelity profiles, stall attribution | **v0 landed** — `sim/` (imported from the seed emulator + new truth layer: scoreboard, WAW/erratum modeling, div shadow, DRAM page costs, per-cause stall stats via `--fidelity`) |
| **jas** | JRISC assembler that refuses to assemble lies: hazard checking as errors, real diagnostics with fix-its, rmac-compatible syntax | **v1 landed** — `sim/crates/jas/`: full GPU+DSP instruction set, two-pass labels, expression evaluator, and a hazard pass that errors on bug-13 WAW, the indexed-store erratum, JUMP/MOVEI in a delay slot, and out-of-range `jr` — with fix-its. Encoding proven by assembling and running in jsim. (Sections/relocations + macros: next.) |
| **jtest** | Verification harness: fidelity-profile diff (catches hardware-correct-but-emulator-wrong code deterministically), shadow diff of candidate vs reference, golden-vector regression | **v1 landed** — `sim/crates/jtest/`: runs JRISC (`.bin` or jas-assembled source) in jsim, compares captured memory + registers; `jtest profiles` flags silicon/BigPEmu divergence with the mechanism. 4 tests. |
| **jopt** | Scheduler, **bytes first**: every transform re-assembled through jas and proven behavior-identical in jsim before it is kept | **v1 landed** — `sim/crates/jopt/`: delay-slot filling with a jsim equivalence certificate (jas rejects a transform that becomes a hazard; jsim rejects one that changes behavior). Found a redundant-cmp + slot-fill and proved it. (Div-shadow packing, software-pipelined back edges: next.) |
| **jcc** | The compiler: a restricted systems language → **auditable** JRISC (jas re-checks every emit for hazards), static register allocation, whole-program SRAM budget ledger | **v1 landed** — `sim/crates/jcc/`: int vars, arithmetic, if/else, while, store; compiles to hazard-clean JRISC verified by running in jsim. 6 tests. (Overlay streaming, fixed-point intrinsics, DRAM placement, 68k backend: next.) |
| **jdbg** | Source-level debugger: breakpoints and stepping by **source line** (not raw address), live register inspection, crash forensics that name the source line | **v1 landed** — `sim/crates/jdbg/`: assembles via jas (keeping the addr→line map), runs in jsim, breaks/steps by source line. 4 tests. (Skunkboard/GameDrive hardware backend behind the same `Session` API, and jcc variable-name inspection: next.) |
| **jprof** | See where the cycles go: per-cause cost breakdown (issue vs stalls vs bus), IPC, plain-language bottleneck diagnosis, and build-to-build diff | **v1 landed** — `sim/crates/jprof/`: turns jsim's stall attribution into a profile that names the bottleneck (it surfaced vbcc's 26%-in-jump-refill) and proves whether an optimization moved it. 3 tests. (Per-region hotspots, frame timelines: next.) |

| **jln** | The linker (successor to rln): resolves cross-object symbols + relocations from jas objects into a loadable image; SRAM overlay groups next | **v1 landed** — `sim/crates/jln/`: `.jo` object format, movei/long relocations, global symbol table, sparse image layout; assemble→object→link→run proven in jsim. 3 tests. |

Build order: jsim → jas → jln → jtest → jopt → jcc → jdbg/jprof.
**The pilot project** — live kernels, a running shadow harness,
and a boot gate that tells us within two minutes whether our output lies.

**`calib/`** — hardware calibration ROMs (Skunkboard + jsim builds): timing
probes that pin jsim's `CAL:` constants against real silicon, with predicted
tables checked in for diffing against bench logs. See `calib/README.md` for
the bench protocol. A retail ROM corpus is earmarked for the accuracy-oracle sweep (jsim vs
BigPEmu differential; point `make compat` at your own dump directory) once
Jerry audio/CRY land.

## sim/ — the emulator seed

`sim/` is the imported working tree of the seed emulator (an instrumentation-first,
deterministic, multi-instance, std-only Rust Jaguar emulator — ~7.5k lines, boots
real ROMs including GPU-rasterized 3D games, true OP scan-out screenshots, JSON
debug CLI). See [`sim/README.md`](sim/README.md) and
[`sim/docs/spec/`](sim/docs/spec/) for its implementation-grade hardware specs
(RISC ISA, Blitter, OP, video timing, accuracy oracle).

Its trajectory here: grow into **jsim** — add the cycle/pipeline/bus truth layer
(scoreboard stalls, div shadow, DRAM page model, fidelity profiles) on top of the
already-working functional core, calibrated against real hardware using the
serialized-measure methodology. The `jagemu` CLI name may be renamed as it takes
on the jsim role.

> Note: the original repo (the seed emulator) is untouched and retains its git
> history; the copy here includes its then-uncommitted work (`tom.rs`, `blit.rs`,
> Cybermorph framebuffer scripts).

## Design anchors

- **Correctness is a database, not folklore.** Community lore has been measurably
  wrong (the two-NOP delay-slot rule halved real capacity; the VC modulus folklore
  said 525, hardware measured 2571; Gouraud is CRY-only on silicon while BigPEmu
  accepts RGB16). Every hazard and timing fact lives as a machine-checked rule in
  jas/jsim with [HW]/[EMU]/[SDK] provenance — the docs and tools cannot disagree.
- **The emulator is the oracle; hardware calibrates the emulator.** Everything
  tests against jsim; jsim tests against silicon.
- **Performance over compatibility, always.** The suite exists to squeeze the
  machine: chip assignment, SRAM bytes, bus behavior — in that order. No design
  decision trades a cycle or an SRAM byte for interoperability with existing
  toolchains. Borrow from the ecosystem where it's free (Wille's ELF-for-JRISC
  reloc spec, rmac-syntax ingestion for existing source), ignore it where it
  constrains: object formats, ABIs, calling conventions, and runtime layout are
  ours to define around what the silicon rewards. When an existing component
  can't serve that goal, we build a new one — the emulator already set the
  precedent.
- **Fully open**, on a platform Hasbro released to the public domain in 1999 — the
  first Jaguar suite with no licensing asterisks.
