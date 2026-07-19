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

- **Blitter XADDINC is a real 16.16 affine DDA** (was approximated as a flat +1
  step), so textured spans sample the atlas correctly. *(commit 0d0a2fc)*
- **GPU/DSP restart on re-kick.** A core kicked after its boot self-test was
  being ignored; it now restarts. *(commit c102187)*

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

### Notes

- 145 workspace tests pass, including regressions for each fix above.
- Differential validation continues to hold jcc68k ≡ gcc -O2 on the shared
  corpus.
