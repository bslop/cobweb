# The Object Processor's share of the bus, as the GPU sees it, 2026-10-02

Bench Jaguar (NTSC), GameDrive cart mode, `jagq` job 138. The program is the clean
platform repository's `bench/gpudisp.s` (game-free): four GPU kernels and the 68000 loop,
timed in VI fields, under four displays, all in DRAM.

| Display | K0 consumed loads | K1 load stream | K2 stores | K3 compute | 68K loop |
|---|---:|---:|---:|---:|---:|
| off | 77 | 27 | 25 | 33 | 46 |
| 16bpp 320x240 (80 phrases) | 90 | 32 | 29 | 33 | 54 |
| 8bpp 320x240 (40 phrases) | 86 | 30 | 28 | 33 | 51 |
| two 16bpp 320x240 (160) | 108 | 38 | 35 | 33 | 64 |

K0 = 1.5 M loads each consumed; K1 = 400 k x 4 unconsumed loads; K2 = 400 k x 4
stores; K3 = 1 M x 4 adds, nothing external; 68K = 200 k load/add iterations.

**Reading.**
- Every bus-bound master slows by the same time ratio, whatever its access pattern:
  x1.17 for 16bpp, x1.11 for 8bpp, x1.40 for two 16bpp bitmaps. Compute-only GPU code
  doesn't slow at all.
- The ratio is 1/(1 - share): the 160-phrase case is x1.40, where a linear tax gives
  x1.33.
- An 8bpp bitmap costs more than its phrases. The share fits a per-phrase fetch term
  plus a per-pixel term: DRAM phrase 1090 ppm, pixel 182 ppm, cartridge phrase 10400
  ppm, the last refitted with the pixel term on job 135.

**Model (this series).** `m68k::op_share_ppm` = DRAM phrases x 1090 + cart phrases x
10400 + pixels x 182 (cap 0.96).
- The 68000 and the Blitter take time x 1/(1 - share).
- Each GPU/DSP external access waits share/(1 - share) x the core's own ticks since its
  previous external access. That excludes the previous wait and is capped at 32. A
  bus-bound loop therefore runs at T/(1 - share), and work inside the core is not
  slowed.
- The old per-access tax (5.75 milli-ticks per phrase, Skunkboard 2026-07-19) is gone.

jsim afterwards, same ROM (`--fidelity silicon`):

| Display | K0 | K1 | K2 | K3 | 68K |
|---|---:|---:|---:|---:|---:|
| off | 83 | 17 | 17 | 33 | 43 |
| 16bpp | 97 | 19 | 19 | 33 | 51 |
| 8bpp | 93 | 19 | 19 | 33 | 49 |
| two 16bpp | 118 | 24 | 24 | 33 | 62 |

The display ratios now match silicon within ~3%. Before, the GPU columns barely moved
(83/85/84/87 for K0).

**Not fixed here:** jsim's GPU runs unconsumed load streams and stores about 35% faster
than silicon even with the display off (K1 17 vs 27, K2 17 vs 25), and consumed loads
8% slower (83 vs 77). That's the base external-access model, calibrated 2026-07-19..21
on another console. It disagrees with this bench and needs its own probes.
