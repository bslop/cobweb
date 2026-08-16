# Tom Blitter — Implementation Specification

Subsystem: the Tom Blitter (2D block move / fill / logic / Gouraud / Z engine).

Scope: enough to implement **copy**, **solid-fill**, and **span-fill** blits
cycle-accurately, plus the full register/bit model for Gouraud + Z-buffer so the
later 3D path drops in without re-architecting.

This is **big-endian** hardware (Motorola 68000 host bus + 64-bit coprocessor
bus). All multi-byte values are MSB-first. Byte/word/long/phrase ordering is
called out explicitly wherever it matters.

## Source legend / authority order

On any conflict, **local official docs win**.

- **[INC]** — `…/ATARI Jaguar SDK/INCLUDE/JAGUAR.INC` lines 216–421. Authoritative
  register + bit equates. TRUST these exact values verbatim.
- **[TRM p.N]** — `Jaguar Technical Reference v8.pdf`, PDF page N (PDF page ==
  printed "Page N"). Blitter chapter = pages 64–82.
- **[reference backend]** — a proven reference backend, verified on
  BigPEmu (solid spans/bands, XPIX addressing, fire-and-forget).
- **[porting notes]** — the internal porting notes.
- **(UNVERIFIED)** — inference / not nailed down in the local docs; cross-check
  against BigPEmu. Collected in **Open questions**.

Terminology: a **phrase** = 64 bits = 8 bytes = one coprocessor-bus transfer.
A **long** = 32 bits. A **word** = 16 bits.

---

## 1. Register map (memory-mapped, big-endian)

Base `$F02200`. All blitter registers are **32-bit (long) accesses** unless the
"width" column says otherwise. Do **not** word-access these (contrast Tom video
registers which are 16-bit) [porting notes §1, "GPU/blitter regs are longs"].

Data registers (`B_SRCD`…`B_Z3`) are conceptually **64-bit**; they occupy two
consecutive longs. The SDK equate points at the **high** long; the low long is at
`equate+4`. `B_SRCD=$F02240` high, `$F02244` low [reference backend]. The data
registers **may only be written while the blitter is idle**
[TRM p.70].

| Addr | [INC] name | Width | Dir | Meaning |
|------|-----------|-------|-----|---------|
| `$F02200` | `A1_BASE`   | 32 | W  | A1 window base ptr. **Must be phrase-aligned** (low 3 bits 0). [TRM p.66,70] |
| `$F02204` | `A1_FLAGS`  | 32 | W  | A1 control flags (pitch/pixel/zoffs/width/xadd/yadd/sign). §3. |
| `$F02208` | `A1_CLIP`   | 32 | W  | A1 clip size: `height<<16 | width`, each 15-bit (top bit of each word ignored). [TRM p.70] |
| `$F0220C` | `A1_PIXEL`  | 32 | RW | A1 pixel pointer: `Y<<16 | X`, both signed 16-bit. §2. |
| `$F02210` | `A1_STEP`   | 32 | W  | A1 outer-loop step (integer): `Ystep<<16 | Xstep`, signed 16-bit each. [TRM p.71] |
| `$F02214` | `A1_FSTEP`  | 32 | W  | A1 outer-loop step (fraction): `Yfrac<<16 | Xfrac`. [TRM p.71] |
| `$F02218` | `A1_FPIXEL` | 32 | RW | A1 pixel pointer fraction: `Yfrac<<16 | Xfrac`. DDA use. [TRM p.72] |
| `$F0221C` | `A1_INC`    | 32 | W  | A1 inner-loop increment (integer): `Yinc<<16 | Xinc`, signed 16-bit. [TRM p.72] |
| `$F02220` | `A1_FINC`   | 32 | W  | A1 inner-loop increment (fraction). [TRM p.72] |
| `$F02224` | `A2_BASE`   | 32 | W  | A2 window base ptr. Phrase-aligned. [TRM p.72] |
| `$F02228` | `A2_FLAGS`  | 32 | W  | A2 control flags. Same layout as A1 except bit 15 = **Mask** (A1 bit 15 unused). §3. |
| `$F0222C` | `A2_MASK`   | 32 | W  | A2 pointer AND-mask (when A2 FLAGS bit15 set). `Ymask<<16 | Xmask`. §2.4. [TRM p.72] |
| `$F02230` | `A2_PIXEL`  | 32 | RW | A2 pixel pointer: `Y<<16 | X`. [TRM p.73] |
| `$F02234` | `A2_STEP`   | 32 | W  | A2 outer-loop step (integer): `Ystep<<16 | Xstep`. A2 has **no** fractional step/inc. [TRM p.73] |
| `$F02238` | `B_CMD`     | 32 | W  | Command. **Write starts the blit.** §6. |
| `$F02238` | (status)    | 32 | R  | Status (IDLE/STOPPED + diagnostic). §7. Same address, read side. |
| `$F0223C` | `B_COUNT`   | 32 | W  | Counters: `outer<<16 | inner`. §1.1. |
| `$F02240` | `B_SRCD`    | 64 | W  | Source data / Gouraud intensity fractions. |
| `$F02248` | `B_DSTD`    | 64 | W  | Destination data / background colour. |
| `$F02250` | `B_DSTZ`    | 64 | W  | Destination Z. |
| `$F02258` | `B_SRCZ1`   | 64 | W  | Source Z1 / computed Z integer parts. |
| `$F02260` | `B_SRCZ2`   | 64 | W  | Source Z2 / computed Z fraction parts. |
| `$F02268` | `B_PATD`    | 64 | W  | Pattern data / computed intensity integer+colour. |
| `$F02270` | `B_IINC`    | 32 | W  | Intensity increment (8.16; top 8 bits = colour, leave 0). [TRM p.76] |
| `$F02274` | `B_ZINC`    | 32 | W  | Z increment (16.16). [TRM p.76] |
| `$F02278` | `B_STOP`    | 32 | W  | Collision stop control: RESUME/ABORT/STOPEN. §5.4. [TRM p.77] |
| `$F0227C` | `B_I3`      | 32 | W  | Intensity reg 3 (8.16, top 8 bits unused). [INC:247] |
| `$F02280` | `B_I2`      | 32 | W  | Intensity reg 2. |
| `$F02284` | `B_I1`      | 32 | W  | Intensity reg 1. |
| `$F02288` | `B_I0`      | 32 | W  | Intensity reg 0. |
| `$F0228C` | `B_Z3`      | 32 | W  | Z reg 3 (16.16). |
| `$F02290` | `B_Z2`      | 32 | W  | Z reg 2. |
| `$F02294` | `B_Z1`      | 32 | W  | Z reg 1. |
| `$F02298` | `B_Z0`      | 32 | W  | Z reg 0. |

**Note the `B_I*` / `B_Z*` ordering reversal vs the TRM.** [INC:247–255] orders
them `B_I3, B_I2, B_I1, B_I0` (descending) at ascending addresses, i.e. `B_I3` is
at the *lowest* address `$F0227C`. The TRM table [TRM p.70 / p.77] lists I0 at
`$F0227C`. **Trust [INC]: `$F0227C`=I3, `$F02288`=I0; `$F0228C`=Z3, `$F02298`=Z0.**
These four-register banks alias the 64-bit `B_PATD`/`B_SRCD` (intensity) and
`B_SRCZ1`/`B_SRCZ2` (Z) — each `B_In` updates one 16-bit lane's integer+colour
view, each `B_Zn` one lane's 16.16 view [TRM p.77]. They are a convenience write
port; emulator may implement them as writes that scatter into the 64-bit shadow
registers (§4.3). (Exact lane→register byte mapping is partly UNVERIFIED — see
Open questions.)

`A1_FLAGS` ↔ field accessor symbols [INC alt names]: `A1_PITCH` etc. are bit-field
selectors, not separate addresses; the TRM worked example writes them as named
sub-fields of `A1_FLAGS` [TRM p.81].

---

## 1.1 The two-level loop and B_COUNT

The blitter runs a **two-level nested loop** [TRM p.66–67]:

- **Inner loop** = one scan line. Length = **inner count**. Traverses pixels
  along a line (X), advancing per the XADD mode (§3.5).
- **Outer loop** = the rows. Length = **outer count**. Between inner-loop passes,
  it adds the **step** value to the pointer(s) (UPDA1/UPDA1F/UPDA2 gated, §5.3).

`B_COUNT` (`$F0223C`, write-only) packs both, **big-endian within the long**:

```
 bits 31..16 : OUTER count  (number of rows / outer iterations)
 bits 15..0  : INNER count  (pixels per inner-loop pass / line length)
```

[TRM p.75: "The low word is the number of iterations of the inner loop… The high
word is the number of iterations of the outer loop."]

- The **inner** counter **reloads from `B_COUNT[15:0]` on every entry to the
  inner loop** [TRM p.75]. The **outer** counter is loaded once.
- Both counters accept values **1…65536**, where **65536 is encoded as 0**
  [TRM p.75]. So a stored value of `0` means 65536 iterations; `N` (1..65535)
  means N. Implement: `effective = (raw == 0) ? 65536 : raw`, applied to each
  16-bit half independently.

Confirmed encoding in proven code [reference backend]:
```
B_COUNT = (outer << 16) | inner          // e.g. span: (1<<16)|n
```
`blit_span` sets outer=1, inner=n (single line). `blit_band` sets
`outer=(y1-y0)`, inner=`RENDER_W` (a rectangle).

> **(UNVERIFIED) Off-by-one in blit_band outer count.** `blit_band` uses
> `outer = y1 - y0`, not `y1 - y0 + 1`, so it fills `(y1-y0)` rows including the
> first → effectively rows `[y0, y1)`. This is the program's choice, not a
> blitter rule; the engine just runs `outer` iterations. Document, don't "fix".

---

## 2. Address generation (A1 / A2)

Each address generator turns a **window pointer (X,Y in pixels)** into a **byte
address** within a window [TRM p.66]. There are two units: **A1** (full-featured,
normally destination) and **A2** (simpler, normally source). `DSTA2` (B_CMD bit
11) swaps these roles [TRM p.74].

### 2.1 Window model

A window is a packed linear array of phrases. Defined by:
- `*_BASE` — byte base, **phrase-aligned** (low 3 bits must be 0) [TRM p.70].
- `WIDTH` field in `*_FLAGS` — width **in pixels**, floating-point encoded (§2.2).
- `PIXEL` field — pixel size (§3.2).
- `PITCH` field — inter-phrase gap (§2.3).

### 2.2 Pixel pointer → address; the WIDTH float encoding

The pixel address within the window is:

```
pixel_index   = X + (WIDTH_pixels * Y)
byte_offset   = (pixel_index * pixel_bits) / 8     // pixel_bits = 2^PIXEL
phrase_gap    = applied per PITCH (§2.3)
address       = BASE + byte_offset (+ pitch gaps)
```

The hardware avoids a multiplier by storing **WIDTH as a 6-bit float** in
`*_FLAGS` bits 9–14 [TRM p.66, p.70]:

```
 bit  14  13  12  11 | 10  9
      E3  E2  E1  E0 | M1  M0
      \---exponent--/  \-mant-/
```

- 4-bit unsigned **exponent** E (bits 13:9 → actually E in bits 14:11),
- 2-bit stored **mantissa** M (bits 10:9), with an **implied leading 1**,
- binary point **after** the implied 1. So value = `1.M1M0 (binary) × 2^E`.

Valid exponent range **0–11** [TRM p.66]. Width must be a whole number of phrases
in the current pixel size. The TRM gives the bit-packed values as `<<9` of a
6-bit code [INC:359–397]; e.g. `WID320 = $00004200` → field `0b100001` =
`(1).01 × 2^8 = 320` [INC:382–383, "WID320 EQU $00004200 ; 1.01 X 2^8"].

Worked examples [TRM p.66, p.78]:

| Pixels | Binary | Float | E(4) | M(2) | Field bits14:9 | `*_FLAGS` value |
|--------|--------|-------|------|------|----------------|-----------------|
| 20  | `00000010100`  | 1.01×2^4 | 0100 | 01 | `010001` | `$00002200` (`WID20`) |
| 80  | `00001010000`  | 1.01×2^6 | 0110 | 01 | `011001` | `$00003200` (`WID80`) |
| 128 | `00010000000`  | 1.00×2^7 | 0111 | 00 | `011100` | `$00003800` (`WID128`) |
| 160 | `00010100000`  | 1.01×2^7 | 0111 | 01 | `011101` | `$00003A00` (`WID160`) |
| 320 | `00101000000`  | 1.01×2^8 | 1000 | 01 | `100001` | `$00004200` (`WID320`) |
| 640 | `01010000000`  | 1.01×2^9 | 1001 | 01 | `100101` | `$00004A00` (`WID640`) |
| 3584| `11100000000`  | 1.11×2^11| 1011 | 11 | `101111` | `$00005E00` (`WID3584`) |

Decoder (emulator, exact):
```
field6 = (FLAGS >> 9) & 0x3F        // 6-bit code
mant   = field6 & 0x3               // M1 M0
exp    = (field6 >> 2) & 0xF        // E3..E0
width_px = (4 | mant) << (exp - 2)  // = (1.M1M0 binary) * 2^exp, since
                                    //   (4|mant) = 1.MM * 4, >>2 cancels
// equivalently: width_px = ((4 + mant) << exp) >> 2
```
Cross-check: `WID320` field `0b100001` → mant=1, exp=8 → `(4|1)<<8>>2 = 5<<6 =
320`. ✔ `WID640` field `0b100101` → mant=1, exp=9 → `5<<7 = 640`. ✔

**For pure horizontal blits (span/band), WIDTH only matters for the Y term.**
Solid spans set Y once and never wrap a line, so any WIDTH ≥ blit width works; the
proven code uses the real surface width (320 or 160) so the same FLAGS value also
serves the multi-row band path [reference backend].

### 2.3 PITCH — inter-phrase gaps

`*_FLAGS` bits 0–1. Distance between successive **pixel** phrases, to interleave Z
/ double-buffer data [TRM p.70]. The TRM's "2 to the power of this value" prose is
contradicted by its own special case; trust [INC:328–331] exactly:

| `PITCH*` | bits1:0 | Gap (phrases between pixel phrases) |
|----------|---------|-------------------------------------|
| `PITCH1` | 00 | **0** phrase gap (contiguous) |
| `PITCH2` | 01 | **1** phrase gap |
| `PITCH4` | 10 | **3** phrase gap |
| `PITCH3` | 11 | **2** phrase gap (special: 2 pixel phrases per Z phrase) |

So byte stride per pixel-phrase advance = `(1 + gap) * 8`. For span/fill blits
PITCH = `PITCH1` (0). [TRM p.81 example uses PITCH=1 to interleave Z.]

### 2.4 A2 mask, signed pointers, clip ranges

- A2 only: when `A2_FLAGS` bit15 (`Mask`) set, the A2 pointer is **logically
  ANDed** with `A2_MASK` (`Ymask<<16 | Xmask`) to wrap within a power-of-two
  rectangle — used to tile/repeat a source pattern [TRM p.67, p.72].
- Pointers (`*_PIXEL`) are **signed 16-bit X and Y**. The address generator only
  produces valid addresses for **X in 0..32767, Y in 0..4095** (Y treated as
  12-bit unsigned, high bits ignored) [TRM p.66, p.72]. Values outside are for
  **clipping** purposes only.
- A1 fractional pointer/inc (`A1_FPIXEL`/`A1_FINC`) drive a 16.16 **DDA** for line
  draw / scaled-rotated source scan [TRM p.67,72]. Not needed for copy/fill.

### 2.5 A1 window clipping (CLIP_A1)

When B_CMD bit6 `CLIP_A1` is set, destination writes are **inhibited** whenever
the A1 pointer leaves its window: X or Y `< 0`, or `>= ` the value in `A1_CLIP`
(`width` low 15 bits, `height` high 15 bits) [TRM p.70, p.79]. The blitter keeps
running (counters still advance); only the write is suppressed. Window origin
(0,0) is the top-left; to clip a sub-rect not at origin, move `A1_BASE` to the
rect's corner [TRM p.70].

---

## 3. `*_FLAGS` register layout (A1 / A2)

Both flags registers share this layout. **A1 bit15 = unused; A2 bit15 = Mask.**
A2 has **no XADD=11 (increment) mode** (A2 lacks DDA hardware). [TRM p.70–72]

| Bits | Field | A1 | A2 | Notes |
|------|-------|----|----|-------|
| 0–1  | PITCH | ✔ | ✔ | §2.3 |
| 2    | unused | – | – | |
| 3–5  | PIXEL | ✔ | ✔ | §3.2 |
| 6–8  | ZOFFS | ✔ | ✔ | §3.3 |
| 9–14 | WIDTH | ✔ | ✔ | §2.2 |
| 15   | (unused) / Mask | unused | Mask | A2 only |
| 16–17| XADD ctrl | ✔ | ✔ (no `11`) | §3.5 |
| 18   | YADD ctrl | ✔ | ✔ | §3.6 |
| 19   | X sign | ✔ | ✔ | §3.7 |
| 20   | Y sign | ✔ | ✔ | §3.7 |

### 3.2 PIXEL — pixel size, bits 3–5

Pixel size = `2^n` bits, `n` = field value 0–5 [INC:335–340, TRM p.70]:

| `PIXEL*` | bits5:3 | n | bits/pixel |
|----------|---------|---|------------|
| `PIXEL1`  | 000 | 0 | 1  |
| `PIXEL2`  | 001 | 1 | 2  |
| `PIXEL4`  | 010 | 2 | 4  |
| `PIXEL8`  | 011 | 3 | 8  |
| `PIXEL16` | 100 | 4 | 16 |
| `PIXEL32` | 101 | 5 | 32 |

`PIXEL16 = $00000020` [INC:339]. Gouraud + Z work in **16-bit pixel mode only**
[TRM p.68, p.81].

### 3.3 ZOFFS — Z phrase offset, bits 6–8

Offset (in phrases) from a pixel phrase to its corresponding Z phrase [TRM p.70].
Values 0 and 7 unused. `ZOFFS1 = $00000040` [INC:346]. Only relevant when Z is in
use.

### 3.5 XADD control — inner-loop X update, bits 16–17

[INC:402–405, TRM p.71]:

| `XADD*` | bits17:16 | Action per inner-loop step |
|---------|-----------|----------------------------|
| `XADDPHR` | 00 | **Phrase mode**: add phrase width and truncate X to the next phrase boundary. Processes up to a whole phrase (4 px @16bpp) per step. |
| `XADDPIX` | 01 | Add one pixel (X += 1, or −1 with X sign). "Pixel addressing" mode. |
| `XADD0`   | 10 | Add zero (X unchanged) — e.g. column fills, Gouraud where X is fixed-per-pixel-group. |
| `XADDINC` | 11 | Add the A1 increment (DDA). A1 only. Overrides YADD (Y also takes the increment). |

`XADDPIX = $00010000` [INC:403]. Phrase mode (`XADDPHR=0`) is the fast path: the
blitter moves/fills 64 bits per bus cycle. Pixel mode advances one pixel at a time
(needed when start/end aren't phrase-aligned, or for per-pixel compare logic).

### 3.6 YADD control — inner-loop Y update, bit 18

[INC:410–411, TRM p.71]:
- `YADD0` (0) — add 0 to Y inside the inner loop (normal raster span).
- `YADD1` (1) — add 1 to Y inside the inner loop (vertical/diagonal traversal).
Overridden by XADD=`11` (increment) on A1.

### 3.7 X sign / Y sign, bits 19 / 20

[INC:415–421, TRM p.71]:
- `XSIGNSUB = $00080000` (bit19=1): with `XADDPIX`, **subtract** pixel size instead
  of add (right-to-left). Only valid with XADD=01.
- `YSIGNSUB = $00100000` (bit20=1): makes `YADD1` into Y **−1** (bottom-to-top).

---

## 4. Data path

64-bit data path. Operates a **phrase at a time** (phrase mode) or **a pixel at a
time** (pixel mode) [TRM p.67].

### 4.1 Source / destination read enables (B_CMD bits 0–5)

[INC:261–266, TRM p.73]. Destination **write** cycles are always performed (subject
to comparator/clip inhibit); everything else is opt-in:

| Bit | `[INC]` | Meaning |
|-----|---------|---------|
| 0 | `SRCEN`  | Read source data in the inner loop. |
| 1 | `SRCENZ` | Read source Z in the inner loop. **Ignored unless SRCEN set.** |
| 2 | `SRCENX` | Extra source read at **start** of inner loop (for re-alignment when source ≠ dest phrase alignment, ≥8bpp). Also does an extra Z read if SRCENZ. |
| 3 | `DSTEN`  | Read destination data in the inner loop. **Required for pixels <8 bits** (write must restore neighbours) and for any write-inhibit/blend mode. |
| 4 | `DSTENZ` | Read destination Z (to compare against computed/source Z). |
| 5 | `DSTWRZ` | Write destination Z. |

> **Alignment rule [TRM p.67–68]:** the blitter auto-aligns source to dest only
> for pixel sizes **≥ 8 bits**. If two source phrases must be read before one dest
> phrase can be written, **set SRCENX**.

### 4.2 Write-data source select (B_CMD bits 16–17) and B_PATD

Write data comes from one of [TRM p.67, p.74]:
- **LFU output** — the **default** (neither PATDSEL nor ADDDSEL set).
- **Pattern data** `B_PATD` — when `PATDSEL` (bit16) set [INC:276].
- **Adder output** (source+dest) — when `ADDDSEL` (bit17) set; signed-offset blend,
  16-bit pixels only [TRM p.74].
- **Computed Gouraud data** — implicit when `GOURD` (bit12) set.

Priority/override [TRM p.67]: a write-back-destination mechanism overrides all of
these when a write-inhibit fires in a mode that still must write (see §5).

`PATDSEL = $00010000` [INC:276]. For a **solid fill** you can either:
(a) preload the fill colour into `B_PATD` (4 copies of the 16-bit colour) and set
`PATDSEL`, **or** (b) preload `B_SRCD` with the colour, set `SRCEN`, and LFU =
REPLACE — the proven path uses **(b)** (§8). Both are valid; (b) lets the same
setup also do copies.

### 4.3 Gouraud shading (GOURD, B_IINC, B_I0..B_I3, TOPBEN/TOPNEN)

16-bit pixel mode only [TRM p.81]. Four pixels computed per inner pass:
- `B_SRCD` (64b) = the four 16-bit **intensity fractions** (Gouraud).
- `B_PATD` (64b) = the four 16-bit **intensity integers + colour** (the actual
  pixel values written).
- `B_IINC` (32b) = the intensity increment, 8.16 fixed (top 8 bits = colour, leave
  0) [TRM p.76].
- `B_I0..B_I3` = per-lane 8.16 convenience views that scatter into the
  `B_PATD`(integer) / `B_SRCD`(fraction) lanes [TRM p.77].

Per inner-loop pass with `GOURD` (bit12) set [TRM p.74, p.81]:
1. add the 16-bit **fraction** part of `B_IINC` (×4, one per lane) to the four
   intensity fractions in `B_SRCD`;
2. add the 8-bit **integer** part of `B_IINC` (×4) **with carry** from step 1 to
   the four intensity integers in `B_PATD`;
3. **carry is blocked from propagating intensity → colour** unless `TOPNEN`
   (bit15, carry into top nibble) / `TOPBEN` (bit14, carry into top byte) are set.
   **Leave TOPBEN/TOPNEN clear for CRY mode** [INC:274–275, TRM p.74].
4. intensity **saturates** (clamps at min/max, does not wrap) [TRM p.81].

Then `PATDSEL` selects `B_PATD` as the write data (the worked example sets
`PATDSEL` for Gouraud) [TRM p.82]. `SRCSHADE` (bit30) is an alternative flat-shade
mode that modulates *source* data by `B_IINC` (use with GOURZ, not GOURD; LFU must
select source) [TRM p.75].

### 4.4 Z-buffering (ZBUFF/GOURZ, ZMODE, B_Z0..B_Z3, B_ZINC, DSTENZ/DSTWRZ)

16-bit pixel mode only [TRM p.81]. **Naming note:** [INC:273] labels B_CMD bit13
`ZBUFF` ("polygon Z data updates"); [TRM p.74] labels the same bit13 `GOURZ`
("polygon Z data updates within the inner loop"). **Same bit, value `$00002000`.**

- `B_SRCZ1` (64b) = four 16-bit **Z integers**; `B_SRCZ2` (64b) = four 16-bit **Z
  fractions** [TRM p.76].
- `B_ZINC` (32b) = 16.16 Z increment [TRM p.76].
- `B_Z0..B_Z3` = per-lane 16.16 convenience views into `B_SRCZ1`/`B_SRCZ2`
  [TRM p.77].
- With bit13 set, each inner pass adds Z fraction inc to `B_SRCZ2`, then Z integer
  inc **with carry** to `B_SRCZ1`; Z **saturates** [TRM p.74, p.81].

**Z comparator (B_CMD bits 18–20, ZMODE):** inhibit the write under the selected
relation of **source Z vs destination Z** [INC:279–281, TRM p.74]. All-zero
disables the comparator. The bits **OR** together:

| Bit | `[INC]` | Inhibit when |
|-----|---------|--------------|
| 18 | `ZMODELT` `$00040000` | source **<** destination |
| 19 | `ZMODEEQ` `$00080000` | source **=** destination |
| 20 | `ZMODEGT` `$00100000` | source **>** destination |

> **Caution [TRM p.82 / errata p.?]:** the worked example sets `ZMODE = 3`
> ("overwrite if new Z ≥ existing"), i.e. it selects the *pass* condition, not the
> *inhibit* condition. Reconcile: the comparator **inhibits** on the selected
> relation; "ZMODE=3" there is shorthand for the field value, context-dependent.
> Treat the per-bit inhibit semantics above ([INC]) as authoritative; verify the
> exact polarity on BigPEmu (Open questions).

For Z to work you typically set `DSTENZ` (read screen Z), `DSTWRZ` (write new Z),
and a ZMODE, plus `DSTEN` to restore inhibited pixels [TRM p.82]. There is a known
errata: **Z comparators fail in pixel mode without BKGWREN** [TRM errata p.?,
search hit line 6420 "Z Comparators fail in pixel mode without BKGWREN"].

### 4.5 Data compare write-inhibit (BCOMPEN / DCOMPEN / CMPDST)

Three comparators [TRM p.68–69]:
- **Bit comparator** — for bit→pixel expansion (character/font paint). Selects a
  source bit via a counter reset each inner-loop entry; the bit gates the write.
  `BCOMPEN` (bit26, `$04000000`) enables write-inhibit from it. Whole-phrase mask
  only works in **8-bit** pixel mode; otherwise pixel-by-pixel [TRM p.69, p.74].
- **Data comparator** — transparent-colour copies / flood-fill search. Compares
  `B_SRCD` vs `B_PATD` (or `B_DSTD` vs `B_PATD` if `CMPDST` bit25 set). `DCOMPEN`
  (bit27, `$08000000`) enables inhibit. 8- and 16-bit modes only [TRM p.75].
- **Z comparator** — §4.4.

Inhibit behaviour [TRM p.74–75]:
- In **pixel mode** (XADD=01), an inhibited write does **not** happen **unless**
  `BKGWREN` (bit28) is set, in which case the destination data is written back
  (background colour from `B_DSTD`).
- In **phrase mode**, an inhibited pixel still gets a destination write — the
  comparator forces write-back of dest data, so you **must** have pre-read it via
  `DSTEN` (else garbage/background appears) [TRM p.69].

`CMPDST = $02000000` [INC:288], `BCOMPEN = $04000000` [INC:289], `DCOMPEN =
$08000000` [INC:290], `BKGWREN = $10000000` [INC:291].

---

## 5. The LFU (Logic Function Unit) — bits 21–24

The LFU computes a **bitwise boolean** of source (S) and destination (D), per bit
position, across the whole 64-bit phrase. Four B_CMD bits each enable one minterm;
the output is the **OR** of the enabled minterms [TRM p.74, INC:282–286]:

| Bit | `[INC]` | value | Minterm contributed |
|-----|---------|-------|---------------------|
| 21 | `LFU_NAN` | `$00200000` | `!S & !D` |
| 22 | `LFU_NA`  | `$00400000` | `!S &  D` |
| 23 | `LFU_AN`  | `$00800000` | ` S & !D` |
| 24 | `LFU_A`   | `$01000000` | ` S &  D` |

So with bits ordered `[A, AN, NA, NAN]` = `[S&D, S&!D, !S&D, !S&!D]`, the 4-bit
field is exactly the **truth table** of `f(S,D)` indexed `(S,D) = (1,1),(1,0),
(0,1),(0,0)`. All 16 boolean ops [INC:299–314]:

| Field (A AN NA NAN) | `[INC]` symbol | value | f(S,D) |
|---------------------|----------------|-------|--------|
| 0000 | `LFU_ZERO`     | `$00000000` | 0 |
| 0001 | `LFU_NSAND`    | `$00200000` | !S & !D |
| 0010 | `LFU_NSAD`     | `$00400000` | !S &  D |
| 0011 | `LFU_NOTS`     | `$00600000` | !S |
| 0100 | `LFU_SAND`     | `$00800000` |  S & !D |
| 0101 | `LFU_NOTD`     | `$00A00000` | !D |
| 0110 | `LFU_N_SXORD`  | `$00C00000` | !(S ^ D) |
| 0111 | `LFU_NSORND`   | `$00E00000` | !S | !D  (= !(S&D)) |
| 1000 | `LFU_SAD`      | `$01000000` |  S &  D |
| 1001 | `LFU_SXORD`    | `$01200000` |  S ^ D |
| 1010 | `LFU_D`        | `$01400000` |  D |
| 1011 | `LFU_NSORD`    | `$01600000` | !S | D |
| 1100 | `LFU_S`        | `$01800000` |  S  |
| 1101 | `LFU_SORND`    | `$01A00000` |  S | !D |
| 1110 | `LFU_SORD`     | `$01C00000` |  S | D |
| 1111 | `LFU_ONE`      | `$01E00000` | 1 |

Convenience aliases [INC:316–320]:
```
LFU_REPLACE = $01800000   ; output = Source        (= LFU_AN | LFU_A)
LFU_XOR     = $01200000   ; output = Source XOR Dest
LFU_CLEAR   = $00000000   ; output = 0
```

### 5.1 ⚠️ The `$01800000` warning (read this)

**Plain copy / replace = `LFU_REPLACE` = `$01800000` = bits 23+24
(`LFU_AN | LFU_A`).** `S = (S&!D) | (S&D)`. [INC:286,285,318; verified on BigPEmu,
reference backend].

> [porting notes §3]: "plain copy LFU = `$01800000`". **This exact value has been
> mis-derived TWICE** in derived sources: (1) a reference backend's original header had a
> wrong LFU *label*; (2) a 2026-06-11 "fix" moved `UPDA1` to `$400` from a bad
> reference (reverted 2026-06-12). [reference backend]: its
> platform header's `UPDA1` was actually `UPDA2` (bit10 not bit9), and its "LFU_B"
> was `LFU_N_SXORD` = `!(S^D)` (`$00C00000`) — which **inverts fills depending on
> stale destination-register state** and "sprayed `0xFFFF` over low RAM".

**Implementation guardrails:**
- `LFU_REPLACE` MUST be `$01800000` (bits 23,24). NOT `$00C00000`.
- `UPDA1` is bit **9** = `$00000200`. `UPDA2` is bit **10** = `$00000400`.
- A self-test: with `LFU_REPLACE` and `SRCEN`, output must equal source **exactly,
  independent of destination contents.** If output depends on D, the field is
  wrong (you have `!(S^D)` or similar).

---

## 5.3 Outer-loop pointer updates (UPDA1F / UPDA1 / UPDA2 / DSTA2)

[INC:268–271, TRM p.73–74]:

| Bit | `[INC]` | value | Action between inner-loop passes (outer loop) |
|-----|---------|-------|-----------------------------------------------|
| 8  | `UPDA1F` | `$00000100` | Add `A1_FSTEP` fraction to A1 pointer fraction. |
| 9  | `UPDA1`  | `$00000200` | Add `A1_STEP` (integer) to A1 pointer. |
| 10 | `UPDA2`  | `$00000400` | Add `A2_STEP` to A2 pointer. |
| 11 | `DSTA2`  | `$00000800` | **Swap roles**: A2=destination, A1=source. |

Each enabled update costs **one extra tick per outer iteration** — only enable
when the blit is >1 row [TRM p.73]. A single-line span sets **none** of these
(outer count = 1) [reference backend]. A multi-row
band sets `UPDA1` and programs `A1_STEP` to rewind X and advance Y one line
[reference backend].

---

## 5.4 Collision stop (B_STOP)

[TRM p.77, INC:245]. `B_STOP` (`$F02278`): bit0 `RESUME`, bit1 `ABORT`, bit2
`STOPEN`. With `STOPEN` set, the blitter **stops** on an inner-loop write-inhibit
when in pixel mode (XADD=01), `BKGWREN` clear, and a matching BCOMPEN/DCOMPEN/ZMODE
condition fires. Status bit1 `STOPPED` goes high. Write `RESUME` to continue or
`ABORT` to terminate to idle. Used for collision detection / search. Not needed
for copy/fill.

---

## 6. Starting a blit & detecting completion

**Writing `B_CMD` (`$F02238`) starts the blitter** — so it must be the **last**
register written when setting up a command [TRM p.73]. The blitter then runs
**autonomously** on the coprocessor bus until the whole operation completes
[TRM p.69]. (Bit7 `NOGO` = diagnostic, prevents the start; keep 0 [TRM p.73].)

**Completion** = poll the **read** side of `$F02238` (status): **bit0 `IDLE`** set
⇒ blitter completely idle, last bus transaction done [TRM p.75]. There is also a
**blitter interrupt** path (Tom INT, `G_BLITLAT`/`B_BLITCLR` in [INC:201,214]) for
interrupt-driven sync.

**The golden synchronization rule** [reference backend, porting notes §3]:

> **Wait-for-idle BEFORE setup, NEVER after start.** Fire-and-forget. The blitter
> runs while the CPU/GPU does the next polygon. Do **not** spin-poll immediately
> after `B_CMD`.

Proven idle wait [reference backend]:
```
while (!(B_CMD & 0x1)) ;        // BLIT_IDLE = bit0
```
On BigPEmu, busy-polling DRAM while the coprocessor runs starves its thread; idle
polling of the **register** is fine, but the architecture should sync loosely (one
big autonomous job + interrupt), not per-primitive [porting notes §1, §3].

**jsim's model of the settle window** [jag_rr, 2026-08-16]: on silicon the
blitter takes several cycles after the `B_CMD` store before BUSY is
observable, so a poll issued immediately after start reads a **stale IDLE** and
the next register write lands inside a running blit. jsim arms `blit_busy` at
`B_CMD`-write time, so by default BUSY appears instantaneously and **this
hazard is invisible here** — a divergence in the forgiving direction, meaning
poll-after-start bugs only ever surface on hardware.

The `bcmd_poll_in_settle` counter always reports polls landing inside the
window. Set **`JAGEMU_BLIT_SETTLE=1`** to also reproduce the behaviour, so such
a poll reads IDLE as it would on silicon. It is off by default because every
kernel in the corpus was written against the forgiving model, and switching it
on globally would break working code to expose a hazard those programs may not
have.

**Short-span optimization** [reference backend]: spans ≤ `SPAN_CPU_LIMIT` (=12
pixels) are filled by the CPU directly — register setup (≈8–10 writes) + the idle
poll costs more than a dozen word stores. Spec doesn't mandate this; it's a
program-side choice.

---

## 7. Status register (read `$F02238`)

[TRM p.75]. Implement at least bit0; bits 2–31 are diagnostic but BigPEmu may
expose them.

| Bit | Name | Meaning |
|-----|------|---------|
| 0 | `IDLE` | Blitter idle, last bus transaction complete. **The completion flag.** |
| 1 | `STOPPED` | Stopped in collision-detect mode (§5.4). |
| 2 | inner IDLE | diagnostic |
| 3–10 | inner SREADX/SZREADX/SREAD/SZREAD/DREAD/DZREAD/DWRITE/DZWRITE | diagnostic inner-state |
| 11 | outer IDLE | diagnostic |
| 12 | outer INNER | diagnostic |
| 13–15 | outer A1FUPDATE/A1UPDATE/A2UPDATE | diagnostic |
| 16–31 | inner count | live inner counter (diagnostic) |

For emulation: drive bit0=1 when idle, 0 while a blit is in flight. If you model
the blitter as instantaneous-at-`B_CMD` (acceptable initially), bit0 reads 1
immediately after — that satisfies the proven idle loop, since programs only
*wait* (they never depend on observing it *busy*). For cycle accuracy, hold bit0
low for the computed duration (§9) and expose bits 16–31 = remaining inner count.

---

## 8. Inner-loop algorithm (reference pseudocode)

This is the per-blit execution model. `dst`/`src` resolve through whichever of
A1/A2 is destination/source after `DSTA2`. Big-endian throughout.

```
fn run_blit(cmd, /* registers latched at B_CMD write */):
    (dst_gen, src_gen) = if cmd.DSTA2 { (A2, A1) } else { (A1, A2) }

    inner0 = decode_count(B_COUNT & 0xFFFF)        // 0 -> 65536
    outer  = decode_count(B_COUNT >> 16)           // 0 -> 65536

    for o in 0..outer:
        // reload inner counter each outer pass
        reset_bit_compare_counter()                // §4.5 bit comparator
        for i in 0..inner0:
            // 1. reads (gated)
            if cmd.SRCENX && i==0: src_prev = read_phrase(src_gen); adv_src()
            if cmd.SRCEN:  src = read(src_gen) [+ SRCENZ ? read_srcZ]
            if cmd.DSTEN:  dst_old = read(dst_gen)
            if cmd.DSTENZ: dstZ = read_destZ(dst_gen)

            // 2. align source to dest (pixels >= 8 bits) §4.1
            src = align(src, src_prev, src_gen.phase, dst_gen.phase)

            // 3. choose write data §4.2
            wd = if cmd.GOURD      { gouraud_step() ; B_PATD }   // §4.3
                 else if cmd.PATDSEL { B_PATD }
                 else if cmd.ADDDSEL { sat_add16(src, dst_old) } // §4.2
                 else              { lfu(cmd.LFU, src, dst_old) }// §5

            // 4. write-inhibit decisions §4.4/§4.5
            inhibit = false
            if cmd.ZMODE != 0 && pixel16:
                if z_relation(src_or_computed_Z, dstZ) matches ZMODE: inhibit=true
            if cmd.BCOMPEN: if bit_comparator_says_skip(): inhibit=true
            if cmd.DCOMPEN: if data_comparator_says_skip(src/dst, B_PATD): inhibit=true
            if cmd.CLIP_A1 && a1_outside_window(): inhibit=true   // §2.5

            // 5. write (always for dest unless inhibited per mode rules §4.5)
            if !inhibit:
                write(dst_gen, wd)
                if cmd.GOURZ /*ZBUFF bit13*/ && cmd.DSTWRZ: write_destZ(computed_Z)
            else:
                if pixel_mode && cmd.BKGWREN: write(dst_gen, B_DSTD) // background
                if phrase_mode:              write(dst_gen, dst_old) // restore
                // collision stop §5.4 may fire here if STOPEN

            // 6. advance inner pointers §3.5/3.6
            advance_X(dst_gen, src_gen, cmd)   // XADDPHR/PIX/0/INC, X sign
            advance_Y_inner(cmd)               // YADD, Y sign

        // 7. outer step (gated) §5.3
        if cmd.UPDA1F: A1.frac  += A1_FSTEP
        if cmd.UPDA1:  A1.ptr   += A1_STEP
        if cmd.UPDA2:  A2.ptr   += A2_STEP

    set_status_idle()
```

Phrase-mode (`XADDPHR`) writes a full phrase (up to 4 px @16bpp) per inner step,
truncating X to the phrase boundary; pixel-mode (`XADDPIX`) writes one pixel and
X+=±1. Comparator/inhibit logic in phrase mode operates per-pixel within the
phrase, forcing dest write-back for skipped pixels (§4.5).

---

## 9. Timing (cycle model)

[TRM p.68–69]:
- The blitter cycles the coprocessor (64-bit DRAM) bus "at a rate limited only by
  external memory speed."
- **One-tick overhead turning a read→write** transfer.
- **One extra tick per outer-loop pointer update** (each of UPDA1F/UPDA1/UPDA2)
  [TRM p.73].
- Holds the bus for the **entire** operation; higher-priority masters (OP, or with
  `BUSHI`/`BUSHI`=bit29 the blitter itself outranks OP) can preempt and suspend it
  [TRM p.69, p.75; INC:292 `BUSHI=$20000000`].

(UNVERIFIED) Exact per-phrase tick counts for read, write, fill, and the
RMW Z/Gouraud path are not enumerated in the local TRM text. For first-pass
emulation, a defensible model: **N_phrases × (read_ticks + write_ticks) +
overheads**, with the bus at the DRAM rate. Cross-check against BigPEmu's timing
or a hardware reference before claiming cycle accuracy. See Open questions.

---

## 10. Worked register recipes (proven)

### 10.1 Solid horizontal span (single line) — VERIFIED [reference backend]

Fill `n` pixels of 16bpp at `(x0,y)` on a 320-wide surface with colour `c`:
```
cc = (c << 16) | c                      // replicate 16-bit colour into 32 bits
wait_idle()                             // BEFORE setup
A1_BASE  = fb                           // phrase-aligned framebuffer base
A1_FLAGS = PIXEL16 | WID320 | XADDPIX   // $00000020 | $00004200 | $00010000
                                        //   = $00014220
A1_PIXEL = (y << 16) | x0               // Y high, X low
B_SRCD   = cc ; B_SRCD1 = cc            // fill data in BOTH longs of the 64b reg
B_COUNT  = (1 << 16) | n                // outer=1 row, inner=n pixels
B_CMD    = LFU_REPLACE                  // $01800000 ; SRCEN NOT needed here*
```
*Proven code sets only `LFU_REPLACE`. With LFU=Source the engine takes source data
from `B_SRCD` even without `SRCEN` in this path on BigPEmu. (UNVERIFIED on real
HW whether SRCEN is strictly required; if a port misbehaves, add `SRCEN`.)

### 10.2 Solid rectangle / band (multi-row) — VERIFIED [reference backend]

Fill rows `[y0, y1)` full-width (`RENDER_W`) with colour `c`:
```
cc = (c << 16) | c
wait_idle()
A1_BASE  = fb
A1_FLAGS = PIXEL16 | WID320 | XADDPIX
A1_PIXEL = (y0 << 16)                          // X = 0
A1_STEP  = (1 << 16) | ((-RENDER_W) & 0xFFFF)  // Ystep=+1, Xstep=-RENDER_W
B_SRCD   = cc ; B_SRCD1 = cc
B_COUNT  = ((y1 - y0) << 16) | RENDER_W        // outer rows, inner = width
B_CMD    = UPDA1 | LFU_REPLACE                 // $00000200 | $01800000
```
After each row, `UPDA1` adds `A1_STEP`: X rewinds by `RENDER_W` (back to column 0)
and Y advances by 1 [reference backend].

### 10.3 GPU-side span (phrase-mode bands) — VERIFIED [reference backend]

```
A1F_VAL  = $00014220   ; PIX16 | WID320 | XADDPIX  (pixel mode, exact spans)
A1F_PHR  = $00004220   ; PIX16 | WID320 | XADDPHR  (phrase mode, 4 px/cycle, bands)
BCMD_SPAN= $01800000   ; LFU = Source (B_SRCD)
```
Note `$4220 = WID320 ($4200) | PIXEL16 ($20)`; add `XADDPIX ($10000)` → `$14220`.
Both `B_SRCD` longs are loaded with the colour before the loop
[reference backend]. The idle wait is `btst #0` on `B_CMD`
[reference backend].

### 10.4 Block (1-D) copy — from TRM example [TRM p.78]

```
A2_BASE = src (phrase-aligned; offset goes in A2 X ptr)
A1_BASE = dst (phrase-aligned; offset in A1 X ptr); Y=0 both
B_COUNT = (1<<16) | length_in_pixels   // outer=1
A1_FLAGS / A2_FLAGS: XADD = 00 (phrase mode) for both
B_CMD   = SRCEN | LFU_REPLACE          // + SRCENX if source not phrase-aligned
```

### 10.5 Gouraud + Z strip — from TRM example [TRM p.81–82]

18-pixel 16bpp shaded Z-buffered strip:
```
A1: BASE=$01600000, PITCH=1 (pixel/Z interleave), PIXEL=4(16bpp), ZOFFS=1,
    WIDTH=$11 (20px = 1.01×2^4), XADD=00, X=1,Y=0
B_PATD = 00DC00C700B1009C   // intensity integers + colour, 4 lanes
B_SRCD = FEDCEAC7D6B1C29C   // intensity fractions, 4 lanes
B_SRCZ1= FFFFE7E7CFCFB7B7   // Z integers
B_SRCZ2= FFFFE000C001A002   // Z fractions
B_IINC = FFA9B66C ; B_ZINC = 9F9F8004
B_COUNT= (1<<16)|18
B_CMD  = DSTEN|DSTENZ|DSTWRZ|CLIP_A1|GOURD|GOURZ|PATDSEL|ZMODE(3)
```

---

## 11. Implementation guardrails (sticky list)

1. `LFU_REPLACE = $01800000` (bits 23,24). Never `$00C00000`. Self-test: output
   must equal source regardless of dest (§5.1).
2. `UPDA1 = $200` (bit9), `UPDA2 = $400` (bit10), `UPDA1F = $100` (bit8),
   `DSTA2 = $800` (bit11). [INC:268–271]
3. `B_COUNT = outer<<16 | inner`; **0 means 65536** in each half.
4. Blitter regs are **32-bit**; data regs are 64-bit = two longs (high at the SDK
   equate, low at +4).
5. Fill colour goes in **both** longs of `B_SRCD` (`B_SRCD`/`B_SRCD1`).
6. `B_CMD` write **starts** the blit; read bit0 = IDLE for completion.
7. Sync rule: **wait-for-idle before setup, never after start** (fire-and-forget).
8. `A1_BASE`/`A2_BASE` must be **phrase-aligned**; sub-phrase offset goes in the X
   pointer.
9. `A1_PIXEL = Y<<16 | X` (Y high, X low). Same for `A2_PIXEL`, `A1_STEP`,
   `A1_INC`, `A2_STEP`.
10. Gouraud/Z/ADDDSEL are **16-bit pixel mode only**.

---

## 12. Open questions (validate against BigPEmu / hardware)

1. **(UNVERIFIED) Exact cycle counts** per phrase read/write, the read→write
   turnaround tick, RMW Z/Gouraud cost, and bus-priority preemption timing
   (§9). Local TRM gives only qualitative "memory-limited + 1 tick" hints. Needed
   for true cycle accuracy.
2. **(UNVERIFIED) Whether `SRCEN` is strictly required** for the LFU=REPLACE
   fill path (§10.1). Proven code omits it on BigPEmu; real-HW behaviour unknown.
3. **(UNVERIFIED) ZMODE polarity:** [INC] bits = *inhibit* conditions; the TRM
   worked example says "overwrite if ≥" (a *pass* condition). Confirm whether
   ZMODELT/EQ/GT select the inhibit relation or the write relation (§4.4).
4. **(UNVERIFIED) B_I0..B_I3 / B_Z0..B_Z3 → 64-bit register lane byte mapping.**
   [INC] orders the equates descending (I3 at lowest addr); confirm which lane of
   `B_PATD`/`B_SRCD`/`B_SRCZ1`/`B_SRCZ2` each updates and the byte positions
   (§1, §4.3).
5. **(UNVERIFIED) Phrase-mode comparator/inhibit interaction with PITCH/ZOFFS** —
   exact dest write-back behaviour for partially-inhibited phrases (§4.5).
6. **(UNVERIFIED) Bit13 name:** `ZBUFF` [INC] vs `GOURZ` [TRM] — same bit
   `$00002000`; the spec treats them as identical (Z-data update enable). Confirm
   no separate ZBUFF-vs-GOURZ distinction exists.
7. **(UNVERIFIED) `blit_band` outer-count off-by-one** (§1.1) — confirm whether
   BigPEmu fills `[y0,y1)` or `[y0,y1]` with `outer = y1-y0`. Program-side, not a
   blitter rule, but worth a screenshot check.
8. **Errata:** "Z Comparators fail in pixel mode without BKGWREN" appears in the
   TRM errata (text line ~6420). Confirm page and whether it affects the intended
   Z path (§4.4).
