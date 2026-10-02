# Blitter copies with a cartridge-window source, 2026-10-02

Bench Jaguar (NTSC), GameDrive cart mode, `jagq` jobs 171 and 186. The program is the
clean platform repository's `bench/blitcost` (game-free). Its `.j64` carries a 320x200
16bpp picture at `$900000` in GameDrive cartridge SDRAM, and a copy of it in DRAM. The
GPU blit server (`lib/jag/blit_gpu.s`) runs the copies into a DRAM framebuffer with a
16bpp 320x200 bitmap displayed. Times in VI fields.

| Copies | Source | Silicon | jsim before | jsim after |
|---|---|---:|---:|---:|
| 400 full-screen, phrase mode | DRAM | 218 | 188 | 189 |
| 400 full-screen, phrase mode | cart | 422 | 188 | 395 |
| 50 of 319x200 from x = 1, pixel mode | DRAM | 96 | 93 | 94 |
| 50 of 319x200 from x = 1, pixel mode | cart | 113 | 93 | 111 |

Job 171 (`examples/blitcart`) gave the same phrase-mode figures, 218 and 421.

jsim charged a cart source the same as DRAM. On silicon it costs, with the OP's stretch
(x1.164 here) taken out:
- +12.2 ticks per source phrase in phrase mode: 204 fields x 443,600 / (400 x 16,000)
  / 1.164
- +2.0 ticks per source pixel in pixel mode: 17 fields x 443,600 / (50 x 63,800) / 1.164

**Model:** `BLIT_CART_PHRASE_EXTRA_X10` = 122 and `BLIT_CART_PIXEL_EXTRA_X10` = 20 per
source access, added when a SRCEN blit's source base is in `$800000-$DFFFFF`
(`tom/blit.rs`), before the OP stretch. The cost follows the width read: 64 bits per
phrase, 16 per pixel. 8bpp pixel reads aren't measured and are charged as 16.

**Remaining:** these are differences from a DRAM source. The DRAM copy itself is still
priced at 5.6 ticks an access (Skunkboard, pixel mode). That leaves phrase-mode DRAM copies
~14% fast on this console (189 vs 218) and pixel-mode copies ~3% fast.
