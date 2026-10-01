# Platform silicon bench, 2026-10-01

Bench Jaguar (NTSC), GameDrive cart mode, through the `jagq` broker. Programs are the
clean platform repository's `bench/bench.s` (functional facts) and `bench/perf.s`
(timing). Both are game-free and assembled with GNU as + `jas`. Results were read off
HDMI captures (720x480); the same ROMs run in `jagemu --fidelity silicon` print the
model's answers on the same rows.

## Blitter (bench.s, jobs 39-44)

| Question | Silicon | jsim before |
|---|---|---|
| A1_FINC fraction bits, k = 1..16 (step 2^(16-k), start 1-4*step, advance at px 4) | low half: all 16 honoured (X); high half: Y | same |
| GPU `load` of B_CMD right after its launching `store`, N = 0..31 NOPs | BUSY at every N (jobs 39, 40) | BUSY (settle off) |
| Per-pixel span (XADDPIX), B_SRCD = $1111_2222 : $3333_4444, x0 = 0..3 | every pixel $2222 | lane by x position |
| same, B_PATD = $5555_6666 : $7777_8888 | every pixel $6666 | lane by x position |
| Phrase-mode span from x = 0, B_SRCD / B_PATD | 3333 4444 1111 2222 / 7777 8888 5555 6666 | 1111 2222 3333 4444 |
| 68000 writes ONE register (B_CMD or B_COUNT) of a GPU-programmed span | lands | lands |
| 68000 programs a span zeroing A1 regs with six `clr.l` | never lands (jobs 39-43) | lands |
| same with `move.l #0`; or one `clr.l` on any single A1 register | lands | lands |
| 68000 B_CMD status read right after its launch | IDLE (jobs 40-44) | BUSY |

## Object Processor (bench.s, jobs 42-44)

Capture scale from the bars: 1.642 capture px per OP pixel. `t` = HC units with
line-buffer pixel 0 at HDB1 and 4 units per pixel (PWIDTH 4).

| Screen | Measured |
|---|---|
| HDB 123, HDE $6BF | XPOS 0/4/7 invisible, XPOS 8 one pixel, XPOS 16 at the left edge; XPOS 340 cut at the right |
| HDB 163 / 203 | every object moved right 10 / 20 px; at HDB 203 XPOS 0 sits on the left edge |
| HDE $63F / $5BF / $53F | right edge at t = 1421 / 1293 / 1166 = (HDE & $3FF) + 846; objects past it not shown |
| HBB $400\|600 / $400\|500 | right edge at t = 1445 / 1343 (expected 1446 / 1346) |
| HBE 150 / 100 | no change: the left cut at t ~ 184 is the capture chain |
| 8bpp CLUT object first in an unscaled list | displays (and as the second object) |

## Timing (perf.s, jobs 45-47), VI fields

| Workload | Silicon (OP on / bare STOP) | jsim before | jsim after |
|---|---|---|---|
| 200000 GPU-issued 5-px textured spans | 55 / 47 | 48 / 48 | 53 |
| 200 phrase fills 320x240x16bpp | 21-22 / 19 | 49 / 50 | 23 |
| 1.5 M GPU DRAM loads | 90 | 84 | 85 |
| 68000 load/add loop x 400000 | 106 / 91 | 64 / 62 | 101 |
| same loop while the GPU runs the spans | 139 | 64 | 128 |
| mix R: 60000 x 16 DBRA | 73 / 63 | 56 / 53 | 74 / 63 |
| mix D: 48000 x 16 move.l (a0)+ | 87 / 74 | 40 / 38 | 86 / 74 |
| mix C: 9000 x 64-byte bytewise copy | 131 / 113 | 71 / 68 | 135 / 116 |
| 68000 long reads of B_CMD showing BUSY during a 68000 320x200 fill (of 4096) | 0, 0, 0 | 2867 | 856 |
| same, word reads of B_CMD+2 | 0, 4096, 4096 | (read the command word) | 4096 |

Fits: 68000 waits per bus cycle fetch 1.3 / read 5.1 / write 10.3 (exact on R, D, C;
mix L predicted 85.8 vs 91 off). OP display share 2.06 thousandths of 68000/Blitter time
per phrase a line (+16.5% at 80 phrases; measured +16-18% on every 68000 mix, +16% fills,
+17% spans). Phrase-mode fill 2.19 ticks a phrase. 68000 instruction during a blit:
x3.0, capped at the blit's remaining time.
