//! The Jaguar RISC processor — one ISA, two instances (Tom GPU @ SRAM `$F03000`,
//! Jerry DSP @ SRAM `$F1B000`). See `docs/spec/RISC_ISA.md`.
//!
//! 16-bit instruction word: `opcode[15:10] reg1[9:5] reg2[4:0]`. 64 registers in
//! two banks. Big-endian fetch (a reference backend's startup sets `BIG_INSTR`, so the two
//! code words of a longword execute in natural big-endian order). The GPU
//! executes from its SRAM (the Tom DRAM-execution constraint).
//!
//! Control registers (`*_FLAGS/PC/CTRL/...`) are memory-mapped *and* used by the
//! core; while the core runs they are authoritative in this struct and synced to
//! the device window at start/stop so the 68000 sees consistent values.

use crate::bus::Bus;
use crate::mem;

mod isa;
pub mod timing;

pub use timing::{Fidelity, TimingStats};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RiscKind {
    Gpu,
    Dsp,
}

impl RiscKind {
    pub fn sram_base(self) -> u32 {
        match self {
            RiscKind::Gpu => mem::G_RAM,
            RiscKind::Dsp => mem::D_RAM,
        }
    }
    pub fn sram_size(self) -> u32 {
        match self {
            RiscKind::Gpu => mem::G_RAM_SIZE as u32,
            RiscKind::Dsp => mem::D_RAM_SIZE as u32,
        }
    }
    /// Base of the 32-bit control-register block.
    pub fn ctrl_base(self) -> u32 {
        match self {
            RiscKind::Gpu => 0xF0_2100,
            RiscKind::Dsp => 0xF1_A100,
        }
    }
    pub fn ctrl_addr(self) -> u32 {
        self.ctrl_base() + 0x14
    }
    pub fn pc_addr(self) -> u32 {
        self.ctrl_base() + 0x10
    }
    pub fn flags_addr(self) -> u32 {
        self.ctrl_base() + 0x00
    }
    fn is_dsp(self) -> bool {
        matches!(self, RiscKind::Dsp)
    }
}

pub struct Risc {
    pub kind: RiscKind,
    /// 64 registers: two banks of 32 (REGPAGE / IMASK select).
    pub regs: [[u32; 32]; 2],
    pub pc: u32,
    /// Full flags register: Z/C/N at bits 0/1/2, IMASK bit 3, REGPAGE bit 14, …
    pub flags: u32,
    pub ctrl: u32,
    pub div_remainder: u32,
    pub div_offset: bool, // 16.16 divide mode (DIV_OFFSET)
    /// G_HIDATA for LOADP/STOREP (GPU) — the *settled* value.
    pub hidata: u32,
    /// A LOADP's high long in flight, and the tick it becomes visible. Unlike a
    /// register result, G_HIDATA is NOT scoreboarded on hardware: reading it
    /// inside the load shadow does not stall, it returns the STALE value (a
    /// kernel that reads G_HIDATA two instructions after LOADP renders garbage
    /// on silicon while jsim, which landed it instantly, looked fine). Modeled
    /// with the same load latency the register result gets.
    pub hidata_next: u32,
    pub hidata_ready: u64,
    pub modulo: u32,      // D_MOD for ADDQMOD/SUBQMOD (DSP)
    pub mac: i64,         // MAC accumulator (40-bit on DSP, modeled as i64)
    pub mtxc: u32,
    pub mtxa: u32,
    pub running: bool,
    pub cycles: u64,
    pub instret: u64,
    /// Deferred jump target from a JUMP/JR (applied after the delay slot).
    pub pending_jump: Option<u32>,
    /// Sliding window for the parked-with-GO detector (see
    /// `timing::Stats::park_spin_max`): the low and high PC seen in the current
    /// run, and how many instructions that run has executed.
    park_lo: u32,
    park_hi: u32,
    park_run: u64,
    /// Pending interrupt latches (bits 0-5 = sources). On entry the core pushes
    /// PC to the R31 stack, sets IMASK, and vectors to `sram_base + 16*source`.
    pub int_latch: u32,
    /// Timing model in effect (see [`timing::Fidelity`]). `Functional` is the
    /// pre-truth-layer behavior: 1 instruction = 1 cycle, no hazards.
    pub fidelity: Fidelity,
    /// Pipeline/scoreboard state + stall attribution (timed profiles only).
    pub pipe: timing::Pipeline,
    /// The previous instruction was a JUMP/JR — the current one is its delay
    /// slot (taken or not), for the slot-content lints.
    prev_was_jump: bool,
    /// Ticks the last slice overran its budget by (a stalled instruction can't
    /// be split). Carried into the next slice so wall-clock ↔ core-cycle
    /// coupling stays exact under timed profiles.
    budget_debt: u32,
    /// Total budget ticks ever granted while running (coupling diagnostics:
    /// `cycles` must never exceed this by more than one instruction).
    pub granted: u64,
    /// PC breakpoints for this core (GPU/DSP debugging). When the PC about to
    /// execute is in this set, `run` stops *before* executing it and records the
    /// address in `bp_hit`, so registers can be inspected at that exact point.
    pub breakpoints: std::collections::HashSet<u32>,
    /// Set to the breakpoint PC when `run` stopped on one; the run loop above
    /// (`Jaguar::run_to_frame`) drains it into a stop reason.
    pub bp_hit: Option<u32>,
    /// Exact per-PC cycle/stall profiler. `None` = off (zero hot-loop cost:
    /// the stats snapshot below is only taken when this is armed).
    pub prof: Option<Box<crate::debug::RiscProfile>>,
    /// Consecutive whole frames this core has been running WITHOUT ever
    /// clearing RISCGO. Reset the moment it stops.
    ///
    /// A liveness signal for the "renders in jagemu, black-screens silicon"
    /// class: a per-frame kernel that stops reaching its done flag hangs on
    /// hardware but looks fine here, because jsim happily runs an infinite loop
    /// forever (COBWEB_BUG_jagemu_runs_code_that_hangs_silicon.md).
    ///
    /// Frame-anchored deliberately, not instruction-anchored. A RESIDENT kernel
    /// — OpenLara's DSP poll loop is one — legitimately runs for the whole
    /// program, so "N million instructions without stopping" would fire on it
    /// every run and be ignored within a day. "Still running K frames later"
    /// separates a per-frame kernel that hung from a resident one that is
    /// working as designed, and only the caller knows which it has: hence
    /// `stuck_after_frames`, off unless set.
    pub frames_running: u32,
    /// Warn once when `frames_running` reaches this. `None` = disabled.
    pub stuck_after_frames: Option<u32>,
    /// Set when the threshold was crossed: `(pc, frames)`, captured AT THE
    /// MOMENT it fired. Not read back off `frames_running` later — the core may
    /// have stopped by then, which resets the streak to zero and made an early
    /// version of this report "never cleared RISCGO for 0 consecutive frames".
    pub stuck_at: Option<(u32, u32)>,
}

impl Risc {
    pub fn new(kind: RiscKind) -> Self {
        Risc {
            kind,
            regs: [[0; 32]; 2],
            pc: kind.sram_base(),
            flags: 0,
            ctrl: 0,
            div_remainder: 0,
            div_offset: false,
            hidata: 0,
            hidata_next: 0,
            hidata_ready: 0,
            modulo: 0,
            mac: 0,
            mtxc: 0,
            mtxa: 0,
            running: false,
            cycles: 0,
            instret: 0,
            pending_jump: None,
            park_lo: 0,
            park_hi: 0,
            park_run: 0,
            int_latch: 0,
            fidelity: Fidelity::default(),
            pipe: timing::Pipeline::default(),
            prev_was_jump: false,
            budget_debt: 0,
            granted: 0,
            breakpoints: std::collections::HashSet::new(),
            bp_hit: None,
            prof: None,
            frames_running: 0,
            stuck_after_frames: None,
            stuck_at: None,
        }
    }

    /// Called at each frame boundary. Counts consecutive frames spent running
    /// and reports the first crossing of `stuck_after_frames`.
    pub fn note_frame(&mut self) {
        if !self.running {
            self.frames_running = 0;
            return;
        }
        self.frames_running += 1;
        if let Some(limit) = self.stuck_after_frames {
            if self.frames_running == limit && self.stuck_at.is_none() {
                self.stuck_at = Some((self.pc, self.frames_running));
                eprintln!(
                    "jagemu: WARNING — {:?} has been running for {} consecutive frames \
                     without clearing RISCGO (pc={:#010X}). A per-frame kernel that never \
                     reaches its done flag hangs on real silicon; jsim will spin here \
                     forever. Ignore this if the kernel is resident by design.",
                    self.kind, self.frames_running, self.pc
                );
            }
        }
    }

    /// Arm the per-PC profiler for this core (see `debug::RiscProfile`).
    pub fn arm_profiler(&mut self) {
        self.prof = Some(Box::new(crate::debug::RiscProfile::new(
            self.kind.sram_base(),
            self.kind.sram_size(),
        )));
    }

    pub fn reset(&mut self) {
        self.regs = [[0; 32]; 2];
        self.pc = self.kind.sram_base();
        self.flags = 0;
        self.ctrl = 0;
        self.div_remainder = 0;
        self.div_offset = false;
        self.hidata = 0;
        self.hidata_next = 0;
        self.hidata_ready = 0;
        self.modulo = 0;
        self.mac = 0;
        self.mtxc = 0;
        self.mtxa = 0;
        self.running = false;
        self.cycles = 0;
        self.instret = 0;
        self.pending_jump = None;
        self.int_latch = 0;
        self.pipe.reset();
        self.prev_was_jump = false;
        self.budget_debt = 0;
        // fidelity is a harness setting, not machine state: survives reset.
    }

    /// G_HIDATA as currently *visible*. A LOADP's high long lands with the same
    /// latency as its register half, but unlike a register it is NOT
    /// scoreboarded — reading inside the shadow does not stall, it just sees the
    /// previous (stale) value, exactly as silicon does.
    #[inline]
    pub fn hidata_now(&mut self) -> u32 {
        if self.cycles >= self.hidata_ready {
            self.hidata = self.hidata_next;
        }
        self.hidata
    }

    /// Is `addr` inside this core's local SRAM?
    #[inline]
    fn in_local(&self, addr: u32) -> bool {
        let base = self.kind.sram_base();
        (base..base + self.kind.sram_size()).contains(&addr)
    }

    /// Latch an interrupt from `source` (e.g. I2S = 1). Taken when enabled and
    /// IMASK is clear and the core is running.
    #[inline]
    pub fn raise_int(&mut self, source: u8) {
        self.int_latch |= 1 << source;
    }

    /// If an enabled interrupt is pending and IMASK is clear, enter it: push the
    /// return PC onto the R31 (bank-0) stack, set IMASK, and vector to
    /// `sram_base + 16*source`.
    fn service_interrupt(&mut self, bus: &mut Bus) -> bool {
        if self.flags & mem::IMASK != 0 {
            return false;
        }
        let enabled = (self.flags >> 4) & 0x1F; // INT_ENA0..4
        let active = self.int_latch & enabled;
        if active == 0 {
            return false;
        }
        let source = 31 - active.leading_zeros(); // highest priority first
        let sp = self.regs[0][31].wrapping_sub(4);
        self.regs[0][31] = sp;
        bus.write32(sp, self.pc);
        self.flags |= mem::IMASK;
        self.pc = self.kind.sram_base() + 16 * source;
        true
    }

    // ── flag accessors ───────────────────────────────────────────────────────
    #[inline]
    fn z(&self) -> bool {
        self.flags & mem::ZERO_FLAG != 0
    }
    #[inline]
    fn c(&self) -> bool {
        self.flags & mem::CARRY_FLAG != 0
    }
    #[inline]
    fn n(&self) -> bool {
        self.flags & mem::NEGA_FLAG != 0
    }
    #[inline]
    fn set_z(&mut self, on: bool) {
        self.set_flag(mem::ZERO_FLAG, on);
    }
    #[inline]
    fn set_c(&mut self, on: bool) {
        self.set_flag(mem::CARRY_FLAG, on);
    }
    #[inline]
    fn set_n(&mut self, on: bool) {
        self.set_flag(mem::NEGA_FLAG, on);
    }
    #[inline]
    fn set_flag(&mut self, bit: u32, on: bool) {
        if on {
            self.flags |= bit;
        } else {
            self.flags &= !bit;
        }
    }
    #[inline]
    fn set_zn(&mut self, v: u32) {
        self.set_z(v == 0);
        self.set_n(v & 0x8000_0000 != 0);
    }
    /// Effective bank: IMASK forces bank 0, else REGPAGE selects.
    #[inline]
    pub fn cur_bank(&self) -> usize {
        if self.flags & mem::IMASK != 0 {
            0
        } else if self.flags & mem::REGPAGE != 0 {
            1
        } else {
            0
        }
    }
    #[inline]
    fn reg(&self, b: usize, n: usize) -> u32 {
        self.regs[b][n]
    }
    #[inline]
    fn set_reg(&mut self, b: usize, n: usize, v: u32) {
        self.regs[b][n] = v;
    }

    // ── device-window helpers (control regs live in the device window) ───────
    #[inline]
    fn win_r32(&self, bus: &Bus, addr: u32) -> u32 {
        match self.kind {
            RiscKind::Gpu => bus.tom.win.r32(addr),
            RiscKind::Dsp => bus.jerry.win.r32(addr),
        }
    }
    #[inline]
    fn win_w32(&self, bus: &mut Bus, addr: u32, v: u32) {
        match self.kind {
            RiscKind::Gpu => bus.tom.win.w32(addr, v),
            RiscKind::Dsp => bus.jerry.win.w32(addr, v),
        }
    }

    // ── instruction fetch (big-endian 16-bit words from local/external) ──────
    #[inline]
    fn fetch16(&mut self, bus: &mut Bus) -> u16 {
        let w = bus.read16(self.pc);
        self.pc = self.pc.wrapping_add(2);
        w
    }

    /// RISC data read with control-register interception. Internal control regs
    /// resolve to this struct's authoritative fields; everything else goes to
    /// the bus (which routes SRAM/DRAM/cart correctly).
    fn dread32(&mut self, bus: &mut Bus, addr: u32) -> u32 {
        let base = self.kind.ctrl_base();
        if (base..base + 0x40).contains(&addr) {
            match addr - base {
                0x00 => self.flags,
                0x10 => self.pc,
                0x14 => self.ctrl,
                0x18 if !self.kind.is_dsp() => self.hidata_now(),
                0x18 => self.modulo,
                0x1C => self.div_remainder,
                _ => self.win_r32(bus, addr),
            }
        } else {
            // HARDWARE: the JRISC ignores the low two address bits on a 32-bit
            // access. Masking (rather than honouring the unaligned address) is
            // what makes a misaligned array behave here as it does on silicon.
            if addr & 3 != 0 {
                self.pipe.stats.unaligned_risc32 += 1;
            }
            bus.read32(addr & !3)
        }
    }

    fn dwrite32(&mut self, bus: &mut Bus, addr: u32, val: u32) {
        let base = self.kind.ctrl_base();
        if (base..base + 0x40).contains(&addr) {
            match addr - base {
                0x00 => {
                    // INT_CLR bits (9-13) write-1-clear the interrupt latches
                    // and read back as 0.
                    let clr = (val >> 9) & 0x1F;
                    self.int_latch &= !clr;
                    self.flags = val & !(0x1F << 9);
                }
                0x04 => self.mtxc = val,
                0x08 => self.mtxa = val,
                0x10 => self.pc = val,
                0x14 => {
                    self.ctrl = val;
                    self.win_w32(bus, addr, val);
                    if val & mem::RISCGO == 0 {
                        self.running = false;
                    }
                }
                0x18 if !self.kind.is_dsp() => {
                    self.hidata = val;
                    self.hidata_next = val;
                    self.hidata_ready = 0;
                }
                0x18 => self.modulo = val,
                0x1C => self.div_offset = val & mem::DIV_OFFSET != 0,
                _ => self.win_w32(bus, addr, val),
            }
        } else {
            if addr & 3 != 0 {
                self.pipe.stats.unaligned_risc32 += 1;
            }
            bus.write32(addr & !3, val);   // JRISC ignores the low 2 bits
        }
    }

    /// Run up to `budget` RISC instructions if started (RISCGO). Loads PC/flags
    /// from the device window on a fresh start; syncs them back on exit.
    pub fn run(&mut self, bus: &mut Bus, budget: u32) {
        let ctrl = self.win_r32(bus, self.kind.ctrl_addr());
        self.ctrl = ctrl;
        if ctrl & mem::RISCGO == 0 {
            if self.running {
                self.sync_back(bus);
                self.running = false;
            }
            return;
        }
        if !self.running {
            // Fresh start: the CPU writes PC before setting RISCGO.
            self.pc = self.win_r32(bus, self.kind.pc_addr());
            self.flags = self.win_r32(bus, self.kind.flags_addr());
            self.running = true;
            self.pending_jump = None;
            // A fresh start is NOT in a delay slot. Without this, `prev_was_jump`
            // survives from wherever the core last halted — for a kernel that
            // ends in a `jr` idle spin, that is always true — so the FIRST
            // instruction of every re-kick was counted as being in a jump's
            // delay slot. A kernel whose entry point is the usual `movei` then
            // reported one `slot_movei` per kick: jag_sonic2 read 20 of them,
            // all at its entry `$F03000`, on a build that renders correctly on
            // real silicon. `slot_movei` is documented hardware-fatal, so a
            // false one costs a session chasing a hazard that is not there.
            self.prev_was_jump = false;
        }

        // CPU→RISC forced interrupt: the 68k (or the other RISC) sets FORCEINT0
        // (ctrl bit 2) to interrupt this core on source 0. Latch it and clear the
        // one-shot trigger so it fires exactly once per request. Without this a
        // 68k↔DSP handshake (e.g. Cybermorph polling D_CTRL after FORCEINT0)
        // deadlocks — the DSP never runs its responder ISR.
        if ctrl & mem::FORCEINT0 != 0 {
            self.int_latch |= 1; // interrupt source 0 = CPU/forced
            let cleared = ctrl & !mem::FORCEINT0;
            self.ctrl = cleared;
            self.win_w32(bus, self.kind.ctrl_addr(), cleared);
        }

        // Budget is in RISC clock ticks. Under `Functional` every instruction
        // costs 1 tick (the historical behavior); timed profiles charge real
        // issue + stall costs, so the same wall-clock slice executes fewer
        // instructions — which is the point. A slice can only end on an
        // instruction boundary, so overrun carries forward as debt — without
        // this, expensive instructions (DIV, external fetch) run faster than
        // wall clock and timing measurements skew with slice granularity.
        self.granted += budget as u64;
        // watchpoint attribution: this core drives the bus for the slice
        bus.cur_master = match self.kind {
            RiscKind::Gpu => crate::bus::Master::Gpu,
            RiscKind::Dsp => crate::bus::Master::Dsp,
        };
        let mut spent: u32 = self.budget_debt.min(budget);
        self.budget_debt -= spent;
        while spent < budget {
            if !self.running {
                break;
            }
            if !bus.watches.is_empty() {
                bus.cur_master_pc = self.pc;
            }
            // Service a pending interrupt between instructions (never in a
            // jump's delay slot, which must run before the transfer). Done before
            // the breakpoint check so an interrupt-vector target (e.g. an ISR
            // entry) is caught rather than executed past.
            if self.pending_jump.is_none() {
                self.service_interrupt(bus);
            }
            // PC breakpoint: stop before executing the marked instruction so the
            // caller can inspect registers at exactly that point. Not taken in a
            // delay slot (the transfer must complete first).
            if !self.breakpoints.is_empty()
                && self.pending_jump.is_none()
                && self.breakpoints.contains(&self.pc)
            {
                self.bp_hit = Some(self.pc);
                break;
            }
            let c = self.step_one(bus);
            // The GPU's own execution IS wall time for the in-flight blit —
            // drain here so a bwait poll loop observes completion. (The DSP
            // does not drain: when both run, the scheduler interleaves them
            // over the same wall clock and draining twice would finish blits
            // at double speed.)
            if self.kind == RiscKind::Gpu {
                bus.tom.blit_busy = bus.tom.blit_busy.saturating_sub(c as u64);
            bus.tom.blit_settle = bus.tom.blit_settle.saturating_sub(c as u64);
            }
            spent += c;
            // The core can stop itself by clearing RISCGO via a STORE.
            if self.ctrl & mem::RISCGO == 0 {
                self.running = false;
            }
        }
        self.budget_debt += spent - budget.min(spent);
        self.sync_back(bus);
    }

    /// Execute one instruction (plus its delay-slot/jump bookkeeping) and
    /// return its cost in RISC ticks.
    fn step_one(&mut self, bus: &mut Bus) -> u32 {
        let was_pending = self.pending_jump.take();
        let in_slot = self.prev_was_jump;
        // Profiler: remember the PC that ISSUES this instruction (not the PC
        // after it) and the stall counters before it runs, so the deltas below
        // attribute every stalled tick to the instruction that paid for it.
        let pc0 = self.pc;
        let stats0 = self.prof.as_ref().map(|_| self.pipe.stats.clone());
        let mut cost = if self.fidelity == Fidelity::Functional {
            let iw = self.fetch16(bus);
            self.prev_was_jump = matches!((iw >> 10) & 0x3F, 52 | 53);
            isa::execute(self, bus, iw);
            1
        } else {
            self.step_timed(bus, in_slot)
        };
        self.instret += 1;
        // Apply a jump issued by the *previous* instruction, now that this
        // (delay-slot) instruction has run.
        if let Some(target) = was_pending {
            self.pc = target;
            if self.fidelity != Fidelity::Functional {
                self.pipe.taken_jump();
                cost += timing::Lat::JUMP_REFILL;
            }
        }
        self.cycles += cost as u64;
        if let Some(s0) = stats0 {
            // Refill is charged here, on the delay slot, because that is where
            // the ticks are actually spent — the slot instruction runs and then
            // the pipe refills. The jump that caused it is the preceding
            // instruction (slot PC - 2, or -6 when the jump was a MOVEI-formed
            // absolute), which is how a `jump_refill` row should be read.
            let d = Self::prof_delta(&s0, &self.pipe.stats, cost);
            if let Some(p) = self.prof.as_mut() {
                p.record(pc0, &d);
            }
        }
        cost
    }

    /// Per-instruction slice of the core's stall counters.
    fn prof_delta(
        a: &timing::TimingStats,
        b: &timing::TimingStats,
        cost: u32,
    ) -> crate::debug::RiscRow {
        crate::debug::RiscRow {
            cycles: cost as u64,
            instrs: 1,
            stall_alu: b.stall_alu - a.stall_alu,
            stall_load: b.stall_load - a.stall_load,
            stall_div: b.stall_div - a.stall_div,
            stall_flags: b.stall_flags - a.stall_flags,
            stall_div_busy: b.stall_div_busy - a.stall_div_busy,
            jump_refill: b.jump_refill - a.jump_refill,
            fetch_external: b.fetch_external - a.fetch_external,
            mem_external: b.mem_external - a.mem_external,
            blit_wait: b.blit_wait - a.blit_wait,
            contention: b.contention - a.contention,
        }
    }

    /// The timed step: scoreboard stalls, hazard modeling, memory costs.
    /// See `timing.rs` for the model and its sources.
    fn step_timed(&mut self, bus: &mut Bus, in_slot: bool) -> u32 {
        use timing::{Fidelity as F, MemClass, PendKind};
        let now = self.cycles;
        let b = self.cur_bank();
        let bcmd_snap = bus.bcmd_busy_reads.load(std::sync::atomic::Ordering::Relaxed);
        self.pipe.settle(now, &mut self.regs, self.fidelity);

        // Peek the instruction word (side-effect-free: code lives in RAM).
        let iw = bus.read16(self.pc);
        let op = ((iw >> 10) & 0x3F) as u8;
        let r1 = ((iw >> 5) & 0x1F) as usize;
        let r2 = (iw & 0x1F) as usize;
        self.prev_was_jump = matches!(op, 52 | 53);

        // ── PARKED-WITH-GO DETECTOR ─────────────────────────────────────────
        // A core still executing inside a <=4-byte window is spinning on
        // itself, and a spinning core has NOT released the bus. See
        // `Stats::park_spin_max` for the hardware bisect and the fix.
        // A window rather than "jr to self" so it catches the equivalent
        // shapes too (a 2-instruction mailbox wait holds the bus just as hard).
        const PARK_SPAN: u32 = 4;
        if self.park_run == 0 {
            self.park_lo = self.pc;
            self.park_hi = self.pc;
            self.park_run = 1;
        } else {
            let lo = self.park_lo.min(self.pc);
            let hi = self.park_hi.max(self.pc);
            if hi.wrapping_sub(lo) <= PARK_SPAN {
                self.park_lo = lo;
                self.park_hi = hi;
                self.park_run += 1;
            } else {
                self.park_lo = self.pc;
                self.park_hi = self.pc;
                self.park_run = 1;
            }
        }
        if self.park_run > self.pipe.stats.park_spin_max {
            self.pipe.stats.park_spin_max = self.park_run;
        }

        if in_slot {
            if op == 38 {
                self.pipe.stats.slot_movei += 1;
                if std::env::var_os("JSIM_HAZARD_TRACE").is_some() {
                    eprintln!("HAZARD slot_movei pc={:#010X}", self.pc);
                }
            }
            if op == 52 || op == 53 {
                self.pipe.stats.slot_jump += 1;
                if std::env::var_os("JSIM_HAZARD_TRACE").is_some() {
                    eprintln!("HAZARD slot_jump pc={:#010X}", self.pc);
                }
            }
        }

        // Issue cost: base + external fetch + addressing overheads.
        let contended = bus.m68k_on_bus;
        let mut cost: u32 = if op == 38 { timing::Lat::MOVEI_ISSUE } else { 1 };
        if !self.in_local(self.pc) {
            let words = if op == 38 { 3 } else { 1 };
            cost += self.pipe.fetch_cost(self.pc, words, self.pc < mem::DRAM_END, contended);
        }
        match op {
            43 | 44 | 58 | 59 => cost += timing::Lat::IDX_LOAD_ISSUE,
            49 | 50 | 60 | 61 => cost += timing::Lat::IDX_STORE_ISSUE,
            54 => cost += self.mtxc & 0xF, // MMULT: one multiply per tick
            _ => {}
        }

        // Scoreboard: stall until read operands / flags are ready.
        let access = timing::classify(iw, self.kind.is_dsp());
        cost += self.pipe.operand_stall(&access, b, now, self.fidelity) as u32;
        if op == 21 {
            cost += self.pipe.div_stall(now) as u32;
        }
        // Pendings that became ready while we stalled must land before the
        // instruction reads its (settled) operands.
        self.pipe.settle(now + cost as u64, &mut self.regs, self.fidelity);

        // Effective address for memory ops (operands are settled/final here —
        // mirrors the address arithmetic in `isa::execute`).
        let q1 = if r1 == 0 { 32u32 } else { r1 as u32 };
        let q2 = if r2 == 0 { 32u32 } else { r2 as u32 };
        let s_val = self.reg(b, r1);
        let is_gpu = !self.kind.is_dsp();
        let ea: Option<u32> = match op {
            39 | 40 | 41 | 45 | 46 | 47 => Some(s_val),
            42 | 48 if is_gpu => Some(s_val),
            43 => Some(self.reg(b, 14).wrapping_add(q1 * 4)),
            44 => Some(self.reg(b, 15).wrapping_add(q1 * 4)),
            // Indexed STORE: offset in reg1 (like the LOAD), data in reg2.
            49 => Some(self.reg(b, 14).wrapping_add(q1 * 4)),
            50 => Some(self.reg(b, 15).wrapping_add(q1 * 4)),
            58 => Some(self.reg(b, 14).wrapping_add(s_val)),
            59 => Some(self.reg(b, 15).wrapping_add(s_val)),
            60 => Some(self.reg(b, 14).wrapping_add(s_val)),
            61 => Some(self.reg(b, 15).wrapping_add(s_val)),
            _ => None,
        };
        let mclass = ea.map(|a| {
            timing::mem_class(a, self.kind.sram_base(), self.kind.sram_size(), self.kind.ctrl_base())
        });
        let is_load =
            matches!(op, 39 | 40 | 41 | 43 | 44 | 58 | 59) || (op == 42 && is_gpu);

        // External accesses (loads AND stores) pay issue-side bus occupancy —
        // HARDWARE: lddram/stdram quiet-bus streams both measured ~+1/access.
        // Loads additionally carry extra result latency into the scoreboard.
        let mut ext_load_lat: u32 = 0;
        // Internal-class accesses go through too: ext_access charges the
        // Blitter-register block ($F022xx) its measured extra bus cycle and
        // returns (0,0) for everything else internal (p_bcmdidle, 2026-07-21).
        if let Some(c) = mclass {
            let (occ, lat) = self.pipe.ext_access(c, ea.unwrap(), contended, now);
            cost += occ;
            if is_load {
                ext_load_lat = lat;
                // Row-thrash contention hits loads only (stores: write-
                // buffered, HARDWARE stdram A == B). ext_access reports the
                // quiet page-hit cost; add the tax here.
                // (flat 68k occupancy tax retired 2026-07-22: the density
                // sweep showed burst-window-only contention, charged in
                // ext_access.) A CONSUMED load under an active 68k still pays
                // extra RESULT latency — the consume stall releases the bus
                // and re-acquisition is slow. HARDWARE: lddramc A−B = 2.7
                // cyc/unit ≈ +8 on the load's latency (unconsumed streams,
                // dens* and lddram, show no such term: A==B there).
                if contended && c == MemClass::Dram {
                    ext_load_lat += 8;
                }
            }
            // Object Processor scan-out steals DRAM cycles from both RISCs every
            // visible line (HARDWARE-CALIBRATED; see OP_TAX_MILLI_*). Unlike the
            // 68k tax this applies to stores too — the OP occupies the bus, it
            // does not thrash a row.
            if c == MemClass::Dram {
                let optax = self.pipe.charge_op_tax(bus.tom.op.phrases_per_line);
                cost += optax;
                self.pipe.note_dram_stretch(optax as u64);
            }
        }

        // TRM errata §2: indexed stores don't scoreboard their DATA register.
        // Under `Silicon` the store writes the stale (pre-producer) value.
        let mut stale_subst: Option<(usize, u32)> = None;
        if timing::is_indexed_store(op) {
            // The data register is reg2 (offset is reg1); the erratum leaves that
            // data register un-scoreboarded.
            let id = (b * 32 + r2) as u8;
            if let Some(stale) = self.pipe.indexed_store_stale_value(id, now + cost as u64) {
                if self.fidelity == F::Silicon {
                    stale_subst = Some((r2, self.regs[b][r2]));
                    self.regs[b][r2] = stale;
                }
            }
        }

        // Bug-13 WAW detection (and dirty-marking for the Silicon landing).
        let write_id = access.write.map(|w| {
            let wb = if access.write_alt_bank { 1 - b } else { b };
            (wb * 32) as u8 + w
        });
        let old_dest = write_id.map(|id| self.regs[(id >> 5) as usize][(id & 31) as usize]);
        if let Some(id) = write_id {
            self.pipe.record_write(id, now + cost as u64);
        }

        let iw2 = self.fetch16(bus);
        debug_assert_eq!(iw, iw2);
        isa::execute(self, bus, iw2);

        if let Some((reg, saved)) = stale_subst {
            self.regs[b][reg] = saved;
        }

        // Result-ready bookkeeping. Latencies anchor to the last issue tick
        // (`end - 1`) — see the convention note on `timing::Lat`.
        let end = now + cost as u64;
        if let (Some(id), Some(old)) = (write_id, old_dest) {
            let newv = self.regs[(id >> 5) as usize][(id & 31) as usize];
            if op == 21 {
                let ready = end - 1 + timing::Lat::DIV;
                self.pipe.push_slow(id, ready, PendKind::Div, newv, old);
                self.pipe.set_div_busy(ready);
            } else if is_load {
                // Indexed loads land one cycle later than plain loads
                // (HARDWARE: ldidx pair = 6 vs ldsram pair = 3).
                let idx_extra = matches!(op, 43 | 44 | 58 | 59) as u64;
                let (kind, lat) = match mclass {
                    Some(MemClass::Internal) | None => {
                        (PendKind::Load, timing::Lat::LOAD_INTERNAL + idx_extra)
                    }
                    _ => (
                        PendKind::ExtLoad,
                        timing::Lat::LOAD_INTERNAL + idx_extra + ext_load_lat as u64,
                    ),
                };
                self.pipe.push_slow(id, end - 1 + lat, kind, newv, old);
                // LOADP's high long (G_HIDATA) lands with the same latency as
                // the register half, but unscoreboarded — a read before this
                // tick sees the stale value, as on silicon.
                if op == 42 && is_gpu {
                    self.hidata_ready = end - 1 + lat;
                }
            } else if !matches!(op, 34 | 35 | 36 | 37 | 38 | 51 | 19) {
                // ALU-class results are written at cycle 3: one bubble for an
                // immediate consumer. MOVE-class (cycle 2) never stalls.
                self.pipe.push_slow(id, end - 1 + timing::Lat::ALU, PendKind::Alu, newv, old);
            }
        }
        if access.sets_flags {
            self.pipe.set_flags_ready(end - 1 + timing::Lat::ALU);
        }

        // A blit launched by this instruction (a store to B_CMD) runs
        // ASYNCHRONOUSLY: blit::run put its duration in bus.tom.blit_busy, and
        // B_CMD reads report busy until it drains. The launch itself costs the
        // GPU nothing beyond the store — real kernels (gpu_geotex) overlap the
        // next span's DDA math with the blit and only bwait at the next launch.
        // Charging the full duration here (the old model) serialized that
        // overlap and over-billed the frame 2.4x vs silicon's NOFILL delta.
        // stats.blit still accumulates the duration: it is "Blitter busy
        // time", the number the wall-clock accounting reports.
        let blit_ticks = std::mem::take(&mut bus.tom.last_blit_ticks);
        if blit_ticks > 0 {
            let launch = std::mem::take(&mut bus.tom.last_blit_launch);
            self.pipe.stats.blit += blit_ticks;
            self.pipe.stats.blit_count += 1;
            self.pipe.stats.blit_launch += launch;
            self.pipe.stats.blit_transfer += blit_ticks - launch;
        }
        // bwait attribution: if this instruction read B_CMD and saw BUSY, its
        // cost is spin time — book it so the split can be silicon-checked.
        if bus.bcmd_busy_reads.load(std::sync::atomic::Ordering::Relaxed) != bcmd_snap {
            self.pipe.stats.blit_wait += cost as u64;
        }
        cost
    }

    /// Write PC/flags back to the device window so the 68000 reads current state.
    fn sync_back(&self, bus: &mut Bus) {
        let pc_addr = self.kind.pc_addr();
        let flags_addr = self.kind.flags_addr();
        let (pc, flags) = (self.pc, self.flags);
        match self.kind {
            RiscKind::Gpu => {
                bus.tom.win.w32(pc_addr, pc);
                bus.tom.win.w32(flags_addr, flags);
            }
            RiscKind::Dsp => {
                bus.jerry.win.w32(pc_addr, pc);
                bus.jerry.win.w32(flags_addr, flags);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Encode a RISC instruction word: opcode[15:10] reg1[9:5] reg2[4:0].
    fn enc(op: u16, r1: u16, r2: u16) -> u16 {
        (op << 10) | (r1 << 5) | r2
    }

    /// Upload `words` to GPU SRAM, point G_PC there, set RISCGO, run `budget`.
    fn run_gpu(words: &[u16], budget: u32) -> Bus {
        let mut bus = Bus::new();
        let base = mem::G_RAM;
        for (i, &w) in words.iter().enumerate() {
            bus.write16(base + (i as u32) * 2, w);
        }
        bus.write32(mem::G_PC, base);
        bus.write32(mem::G_CTRL, mem::RISCGO);
        let mut gpu = Risc::new(RiscKind::Gpu);
        gpu.run(&mut bus, budget);
        bus
    }

    /// A kernel that is re-kicked must not report its ENTRY instruction as
    /// living in a jump's delay slot.
    ///
    /// Regression: `run()` cleared `pending_jump` on a fresh start but left
    /// `prev_was_jump` set from wherever the core last halted. A kernel that
    /// ends in a `jr` idle spin — the normal shape — therefore reported one
    /// `slot_movei` per kick, because the usual entry instruction is a `movei`.
    /// jag_sonic2 read 20 of them, all at its entry `$F03000`, on a build that
    /// renders correctly on real silicon. `slot_movei` is documented
    /// hardware-fatal, so a false one costs a session chasing a hazard that is
    /// not there.
    #[test]
    fn a_re_kicked_kernel_entry_is_not_in_a_delay_slot() {
        let base = mem::G_RAM;
        // jump (r0) ; nop      — halts the core with prev_was_jump set, exactly
        //                        as an idle spin's back-edge leaves it.
        // then, on the NEXT kick: movei #0,r1 at the entry.
        let spin = [enc(52, 0, 0), enc(57, 0, 0)];
        let entry = [enc(38, 0, 1), 0x0000, 0x0000, enc(57, 0, 0)];

        let mut bus = Bus::new();
        for (i, &w) in spin.iter().enumerate() {
            bus.write16(base + 0x100 + (i as u32) * 2, w);
        }
        for (i, &w) in entry.iter().enumerate() {
            bus.write16(base + (i as u32) * 2, w);
        }
        let mut gpu = Risc::new(RiscKind::Gpu);

        // First kick: run the jump so prev_was_jump is left set, then halt.
        bus.write32(mem::G_PC, base + 0x100);
        bus.write32(mem::G_CTRL, mem::RISCGO);
        gpu.run(&mut bus, 2);
        bus.write32(mem::G_CTRL, 0);
        gpu.run(&mut bus, 1); // observe GO low -> running = false

        // Second kick at the entry `movei`. That is a fresh start, not a slot.
        bus.write32(mem::G_PC, base);
        bus.write32(mem::G_CTRL, mem::RISCGO);
        gpu.run(&mut bus, 4);

        assert_eq!(
            gpu.pipe.stats.slot_movei, 0,
            "a re-kicked kernel's entry movei was counted as being in a delay slot"
        );
    }

    #[test]
    fn movei_and_store_to_dram() {
        // movei #$00100000,r1 ; movei #$CAFEBABE,r2 ; store r2,(r1)
        let prog = [
            enc(38, 0, 1), 0x0000, 0x0010, // movei #$00100000,r1
            enc(38, 0, 2), 0xBABE, 0xCAFE, // movei #$CAFEBABE,r2 (LE word order)
            enc(47, 1, 2), // store r2,(r1): addr=reg1=r1, data=reg2=r2
            enc(57, 0, 0), // nop
        ];
        let mut bus = run_gpu(&prog, 16);
        assert_eq!(bus.read32(0x0010_0000), 0xCAFE_BABE);
    }

    #[test]
    fn arithmetic_add_and_shift() {
        // r1=5; r2=3; add r1,r2 (r2=8); shlq #2,r2 (r2=32); store r2,(addr)
        let prog = [
            enc(35, 5, 1),  // moveq #5,r1
            enc(35, 3, 2),  // moveq #3,r2
            enc(0, 1, 2),   // add r1,r2  -> r2=8
            enc(24, 32 - 2, 2), // shlq #2,r2 -> 32 (count encoded 32-n)
            enc(38, 0, 3), 0x0000, 0x0010, // movei #$00100000,r3
            enc(47, 3, 2),  // store r2,(r3)
            enc(57, 0, 0),
        ];
        let mut bus = run_gpu(&prog, 16);
        assert_eq!(bus.read32(0x0010_0000), 32);
    }

    #[test]
    fn jr_loop_sums_1_to_5() {
        // Sum 1..5 using a JR loop with a delay slot.
        //  r1=0 (acc); r2=5 (counter)
        // loop: add r2,r1 ; subq #1,r2 ; cmpq #0,r2 ; jr NE,loop ; nop(delay)
        // then: movei #addr,r3 ; store r1,(r3)
        let prog = [
            enc(35, 0, 1),       // 0: moveq #0,r1
            enc(35, 5, 2),       // 1: moveq #5,r2
            // loop @ word index 2:
            enc(0, 2, 1),        // 2: add r2,r1
            enc(6, 1, 2),        // 3: subq #1,r2
            enc(31, 0, 2),       // 4: cmpq #0,r2  (sets Z when r2==0)
            enc(53, (-4i16 as u16) & 0x1F, 0x01), // 5: jr NE,(-4 words) ; cc=01 (NZ)
            enc(57, 0, 0),       // 6: nop (delay slot)
            enc(38, 0, 3), 0x0000, 0x0010, // 7: movei #$00100000,r3
            enc(47, 3, 1),       // 10: store r1,(r3)
            enc(57, 0, 0),
        ];
        let mut bus = run_gpu(&prog, 200);
        assert_eq!(bus.read32(0x0010_0000), 15); // 5+4+3+2+1
    }

    // ── jsim truth layer (timed profiles) ───────────────────────────────────

    /// Append a self-stop (store 0 → G_CTRL) so timed programs halt cleanly
    /// instead of running the budget out through zeroed SRAM. Uses r29/r30 —
    /// timed tests must avoid those.
    fn with_stop(prog: &[u16]) -> Vec<u16> {
        let mut v = prog.to_vec();
        v.extend_from_slice(&[
            enc(38, 0, 30),
            (mem::G_CTRL & 0xFFFF) as u16,
            (mem::G_CTRL >> 16) as u16,
            enc(35, 0, 29),
            enc(47, 30, 29),
            enc(57, 0, 0),
        ]);
        v
    }

    /// Upload `words` at `base`, apply `pre` bus writes, run under `fid`.
    fn run_fid(words: &[u16], base: u32, budget: u32, fid: Fidelity, pre: &[(u32, u32)]) -> (Bus, Risc) {
        let mut bus = Bus::new();
        for &(a, v) in pre {
            bus.write32(a, v);
        }
        for (i, &w) in words.iter().enumerate() {
            bus.write16(base + (i as u32) * 2, w);
        }
        bus.write32(mem::G_PC, base);
        bus.write32(mem::G_CTRL, mem::RISCGO);
        let mut gpu = Risc::new(RiscKind::Gpu);
        gpu.fidelity = fid;
        gpu.run(&mut bus, budget);
        (bus, gpu)
    }

    /// A dependent ALU pair pays exactly the one-cycle bubble (TRM p.62);
    /// interleaving two chains pays nothing — the canonical scheduling win.
    #[test]
    fn timed_alu_dependency_bubble() {
        let dep = with_stop(&[
            enc(35, 1, 1), // moveq #1,r1
            enc(35, 0, 2), // moveq #0,r2
            enc(35, 0, 3), // moveq #0,r3
            enc(0, 1, 2),  // add r1,r2
            enc(0, 2, 3),  // add r2,r3  ← reads r2 one tick after it's produced
        ]);
        let (_, gpu) = run_fid(&dep, mem::G_RAM, 500, Fidelity::Silicon, &[]);
        assert_eq!(gpu.pipe.stats.stall_alu, 1);
        assert_eq!(gpu.pipe.stats.waw_hazards, 0);

        let interleaved = with_stop(&[
            enc(35, 1, 1),
            enc(35, 1, 3),
            enc(0, 1, 2), // add r1,r2
            enc(0, 3, 4), // add r3,r4  ← independent, fills the bubble
            enc(0, 1, 2), // add r1,r2  ← r2 ready by now
            enc(0, 3, 4),
        ]);
        let (_, gpu) = run_fid(&interleaved, mem::G_RAM, 500, Fidelity::Silicon, &[]);
        assert_eq!(gpu.pipe.stats.stall_alu, 0);
    }

    /// ☠ A kernel PARKED ON `jr .idle` WITH GO STILL SET is counted, because
    /// on real Tom it never stops arbitrating for the bus and the 68000 starves
    /// until VI service stops — a frozen picture with no exception and no crash
    /// handler. `jag_s3k` bisected it on silicon; `jag_sonic2` had the same
    /// failure (a title screen that rendered perfectly and never advanced).
    ///
    /// No bus-cycle model can see it: `jr .idle` executes from the core's own
    /// internal SRAM and makes no external access at all. Validated against the
    /// real defect in `jag_sonic2`'s compositor, both directions:
    /// parked 195,529 · clearing its own GO 3.
    #[test]
    fn park_spin_flags_a_kernel_that_never_releases_the_bus() {
        // `.idle: jr .idle` — a branch to itself (target = pc + 2 + disp*2, so
        // disp = -1), with a nop in its single delay slot. Runs to the budget
        // because nothing will ever stop it.
        let parked = [enc(53, (-1i16 as u16) & 0x1F, 0), enc(57, 0, 0)];
        let (_, gpu) = run_fid(&parked, mem::G_RAM, 4000, Fidelity::Silicon, &[]);
        assert!(
            gpu.pipe.stats.park_spin_max > 1000,
            "a self-parked kernel must be flagged, got {}",
            gpu.pipe.stats.park_spin_max
        );

        // The same work, ended the way silicon wants: clear GO and stop. The
        // park becomes unreachable, so the counter stays small.
        let stopped = with_stop(&[
            enc(35, 1, 1), // moveq #1,r1
            enc(35, 2, 2), // moveq #2,r2
            enc(0, 1, 2),  // add  r1,r2
        ]);
        let (_, gpu2) = run_fid(&stopped, mem::G_RAM, 4000, Fidelity::Silicon, &[]);
        assert!(
            gpu2.pipe.stats.park_spin_max < 100,
            "a kernel that clears its own GO must NOT be flagged, got {}",
            gpu2.pipe.stats.park_spin_max
        );
    }

    /// DIV's quotient lands at cycle 18: an immediate consumer stalls 17
    /// ticks; 17 independent instructions in the shadow make it free.
    #[test]
    fn timed_div_shadow() {
        let naive = with_stop(&[
            enc(35, 20, 2), // moveq #20,r2
            enc(35, 3, 1),  // moveq #3,r1
            enc(21, 1, 2),  // div r1,r2 → 6
            enc(34, 2, 3),  // move r2,r3 ← immediate quotient read
        ]);
        let (_, gpu) = run_fid(&naive, mem::G_RAM, 500, Fidelity::Silicon, &[]);
        assert_eq!(gpu.pipe.stats.stall_div, 17);
        assert_eq!(gpu.regs[0][3], 6);

        let mut shadowed = vec![enc(35, 20, 2), enc(35, 3, 1), enc(21, 1, 2)];
        shadowed.extend(std::iter::repeat(enc(57, 0, 0)).take(17)); // shadow work
        shadowed.push(enc(34, 2, 3));
        let (_, gpu) = run_fid(&with_stop(&shadowed), mem::G_RAM, 500, Fidelity::Silicon, &[]);
        assert_eq!(gpu.pipe.stats.stall_div, 0);
        assert_eq!(gpu.regs[0][3], 6);
    }

    /// Bug 13: writes are unprotected. A fast write racing a pending external
    /// load loses — the load lands LAST and the register holds its value.
    /// `Functional` keeps the naive (wrong-on-silicon) result.
    #[test]
    fn timed_waw_bug13_landing_order() {
        let mut prog = vec![
            enc(38, 0, 3),
            0x0000,
            0x0010,        // movei #$00100000,r3
            enc(41, 3, 2), // load (r3),r2      ← slow external load
            enc(35, 3, 2), // moveq #3,r2       ← races it: the bug-13 WAW
        ];
        prog.extend(std::iter::repeat(enc(57, 0, 0)).take(16)); // let it land
        prog.extend([
            enc(38, 0, 4),
            0x0010,
            0x0010,        // movei #$00100010,r4
            enc(47, 4, 2), // store r2,(r4)
        ]);
        let prog = with_stop(&prog);
        let pre = [(0x0010_0000u32, 0xDEAD_BEEFu32)];

        let (mut bus, gpu) = run_fid(&prog, mem::G_RAM, 800, Fidelity::Silicon, &pre);
        assert_eq!(gpu.pipe.stats.waw_hazards, 1);
        assert_eq!(bus.read32(0x0010_0010), 0xDEAD_BEEF, "slow write must land last");

        let (mut bus, _) = run_fid(&prog, mem::G_RAM, 800, Fidelity::Functional, &pre);
        assert_eq!(bus.read32(0x0010_0010), 3, "functional keeps the naive result");
    }

    /// TRM errata §2: indexed stores don't scoreboard their DATA register —
    /// storing a quotient through (R14+n) right after DIV writes the STALE
    /// value. A dependent touch (`or r2,r2`) first makes it correct.
    #[test]
    fn timed_indexed_store_erratum() {
        let broken = with_stop(&[
            enc(38, 0, 14),
            0x0000,
            0x0010,         // movei #$00100000,r14
            enc(35, 20, 2), // moveq #20,r2
            enc(35, 3, 1),  // moveq #3,r1
            enc(21, 1, 2),  // div r1,r2 → 6 (r2 was 20)
            enc(49, 1, 2),  // store r2,(r14+1): offset=reg1=1, data=reg2=r2 ← unprotected
        ]);
        let (mut bus, gpu) = run_fid(&broken, mem::G_RAM, 500, Fidelity::Silicon, &[]);
        assert_eq!(gpu.pipe.stats.indexed_store_stale, 1);
        assert_eq!(bus.read32(0x0010_0004), 20, "stale pre-divide value stored");

        let fixed = with_stop(&[
            enc(38, 0, 14),
            0x0000,
            0x0010,
            enc(35, 20, 2),
            enc(35, 3, 1),
            enc(21, 1, 2),
            enc(10, 2, 2), // or r2,r2 — dependent touch, stalls till quotient
            enc(49, 1, 2), // store r2,(r14+1): offset=reg1=1, data=reg2=r2
        ]);
        let (mut bus, gpu) = run_fid(&fixed, mem::G_RAM, 500, Fidelity::Silicon, &[]);
        assert_eq!(gpu.pipe.stats.indexed_store_stale, 0);
        assert!(gpu.pipe.stats.stall_div > 0, "the touch pays the shadow");
        assert_eq!(bus.read32(0x0010_0004), 6, "quotient stored after the touch");
    }

    /// Taken JUMP/JR costs the refill; flags consumed right after a CMP pay
    /// the one-tick flag latency. Semantics (delay slot, result) unchanged.
    #[test]
    fn timed_jump_refill_and_flag_latency() {
        let prog = with_stop(&[
            enc(35, 0, 1),
            enc(35, 5, 2),
            enc(0, 2, 1),                         // loop: add r2,r1
            enc(6, 1, 2),                         // subq #1,r2
            enc(31, 0, 2),                        // cmpq #0,r2
            enc(53, (-4i16 as u16) & 0x1F, 0x01), // jr NE,loop
            enc(57, 0, 0),                        // delay slot
            enc(38, 0, 3),
            0x0000,
            0x0010,
            enc(47, 3, 1), // store r1,(r3)
        ]);
        let (mut bus, gpu) = run_fid(&prog, mem::G_RAM, 800, Fidelity::Silicon, &[]);
        assert_eq!(bus.read32(0x0010_0000), 15, "timed profile must not change semantics");
        assert_eq!(gpu.pipe.stats.jump_refill, 12, "4 taken back-edges x 3-tick refill (HW-calibrated)");
        assert_eq!(gpu.pipe.stats.stall_flags, 5, "5 jr executions, each 1 tick after cmpq");
    }

    /// Executing from DRAM pays per-word fetch costs (the GPU-in-main tax);
    /// the same program in local SRAM pays none.
    #[test]
    fn timed_external_fetch_tax() {
        let prog = with_stop(&[
            enc(35, 1, 1),
            enc(0, 1, 2),
            enc(0, 1, 2),
            enc(0, 1, 2),
        ]);
        let (_, local) = run_fid(&prog, mem::G_RAM, 2000, Fidelity::Silicon, &[]);
        let (_, external) = run_fid(&prog, 0x8000, 2000, Fidelity::Silicon, &[]);
        assert_eq!(local.pipe.stats.fetch_external, 0);
        assert!(external.pipe.stats.fetch_external > 0);
        assert!(
            external.cycles > local.cycles * 4,
            "external execution must be several times slower (local {}, external {})",
            local.cycles,
            external.cycles
        );
        assert_eq!(external.regs[0][2], 3, "same result regardless of placement");
    }

    /// The documented BigPEmu mismodel: an external load consumed across a
    /// taken jump is not scoreboarded. Silicon stalls (correct data, honest
    /// cost); the BigPEmu profile skips the stall and counts the divergence.
    /// Uses a cross-chip (ExtOther, ~14-cycle) load so the result is still in
    /// flight when consumed after the jump under the HW-calibrated latencies.
    #[test]
    fn timed_bigpemu_divergence_counter() {
        let prog = with_stop(&[
            enc(38, 0, 3),
            0xB000,
            0x00F1,        // movei #$00F1B000,r3 (DSP RAM: cross-chip read)
            enc(41, 3, 2), // load (r3),r2 — external, slow
            enc(53, 2, 0), // jr T,+2 words (skip one)
            enc(57, 0, 0), // delay slot
            enc(57, 0, 0), // skipped
            enc(34, 2, 5), // move r2,r5 — consumed across the taken jump
        ]);
        let pre = [(0x00F1_B000u32, 0x1234_5678u32)];

        let (_, gpu) = run_fid(&prog, mem::G_RAM, 800, Fidelity::Silicon, &pre);
        assert!(gpu.pipe.stats.stall_load > 0, "silicon scoreboards the load");
        assert_eq!(gpu.pipe.stats.bigpemu_divergence, 0);
        assert_eq!(gpu.regs[0][5], 0x1234_5678);

        let (_, gpu) = run_fid(&prog, mem::G_RAM, 800, Fidelity::BigPEmu, &pre);
        assert_eq!(gpu.pipe.stats.stall_load, 0, "bigpemu profile skips the stall");
        assert_eq!(gpu.pipe.stats.bigpemu_divergence, 1, "…and counts the divergence");
    }

    /// 68k bus contention (row thrash): page-hit external streams cost more
    /// while the 68000 is on the bus; a STOPped 68k removes the tax. Matches
    /// the bench: lddram A/B = 2.1x, ldstride A == B (misses see no extra).
    #[test]
    fn timed_contention_row_thrash() {
        // Sequential external loads, rotating destinations (no WAW).
        let mut body = vec![
            enc(38, 0, 10),
            0x0000,
            0x0014, // movei #$00140000,r10
        ];
        for i in 0..24 {
            body.push(enc(41, 10, 1 + (i % 4)));
            body.push(enc(3, 4, 10)); // addqt #4,r10
        }
        let prog = with_stop(&body);

        let run_with = |on_bus: bool| {
            let mut bus = Bus::new();
            bus.m68k_on_bus = on_bus;
            for (i, &w) in prog.iter().enumerate() {
                bus.write16(mem::G_RAM + (i as u32) * 2, w);
            }
            bus.write32(mem::G_PC, mem::G_RAM);
            bus.write32(mem::G_CTRL, mem::RISCGO);
            let mut gpu = Risc::new(RiscKind::Gpu);
            gpu.fidelity = Fidelity::Silicon;
            gpu.run(&mut bus, 5000);
            gpu
        };
        let busy = run_with(true);
        let quiet = run_with(false);
        assert!(quiet.pipe.stats.contention == 0);
        assert!(busy.pipe.stats.contention > 0, "page-hit stream must pay the thrash tax");
        assert!(
            busy.cycles > quiet.cycles + 60,
            "busy {} vs quiet {} — contention must cost real cycles",
            busy.cycles,
            quiet.cycles
        );
    }

    /// Wall-clock coupling: driving the core in small scheduler-style slices
    /// must never let it consume more cycles than the budget granted (plus at
    /// most one instruction of overrun carried as debt). This is what makes
    /// VC-timed measurements (the calibration ROM) meaningful.
    #[test]
    fn timed_budget_debt_couples_wall_clock() {
        // External program: expensive per-instruction fetch costs.
        let prog = with_stop(&[
            enc(35, 1, 1),
            enc(0, 1, 2),
            enc(0, 1, 2),
            enc(0, 1, 2),
            enc(0, 1, 2),
            enc(0, 1, 2),
            enc(0, 1, 2),
        ]);
        let mut bus = Bus::new();
        for (i, &w) in prog.iter().enumerate() {
            bus.write16(0x8000 + (i as u32) * 2, w);
        }
        bus.write32(mem::G_PC, 0x8000);
        bus.write32(mem::G_CTRL, mem::RISCGO);
        let mut gpu = Risc::new(RiscKind::Gpu);
        gpu.fidelity = Fidelity::Silicon;
        let mut granted = 0u64;
        for _ in 0..200 {
            gpu.run(&mut bus, 8);
            granted += 8;
            if !gpu.running {
                break;
            }
        }
        assert!(!gpu.running, "program must finish");
        assert!(
            gpu.cycles <= granted + 32,
            "core consumed {} cycles on {} granted — wall-clock decoupled",
            gpu.cycles,
            granted
        );
        assert!(
            gpu.cycles > granted / 2,
            "core consumed only {} of {} granted — throttled too hard",
            gpu.cycles,
            granted
        );
    }

    /// Functional profile is byte-for-byte the historical behavior: 1 tick per
    /// instruction, zero stalls recorded.
    #[test]
    fn functional_profile_unchanged() {
        let prog = with_stop(&[
            enc(35, 20, 2),
            enc(35, 3, 1),
            enc(21, 1, 2),
            enc(34, 2, 3),
        ]);
        let (_, gpu) = run_fid(&prog, mem::G_RAM, 500, Fidelity::Functional, &[]);
        assert_eq!(gpu.cycles, gpu.instret);
        assert_eq!(gpu.pipe.stats.total_stall(), 0);
        assert_eq!(gpu.regs[0][3], 6);
    }

    // ── JRISC per-PC profiler ───────────────────────────────────────────────

    /// Run `words` with the per-PC profiler armed.
    fn run_profiled(words: &[u16], budget: u32, fid: Fidelity) -> Risc {
        let mut bus = Bus::new();
        for (i, &w) in words.iter().enumerate() {
            bus.write16(mem::G_RAM + (i as u32) * 2, w);
        }
        bus.write32(mem::G_PC, mem::G_RAM);
        bus.write32(mem::G_CTRL, mem::RISCGO);
        let mut gpu = Risc::new(RiscKind::Gpu);
        gpu.fidelity = fid;
        gpu.arm_profiler();
        gpu.run(&mut bus, budget);
        gpu
    }

    /// The profile is a partition of the core's own cycle count: every tick the
    /// core charged itself lands on exactly one PC, and every instruction is
    /// counted once. If this drifts, a histogram silently under- or
    /// over-reports and the hot spot it names is not the real one.
    #[test]
    fn profile_totals_match_core() {
        let prog = with_stop(&[
            enc(35, 20, 2), // moveq #20,r2
            enc(35, 3, 1),  // moveq #3,r1
            enc(21, 1, 2),  // div r1,r2   ← 17-tick shadow
            enc(34, 2, 3),  // move r2,r3  ← consumer stalls
        ]);
        let gpu = run_profiled(&prog, 500, Fidelity::Silicon);
        let p = gpu.prof.as_ref().unwrap();
        assert_eq!(p.total.cycles, gpu.cycles, "profiled ticks must equal core cycles");
        assert_eq!(p.total.instrs, gpu.instret, "profiled instrs must equal instret");
        assert_eq!(p.total.stall_div, gpu.pipe.stats.stall_div);
        // The DIV consumer is the instruction that pays the shadow, and the
        // profiler must name IT — not the DIV that opened the shadow.
        let consumer = mem::G_RAM + 3 * 2;
        let row = p.all().into_iter().find(|(pc, _)| *pc == consumer).unwrap().1;
        assert!(row.stall_div > 0, "the stall belongs to the consuming PC");
    }

    /// Attributed stalls can never exceed the cycles the core actually charged.
    /// They did: an instruction reading two in-flight registers stalls ONCE for
    /// the longer wait, but both waits were being added to the counters, so a
    /// load-heavy kernel reported `stall_load` above 100% of its own cycles.
    #[test]
    fn stall_counters_do_not_double_count_operands() {
        // r2 and r3 both land late (two DIVs), then one instruction reads both.
        let prog = with_stop(&[
            enc(35, 3, 1),  // moveq #3,r1
            enc(35, 20, 2), // moveq #20,r2
            enc(35, 40, 3), // moveq #40,r3
            enc(21, 1, 2),  // div r1,r2
            enc(21, 1, 3),  // div r1,r3
            enc(0, 2, 3),   // add r2,r3   ← reads BOTH pending results
        ]);
        let gpu = run_profiled(&prog, 500, Fidelity::Silicon);
        let s = &gpu.pipe.stats;
        assert!(
            s.total_stall() <= gpu.cycles,
            "attributed stalls {} exceed core cycles {}",
            s.total_stall(),
            gpu.cycles
        );
        let p = gpu.prof.as_ref().unwrap();
        for (pc, r) in p.all() {
            assert!(
                r.total_stall() <= r.cycles,
                "pc {pc:#010X}: stalls {} exceed its own cycles {}",
                r.total_stall(),
                r.cycles
            );
        }
    }

    /// Refill is charged to the delay slot — that is where the ticks are spent.
    /// This is the counter a kernel author chases when a branchy loop is slow,
    /// and it was previously only available as a core-wide total.
    #[test]
    fn profile_attributes_jump_refill_per_pc() {
        // jr always to +2 words, delay slot, then the stop epilogue.
        let prog = with_stop(&[
            enc(35, 0, 1),                    // 0: moveq #0,r1
            enc(53, 2 & 0x1F, 0x00),          // 1: jr T,(+2 words)
            enc(57, 0, 0),                    // 2: nop   ← delay slot
            enc(57, 0, 0),                    // 3: nop   (skipped)
            enc(57, 0, 0),                    // 4: nop   (target)
        ]);
        let gpu = run_profiled(&prog, 500, Fidelity::Silicon);
        let p = gpu.prof.as_ref().unwrap();
        let slot = mem::G_RAM + 2 * 2;
        let row = p.all().into_iter().find(|(pc, _)| *pc == slot).unwrap().1;
        assert_eq!(
            row.jump_refill,
            timing::Lat::JUMP_REFILL as u64,
            "the taken jump's refill lands on its delay slot"
        );
        assert_eq!(p.total.jump_refill, gpu.pipe.stats.jump_refill);
    }

    /// A zero divisor must be COUNTED. jsim answers 0xFFFFFFFF and continues,
    /// which is why a kernel that dropped its degenerate-face cull rendered a
    /// normal frame here and black-screened a real Jaguar
    /// (COBWEB_BUG_jagemu_runs_code_that_hangs_silicon.md). The count is the
    /// whole point: silicon's behaviour is unmeasured, so the event is reported
    /// rather than modelled.
    #[test]
    fn div_by_zero_is_counted_not_silent() {
        let prog = with_stop(&[
            enc(35, 7, 2),  // moveq #7,r2   (dividend)
            enc(35, 0, 1),  // moveq #0,r1   (divisor = 0)
            enc(21, 1, 2),  // div r1,r2
        ]);
        let (_, gpu) = run_fid(&prog, mem::G_RAM, 500, Fidelity::Silicon, &[]);
        assert_eq!(gpu.pipe.stats.div_by_zero, 1, "a zero divisor must be counted");
        // The benign value is deliberately unchanged — this is a counter, not a
        // behaviour change, so no existing timing or result moves.
        assert_eq!(gpu.regs[0][2], 0xFFFF_FFFF);

        // And a normal divide must not trip it.
        let ok = with_stop(&[
            enc(35, 20, 2),
            enc(35, 4, 1),
            enc(21, 1, 2),
        ]);
        let (_, gpu) = run_fid(&ok, mem::G_RAM, 500, Fidelity::Silicon, &[]);
        assert_eq!(gpu.pipe.stats.div_by_zero, 0);
        assert_eq!(gpu.regs[0][2], 5);
    }

    /// The liveness watchdog counts CONSECUTIVE frames spent running and fires
    /// once. Frame-anchored rather than instruction-anchored so a resident
    /// kernel (OpenLara's DSP poll loop) does not trip it every run — a warning
    /// that always fires is one nobody reads.
    #[test]
    fn watchdog_counts_frames_and_resets_on_stop() {
        let mut gpu = Risc::new(RiscKind::Gpu);
        gpu.stuck_after_frames = Some(3);
        gpu.running = true;
        gpu.pc = 0x00F0_3010;
        for _ in 0..2 {
            gpu.note_frame();
        }
        assert_eq!(gpu.frames_running, 2);
        assert!(gpu.stuck_at.is_none(), "must not fire before the threshold");

        gpu.note_frame();
        assert_eq!(
            gpu.stuck_at,
            Some((0x00F0_3010, 3)),
            "fires at the threshold, capturing pc AND the frame count"
        );

        // The captured count must survive the core stopping (it reset the live
        // streak to 0 and used to be reported as "0 consecutive frames").
        gpu.running = false;
        gpu.note_frame();
        assert_eq!(gpu.stuck_at, Some((0x00F0_3010, 3)), "captured count must not decay");

        // Stopping clears the streak.
        gpu.running = false;
        gpu.note_frame();
        assert_eq!(gpu.frames_running, 0);

        // A core that never runs never trips it.
        let mut idle = Risc::new(RiscKind::Dsp);
        idle.stuck_after_frames = Some(1);
        for _ in 0..10 {
            idle.note_frame();
        }
        assert!(idle.stuck_at.is_none());
    }

    /// A DSP kernel running from local SRAM and a GPU kernel running from DRAM
    /// must not share a histogram slot. Masking a JRISC PC into one flat 2 MB
    /// window (what the 68k profiler does) aliases `$F03000` onto `$103000`.
    #[test]
    fn profile_separates_sram_from_dram_addresses() {
        use crate::debug::{RiscProfile, RiscRow};
        let mut p = RiscProfile::new(mem::G_RAM, 0x1000);
        let one = RiscRow { cycles: 7, instrs: 1, ..Default::default() };
        p.record(mem::G_RAM, &one); // $F03000, local SRAM
        p.record(mem::G_RAM & 0x1FFFFF, &one); // $103000, DRAM — the alias
        let rows = p.all();
        assert_eq!(rows.len(), 2, "SRAM and its DRAM alias must be distinct rows");
        assert!(rows.iter().all(|(_, r)| r.cycles == 7));
        assert_eq!(p.total.cycles, 14);
    }
}
