//! The Jaguar memory map and hardware register addresses.
//!
//! Authoritative source: the official Atari `JAGUAR.INC` (1992-1994), as found
//! in the Atari SDK's `JAGUAR.INC`, cross-checked
//! against a proven reference backend's `jaguar.h`.
//!
//! Everything here is a *byte address* in the 68000's 24-bit address space.
//! The Jaguar is **big-endian**.

#![allow(dead_code)]

// ───────────────────────────── Main memory ─────────────────────────────────

/// First system RAM location.
pub const DRAM_START: u32 = 0x00_0000;
/// End of DRAM (exclusive). The Jaguar console has 2 MB of DRAM.
pub const DRAM_END: u32 = 0x20_0000;
/// Size of DRAM in bytes (2 MB).
pub const DRAM_SIZE: usize = (DRAM_END - DRAM_START) as usize;
/// Beginning of non-reserved RAM (`USERRAM`). The low 16 KB hold the 68k vector
/// table + Atari-reserved scratch.
pub const USERRAM: u32 = 0x00_4000;
/// `INITSTACK` — the canonical initial stack pointer (`ENDRAM - 4`).
pub const INITSTACK: u32 = DRAM_END - 4;

// ───────────────────────── Cartridge / boot ROM ────────────────────────────

/// Cartridge ROM space (mirrored/decoded across this window on real hardware).
pub const CART_START: u32 = 0x80_0000;
pub const CART_END: u32 = 0xE0_0000;
/// Boot ROM (BIOS) base.
pub const BOOTROM_START: u32 = 0xE0_0000;
pub const BOOTROM_END: u32 = 0xE2_0000;

// ────────────────────────────── TOM (video) ────────────────────────────────

/// TOM internal register base.
pub const TOM_BASE: u32 = 0xF0_0000;
pub const TOM_END: u32 = 0xF1_0000;

pub const MEMCON1: u32 = TOM_BASE + 0x00; // 16-bit
pub const MEMCON2: u32 = TOM_BASE + 0x02; // 16-bit
pub const HC: u32 = TOM_BASE + 0x04; // Horizontal count (16-bit)
pub const VC: u32 = TOM_BASE + 0x06; // Vertical count, half-lines (16-bit)
pub const LPH: u32 = TOM_BASE + 0x08;
pub const LPV: u32 = TOM_BASE + 0x0A;
pub const OB0: u32 = TOM_BASE + 0x10; // Current object phrase (read-back)
pub const OB1: u32 = TOM_BASE + 0x12;
pub const OB2: u32 = TOM_BASE + 0x14;
pub const OB3: u32 = TOM_BASE + 0x16;
/// Object List Pointer. Physically two 16-bit halves: OLPL @ $20, OLPH @ $22.
/// Software writes the 32-bit pointer **word-swapped** (see `tom`), so the OP
/// reads the list address as `(OLPH << 16) | OLPL`.
pub const OLP: u32 = TOM_BASE + 0x20; // OLPL (low half lives here)
pub const OLPH: u32 = TOM_BASE + 0x22; // OLPH (high half)
pub const ODP: u32 = TOM_BASE + 0x24; // Object data pointer (read-back)
pub const OBF: u32 = TOM_BASE + 0x26; // Object Processor flag (16-bit)
pub const VMODE: u32 = TOM_BASE + 0x28; // Video mode (16-bit)
pub const BORD1: u32 = TOM_BASE + 0x2A; // Border red/green (16-bit)
pub const BORD2: u32 = TOM_BASE + 0x2C; // Border blue (16-bit)
pub const HP: u32 = TOM_BASE + 0x2E; // Horizontal period
pub const HBB: u32 = TOM_BASE + 0x30; // Horizontal blank begin
pub const HBE: u32 = TOM_BASE + 0x32; // Horizontal blank end
pub const HS: u32 = TOM_BASE + 0x34; // Horizontal sync
pub const HVS: u32 = TOM_BASE + 0x36; // Horizontal vertical sync
pub const HDB1: u32 = TOM_BASE + 0x38; // Horizontal display begin 1
pub const HDB2: u32 = TOM_BASE + 0x3A; // Horizontal display begin 2
pub const HDE: u32 = TOM_BASE + 0x3C; // Horizontal display end
pub const VP: u32 = TOM_BASE + 0x3E; // Vertical period
pub const VBB: u32 = TOM_BASE + 0x40; // Vertical blank begin
pub const VBE: u32 = TOM_BASE + 0x42; // Vertical blank end
pub const VS: u32 = TOM_BASE + 0x44; // Vertical sync
pub const VDB: u32 = TOM_BASE + 0x46; // Vertical display begin
pub const VDE: u32 = TOM_BASE + 0x48; // Vertical display end
pub const VEB: u32 = TOM_BASE + 0x4A;
pub const VEE: u32 = TOM_BASE + 0x4C;
pub const VI: u32 = TOM_BASE + 0x4E; // Vertical interrupt scanline (16-bit)
pub const PIT0: u32 = TOM_BASE + 0x50; // Programmable interrupt timer (lo)
pub const PIT1: u32 = TOM_BASE + 0x52; // Programmable interrupt timer (hi)
pub const BG: u32 = TOM_BASE + 0x58; // Background color (16-bit)

pub const INT1: u32 = TOM_BASE + 0xE0; // CPU interrupt control register (16-bit)
pub const INT2: u32 = TOM_BASE + 0xE2; // CPU interrupt resume register (16-bit)

pub const CLUT: u32 = TOM_BASE + 0x400; // Color lookup table (256 × 16-bit)
pub const CLUT_END: u32 = CLUT + 0x200;
pub const LBUFA: u32 = TOM_BASE + 0x800; // Line buffer A
pub const LBUFB: u32 = TOM_BASE + 0x1000; // Line buffer B
pub const LBUFC: u32 = TOM_BASE + 0x1800; // Line buffer current

// INT1 (CPU interrupt control) bit masks.
pub const C_VIDENA: u16 = 0x0001; // enable video time-base (VI) interrupt
pub const C_GPUENA: u16 = 0x0002;
pub const C_OPENA: u16 = 0x0004;
pub const C_PITENA: u16 = 0x0008;
pub const C_JERENA: u16 = 0x0010;
pub const C_VIDCLR: u16 = 0x0100;
pub const C_GPUCLR: u16 = 0x0200;
pub const C_OPCLR: u16 = 0x0400;
pub const C_PITCLR: u16 = 0x0800;
pub const C_JERCLR: u16 = 0x1000;

// VMODE bits.
pub const VM_VIDEN: u16 = 0x0001;
pub const VM_MODE_MASK: u16 = 0x0006;
pub const VM_CRY16: u16 = 0x0000;
pub const VM_RGB24: u16 = 0x0002;
pub const VM_DIRECT16: u16 = 0x0004;
pub const VM_RGB16: u16 = 0x0006;
pub const VM_GENLOCK: u16 = 0x0008;
pub const VM_INCEN: u16 = 0x0010;
pub const VM_BINC: u16 = 0x0020;
pub const VM_CSYNC: u16 = 0x0040;
pub const VM_BGEN: u16 = 0x0080;
pub const VM_VARMOD: u16 = 0x0100;
pub const VM_PWIDTH_SHIFT: u32 = 9;
pub const VM_PWIDTH_MASK: u16 = 0x0E00;

/// CONFIG bit selecting NTSC (set) vs PAL (clear). Read at $F00036 / $F14002.
pub const VIDTYPE_NTSC: u16 = 0x10;

// ──────────────────── TOM GPU (RISC) control registers ─────────────────────

pub const G_FLAGS: u32 = TOM_BASE + 0x2100;
pub const G_MTXC: u32 = TOM_BASE + 0x2104;
pub const G_MTXA: u32 = TOM_BASE + 0x2108;
pub const G_END: u32 = TOM_BASE + 0x210C;
pub const G_PC: u32 = TOM_BASE + 0x2110;
pub const G_CTRL: u32 = TOM_BASE + 0x2114;
pub const G_HIDATA: u32 = TOM_BASE + 0x2118;
pub const G_REMAIN: u32 = TOM_BASE + 0x211C;
pub const G_DIVCTRL: u32 = TOM_BASE + 0x211C;
/// GPU internal SRAM. The GPU executes from here (the Tom DRAM-execution bug
/// means kernels must be copied into SRAM and run from there).
pub const G_RAM: u32 = TOM_BASE + 0x3000;
pub const G_RAM_SIZE: usize = 4 * 1024;
pub const G_RAM_END: u32 = G_RAM + G_RAM_SIZE as u32;

// ──────────────────────────── TOM Blitter ──────────────────────────────────

pub const A1_BASE: u32 = TOM_BASE + 0x2200;
pub const A1_FLAGS: u32 = TOM_BASE + 0x2204;
pub const A1_CLIP: u32 = TOM_BASE + 0x2208;
pub const A1_PIXEL: u32 = TOM_BASE + 0x220C;
pub const A1_STEP: u32 = TOM_BASE + 0x2210;
pub const A1_FSTEP: u32 = TOM_BASE + 0x2214;
pub const A1_FPIXEL: u32 = TOM_BASE + 0x2218;
pub const A1_INC: u32 = TOM_BASE + 0x221C;
pub const A1_FINC: u32 = TOM_BASE + 0x2220;
pub const A2_BASE: u32 = TOM_BASE + 0x2224;
pub const A2_FLAGS: u32 = TOM_BASE + 0x2228;
pub const A2_MASK: u32 = TOM_BASE + 0x222C;
pub const A2_PIXEL: u32 = TOM_BASE + 0x2230;
pub const A2_STEP: u32 = TOM_BASE + 0x2234;
pub const B_CMD: u32 = TOM_BASE + 0x2238; // write starts a blit; read = status
pub const B_COUNT: u32 = TOM_BASE + 0x223C; // outer<<16 | inner
pub const B_SRCD: u32 = TOM_BASE + 0x2240; // source data (64-bit, two longs)
pub const B_DSTD: u32 = TOM_BASE + 0x2248;
pub const B_DSTZ: u32 = TOM_BASE + 0x2250;
pub const B_SRCZ1: u32 = TOM_BASE + 0x2258;
pub const B_SRCZ2: u32 = TOM_BASE + 0x2260;
pub const B_PATD: u32 = TOM_BASE + 0x2268;
pub const B_IINC: u32 = TOM_BASE + 0x2270;
pub const B_ZINC: u32 = TOM_BASE + 0x2274;
pub const B_STOP: u32 = TOM_BASE + 0x2278;
pub const B_I3: u32 = TOM_BASE + 0x227C;
pub const B_I2: u32 = TOM_BASE + 0x2280;
pub const B_I1: u32 = TOM_BASE + 0x2284;
pub const B_I0: u32 = TOM_BASE + 0x2288;
pub const B_Z3: u32 = TOM_BASE + 0x228C;
pub const B_Z2: u32 = TOM_BASE + 0x2290;
pub const B_Z1: u32 = TOM_BASE + 0x2294;
pub const B_Z0: u32 = TOM_BASE + 0x2298;

// B_CMD command-register bits (verbatim from JAGUAR.INC — DO NOT re-derive;
// the porting notes record this being mis-derived twice).
pub const BC_SRCEN: u32 = 0x0000_0001; // d00 source data read (inner loop)
pub const BC_SRCENZ: u32 = 0x0000_0002; // d01 source Z read
pub const BC_SRCENX: u32 = 0x0000_0004; // d02 source data read (realign)
pub const BC_DSTEN: u32 = 0x0000_0008; // d03 dest data read
pub const BC_DSTENZ: u32 = 0x0000_0010; // d04 dest Z read
pub const BC_DSTWRZ: u32 = 0x0000_0020; // d05 dest Z write
pub const BC_CLIP_A1: u32 = 0x0000_0040; // d06 A1 clipping enable
pub const BC_UPDA1F: u32 = 0x0000_0100; // d08 A1 update step fraction
pub const BC_UPDA1: u32 = 0x0000_0200; // d09 A1 update step
pub const BC_UPDA2: u32 = 0x0000_0400; // d10 A2 update step
pub const BC_DSTA2: u32 = 0x0000_0800; // d11 reverse usage of A1 and A2
pub const BC_GOURD: u32 = 0x0000_1000; // d12 Gouraud shading
pub const BC_ZBUFF: u32 = 0x0000_2000; // d13 polygon Z updates
pub const BC_TOPBEN: u32 = 0x0000_4000; // d14 intensity carry into byte
pub const BC_TOPNEN: u32 = 0x0000_8000; // d15 intensity carry into nibble
pub const BC_PATDSEL: u32 = 0x0001_0000; // d16 select pattern data
pub const BC_ADDDSEL: u32 = 0x0002_0000; // d17 diagnostic
pub const BC_ZMODELT: u32 = 0x0004_0000;
pub const BC_ZMODEEQ: u32 = 0x0008_0000;
pub const BC_ZMODEGT: u32 = 0x0010_0000;
// d21-d24 logic-function control nibble:
pub const BC_LFU_NAN: u32 = 0x0020_0000; // !src & !dst
pub const BC_LFU_NA: u32 = 0x0040_0000; // !src &  dst
pub const BC_LFU_AN: u32 = 0x0080_0000; //  src & !dst
pub const BC_LFU_A: u32 = 0x0100_0000; //  src &  dst
pub const BC_LFU_MASK: u32 = 0x01E0_0000;
pub const BC_CMPDST: u32 = 0x0200_0000;
pub const BC_BCOMPEN: u32 = 0x0400_0000;
pub const BC_DCOMPEN: u32 = 0x0800_0000;
pub const BC_BKGWREN: u32 = 0x1000_0000;
pub const BC_BUSHI: u32 = 0x2000_0000;
pub const BC_SRCSHADE: u32 = 0x4000_0000;

/// Source REPLACEs destination (plain copy). `LFU_AN | LFU_A`.
pub const LFU_REPLACE: u32 = 0x0180_0000;
pub const LFU_XOR: u32 = 0x0120_0000;
pub const LFU_CLEAR: u32 = 0x0000_0000;

// A1/A2 FLAGS bit fields.
pub const AF_PITCH_MASK: u32 = 0x0000_0003; // d00-d01
pub const AF_PIXEL_SHIFT: u32 = 3; // d03-d05 bit depth (2^n)
pub const AF_PIXEL_MASK: u32 = 0x0000_0038;
pub const AF_ZOFFS_SHIFT: u32 = 6; // d06-d08
pub const AF_WIDTH_SHIFT: u32 = 9; // d09-d14 (6-bit float)
pub const AF_WIDTH_MASK: u32 = 0x0000_7E00;
pub const AF_XADD_SHIFT: u32 = 16; // d16-d17
pub const AF_XADD_MASK: u32 = 0x0003_0000;
pub const AF_XADDPHR: u32 = 0x0000_0000;
pub const AF_XADDPIX: u32 = 0x0001_0000;
pub const AF_XADD0: u32 = 0x0002_0000;
pub const AF_XADDINC: u32 = 0x0003_0000;
pub const AF_YADD1: u32 = 0x0004_0000; // d18
pub const AF_XSIGNSUB: u32 = 0x0008_0000; // d19
pub const AF_YSIGNSUB: u32 = 0x0010_0000; // d20

// ───────────────────────────── JERRY ───────────────────────────────────────

pub const JERRY_BASE: u32 = 0xF1_0000;
pub const JERRY_END: u32 = 0xF2_0000;

pub const JPIT1: u32 = TOM_BASE + 0x10000; // Timer 1 prescaler (16-bit)
pub const JPIT2: u32 = TOM_BASE + 0x10002; // Timer 1 divider
pub const JPIT3: u32 = TOM_BASE + 0x10004; // Timer 2 prescaler
pub const JPIT4: u32 = TOM_BASE + 0x10006; // Timer 2 divider
pub const J_INT: u32 = TOM_BASE + 0x10020; // Jerry interrupt control (16-bit)

pub const JOYSTICK: u32 = TOM_BASE + 0x14000; // joypad rows + mute (16-bit)
pub const JOYBUTS: u32 = TOM_BASE + 0x14002; // joypad buttons / CONFIG (16-bit)
pub const CONFIG: u32 = TOM_BASE + 0x14002;

// Jerry DSP (RISC) control registers.
pub const D_FLAGS: u32 = TOM_BASE + 0x1A100;
pub const D_MTXC: u32 = TOM_BASE + 0x1A104;
pub const D_MTXA: u32 = TOM_BASE + 0x1A108;
pub const D_END: u32 = TOM_BASE + 0x1A10C;
pub const D_PC: u32 = TOM_BASE + 0x1A110;
pub const D_CTRL: u32 = TOM_BASE + 0x1A114;
pub const D_MOD: u32 = TOM_BASE + 0x1A118;
pub const D_REMAIN: u32 = TOM_BASE + 0x1A11C;
pub const D_DIVCTRL: u32 = TOM_BASE + 0x1A11C;
pub const D_MACHI: u32 = TOM_BASE + 0x1A120;
pub const DAC1: u32 = TOM_BASE + 0x1A140; // Left PWM DAC (14-bit in 16-bit reg)
pub const DAC2: u32 = TOM_BASE + 0x1A144; // Right PWM DAC
pub const L_I2S: u32 = TOM_BASE + 0x1A148; // Left I2S transmit (16-bit)
pub const R_I2S: u32 = TOM_BASE + 0x1A14C; // Right I2S transmit
pub const SCLK: u32 = TOM_BASE + 0x1A150;
pub const SMODE: u32 = TOM_BASE + 0x1A154;
/// DSP interrupt source numbers (vector = D_RAM + 16*n).
pub const DSP_INT_I2S: u8 = 1;
/// DSP internal SRAM (8 KB). The DSP executes from here.
pub const D_RAM: u32 = TOM_BASE + 0x1B000;
pub const D_RAM_SIZE: usize = 8 * 1024;
pub const D_RAM_END: u32 = D_RAM + D_RAM_SIZE as u32;
/// Built-in Jerry wavetable ROM (8 × 128-sample, 16-bit tables).
pub const ROM_TABLE: u32 = TOM_BASE + 0x1D000;

// Shared GPU/DSP control-register bits (G_CTRL / D_CTRL).
pub const RISCGO: u32 = 0x0000_0001; // start GPU or DSP
pub const CPUINT: u32 = 0x0000_0002; // allow RISC to interrupt the 68k
pub const FORCEINT0: u32 = 0x0000_0004;
pub const SINGLE_STEP: u32 = 0x0000_0008;
pub const SINGLE_GO: u32 = 0x0000_0010;
pub const REGPAGE: u32 = 0x0000_4000; // register-bank select
pub const DMAEN: u32 = 0x0000_8000; // enable DMA LOAD/STORE

// Shared GPU/DSP flags-register bits.
pub const ZERO_FLAG: u32 = 0x0000_0001;
pub const CARRY_FLAG: u32 = 0x0000_0002;
pub const NEGA_FLAG: u32 = 0x0000_0004;
pub const IMASK: u32 = 0x0000_0008;

/// Divide-unit control bit: when set, DIV treats operands as 16.16 fixed-point.
pub const DIV_OFFSET: u32 = 0x0000_0001;

/// True if `addr` lies in the 2 MB DRAM window.
#[inline]
pub fn is_dram(addr: u32) -> bool {
    addr < DRAM_END
}

/// True if `addr` lies in the TOM register / SRAM window.
#[inline]
pub fn is_tom(addr: u32) -> bool {
    (TOM_BASE..TOM_END).contains(&addr)
}

/// True if `addr` lies in the JERRY register / SRAM window.
#[inline]
pub fn is_jerry(addr: u32) -> bool {
    (JERRY_BASE..JERRY_END).contains(&addr)
}
