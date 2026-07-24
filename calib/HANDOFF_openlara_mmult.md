# MMULT Phase-0 gate — silicon answers for OpenLara

Everything below is measured on real Tom (Skunkboard, 2026-07-23/24) and now
modeled faithfully in jsim. This closes `COBWEB_REQ_mmult_silicon_probe.md`.
Probe sources: `calib/probes.s` (`p_mm_*`); driver `calib/main.c`.

## The three original asks

| Ask | Verdict |
|---|---|
| **p_mmult** — operand layout / width / MAC semantics | **ANSWERED** (below) |
| **p_ldjumprn** — load-across-`jump(rN)` erratum | **REFUTED** — silicon scoreboards the in-flight load across an absolute `jump(rN)` (dram=ABCD1234, sram=5678DEF0). Not a value-corruption erratum; jsim is faithful. |
| **p_mmultw / p_mmulta** — per-MMULT timing | **PENDING one flash** (see Timing) — but auto-advance makes it moot for the re-point half (see below) |

## MMULT semantics (all silicon-validated, jsim now matches)

1. **Matrix operand** — in local RAM at `MTXADDR`, **one element per 32-bit
   word (stride 4)**, value in the **LOW 16 bits**, sign-extended (s16).
   *Pack matrices low, not high.* (jsim previously read the high half — fixed,
   `isa.rs::mmult`.) Proof: `p_mm_mmlo` low-half → 32, `p_mm_mmhi` high-half → 0.
2. **Vector operand** — bank-1 registers, **two s16 packed per reg** (low
   element first), populated via `moveta` from bank 0. No `REGPAGE` switch
   needed (the switch-in-bank-1 form wedged; the moveta form is clean).
3. **Layout** — **row-major** (`MTXC` `MADDW=0`). Row0·V = 32 (a column
   reading would give 654). `MTXA` = byte offset into local RAM.
4. **Result** — s16 operands → **full s32** result (`p_mm_mmovf`:
   −32768·4 = 0xFFFE0000). Safe for the accumulator range.
5. **MAC resets per MMULT** — back-to-back MMULTs do **not** accumulate onto
   each other (`p_mm_mm2s`: m2 = row1 alone = 320, not 32+320).
6. **`MTXADDR` AUTO-ADVANCES one row (`MWIDTH`×4) per MMULT.** Set `MTXA`
   **once**, issue `MWIDTH` MMULTs, and it walks the rows — a full
   matrix×vector with a single MTXA setup. (`p_mm_mm2s`/`p_mm_mm3s`: MTXA set
   once → rows 0,1,2 = 32/320/3200.) An explicit MTXA write between MMULTs
   overrides it (per-row re-point), but you no longer need that. by-column
   advance is inferred (`+4`), UNVERIFIED.
7. **⚠ NEVER emit two ADJACENT MMULTs** — they **hard-wedge the GPU** (bug 23:
   bus held, only a power-cycle recovers). The systolic MAC must drain first.
   `p_mm_mm2` (0 gap) wedged; `p_mm_mm2s` (8-instruction gap) ran clean. A
   settle of a few instructions between MMULTs is required.

## Kernel-adoption checklist (OpenLara side)

- [ ] Pack transform matrices with each element in the **low 16 bits** of a
      stride-4 word (was high-half → would read as all zeros on silicon).
- [ ] Build the matrix×vector as **set `MTXA` once → `MWIDTH` MMULTs**
      (auto-advance walks the rows); drop any per-row MTXA re-points.
- [ ] Ensure a **settle between consecutive MMULTs** (never adjacent). With
      auto-advance you have real work (the vector setup / result store) to
      fill the gap anyway.
- [ ] Keep the vector in **bank-1** via `moveta`; row-major matrix.

## RUNBATCH silicon-only crash — leading hypothesis

The earlier div / load-across-jump erratum theories were both **refuted** on
silicon. The **adjacent-MMULT hard-wedge (#7)** is now the prime suspect: a
naive 3×3 transform emits three MMULTs, and if they're back-to-back the GPU
wedges exactly as RUNBATCH does. **Check OpenLara's vertex kernel for
consecutive MMULTs** — that lives on your side. jas now **errors** on this at
assemble time (`rejects_adjacent_mmults`), so rebuilding the kernel through
jas will point at any offending pair.

## What shipped on the Cobweb side (already on `main`, v0.1.0-successor)

- `jsim` `isa.rs::mmult`: low-half matrix read + MTXADDR auto-advance. Byte-
  matches silicon on all `p_mm_*` arms; 42 jag-core tests pass.
- `jas`: hard error on two adjacent MMULTs, with a settle fix-it.
- ISA spec `RISC_ISA.md §7.2` documents all of the above.

## Timing (pending the final flash before the board is returned)

`p_mmultw` (per-MMULT throughput at MTXC=3) and `p_mmulta` (MMULT + a per-call
MTXA write). jsim baseline: mmultw ≈ 4.04 cyc/MMULT, mmulta ≈ 5.05 (control
write ~1 cyc). Silicon numbers: **_TBD — last rig flash._** Note: with
auto-advance, the per-row MTXA-write cost (`mmulta − mmultw`) no longer
applies to the vertex kernel (set MTXA once), so `mmultw` alone is the number
that matters for the frame budget.
