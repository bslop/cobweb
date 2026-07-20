//! Motorola 68000 interpreter, wired for the Jaguar.
//!
//! Big-endian, plain 68000 (no 020 extensions — `bsr.l`/`bra.l` don't exist and
//! decode as `bsr.s -1`, landing on an odd address → address error, exactly as
//! on hardware). All Jaguar interrupts arrive at **level 2** and vector through
//! **vector 64 (`$100`)**. See `docs/spec/M68K_JAGUAR.md`.
//!
//! Scope: the P0+P1 instruction set the conveyor belt's `m68k-aout-gcc` output
//! and boot code actually use. Cycle counts are MC68000 base figures (memory
//! wait-state refinement is a later pass).

use crate::bus::Bus;
use crate::debug::Debugger;

// Condition-code register bits (in the low byte of SR).
const FLAG_C: u16 = 1 << 0;
const FLAG_V: u16 = 1 << 1;
const FLAG_Z: u16 = 1 << 2;
const FLAG_N: u16 = 1 << 3;
const FLAG_X: u16 = 1 << 4;

const SR_S: u16 = 0x2000; // supervisor
const SR_T: u16 = 0x8000; // trace
const SR_MASK_BITS: u16 = 0x0700; // interrupt mask
const SR_VALID: u16 = SR_T | SR_S | SR_MASK_BITS | 0x001F;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Size {
    Byte,
    Word,
    Long,
}

impl Size {
    #[inline]
    fn bytes(self) -> u32 {
        match self {
            Size::Byte => 1,
            Size::Word => 2,
            Size::Long => 4,
        }
    }
    #[inline]
    fn mask(self) -> u32 {
        match self {
            Size::Byte => 0xFF,
            Size::Word => 0xFFFF,
            Size::Long => 0xFFFF_FFFF,
        }
    }
    #[inline]
    fn msb(self) -> u32 {
        match self {
            Size::Byte => 0x80,
            Size::Word => 0x8000,
            Size::Long => 0x8000_0000,
        }
    }
}

/// A decoded effective-address target. Predecrement/postincrement side effects
/// are applied once, at decode time.
#[derive(Clone, Copy)]
enum Ea {
    D(usize),
    A(usize),
    Mem(u32),
    Imm(u32),
}

pub struct M68k {
    pub d: [u32; 8],
    pub a: [u32; 8], // a[7] is the active stack pointer
    pub usp: u32,
    pub ssp: u32,
    pub pc: u32,
    pub sr: u16,
    /// Highest pending interrupt level (0 = none). The Jaguar drives level 2.
    pub pending_level: u8,
    /// `STOP` halted the CPU until an interrupt arrives.
    pub stopped: bool,
    pub cycles: u64,
    /// Nesting depth of interrupt handlers (for ISR-vs-main attribution).
    pub isr_depth: u32,
    pub instret: u64,
    /// Set when an unrecognized opcode is hit, for the debugger: (pc, opcode).
    pub last_illegal: Option<u32>,
    pub last_illegal_op: u16,
    /// Count of illegal/unimplemented opcodes hit — a bring-up signal that the
    /// decoder is missing something.
    pub illegal_count: u64,
}

impl Default for M68k {
    fn default() -> Self {
        Self::new()
    }
}

impl M68k {
    pub fn new() -> Self {
        M68k {
            d: [0; 8],
            a: [0; 8],
            usp: 0,
            ssp: 0,
            pc: 0,
            sr: 0x2700,
            pending_level: 0,
            stopped: false,
            cycles: 0,
            isr_depth: 0,
            instret: 0,
            last_illegal: None,
            last_illegal_op: 0,
            illegal_count: 0,
        }
    }

    /// Hardware reset: SR=$2700 (supervisor, mask 7), SSP←[$0], PC←[$4].
    pub fn reset(&mut self, bus: &mut Bus) {
        self.sr = 0x2700;
        self.ssp = bus.read32(0);
        self.a[7] = self.ssp;
        self.pc = bus.read32(4);
        self.pending_level = 0;
        self.stopped = false;
    }

    #[inline]
    pub fn set_pc(&mut self, pc: u32) {
        self.pc = pc;
    }

    /// Raise an interrupt request at `level` (Jaguar always uses 2). Cleared on
    /// acknowledge.
    #[inline]
    pub fn request_interrupt(&mut self, level: u8) {
        if level > self.pending_level {
            self.pending_level = level;
        }
    }

    // ── flag helpers ─────────────────────────────────────────────────────────
    #[inline]
    fn set_flag(&mut self, f: u16, on: bool) {
        if on {
            self.sr |= f;
        } else {
            self.sr &= !f;
        }
    }
    #[inline]
    fn flag(&self, f: u16) -> bool {
        self.sr & f != 0
    }
    #[inline]
    fn set_nz(&mut self, v: u32, size: Size) {
        self.set_flag(FLAG_N, v & size.msb() != 0);
        self.set_flag(FLAG_Z, v & size.mask() == 0);
    }

    #[inline]
    fn supervisor(&self) -> bool {
        self.sr & SR_S != 0
    }
    #[inline]
    fn int_mask(&self) -> u8 {
        ((self.sr & SR_MASK_BITS) >> 8) as u8
    }

    /// Write SR, swapping the active stack pointer if the S bit changes.
    fn set_sr(&mut self, val: u16) {
        let old_s = self.sr & SR_S;
        let new_s = val & SR_S;
        if old_s != new_s {
            if new_s != 0 {
                self.usp = self.a[7];
                self.a[7] = self.ssp;
            } else {
                self.ssp = self.a[7];
                self.a[7] = self.usp;
            }
        }
        self.sr = val & SR_VALID;
    }

    // ── instruction-stream fetch ─────────────────────────────────────────────
    #[inline]
    fn fetch16(&mut self, bus: &mut Bus) -> u16 {
        let w = bus.read16(self.pc);
        self.pc = self.pc.wrapping_add(2);
        w
    }
    #[inline]
    fn fetch32(&mut self, bus: &mut Bus) -> u32 {
        let l = bus.read32(self.pc);
        self.pc = self.pc.wrapping_add(4);
        l
    }

    // ── stack ────────────────────────────────────────────────────────────────
    #[inline]
    fn push32(&mut self, bus: &mut Bus, v: u32) {
        self.a[7] = self.a[7].wrapping_sub(4);
        bus.write32(self.a[7], v);
    }
    #[inline]
    fn push16(&mut self, bus: &mut Bus, v: u16) {
        self.a[7] = self.a[7].wrapping_sub(2);
        bus.write16(self.a[7], v);
    }
    #[inline]
    fn pop32(&mut self, bus: &mut Bus) -> u32 {
        let v = bus.read32(self.a[7]);
        self.a[7] = self.a[7].wrapping_add(4);
        v
    }
    #[inline]
    fn pop16(&mut self, bus: &mut Bus) -> u16 {
        let v = bus.read16(self.a[7]);
        self.a[7] = self.a[7].wrapping_add(2);
        v
    }

    /// Execute one instruction (or take a pending interrupt). Returns cycles.
    ///
    /// Wraps the core step so the profiler can attribute cycles to the PC that
    /// *issued* the instruction (not the PC after it), and can tell a sleeping
    /// 68000 apart from a spinning one.
    pub fn step(&mut self, bus: &mut Bus, dbg: &mut Debugger) -> u32 {
        if dbg.prof.is_none() {
            return self.step_inner(bus, dbg);
        }
        let pc0 = self.pc;
        let was_stopped = self.stopped;
        let c = self.step_inner(bus, dbg);
        let in_isr = self.isr_depth > 0;
        if let Some(p) = dbg.prof.as_mut() {
            p.record(pc0, c, in_isr, was_stopped);
        }
        c
    }

    fn step_inner(&mut self, bus: &mut Bus, dbg: &mut Debugger) -> u32 {
        // Service interrupts first.
        if self.pending_level != 0 {
            let lvl = self.pending_level;
            if lvl == 7 || lvl > self.int_mask() {
                self.stopped = false;
                return self.take_interrupt(bus, lvl);
            }
        }
        if self.stopped {
            return 4; // idle, waiting for an interrupt
        }

        if self.pc & 1 != 0 {
            return self.exception(bus, 3, true); // address error: odd PC fetch
        }

        if dbg.enabled {
            dbg.on_fetch(self.pc);
        }

        let op = self.fetch16(bus);
        self.instret += 1;
        let c = self.execute(bus, op);
        self.cycles += c as u64;
        c
    }

    fn take_interrupt(&mut self, bus: &mut Bus, level: u8) -> u32 {
        let old_sr = self.sr;
        // Enter supervisor, set mask to `level`, clear trace.
        let mut new_sr = self.sr | SR_S;
        new_sr &= !SR_MASK_BITS;
        new_sr |= (level as u16) << 8;
        new_sr &= !SR_T;
        self.set_sr(new_sr);
        // Short exception frame: push PC then SR.
        self.push32(bus, self.pc);
        self.push16(bus, old_sr);
        // Jaguar IACK supplies vector 64 for its (level-2) interrupt; fall back
        // to the architectural autovector for any other level.
        let vector = if level == 2 { 64 } else { 24 + level as u32 };
        self.pc = bus.read32(vector * 4);
        self.isr_depth += 1;
        self.pending_level = 0; // acknowledged
        44
    }

    /// Generic exception entry (traps, illegal, divide-by-zero, address error).
    fn exception(&mut self, bus: &mut Bus, vector: u32, group0: bool) -> u32 {
        let old_sr = self.sr;
        let mut new_sr = self.sr | SR_S;
        new_sr &= !SR_T;
        self.set_sr(new_sr);
        if group0 {
            // Group-0 (address/bus error) frame: extra status word, fault
            // address, IR, SR, PC. Approximate but shaped correctly.
            self.push32(bus, self.pc);
            self.push16(bus, old_sr);
            self.push16(bus, 0); // instruction register (approx)
            self.push32(bus, self.pc); // access address (approx)
            self.push16(bus, 0); // special status word (approx)
        } else {
            self.push32(bus, self.pc);
            self.push16(bus, old_sr);
        }
        self.pc = bus.read32(vector * 4);
        34
    }

    // ── effective-address decode/read/write ──────────────────────────────────

    /// Decode an EA, applying predecrement/postincrement once.
    fn decode_ea(&mut self, bus: &mut Bus, mode: u16, reg: u16, size: Size) -> Ea {
        let reg = reg as usize;
        match mode {
            0 => Ea::D(reg),
            1 => Ea::A(reg),
            2 => Ea::Mem(self.a[reg]),
            3 => {
                // (An)+ — A7 byte access bumps by 2 to stay word-aligned.
                let inc = if reg == 7 && size == Size::Byte { 2 } else { size.bytes() };
                let addr = self.a[reg];
                self.a[reg] = self.a[reg].wrapping_add(inc);
                Ea::Mem(addr)
            }
            4 => {
                let dec = if reg == 7 && size == Size::Byte { 2 } else { size.bytes() };
                self.a[reg] = self.a[reg].wrapping_sub(dec);
                Ea::Mem(self.a[reg])
            }
            5 => {
                let disp = self.fetch16(bus) as i16 as i32;
                Ea::Mem(self.a[reg].wrapping_add(disp as u32))
            }
            6 => Ea::Mem(self.index_ea(bus, self.a[reg])),
            7 => match reg {
                0 => {
                    let a = self.fetch16(bus) as i16 as i32 as u32; // abs.w sign-extended
                    Ea::Mem(a)
                }
                1 => {
                    let a = self.fetch32(bus); // abs.l
                    Ea::Mem(a)
                }
                2 => {
                    let base = self.pc;
                    let disp = self.fetch16(bus) as i16 as i32;
                    Ea::Mem(base.wrapping_add(disp as u32)) // (d16,PC)
                }
                3 => {
                    let base = self.pc;
                    Ea::Mem(self.index_ea(bus, base)) // (d8,PC,Xn)
                }
                4 => match size {
                    Size::Byte => {
                        let w = self.fetch16(bus);
                        Ea::Imm((w & 0xFF) as u32)
                    }
                    Size::Word => Ea::Imm(self.fetch16(bus) as u32),
                    Size::Long => Ea::Imm(self.fetch32(bus)),
                },
                _ => Ea::Imm(0),
            },
            _ => Ea::Imm(0),
        }
    }

    /// Brief-extension-word indexed addressing: `(d8,An,Xn)` / `(d8,PC,Xn)`.
    fn index_ea(&mut self, bus: &mut Bus, base: u32) -> u32 {
        let ext = self.fetch16(bus);
        let disp = ext as i8 as i32 as u32;
        let xreg = ((ext >> 12) & 0x7) as usize;
        let is_a = ext & 0x8000 != 0;
        let long = ext & 0x0800 != 0;
        let raw = if is_a { self.a[xreg] } else { self.d[xreg] };
        let idx = if long { raw } else { raw as i16 as i32 as u32 };
        base.wrapping_add(disp).wrapping_add(idx)
    }

    fn ea_read(&mut self, bus: &mut Bus, ea: Ea, size: Size) -> u32 {
        match ea {
            Ea::D(r) => self.d[r] & size.mask(),
            Ea::A(r) => self.a[r] & size.mask(),
            Ea::Imm(v) => v & size.mask(),
            Ea::Mem(a) => match size {
                Size::Byte => bus.read8(a) as u32,
                Size::Word => bus.read16(a) as u32,
                Size::Long => bus.read32(a),
            },
        }
    }

    fn ea_write(&mut self, bus: &mut Bus, ea: Ea, size: Size, val: u32) {
        match ea {
            Ea::D(r) => {
                let m = size.mask();
                self.d[r] = (self.d[r] & !m) | (val & m);
            }
            Ea::A(r) => {
                // Address registers are written full-width; word writes sign-extend.
                self.a[r] = match size {
                    Size::Word => val as u16 as i16 as i32 as u32,
                    _ => val,
                };
            }
            Ea::Imm(_) => {}
            Ea::Mem(a) => match size {
                Size::Byte => bus.write8(a, val as u8),
                Size::Word => bus.write16(a, val as u16),
                Size::Long => bus.write32(a, val),
            },
        }
    }

    // ── condition codes ──────────────────────────────────────────────────────
    fn cond(&self, c: u16) -> bool {
        let n = self.flag(FLAG_N);
        let z = self.flag(FLAG_Z);
        let v = self.flag(FLAG_V);
        let cc = self.flag(FLAG_C);
        match c {
            0 => true,                       // T
            1 => false,                      // F
            2 => !cc && !z,                  // HI
            3 => cc || z,                    // LS
            4 => !cc,                        // CC/HS
            5 => cc,                         // CS/LO
            6 => !z,                         // NE
            7 => z,                          // EQ
            8 => !v,                         // VC
            9 => v,                          // VS
            10 => !n,                        // PL
            11 => n,                         // MI
            12 => n == v,                    // GE
            13 => n != v,                    // LT
            14 => !z && (n == v),            // GT
            15 => z || (n != v),             // LE
            _ => false,
        }
    }

    // ── arithmetic primitives (set flags) ────────────────────────────────────
    fn do_add(&mut self, s: u32, d: u32, size: Size, with_x: bool) -> u32 {
        let m = size.mask();
        let x = if with_x && self.flag(FLAG_X) { 1u64 } else { 0 };
        let r = (d as u64 & m as u64) + (s as u64 & m as u64) + x;
        let res = (r as u32) & m;
        let sm = size.msb();
        let carry = r & (m as u64 + 1) != 0;
        let overflow = ((s ^ res) & (d ^ res) & sm) != 0;
        self.set_flag(FLAG_C, carry);
        self.set_flag(FLAG_X, carry);
        self.set_flag(FLAG_V, overflow);
        self.set_flag(FLAG_N, res & sm != 0);
        if with_x {
            if res != 0 {
                self.set_flag(FLAG_Z, false);
            }
        } else {
            self.set_flag(FLAG_Z, res == 0);
        }
        res
    }

    fn do_sub(&mut self, s: u32, d: u32, size: Size, with_x: bool) -> u32 {
        let m = size.mask();
        let x = if with_x && self.flag(FLAG_X) { 1u64 } else { 0 };
        let r = (d as u64 & m as u64)
            .wrapping_sub(s as u64 & m as u64)
            .wrapping_sub(x);
        let res = (r as u32) & m;
        let sm = size.msb();
        let borrow = r & (m as u64 + 1) != 0;
        let overflow = ((s ^ d) & (d ^ res) & sm) != 0;
        self.set_flag(FLAG_C, borrow);
        self.set_flag(FLAG_X, borrow);
        self.set_flag(FLAG_V, overflow);
        self.set_flag(FLAG_N, res & sm != 0);
        if with_x {
            if res != 0 {
                self.set_flag(FLAG_Z, false);
            }
        } else {
            self.set_flag(FLAG_Z, res == 0);
        }
        res
    }

    /// CMP: like SUB but discards the result and never touches X.
    fn do_cmp(&mut self, s: u32, d: u32, size: Size) {
        let m = size.mask();
        let r = (d as u64 & m as u64).wrapping_sub(s as u64 & m as u64);
        let res = (r as u32) & m;
        let sm = size.msb();
        self.set_flag(FLAG_C, r & (m as u64 + 1) != 0);
        self.set_flag(FLAG_V, ((s ^ d) & (d ^ res) & sm) != 0);
        self.set_flag(FLAG_N, res & sm != 0);
        self.set_flag(FLAG_Z, res == 0);
    }

    fn do_logic_flags(&mut self, res: u32, size: Size) {
        self.set_flag(FLAG_C, false);
        self.set_flag(FLAG_V, false);
        self.set_nz(res, size);
    }
}

mod exec;

impl M68k {
    /// Service a GameDrive GDBIOS call. The synthetic BIOS block's dispatch
    /// entries are `trap #n ; rts`, so the 68000 arrives here with the ROM's
    /// register ABI intact (see `gamedrive` and OpenLara's `gdbios.S`):
    ///
    /// | fn | trap | in | out (d0) |
    /// |----|------|----|----|
    /// | INIT   | 1  | —                                   | 0 |
    /// | CARDIN | 9  | —                                   | 1 = card present |
    /// | FOPEN  | 10 | a0=name, d0.w=mode                  | handle, or -1 |
    /// | FCLOSE | 11 | d0.w=handle                         | 0 |
    /// | FREAD  | 13 | d0=(flags<<16)|handle, a0=buf, d1=n | **0 = success** |
    /// | FSIZE  | 0  | d0.w=handle                         | size |
    ///
    /// Returns `None` for a trap we don't own, so it takes the normal path.
    fn gamedrive_trap(&mut self, bus: &mut Bus, trap: u8) -> Option<u32> {
        use crate::gamedrive as gd;
        let (d0, d1, a0) = (self.d[0], self.d[1], self.a[0]);
        if std::env::var_os("JAGEMU_GD_DEBUG").is_some() {
            eprintln!("GDTRAP #{trap} d0={d0:#010X} d1={d1:#010X} a0={a0:#010X}");
        }
        let result = match trap {
            gd::FN_INIT => 0,
            gd::FN_CARDIN => bus.gamedrive.as_ref()?.card_in(),
            gd::FN_FOPEN => {
                let mut name = String::new();
                for i in 0..64u32 {
                    let c = bus.read8(a0.wrapping_add(i));
                    if c == 0 {
                        break;
                    }
                    name.push(c as char);
                }
                bus.gamedrive.as_mut()?.fopen(&name)
            }
            gd::FN_FCLOSE => bus.gamedrive.as_mut()?.fclose(d0 as u16),
            gd::FN_FREAD => match bus.gamedrive.as_mut()?.fread(d0 as u16, d1) {
                Some(data) => {
                    for (i, b) in data.iter().enumerate() {
                        bus.write8(a0.wrapping_add(i as u32), *b);
                    }
                    0 // upstream convention: 0 means SUCCESS, not a byte count
                }
                None => u32::MAX,
            },
            gd::TRAP_FOR_FSIZE => bus.gamedrive.as_ref()?.fsize(d0 as u16),
            _ => return None,
        };
        self.d[0] = result;
        Some(20)
    }
}
