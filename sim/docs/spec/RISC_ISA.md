# Jaguar RISC ISA (Tom GPU + Jerry DSP) — Implementation Spec

**Scope:** the single 32-bit RISC core ("JRISC") that the Jaguar instantiates
**twice** — once as the **Tom GPU** and once as the **Jerry DSP**. They share one
instruction set, one encoding, one register-file model, and one flags/control
model. They differ only in: a handful of opcode *meanings* (6 opcode slots), a
few control-register addresses/bits, internal RAM size, the interrupt source
table, and the DSP's 40-bit MAC accumulator + modulo/circular-buffer support.

This spec is meant to be turned directly into a Rust `match`-on-opcode decoder +
executor. Where a value is not nailed down by an official doc it is tagged
**(UNVERIFIED)** and listed in *Open questions*.

**Hardware is big-endian.** All multi-byte values described below are big-endian
unless explicitly stated. The one important exception is `MOVEI` immediate data,
which is **little-endian within the instruction stream** (see §6).

## Primary sources

- **TRM** = *Jaguar Technical Reference Manual, Revision 8* —
  the Atari SDK's `Jaguar Technical Reference v8.pdf`.
  Cited by the page number printed in the PDF footer (e.g. "TRM p.45"). GPU
  instruction set: pp.44–58. GPU control regs: pp.59–61. Pipelining/MAC/divide
  prose: pp.33–43. DSP chapter (differences): pp.97–111.
- **INC** = official SDK register equates —
  the Atari SDK's `JAGUAR.INC`
  (cited by line number).
- **Reference backend** = known-good homebrew that runs on BigPEmu.
- **Porting notes** = the internal porting notes.

---

## 1. Instruction word format

Every instruction is **one 16-bit word**. TRM p.44 / p.100:

```
 bit:  15 14 13 12 11 10 | 9  8  7  6  5 | 4  3  2  1  0
       \---- opcode ----/ \--- reg1 ----/ \--- reg2 ----/
            (6 bits)         (5 bits)        (5 bits)
```

- **opcode** = bits 15..10 (6 bits, values 0..63). This is the primary decode key.
- **reg1** = bits 9..5 (5 bits). The **source** operand register number *or* an
  immediate / condition-code / sub-opcode field, depending on the instruction.
- **reg2** = bits 4..0 (5 bits). The **destination** operand register number *or*
  the only operand of single-operand instructions *or* the condition code (for
  `JUMP`).

Decode helpers:
```
opcode = (iw >> 10) & 0x3F
reg1   = (iw >>  5) & 0x1F
reg2   =  iw        & 0x1F
```

Operand-order convention in mnemonics is `OP <source>,<destination>`, i.e.
`ADD Rsrc, Rdst` → `reg1=src`, `reg2=dst`, and the result is written to `reg2`
(TRM p.44).

> TRM p.44/p.100: "The reg1 field of single operand instructions must always be
> set to zero for compatibility with manufacturing test modes and future
> enhancements." (Decoder may ignore reg1 for single-op instructions; do not
> assume it is 0 in adversarial input — mask it off.)

### 1.1 Quick / immediate field encodings (the subtle part)

Several opcodes reuse the `reg1` field as a small immediate. The **assembler**
presents human ranges; the **wire encoding** in `reg1` is what the decoder sees.
The following mappings are the community-standard JRISC encodings (Virtual
Jaguar / MAME), consistent with TRM's range statements. Treat the raw-bit
mappings tagged **(UNVERIFIED-ENC)** as "validate against BigPEmu / a known-good
assembled blob" — the *ranges* are TRM-verified, the *bit pattern* is inference
from the assembler's stated behavior.

| Field use | Mnemonic range (TRM) | `reg1` raw value → semantic value |
|---|---|---|
| `ADDQ`,`ADDQT`,`SUBQ`,`SUBQT`,`ADDQMOD`,`SUBQMOD` quick data | 1..32 | raw `0` ⇒ 32, else raw value (1..31) **(UNVERIFIED-ENC)** |
| `MOVEQ` data | 0..31 | raw value directly (0..31), zero-extended to 32 bits |
| `CMPQ` data | −16..+15 | `reg1` is a **5-bit signed** field, sign-extend to 32 bits |
| `BSET`,`BCLR`,`BTST` bit number | 0..31 | raw value directly = bit index |
| `SHLQ` left shift count | 1..32 | **stored as `32 - n`**; raw `0`⇒shift 32, raw `k`⇒shift `32-k` **(UNVERIFIED-ENC, but TRM p.55 states "encoded as 32-n")** |
| `SHRQ`,`SHARQ`,`RORQ` right count | 1..32 | raw `0` ⇒ 32, else raw value (1..31) **(UNVERIFIED-ENC)** |
| `LOAD/STORE (R14+n)/(R15+n)` index | 1..32 (longwords) | raw `0` ⇒ 32, else raw value; address offset = `value*4` bytes **(UNVERIFIED-ENC)** |
| `JR cc,n` relative offset | −16..+15 words | `reg1` is **5-bit signed**, sign-extend; target = `addr_of_next_instr + offset*2` |
| `JUMP cc,(Rn)` condition code | — | `reg2` holds the 5-bit cc (see §5); `reg1`=address reg |

**Implementation note (quick→32):** the consistent rule across all "1..32"
quick fields is *"raw 0 decodes as 32, raw 1..31 decode as themselves."* This is
the same trick `SHLQ`/`SHRQ` and `ADDQ` use; only `SHLQ` additionally inverts
(`32-n`). Confirm with a 1-instruction BigPEmu probe before freezing.

---

## 2. Register file

TRM p.42/p.43, p.99:

- **64 registers total**, each **32 bits**, in **two banks of 32**: bank 0
  ("primary") and bank 1 ("secondary"/"alternate").
- The 5-bit `reg1`/`reg2` fields address **within the currently selected bank**
  (0..31).
- **Bank selection:**
  - `REGPAGE` (flags bit 14) selects bank 1 when set, bank 0 when clear.
  - **`IMASK` (flags bit 3) overrides `REGPAGE`: when `IMASK=1`, bank 0 is
    forced** regardless of `REGPAGE` (TRM p.43/p.60). So effective bank =
    `IMASK ? 0 : (REGPAGE ? 1 : 0)`.
- **`MOVETA`/`MOVEFA`** move *across* banks: source/destination "alternate"
  register is in the **other** bank from the currently selected one (TRM p.51).
  So if current bank is `b`, `MOVEFA src,dst` reads `src` from bank `1-b`, writes
  `dst` in bank `b`; `MOVETA src,dst` reads `src` from bank `b`, writes `dst` in
  bank `1-b`.
- **Special registers (by convention / hardware-forced for interrupts; TRM
  p.39):**
  - **R31 = interrupt stack pointer.** On interrupt entry the return address is
    pushed onto the R31 stack.
  - **R30 is corrupted on interrupt entry** (do not rely on it across an int).
  - **R14 / R15** are the index base registers for the indexed load/store forms.
    They are otherwise general.
  - Bank 1 (secondary) holds one matrix operand for `MMULT` (the "raison d'être
    of the second bank", TRM p.42).

**Rust model suggestion:** `regs: [[u32; 32]; 2]`, plus a derived
`fn cur_bank(&self) -> usize { if self.imask {0} else {self.regpage as usize} }`.

---

## 3. Flags register (`G_FLAGS` / `D_FLAGS`)

Three ALU flags live in the flags register; the same register also holds IMASK,
interrupt enables/clears, REGPAGE, DMAEN. Bit layout (TRM pp.59–60 GPU, pp.108–109
DSP; INC lines 605–657):

| Bit | Name | INC equate | Meaning |
|---|---|---|---|
| 0 | **ZERO_FLAG (Z)** | `ZERO_FLAG=$01` | ALU zero flag |
| 1 | **CARRY_FLAG (C)** | `CARRY_FLAG=$02` | ALU carry/borrow flag |
| 2 | **NEGA_FLAG (N)** | `NEGA_FLAG=$04` | ALU negative flag (= result bit 31) |
| 3 | **IMASK** | `IMASK=$08` | Interrupt master mask; set by HW on int entry, cleared only by writing 0 (writing 1 has no effect). Forces bank 0. |
| 4..8 | **INT_ENA0..4** | — | Per-source interrupt enables (overridden by IMASK). GPU: bit4=CPU,5=DSP,6=PIT/timing,7=OP,8=Blitter ena (INC 222-226). DSP: bit4=CPU,5=I2S,6=Tim1,7=Tim2,8=Ext0 (INC). |
| 9..13 | **INT_CLR0..4** | — | Write-1-to-clear the matching interrupt latch; reads as 0; write 0 = no change. |
| 14 | **REGPAGE** | `REGPAGE=$4000` | Register bank select (overridden by IMASK). |
| 15 | **DMAEN** | `DMAEN=$8000` | LOAD/STORE run at DMA bus priority instead of GPU/DSP priority. Does not affect program fetch. |
| 16 | **INT_ENA5** (DSP only) | — | Enable for DSP int 5 (External int 1). GPU has no bit 16 here. |
| 17 | **INT_CLR5** (DSP only) | — | Clear for DSP int-5 latch. |

Flag-value mapping inside the emulator's ALU: keep Z/C/N as three booleans and
recompose into bits 0/1/2 on a flags read; recompose IMASK→bit3 etc.

**Pipelining hazard (must model for accuracy):** TRM p.60 WARNING — a value
written to the flag bits is **not** usable by the immediately-following
instruction; at least one instruction must lie between a `STORE` (or flags
write) and a flags-dependent instruction. For a cycle-accurate core this falls
out of the pipeline model (§9); for an instruction-stepped core, document it but
real code already obeys it.

**Which flags each instruction touches is specified per-opcode in §4.** General
rule (TRM p.38):
- **Z** — set if result == 0 (for the ops that affect flags).
- **N** — set if result bit 31 == 1.
- **C** — carry/borrow out of add/sub; bit shifted out for single-bit shift;
  *undefined* after logical ops, MULT/IMULT, etc. (we should still pick a
  deterministic behavior — see §4 notes — but software must not rely on it).

---

## 4. Complete opcode table

`opcode` = bits 15..10. Below, **`Rn`** = register addressed by that field in the
current bank; **`Rs`**=reg1 register, **`Rd`**=reg2 register; `D`=current value of
Rd, `S`=current value of Rs. "Wr" column = which field the result is written to.
"Flags" uses Z/C/N; `—` = unaffected. "Delay" = has a 1-instruction jump delay
slot (only the two jumps do). Cycle numbers are from TRM's "Register Usage"
(pipeline stage at which the result becomes valid); they matter for the
cycle-accurate scoreboard model (§9).

Opcode numbers are **decimal** (as printed in TRM). Same for both GPU and DSP
**except** the 6 slots called out in §4.2.

### 4.1 Shared opcodes (identical on GPU and DSP)

| Op# | Mnemonic | reg1 use | Operation (writes reg2 unless noted) | Z | C | N | Notes |
|----:|----------|----------|--------------------------------------|---|---|---|-------|
| 0 | `ADD Rs,Rd` | src reg | `Rd = D + S` | z | carry-out | n | TRM p.45 |
| 1 | `ADDC Rs,Rd` | src reg | `Rd = D + S + C_in` | z | carry-out | n | carry-in = current C; p.45 |
| 2 | `ADDQ n,Rd` | quick 1..32 | `Rd = D + n` | z | carry-out | n | p.45 |
| 3 | `ADDQT n,Rd` | quick 1..32 | `Rd = D + n` | — | — | — | transparent to flags; p.45 |
| 4 | `SUB Rs,Rd` | src reg | `Rd = D − S` | z | borrow-out | n | p.57 |
| 5 | `SUBC Rs,Rd` | src reg | `Rd = D − S − C_in` | z | borrow-out | n | borrow-in = C; p.58 |
| 6 | `SUBQ n,Rd` | quick 1..32 | `Rd = D − n` | z | borrow-out | n | p.58 |
| 7 | `SUBQT n,Rd` | quick 1..32 | `Rd = D − n` | — | — | — | transparent; p.58 |
| 8 | `NEG Rd` | (0) | `Rd = 0 − D` | z | borrow-out | n | $80000000 unchanged-result; p.52 |
| 9 | `AND Rs,Rd` | src reg | `Rd = D & S` | z | undef | n | p.46 |
| 10 | `OR Rs,Rd` | src reg | `Rd = D \| S` | z | undef | n | p.53 |
| 11 | `XOR Rs,Rd` | src reg | `Rd = D ^ S` | z | undef | n | p.58 |
| 12 | `NOT Rd` | (0) | `Rd = ~D` (= D ^ 0xFFFFFFFF) | z | undef | n | p.52 |
| 13 | `BTST n,Rd` | bit 0..31 | test bit n of D (no write) | bit==0 | undef | undef | p.46; **Z set if selected bit is 0** |
| 14 | `BSET n,Rd` | bit 0..31 | `Rd = D \| (1<<n)` | z (result==0) | undef | n | p.46 |
| 15 | `BCLR n,Rd` | bit 0..31 | `Rd = D & ~(1<<n)` | result==0 | undef | bit31 of result | p.46 |
| 16 | `MULT Rs,Rd` | src reg | `Rd = (D & 0xFFFF) * (S & 0xFFFF)` unsigned 16×16→32 | z | undef | bit31 | p.51 |
| 17 | `IMULT Rs,Rd` | src reg | `Rd = sext16(D)*sext16(S)` signed 16×16→32 | z | undef | n | p.47 |
| 18 | `IMULTN Rs,Rd` | src reg | `acc = sext16(D)*sext16(S)` — **no write-back**; starts MAC group | z | undef | n | p.48; see §7 |
| 19 | `RESMAC Rd` | (0) | `Rd = acc` (write accumulated result) | — | — | — | p.53; see §7 |
| 20 | `IMACN Rs,Rd` | src reg | `acc += sext16(D)*sext16(S)` — **no write-back** | — | — | — | p.47; see §7 |
| 21 | `DIV Rs,Rd` | src reg (divisor) | `Rd = D / S` unsigned (or 16.16, see §8) | — | — | — | 16-tick latency; remainder→`G_REMAIN`; p.47 |
| 22 | `ABS Rd` | (0) | `Rd = (D<0)? −D : D` | z | set if operand was negative | **cleared** | $80000000 left unchanged with N set; p.45 |
| 23 | `SH Rs,Rd` | src reg | shift: if `S>=0` shift right by S, else left by `-S`; ≥32 ⇒ 0; zero filled | z | see note | n | C = bit shifted out (bit0 for right, bit31 for left); p.54 |
| 24 | `SHLQ n,Rd` | **32−n** | `Rd = D << n` (n=1..32) | z | bit31 of pre-shift D | n | encoded 32−n; p.55 |
| 25 | `SHRQ n,Rd` | quick 1..32 | `Rd = D >> n` logical (zero fill) | z | bit0 of pre-shift D | n | p.55 |
| 26 | `SHA Rs,Rd` | src reg | arithmetic SH (right shift sign-fills) | z | see SH note | n | p.55 |
| 27 | `SHARQ n,Rd` | quick 1..32 | `Rd = D >> n` arithmetic (sign fill) | z | bit0 of pre-shift D | n | p.55 |
| 28 | `ROR Rs,Rd` | src reg | rotate right by `S & 0x1F` | z | bit31 of result | n | p.53 |
| 29 | `RORQ n,Rd` | quick 1..32 | rotate right by n | z | bit31 of result | n | p.53 |
| 30 | `CMP Rs,Rd` | src reg | flags of `D − S` (no write) | z (equal) | borrow-out | n (S>D) | p.46 |
| 31 | `CMPQ n,Rd` | signed −16..+15 | flags of `D − sext5(n)` (no write) | z | borrow-out | n | p.47 |
| 34 | `MOVE Rs,Rd` | src reg | `Rd = S` | — | — | — | p.51 |
| 35 | `MOVEQ n,Rd` | data 0..31 | `Rd = zext(n)` | — | — | — | p.51 |
| 36 | `MOVETA Rs,Rd` | src reg | move to **other** bank: `bank[1-b][Rd] = bank[b][Rs]` | — | — | — | p.51 |
| 37 | `MOVEFA Rs,Rd` | src reg | move from **other** bank: `bank[b][Rd] = bank[1-b][Rs]` | — | — | — | p.51 |
| 38 | `MOVEI n,Rd` | (0) | `Rd =` next 2 instr words as 32-bit imm (little-endian, §6) | — | — | — | atomic; PC advances 2 words; p.51 |
| 39 | `LOADB (Rs),Rd` | addr reg | `Rd = zext8( mem8[S] )` (external only; internal RAM does 32-bit read) | — | — | — | p.49 |
| 40 | `LOADW (Rs),Rd` | addr reg | `Rd = zext16( mem16[S] )` (external; internal ⇒ 32-bit) | — | — | — | word-aligned; p.49 |
| 41 | `LOAD (Rs),Rd` | addr reg | `Rd = mem32[S]` | — | — | — | longword-aligned; p.48 |
| 43 | `LOAD (R14+n),Rd` | quick 1..32 | `Rd = mem32[R14 + n*4]` | — | — | — | offset in **longwords**; +2 ticks; p.49 |
| 44 | `LOAD (R15+n),Rd` | quick 1..32 | `Rd = mem32[R15 + n*4]` | — | — | — | p.49 |
| 45 | `STOREB Rs,(Rd)` | addr=reg1 | `mem8[S] = Rd_value & 0xFF` (external; internal ⇒ 32-bit) | — | — | — | **reg1=address, reg2=data**; p.56 |
| 46 | `STOREW Rs,(Rd)` | addr=reg1 | `mem16[S] = Rd_value & 0xFFFF` | — | — | — | p.56 |
| 47 | `STORE Rs,(Rd)` | addr=reg1 | `mem32[S] = Rd_value` | — | — | — | p.55 |
| 49 | `STORE Rs,(R14+n)` | quick 1..32 | `mem32[R14 + n*4] = Rs_value` | — | — | — | **here reg1=data src, reg2=n** (see §4.3); p.56 |
| 50 | `STORE Rs,(R15+n)` | quick 1..32 | `mem32[R15 + n*4] = Rs_value` | — | — | — | p.56 |
| 51 | `MOVE PC,Rd` | (0) | `Rd = PC` (pipeline-corrected; see §9) | — | — | — | only way to read own PC; p.51 |
| 52 | `JUMP cc,(Rs)` | addr reg | if cc(flags) then PC←S (after delay slot) | — | — | — | cc in **reg2**; delay slot; §5 |
| 53 | `JR cc,n` | signed n | if cc then PC←(next_instr + n*2) | — | — | — | cc in **reg2**; delay slot; §5 |
| 54 | `MMULT Rs,Rd` | src (bank1) | systolic matrix multiply; result→Rd | z | carry-out | n | flags reflect final MAC; §7; p.50 |
| 55 | `MTOI Rs,Rd` | src reg | mantissa-to-integer from IEEE-754 float in S | z | undef | n | sign-extended from bit 23; p.51 |
| 56 | `NORMI Rs,Rd` | src reg | normalization integer of S (signed shift count) | z | undef | n | p.52 |
| 57 | `NOP` | (0) | nothing | — | — | — | occupies only pipe stage 1; p.52 |
| 58 | `LOAD (R14+Rs),Rd` | offset reg | `Rd = mem32[R14 + Rs]` (byte offset) | — | — | — | p.49 |
| 59 | `LOAD (R15+Rs),Rd` | offset reg | `Rd = mem32[R15 + Rs]` | — | — | — | p.49 |
| 60 | `STORE Rs,(R14+Rd)` | data=reg1 | `mem32[R14 + Rd_value] = Rs_value` | — | — | — | reg2 supplies offset value AND is "destination field"; p.56 |
| 61 | `STORE Rs,(R15+Rd)` | data=reg1 | `mem32[R15 + Rd_value] = Rs_value` | — | — | — | p.56 |

Opcodes **32, 33, 42, 48, 62, 63** are the **divergent** slots — see §4.2.

### 4.2 Divergent opcodes (GPU vs DSP)

These six opcode numbers decode to **different instructions** depending on whether
the core is the Tom GPU or the Jerry DSP. TRM p.59 (GPU) and TRM p.100 ("Differences
from the GPU Instruction set: LOADP, SAT8, SAT16, SAT24, STOREP, PACK and UNPACK
are absent; SAT16S, SAT32S, ADDQMOD, SUBQMOD and MIRROR have been added").

| Op# | **GPU (Tom)** | **DSP (Jerry)** |
|----:|---------------|-----------------|
| 32 | `SAT8 Rd` — saturate signed→[0,255]; Z=z, N=cleared, C=undef (p.54) | `SUBQMOD n,Rd` — `SUBQ` then modulo-mask via `D_MOD`; Z=z, C=borrow(pre-mask), N=n (p.108) |
| 33 | `SAT16 Rd` — saturate signed→[0,65535]; Z=z, N=cleared, C=undef (p.54) | `SAT16S Rd` — saturate signed→[−32768,+32767]; Z=z, N=cleared, C=undef (p.106) |
| 42 | `LOADP (Rs),Rd` — 64-bit read: low long→Rd, high long→`G_HIDATA`; external only (p.50) | `SAT32S Rd` — saturate the 40-bit MAC accumulator→signed 32-bit; Z=z, N=n, C=undef (p.106) |
| 48 | `STOREP Rs,(Rd)` — 64-bit write: low long from Rd, high long from `G_HIDATA` (p.56) | `MIRROR Rd` — bit-reverse Rd (bit0↔bit31, …); Z=z, N=n, C=undef (p.104) |
| 62 | `SAT24 Rd` — saturate signed→[0,16777215]; Z=z, N=cleared, C=undef (p.54) | **unused/reserved (UNVERIFIED)** — DSP has no SAT24 and no op#62 listed (TRM omits it). Treat as illegal/NOP and log. |
| 63 | `PACK`/`UNPACK Rd` — CRY pixel pack/unpack; selected by reg1 (see §4.4) (p.53) | `ADDQMOD n,Rd` — `ADDQ` then modulo-mask via `D_MOD`; Z=z, C=carry, N=n (p.101) |

So the decoder needs a `is_dsp: bool` (or two decode tables). Everything else is
shared.

### 4.3 STORE operand-field caveat (read this twice)

The mnemonic is `STORE <source>,<dest-address>`. For the plain forms the
**address** comes from `reg1` and the **data** from `reg2`:

- `STOREB/STOREW/STORE (op 45/46/47)`: `address = reg[reg1]`, `data = reg[reg2]`.
  (TRM "Register Usage": *Cycle 1: Source register read & Destination register
  read*, where for stores the source reg holds the address and the destination
  reg holds the data — i.e. reg1=address, reg2=data.)

For the **indexed** STORE forms the roles invert in a way that is easy to get
wrong:

- `STORE Rs,(R14+n)` / `(R15+n)` (op 49/50): `reg1` = **data** source register,
  `reg2` = the **5-bit index n** (1..32 longwords). Base = R14/R15.
- `STORE Rs,(R14+Rd)` / `(R15+Rd)` (op 60/61): `reg1` = **data** source register,
  `reg2` = register whose value is the **byte offset** added to R14/R15.

The corresponding LOAD forms (43/44/58/59) are unambiguous: `reg1` carries the
index/offset, `reg2` is the load destination.

> This asymmetry is the #1 JRISC decoder bug. Cross-checked against a reference
> backend, which only uses `store r0,(r1)` style plain stores and `load
> (r14+n)` style indexed loads, so the indexed-store field order is **(UNVERIFIED
> by local known-good code)** — validate the (R14+n)/(R14+Rn) STORE field
> assignment against BigPEmu or against Virtual Jaguar's `dsp.cpp`/`gpu.cpp`.

### 4.4 PACK / UNPACK (op 63, GPU only)

Single opcode; `reg1` selects the direction (TRM p.53):
- `reg1 == 0` ⇒ **PACK Rd**: unpacked→16-bit CRY. Map bits: `[25:22]→[15:12]`,
  `[16:13]→[11:8]`, `[7:0]→[7:0]`.
- `reg1 == 1` ⇒ **UNPACK Rd**: 16-bit CRY→unpacked. Map bits: `[15:12]→[25:22]`,
  `[11:8]→[16:13]`, `[7:0]→[7:0]`, all other bits 0.

Flags: ZNC unaffected for both (TRM p.53/58).

---

## 5. Condition codes (JUMP / JR)

The 5-bit condition code lives in **`reg2`** of `JUMP`/`JR`. Bit meanings
(TRM p.41, p.47):

```
cc bit 0: zero flag must be CLEAR for jump
cc bit 1: zero flag must be SET   for jump
cc bit 2: "selected flag" must be CLEAR for jump
cc bit 3: "selected flag" must be SET   for jump
cc bit 4: selected flag = N if set, = C if clear
```
If multiple condition bits are set they are **ANDed** (all must hold). Evaluation:

```
sel  = (cc & 0x10) ? N : C          // bit4 picks N vs C
ok   = true
if (cc & 0x01) ok &= !Z
if (cc & 0x02) ok &=  Z
if (cc & 0x04) ok &= !sel
if (cc & 0x08) ok &=  sel
jump_taken = ok
```
`cc==0x00` ⇒ always jump; `cc==0x1F` ⇒ never. The "useful" assembler mnemonics
(TRM p.41):

| cc (hex) | mnemonic | jump if |
|---|---|---|
| 00 | (always) | always |
| 01 | NZ / NE | Z clear |
| 02 | Z / EQ | Z set |
| 04 | NC / CC | C clear |
| 05 | NC NZ | C clear AND Z clear |
| 06 | NC Z | C clear AND Z set |
| 08 | C / CS | C set |
| 09 | C NZ | C set AND Z clear |
| 0A | C Z | C set AND Z set |
| 14 | NN / PL | N clear |
| 15 | NN NZ | N clear AND Z clear |
| 16 | NN Z | N clear AND Z set |
| 18 | N / MI | N set |
| 19 | N NZ | N set AND Z clear |
| 1A | N Z | N set AND Z set |
| 1F | (never) | never |

**Porting note (the internal porting notes §5):** RMAC does **not** expose `LT`/`LE`-style signed
condition mnemonics — the JRISC condition set is the carry/zero/negative-flag set
above (no V flag, no synthesized signed comparisons). Signed comparisons must be
built from N/Z by the programmer. The *hardware* condition test is exactly the
5-bit field above; the assembler limitation is a separate concern (the emulator
only ever sees the 5-bit field). Codes not in the table evaluate per the boolean
formula and are "reserved" per TRM (still deterministic).

**A reference backend confirms:** only `EQ`, `NE`, `MI` and unconditional jumps
are used; forward jumps are always `movei #target,rN; jump <cc>,(rN)` because
`JR` has only ±15-word reach and the kernel "has no forward jr" (file header
comment, lines 29–31). Every `jump`/`jr` is followed by **2 NOPs** in that code
(see §9 — only 1 is architecturally required as the delay slot; the 2nd is a
scoreboard-safety convention).

---

## 6. MOVEI — 32-bit immediate (op 38)

TRM p.51: "32-bit register load with next 32-bits of instruction stream. **The
first word in the instruction stream is the low word, the second the high
word.**"

So the immediate is **little-endian at word granularity within the instruction
stream**, even though the machine is otherwise big-endian:

```
iw0 = the MOVEI opcode word        (at PC)
w1  = instruction word at PC+2     -> LOW  16 bits of immediate
w2  = instruction word at PC+4     -> HIGH 16 bits of immediate
imm32 = (u32(w2) << 16) | u32(w1)
Rd = imm32
PC += 6   // 3 words total consumed
```

This holds **regardless of `BIG_INSTR`** (the data-organization bit). TRM p.60:
"However, move immediate data **remains little-endian**, i.e. the data must always
be in the order low word then high word in the instruction stream." `BIG_INSTR`
only swaps the order the two halves of a *longword* of **code** are *executed*
(low-word-then-high vs high-then-low); it never swaps MOVEI's operand words.

**Atomicity (TRM p.39):** `MOVEI` locks out interrupts while it fetches its two
immediate words — an interrupt cannot land between the opcode and its data.

**Illegal combo (TRM p.40):** do **not** place `MOVEI` in a jump delay slot — the
jump takes effect before the data is fetched and the immediate comes from the
wrong address. Real assemblers/code avoid it; the emulator should still execute
deterministically if it occurs (follow the hardware: the jump's PC change wins,
so the immediate is read from the post-jump stream). Behavior here is
**(UNVERIFIED)** — flag if a ROM hits it.

---

## 7. Multiply / MAC chain and matrix multiply

### 7.1 MAC chain (IMULTN / IMACN / RESMAC)

TRM p.42, p.47-53. The MAC group computes `Σ (signed16 × signed16)` into a hidden
accumulator, then writes it out:

```
IMULTN Rs,Rd : acc  = sext16(Rd_lo) * sext16(Rs_lo)     // start; NO write-back
IMACN  Rs,Rd : acc += sext16(Rd_lo) * sext16(Rs_lo)     // accumulate; NO write-back
RESMAC Rd    : Rd = acc                                  // write result
```
- On the **DSP**, `acc` is a **40-bit signed** accumulator (8 overflow bits above
  bit 31; TRM p.99). On the **GPU** it is effectively 32-bit visible (the TRM
  describes the 40-bit form only for the DSP). For the emulator, model `acc` as
  `i64` on both and only expose the extra 8 bits on the DSP via `SAT32S` and
  `D_MACHI`.
- **`D_MACHI` (`$F1A120`, INC line 536)** exposes the top byte (bits 39..32) of the
  DSP MAC accumulator. The GPU has **no** documented MACHI register
  **(UNVERIFIED — GPU MAC overflow visibility)**.
- **Sequencing rule (TRM p.40, atomic):** an `IMULTN`/`IMACN` must be followed
  **only** by another `IMACN` or by `RESMAC` — no intervening instruction. Each
  `IMULTN`/`IMACN` is atomic with its successor (interrupts locked out across the
  pair). The emulator can treat the whole `IMULTN … IMACN* RESMAC` run as an
  uninterruptible group.
- `IMULTN`/`IMULT`/`IMACN` operate on the **low 16 bits** of each operand,
  sign-extended.
- Flags: `IMULTN` sets Z/N from its product; `IMACN` leaves flags unaffected;
  `RESMAC` leaves flags unaffected (TRM p.47-53).

### 7.2 Systolic matrix multiply (MMULT, op 54)

TRM p.42, p.50. `MMULT Rs,Rd` runs an internally-generated `IMULTN; IMACN×k;
RESMAC` sequence of length = `MWIDTH` (3..15), producing one dot-product term:

- **One matrix operand is in the secondary register bank** (bank 1), **packed two
  16-bit elements per 32-bit register**. `Rs` (reg1) names the bank-1 register
  holding the first two elements of the row.
- **The other matrix operand is in local RAM**, addressed by **`MTXADDR`**
  (matrix address register), traversed by **row** or **column** per `MADDW`.
  Each element is **one 32-bit word (stride 4)**, and the value is taken from
  the **LOW 16 bits** of that word, sign-extended (the MAC datapath is s16,
  §7.1). *(Silicon 2026-07-23, `p_mm_mmlo`: value in the low half → correct,
  high half → 0. jsim originally read the high half — fixed.)*
- **`MTXADDR` AUTO-ADVANCES by one row/column per MMULT** (by-row: += `MWIDTH`×4
  bytes), so a run of MMULTs with `MTXADDR` written **once** walks the matrix —
  the systolic array's purpose (set `MTXA`, issue `MWIDTH` MMULTs, collect the
  matrix×vector product). An explicit write to `MTXA` between MMULTs overrides
  the advance (per-row re-point). *(Silicon 2026-07-24, `p_mm_mm2s`/`p_mm_mm3s`:
  `MTXA` set once → successive MMULTs read rows 0,1,2 = 32/320/3200. by-column
  advance is inferred, UNVERIFIED.)*
- **Two ADJACENT MMULTs hard-wedge real Tom** (bug 23, bus held; power-cycle to
  recover). Separate consecutive MMULTs by a settle (≥ a few instructions; an
  8-nop gap is safe, zero is not). *(Silicon 2026-07-23, `p_mm_mm2` vs
  `p_mm_mm2s`.)*
- **`MWIDTH`** = number of multiply/accumulate terms (matrix width 3..15).
- Result is written to `Rd` in the **currently selected** bank; the MAC
  **resets per MMULT** (back-to-back MMULTs do not accumulate onto each other).
- Flags reflect the **final** MAC: Z, N, and C=carry-out (TRM p.50).
- **Atomic:** interrupts locked out for the whole MMULT (TRM p.39).
- **Illegal combo (TRM p.40):** `MMULT` must **not** be preceded by a `LOAD` or
  `STORE`.

**Control registers (GPU `G_MTXC/G_MTXA`, DSP `D_MTXC/D_MTXA`):**

| Reg | Addr (GPU / DSP) | Bits | Meaning |
|---|---|---|---|
| `*_MTXC` (Matrix Control, **write-only**) | `$F02104` / `$F1A104` | 0..3 `MWIDTH` (3..15); 4 `MADDW` (0=row, 1=column) | INC 177/528; TRM p.60/109 |
| `*_MTXA` (Matrix Address, **write-only**) | `$F02108` / `$F1A108` | 2..11 `MTXADDR` (address into local RAM) | INC 178/529; TRM p.60/110 |

INC equates: `MATRIX3..MATRIX15 = 3..15`; `MATROW=$00`, `MATCOL=$10` (INC).

The exact element-fetch ordering (which packed half is element 0, row-vs-column
stride in bytes) is **(UNVERIFIED)** at the wire level — derive from a known-good
DCT/3D-rotate routine or Virtual Jaguar and validate on BigPEmu.

---

## 8. Divide unit (DIV, op 21)

TRM p.43, p.47; control at `G_DIVCTRL`/`D_DIVCTRL` = `$F0211C`/`$F1A11C`
(write-only alias of the read-only remainder register `G_REMAIN`/`D_REMAIN`).

- `DIV Rs,Rd`: **unsigned** 32-bit divide. `Rd (dividend) / Rs (divisor)` →
  quotient in `Rd`. **Remainder** is left in `G_REMAIN`/`D_REMAIN` (read-only).
- **Latency:** serial, **2 bits/tick ⇒ 16 ticks** to complete. The result is
  **scoreboarded**: any instruction that reads the quotient register, reads the
  remainder, or starts another `DIV` while the unit is busy **stalls** until done
  (1..16 wait states; TRM p.61). Flags are **unaffected** by `DIV`.
- **Remainder caveat:** the remainder register may read **negative**, in which
  case it holds `(true_remainder − divisor)`; if positive it is the true
  remainder (TRM p.43). Software must correct.
- **16.16 mode:** `DIV_OFFSET` (bit 0 of the divide-control register, INC 657
  `DIV_OFFSET=$01`). When set, the divide treats operands as **unsigned 16.16
  fixed-point** and yields a 16.16 quotient; when clear, plain 32-bit integer
  divide. A reference backend sets this once per frame: `moveq #1,r0; store r0,(G_DIVCTRL)` for
  "16.16 divide mode, frame-global".
- **Sign handling is the programmer's job** — the unit is unsigned only. A reference
  backend notes: "div unsigned -> explicit sign handling."

**Emulator model for 16.16:** dividend treated as `dividend << 16` then integer
divide by divisor (i.e. `quotient = (u64(dividend) << 16) / divisor`), remainder
accordingly. Confirm the exact remainder semantics in 16.16 mode against BigPEmu
**(UNVERIFIED-DETAIL)**.

---

## 9. Pipeline, delay slots, scoreboard, PC semantics

TRM pp.33-35, p.40. For a **cycle-accurate** core, model a 4-stage pipe with a
register scoreboard; for a faster instruction-stepped core, model only the
*observable* effects (delay slot + MOVE PC value + DIV/load latency + flag-write
hazard). Observable rules:

1. **Jump delay slot (1 instruction).** Both `JUMP` and `JR` execute the **one
   instruction after them** before the control transfer happens (TRM p.35/p.40).
   The PC change is deferred by exactly one instruction. This is the only
   architectural delay slot. *(A reference backend uses 2 NOPs after each jump, but the 2nd is a
   scoreboard-safety convention, not architecture — see rule 5.)*

2. **`MOVE PC,Rd` value (op 51).** Returns a **pipeline-corrected** PC, i.e. the
   address of the *current* `MOVE PC` instruction adjusted for prefetch (TRM
   p.51). The exact constant offset (how many words ahead the raw PC is) is
   **(UNVERIFIED)** — Virtual Jaguar uses the address such that the value equals
   the instruction's own address; pin this with a BigPEmu probe (`movei #here,r0`
   vs `move PC,r1` at a known address). Do **not** place `MOVE PC,Rd` immediately
   after a jump (TRM p.40, illegal combo — value unreliable).

3. **Load latency / scoreboard.** A `LOAD`'s destination register is **not valid
   immediately**: internal RAM ~cycle 3-4 (5-6 for indexed), external memory
   subject to bus latency (TRM p.48-49). Reading the destination too soon stalls
   (scoreboard) until the data lands. A reference backend's rule: "≥2 instructions
   after load before use." Model: a load marks its dest "pending" for N cycles; a
   read of a pending reg inserts wait states.

4. **MULT latency.** A reference backend notes: "2 nops after mult before reading the
   product." TRM "Register Usage" says MULT/IMULT write at **cycle 3**. Model the
   product as available 2 instructions later; earlier reads stall.

5. **Flag-write hazard.** Flags written by an instruction (esp. via `STORE` to
   the flags reg, or any flag-affecting op) are not usable by the *immediately
   following* instruction; ≥1 instruction must intervene (TRM p.60 WARNING).

6. **`G_CTRL`/`D_CTRL` start/stop & PC corruption.** When `RISCGO`/`GPUGO` is
   **cleared**, the prefetch queue is discarded and the **PC value is corrupted**
   (TRM p.40/p.110). The CPU must always **write the PC before setting GPUGO**.
   After a stop, the read-back PC is not the resume point. Emulator: on
   `GPUGO 1→0`, invalidate prefetch; on `0→1`, begin fetch at the written PC.

7. **Atomic instruction groups (no interrupt may land inside):** `MOVEI` (+its 2
   data words), `MMULT`, and each `IMULTN`/`IMACN`→successor pair, and a jump→its
   delay-slot instruction (TRM p.39).

8. **Illegal combinations (TRM p.40)** — deterministic-but-don't-rely-on:
   - `MOVEI` in a jump delay slot (immediate fetched from wrong PC).
   - two jumps back-to-back ("results not predictable").
   - `MOVE PC,Rd` right after a jump (value unreliable).
   - non-`IMACN`/`RESMAC` after `IMULTN`/`IMACN`.
   - `LOAD`/`STORE` immediately before `MMULT`.

---

## 10. Control / status register (`G_CTRL` / `D_CTRL`)

`G_CTRL=$F02114`, `D_CTRL=$F1A114` (INC 181/532). 32-bit, read/write. TRM
pp.60-61 / pp.110-111. **Shared bit names from INC (lines 605-612):**

| Bit | Name | INC equate | Function |
|---|---|---|---|
| 0 | **RISCGO** (GPUGO/DSPGO) | `RISCGO=$01` | Start/stop the core. Any master may **set**; only the core itself should **clear** (except CPU may clear during single-step). Clearing discards prefetch & corrupts PC. |
| 1 | **CPUINT** | `CPUINT=$02` | Write 1 ⇒ core raises an interrupt to the 68000. No ack, no clear needed. Reads 0. |
| 2 | **FORCEINT0** (GPUINT0/DSPINT0) | `FORCEINT0=$04` | Write 1 ⇒ force a type-0 interrupt **on this core** (used to kick the GPU/DSP from outside). Reads 0. |
| 3 | **SINGLE_STEP** | `SINGLE_STEP=$08` | Enable single-step. Read-back as **SINGLE_STOP** = 1 means the core has halted awaiting `SINGLE_GO`. |
| 4 | **SINGLE_GO** | `SINGLE_GO=$10` | Write 1 ⇒ advance one instruction while in single-step. Reads 0. |
| 5 | — | — | unused; write 0. |
| 6..10 | **INT_LAT0..4** | `*_..LAT` (INC) | Interrupt latch status (which int is pending). Cleared via INT_CLR in flags. Write = no effect. |
| 11 | **BUS_HOG** | — | Hold the external bus between program fetches (faster core, starves lower-priority masters). |
| 12..15 | **VERSION** | — | Read-only silicon version. GPU: 1=test, 2=production. DSP: 2=production. (Pick **2** for the emulated part unless a ROM needs otherwise.) |
| 16 | **INT_LAT5** (DSP only) | — | Latch for DSP int 5. |

**Single-step protocol (TRM p.40):** set PC; set `GPUGO|SINGLE_STEP`; poll
`SINGLE_STOP` (read bit 3) ⇒ first instr done; write `SINGLE_GO` (keep
GPUGO+SINGLE_STEP); poll `SINGLE_STOP`; repeat. The emulator's debug single-step
should mirror this so a debugger script can drive it.

**Interrupt entry behavior on the RISC (TRM p.39):**
1. On accepted interrupt `k` (k = source number), HW pushes the **address of the
   last instruction executed** onto the R31 stack (`R31 -= 4; mem32[R31] = ret`),
   sets **IMASK** (forces bank 0), and vectors to **`local_RAM_base + 16*k`**
   (16 *bytes* per vector — i.e. 4 instruction words of space per handler; GPU
   base `$F03000`, DSP base `$F1B000`).
2. **R30 is corrupted** by the entry sequence; **R28/R29 are conventionally
   scratch** for the ISR (the example ISR uses them).
3. The ISR returns by reading `mem32[R31]`, **adding 2** (to point past the
   interrupted instruction), bumping `R31 += 4`, and `JUMP (Rn)`; IMASK is cleared
   by writing 0 to flags bit 3 — but **not in the jump's delay slot** (the internal porting notes /
   TRM p.6480 region: "you can't put the IMASK clear in the delay slot of the jump
   out of the interrupt").
4. Priority: if two interrupts arrive within a few ticks, the **higher-numbered**
   is serviced first; otherwise software-prioritized. Source→vector mapping:

| GPU int | source | DSP int | source |
|---|---|---|---|
| 0 | CPU | 0 | CPU |
| 1 | DSP (from Jerry) | 1 | I²S |
| 2 | Timing generator (PIT) | 2 | Timer 0 |
| 3 | Object Processor | 3 | Timer 1 |
| 4 | Blitter | 4 | External int 0 |
| — | — | 5 | External int 1 |

---

## 11. Data-organization register (`G_END` / `D_END`)

`$F0210C` / `$F1A10C`, **write-only** (INC 179/530). TRM p.60/110. Controls
endianness of register I/O, pixels, and code-word execution order:

| Bit | Name | INC | Effect when set |
|---|---|---|---|
| 0 | `BIG_IO` | `BIG_IO=$00010001` | 32-bit I/O-space registers are big-endian (MS 16 bits at lower address). |
| 1 | `BIG_PIX` | `BIG_PIX=$00020002` | Pixel organization big-endian. |
| 2 | `BIG_INSTR` | `BIG_INST=$00040004` | Execute the two code words of a longword **high-word-then-low** instead of low-then-high. **Does NOT affect MOVEI** (always low-then-high; §6). |

INC defines these as both-halves values (`$000n000n`) because the doc-reg is
written to both 16-bit halves when the current contents are unknown (TRM p.60).
For a from-cold emulator, the post-reset default is **(UNVERIFIED)** — the SDK
startup typically writes a known value; assume `BIG_INSTR=0` (low-then-high
execution) unless a ROM sets it. Most Jaguar code runs with `BIG_INSTR` clear.

---

## 12. Memory map of a RISC core (for the executor's address decode)

GPU (Tom), TRM p.36:
```
$F02000-$F021FF  GPU control registers (flags, MTXC/A, END, PC, CTRL, HIDATA, REMAIN/DIVCTRL)
$F02200-$F022FF  Blitter registers
$F02300-$F02FFF  reserved
$F03000-$F03FFF  GPU local RAM (4 KB = 1K × 32-bit)   <-- code+data, single-cycle
$F04000-$F0FFFF  reserved
```
DSP (Jerry), TRM p.97:
```
$F1A000-$F1A1FF  DSP control registers
$F1B000-$F1CFFF  DSP local RAM (8 KB)
$F1D000-$F1DFFF  wave-table ROM (8 × 128-entry signed-16 tables, sign-extended to 32-bit)
```
Both cores see local RAM + local regs + the full external map in one flat 32-bit
address space; **internal space is 32-bit-only** (byte/word/phrase loads/stores
to *internal* addresses degrade to 32-bit accesses — TRM p.37, repeated in every
LOADB/LOADW/STOREB/STOREW description). External space supports 8/16/32/64-bit.

**Wave-table ROM (DSP):** 8 tables at `$F1D000` + `0x200*i`: TRI, SINE, AMSINE,
SINE12W, CHIRP16, NTRI, DELTA, NOISE (TRM p.98; INC ~520). Each 128 signed-16
entries, sign-extended to 32-bit, appearing as 1K longwords.

### 12.1 The Tom DRAM-execution constraint (must enforce/emulate)

**GPU code must execute from GPU SRAM (`$F03xxx`), not from DRAM.** This is the
well-known "Tom DRAM-execution bug": the Tom GPU cannot reliably fetch/execute
instructions out of main DRAM. The internal porting notes: "GPU executes from SRAM, not DRAM (the
Tom DRAM-execution bug). Copy kernels into GPU SRAM (`$F03xxx`) at init, run from
there." A reference backend uploads the kernel into `G_SRAM=0xF03000` and only ever sets
`G_PC` into that range. The emulator should (a) run correctly when code is in
SRAM, and (b) **(UNVERIFIED policy)** decide whether to replicate the bug
(garbage when PC is in DRAM) or just warn — recommend: log a warning and still
execute, since BigPEmu's exact misbehavior here is unconfirmed. The DSP does
**not** have this restriction (it runs from its own RAM and from external memory).

---

## 13. Suggested Rust decoder skeleton

```rust
#[derive(Clone, Copy)]
pub enum Variant { Gpu, Dsp }

pub fn step(core: &mut Risc, mem: &mut Bus) {
    let iw = core.fetch_word();                 // big-endian u16 from local/ext
    let op  = (iw >> 10) & 0x3F;
    let r1  = ((iw >> 5) & 0x1F) as usize;      // reg1
    let r2  = (iw & 0x1F) as usize;             // reg2
    let b   = core.cur_bank();                  // imask ? 0 : regpage as usize
    match op {
        0  => alu_add(core, b, r1, r2, false),
        1  => alu_add(core, b, r1, r2, true /*use C*/),
        2  => alu_addq(core, b, r1, r2, /*flags=*/true),
        3  => alu_addq(core, b, r1, r2, /*flags=*/false),
        4  => alu_sub(core, b, r1, r2, false),
        5  => alu_sub(core, b, r1, r2, true),
        6  => alu_subq(core, b, r1, r2, true),
        7  => alu_subq(core, b, r1, r2, false),
        8  => alu_neg(core, b, r2),
        9  => alu_and(core, b, r1, r2),
        10 => alu_or(core, b, r1, r2),
        11 => alu_xor(core, b, r1, r2),
        12 => alu_not(core, b, r2),
        13 => bit_btst(core, b, r1, r2),
        14 => bit_bset(core, b, r1, r2),
        15 => bit_bclr(core, b, r1, r2),
        16 => mul_mult(core, b, r1, r2),
        17 => mul_imult(core, b, r1, r2),
        18 => mac_imultn(core, b, r1, r2),
        19 => mac_resmac(core, b, r2),
        20 => mac_imacn(core, b, r1, r2),
        21 => div_div(core, b, r1, r2),
        22 => alu_abs(core, b, r2),
        23 => sh_sh(core, b, r1, r2),
        24 => sh_shlq(core, b, r1, r2),         // count = 32 - r1
        25 => sh_shrq(core, b, r1, r2),
        26 => sh_sha(core, b, r1, r2),
        27 => sh_sharq(core, b, r1, r2),
        28 => sh_ror(core, b, r1, r2),
        29 => sh_rorq(core, b, r1, r2),
        30 => alu_cmp(core, b, r1, r2),
        31 => alu_cmpq(core, b, r1, r2),        // r1 signed 5-bit
        // 32,33,42,48,62,63 diverge by Variant:
        32 => match core.variant { Variant::Gpu => sat8(core,b,r2),
                                   Variant::Dsp => subqmod(core,b,r1,r2) },
        33 => match core.variant { Variant::Gpu => sat16(core,b,r2),
                                   Variant::Dsp => sat16s(core,b,r2) },
        34 => mov_move(core, b, r1, r2),
        35 => mov_moveq(core, b, r1, r2),       // imm = r1 (0..31)
        36 => mov_moveta(core, b, r1, r2),
        37 => mov_movefa(core, b, r1, r2),
        38 => mov_movei(core, b, r2, mem),      // reads next 2 words, LE
        39 => ld_loadb(core, b, r1, r2, mem),
        40 => ld_loadw(core, b, r1, r2, mem),
        41 => ld_load(core, b, r1, r2, mem),
        42 => match core.variant { Variant::Gpu => ld_loadp(core,b,r1,r2,mem),
                                   Variant::Dsp => sat32s(core,b,r2) },
        43 => ld_load_r14n(core, b, r1, r2, mem),
        44 => ld_load_r15n(core, b, r1, r2, mem),
        45 => st_storeb(core, b, r1, r2, mem),  // addr=r1, data=r2
        46 => st_storew(core, b, r1, r2, mem),
        47 => st_store(core, b, r1, r2, mem),
        48 => match core.variant { Variant::Gpu => st_storep(core,b,r1,r2,mem),
                                   Variant::Dsp => mirror(core,b,r2) },
        49 => st_store_r14n(core, b, r1, r2, mem),  // data=r1, n=r2
        50 => st_store_r15n(core, b, r1, r2, mem),
        51 => mov_movepc(core, b, r2),
        52 => ctl_jump(core, b, r1, /*cc=*/r2),     // delay slot
        53 => ctl_jr(core, b, /*offs=*/r1, /*cc=*/r2),  // delay slot
        54 => mac_mmult(core, b, r1, r2, mem),
        55 => fp_mtoi(core, b, r1, r2),
        56 => fp_normi(core, b, r1, r2),
        57 => /* NOP */ {},
        58 => ld_load_r14rn(core, b, r1, r2, mem),
        59 => ld_load_r15rn(core, b, r1, r2, mem),
        60 => st_store_r14rn(core, b, r1, r2, mem), // data=r1, off=r2
        61 => st_store_r15rn(core, b, r1, r2, mem),
        62 => match core.variant { Variant::Gpu => sat24(core,b,r2),
                                   Variant::Dsp => illegal(core, iw) },
        63 => match core.variant {
                  Variant::Gpu => if r1==0 { pack(core,b,r2) } else { unpack(core,b,r2) },
                  Variant::Dsp => addqmod(core,b,r1,r2) },
        _  => unreachable!(),
    }
}
```

**Delay slot implementation:** keep `pending_jump: Option<u32>`. `JUMP`/`JR` set
it (if condition true) to the target; **after** executing the *next* instruction,
apply it. The delay-slot instruction sees the not-yet-changed PC. Disallow MOVEI
/ MOVE PC / second jump in that slot per §9.8 (or just execute deterministically
and log).

---

## 14. Verification checklist (probe against BigPEmu)

Each item validates an **(UNVERIFIED)** claim above. Build a tiny GPU SRAM kernel
that writes results to a DRAM mailbox (a reference backend pattern) and read them back:

1. **Quick-field 32 encoding:** `addq #32,r0` with `r0=0` ⇒ expect 32. Confirms
   raw `reg1=0` ⇒ 32.
2. **SHLQ 32−n:** assemble `shlq #1,r0` (r0=1) ⇒ 2; `shlq #31,r0` ⇒ check; dump
   the raw 16-bit instruction word to confirm `reg1 == 32−n`.
3. **CMPQ sign:** `cmpq #-1,r0` with r0=0 ⇒ N set (0 − (−1) = 1 > 0 ⇒ N clear,
   actually) — design the probe to pin sign-extension direction.
4. **MOVEI word order:** `movei #$12345678,r0`; dump assembled words; confirm
   word at PC+2 = `$5678`, PC+4 = `$1234`.
5. **MOVE PC offset:** `here: move PC,r0` then store r0; compare to `here`.
6. **Indexed STORE field order (op 49/60):** assemble `store r5,(r14+3)` and
   `store r5,(r14+r6)`, dump the words, confirm reg1=data(5), reg2=index/offset.
7. **JUMP cc semantics:** loop testing each cc value against known Z/C/N states.
8. **DIV remainder sign & 16.16:** divide pairs with known remainders, read
   `G_REMAIN` in both modes.
9. **Bank override by IMASK:** set REGPAGE, raise an int, confirm the ISR sees
   bank 0.
10. **DSP op 62:** run an instruction word with opcode 62 on the DSP; observe
    whether it NOPs, faults, or aliases.

---

## Open questions (validate against BigPEmu / Virtual Jaguar / MAME `dsp.cpp`,`gpu.cpp`)

1. **(UNVERIFIED-ENC)** Exact raw-bit encoding of the "1..32" quick fields (the
   "raw 0 ⇒ 32" rule) for ADDQ/SUBQ/SHRQ/SHARQ/RORQ/index. TRM gives ranges, not
   bit patterns. Item 1/2 above pins it.
2. **(UNVERIFIED-ENC)** `SHLQ` `32−n` raw encoding — TRM states it in prose
   (p.55) but verify the assembled word.
3. **(UNVERIFIED)** Indexed-**STORE** field assignment (op 49/50 data-vs-index;
   op 60/61 data-vs-offset). A reference backend's known-good code doesn't exercise it. Item 6.
4. **(UNVERIFIED)** `MOVE PC,Rd` pipeline correction constant (how many words the
   returned value differs from the raw fetch PC). Item 5.
5. **(UNVERIFIED)** GPU MAC accumulator width / overflow visibility (DSP is 40-bit
   with `D_MACHI`; GPU MACHI not documented).
6. **(UNVERIFIED)** DSP opcode **62** behavior (no instruction documented there).
   Item 10.
7. **(UNVERIFIED)** `MMULT` element fetch order: which packed half is element 0,
   row/column stride in bytes, and exactly how `MTXADDR` increments.
8. **(UNVERIFIED-DETAIL)** 16.16 `DIV` remainder semantics and whether
   `DIV_OFFSET` is sticky vs per-op.
9. **(UNVERIFIED)** Post-reset default of `G_END`/`D_END` (`BIG_INSTR`/`BIG_PIX`/
   `BIG_IO`). Assume 0; SDK startup writes a known value.
10. **(UNVERIFIED)** Whether to faithfully reproduce the Tom DRAM-execution bug
    (garbage when GPU PC is in DRAM) or just warn. BigPEmu's exact behavior here
    is unconfirmed; recommend warn-and-execute.
11. **(UNVERIFIED)** Behavior of the documented "illegal combinations" (§9.8) on
    real silicon / BigPEmu — emulator should be deterministic; match a reference
    if a real ROM depends on it.
12. **(UNVERIFIED)** Carry-flag value after logical/MULT ops ("undefined" per TRM)
    — pick a deterministic convention (e.g. leave C unchanged) and confirm no ROM
    depends on a specific value; BigPEmu may have its own convention.

## External cross-references

- Jaguar Technical Reference Manual rev 8 (same as local TRM):
  <https://www.hillsoftware.com/files/atari/jaguar/jag_v8.pdf>
- Official SDK / equates (cubanismo mirror): <https://github.com/cubanismo/jaguar-sdk>

For the *exact* wire encodings of the items tagged (UNVERIFIED-ENC), the most
practical authority is a known-good open-source JRISC emulator core (Virtual
Jaguar `src/dsp.cpp` / `src/gpu.cpp`, or MAME's Jaguar driver) cross-checked
against a BigPEmu probe — local official docs (TRM/INC) win on any conflict of
*semantics*, but they do not always spell out the raw bit pattern.
