# Cobweb — Changelog

All notable changes to the Cobweb JRISC/68k toolchain (jas, jsim, jopt, jtest,
jcc68k, jln). Newest first. Dates are when the work landed; version tags are
assigned at release.

## Unreleased

### 2026-07-27 — silicon: JRISC DIV truncates (jsim faithful)

- **`calib` `p_divround`**, authored and flashed same day. Six cases where
  truncate and round disagree, plus three exact controls, in BOTH integer and
  `DIV_OFFSET` 16.16 mode. **Silicon truncates in every one** (7/2=3, 5/2=2,
  8/3=2, 1/2=0, FFFFFFFF/2=7FFFFFFF, 2/3 16.16=0000AAAA); all controls agree.
  jsim's `d / s` is bit-faithful — no change needed.
- Closes ask 3 of `COBWEB_BUG_jagemu_runs_code_that_hangs_silicon.md`. That
  report is now fully answered bar silicon's divide-by-zero behaviour, which
  stays counted-not-modelled until someone measures it.
- **Refutes the rounding hypothesis behind OpenLara's A1** (Lara's head loses
  faces on silicon, none in jagemu). The two dividers agree bit for bit, so a
  rounding difference cannot be changing which sub-pixel faces survive the
  backface cull. Remaining candidates are hazards, not arithmetic: bug 25
  (DIV while the divider is busy) and bug 13 (WAW) — both counted, and now
  attributable to a PC via `--pc-histogram --core gpu`.

### 2026-07-27 — jsim: divide-by-zero counter + GPU/DSP liveness watchdog

Both from `COBWEB_BUG_jagemu_runs_code_that_hangs_silicon.md`, OpenLara's top
blocker: four kernel changes rendered fine in jagemu and black-screened a real
Jaguar in one session, each costing a 195-second flash plus a power-cycle.

- **`div_by_zero` counter** (`gpu.timing`/`dsp.timing`, plus an unconditional
  stderr warning). `isa.rs` answered `0xFFFFFFFF` on a zero divisor and carried
  on, which is exactly why the `KEEPDEGEN` case — dropping a degenerate-face
  cull, letting a zero-area face reach the edge walker with `dy = 0` — rendered
  a normal frame here and hung silicon. **Counted, not modelled**: the reporter
  observed silicon "hangs, or produces a value that makes the y-walk never
  terminate", and which of those it is has not been measured. Guessing would
  make jsim confidently wrong in a new way. The benign value is unchanged, so
  no timing or calibration constant moves.
- **`--watchdog N`**: warn when a core runs N consecutive frames without ever
  clearing RISCGO. Frame-anchored, not instruction-anchored — a resident kernel
  (OpenLara's DSP poll loop) legitimately runs forever, so "N million
  instructions without stopping" would fire every run and be ignored within a
  day. Opt-in, because only the caller knows whether its kernel is per-frame.
- Found immediately on a shipped probe ROM: `probes/RP_jpose.cof` executes
  **888 GPU divide-by-zeros in 120 frames**.

Also closes ask 4 of that report (a GPU PC histogram) — shipped earlier today.
Ask 3, divide ROUNDING vs silicon, still needs a bench.

### 2026-07-27 — jsim: CRY16 scan-out read the transposed chroma entry

- **`cry16_to_rgb` indexed the base table backwards.** The chroma index is the
  whole high byte used flat (`base[(px >> 8) & 0xFF]`); jsim computed
  `red*16 + cyan` and so read the TRANSPOSE. Every CRY16 screenshot showed
  plausible-but-wrong colours, which is why it survived: the transpose of a
  colour is another colour, and a cube rendered every face wrong still looks
  like a shaded cube. Silicon-adjudicated against a bubsy3d capture — four
  authored face colours now decode exactly (`1DDD`→(29,215,220),
  `ECF2`→(241,218,32), `7ECA`→(25,201,38), `E2E3`→(226,33,30)).
- **Intensity scale is `>> 8`, not `/ 255`** — at most one count, invisible on
  its own, and completely masked by the index bug.
- **The base table was never wrong.** It is byte-for-byte the authentic Atari
  `tga2cry` array (verified 256/256 against `crypal_tables.h`). The filed report
  (`COBWEB_BUG_cry16_decode.md`) named the table as prime suspect; it was the
  one part that was correct.
- **The old corner tests asserted the bug.** `$0FFF` was checked as pure red;
  `cry_base_rgb[0x0F]` is `(0,255,255)` — cyan. They were written from the same
  wrong convention as the decoder, so they confirmed it rather than catching
  it. Replaced with corrected corners plus the four interior silicon vectors —
  interior because a transposed table passes at the corners and fails
  everywhere a real renderer lives.

This unblocks using a jsim screenshot to judge any shaded-3D Jaguar project:
the Blitter's Gouraud path is CRY-only on silicon, so those projects all render
in CRY.

### 2026-07-27 — jsim: GPU/DSP PC histogram, and the stall counters stop double-counting

- **`--pc-histogram` now profiles Tom and Jerry, not just the 68000.**
  `jagemu run <rom> --pc-histogram --core 68k|gpu|dsp|all` gives exact per-PC
  cycle attribution for the JRISC cores, with the stall categories sliced per
  instruction: `stall_load`, `stall_alu`, `stall_div`, `stall_div_busy`,
  `stall_flags`, `jump_refill`, `fetch_external`, `mem_external`, `blit_wait`,
  `contention`. A core-wide `jump_refill` total says a kernel is refilling the
  pipe without saying which jump does it — chasing one meant reading a listing
  by hand (`COBWEB_REQ_68k_pc_histogram.md`). Exact, not sampled: a JRISC hot
  loop is often a handful of instructions inside a 4 KB SRAM window, and any
  sampling interval cheap enough to run is coarse enough to miss it.
  Zero cost when unarmed; ~9% when armed. Machine state is bit-identical with
  and without the profiler (asserted in-tree).
- **Symbolization for RISC kernels:** `jas --map <file>` emits `ADDR label`
  (labels only, not `equ` constants); `jagemu --gpu-map` / `--dsp-map` consume
  it, so a GPU profile names routines instead of raw addresses.
- **`--prof-json <file>` + `sim/tools/profdiff.py`:** the full per-PC profile
  for every profiled core, and a diff. This is what makes a *work move*
  priceable — a top-K table cannot answer "did moving this to the DSP help?",
  because the routine that appeared is rarely near the top and the routine that
  vanished leaves no row behind.
- **Counter fix: attributed stalls could exceed the core's own cycle count.**
  An instruction reading two in-flight registers stalls *once*, for the longer
  wait; the counters were adding *both*. A load-heavy kernel reported
  `stall_load` at 109% of its cycles. Only the binding operand is charged now.
  Cost was always the max, so **no modeled timing changes** — the fps model,
  every calibration constant, and all 46 jag-core tests are untouched.
- **`calib`: new `p_dsphammerw`, the write-side twin of `p_dsphammer`.**
  `lddramj` proved Jerry's DRAM *reads* do not slow Tom (656 vs 656), which is
  why jsim models no Tom↔Jerry arbitration. Writes were never probed, and
  writes are what a Jerry-side vertex transform actually produces. Retired from
  the default run like its sibling (Jerry saturating the shared bus has
  hard-wedged the console); enable deliberately.

### 2026-07-24 — jas: refuse two adjacent MMULTs (silicon hard-wedge)

- **`jas` now errors on two immediately-consecutive MMULTs.** They hang the
  GPU on real Tom (bug 23: bus held, only a power-cycle recovers) — the first
  MMULT's systolic MAC has not drained when the second issues. Silicon-proven
  (`calib` `p_mm_mm2` wedged at a zero-instruction gap; `p_mm_mm2s` ran clean
  with an 8-instruction gap). The fix-it points at a settle, and notes that
  MTXADDR auto-advances so a run of MMULTs with MTXA set once still walks the
  matrix. A NOP (or any instruction) between the pair clears the error. This
  is the assemble-time guard for the exact "passes in jsim, wedges on silicon"
  class jas exists to catch — a likely cause of OpenLara's RUNBATCH crash.

### 2026-07-24 — jsim MMULT: MTXADDR auto-advance modeled

- **`jsim`: MMULT now auto-advances MTXADDR by one row (`MWIDTH`×4) per call**,
  so a run of MMULTs with `MTXA` written once walks the matrix — the systolic
  array's intended use for a matrix×vector product. jsim previously left
  `MTXA` fixed and recomputed row 0 every call. Silicon-confirmed on a
  Skunkboard (`p_mm_mm2s`/`p_mm_mm3s`: `MTXA` set once → successive MMULTs
  read rows 0,1,2 = 32/320/3200); jsim now byte-matches, 42 jag-core tests
  pass. An explicit `MTXA` write between MMULTs still overrides (per-row
  re-point). By-column advance is inferred (`+4`), UNVERIFIED. Also recorded
  (silicon): MAC resets per MMULT, and **two adjacent MMULTs hard-wedge the
  GPU** (bug 23) — a settle is required between them. ISA §7.2 updated. This
  closes the MMULT Phase-0 gate for OpenLara's vertex transform.

## v0.1.0 — 2026-07-24

First tagged baseline: the JRISC/68k toolchain (jas, jsim, jopt, jtest,
jcc68k, jln) with the silicon-calibration suite. Anchors the "before" point
for the ongoing Skunkboard MMULT/hardware-fidelity work.

### 2026-07-24 — jsim MMULT read the wrong half of the matrix word

- **`jsim`: MMULT read the matrix operand from the HIGH 16 bits of each
  stride-4 local-RAM word; real Tom reads the LOW 16.** Fixed
  `isa.rs::mmult` (`read16(addr)` → `read16(addr + 2)`), matching its own
  ISA §7.1 (the MAC datapath takes the low 16 bits of each operand). Every
  matrix packed high-half transformed to all zeros on silicon while jsim
  computed the right answer — a silent divergence in the exact path
  OpenLara's vertex transform uses. Proven on a Skunkboard by a bisection
  ladder (`calib` `p_mm_*`): matrix in the high half → 0, low half → 32;
  jsim now byte-matches silicon on all arms, 42 jag-core tests still pass.
- **Silicon-validated MMULT semantics (for `jas`/docs and callers):** matrix
  = one element per 32-bit word (stride 4) in the **low 16 bits**, s16;
  vector = bank-1 regs, two s16 packed, via `moveta`; **row-major**;
  s16 operands → full **s32** result; and **two adjacent MMULTs hard-wedge
  the GPU** (bug 23) — a settle is required between them. (MTXADDR
  auto-advance is under silicon confirmation on `wip/mmult-mtxa-autoadvance`.)

### 2026-07-22 — hazard counters now name the address

- **`JSIM_HAZARD_TRACE=1` prints the PC of every `slot_movei`/`slot_jump`
  event to stderr.** A nonzero hazard counter means the code is wrong on
  real silicon, but a bare count is not actionable in a 1.4 MB image.
  First use localized Quake's 14 slot_movei events/900 frames to a single
  PC — the DSP kernel's entry point, executed with stale delay-slot state
  because the 68k halted the core mid-spin (a taken jump 2 of every 3
  cycles) before every relaunch. Env-gated, off by default, deterministic.

### 2026-07-22 — CRY16 scan-out byte order was swapped

- **`cry16_to_rgb` read intensity from the high byte; the TRM format is
  cyan[15:12] red[11:8] Y[7:0].** Every CRY16 title scanned out as chroma
  noise (the previous "verified against Cybermorph" was an eyeball of
  white credits text — white survives either byte order, so it proved
  nothing). Verified the corrected order against a CRY framebuffer that
  renders correctly on BigPEmu *and* real silicon (the Quake port's
  textured E1M1 scene), plus sanity anchors $0Fyy=red ramp, $F0yy=cyan,
  $88FF≈white; Cybermorph's credits fringe-free after. `tom/cry.rs`.

Headline: the simulator now renders OpenLara's textured 3D room, the optimizer
proves its wins against real rendered output, and a one-character assembler bug
that had been silently dropping conditional blocks is fixed.

### 2026-07-21 — jrom: every build becomes a cartridge

- **New tool `jrom`**: `jrom game.cof -o game.j64 [--rom game.rom]`
  packages any Jaguar executable (COF/ABS/JAG/raw — ingested through
  jag-core's own loader, so there is exactly one format authority) as a
  bootable cartridge: SubQMod's signed universal header (vendored, the
  block every homebrew toolchain ships — passes the real boot ROM's
  cart authentication) + a jas-assembled boot stub at $802000 that
  restores the RAM image and jumps to the entry. `.j64` (1MB-multiple)
  for MiSTer's Jaguar core / BigPEmu / Virtual Jaguar / flash carts;
  `.rom` for the Alpine convention. Validated end to end in jsim's
  cart-boot path: OpenLara's 1.5MB image as a 2MB .j64 boots to the
  rendered game, 175/76800 px off the COF run (copy-loop phase).
- **jas: PC-relative EAs were silently miscompiled** — `lea label(pc)`
  encoded the label's absolute address as the 16-bit displacement
  instead of target−(PC+2); same for d8(pc,Xn). Caught by jrom's
  boot-stub test running in jsim; fixed for both forms.

### 2026-07-21 — session 2: the bwait poll priced, the residual cornered

Second console (USB fault turned out to be the finnicky-Skunkboard kind —
a bounce cleared it; the cross-console test still isolated the earlier
drops to the USB path). Full suite re-run (`calib/bench_20260721_s2.log`):

- **`bcmdidle`: 2.02 cyc/poll on silicon vs 1.01 modeled** — a GPU read
  of a Blitter register pays one extra bus cycle. Charged for the
  $F022xx block (Internal-class accesses now route through ext_access);
  jsim reproduces 2.02 exactly. `bcmdbusy` matches (blit-bound, ✓).
- blitrmw/ldunderb reproduce session 1 byte-for-byte-close on the
  SECOND console — the calibration is rig-stable.
- Anchor ladder: the charge closed ~2pp; geometry builds sit at a
  UNIFORM +28% with the ALLCULL floor exact (+0.2%). The residual shape
  matches the disclosed mode-A dense-stream DRAM nonlinearity (lddramc
  +10% measured edge); density sweep is the remaining path. The uniform
  bias cancels in A/B, so jsim ranks optimizations correctly today —
  only absolute fps on dense-geometry builds reads high.

### 2026-07-21 — first remote-driven silicon session: both blit questions answered

Bench run end-to-end from the desk (jcp flash + reset, jagtap eyes,
capture-side fps decode): health gate passed (golden clean at ~3.5 by
bar decode vs 3.75 cert), full calib suite on Jaguar B
(`calib/bench_20260721.log`), and the two prepared probes answered:

- **`blitrmw`: silicon 216 vs modeled 453 — a DSTEN RMW pays ONE access
  per pixel, not two.** The dest read and write-back share the page
  window. Their round-2 suspect #1 was right: the 2026-07-20 DSTEN
  charge over-priced non-SRCEN RMW 2x. Recalibrated (charge kept only
  for the unprobed SRCEN+DSTEN shape); jsim blitrmw now 235 (+8.8%).
- **`ldunderb`: silicon 3600 vs jsim 3487 — staging-under-blit
  contention REFUTED** (+3.2%). The under-charge is not there.
- Post-recalibration ladder: ALLCULL floor exact (9.57/9.55); every
  geometry build uniformly ~+30% optimistic (v4b 5.16/3.89, nofill
  5.82/4.51, TC 4.89/3.75). With per-blit and floor silicon-exact, the
  residual is the disclosed 68k/bus regime nonlinearity — measured
  piece: consumed DRAM loads with the 68k active read 8.83 cyc/unit vs
  8.00 modeled (+10%) — plus a NEW named suspect: the bwait B_CMD
  register-read poll is unprobed and the shaded build adds thousands
  per frame (jsim prices shade ~free; silicon pays 23%). Next probes:
  `p_bwaitcost` + the density sweep. No constants touched beyond the
  bench-anchored DSTEN correction.
- hwq TOPPHR + UPDA2 flashed after a power-cycle
  (`calib/hwq_20260721.log`): **both GOOD on silicon** — the top phrase
  of GPU SRAM is stable (jas lint retired; OpenLara gets its last 8
  bytes back) and UPDA2 steps the DSTA2-swapped destination exactly as
  jagemu models (UPDA2-only blits were never a corruption risk).
  XJUMP/CTRL scoreboard verdicts re-confirmed on this rig.

### 2026-07-21 — the floor decomposed, and the bench pack is loaded

- **ALLCULL rebuilt and run in jsim: 9.57 fps vs 9.55 silicon (+0.2%).**
  The empty-frame floor is modeled essentially perfectly — all remaining
  rect-shade optimism lives in the geometry path. The floor itself is the
  CULL WALK, not Lara: ~40% of wall of GPU time survives with every face
  culled (staged + transformed + cull-tested per face). Jerry runs ~93.5%
  of wall in both builds while jsim charges his bus traffic ~nothing —
  the Tom↔Jerry contention GAP is now the prime suspect, with numbers.
- **Two new calibration probes, dogfooded** (`calib/probes.s` + parser):
  `blitrmw` (DSTEN dest-READ price; jsim 453 vs blitbg 451 — silicon
  below parity means RMW reads coalesce and the DSTEN charge comes down)
  and `ldunderb` (DRAM loads under a 2048-px blit; jsim 3487 ≈ zero
  contention — silicon minus that IS the staging-under-blit coefficient).
- **`calib/NEXT_BENCH.md`** — the one-flash-cycle checklist: rig health
  gate, calib suite, the top-phrase/UPDA probe, and the OpenLara arms,
  each with its decision rule written down before the measurement.

### 2026-07-21 — frameview: telemetry gets a face

- **`sim/tools/frameview.py`** — renders one or two `jagemu run` JSONs as
  a self-contained HTML frame-anatomy card (inline SVG, light+dark,
  hover tooltips, table view; no external requests). The GPU wall bar
  splits Execute / Jump refill / Scoreboard stalls / External access /
  **Blitter wait (paid)** with idle in de-emphasis gray, and draws the
  asynchronous Blitter **busy ledger** as its own labeled row — busy
  and paid cannot be conflated again by construction. Pair-diff mode
  annotates per-segment deltas; `--fps LABEL=JSIM[:SILICON]` adds the
  jsim-vs-silicon ladder. Palette validated for CVD/contrast in both
  modes.
- First run of the card caught a wrong claim in the round-2 blit
  response: LAD_nofill is NOT blit-free (22.8%-of-wall busy remains —
  44k big blits vs the full build's 1.28M launches), so the NOFILL
  subtraction isolates per-span launches, not the Blitter. Corrected
  in the BUG file.

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
