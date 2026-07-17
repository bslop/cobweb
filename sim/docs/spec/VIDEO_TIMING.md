# Atari Jaguar — Video Mode, Display Timing, Master Clock & Scheduler Spec

Implementation-grade specification for the cycle-accurate Rust emulator.
Subsystem: **video mode + display timing + master-clock / scheduler model**
that drives frames deterministically.

**Endianness:** The Jaguar is **big-endian** (Motorola 68000 host CPU; TOM's
`MEMCON1.BIGEND` bit 12 is set on console hardware). All multi-byte values in
DRAM and all TOM/JERRY registers are big-endian: the lowest address holds the
most-significant byte. Where byte/word/long ordering matters it is called out
explicitly below.

**Source legend**
- `INC` = the Atari SDK's `JAGUAR.INC` (official Atari SDK equates — authoritative for addresses/bit values).
- `TRM p.N` = *Jaguar Technical Reference Manual Rev 8* (`Jaguar Technical Reference v8.pdf`), page N as printed in the manual footer ("Page N").
- `REF` = a proven reference homebrew backend (runs correctly on BigPEmu + hardware).
- Provenance shown as "the internal porting notes" = internal Jaguar porting notes.
- `VJ` = Virtual Jaguar-Rx source (`github.com/djipi/Virtual-Jaguar-Rx`) — used to corroborate numeric timing constants the TRM leaves implicit.
- Tags: **VERIFIED** = from official docs or proven code. **(UNVERIFIED)** = inference; see *Open questions*.

---

## 1. Master clocks

### 1.1 The single crystal and the divider tree

The Jaguar has **one master oscillator** locked to the colour subcarrier. Both
the video/pixel clock and the RISC ("processor") clock are derived from it by
TOM/JERRY dividers, and the 68000 runs at exactly **half** the RISC clock.
(TRM p.83–85: JERRY synthesises chroma, video and processor clocks; "This clock
[processor clock] is divided by two to provide a clock for an external
processor" — the 68000 is that external processor.)

The widely-cited "26.591 MHz" RISC / "13.295 MHz" 68000 figures are rounded.
The exact rates used by Virtual Jaguar (the de-facto reference for emulator
timing) are: **VERIFIED (VJ)**

| Clock | NTSC (Hz) | PAL (Hz) | Notes |
|---|---|---|---|
| RISC clock (GPU, DSP, OP, blitter, memory, TOM time-base) | **26 590 906** | **26 593 900** | "Processor clock" in the TRM |
| 68000 clock | **13 295 453** | **13 296 950** | Exactly RISC/2 |

Relationships the scheduler must hold true:
- `m68k_clock = risc_clock / 2` **exactly** (TRM p.83: ÷2 for external CPU). The VJ constants satisfy this (13 295 453 × 2 = 26 590 906).
- The **video/pixel clock is programmable**, derived from the same time base by the `PWIDTH` divider in `VMODE` (TRM p.13–14). It is *not* a fixed third crystal. "The video time base generator is programmed in cycles of the **video clock** and not the pixel clock produced by this divider." (TRM p.14)
- For TV output the **pixel clock** is in the 6–15 MHz range (TRM p.11–12, p.83: "typically between 6 MHz and 12 MHz"; p.11: "12 to 15 MHz"). The **video clock** is a small integer multiple of the pixel clock (PWIDTH = video-clock cycles per pixel, 1–8). The line buffer is read out at the video clock (40 MHz max, TRM p.11).

(UNVERIFIED) The exact crystal is conventionally stated as the NTSC value
`26.590906 MHz ≈ 7.5 × 3.579545 MHz` (7.5× colour burst). The PAL value is a
slightly different multiple of 4.43361875 MHz. For the emulator only the two
table values above matter; do not depend on the crystal-derivation arithmetic.

### 1.2 Ratios the scheduler should use

Define the **RISC clock as the master timebase** (finest granularity that
matters), then:

```
risc_cycles_per_m68k_cycle = 2          // VERIFIED, exact
op/blitter/gpu/dsp tick at risc_clock
68000 ticks at risc_clock / 2
```

The video time-base (HC/VC, OP scheduling, VI) is driven by the **video clock**,
which equals `risc_clock / VCLK_divider`. On the console with the normal
`CLK1/CLK2` reset configuration the **video clock equals the RISC clock**
(UNVERIFIED — see §6) so practical emulators treat one half-line as a fixed
real-time slice (§5) rather than re-deriving the video clock. Two equivalent
clean models:

- **Cycle model (recommended for cycle-accuracy):** advance an integer RISC-cycle counter; convert other domains by the fixed ratios above and by HP/VP-derived cycle counts (§5.2).
- **Microsecond model (what VJ uses, simplest to get right):** schedule on a half-line callback of fixed real duration (§5.1), and convert a time slice `Δt_µs` to cycles by
  - `m68k_cycles = round(Δt_µs × m68k_clock / 1_000_000)`
  - `risc_cycles = round(Δt_µs × risc_clock / 1_000_000)`
  **VERIFIED (VJ `jaguar.cpp`: `m68k_execute(USEC_TO_M68K_CYCLES(t)); GPUExec(USEC_TO_RISC_CYCLES(t));`)**.

---

## 2. Frame structure

### 2.1 Counters: HC ($F00004) and VC ($F00006)

Both are **16-bit registers** (read/write; writable only for ASIC test). **VERIFIED (INC:57–58; TRM p.12).**

| Reg | Addr | Width | Meaning |
|---|---|---|---|
| `HC` | `$F00004` | 16-bit | Horizontal Count |
| `VC` | `$F00006` | 16-bit | Vertical Count |

- **HC**: a **10-bit counter** counting 0…HP (the horizontal-period value) **twice per video line**; **bit 10** ($0400) selects which half of the line is being generated. Incremented by the **pixel clock**. (TRM p.12) So HC's full range is 11 significant bits: `[10]=half-of-line, [9:0]=position`.
- **VC**: an **11-bit counter** ($0…$7FF) counting **half-lines** (`0…VP`), incremented **every half-line**; **bit 11** ($0800) selects odd/even field. (TRM p.12) "The vertical counter is incremented every half line in order to support interlaced displays." So **VC counts half-lines, not lines.** A full progressive frame is therefore ~`VP+1` half-lines = ~`(VP+1)/2` displayed lines.

Why half-lines: NTSC/PAL fields differ by a half-line; counting half-lines lets one counter express both interlaced fields and progressive frames. On the console the OP is almost always run non-interlaced (every object YPOS is even — TRM p.20), so **even VC values = visible lines** and the emulator can map line `L = VC/2`.

### 2.2 Scanlines / half-lines per frame

The number of half-lines per field is `VP+1` (TRM p.16: "The number is one more
than the value written into [VP]"). VP lives at `$F0003E` (**16-bit, WO**, INC
line 683 region / TRM p.16). Standard values: **VERIFIED (VJ `tom.cpp`)**

| | NTSC | PAL |
|---|---|---|
| `VP` register value | **523** | **623** |
| Half-lines per field (`VP+1`) | **524** | **624** |
| Displayed lines per frame (progressive, `(VP+1)/2`) | **262** | **312** |
| Full TV lines incl. interlace pairing | 525 | 625 |

So: **NTSC ≈ 262–263 lines/frame, PAL ≈ 312–313 lines/frame** (the ±1 is the
interlace half-line; progressive console output uses the even count). The
emulator should treat one **frame = `VP+1` half-lines** and step VC 0…VP.

The SDK's `*_HEIGHT`/`*_VMID` constants (INC:132–140) are *programming hints in
half-lines/pixel-clocks*, not the field length:
- `NTSC_HEIGHT = 241` (INC:134) — number of **scanlines** of active picture the SDK centres (used as `bitmap HEIGHT` and to compute VDB). `PAL_HEIGHT = 287`.
- `NTSC_VMID = 266` (INC:135) — **middle of the field in half-lines**. `PAL_VMID = 322`.
- `NTSC_WIDTH = 1409` (INC:132) — **width of the active line in pixel clocks**. `NTSC_HMID = 823` — middle of the line in pixel clocks. `PAL_WIDTH = 1381`, `PAL_HMID = 843`.
- These feed the REF/Doom programming formulas in §4.3; they are *not* HP/VP.

### 2.3 Active display window registers

All of these are **16-bit, write-only** (TRM p.13–16; INC:70–76). Values are in
**video-clock cycles** (horizontal) or **half-lines** (vertical).

| Reg | Addr | Width | Field | Meaning |
|---|---|---|---|---|
| `HDB1` | `$F00038` | 16-bit (11 sig) | Horiz Display Begin 1 | HC value at which OP starts (1st run) & line buffers swap |
| `HDB2` | `$F0003A` | 16-bit (11 sig) | Horiz Display Begin 2 | HC value for 2nd OP run (mid-line) — used for >360-px modes |
| `HDE` | `$F0003C` | 16-bit (11 sig) | Horiz Display End | HC value at which display ends → border/black |
| `HBB` | `$F00030` | 16-bit (11 sig) | Horiz Blank Begin | MSB usually set (blank in 2nd half of line) |
| `HBE` | `$F00032` | 16-bit (11 sig) | Horiz Blank End | MSB usually clear |
| `HP` | `$F0002E` | 16-bit (10 sig) | Horiz Period | half-line period in **video-clock** cycles; actual period = `HP+1` |
| `VDB` | `$F00046` | 16-bit (11 sig) | Vert Display Begin | half-line on which OP processing begins |
| `VDE` | `$F00048` | 16-bit (11 sig) | Vert Display End | half-line on which OP processing ends |
| `VBB` | `$F00040` | 16-bit (11 sig) | Vert Blank Begin | half-line vertical blank starts |
| `VBE` | `$F00042` | 16-bit (11 sig) | Vert Blank End | half-line vertical blank ends |
| `VS` | `$F00044` | 16-bit (11 sig) | Vert Sync | half-line on which vertical sync begins (to VP) |
| `VP` | `$F0003E` | 16-bit (11 sig) | Vert Period | half-lines/field − 1 |
| `VI` | `$F0004E` | 16-bit (11 sig) | Vert Interrupt | half-line at which VI fires (see §3) |

**Active-display semantics the OP/scanline engine must honour:**
- A scanline is "active" (OP runs, pixels shift out of line buffer) when `VDB ≤ VC ≤ VDE`. Outside that range, **border colour** (BORD) or **black** is shown (TRM p.16: "Object processing restarts on every line until the half line specified by VDE. The border colour (or black) is displayed outside these active lines.").
- Horizontally, between `HDB1/HDB2` and `HDE` the line buffer is displayed; outside, border/black (TRM p.15).
- **REF programs `VDE = $FFFF`** (a value > VP) to mean "never stop OP before end of field"; the actual bottom is then bounded by VP/VBB. **VERIFIED (REF: `VDE = 0xFFFF;`).** The emulator must clamp: OP runs while `VC ≥ VDB && VC ≤ min(VDE, VP)` and the object's own YPOS/HEIGHT gate visibility.
- `HDB1` may equal `HDB2` (or one may be set beyond line length) to make the OP run **once** per line; distinct values make it run **twice** for >360-word lines (TRM p.15). REF sets `HDB1 == HDB2` (single OP pass).

VJ's typical decoded values for cross-checking a default 320-wide setup
(**VERIFIED VJ `tom.cpp`**, useful as sanity anchors): NTSC `VDB=38, VDE=518`,
visible VC band `31…511`, `HP=844`; PAL `VDB=38, VDE=518`, visible `67…579`,
`HP=850`. (These are VJ's internal display-extraction constants, not register
resets.)

---

## 3. VMODE register ($F00028) — 16-bit

**`VMODE` is 16-bit, write-only** (INC:67; TRM p.13). **CRITICAL ACCESS RULE
(the internal porting notes): never read/write it (or any TOM video reg) as 32-bit.** A
32-bit store `VMODE = $06C7` lands big-endian as **VMODE = $0000** with `$06C7`
spilled into the neighbouring `BORD1` ($F0002A) — zeroing PWIDTH so every pixel
becomes ~4× too narrow and the whole frame squishes into a ~70 px strip. The
emulator's bus model **must** decode `$F00028` and `$F0002A` as independent
16-bit words; a 32-bit access at `$F00028` writes the high word to VMODE and the
low word to BORD1 (and the reverse on read). Treat 32-bit access to the
16-bit TOM register file as two adjacent word accesses, big-endian order.

### 3.1 Bit layout (VERIFIED — TRM p.13–14; INC:149–170)

| Bits | Name | Values / meaning |
|---|---|---|
| 0 | **VIDEN** | 1 = enable video time-base generator. **Master "video on" switch.** (`VIDEN EQU $0001`, INC:149) |
| 1–2 | **MODE** | Colour mode, 2-bit field (see table below) |
| 3 | **GENLOCK** | Enable digital genlock. **Not supported on Jaguar console** (INC:156 `GENLOCK EQU $0008`). Emulator: treat as no-op. |
| 4 | **INCEN** | Enable encrustation (external video mux via CRY LSB). `$0010` |
| 5 | **BINC** | Select local border colour when encrustation enabled. `$0020` |
| 6 | **CSYNC** | Enable composite sync on vsync output. `$0040` |
| 7 | **BGEN** | Clear line buffer to **BG** ($F00058) after display. Only effective in CRY & RGB16 modes. `$0080` |
| 8 | **VARMOD** | Variable colour-resolution mode. When set, each line-buffer word's LSB selects CRY (LSB=0) vs RGB (LSB=1) for the other 15 bits. `$0100` |
| 9–11 | **PWIDTH** | Pixel width in **video-clock cycles**; actual width = `field+1` (1…8 clocks). See encoding below. |
| 12–15 | Unused | Write zero. |

### 3.2 MODE (bits 1–2) colour modes (VERIFIED — INC:151–154; TRM p.13–14)

| MODE | VMODE bits[2:1] | `VMODE` mask | Name | Line-buffer interpretation |
|---|---|---|---|---|
| 0 | %00 | `$0000` `CRY16` | **16-bit CRY** | Each 32-bit LB entry = two 16-bit CRY pixels; LSB-byte is intensity. Converted to 8:8:8 RGB via CLUT+multiplier. |
| 1 | %01 | `$0002` `RGB24` | **24-bit RGB** | Each 32-bit LB entry = one pixel, 8R/8G/8B + 8 unused. Read at full video clock. |
| 2 | %10 | `$0004` `DIRECT16` | **16-bit DIRECT** | 32-bit LB word split into two 16-bit words driven onto R/G on alternate video-clock phases (external mux). Blanking/active on the 2 LSBs of blue. |
| 3 | %11 | `$0006` `RGB16` | **16-bit RGB** | Each 32-bit LB entry = two 16-bit RGB pixels. **Bits layout per TRM: [5:0]=green, [10:6]=blue, [15:11]=red.** |

**RGB16 pixel layout (the one homebrew uses) — VERIFIED & load-bearing
(the internal porting notes):** `R5 = bits 15:11, B5 = bits 10:6, G6 = bits 5:0` — **blue in
the MIDDLE**, R5B5G6 (5-6-5 by width but ordered R/B/G). Written big-endian
exactly as the 68000 stores a word. Example test-card colours (the internal porting notes):
`$F800` = red, `$07C0` = green, `$003F` = blue. The emulator's RGB16→host-RGB
converter must use this exact bit order, not the conventional RGB565.

**Direct-color VMODE constant — VERIFIED (the internal porting notes; REF):**
the standard 320-wide direct-colour mode is **`VMODE = $06C7`**, i.e.
`PWIDTH4 ($0600) | BGEN ($0080) | CSYNC ($0040) | RGB16 ($0006) | VIDEN ($0001)`
= `$06C7`. The internal porting notes warn the `$xxC3` ("DIRECT16") mode "renders nothing sanely
under BigPEmu" and that the jaguar.inc comment calling DIRECT16 '5-5-5 RGB
packed' is wrong — **use MODE %11 (RGB16) for direct color.**

### 3.3 PWIDTH (bits 9–11) encoding (VERIFIED — INC:163–170; TRM p.14)

Pixel width in video-clock cycles = `(field value) + 1`.

| Field (bits 11:9) | `VMODE` mask (INC name) | Pixel width (video clocks) |
|---|---|---|
| 0 | `$0000` PWIDTH1 | 1 |
| 1 | `$0200` PWIDTH2 | 2 |
| 2 | `$0400` PWIDTH3 | 3 |
| 3 | `$0600` PWIDTH4 | 4 |
| 4 | `$0800` PWIDTH5 | 5 |
| 5 | `$0A00` PWIDTH6 | 6 |
| 6 | `$0C00` PWIDTH7 | 7 |
| 7 | `$0E00` PWIDTH8 | 8 |

PWIDTH=4 (`$0600`) with `NTSC_WIDTH=1409` video clocks gives ~`1409/(2·4)≈176`
visible pixel positions per half-line ⇒ a ~352-position full line, into which a
320-px display is centred (REF). The display width **must
be an integer multiple of the pixel width** (TRM p.14).

### 3.4 Other VMODE-adjacent video colour registers (all 16-bit, WO)

| Reg | Addr | Width | Meaning |
|---|---|---|---|
| `BORD1` | `$F0002A` | 16-bit | Border colour: **low byte = Red, high byte = Green** (TRM p.14) |
| `BORD2` | `$F0002C` | 16-bit | Border colour: low byte = Blue |
| `BG` | `$F00058` | 16-bit | Background CRY colour the line buffer is cleared to when BGEN set (TRM p.16; INC:79) |
| `OBF` | `$F00026` | 16-bit | Object-Processor flag (bit 0 testable by OP branch; any write restarts OP after a GPU interrupt object) (TRM p.12) |
| `OLP` | `$F00020` | **32-bit** | Object List Pointer — **the one 32-bit TOM reg in this group** (INC:65). Phrase-aligned (bottom 3 bits 0). REF writes it **word-swapped**: `OLP = (olp>>16)|(olp<<16)` (REF). See *Open questions*. |

---

## 4. The Vertical Interrupt (VI)

### 4.1 When it fires

`VI` (`$F0004E`, 16-bit WO, INC:76) holds the **half-line** at which the video
interrupt is generated (TRM p.17). The TOM time-base compares the half-line
counter VC against VI each half-line; when `VC == VI` it latches a pending video
interrupt. **"This number must be odd for non-interlaced setups."** (TRM p.17).

REF programs **`VI = a_vdb - 2`** where `a_vdb = VMID - HEIGHT` is the
display-begin half-line, so the interrupt fires **2 half-lines before the
display starts** — giving the ISR time to rebuild the OP list before the first
visible YPOS line (REF). To suppress
VI entirely, write `VI = $FFFF` (a value VC never reaches — REF).

So for a deterministic scheduler: **fire VI when the half-line counter reaches
the VI value**, once per field. With REF's settings that is ~near the top of
active display, *not* at the bottom — the name "vblank" in homebrew is a
misnomer; it is a programmable raster interrupt typically placed just above the
visible window.

### 4.2 How it reaches the 68000 (VERIFIED, with one corrected detail)

1. **Enable:** set bit 0 of `INT1` ($F000E0). `C_VIDENA EQU $0001` (INC:35). INT1 is **16-bit RW** (INC:81). REF: `INT1 = 0x0001;`.
2. **TOM asserts IRQ level 2** on the 68000's IPL pins for *all five* TOM interrupt sources (video, GPU, OP/stop-object, PIT, JERRY) — they share the single TOM→68000 line. **VERIFIED (VJ `tom.cpp`: every TOM interrupt path calls `m68k_set_irq(2)`).**
3. **Vector:** the IACK cycle supplies **vector number 64**, so the 68000 dispatches through **`$100` (= 64 × 4)**. INC calls this `LEVEL0 / USER0 EQU $100` (INC:29–30) — a misleading name: it is **user interrupt vector 0 (vector 64), NOT the level-0 autovector.** This is a **vectored** interrupt, not autovectored: the standard 68000 level-2 autovector would be `$68` (vector 26), but the Jaguar/BigPEmu route VI to `$100`. **VERIFIED** (INC:29; REF installs handler at `$100`, `JAG_AUTOVEC = *(uint32_t*)0x100`; Virtual Jaguar porting note: "change Level 2 interrupt vector to 0x100").
4. **IPL / SR caveat (the internal porting notes, load-bearing):** because the VI arrives at **IPL 2 (≤ 3)**, a 68000 status-register interrupt mask of `$2300`/`$2500`/`$2700` (IPL 3/5/7 — common Genesis/arcade idioms) **silently starves the VI**. The 68000 only takes an IRQ whose level is **strictly greater** than the SR mask. So to receive VI (level 2) the SR mask must be **≤ 1**: run at **IPL 0** (`move #$2000,sr`). REF does exactly this: `move.w #0x2000,%sr` after enabling INT1. BigPEmu boots the 68000 at **IPL 0**. The emulator's 68000 core must implement the standard rule: take the IRQ iff `irq_level > (SR>>8 & 7)` (level 7 NMI excepted), and supply vector 64 on IACK for TOM IRQs.
5. **ISR exit protocol:** the handler must (a) **clear** the pending video latch by writing `C_VIDCLR ($0100)` to INT1 — keeping enable set (REF writes `INT1 = $0101` = `C_VIDCLR|C_VIDENA`), and (b) write **any** value to `INT2` ($F000E2, WO) to restore GPU/blitter bus priority (TRM p.17: "INT2 must always be written to at the end of a CPU interrupt service routine"). REF: `INT2 = 0` then `rte`.

### 4.3 REF programming formulas (VERIFIED — reproduce for default modes)

For a `WIDTH×HEIGHT` centred display:
```
ntsc   = (CONFIG & $10) != 0          // CONFIG=$F00036 read; bit4: 1=NTSC,0=PAL (INC:145 VIDTYPE=$10)
width  = ntsc ? 1409 : 1381           // *_WIDTH  (pixel clocks)
hmid   = ntsc ? 823  : 843            // *_HMID
height = ntsc ? 241  : 287            // *_HEIGHT (scanlines)
vmid   = ntsc ? 266  : 322            // *_VMID   (half-lines)

HDE  = (width/2 - 1) | $0400          // MSB set: display end in 2nd half of line
HDB1 = HDB2 = hmid - width/2 + 4      // left edge, single OP pass
a_vdb = vmid - height                 // top of active window, in half-lines
VDB  = a_vdb
VDE  = $FFFF                          // "never stop early"; clamp to VP in emu
VI   = a_vdb - 2                      // raster IRQ 2 half-lines before display
BG = BORD1 = BORD2 = 0
VMODE = $06C7                         // RGB16 direct, PWIDTH4, BGEN, CSYNC, VIDEN
```
**INTERLACE NOTE:** REF's `a_vdb = vmid - height` can be even or odd depending on
parity; TRM requires VI odd for non-interlaced. The emulator should not enforce
this — just compare `VC == VI` each half-line and accept REF's value as-is
(BigPEmu does). YPOS for objects is `BASE_Y*2` (half-lines) since VC counts
half-lines (REF shifts `BASE_Y<<4`, i.e. `<<1` for ×2 then `<<3`
into the YPOS field).

---

## 5. Alternative interrupt sources: PIT (TOM) and JERRY timers

### 5.1 TOM PIT0/PIT1 ($F00050 / $F00052) — 16-bit WO each

`PIT0` ($F00050) and `PIT1` ($F00052) are a **16-bit register pair** controlling
a CPU/GPU interrupt frequency (INC:77–78; TRM p.17). Timing model (TRM p.17):

```
stage1 = system_clock / (PIT0 + 1)     // if PIT0 == 0 the timer is DISABLED
pit_irq_freq = stage1 / (PIT1 + 1)     // output generates the interrupt
```
i.e. **interrupt period = `(PIT0+1)·(PIT1+1)` system-clock cycles** (system clock
= RISC/processor clock, §1). Enable via `C_PITENA ($0008)` in INT1; clear pending
via `C_PITCLR ($0800)` (INC:38,44). It asserts the same **IRQ level 2 / vector
64 ($100)** path as VI (VERIFIED VJ `TOMPITCallback`: `m68k_set_irq(2)`). The
GPU has its own PIT-enable/clear bits in `G_FLAGS` (`G_PITENA $40`, `G_PITCLR
$800`, INC:194,199).

### 5.2 JERRY timers JPIT1–JPIT4 ($F10000–$F10006) — 16-bit WO each

JERRY has **two identical timers**, each a pair of 16-bit dividers (TRM p.85–86;
INC:428–431):

| Reg | Write addr | Read addr | Role |
|---|---|---|---|
| `JPIT1` | `$F10000` | `$F10036` | Timer 1 pre-scaler (÷ N+1) |
| `JPIT2` | `$F10002` | (paired) | Timer 1 divider (÷ M+1) |
| `JPIT3` | `$F10004` | — | Timer 2 pre-scaler |
| `JPIT4` | `$F10006` | — | Timer 2 divider |

Timing model (TRM p.85): stage 1 (pre-scaler) divides the **processor clock** by
`N+1`; stage 2 divides that by `M+1`.
```
jerry_timer_freq = processor_clock / ((N+1)·(M+1))   // N=JPIT1/3, M=JPIT2/4
```
Range ~4 … 4 billion. Writing the registers **presets** the counters (usable as
one-shot delays); they are **readable** to measure elapsed time. Timer 1 is
conventionally the **audio sample-rate** clock; timer 2 a music-tempo clock.
Their outputs interrupt the **DSP** or the **68000** (the 68000 path is the
JERRY interrupt = `INT1` bit 4, `C_JERENA $0010` / `C_JERCLR $1000`, INC:39,45;
TRM p.17 source 4 — "active high edge-triggered; first interrupt on the first
rising edge after enable"). Read addresses differ from write addresses (TRM
p.85; note `$F10036` is the JPIT1 read address).

For the scheduler, model each JERRY timer as a free-running down-counter of
period `(N+1)·(M+1)` processor-clock cycles that raises its target IRQ on
underflow and reloads.

---

## 6. Recommended deterministic scheduler design

Goal: step **exactly N frames headlessly**, bit-reproducibly. Use the
**half-line callback model** (proven by Virtual Jaguar; matches the hardware's
half-line granularity for VC/VI/OP) with the RISC clock as the cycle reference.

### 6.1 Constants (per region, chosen once at reset from CONFIG bit 4)

```
NTSC: risc_hz = 26_590_906, m68k_hz = 13_295_453, VP = 523, half_lines = 524
PAL:  risc_hz = 26_593_900, m68k_hz = 13_296_950, VP = 623, half_lines = 624
half_line_us = NTSC ? 31.777_777_778 : 32.0          // VERIFIED VJ jaguar.cpp
```
`half_line_us` is the real duration of one VC tick. (VJ uses these exact values;
they reproduce ~59.94 Hz NTSC / 50 Hz PAL: `1e6 / (524 × 31.7778) ≈ 60.0`,
`1e6 / (624 × 32.0) = 50.08`.) **(UNVERIFIED nuance:** the precise NTSC field
rate is 59.94 Hz; whether to use 524 half-lines × 31.778 µs or derive from
`risc_hz/(VP+1)/cycles_per_halfline` should be validated against BigPEmu frame
counts — see *Open questions*.)

### 6.2 Cycle budget per half-line (cycle model)

```
risc_cycles_per_halfline = round(half_line_us × risc_hz / 1e6)   // ≈ 845 NTSC, 851 PAL
m68k_cycles_per_halfline = risc_cycles_per_halfline / 2          // ≈ 422 / 425
```
These are close to the TRM/VJ `HP` values (HP+1 video-clock cycles per
half-line: VJ NTSC `HP=844`→845, PAL `HP=850`→851), consistent with **video
clock ≈ RISC clock** on the console (see *Open questions* #2). Prefer driving
the budget from `(HP+1)` when the guest has programmed HP, falling back to the
constants above before HP is written.

### 6.3 Frame / half-line loop (deterministic, headless)

```
fn step_one_frame(state):
    for hl in 0 ..= VP:                      # half_lines = VP+1 ticks
        VC = hl                              # publish into $F00006 (with field bit if interlaced)
        # 1. run CPUs for this half-line's cycle budget, interleaved fine enough
        #    for bus contention determinism (e.g. in K sub-slices):
        run_m68k(m68k_cycles_per_halfline)   # honors SR IPL mask vs pending IRQ level
        run_risc(risc_cycles_per_halfline)   # GPU + DSP + blitter share this; OP below
        # 2. Object Processor: runs once per ACTIVE line (see 6.4)
        if VDB <= hl && hl <= min(VDE, VP):
            if hl is even (non-interlaced):  # OP renders on whole lines
                object_processor_run(line = hl/2)   # builds the line buffer for display
        # 3. Vertical interrupt compare (do AFTER VC update, BEFORE next half-line)
        if hl == (VI & 0x7FF) && (INT1 & C_VIDENA):
            tom_raise_irq(source = VIDEO)    # sets pending, asserts IPL2/vector64
        # 4. PIT / JERRY timers: decrement by this half-line's cycle budget,
        #    raise their IRQs on underflow (independent of raster position)
        advance_timers(risc_cycles_per_halfline)
    # end of field
    present_framebuffer()                    # scan-out is the OP line buffers / direct DRAM read
    frame_count += 1

fn run_n_frames(n): for _ in 0..n { step_one_frame(state) }
```

### 6.4 When the OP runs (per scanline)

- The Object Processor restarts from `OLP` **every active line** when HC reaches `HDB1` (and again at `HDB2` if distinct) (TRM p.15). It runs while `VDB ≤ VC ≤ VDE`.
- The OP **destroys/advances the object list each line** (bitmap HEIGHT decrements, data pointer advances) — so guest ISRs rebuild the list at VI before the YPOS line (REF). The emulator's OP must therefore re-walk from `OLP` each line, not cache.
- In the half-line model, run the OP **once per even VC** (non-interlaced) at the point HC would cross HDB1. For deterministic headless rendering it is sufficient to run it once per displayed line at a fixed sub-position within the half-line, *after* the CPUs have had that half-line's cycles (so writes that the ISR queued at VI are visible). VC must be **latched** at OP start (TRM p.20: "vertical counter is latched when the Object Processor starts so it has the same value across the whole line").

### 6.5 Determinism rules

- **Fixed integer cycle budgets per half-line**; never use wall-clock. Carry any rounding remainder forward (accumulate fractional `half_line_us × hz` and floor each tick) so totals stay exact over a frame.
- **Interleave** the 68000 and RISC at a fixed sub-slice granularity (e.g. 1/4 half-line) if bus-contention accuracy is required; if not modelling contention, run them sequentially per half-line — but pick one and keep it stable.
- **IRQ delivery is edge-checked once per half-line** at the VC==VI / timer-underflow boundary; the 68000 takes it at the next instruction boundary where `pending_level > SR_mask`.
- Seed everything (RNG-free); `run_n_frames(n)` from a fixed reset state must be byte-identical across runs.
- **Headless capture caveat (the internal porting notes):** a framebuffer dump that reads the DRAM the 68000 wrote is **not** the same as the OP scan-out. For a faithful "what's on screen" image the emulator must present the **OP-composited line buffers** (or, for the simple single-bitmap direct case, the DRAM the OP would read via the object's DATA pointer). Validate the pixel format with a fill-and-look test card (the internal porting notes) before trusting captures.

---

## 7. Quick register reference (this subsystem)

| Reg | Addr | Width | R/W | Source |
|---|---|---|---|---|
| HC | `$F00004` | 16 | RW | INC:57, TRM p.12 |
| VC | `$F00006` | 16 | RW | INC:58, TRM p.12 |
| OLP | `$F00020` | **32** | WO | INC:65 |
| OBF | `$F00026` | 16 | WO | INC:66 |
| VMODE | `$F00028` | 16 | WO | INC:67, TRM p.13 |
| BORD1 | `$F0002A` | 16 | WO | INC:68 |
| BORD2 | `$F0002C` | 16 | WO | INC:69 |
| HP | `$F0002E` | 16 | WO | TRM p.14 |
| HBB | `$F00030` | 16 | WO | TRM p.14 |
| HBE | `$F00032` | 16 | WO | TRM p.14 |
| HS | `$F00034` | 16 | WO | TRM p.14 |
| HVS / CONFIG | `$F00036` | 16 | WO / RO | TRM p.15; CONFIG read bit4=NTSC (INC:145) |
| HDB1 | `$F00038` | 16 | WO | INC:70, TRM p.15 |
| HDB2 | `$F0003A` | 16 | WO | INC:71, TRM p.15 |
| HDE | `$F0003C` | 16 | WO | INC:72, TRM p.15 |
| VP | `$F0003E` | 16 | WO | TRM p.16 |
| VBB | `$F00040` | 16 | WO | TRM p.16 |
| VBE | `$F00042` | 16 | WO | TRM p.16 |
| VS | `$F00044` | 16 | WO | INC:73, TRM p.16 |
| VDB | `$F00046` | 16 | WO | INC:74, TRM p.16 |
| VDE | `$F00048` | 16 | WO | INC:75, TRM p.16 |
| VEB | `$F0004A` | 16 | WO | TRM p.17 |
| VEE | `$F0004C` | 16 | WO | TRM p.17 |
| VI | `$F0004E` | 16 | WO | INC:76, TRM p.17 |
| PIT0 | `$F00050` | 16 | WO | INC:77, TRM p.17 |
| PIT1 | `$F00052` | 16 | WO | INC:78, TRM p.17 |
| HEQ | `$F00054` | 16 | WO | TRM p.17 |
| BG | `$F00058` | 16 | WO | INC:79, TRM p.16 |
| INT1 | `$F000E0` | 16 | RW | INC:81, TRM p.16 |
| INT2 | `$F000E2` | 16 | WO | INC:82, TRM p.17 |
| JPIT1 | `$F10000` (rd `$F10036`) | 16 | WO/RO | INC:428, TRM p.85 |
| JPIT2 | `$F10002` | 16 | WO | INC:429 |
| JPIT3 | `$F10004` | 16 | WO | INC:430 |
| JPIT4 | `$F10006` | 16 | WO | INC:431 |
| CLK1 | `$F10010` | 16 | WO | TRM p.84 (proc clock divider) |
| CLK2 | `$F10012` | 16 | WO | TRM p.84 (video clock divider) |
| CLK3 | `$F10014` | 16 | WO | TRM p.85 (chroma divider, reset $3F) |
| CONFIG | `$F14002` | 16 | RW | INC:437 (also NTSC/PAL) |

INT1 bit map (INC:35–45): bit0 VIDENA, 1 GPUENA, 2 OPENA, 3 PITENA, 4 JERENA;
bits 8–12 = clear bits for the same sources (`$0100`…`$1000`).

---

## 8. Open questions (validate against BigPEmu)

1. **Exact field rate / half-line duration.** Use VJ's `31.7778 µs` (NTSC) /
   `32.0 µs` (PAL) per half-line, or derive `risc_cycles_per_halfline` from
   `(HP+1)` once the guest writes HP? They differ by ~1 cycle. Validate by
   counting how many 68000 cycles BigPEmu executes between two consecutive VIs
   for the REF setup. **(UNVERIFIED which is canonical.)**

2. **Video clock vs RISC clock ratio on the console.** This spec assumes
   `video_clock == risc_clock` (CLK1/CLK2 at reset), so HP is in RISC cycles and
   `risc_cycles_per_halfline ≈ HP+1`. Confirm the console's actual VCLK/PCLK
   divider config; if video clock ≠ RISC clock, the per-half-line RISC budget
   must scale by that ratio. **(UNVERIFIED — TRM p.84 says CLK2 resets to 0,
   PCLKDIV per N+1; the effective console ratio is not stated numerically.)**

3. **VI compare semantics: `==` vs `>=`, and field bit.** Does TOM fire VI on
   exact `VC == VI` (this spec) or `VC >= VI`? And is the comparison against the
   full 12-bit VC (incl. field bit $0800) or the low 11 bits? REF sets VI to a
   value reached once per field; confirm on interlaced setups. **(UNVERIFIED.)**

4. **OLP word-swap.** REF writes `OLP = (olp>>16)|(olp<<16)` and
   comments "OLP is written with its 16-bit halves swapped." TRM p.12 describes
   OLP as a plain 32-bit pointer. This swap is likely a REF/Doom-lineage idiom
   compensating for how the 68000 store lands, OR a genuine TOM quirk. The
   emulator's OLP decode must match whichever BigPEmu does — **verify by reading
   back the OP's effective list address.** **(UNVERIFIED — affects OP, not the
   raster scheduler, but flagged here because it's in the video init path.)**

5. **Which clock feeds PIT vs JERRY timers.** TRM says PIT divides "the system
   clock" (p.17) and JERRY timers divide "the processor clock" (p.85). This spec
   treats both as the RISC/processor clock. Confirm PIT's "system clock" is not
   the 68000-rate clock. **(UNVERIFIED.)**

6. **Exact 68000 IRQ level for non-video TOM sources.** VJ uses level 2 for the
   PIT path; this spec assumes all five TOM sources share level 2 / vector 64.
   Confirm GPU/OP/JERRY-relayed interrupts also vector through `$100` and not a
   distinct level. **(UNVERIFIED for GPU/OP/JERRY sources; VERIFIED for
   video+PIT.)**
