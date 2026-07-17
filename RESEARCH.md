# Atari Jaguar Compiler Suite — Ecosystem & Hardware Research

Survey date: 2026-07-17. Compiled from the Tom & Jerry Technical Reference Manual Rev 8,
released source (vbcc jrisc backend, Jaguar Doom, MAME), project sites/repos, and
community archives (AtariAge via Wayback/snippets — the forum 403s automated fetches).

---

## 1. Every compiler that has ever targeted JRISC (Tom GPU / Jerry DSP)

There have been exactly **two** real compilers in 30 years:

### 1.1 Brainstorm GCC 2.6.3 "agpu" (1994–95, Atari's official dev compiler)
- GCC 2.6.x port by Brainstorm (France) for Atari Corp; emitted MadMac-syntax asm.
  Development stopped 21-Feb-1995 (bugfix cycle driven by Rebellion during AvP).
- ABI: args r0–r5, return r0, r17/r18 (or r29/r31, varies by release) FP/SP;
  used the **alternate register bank as extra locals** (`-mnoalt` to disable).
- Did schedule/interleave (filled delay slots, paired ALU ops) but assumed
  local-RAM timing only. TEXT-segment-only output; locals must be `int`;
  no floats, no long long; `-msiz` failed the build if code exceeded 4K/8K.
- **No hardware-bug workarounds** (its own TODO #1: "Handle GPU's scoreboard bugs"),
  **no main-RAM execution support** (planned `__attribute__((ext_memory)))` never shipped.
- Recovered 2023 by Zerosquare (Linux a.out build) and integrated into
  **cubanismo/jaguar-sdk** (Nov 2023) via a userspace a.out loader — this is the
  "JRISC C compiler in my SDK" from AtariAge topic 356970. Source of the agpu
  machine description never surfaced (binaries only; GPL implications unresolved).
- DOS build still downloadable: http://fadest.free.fr/gcc263pc.zip

### 1.2 vbcc `jrisc` backend (Volker Barthelmann, 2025–26 — current state of the art)
- Written during 2025; "vbcc jrisc preview 1" released ~May 2026.
  Source: http://www.ibaug.de/vbcc/vbcc.tar.gz (`machines/jrisc/`, 1848-line machine.c);
  toolchain: http://www.ibaug.de/vbcc/vbccjrisc.zip. Assembles with vasm
  (`vasmjagrisc_std -opt-jr=r28`), links with vlink.
- **Implements:**
  - 64 registers modeled (alternate bank modeled but currently disabled);
    register pairs for future 64-bit; custom register-allocator cost functions.
  - Placement attributes: `__gpulocal` / `__dsplocal` / `__gpumain` / `__dspmain`
    (+ CLI equivalents), sections `.gpu`/`.dsp`.
  - **Main-RAM JUMP/JR bug workarounds** via `-workaround=<bitmask>`:
    32-bit-align all jumps (NOP-pad, conditional jr → movei+jump cc), two NOPs
    after jumps, 8-byte-align jump targets + same-page `+2` movei fixup.
  - **Banking** (call-gated overlays): `__gpubank(n)` / `__dspbank(n)` →
    `.gpubankN` sections + `___gpucallbanked` runtime thunks; default 4 banks/core.
  - Scheduler pass `vscjrisc` (latencies: move 2, ALU 3, load 4, indexed load 6,
    div 18; CCR hazard tracking) — self-described "V0.0", work in progress.
  - Runtime: 68k bootstraps, `main()` on GPU by default (or DSP via `-ldspmain`),
    optional `__main68k` via bundled vbccm68k; framebuffer stdio console;
    three stacks per core; `__blocal` local-RAM string/mem functions;
    targets: .j64 ROM, RAM binary, Skunkboard.
  - Signed div/mod + 32×32 mul via per-memory-space lib calls; inline MOD reads
    G_REMAIN/D_REMAIN with sign fixup.
- **Missing:** floating point, long long, VLAs, `__interrupt` (ISRs must be asm).
- **License:** vbcc freeware — free non-commercial use only; commercial use needs
  the author's written consent; not OSI open source; no public repo for this backend.
- No published compiled-vs-hand-asm benchmarks anywhere (the samples dir ships a
  dhrystone/hennessy/mandelbrot benchmark matrix, but no numbers published).

### 1.3 Things that are *not* compilers (common confusions)
- **The Removers never had a compiler**: their C support is stock m68k-MiNT GCC;
  all their JRISC code is hand asm. smac/sln were SubQMod's MadMac/ALN fixes
  (ancestors of rmac/rln), not Removers tools.
- **No GCC or LLVM backend for JRISC exists** (verified via GitHub/targeted search).
  Adjacent: cubanismo/jrisc_tools (C disassembler/encoder + branch-alignment
  analysis), laoo/TomNJerry (WIP simulator), blipjoy/blipjag (Rust assembler),
  djipi/Jwarn (wait-state warning tool).

---

## 2. Assemblers, linkers, debug infrastructure

| Tool | What | Status |
|---|---|---|
| **rmac/rln** | MadMac/ALN lineage (Dyer → SubQMod smac/sln → Hammons/ggn). 68k+6502+JRISC+56001; unique **Object Processor list assembler mode**. The mainstream homebrew default. | Active; repos at tiddly.mooo.com (GitLab mirror ggnkua/rmac-mirror); rmac.is-slick.com site lags the repos |
| **vasm/vlink** | Wille/Barthelmann. jagrisc module since 2015; madmac-compat syntax; `-opt-jr` (JR→MOVEI+JUMP rewriting), `AJR`/`AJUMP` aligned-jump pseudos (v2.0f, Jul 2026); **documented ELF machine/reloc types for JRISC** (sun.hasenbraten.de/~frank/docs/elf_jrisc.html) — the only relocatable-object standard for GPU/DSP code | Very active (2.0f Jul 2026); free for non-commercial use |
| **jlinker** | Removers' OCaml ALN replacement | Active (Apr 2026) |
| **cubanismo/jaguar-sdk** | Modernized Atari SDK: rmac/rln, m68k GCC, agpu GCC 2.6.3, jcp, dosemu-wrapped DOS tools, **GDB with JRISC disassembly (jrisc_tools) + jserve Skunkboard GDB stub**, Docker | Active (Mar 2026). Dropped vbcc/vasm deliberately |
| **BigPEmu** | Accuracy leader (full library + Jaguar CD/VR/JagLink). **Scripting API**: breakpoints, memory r/w, audio frames; Noesis-integrated debugger | Closed source, active (~1.221) |
| **Virtual-Jaguar-Rx** (djipi) | Only open-source dev-debugger: M68K + C source-level tracing, GPU/DSP memory browsers, Alpine mode | Active-ish (2024–25); mediocre accuracy |
| **MAME** | jaguar driver flagged NOT_WORKING; CPU core not cycle-accurate — don't validate timing against it | — |
| Hardware | **Skunkboard** (jcp, ~250–280 KB/s to RAM, skunklib printf-over-USB), **JagGD** (JagGDCmd `-rd` debug stub; open_jaggd), BJL | Active |
| IDE support | **None.** No LSP, no VS Code JRISC extension, no DAP adapter for any emulator | Gap |

Platform licensing: Hasbro released all Jaguar rights/patents into the public
domain May 14, 1999 — the platform itself is legally open.

---

## 3. Runtimes, engines, and what shipped games actually did

- **Raptor / JagStudio** (Reboot): 68k+RISC asm engine; OP-driven sprites/tilemaps/
  particles; BASIC (BCX→C), C, asm front ends. Closed source (open-sourcing
  "planned"); JagStudio v1.11 (2023), Raptor 2.0.31.
- **U-235 SoundEngine**: DSP-resident audio (8 voices, MOD playback, 16 kHz default);
  its 8 KB binary **occupies Jerry's entire local RAM** — the DSP becomes an audio
  chip, period. "Kudos Ware" license, closed.
- **rmvlib/jlibc** (Removers, LGPL-2.1): the only open runtime. OP list management,
  blitter effects, DSP sound driver; 68k-centric C.
- **Processor splits of the best commercial titles:**
  - **Doom** (source released): 68k = C game logic; GPU = renderer in **nine
    sequentially-loaded overlays** (resident stub at $F03000 with mailbox protocol
    `_gpucodestart`/`_gpufinished`; GPU copies its own overlays; LOADPOINT $F03100);
    DSP = SFX + collision/math assist — fully saturated, which is why in-level
    music was cut.
  - **BattleSphere** (best-documented split): 68k = pads/housekeeping only;
    GPU = game loop, collision, 2nd-stage polygon pipeline; DSP = 1st-stage polygon
    pipeline + networking + sound + timers; Blitter = raster; OP = all screen
    composition. 60 FPS max / 30 avg.
  - **Iron Soldier**: aggressive LOD (~200 polys/robot), 30 FPS.
  - **AvP**: everything on the 68k (AI in 68k asm) → the famous frame rate.
  - **Another World port** (Removers): 68k JIT of VM bytecode, GPU drove blitter
    raster, Jerry ran a *Paula emulator*, GPU-decompressed LZ77 backgrounds.
- Performance facts: blitter texturing ≈ 3.8 Mpix/s (~7 cycles/px); flat/gouraud
  phrase-mode 10–20× faster than textured — why smart games avoided texturing.
  Stopping the 68k: +15.5% measured in a bus-heavy renderer; ~5–10% typical
  (Atari Owl); Leonard Tramiel: best 68k instruction is `halt`.

---

## 4. Hardware ground truth a code generator must obey

(Full detail: TRM Rev 8, jag_v8.pdf at hillsoftware.com; errata pp.133–141.)

### ISA / registers
- Two banks × 32 × 32-bit regs; bank via FLAGS.REGPAGE; **IMASK forces bank 0**
  (interrupt bank). R31.b0 = interrupt SP, R30.b0 corrupted by dispatch.
  MMULT's matrix operand lives in the *other* bank.
- All insns 16-bit; MOVEI is 48-bit, immediate words **always little-endian word
  order** regardless of BIG_INSTR.
- Flags: Z, N, C only — **no V**; C undefined after logical/multiply ops.
  ADDQT/SUBQT are flag-transparent (pointer math between compare and branch).
  ABS/NEG fail on $80000000.
- GPU-only: LOADP/STOREP (via unscoreboarded G_HIDATA), SAT8/16/24, PACK/UNPACK.
  DSP-only (same opcode slots!): SAT16S/SAT32S, ADDQMOD/SUBQMOD (circular buffers
  via D_MOD), MIRROR. Two sub-targets, one family.
- DIV: unsigned only, 18-cycle result, parallel with ALU; remainder in
  G_REMAIN/D_REMAIN (may need divisor fixup); DIV_OFFSET → 16.16 fixed divide.
- MAC group IMULTN→IMACN*→RESMAC is atomic and **nothing else may intervene**.
  MMULT: no jump into it, no load/store before it, 2 insns between MMULTs;
  DSP MTXA hard-limited to the first 4K of DSP RAM (Jerry bug 6).
- JUMP condition field: 5-bit (EQ/NE/CC/CS/PL/MI/HI combos, $00 always, $1F never).

### Pipeline (scheduler model)
- Prefetch queue (2 longwords) + read/compute/writeback; dual-ported regfile.
- Scoreboard protects **reads only**. Two unprotected-write bugs define hard
  codegen invariants:
  - **Never write a register twice without an intervening read** (bug 13; WAW
    completes out of order). Canonical guard: `or rn,rn`.
  - **Indexed stores don't scoreboard their data register** (bug 2) — unsafe when
    data comes from DIV or an external load; interpose a dependent op.
  - Same class: consecutive DIVs < 16 clocks apart using the first quotient (bug 25).
- Result-use latencies: ALU 1-cycle bubble; MOVE/MOVEQ none; internal load 3–4;
  indexed load 5–6; DIV 18. Taken jump = 3 cycles from local RAM.
- **One delay slot**, always executed, interrupt-atomic with the jump. Forbidden
  in slot: MOVEI, JUMP/JR, MOVE PC; IMASK-clear must not sit in an interrupt
  return's slot (bank-switch hazard).
- Canonical optimization: interleave two independent dependency chains
  (documented 6-vs-10-cycle example, TRM p.62).

### Memory model
- GPU: 4 KB local RAM at $F03000; DSP: 8 KB at $F1B000 (+2 KB wave ROM).
  Interrupt vectors occupy base+16×n; code starts above them.
- **Local RAM is 32-bit-aligned-access only** — LOADB/LOADW silently degrade to
  long accesses. char/short in local RAM need shift/mask or longword slots.
- External masters see local RAM as 16-bit; a **+$8000 mirror is 32-bit
  write-only** — blitter phrase-blit into the mirror is the fast code-upload path.
- Bus priority (hi→lo): refresh > DSP-DMA > GPU-DMA > Blitter-hi > **OP** >
  DSP > 68k-under-interrupt > GPU > Blitter > 68k. Never raise anything above
  the OP (bug 24: line-buffer corruption — GPU DMAEN and Blitter BUSHI are
  effectively forbidden). 68k ISRs must write INT2 ($F000E2) or GPU/blitter
  priorities stay depressed. G_CTRL.BUS_HOG speeds GPU-in-main at the cost of
  starving lower masters. Jerry's external bus is 16-bit (half bandwidth).

### Running RISC code from main RAM ("GPU in main")
- Official errata (bug 15) says impossible; community (AtariOwl + Steve Scavone,
  formalized Oct 2009) proved it works under alignment rules:
  1. JUMP sources LONG-aligned; 2. MOVEI before cross-memory jumps (settles the
  prefetch queue); 3. JR internal-page targets at long+2 parity, external-page
  LONG-aligned; 4. two NOPs after every JUMP/JR.
- Measured: **≈8.5× slower than local RAM** on a quiet bus (U-235 experiment:
  ~2400 vs ~20900 ticks for 1200 MOVEs); Owl: 20–90% of local speed depending on
  bus load, still ≈2× a 68k. Typo's data point: 200 collision tests serial on
  GPU-in-main lost to running them in parallel on the otherwise-idle 68k.
- Modern PoC on real hardware: 42Bastian/JaguarDemos `yarc_in_main` (2023).
- Data in main RAM was never buggy — only instruction fetch of JUMP/JR.

### Overlays
- Doom: resident GPU stub + mailbox protocol, 9 renderer phase overlays,
  GPU self-copies code (paired 32-bit load/store loop). Faster method per TRM:
  blitter phrase-blit into the +$8000 alias.
- Recommended modern pattern (CyranoJ): ~2 KB double-buffered chunks — execute
  one half of local RAM while the blitter fills the other half.
- **Atari's 1995 FAQ claimed the dev-kit RISC compiler "transparently swaps code
  through the cache… lets developers write RISC code without concern for the
  cache size limits" — an overlay-managing compiler was the official plan of
  record. It never shipped. Nobody has built it in the 31 years since.**

### Interrupts / IPC
- Vector = base+16×n; IMASK set on entry; pushed PC is *last executed* insn
  (ISR must addq #2); no hardware register save; return sequence is a fixed
  5-insn idiom with the FLAGS restore constraints above.
- All cross-processor signalling is mailbox/doorbell: G_CTRL/D_CTRL bit 2
  (host→RISC IRQ0), CTRL bit 1 → 68k INT1 latch ($F000E0); Jerry's IRQ out is
  wired to GPU IRQ1. **Only the local processor may clear its own GO bit**
  (bug 23) — stop requests must go through a shared-memory flag.
- Host 32-bit accesses into local RAM are atomic and highest-priority → simple
  semaphores work; multi-word structures need Doom-style flag protocols.

---

## 5. Gap analysis — what nobody provides today

1. **No modern open compiler.** The only two ever: a 1995 GCC with no bug
   workarounds, and a 2026 non-OSI preview with no floats and a v0.0 scheduler.
   No LLVM or modern GCC backend exists. No public benchmarks of compiled vs
   hand asm.
2. **No automatic overlay streaming.** vbcc's banking is call-gated thunks with
   manually assigned banks; Doom's system was hand-built. The double-buffered
   blitter-paging pattern has never been automated. (Atari promised exactly this
   in 1995.)
3. **No whole-program placement.** Hot-code-in-local / cold-code-in-main (with
   alignment workarounds) / data-vs-code placement is entirely manual everywhere.
4. **No dual-core awareness.** Nothing helps split work GPU/DSP or generates the
   mailbox/semaphore glue (BattleSphere's split was months of hand asm). U-235
   monopolizing the DSP is the norm because sharing Jerry is too hard.
5. **No hazard-complete scheduler.** The WAW bug, indexed-store bug, DIV bugs,
   MMULT restrictions, and delay-slot rules as *correctness* constraints +
   dual-chain interleaving as optimization — no tool models all of it.
6. **No C-level interrupt story.** `__interrupt` unimplemented in vbcc; ISRs are
   hand asm everywhere.
7. **No fixed-point / CRY language support.** DIV_OFFSET 16.16 divide, MAC
   pipeline, PACK/UNPACK exist in silicon but no language surface.
8. **No Object Processor compiler.** rmac has an OP *assembler* mode; nothing
   compiles a scene/display description into optimal OP lists + the required
   per-frame refresh code.
9. **No modern IDE/debug integration.** No LSP, no DAP; BigPEmu's scripting API
   and cubanismo's GDB+jserve stack are unexploited foundations.
10. **Ecosystem licensing is fragmented**: vbcc (non-commercial), Raptor (closed),
    U-235 (closed); the only open pieces are rmac/rln (informal), vasm (non-comm),
    Removers (LGPL), jaguar-sdk (grey). A clean, fully open suite would be first.

## 6. Assets worth building on (rather than rewriting)

- **Frank Wille's ELF machine/relocation spec for JRISC** — an existing object
  format standard; adopt it for interop with vasm/vlink.
- **jaguar-sdk's GDB + jserve + jrisc_tools** — debugging plumbing to integrate.
- **BigPEmu's scripting API** — automated benchmark/CI harness on the accuracy-
  leading emulator; Virtual-Jaguar-Rx for open-source introspection.
- **Skunkboard/JagGD CLI tools** — hardware-in-the-loop testing.
- **Doom source + rmvlib (LGPL)** — reference implementations of overlay
  protocols, OP management, interrupt idioms.
- **TRM Rev 8 errata list** — the complete correctness spec (29 Tom + 6 Jerry
  bugs; the compiler-relevant ones are 2, 13, 15, 16, 23, 24, 25 + delay-slot
  and MMULT sequencing rules).

## 7. Key sources

- TRM Rev 8: https://www.hillsoftware.com/files/atari/jaguar/jag_v8.pdf
- JRISC doc: https://www.mulle-kybernetik.com/jagdox/risc_doc.html
- vbcc: http://www.compilers.de/vbcc.html · http://www.ibaug.de/vbcc/vbccjrisc.zip
- vasm + ELF spec: http://sun.hasenbraten.de/vasm/ · http://sun.hasenbraten.de/~frank/docs/elf_jrisc.html
- rmac: https://rmac.is-slick.com/
- jaguar-sdk: https://github.com/cubanismo/jaguar-sdk · jrisc_tools: https://github.com/cubanismo/jrisc_tools
- Removers: https://github.com/theRemovers
- GPU-in-main: https://atariowlproject.blogspot.com/2009/10/atari-jaguar-homebrew-whats-this-lay.html · http://www.u-235.co.uk/gpu-in-main-science/ · https://github.com/42Bastian/JaguarDemos/tree/main/yarc_in_main
- Doom source: https://github.com/JNechaevsky/jaguar-doom
- BigPEmu: https://www.richwhitehouse.com/jaguar/ · VJ-Rx: https://github.com/djipi/Virtual-Jaguar-Rx
- BattleSphere architecture: https://scatologic.com/faq.html
- 1995 Atari FAQ (overlay-compiler promise): https://www.atariarchives.org/cfn/09/03/01.php
- Hasbro open-platform release: https://en.wikipedia.org/wiki/Atari_Jaguar
