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
