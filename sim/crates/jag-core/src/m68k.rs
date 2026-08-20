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

/// Extra 68k cycles per instruction-FETCH bus cycle, tenths (calibrated).
const M68K_FETCH_WAIT_X10: u32 = 3;
/// Extra 68k cycles per DATA bus cycle, tenths (calibrated).
const M68K_DATA_WAIT_X10: u32 = 5;

/// Longest run of same-address 68000 DRAM operand reads that real silicon
/// survives. **[HW]**, jag_viewpoint 2026-08-18: with the poll count as the
/// only variable and the GPU halted, 16 iterations completed and 1024 stopped
/// (within the first ~64), so the survivable budget is between 16 and ~128.
///
/// The 68000 is the lowest-priority bus master (refresh > OP > Blitter > GPU >
/// DSP > 68k), and a tight poll never yields the bus, so on the machine it can
/// simply stop. **jsim runs such a loop to completion**, which is exactly the
/// class of design error a simulator is supposed to catch: a rehosted game's
/// mailbox handshake or vblank wait passes every emulator and dies on silicon.
///
/// This is reported, not enforced: stalling the core would change behaviour
/// for every project on a threshold whose exact value is not yet measured.
/// `m68k_dram_poll_max` in the run state is the warning.
pub const M68K_DRAM_POLL_BUDGET: u32 = 128;

/// How many DRAM reads by other instructions a poll candidate survives before
/// it is replaced. A compiled poll interleaves its own read with its spilled
/// locals', so this must be greater than the number of other DRAM operands in
/// the loop body — and small enough that a genuine stream of unrelated reads
/// retires a stale candidate. 16 covers every compiled poll seen so far.
const POLL_MISS_TOLERANCE: u32 = 16;

/// ── OBJECT-PROCESSOR TAX ON THE 68000 ───────────────────────────────────────
///
/// The two constants above are a CONSTANT wait per bus cycle. The RISC cores
/// pay that same kind of constant *plus* a LOAD-DEPENDENT charge that scales
/// with how much the Object Processor is fetching this line
/// (`Pipe::charge_op_tax`, `risc/timing.rs`). The 68000 had no load-dependent
/// term at all, so a 68k DRAM loop cost the same whether the OP was scanning a
/// full-width bitmap or drawing nothing.
///
/// That is the wrong way round for this machine: the 68000 is the LOWEST
/// priority bus master (refresh > OP > Blitter > GPU > DSP > 68k), so it is the
/// master that should lose the most when the OP is busy. The symptom is a
/// hardware/simulator divergence with a very specific shape — a 68k poll loop
/// that always makes progress in simulation and stalls out on silicon while the
/// OP is scanning — and no counter in the model could see it, because the model
/// had no term for it.
///
/// ⭐ THE COEFFICIENT IS DERIVED, NOT FITTED. The RISC tax is a hardware
/// calibration of the same physical bus occupancy: `OP_TAX_MILLI_NUM/DEN` =
/// 5.75 milli-ticks per phrase per access, in RISC ticks. The scheduler runs the
/// RISCs at exactly 2x the 68000 clock (`risc_ticks = cpu_cycles * 2`), so the
/// identical occupancy expressed in 68000 cycles is half of it: 2.875
/// milli-cycles per phrase per access. No new constant is being invented, and
/// nothing here was tuned to make a particular program behave.
///
/// Charged on DRAM cycles ONLY (`bus.m68k_dram_cycles`) — Tom and Jerry register
/// accesses are not on the DRAM bus and the OP does not contend for them.
const M68K_OP_TAX_MILLI_NUM: u64 = 2875;
const M68K_OP_TAX_MILLI_DEN: u64 = 1000;

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
    /// Bus cycles spent on instruction FETCH by the current instruction —
    /// subtracted from the bus's total to split fetch from data.
    fetch_ba: u32,
    /// Sub-cycle remainder of the external-bus wait charge (tenths).
    bus_debt: u32,
    /// Fractional Object-Processor tax owed, in milli-cycles (see
    /// `M68K_OP_TAX_MILLI_NUM`). Carried between instructions so a charge
    /// smaller than one cycle per access still accumulates instead of vanishing.
    op_tax_debt: u64,
    /// The (PC, address) pair the 68000 is currently re-reading, and how many
    /// times that same instruction has read that same address. See
    /// `M68K_DRAM_POLL_BUDGET`.
    ///
    /// ☠️ KEYED ON THE PC, NOT ON THE ADDRESS ALONE. The first version required
    /// the run to be *consecutive* same-address reads, which only holds when the
    /// loop body touches exactly ONE DRAM address. Real compiled polls do not:
    /// in a large function the loop's own locals are spilled to the stack, so
    /// `while ((int)(frame_count - t0) < tgt) ;` reads three different addresses
    /// per iteration and the run reset every time. Measured on jag_openlara's
    /// FMV player, which spins on `frame_count` for three fields out of four and
    /// on a decode mailbox up to 240,000 times a frame: the detector reported
    /// `poll_max 2` — the same value it reports for a ROM with no spin at all.
    /// A detector that reads the same on a known-bad and a known-good build is
    /// not a weak detector, it is an absent one.
    poll_pc: u32,
    poll_addr: Option<u32>,
    poll_run: u32,
    /// DRAM reads by OTHER instructions seen since the candidate last matched.
    /// A poll loop interleaves its own read with its locals', so the candidate
    /// has to survive a few misses or it can never establish a run at all.
    poll_miss: u32,
    /// The distinct addresses those other reads touched. ☠️ THIS IS WHAT KEEPS
    /// A BULK COPY FROM READING AS A POLL. Tolerating other reads (above) is
    /// necessary but not sufficient: `for (i=0;i<n;i++) dst[i]=src[i];` with
    /// its locals spilled has a same-PC same-address read every iteration (the
    /// loop counter) while `src[i]` walks — and the first version of this
    /// change reported jag_openlara's GPU-kernel copy as a 555-deep poll.
    /// A poll's other reads are the SAME few slots forever; a streaming loop's
    /// are all different. More distinct addresses than this holds ⇒ progress,
    /// and the candidate is dropped.
    poll_miss_seen: [u32; 4],
    poll_miss_n: u8,
    /// Longest same-instruction/same-address DRAM read run of the whole
    /// session, and one address the offending loop reads. ⚠ When a loop reads
    /// several DRAM operands, each is a repeat of itself, so this names
    /// whichever the detector locked onto first — the LOOP is the finding, not
    /// the address.
    pub poll_max: u32,
    pub poll_max_addr: u32,
    /// PC of the instruction doing the polling. ⭐ This is the half you can
    /// act on: the address alone sends you hunting through a link map, while
    /// the PC lands in a disassembly (or a `--map` symbolisation) on the exact
    /// loop to bound.
    pub poll_max_pc: u32,
    /// 68000 writes that landed nowhere this model maps. `stray_writes` counts
    /// the unmapped hole; `cart_writes` counts stores into cartridge ROM, kept
    /// apart because they are a different mistake. The first of each is kept
    /// with the PC that issued it, which is the part that finds the bug.
    pub stray_writes: u64,
    pub stray_write_addr: u32,
    pub stray_write_pc: u32,
    pub cart_writes: u64,
    /// Whole 68000 cycles lost to Object-Processor bus occupancy since reset.
    /// ⭐ This is the counter the model previously had no way to express: the
    /// 68k had no load-dependent bus term, so there was nothing to count.
    pub op_tax_cycles: u64,
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
            fetch_ba: 0,
            bus_debt: 0,
            op_tax_debt: 0,
            poll_pc: 0,
            poll_addr: None,
            poll_run: 0,
            poll_miss: 0,
            poll_miss_seen: [0; 4],
            poll_miss_n: 0,
            poll_max: 0,
            poll_max_addr: 0,
            poll_max_pc: 0,
            stray_writes: 0,
            stray_write_addr: 0,
            stray_write_pc: 0,
            cart_writes: 0,
            op_tax_cycles: 0,
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
        self.fetch_ba += 1;
        bus.m68k_in_fetch = true;
        let w = bus.read16(self.pc);
        bus.m68k_in_fetch = false;
        self.pc = self.pc.wrapping_add(2);
        w
    }
    #[inline]
    fn fetch32(&mut self, bus: &mut Bus) -> u32 {
        bus.m68k_in_fetch = true;
        let l = bus.read32(self.pc);
        bus.m68k_in_fetch = false;
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
        bus.m68k_bus_cycles = 0;
        bus.m68k_dram_read_addr = None;
        bus.m68k_dram_wrote = false;
        bus.m68k_stray_write = None;
        self.fetch_ba = 0;
        let pc0 = self.pc;
        let was_stopped = self.stopped;
        let c0 = self.step_inner(bus, dbg);

        // External-bus wait charge (HARDWARE-CALIBRATED, see the constants):
        // textbook 68000 timings assume a 4-cycle bus the Jaguar does not give
        // it — every fetch and operand goes off-chip through Tom's memory
        // controller. Fetch and data are charged separately: three probes of
        // different mixes (m68kreg fetch-only, m68kbus long reads, m68kcpy the
        // byte-copy loop) pin two knobs, and the third mix validates the fit.
        // A flat blended constant matched whichever probe calibrated it and
        // was 45% wrong on real programs (wip/m68k-bus-wait, superseded).
        // Same-address DRAM read run — the shape of a mailbox or status poll.
        // A write clears it: a loop that stores is not a pure spin.
        // A write into nowhere, attributed to the instruction that issued it.
        // Recorded here rather than in the bus because only the CPU knows pc0.
        if let Some((a, cart)) = bus.m68k_stray_write {
            if cart != 0 {
                self.cart_writes += 1;
            } else {
                if self.stray_writes == 0 {
                    self.stray_write_addr = a;
                    self.stray_write_pc = pc0;
                }
                self.stray_writes += 1;
            }
        }

        if bus.m68k_dram_wrote {
            // A store means the loop is doing work and yielding the bus.
            self.poll_addr = None;
            self.poll_run = 0;
            self.poll_miss = 0;
            self.poll_miss_n = 0;
        } else if let Some(a) = bus.m68k_dram_read_addr {
            if self.poll_addr == Some(a) && self.poll_pc == pc0 {
                // The same instruction reading the same address again: the
                // signature of a poll, whatever else the loop body touches.
                self.poll_run += 1;
                self.poll_miss = 0;
            } else if self.poll_addr.is_none() || self.poll_miss >= POLL_MISS_TOLERANCE {
                // No candidate, or the one we had has gone quiet — adopt this.
                self.poll_pc = pc0;
                self.poll_addr = Some(a);
                self.poll_run = 1;
                self.poll_miss = 0;
                self.poll_miss_n = 0;
            } else if self.poll_miss_seen[..self.poll_miss_n as usize].contains(&a) {
                // A read we have seen before alongside this candidate — the
                // loop's own spilled locals. Tolerate it.
                self.poll_miss += 1;
            } else if (self.poll_miss_n as usize) < self.poll_miss_seen.len() {
                self.poll_miss_seen[self.poll_miss_n as usize] = a;
                self.poll_miss_n += 1;
                self.poll_miss += 1;
            } else {
                // A FIFTH distinct companion address: this loop is walking
                // memory, not waiting on it. Not a poll — drop the candidate.
                self.poll_addr = None;
                self.poll_run = 0;
                self.poll_miss = 0;
                self.poll_miss_n = 0;
            }
            if self.poll_run > self.poll_max {
                self.poll_max = self.poll_run;
                self.poll_max_addr = self.poll_addr.unwrap_or(a);
                self.poll_max_pc = self.poll_pc;
            }
        }

        let total = std::mem::take(&mut bus.m68k_bus_cycles);
        let dram = std::mem::take(&mut bus.m68k_dram_cycles);
        let fetch = self.fetch_ba.min(total);
        let data = total - fetch;
        self.bus_debt += fetch * M68K_FETCH_WAIT_X10 + data * M68K_DATA_WAIT_X10;
        let extra = self.bus_debt / 10;
        self.bus_debt -= extra * 10;

        // Object-Processor tax: the OP holds DRAM while it scans, and the 68000
        // is the lowest-priority master, so it waits. Accumulated in
        // milli-cycles so a sub-cycle-per-access charge is not rounded away —
        // the same debt trick `charge_op_tax` uses on the RISC side.
        let phrases = bus.tom.op.phrases_per_line as u64;
        let mut op_extra: u32 = 0;
        if phrases > 0 && dram > 0 {
            self.op_tax_debt +=
                dram as u64 * phrases * M68K_OP_TAX_MILLI_NUM / M68K_OP_TAX_MILLI_DEN;
            let whole = self.op_tax_debt / 1000;
            if whole > 0 {
                self.op_tax_debt -= whole * 1000;
                self.op_tax_cycles += whole;
                op_extra = whole as u32;
            }
        }

        self.cycles += (extra + op_extra) as u64;
        let c = c0 + extra + op_extra;

        if dbg.prof.is_some() {
            let in_isr = self.isr_depth > 0;
            if let Some(p) = dbg.prof.as_mut() {
                p.record(pc0, c, in_isr, was_stopped);
            }
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
    /// | FSEEK  | 12 | d0=(flags<<16)|handle, d1=offset    | 0, or -1 |
    /// | FREAD  | 13 | d0=(flags<<16)|handle, a0=buf, d1=n | **0 = success** |
    /// | FTELL  | 15 | d0.w=handle                         | position |
    /// | FSIZE  | 0  | d0.w=handle                         | size |
    /// | ASYNCPOS    | 2 | —                              | dst end (a guess) |
    /// | ASYNCWAIT   | 3 | —                              | 0 (already done) |
    /// | ASYNCACTIVE | 4 | —                              | 0 (never busy) |
    ///
    /// The trap numbers above are NOT the function indices for anything over
    /// 15 — see `gamedrive::FN_TRAP`, which owns the mapping in both
    /// directions. Dispatch is on the FUNCTION, resolved through it.
    ///
    /// Returns `None` for a trap we don't own, so it takes the normal path.
    fn gamedrive_trap(&mut self, bus: &mut Bus, trap: u8) -> Option<u32> {
        use crate::gamedrive as gd;
        let (d0, d1, a0) = (self.d[0], self.d[1], self.a[0]);
        if std::env::var_os("JAGEMU_GD_DEBUG").is_some() {
            eprintln!("GDTRAP #{trap} d0={d0:#010X} d1={d1:#010X} a0={a0:#010X}");
        }
        let result = match gd::fn_of_trap(trap)? {
            // INIT and InitGPURead have no host-side state to set up; the point
            // of publishing InitGPURead is that the hardware-correct call
            // sequence must not fault here. See gamedrive::FN_TRAP.
            gd::FN_INIT | gd::FN_INITGPUREAD => 0,
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
            // GD_FREAD_GPU_ASYNC (flags 2) is METERED when --sd-rate is set:
            // the bytes arrive a frame at a time, so a loader's wait loop is
            // genuinely exercised instead of finding the buffer already full.
            // Every other mode — and rate 0, the default — completes here.
            gd::FN_FREAD
                if (d0 >> 16) == gd::FREAD_GPU_ASYNC as u32
                    && bus.gamedrive.as_ref().is_some_and(|g| g.rate() > 0) =>
            {
                if bus.gamedrive.as_mut()?.fread_async_start(d0 as u16, a0, d1) {
                    0
                } else {
                    u32::MAX
                }
            }
            gd::FN_FREAD => match bus.gamedrive.as_mut()?.fread(d0 as u16, d1) {
                Some(data) => {
                    for (i, b) in data.iter().enumerate() {
                        bus.write8(a0.wrapping_add(i as u32), *b);
                    }
                    bus.gamedrive.as_mut()?.set_async_pos(a0.wrapping_add(d1));
                    0 // upstream convention: 0 means SUCCESS, not a byte count
                }
                None => u32::MAX,
            },
            gd::FN_FSIZE => bus.gamedrive.as_ref()?.fsize(d0 as u16),
            // FSEEK packs the flags into the high half of d0, like FREAD does.
            gd::FN_FSEEK => {
                bus.gamedrive
                    .as_mut()?
                    .fseek(d0 as u16, (d0 >> 16) as u16, d1 as i32)
            }
            gd::FN_FTELL => bus.gamedrive.as_ref()?.ftell(d0 as u16),
            gd::FN_ASYNCPOS => bus.gamedrive.as_ref()?.async_pos(),
            // Blocking wait: deliver everything still outstanding at once. With
            // no rate set nothing is ever outstanding, so this is a no-op.
            gd::FN_ASYNCWAIT => {
                gd::finish_async(bus);
                0
            }
            gd::FN_ASYNCACTIVE => bus.gamedrive.as_ref()?.async_active(),
            _ => return None,
        };
        self.d[0] = result;
        Some(20)
    }
}

#[cfg(test)]
mod poll_tests {
    use super::*;
    use crate::bus::Bus;

    /// Load a program at $4000 and run it, returning the core.
    fn run(prog: &[u8], steps: usize, seed: Option<(u32, u16)>) -> M68k {
        let mut bus = Bus::new();
        for (i, b) in prog.iter().enumerate() {
            bus.write8(0x4000 + i as u32, *b);
        }
        if let Some((addr, v)) = seed {
            bus.write16(addr, v);
        }
        let mut cpu = M68k::new();
        cpu.reset(&mut bus);
        cpu.sr = 0x2700;
        cpu.set_pc(0x4000);
        // The loader above went through the 68k write path; start the
        // measurement from a clean slate.
        cpu.poll_max = 0;
        cpu.poll_max_addr = 0;
        cpu.poll_max_pc = 0;
        let mut dbg = crate::debug::Debugger::new();
        for _ in 0..steps {
            cpu.step(&mut bus, &mut dbg);
        }
        cpu
    }

    /// `run`, but with a0 seeded so a `(a0)+` source walks real memory.
    fn run_with_a0(prog: &[u8], steps: u32, seed: Option<(u32, u16)>, a0: u32) -> M68k {
        let mut bus = Bus::new();
        for (i, b) in prog.iter().enumerate() {
            bus.write8(0x4000 + i as u32, *b);
        }
        if let Some((addr, v)) = seed {
            bus.write16(addr, v);
        }
        let mut cpu = M68k::new();
        cpu.reset(&mut bus);
        cpu.sr = 0x2700;
        cpu.set_pc(0x4000);
        cpu.a[0] = a0;
        cpu.poll_max = 0;
        cpu.poll_max_addr = 0;
        cpu.poll_max_pc = 0;
        let mut dbg = crate::debug::Debugger::new();
        for _ in 0..steps {
            cpu.step(&mut bus, &mut dbg);
        }
        cpu
    }

    /// A mailbox/status spin — the shape that stops the 68000 dead on silicon
    /// while every simulator runs it to completion.
    ///
    ///     loop: move.w ($1000).l,d0
    ///           bne.s  loop
    #[test]
    fn same_address_dram_spin_is_counted() {
        let prog = [0x30, 0x39, 0x00, 0x00, 0x10, 0x00, 0x66, 0xF8];
        let cpu = run(&prog, 40, Some((0x1000, 1)));
        assert!(
            cpu.poll_max >= 15,
            "a 20-iteration spin should register as a long poll run, got {}",
            cpu.poll_max
        );
        assert_eq!(cpu.poll_max_addr, 0x1000, "wrong address blamed");
        assert!(
            cpu.poll_max > M68K_DRAM_POLL_BUDGET / 16,
            "the detector must be sensitive well below the hardware budget"
        );
    }

    /// A write into the unmapped hole. This model drops it; silicon does not
    /// decode every address line, so it can alias onto a live device — which
    /// is how a rehost carrying addresses from the original machine dies on
    /// hardware while every emulator run is clean.
    ///
    ///     move.w #$1234,($500000).l
    #[test]
    fn an_unmapped_write_is_counted_and_attributed() {
        let prog = [0x33, 0xFC, 0x12, 0x34, 0x00, 0x50, 0x00, 0x00];
        let cpu = run(&prog, 1, None);
        assert_eq!(cpu.stray_writes, 1, "an unmapped store must be counted");
        assert_eq!(cpu.stray_write_addr, 0x0050_0000, "wrong address blamed");
        assert_eq!(
            cpu.stray_write_pc, 0x4000,
            "the PC that ISSUED it is the point (the harness loads at $4000)"
        );
    }

    /// ☠ A guard needs a test that proves it BLOCKS, not only one that proves
    /// it passes good input. An odd-address word store is an address error on
    /// real silicon and a silent success here; before this counter existed the
    /// class could not be observed at all, so the assertion that matters is
    /// that a deliberately unaligned store is COUNTED and attributed.
    #[test]
    fn unaligned_word_store_is_counted_with_its_pc() {
        let mut bus = Bus::new();
        let mut cpu = M68k::new();
        let mut dbg = Debugger::default();
        // move.w #$1234,($00100001).l — an ODD destination in DRAM.  ⚠ $33FC, not
        // $31FC: the latter is absolute SHORT and eats only one extension word,
        // so the operand decoded as $1234 and the store was aligned after all.
        let prog: [u16; 4] = [0x33FC, 0x1234, 0x0010, 0x0001];
        for (i, w) in prog.iter().enumerate() {
            let a = 0x4000 + (i as u32) * 2;
            bus.dram[a as usize] = (w >> 8) as u8;
            bus.dram[a as usize + 1] = *w as u8;
        }
        cpu.pc = 0x4000;
        bus.cur_master = crate::bus::Master::Cpu;
        bus.cur_master_pc = cpu.pc;
        cpu.step(&mut bus, &mut dbg);
        assert_eq!(bus.m68k_unaligned, 1, "an odd-address word store must be counted");
        assert_eq!(bus.m68k_unaligned_addr, 0x0010_0001, "wrong address blamed");
        assert_eq!(bus.m68k_unaligned_pc, 0x4000, "wrong PC blamed");
        assert_eq!(bus.m68k_unaligned_pcs, vec![0x4000], "the PC set must carry it");
    }

    /// And the other half: an ALIGNED store must not be counted, or the
    /// counter reads as a permanent alarm and stops meaning anything.
    #[test]
    fn aligned_word_store_is_not_counted() {
        let mut bus = Bus::new();
        let mut cpu = M68k::new();
        let mut dbg = Debugger::default();
        // move.w #$1234,$00100000 — the same store, EVEN destination.
        let prog: [u16; 4] = [0x33FC, 0x1234, 0x0010, 0x0000];
        for (i, w) in prog.iter().enumerate() {
            let a = 0x4000 + (i as u32) * 2;
            bus.dram[a as usize] = (w >> 8) as u8;
            bus.dram[a as usize + 1] = *w as u8;
        }
        cpu.pc = 0x4000;
        bus.cur_master = crate::bus::Master::Cpu;
        bus.cur_master_pc = cpu.pc;
        cpu.step(&mut bus, &mut dbg);
        assert_eq!(bus.m68k_unaligned, 0, "an aligned store must NOT be counted");
    }

    /// ⭐ The case this counter exists for: a Genesis rehost still carrying a
    /// work-RAM address, which the 68000 truncates to 24 bits and lands in the
    /// hole at $FFxxxx. Invisible in every other way — no bus error, no
    /// illegal opcode, and the store simply evaporates.
    ///
    ///     move.b #$56,($FF8000).l
    #[test]
    fn a_stale_genesis_workram_write_is_counted() {
        let prog = [0x13, 0xFC, 0x00, 0x56, 0x00, 0xFF, 0x80, 0x00];
        let cpu = run(&prog, 1, None);
        assert_eq!(cpu.stray_writes, 1, "$FFxxxx is the hole, not DRAM");
    }

    /// Writes that land somewhere real must NOT be reported, or the counter is
    /// noise and gets switched off: DRAM, and Tom's own registers.
    ///
    ///     move.w #$06C7,($F00028).l   ; VMODE
    ///     move.w #$0000,($1C0000).l   ; DRAM
    #[test]
    fn mapped_writes_are_not_stray() {
        let prog = [
            0x33, 0xFC, 0x06, 0xC7, 0x00, 0xF0, 0x00, 0x28, // Tom VMODE
            0x33, 0xFC, 0x00, 0x00, 0x00, 0x1C, 0x00, 0x00, // DRAM
        ];
        let cpu = run(&prog, 2, None);
        assert_eq!(cpu.stray_writes, 0, "Tom and DRAM are mapped");
    }

    /// A store into cartridge ROM goes nowhere too, but it is an ordinary bug
    /// rather than one silicon can turn into a device poke, so it is counted
    /// separately and must not inflate the stray count.
    ///
    ///     move.w #$1111,($900000).l
    #[test]
    fn a_cart_write_is_counted_apart() {
        let prog = [0x33, 0xFC, 0x11, 0x11, 0x00, 0x90, 0x00, 0x00];
        let cpu = run(&prog, 1, None);
        assert_eq!(cpu.cart_writes, 1, "a ROM store is a cart write");
        assert_eq!(cpu.stray_writes, 0, "and must not inflate the stray count");
    }

    /// The same loop shape, but STORING. Not a spin: the bus is released, and
    /// hardware has no trouble with it, so it must not be reported.
    ///
    ///     loop: move.w d0,($1000).l
    ///           bne.s  loop
    #[test]
    fn a_loop_that_writes_is_not_a_spin() {
        let prog = [0x33, 0xC0, 0x00, 0x00, 0x10, 0x00, 0x66, 0xF8];
        let cpu = run(&prog, 40, None);
        assert_eq!(cpu.poll_max, 0, "a storing loop is not a poll");
    }

    /// ☠️ THE REGRESSION THIS DETECTOR WAS BLIND TO (jag_openlara, 2026-08-18).
    /// A poll whose loop body also reads a spilled local — which is every poll
    /// a C compiler emits inside a large function — reset the run on each
    /// iteration and reported `poll_max 2`, indistinguishable from a ROM with
    /// no spin. The run must survive reads from OTHER instructions.
    ///
    ///     loop: move.w ($2000).l,d1     ; a spilled local, different address
    ///           move.w ($1000).l,d0     ; the poll itself — sets the flags
    ///           bne.s  loop
    #[test]
    fn a_poll_survives_other_reads_in_the_same_loop() {
        let prog = [
            0x32, 0x39, 0x00, 0x00, 0x20, 0x00, // move.w ($2000).l,d1
            0x30, 0x39, 0x00, 0x00, 0x10, 0x00, // move.w ($1000).l,d0
            0x66, 0xF2, // bne.s loop
        ];
        let cpu = run(&prog, 60, Some((0x1000, 1)));
        assert!(
            cpu.poll_max >= 15,
            "a spin with a spilled local in the loop must still register, got {}",
            cpu.poll_max
        );
        // With more than one DRAM read in the loop, EVERY one of them is a
        // same-instruction same-address repeat, so any of them is a fair thing
        // to name: the culprit is the non-yielding loop, not one address in it.
        assert!(
            cpu.poll_max_addr == 0x1000 || cpu.poll_max_addr == 0x2000,
            "the blamed address must be one the loop actually reads, got {:#X}",
            cpu.poll_max_addr
        );
    }

    /// ☠️ A BULK COPY IS NOT A POLL, even though its spilled loop counter is a
    /// same-PC same-address read every iteration. Tolerating companion reads
    /// (the test above) is necessary; without also noticing that they WALK,
    /// this reported jag_openlara's GPU-kernel copy loop as a 555-deep poll.
    ///
    ///     loop: move.w ($1000).l,d0     ; stands in for the spilled counter
    ///           move.w (a0)+,d1         ; the copy's source — a NEW address
    ///           bne.s  loop             ; each time round
    #[test]
    fn a_walking_read_is_not_a_poll() {
        let prog = [
            0x30, 0x39, 0x00, 0x00, 0x10, 0x00, // move.w ($1000).l,d0
            0x32, 0x18, //                         move.w (a0)+,d1
            0x66, 0xF6, //                         bne.s loop
        ];
        let cpu = run_with_a0(&prog, 200, Some((0x1000, 1)), 0x2000);
        assert!(
            cpu.poll_max <= M68K_DRAM_POLL_BUDGET,
            "a streaming copy must not be reported as a poll, got {}",
            cpu.poll_max
        );
    }

    /// Instruction fetch is a DRAM read too, since code lives in DRAM. If
    /// fetches counted, every straight-line program would look like a spin.
    #[test]
    fn instruction_fetch_is_not_an_operand_read() {
        // nop x 8, then bra.s back to the top: reads nothing but itself.
        let mut prog = Vec::new();
        for _ in 0..8 {
            prog.extend_from_slice(&[0x4E, 0x71]); // nop
        }
        prog.extend_from_slice(&[0x60, 0xEE]); // bra.s back to the top
        let cpu = run(&prog, 40, None);
        assert_eq!(
            cpu.poll_max, 0,
            "fetches must not be mistaken for operand reads"
        );
    }
}
