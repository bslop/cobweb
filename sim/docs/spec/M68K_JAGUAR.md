# M68000 in the Atari Jaguar — Implementation Spec

**Subsystem:** Motorola 68000 (MC68000) as integrated in the Atari Jaguar.
**Audience:** Rust emulator author implementing a cycle-accurate core.
**Scope:** Jaguar-*specific* behavior + a prioritized 68k implementation
checklist. The generic 68000 ISA encoding is referenced to external docs;
this document concentrates on what the Jaguar wiring, memory map, boot, and
toolchain impose.

**Endianness:** The Jaguar is **big-endian** throughout. The 68000 is natively
big-endian, so 68k accesses need no swabbing. A 32-bit long at address `A`
stores its MSB at byte `A` and its LSB at `A+3`. A 16-bit word stores MSB at
`A`, LSB at `A+1`. Tom/Jerry's RISC and pixel engines have programmable
endianness (set big at boot — see §6); the 68k path is always big-endian.

**Legend:** *VERIFIED* = backed by official docs or proven reference code,
cited inline. *(UNVERIFIED)* = inference or cross-emulator behavior to
validate against BigPEmu; collected in §10.

---

## 1. CPU identity, clock, and what variant to emulate

| Fact | Value | Source |
|---|---|---|
| CPU | Motorola MC68000 (16/32 ISA, 16-bit data bus, 24-bit address bus) | Tech Ref v8 p.8 "Microprocessor Interface" |
| 68000 clock (NTSC) | **13.295 MHz** | Wikipedia *Atari Jaguar*; cross-checked AtariAge |
| Tom/Jerry RISC clock (NTSC) | **26.591 MHz** each (2× the 68k) | Wikipedia *Atari Jaguar* |
| Address bus | 24 lines → 16 MB physical space; A0 not emitted (word machine, uses UDS/LDS) | MC68000 datasheet |
| Data bus | 16 bits. Longword access = two bus cycles, **high word first** (big-endian) | MC68000 datasheet |

**Emulate a plain 68000, NOT a 68010/68020.** This matters:
- No 32-bit hardware multiply/divide. `MULU.W/MULS.W` are 16×16→32; `DIVU.W/
  DIVS.W` are 32÷16→16:16. (See §7, §8.) m68k-aout-gcc emits **software**
  `__mulsi3/__divsi3` for 32-bit math.
- `bsr.l`/`bra.l` (32-bit displacement, opcode `0x61FF`/`0x60FF`) **do not
  exist on the 68000** — they decode as `bsr.s -1`/`bra.s -1` (8-bit
  displacement = `0xFF`) and jump to an odd address → address error. This is a
  real, observed crash; the SDK's libgcc was built for 68020 and must be
  replaced with 68000-safe routines. Source: a reference backend's.
- No `MOVE from CCR` privilege quirks beyond the standard 68000 supervisor model.
- The exception stack frame is the **short (6-byte / group-2) 68000 frame**
  for most exceptions: pushes SR (word) then PC (long). Bus/address errors push
  the longer 68000 group-0 frame (see §4.4). Do **not** emulate 68010+ format
  words.

---

## 2. Memory map from the 68000's view (ROMHI = 1)

The Jaguar boots with the boot-ROM window mirrored across the whole 16 MB
space until `MEMCON1` ($F00000) is written. Real hardware then selects one of
two maps by the **ROMHI** bit. A "68000 system naturally operates with RAM at
0," so **ROMHI = 1 is assumed throughout** (Tech Ref v8 p.9 "A 68000 system
will naturally operate with RAM at 0, so the ROMHI map is assumed"). Use this
map; you do not need the ROMHI=0 (`$1xxxxx` register) variant for any 68k
software.

### 2.1 Top-level map (ROMHI = 1) — VERIFIED, Tech Ref v8 pp.9–10

| Range | Size | Contents |
|---|---|---|
| `$000000`–`$1FFFFF` | 2 MB | **DRAM** (DRAM0). Main system RAM. Code+data+stack live here. |
| `$200000`–`$3FFFFF` | 2 MB | DRAM mirror / DRAM0 upper. On a 2 MB console this is *not populated* — see §2.3. |
| `$400000`–`$7FFFFF` | 4 MB | DRAM1 region (unpopulated on retail 2 MB console). |
| `$800000`–`$DFFFFF` | 6 MB | **ROM1 = Cartridge ROM** (cart space). |
| `$E00000`–`$EFFFFF` | 1 MB | **Bootstrap ROM** (the boot ROM; reset vector lives here — see §3). |
| `$F00000`–`$F0FFFF` | 64 KB | **Tom** internal registers + CLUT + line buffers + GPU + Blitter. |
| `$F10000`–`$F1FFFF` | 64 KB | **Jerry** (timers, interrupt ctrl, joypad/GPIO, DSP, sample ROM). |
| `$F20000`–`$FFFFFF` | — | Remainder of ROM0 decode / unused register space. |

Authoritative chip-internal base: `BASE EQU $F00000` (Tom), Jerry at
`$F10000` (`BASE + $10000`). Source: `JAGUAR.INC:51,428`.

### 2.2 Key fixed addresses (for the loader, boot, and register stubs)

| Symbol | Address | Meaning | Source |
|---|---|---|---|
| `DRAM` | `$000000` | First RAM location | `JAGUAR.INC:20` |
| `USERRAM` | `$004000` | First non-reserved RAM (`$0`–`$3FFF` is vectors + reserved) | `JAGUAR.INC:21` |
| `ENDRAM` | `$200000` | End of 2 MB DRAM | `JAGUAR.INC:22` |
| `INITSTACK` | `$1FFFFC` (`ENDRAM-4`) | Conventional initial SSP | `JAGUAR.INC:23`; SDK `STARTUP.S:63` (`move.l #INITSTACK,a7`) |
| Cart base | `$800000` | Cartridge mapped here | Tech Ref v8 p.9; VJ `file.c:138` |
| Cart exec ptr | `$800404` | Boot ROM reads 68k entry from here (cart header offset `$404`) | VJ `file.c:140` (`jaguarRunAddress = GET32(jagMemSpace, 0x800404)`) |
| Boot ROM | `$E00000` | Bootstrap ROM; first 8 bytes = 68k reset vector | Tech Ref v8 p.9; VJ `jaguar.c:802` |
| Tom regs | `$F00000`+ | See Tom spec; **most are 16-bit** (see §2.5) | `JAGUAR.INC:51` |
| `INT1` | `$F000E0` | CPU interrupt control/ack (16-bit) | `JAGUAR.INC:81` |
| `INT2` | `$F000E2` | CPU interrupt resume (write any value at ISR end) | `JAGUAR.INC:82` |
| Jerry regs | `$F10000`+ | Timers `$F10000-06`, J_INT `$F10020`, joypad `$F14000/02` | `JAGUAR.INC:428–437` |

### 2.3 NO BUS ERRORS — the single most important Jaguar 68k bus rule

**The Jaguar 68000 never takes a bus error.** There is no `/BERR` path that
fires on unmapped access — the memory controller makes the whole space "look
64 bits wide" and decoded (Tech Ref v8 p.7 memory controller). Reads of
nothing return a constant; writes to nothing vanish. This is **load-bearing
emulator behavior**: code that relies on a bus-error handler to detect missing
hardware *never runs at all*, and garbage reads silently steer control flow.

**Exact documented behavior (probe-verified on BigPEmu, a reference homebrew title 2026-06;
the internal porting notes :68-76):**

| Access | Result |
|---|---|
| Unmapped **high-memory** read via sign-extended `(xxx).w` → effective addr `$FFFFxxxx` | returns **`$FFFF`** (word); byte read returns `$FF` |
| Unmapped **cart-space** read (`$A1xxxx`, `$C0xxxx`, i.e. mapped-but-empty cart) | returns **`$0000`** |
| Write to **either** unmapped region | **vanishes silently**, no fault |
| Any access | **never** raises bus error, **never** hangs, **never** faults |

Cross-check (Virtual Jaguar's default unmapped stubs): word read → `0xFFFF`,
byte → `0xFF` (`vj_jaguar.c:73,78`). **However**, BigPEmu's documented split
(`$FFFF` for high mirror vs `$0000` for empty cart) is the *authoritative target*
— implement the two-region rule above, not a single constant.

**Implementation:** the bus read function returns a region-dependent fill for
any address that does not decode to RAM / cart-with-data / a known register;
the bus write function is a no-op for the same. Never signal `/BERR`. *(The
exact boundary of "high-memory $FFFFxxxx returns $FFFF" vs "cart space returns
$0000" — confirm the cutoff against BigPEmu; see §10.)*

### 2.4 Address wrapping

The 68000 only drives **A1–A23** (24 address lines). All effective addresses
are masked to **24 bits** (`addr & 0x00FFFFFF`) before bus access — confirmed
by VJ masking every access with `offset & 0xFFFFFF` (`vj_jaguar.c:517,540,
566,596,630,639`). So `$FF000000`-style 32-bit pointers wrap into the `$Fxxxxx`
register space. A `(xxx).w` operand is sign-extended to 32 bits first
(`$8000`→`$FFFF8000`) **then** masked to 24 bits → `$FF8000` (Jerry/register
space). This is why "unmapped `(xxx).w` reads" land at `$FFFFxxxx`≡`$FFxxxx`.

### 2.5 Register access width — CRITICAL

Tom's internal registers are **mostly 16 bits wide** (Tech Ref v8 p.10: "Internal
Memory is mostly 16 bits wide"). **Never access video registers as 32-bit.**
`VMODE` (`$F00028`), `BORD1` (`$F0002A`), `HDB/HDE`, `VDB/VDE`, `BG`, `VI`,
`INT1/INT2`, `OBF` are all **words**. Only `OLP` (`$F00020`) and the GPU/Blitter
register file (`$F02xxx`) are **longs**. A 32-bit store to a 16-bit register
spills the low half into the *next* register (big-endian): e.g. a 32-bit
`VMODE=$000006C7` write lands `VMODE=$0000` + `$06C7` into `BORD1`, zeroing
PWIDTH. Source: the internal porting notes; widths in
`JAGUAR.INC`. The bus model must therefore route by address to a 16-bit or
32-bit register handler, not assume a uniform width.

---

## 3. Reset and boot

### 3.1 68000 reset sequence (hardware)

On `/RESET`, the 68000:
1. Enters **supervisor mode**, sets `SR = $2700` (S=1, interrupt mask = 7,
   T=0). VERIFIED MC68000 datasheet.
2. Reads the **initial Supervisor Stack Pointer (SSP/A7)** from the long at
   physical `$000000`–`$000003` (MSB-first).
3. Reads the **initial Program Counter (PC)** from the long at
   `$000004`–`$000007`.
4. Begins fetching instructions at PC. Big-endian 16-bit prefetch.

Because the boot ROM is **mirrored across the entire address space at reset
until `MEMCON1` is written** (Tech Ref v8 pp.9,11: "ROM0 repeats every 2 Mbytes
until this register is written to"), the longs at `$000000/$000004` are read
from the boot ROM image — i.e. the **boot ROM's first 8 bytes are the 68k reset
vector**. VERIFIED by VJ copying `jagMemSpace + 0xE00000` (boot ROM) into
`jaguarMainRAM[0..7]` so the reset vector resolves (`vj_jaguar.c:802`:
`memcpy(jaguarMainRAM, jagMemSpace + 0xE00000, 8)`).

### 3.2 Boot ROM responsibilities (high level)

The real boot ROM at `$E00000`:
- Sets up `MEMCON1/MEMCON2` to configure DRAM and lock in the ROMHI map.
- Validates the cartridge (the Atari "encryption"/checksum boot check).
- Reads the **cartridge execution address from cart header offset `$404`**
  (physical `$800404`, big-endian long) and **jumps to it** to start the game.
  VERIFIED: VJ sets `jaguarRunAddress = GET32(jagMemSpace, 0x800404)` for cart
  images (`file.c:140`). The cart header lives at `$800000`; offset `$400`
  region holds the boot/execution descriptor, `$404` = entry long.

### 3.3 Two boot models for the emulator

You must support both, because most homebrew/test loads are RAM executables,
not encrypted carts:

**(A) Cartridge / real-BIOS path (`useJaguarBIOS && cartInserted`):**
- Place the boot ROM image at `$E00000`.
- Copy boot ROM `[0..7]` → RAM `[0..7]` so the 68k reset vector resolves
  (or, if your boot ROM is mapped/mirrored, simply let the reset fetch read
  `$0/$4` through the boot-ROM mirror).
- Pulse reset → 68k loads SSP and PC, runs the boot ROM, which configures
  memory, validates the cart, and jumps to `[$800404]`.

**(B) HLE / no-BIOS path (RAM-loaded `.cof/.abs/JagServer`, or HLE cart):**
- Do not run a boot ROM. Instead, directly seed the reset vector:
  - `RAM[0..3]` (SSP) = top-of-RAM for RAM loads (`HLE_SSP_RAMLOAD`), or the
    historical `$4000` for HLE cart (`HLE_SSP_CART`). VERIFIED `vj_jaguar.c:
    809-812`.
  - `RAM[4..7]` (initial PC) = `jaguarRunAddress` (the loader's entry — see §5).
- Populate the exception vector table with safe stubs (RTE for vectors 4–255;
  long-frame handler for 2–3). **Vector 64 (`$100`) is the Jaguar interrupt
  vector — keep it pointed at a real handler, not the RTE stub.** VERIFIED
  `vj_jaguar.c:833-847`.
- Replicate post-BIOS hardware state the game expects (e.g. `MEMCON1`,
  endianness regs). *(Exact post-BIOS register snapshot is HLE-specific;
  validate against BigPEmu — §10.)*

### 3.4 What homebrew startup code actually does (reference)

A typical RAM executable's `_start` (a reference backend's), in order:
1. `move.w #$2700,%sr` — interrupts off during setup (mask 7).
2. `move.l #$00070007,$F0210C` — `G_END`: set **GPU big-endian** (both halves).
3. `move.l #$00050007` style → `$F1A10C` `D_END`: set **DSP big-endian**.
   (Reference uses `$00050005`.)
4. `lea $200000,%sp` — park SSP at top of 2 MB DRAM (== `ENDRAM`).
5. `move.w #$FFFF,$F0004E` (`VI`) — suppress vertical interrupts initially.
6. Install a catch-all handler into vectors 2..255 (a `dbra` loop writing the
   handler addr to `$8`, `$C`, … — note vectors start at `$8` because
   `$0/$4` are SSP/PC).
7. Zero `.bss` (and, per porting notes, **zero the stack region up to ENDRAM**
   — uninitialized C locals expecting 0 break only on complex code paths;
   the internal porting notes).
8. `jsr main`.

The emulator does not execute this *for* the program — but the bus/endianness
side effects (the `$F0210C/$F1A10C` writes, `VI=$FFFF`, big SSP) are how to
recognize a "known raw homebrew startup" (VJ uses exactly the
`23FC 0007 0007 ... 00F0210C` signature to infer a raw binary load address —
`file.c:39-41`).

---

## 4. Interrupts

### 4.1 The hardware truth: ALL Jaguar interrupts reach the 68k at IPL **level 2**

This corrects the porting note's empirical "≤ 3". Tracing the Jaguar schematic
IPL lines (VERIFIED, VJ `jaguar.c:301-306`):
- **IPL1** is connected to TOM's `INTL` output (the only active interrupt line
  into the 68k).
- **IPL0 and IPL2 are tied to Vcc** (4.7 kΩ pull-ups) → always inactive.

`IPL2 IPL1 IPL0` are **active-low** on the 68000. With IPL2=1 (inactive),
IPL0=1 (inactive), IPL1 driven by TOM: when TOM asserts INTL, the 68k sees
`IPL = %010` inverted → **interrupt priority level 2**. So **every** 68k
interrupt source (Video/VI, GPU, Object/stop, PIT timer, Jerry) is funneled
through TOM and presented as a **single level-2 autovectored interrupt**.

**Consequences:**
- All Jaguar 68k interrupts are **maskable** (level 2 < 7). There is no NMI.
- A level-2 interrupt is taken only when the SR interrupt mask is **< 2**, i.e.
  mask ∈ {0,1}. `move #$2300,sr` sets mask = **3** → **masks the VI**. Any
  Genesis/arcade-style mask of `$2300/$2500/$2700` silently starves the VI.
  **Run the 68k at IPL 0** (`SR` mask 0) if you need every frame. VERIFIED
  effect: the internal porting notes. (Note BigPEmu "boots the 68k at IPL
  0".)
- The reset state SR=$2700 (mask 7) blocks interrupts until the program lowers
  the mask.

### 4.2 Interrupt acknowledge + vector

When the 68k acknowledges a level-2 interrupt:
- The Jaguar supplies an **autovector** path → the 68k uses **autovector #2**…
  **but** the documented/observed effective vector for Jaguar software is
  **vector 64 = address `$100`** (`LEVEL0`/`USER0`). VERIFIED:
  - `JAGUAR.INC:29-30`: `LEVEL0 EQU $100`, `USER0 EQU $100`,
    "68000 Level 0 Autovector Interrupt".
  - VJ ack handler returns vector **64** for level 2 (`jaguar.c:311-314`:
    `if (level == 2) { m68k_set_irq(0); return 64; }`).

> **Reconciling "autovector" vs "vector 64":** Vector 64 ($100) is the first
> **user-defined** interrupt vector on the 68000 (vectors 0–63 = $000–$0FF are
> the architectural exceptions; 64–255 = $100–$3FF are user vectors). The
> Jaguar's interrupt logic supplies vector number 64 during the IACK cycle
> (it does *not* use the level-2 autovector at $68). Treat it as: **level-2
> interrupt → vector number 64 → handler address fetched from long at `$100`.**
> The SDK name "LEVEL0/USER0 = $100" reflects this being the system's first
> usable interrupt slot. *(Whether real hardware drives vector 64 on the data
> bus during IACK, or true-autovectors and the BIOS just installs the handler
> at the autovector slot, is worth a BigPEmu cross-check — §10. Functionally,
> install your single dispatcher at `$100` and treat the level-2 IRQ as
> vectoring there.)*

**Reset-time vector clear:** the ack handler must clear the pending IRQ line
when acknowledged (`m68k_set_irq(0)` in VJ) or the BIOS re-enters and fails
(`jaguar.c:313` "Without this, the BIOS fails").

### 4.3 The five interrupt sources, multiplexed into the one level-2 line

`INT1` (`$F000E0`, RW, 16-bit) enables/identifies/acks. Bits (VERIFIED Tech Ref
v8 pp.16-17; `JAGUAR.INC:35-45`):

| Bit | Enable | Clear (write) | Source |
|---|---|---|---|
| 0 | `C_VIDENA` $0001 | `C_VIDCLR` $0100 | **Video** time-base (VI line) |
| 1 | `C_GPUENA` $0002 | `C_GPUCLR` $0200 | **GPU** register write |
| 2 | `C_OPENA` $0004 | `C_OPCLR` $0400 | **Object Processor** stop object |
| 3 | `C_PITENA` $0008 | `C_PITCLR` $0800 | **PIT** programmable timer (in TOM) |
| 4 | `C_JERENA` $0010 | `C_JERCLR` $1000 | **Jerry** (edge-triggered, active high) |

- **Read** of `INT1` bits 0–4 = which interrupts are **pending**.
- **Write** bits 0–4 = enable mask; bits 8–12 = **clear/ack** the pending source.
- The ISR must (a) read `INT1` to discover the source, (b) handle it, (c)
  **write the clear bit** for that source (e.g. `INT1 = $0101` clears+keeps
  video enabled — a reference backend's), and (d) **write `INT2`** to restore
  bus priorities (see §4.5).

### 4.4 Exception vector table (low RAM `$000`–`$3FF`)

256 vectors × 4 bytes = `$400` bytes. Big-endian longs. Layout the emulator
and loader must respect:

| Vector # | Address | Use |
|---|---|---|
| 0 | `$000` | Initial SSP (reset) |
| 1 | `$004` | Initial PC (reset) |
| 2 | `$008` | Bus error (**never fires on Jaguar** — see §2.3, but keep a stub) |
| 3 | `$00C` | Address error (CAN fire: odd-address word/long access, `bsr.l` bug) |
| 4 | `$010` | Illegal instruction |
| 5 | `$014` | Zero divide (`DIVU/DIVS` by 0) |
| 6 | `$018` | CHK |
| 7 | `$01C` | TRAPV |
| 8 | `$020` | Privilege violation |
| 9 | `$024` | Trace |
| 10 | `$028` | Line-A (`$Axxx`) emulator |
| 11 | `$02C` | Line-F (`$Fxxx`) emulator |
| 24 | `$060` | Spurious interrupt |
| 25–31 | `$064`–`$07C` | Autovectors level 1–7 (level-2 autovector = #26 `$068`) |
| 32–47 | `$080`–`$0BC` | TRAP #0–#15 |
| **64** | **`$100`** | **Jaguar interrupt dispatcher** (`LEVEL0`/`USER0`) |
| 65–255 | `$104`–`$3FF` | User vectors (unused) |

**Group-0 (bus/address error) stack frame (68000):** pushes 7 words — extra
status word, access address (long), instruction register (word), SR (word),
PC (long). The catch-all handler in a reference backend's dumps the top 8
longs of the frame, confirming the group-0 frame shape. **Group-1/2** (most
others) push the short 6-byte frame: SR (word) then PC (long). Implement both;
the rest of the Jaguar exceptions never trigger group-0 except address error.

### 4.5 `INT2` resume (bus-priority restore) — do not omit

`INT2` (`$F000E2`, WO). VERIFIED Tech Ref v8 p.17 / `JAGUAR.INC:82`:
"When an interrupt is applied to the CPU the **bus priorities of the GPU and
Blitter are reduced** so the CPU can service the interrupt promptly. The
priorities are restored by **writing any value to this register**. This should
always be done at the end of an ISR." Effect: after the `INT2` write, the
Blitter/GPU may restart, and **no further 68k instructions execute until the
next interrupt, or the GPU/Blitter operation completes**. The emulator's bus
arbiter must model: interrupt entry lowers GPU/Blitter priority; `INT2` write
restores it (and may stall the 68k as just described). *(Exact stall/timing of
"no further instructions until GPU/Blitter completes" — model conservatively
and validate; §10.)*

### 4.6 The VI line and PIT (for completeness; detail in Tom/Jerry specs)

- `VI` (`$F0004E`, WO, 11-bit): half-line on which the video interrupt fires.
  Must be **odd** for non-interlaced. The reference backend sets `VI = vdb-2` to fire just
  before display ( comment). `VI = $FFFF` disables it.
- `PIT[0-1]` (`$F00050/$F00052`, WO): system clock ÷ (PIT0+1) ÷ (PIT1+1)
  generates timer interrupts. PIT0=0 disables. (Tech Ref v8 p.16.)

---

## 5. File loaders (cartridge / COF / ABS / JAG / ROM / raw)

The loader maps the file's code into the 68k address space and produces an
**entry PC** (`jaguarRunAddress`) + **load range** (for stack/clear logic).
**All multi-byte header fields are big-endian.** Authoritative source: Virtual
Jaguar `src/core/file.c` (proven loader). Detection (`ParseFileType`) checks
the first bytes, then file-size heuristics.

### 5.1 Type detection (order matters) — VERIFIED `file.c:77-117`

| First bytes | Type | Notes |
|---|---|---|
| `60 1B` | **ABS/COFF type 1** (DRI/Alcyon absolute) | magic `$601B` |
| `01 50` | **ABS/COFF type 2** (BSD/COFF, `$0150`) | the rln/COFF "magic" `0x0150` (mc68k COFF) |
| `60 1A` + `"JAG"` at `$1C` | **Jag Server** executable | `$601A` then bytes `'J','A','G'` at offset `$1C` |
| `60 1A` (no "JAG") | **WTFOMGBBQ** (older RAM-loaded `.jag`) | headerless-ish, load addr at `$1C` |
| size % 1 MB == 0, or size == 128 KB | **ROM** (cartridge) | also Memory Track 128 KB |
| (size + 8 KB) % 1 MB == 0 | **ALPINE** (`.rom`, Alpine board image) | |
| raw 68k startup signature (§5.6) | **RAW_BINARY** | |
| else | none / unsupported | |

### 5.2 ROM / cartridge (`JST_ROM`) — VERIFIED `file.c:135-142`

- `memcpy(mem + $800000, file, size)` — cart maps at `$800000`.
- `jaguarRunAddress = GET32(mem, $800404)` — **entry from cart header
  offset `$404`** (big-endian long).
- Set `cartInserted = true`. Boots via the BIOS path (§3.3 A) on real hardware;
  the emulator may HLE the BIOS but must honor the `$800404` entry.

### 5.3 Alpine `.rom` (`JST_ALPINE`) — VERIFIED `file.c:143-155`

- `memset(mem + $800000, 0xFF, $2000)` (skip the 8 KB header area, fill `$FF`).
- `memcpy(mem + $802000, file, size)` — **loads and runs at `$802000`**.
- `jaguarRunAddress` stays `$802000` (the default set at `file.c:131`).
- (Also stubs the illegal-instruction vector to a local `bra Here` loop.)

### 5.4 ABS type 1 (`$601B`, DRI/Alcyon) — VERIFIED `file.c:156-166`

| Field | File offset | Width | Meaning |
|---|---|---|---|
| magic | `$00` | word | `$601B` |
| text size | `$02` | long | |
| data size | `$06` | long | |
| load addr | `$16` | long | **run == load** for type 1 |
| code image | `$24` | — | start of text+data bytes |

Load: `codeSize = text + data`; `memcpy(mem + loadAddr, file + $24, codeSize)`;
`runAddress = loadAddr`; `loadedRAM = [loadAddr, loadAddr+codeSize)`.

### 5.5 ABS type 2 / COFF `$0150` (rln output) — VERIFIED `file.c:167-176`

| Field | File offset | Width | Meaning |
|---|---|---|---|
| magic | `$00` | word | `$0150` (mc68k COFF) |
| text size | `$18` | long | |
| data size | `$1C` | long | |
| **run addr** | `$24` | long | entry PC |
| **load addr** | `$28` | long | where to copy |
| code image | `$A8` | — | start of text+data bytes |

Load: `codeSize = text + data`; `memcpy(mem + loadAddr, file + $A8, codeSize)`;
`runAddress = GET32(file,$24)`; `loadAddr = GET32(file,$28)`. **This is the
COFF executable rln emits** — the case the project's toolchain produces.

### 5.6 Jag Server (`$601A` + "JAG") — VERIFIED `file.c:177-194`

| Field | File offset | Width |
|---|---|---|
| magic | `$00` | word `$601A` |
| "JAG" tag | `$1C` | 3 bytes |
| load addr | `$22` | long |
| run addr | `$2A` | long |
| code image | `$2E` | — |

`codeSize = fileSize - $2E`; copy `file+$2E` → `mem+loadAddr`; run = `$2A`.

### 5.7 WTFOMGBBQ / older `.jag` (`$601A`, no "JAG") — VERIFIED `file.c:195-204`

Load addr is a **little-endian-in-bytes** field at `$1C`:
`load = file[$1F]<<24 | file[$1E]<<16 | file[$1D]<<8 | file[$1C]`.
`codeSize = fileSize - $20`; copy `file+$20` → `mem+load`; run = load.

### 5.8 Raw binary (no header) — VERIFIED `file.c:27-74,205-222`

Recognized only if the first bytes are the canonical homebrew big-endian-setup
preamble: `GET16($0)==$23FC && GET32($2)==$00070007 && GET32($6)==$00F0210C`
(i.e. `move.l #$00070007,$F0210C` = set GPU big-endian as the first
instruction). Then the load base is **inferred** by scanning for absolute
`jsr/jmp/lea/move.l #imm,An` opcodes (`$4EB9 $4EF9 $41F9 $2039 $2079 $2279`)
whose target operand lands inside one of the candidate bases
`{$00802000, $00020000, $00004000}` — pick the base with the most hits (≥2).
`runAddress = base`. Copy whole file to `mem + base`.

### 5.9 BigPEmu COF quirks the loader must honor — VERIFIED the internal porting notes

- **BigPEmu refuses COF sections below vaddr `$2000`.** Do not place text/data
  below `$2000`. (Your own loader should likewise reject/relocate them, or at
  least warn.)
- **BigPEmu ignores the COF entry field — execution always starts at text
  start.** So: if a program's real entry ≠ text start, put a **jump page at
  text start**. *For the emulator:* match BigPEmu by setting the entry PC to
  **text-section start**, not the header's run-address field, when emulating
  "BigPEmu-compatible" mode. (VJ honors the run field at `$24`; BigPEmu does
  not. These differ — make it a config flag and default to BigPEmu behavior for
  parity. §10.)
- **rln pads inter-object gaps with zeros.** Zero longs decode as `ORI.B #0,D0`
  (`$0000`) chains; an odd count desyncs the stream into the next object's
  first opcode. The emulator faithfully executes these zeros — so a program
  that "falls through" gaps will misbehave exactly as on hardware. (Loader:
  nothing to do; this is a program bug, but useful to know when diagnosing.)
- **rln COF symbols** are 12-byte **BSD a.out `nlist`** records, emitted only
  with `-s`/`-l`; without them `symptr` points at EOF. (For a debug symbol
  loader: each nlist is `{ n_strx:long, n_type:byte, n_other:byte,
  n_desc:word, n_value:long }` = 12 bytes, big-endian. The COFF symbol table
  pointer/count live in the COFF file header; with the Jaguar's BSD variant,
  symbols are nlist not standard 18-byte COFF syments.) *(Exact nlist field
  packing for rln — validate against an `rln -s` output; §10.)*

### 5.10 Loader → reset glue

After loading, set the reset vector for the HLE path (§3.3 B):
`RAM[0..3] = SSP` (top-of-RAM for RAM loads), `RAM[4..7] = runAddress`. For the
ROM/cart path, run the BIOS (or HLE it) so it jumps to `[$800404]`. Record
`loadedRAMStart/End` so RAM randomization/clear does not clobber the image
(VJ preserves `[loadedRAMStart, loadedRAMEnd)` during reset —
`jaguar.c:766-791`).

---

## 6. Endianness / RISC-mode writes the 68k boot issues

Not 68k instructions per se, but the 68k boot must (and the emulator must honor)
these big-endian configuration writes, all **32-bit** stores to GPU/DSP regs:

| Write | Address | Meaning | Source |
|---|---|---|---|
| `$00070007` → `G_END` | `$F0210C` | GPU data org big-endian (IO/pix/inst) | `JAGUAR.INC:179,649-651`; |
| `$00050005` → `D_END` | `$F1A10C` | DSP data org big-endian | |

`BIG_IO=$00010001`, `BIG_PIX=$00020002`, `BIG_INST=$00040004`
(`JAGUAR.INC:649-651`). The GPU write `$0007` = IO|PIX|INST all big; DSP `$0005`
= IO|INST big (pixels N/A on DSP). The 68k always reads/writes these regs as
big-endian longs.

---

## 7. Prioritized 68000 instruction implementation checklist

Priority by what m68k-aout-gcc output + Jaguar boot/startup code actually use
heavily. Implement **P0** first to boot anything; **P1** to run real C; **P2**
for completeness. Encodings: see [external 68000 reference] (e.g. the M68000
Programmer's Reference Manual / the "Yacht.txt" opcode table) — cited at end.

### P0 — required to boot and run startup code / loaders

- **MOVE** (`.b/.w/.l`), **MOVEA** (to An; `.w` sign-extends to 32). All
  addressing modes below. Sets N,Z; clears V,C (MOVE). MOVEA sets no flags.
- **MOVEM** `.w/.l`, register list ↔ memory, both `-(An)` predecrement (push,
  reversed list order) and `(An)+` postincrement (pop). Used by every
  prologue/ISR ( `movem.l %d0-%d1/%a0-%a1,-(%sp)` / restore).
- **LEA**, **PEA** (`lea exc_catch(%pc),%a1`, `lea $200000,%sp`,
  `lea $8,%a0`).
- **Bcc** (all 14 conditions), **BRA**, **BSR** — `.s` (8-bit) and `.w`
  (16-bit) displacements **only** (no `.l` on 68000! §1).
- **JMP**, **JSR**, **RTS**, **RTE** (RTE restores SR+PC from supervisor stack
  — the ISR exit). **RTR**.
- **DBcc** (`DBRA`/`DBF` especially) — decrement-and-branch;
  `dbra %d0,0b`. Condition false → dec Dn.w → if ≠ -1 branch. True → fall
  through.
- **ADD/ADDA/ADDI/ADDQ**, **SUB/SUBA/SUBI/SUBQ**, **CMP/CMPA/CMPI/CMPM**.
  ADDQ/SUBQ data 1–8. ADDA/SUBA/CMPA set no flags.
- **AND/ANDI**, **OR/ORI**, **EOR/EORI**, **NOT**, **NEG/NEGX**.
- **ANDI/ORI/EORI to CCR** and **to SR** (privileged) — `move.w #$2700,%sr`,
  `move #$2300,sr` are `MOVE to SR` (privileged); also `MOVE from SR`,
  `MOVE USP`.
- **CLR** (`clr.l (%a0)+` bss clear), **TST**.
- **EXT** (`.w` byte→word, `.l` word→long; sign-extend).
- **SWAP**, **EXG**.
- **Shifts/rotates:** `LSL/LSR`, `ASL/ASR`, `ROL/ROR`, `ROXL/ROXR` — register
  and memory (`.b/.w/.l`); count by immediate (1–8) or by Dn.
- Immediate forms `#imm` of all the above.

### P1 — required for typical C games

- **MULU.W / MULS.W** (16×16→32). **DIVU.W / DIVS.W** (32÷16→16:16; sets V on
  overflow, traps vector 5 on /0). **No 32-bit MUL/DIV** (§8).
- **BTST/BSET/BCLR/BCHG** — bit ops, immediate or Dn bit number; on Dn operates
  mod 32, on memory mod 8.
- **Scc** (set byte to `$FF`/`$00` by condition).
- **LINK / UNLK** (frame setup; `LINK An,#disp` pushes An, An←SP, SP+=disp).
- **MOVEQ** (8-bit sign-extended immediate → Dn, very common).
- **TRAP #n**, **TRAPV**, **CHK**.
- **PEA**, **MOVE to/from CCR**.

### P2 — completeness / rarely emitted by gcc but legal

- **ABCD/SBCD/NBCD**, **NEGX/ADDX/SUBX** (BCD/extended; gcc rarely emits).
- **TAS** (read-modify-write; the Jaguar bus is fine with it).
- **MOVEP** (peripheral move; some hand-asm uses it — but Jaguar regs are
  word-aligned so unusual).
- **RESET** (privileged; asserts `/RESET` to peripherals — on Jaguar, define as
  a no-op or peripheral reset; validate §10), **STOP** (loads SR, halts until
  interrupt — used by idle loops; must wake on the level-2 IRQ), **NOP**.
- **ILLEGAL** (`$4AFC`), **Line-A/Line-F** traps.

### Addressing modes (implement ALL — gcc uses every one)

| Mode | Syntax | Notes |
|---|---|---|
| Dn | `%d0` | data register direct |
| An | `%a0` | address register direct |
| (An) | `(%a0)` | indirect |
| (An)+ | `(%a0)+` | postincrement (byte to A7 bumps by 2 to keep SP even) |
| -(An) | `-(%a0)` | predecrement (same A7 special-case) |
| (d16,An) | `8(%a0)` | 16-bit signed displacement |
| (d8,An,Xn) | `(d8,%a0,%d1.w)` | 8-bit disp + index reg (Xn `.w` sign-ext or `.l`) |
| (xxx).w | `$8000.w` | absolute short — **sign-extended to 32, then masked to 24** (§2.4) |
| (xxx).l | `$F00028` | absolute long |
| (d16,PC) | `label(%pc)` | PC-relative (`exc_catch(%pc)`) |
| (d8,PC,Xn) | `(d8,%pc,%d0)` | PC-relative indexed |
| #imm | `#$2700` | immediate (size-dependent extension words) |

**A7 (SP) special case:** byte access via `(A7)+`/`-(A7)` adjusts SP by **2**,
not 1, to keep the stack word-aligned. VERIFIED MC68000 behavior.

---

## 8. Multiply / divide — NO 32-bit hardware, flag gotchas

VERIFIED the internal porting notes,:
- **Only 16-bit hardware multiply** (`MULU.W/MULS.W`, 16×16→32) and **16-bit
  divide** (`DIVU.W/DIVS.W`, 32÷16→16:16). 32-bit `*`/`/` are **software**
  (`__mulsi3`, `__divsi3`, `__udivsi3`, `__modsi3`, `__umodsi3`) — see the
  68000-safe implementations in a reference backend's.
- A naive `__mulsi3` **mishandles negative operands** because `MULS.W` treats
  the unsigned 16-bit halves as signed; the real routine uses `MULU.W` partials
  with high-bit corrections.
- `DIVS.W` ≈ **150 cycles** (the porting note's figure for hot-loop budgeting).
  `DIVU.W` similar order. Implement realistic cycle counts (§9) so timing-
  sensitive code paces correctly.
- **Divide flag behavior:** `DIVU/DIVS` set Z/N from the **quotient**; set
  **V** (and leave operands unchanged) on **overflow** (quotient > 16 bits);
  **C is always cleared**; divide-by-zero raises **vector 5** (`$014`). On a
  68000, on overflow the operation is aborted and V set (no result written).
- `MULU/MULS` set N,Z from the 32-bit result; clear V,C.

---

## 9. Cycle timing (for cycle-accuracy)

Use standard MC68000 instruction timings (clocks at the **68k clock =
13.295 MHz**). The Jaguar adds memory-controller wait states the emulator
should model:

- **DRAM** ($0–$1FFFFF): fast-page-mode cycle = **2 clock ticks** per transfer;
  a row change (RAS) costs **3–7 extra ticks** depending on DRAMSPEED
  (`MEMCON1` bits 5,6). (Tech Ref v8 p.7 memory controller, p.10 MEMCON1.)
- **ROM/cart**: ROM cycle time programmable 10/8/6/5 clocks (`MEMCON1`
  ROMSPEED bits 3,4); FASTROM bit = 2 (test only). (Tech Ref v8 p.10.)
- **Peripheral / register space** ($F1xxxx external): IOSPEED bits 11,12 =
  18/10/4/6 clocks overall cycle. (Tech Ref v8 p.10.)
- **Bus arbitration:** the 68k normally has the **lowest** bus priority; under
  interrupt its priority is raised (Tech Ref v8 p.8 "CPU under interrupt"); the
  GPU/Blitter can steal cycles (`BUSHI` priority bit). The `INT2` resume
  mechanism (§4.5) re-lowers it.

**Baseline (no contention)** per the MC68000 timing tables (clocks): MOVE
reg→reg 4; MOVE mem→reg / reg→mem ~8–12; ADD/SUB/AND/OR/CMP reg 4 (.l reg 6–8);
`MULU.W` ~70; `MULS.W` ~70; `DIVU.W` ~140; `DIVS.W` ~158; Bcc taken 10 / not
taken 8 (.b) ; JSR 16; RTS 16; RTE 20; MOVEM n regs ~8+4n (.w)/8+8n(.l). These
are the architectural numbers; **add memory wait states per region above**.
*(For a first pass, a flat "instruction base cycles + per-access region wait"
model is acceptable; refine against BigPEmu frame timing — §10.)*

---

## 10. Open questions (validate against BigPEmu)

1. **Unmapped-read boundary.** Confirm the exact address cutoff where reads
   return `$FFFF` (high `$FFFFxxxx` mirror) vs `$0000` (empty cart space
   `$A1xxxx/$C0xxxx`). Probe staged reads across `$800000–$DFFFFF`,
   `$E00000+`, and high mirrors. (Porting note gives the rule; need the
   boundary.)
2. **Interrupt vector mechanism.** Does real hardware/BigPEmu drive **vector
   number 64** on the data bus during the level-2 IACK, or true-autovector
   (vector 26 @ `$068`) with the handler conventionally installed at `$100`?
   VJ returns 64; SDK names `$100`. Functionally install at `$100`; confirm the
   precise IACK path.
3. **Effective IPL.** Schematic says **exactly level 2** (IPL1 only). Porting
   note observed "≤ 3". Confirm that `move #$2300,sr` (mask 3) masks the VI and
   `#$2100`/`#$2000` (mask ≤1) lets it through — i.e. the level is 2, not 3.
4. **COF entry handling.** BigPEmu **ignores** the COF run-address field and
   starts at **text start**; VJ honors the `$24` field. Default the emulator to
   BigPEmu behavior (text start) for parity; make it a flag.
5. **HLE post-BIOS state.** The exact register snapshot the real BIOS leaves
   (MEMCON1/2 values, CLUT, VMODE, INT1, stack) before jumping to the cart.
   Needed for accurate no-BIOS boot. Snapshot from a BigPEmu cart boot.
6. **`INT2` stall semantics.** Precisely how long the 68k stalls after the
   `INT2` write ("no further instructions until GPU/Blitter completes" — Tech
   Ref v8 p.17). Model and measure.
7. **`RESET` instruction effect on Jaguar peripherals** (Tom/Jerry reset vs
   no-op). And **`STOP`** wake conditions under the level-2-only IRQ wiring.
8. **rln nlist symbol packing** — confirm the 12-byte BSD a.out nlist field
   order/widths against an actual `rln -s`/`-l` output for the debug loader.
9. **`MEMCON1` written value in practice** — whether games re-write it (and
   thus change ROM/DRAM timing mid-run) often enough to matter for cycle
   accuracy.

---

## 11. Authoritative source index

- **Tech Ref v8 PDF** (the "bible"): the Atari SDK's `Jaguar Technical Reference
  v8.pdf` (141 pp). Memory map pp.9–11; register table pp.10–17; CPU/bus
  pp.6–8; interrupts pp.16–17.
- **Official register equates:** the Atari SDK's `JAGUAR.INC` (`DRAM/ENDRAM/INITSTACK` :20-23,
  `LEVEL0/USER0` :29-30, `INT1/INT2` :81-82, endianness :649-651).
- **SDK startup model:** `.../EXAMPLES/3DDEMO/STARTUP.S` (`move.l #INITSTACK,a7`
  :63).
- **Proven reference backend:** boot/ISR/endian setup, 68000-safe mul/div
  (`bsr.l` bug note), and a linker layout that links at `$4000`.
- **Porting notes (hard-won accuracy):** the internal porting notes
  (no-bus-errors :68-76, COF/rln quirks :77-84, IPL :86-91, 68k mul/div/boot
  :132-146).
- **Proven loader (file formats):** Virtual Jaguar libretro
  `src/core/file.c` (magic numbers + offsets) and `src/core/jaguar.c`
  (reset/IRQ-ack/IPL wiring, lines 301-317, 747-920) — cross-checked, not
  local-authoritative; BigPEmu wins on conflicts.
- **External 68000 ISA reference** (opcode encodings, cycle tables): *M68000
  8-/16-/32-Bit Microprocessors User's Manual* (Motorola MC68000UM); the
  community "Yacht.txt" 68000 opcode/cycle table; Motorola MC68000 datasheet.
  Use these for instruction *encoding and base cycle counts*; Jaguar docs win
  on memory/interrupt/timing behavior.
