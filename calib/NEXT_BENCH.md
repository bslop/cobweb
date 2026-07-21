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
