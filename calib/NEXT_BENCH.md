# Next bench session — one flash cycle settles every open constant

Everything below is prepared and dogfooded in jsim; the rig work is
mechanical. Order matters (rig health first, cheapest highest-value next).

## 0. Rig health gate (before believing anything)

Physical reseat + cool-down (the 2026-07-20 fault: same session went clean
11:50 → streaked 15:23 with no reflash — console-side, per the jagtap
frames). Then flash the golden `TC_final.cof`:

- must render clean (jagtap live view, or `framecheck.py` against a jsim
  screenshot of the same build) and read **3.75 fps**.
- Until it does, no verdict below counts.

## 1. Calibration suite (one flash, ~2 min)

`calib/build/calib_skunk.cof` — now includes two NEW probes at the end of
the blit block:

| probe | question it settles | jsim baseline |
|---|---|---|
| `blitrmw` | DSTEN dest-READ price (rect-shade's RMW). jsim: 453 vs blitbg 451. **Silicon below blitbg ⇒ RMW reads coalesce ⇒ DSTEN charge comes down**; at parity the current charge stands. | 453 |
| `ldunderb` | GPU DRAM loads WHILE a 2048-px blit holds the bus. jsim: 3487 (≈ no contention). **Silicon minus 3487 = the staging-under-blit contention coefficient**, the prime suspect for the +18% rect-shade optimism. | 3487 |

Also re-anchors the whole existing table for free. Read results as always:
`python3 calib/parse_results.py` on the skunk console dump.

## 2. Top-phrase + UPDA probe (one flash, 10 seconds)

`calib/p_topphrase_upda.s` (assemble line in its header). Verdicts in DRAM:

- `$100 = $600D0001` → $F03FF8-FFF stable → the jas top-phrase lint can be
  retired with this log as provenance; OpenLara gets its last 8 bytes back.
- `$108 = $600D0002` → UPDA2 steps the DSTA2-swapped dest (jsim semantics
  confirmed on silicon); `$BAD00002` → jagemu has an emulation gap that
  silently corrupts DRAM — file it, model it.

## 3. OpenLara arms (their §6 plan + one re-take)

- RECTSHADE_v3 on the healthy rig (their plan; expect ~5+ from 3.75).
- Re-take the NOFILL pair with the CURRENT flags on Jaguar B — and note
  the frameview finding: **LAD_nofill is a launch isolator, not blit-free**
  (22.8%-of-wall busy remains: the big clears). Don't read its delta as
  "the Blitter".

## What the sim side already pinned (no rig needed, 2026-07-21)

- ALLCULL floor: jsim 9.57 vs silicon 9.55 — the empty-frame floor is
  modeled right; ALL remaining optimism lives in the geometry path.
- The floor is the cull walk, not Lara: 40% of wall of GPU time survives
  with every face culled (each face still staged+transformed+cull-tested).
  Hierarchical/room-level early-out and LOD attack this directly.
- Jerry runs ~93.5% of wall in BOTH builds; jsim charges his DRAM traffic
  ~nothing (the known Tom↔Jerry contention GAP). `ldunderb` plus a future
  DSP-side twin is the measurement path.

---

## SESSION RESULTS (2026-07-21, Jaguar B, remote-driven)

Health gate: PASSED — golden renders clean, bar decodes ~3.5 (capture-side
decode tolerance vs the 3.75 cert; the fault regime read 5.29 WITH streaks).

| probe | silicon | jsim before | verdict |
|---|---|---|---|
| blitrmw | **216** | 453 | **DSTEN RMW = ONE access/px** — the read rides the write's page window. Charge removed for non-SRCEN DSTEN (kept for the unprobed SRCEN+DSTEN shape). jsim now 235 (+8.8%). |
| ldunderb | **3600** | 3487 | staging-under-blit contention **refuted** (+3.2% only). |
| lddramc A | **8.83 cyc/u** | 8.00 | consumed DRAM loads with the 68k ACTIVE are +10% under-charged — one measured piece of the regime gap. |

Post-recalibration anchor ladder: ALLCULL **9.57 vs 9.55** (floor exact);
v4b 5.16 / nofill 5.82 / TC 4.89 vs silicon 3.89 / 4.51 / 3.75 — a uniform
**+30% on every geometry build**. With per-blit and floor both silicon-exact,
the whole residual is the disclosed 68k/bus regime nonlinearity — plus one
NEW named suspect: **the bwait B_CMD poll itself is unprobed** (a Tom
REGISTER read from the GPU, possibly priced differently under a busy
Blitter; the shaded build adds thousands of poll spins per frame, and jsim
thinks shade is ~free while silicon pays 23%).

## hwq verdicts (same day, post power-cycle — calib/hwq_20260721.log)

- **TOPPHR GOOD**: $F03FF8-FFF writable and stable on silicon. The jas
  top-phrase lint is retired with this log as provenance; the last 8
  bytes of GPU SRAM are usable.
- **UPDA2 GOOD**: under DSTA2, UPDA2 steps the swapped destination with
  per-row re-home — jagemu's model confirmed on silicon; UPDA2-only
  blits were never a corruption risk.
- XJUMP/CTRL re-confirmed GOOD on this rig.

## Remaining for the next session
1. **Flash the updated suite** — `p_bcmdidle`/`p_bcmdbusy` (the bwait
   poll-cost probes, the +30% suspect) are WRITTEN, registered, and
   dogfooded in jsim (idle: 1.01 cyc/poll — jsim prices B_CMD reads as
   ~free; busy: blit-duration-bound). Blocked 2026-07-21 by a
   physical-layer USB fault: every upload >~16KB drops at a consistent
   offset ("can't connect"), small uploads (hwq 3KB) pass, and even
   successful large uploads crawled at 7KB/s. Software USB reset did
   not help. Suspects: cable/port/hub, or the same session-length
   thermal degradation as the 07-20 streak fault (cart-edge side).
   Swap the USB path and/or let the rig cool, then:
   `script -qefc "jcp -c build/calib_skunk.cof" bench.log`
   **2026-07-21 update: console swapped — SAME drop at the same ~16KB
   offset on the second Jaguar/Skunkboard. The fault is the USB path
   (cable/port/hub), not either console.** Small transfers (3KB) pass;
   sustained bulk fails — classic marginal cable. Swap the cable or
   move to a direct motherboard port, then re-run.
2. The density-sweep family for the mode-A DRAM regime model.

---

## SESSION 2 RESULTS (2026-07-21 evening, second console, USB recovered by bounce)

Full suite ran (`bench_20260721_s2.log`). The bwait suspect: **partially
confirmed.** `bcmdidle` silicon 2.02 cyc/poll vs jsim 1.01 — a GPU read of
a Blitter register costs one extra bus cycle. Charged ($F02200-7F reads,
+1 occ; bcmdidle now 2.02 exact). `bcmdbusy` 3494 vs 3487 ✓ (blit-bound).
blitrmw/ldunderb reproduced session 1 on the second console (216/3599) —
cross-rig stability.

Anchor ladder after the charge: v4b +28.0%, nofill +27.7%, TC +28.8%,
ALLCULL +0.2%. The poll charge closed ~2pp; the residual is UNIFORM across
geometry builds with the floor exact — consistent with the known mode-A
dense-stream DRAM regime (lddramc +10% is its measured edge; the disclosed
"dense streams need more data charge" nonlinearity). The density sweep
remains the measurement path. NOTE: the uniform bias cancels in A/B
comparisons, so jsim correctly RANKS optimizations on these kernels today;
only absolute fps on dense-geometry builds reads ~28% high.

---

## READY TO FLASH (2026-07-21 night — authored off-rig, dogfooded)

`p_dens2/6/14/30` — the density sweep: one DRAM load per 4/8/16/32
instructions, modes A+B. jsim baselines (cyc/instr): A 2.51/2.25/2.13/2.07,
B 1.51/1.76/1.87/1.94. Silicon-minus-these vs density pins the mode-A
bus-grant regime (the last +28%). Decision rule: fit the measured curve,
replace the flat contention constant with the density-aware form, then the
whole anchor ladder must re-validate in one pass (floor exact, geometry
builds within ~5%). One flash of `build/calib_skunk.cof`, any rig, any time.

---

## FIT RESULTS (2026-07-22) — regime model landed; the +28% is NOT density

Two-term density model implemented (issue-density gap, re-arbitration
window [5,10) +2 quiet / +6 contended, streaming <5 +4 contended,
consumed-load contended latency +8, OP-stretch excluded from gaps).
The ENTIRE probe table now fits silicon: dens2..30 A+B within 3%,
lddram/lddramc within 5%, lddramop 2.23 vs 2.29, all prior anchors
byte-stable. But the geometry ladder DID NOT MOVE (still +28.0/27.7/
28.8, floor exact): the game renders with the 68k STOPped, so the
contended terms never fire in-game. Density is refuted as the game
residual.

Remaining suspects, now sharply framed (everything else is exact):
1. **Consumed loads WHILE the Blitter runs** — the one unprobed
   combination (ldunderb was unconsumed; lddramc was blitter-idle).
   The geotex kernel does exactly this: staging consumes under async
   fill. Probe: p_ldcunderb (lddramc body + long blit in flight).
2. **Jerry's I2S/DSP DRAM traffic vs Tom** — dsp runs 93.5% of wall
   in-game; lddramj (parked hammer) showed nothing, but the REAL
   pose/audio access pattern may differ. Probe: dsp running the
   actual AUDIO_PUMP loop while Tom streams.

---

## 2026-07-22 SESSION 2: the micro-matrix is EXHAUSTED — all cells benign

`ldcunder` silicon: **A 4.96 / B 4.97** vs jsim baseline 9.18/6.51 —
consumed loads under a running blit are CHEAPER than idle-bus lddramc
(6.10), not costlier. (Mechanism worth a note: the streaming blit
appears to hold the bus grant/row in Tom's favor.) With this, every
micro-access shape is probed and none explains the +28%:
staging-alone exact · unconsumed-under-blit +3% · consumed-idle
modeled · consumed-under-blit CHEAPER · polls priced · launches priced
· DSTEN priced · density regime modeled · Jerry exonerated (ALLCULL).

The residual is STRUCTURAL — per-face-loop-shaped, invisible to
straight-line probes. Next discriminator (ALL SIM-SIDE): rebuild the
silicon flag ladder in jsim — HALFSPAN 4.13 / NODIV 3.98 / HALFRES
4.51 / SLITDISPLAY 4.00 (+ full 3.89, NOFILL 4.51, ALLCULL 9.55) —
and find which flag's DELTA diverges. That names the slice (span walk
vs div vs per-line vs OP) carrying the missing 56 ms/frame of
geometry-path time. Remaining unprobed micro-cell for completeness:
DRAM STORES under a running blit (vtxcache writes).

---

## 2026-07-22 FLAG-LADDER DECOMPOSITION (jsim vs silicon, byte-exact arms)

Δms saved vs full build (jsim / silicon):
- HALFSPAN: 16.6 / 14.9 — span walk modeled ✓ (slightly over)
- SLIT: 7.0 / 7.1 — OP scan-out tax EXACT ✓
- NOFILL: 27.2 / 35.3 — **fill+launch coupling under-modeled ~8 ms** →
  prime suspect: B_CMD STORE into a still-BUSY Blitter (rect-shade fires
  the shade blit while the span blit runs; jsim queues it free, silicon
  likely holds the writer). The one un-probed launch shape.
  → probe p_fireintobusy: launch 128px, immediately store a second
  launch, time to both-complete; vs sequential-with-bwait control.
- NODIV: 0.0 / 5.8 — **jsim thinks removing DIVs saves NOTHING; silicon
  saves 5.8 ms** → DIV cost under-modeled in the geotex shape (divhot/
  divsh matched — the in-kernel shape differs; suspect divider-busy
  chains or DIV×external interaction). → probe p_divext: DIV + staging
  load interleave, the kernel's actual pattern.
- ALLCULL: 96.3 / 152.4 — geometry adds 152 ms on silicon, 96 in jsim;
  after fill (8) + div (6), **~42 ms remains in the untoggled core**
  (per-face setup + staging + walk executed for DRAWN faces). Next
  discriminator after the two probes above.
(HALFRES arm: bar decode fails at that resolution — re-derive from
PROFILE bars next time.)

---

## 2026-07-22 SESSION 3: both structural suspects REFUTED — model exact again

- **fib: silicon 1.86/1.84 vs jsim 1.87 — fire-into-busy is QUEUED.**
  jsim's async Blitter model is confirmed at the launch-into-busy shape;
  rect-shade's overlap is real on silicon. The 8ms NOFILL-slice gap is
  NOT launch holding.
- **divext: silicon B 4.76 vs jsim 4.69** — integer DIV × consumed
  staging interleave matches. ldcunder reproduced (4.96/4.97, 2nd
  session). Full log: bench_20260722_s2.log.

NEW HYPOTHESIS for the NODIV 5.8ms: geotex divides run in **DIV_OFFSET
(16.16) mode**; every div probe so far (divhot/divsh/divext) used
integer mode, and jsim prices both at 18 cycles. → probe p_divoff:
divext body with DIV_OFFSET set. If 16.16 division is slower on
silicon, that is the slice.
NOFILL 8ms: launch-hold refuted; remaining candidates are second-order
(blitter↔GPU row interaction — though ldcunder argues against — or the
NOFILL arm's own workaround-path costs). Lower priority than the ~42ms
untoggled core, which is now the dominant unknown. Next discriminator
for the core: per-face-count scaling (ROOMCAP=N ladder in jsim vs
silicon — is the miss per-face-constant or per-pixel?).

---

## 2026-07-22 SESSION 4: ROOMCAP=1 on silicon — THE MISS SCALES WITH SCENE SIZE

ROOMCAP=1 (one nearest room drawn): silicon **~6.4 fps** by
tick-calibrated tap decode (same decode reads golden 7% low → true
6.4-6.9) vs jsim **6.00** — the +28% optimism is GONE at small scenes
(possibly inverted). Full build: silicon 3.89 vs jsim 4.98 (+28%).
divoff also refuted this session (4.79 ≈ integer 4.76).

VERDICT: the missing ~42+ms is PER-FACE/PER-ROOM-PROPORTIONAL, not
fixed — a cost in the drawn-face path that grows with geometry volume,
which every straight-line probe individually prices correctly. The
whole is costing more than the sum of its measured parts.

Next probe (the meso-probe): a synthetic mini-geotex in the calib
harness — a LOOP of {staging loads, DDA-ish dependent ALU, conditional
branches, blit launch} per fake face, N faces. If the loop reproduces
the per-face excess, bisect it by removing one ingredient at a time —
the first structure-level measurement after two days of exact
micro-probes. Candidate mechanisms the micro-probes cannot see:
branch/jump costs in loop context (jr probe was a tight spin, not a
branchy loop body), SRAM instruction-fetch interaction with data
accesses, pipeline refill patterns unique to mixed code.

---

## PRIORITY RESET (2026-07-23) — correctness before fps

Mission is bidirectional faithfulness (jsim ⇄ silicon), so a dev never
ships jsim-passing code that black-wedges on hardware. That makes the
OpenLara round 5-6 CORRECTNESS gaps the top priority, above the +28%
fps residual:

1. **p_divlat** (built, dogfooded, committed dc59c19) — smallest K with
   v=0x55 on silicon = true div readable-latency. ONE flash. Unblocks:
   recalibrate Lat::DIV (currently 18), then poison early div-dest reads
   under Silicon fidelity.
2. **load-consumed-across-taken-jump**: lift out of the BigPEmu-only gate
   (timing.rs read_stall) — silicon does it too (their black-wedge
   repro). Needs the erratum's true width (their prototype over-fired on
   every loop-back load) — a characterization probe: internal load,
   taken jump to consumer, sweep shadow length, check VALUE.
3. THEN the +28% concurrency residual (meso-probe).

Do NOT enable poisoning blind: it over-fires on correctly-scheduled
code (70K false positives, their round 6) — LESS faithful, not more.

---

## DIV CORRECTNESS: RESOLVED (2026-07-23) — refuted, no model change

p_divlat on silicon (both operands, K=0..15): the div dest reads the
CORRECT quotient at every K — small 0x55 AND large 0x2AAAAAA5 (late
significant bits). Silicon SCOREBOARDS the div destination (an
un-interlocked K=0 read would return the stale dividend 0xFF, not the
quotient). divhot confirms the ~16-cyc consume stall (silicon 6.68 vs
model 6.67). So jsim's read_stall/Lat::DIV=18 is already faithful;
OpenLara round-5 "garbage on early div read" is REFUTED. No div poison.

REMAINING correctness item: load-consumed-across-taken-jump (round 5.2).
Author p_ldjump: internal SRAM load, taken jump to the consumer, sweep
shadow length, check VALUE. If silicon corrupts (stale/garbage) where
jsim serves correct, that is the real erratum — and lift it out of the
BigPEmu-only gate. Then the +28% concurrency residual (meso-probe).

---

## LOAD-ACROSS-JUMP: REFUTED for jr (2026-07-23) — correctness ledger clears

p_ldjump on silicon: dram=ABCD1234 sram=5678DEF0 — both the DRAM load
(in flight ~15cyc, consumed ~5cyc later at the jr target) and the SRAM
load read their seeded truths. Silicon SCOREBOARDS across the taken jr;
jsim is faithful; round 5.2 refuted like round 5.1 (div).

BOTH headline correctness claims (rounds 5.1 div, 5.2 load-jump) are now
refuted on silicon. jsim's correctness model is faithful on every case
probed. The maintainer's real failures (0.14fps, black-wedge) are
misattributed — likely re-issued-DIV-while-busy (TRM bug 25) or a plain
bug-13 WAW, not scoreboard-drop.

Remaining (both LOW priority — pattern strongly favors "jsim is right"):
- absolute jump(rN) variant of load-across-jump (needs a runtime-address
  probe; jr used a relative jump).
- THE MAIN EVENT: the +28% concurrency residual (meso-probe). This is now
  the only substantial accuracy gap left — everything else fits silicon.

---

## HANDOFF TO OPENLARA (2026-07-23) — the +28% discriminator is ready to flash

The mechanism is found; one flash confirms it. The kernel's bwait is a
software-pipelined spin: SILICON is compute-bound (per-face compute >
blit, the spin exits free), jsim is spin-bound (compute too fast, spins
on the blit). So the +28% is jsim's per-face COMPUTE being ~28% too
FAST, which un-hides the blit — round-4's "concurrency overcharge" is
the symptom, not the cause.

**p_face** (committed) times that compute in isolation: 2 perspective
divides + a 16-px DDA span walk with per-pixel edge branches (jr, the
real mix, no blit/no sync). jsim baseline: **face B 3.26 cyc/instr,
face A 4.22**.

TO FLASH (after a Skunkboard reconnect — it dropped off USB here):
  cd calib && make            # rebuild calib_skunk.cof with p_face
  script -qefc "jcp -c build/calib_skunk.cof" bench.log
  # wait for CAL face A and B (mid-table, both modes), then CAL DONE
  python3 parse_results.py --console bench.log | grep -E "face|jr"

DECISION RULE:
  * face silicon B ~= 4.2 (>~28% over jsim's 3.26) -> the +28% lives in
    the per-face compute mix. BISECT: rebuild p_face variants removing
    (a) the per-pixel branches, (b) the divides, (c) the loads, one at a
    time; whichever removal collapses the jsim-vs-silicon gap names the
    culprit. Strongest prior: branch/jump refill in a real branchy loop
    (the isolated jr probe is a tight 2-instr spin; mixed code differs).
  * face silicon ~= jsim 3.26 -> the gap is NOT the compute mix; it is
    specifically the blit-spin interaction (jsim's blit_busy drain vs the
    poll-spin cadence). Then probe: launch + measured compute + bwait,
    varying compute length across the blit duration.

Everything else fits silicon. This is the last substantial accuracy gap.

---

## 2026-07-23 p_face RESULT — the +28% is NOT the compute; it's the blit-spin

Clean silicon flash (calibface, mode B): **face silicon 3.55 vs jsim 3.26
= +8.9%** (mode A even UNDER: 4.06 vs 4.22). The synthetic per-face
compute (2 divides + 16px DDA + per-pixel edge branches) is only ~9% too
fast in jsim — NOT the +28%. So the residual is dominated by the
BLIT-SPIN INTERACTION, exactly where round-4 pointed: jsim's compute
being only mildly fast still un-hides the blit because the kernel is
right at the compute/blit balance point, but the bulk of the +28% is in
how jsim's blit_busy-drain vs the poll-spin cadence accumulates across a
full frame of launches — not in the raw compute.

NEXT PROBE (the real one): p_ovlap — launch a 128px blit, do exactly
blit-duration-worth of GPU compute (no bwait), THEN bwait; vs p_serial —
launch, bwait immediately, then the same compute. On silicon the compute
hides the blit (ovlap ~= compute, serial ~= blit+compute); the delta =
blit hidden. If jsim's delta < silicon's, jsim under-credits the overlap
= the +28%. This isolates the concurrency accounting directly.

Staged but NOT silicon-confirmed (board dropped off USB mid-flash):
facenb (no branches) / facebr (3 br/px) branch-density bisection of the
9%. jsim facebr mode-B did not complete in the sim peek — VERIFY the
facebr probe before flashing (possible wedge; the 48 unrolled jr targets
need a check). face itself is proven.

---

## 2026-07-23 p_ovlap / p_serial — jsim DOES credit the overlap (dogfood, no board)

Built OVLAP_ONLY fast build; jsim (silicon fidelity):

    ovlap  (launch -> ~blit-worth of independent compute -> bwait)  118 ticks
    serial (launch -> bwait -> the same compute)                    228 ticks

serial - ovlap = 110 ticks of blit FULLY HIDDEN under compute. jsim's
async blit_busy-drain model credits the overlap correctly — ovlap is
~half of serial because the ~110-tick blit disappears under the compute.

=> The +28% is NOT "jsim serializes what silicon overlaps." The
concurrency accounting is structurally sound. What remains: does SILICON
hide MORE or LESS blit than jsim? Flash calibovlap_skunk and compare the
silicon ovlap/serial delta to jsim's 110. If silicon's delta > 110, jsim
hides too little (charges the peeking blit as bwait spin) = the
overcharge. If silicon's delta < 110, jsim hides too much. Either way
this probe pins the sign and size of the balance-point error directly —
it is THE flash to run next.

Also captured (jsim, wide-window peek --len 2048 — the earlier "facebr
wedge" was only the 1024-byte peek window truncating slots >=32, NOT a
real wedge; all six face-bisection numbers are valid):

    face   B 58305 cyc (1 edge branch/px)
    facenb B 49855 cyc (0 branches)
    facebr B 74360 cyc (3 branches/px)

Monotone in branch count as expected; needs silicon ratio to bisect the
~9% compute gap by branch density (secondary to the ovlap flash).

---

## 2026-07-23 p_ovlap/p_serial SILICON — EXACT. Validates the async-blitter fix.

Fresh-bounce flash, silicon (half-line ticks):

    ovlap  A 118  B 117     (jsim 118 / 118)
    serial A 227  B 227     (jsim 228 / 227)

Silicon MATCHES jsim to within 1 tick. Blit-hidden delta serial-ovlap =
109 silicon vs 110 jsim — identical. jsim's blit/compute overlap
accounting is silicon-EXACT.

WHAT THIS ACTUALLY CONFIRMS (corrected framing — I re-read OpenLara's
CURRENT reports after running this): the big fps gap (jsim 7.50 vs hw
4.9) is already CLOSED. The async-Blitter charge took it to 5.43 (+11%);
OP scan-out contention is modeled (+11.1%); Tom<->Jerry contention was
measured NULL (656 vs 656) and the submitter WITHDREW that report (their
42% counter was a PIT-readback artifact, since fixed); the residual
frame time was traced to a bytewise framebuffer memcpy on the 68000
(game-side, via the 68k PC histogram). So this ovlap/serial probe is NOT
hunting an open bug — it INDEPENDENTLY VALIDATES the async-blitter
concurrency model that closed the gap: the overlap the fix relies on is
silicon-exact. A clean confirmation to hand back, not a new defect.

(Earlier draft of this note speculated the residual was "frame-scale OP
contention" — that was wrong; OP contention is already modeled at
+11.1% and the residual was the 68k memcpy. Corrected here.)

Still possibly open (separate report, COBWEB_GAP_jerrypose_fps_
overprediction, 2026-07-20 @ af1c3f6): jsim +27% when pose work moves
68k->Jerry (jsim 6.00 vs silicon 4.72). Two-sided: 68k over-charged
(the 68k side was since re-anchored in 147eda8, whole-program within 1%)
and Jerry's marginal load invisible (resident-poll shows 98-100% busy
either way). The Jerry-undercharge half wants a check against current
HEAD before any new probe — do NOT model Tom<->Jerry DRAM contention
(measured null). See the ovlap face-bisection probes for branch density
if the compute path is ever re-opened.

Also captured (jsim, wide-window peek --len 2048): full face/facenb/
facebr branch-density bisection, monotone in branch count; needs a
silicon ratio only if the +9% compute path is re-opened (low priority).

---

## 2026-07-23 QUEUED TO FLASH — 3 OpenLara probe reqs (dogfooded, board down)

All authored + dogfooded in jsim, committed (5d3a0bc), waiting on a
Skunkboard bounce. Flash calibdl_skunk.cof (custom runners, console) for
the first two; calib_skunk.cof (full suite) for the timing pair.

p_mmult (THE Phase-0 gate). Console: CAL MMULT o0=.. o1=.. o2=.. ovf=..
m1=.. m2=..  jsim says: o0=20 o1=140 o2=C80 ovf=FFFE0000 m1=20 m2=20.
  - o0/o1/o2 = 32/320/3200 => matrix-in-SRAM . vector-in-regs, ROW-major
    (jsim's isa.rs is right, RISC_ISA.md §7.2's "bank-1 = matrix" wording
    is loose). o0=654 (0x28E) instead => column/transpose; then OpenLara
    lays the kernel out the other way.
  - ovf=FFFE0000 => signed s16 operands + full s32 result (safe for the
    3x32767x4096 accumulator range). Anything truncated to 16 bits => not.
  - m1==m2 (both 20) => MMULT RESETS the MAC per call (the 3-MMULT/vert
    plan is safe). m2==40 => it accumulates; kernel must RESMAC between.

p_ldjumprn. Console: CAL LDJUMPRN dram=.. sram=..  jsim: ABCD1234 /
5678DEF0 (scoreboards across jump(rN) exactly as across jr).
  - both truths => erratum fully refuted for the absolute-jump form too;
    the RUNBATCH silicon crash is bug-25/bug-13 (kernel side).
  - stale/garbage => the erratum IS real for jump(rN); jsim Silicon must
    then model an unsettled load whose consumer is reached via jump(rN),
    and it explains the wedge. OpenLara already has the or-settle fix.

p_mmultw / p_mmulta (timing). jsim: width-3 MMULT 4.04 cyc, +MTXA write
5.05 (control write ~1 cyc). Silicon delta mmulta-mmultw = the real
per-row MTXA re-point cost; if >> 1 cyc it changes the 3-MMULT/vert math.

---

## 2026-07-23 SILICON: p_ldjumprn CONFIRMED — jump(rN) scoreboards (erratum refuted)

Flashed calibdl_skunk, clean console:

    CAL LDJUMP   dram=ABCD1234 sram=5678DEF0
    CAL LDJUMPRN dram=ABCD1234 sram=5678DEF0

Real Tom serves BOTH truths across an absolute jump(rN) with a runtime
target — it scoreboards the in-flight load exactly as it does across jr.
**The load-across-jump erratum is fully refuted for the absolute-jump
form too.** => OpenLara's RUNBATCH silicon-only crash is NOT this; it's
bug-25 (DIV-while-busy) or bug-13 (WAW), kernel-side. jsim's Silicon
fidelity is faithful here — no jump(rN) value-corruption model needed.

p_mmult (Phase-0 gate) did NOT make it over USB: the finicky link dropped
right after LDJUMPRN, before the MMULT line printed (the whole suite was
computed on the Jaguar, but the console dropout ate the line). FIX for
next flash: reordered so p_mmult prints FIRST (right after DIVLAT), in the
healthy post-bounce window. calibdl_skunk.cof rebuilt and re-dogfooded
(o0/o1/o2=20/140/C80, ovf=FFFE0000, m1=m2=20). One clean flash captures it.

---

## 2026-07-23 SILICON: p_mmult WEDGES real Tom — the bank-0 rewrite did NOT cure it

Clean post-bounce flash of calibdl_skunk (USB healthy: 5.67s transfer, all
16 DIVLAT rows printed, beacon fired). Console:

    CAL DIVLAT k=0..0F  sm=00000055 lg=2AAAAAA5   (all 16, clean)
    CAL MMSTART
    L WEDGED: GPU stuck (bug 23 - no external GO clear). Power-cycle.
    <bus-held garbage — GPU holds the bus, console corrupts>

**MMSTART printed, MMULT never did, and the 68k-side force-stop
(G_CTRL=0, main.c:567) could NOT recover it — bug 23: the external GO
clear does not halt a wedged GPU. Board needs a physical power-cycle.**

The beacon did its whole job: this is unambiguously a GPU WEDGE, not a USB
drop (link was clean through DIVLAT + the beacon; jcp then lost the 68k
handshake because the wedged GPU holds the bus).

This is now the SECOND distinct MMULT formulation to wedge real Tom:
- REGPAGE-switch version (pre-91c48f8): DIVLAT printed, then hung on MMULT.
- bank-0 / moveta version (91c48f8, "stays in bank 0", meant to be the
  fix): STILL wedges at the same point.

So "run MMULT entirely from bank 0, populate the bank-1 vector via moveta"
did NOT cure the wedge. Both formulations run CLEAN in jsim and wedge on
silicon — a real bidirectional-faithfulness gap (jsim-passing code that
black-wedges on hardware, exactly the failure class the 2026-07-23 priority
reset put first). MMULT operand-layout / s16 / MAC semantics remain
UNCAPTURED; the Phase-0 gate for OpenLara's vertex-transform prize is still
open, now blocked by the wedge rather than by USB.

Next-session diagnosis (isolate what in the MMULT drive sequence wedges —
jsim can't reproduce it, so this must be bisected on silicon, each arm a
tiny probe that self-stops and prints a beacon before touching MMULT):
1. **A single MMULT, nothing else** — MTXC=width, MTXA set, one `mmult`,
   settle, self-stop. If THIS wedges, MMULT itself (not the moveta/vector
   setup or the multi-row loop) is the trigger.
2. **moveta-then-read WITHOUT mmult** — confirm the bank-1 population path
   is innocent (strong prior it is).
3. **Vary settle length after mmult** — the MAC may still be draining when
   the self-stop store to GCTRL executes; a self-stop into a busy MAC is a
   known wedge shape. Try a much longer drain + an explicit MAC-idle spin
   before the GCTRL write.
4. **MTXC width sweep (1,2,3)** — width-3 systolic pass may read past the
   3-word matrix / cross a bank boundary on silicon.
Each arm is one flash; the first that stays clean names the trigger.
Until then the board is WEDGED — power-cycle before ANY further flash.

### LADDER BUILT + DOGFOODED (2026-07-23) — blocked on USB connect

The bisection ladder above is IMPLEMENTED as one flash: four minimal arms
`p_mm_{nov,w1,w3,w3s}` (probes.s, after p_mmult) driven by a loop in main.c
that runs BEFORE the old wedging p_mmult and prints one line per arm:

    CAL MMBIS nov v0=000000A0 v1=00000000
    CAL MMBIS w1  v0=00000004 v1=00000000
    CAL MMBIS w3  v0=00000020 v1=00000000
    CAL MMBIS w3s v0=00000020 v1=00000000     <- or "CAL MMBIS w3s WEDGED"

Each arm takes its result slot from PRMRESULT (distinct per arm), self-stops
from bank 0, and writes magic last; main.c breaks the ladder on the first arm
whose magic never lands (= the wedge trigger; board then dead, bug 23).

jsim dogfood (calibdl_sim.cof, peek $105000): ALL FOUR clean —
nov=A0 / w1=4 / w3=20 / w3s=20, every magic C0DED04E. The wedge is
silicon-only, so jsim can only confirm the arms are well-formed; silicon is
the discriminator. Arm ORDER is simplest->wedging so one flash bisects up to
the first wedge and prints every clean arm before it.

Arms:
- nov: full setup (matrix, MTXC=3, MTXA, moveta vector) but NO mmult.
- w1 : one width-1 mmult, 32-nop MAC drain both before store and before GO=0.
- w3 : one width-3 mmult, same long drains.
- w3s: one width-3 mmult, MINIMAL drain then immediate GO=0 (the wedging shape).
Decode rule as in the plan above (w3s-only wedge => drain the MAC before
clearing GO; w3 wedge => width-3 systolic; w1 wedge => mmult+self-stop pair).

TO FLASH (calibdl_skunk.cof — MMBIS prints first, right after DIVLAT):
    cd calib && make build/calibdl_skunk.cof
    script -qefc "jcp -c build/calibdl_skunk.cof" bench_mmbis_20260723.log

STATUS 2026-07-23 night: board power-cycled, but the Skunkboard USB link is
back in its marginal state — "can't connect with skunkboard" on 4 straight
attempts (one flash DID connect earlier tonight → the wedge finding above).
This is the documented cable/port fault, NOT the probe and NOT the console.
Needs a physical USB bounce (reseat/swap cable, different port/hub) before
the ladder can go over. Everything software-side is ready; it is one clean
connect away from the answer.

### ROUND 1 SILICON RESULT (2026-07-23 night) — TWO surprises

The 4-arm ladder flashed clean (board off→on caught a good USB window).
Console (log later overwritten by failed reconnects; values transcribed here):

    CAL MMBIS nov v0=000000A0 v1=00000000     <- setup path OK (sentinel)
    CAL MMBIS w1  v0=00000000 v1=00000000     <- expected 4
    CAL MMBIS w3  v0=00000000 v1=00000000     <- expected 20
    CAL MMBIS w3s v0=00000000 v1=00000000     <- expected 20

1. **NO arm WEDGED.** All four self-stopped (each printed a value line, none
   "WEDGED"). A single MMULT + self-stop does NOT wedge silicon — not at
   width 1 or 3, not at 32-nop or 2-nop drain. => the full-p_mmult wedge is a
   **MULTI-mmult effect** (back-to-back MAC pair w/ no settle, or the 4-row
   repeat), NOT a single op and NOT self-stop-into-busy-MAC. The whole
   "drain the MAC" hypothesis is REFUTED; drain length made no difference.

2. **Every mmult arm returned v0=0** (jsim gives 4/20/20; nov's non-mmult
   sentinel A0 came back correct, so store/slot/self-stop all work). w3
   (32-nop) and w3s (2-nop) are identical zeros => NOT a result-not-retired
   race; silicon MMULT is genuinely reading ZERO operands in this setup.
   This is a real jsim<->silicon MMULT faithfulness gap — the "vertices
   transform to all-zero garbage" failure class OpenLara would hit. It also
   moots the row-vs-column layout question until the zero is explained.

### ROUND 2 — WHY-ZERO bisection (built + dogfooded, waiting on USB)

Ladder extended to 7 arms (nov,w1,w3,w3s + mmhi,mmlo,mrd). New arms, primary
hypothesis = silicon reads each matrix element from the LOW 16 bits of its
SRAM word while jsim/mmult_ref use the HIGH 16 (stride-4 high-word):
  - mmhi: matrix in HIGH 16, Rd preseeded $0000DEAD before mmult.
          silicon 0 = mmult wrote zero; DEAD = mmult left Rd untouched
          (result lands elsewhere); 20 = correct.
  - mmlo: matrix in LOW 16, same preseed. **20 here => silicon reads the
          low half (the answer; jsim's bus.read16 high-half is the bug).**
  - mrd : store $00010000 to $F03A00, load it straight back (SRAM sanity —
          rules out "the matrix store never landed").
jsim dogfood (peek): nov A0 / w1 4 / w3 20 / w3s 20 / mmhi 20 / mmlo 0 /
mrd 00010000 — all self-stop. mmlo=0 in jsim is deliberate (it's the
silicon discriminator). Old wedging p_mmult now guarded behind
RUN_OLD_MMULT (off) so the session ends cleanly (ldjump/DONE) instead of on
a bus-held wedge. calibdl_skunk.cof rebuilt.

TO FLASH (once USB reconnects — needs a physical cable/port bounce):
    cd calib && make build/calibdl_skunk.cof
    script -qefc "jcp -c build/calibdl_skunk.cof" bench_mmbis_20260723.log
    grep -aE "CAL MMBIS" bench_mmbis_20260723.log
DECODE: mmlo=20 & mmhi=0 => low-half matrix read (fix jsim isa.rs mmult to
read the low 16, re-anchor, tell OpenLara to pack matrices low). mmhi=DEAD
=> mmult doesn't write Rd on silicon (result in MAC/elsewhere — chase that).
both mmhi/mmlo=0 & mrd ok => not the matrix half; the ZERO is vector-side
(moveta/bank-1 sourcing) — author a vector-readback (movefa) round next.
Then, SEPARATELY, the multi-mmult WEDGE still needs its own probe: two
back-to-back mmults (the mac1/mac2 pair) + self-stop, and a 4-row loop.

### ROUND 2 SILICON RESULT (2026-07-23 night) — SOLVED + jsim FIXED

Flashed clean after the USB bounce (Skunkboard red screen = ready):

    CAL MMBIS nov  v0=000000A0     setup OK
    CAL MMBIS w1   v0=00000000
    CAL MMBIS w3   v0=00000000
    CAL MMBIS w3s  v0=00000000     round 1 reproduced
    CAL MMBIS mmhi v0=00000000     matrix HIGH half, Rd preseeded $DEAD -> WROTE 0 (not inert)
    CAL MMBIS mmlo v0=00000020     matrix LOW half -> 32.  *** the answer ***
    CAL MMBIS mrd  v0=00010000     SRAM store/readback fine

**SILICON MMULT READS THE MATRIX OPERAND FROM THE LOW 16 BITS of each
stride-4 local-RAM word; jsim read the HIGH 16.** Proof chain: mrd => the
store lands; mmhi (value in high half) => mmult reads low=0, result 0, and
it actively WROTE 0 (the $DEAD preseed was overwritten — mmult is not inert,
Rd really holds the result); mmlo (value in low half) => reads 1,2,3 -> 32.
mmlo=32 also needs the bank-1 vector [4,5,6] (moveta) read correctly, so it
simultaneously CONFIRMS: vector = bank-1 packed 2xs16 via moveta (jsim
right); layout = ROW-major (row0.V=32, not the column 654); and a single
width-3 mmult does NOT wedge once the matrix is where silicon looks. Stride
is 4 (one element/word), NOT packed-2 (that would give [0,1,0]->5, not 32).
Consistent with the ISA doc: MMULT = internal IMULTN;IMACN*k;RESMAC, and
§7.1 says the MAC datapath takes the LOW 16 bits of each operand — jsim
contradicted its own spec.

jsim FIX (committed to isa.rs::mmult): `read16(addr)` -> `read16(addr + 2)`
(big-endian low half). Re-dogfooded the SAME calibdl_sim.cof: jsim now
byte-matches silicon on ALL SEVEN arms (nov A0 / w1 w3 w3s mmhi 0 / mmlo 20 /
mrd 00010000). All 42 jag-core tests still pass. The MMULT operand-read
faithfulness gap is CLOSED.

### PHASE-0 GATE — what's settled vs still open

SETTLED for OpenLara's vertex transform:
- Matrix operand: one element per 32-bit local-RAM word (stride 4), value in
  the **LOW 16 bits** (sign-extended s16). Pack matrices low, not high.
- Vector operand: bank-1 registers, two s16 packed per reg (low element
  first), populated via moveta from bank 0 (no REGPAGE switch needed).
- Layout: ROW-major (MTXC MADDW=0). MTXA = byte offset into local RAM.
- A single width-3 MMULT is correct and does not wedge.

STILL OPEN:
1. **The multi-MMULT WEDGE** — the full 4-row + back-to-back mac1/mac2 probe
   wedges real Tom (bug 23). Round 1 proved a single mmult+self-stop is fine,
   so the trigger is the mac-pair (two mmults, no settle between) or the row
   loop. Next probe: p_mm2 = two back-to-back width-3 mmults (matrix LOW
   half now) + self-stop; and p_mmrow = the 4-row loop. Bisects the wedge.
2. **s16/s32 result width (ovf)** — round 2 used small positives; the
   -32768*4 = FFFE0000 overflow arm was not re-run with the low-half matrix.
   Add p_mm_ovf (ovf row in the low half) next flash.
3. **MAC reset-vs-accumulate (m1/m2)** — untested on silicon; needs the
   mac-pair, which is also suspect #1. One probe can answer both: two mmults,
   read both results — if it does NOT wedge, compare m1 vs m2 (equal=reset).

### ROUND 3 STAGED (2026-07-23 night) — built + dogfooded, waiting on a bounce

Ladder extended to 10 arms; 3 new (all LOW-half matrix now), ordered
safe->risky so a multi-mmult wedge can't rob the single-mmult data:
  - mmovf : one width-3 mmult, ovf row [-32768,0,0] -> v0 = FFFE0000
            (s16 operand + s32 result). Truncated => not full s32.
  - mm2   : TWO back-to-back width-3 mmults (mac pair). v0=m1 v1=m2. Self-stop
            + m1==m2==20 => MMULT resets MAC/call (3-per-vertex plan safe);
            m2==40 => accumulates. WEDGE => the pair is the wedge trigger.
  - mmrow : FOUR width-3 mmults w/ per-row MTXA re-point (real kernel shape).
            v0=o0=20, v1=o3/ovf=FFFE0000. Self-stop => the 4-row loop is fine
            (wedge was tied to the old high-half setup); WEDGE => row loop is it.
jsim dogfood (peek): mmovf FFFE0000 / mm2 20,20 / mmrow 20,FFFE0000 — all
self-stop. calibdl_skunk.cof rebuilt.

### ROUND 3 SILICON RESULT (2026-07-23 night) — s32 confirmed; the WEDGE is found

Flashed clean (connected on retry). Arms nov..mmovf all printed and matched
jsim; then mm2 hung the console (clean tail, no output — bus-held hard wedge,
68k guard stalls on the read and never prints). Killed after ~4.5 min.

    CAL MMBIS nov  v0=000000A0
    CAL MMBIS w1/w3/w3s/mmhi  v0=00000000
    CAL MMBIS mmlo v0=00000020
    CAL MMBIS mrd  v0=00010000
    CAL MMBIS mmovf v0=FFFE0000        <- s16 operand + FULL s32 result CONFIRMED
    CAL MMBIS mm2  (no line — hard wedge on the back-to-back pair)

- **mmovf = FFFE0000**: MMULT sign-extends s16 operands and produces a full
  s32 result (-32768*4 = -131072). Safe for OpenLara's accumulator range.
  Open item CLOSED.
- **THE MULTI-MMULT WEDGE IS TWO ADJACENT MMULTS.** mm2 = `mmult r2,r6;
  mmult r2,r7` with ZERO instructions between -> hard-wedges real Tom (bug
  23, bus held). Round 1 already proved a single mmult+self-stop never
  wedges at any drain. So the original p_mmult wedged at its mac1/mac2 pair
  (identical shape). NAMED. jsim runs adjacent mmults fine = a faithfulness
  gap: jas should lint adjacent MMULTs and/or jsim-Silicon should model the
  wedge. (mm2's reset-vs-accumulate values were lost to the wedge.)

### ROUND 4 STAGED — settle threshold + reset/accumulate (built + dogfooded)

11-arm ladder, reordered so the known wedger mm2 (0-gap) runs LAST; new
mm2s (8-nop gap between the pair) and mmrow (~13-instr gap) run BEFORE it
(settle DESCENDING). One flash bisects the settle threshold:
  - mmrow  self-stop => a ~13-instr gap is safe (full 4-row kernel works);
           v0=o0=20, v1=o3=FFFE0000 (layout holds through the loop).
  - mm2s   self-stop => 8 nops already avoids the wedge; v0=m1 v1=m2 finally
           answer MAC reset(20/20) vs accumulate(20/40). WEDGE => 8 too few.
  - mm2    hard-wedges again (reconfirm; ends the session — expect a hang
           after mm2s, ctrl-C then).
Outcome => OpenLara/jas rule: never emit adjacent MMULTs, separate by >=
(threshold) instructions; and the reset/accumulate answer sets the
3-MMULT/vertex plan. jsim dogfood: mmrow 20/FFFE0000, mm2s 20/20, mm2 20/20
(sim doesn't wedge). calibdl_skunk.cof rebuilt.

BOARD IS WEDGED (mm2 bug-23). **Power-cycle the Jaguar** before the next
flash, then:
    script -qefc "jcp -c build/calibdl_skunk.cof" bench_mmbis_20260723.log
    grep -a "CAL MMBIS" bench_mmbis_20260723.log
    # expect through mm2s, then a hang at mm2 — ctrl-C

