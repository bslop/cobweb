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

**Model (this series):** `m68k::op_share_ppm` = DRAM phrases x 1770 + cart phrases x
11200 millionths, capped at 0.96. The 68000 and the Blitter take time x 1/(1 - share).
The GPU/DSP keep their per-access DRAM tax, and each external access waits a further
21.5 (DRAM) or 28.5 (other external) ticks x share/(1 - share) of the cartridge share.
jsim afterwards (same ROM, `--fidelity silicon`):

| Display | 68K | GD | GC | 68C |
|---|---:|---:|---:|---:|
| off | 43 | 83 | 10 | 43 |
| 16bpp in DRAM | 51 | 84 | 10 | 51 |
| 16bpp in the cart | 442 | 756 | 98 | 442 |
| 8bpp in DRAM | 47 | 84 | 10 | 47 |
| 8bpp in the cart | 79 | 143 | 18 | 79 |

Before this series, the cart rows equalled the DRAM rows. The remaining gaps: the GPU's
DRAM-display tax is still per access (silicon GD +16% with a DRAM bitmap, jsim +1%), and
68000 cart reads run ~10% fast in jsim.
