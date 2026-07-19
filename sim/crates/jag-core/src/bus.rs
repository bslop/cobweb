//! The system bus: the 68000's 24-bit big-endian view of Jaguar memory, plus
//! the memory-mapped TOM and JERRY device state.
//!
//! ## Jaguar bus behavior (from the internal porting notes,
//! probe-verified on BigPEmu)
//!
//! There are **NO 68k bus errors** on the Jaguar. Specifically:
//! * unmapped *high* reads return `$FFFF`,
//! * unmapped *cartridge-space* reads (`$A1xxxx`, `$C0xxxx` with no cart) return
//!   `$0000`,
//! * writes to either **vanish silently**.
//!
//! This is load-bearing emulator behavior: code that relies on a bus-error
//! handler to emulate missing hardware never runs, and garbage reads silently
//! steer control flow. We replicate it exactly (and make the
//! genuinely-different-on-hardware cases configurable later).
//!
//! The processor *register files* (68k, GPU, DSP) live in their own structs at
//! the `Jaguar` level; the `Bus` owns only memory and device register state, so
//! a CPU step borrows `&mut Bus` without ever aliasing another processor.

use crate::mem;

/// The 68000 external address bus is 24 bits.
pub const ADDR_MASK: u32 = 0x00FF_FFFF;

/// A contiguous, byte-addressed, big-endian memory window. Storing device
/// registers as raw bytes makes the Jaguar's width-sensitive register aliasing
/// fall out for free — e.g. a (buggy) 32-bit store to the 16-bit `VMODE`
/// register correctly spills into `BORD1`, reproducing the real corruption.
pub struct Window {
    pub bytes: Box<[u8]>,
    base: u32,
}

impl Window {
    fn new(base: u32, size: usize) -> Self {
        Window { bytes: vec![0u8; size].into_boxed_slice(), base }
    }
    #[inline]
    fn off(&self, addr: u32) -> usize {
        (addr - self.base) as usize
    }
    #[inline]
    pub fn r8(&self, addr: u32) -> u8 {
        self.bytes[self.off(addr)]
    }
    #[inline]
    pub fn w8(&mut self, addr: u32, v: u8) {
        let o = self.off(addr);
        self.bytes[o] = v;
    }
    #[inline]
    pub fn r16(&self, addr: u32) -> u16 {
        let o = self.off(addr);
        u16::from_be_bytes([self.bytes[o], self.bytes[o + 1]])
    }
    #[inline]
    pub fn w16(&mut self, addr: u32, v: u16) {
        let o = self.off(addr);
        let b = v.to_be_bytes();
        self.bytes[o] = b[0];
        self.bytes[o + 1] = b[1];
    }
    #[inline]
    pub fn r32(&self, addr: u32) -> u32 {
        let o = self.off(addr);
        u32::from_be_bytes([
            self.bytes[o],
            self.bytes[o + 1],
            self.bytes[o + 2],
            self.bytes[o + 3],
        ])
    }
    #[inline]
    pub fn w32(&mut self, addr: u32, v: u32) {
        let o = self.off(addr);
        let b = v.to_be_bytes();
        self.bytes[o..o + 4].copy_from_slice(&b);
    }
}

/// TOM device state (video/OP/Blitter/GPU). The 64 KB register window holds the
/// video registers, CLUT, line buffers, blitter regs, GPU control regs and the
/// 4 KB GPU SRAM; typed sub-engines (`Op`, `Blitter`) keep their internal
/// latches separately. Populated incrementally as the engines are implemented.
pub struct Tom {
    pub win: Window,
    /// INT1 interrupt-enable mask (bits 0-4). Read returns *pending*, not this.
    pub int1_enable: u16,
    /// INT1 pending latches (bits 0-4): which interrupts have occurred. Games
    /// poll this (e.g. vblank-wait `btst #0,INT1`) or read it in the ISR.
    pub int1_pending: u16,
    /// The Object Processor's persistent scan-out framebuffer. The OP composites
    /// into this **one display line at a time** as the scheduler crosses each
    /// active scanline (so a game that rebuilds its object list every vblank is
    /// captured at scan-out time, not as a torn end-of-frame snapshot). See
    /// `tom::op_render_line`.
    pub fb: crate::tom::Framebuffer,
    /// The last **fully composited** field, snapshotted from `fb` at each frame
    /// boundary (see `scheduler`). `fb` is the *live* canvas — mid-field it may be
    /// freshly cleared with only the top lines drawn — so screen captures read
    /// `presented` to always get a coherent, complete frame regardless of where a
    /// run happens to stop.
    pub presented: crate::tom::Framebuffer,
    /// Per-field Object Processor cursor (geometry + anchor), reset each frame.
    pub op: crate::tom::OpState,
    /// RISC-tick cost of the most recent blit (`blit::run` sets it; the timed
    /// RISC step charges it to the launching `B_CMD` store, so the GPU's
    /// bwait-spin costs real time — HARDWARE-CALIBRATED, see `blit::cost`).
    pub last_blit_ticks: u64,
}

impl Tom {
    fn new() -> Self {
        Tom {
            win: Window::new(mem::TOM_BASE, 0x1_0000),
            int1_enable: 0,
            int1_pending: 0,
            fb: crate::tom::Framebuffer::solid(320, 240, 0, 0, 0),
            presented: crate::tom::Framebuffer::solid(320, 240, 0, 0, 0),
            op: crate::tom::OpState::default(),
            last_blit_ticks: 0,
        }
    }
}

/// JERRY device state (timers/audio/joypad/DSP). 64 KB register window holds
/// timer regs, joypad regs, DSP control regs and the 8 KB DSP SRAM.
pub struct Jerry {
    pub win: Window,
    /// Current injected joypad state, one entry per controller port.
    /// Bit layout is the joyedge format (see `jerry::Button`).
    pub pads: [u32; 2],
    /// Last value written to JOYSTICK — selects the controller matrix column.
    pub strobe: u16,
}

impl Jerry {
    fn new() -> Self {
        Jerry { win: Window::new(mem::JERRY_BASE, 0x1_0000), pads: [0; 2], strobe: 0x81FE }
    }
}

pub struct Bus {
    /// The 68000 is actively using the main bus (i.e. running, not STOPped).
    /// The 68k has no cache — when running it fetches from DRAM continuously,
    /// taxing GPU/DSP external page-hit accesses (row thrash; see timing.rs).
    /// Maintained by the scheduler; defaults to true (games run the 68k).
    pub m68k_on_bus: bool,
    /// 2 MB main DRAM.
    pub dram: Box<[u8]>,
    /// Cartridge ROM image (empty if none loaded).
    pub cart: Vec<u8>,
    /// Optional boot ROM.
    pub bootrom: Vec<u8>,
    pub tom: Tom,
    pub jerry: Jerry,
    /// Count of bus accesses, for the debugger / profiler.
    pub access_count: u64,
    /// Captured stereo audio (interleaved L,R 16-bit) when `audio_capture` is on.
    pub audio: Vec<i16>,
    pub audio_capture: bool,
    /// Sample rate of the captured audio (Hz), for the WAV header.
    pub audio_rate: u32,
}

impl Default for Bus {
    fn default() -> Self {
        Self::new()
    }
}

impl Bus {
    pub fn new() -> Self {
        Bus {
            m68k_on_bus: true,
            dram: vec![0u8; mem::DRAM_SIZE].into_boxed_slice(),
            cart: Vec::new(),
            bootrom: Vec::new(),
            tom: Tom::new(),
            jerry: Jerry::new(),
            access_count: 0,
            audio: Vec::new(),
            audio_capture: false,
            audio_rate: 44_100,
        }
    }

    /// Load a cartridge image (raw bytes mapped at `$800000`).
    pub fn load_cart(&mut self, data: Vec<u8>) {
        self.cart = data;
    }

    // ── 8-bit access ────────────────────────────────────────────────────────

    #[inline]
    pub fn read8(&mut self, addr: u32) -> u8 {
        self.access_count += 1;
        let a = addr & ADDR_MASK;
        if mem::is_dram(a) {
            self.dram[a as usize]
        } else if (mem::CART_START..mem::CART_END).contains(&a) {
            let idx = (a - mem::CART_START) as usize;
            self.cart.get(idx).copied().unwrap_or(0x00)
        } else if (mem::BOOTROM_START..mem::BOOTROM_END).contains(&a) {
            let idx = (a - mem::BOOTROM_START) as usize;
            self.bootrom.get(idx).copied().unwrap_or(0xFF)
        } else if mem::is_tom(a) {
            self.tom_read8(a)
        } else if mem::is_jerry(a) {
            self.jerry_read8(a)
        } else {
            // Unmapped: high gaps read $FF.
            0xFF
        }
    }

    #[inline]
    pub fn write8(&mut self, addr: u32, v: u8) {
        self.access_count += 1;
        let a = addr & ADDR_MASK;
        if mem::is_dram(a) {
            self.dram[a as usize] = v;
        } else if mem::is_tom(a) {
            self.tom_write8(a, v);
        } else if mem::is_jerry(a) {
            self.jerry_write8(a, v);
        } else {
            // Cart-space and unmapped writes vanish silently (no bus error).
        }
    }

    // ── 16-bit access (big-endian) ──────────────────────────────────────────

    #[inline]
    pub fn read16(&mut self, addr: u32) -> u16 {
        let a = addr & ADDR_MASK;
        if mem::is_dram(a) && a + 1 < mem::DRAM_END {
            let i = a as usize;
            u16::from_be_bytes([self.dram[i], self.dram[i + 1]])
        } else if mem::is_tom(a) {
            self.tom_read16(a)
        } else {
            ((self.read8(a) as u16) << 8) | self.read8(a.wrapping_add(1)) as u16
        }
    }

    #[inline]
    pub fn write16(&mut self, addr: u32, v: u16) {
        let a = addr & ADDR_MASK;
        if mem::is_dram(a) && a + 1 < mem::DRAM_END {
            let i = a as usize;
            let b = v.to_be_bytes();
            self.dram[i] = b[0];
            self.dram[i + 1] = b[1];
        } else if mem::is_tom(a) {
            self.tom_write16(a, v);
        } else {
            self.write8(a, (v >> 8) as u8);
            self.write8(a.wrapping_add(1), v as u8);
        }
    }

    fn tom_read16(&self, a: u32) -> u16 {
        if a == mem::INT1 {
            // Reading INT1 returns which interrupt sources are *pending*.
            return self.tom.int1_pending;
        }
        self.tom.win.r16(a)
    }

    fn tom_write16(&mut self, a: u32, v: u16) {
        if a == mem::INT1 {
            // Low bits 0-4 = enable mask; high bits 8-12 = write-1-to-clear the
            // matching pending latch. Many ISRs also re-assert the enable bit
            // (e.g. $0101) which both keeps video enabled and acks bit 0; some
            // games (a reference homebrew title) write $0001 to ack+enable, so clear on either the
            // low enable bit or the high clear bit.
            self.tom.int1_enable = v & 0x001F;
            let clr = ((v >> 8) | v) & 0x001F;
            self.tom.int1_pending &= !clr;
            self.tom.win.w16(a, v);
            return;
        }
        self.tom.win.w16(a, v);
    }

    // ── 32-bit access (big-endian) ──────────────────────────────────────────

    #[inline]
    pub fn read32(&mut self, addr: u32) -> u32 {
        let a = addr & ADDR_MASK;
        if mem::is_dram(a) && a + 3 < mem::DRAM_END {
            let i = a as usize;
            u32::from_be_bytes([
                self.dram[i],
                self.dram[i + 1],
                self.dram[i + 2],
                self.dram[i + 3],
            ])
        } else if mem::is_tom(a) {
            self.tom_read32(a)
        } else {
            // JERRY (incl. the joypad at $F14000) composes via the byte path so
            // device interception applies.
            ((self.read16(a) as u32) << 16) | self.read16(a.wrapping_add(2)) as u32
        }
    }

    #[inline]
    pub fn write32(&mut self, addr: u32, v: u32) {
        let a = addr & ADDR_MASK;
        if mem::is_dram(a) && a + 3 < mem::DRAM_END {
            let i = a as usize;
            let b = v.to_be_bytes();
            self.dram[i..i + 4].copy_from_slice(&b);
        } else if mem::is_tom(a) {
            self.tom_write32(a, v);
        } else {
            self.write16(a, (v >> 16) as u16);
            self.write16(a.wrapping_add(2), v as u16);
        }
    }

    fn tom_read32(&self, a: u32) -> u32 {
        if a == mem::B_CMD {
            // The Blitter is synchronous in this model, so it always reads idle.
            return crate::tom::blit::BLIT_IDLE;
        }
        self.tom.win.r32(a)
    }

    fn tom_write32(&mut self, a: u32, v: u32) {
        self.tom.win.w32(a, v);
        if a == mem::B_CMD {
            crate::tom::blit::run(self, v);
        }
    }

    // ── TOM register dispatch ───────────────────────────────────────────────
    // Side-effecting registers (B_CMD start-blit, OLP, HC/VC counters, GPU
    // control) are intercepted here; everything else hits the raw window so
    // width-aliasing behavior is preserved. Engines fill these in as built.

    fn tom_read8(&mut self, a: u32) -> u8 {
        self.tom.win.r8(a)
    }

    fn tom_write8(&mut self, a: u32, v: u8) {
        self.tom.win.w8(a, v);
    }

    // ── JERRY register dispatch ─────────────────────────────────────────────

    fn jerry_read8(&mut self, a: u32) -> u8 {
        // The joypad lives at $F14000 (JOYSTICK<<16 | JOYBUTS, read as 32-bit).
        if (mem::JOYSTICK..mem::JOYSTICK + 4).contains(&a) {
            let joy = crate::jerry::joy32(self.jerry.strobe, self.jerry.pads[0]);
            let shift = 8 * (3 - (a - mem::JOYSTICK));
            return (joy >> shift) as u8;
        }
        self.jerry.win.r8(a)
    }

    fn jerry_write8(&mut self, a: u32, v: u8) {
        self.jerry.win.w8(a, v);
        // A write to JOYSTICK selects the controller-scan column.
        if a == mem::JOYSTICK || a == mem::JOYSTICK + 1 {
            self.jerry.strobe = self.jerry.win.r16(mem::JOYSTICK);
        }
    }

    // ── Debug helpers (no side effects, no access counting) ─────────────────

    /// Read `len` bytes for inspection without triggering device side effects
    /// or bumping the access counter. Used by the debug API / screenshots.
    pub fn peek(&self, addr: u32, out: &mut [u8]) {
        for (k, b) in out.iter_mut().enumerate() {
            let a = (addr.wrapping_add(k as u32)) & ADDR_MASK;
            *b = if mem::is_dram(a) {
                self.dram[a as usize]
            } else if (mem::CART_START..mem::CART_END).contains(&a) {
                self.cart.get((a - mem::CART_START) as usize).copied().unwrap_or(0)
            } else if mem::is_tom(a) {
                self.tom.win.r8(a)
            } else if mem::is_jerry(a) {
                self.jerry.win.r8(a)
            } else if (mem::BOOTROM_START..mem::BOOTROM_END).contains(&a) {
                self.bootrom.get((a - mem::BOOTROM_START) as usize).copied().unwrap_or(0xFF)
            } else {
                0xFF
            };
        }
    }

    /// Write `bytes` for debugging (DRAM and device windows only).
    pub fn poke(&mut self, addr: u32, bytes: &[u8]) {
        for (k, &b) in bytes.iter().enumerate() {
            let a = (addr.wrapping_add(k as u32)) & ADDR_MASK;
            if mem::is_dram(a) {
                self.dram[a as usize] = b;
            } else if mem::is_tom(a) {
                self.tom.win.w8(a, b);
            } else if mem::is_jerry(a) {
                self.jerry.win.w8(a, b);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dram_roundtrip_big_endian() {
        let mut bus = Bus::new();
        bus.write32(0x1000, 0x1234_5678);
        assert_eq!(bus.read32(0x1000), 0x1234_5678);
        // Big-endian byte order.
        assert_eq!(bus.read8(0x1000), 0x12);
        assert_eq!(bus.read8(0x1003), 0x78);
        assert_eq!(bus.read16(0x1000), 0x1234);
        assert_eq!(bus.read16(0x1002), 0x5678);
    }

    #[test]
    fn unmapped_high_reads_ffff_writes_vanish() {
        let mut bus = Bus::new();
        // A gap above JERRY reads as $FFFF and swallows writes.
        assert_eq!(bus.read16(0xF5_0000), 0xFFFF);
        bus.write16(0xF5_0000, 0x1234);
        assert_eq!(bus.read16(0xF5_0000), 0xFFFF);
    }

    #[test]
    fn empty_cart_space_reads_zero() {
        let mut bus = Bus::new();
        assert_eq!(bus.read16(0xA1_0000), 0x0000);
        // Writes vanish (no panic, no effect).
        bus.write32(0xC0_0000, 0xDEAD_BEEF);
        assert_eq!(bus.read32(0xC0_0000), 0x0000_0000);
    }

    #[test]
    fn tom_register_width_aliasing() {
        // A 32-bit store across the 16-bit VMODE/BORD1 pair must spill — this
        // is the real hardware bug the porting notes document.
        let mut bus = Bus::new();
        bus.write32(mem::VMODE, 0x06C7_1234);
        assert_eq!(bus.read16(mem::VMODE), 0x06C7);
        assert_eq!(bus.read16(mem::BORD1), 0x1234);
    }
}
