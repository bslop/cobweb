# Cobweb jsim calibration suite

Timing-probe ROMs that measure the real Jaguar's JRISC pipeline and bus
behavior, so jsim's `CAL:`-tagged constants (`sim/crates/jag-core/src/risc/timing.rs`)
become hardware facts instead of provisional numbers. The same ROM runs in
jsim, so every bench session directly produces a predicted-vs-measured diff.

The clock is **self-calibrating**: probe `vcmod` measures the VC wrap modulus
on the actual rig (the folklore modulus has been wrong before — the pilot project
measured 2571 where folklore said 525). All timing math happens host-side in
`parse_results.py`; the console prints raw VC readings only.

## Builds

```sh
make            # both targets, via the cubanismo/jaguar-sdk Docker image
```

- `build/calib_skunk.cof` — **Skunkboard bench build**: results stream over the
  USB console (plus the DRAM table).
- `build/calib_sim.cof` — silent build for jsim (and GameDrive): results in the
  DRAM table only, at `0x100000`.

## Bench protocol (Skunkboard)

```sh
jcp -c build/calib_skunk.cof | tee bench_$(date +%Y%m%d).log
# wait for "CAL DONE" (~2-3 s of probe time; console I/O dominates)
python3 parse_results.py --console bench_YYYYMMDD.log
```

Notes:
- The suite runs every probe twice: **mode A** (68k busy-polling — bus noise
  present) and **mode B** (68k STOPped between vertical interrupts — quiet
  bus). The A/B delta is itself a measurement (Typo measured +15.5% from
  stopping the 68k in a bus-heavy renderer).
- The **GPU-in-main probes run last, mode A only**. They follow the
  Owl/Scavone alignment rules, but if they wedge on your silicon revision the
  GPU cannot be recovered externally (TRM bug 23) — the console will print
  `CAL WEDGED`, all earlier results are already printed/stored, and a power
  cycle is needed.
- Two consoles? Run the suite on both — per-unit silicon variation on the
  errata-heavy paths is itself worth knowing.

## Predicted values (run before the bench, diff after)

```sh
cd ../sim
./target/release/jagemu peek ../calib/build/calib_sim.cof \
    --at 0x100000 --len 1024 --frames 2400 --fidelity silicon > /tmp/sil.json
python3 ../calib/parse_results.py --peek /tmp/sil.json
```

Reference outputs are checked in: `PREDICTIONS_silicon.txt` (the model's
claims) and `PREDICTIONS_functional.txt` (the null model — hardware should
*disagree* with this one; if it doesn't, the probe is broken).

**STATUS: HARDWARE-CALIBRATED, TWO SESSIONS** (bench 2026-07-17;
session 1 `bench_20260717.log`, session 2 `bench_20260717_s2.log` — 18 probes
x 2 modes). Final accuracy: **32/32 probe measurements matched, mean abs
error 0.059 cyc/instr, max 0.27** — including the 68k-contention split, which
jsim now reproduces from first principles (row-thrash model: page-hit loads
+4 occ/+4 lat, fetches +7/word, stores and page-misses untaxed).

Session-2 discoveries: consumed DRAM load-to-use is ~16 cycles quiet (not
the ~4 first guessed); **truly-quiet GPU-in-main tax is 6.24x local** — U-235's
famous 8.5x was measured with the 68k idling-but-not-STOPped, so the folklore
number embeds partial contention; stores never pay contention (write buffer).

Session-1 table (first calibration pass), cyc/instr:

| probe | jsim | hardware | note |
|---|---|---|---|
| nop/move/moveq/addind | 1.00 | 1.00-1.01 | local issue rate (settles the U-235 "2 ticks/MOVE" — JPIT artifact) |
| adddep | 2.00 | 2.00-2.01 | ALU bubble confirmed |
| ldsram | 1.50 | 1.50 | internal load ready at start+2 (faster than TRM's "cycle 3-4" read) |
| ldidx | 3.00 | 3.01 | indexed +1 latency, +2 issue |
| lddram (quiet) | 2.00 | 2.05 | issue-side bus occupancy ~1/access |
| lddram (68k polling) | 2.00 | **4.31** | 68k CONTENTION — known-unmodeled, the next bus-model target |
| ldstride | 2.51 | 2.56 | page-miss occupancy ~2 |
| stdram | 2.00 | 2.04 | stores pay occupancy too |
| divhot | 6.67 | 6.69 | DIV quotient at cycle 18 confirmed |
| divsh | 1.00 | 1.05 | shadow-filled DIV free |
| jr | 2.33 | 2.34 | taken-jump refill = 3 |
| mainmov/mainnop | 13.19 | 13.46-13.48 | GPU-in-main tax under busy 68k (quiet-bus lower; U-235 ~8.5x) |
| VC modulus | 524 | 524 | emulator aligned to hardware |

Notable: the GPU-in-main probes ran clean on real silicon — the Owl/Scavone
alignment discipline works as encoded. 68k bus noise on LOCAL GPU code is
<1%; on DRAM-bound GPU work it is >2x — "lay off the 68k" quantified.

Original pre-calibration claims, for the record:

| probe | claim | pins down |
|---|---|---|
| nop/move/moveq/addind | 1.00 cyc/instr | local issue rate |
| adddep | 2.00 | ALU result bubble (`Lat::ALU`) |
| ldsram | 2.00 | internal load latency (`Lat::LOAD_INTERNAL`) |
| ldidx | 3.00 | indexed-load overhead (`Lat::IDX_LOAD_ISSUE`) |
| divhot | 6.67 (20 cyc/unit) | DIV shadow (`Lat::DIV`) |
| divsh | 1.00 | shadow-filled DIV is free |
| jr | 2.00 | taken-JR refill (`Lat::JUMP_REFILL`) + flag latency |
| lddram / ldstride / stdram | 1.50 | DRAM data path (`DRAM_DATA_HIT/MISS`) |
| mainmov / mainnop | 8.12 | external fetch tax (`EXT_FETCH_HIT/MISS`) |
| nop A vs B | 1.00 | 68k bus contention (NOT yet modeled — hardware will differ!) |

Key open questions for the rig:
1. **JPIT/VC tick ratio** — the U-235 experiment measured 2402 ticks for 1200
   local MOVEs, implying 2 ticks/MOVE where the TRM implies 1. The `move`
   probe against the self-measured VC modulus settles it.
2. **True external fetch cost** — we chose EXT_FETCH_HIT=7 to land near
   U-235's ~8.5x; the `mainnop`/`mainmov` pair measures it directly.
3. **A/B contention** — jsim predicts 1.00 (no contention model yet); the
   hardware delta tells us what the bus model must charge.

## How this was validated before touching hardware

The whole suite ran in jsim first, which caught two real bugs — one in each
direction:
- a jsim scheduler bug (budget overrun not carried between slices — expensive
  instructions ran faster than wall clock), now covered by the
  `timed_budget_debt_couples_wall_clock` unit test;
- a probe bug (the GPU-in-main body's settle `movei` clobbered `r22`, the
  harness's wrap-detection register, silently eating VC wraps — a
  plausible-but-wrong number that would have poisoned the hardware log).

That loop — probe validates model, model validates probe — is the jtest
methodology in miniature.

## Files

- `probes.s` — GPU probe kernels (rmac). Register plan in the header comment;
  bodies may use r0–r14 only.
- `main.c` — 68k harness: stages kernels, runs A/B modes, prints raw results.
- `bootstub.s` — entry, VI handler (INT1 ack + INT2 priority restore), STOP.
- `parse_results.py` — console/peek decoding, per-instruction math, CAL knobs.
- `PREDICTIONS_*.txt` — jsim reference tables to diff bench logs against.
