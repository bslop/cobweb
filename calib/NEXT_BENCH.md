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
