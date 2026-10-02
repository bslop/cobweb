# The GPU's DRAM access rate, display off, 2026-10-02

Bench Jaguar (NTSC), GameDrive cart mode, `jagq` job 140. The program is the clean
platform repository's `bench/gpurate.s` (game-free): ten GPU kernels from local RAM, each
timed in VI fields with the Object Processor on a bare STOP list.

| Kernel | Body (each loop also: subq, jr, nop) | Iterations | Silicon | jsim before | jsim after |
|---|---|---:|---:|---:|---:|
| L1 | 1 load, one address | 2.0 M | 48 | 51 | 51 |
| L2 | 2 loads | 1.5 M | 71 | 46 | 56 |
| L4 | 4 loads | 1.0 M | 66 | 40 | 60 |
| L8 | 8 loads | 0.5 M | 52 | 30 | 54 |
| Q4 | 4 loads, consecutive longs (+addq each) | 1.0 M | 66 | 54 | 63 |
| S1 | 1 store | 2.0 M | 43 | 51 | 51 |
| S2 | 2 stores | 1.5 M | 64 | 46 | 56 |
| S4 | 4 stores | 1.0 M | 61 | 40 | 60 |
| S8 | 8 stores | 0.5 M | 50 | 30 | 54 |
| N4 | 4 nops | 3.0 M | 76 | 76 | 76 |

In ticks per iteration (a field is about 443,600 ticks): silicon streams a burst at one
access per ~4.9 ticks (L8: (46.1 - 7.2) / 8; stores 4.65), whatever the address. jsim let
back-to-back accesses through at ~2.4.

**Model:** two DRAM accesses by one RISC core start at least `DRAM_MIN_SPACING` = 5 ticks
apart (`risc/timing.rs`, `ext_access`). The 2026-07-19 `lddram` stream (gap ~4, "pays
nothing") sits at that limit already, so it's consistent.

The same change on `bench/gpudisp.s` (job 138), display off / 16bpp / 8bpp / two 16bpp:
- load streams: jsim 25/29/28/35 (silicon 27/32/30/38)
- stores: jsim 25/29/28/35 (silicon 25/29/28/35)
- before: 17/19/18/20

**Remaining:**
- 2-access bursts are 12-21% fast in jsim: silicon charges more to start a burst.
- A lone store is 19% slow: silicon's write buffer issues it faster than a load.
- Consumed loads are ~8% slow (K0 83 vs 77).
