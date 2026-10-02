# Object Processor bitmaps in the cartridge window, 2026-10-02 (platform issue 0011)

Bench Jaguar (NTSC), GameDrive cart mode, `jagq` job 135. The program is the clean
platform repository's `bench/cartbus.s` (game-free): four fixed workloads, each timed in
VI fields, under five display set-ups. The cart bitmaps and reads use cart page 1
($900000, bank 1 at boot). Only the bus traffic matters, not the contents.

Workloads: 68K = 200,000 iterations of a 68000 DRAM load/add loop; GD = 1.5 M GPU DRAM
loads (each consumed); GC = 150,000 GPU loads from the cart window; 68C = the 68K loop
reading the cart window.

| Display | 68K | GD | GC | 68C |
|---|---:|---:|---:|---:|
| off (STOP only) | 46 | 77 | 12 | 50 |
| 16bpp 320x240 in DRAM (80 phrases a line) | 54 | 89 | 14 | 58 |
| **16bpp 320x240 in the cart** | **437** | **686** | **95** | **490** |
| 8bpp 320x240 in DRAM (40 phrases) | 51 | 85 | 13 | 55 |
| **8bpp 320x240 in the cart** | **84** | **139** | **20** | **90** |

**Reading.** With the OP holding the bus for a share s of each displayed line, the other
masters run at (1 - s) of their speed. Every cart cell fits a share of 0.0112 a phrase
(68000: 0.0113 from the 8bpp row, 0.0112 from the 16bpp row; GPU likewise), against
0.00177 for a DRAM phrase (the job-47 calibration, x1.165 at 80 phrases). **A phrase
fetched from the cartridge costs about 6.3 times a DRAM phrase.** A full-width 16bpp
bitmap in the cart takes about 90% of the bus: everything else runs 8-10 times slower.
GPU reads from the cart cost about 35 ticks a loop against about 23 from DRAM, with the
display off.

**Model:** see `2026-10-02-op-share-gpu.md`, which refits the share with a per-pixel term
and replaces the RISC taxes. jsim afterwards (same ROM, `--fidelity silicon`):

| Display | 68K | GD | GC | 68C |
|---|---:|---:|---:|---:|
| off | 43 | 83 | 10 | 43 |
| 16bpp in DRAM | 51 | 97 | 11 | 51 |
| 16bpp in the cart | 417 | 789 | 82 | 417 |
| 8bpp in DRAM | 49 | 92 | 11 | 49 |
| 8bpp in the cart | 83 | 158 | 18 | 83 |

Before, the cart rows equalled the DRAM rows. Cart cells are now within ~15% of silicon:
the GPU's ratio under cart contention is a little lower on silicon than the 68000's,
and one shared share coefficient can't capture both. The remaining gaps are the GPU's
base access rate (see the other log) and 68000 cart reads ~15% fast.
