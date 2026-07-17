//! jdbg — source-level JRISC debugging, over jsim.
//!
//! The state of the art for a Jaguar hardware crash is: paint bit patterns into
//! the framebuffer, film the TV, decode the capture with a script. That was the
//! *right* engineering under the available tools — which is the indictment.
//! jdbg replaces it. It assembles JRISC source through jas (keeping the
//! address→source-line map jas already produces), runs it in jsim, and lets you
//! set breakpoints and step **by source line**, inspect registers, and get a
//! crash report that says "your PC was here, in this source line" — the thing
//! jagemu's address-level debug can't do.
//!
//! v1 backend is the emulator (deterministic, no hardware needed). The same
//! `Session` API is what a Skunkboard/GameDrive backend will implement next, so
//! one frontend drives silicon and simulator identically.

use std::collections::BTreeMap;

use jag_core::risc::Fidelity;
use jag_core::{mem, Bus, Risc, RiscKind};

/// A loaded debug session.
pub struct Session {
    bus: Bus,
    core: Risc,
    source: Vec<String>,
    /// instruction address -> 1-based source line
    addr_line: BTreeMap<u32, usize>,
    /// 1-based source line -> first instruction address
    line_addr: BTreeMap<usize, u32>,
    org: u32,
    /// First address past the loaded program (used to detect wild jumps).
    end_addr: u32,
    steps: u64,
    halted: bool,
}

/// Why a run stopped.
#[derive(Debug, Clone, PartialEq)]
pub enum Stop {
    /// Hit a breakpoint at this source line.
    Breakpoint(usize),
    /// The program self-halted (cleared RISCGO).
    Halted,
    /// Step budget exhausted (possible infinite loop / runaway).
    Budget,
    /// PC left the loaded code region — a likely wild jump / crash.
    Escaped(u32),
}

impl Session {
    /// Assemble `src` for `target` and load it into a fresh machine. Errors are
    /// the jas diagnostics (a program that won't assemble can't be debugged).
    pub fn load(src: &str, target: RiscKind) -> Result<Session, Vec<String>> {
        let opts = jas::Options {
            target: match target {
                RiscKind::Gpu => jas::Target::Gpu,
                RiscKind::Dsp => jas::Target::Dsp,
            },
            org: match target {
                RiscKind::Gpu => mem::G_RAM,
                RiscKind::Dsp => mem::D_RAM,
            },
            ..Default::default()
        };
        let out = jas::assemble(src, &opts);
        if out.errors() > 0 {
            return Err(out.diags.iter().map(|d| d.to_string()).collect());
        }

        let mut addr_line = BTreeMap::new();
        let mut line_addr = BTreeMap::new();
        for e in &out.emitted {
            if e.op.is_some() {
                addr_line.insert(e.addr, e.line);
                line_addr.entry(e.line).or_insert(e.addr);
            }
        }

        let mut bus = Bus::new();
        for (i, b) in out.bytes.iter().enumerate() {
            bus.write8(out.org + i as u32, *b);
        }
        let (pc_a, ctrl_a) = match target {
            RiscKind::Gpu => (mem::G_PC, mem::G_CTRL),
            RiscKind::Dsp => (mem::D_PC, mem::D_CTRL),
        };
        bus.write32(pc_a, out.org);
        bus.write32(ctrl_a, mem::RISCGO);
        let mut core = Risc::new(target);
        core.fidelity = Fidelity::Functional; // clean 1-instruction stepping

        Ok(Session {
            bus,
            core,
            source: src.lines().map(|s| s.to_string()).collect(),
            addr_line,
            line_addr,
            org: out.org,
            end_addr: out.org + out.bytes.len() as u32,
            steps: 0,
            halted: false,
        })
    }

    pub fn pc(&self) -> u32 {
        self.core.pc
    }
    pub fn regs(&self) -> [u32; 32] {
        self.core.regs[self.core.cur_bank()]
    }
    pub fn steps(&self) -> u64 {
        self.steps
    }
    pub fn halted(&self) -> bool {
        self.halted
    }

    /// Source line the PC currently sits on (nearest instruction at or below PC).
    pub fn pc_line(&self) -> Option<usize> {
        self.addr_line
            .range(..=self.core.pc)
            .next_back()
            .map(|(_, &l)| l)
            .or_else(|| self.addr_line.get(&self.core.pc).copied())
    }

    /// The text of 1-based source line `n`.
    pub fn source_line(&self, n: usize) -> Option<&str> {
        self.source.get(n - 1).map(|s| s.as_str())
    }

    /// The address a breakpoint on source line `n` maps to.
    pub fn line_addr(&self, n: usize) -> Option<u32> {
        self.line_addr.get(&n).copied()
    }

    /// Execute exactly one instruction. Returns false if the machine has halted.
    pub fn step(&mut self) -> bool {
        if self.halted {
            return false;
        }
        let ctrl_a = match self.core.kind {
            RiscKind::Gpu => mem::G_CTRL,
            RiscKind::Dsp => mem::D_CTRL,
        };
        self.core.run(&mut self.bus, 1);
        self.steps += 1;
        // The program self-halts by clearing RISCGO; detect it.
        if !self.core.running || self.bus.read32(ctrl_a) & mem::RISCGO == 0 {
            self.halted = true;
        }
        !self.halted
    }

    /// Run until a breakpoint (source line in `breaks`), a halt, PC escape, or
    /// `max_steps`. Returns the stop reason.
    pub fn run(&mut self, breaks: &[usize], max_steps: u64) -> Stop {
        let bp_addrs: Vec<u32> = breaks.iter().filter_map(|l| self.line_addr(*l)).collect();
        let mut n = 0;
        while n < max_steps {
            if !self.step() {
                return Stop::Halted;
            }
            n += 1;
            let pc = self.core.pc;
            if pc < self.org || pc >= self.end_addr {
                // executing outside loaded code — wild jump (unless it's the
                // control-reg space the self-stop writes, already handled).
                return Stop::Escaped(pc);
            }
            if bp_addrs.contains(&pc) {
                return Stop::Breakpoint(self.pc_line().unwrap_or(0));
            }
        }
        Stop::Budget
    }

    /// A crash/stop forensics report: where the PC is, in source, plus registers.
    pub fn report(&self, stop: &Stop) -> String {
        let mut s = String::new();
        let what = match stop {
            Stop::Breakpoint(l) => format!("breakpoint at line {l}"),
            Stop::Halted => "program halted (RISCGO cleared)".into(),
            Stop::Budget => "step budget exhausted (possible infinite loop)".into(),
            Stop::Escaped(pc) => format!("PC escaped loaded code at ${pc:06X} — wild jump / crash"),
        };
        s.push_str(&format!("stop: {what}\n"));
        s.push_str(&format!("  pc=${:06X}  steps={}\n", self.core.pc, self.steps));
        if let Some(l) = self.pc_line() {
            s.push_str(&format!("  at line {l}: {}\n", self.source_line(l).unwrap_or("").trim()));
        }
        let regs = self.regs();
        s.push_str("  registers (nonzero):");
        for (i, r) in regs.iter().enumerate() {
            if *r != 0 {
                s.push_str(&format!(" r{i}=${r:X}"));
            }
        }
        s.push('\n');
        s
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const STOP: &str = "        movei #$00F02114,r30\n        moveq #0,r29\n        store r29,(r30)\n        nop\n";

    fn prog() -> String {
        format!(
            "        .gpu\n\
             \x20       moveq #0,r1\n\
             \x20       moveq #5,r2\n\
             loop:   add r2,r1\n\
             \x20       subq #1,r2\n\
             \x20       cmpq #0,r2\n\
             \x20       movei #loop,r18\n\
             \x20       jump ne,(r18)\n\
             \x20       nop\n\
             \x20       movei #$00100000,r3\n\
             \x20       store r1,(r3)\n{STOP}"
        )
    }

    #[test]
    fn steps_and_maps_source_lines() {
        let mut s = Session::load(&prog(), RiscKind::Gpu).unwrap();
        // first instruction is on line 2 (moveq #0,r1)
        assert_eq!(s.pc_line(), Some(2));
        s.step();
        assert_eq!(s.pc_line(), Some(3)); // moveq #5,r2
    }

    #[test]
    fn breakpoint_by_source_line() {
        let mut s = Session::load(&prog(), RiscKind::Gpu).unwrap();
        // break at the `add` on line 4 (the loop body)
        let stop = s.run(&[4], 10_000);
        assert_eq!(stop, Stop::Breakpoint(4));
    }

    #[test]
    fn runs_to_halt_with_correct_result() {
        let mut s = Session::load(&prog(), RiscKind::Gpu).unwrap();
        let stop = s.run(&[], 100_000);
        assert_eq!(stop, Stop::Halted);
        // r1 accumulated 5+4+3+2+1 = 15
        assert_eq!(s.regs()[1], 15);
    }

    #[test]
    fn crash_report_names_the_source_line() {
        let mut s = Session::load(&prog(), RiscKind::Gpu).unwrap();
        let stop = s.run(&[4], 10_000);
        let rep = s.report(&stop);
        assert!(rep.contains("line 4"));
        assert!(rep.contains("add"));
    }
}
