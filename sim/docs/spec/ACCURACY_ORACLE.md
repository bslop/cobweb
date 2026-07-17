# ACCURACY ORACLE — Consolidated Jaguar/BigPEmu Accuracy & Quirk Catalogue

Implementation-grade regression-oracle target list for a from-scratch,
cycle-accurate Atari Jaguar emulator in Rust. **v1 success bar = match BigPEmu**
on the behaviors below; hardware-only facts are flagged where BigPEmu reportedly
diverges so the author can make them configurable.

**Conventions**
- All Jaguar hardware is **big-endian**. Multi-byte values are MSB-first.
  Word = 16-bit, long = 32-bit, phrase = 64-bit (8 bytes, the OP/Blitter unit).
- Register addresses below are absolute (BASE = `$F00000`).
- Each entry tags: **[HW]** = hardware truth the emulator must replicate;
  **[BIGPEMU]** = BigPEmu-specific behavior to match for the oracle;
  **[HW≠BIGPEMU]** = the two reportedly differ → make it configurable.
- `(UNVERIFIED)` = inferred / single-source / BigPEmu-only; validate against
  BigPEmu and (where noted) real hardware. Listed again in *Open questions*.

**Provenance.** Facts below are attributed generically to "a reference backend",
"a reference title", or "the internal porting notes"; none maps to a specific
game or project.
Authoritative register source: the Atari SDK's `JAGUAR.INC`
(mirrored in `/opt/BigPEmu/Scripts/include/jagregs.h`).

---

## 0. Cross-cutting BigPEmu execution model

- **[BIGPEMU] GPU and 68000 run on separate host threads with approximated
  sync.** (the internal porting notes §1; a reference title's notes.) This single fact drives
  most GPU gotchas. The emulator does NOT have to reproduce the *threading*, but
  must reproduce the *observable consequences* below (notably the "GPU store to
  G_CTRL doesn't halt" quirk if matching BigPEmu) and should NOT reproduce the
  performance artifacts (they are emulator-specific, not hardware).
- **[BIGPEMU] "Fire GPU then immediately halt" reads as 'GPU never ran'.**
  Pattern `G_CTRL=GPU_RUN; G_CTRL=0;` gives the GPU thread no scheduling quantum;
  even an intervening `G_FLAGS` read does not trigger execution.
  (a reference title's notes.) Real HW: GPU writes its sentinel in ~188 ns
  (5 instrs × 37.6 ns @ 26.59 MHz) and the 68000 sees it on the first poll.
  → **[HW≠BIGPEMU]**, configurable: a cycle-accurate emulator should let the GPU
  make progress in this window (HW-correct), and optionally expose a
  "BigPEmu-compat" mode where a fire+immediate-halt produces no GPU output.
- **[BIGPEMU] 68000 busy-polling DRAM while the GPU runs starves the GPU thread**
  (~hundreds of µs / ~500 µs per 68k↔GPU DRAM sync; GPU-RAM reads from the 68k
  cost ~1 ms each). (the internal porting notes §1; a reference title's notes.) Pure
  emulator artifact — **do NOT replicate**; it only matters as an explanation of
  why guest code uses DRAM mailbox + register-only delays.
- **[BIGPEMU] Determinism:** frame-counted capture is reproducible. Scripts fire
  at a fixed local emu-frame (60/150/300/360/800). Modules that alter timing must
  call `bigpemu_set_module_usage_flags(pMod, BIGPEMU_MODUSAGE_DETERMINISMWARNING)`
  (`avp.c:264`). Our emulator must be **frame-count deterministic** for the
  capture/verify flows to port.

---

## 1. 68000 / Bus

### Memory map (verified equates — `jagregs.h`, `JAGUAR.INC`)
- **[HW]** DRAM: `$000000`–`$1FFFFF` (2 MB). Top-of-DRAM = ENDRAM. (the internal porting notes §2;
  a reference backend memory map.)
- **[HW]** TOM register BASE = `$F00000`. GPU local SRAM `$F03000`–`$F03FFF`
  (4 KB). DSP (Jerry) local SRAM `$F1B000`–`$F1CFFF` (8 KB, `D_RAM`..`D_ENDRAM`).
  (`jagregs.h:120-121,393-394`.)
- **[HW]** Cartridge space `$800000+`; other unmapped high regions per probe below.

### Bus-error / unmapped-access behavior — **THE flagship HW≠BIGPEMU divergence**
- **[HW≠BIGPEMU] BigPEmu never raises 68k bus errors.** (the internal porting notes §1 "68k bus
  behavior", probe-verified a reference title 2026-06.) Exact observed returns:
  - Unmapped **high-memory** reads (sign-extended `(xxx).w` → `$FFFFxxxx`) return
    **`$FFFF`**.
  - Unmapped **cart-space** reads (`$A1xxxx`, `$C0xxxx`) return **`$0000`**.
  - Writes to either **vanish silently**. Nothing hangs, nothing faults.
  - Consequence: code relying on a 68k bus-error handler to emulate missing
    hardware **never runs**; garbage reads silently steer control flow.
  → Make configurable: a **"real bus-error" mode** (assert the 68000 bus-error
    exception vector, group-0 fault) vs. a **"BigPEmu-compat" mode** with the
    exact `$FFFF`/`$0000`/silent-write semantics above. Default = BigPEmu-compat
    for the oracle; expose the mode in the debug API.

### Interrupt / IPL behavior
- **[BIGPEMU] The Jaguar Video Interrupt (VI) reaches the 68000 at a LOW IPL.**
  Empirically masked by `move #$2300,sr` and up — level **≤ 3** under BigPEmu,
  vectored via **`$100`/USER0** (autovector). BigPEmu boots the 68000 at **IPL 0**.
  (the internal porting notes §1.) Genesis/arcade-style masks (`$2300/$2500/$2700`) silently starve
  the VI. → Implement VI as an autovectored IRQ at a **low level (≤3)** and boot
  with **SR IPL = 0**. `(UNVERIFIED exact level)` — validate 1/2/3 against BigPEmu.
- **[HW]** TOM `INT1` (`$F000E0`) bit 0 = video-interrupt enable; ISR must
  re-acknowledge. (a reference backend writes `INT1=0x0001`; ISR runs at `move.w
  #0x2000,sr` → IPL 0.) Re-entrancy is the guest's problem at IPL 0.
- **[HW]** Jerry interrupt-control register `J_INT` = `$F10020`; enable/clear bit
  layout `J_*ENA`/`J_*CLR` per `jagregs.h:314-340` (EXT/DSP/TIM1/TIM2/ASYN/SYN).
- **[HW]** TOM `INT2` = `$F000E2`.

### 68000 arithmetic / toolchain-visible HW limits
- **[HW] No 32-bit hardware multiply/divide** on the 68000. `MULS.W`/`DIVS.W`
  only. `DIVS.W` ≈ **~150 cycles** (worst case). (the internal porting notes §2.) Software
  `__mulsi3`/`__divsi3` needed; naive `__mulsi3` mishandles negative operands
  (16-bit halves are signed under `MULS.W`).
- **[HW] `bsr.l` (opcode `0x61FF`) is NOT a 68000 instruction.** A real 68000
  decodes `0x61FF` as `bsr.s -1` → odd-address jump → **address error** (group-0,
  vector 3). (a reference backend "libgcc poisoning".)
  → The emulator MUST raise an **address error** on a branch to an odd target and
  on odd-address word/long access (unless BigPEmu suppresses it — see open Qs).
  This bit real ports on BigPEmu, so BigPEmu **does** fault here (at least via the
  resulting odd-address access). `(UNVERIFIED whether BigPEmu faults on the
  decode itself vs. on the subsequent odd fetch)`.
- **[HW]** 68000 clock ≈ **13.3 MHz** ("~13.3 MHz 68000", the internal porting notes §4). BigPEmu
  exposes the relation via `bigpemu_jag_m68k_cycle_for_usec()` /
  `bigpemu_jag_usec_for_m68k_cycle()`. `(UNVERIFIED exact divisor)`.

### Boot / reset state
- **[HW]** **Zero BSS *and the stack region* up to ENDRAM at boot.** C code reads
  uninitialized locals expecting 0; clearing only BSS leaves stack garbage →
  bugs that appear only on complex code paths (heavy renderer black-screens while
  simple scene works). Zeroing all DRAM to ENDRAM ~0.2 s on HW. (the internal porting notes §2.)
  → For the oracle: DRAM power-on contents should be a **defined, reproducible**
  pattern (BigPEmu auto-resets state on software load — `bigpcrt.h:200`); pick
  zero-fill and document it so capture diffs are stable. `(UNVERIFIED: BigPEmu's
  exact power-on DRAM fill — assume 0; confirm.)`
- **[HW]** `G_END`/`D_END` (endianness-select for GPU/DSP) must be set first
  thing at reset: `G_END=0x00070007`, `D_END=0x00050007`/`0x00050005` (see RISC
  §). (a reference backend provenance; gives
  `G_END=0x00070007`, `D_END=0x00050005`.)
- **[HW]** CONFIG register `$F00036`: **bit 4 = NTSC (1) / PAL (0)**.
  (a reference backend `(CONFIG & 0x10)`.) Note `jagregs.h` aliases
  `CONFIG=JOYBUTS=$F14002`; the NTSC bit is read at `$F00036` in the proven path.
  `(UNVERIFIED reconciliation of the two CONFIG addresses — see open Qs.)`

---

## 2. TOM — Video timing & Object Processor (OP)

### Register width — **critical, cost real debugging**
- **[HW] TOM video registers are 16-BIT. Never access them 32-bit.** (a reference title
  2026-06, via the internal porting notes §1 "Video output".) Words: VMODE `$F00028`, BORD1
  `$F0002A`, BORD2 `$F0002C`, HC `$F00004`, VC `$F00006`, HDB1 `$F00038`, HDB2
  `$F0003A`, HDE `$F0003C`, VS `$F00044`, VDB `$F00046`, VDE `$F00048`, VI
  `$F0004E`, BG `$F00058`, OBF `$F00026`, PIT0/1 `$F00050/52`, INT1/2
  `$F000E0/E2`. **Longs only:** OLP `$F00020`, and all GPU/Blitter/DSP regs.
  - Failure mode the emulator must reproduce: a 32-bit `VMODE=$06C7` store lands
    big-endian as VMODE=`$0000` + `$06C7` spilled into BORD1, **zeroing VMODE's
    PWIDTH** → every pixel ~4× too narrow → frame squished to ~70px-wide strip
    (full height, correct colors). So a 32-bit write to a word register must
    write **two adjacent word registers** (high half → addr, low half → addr+2),
    big-endian. (the internal porting notes §1.)

### VMODE (`$F00028`) bit layout (`jagregs.h:86-109`)
- **[HW]** VIDEN bit0=`$0001`. MODE field (bits 2:1): CRY16=`$0000`, RGB24=`$0002`,
  DIRECT16=`$0004`, **RGB16=`$0006`**. GENLOCK `$0008`, INCEN `$0010`, BINC
  `$0020`, CSYNC `$0040`, BGEN `$0080`, VARMOD `$0100`. PWIDTH (bits 11:9)
  `$0200`/step (PWIDTH1..8 = `$0000`,`$0200`,…,`$0E00`).
- **[BIGPEMU] Use VMODE `$xxC7` for direct-color** (MODE=RGB16 %11 per the proven
  value `$06C7` = VIDEN|RGB16|CSYNC|BGEN|PWIDTH4). The **`$xxC3` ('DIRECT16')
  mode renders nothing sanely under BigPEmu**; a legacy `jaguar.inc` comment
  calling it "5-5-5 RGB packed" is **wrong**. (the internal porting notes §1.) Proven good value =
  `0x06C7` (a reference backend).

### Pixel format — RGB16 ("RBG16")
- **[HW] RGB16 layout: R5 bits 15:11, B5 bits 10:6, G6 bits 5:0** — **blue in the
  MIDDLE**, green 6-bit. Written big-endian exactly as the 68000 stores words.
  (the internal porting notes §1; a reference backend; macro `JRGB(r,g,b) =
  ((r&0x1F)<<11)|((b&0x1F)<<6)|(g&0x3F)`.) Used by reference titles.
- **[HW]** CRY16 is the other common 16bpp mode (Cyan-Red-intensitY); Jaguar Doom
  framebuffers are CRY16. Emulator must
  composite both CRY and RGB OP objects.
- **[HW] Genesis CRAM → RGB conversion is `----BBB-GGG-RRR-`** (B bits 11:9, G
  7:5, R 3:1), **not** nibble-packed at 8:6/5:3/2:0. (a reference title/a reference title asset
  note, the internal porting notes §1.) This is an asset-converter fact, not an OP fact, but
  relevant if emulating Genesis-derived palettes.

### NTSC/PAL timing constants (`jagregs.h:76-84`)
- **[HW]** NTSC: WIDTH=1409, HMID=823, HEIGHT=241, VMID=266.
  PAL: WIDTH=1381, HMID=843, HEIGHT=287, VMID=322. **X units = pixel clocks,
  Y units = half-lines** (vertical counter increments every half-line).
- **[HW]** Proven init math (a reference backend, provenance Jaguar Doom):
  - `HDE = (width/2 - 1) | $400`
  - `HDB1 = HDB2 = hmid - width/2 + 4`
  - `VDE = $FFFF` (programmed wide-open; computed vde only used for layout)
  - `VDB = a_vdb`; **VI fires at `vdb-2`** (just before display, so the ISR can
    rebuild the OP list before scan-out).
  - Bitmap **HEIGHT is in lines**, **YPOS in half-lines** (= BASE_Y*2).
  - `BG=0, BORD1=0, BORD2=0`.
- **[HW]** HC (`$F00004`) horizontal counter, VC (`$F00006`) vertical counter are
  readable position counters. BigPEmu exposes `bigpemu_jag_get_line_count()`,
  `get_horizontal_period()`, `get_frame_period()`, `get_display_region()`.

### Object Processor list format
- **[HW]** OLP (`$F00020`, **long**) points at the OP list; **written with its
  two 16-bit halves swapped**: `OLP = (olp>>16) | (olp<<16)`. (a reference backend.) `(UNVERIFIED whether the swap is HW-required or a Doom-ism;
  the proven path does it — validate.)`
- **[HW]** OBF (`$F00026`, word) = Object-list Flag; cleared to 0 after building.
- **[HW] OP object types** (`jagregs.h:41-45`): BITOBJ=0, SCBITOBJ=1 (scaled),
  GPUOBJ=2, BRANCHOBJ=3, STOPOBJ=4. Object phrase low 3 bits = type.
- **[HW] BITMAP object encoding** (2 phrases) — proven build (a reference backend), all big-endian longword pairs:
  ```
  op[0] = (fb_addr << 8) | (link >> 8)
  op[1] = (link << 24) | (HEIGHT << 14) | (YPOS << 4)     ; YPOS in half-lines
  op[2] = SCREEN_PWIDTH >> 4
  op[3] = (SCREEN_PWIDTH << 28) | (SCREEN_PWIDTH << 18)
        | (1<<15)            ; PITCH 1 (contiguous)
        | (4<<12)            ; DEPTH = 16bpp (O_DEPTH16, jagregs.h:56)
        | BASE_X
  ; where link = (&next_object) >> 3  (phrase address)
  ; SCREEN_PWIDTH = (RENDER_W*2)/8  (phrases per line; 320→80)
  ```
- **[HW] OP LINK field is 19 bits**, phrase bits **42:24**, split across both
  longs: **high long bits 10:0 = link[18:8]**, **low long bits 31:24 =
  link[7:0]**. Masking the high part with `$FF` instead of **`$7FF`** truncates
  lists at ≥`$80000`. The OP **re-follows LINK every scanline**, so a bogus link
  on non-zero memory draws garbage over the frame (latent until something real
  occupies the bogus address). (the internal porting notes §1.)
- **[HW]** DEPTH field encodings (`jagregs.h:52-57`): O_DEPTH1=0…O_DEPTH8=3<<12,
  **O_DEPTH16=4<<12, O_DEPTH32=5<<12**. Object flags: O_REFLECT=`$2000`,
  O_RMW=`$4000`, O_TRANS=`$8000`, O_RELEASE=`$10000`. GAP/BREQ fields
  `jagregs.h:59-71`.
- **[HW]** STOP object = type 4: `op[4]=0; op[5]=4`. (a reference backend.)
- **[BIGPEMU] OP bitmap initial data pointers can silently break for some
  addresses ≥ `$100000`** — specifically **bit 16 of the byte address set fails**
  (observed: `$159780` reads other memory; `$102000` and `$1C0000` work). The
  per-scanline data *increment* traverses high addresses fine; only the parsed
  *initial* pointer misbehaves. (the internal porting notes §1.) → **[HW≠BIGPEMU] `(UNVERIFIED)`**:
  likely a BigPEmu OP-pointer-parse quirk; the emulator should parse the full
  pointer correctly (HW) but the author must know capture flows may have placed
  framebuffers to dodge this. Flag configurable / document.
- **[BIGPEMU]** OP register/state largely accessed via memory reads in scripts;
  BigPEmu additionally provides a **script-side OP compositor** API
  (`bigpemu_jag_op_add_poly`, `op_create_frame_tex`, `op_render_bitmap_object_to_buffer`,
  `op_set_special_transparency`, `op_clear_buffers`, `op_set_alpha_fill`,
  `op_enable_play_area_scissor`) — this is BigPEmu's HLE poly path, **not** Jaguar
  HW. Our emulator does not need it for the oracle but should be aware capture
  tooling may invoke it.

### Headless capture caveat — **OP scan-out vs DRAM**
- **[BIGPEMU/method] Headless framebuffer dumps read the DRAM the 68000 wrote,
  not the OP scan-out.** They PASS even when VMODE/OP is misconfigured and the
  on-screen image is wrong. (a reference title/a reference title, the internal porting notes §1.) → The emulator's debug
  API must expose **both** raw DRAM reads (`sysmemread`) **and** a **true
  composited OP scan-out / window grab** (screenshot of what OP actually
  produces), so regression diffs catch VMODE/PWIDTH/OP bugs. This is a v1 debug-
  API requirement (see §8).

---

## 3. Blitter (TOM)

### Authoritative B_CMD (`$F02238`) bits — trust ONLY `JAGUAR.INC`/`jagregs.h`
This line "has been wrong TWICE in derived sources" — use the official equates
(`jagregs.h:180-308`), never a reference backend's old header. (the internal porting notes §3;
a reference backend; a reference title.)

- **[HW] Source/dest enables:** SRCEN=`$01`, SRCENZ=`$02`, SRCENX=`$04`,
  DSTEN=`$08`, DSTENZ=`$10`, DSTWRZ=`$20`, CLIP_A1=`$40`.
- **[HW] Address-update flags (the historically-mislabeled ones):**
  **UPDA1F=`$100` (bit 8), UPDA1=`$200` (bit 9), UPDA2=`$400` (bit 10),
  DSTA2=`$800` (bit 11).** (the internal porting notes §3 calls these out explicitly.)
- **[HW]** GOURD=`$1000`, ZBUFF=`$2000`, TOPBEN=`$4000`, TOPNEN=`$8000`,
  PATDSEL=`$10000`, ADDDSEL=`$20000`. Z-compare modes ZMODELT=`$40000`,
  ZMODEEQ=`$80000`, ZMODEGT=`$100000`.
- **[HW] LFU (logic function) field, bits 24:21** (`jagregs.h:202-232`):
  - **Plain copy / "source" = LFU `$01800000`** (LFU_REPLACE = LFU_SAND|LFU_SAD).
  - **`$00C00000` = NOT(S^D)** (LFU_N_SXORD). **Do NOT mislabel this as "LFU_B".**
  - LFU_XOR=`$01200000`, LFU_CLEAR=`$00000000`, LFU_ONE=`$01E00000`,
    LFU_D=`$01400000`, LFU_S=`$01800000`, LFU_NOTD=`$00A00000`, etc.
  - **Solid fill = `B_SRCD` loaded + LFU `$01800000`** (source replaces dest).
    Mislabeling (UPDA1 at the wrong bit, LFU_B at `$00C00000`) inverted fills
    through stale register state and **sprayed `0xFFFF` over low RAM**. (a reference backend.)
- **[HW]** CMPDST=`$2000000`, BCOMPEN=`$4000000`, DCOMPEN=`$8000000`,
  BKGWREN=`$10000000`, **BUSHI=`$20000000`**, SRCSHADE=`$40000000`.

### A1/A2 flags (`A1_FLAGS=$F02204`, `A2_FLAGS=$F02228`) — pixel addressing
- **[HW]** PITCH field bits 1:0: PITCH1=0, PITCH2=1, PITCH4=2, PITCH3=3.
- **[HW]** **PIXEL (depth) field bits 5:3:** PIXEL1=0, PIXEL2=`$08`, PIXEL4=`$10`,
  PIXEL8=`$18`, **PIXEL16=`$20`**, PIXEL32=`$28`. (`jagregs.h:239-244`.)
- **[HW]** ZOFFS bits 8:6, WIDTH field bits 14:9 (`WIDn` table `jagregs.h:255-293`),
  X/Y add-control bits 17:15 / 18 (`XADDPHR=0, XADDPIX=$10000, XADD0=$20000,
  XADDINC=$30000`; `YADD0/1`), X/Y sign bits 19/20.

### Blitter usage facts (proven on BigPEmu) — a reference backend
- **[HW/BIGPEMU] Fire-and-forget: wait for blitter idle BEFORE setup, never after
  start.** Solid spans/bands work with XPIX (pixel) addressing; setup ≈ a dozen
  register writes; short spans (<~12 px) stay on the 68000. (the internal porting notes §3; a reference backend, a reference title.)
- **[HW] PHRASE-mode bands (XADDPHR) move 4 px/cycle**; counts/steps stay in
  PIXELS and B_SRCD lane order is unchanged from pixel mode (verified by a 68k
  probe matrix: variant count=320/step=-320 scored 640/640). Every blit site sets
  A1_FLAGS after its idle wait. (a reference backend "30FPS PROGRAM".)
- **[BIGPEMU≠HW? lane order] Pixel-mode blits take `B_SRCD` lanes RELATIVE TO THE
  START PIXEL** (first pixel = the extra-payload color), **NOT absolute phrase
  address**, as measured on BigPEmu with a controlled blit matrix. **Real
  hardware may route the two-pixel pattern by ABSOLUTE address** — the dither
  emitter keys lane choice off x0 parity to be safe. (a reference backend "DITHER
  PRIMITIVE".) → **[HW≠BIGPEMU] `(UNVERIFIED on HW)`**: make B_SRCD pattern phase
  origin (start-pixel vs absolute-phrase) configurable; default = BigPEmu
  (start-pixel-relative) for the oracle. Solid rects at any x0/width parity are
  unaffected.
- **[BIGPEMU]** BigPEmu exposes raw blitter registers individually via
  `bigpemu_jag_blitter_raw_get/set(EBigPEmuBlitterRaw, val)` over the enum
  `kBPE_BlitterRaw_{A1BASE,A1FLAGS,A1CLIP,A1PIXEL,A1STEP,A1FSTEP,A1FPIXEL,A1IINC,
  A1FINC,A2BASE,A2FLAGS,A2MASK,A2PIXEL,A2STEP,B_CMD,COUNT,SRCD0,SRCD1,DSTD0,DSTD1,
  DSTZ0,DSTZ1,SRCZ10,SRCZ11,SRCZ20,SRCZ21,PATD0,PATD1,IINC,ZINC}`
  (`bigp_shared.h:488-522`). Plus `bigpemu_jag_blitter_set_excycles(n)` to throttle
  blitter execution cycles (AvP sets 0 = "free", `avp.c:153`). Our debug API
  should expose the same per-register granularity.
- **[HW]** Blitter register block `$F02200`–`$F02298`; B_COUNT `$F0223C`
  (low 16 = inner/pixels, high 16 = outer/lines), B_SRCD `$F02240`, B_DSTD
  `$F02248`, B_PATD `$F02268`, B_IINC `$F02270`, B_ZINC `$F02274`, B_STOP
  `$F02278`. (`jagregs.h:143-178`.)

---

## 4. GPU / DSP — Jaguar RISC

### Shared register map
- **[HW]** GPU control block `$F02100`+: G_FLAGS `$F02100`, G_MTXC `$F02104`,
  G_MTXA `$F02108`, G_END `$F0210C`, G_PC `$F02110`, G_CTRL `$F02114`, G_HIDATA
  `$F02118`, G_REMAIN / **G_DIVCTRL** `$F0211C`. GPU SRAM `$F03000`–`$F03FFF`.
  (`jagregs.h:111-121`.)
- **[HW]** DSP control block `$F1A100`+: D_FLAGS `$F1A100`, D_MTXC `$F1A104`,
  D_MTXA `$F1A108`, D_END `$F1A10C`, D_PC `$F1A110`, D_CTRL `$F1A114`, D_MOD /
  MOD_MASK `$F1A118`, D_REMAIN/D_DIVCTRL `$F1A11C`, D_MACHI `$F1A120`. DSP SRAM
  `$F1B000`–`$F1CFFF` (8 KB). (`jagregs.h:383-394,320`.)
- **[HW] G_CTRL / D_CTRL bits** (`jagregs.h:420-427`): **RISCGO=`$01` (bit 0,
  run)**, CPUINT=`$02`, FORCEINT0=`$04`, SINGLE_STEP=`$08`, SINGLE_GO=`$10`,
  **REGPAGE=`$4000` (selects register bank)**, **DMAEN=`$8000`**.
- **[HW] G_FLAGS condition/status bits** (`jagregs.h:429-435`): ZERO_FLAG=`$01`,
  CARRY_FLAG=`$02`, NEGA_FLAG=`$04`, **IMASK=`$08`**. Bank-control & latch bits
  G_CPUENA…G_BLITLAT (`jagregs.h:123-141`); the DSP analogues D_CPUENA… include
  D_I2SENA=`$20`, D_TIM1ENA=`$40`, D_TIM2ENA=`$80` (audio/timer int enables).
- **[HW] G_DIVCTRL / D_DIVCTRL `$F0211C`/`$F1A11C`:** **DIV_OFFSET bit 0 = 1
  selects 16.16 fractional divide mode** (`jagregs.h:435`). The GPU divider is
  used for perspective; the proven kernel sets `G_DIVCTRL=1` once per launch.
  (a reference backend "GPU span engine".)
- **[HW]** Endianness selects: **`G_END=0x00070007`** (a reference backend). `D_END` =
  `0x00050005` — NOTE the Doom-lineage code
  uses the DSP big-endian config; set both at reset before any RISC run.
  `(UNVERIFIED exact D_END value 0x00050005 vs 0x00050007 — see open Qs.)`

### GPU-from-DRAM bug (architectural HW truth)
- **[HW] GPU code must execute from GPU local SRAM (`$F03000`–`$F03FFF`), NOT
  DRAM.** There is a hardware bug in Tom's jump/branch handling when executing
  from main DRAM. (the internal porting notes §3; a reference title's notes.) → The emulator
  should reproduce the constraint: GPU branch/jump behavior in DRAM is undefined/
  broken; flag DRAM execution. `(UNVERIFIED exact failure mode — likely
  mis-fetched jump target; BigPEmu may or may not reproduce the bug.)`

### G_CTRL halt-from-GPU quirk — **HW≠BIGPEMU**
- **[HW≠BIGPEMU] A `STORE Rsrc,(G_CTRL)` writing 0 from WITHIN GPU code does NOT
  halt the GPU under BigPEmu** — execution continues past the instruction. On
  real HW this clears RISCGO and the GPU halts. (a reference title's notes.)
  → Workaround guests use: kernel ends with sentinel + finite NOP sled; the 68000
  reloads kernel + G_PC and restarts G_CTRL=1 for every batch (a reference backend).
  Make configurable: default BigPEmu-compat (GPU-side G_CTRL=0 is a no-op), with
  a "HW-accurate" option that halts.
- **[HW≠BIGPEMU] Wild GPU running past the kernel:** under BigPEmu the GPU keeps
  executing arbitrary SRAM/DRAM bytes; the guest bounds kernels and forces
  `G_CTRL=0` from the 68000 on timeout (a reference backend,
  a reference title's notes). Emulator must let the 68000 force-halt via
  G_CTRL write at any time.
- **[HW] Sentinels must live in main DRAM**, not GPU RAM — both for HW
  (cross-bus visibility) and because BigPEmu GPU-RAM reads from the 68k are
  prohibitively slow. (a reference title's notes.)

### Register banking & introspection
- **[BIGPEMU]** GPU/DSP have **two 32-register banks** selected by REGPAGE.
  BigPEmu exposes absolute index 0..63 (`bigpemu_jag_gpu_set_reg/get_reg`),
  current-bank 0..31 (`gpu_curbank_*`), and alt-bank (`gpu_altbank_*`); same for
  DSP. (`bigpcrt.h:221-241`.) → Our debug API must expose **both banks** of GPU
  and DSP registers, 0..63 absolute and 0..31 per-bank, plus PC get/set.
- **[BIGPEMU]** `bigpemu_jag_gpu_set_pipeline_enabled` / `dsp_set_pipeline_enabled`
  toggle pipeline emulation (accuracy vs. speed). `consume_cycles` lets scripts
  inject RISC cycle accounting. Free RISC breakpoints via
  `bigpemu_jag_inject_risc_bp` **stomp 8 bytes** at the address (the BP is encoded
  in-stream). (`bigpcrt.h:243-247`.)

### RMAC assembler quirks (toolchain, affect generated GPU code the emulator runs)
- **[toolchain] `rmac` forward `jr cc,label` is buggy** (off by −64 bytes per
; "no forward jr" per a reference backend). Workaround:
  `movei #label,rN` + `jump (rN)`. → Not an emulator requirement, but explains
  why proven kernels never use forward `jr`. Validate JR ±range and the `movei`
  immediate are emulated correctly.
- **[HW pipeline] 2 NOPs after every `jr`/`jump`; ≥2 instrs after a `load`; 2
  NOPs after `mult`.** `movei #label` is valid at the kernel's assembled origin
  (`$F03000`). (a reference backend "Kernel facts"; a reference title GPU design.) → The
  emulator's RISC pipeline must model **load latency** and **branch-delay slots**
  consistent with these (jump takes effect after 2 slots; load result not ready
  for ≥2 instrs; mult result not ready for 2). `(UNVERIFIED exact slot counts —
  cross-check JRISC ISA + BigPEmu.)`
- **[HW frecip]** `frecip` via an **INTEGER divide of `0xFFFFFFFF`**: a 16.16
  divide of 1.0 is off by 1 ulp whenever z divides 2^32 — the GPU kernel mirrors
  the C reference exactly for parity. (a reference backend.) Divide unit semantics
  (16.16 mode, sign handling) must be bit-exact for the oracle.

### GPU-consumed struct alignment — **HW truth**
- **[HW] Any struct whose address the GPU reads needs 4-byte alignment.** The
  m68k C ABI aligns structs to **2 bytes**; an address at 2 mod 4 makes Tom's
  long-loads **silently misread** (observed: blocks 1,3,5 corrupted, 0,2,4 fine).
  (a reference backend "GOTCHA THAT COST A DAY".) → The emulator's GPU long-load from
  a 2-mod-4 address must reproduce the misread (i.e., model the actual phrase/long
  fetch alignment), so guest bugs surface identically. `(UNVERIFIED exact misread
  pattern — capture from BigPEmu.)`

---

## 5. JERRY — Timers, Interrupts, Audio (DSP I/O), Clocks

### Timers / interrupt control
- **[HW]** Jerry programmable timers PIT-style: `JPIT1`–`JPIT4` `$F10000`–`$F10006`;
  TOM PIT0/PIT1 `$F00050/52`. `J_INT` `$F10020`. Enable bits J_TIM1ENA=`$04`,
  J_TIM2ENA=`$08`; clear bits J_TIM1CLR/J_TIM2CLR. (`jagregs.h:309-340`.)
- **[BIGPEMU]** Interrupt enable/pending are introspectable per-chip:
  `bigpemu_jag_tom_get/set_inten`, `tom_get/set_intpend`,
  `jerry_get/set_inten`, `jerry_get/set_intpend` (`bigpcrt.h:263-271`). → Debug
  API must expose TOM and Jerry **inten** and **intpend** masks (get+set).

### Audio / DSP I2S
- **[HW]** Audio output registers: `L_I2S`=`$F1A148`, `R_I2S`=`$F1A14C`,
  `SCLK`=`$F1A150`, `SMODE`=`$F1A154`, `MOD_MASK`=`$F1A118`. (`jagregs.h:320-326`.)
  DSP timer/I2S int enables D_I2SENA=`$20`, D_TIM1ENA=`$40`, D_TIM2ENA=`$80`
  (`jagregs.h:397-399`).
- **[HW]** Jerry wavetable ROM table at `$F1D000`: ROM_TRI `$F1D000`, ROM_SINE
  `$F1D200`, ROM_AMSINE `$F1D400`, ROM_12W `$F1D600`, ROM_CHIRP16 `$F1D800`,
  ROM_NTRI `$F1DA00`, ROM_DELTA `$F1DC00`, ROM_NOISE `$F1DE00`. (`jagregs.h:372-381`.)
- **[BIGPEMU/method] Jaguar-Doom-style DSP audio layout** (the capture flow's
  reference): DSP code `.org $F1B000`; samplecount @ `$F1B02C`,
  codestart @ `$F1B030`, finished @ `$F1B034`; an 8 KB audio ring buffer at
  `$001F0000`–`$001F2000` in DRAM. The verify flow reads `read32(DSP_SAMPLECOUNT)`
  to confirm audio is advancing. → Audio is validated by **sample-count progress**
  and a DRAM ring buffer, not by listening; the emulator's audio engine should
  advance these guest-visible counters deterministically.
- **[BIGPEMU]** AvP frame-rate cap is software via a `vbcount` word the script
  overwrites; "uncapped framerate = NTSC 60/(vbcount-1), PAL 50/(vbcount-1)"
  (`avp.c:70-73`) — a game-logic fact, but confirms BigPEmu's NTSC=60 / PAL=50
  field rate.

### Clocks
- **[HW]** Video pixel clock is **fully programmable**, "typically 12–15 MHz for
  TV", up to 40 MHz; vertical counter ticks every half-line. (TRM v8,
  "video timing is completely programmable in units of the pixel clock".)
- **[BIGPEMU]** Master clock exposed by `bigpemu_jag_master_clock_mhz()`; RISC and
  68k cycle⇄µs conversions by `risc_cycle_for_usec`/`m68k_cycle_for_usec` and
  inverses (`bigpcrt.h:297-301`). Quoted GPU clock **≈ 26.59 MHz** (37.6 ns/instr)
  in a reference title's notes; 68000 **≈ 13.3 MHz** in the internal porting notes §2/§4.
  `(UNVERIFIED exact master clock — read it from BigPEmu's API at runtime and
  match; NTSC vs PAL master clocks differ.)`
- **[BIGPEMU]** NTSC/PAL selectable; `bigpemu_jag_is_ntsc()` returns the field
  type. Frame period / horizontal period queryable.

---

## 6. Input / Joypad

### Hardware register & scan
- **[HW]** `JOYSTICK`=`$F14000` (write = column strobe / read = rows),
  `JOYBUTS`=`CONFIG`=`$F14002`. A **32-bit read of `$F14000` returns JOYSTICK in
  the high word, JOYBUTS in the low word.** (a reference backend.)
- **[HW] 4-strobe matrix read** (proven Jaguar-Doom lineage, a reference backend):
  Row data arrives **active-low** in JOYSTICK bits 11:8 (longword bits 27:24),
  fire buttons in JOYBUTS bits 1:0. Mask `$F0FFFFFC` passes exactly those.
  Strobe → rotate → AND-accumulate, then invert to active-high:
  ```
  strobe $81FE, ror 4 : bits 23:20 = R,L,D,U;  bit 29 = A,  bit 28 = Pause
  strobe $81FD, ror 8 : bits 19:16 = 7,4,1,*;  bit 25 = B
  strobe $81FB, rol 12: bits  7:4  = 2,5,8,0;  bit 13 = C
  strobe $81F7, rol 8 : bits  3:0  = 3,6,9,#;  bit  9 = Option
  ```
  Decoded matrix bit positions (`jagregs.h:342-370`, active-high): U=20, D=21,
  L=22, R=23, A=29, B=25, C=13, Option=9, Pause=28; keypad `*`=16,7=17,4=18,1=19,
  0=4,8=5,5=6,2=7,#=0,9=1,6=2,3=3.
- **[HW] WARNING — do NOT use a reference backend's `isr.s` mask `$FFFFFF0F`:** it reads
  the **wrong half** of the longword (bug #3, which is why that project needed an
  input shim). Use the `$F0FFFFFC` mask + rotations above. (a reference backend,
  "hardware init provenance".)
- **[HW]** ANY_JOY=`$00F00000`, ANY_FIRE=`$32002200`, ANY_KEY=`$000F00FF`
  composite masks (`jagregs.h:368-370`).

### BigPEmu input quirks — **HW≠BIGPEMU / method**
- **[BIGPEMU] `bigpemu_jag_get_buttons(0)` returns `0xFFFFFFFF` (all pressed) when
  no real controller is attached (headless).** Do NOT use it for input.
  (the internal porting notes §1.) The emulator's equivalent should return a **defined no-input
  value** (recommend 0 = nothing pressed, but expose the BigPEmu behavior as an
  option if a flow depends on it).
- **[BIGPEMU] Joypad cannot be verified headless via the real pad path.** BigPEmu
  only applies keyboard→pad bindings for an **activated window**, which Wine never
  grants under bare Xvfb → `pad_raw` stays 0 even though `xdotool` keys reach the
  emulator (probe via `vkup_frames`). (a reference backend.) → Our emulator MUST
  provide **direct programmatic input injection** that works headless (write the
  JOYSTICK/JOYBUTS matrix directly, or a button word), independent of any
  windowing/binding layer. This is the single biggest input-API gap to close.
- **[BIGPEMU/method] "JOYSHIM" pattern** (reliable bring-up input): a BigPEmu
  input-frame script reads the host keyboard and **writes a button word to a
  fixed DRAM address** (e.g. `$001000`), big-endian MSB-first; the game polls that
  address instead of the JOYSTICK register; game clears it at boot. A heartbeat
  byte `0xA5` is written to `JOYSHIM+4` to prove liveness. (the internal porting notes §1;
  a reference backend's input driver `:9,61,77-81`.) → Our debug API should support **both**: native
  JOYSTICK-matrix injection AND arbitrary DRAM writes per input-frame, so existing
  shim flows port verbatim.
- **[BIGPEMU] Script input API** (must be matched/exceeded):
  `bigpemu_input_get_input_size()` (entry size, **can exceed 128 → size buffers
  ≥256 and guard**), `get_all_held_inputs(buf,maxHeld)`,
  `create_input_from_vk(buf, VK)` (**Windows VK codes**, valid on all platforms),
  `input_in_set(set, count, key)`, `get_input_data_version()`, `get_device_count()`,
  `get_input_name()`. (`bigpcrt.h:717-728`, `turbo_joy.c`, a reference backend's input driver,
  a reference title's verify harness `:33-37`.)
- **[BIGPEMU] Per-frame input override** for full controllers: the
  `register_event_input_frame` callback gets `TBigPEmuInputFrameParams { mInputCount
  (mutable), mMaxInputCount, TBigPEmuInput *mpInputs }`; each `TBigPEmuInput =
  { mType, mButtons (32-bit emulated button bits), mExButtons, mAnalogs[4],
  mResv[4] }`. Scripts mutate `mButtons` directly to drive the emulated pad
  (`turbo_joy.c:329-376`). → Our injection API should expose the **same
  per-device button-bit + analog struct** so turbo/macro flows port.
- **[BIGPEMU] Device types** (`bigp_shared.h:595-604`): Standard=0, Rotary,
  Analog, Driving, AnalogADC, HeadTracker. `bigpemu_jag_get_device_type(i)`,
  `get_buttons(i)`, `get_exbuttons(i)`, `get_analogs(float[8], i)`. AvP reads
  analogs for mouse-look (`avp.c:91-109`).

---

## 7. Toolchain / COF / ROM loading (affects what the emulator must accept)

- **[BIGPEMU] BigPEmu refuses COF sections below vaddr `$2000`** and **ignores
  the COF entry field** — execution always starts at **text start**.
  (the internal porting notes §1.) → Our loader must (a) accept the same COF format, (b) for the
  oracle, **start at text start** regardless of the entry field (configurable:
  honor entry vs. force text-start). Put a jump page at text start if the real
  entry differs. Proven load/entry address = **`$4000`** across projects.
- **[HW/loader] COF a.out magic must be 32-bit `0x00000107` exactly** (as `rln`
  writes it). BigPEmu **rejects the 16-bit magic+vstamp variant.** Image at file
  offset **`0xA8`**, load/entry `$4000`. (a reference title; a reference title.) → Loader must parse
  this COF header layout.
- **[loader] `rln` pads inter-object gaps with zeros.** An odd number of zero-longs
  executes as `ORI.B #0,D0` chains that desync the instruction stream into the
  next object's first opcode. (the internal porting notes §1.) Not an emulator requirement, but
  explains "fall-through" crashes; the 68000 core must decode `$0000 0000` as the
  guest 68000 does (`ORI.B #0,D0`, 2 longs → harmless NOP-ish but advances PC).
- **[loader] `rln` COF symbols are BSD a.out nlist records (12-byte), emitted only
  with `-s`/`-l`;** without them `symptr` points at EOF; `rln -m` prints a load
  map. (the internal porting notes §1.) → If the debug API resolves symbols, parse 12-byte BSD
  nlist.
- **[HW] No 64-bit int math** in guest cores: `__muldi3`/`__divdi3` cost thousands
  of cycles (made first frame >4 s). System libgcc is 68020-built (`bsr.l` poison,
  see §1). (a reference backend, a reference title, a reference title.) Emulator-irrelevant except that
  such code, if present, executes slowly — cycle counts must reflect it.
- **[BIGPEMU/launcher] Headless:** BigPEmu runs under Wine; `BIGPEMU_HEADLESS=1`
  renders off-screen, else it lands unfocused and **pauses** (looks like a hang).
  Launch is **flock-serialized** (one instance across all `jag_*` projects).
  (a reference backend; the internal porting notes §5.) → Our emulator should support **true
  headless** (no window, no pause-on-unfocus) and **many concurrent instances**
  (explicit goal of this project; removes the flock bottleneck).
- **[BIGPEMU/scripts] The script CVM compiler does NOT support `goto`** (crashes
  at compile); delete a stale `.bigpcvm` to force recompile. (the internal porting notes §1.)
  Irrelevant to our native API but explains the C-script style.
- **[BIGPEMU] FNV-1a64 ROM hashing:** scripts gate on `bigpemu_get_loaded_fnv1a64()`
  to detect a specific game (`avp.c:228`). → Our loader should expose a stable
  content hash of the loaded ROM/COF.

---

## 8. Verification-method facts (oracle methodology)

- **[method] Verify video with EYES / true scan-out, not thresholds.**
  Mean-brightness / motion / nonzero-byte checks **pass on garbage** (noise sailed
  through seven milestones; squished-frame VMODE bug passed every headless DRAM
  dump). (a reference title, a reference title, the internal porting notes §1.) → The oracle harness must compare the
  **composited OP output** (true screenshot), and bring-up must render a **test
  card** (solid bands `$F800`/`$07C0`/`$003F` + 1-px checker) read off a
  screenshot to nail pixel format / byte order / pixel pairing / address path in
  one run.
- **[method] Crash-scratch convention** the verify scripts rely on: an unhandled
  68k exception handler (in guest/) writes magic
  **`$EEEE0000` at `$820`**, SSP at `$824`, then 8 frame longs at `$828+` (faulting
  PC inside). Scripts dump `$800`–`$8FF` and flag `crash==0xEEEE0000`.
  (a reference title/a reference title scripts; a reference backend.) → Our
  emulator should make **68000 exception capture first-class** (faulting vector,
  PC, SSP, the exception stack frame) so this convention is reproducible — and,
  better, expose it directly via the debug API rather than via a guest handler.
- **[method] Game-published debug block at `$3F00`** (DRAM), refreshed every
  vblank by the guest ISR, magic + live state so scripts need no hardcoded layout:
  e.g. `{magic, frame/vbi_count, front_fb_ptr, ...}` — magics seen: `0x41334456`,
  `0x4D4F4E56`, `0x4D4B3256` (per-project 4-char ASCII tags). Extended fields
  carry profiling (frames_rendered<<16|vsyncs, pad, positions, gpu timeouts/spans).
  (a reference backend; other reference titles' verify harnesses.) Our harness can keep
  this guest-cooperative convention; our debug API should also read it natively.

---

## 9. Emulator debug-API requirements (must match or exceed BigPEmu scripts)

The existing capture/verify flows are BigPEmu C scripts compiled to `.bigpcvm`.
To port them, the native Rust debug API must provide **at least** the following
primitives. **Threading note from BigPEmu** (`bigpcrt.h:180-185`): all `jag_*`
state access happens on the **emulator thread** (frame events / breakpoint
callbacks); our API can be simpler if single-threaded-deterministic, but must
preserve the *semantics*.

### 9.1 Memory access (primary)
- `read8/16/32/64(addr) -> uN` and `write8/16/32/64(addr, val)` following the
  **68k bus path**, big-endian. (BigPEmu: `bigpemu_jag_read8..64`,
  `write8..64`, `bigpcrt.h:189-196`.)
- Bulk: `sysmemread(dst, addr, len)`, `sysmemwrite(src, addr, len)`,
  `sysmemcmp(src, addr, len)`, `sysmemset(addr, val, len)` — RAM/ROM ranges.
  (`bigpcrt.h:201-204`.) Note BigPEmu VM endianness is little while Jaguar is big;
  our API is native Rust so just document endianness clearly.
- **Both** raw-DRAM read **and** a separate **OP-composited scan-out read /
  framebuffer grab** (see §2 caveat) — the key thing BigPEmu's `sysmemread`
  *alone* cannot do. This is a required addition, not just parity.

### 9.2 CPU/RISC state
- 68000: `m68k_get_pc/set_pc`, `get_dreg/set_dreg(0..7)`, `get_areg/set_areg(0..7)`,
  `m68k_consume_cycles(n)`. (`bigpcrt.h:210-216`.) **Exceed**: also expose SR/CCR,
  USP/SSP, and the last-exception record (vector/PC/frame).
- GPU & DSP: `gpu_get_pc/set_pc`, `gpu_get_reg/set_reg(0..63 absolute)`,
  `gpu_curbank/altbank_get_reg/set_reg(0..31)`, `gpu_consume_cycles`,
  `gpu_set_pipeline_enabled`; identical DSP set. (`bigpcrt.h:219-241`.) **Exceed**:
  expose G_FLAGS/D_FLAGS, G_CTRL/D_CTRL, divider state, and bank-select bit.

### 9.3 Breakpoints
- 68000 PC breakpoints with callbacks: `m68k_bp_add(addr, cb)`, `m68k_bp_del(addr)`;
  **auto-cleared on software load** (set them in the sw-loaded event).
  (`bigpcrt.h:206-209`.) A callback can mutate state then `m68k_set_pc(addr+off)`
  to skip/patch logic (AvP stereo, `avp.c:161,201`).
- RISC breakpoints: `inject_risc_bp(addr, cb)` — note BigPEmu's stomps 8 bytes
  in-stream; **our implementation should offer non-invasive RISC breakpoints**
  (improvement) while still allowing the inject style for ported scripts.
- **Exceed**: data/watchpoints (read/write to an address range) — BigPEmu only
  offers this via the RW-handler hook (below).

### 9.4 Memory-mapped RW handlers (device hooks)
- `set_rw_handler(cb, startAddr, endAddr, rwMask)` with `rwMask` ∈
  `{READ=1<<0, WRITE=1<<1}` (`bigp_shared.h:933-934`), aligned to
  `get_rw_handler_alignment()`; **RAM/ROM not supported**, register/IO ranges only;
  reset on software load. (`bigpcrt.h:312-317`.) → Provide region-scoped
  read/write intercept callbacks; ideally also for RAM (improvement).

### 9.5 Frame / timing / region introspection
- `get_frame_count() -> u64` (video frames), `get_line_count()`, `is_ntsc()`,
  `get_horizontal_period()`, `get_frame_period()`, `master_clock_mhz()`,
  `get_exec_time()`, `get_display_region(rect[4])`,
  `risc_cycle_for_usec`/`usec_for_risc_cycle` and the m68k pair,
  `get_vmode_divisor()`. (`bigpcrt.h:280-305`.) Plus an **emu-thread frame event**
  to fire capture at a deterministic frame (BigPEmu:
  `register_event_emu_thread_frame`; also video/input/audio frame events,
  `sw_loaded`/`sw_unloaded`, save/load-state events, `bigpcrt.h` event section).

### 9.6 Interrupt & blitter introspection
- `tom_get/set_inten`, `tom_get/set_intpend`, `jerry_get/set_inten`,
  `jerry_get/set_intpend`. (`bigpcrt.h:263-271`.)
- `blitter_raw_get/set(field)` over the full register enum (§3),
  `blitter_set_excycles(n)`. (`bigpcrt.h:273-275`.)

### 9.7 Input injection (must EXCEED BigPEmu — close the headless gap)
- Per-input-frame override of the emulated pad: a `TBigPEmuInput`-equivalent per
  device (`mButtons` 32-bit, `mExButtons`, `mAnalogs[4]`, `mType`), settable each
  frame; plus `mInputCount` mutability. (`turbo_joy.c`; `bigp_shared.h:715-730`.)
- **Headless-safe direct injection** that bypasses any window/binding layer
  (the BigPEmu gap): write the JOYSTICK/JOYBUTS matrix or a JOYSHIM DRAM button
  word programmatically. Support the VK-code helper equivalents if porting
  keyboard-reading scripts (`create_input_from_vk`, `input_in_set`,
  `get_all_held_inputs`, `get_input_size`).
- Device-type / analog read-back: `get_device_type`, `get_buttons`,
  `get_exbuttons`, `get_analogs`. Return a **defined no-input value (0)** when
  nothing is attached (do NOT mimic BigPEmu's `0xFFFFFFFF` by default).

### 9.8 File output / host I/O
- Write captured buffers to host files: BigPEmu uses `fs_open_user(name, write)`,
  `fs_write(buf, n, fh)`, `fs_close(fh)` (paths under ScriptData), plus
  `printf_notify(...)`/`printf(...)` logging. (Every verify script.) → Provide a
  host-side file/stdout sink the deterministic runner can write capture artifacts
  to (binary framebuffers, result text, scratch dumps).
- **Exceed**: a structured (JSON) capture record per frame and a built-in
  "dump regions + framebuffer + crash record at frame N" mode, so the common
  reference capture/verify script patterns become a single native command.

### 9.9 Execution control & determinism
- `set_paused(bool)`, single-step (68000 and RISC via SINGLE_STEP/SINGLE_GO),
  run-to-frame-N, run-N-frames. Clock scaling
  (`set_m68k_clock_scale`, `set_risc_clock_scale`, `set_lockcycles`,
  `bigpcrt.h:259-261`) for the rare timing-sensitive flow. Deterministic seeding
  of any nondeterminism (power-on RAM, RNG) so frame-N captures are byte-stable.

---

## 10. Quantitative oracle baselines (for regression diffs, not correctness gates)

These are BigPEmu-measured guest performance numbers — useful only to detect gross
regressions in *our* emulator's timing model, NOT as accuracy targets (BigPEmu ≠ HW
for performance, the internal porting notes §1/§7).
- **[BIGPEMU]** a reference backend's castle scene: pure-68k C renderer ≈ **109 vsyncs/frame**;
  fully optimized (GPU geometry+raster, triple-buffer, paced) **locked to cadence 3
  = 20 fps**. (a reference backend "Performance reality".) Calibration: **~2000 68k
  instructions ≈ 0.1 vsync** under BigPEmu's tax; a frame budget ≈ 40–60k instrs.
- **[BIGPEMU]** a reference homebrew title's 68k software span-fill baseline: `g_frame_tics=22`
  (~629 ms render), full BSP drawn, `fb_nonzero > 38400/153600`.
  (a reference title's a reference title's notes.)
- **[BIGPEMU]** Software 3D of a real scene on the 68000 ≈ **1–2 fps**; bottleneck
  is per-vertex transform+projection (DIVS-heavy), not pixel fill (the internal porting notes §4).

---

## Open questions (validate against BigPEmu, and HW where noted)

1. **VI interrupt level** — is it exactly IPL 1, 2, or 3 under BigPEmu? (the internal porting notes
   says ≤3, vectored via `$100`/USER0.) Pin it; affects SR-mask behavior.
2. **Bus-error mode default** — confirm exact returns ($FFFF high-mem read, $0000
   cart read, silent writes) at byte/word/long widths and across all unmapped
   regions; decide HW vs BigPEmu-compat default. (the internal porting notes §1.)
3. **`bsr.l`/odd-address fault** — does BigPEmu raise the 68000 address error on
   the bad *decode* (`0x61FF`→`bsr.s -1`) or only on the subsequent odd-address
   fetch? Does it raise it at all, or just execute oddly?
4. **CONFIG NTSC bit** — reconcile `(CONFIG&0x10)` read at `$F00036` (proven path)
   vs. `jagregs.h` aliasing CONFIG=`$F14002`. Which address carries the NTSC bit
   in BigPEmu?
5. **Power-on DRAM contents** — does BigPEmu zero-fill DRAM on software load (the
   capture flows assume reproducibility)? Confirm the fill value.
6. **`D_END` value** — `0x00050005` vs `0x00050007`; which does BigPEmu require for
   DSP big-endian operation? (the reference config says `0x00050005`.)
7. **OLP half-swap** — is writing OLP with its 16-bit halves swapped a HW
   requirement or a Jaguar-Doom idiom that the proven path inherited? (a reference backend.)
8. **OP high-address pointer bug** (bit 16 of byte address ≥`$100000` fails under
   BigPEmu) — is this a BigPEmu OP-parse bug (don't replicate) or HW? Make
   configurable; default = correct (HW) parsing. (the internal porting notes §1.)
9. **B_SRCD pixel-mode lane phase** — start-pixel-relative (BigPEmu) vs absolute-
   phrase (suspected HW). Confirm on real hardware; expose a config toggle.
   (a reference backend.)
10. **GPU-from-DRAM bug** — does BigPEmu reproduce the Tom DRAM-execution
    jump/branch bug, or does it run DRAM GPU code fine? (a reference title's notes
    treats it as architectural.)
11. **GPU G_CTRL=0-from-GPU halt** — confirm it's a no-op under BigPEmu (HW halts);
    expose the toggle. (a reference title's notes.)
12. **RISC pipeline slot counts** — exact load latency (≥2 instrs?), branch-delay
    (2 slots?), mult latency (2 slots?), and divider timing in 16.16 mode.
    Cross-check the JRISC ISA and BigPEmu. (a reference backend "Kernel facts".)
13. **GPU long-load from 2-mod-4 address** — capture BigPEmu's exact misread
    pattern (which blocks corrupt) so guest alignment bugs surface identically.
    (a reference backend "GOTCHA THAT COST A DAY".)
14. **Master clock(s)** — exact NTSC and PAL master/GPU/68k clock values
    (26.59 MHz GPU and 13.3 MHz 68k are approximate/single-source); read from
    BigPEmu's `master_clock_mhz` and lock NTSC/PAL separately.
15. **`bigpemu_jag_get_buttons` no-controller value** — confirm `0xFFFFFFFF` and
    decide our default (recommend 0); document either way.
16. **COF entry handling** — confirm BigPEmu always starts at text start and
    rejects sections below `$2000`; decide whether our loader honors the entry
    field behind a flag. (the internal porting notes §1.)
