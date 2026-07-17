# Jerry — Timers, Interrupt Routing, Audio Output Path, Joypad

Implementation-grade specification for the Atari Jaguar **Jerry** chip's
non-DSP peripherals: the two programmable timers, the Jerry→Tom→68k interrupt
routing, the I²S/PWM audio output path, the built-in wavetable ROM, and the
joypad/controller interface.

The DSP RISC core itself (instruction set, register file, pipeline) is covered
in `RISC_ISA.md`. This document covers everything *around* the DSP that lives in
the Jerry address space (`$F10000`–`$F1FFFF`), plus the joystick decode logic
(`$F14000`).

**Endianness:** the Jaguar is **big-endian**. All Jerry/DSP registers are
physically 16-bit. Reads/writes of 32-bit ("long") quantities to the I²S/DAC and
DSP control registers are performed high-word-first (lower address = more
significant word). The Tech Reference is explicit that the addresses given for
I²S/DSP registers are "a big-endian view of their position in the memory map"
(Tech Ref p.91). Word order matters most for the 32-bit DSP control registers
(`$F1Axxx`) and the DAC/I²S registers.

**Source legend:**
- **[INC]** = `Atari Jaguar SDK/.../INCLUDE/JAGUAR.INC` (authoritative equates), line cited.
- **[TR p.N]** = *Jaguar Technical Reference Manual Rev 8*, PDF page N.
- **[reference backend]** = a proven reference homebrew backend, verified on BigPEmu + hardware.
- **the internal porting notes** = internal Jaguar porting notes.
- **(UNVERIFIED)** marks inference not directly stated in an official source; see *Open Questions*.

---

## 0. System clock — the time base for everything below

| Quantity | Value | Source |
|---|---|---|
| Jaguar master / system clock (Tom & Jerry "processor clock") | **26.591 MHz** (NTSC), ~26.594 MHz (PAL, near-identical) | [WebSearch: AtariAge/Wikipedia FAQ] |
| Nominal period | **~37.61 ns** per system-clock tick | derived |
| 68000 clock | system clock / 2 = **13.295 MHz** | [WebSearch] |

The timers, I²S serial clock, PWM DAC pulse rate, and UART baud generator are all
derived from this single 26.591 MHz "processor clock." The Tech Reference calls
it "the processor clock" / "System Clock Frequency" throughout [TR p.85, p.91].

> (UNVERIFIED, low risk) The exact NTSC value is commonly quoted as 26.590906 MHz
> (chroma-locked); PAL ~26.593900 MHz. The manual never prints a numeric clock
> rate. For timer-period math any value in 26.59–26.60 MHz is within audio
> tolerance; pick **26_590_906 Hz (NTSC) / 26_593_900 Hz (PAL)** as constants and
> let the emulator advance timers off the same tick counter the rest of the
> machine uses. Validate exact sample rates against BigPEmu if cycle-exact audio
> timing is ever needed.

---

## 1. Programmable Timers (JPIT1–JPIT4)

### 1.1 Register map

Jerry contains **two identical timers**, each a two-stage cascade of 16-bit
down-counters: a *pre-scaler* (first stage) feeding a *divider* (second stage).
Write and read addresses **differ** [TR p.85].

| Name | Write addr | Read addr | Width | Role | Source |
|---|---|---|---|---|---|
| `JPIT1` | `$F10000` | `$F10036` | 16 | Timer 1 pre-scaler (N) | [INC:428][TR p.85] |
| `JPIT2` | `$F10002` | `$F10038` | 16 | Timer 1 divider (M) | [INC:429][TR p.86] |
| `JPIT3` | `$F10004` | `$F1003A` | 16 | Timer 2 pre-scaler (N) | [INC:430][TR p.85] |
| `JPIT4` | `$F10006` | `$F1003C` | 16 | Timer 2 divider (M) | [INC:431][TR p.86] |

> Note: [TR p.85] has a typo printing `JPIT4 ... F10002 WO` for the *write*
> address; the authoritative [INC:431] gives `JPIT4 = BASE+$10006`. **Use
> `$F10006` write / `$F1003C` read for JPIT4.** The read-address column in the
> manual (`$F1003C`) is internally consistent and confirms this.

All four are accessed as **16-bit** registers. The pre-scaler holds N, the
divider holds M, where N and M are the raw 16-bit values written.

### 1.2 Period / fire equation (VERIFIED)

> "The first stage (loosely called the pre-scaler) divides the processor clock by
> **N + 1**. The second stage divides this frequency by **M + 1** ... It is
> therefore possible to achieve frequency division in the range four to four
> billion." [TR p.85]

So the timer's interrupt frequency is:

```
f_timer = f_sysclk / ((N + 1) * (M + 1))
```

and the period in system-clock ticks between interrupts is:

```
ticks_per_fire = (N + 1) * (M + 1)
```

- Minimum division = 4 (N=1, M=1 → 2*2) ... range up to ~4.29e9 (both =$FFFF →
  65536*65536) [TR p.85].
- **Timer 1** is intended for the **audio sample rate** (drives the PWM DACs)
  [TR p.85, INC comment "sample rate"]. **Timer 2** is intended for **music
  tempo** [TR p.85].

### 1.3 Counter behavior (VERIFIED)

- Both stages are **down-counters** loaded when their register is **written** and
  reloaded automatically **when they reach zero** [TR p.85–86].
- Writing a register **presets** the counter (so timers can act as one-shot
  programmable delays) [TR p.85].
- Registers are **readable** (read address column) — used to measure elapsed time
  ("to profile code or measure time between joystick events") [TR p.85].
- The pre-scaler output of **Timer 1** also drives the PWM DAC pulse generator;
  if the PWM DACs are used, JPIT1 must divide by ≥130 to guarantee a return to
  zero (pulses are 1–129 system-clock cycles wide) [TR p.86, p.88].

**Emulator model (recommended, two-stage decrement):**

```
struct Timer {
    n_reload: u16,   // last value written to pre-scaler (JPIT1/3)
    m_reload: u16,   // last value written to divider     (JPIT2/4)
    pre: u32,        // current pre-scaler count
    div: u32,        // current divider count
}
// On write to pre-scaler reg: n_reload = val; pre = val as u32 + 1; (preset)
// On write to divider   reg: m_reload = val; div = val as u32 + 1;
// Tick once per system-clock cycle (or batch: subtract elapsed ticks):
fn tick(&mut self) -> bool { // returns true on a "fire" (div reached 0)
    if self.pre == 0 { self.pre = self.n_reload as u32 + 1; }
    self.pre -= 1;
    if self.pre == 0 {
        self.pre = self.n_reload as u32 + 1;   // reload pre-scaler
        if self.div == 0 { self.div = self.m_reload as u32 + 1; }
        self.div -= 1;
        if self.div == 0 {
            self.div = self.m_reload as u32 + 1; // reload divider
            return true;                          // INTERRUPT
        }
    }
    false
}
```

For performance, batch by computing `ticks_per_fire = (N+1)*(M+1)` and a single
modular phase accumulator instead of decrementing per cycle — both produce the
same fire cadence. The per-cycle model above is the cycle-accurate reference.

**Read-back value (UNVERIFIED detail):** the manual says the counters are
readable "really for chip test purposes" [TR p.86]. It does **not** specify
whether the read returns the live pre-scaler count, the live divider count, or
the last written value. A safe v1 stub returns the **current divider count**
(`div - 1`, i.e. counting toward zero). Most games only read these to measure
elapsed time; confirm exact read semantics against BigPEmu if a game depends on
it. *See Open Questions.*

### 1.4 Timer interrupt enable / clear — Jerry `J_INT` (`$F10020`)

When a timer's divider reaches zero it raises a **maskable** interrupt that can be
routed to **either the DSP or the 68k CPU**, independently [TR p.86]. The CPU
path is gated through Jerry's interrupt control register `J_INT`.

`J_INT` = `$F10020`, R/W, 16-bit [INC:433][TR p.86]. Bit layout
([INC:451–463], [TR p.86–87]):

| Bit | Constant | Meaning (enables source / pending when read) |
|---|---|---|
| 0 | `J_EXTENA` ($0001) | Enable external interrupt |
| 1 | `J_DSPENA` ($0002) | Enable DSP interrupt |
| 2 | `J_TIM1ENA` ($0004) | **Enable Timer 1** (sample rate) |
| 3 | `J_TIM2ENA` ($0008) | **Enable Timer 2** (tempo) |
| 4 | `J_ASYNENA` ($0010) | Enable asynchronous serial (UART) interrupt |
| 5 | `J_SYNENA` ($0020) | Enable synchronous serial (I²S) interrupt |
| 8 | `J_EXTCLR` ($0100) | Clear pending external |
| 9 | `J_DSPCLR` ($0200) | Clear pending DSP |
| 10 | `J_TIM1CLR` ($0400) | **Clear pending Timer 1** |
| 11 | `J_TIM2CLR` ($0800) | **Clear pending Timer 2** |
| 12 | `J_ASYNCLR` ($1000) | Clear pending async serial |
| 13 | `J_SYNCLR` ($2000) | Clear pending sync serial (I²S) |

Semantics (VERIFIED [TR p.87]):
- **Bits 0–5** are the per-source **enables**.
- **Reading bits 0–5** returns the **pending** status for each source (which
  interrupts are currently asserted/latched). *Note the dual use: same bit
  positions are "enable" on write and "pending" on read.*
- **Writing a 1 to bits 8–13** **clears** the pending latch for the
  corresponding source. Writing 0 leaves it unchanged.

**Emulator model for `J_INT`:**
- Keep `j_int_enable: u16` (bits 0–5) and `j_int_pending: u16` (bits 0–5).
- On write: `j_int_enable = value & 0x003F;` then for each set clear bit
  `(value >> 8) & 0x3F`, clear the matching `j_int_pending` bit.
- On read: return `(j_int_enable & 0x003F) | (j_int_pending & 0x003F)` — i.e. the
  ISR reads pending in the low 6 bits. (Enables and pending share bit positions;
  hardware OR is the simplest faithful model. *UNVERIFIED whether read returns
  enables OR'd with pending or pending-only; treat low 6 bits as pending for ISR
  dispatch — that is what software relies on.* See Open Questions.)
- When **any** enabled Jerry source is pending → assert the single Jerry→Tom line
  (see §2).

---

## 2. Interrupt routing — Jerry → Tom → 68000, and to the DSP

The Jaguar has **two** independent interrupt aggregation points:
1. **Tom's `INT1`** (`$F000E0`) — the only thing the **68000** ever sees.
2. **DSP flags** (`D_FLAGS` `$F1A100`) — what the **DSP RISC core** sees.

Jerry's own `J_INT` ($F10020, §1.4) sits *upstream* of both for its peripheral
sources.

### 2.1 The 68000 view — single Level-2 autovector

**Critical fact:** all five Tom `INT1` sources are OR-ed into **one** 68000
interrupt request, delivered as **autovector Level 2**, i.e. 68k exception
**vector 64 = address `$100`** [reference backend]. There are no
separate 68k vectors per source — the ISR reads `INT1` to discover the cause.

```
68k vector 64  @  $00000100   <- ALL Jaguar interrupts land here (autovector L2)
```
[reference backend]

> (UNVERIFIED, high confidence) The autovector level is **2** (IPL2). Confirmed by
> community docs and the SDK convention `LEVEL0 = $100`. The 68k samples IPL lines;
> Jaguar wires the combined Tom interrupt to IPL level 2 so it vectors through
> autovector `$68`+... no — through the **autovector for level 2 = $00000068**?
> **NO.** The Jaguar uses the level-encoded autovector. Empirically the install
> point in proven code is **`$100`** (vector 64), which corresponds to the way
> Atari's tools relocate the vector. **Use `$100` as the dispatch address — that
> is what shipping code installs and what BigPEmu honors** [reference backend]. Validate the precise 68k IPL level against BigPEmu only if you
> implement true 68k autovector decoding; for emulation just call the handler at
> the long stored in `$100` when an enabled INT1 source fires. See Open Questions.

### 2.2 Tom `INT1` — CPU Interrupt Control Register (`$F000E0`)

`INT1` = `$F000E0`, R/W, 16-bit [INC:81][TR p.16]. `INT2` = `$F000E2`, WO
(resume register) [INC:82][TR p.17].

The **five** sources [TR p.16, INC:35–45]:

| Bit (enable) | Bit (clear) | Constant pair | Source |
|---|---|---|---|
| 0 | 8 | `C_VIDENA`/`C_VIDCLR` ($0001/$0100) | **Video** time-base (line = `VI` register) |
| 1 | 9 | `C_GPUENA`/`C_GPUCLR` ($0002/$0200) | **GPU** register-write interrupt |
| 2 | 10 | `C_OPENA`/`C_OPCLR` ($0004/$0400) | **Object Processor** stop-object |
| 3 | 11 | `C_PITENA`/`C_PITCLR` ($0008/$0800) | **Timer** — the *PIT in Tom* (`PIT0/PIT1`, **not** Jerry's timers) |
| 4 | 12 | `C_JERENA`/`C_JERCLR` ($0010/$1000) | **Jerry** — combined Jerry interrupt input to Tom |

Semantics [TR p.16]:
- Bits 0–4 enable; **reading** bits 0–4 gives **pending** status.
- Writing 1 to bits 8–12 clears the corresponding pending latch.
- **Bit 4 (Jerry) is "active-high edge-triggered" — the first interrupt occurs on
  the first rising edge after it is enabled** [TR p.16]. This matters: the Jerry
  line into Tom is edge-detected, so the emulator should latch a Jerry→Tom
  *rising edge* (transition from "no enabled Jerry source pending" → "some enabled
  Jerry source pending") into INT1 bit 4's pending latch.

> **Naming caution:** Tom INT1 **bit 3 = "Timer" is Tom's own PIT** (`PIT0`/`PIT1`
> at `$F00050/$F00052`), a *separate* timer from Jerry's JPIT1–4. Jerry's timer
> interrupts reach the 68k **via INT1 bit 4 (Jerry)**, after passing through
> `J_INT` bits 2/3. Do not conflate `C_PITENA` (Tom PIT) with `J_TIM1ENA`
> (Jerry timer 1).

### 2.3 The full Jerry→68k chain (timer example)

For a Jerry **Timer 1** interrupt to reach the 68000:

```
Timer1.divider == 0
   │  (raises Jerry Timer-1 source)
   ▼
J_INT bit 2 (J_TIM1ENA) enabled?  ──no──> dropped
   │ yes -> set J_INT pending bit 2
   ▼
Any enabled Jerry source pending  ==> assert Jerry-to-Tom line (rising edge)
   ▼
Tom INT1 bit 4 (C_JERENA) enabled?  ──no──> dropped
   │ yes -> latch INT1 pending bit 4 (edge-triggered)
   ▼
68k IPL -> Level 2 autovector -> jump through $100
   ▼
ISR reads INT1 (cause=bit4 Jerry), reads J_INT (low bits, cause=bit2 Timer1),
services it, writes J_INT |= J_TIM1CLR ($0400) to clear Jerry latch,
writes INT1 with C_JERCLR ($1000)+C_JERENA ($0010) to clear+re-enable,
writes INT2 ($F000E2) any value to resume Blitter/GPU bus priority,
RTE.
```

**Required ISR epilogue [reference backend]:**
- Write `INT1` with the source's clear bit **plus** re-enable bits (e.g. video
  ISR writes `$0101` = clear video pending + keep video enabled [reference backend]).
- **Always write `INT2` (`$F000E2`) any value at the end of every ISR** — this
  restores GPU/Blitter bus priority that Tom lowered on interrupt entry [TR p.17].
  Omitting it stalls the Blitter/GPU. The emulator must model: on 68k interrupt
  acknowledge, lower GPU/Blitter bus priority; on `INT2` write, restore it.

### 2.4 Priority / encoding the 68k sees

There is **one** 68k interrupt level (Level 2) for all sources. "Priority" among
the five INT1 sources is **software-resolved** inside the single ISR by testing
INT1's pending bits in whatever order the handler chooses. Hardware does not rank
them at the 68k level [TR p.16 — bits simply OR into one request].

(For completeness, the *bus* priority list — unrelated to interrupt vectoring —
is, highest first: daisy-chain master, DSP@DMA, GPU@DMA, Blitter@high, CPU,
DSP@normal, ..., GPU@normal, Blitter@normal [TR p.13].)

### 2.5 The DSP view — `D_FLAGS` / `D_CTRL`

The DSP RISC core is interrupted **independently** of the 68k. The DSP shares the
GPU's interrupt model: **an interrupt forces a call to local-RAM address
`16 * interrupt_number` (bytes) from the base of DSP RAM** (`D_RAM = $F1B000`)
[TR p.38].

So DSP interrupt vectors are at:

```
DSP_RAM_BASE ($F1B000) + 16 * irq_number
```

**DSP interrupt source numbering [TR p.98]** (the DSP has *six* sources, distinct
from the GPU's five):

| IRQ # | DSP RAM vector | Source |
|---|---|---|
| 0 | `$F1B000` | **CPU** interrupt (any processor writing the DSP control reg) |
| 1 | `$F1B010` | **I²S** interface interrupt (synchronous serial / sample clock) |
| 2 | `$F1B020` | **Timer 0** (Jerry Timer 1 in JPIT terms) |
| 3 | `$F1B030` | **Timer 1** (Jerry Timer 2 in JPIT terms) |
| 4 | `$F1B040` | **External interrupt 0** |
| 5 | `$F1B050` | **External interrupt 1** |

> **Numbering caveat:** [TR p.98] lists DSP "Timer interrupt 1 = #3, Timer
> interrupt 0 = #2". Map: DSP IRQ2 ← Jerry **Timer 1** (JPIT1/2), DSP IRQ3 ←
> Jerry **Timer 2** (JPIT3/4). This is *not* the same as the GPU's table (GPU
> IRQ2 = Timing generator). Keep DSP and GPU interrupt tables separate.

**`D_FLAGS` (`$F1A100`, 32-bit) — DSP enable + latch bits [INC:527, 544–556]:**

| Constant | Value | Meaning |
|---|---|---|
| `D_CPUENA` | $00000010 | Enable CPU interrupt |
| `D_I2SENA` | $00000020 | Enable I²S interrupt |
| `D_TIM1ENA` | $00000040 | Enable Timer 1 interrupt |
| `D_TIM2ENA` | $00000080 | Enable Timer 2 interrupt |
| `D_EXT0ENA` | $00000100 | Enable external interrupt 0 |
| `D_EXT1ENA` | $00010000 | Enable external interrupt 1 |
| `D_CPUCLR` | $00000200 | Clear CPU interrupt latch |
| `D_I2SCLR` | $00000400 | Clear I²S latch |
| `D_TIM1CLR` | $00000800 | Clear Timer 1 latch |
| `D_TIM2CLR` | $00001000 | Clear Timer 2 latch |
| `D_EXT0CLR` | $00002000 | Clear external 0 latch |
| `D_EXT1CLR` | $00020000 | Clear external 1 latch |

Also in `D_FLAGS` (shared GPU/DSP RISC flags, [RISC_ISA.md]): bit 3 = **IMASK**
(master interrupt mask, set on interrupt entry) [TR p.38]. On DSP interrupt entry
the master mask is set, the last-instruction address is pushed to the R31 stack,
and execution jumps to the RAM vector. ISR must `bclr 3` (clear IMASK), `bset`
the source's latch-clear bit, `addq 2` to the saved return address, and `jump`
[TR p.38].

**`D_CTRL` (`$F1A114`, 32-bit) — DSP control/status [INC:532, 562–570]:**

| Constant | Value | Meaning |
|---|---|---|
| `DSPGO` | $00000001 | Start DSP (write 1 to run) |
| `DSPINT0` | $00000004 | Generate a DSP **CPU-interrupt** (IRQ0) — how the 68k/GPU pokes the DSP |
| `D_CPULAT` | $00000040 | CPU interrupt latch (status, read) |
| `D_I2SLAT` | $00000080 | I²S latch |
| `D_TIM1LAT` | $00000100 | Timer 1 latch |
| `D_TIM2LAT` | $00000200 | Timer 2 latch |
| `D_EXT1LAT` | $00000400 | External 1 latch |
| `D_EXT2LAT` | $00010000 | External 2 latch |

The DSP control-register write with `DSPINT0` is how *another* bus master sends
the DSP its IRQ0; the DSP sends the 68k an interrupt via `J_INT` bit 1
(`J_DSPENA`) → Tom INT1 bit 4 (Jerry). (The DSP raising a 68k interrupt is the
"DSP may generate an interrupt by writing to a port" path [TR p.86].)

---

## 3. Audio output path

Two distinct DACs exist; **both are driven by Jerry's timer/serial clocks**:
1. **PWM DACs (`DAC1`/`DAC2`)** — the actual Jaguar console audio path
   (14-bit pulse-width-modulated stereo, integrated by external RC). Driven by
   **Timer 1 pre-scaler**.
2. **I²S synchronous serial interface (`L_I2S`/`R_I2S`)** — a digital serial
   audio port (the "SSI"/I²S) with its own clock (`SCLK`) and mode (`SMODE`).
   Used to clock samples out at the audio sample rate and to raise the **I²S
   interrupt** that the DSP services every sample.

For a Jaguar console, the PWM DACs are the physical output; the I²S interface is
the timing/clock spine that most sound engines hang their per-sample ISR on. The
canonical loop is: **I²S interrupt fires at sample rate → DSP computes L/R sample
→ DSP writes `L_I2S`/`R_I2S` (or `DAC1`/`DAC2`)** [TR p.88, p.97].

### 3.1 PWM DAC registers

| Name | Addr | Width | Source |
|---|---|---|---|
| `DAC1` (Left) | `$F1A140` | 14-bit (in 16-bit reg) | [TR p.89] |
| `DAC2` (Right) | `$F1A144` | 14-bit (in 16-bit reg) | [TR p.89] |

- Two's complement, reset to 0, **only the most-significant 14 bits used**
  [TR p.88].
- **All transfers must be 32-bit** even though the register is 16-bit [TR p.88].
- Double-buffered: a new sample written before the next pulse period is latched
  at the period boundary [TR p.88].
- The PWM mechanism **does not start until Timer 1 is programmed** [TR p.88].
- Pulse rate = Timer 1 pre-scaler frequency, up to ~240 kHz; pulses 1–129 sysclk
  wide → pre-scaler must divide ≥130 [TR p.88].
- Startup: write values ramping from 8000→0 at sample rate to avoid a power-on
  click [TR p.88].

### 3.2 I²S / Synchronous Serial Interface (SSI)

| Name | Addr | Width | Dir | Source |
|---|---|---|---|---|
| `L_I2S` / `LTXD` (Left transmit) | `$F1A148` | 16 | WO | [INC:444][TR p.92] |
| `R_I2S` / `RTXD` (Right transmit) | `$F1A14C` | 16 | WO | [INC:445][TR p.92] |
| `LRXD` (Left receive) | `$F1A148` | 16 | RO | [TR p.92] |
| `RRXD` (Right receive) | `$F1A14C` | 16 | RO | [TR p.92] |
| `SCLK` (Serial clock freq) | `$F1A150` | 8 | WO | [INC:441][TR p.91] |
| `SSTAT` (Serial status) | `$F1A150` | 16 | RO | [TR p.92] |
| `SMODE` (Serial mode) | `$F1A154` | 6 | WO | [INC:442][TR p.91] |

All accessed as 32-bit transfers though physically ≤16-bit [TR p.91].

**`SCLK` — serial clock frequency [TR p.91]:**
```
SerialClock = SystemClock / (2 * (N + 1))     where N = value written to SCLK (8-bit)
```

**`SMODE` bit layout [TR p.91–92]:**

| Bit | Name | Meaning |
|---|---|---|
| 0 | `INTERNAL` | Enable serial clock + word-strobe outputs (Jerry = master) |
| 1 | `MODE` | 0 = mode16 (I²S, 16-bit words); 1 = mode32 (32-bit packets) |
| 2 | `WSEN` | Enable word-strobe generation (high 16 SCLKs / low 16 SCLKs) |
| 3 | `RISING` | Enable interrupt on **rising** edge of word strobe |
| 4 | `FALLING` | Enable interrupt on **falling** edge of word strobe |
| 5 | `EVERYWORD` | Enable interrupt on MSB of every word transmitted/received |

**`SSTAT` (read at `$F1A150`) [TR p.92]:**
- Bit 0 `WS` — current state of the Word Strobe pin (which channel). *The manual
  says do NOT use this to read input data — read the interrupt control register
  instead.*
- Bit 1 `Left` — in mode32, internal counter's current word (L/R).

**Mode16 (I²S) timing [TR p.90–91] (VERIFIED):**
- 16-bit word length, MSB first.
- Word strobe transitions mark left/right word boundaries; strobe **precedes data
  by one bit**.
- A complete L+R frame = 32 SCLK cycles (16 left + 16 right), with one word strobe
  rising edge per frame in mode16.
- **Interrupt is generated on the rising edge of word strobe** (when `RISING`
  enabled) — this is the per-sample-frame I²S interrupt.
- `R_I2S`/`RTXD` is loaded into the shift register after the **rising** edge of
  word strobe; `L_I2S`/`LTXD` after the **falling** edge [TR p.92].

**Sample-clock model — the timing-accurate part the v1 emulator MUST get right:**

The I²S sample rate (frame rate) is:
```
f_sample = SerialClock / 32          (mode16: 32 SCLKs per L+R frame)
         = SystemClock / (2*(N+1)) / 32
```
and the I²S interrupt cadence in system-clock ticks is:
```
i2s_ticks_per_frame = 64 * (N + 1)   (mode16; from f_sample above)
```
> (UNVERIFIED arithmetic) The `*32` frame length and hence `64*(N+1)` follow from
> "16 left + 16 right" and `SCLK = sysclk/(2*(N+1))`. Confirm against BigPEmu that
> the I²S interrupt fires once per 32-SCLK frame in mode16 with `RISING` set, and
> whether `EVERYWORD` doubles it. *See Open Questions.*

**v1 audio stub (do this, even with no sound output):**
A v1 emulator may output **silence** but **must** advance the I²S sample clock and
raise the I²S interrupt with correct cadence, because sound engines time their DSP
ISR (and often game logic frame pacing / RNG / DSP→68k signaling) off it:

1. When `SMODE` bit0 (`INTERNAL`) is set and bit2 (`WSEN`) is set, start the I²S
   clock. Compute `i2s_ticks_per_frame` from `SCLK` (above).
2. Every `i2s_ticks_per_frame` system-clock ticks, generate the I²S interrupt:
   - Set DSP I²S latch (`D_I2SLAT` in `D_CTRL`); if `D_I2SENA` set in `D_FLAGS`,
     deliver DSP IRQ1 (vector `$F1B010`).
   - Set Jerry `J_INT` pending bit 5; if `J_SYNENA` set, raise Jerry→Tom; if Tom
     `C_JERENA` set, latch INT1 bit 4 → 68k IRQ.
3. On each I²S frame, **consume** `L_I2S`/`R_I2S` (read current values into the
   "shift register," i.e. into the host audio buffer if you output sound; discard
   if silent). Mark the TX registers as "transferred" so software's "only update
   when previous contents transferred" handshake works.
4. `L_I2S`/`R_I2S` writes from the DSP just store into the TX latch; no external
   bus cost is modeled for DSP-local writes.

This keeps audio-driven timing (sample ISR rate, DSP load, any
sample-counter-based game logic) correct even with zero audio output. Real audio
output is then just "tap the L/R values at frame time into a host ring buffer."

### 3.3 Wave Table ROM (`ROM_TABLE` `$F1D000`)

Jerry has a built-in **2 KB wavetable ROM** at `$F1D000`–`$F1DFFF` [TR p.98,
INC:512–521]. **Eight tables, 128 entries each, signed 16-bit, sign-extended to
32-bit** so the ROM appears as **1K × 32-bit locations** (only low 16 bits
significant) [TR p.98].

Layout — each table occupies `128 entries * 4 bytes = $200` bytes:

| Addr | Name | Description | Source |
|---|---|---|---|
| `$F1D000` | `ROM_TRI` | Triangle wave | [INC:514][TR p.98] |
| `$F1D200` | `ROM_SINE` | Full-amplitude sine | [INC:515][TR p.98] |
| `$F1D400` | `ROM_AMSINE` | Amplitude-modulated sine | [INC:516][TR p.98] |
| `$F1D600` | `ROM_12W` (`SINE12W`) | sine(x)+sine(2x) (2nd harmonic) | [INC:517][TR p.98] |
| `$F1D800` | `ROM_CHIRP16` | Chirp (rising-frequency sine) | [INC:518][TR p.98] |
| `$F1DA00` | `ROM_NTRI` | Triangle + noise | [INC:519][TR p.98] |
| `$F1DC00` | `ROM_DELTA` | Positive spike / delta | [INC:520][TR p.98] |
| `$F1DE00` | `ROM_NOISE` | White noise | [INC:521][TR p.98] |

**Emulator model:** a read-only 4 KB region. Each 32-bit long at
`$F1D000 + table*0x200 + i*4` returns the sign-extended 16-bit sample
`(i32)(sample16)` (high 16 bits = sign extension), big-endian. The DSP reads these
as 32-bit longs (low 16 = sample).

> (UNVERIFIED) The **exact sample values** of each table are not printed in the
> manual. For correctness of any game that copies/plays these tables, the ROM
> contents must match real Jerry. v1: synthesize TRI/SINE analytically (good
> enough for most), but the noise/chirp/delta tables are fixed hardware patterns.
> **Best path: dump `$F1D000–$F1DFFF` from BigPEmu** (or a real console) and embed
> the 4 KB blob. Until then, generate TRI = linear ramp ±, SINE = round(sin),
> mark the rest as approximations. *See Open Questions.*

---

## 4. Joypad / Controller Interface

### 4.1 Registers

| Name | Addr | Width | Source |
|---|---|---|---|
| `JOYSTICK` / `JOY1` | `$F14000` | 16 R/W | [INC:435][TR p.95] |
| `JOYBUTS` / `JOY2` / `CONFIG` | `$F14002` | 16 R/W | [INC:436–437][TR p.95] |

A **32-bit read of `$F14000`** returns `JOYSTICK` in the **high word** and
`JOYBUTS` in the **low word** [reference backend]. This combined longword is what the
button-bit numbering below refers to.

**Write semantics (the strobe/column select) [TR p.95]:**
- Writing `JOYSTICK` latches the low 8 data bits into the joystick **output
  latch** (column/row strobe select). Bit 15 enables the joystick outputs
  (drives JOY0–JOY7 as outputs); cleared by reset.
- Reading `JOYSTICK` enables the input buffers and returns the 16 joystick input
  lines (active-low). Reading `JOYBUTS` returns the 4 button inputs (active-low).

The controller is a **4-row × 4-column matrix** plus 2 fire-button lines. You
select a row by writing a strobe value to `JOYSTICK`, then read back the column
data. Four strobes cover the whole pad.

### 4.2 Button bit layout (LONGword) — [INC:469–504]

`JAGUAR.INC` documents the 32-bit format (from `JOYTEST/JT_LOOP.S`):

```
Format:  xxApxxBx RLDU147* xxCxxxox 2580369#
bit:     31......24 23....16 15.....8 7......0
```

Exact bit numbers [INC:473–497]:

| Bit | Signal | Bit | Signal | Bit | Signal | Bit | Signal |
|---|---|---|---|---|---|---|---|
| 29 | `FIRE_A` | 23 | `JOY_RIGHT` | 13 | `FIRE_C` | 7 | `KEY_2` |
| 28 | `PAUSE` | 22 | `JOY_LEFT` | 9 | `OPTION` | 6 | `KEY_5` |
| 25 | `FIRE_B` | 21 | `JOY_DOWN` | 19 | `KEY_1` | 5 | `KEY_8` |
| | | 20 | `JOY_UP` | 18 | `KEY_4` | 4 | `KEY_0` |
| 16 | `KEY_STAR` (*) | | | 17 | `KEY_7` | 3 | `KEY_3` |
| | | | | | | 2 | `KEY_6` |
| | | | | | | 1 | `KEY_9` |
| | | | | | | 0 | `KEY_HASH` (#) |

Group masks [INC:499–504]:
- `ANY_JOY = $00F00000` (D-pad: bits 20–23)
- `ANY_FIRE = $32002200` (A/B/C/Option/Pause)
- `ANY_KEY = $000F00FF` (keypad 0–9,*,#)

### 4.3 The 4-strobe matrix scan — VERIFIED against a reference backend

[reference backend] is the proven Jaguar Doom lineage scan (verified on BigPEmu +
hardware). The header comment is the authoritative behavioral spec:

> "Row data arrives **active-low** in JOYSTICK bits 11:8 (longword bits 27:24),
> fire buttons in JOYBUTS bits 1:0. Mask `$F0FFFFFC` passes exactly those; each
> strobe's nibble is then rotated to its own bit range and AND-accumulated. After
> inversion (active high):"

| Write to `$F14000` | rot of 32-bit read | Yields (active-high) | Source |
|---|---|---|---|
| `$81FE` | `ror 4` | bits 23:20 = R,L,D,U; bit 29 = A, bit 28 = Pause | [reference backend] |
| `$81FD` | `ror 8` | bits 19:16 = 7,4,1,*; bit 25 = B | [reference backend] |
| `$81FB` | `rol 12` (`ror 20`) | bits 7:4 = 2,5,8,0; bit 13 = C | [reference backend] |
| `$81F7` | `rol 8` (`ror 24`) | bits 3:0 = 3,6,9,#; bit 9 = Option | [reference backend] |

The scan algorithm [reference backend]:
```
acc = 0xFFFFFFFF
for each (strobe, rot):
    JOYSTICK = strobe                 // write column-select
    v = read32($F14000) | 0xF0FFFFFC  // force don't-care bits to 1
    acc &= ror32(v, rot)
raw = ~acc                            // active high; pressed bit = 1
```

**Strobe encoding:** the low byte of the strobe (`$FE/$FD/$FB/$F7`) is the
**active-low column select** — exactly one of bits 0–3 low selects one of the 4
matrix rows. `$81xx`: bit 15 set = enable outputs, bit 8 set (`$01`) keeps the
upper row line high. So:

| Strobe low byte | Active-low column (bit cleared) | Selected pad rows |
|---|---|---|
| `$FE` (1111_1110) | col 0 | R,L,D,U + A,Pause |
| `$FD` (1111_1101) | col 1 | 7,4,1,* + B |
| `$FB` (1111_1011) | col 2 | 2,5,8,0 + C |
| `$F7` (1111_0111) | col 3 | 3,6,9,# + Option |

### 4.4 What the emulator must implement (read multiplexing)

When a game **writes** `$F14000` (16-bit) the low byte selects which matrix column
is active. When it then **reads** `$F14000` (16-bit `JOYSTICK`) / `$F14002`
(`JOYBUTS`), the emulator must return the **active-low** state of the buttons in
the selected column.

For a 32-bit read of `$F14000` (high=JOYSTICK, low=JOYBUTS), given the last
strobe written to `$F14000`:

- **Row/direction bits** appear in JOYSTICK bits **11:8** (= longword bits
  **27:24**), **active-low** (0 = pressed).
- **Fire bits** appear in JOYBUTS bits **1:0** (active-low).
- All other bits read as **1** (pull-ups), which is why the mask `$F0FFFFFC` ORs
  them to 1 [reference backend].

**Recommended emulator model.** Keep injected button state as a 32-bit
"pressed = 1" word using the [INC] bit numbers (§4.2). On a read of `$F14000`,
look at the last-written strobe low byte to pick the active column, then place
that column's 4 direction bits into JOYSTICK[11:8] and the column's fire/extra
bit into the correct JOYBUTS/JOYSTICK bit, **active-low (pressed → 0)**, leaving
all other bits = 1. Concretely, the inverse of the scan table:

```
// pressed: bit set = button down, using INC bit numbers (FIRE_A=29, JOY_UP=20, ...)
fn read_joy32(pressed: u32, last_strobe_lo: u8) -> u32 {
    let mut out = 0xFFFFFFFFu32;             // all released (pull-ups)
    let put = |out: &mut u32, down: bool, bit: u32| {
        if down { *out &= !(1u32 << bit); }  // active-low: pressed -> clear bit
    };
    match last_strobe_lo {
        0xFE => { // col0: R,L,D,U in 27:24 ; A in (read pos), Pause
            put(&mut out, pressed & (1<<23) != 0, 27); // RIGHT -> bit27 (rol of 23..)
            put(&mut out, pressed & (1<<22) != 0, 26); // LEFT
            put(&mut out, pressed & (1<<21) != 0, 25); // DOWN
            put(&mut out, pressed & (1<<20) != 0, 24); // UP
            put(&mut out, pressed & (1<<29) != 0, 1);  // A   -> JOYBUTS bit1
            put(&mut out, pressed & (1<<28) != 0, 0);  // Pause-> JOYBUTS bit0
        }
        0xFD => { /* col1: 7,4,1,* in 27:24 ; B in JOYBUTS */ }
        0xFB => { /* col2: 2,5,8,0 in 27:24 ; C in JOYBUTS */ }
        0xF7 => { /* col3: 3,6,9,# in 27:24 ; Option in JOYBUTS */ }
        _ => {}
    }
    out
}
```

> The exact in-register bit position for each direction within JOYSTICK[11:8] and
> each fire within JOYBUTS[1:0] is derivable by **inverting** the rotation table
> in §4.3 (it is the position the scan rotates *from*). The cleanest correct
> implementation is: build the full active-high `raw` longword from injected
> state using §4.2 bit numbers, invert to active-low, then for the selected
> strobe write the four row bits into JOYSTICK[11:8] and the strobe's fire bit
> into its JOYBUTS position — i.e. run the §4.3 mapping **backwards**. Validate
> by feeding the emulator's `$F14000` through the real reference joypad scan and
> checking the recovered `PAD_*` flags round-trip. *(This round-trip test is the
> single best joypad correctness check; see Open Questions for the per-bit
> position confirmation.)*

### 4.5 NTSC/PAL flag in CONFIG (`$F14002`)

`CONFIG` aliases `JOYBUTS` at `$F14002` [INC:437]. The PAL/NTSC flag:

```
VIDTYPE = $10        ; bit 4
NTSC = bit set (1), PAL = bit clear (0)
```
[INC:142–145]: "This mask will extract the PAL/NTSC flag bit from the CONFIG
register. **NTSC = Bit Set, PAL = Bit Clear.**"

> Cross-reference: a reference backend's aliases `CONFIG` to the Tom
> register `$F00036` ("bit 4 1=NTSC") **as well**. The PAL/NTSC bit (bit4,
> $0010) is readable both at Tom's `$F00036` (HVS/CONFIG) and via Jerry's
> `JOYBUTS/CONFIG $F14002` per the SDK. **Emulator:** expose VIDTYPE (bit4) in the
> `$F14002` read result = 1 for NTSC, 0 for PAL, matching the configured region.
> (UNVERIFIED whether the bit physically lives in Jerry's button register or only
> Tom's $F00036; the SDK comment attaches it to CONFIG=$F14002. Set it in both
> reads to be safe.) *See Open Questions.*

### 4.6 The "no controller → `0xFFFFFFFF`" trap the internal porting notes

**Critical accuracy note for BigPEmu parity:**
> "`bigpemu_jag_get_buttons(0)` returns `0xFFFFFFFF` (all buttons pressed) when no
> real controller is attached (headless). Do NOT use it for input." the internal porting notes

`0xFFFFFFFF` is the **active-low all-released** raw read *before* the
`~acc` inversion — but `bigpemu_jag_get_buttons` returns it as if it were the
final value, so consumers see "all pressed." Two implications for **this**
emulator:

1. **Default idle state of `$F14000`/`$F14002` reads MUST be all-1s
   (`0xFFFFFFFF` for the 32-bit read), i.e. *nothing pressed* in active-low
   convention.** A correctly written game runs a reference backend's joypad scan, gets
   `acc = 0xFFFFFFFF`, inverts to `raw = 0` → no buttons. This is the right
   behavior; do **not** invert it.
2. To **inject** input, the emulator must place active-low pressed bits into the
   correct strobe column (§4.4) so a game running the real 4-strobe scan recovers
   them. **Do not** expose a pre-decoded `0xFFFFFFFF` "all buttons" word to game
   code; that is the headless artifact the porting notes warn against. The
   JOYSHIM fallback (script writes a button word to fixed DRAM `$001000`, game
   polls it the internal porting notes) is only needed when the JOYSTICK register path is
   unavailable — a from-scratch emulator should implement the real register
   correctly and not need it.

---

## 5. Memory map summary (Jerry peripherals)

| Range / addr | Contents |
|---|---|
| `$F10000`–`$F10006` | JPIT1–JPIT4 timer write regs |
| `$F10020` | `J_INT` Jerry interrupt control |
| `$F10030`–`$F10036` | UART (ASIDATA/ASICTRL/ASISTAT/ASICLK) — async serial |
| `$F10036`–`$F1003C` | JPIT1–JPIT4 timer **read** regs |
| `$F14000` | `JOYSTICK`/`JOY1` |
| `$F14002` | `JOYBUTS`/`JOY2`/`CONFIG` (+ VIDTYPE bit4) |
| `$F14800`–`$F17FFF` | GPIO decodes (CD iface, DMA ack, cartridge, paddle) [TR p.96] |
| `$F1A100`–`$F1A11F` | DSP control regs (`D_FLAGS`,`D_CTRL`,`D_PC`, etc.) |
| `$F1A140`/`$F1A144` | `DAC1`/`DAC2` PWM DACs |
| `$F1A148`/`$F1A14C` | `L_I2S`/`R_I2S` (LTXD/RTXD wr, LRXD/RRXD rd) |
| `$F1A150` | `SCLK` (wr) / `SSTAT` (rd) |
| `$F1A154` | `SMODE` |
| `$F1B000`–`$F1CFFF` | DSP local RAM (8 KB) — also DSP IRQ vectors at base+16*n |
| `$F1D000`–`$F1DFFF` | Wave table ROM (8 tables × 128 × signext16→32) |

Tom-side interrupt registers (the 68k's only view):
| `$F000E0` | `INT1` CPU interrupt control (5 sources, bit4=Jerry) |
| `$F000E2` | `INT2` resume register (write at end of every ISR) |
| 68k vector 64 = `$00000100` | autovector L2 — all interrupts dispatch here |

---

## 6. Implementation checklist (v1, timing-accurate)

1. **Timers:** model JPIT1–4 as two-stage down-counters; fire on
   `(N+1)*(M+1)` ticks; preset on write; readable. Route Timer1→J_INT bit2,
   Timer2→J_INT bit3 (and to DSP IRQ2/IRQ3 in parallel).
2. **J_INT ($F10020):** enables in bits0–5, pending readable in bits0–5, clears
   via writing bits8–13. Aggregate enabled-pending → Jerry→Tom line.
3. **INT1 ($F000E0):** bit4 = Jerry (edge-triggered rising), bit3 = Tom PIT,
   bits0–2 = video/GPU/OP. Any enabled pending → 68k Level-2 IRQ → dispatch via
   `$100`. On `INT2` write, restore GPU/Blitter bus priority.
4. **DSP interrupts:** independent; vector = `$F1B000 + 16*n`; enables in
   `D_FLAGS`, latches/start in `D_CTRL`. IRQ1=I²S, IRQ2=Timer1, IRQ3=Timer2,
   IRQ0=CPU (via `DSPINT0`).
5. **I²S:** when `SMODE.INTERNAL|WSEN`, advance a sample-frame clock of
   `64*(N+1)` ticks (mode16, from `SCLK`); each frame: raise I²S interrupt to DSP
   (IRQ1) and to Jerry (J_INT bit5), and consume `L_I2S`/`R_I2S`. Output silence
   is fine for v1; cadence must be correct.
6. **PWM DACs:** accept 32-bit writes to `$F1A140/4`, latch top 14 bits,
   double-buffer at Timer1 pre-scaler rate. (Audio output optional in v1.)
7. **Wave ROM:** read-only `$F1D000–$F1DFFF`, sign-extended 16→32; embed a real
   dump if available, else synthesize TRI/SINE.
8. **Joypad:** model column-strobe write to `$F14000` low byte; reads return
   active-low column data (rows in JOYSTICK[11:8], fires in JOYBUTS[1:0], all
   else =1). Idle = all 1s = nothing pressed. Inject by placing active-low bits
   into the strobe-selected column so the real reference joypad scan recovers them.
9. **CONFIG/VIDTYPE:** expose bit4 (`$10`) on `$F14002` (and Tom `$F00036`): 1 =
   NTSC, 0 = PAL.

---

## 7. Open Questions (validate against BigPEmu / hardware)

1. **System clock exact value.** Manual prints no Hz. Using 26.590906 MHz
   (NTSC)/26.593900 MHz (PAL). Confirm if cycle-exact audio sample rates are
   needed. (§0)
2. **Timer read-back semantics.** Does reading `$F10036/38/3A/3C` return the live
   pre-scaler count, live divider count, or last-written value? Spec assumes live
   divider count. (§1.3)
3. **J_INT read value.** Does reading `$F10020` return pending-only, or
   enables-OR-pending, in bits0–5? Spec dispatches on low-6 = pending. (§1.4)
4. **68k IPL level.** Confirm the combined Tom interrupt is IPL **2** and that the
   single dispatch address used in practice is `$100` (vector 64). Proven code
   installs at `$100`. (§2.1)
5. **INT1 bit4 edge model.** Confirm Jerry→Tom is rising-edge-triggered and how a
   second Jerry source asserting while bit4 is still pending behaves (coalesced?).
   (§2.2)
6. **I²S interrupt cadence.** Confirm one interrupt per 32-SCLK frame in mode16
   with `RISING` set (= `64*(N+1)` ticks), and the effect of `FALLING`/
   `EVERYWORD` (doubling? per-word?). (§3.2)
7. **DAC vs I²S as the real output.** Confirm whether console audio is taken from
   `DAC1/2` (PWM) or `L_I2S/R_I2S`, and whether games write both. (§3)
8. **Wave ROM contents.** Exact 8×128×16-bit sample values are not in the manual.
   Dump `$F1D000–$F1DFFF` from BigPEmu/hardware and embed. (§3.3)
9. **Joypad per-bit positions.** Confirm the exact bit position of each direction
   within JOYSTICK[11:8] and each fire within JOYBUTS[1:0] by round-tripping
   injected state through a reference backend's joypad driver. (§4.4)
10. **VIDTYPE location.** Confirm whether bit4 physically lives in Jerry's
    `$F14002` button register, Tom's `$F00036`, or both. Spec sets both. (§4.5)
11. **JPIT4 write address.** Manual table prints `$F10002` (typo); INC says
    `$F10006`. Spec uses `$F10006`. Confirm. (§1.1)
