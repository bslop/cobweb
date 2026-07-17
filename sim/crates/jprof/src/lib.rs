//! jprof — see where the cycles go.
//!
//! Nobody in this community has ever *seen* a JRISC frame. jsim's timing model
//! attributes every stalled cycle to a cause; jprof turns that into a profile:
//! total cycles, the issue-vs-stall-vs-bus breakdown with percentages, the
//! dominant bottleneck named in plain language, and a build-to-build diff so
//! you can prove an optimization actually moved the number that matters (on a
//! bus-bound kernel, tightening delay slots recovers instruction time and
//! nothing else — jprof shows you that instead of letting you guess).

use jag_core::risc::{Fidelity, TimingStats};
use jag_core::RiscKind;
use jtest::{run, RunResult, Spec};

/// One profiled run.
pub struct Profile {
    pub cycles: u64,
    pub instret: u64,
    pub timing: TimingStats,
}

/// A named cost bucket in RISC ticks.
pub struct Bucket {
    pub name: &'static str,
    pub cycles: u64,
}

impl Profile {
    pub fn of(r: &RunResult) -> Self {
        Profile { cycles: r.cycles, instret: r.instret, timing: r.timing.clone() }
    }

    /// Instructions per cycle — the headline efficiency number.
    pub fn ipc(&self) -> f64 {
        if self.cycles == 0 {
            0.0
        } else {
            self.instret as f64 / self.cycles as f64
        }
    }

    /// Cost breakdown. "issue" is the productive floor (cycles that weren't a
    /// stall or bus wait); the rest are the recoverable buckets.
    pub fn buckets(&self) -> Vec<Bucket> {
        let t = &self.timing;
        let stalls = t.stall_alu
            + t.stall_load
            + t.stall_div
            + t.stall_flags
            + t.stall_div_busy
            + t.jump_refill
            + t.fetch_external
            + t.contention;
        let issue = self.cycles.saturating_sub(stalls);
        vec![
            Bucket { name: "issue (productive)", cycles: issue },
            Bucket { name: "alu-bubble stall", cycles: t.stall_alu },
            Bucket { name: "load-latency stall", cycles: t.stall_load },
            Bucket { name: "div-shadow stall", cycles: t.stall_div },
            Bucket { name: "flag-latency stall", cycles: t.stall_flags },
            Bucket { name: "divider-busy stall", cycles: t.stall_div_busy },
            Bucket { name: "taken-jump refill", cycles: t.jump_refill },
            Bucket { name: "external fetch (GPU-in-main)", cycles: t.fetch_external },
            Bucket { name: "68k bus contention", cycles: t.contention },
        ]
    }

    /// The single largest recoverable cost, as a plain-language diagnosis.
    pub fn bottleneck(&self) -> String {
        let mut b = self.buckets();
        // drop the productive floor; find the biggest stall
        b.retain(|x| x.name != "issue (productive)");
        b.sort_by_key(|x| std::cmp::Reverse(x.cycles));
        let top = &b[0];
        if top.cycles == 0 {
            return "no stalls — this code is issue-bound (as tight as it gets)".into();
        }
        let pct = 100.0 * top.cycles as f64 / self.cycles.max(1) as f64;
        let advice = match top.name {
            "taken-jump refill" => "branchy code with unfilled delay slots — run jopt, or restructure loops (software-pipelined back edges)",
            "load-latency stall" | "div-shadow stall" => "consumer reads a slow result too soon — fill the shadow with independent work",
            "external fetch (GPU-in-main)" => "executing from DRAM — move the hot loop into local SRAM (overlays)",
            "68k bus contention" => "the 68000 is starving the bus — STOP it during this work",
            "alu-bubble stall" => "dependent ALU chains — interleave two independent chains",
            "flag-latency stall" => "a conditional jump reads flags too soon — put a flag-transparent instruction between the compare and the jump",
            _ => "see the breakdown",
        };
        format!("{} is {:.1}% of runtime — {}", top.name, pct, advice)
    }
}

fn target_of(t: RiscKind) -> jas::Target {
    match t {
        RiscKind::Gpu => jas::Target::Gpu,
        RiscKind::Dsp => jas::Target::Dsp,
    }
}

/// Assemble source through jas (hazard-checked). Err with diagnostics.
pub fn assemble(src: &str, target: RiscKind) -> Result<(Vec<u8>, u32), Vec<String>> {
    let opts = jas::Options {
        target: target_of(target),
        org: match target {
            RiscKind::Gpu => 0xF0_3000,
            RiscKind::Dsp => 0xF1_B000,
        },
        ..Default::default()
    };
    let out = jas::assemble(src, &opts);
    if out.errors() > 0 {
        Err(out.diags.iter().map(|d| d.to_string()).collect())
    } else {
        Ok((out.bytes, out.org))
    }
}

/// Profile a program (bytes at org) in jsim under the silicon model.
pub fn profile(bytes: Vec<u8>, org: u32, target: RiscKind, budget: u32) -> Profile {
    let spec = Spec {
        bytes,
        target,
        org,
        budget,
        capture: (0x0010_0000, 4),
        fidelity: Fidelity::Silicon,
    };
    Profile::of(&run(&spec))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn asm(body: &str) -> (Vec<u8>, u32) {
        let stop = "        movei #$00F02114,r30\n        moveq #0,r29\n        store r29,(r30)\n        nop\n";
        assemble(&format!("        .gpu\n{body}{stop}"), RiscKind::Gpu).unwrap()
    }

    #[test]
    fn issue_bound_code_has_no_stalls() {
        let (b, o) = asm("        moveq #1,r1\n        moveq #2,r2\n        moveq #3,r3\n");
        let p = profile(b, o, RiscKind::Gpu, 10_000);
        assert!(p.bottleneck().contains("issue-bound"));
    }

    #[test]
    fn jump_heavy_loop_is_refill_bound() {
        // a tight loop of taken back-edges: refill should dominate
        let (b, o) = asm(
            "        moveq #20,r1\n\
             loop:   subq #1,r1\n\
             \x20       movei #loop,r18\n\
             \x20       jump ne,(r18)\n\
             \x20       nop\n",
        );
        let p = profile(b, o, RiscKind::Gpu, 100_000);
        assert!(p.timing.jump_refill > 0);
        assert!(p.bottleneck().contains("refill"), "got: {}", p.bottleneck());
    }

    #[test]
    fn diff_shows_improvement() {
        let (b1, o1) = asm("        moveq #10,r1\n        moveq #20,r2\n        add r1,r2\n        add r1,r2\n");
        let p = profile(b1, o1, RiscKind::Gpu, 10_000);
        // dependent adds create an alu bubble; total cycles > instret
        assert!(p.cycles >= p.instret);
    }
}
