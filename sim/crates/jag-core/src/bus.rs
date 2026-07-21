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
    /// Offset of `addr` within the window, or `None` if it falls outside.
    ///
    /// Every accessor below is bounds-checked. A wild address used to index the
    /// backing array directly and ABORT THE PROCESS
    /// (COBWEB_BUG_bus_oob_panic.md: "index out of bounds: the len is 65536 but
    /// the index is 65536", from a 16-bit read straddling the final byte).
    /// Real hardware floats the bus and returns garbage for an unmapped access;
    /// it does not halt the machine, and a panic here killed long profiling
    /// runs and made any experiment that put a kernel into a bad state unusable.
    /// Reads off the end now return 0 and writes are dropped.
    #[inline]
    fn off(&self, addr: u32) -> Option<usize> {
        let o = addr.wrapping_sub(self.base) as usize;
        if o < self.bytes.len() {
            Some(o)
        } else {
            None
        }
    }
    #[inline]
    fn get(&self, addr: u32, n: usize) -> Option<usize> {
        let o = self.off(addr)?;
        if o + n <= self.bytes.len() {
            Some(o)
        } else {
            None
        }
    }
    #[inline]
    pub fn r8(&self, addr: u32) -> u8 {
        self.get(addr, 1).map_or(0, |o| self.bytes[o])
    }
    #[inline]
    pub fn w8(&mut self, addr: u32, v: u8) {
        if let Some(o) = self.get(addr, 1) {
            self.bytes[o] = v;
        }
    }
    #[inline]
    pub fn r16(&self, addr: u32) -> u16 {
        self.get(addr, 2)
            .map_or(0, |o| u16::from_be_bytes([self.bytes[o], self.bytes[o + 1]]))
    }
    #[inline]
    pub fn w16(&mut self, addr: u32, v: u16) {
        if let Some(o) = self.get(addr, 2) {
            let b = v.to_be_bytes();
            self.bytes[o] = b[0];
            self.bytes[o + 1] = b[1];
        }
    }
    #[inline]
    pub fn r32(&self, addr: u32) -> u32 {
        self.get(addr, 4).map_or(0, |o| {
            u32::from_be_bytes([
                self.bytes[o],
                self.bytes[o + 1],
                self.bytes[o + 2],
                self.bytes[o + 3],
            ])
        })
    }
    #[inline]
    pub fn w32(&mut self, addr: u32, v: u32) {
        if let Some(o) = self.get(addr, 4) {
            self.bytes[o..o + 4].copy_from_slice(&v.to_be_bytes());
        }
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
    /// Launch-overhead part of `last_blit_ticks` (for the counter split).
    pub last_blit_launch: u64,
    /// Ticks until the in-flight blit completes — the Blitter is ASYNCHRONOUS.
    /// HARDWARE (calib 2026-07-19, 1/2/4/8/256-px probes + OpenLara's NOFILL
    /// delta): per-blit cost matches jsim within ~5% at every span length, yet
    /// the whole-frame fill charge was 24% (jsim) vs ~10% (silicon). The
    /// difference is concurrency, not cost: gpu_geotex launches a span and
    /// runs the next span's DDA math (GPU-SRAM-local) WHILE the Blitter works,
    /// bwaiting only at the top of the next launch — serialized charging bills
    /// the overlap twice. B_CMD reads report busy until this drains (GPU
    /// instruction time while the GPU runs; scheduler wall time while it
    /// doesn't).
    pub blit_busy: u64,
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
            last_blit_launch: 0,
            blit_busy: 0,
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
    /// Bus cycles consumed by the CURRENT 68000 instruction (fetch + data).
    /// Zeroed by `M68k::step` before each instruction and read back after, so
    /// the RISCs bumping it between 68k steps is harmless — the scheduler runs
    /// each 68k instruction to completion before granting GPU/DSP budget.
    /// Basis for the HARDWARE-CALIBRATED external-bus charge (see m68k.rs
    /// M68K_FETCH_WAIT_X10 / M68K_DATA_WAIT_X10).
    pub m68k_bus_cycles: u32,
    /// 2 MB main DRAM.
    pub dram: Box<[u8]>,
    /// Cartridge ROM image (empty if none loaded).
    pub cart: Vec<u8>,
    /// Optional boot ROM.
    pub bootrom: Vec<u8>,
    pub tom: Tom,
    /// Attached GameDrive/SD (emulated SPI device at $F16002-5). None = absent,
    /// which is what `gd_install` detects via its bounded SPI waits.
    pub gamedrive: Option<crate::gamedrive::GameDrive>,
    pub jerry: Jerry,
    /// Count of bus accesses, for the debugger / profiler.
    pub access_count: u64,
    /// Captured stereo audio (interleaved L,R 16-bit) when `audio_capture` is on.
    pub audio: Vec<i16>,
    pub audio_capture: bool,
    /// Sample rate of the captured audio (Hz), for the WAV header.
    pub audio_rate: u32,
    /// Write-watchpoint ranges `[lo, hi]` (inclusive). When non-empty, every
    /// write from ANY master — 68k, GPU, DSP, or the Blitter — that lands in
    /// a range is logged with who wrote it and from where. "Who wrote this
    /// byte" is the first question whenever silicon and emulator disagree
    /// (COBWEB_REQ_rectshade_and_calibration §5.1).
    pub watches: Vec<(u32, u32)>,
    /// First [`WATCH_LOG_CAP`] hits (the total keeps counting past the cap).
    pub watch_log: Vec<WatchHit>,
    /// Total watched writes seen (including beyond the log cap).
    pub watch_total: u64,
    /// Which master is currently driving bus writes (engines set this).
    pub cur_master: Master,
    /// The driving master's PC at the current write (best-effort; engines
    /// refresh it per instruction while watches are armed).
    pub cur_master_pc: u32,
    /// Nonzero while inside a composed write (write32→write16→write8), so a
    /// single logical store logs once, at its true width.
    watch_suppress: u8,
    /// Scheduler-maintained frame counter mirror (for watch-hit context).
    pub frame_mirror: u64,
    /// B_CMD reads that observed BUSY (the bwait spin, counted at the bus).
    /// Atomic because the read path is `&self`; single-threaded ordering is
    /// all we need (Relaxed).
    pub bcmd_busy_reads: std::sync::atomic::AtomicU64,
}

/// Cap on retained watch hits — enough to see the pattern, bounded so a
/// watched framebuffer clear can't eat memory.
pub const WATCH_LOG_CAP: usize = 256;

/// A bus master, for watchpoint attribution.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Master {
    Cpu,
    Gpu,
    Dsp,
    Blitter,
    /// Host-side pokes (ROM load, debugger) — not machine activity.
    Host,
}

impl Master {
    pub fn name(self) -> &'static str {
        match self {
            Master::Cpu => "68k",
            Master::Gpu => "gpu",
            Master::Dsp => "dsp",
            Master::Blitter => "blitter",
            Master::Host => "host",
        }
    }
}

/// One logged watched write.
#[derive(Debug, Clone, Copy)]
pub struct WatchHit {
    pub addr: u32,
    pub value: u32,
    /// Access width in bits (8/16/32).
    pub size: u8,
    pub master: Master,
    /// PC of the writing master (0 for the Blitter — it has no PC; the log
    /// entry's master tells you to look at the launching B_CMD instead).
    pub pc: u32,
    /// Frame counter mirror at the time of the write (scheduler-maintained).
    pub frame: u64,
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
            m68k_bus_cycles: 0,
            dram: vec![0u8; mem::DRAM_SIZE].into_boxed_slice(),
            cart: Vec::new(),
            bootrom: Vec::new(),
            tom: Tom::new(),
            gamedrive: None,
            jerry: Jerry::new(),
            access_count: 0,
            audio: Vec::new(),
            audio_capture: false,
            audio_rate: 44_100,
            watches: Vec::new(),
            watch_log: Vec::new(),
            watch_total: 0,
            cur_master: Master::Host,
            cur_master_pc: 0,
            watch_suppress: 0,
            frame_mirror: 0,
            bcmd_busy_reads: std::sync::atomic::AtomicU64::new(0),
        }
    }

    /// Arm a write-watch on the inclusive range `[lo, hi]`.
    pub fn add_watch(&mut self, lo: u32, hi: u32) {
        self.watches.push((lo.min(hi), lo.max(hi)));
    }

    /// Disarm all watches and clear the log.
    pub fn clear_watches(&mut self) {
        self.watches.clear();
        self.watch_log.clear();
        self.watch_total = 0;
    }

    /// Log `addr` if it falls in a watched range (called by every write path;
    /// the empty-vec check keeps the unwatched hot path to one branch).
    #[inline]
    fn watch_note(&mut self, addr: u32, size: u8, value: u32) {
        if self.watches.is_empty() || self.watch_suppress > 0 {
            return;
        }
        let hi_byte = addr + (size as u32 / 8) - 1;
        if self.watches.iter().any(|&(lo, hi)| hi_byte >= lo && addr <= hi) {
            self.watch_total += 1;
            if self.watch_log.len() < WATCH_LOG_CAP {
                self.watch_log.push(WatchHit {
                    addr,
                    value,
                    size,
                    master: self.cur_master,
                    pc: self.cur_master_pc,
                    frame: self.frame_mirror,
                });
            }
        }
    }

    /// Load a cartridge image (raw bytes mapped at `$800000`).
    pub fn load_cart(&mut self, data: Vec<u8>) {
        self.cart = data;
    }

    // ── 8-bit access ────────────────────────────────────────────────────────

    #[inline]
    pub fn read8(&mut self, addr: u32) -> u8 {
        self.m68k_bus_cycles += 1;
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
        self.m68k_bus_cycles += 1;
        self.access_count += 1;
        let a = addr & ADDR_MASK;
        self.watch_note(a, 8, v as u32);
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
            self.m68k_bus_cycles += 1;
            let i = a as usize;
            u16::from_be_bytes([self.dram[i], self.dram[i + 1]])
        } else if mem::is_tom(a) {
            self.m68k_bus_cycles += 1;
            self.tom_read16(a)
        } else {
            // Composes via read8, which counts each byte — but a word access
            // on the 16-bit bus is ONE cycle, not two, so compensate. Without
            // this every cart-ROM/Jerry long read (read32 -> read16 x2 ->
            // read8 x4) counted 4 cycles instead of 2 and the 68k wait charge
            // double-billed exactly the accesses the DRAM-only calibration
            // probes never exercised.
            let v = ((self.read8(a) as u16) << 8) | self.read8(a.wrapping_add(1)) as u16;
            self.m68k_bus_cycles -= 1;
            v
        }
    }

    #[inline]
    pub fn write16(&mut self, addr: u32, v: u16) {
        let a = addr & ADDR_MASK;
        self.watch_note(a, 16, v as u32);
        if mem::is_dram(a) && a + 1 < mem::DRAM_END {
            self.m68k_bus_cycles += 1;
            let i = a as usize;
            let b = v.to_be_bytes();
            self.dram[i] = b[0];
            self.dram[i + 1] = b[1];
        } else if mem::is_tom(a) {
            self.m68k_bus_cycles += 1;
            self.tom_write16(a, v);
        } else {
            self.watch_suppress += 1;
            self.write8(a, (v >> 8) as u8);
            self.write8(a.wrapping_add(1), v as u8);
            self.watch_suppress -= 1;
            self.m68k_bus_cycles -= 1; // one bus cycle per word, as in read16
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
            self.m68k_bus_cycles += 2; // a long is two 16-bit bus cycles
            let i = a as usize;
            u32::from_be_bytes([
                self.dram[i],
                self.dram[i + 1],
                self.dram[i + 2],
                self.dram[i + 3],
            ])
        } else if mem::is_tom(a) {
            self.m68k_bus_cycles += 2;
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
        self.watch_note(a, 32, v);
        if mem::is_dram(a) && a + 3 < mem::DRAM_END {
            self.m68k_bus_cycles += 2;
            let i = a as usize;
            let b = v.to_be_bytes();
            self.dram[i..i + 4].copy_from_slice(&b);
        } else if mem::is_tom(a) {
            self.m68k_bus_cycles += 2;
            self.tom_write32(a, v);
        } else {
            self.watch_suppress += 1;
            self.write16(a, (v >> 16) as u16);
            self.write16(a.wrapping_add(2), v as u16);
            self.watch_suppress -= 1;
        }
    }

    fn tom_read32(&self, a: u32) -> u32 {
        if a == mem::B_CMD {
            // Data-wise the blit completed at launch; TIME-wise it is still on
            // the bus until blit_busy drains. bit0 = idle.
            if self.tom.blit_busy > 0 {
                self.bcmd_busy_reads
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            }
            return if self.tom.blit_busy > 0 {
                crate::tom::blit::BLIT_IDLE & !1
            } else {
                crate::tom::blit::BLIT_IDLE
            };
        }
        self.tom.win.r32(a)
    }

    fn tom_write32(&mut self, a: u32, v: u32) {
        self.tom.win.w32(a, v);
        if a == mem::B_CMD {
            {
                // Blitter writes are attributed to the Blitter, not to the
                // master that stored B_CMD (watch hits say who really wrote).
                let (m, pc) = (self.cur_master, self.cur_master_pc);
                self.cur_master = Master::Blitter;
                self.cur_master_pc = 0;
                crate::tom::blit::run(self, v);
                self.cur_master = m;
                self.cur_master_pc = pc;
            }
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
        if let Some(gd) = self.gamedrive.as_ref() {
            match a {
                crate::gamedrive::SPI_DATAB => return gd.read_datab(),
                crate::gamedrive::SPI_STATUS => return (gd.status() >> 8) as u8,
                x if x == crate::gamedrive::SPI_STATUS + 1 => return gd.status() as u8,
                _ => {}
            }
        }
        // The joypad lives at $F14000 (JOYSTICK<<16 | JOYBUTS, read as 32-bit).
        if (mem::JOYSTICK..mem::JOYSTICK + 4).contains(&a) {
            let joy = crate::jerry::joy32(self.jerry.strobe, self.jerry.pads[0]);
            let shift = 8 * (3 - (a - mem::JOYSTICK));
            return (joy >> shift) as u8;
        }
        self.jerry.win.r8(a)
    }

    fn jerry_write8(&mut self, a: u32, v: u8) {
        if std::env::var_os("JAGEMU_AUDIO_DEBUG").is_some()
            && (mem::L_I2S..mem::L_I2S + 8).contains(&a) && v != 0 {
            eprintln!("I2SW {a:#08X} <- {v:#04X}");
        }
        if self.gamedrive.is_some() && (crate::gamedrive::SPI_STATUS..=crate::gamedrive::SPI_DATAB).contains(&a) {
            // byte-wide access into the SPI window: compose onto the word path
            let gd = self.gamedrive.as_mut().unwrap();
            match a {
                crate::gamedrive::SPI_DATAB => gd.write_data(v as u16),
                x if x == crate::gamedrive::SPI_STATUS + 1 => gd.write_status(v as u16),
                _ => {}
            }
            return;
        }
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

#[cfg(test)]
mod oob_tests {
    use super::*;

    /// COBWEB_BUG_bus_oob_panic.md — a wild or straddling access must not abort
    /// the process. Hardware floats the bus; jsim returns 0 and drops writes.
    #[test]
    fn window_access_past_the_end_does_not_panic() {
        let mut w = Window::new(0xF00000, 0x10000);
        let last = 0xF00000 + 0xFFFF;
        assert_eq!(w.r8(last + 1), 0, "one past the end reads 0");
        assert_eq!(w.r16(last), 0, "16-bit read straddling the end reads 0");
        assert_eq!(w.r32(last - 1), 0, "32-bit read straddling the end reads 0");
        assert_eq!(w.r32(0xFFFFFFFF), 0, "wild address reads 0");
        // Writes off the end are dropped, not panics, and do not corrupt the tail.
        w.w16(last, 0xDEAD);
        w.w32(last - 1, 0xDEADBEEF);
        w.w8(last + 1, 0xFF);
        w.w32(0xFFFFFFFF, 0xDEADBEEF);
        assert_eq!(w.r8(last), 0, "dropped write left the last byte untouched");
        // In-range accesses still work.
        w.w32(0xF00000, 0x01020304);
        assert_eq!(w.r32(0xF00000), 0x01020304);
        assert_eq!(w.r16(0xF00002), 0x0304);
    }
}
