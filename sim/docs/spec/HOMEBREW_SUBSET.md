# Homebrew Subset — Implementation Priority Spec for the Jaguar Emulator (v1)

**Purpose.** This document defines the *exact* slice of Atari Jaguar hardware that
the real conveyor-belt projects in this workspace depend on, derived by surveying
their Jaguar backends. It exists to drive the emulator's implementation **order**:
what must work for v1, what is needed soon, and what can be deferred.

**Method.** Eight project backends were read directly (file:line cited throughout).
Five of them (`Reference A`, `Reference B`, `Reference C`, `Reference D`, `Reference E`)
share a **byte-identical boot/video/joypad skeleton** (verified by md5 below), so
their requirements collapse to one common core. `Reference F`, `Reference G`, and
`Reference H` diverge and add the "needed soon"/"rare" features.

**Conventions.** Jaguar is **big-endian**. All multi-byte values are stored
MSB-first. "Phrase" = 64 bits = 8 bytes. "Long" = 32 bits. Tom/Jerry I/O
registers are memory-mapped at `$F00000+` (Tom) and `$F10000+` (Jerry). Tom video
registers are **16-bit** and **must** be accessed as words; OP/GPU/Blitter/DSP
registers are **32-bit longs**.

VERIFIED = confirmed against official SDK `JAGUAR.INC`, the Technical Reference
("the bible"), or proven-on-BigPEmu reference code. INFERENCE/(UNVERIFIED) =
reasoned from project code but not cross-checked against the bible or BigPEmu;
collected in **Open Questions**.

---

## 0. Survey summary table

| Project | Video mode | Res / FB addr | OP objects | Blitter | GPU (SRAM kernel) | DSP/audio | Timers | Joypad | Boot |
|---|---|---|---|---|---|---|---|---|---|
| **Reference A** (cleanest) | RGB16 `$06C7` | 320×240, 3× FB in BSS | 1 BITMAP→STOP | yes: solid spans/bands | yes: span/poly rasteriser | no | no | yes, port 1 | `$4000`, 68k |
| **Reference B** | RGB16 `$06C7` | 320×240, 3× FB in BSS | 1 BITMAP→STOP | (skeleton) | (skeleton) | no | no | yes | `$4000` |
| **Reference C** | RGB16 `$06C7` | 320×240, 3× FB | 1 BITMAP→STOP | (skeleton) | (skeleton) | no | no | yes | `$4000` |
| **Reference D** | RGB16 `$06C7` | 320×224, 3× FB | 1 BITMAP→STOP | (skeleton) | no | DSP voice model (sw mixer stub) | no | yes | `$4000` |
| **Reference E** | RGB16 `$06C7`, **8bpp CLUT** | 320×H, 3× FB, CLUT `$F00400` | 1 BITMAP→STOP, DEPTH=8bpp | (skeleton) | no | no | no | yes | `$4000` |
| **Reference F** | **CRY16** `$0EC1` | 160×180, FB `$1D0000` | 1 BITMAP→STOP | yes: clear/fill | yes: span-fill + geom | **yes: Jerry DSP PCM** | (uses VI only) | yes, ISR table | `$4000` |
| **Reference G** | **CRY16** `$06C1`, OP **PITCH=4** scaling | 80×50→320×200, FB `$1D0000` | 1 BITMAP→STOP w/ vertical 4× | no | yes: render kernel | no | no | (keyboard shim) | `$4000` |
| **Reference H** | **RGB16** `$xxC7` | 320×240, FB in DRAM | **MULTI: bg + N sprites + STOP**, TRANS+REFLECT | (none) | no | DSP sound | no | yes, port 1 | `$4000` |

Skeleton equality (md5, verified):
the shared boot/video/link skeleton is **byte-identical** across Reference A /
Reference B / Reference C / Reference D / Reference E.
The video/joypad sources differ only in resolution macros.

---

## 1. MUST-HAVE for v1 (the intersection — every project needs this)

This is the minimum to boot *any* of these ROMs to a correct displayed frame.
Implement in this order.

### 1.1 — 68000 CPU + 2 MB DRAM + big-endian bus
- **DRAM:** 2 MB at `$000000`–`$1FFFFF`. Stack initialized to `$200000`
  (top of DRAM) by every startup. (Reference A `lea 0x200000,%sp`;
  Reference F `move.l #ENDRAM,a7`; ENDRAM=`$200000`.)
- **Load address `$4000`:** all 8 ROMs load and enter at `$4000`. The boot ROM
  copies cart bytes to DRAM from `$4000` and jumps there (Reference F;
  Reference A `. = 0x4000`).
- **68k vector table at `$000000`:** vectors 0 (reset SSP) / 1 (reset PC), then
  vectors 2..255 at `$0008..$03FF` (4 bytes each). All projects install a
  catch-all exception handler into vectors 2..255 at boot (Reference A;
  Reference H `.vec_loop`). **VERIFIED.**
- **Interrupt autovector:** all Jaguar interrupts arrive at **68k vector 64 =
  address `$100`** (Reference A `JAG_AUTOVEC = 0x100`; init code also uses
  `USER0`). The CPU runs at IPL set by `move.w #0x2000,%sr` to *enable* level-?
  interrupts (Reference A), `#0x2700` to mask all. **VERIFIED.**
- **No 32-bit HW multiply/divide on the 68000.** Projects link a software
  `__mulsi3`/`__divsi3`. The emulator just needs a correct 68000;
  this is a *guest* concern, but note `MULS.W`/`DIVS.W` timing matters for
  cycle accuracy (`DIVS.W` ≈ 150 cycles — the internal porting notes).
- **Endianness setup writes (must be accepted, side-effect optional for v1):**
  - GPU big-endian: `move.l #$00070007,$F0210C` (`G_END`) — Reference A.
  - DSP big-endian: `move.l #$00050007`… actually `#$00050005,$F1A10C` (`D_END`).
    (`G_END`=`BASE+$210C`, `D_END`=`BASE+$1A10C`; JAGUAR.INC:179,530.)
  - Reference H also writes `MEMCON1` (`$F00000`) = `$00070007` at boot
    ("long bus, 16MHz 68k"). For v1 these can be accepted-and-ignored
    NOPs; revisit if a ROM reads them back.

### 1.2 — Tom video timing registers (write-accept + NTSC/PAL detect)
The common skeleton programs these (Reference A). Addresses
VERIFIED against JAGUAR.INC:

| Reg | Addr | Width | Notes |
|---|---|---|---|
| `MEMCON1/2` | `$F00000/$F00002` | 16 | memory config; accept |
| `HC`/`VC` | `$F00004/$F00006` | 16 | horizontal / vertical counters (read) |
| `OLP` | `$F00020` | **32** | Object List Pointer (word-swapped, see 1.4) |
| `OBF` | `$F00026` | 16 | Object Processor Flag (write to re-arm OP) |
| `VMODE` | `$F00028` | 16 | video mode (see 1.3) |
| `BORD1/2` | `$F0002A/$F0002C` | 16 | border color |
| `HDB1/HDB2` | `$F00038/$F0003A` | 16 | horizontal display begin |
| `HDE` | `$F0003C` | 16 | horizontal display end |
| `VDB` | `$F00046` | 16 | vertical display begin (half-lines) |
| `VDE` | `$F00048` | 16 | vertical display end (programmed `$FFFF`) |
| `VI` | `$F0004E` | 16 | vertical interrupt scanline |
| `BG` | `$F00058` | 16 | background color |
| `INT1` | `$F000E0` | 16 | CPU interrupt control |
| `INT2` | `$F000E2` | 16 | CPU interrupt resume |
| `CONFIG`/`HVS` | `$F00036` **and** `$F14002` | 16 | **bit 4 = NTSC(1)/PAL(0)** |

- **NTSC/PAL detect:** `int ntsc = (CONFIG & 0x10) != 0;` (Reference A).
  Note JAGUAR.INC:437 puts `CONFIG` at `$F14002` (== `JOYBUTS`); Reference A
  aliases it at `$F00036`. **The emulator should return bit 4 of
  whatever register the ROM reads** as the NTSC/PAL flag. For headless v1, return
  **NTSC (bit 4 = 1)** consistently. **VERIFIED addr / (UNVERIFIED) which alias
  each project reads.**
- **Horizontal window math (Jaguar Doom lineage, identical in 5 projects):**
  `HDE = (width/2 - 1) | 0x400`; `HDB1=HDB2 = hmid - width/2 + 4`
  (Reference A).
- **Vertical window:** `VDB = vmid - height`; `VDE = 0xFFFF` (wide open).
- **NTSC timing constants** (Reference A):
  `NTSC_WIDTH=1409 HMID=823 HEIGHT=241 VMID=266`;
  `PAL_WIDTH=1381 HMID=843 HEIGHT=287 VMID=322`.
- **For v1 you do NOT need cycle-accurate beam timing** to get a correct
  framebuffer dump — see §1.5 on the headless path. You DO need it for true OP
  scan-out screenshots and for `HC`/`VC` reads if a ROM polls them.

### 1.3 — VMODE decoding (RGB16 is the v1 must-have)
`VMODE` ($F00028, 16-bit). The common skeleton writes **`$06C7`**. Decode
(JAGUAR.INC:149-170, **VERIFIED**):

| Bits | Field | Value in `$06C7` | Meaning |
|---|---|---|---|
| 0 | `VIDEN` | 1 | enable video timebase |
| 2:1 | MODE | `%11` = `$0006` | **RGB16** (also: `$0000`=CRY16, `$0002`=RGB24) |
| 3 | `GENLOCK` | 0 | unsupported on console |
| 5 | `BINC` | 0 | local border color |
| 6 | `CSYNC` | 1 | composite sync |
| 7 | `BGEN` | 1 | clear line buffer to BG color |
| 8 | `VARMOD` | 0 | variable-color resolution |
| 11:9 | `PWIDTH` | `%011` = `$0600` | **PWIDTH4** = pixels 4 clocks wide → 320 visible |

So `$06C7 = RGB16 | CSYNC | BGEN | VIDEN | PWIDTH4`. **VERIFIED**
(`0x0006|0x0040|0x0080|0x0001|0x0600 == 0x6C7`).

**RGB16 pixel layout (the v1 critical detail): `R5[15:11] B5[10:6] G6[5:0]`** —
**blue in the middle, green in the low 6 bits**. This is NOT standard RGB565.
(Reference H `RBG()` macro; Reference E
`(r<<11)|(b<<6)|(g<<1)`; the internal porting notes — **VERIFIED across
two independent sources**.) Test card: `$F800`=red, `$003F`=green-ish? — author
colors through the documented packing.
- Common pitfall: `$xxC3` (DIRECT16) renders nothing under BigPEmu; use `$xxC7`.

**CRY16 (`MODE=%00`, VMODE `$0EC1`/`$06C1`) is NEEDED-SOON, not v1** — only
Reference F and Reference G use it (§2.3).

**8bpp CLUT (`DEPTH=3`) is NEEDED-SOON** — only Reference E (§2.4); requires the
256-entry CLUT at `$F00400` (16-bit RGB entries, JAGUAR.INC:84).

### 1.4 — Object Processor: single BITMAP object → STOP (the core of v1)
**This is the single most important subsystem.** All 8 projects display via the
OP reading a hand-built object list from DRAM. v1 needs exactly **two object
types: BITMAP (type 0) and STOP (type 4).**

**OLP register (`$F00020`, 32-bit):** holds the object-list phrase address, but
written **with its two 16-bit halves swapped**:
`OLP = (olp >> 16) | (olp << 16);` (Reference A; Reference F).
The emulator must un-swap to recover the real pointer.
**VERIFIED (consistent across all projects).**

**BITMAP object = 2 phrases (4 longs).** Field packing as built by Reference A
and Reference F:

```
// link = phrase address of next object = (&next_object) >> 3
op[0] = (fb_addr << 8) | (link >> 8);              // DATA[31:0] high | LINK[18:8]
op[1] = (link << 24)                               // LINK[7:0]
      | (HEIGHT << 14)                             // height in LINES
      | (YPOS   << 4);                             // YPOS in HALF-LINES (line<<... )
                                                   // TYPE=0 (BITMAP) in low 3 bits
op[2] = (flags) | (DWIDTH_high);                   // (PWIDTH>>4) etc.
op[3] = (DWIDTH << 28) | (IWIDTH << 18)
      | (PITCH << 15) | (DEPTH << 12) | XPOS;      // XPOS in pixels
```

Key field semantics (cross-checked with the internal porting notes and
Reference H):
- **LINK is 19 bits** (phrase bits 42:24), **split across both longs**: high-long
  bits 10:0 = `link[18:8]`, low-long bits 31:24 = `link[7:0]`. Mask with `$7FF`
  not `$FF` (a `$FF` mask truncates lists at addresses ≥ `$80000`). LINK is a
  **phrase** address (byte addr >> 3). The OP **re-follows LINK every scanline**,
  so a bad link draws garbage. **VERIFIED.**
- **DATA pointer** (`op[0] >> 8` ... actually `fb_addr` shifted): byte address of
  the bitmap pixel data in DRAM. Caveat: OP bitmap data pointers can misbehave
  for some addresses with bit 16 set ≥ `$100000` under BigPEmu (`$102000`,
  `$1C0000` work). (the internal porting notes — **(UNVERIFIED)** emulator
  behavior; likely a BigPEmu quirk we should NOT replicate.)
- **HEIGHT** = lines (bits ~`23:14` of op[1]).
- **YPOS** = first display half-line; bitmap line N shown at YPOS in **half-lines**
  (`BASE_Y << ...`). Reference A: `(uint32_t)BASE_Y << 4`. Reference F `BASE_Y=24`,
  Reference A `BASE_Y=16`. (Reference H confirms "YPOS encodes half-lines".)
- **DWIDTH** (display width, phrases) and **IWIDTH** (image/data width, phrases) —
  `op[3]` bits 31:28 + (high byte in op[2]) for DWIDTH, bits 27:18 for IWIDTH.
  `SCREEN_PWIDTH = (RENDER_W*2)/8 = 80` for 320px 16bpp (Reference A).
- **PITCH** (op[3] bits ~16:15): `1` = contiguous/no gap. PITCH bit values:
  `PITCH1=0 PITCH2=1 PITCH4=2 PITCH3=3` (JAGUAR.INC:328-331). Reference A uses
  `1<<15`. **NOTE the field meaning differs from the equate** — see Open Q.
- **DEPTH** (op[3] bits 14:12): `O_DEPTH1=0 ... O_DEPTH8=3 O_DEPTH16=4 O_DEPTH32=5`
  (JAGUAR.INC:105-110, **VERIFIED**). v1 only needs **DEPTH16 (4)**; needed-soon
  **DEPTH8 (3)** for Reference E.
- **XPOS** (op[3] low 12 bits): horizontal pixel position. Reference A `BASE_X=16`.

**STOP object = 1 phrase (2 longs):** `op[N]=0; op[N+1]=4;` (TYPE field = 4)
(Reference A; Reference F `_stopobj: dc.l 0; dc.l 4`).
**VERIFIED.**

**OP re-arm:** write `OBF = 0` after pointing OLP (Reference A). The OP destroys its working copy each field, so the ISR rebuilds
the list and rewrites OLP/OBF every vblank (see §1.6).

**v1 OP rendering model (sufficient for all 5 RGB16 projects + Reference F/Reference G
single-object):** Walk the object list from the un-swapped OLP. For each BITMAP,
for each displayed scanline in [YPOS/2 .. YPOS/2+HEIGHT), read the source row at
`DATA + line*IWIDTH_bytes`, composite DWIDTH pixels at XPOS into the line buffer
in the object's DEPTH format, advance via LINK; on STOP, end the frame. Output a
320×240 (or per-project) RGB buffer. **This alone displays every v1 ROM.**

### 1.5 — Headless framebuffer access (the test harness path)
Every project writes a **debug block at DRAM `$3F00`** (longs, big-endian) that
the BigPEmu verify scripts read to locate the live framebuffer:
- `$3F00` [0] = magic (per project: Reference A=`$41334456`, Reference E=`$4D4B3356`)
- [1] = frame_count, [2] = **front framebuffer address** (the displayed FB),
  [3] = dims `(W<<16)|H` (Reference E).
- Reference A extends with profiling/game-state in [3..15].
- Crash scratch at `$820`: magic `$EEEE0000`, SSP, 8 exception-frame longs
  (Reference A). Auto-start pokes at `$3F80/$3F84` magic
  `$0A3DC0DE` (Reference A).

**Implication for the emulator:** expose a debug API to (a) read/write arbitrary
DRAM, (b) dump a framebuffer at a guest-supplied address in a guest-supplied
DEPTH/format, and (c) run N frames headless deterministically. A raw DRAM dump of
the FB at `[2]` *bypasses the OP*, so it validates what the CPU/GPU wrote but NOT
the OP scan-out (PWIDTH/scaling bugs are invisible to it — the internal porting
notes). Provide **both** a DRAM-FB dump and a **true OP composite** screenshot.

### 1.6 — Vertical interrupt (VI) — required for the frame loop
- `VI = a_vdb - 2` (fires ~2 half-lines **before** display start) so the ISR
  rebuilds the OP list before the YPOS line is scanned (Reference A;
  Reference F; Reference H). **VERIFIED.**
- Enable: `INT1 = 0x0001` (Reference A).
- ISR stub (vector `$100`): save d0-d1/a0-a1, call C handler, then
  **`INT1 = $0101`** (clear video latch bit, keep enabled) and **`INT2 = $0000`**
  (resume), `rte` (Reference A; Reference H). **VERIFIED.**
- The handler increments `frame_count`, rebuilds the OP list, rewrites OLP+OBF,
  and latches the pending double/triple-buffer swap (Reference A).
- Some ROMs poll the latch instead of using the vector: Reference G `vsync()` spins
  on `INT1 & 1` then writes `INT1 = 1`. Support both.
- **Emulator requirement:** a per-frame VI at the configured scanline, an
  acknowledge/latch model for `INT1`, and the autovector through `$100`.

### 1.7 — Joypad, port 1 (4-strobe matrix scan)
- Registers: `JOYSTICK` (`$F14000`, 16-bit, **write** = column strobe select),
  `JOYBUTS` (`$F14002`). A **32-bit read of `$F14000`** returns JOYSTICK in the
  high word, JOYBUTS in the low word (Reference A). **VERIFIED.**
- Scan: write strobe, read longword, mask `$F0FFFFFC` (keeps row bits 27:24 +
  fire bits 1:0), rotate per-strobe, AND-accumulate, invert to active-high
  (Reference A, identical logic in Reference H,
  Reference E). **VERIFIED.**

  | Strobe (write to JOYSTICK) | Rotate | Bits decoded |
  |---|---|---|
  | `$81FE` | ror 4 | 23:20 = R,L,D,U; 29 = A; 28 = Pause |
  | `$81FD` | ror 8 | 19:16 = 7,4,1,*; 25 = B |
  | `$81FB` | rol 12 | 7:4 = 2,5,8,0; 13 = C |
  | `$81F7` | rol 8 | 3:0 = 3,6,9,#; 9 = Option |

  Active-high decoded bits: UP=20 DOWN=21 LEFT=22 RIGHT=23 PAUSE=28 A=29 B=25
  C=13 OPTION=9 (Reference A). **VERIFIED.**
- **Emulator requirement:** model the column-strobe matrix so that writing a
  strobe select to `$F14000` then reading the longword returns the active-low
  matrix nibble for that column. Buttons are active-low in hardware; ROMs invert.

### v1 acceptance test
A correct v1 boots **Reference A / Reference B / Reference C / Reference D / Reference E**
(all RGB16; Reference E also exercises 8bpp CLUT) to a stable displayed frame, runs
the VI loop, swaps buffers, and reads joypad input — with a headless N-frame
runner that dumps both the DRAM FB and the true OP composite.

---

## 2. NEEDED SOON (the next tranche — adds Reference F, Reference G, audio, GPU)

### 2.1 — Blitter (Tom): solid spans / bands / framebuffer clear+fill
Used by Reference A and Reference F. Registers (JAGUAR.INC
:220-236, **VERIFIED**; Reference A):

| Reg | Addr | Use |
|---|---|---|
| `A1_BASE` | `$F02200` | dest base (phrase aligned) |
| `A1_FLAGS` | `$F02204` | pixel size / width / addressing mode |
| `A1_PIXEL` | `$F0220C` | `(y<<16)|x` |
| `A1_STEP` | `$F02210` | per-line pointer step `(1<<16)|(-W & 0xFFFF)` |
| `B_CMD` | `$F02238` | write=start; **read bit0=idle** |
| `B_COUNT` | `$F0223C` | `(lines<<16)|pixels` |
| `B_SRCD` | `$F02240` | source/fill pattern (low long) |
| `B_SRCD1` | `$F02244` | source/fill pattern (high long) |

**B_CMD bit values (official JAGUAR.INC, trust ONLY these — wrong twice in derived
sources):** `UPDA1F=$100(d08) UPDA1=$200(d09) UPDA2=$400(d10) DSTA2=$800(d11)`;
plain source-copy LFU = **`$01800000`** (`LFU_SAND|LFU_SAD`); `$00C00000` =
NOT(S^D). `SRCEN=$01(d00)`. (Reference A, Reference F,
the internal porting notes. **VERIFIED**, with an
explicit warning that two prior "fixes" got UPDA1/LFU wrong.)

`A1_FLAGS` bits used: `PIXEL16 = $20` (4<<3); `XADDPIX = $10000` (advance 1 pixel
per inner step); surface width encodings `WID320 = $4200`, `WID160 = $3A00`
(Reference A, Reference F).

**Usage pattern (fire-and-forget):** wait-for-idle (`while(!(B_CMD & 1))`)
**before** setup, never after start; set A1_BASE/A1_FLAGS/A1_PIXEL/B_SRCD/
B_SRCD1/B_COUNT, then `B_CMD = LFU_REP` (single span) or
`B_CMD = UPDA1 | LFU_REP` (multi-line band, stepping A1 by A1_STEP per pass)
(Reference A, Reference F). Short spans (≤12 px)
stay on the CPU. **VERIFIED.**

Minimum Blitter for "needed soon": **solid-color rectangular fill** (the
LFU=`$01800000`, no SRCEN path) with UPDA1 line stepping and XADDPIX pixel
addressing, 16bpp. That covers every documented use. Source-copy/RMW/Z/Gouraud
modes are **rare/defer** (§3).

### 2.2 — GPU (Tom RISC) running an SRAM kernel
Used by Reference A (span/poly rasteriser), Reference F, and Reference G.
Registers (JAGUAR.INC:176-186, **VERIFIED**; Reference A):

| Reg | Addr | Use |
|---|---|---|
| `G_FLAGS` | `$F02100` | GPU flags |
| `G_END` | `$F0210C` | data org (big-endian `$00070007`) |
| `G_PC` | `$F02110` | GPU program counter |
| `G_CTRL` | `$F02114` | bit0 `GPU_RUN`/GO, bit1 `GPU_SINGLE` |
| `G_RAM` | `$F03000` | **GPU internal SRAM, 4 KB** (`G_ENDRAM = G_RAM+4096`) |

- **The GPU executes from SRAM, not DRAM.** Kernels are copied to `$F03000` via
  32-bit writes at init, then `G_PC = $F03000; G_CTRL = 1` to run (Reference A;
  Reference G). **VERIFIED.**
- **Handshake via DRAM mailbox, not synchronous halt.** Reference A uploads once,
  restarts per batch, and polls a DRAM "done" sentinel with a bounded timeout;
  on timeout forces `G_CTRL=0`. The kernel ends with a sentinel
  write + finite NOP sled, **not** an infinite spin (a Reference F kernel
  header explains BigPEmu starves the 68k thread on a spinning GPU). Parameters
  are passed in a fixed SRAM slot (`G_PARAMS = $F03F00`, Reference A).
- **Emulator requirement:** implement the Jaguar RISC ISA (shared GPU/DSP core),
  4 KB GPU SRAM at `$F03000`, `G_PC`/`G_CTRL` start/stop, and let the kernel
  read/write DRAM. The GPU and 68k can be cycle-interleaved (BigPEmu runs them on
  separate threads; for determinism the emulator should interleave deterministically).
  Cross-check ISA against the JRISC reference (e.g. `MOVEI`, `STOREW`, `LOADW`,
  `ADDQMOD`, `JUMP`/`JR` with 2 delay-slot NOPs — seen in the reference kernels).

### 2.3 — CRY16 video mode (Reference F, Reference G)
`VMODE` MODE=`%00` (`CRY16=$0000`). Reference F uses `$0EC1`/`$0EC1` style at
160-wide (`move.w #$EC1,VMODE`); Reference G `$06C1` at 320-wide. **CRY** = Cyan-Red-intensitY: 16-bit pixel `C[15:12] R[11:8]
Y[7:0]` (intensity in low byte). The emulator needs a CRY→RGB conversion table
for the OP/output stage. Reference G embeds a precomputed CRY LUT. **Decode table itself is (UNVERIFIED) here — see Open Q;
cross-check against the bible's CRY color table.**

### 2.4 — 8bpp CLUT bitmaps + CLUT (Reference E)
- OP BITMAP with **DEPTH=3 (8bpp)**; each pixel byte indexes the **256-entry
  CLUT at `$F00400`** (16-bit RGB16 entries, JAGUAR.INC:84). Reference E fills CLUT
  via `plat_set_palette` (, same `R5 B5 G5<<1` packing).
  `SCREEN_PWIDTH = FB_W/8` (1 byte/pixel → 8 px/phrase). **VERIFIED addr;
  semantics straightforward.**

### 2.5 — Jerry DSP audio (Reference F, Reference D, Reference H)
Used for PCM playback. Registers (JAGUAR.INC:530-538, Reference F):

| Reg | Addr | Use |
|---|---|---|
| `D_END` | `$F1A10C` | DSP data org (`$00050005` big-endian) |
| `D_PC` | `$F1A110` | DSP program counter |
| `D_CTRL` | `$F1A114` | bit0 run |
| `D_RAM` | `$F1B000` | **DSP internal SRAM, 8 KB** (`D_ENDRAM=D_RAM+8192`) |
| `MODMASK`| `$F1A118` | address wrap mask (e.g. `$FFFFF000` = 4 KB ring) |
| `SCLK` | `$F1A148` | serial clock divider (Reference F writes 602) |
| `SMODE` | `$F1A14C` | serial mode (SDEN=1) |
| `LTXD` | `$F1A150` | left I2S transmit data |
| `RTXD` | `$F1A154` | right I2S transmit data |

- Reference F: DSP program polls a 4 KB-aligned DRAM ring at
  `$1C0000` (2048×16-bit samples), writes each sample to LTXD+RTXD with a timed
  delay (~11025 Hz mono); the 68k mixes into the ring. DSP
  started by `D_PC = dsp_mix_start; D_CTRL = $00010001`.
  **VERIFIED addresses/flow.**
- Reference D has only a *voice model + software mixer stub* — DSP
  program "to be finalized on hardware". Reference H ships its own DSP sound program.
- **Emulator requirement (needed-soon):** Jerry RISC core (same ISA as GPU but
  8 KB SRAM at `$F1B000`), the I2S serial output path (LTXD/RTXD → audio samples
  at the SCLK-derived rate), and `MODMASK` address wrapping. Sound is lower
  priority than video for the conveyor goals; the DSP RISC core is shared work
  with the GPU.

### 2.6 — Multi-object OP composite with transparency + reflect (Reference H)
Reference H builds a list of **bg BITMAP + up to N sprite BITMAPs + STOP**, z-ordered
by list order, each sprite using **`O_TRANS = $00008000`** (color 0 transparent)
and optionally **`O_REFLECT = $00002000`** (horizontal flip) in the object's high
long (Reference H; JAGUAR.INC:100-102 **VERIFIED**:
`O_REFLECT=$2000 O_RMW=$4000 O_TRANS=$8000`). This is the same BITMAP type as v1
plus two flag bits and N objects per list. **Recommended to fold into v1's OP
implementation** since it's a small delta (honor TRANS/REFLECT, iterate N
objects) and unblocks a whole project.

---

## 3. RARE / DEFER (implement last or stub)

- **RGB24 mode** (`VMODE` MODE=`%01`, `$0002`): no surveyed project uses it.
- **CRY scaling beyond simple OP PITCH replication / SCALED objects:** only
  Reference G uses **OP vertical scaling via PITCH=4** line-replication (not a SCALED
  object) — `op_list[3] |= (4<<15)`, `IWIDTH=20`, `DWIDTH=80` to turn 80×50 into
  320×200. True **SCALED objects (type 1)** with horizontal/
  vertical scale registers are **not used by any project** — defer.
- **OP BRANCH objects (type 3), GPU objects (type 2):** unused — defer.
- **Z-buffer, Gouraud shading, phrase-mode textured Blitter, RMW Blitter:** no
  project uses Blitter Z/Gouraud; rasterisation is done by GPU kernels writing
  flat/patterned 16-bit pixels (Reference A). Defer.
- **Timers (JPIT1/JPIT2 `$F10000/$F10002`, JAGUAR.INC:428-429):** none of the
  surveyed projects program the programmable timers; they all pace off **VI**.
  Implement VI first; timers can be stubbed until a ROM needs them. **(Audio
  timing in Reference F comes from a DSP delay loop, not a hardware timer.)**
- **Cartridge EEPROM / save, GPIO, cartridge bank switching:** unused — defer.
- **Second joypad port, analog/rotary, Team Tap:** only port 1 is read. Defer
  port 2+ (Reference E `plat_input` returns 0 for `player != 0`).
- **GPU/Blitter as 68k-visible register aliasing quirks** (G_PC/G_CTRL overlap
  A1_PIXEL/A1_STEP when read by the 68k — Reference G note):
  document but low priority.

---

## 4. Boot sequence (canonical, shared by all 8 — implement once)

This is the exact common startup the emulator must satisfy (Reference A,
Reference F, Reference H):

1. Loader copies cart image into DRAM at `$4000`, jumps to `$4000` (= `_start`).
2. `move.w #$2700,%sr` — mask interrupts.
3. `move.l #$00070007,$F0210C` (G_END) — GPU big-endian.
4. `move.l #$00050005,$F1A10C` (D_END) — DSP big-endian.
   (Reference H also `MEMCON1 = $00070007`.)
5. `lea $200000,%sp` — stack at top of DRAM.
6. `move.w #$FFFF,$F0004E` (VI) — suppress VI during setup.
7. Install catch-all exception handler into 68k vectors 2..255 (`$0008..$03FF`).
8. **Clear .bss (and ideally the whole region up to `$200000`)** — many ROMs
   rely on zeroed uninitialized DRAM, not just BSS (the internal porting
   notes; Reference H clears BSS→`$200000`). Reference A clears only
   the linker `.bss` span; Reference F/Reference H clear to ENDRAM.
9. `jsr main` / `jsr _jag_main`; on return, spin forever.
10. main installs the VI vector at `$100`, programs video timing + VMODE, builds
    the OP list, sets `VI = vdb-2`, `INT1 = 1`, `move.w #$2000,%sr` (enable IRQ),
    then enters the per-frame loop.

**Linker layout (Reference A):** `. = 0x4000`; `.text` (startup first) →
`.rodata` → `.data` → `.bss (NOLOAD)`; `__bss_start`/`__bss_end` symbols mark the
BSS clear range. **VERIFIED.**

---

## 5. ROM / COF format (cartridge loader requirement)

All 8 projects ship a **`.cof`** (TI/m68k COFF) plus a raw **`.bin`** (and an
`.elf`). The emulator's cartridge loader should accept **at least `.cof` and raw
`.bin`**. COF layout (Reference A, **VERIFIED** — matches rln /
Virtual Jaguar / BigPEmu / Skunkboard jcp):

- **File header (20 bytes, big-endian):** magic **`0x0150`** (m68k COFF),
  numSections=3, …, optHdrSize=28, flags=`0x0003` (no relocs, executable).
  (All 8 `.cof` files confirmed to start with magic `0x0150`.)
- **a.out optional header (28 bytes):** magic `0x0107`, tsize, dsize=0, bsize,
  entry, text_start (= `$4000`), data_start.
- **3 section headers (40 bytes each):** `.text` (`STYP_TEXT=0x20`) at file offset
  `0xA8`, `.data` (`0x40`), `.bss` (`0x80`).
- **Raw image** begins at file offset **`0xA8`**, loaded contiguously at `--addr`
  (`$4000`), entry `$4000`.

So the minimal loader: read COFF, find `.text` `scnptr`/`paddr`/`size`, copy image
to DRAM at `paddr`, set PC = entry. A raw `.bin` is just "load at `$4000`, PC =
`$4000`".

---

## 6. Test fixtures (ready-built ROMs in this workspace)

Use these as emulator boot/regression fixtures. All are real build outputs; the
`.cof` is the loadable ROM, `.bin` the raw image. (Sizes as of survey.)

| Project | ROM (.cof) | Raw (.bin) | Exercises (v1 relevance) |
|---|---|---|---|
| Reference A | `.cof` (57 KB) | `.bin` | **RGB16 OP, Blitter, GPU span kernel** — the cleanest full v1+soon ROM. GPU blob: `gpu_span.bin`/`.elf` |
| Reference B | `.cof` (288 KB) | `.bin` | RGB16 OP single object (v1 core) |
| Reference C | `.cof` (12 KB) | `.bin` | **Smallest RGB16 ROM** — ideal first bring-up fixture |
| Reference D | `.cof` (13 KB) | `.bin` | RGB16 OP 320×224 + DSP voice model |
| Reference E | `.cof` (887 KB) | `.bin` | **8bpp CLUT OP path** + CLUT `$F00400` |
| Reference F | `.cof` (1.25 MB) | `.bin` | **CRY16 160×180**, Blitter clear/fill, GPU span+geom kernels, **Jerry DSP PCM** |
| Reference G | `.cof` (873 KB) | — | **CRY16 + OP PITCH=4 vertical scaling**, GPU render kernel |
| Reference H | `.cof` (2.2 MB) | — | **Multi-object OP composite, TRANS + REFLECT sprites**, RGB16 |

**Golden reference dumps already captured** (use for byte-parity checks of the
DRAM framebuffer — these are DRAM dumps, *not* OP scan-out, so they validate the
CPU/GPU/Blitter pipeline up to the OP):
- Reference F: `build/capture_*/boot_verify_fb.bin`, `…_fb_full.bin`,
  `…_scratch.bin` (the `$800` crash scratch), `…_code.bin`.
- Reference H: `build/test_out/*_fb*.bin`, `build/{flow,op,input_test}_out/dbg.bin`
  (the `$3F00` debug block snapshots).
- Reference G / Reference A: `build/test_out/boot_verify_fb.bin`, `…/fb.bin`, `ref.bin`.

These let the emulator author diff `emulator FB dump @ $3F00[2]` against a
known-good BigPEmu capture without a display.

---

## 7. Ranked implementation order (the bottom line)

1. **68000 + 2 MB DRAM + big-endian bus + `$4000` load + COF loader** (§1.1, §5).
2. **Boot sequence acceptance** (endianness regs, BSS clear, vectors, stack) (§4).
3. **Tom video register file + NTSC detect + VMODE RGB16 decode** (§1.2, §1.3).
4. **Object Processor: BITMAP + STOP, single object, RGB16 composite → frame**
   (§1.4) — *the keystone*.
5. **VI interrupt + frame loop + double/triple-buffer swap** (§1.6).
6. **Joypad port 1 matrix scan** (§1.7).
7. **Headless N-frame runner + DRAM-FB dump + true-OP screenshot + `$3F00`/`$820`
   debug API** (§1.5) — needed to *verify* everything above against the fixtures.
   → **At this point all 5 RGB16 ROMs boot and verify.**
8. **8bpp CLUT depth + CLUT `$F00400`** (§2.4) → unblocks Reference E fully.
9. **Multi-object OP + TRANS + REFLECT** (§2.6) → unblocks Reference H.
10. **Blitter solid fill/span/band** (§2.1).
11. **GPU RISC core + 4 KB SRAM kernels + DRAM mailbox** (§2.2) → unblocks
    Reference A/Reference F GPU paths.
12. **CRY16 mode + CRY→RGB table** (§2.3) → unblocks Reference F/Reference G display.
13. **OP PITCH line-replication scaling** (§3, for Reference G).
14. **Jerry DSP RISC core + I2S audio** (§2.5).
15. Defer everything in §3 not listed above.

---

## 8. Open questions (validate against BigPEmu / the bible)

1. **(UNVERIFIED) Exact OP BITMAP bit field boundaries.** The packing in §1.4 is
   reconstructed from the shift constants in Reference A and Reference H,
   cross-checked with the internal porting notes. Confirm the
   precise bit ranges of HEIGHT, YPOS, DWIDTH, IWIDTH, PITCH, DEPTH, XPOS within
   the two phrases against the Technical Reference's Object Processor chapter
   (the bible PDF: the Atari SDK's `Jaguar Technical
   Reference v8.pdf` — search "BITMAP object", "OBJLIST"). In particular **PITCH:**
   projects write `1<<15` for "contiguous", but JAGUAR.INC's `PITCH1=0`/`PITCH2=1`
   equates are *field values*, so the bit position used by the projects
   (`1<<15`) must be reconciled with the field's documented location.
2. **(UNVERIFIED) YPOS scaling.** Projects pass `BASE_Y << 4` and comments say
   "YPOS in half-lines". Confirm whether the OP interprets YPOS as half-lines and
   the exact shift, vs. lines, against the bible.
3. **(UNVERIFIED) CRY16 → RGB conversion table.** Needed for Reference F/Reference G.
   Take the canonical CRY color table from the Technical Reference (or compare
   against Reference G's) rather than inventing one.
4. **(UNVERIFIED) RGB16 channel widths.** Two project sources agree on
   `R5[15:11] B5[10:6] G6[5:0]`, but Reference E's CLUT packs green as `g5<<1` (5
   bits in 6:1) while Reference H packs `g>>2` (6 bits in 5:0). Confirm green is 6
   bits (G6) and the low-bit alignment against the bible/BigPEmu test card.
5. **(UNVERIFIED) Which address the NTSC/PAL `CONFIG` bit is read from.**
   JAGUAR.INC:437 says `$F14002`; Reference A aliases `$F00036`. Implement bit 4 on
   both reads; confirm hardware behavior.
6. **(UNVERIFIED) `INT1` latch/ack semantics.** ISRs write `INT1 = $0101`
   (clear+enable) and `INT2 = $0000`. Confirm exact bit meaning (which bit is the
   video latch, which enables, what INT2 resume does) against the bible's
   interrupt chapter — needed for correct re-fire timing.
7. **(UNVERIFIED) Jaguar RISC (GPU/DSP) ISA details and timing.** The kernels use
   `MOVEI` (3-word immediate), `STOREW`/`LOADW`, `ADDQMOD`, `JUMP`/`JR` with two
   delay-slot NOPs, `STORE`. Validate the full opcode set, encodings, register
   file (r0..r31, banks), and cycle counts against the JRISC reference / bible
   before relying on cycle accuracy. (Audio rate in the DSP kernel assumes
   ~26.59 MHz and ~3 cycles/inner-instruction.)
8. **(UNVERIFIED) Whether to replicate BigPEmu's OP high-address data-pointer
   quirk** (bit 16 set ≥ `$100000` misreads). This is likely a BigPEmu bug, not
   real hardware; the emulator should probably *not* replicate it — confirm.
9. **(UNVERIFIED) Blitter `WID320`/`WID160` width encodings** (`$4200`/`$3A00`):
   these encode surface width in a non-obvious format. Confirm the encoding
   formula against the bible's Blitter chapter for arbitrary widths.
10. **(INFERENCE) GPU/68k interleave determinism.** BigPEmu runs them on separate
    threads and *approximates* sync; the porting notes warn its GPU performance is
    not a real-hardware proxy. For a deterministic emulator, define a fixed
    interleave (e.g. cycle-stepped) and validate the GPU-kernel fixtures
    (Reference A span kernel, Reference F writing `$BEEF` to
    `$001800`) round-trip correctly.
