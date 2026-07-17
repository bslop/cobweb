# Tom Object Processor (OP) — Implementation Spec

Implementation-grade specification for the Atari Jaguar **Tom Object Processor**
(the video compositor) for a from-scratch, cycle-accurate Rust emulator.

**Scope:** the OP is the unit that produces the *actual on-screen image* — the
"true scan-out." It walks a display list of 64-/128-/192-bit object "phrases"
once per scanline, expands pixels into a line buffer, and the pixel-generator
reads that line buffer to the display. **The emulator's screenshot/parity path
MUST capture this OP scan-out, not the DRAM the 68000 wrote.** A headless dump of
the framebuffer DRAM looks correct while the screen is wrong (e.g. squished mode,
truncated link list). This is called out repeatedly below; see
the internal porting notes.

**Endianness:** Jaguar is **big-endian**. Object phrases are stored as two (or
three) 32-bit longwords, MSB-first. Throughout this doc, "high long" = the
longword at the *lower* byte address (it holds the more-significant 32 bits of
the 64-bit phrase, bits 63:32); "low long" = the longword at the *higher* byte
address (bits 31:0). When reading from a `&[u8]` cartridge/DRAM image, assemble
each long as `(b[0]<<24)|(b[1]<<16)|(b[2]<<8)|b[3]`.

### Source legend
- **[INC]** `…/INCLUDE/JAGUAR.INC` — official Atari SDK equates (authoritative).
- **[TRM p.N]** Jaguar Technical Reference Manual, Revision 8 (`Jaguar Technical
  Reference v8.pdf`, 141 pages). Page numbers are the *printed* manual page (the
  "Page N" footer), which run ~6 ahead of the PDF sheet number.
- **[reference backend]** — known-good homebrew OP setup proven on
  BigPEmu and real hardware.
- **[the internal porting notes]** — cross-project
  accuracy notes.
- **(UNVERIFIED)** = inference / not directly in the official docs. Validate
  against BigPEmu. Collected in *Open Questions*.

---

## 1. Register map (Tom video, all base $F00000)

All of these are **16-bit** registers except **OLP** (32-bit). **Never access
the 16-bit ones as 32-bit** — a 32-bit `VMODE` store spills its high word into
BORD1 and zeroes PWIDTH, squishing the whole frame into a ~70px strip.
[the internal porting notes]

| Name  | Addr     | W  | Access | Purpose | Source |
|-------|----------|----|--------|---------|--------|
| OB0   | $F00010  | 16 | RO     | Current object phrase, word 0 (bits 63:48) | [INC:61], [TRM p.12] |
| OB1   | $F00012  | 16 | RO     | Current object phrase, word 1 (bits 47:32) | [INC:62] |
| OB2   | $F00014  | 16 | RO     | Current object phrase, word 2 (bits 31:16) | [INC:63] |
| OB3   | $F00016  | 16 | RO     | Current object phrase, word 3 (bits 15:0)  | [INC:64] |
| OLP   | $F00020  | 32 | WO     | **Object List Pointer** (start of list) | [INC:65], [TRM p.12] |
| OBF   | $F00026  | 16 | WO     | Object Processor Flag (restart / branch flag) | [INC:66], [TRM p.12] |
| VMODE | $F00028  | 16 | WO     | Video mode (VIDEN/MODE/PWIDTH/…) | [INC:67], [TRM p.12-13] |
| BORD1 | $F0002A  | 16 | WO     | Border colour, Red (low byte) & Green (high byte) | [INC:68], [TRM p.14] |
| BORD2 | $F0002C  | 16 | WO     | Border colour, Blue (low byte) | [INC:69], [TRM p.14] |
| HDB1  | $F00038  | 16 | WO     | Horizontal Display Begin 1 (11-bit) — OP start trigger | [INC:70], [TRM p.14] |
| HDB2  | $F0003A  | 16 | WO     | Horizontal Display Begin 2 (11-bit) — 2nd OP start | [INC:71], [TRM p.14] |
| HDE   | $F0003C  | 16 | WO     | Horizontal Display End (11-bit) | [INC:72], [TRM p.14] |
| VDB   | $F00046  | 16 | WO     | Vertical Display Begin (half-lines) | [INC:74] |
| VDE   | $F00048  | 16 | WO     | Vertical Display End (half-lines) | [INC:75] |
| VI    | $F0004E  | 16 | WO     | Vertical Interrupt line (half-lines) | [INC:76] |
| BG    | $F00058  | 16 | WO     | Background colour (CRY) line-buffer clear value | [INC:79], [TRM p.17] |
| INT1  | $F000E0  | 16 | RW     | CPU interrupt control (bit 2 = Object/STOP int) | [INC:81], [TRM p.16-17] |
| HC    | $F00004  | 16 | RO     | Horizontal count (HC bit 10 = "second half of line") | [INC:57] |
| VC    | $F00006  | 16 | RO     | Vertical count, in **half-lines** — the OP's YPOS comparand | [INC:58] |
| CONFIG/CLK| $F00036 | 16 | RW   | Read: bit 4 = 1 NTSC / 0 PAL (this addr is also HVS WO) | [reference backend], [the internal porting notes] |
| CLUT  | $F00400  | 16×256 | RW | Colour Look-Up Table, 256 × 16-bit entries | [INC:84], [TRM p.17] |
| LBUFA | $F00800  | —  | RW     | Line buffer A (test access) | [INC:86], [TRM p.17] |
| LBUFB | $F01000  | —  | RW     | Line buffer B (test access) | [INC:87], [TRM p.17] |
| LBUFC | $F01800  | —  | RW     | Currently-writing line buffer (GPU helper access) | [INC:88], [TRM p.17] |

**OLP word-swap gotcha:** the 68000 writes OLP as two 16-bit halves. The proven
code writes it **with its halves swapped**: `OLP = (olp >> 16) | (olp << 16)`
[reference backend].
This is a property of how the address bus presents the two words to the register,
not of the OP itself. **For the emulator's register model:** treat $F00020 as a
single 32-bit value the OP consumes directly (so a 32-bit write at $F00020 stores
the value verbatim). The swap is the *guest software's* concern when it writes
two words; emulate the bus faithfully and it falls out. If you model OLP as two
16-bit subregisters OLPLO=$F00020 / OLPHI=$F00022 [INC:675-676], the OP must
recombine them. **(UNVERIFIED which half lands at which address — validate by
reading back the swap in a reference backend's working setup; the working code's net effect is
that the *final 32-bit OLP* equals the list base address.)**

**OBF semantics:** bit 0 is the OP branch flag tested by `O_BROP` branch objects
and set/cleared by the GPU. **Any write to OBF restarts the OP** after a GPU
interrupt object [TRM p.12 lines 500-503]. `C_OPENA=$0004` [INC:37] is the GPU
control-register bit that enables the OP→GPU interrupt.

---

## 2. Display-list model

The OP holds an internal **current-object pointer**, initialised from **OLP** at
the start of each OP run. It reads the object at that pointer, dispatches on its
**TYPE** field (low 3 bits of the first phrase, bits 2:0), acts, then advances:
- BITMAP / SCALED BITMAP: process, then continue at **LINK**.
- BRANCH: continue at **LINK** (if condition true) or at the *next phrase in
  memory* (if false).
- GPU: stall until the GPU writes OBF, then continue at the *next phrase*.
- STOP: end the OP run for this line.

**When does the OP run?** Once (or twice) per **display line**, triggered when the
horizontal counter HC matches **HDB1** (and again at **HDB2** for split/dual
line-buffer modes). At that instant: the OP starts execution at OLP, the two line
buffers swap, and pixels begin shifting out of the now-displayed buffer. [TRM
p.14 lines 610-619]. For a single-pass 320-wide mode, HDB1==HDB2 (or HDB2 > line
length) so the OP runs **once per line**. [TRM p.14 line 618-619]

**The OP re-walks the WHOLE list every scanline.** It re-reads OLP and follows
LINK fresh each line. It does **not** cache the list. Two consequences:
1. A bogus LINK (e.g. truncated to fewer bits) makes the OP wander into
   non-zero memory and draw garbage *every line* — latent until something real
   occupies the bad address. [the internal porting notes]
2. The VI (vertical interrupt) fires at **vdb-2**, *before* the display field, so
   the ISR can **rebuild the object list before the OP reaches the YPOS line** —
   "the OP destroys it." [reference backend]. "Destroys" = the OP writes
   modified headers back (HEIGHT decremented, DATA advanced — see §5), so a
   static list left in DRAM will not survive a frame intact; the standard pattern
   rebuilds it from scratch each vblank. [reference backend]

### Object alignment / phrase boundary
- Every object starts on a **phrase (8-byte / 64-bit) boundary** → bottom 3 bits
  of any object address are 0. [TRM p.12 line 494, p.19 line 782]
- **Scaled** bitmap objects must start on a **32-byte boundary**. [TRM p.20 line
  857] (i.e. the 3-phrase scaled header is 24 bytes but alignment is 32.)
  **(UNVERIFIED why 32 not 24 — likely a write-back/DMA alignment constraint;
  emulator need not enforce, just parse from the object's address.)**

### Phrase / longword extraction (big-endian)
Given an object at byte address `A` in the 24-bit address space:
```
p0_hi = read_u32_be(A + 0)   // phrase[0] bits 63:32
p0_lo = read_u32_be(A + 4)   // phrase[0] bits 31:0
p1_hi = read_u32_be(A + 8)   // phrase[1] bits 63:32
p1_lo = read_u32_be(A + 12)  // phrase[1] bits 31:0
// scaled adds phrase[2] at A+16 / A+20
```
A field spanning bits `[hi:lo]` of the 64-bit phrase is recovered as:
```
phrase64 = (p_hi as u64) << 32 | (p_lo as u64);
field    = (phrase64 >> lo) & ((1u64 << (hi - lo + 1)) - 1);
```
This is the **authoritative** way to decode fields; the split-long formulas
below (§3.1) are just the same thing expressed for guest code that builds the
phrase one 32-bit long at a time.

---

## 3. Object phrase formats

TYPE field = phrase[0] bits **2:0**:

| TYPE | Name           | Equate          | Header size | Source |
|------|----------------|-----------------|-------------|--------|
| 0    | BITMAP         | `BITOBJ=0`      | 2 phrases (16 B) | [INC:94] |
| 1    | SCALED BITMAP  | `SCBITOBJ=1`    | 3 phrases (24 B, 32-B aligned) | [INC:95] |
| 2    | GPU            | `GPUOBJ=2`      | 1 phrase (8 B) | [INC:96] |
| 3    | BRANCH         | `BRANCHOBJ=3`   | 1 phrase (8 B) | [INC:97] |
| 4    | STOP           | `STOPOBJ=4`     | 1 phrase (8 B) | [INC:98] |

### 3.1 BITMAP object (TYPE 0) — [TRM p.19-20 lines 780-853]

**First phrase** (64 bits):

| Bits  | Field  | Width | Meaning |
|-------|--------|-------|---------|
| 2:0   | TYPE   | 3  | = 0 |
| 13:3  | YPOS   | 11 | Vertical position in **half-lines**. Object active while `VC >= YPOS && HEIGHT > 0`. Even for even lines / odd for odd lines if interlaced; always even if non-interlaced. |
| 23:14 | HEIGHT | 10 | Number of data lines remaining. Decremented per displayed line (by 1 non-interlaced, 2 interlaced; clamped at 0). Written back. |
| 42:24 | LINK   | 19 | Phrase address of next object; replaces bits **21:3** of OLP (so within the same 4 MB). |
| 63:43 | DATA   | 21 | Phrase address of pixel data; defines bits **23:3** of the data address (anywhere in 16 MB). Written back (advanced per line). |

**Field bit-positions are VERIFIED from [TRM p.19].** Note the TRM text says
"YPOS bits 3-13" and "HEIGHT bits 14-23" (an 11-bit YPOS and a 10-bit HEIGHT);
this matches the reference backend code below.

**Second phrase** (64 bits):

| Bits  | Field    | Width | Meaning |
|-------|----------|-------|---------|
| 11:0  | XPOS     | 12 | Signed X of first pixel in line buffer (range -2048..+2047). 0 = left-most line-buffer pixel. |
| 14:12 | DEPTH    | 3  | bits/pixel: 0=1, 1=2, 2=4, 3=8, 4=16, 5=24. [INC:105-110] |
| 17:15 | PITCH    | 3  | Phrase stride of *next phrase within a line*: `data += 8*PITCH` bytes. 1=contiguous, 0=repeat same phrase, >1=skip embedded (e.g. Z) data. |
| 27:18 | DWIDTH   | 10 | Data width in phrases. Next line of pixels at `8*(DATA + DWIDTH)` bytes. |
| 37:28 | IWIDTH   | 10 | Image width in phrases (**must be non-zero**); usable for clipping. |
| 44:38 | INDEX    | 7  | CLUT base / palette MSBs for 1–4 bpp objects (see §6.2). |
| 45    | REFLECT  | 1  | Draw right→left (horizontal flip); line-buffer address decrements. |
| 46    | RMW      | 1  | Read-modify-write: add object colour to existing line-buffer value (signed I + two colour vectors). Halves write rate. |
| 47    | TRANS    | 1  | Transparency: logical colour 0 (and reserved physical colours) not written. |
| 48    | RELEASE  | 1  | Release the bus between data fetches (low-colour objects). **Emulator: timing-only; no pixel effect.** |
| 54:49 | FIRSTPIX | 6  | First pixel to display (clip left). LSB only meaningful for scaled. In 1bpp all 6 bits significant; in 2bpp only top 4; 0 = whole phrase. |
| 63:55 | —        | 9  | Unused, write 0. |

**Split-long encoding (for cross-check with guest code).** The reference backend list builder
[reference backend] constructs the BITMAP first phrase from `fb_addr` (a *byte*
address) and `link` (a *phrase* address = `(&next) >> 3`):
```c
op_list[0] = (fb_addr << 8) | (link >> 8);   // high long (bits 63:32)
op_list[1] = (link << 24)                     // low long (bits 31:0)
           | (RENDER_H << 14)                 // HEIGHT
           | (BASE_Y  <<  4);                 // YPOS (BASE_Y*2 half-lines, *2 from <<4 vs <<3... see note)
```
Decoding `op_list[0] = (fb_addr<<8) | (link>>8)`:
- high-long bits 31:8 = `fb_addr[23:0]`… but only the top bits matter: this
  places **DATA** (phrase bits 63:43 → high-long bits 31:11) and the **top of
  LINK** (high-long bits 10:0 = `link[18:8]`). i.e. **`fb_addr << 8`** puts the
  *byte* framebuffer address such that its phrase part lands in DATA; **`link >>
  8`** drops `link`'s low 8 bits into high-long bits 10:0.
- low-long bits 31:24 = `link << 24` = **`link[7:0]`** (the remaining 8 LINK
  bits), low-long bits 23:14 = HEIGHT, low-long bits 13:4 = YPOS (`BASE_Y<<4`).

This **confirms the LINK split called out in [the internal porting notes]**:

> **LINK is 19 bits = phrase bits 42:24, split across both longs: high long bits
> 10:0 = link[18:8], low long bits 31:24 = link[7:0].** Masking the high part
> with `$FF` instead of `$7FF` truncates lists at ≥ $80000.

For the emulator, do **not** reconstruct via the split formulas — just extract
LINK = `(phrase64 >> 24) & 0x7FFFF` and form the next-object byte address as:
```
next_obj = (OLP & ~0x3FFFFF) | (LINK << 3)   // LINK replaces OLP bits 21:3
```
i.e. LINK is a **phrase** index; multiply by 8 for bytes, and it overrides OLP
bits 21:3 (22-bit window = 4 MB), preserving OLP bits 23:22. **(UNVERIFIED
exactly which OLP bits survive: TRM says "bits 3 to 21" are replaced [p.12 line
495]; so OLP bits 23:22 persist, giving the "same 4 MB" wording [p.19 line 798].
a reference backend keeps the whole list well under 4 MB so it never exercises the high bits —
validate against BigPEmu with a list crossing a 4 MB boundary.)**

Similarly DATA = `(phrase64 >> 43) & 0x1FFFFF` is a **phrase** address; the byte
data address = `DATA << 3` (21 phrase bits → bits 23:3 of a 24-bit byte
address). [TRM p.19 lines 799-802]

> **YPOS note:** a reference backend writes `BASE_Y << 4` into the first long, but YPOS is
> phrase bits 13:3 = low-long bits 13:3. `BASE_Y << 4` = `BASE_Y*2 << 3`, i.e.
> it stores **`BASE_Y*2` in half-line units** at the YPOS position. This matches
> [reference backend] "YPOS in half-lines (BASE_Y\*2)" and "HEIGHT in lines." So for
> a top-of-object screen line `Y`, the field value is `Y*2` (non-interlaced).
> HEIGHT is in **whole lines** (`RENDER_H << 14` → field 23:14). **VERIFIED via
> a reference backend + TRM agreement.**

**Data-pointer ≥ $100000 gotcha:** the *initial parsed* data pointer can
silently break for some byte addresses with **bit 16 set** (observed under
BigPEmu: `$102000` and `$1C0000` work; `$159780` reads other memory) while the
per-line increment traverses high addresses fine. [the internal porting notes]. This is a
**BigPEmu quirk to be aware of for parity**, *not* a hardware behaviour to
replicate — a from-scratch emulator should decode DATA as the full 21-bit phrase
address with no bit-16 anomaly, and flag any divergence from BigPEmu here as an
*emulator* difference, not a bug. (Listed in Open Questions.)

### 3.2 SCALED BITMAP object (TYPE 1) — [TRM p.20 lines 855-878]

First 128 bits **identical to BITMAP** (TYPE=1). One extra (third) phrase:

| Bits  | Field     | Width | Meaning |
|-------|-----------|-------|---------|
| 7:0   | HSCALE    | 8 | 3-bit integer + 5-bit fraction. Pixels written to line buffer per source pixel (horizontal zoom). 0x20 = 1.0× (1 dest per src). |
| 15:8  | VSCALE    | 8 | 3-bit integer + 5-bit fraction. Display lines drawn per source line (vertical zoom). =HSCALE keeps aspect. 0x20 = 1.0×. |
| 23:16 | REMAINDER | 8 | 3-bit integer + 5-bit fraction. Lines left to draw from current source line. Decremented by 1 (one *whole*, i.e. `0x20`) per display line; when it goes negative, add VSCALE until positive, decrementing HEIGHT each add. Written back. |
| 63:24 | —         | 40 | Unused, write 0. |

**Scaling math (VERIFIED text [TRM p.20 lines 868-874]):**
- **Horizontal:** scaled objects write **one pixel per cycle** (not pairs). The
  line-buffer address advances independently of the source-pixel counter; if the
  LB address increments at HSCALE relative to the source counter, the image is
  HSCALE× wide. Implement as a 3.5 fixed-point accumulator: emit dest pixels
  while consuming source pixels at rate `1/HSCALE`. [TRM p.23 lines 1018-1020]
- **Vertical:** per display line, `REMAINDER -= 0x20` (one whole line). While
  `REMAINDER < 0`: `REMAINDER += VSCALE`; `HEIGHT -= 1` (advance source line by
  one DWIDTH stride). So VSCALE < 0x20 repeats source lines (zoom up), VSCALE >
  0x20 skips them (zoom down). **(UNVERIFIED that the decrement step is exactly
  `0x20`/"one whole" vs `1` LSB; TRM says "decremented by one" but the field is
  3.5 fixed-point, so "one" = one integer unit = 0x20. Validate against
  BigPEmu.)**

The write-back for scaled objects advances DATA "by a multiple of the data
width" and modifies REMAINDER. [TRM p.22 lines 985-987]

### 3.3 GPU object (TYPE 2) — [TRM p.20 lines 881-895]

Single phrase. Interrupts the GPU so it can act on the OP's behalf
(palette load, perspective, etc.). The OP **resumes when the GPU writes OBF**.

| Bits  | Field | Width | Meaning |
|-------|-------|-------|---------|
| 2:0   | TYPE  | 3  | = 2 |
| 13:3  | YPOS  | 11 | Active when `VC == YPOS`, **unless YPOS == 0x7FF** → active for *all* VC. |
| 63:14 | DATA  | 50 | Free for the GPU ISR; memory-mapped as OB0–OB3 so the GPU reads them as data/pointer. |

After the GPU writes OBF, **execution continues with the next phrase in memory**
(the phrase immediately following this one), *not* a LINK. The GPU may set/clear
OBF to redirect via a following BRANCH object. [TRM p.20 lines 886-895]

**Emulator note:** when the OP hits a GPU object it must (a) latch this phrase
into OB0–OB3, (b) raise the GPU "object" interrupt, and (c) **suspend the OP
mid-list** until OBF is written. In a non-cycle-stepped video model you can treat
this as: run the GPU's OP-interrupt handler synchronously, then continue. The
"active only when VC==YPOS" condition means on lines where it isn't active the OP
**falls through to the next phrase without triggering the GPU**. (UNVERIFIED:
whether a non-active GPU object still consumes a phrase of walk time / advances
to next phrase — almost certainly yes; validate.)

### 3.4 BRANCH object (TYPE 3) — [TRM p.21 lines 897-913]

Single phrase. Conditionally redirects OP flow.

| Bits  | Field | Width | Meaning |
|-------|-------|-------|---------|
| 2:0   | TYPE  | 3  | = 3 |
| 13:3  | YPOS  | 11 | Comparand for CC (the "branch target test value"). |
| 15:14 | CC    | 2  | Condition (see table). |
| 23:16 | —     | 8  | unused |
| 42:24 | LINK  | 19 | Branch-taken target (same encoding as BITMAP LINK). |
| 63:43 | —     | 21 | unused |

**CC field [INC:120-124], [TRM p.21 lines 903-909]:** (the equates pre-shift by
14, e.g. `O_BRGT = 1<<14`)

| CC | Equate      | Branch taken if |
|----|-------------|-----------------|
| 0  | `O_BREQ`    | `YPOS == VC` **OR** `YPOS == 0x7FF` |
| 1  | `O_BRGT`    | `YPOS > VC` |
| 2  | `O_BRLT`    | `YPOS < VC` |
| 3  | `O_BROP`    | OP flag (OBF bit 0) is set |
| 4  | `O_BRHALF`  | On second half of display line (**HC bit 10 == 1**) |

**Note CC is 2 bits in the table but `O_BRHALF = 4<<14` needs a 3rd bit.**
The TRM lists CC as "bits 14-15" (2 bits) yet defines value 4. The INC equate
`O_BRHALF=(4<<14)` sets phrase bit **16**. **Resolution (INFERENCE, mark
UNVERIFIED):** CC is effectively **3 bits, bits 16:14**; the TRM's "14-15" is a
documentation error. Implement CC as `(phrase >> 14) & 7`. Validate value 4
(BRHALF) against BigPEmu. *Listed in Open Questions.*

`VC` here is the **vertical count in half-lines** ($F00006) latched at OP start.
[INC:58]. When branch is **not** taken, the OP continues at the **next phrase in
memory**. [TRM p.21 line 899]

### 3.5 STOP object (TYPE 4) — [TRM p.21 lines 915-922]

Single phrase. Ends OP processing for this line and **interrupts the host
(68000)** if enabled.

| Bits  | Field | Width | Meaning |
|-------|-------|-------|---------|
| 2:0   | TYPE  | 3  | = 4 |
| 63:3  | DATA  | 61 | Free for the CPU ISR (memory-mapped; data or pointer). |

- The **Object interrupt** (INT1 bit 2) is generated by STOP objects. [TRM p.16
  line 720]. Whether the interrupt actually fires is gated by `O_STOPINTS =
  $00000008` (phrase bit 3) [INC:126] — i.e. **bit 3 of the STOP phrase's first
  long enables the STOP interrupt.** So a "silent" STOP has bit 3 = 0; a reference backend uses
  `op_list[5] = 4` (TYPE=4, bit3=0) → no STOP IRQ [reference backend].
  **(UNVERIFIED that bit 3 is the gate vs. INT1 bit 2 enable alone — TRM doesn't
  cross-reference O_STOPINTS to a phrase bit; the INC value `$8` strongly implies
  phrase bit 3. Validate: does a STOP with bit 3 clear suppress the OP interrupt
  even when INT1 bit 2 is enabled?)**
- After STOP, Jaguar performs refresh cycles to drain the refresh counter. [TRM
  p.25 lines 1091-1093]. **Timing-only; no pixel effect.**
- Host ISR must restart the OP by **writing OBF** ($F00026) at the end of
  service. [TRM p.99-ish, lines 1753-1754]

---

## 4. Per-scanline scan-out algorithm

The OP composites one **line buffer** per display line. Reference algorithm
(non-interlaced, single OP pass; the common 320×240 case):

```
fn op_run_scanline(vc_halflines):           // called when HC == HDB1
    cur = OLP (full 32-bit byte address)
    loop:
        phrase = read_phrase64(cur)
        type   = phrase & 7
        match type:
          BITMAP (0) | SCALED (1):
              ypos   = (phrase >> 3)  & 0x7FF
              height = (phrase >> 14) & 0x3FF
              if vc_halflines >= ypos && height > 0:
                  draw_bitmap(phrase, cur, scaled = (type==1))
                  // write-back: HEIGHT-1, DATA += DWIDTH stride, (scaled: REMAINDER)
                  write_back_header(cur, ...)
              // ALWAYS follow LINK (even when not active — the header still links)
              link = (phrase >> 24) & 0x7FFFF
              cur  = (OLP & 0x00C00000) | (link << 3)   // see OLP-bit caveat
          GPU (2):
              ypos = (phrase >> 3) & 0x7FF
              if vc_halflines == ypos || ypos == 0x7FF:
                  latch OB0..OB3 = phrase; raise GPU object IRQ; wait OBF write
              cur += 8                            // next phrase in memory
          BRANCH (3):
              if cc_taken(cc, ypos, vc_halflines, opf, hc):
                  cur = (OLP & 0x00C00000) | (link << 3)
              else:
                  cur += 8
          STOP (4):
              if phrase_bit3_set: raise Object interrupt (INT1 bit2)
              break                              // end of line
```

**Activation rule (BITMAP/SCALED):** object is drawn this line iff
`VC >= YPOS && HEIGHT > 0`, with VC in **half-lines**. [TRM p.19 lines 786-795].
So an object with YPOS = `Y*2` first appears on screen line `Y` and persists for
HEIGHT lines (HEIGHT decremented each line until 0).

**Important subtlety — does the OP follow LINK when the object is *not* active?**
The TRM describes HEIGHT/DATA write-back only for displayed lines, but LINK is a
list-structure field. **The OP always advances to LINK after a BITMAP/SCALED
object** (an inactive object is simply skipped for drawing but still links
onward) — otherwise an off-screen sprite earlier in the list would terminate the
list. **(INFERENCE — strongly implied, mark UNVERIFIED; validate that an
above-its-YPOS object still chains to its LINK.)** a reference backend's list only ever has one
active bitmap then STOP, so it doesn't disambiguate. *Open Question.*

### Line-buffer double buffering
- Two line buffers **A** ($F00800) and **B** ($F01000), each **360 × 32-bit**.
  While the OP writes one, the pixel generator reads the other; **they swap at
  HDB1 (start) and optionally HDB2 (middle)** of each display line. [TRM p.7
  lines 200-203, p.14 lines 610-619, p.23 lines 1021-1023]
- Each 32-bit LB entry holds **two 16-bit pixels** (CRY or RGB16) **or one 24-bit
  pixel**. The lower-address 16-bit word = the **left** pixel. [TRM p.17 lines
  754-758, p.22 line 963]
- **$F01800 (LBUFC)** is the *currently-writing* buffer, exposed so the GPU can
  help fill it. **$F00800/$F01000 are for test only.** [TRM p.17 lines 760-762]
- Add **$8000** to any LB range for **32-bit writes** (Blitter acceleration).
  [TRM p.17 line 763]
- After the buffer is displayed, if **VMODE bit 7 BGEN** is set, the line buffer
  is cleared to the **BG** colour (CRY) — only in CRY/RGB16 modes. [TRM p.13 line
  546-548]. This is how the background colour fills un-drawn line-buffer pixels.

### Emulator simplification (recommended, NON-cycle-accurate compositing)
For a faithful *image* (parity with BigPEmu's scan-out) without modelling the
40 MHz video clock pixel-by-pixel, per display line `y` (0-based within the
active window, `VC = VDB-relative half-line`):
1. Allocate a 360-entry `u16` line buffer; **clear it to BG** (if BGEN) or leave
   "transparent/black."
2. Walk the list as above; for each active BITMAP/SCALED, expand its pixels and
   write 16-bit physical colours into the line buffer at `XPOS + i` (decrement if
   REFLECT), honouring TRANS/RMW.
3. After STOP, read line-buffer entries `[0 .. active_width)` and convert each
   16-bit physical pixel to host RGB per the **VMODE MODE** field (§6).
4. The active horizontal window maps line-buffer pixel index → screen X via the
   HDB1/HDE window; for the standard mode just take `XPOS=BASE_X … BASE_X+W`.

This produces the **true scan-out** the screenshot path must capture.

---

## 5. Header write-back

After drawing an active line of a BITMAP/SCALED object, the OP **writes the
modified header back to DRAM** [TRM p.7 line 233-234, p.22 lines 985-987]:
- **HEIGHT -= 1** (non-interlaced) or **2** (interlaced); clamp at 0. [TRM p.19
  lines 792-795]
- **DATA += DWIDTH** (in phrases) → next line's pixel data at `8*(DATA+DWIDTH)`.
  [TRM p.20 lines 826-827]. For **scaled** objects DATA advances by a *multiple*
  of DWIDTH (= number of source lines consumed this display line). [TRM p.22 line
  986]
- **SCALED:** REMAINDER updated as in §3.2.

**This is why the standard pattern rebuilds the list every vblank** — the in-DRAM
header is mutated during the frame. [reference backend]. An emulator MUST
implement write-back (HEIGHT decrement + DATA advance) or multi-line sprites will
only ever draw their first line / wrong data. The write-back targets the
**original object address** `cur`, modifying the same two longs the OP just read
(big-endian).

**Write-back encoding:** recompose the two longs with the new HEIGHT (bits 23:14)
and new DATA (bits 63:43), preserving all other bits, and store as two
big-endian u32 at `cur` and `cur+4`. (UNVERIFIED whether real HW writes back the
*whole* 2 phrases or just the first phrase / just the affected longwords — TRM
says "the modified header is written back" implying the header. Safe approach:
rewrite only the first phrase's two longs since YPOS/HEIGHT/LINK/DATA all live
there. Validate against BigPEmu by reading the list back after a frame.)

---

## 6. Pixel formats & colour expansion

### 6.1 Line-buffer physical pixel = 16-bit CRY or 16-bit RGB
The line buffer always holds **physical** 16-bit colours (post-CLUT). The
**VMODE MODE** field (bits 2:1) decides how the *pixel generator* interprets them
on output [TRM p.12-13 lines 507-533], [INC:151-154]:

| MODE | VMODE value | Name        | Interpretation |
|------|-------------|-------------|----------------|
| 0    | `CRY16=$0000` | 16-bit CRY  | each 16-bit LB word = CRY pixel → CRY-table → 24-bit RGB |
| 1    | `RGB24=$0002` | 24-bit RGB  | each 32-bit LB entry = one RGB pixel (R,G,B,unused); CLUT bypassed |
| 2    | `DIRECT16=$0004` | 16-bit direct | external mux/DAC; renders nothing sane under BigPEmu [the internal porting notes] |
| 3    | `RGB16=$0006` | 16-bit RGB  | each 16-bit LB word = RGB16 pixel (layout below) |

`VMODE` also packs (low byte) **VIDEN $0001** (enable), **GENLOCK $0008**,
**INCEN $0010**, **BINC $0020**, **CSYNC $0040**, **BGEN $0080**, **VARMOD
$0100**, **PWIDTH bits 11:9** (pixel width = field+1). [TRM p.12-13]. The proven
320-wide mode is **`VMODE = $06C7`** = RGB16 ($6) | CSYNC ($40) | BGEN ($80) |
VIDEN ($1) | PWIDTH=3→width 4 ($600). [reference backend], [the internal porting notes]

#### RGB16 bit layout (THE critical one — "blue in the middle")
A 16-bit RGB physical pixel decodes as [TRM p.24 lines 1061-1063], [the internal porting notes],
[reference backend]:

| Bits  | Channel | Width |
|-------|---------|-------|
| 15:11 | **Red** | 5 (top 5 bits of red) |
| 10:6  | **Blue**| 5 (top 5 bits of blue) |
| 5:0   | **Green** | 6 (top 6 bits of green) |

i.e. **R5 B5 G6 — blue is in the MIDDLE, not the usual R-G-B order.** Written
big-endian exactly as the 68000 stores the word. The reference backend packer (authoritative):
```c
#define JRGB(r,g,b) ((u16)(((r & 0x1F) << 11) | ((b & 0x1F) << 6) | (g & 0x3F)))
```
To host RGB888 (scaling top-bits to 8-bit, replicate-high or `<<3`/`<<2`):
```
r5 = (px >> 11) & 0x1F;  b5 = (px >> 6) & 0x1F;  g6 = px & 0x3F;
R8 = (r5 << 3) | (r5 >> 2);  G8 = (g6 << 2) | (g6 >> 4);  B8 = (b5 << 3) | (b5 >> 2);
```
**WARNING:** the legacy `DIRECT16=$0004` mode and its old "5-5-5 packed" comment
are wrong/non-functional under BigPEmu — use `$xxC7` (RGB16). [the internal porting notes]

#### CRY16 decode (Cyan-Red-Y / colour + intensity)
A 16-bit CRY pixel splits as [TRM p.17 lines 755, p.24 lines 1055-1060]:
- **high byte (bits 15:8) = COLOUR** = `{ Cred=bits 15:12 (4b), Cyan=bits 11:8 (4b) }`
  i.e. two 4-bit chroma coordinates (the X/Y of the distorted-hexagon colour
  square; [TRM p.27-28]).
- **low byte (bits 7:0) = INTENSITY** (8-bit brightness). Intensity 0 = black.

Decode to RGB888 via three 16×16 **modifier ROM tables** indexed by the colour
byte, then **multiply by intensity and shift** [TRM p.28 lines 1252-1304]:
```
let cr = (px >> 12) & 0xF;   // "red" chroma coordinate  (row)
let cy = (px >>  8) & 0xF;   // "cyan" chroma coordinate (col)
let y  =  px & 0xFF;         // intensity
let r_mod = CRY_RED  [cr][cy];   // 0..255  (tables below)
let g_mod = CRY_GREEN[cr][cy];
let b_mod = CRY_BLUE [cr][cy];
let R8 = (r_mod * y) >> 8;   // (UNVERIFIED exact rounding: >>8 vs /255)
let G8 = (g_mod * y) >> 8;
let B8 = (b_mod * y) >> 8;
```
**(UNVERIFIED scaling:** TRM says "the modifier values … multiplied by the
intensity value." The modifiers are 0..255 and intensity 0..255, so the natural
8-bit result is `(mod * y) / 255`. Many emulators use `(mod * y) >> 8` which is
off by ~1 at full scale. Validate the exact formula and which index is row vs
column against BigPEmu — the table is symmetric enough that row/col swap mostly
self-corrects but edge colours differ.)**

The CRY modifier tables (each 16 rows × 16 columns, row index = high nibble of
colour byte, **[TRM p.28 lines 1257-1304]**):

`CRY_RED` (rows 0..15, cols 0..15):
```
  0   0   0   0   0   0   0   0   0   0   0   0   0   0   0   0
 34  34  34  34  34  34  34  34  34  34  34  34  34  34  19   0
 68  68  68  68  68  68  68  68  68  68  68  68  64  43  21   0
102 102 102 102 102 102 102 102 102 102 102  95  71  47  23   0
135 135 135 135 135 135 135 135 135 135 130 104  78  52  26   0
169 169 169 169 169 169 169 169 169 170 141 113  85  56  28   0
203 203 203 203 203 203 203 203 203 183 153 122  91  61  30   0
237 237 237 237 237 237 237 237 230 197 164 131  98  65  32   0
255 255 255 255 255 255 255 255 247 214 181 148 115  82  49  17
255 255 255 255 255 255 255 255 255 235 204 173 143 112  81  51
255 255 255 255 255 255 255 255 255 255 227 198 170 141 113  85
255 255 255 255 255 255 255 255 255 255 249 223 197 171 145 119
255 255 255 255 255 255 255 255 255 255 255 248 224 200 177 153
255 255 255 255 255 255 255 255 255 255 255 255 252 230 208 187
255 255 255 255 255 255 255 255 255 255 255 255 255 255 240 221
255 255 255 255 255 255 255 255 255 255 255 255 255 255 255 255
```
`CRY_GREEN` (rows 0..15, cols 0..15):
```
  0  17  34  51  68  85 102 119 136 153 170 187 204 221 238 255
  0  19  38  57  77  96 115 134 154 173 192 211 231 250 255 255
  0  21  43  64  86 107 129 150 172 193 215 236 255 255 255 255
  0  23  47  71  95 119 142 166 190 214 238 255 255 255 255 255
  0  26  52  78 104 130 156 182 208 234 255 255 255 255 255 255
  0  28  56  85 113 141 170 198 226 255 255 255 255 255 255 255
  0  30  61  91 122 153 183 214 244 255 255 255 255 255 255 255
  0  32  65  98 131 164 197 230 255 255 255 255 255 255 255 255
  0  32  65  98 131 164 197 230 255 255 255 255 255 255 255 255
  0  30  61  91 122 153 183 214 244 255 255 255 255 255 255 255
  0  28  56  85 113 141 170 198 226 255 255 255 255 255 255 255
  0  26  52  78 104 130 156 182 208 234 255 255 255 255 255 255
  0  23  47  71  95 119 142 166 190 214 238 255 255 255 255 255
  0  21  43  64  86 107 129 150 172 193 215 236 255 255 255 255
  0  19  38  57  77  96 115 134 154 173 192 211 231 250 255 255
  0  17  34  51  68  85 102 119 136 153 170 187 204 221 238 255
```
`CRY_BLUE` (rows 0..15, cols 0..15):
```
255 255 255 255 255 255 255 255 255 255 255 255 255 255 255 255
255 255 255 255 255 255 255 255 255 255 255 255 255 255 240 221
255 255 255 255 255 255 255 255 255 255 255 255 252 230 208 187
255 255 255 255 255 255 255 255 255 255 255 248 224 200 177 153
255 255 255 255 255 255 255 255 255 255 249 223 197 171 145 119
255 255 255 255 255 255 255 255 255 255 227 198 170 141 113  85
255 255 255 255 255 255 255 255 255 235 204 173 143 112  81  51
255 255 255 255 255 255 255 255 247 214 181 148 115  82  49  17
237 237 237 237 237 237 237 237 230 197 164 131  98  65  32   0
203 203 203 203 203 203 203 203 203 183 153 122  91  61  30   0
169 169 169 169 169 169 169 169 169 170 141 113  85  56  28   0
135 135 135 135 135 135 135 135 135 135 130 104  78  52  26   0
102 102 102 102 102 102 102 102 102 102 102  95  71  47  23   0
 68  68  68  68  68  68  68  68  68  68  68  68  64  43  21   0
 34  34  34  34  34  34  34  34  34  34  34  34  34  34  19   0
  0   0   0   0   0   0   0   0   0   0   0   0   0   0   0   0
```
**(UNVERIFIED which nibble indexes rows vs columns** — TRM prints the tables as
16 rows × 16 entries but doesn't bind "row = bits 15:12" explicitly. The natural
binding is row = high nibble (bits 15:12, "red" coord), column = low nibble
(bits 11:8, "cyan" coord). The tables' symmetry (RED is row-symmetric, BLUE is
RED flipped, GREEN is column-ramped & row-symmetric) is consistent with this.
Validate against BigPEmu with a CRY test card.)**

#### RGB24 (true colour, MODE 1)
Each 32-bit LB entry is one pixel; **CLUT bypassed**. Byte order in the LB: the
less-significant byte of the **low-address** 16-bit word = **Red**; the
more-significant byte = **Green**; the less-significant byte of the **high-address**
word = **Blue**; the fourth byte unused. [TRM p.17 lines 756-758]. So for a
32-bit big-endian LB long `[b0 b1 b2 b3]`: `b1=Green, b0=Red, b3=Blue` (per the
"low word = R(lo)/G(hi), high word = B(lo)" description). **(UNVERIFIED exact
byte mapping — TRM wording is convoluted; for a 24-bit-mode emulator, validate
with a known RGB24 frame. Rarely used by games.)**

### 6.2 CLUT (palette) — $F00400, 256 × 16-bit
- Translates an 8-bit logical colour index → 16-bit **physical** colour (CRY or
  RGB16, per VMODE). [TRM p.17 lines 742-748]
- **256 entries**, 16-bit each, occupying $F00400–$F005FE (table A) and mirrored
  $F00600–$F007FE (table B). **Writing either range writes BOTH tables**
  (two identical CLUTs exist so two pixels/cycle can be looked up). [TRM p.17
  lines 746-748]. **Emulator: model a single 256-entry `[u16;256]`; mirror the
  $600 range to it; reads from $400 and $600 ranges both return it.**
- **Index formation by DEPTH** [TRM p.23 lines 1013-1017]:
  - **8 bpp:** the 8-bit pixel value is the full CLUT index (0..255).
  - **1/2/4 bpp:** the pixel's `n` bits form the **low** bits; the **high** bits
    come from the object's **INDEX** field (phrase[1] bits 44:38, 7 bits). "The
    top 7 to 4 bits of the index provide the most significant bits of the palette
    address." [TRM p.20 lines 830-831]. Concretely:
    - 4 bpp: index = `(INDEX_high3 << 4) | pix4`  (top 4 bits from INDEX → 16-colour banks)
    - 2 bpp: index = `(INDEX_high6 << 2) | pix2`
    - 1 bpp: index = `(INDEX_high7 << 1) | pix1`
    **(UNVERIFIED exact bit alignment of INDEX into the CLUT address per depth —
    TRM says "top 7 to 4 bits"; the convention above gives a contiguous index but
    validate against a known 4bpp object on BigPEmu.)**
- **16 / 24 bpp:** CLUT bypassed entirely (pixels are already physical). [TRM
  p.22-23 lines 1008-1012]

### 6.3 TRANS / REFLECT / RMW
- **TRANS (phrase[1] bit 47):** logical colour **0** (and "reserved physical
  colours") is transparent — the pixel is **not written** to the line buffer
  (the existing/background value shows). [TRM p.20 line 835], [INC:102]. Applies
  to 1/2/4/8/16-bit modes (colour zero). [TRM p.7 line 238]. **Emulator:** skip
  the write when `pixel == 0` and TRANS is set. **(UNVERIFIED what "reserved
  physical colours" means for 16-bit — likely none in practice; treat as
  "value 0 transparent.")**
- **REFLECT (phrase[1] bit 45):** draw the object **right→left**; the line-buffer
  write address **decrements** instead of increments. [TRM p.20 line 832, p.23
  line 1027], [INC:100]. Pixels are read from the source in the same order but
  placed at decreasing X starting from XPOS. **(UNVERIFIED whether the start X is
  XPOS or XPOS+width-1 — validate; the natural HW behaviour is the LB address
  starts at XPOS and decrements, mirroring about XPOS.)**
- **RMW (phrase[1] bit 46):** **read-modify-write** — instead of overwriting,
  the object's colour is **added** to the existing line-buffer pixel, as
  **signed offsets** for intensity and the two colour vectors (i.e. component-wise
  signed add in CRY space). Halves the write rate. [TRM p.20 lines 833-834, p.7
  lines 242-243], [INC:101]. **Emulator:** `lb[x] = clamp_components(lb[x] +
  signed(src))` in CRY (or RGB16) component space. **(UNVERIFIED clamping
  behaviour and exact per-channel field widths for the add — used for
  shadow/fog/lighting; validate against a game that uses RMW such as a
  lighting/shadow effect.)**

---

## 7. Worked example — the proven 320×240 RGB16 setup (a reference backend/Jaguar Doom)

This is the **known-good** minimal display list: one BITMAP object pointing at a
320×240 RGB16 framebuffer, then a STOP. [reference backend]

```c
// op_list[] is a u32[6], 16-byte aligned. fb_addr = framebuffer byte address.
// link = (&op_list[4]) >> 3   (phrase address of the STOP object)
op_list[0] = (fb_addr << 8) | (link >> 8);        // DATA + LINK[18:8]
op_list[1] = (link << 24)                          // LINK[7:0]
           | (RENDER_H << 14)                       // HEIGHT = 240 lines
           | (BASE_Y  <<  4);                        // YPOS = BASE_Y*2 half-lines (16*2=32)
op_list[2] = SCREEN_PWIDTH >> 4;                    // (high long of phrase[1]) DWIDTH/IWIDTH high bits
op_list[3] = (SCREEN_PWIDTH << 28)                  // IWIDTH bits
           | (SCREEN_PWIDTH << 18)                  // DWIDTH bits
           | (1u << 15)                             // PITCH = 1 (contiguous)
           | (4u << 12)                             // DEPTH = 4 → 16 bpp
           | BASE_X;                                 // XPOS = 16
op_list[4] = 0;                                     // STOP phrase, hi long
op_list[5] = 4;                                     // STOP: TYPE=4, bit3=0 (no IRQ)
// SCREEN_PWIDTH = (320*2)/8 = 80 phrases per line
```
- `OLP = (olp >> 16) | (olp << 16)` (the word-swap) [reference backend].
- `VMODE = $06C7`; `VI = vdb-2`; `HDE = (width/2-1)|$400`; `HDB1=HDB2 = hmid -
  width/2 + 4`; `VDB = vmid-height`; `VDE = $FFFF`. [reference backend].
  Note **VDE is parked at $FFFF** (wide open); the computed vde is layout-only.
  [reference backend]
- The **ISR rebuilds this list every vblank** before the OP reaches YPOS.
  [reference backend]

Decoding `op_list[3]` confirms field positions: bits 11:0 = XPOS=`BASE_X`; bits
14:12 = DEPTH=`4` (16bpp); bits 17:15 = PITCH=`1`; bits 27:18 = DWIDTH=80; bits
37:28 (here the top 4 bits of op_list[3] = IWIDTH low + op_list[2] bit0 = IWIDTH
high) = IWIDTH=80. This is consistent with §3.1.

---

## 8. Emulator implementation checklist

1. **Model registers** at the addresses in §1; OLP is the only 32-bit one.
   Mirror CLUT $600→$400. Honour the LBUF mirrors.
2. **OP trigger:** run `op_run_scanline(VC)` once per active display line (when
   HC==HDB1; once per line for HDB1==HDB2). VC counts **half-lines**.
3. **List walk + dispatch** per §4; **19-bit LINK** (`& 0x7FFFF`, *not* `& 0xFF`)
   forming `next = (OLP & 0x00C00000) | (LINK<<3)`. [the internal porting notes]
4. **21-bit DATA** (`& 0x1FFFFF`) → byte addr `DATA<<3`; full address, no bit-16
   anomaly. [the internal porting notes]
5. **Pixel expansion** by DEPTH: 1/2/4/8 → CLUT (with INDEX high bits) → physical
   16-bit; 16/24 → direct. Two pixels/clock unscaled, one/clock scaled — but for
   an *image-accurate* (not cycle) model just emit all pixels.
6. **TRANS / REFLECT / RMW** per §6.3.
7. **Header write-back** per §5 (HEIGHT--, DATA+=DWIDTH, scaled REMAINDER) — or
   multi-line sprites break. Mutate the source phrase in DRAM, big-endian.
8. **Line-buffer → screen** conversion by VMODE MODE (CRY/RGB16/RGB24) per §6.1.
   **This is the true scan-out — the screenshot path captures THIS, never the
   guest's framebuffer DRAM.** [the internal porting notes]
9. **STOP** → Object interrupt (INT1 bit 2) if STOP bit 3 set; OP ends; restarts
   on OBF write.
10. **GPU object** → raise GPU interrupt, OB0–3 latched, resume on OBF write.

---

## 9. Open questions (validate against BigPEmu)

1. **OLP word-swap mechanics:** which 16-bit half (OLPLO $F00020 / OLPHI $F00022)
   holds which address half, and whether a single 32-bit write at $F00020 should
   be stored verbatim. a reference backend's swap net-effect = final OLP == list base. (§1)
2. **OLP high-bit preservation on LINK:** TRM says LINK replaces OLP bits 21:3;
   confirm OLP bits 23:22 persist (4 MB window) with a list crossing $400000. (§3.1)
3. **CC field width / BRHALF:** is CC 2 bits (TRM) or 3 bits (INC `O_BRHALF=4<<14`
   → phrase bit 16)? Implement as 3 bits and verify value 4 behaviour. (§3.4)
4. **LINK followed for inactive objects:** does a BITMAP/SCALED whose YPOS/HEIGHT
   excludes it this line still chain to its LINK? (Assumed yes.) (§4)
5. **Header write-back extent:** whole 2-phrase header vs just first phrase's two
   longs; read list back after a frame on BigPEmu. (§5)
6. **CRY decode formula:** `(mod*y)>>8` vs `(mod*y)/255`, and row/column index
   binding (high nibble = "red" coord?). Validate with a CRY test card. (§6.1)
7. **CRY16 vs DIRECT16:** confirm DIRECT16 ($xxC3/$xx04) renders nothing useful
   under BigPEmu and RGB16 ($xxC7) is the path. (§6.1) [the internal porting notes]
8. **CLUT index assembly for 1/2/4 bpp:** exact alignment of INDEX field
   high-bits into the palette address per depth. (§6.2)
9. **REFLECT start X:** does the decrementing LB address start at XPOS or
   XPOS+width-1? (§6.3)
10. **RMW component model:** field widths and clamping of the signed CRY add;
    validate with a shadow/lighting effect. (§6.3)
11. **STOP interrupt gate:** is phrase bit 3 (`O_STOPINTS=$8`) the per-object
    enable, in addition to INT1 bit 2? (§3.5)
12. **VSCALE/REMAINDER step:** decrement of "one whole" = `0x20` (integer unit)
    vs `1` LSB. (§3.2)
13. **Scaled HSCALE accumulator:** exact fixed-point semantics (3.5 format,
    0x20=1.0×) for horizontal pixel emission. (§3.2)
14. **DATA ≥ $100000 bit-16 anomaly** is a BigPEmu quirk [the internal porting notes]; the
    from-scratch emulator should NOT replicate it — flag divergence here as an
    emulator difference, decide whether to match BigPEmu for byte-parity. (§3.1)
15. **Two OP passes per line (HDB1+HDB2):** the >1 line-buffer-width modes
    (e.g. 720px); not needed for 320-wide but required for full accuracy. (§2,§4)
```
