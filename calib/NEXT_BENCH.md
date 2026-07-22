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

