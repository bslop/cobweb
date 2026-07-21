# Cobweb — Changelog

All notable changes to the Cobweb JRISC/68k toolchain (jas, jsim, jopt, jtest,
jcc68k, jln). Newest first. Dates are when the work landed; version tags are
assigned at release.

## Unreleased

Headline: the simulator now renders OpenLara's textured 3D room, the optimizer
proves its wins against real rendered output, and a one-character assembler bug
that had been silently dropping conditional blocks is fixed.

### 2026-07-20 — blit counter split: busy vs paid, and the sign of the "over-charge"

Round 2 of the Blitter bug (night, healthy rig, byte-exact probe pair):

- **`gpu.timing.blit` is now split** into `blit_launch` / `blit_transfer`
  (the asynchronous BUSY ledger) and **`blit_wait`** — the measured
  cycles the GPU spends on B_CMD reads that observe busy, i.e. what the
  frame actually pays. On RECTSHADE_v4b: busy 54.2% of GPU cycles, paid
  wait **7.3%** — the busy ledger overstates the paid cost 7x under
  rect-shade's deliberate overlap, which is what read as a "3-4x
  over-charge" in the report.
- **The diagnosis flips**: jsim's pair-implied fill share on their
  kernel is 7.8% vs 13.6% silicon — an UNDER-charge. A/B on their probe
  proves the DSTEN recharge right: without it the full build outruns
  NOFILL (negative fill share, physically impossible); with it the sign
  is correct and 45% of the gap toward silicon closes.
- Remaining error decomposed with their own pair: the NOFILL arm alone
  is +10.4% optimistic (blit-independent — staging/external-load side),
  and GPU external accesses currently pay no contention while the
  Blitter holds DRAM (0.1% measured). Two probes specified (DSTEN RMW
  price, staging-under-blit contention) before any constant moves.

### 2026-07-20 — the rect-shade report: watchpoints, framecheck, and a DSTEN charge

Same-day response to `COBWEB_REQ_rectshade_and_calibration.md`:

- **Write-watchpoints** — `jagemu run --watch 0xLO..0xHI` + serve/ctl
  `watch`/`unwatch`/`watchlog`. Every write from ANY master logs
  `{addr, value, size, master: 68k|gpu|dsp|blitter, pc, frame}`;
  Blitter writes attribute to the Blitter, not to whoever stored B_CMD
  (unit-tested). "Who wrote this byte" is now one run.
- **Blit cost: DSTEN dest-reads were uncharged** — a DSTEN blit is a
  read-modify-write; every dest phrase pays twice on silicon. Charged as
  access counting (constants untouched): OpenLara's SHADED build gains
  19.2% Blitter busy time in jsim (the +30%-optimism outlier's first
  mechanistic piece), while the calib bench table and the Caves/NOFILL
  anchors are byte-identical (no DSTEN in those paths). Measured on
  their probe: ~3,700 launches/game-frame; the residual gap has two
  named suspects (free fire-into-busy B_CMD stores, launch-density bus
  interference) gated on the density-sweep probes.
- **`sim/tools/framecheck.py`** — scored emulator-vs-hardware frame diff
  (auto-crop/rescale/luma-normalize; pct_bad + streak_score tuned to the
  reported vertical-streak signature; exit code for gating). Self-tested
  both directions.
- **jas lint**: GPU code/data claiming $F03FF8-$F03FFF (top phrase of
  GPU SRAM, unproven on silicon) warns until the sentinel probe passes.
- **`calib/p_topphrase_upda.s`** — one rig probe, two verdicts: the
  top-phrase sentinel and the DSTA2/UPDA outer-step question. Dogfooded
  in jsim (jsim binds UPDA1→A1 set, UPDA2→A2 set, independent of the
  role swap; writing the probe caught the A2_MASK layout trap).
- `jagemu serve` honors `--fidelity` (was silently functional-only).
- Live-rig evidence for their quarantined fault: the capture tap holds a
  clean 11:50 frame and a streaked 15:23 frame of the SAME session —
  consistent with in-place console degradation, not any flashed build.

### 2026-07-20 — audio gets its instruments

- **`jagemu audiocheck <wav|rom> [--against <wav|rom>]`** — the audio
  counterpart of the screenshot pixel-diff. Alone: a health report
  (peak/RMS dBFS, DC offset, clipping, silence ratio, leading silence,
  longest dropout gap, L/R correlation, top spectral peaks via a
  hand-rolled std-only FFT). With `--against`: lag-aligned comparison of
  loudness envelope + average spectrum against an oracle capture —
  builds boot at different speeds, so the lag is measured (envelope
  cross-correlation), not assumed. Passing a ROM instead of a .wav
  captures on the fly. Validated on OpenLara: same-build captures
  pressed 100 frames apart → lag −1.65s detected, envelope corr 0.997,
  MATCH; non-equivalent stimuli → honest MISMATCH. Reads any 16-bit PCM
  WAV, so hardware captures go through the SAME analyzer as simulator
  output. 11 unit tests (tone/clip/DC/dropout/delay synthesis).
- **`sim/tools/jagtap.py`** — splits the USB capture of the real Jaguar
  between the human and Claude (a V4L2 device allows one consumer; the
  tap opens it once, MJPEG passthrough, no transcode). Human: live
  browser view. Claude: `/frame.jpg` or an atomically-rewritten `--snap`
  file. `--audio` keeps a 2-file WAV ring `audiocheck` can read.
  Verified live against the console mid-session (OpenLara on silicon,
  28 fps through the tap).

### 2026-07-20 — adoption report round 2: the full-TU switch

OpenLara switched three TUs to jcc68k the same day and filed round 2:
what broke when they tried the rest. All four items, root causes found:

- **Runtime-helper ABI (their item 2, "runtime miscompile")** — jcc68k
  called its libgcc-NAMED helpers (`__mulsi3`…) with operands in D0/D1;
  their link satisfies those symbols with divmod68k.S, which implements
  libgcc's STACK-argument convention — so every mul/div computed stack
  garbage, and gpu.c/jerry.c (mul-heavy boot paths) died on a black
  screen. The suspected volatile-MMIO ordering was a red herring. jcc68k
  now uses the libgcc convention at every call site and in its own
  `--runtime` (drop-in interchangeable both directions, test-pinned).
  **gpu.c and jerry.c now boot and render** — verified in jagemu against
  the all-gcc oracle (387/76800 px animation-phase diff).
- **Inline asm (their item 1)** — never silently dropped again: basic
  `asm("…")` passes through (`%` prefixes normalized), extended asm
  supports the corpus subset (one `"+d"/"=d"` output, one `"d"` input —
  covers main.c's `muls.w %1,%0` hot-path idiom, execution-tested),
  anything richer is a hard error with true file:line. jas learned
  `stop #imm` for the interrupt-sleep idiom.
- **Sections & statics (their item 3)** — zero-initialized and
  uninitialized globals land in `.bss` (NOBITS in ELF: main.c's 607KB of
  literal zeros became 8KB of .data), `aligned(N)` attributes are honored
  (GPU-shared mailboxes were previously aligned by luck), volatile locals
  are never register-promoted, and unreferenced statics plus
  statically-unreachable tails (code after `for(;;)`) are eliminated like
  gcc -O2 — main.o: 80KB text/599KB bss → 52KB/101KB.
- **Code size (their item 4, partial)** — mul/div/mod by power-of-two
  constants (array index scaling included) are shifts/masks now, not
  runtime calls; video.c's framebuffer clear was ~460k `__mulsi3` calls
  (≈8 real seconds of boot — their "video.c miscompile" was actually
  this slowdown; it now boots normally).
- **`long long` is a hard error** — jcc68k silently sized it at 32 bits;
  main.c's frustum cull overflowed and discarded every room (Lara on a
  black void). No 64-bit support on the 68000 yet, so it errors with
  file:line instead of wrong-rendering.

End state: **7 of 8 OpenLara C TUs build, boot, and render with jcc68k**
(byte-for-byte scene parity with the gcc oracle in jagemu); main.c waits
only on its two `long long` sites being restructured source-side.

### 2026-07-20 — the jcc68k adoption report, worked through

OpenLara filed `COBWEB_REQ_jcc68k_adoption.md` after switching its six
kernels to jas: what blocked jcc68k as their code generator, most impactful
first. All five items:

- **jas `--elf-obj` (item 1, "the single unlock")** — writes an ELF32
  big-endian m68k relocatable object GNU `ld` accepts, so a jaguar.ld
  project migrates to jcc68k/jas one translation unit at a time. Real
  `.text`/`.data`/`.bss` sections, RELA relocations (`R_68K_32` /
  `R_68K_16` / `R_68K_PC16`), locals + globals + externs in the symtab.
  Verified against `m68k-linux-gnu-ld` with a MEMORY-region script:
  cross-object `jsr`/`lea`/`bsr.w` and `.data` symbols resolve
  byte-exactly; OpenLara's `video.c` links and `ld -r`-merges clean.
  Flow doc: `docs/gnu-interop.md`. Found on the way and fixed: jln
  patched word-branch relocations with the *absolute* address instead of
  a displacement (every cross-object 68k `bsr.w` landed in the weeds),
  and abs.w relocations were typed as 4-byte patches in a 2-byte slot.
- **jcc68k leaf/prologue codegen (item 2)** — `link`/`unlk` only when the
  frame is actually used; save/restore only the callee-saved registers
  the body names (one register → `move.l`, not `movem`). The report's
  `blit_wait()` drops from link + 10-register movem round trip to the
  bare 3-instruction spin + rts.
- **runtime lib (item 3)** — already shipped as `jcc68k --runtime`; with
  `--elf-obj` it now assembles to a `jrt68k.o` for `-nostdlib` GNU links
  (soft mul/div/mod + the 16.16 fix helpers, libgcc not required).
- **diagnostic line attribution (item 4)** — the preprocessor emits
  `# N "file"` line markers and the lexer consumes them, so errors name
  the true source file:line instead of a position in the expanded text.
  The report's needle ("non-constant expression in initializer" at a
  bogus line 1573) resolves instantly now — and all 8 OpenLara jaguar C
  translation units compile clean with the MULTIROOM flag set.
- **dialect notes (item 5)** — bare `.long` is rmac's long-align (was a
  silent no-op that left a GPU table 2-misaligned on silicon); data
  directives with no operands warn instead of emitting nothing;
  `.qphrase` added; hazard fix-its now say `move rX,rX` settles the
  scoreboard exactly like `or rX,rX` (both were already credited —
  regression tests pin it).

### 2026-07-20 — the 68000 pays for its bus

- **jsim: 68k external-bus wait charge, split fetch/data** — the cacheless
  68000 no longer gets textbook timings against free memory. Whole-program
  validation on OpenLara's Caves: **4.95 fps vs 4.90 on hardware (+1.0%)**,
  NOFILL 5.43 vs 5.45 (−0.4%), fill share 8.8% vs 10.1%. Constants are
  game-anchored; the disclosed gap is that dense synthetic access streams
  (the calib probes) need ~16x the data charge to match silicon — no linear
  model fits both regimes, the evidence points at bus-grant queueing, and a
  density-sweep probe family is queued to pin it. Supersedes
  `wip/m68k-bus-wait`. *(commit 147eda8)*

### 2026-07-19 — the asynchronous Blitter, a 68k profiler, and the fixture pipeline

- **jsim: the Blitter is asynchronous** — resolves OpenLara's reported 2.4x
  fill over-charge, and every cost-side hypothesis died on hardware first:
  XADDINC source coalescing (refuted — `du=0.25` costs the same as `du=1.0` on
  silicon), short-span launch overhead (refuted — new 1/2/4-px probes match
  within ~5% across the whole 1–256 px curve). The real mechanism was
  concurrency: gpu_geotex overlaps each blit with the next span's DDA math and
  jsim serialized it by charging the full duration to the launching `B_CMD`
  store. Launch now costs a store; `B_CMD` reads report busy until the
  (unchanged, silicon-calibrated) duration drains. NOFILL fill share: 24% →
  **10.5% vs 10.1% on hardware**; Caves fps 4.59 → 5.37 vs hw 4.9.
  *(commits d384217, f64f3f7, 168a834)*
- **jagemu: exact 68k cycle profiler** (`run --pc-histogram [--map] [--bucket]
  [--top]`) — per-PC cycle attribution (not sampled), STOP-sleep tracked
  separately, ISR-vs-main split, plus wall-clock accounting per master.
  Requested by OpenLara; first run also caught its own trap: the tree's stale
  non-AUTOSTART binary sits on the title screen forever, and profiling it
  produced a convincing wrong answer for three reply files before a screenshot
  exposed it. Corrections issued same-day. *(commits 714ea70, 6b52c50, 737fbf4)*
- **jsim: wild bus accesses no longer abort the process** — `Window` accessors
  are bounds-checked (reads off the end return 0, writes drop), fixing the
  reported crash that killed long profiling runs. *(commit 7336d6a)*
- **jas/jopt: `-d NAME[=VALUE]` build defines** — previously neither tool
  could assemble a kernel that needs its Makefile define set (gpu_geotex needs
  eight), which made jopt unusable on the one kernel that matters.
  *(commit c122535)*
- **jopt: fixture pipeline proven end to end** — `calib/mkfixture.py`
  snapshots live state out of jagemu into a certificate fixture (deriving the
  capture region from the kernel's own params, zeroing the self-masking
  overlap), and jopt now lands **34 certified delay-slot fills on the
  production gpu_geotex** (3526 → 3458 bytes) with byte-identical rendered
  output, verified independently via the new `jtest` `fxrun` post-mortem
  example. *(commits c122535, e9041c4)*
- **calib: 68k timing calibration set** — the 68000 measured against silicon
  at three instruction mixes (fetch-only 1.29x, DRAM reads 1.54x, bytewise
  copy 1.73x — jsim too fast in all three), OP-vs-68k contention null three
  ways. A flat per-bus-cycle charge is calibrated and parked on
  `wip/m68k-bus-wait`: correct on the bench, wrong to merge until a split
  fetch/data model exists. *(commits 4595b82, 78a6808, ea50a5c, 87dd39e;
  resolved next day by 147eda8 — see 2026-07-20 above)*

### jas — the hazard-aware assembler

- **`.if`/`.rept` conditions accept a lone `=` as equality.** The expression
  lexer handled `==` but not a single `=`, so `.if NOFILL=0` (and every
  `.if SYM=N`, the form rmac uses) failed to lex, evaluated false, and the whole
  block was silently dropped. This had been mis-assembling every rmac-authored
  kernel: `.if X=0` blocks rmac *includes* were being *excluded*. The concrete
  casualty was gpu_geotex's Blitter launch (`store → B_CMD`, inside `.if
  NOFILL=0`) — jas dropped it, so the kernel set up every span's blitter
  registers but never fired a blit and rendered nothing in the simulator.
  *(commit 9fda058)*
- **Hazards are attributed to true source lines across preprocessing.** The
  hazard pass numbered from the *expanded* text, so any `.if`/`.include`/`.macro`
  that added or removed lines shifted every reported location. A per-output-line
  source map now threads through the diagnostics, so the reported line,
  register, and producer point at the original source. *(commit 589d641)*
- **JRISC indexed store operand fields corrected** (offset = reg1, data = reg2),
  matching hardware and rmac. *(commit 9476039)*

### jsim — the cycle-honest simulator

- **STOREP/LOADP phrase long order corrected** (big-endian: the high long,
  G_HIDATA, belongs at the *lower* address; jsim had both ops swapped). A silent
  bug — phrase transactions round-tripped fine in jsim but ran transposed on
  silicon (hardware-confirmed on Skunkboard: gpu_geotex vertices came out with
  x/y swapped). Unblocks verifying `storep`/`loadp` bus-traffic optimizations in
  jsim. *(commit f5197f3)*
- **Blitter now charges its DRAM-bus time** (was free/synchronous). Calibrated on
  the Skunkboard (new probes p_blitsm/p_blitbg): 16 launch ticks + 5.6 ticks per
  phrase access; jsim now predicts the measured 30 / 450 ticks for an 8 / 256-px
  SRCEN span. *(commit 674767d)*

  **End-to-end effect (OpenLara Caves 320×240, HWH_clean, 2600 frames):**

  | | fps | vs hardware |
  |---|---|---|
  | jsim before (blit cost zeroed) | 7.50 | +53% |
  | jsim after (calibrated) | **5.43** | **+11%** |
  | real Jaguar | 4.9 | — |

  Zeroing the constants reproduces the reported 7.50 exactly, so the movement is
  attributable to this fix alone. The free Blitter — not bus contention — was the
  dominant missing term: it accounted for ~2.1 of the 2.6 fps gap.
- **LOADP's G_HIDATA lands late and unscoreboarded**, so an early read sees the
  stale value as silicon does (it previously landed instantly, making a kernel
  that renders garbage on hardware look correct in jsim). *(commit 6f1fd08)*
- **Blitter XADDINC is a real 16.16 affine DDA** (was approximated as a flat +1
  step), so textured spans sample the atlas correctly. *(commit 0d0a2fc)*
- **GPU/DSP restart on re-kick.** A core kicked after its boot self-test was
  being ignored; it now restarts. *(commit c102187)*
- **68k↔DSP `D_CMD` mailbox verified.** The resident-DSP command handshake
  (68k writes Jerry SRAM, the DSP poll loop dispatches and clears it) was an
  unexercised path. Confirmed correct — shared bus, no per-core cache; a
  stopped 68k still advances time so the scheduler keeps servicing the DSP —
  and locked in with regression tests covering Jerry SRAM, DRAM, and the
  sleep-in-STOP case. No fix needed. *(commit 2f425be)*

### jopt — the scheduler that can't ship a wrong answer

- **Certify against a fixture (`--fixture <file>`).** A render kernel never halts
  in isolation — with zeroed memory gpu_geotex loops on garbage, so the
  equivalence certificate compared a meaningless budget-cutoff snapshot and
  rejected every live-code fill. A fixture supplies the kernel's real input state
  (param block, geometry, camera, atlas, a framebuffer to capture) so the run
  completes and the certificate compares actual rendered output. Directives:
  `budget`, `capture`, `long <addr> <value>`, `blob <addr> <file>`. On gpu_geotex
  this accepts 4 delay-slot fills in the live span/edge-walk code — each proven
  to leave the rendered image byte-identical — where isolation accepted none.
  *(commit 3a74aed)*
- **v2 delay-slot scheduler.** v1 could only try the instruction immediately
  before a jump (almost always the compare a conditional jump consumes). v2 walks
  the straight-line block backward for any donor it can legally sink into the
  slot — gated by three sound preconditions (dominance, data-independence,
  flag-safety) before the jsim certificate — decoded with the simulator's own
  `timing::classify`. *(commit 65d281e)*
- **Inactive-block awareness.** jopt reasons over the assembled instruction
  stream, so instructions inside a disabled `.if` block are never candidates;
  such wasted slots are reported as `skipped-inactive` instead of the old
  phantom "accepted / 0 saved". *(commit 65d281e)*
- **`--allow-input-hazards`** lets jopt optimize inputs that already contain
  benign, hardware-correct hazards; the jsim equivalence certificate still gates
  every output. *(commit fd9c761)*

### jtest — verification as a product

- **`run_with(spec, presets)`** applies `(addr, bytes)` memory presets before a
  run — the fixture primitive jopt builds on; a kernel can be run against the
  input state it expects instead of zeroed memory. `run` is unchanged.
  *(commit 3a74aed)*

### jcc68k — the C compiler

- Register-resident evaluation stack, cheap-operand folding, and hot-local
  register allocation; ELF symbol + GCC asm-label conventions so real OpenLara
  translation units link. *(commits e1a8e33, 34528eb, e9dc6da, d8499b9)*

### Known gaps (planned — not in this release)

- **Frame-time prediction is optimistic (~+35%) for bus-bound scenes.** jsim
  renders correctly and its per-core issue/stall model is hardware-calibrated,
  but three things are unmodeled, so predicted fps runs high (measured ~7.5 vs
  ~4.9 on hardware for OpenLara Caves 320×240). Diagnosed in
  COBWEB_GAP_bus_contention_and_blitter_fill_timing; each needs a
  hardware-calibration pass, so they are staged deliberately rather than guessed:
  **Status: mostly closed — fps over-prediction is down from +53% to +11%.**
  1. ~~**Synchronous/free Blitter.**~~ **DONE** (674767d) — this was the dominant
     term, not contention.
  3. ~~**LOADP/G_HIDATA zero-latency.**~~ **DONE** (6f1fd08).
  2. **Multi-master DRAM contention — partly answered by measurement.** Jerry is
     NOT a contributor: a resident DSP hammering DRAM did not measurably slow
     Tom (656 vs 656 ticks, twice, DSP provably running), so the GPU appears to
     outrank the DSP in bus arbitration. The remaining ~11% residual is the only
     budget left for an OP-scan-out term, so a full arbitration model is likely
     over-engineering; measure the OP first and size it against that residual.
     The original text follows for reference. The one shared 64-bit DRAM bus
     (68k + Tom + Jerry + Blitter + OP scan-out) is modeled only for the
     68k↔GPU pair (`CONTENTION_HIT_EXTRA`, gated on `bus.m68k_on_bus`) — and that
     gate is *off* during render because `gpu_sync` STOPs the 68k. Plan: extend
     arbitration to Tom↔Jerry↔Blitter↔OP and surface it in the existing
     `contention`/`mem_external` counters. This is the dominant term.
  3. **`LOADP`/`G_HIDATA` load-use latency.** The `r2` result is scoreboarded but
     `G_HIDATA` is written immediately (zero-latency), so reading it too soon
     succeeds in jsim while silicon returns stale data. Plan: land `hidata` on
     the load's `ready_at` via the existing slow-value machinery.

### Notes

- 149 workspace tests pass, including regressions for each fix above.
- Differential validation continues to hold jcc68k ≡ gcc -O2 on the shared
  corpus.
