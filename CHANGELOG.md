# Cobweb — Changelog

All notable changes to the Cobweb JRISC/68k toolchain (jas, jsim, jopt, jtest,
jcc68k, jln). Newest first. Dates are when the work landed; version tags are
assigned at release.

## Unreleased

Headline: the simulator now renders OpenLara's textured 3D room, the optimizer
proves its wins against real rendered output, and a one-character assembler bug
that had been silently dropping conditional blocks is fixed.

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
