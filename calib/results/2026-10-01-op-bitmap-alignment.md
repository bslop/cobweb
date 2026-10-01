# Object-list alignment on silicon, 2026-10-01 (platform issue 0010)

Bench Jaguar (NTSC), GameDrive cart mode, through the `jagq` broker, job 55. The program
is the clean platform repository's `bench/olist.s` (game-free): two object lists rebuilt
by the VI handler every field and swung through OLP in turn. Each case puts both lists at
the same offset from a 1 KB boundary and may pad the list with one-phrase BRANCH objects
that always continue to the next phrase, so the list start and the bitmap object move
independently. The bitmap is an unscaled 320x200 16bpp object followed by STOP. Each
case holds ~4 s; 40 captures, 1 s apart, every capture classified.

| Case | List start | Pad | Bitmap object | Silicon (captures) | jsim 2026-10-02 |
|---|---|---|---|---|---|
| 0 | +0 | 0 | +0 | clean (4/4) | clean |
| 1 | +8 | 0 | +8 | **garbage (3/3)** | clean |
| 2 | +8 | 1 | +16 | clean (3/3) | clean |
| 3 | +0 | 1 | +8 | **garbage (4/4)** | clean |
| 4 | +16 | 0 | +16 | clean (3/3) | clean |
| 5 | +24 | 1 | +32 | clean (3/3) | clean |
| 6 | +0 | 2 | +16 | clean (3/3) | clean |
| 7 | +0 | 0 | +0 | clean (17/17) | clean |

Garbage = horizontal noise rows and wrong colours over the whole window, including where
the background colour would show.

**Rule:** an unscaled BITMAP object must start on a 16-byte (double-phrase) boundary. The
list start needs only phrase (8-byte) alignment, and 16 mod 32 is fine for the object.

jsim after this change: every fidelity counts the object (`op.bitmap_misaligned_*`) and
jagemu warns after the run; `--fidelity silicon` draws nothing from the object on;
`--strict=op-align` stops at it (`strict_fault.kind` = `op_bitmap_misaligned`,
`addr` = the object). On `olist.j64` that is `$0FC408`, case 1, frame 357.
