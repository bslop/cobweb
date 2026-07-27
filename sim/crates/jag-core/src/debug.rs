//! The debugging core: breakpoints, watchpoints, trace, and stop reasons that
//! the machine reports back to the control layer.
//!
//! This is the foundation of the "instrumentation-first" design — the 68k and
//! RISC cores call into the `Debugger` on fetch/access so AI-driven debugging
//! (set breakpoint → run → inspect) is repeatable and cheap.

use std::collections::HashSet;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StopReason {
    /// Ran to the requested absolute frame number.
    ReachedFrame(u64),
    /// Execution PC hit a breakpoint.
    Breakpoint(u32),
    /// The GPU PC hit a breakpoint (registers frozen at that instruction).
    GpuBreakpoint(u32),
    /// The DSP PC hit a breakpoint.
    DspBreakpoint(u32),
    /// A watched memory address was accessed.
    Watchpoint { addr: u32, write: bool },
    /// A single requested step completed.
    StepComplete,
    /// The CPU executed an unrecognized/illegal opcode.
    Illegal { pc: u32, op: u32 },
    /// The CPU is stopped (STOP instruction) with no pending interrupt.
    Halted,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WatchKind {
    Read,
    Write,
    Access,
}

#[derive(Default)]
pub struct Debugger {
    /// Fast gate: when false the hot loop skips all debug hooks entirely.
    pub enabled: bool,
    pub breakpoints: HashSet<u32>,
    pub watch_read: HashSet<u32>,
    pub watch_write: HashSet<u32>,
    /// Set mid-instruction (watchpoint / illegal); drained by the run loop.
    pub pending_stop: Option<StopReason>,
    /// Ring buffer of recently fetched PCs (for `trace`).
    pub trace: bool,
    pub trace_log: std::collections::VecDeque<u32>,
    pub trace_cap: usize,
    /// Stop the run loop when the CPU hits an illegal/unimplemented opcode.
    /// Off by default (illegal opcodes vector normally); invaluable in bring-up.
    pub stop_on_illegal: bool,
    /// Exact 68k cycle profiler. `None` = off (zero hot-loop cost).
    pub prof: Option<Box<Profile>>,
}

impl Debugger {
    pub fn new() -> Self {
        Debugger {
            enabled: false,
            breakpoints: HashSet::new(),
            watch_read: HashSet::new(),
            watch_write: HashSet::new(),
            pending_stop: None,
            trace: false,
            trace_log: std::collections::VecDeque::new(),
            trace_cap: 4096,
            stop_on_illegal: false,
            prof: None,
        }
    }

    fn recompute_enabled(&mut self) {
        self.enabled = self.trace
            || !self.watch_read.is_empty()
            || !self.watch_write.is_empty();
    }

    pub fn add_breakpoint(&mut self, pc: u32) {
        self.breakpoints.insert(pc & !1);
    }
    pub fn remove_breakpoint(&mut self, pc: u32) {
        self.breakpoints.remove(&(pc & !1));
    }
    pub fn clear_breakpoints(&mut self) {
        self.breakpoints.clear();
    }

    pub fn add_watchpoint(&mut self, addr: u32, kind: WatchKind) {
        match kind {
            WatchKind::Read => {
                self.watch_read.insert(addr);
            }
            WatchKind::Write => {
                self.watch_write.insert(addr);
            }
            WatchKind::Access => {
                self.watch_read.insert(addr);
                self.watch_write.insert(addr);
            }
        }
        self.recompute_enabled();
    }

    pub fn set_trace(&mut self, on: bool) {
        self.trace = on;
        self.recompute_enabled();
    }

    /// Is `pc` a breakpoint? Cheap; the run loop calls this before each step.
    #[inline]
    pub fn is_breakpoint(&self, pc: u32) -> bool {
        !self.breakpoints.is_empty() && self.breakpoints.contains(&(pc & !1))
    }

    /// Called by the CPU at instruction fetch (only when `enabled`).
    #[inline]
    pub fn on_fetch(&mut self, pc: u32) {
        if self.trace {
            if self.trace_log.len() >= self.trace_cap {
                self.trace_log.pop_front();
            }
            self.trace_log.push_back(pc);
        }
    }

    /// Called by the bus on a watched access (only when watchpoints exist).
    #[inline]
    pub fn on_access(&mut self, addr: u32, write: bool) {
        if write {
            if self.watch_write.contains(&addr) {
                self.pending_stop = Some(StopReason::Watchpoint { addr, write: true });
            }
        } else if self.watch_read.contains(&addr) {
            self.pending_stop = Some(StopReason::Watchpoint { addr, write: false });
        }
    }

    #[inline]
    pub fn take_stop(&mut self) -> Option<StopReason> {
        self.pending_stop.take()
    }
}

// ── 68k profiler ────────────────────────────────────────────────────────────

/// Exact per-PC cycle attribution for the 68000.
///
/// Not a sampling profiler: every instruction's cycles are attributed to the PC
/// that issued it, so there is no sampling error and short hot routines cannot
/// hide between samples. Requested in `COBWEB_REQ_68k_pc_histogram.md`, where
/// the reporter specifically asked for CYCLE attribution over instruction
/// counts — they had measured +48% instructions retired with *zero* fps change,
/// which makes retirement count a poor proxy for frame cost on this workload.
///
/// `STOP`-sleeping cycles are tracked separately: a 68000 asleep waiting for an
/// interrupt is not spending frame time on anything, and folding that into the
/// PC where it happens to be parked would read as a hot spot.
pub struct Profile {
    /// Cycles indexed by word address within the 2 MB main-RAM window.
    cycles: Vec<u64>,
    instrs: Vec<u32>,
    pub isr_cycles: u64,
    pub isr_instrs: u64,
    pub main_cycles: u64,
    pub main_instrs: u64,
    pub stopped_cycles: u64,
    pub total_cycles: u64,
}

const PROF_SLOTS: usize = 0x200000 >> 1;

impl Default for Profile {
    fn default() -> Self {
        Self::new()
    }
}

impl Profile {
    pub fn new() -> Self {
        Profile {
            cycles: vec![0; PROF_SLOTS],
            instrs: vec![0; PROF_SLOTS],
            isr_cycles: 0,
            isr_instrs: 0,
            main_cycles: 0,
            main_instrs: 0,
            stopped_cycles: 0,
            total_cycles: 0,
        }
    }

    pub fn record(&mut self, pc: u32, cyc: u32, in_isr: bool, was_stopped: bool) {
        self.total_cycles += cyc as u64;
        if was_stopped {
            self.stopped_cycles += cyc as u64;
            return; // asleep in STOP — not frame cost, and not this PC's fault
        }
        let i = ((pc & 0x1FFFFF) >> 1) as usize;
        if i < self.cycles.len() {
            self.cycles[i] += cyc as u64;
            self.instrs[i] += 1;
        }
        if in_isr {
            self.isr_cycles += cyc as u64;
            self.isr_instrs += 1;
        } else {
            self.main_cycles += cyc as u64;
            self.main_instrs += 1;
        }
    }

    /// Hottest addresses by cycles: `(pc, cycles, instrs)`, descending.
    pub fn top(&self, k: usize) -> Vec<(u32, u64, u32)> {
        let mut v: Vec<(u32, u64, u32)> = self
            .cycles
            .iter()
            .enumerate()
            .filter(|(_, &c)| c > 0)
            .map(|(i, &c)| ((i as u32) << 1, c, self.instrs[i]))
            .collect();
        v.sort_unstable_by(|a, b| b.1.cmp(&a.1));
        v.truncate(k);
        v
    }

    /// Hot regions: cycles summed into `gran`-byte buckets, descending. Better
    /// than raw PCs for locating a *routine* rather than its hottest single
    /// instruction.
    pub fn top_buckets(&self, gran: u32, k: usize) -> Vec<(u32, u64, u32)> {
        let g = (gran.max(2) >> 1) as usize;
        let mut buckets: std::collections::HashMap<usize, (u64, u32)> =
            std::collections::HashMap::new();
        for (i, &c) in self.cycles.iter().enumerate() {
            if c > 0 {
                let e = buckets.entry(i / g).or_insert((0, 0));
                e.0 += c;
                e.1 += self.instrs[i];
            }
        }
        let mut v: Vec<(u32, u64, u32)> = buckets
            .into_iter()
            .map(|(b, (c, n))| (((b * g) as u32) << 1, c, n))
            .collect();
        v.sort_unstable_by(|a, b| b.1.cmp(&a.1));
        v.truncate(k);
        v
    }
}

// ── JRISC profiler (Tom GPU / Jerry DSP) ────────────────────────────────────

/// One PC's worth of attributed cycles.
///
/// The stall fields are the same categories the whole-core `TimingStats`
/// reports, sliced per instruction — so a hot spot can be read as *why* it is
/// hot, not just *that* it is. That was the missing half of the 68k histogram:
/// a core-wide `jump_refill` total says a kernel is refilling the pipe without
/// saying which jump does it, which left static analysis as the only way to
/// chase it.
#[derive(Clone, Copy, Default, Debug)]
pub struct RiscRow {
    pub cycles: u64,
    pub instrs: u64,
    pub stall_alu: u64,
    pub stall_load: u64,
    pub stall_div: u64,
    pub stall_flags: u64,
    pub stall_div_busy: u64,
    pub jump_refill: u64,
    pub fetch_external: u64,
    pub mem_external: u64,
    pub blit_wait: u64,
    pub contention: u64,
}

impl RiscRow {
    /// Total attributed stall ticks (excludes external fetch/data occupancy,
    /// matching `TimingStats::total_stall`).
    pub fn total_stall(&self) -> u64 {
        self.stall_alu
            + self.stall_load
            + self.stall_div
            + self.stall_flags
            + self.stall_div_busy
            + self.jump_refill
    }

    fn add(&mut self, o: &RiscRow) {
        self.cycles += o.cycles;
        self.instrs += o.instrs;
        self.stall_alu += o.stall_alu;
        self.stall_load += o.stall_load;
        self.stall_div += o.stall_div;
        self.stall_flags += o.stall_flags;
        self.stall_div_busy += o.stall_div_busy;
        self.jump_refill += o.jump_refill;
        self.fetch_external += o.fetch_external;
        self.mem_external += o.mem_external;
        self.blit_wait += o.blit_wait;
        self.contention += o.contention;
    }
}

/// Exact per-PC cycle attribution for a JRISC core, the GPU/DSP counterpart of
/// [`Profile`].
///
/// Exact, not sampled, for the same reason: a JRISC kernel's hot loop is often
/// a handful of instructions inside a 4 KB SRAM window, and any sampling
/// interval coarse enough to be cheap is coarse enough to miss it.
///
/// Addresses are routed to one of three stores rather than a single flat array,
/// because a JRISC PC is not confined to one window: kernels run from local
/// SRAM (`$F03000` / `$F1B000`), from DRAM when they outgrow it, and
/// occasionally from ROM. A flat 2 MB mask — which is what the 68k profiler
/// uses — would alias `$F03000` onto DRAM `$103000` and silently merge two
/// unrelated hot spots.
pub struct RiscProfile {
    /// Base/size of this core's local SRAM window (GPU 4 KB, DSP 8 KB).
    sram_base: u32,
    /// `pc -> row index + 1` for local SRAM (0 = never executed).
    sram_idx: Vec<u32>,
    /// `pc -> row index + 1` for the 2 MB DRAM window.
    dram_idx: Vec<u32>,
    /// Anything else (ROM, device windows) — rare, so a map is fine.
    other_idx: std::collections::HashMap<u32, u32>,
    rows: Vec<RiscRow>,
    pub total: RiscRow,
}

impl RiscProfile {
    pub fn new(sram_base: u32, sram_size: u32) -> Self {
        RiscProfile {
            sram_base,
            sram_idx: vec![0; (sram_size >> 1) as usize],
            dram_idx: vec![0; 0x200000 >> 1],
            other_idx: std::collections::HashMap::new(),
            rows: Vec::new(),
            total: RiscRow::default(),
        }
    }

    /// Slot for `pc`, allocating a row on first execution.
    #[inline]
    fn slot(&mut self, pc: u32) -> usize {
        let cell: &mut u32 = if pc >= self.sram_base
            && ((pc - self.sram_base) >> 1) < self.sram_idx.len() as u32
        {
            let i = ((pc - self.sram_base) >> 1) as usize;
            &mut self.sram_idx[i]
        } else if pc < 0x200000 {
            &mut self.dram_idx[(pc >> 1) as usize]
        } else {
            self.other_idx.entry(pc).or_insert(0)
        };
        if *cell == 0 {
            self.rows.push(RiscRow::default());
            *cell = self.rows.len() as u32; // stored +1 so 0 means "unused"
        }
        (*cell - 1) as usize
    }

    /// Attribute one instruction's cycles and stalls to the PC that issued it.
    #[inline]
    pub fn record(&mut self, pc: u32, row: &RiscRow) {
        self.total.add(row);
        let i = self.slot(pc);
        self.rows[i].add(row);
    }

    /// Hottest PCs by cycles, descending.
    pub fn top(&self, k: usize) -> Vec<(u32, RiscRow)> {
        let mut v = self.all();
        v.sort_unstable_by(|a, b| b.1.cycles.cmp(&a.1.cycles));
        v.truncate(k);
        v
    }

    /// Hot regions: cycles summed into `gran`-byte buckets, descending. Better
    /// for naming a *routine* than its hottest single instruction.
    pub fn top_buckets(&self, gran: u32, k: usize) -> Vec<(u32, RiscRow)> {
        let g = gran.max(2) as u64;
        let mut buckets: std::collections::HashMap<u64, RiscRow> = std::collections::HashMap::new();
        for (pc, r) in self.all() {
            buckets.entry(pc as u64 / g).or_default().add(&r);
        }
        let mut v: Vec<(u32, RiscRow)> =
            buckets.into_iter().map(|(b, r)| ((b * g) as u32, r)).collect();
        v.sort_unstable_by(|a, b| b.1.cycles.cmp(&a.1.cycles));
        v.truncate(k);
        v
    }

    /// Every executed PC with its row, unordered.
    pub fn all(&self) -> Vec<(u32, RiscRow)> {
        let mut v = Vec::with_capacity(self.rows.len());
        for (i, &c) in self.sram_idx.iter().enumerate() {
            if c != 0 {
                v.push((self.sram_base + ((i as u32) << 1), self.rows[(c - 1) as usize]));
            }
        }
        for (i, &c) in self.dram_idx.iter().enumerate() {
            if c != 0 {
                v.push(((i as u32) << 1, self.rows[(c - 1) as usize]));
            }
        }
        for (&pc, &c) in self.other_idx.iter() {
            if c != 0 {
                v.push((pc, self.rows[(c - 1) as usize]));
            }
        }
        v
    }
}
