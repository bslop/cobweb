//! jtest — verification as a product.
//!
//! The wishlist's premise: on this hardware, "produces bytes that run" is the
//! easy part; *confidence* is the missing part. jtest turns the shadow-harness
//! pattern into a tool. Three checks, all built on jsim:
//!
//! * **Fidelity-profile diff** — run the same program under `silicon` and
//!   `bigpemu` and report where they diverge. This is how you catch code that
//!   is hardware-correct but emulator-wrong (or the reverse) *before* a
//!   hardware session, deterministically. jsim's `bigpemu_divergence` counter
//!   pinpoints the mechanism.
//! * **Shadow diff** — run a candidate and a reference program and compare a
//!   captured memory region and the register file. This is the on-device
//!   dual-compute harness, run in the simulator: it caught a real emulator bug
//!   on its first use in the source project.
//! * **Golden vectors** — snapshot a region once, then fail any future run that
//!   changes it. The cheapest regression gate there is.
//!
//! A program is either a flat `.bin` or JRISC source assembled through jas —
//! so a hazard-clean assemble and a behavioral check are one command apart.

use jag_core::risc::{Fidelity, TimingStats};
use jag_core::{mem, Bus, Risc, RiscKind};

/// How to build and run a program.
#[derive(Clone)]
pub struct Spec {
    pub bytes: Vec<u8>,
    pub target: RiscKind,
    pub org: u32,
    /// RISC-tick budget.
    pub budget: u32,
    /// Region of DRAM to capture as the observable result: (addr, len).
    pub capture: (u32, u32),
    pub fidelity: Fidelity,
}

impl Spec {
    pub fn gpu(bytes: Vec<u8>) -> Self {
        Spec {
            bytes,
            target: RiscKind::Gpu,
            org: mem::G_RAM,
            budget: 100_000,
            capture: (0x0010_0000, 256),
            fidelity: Fidelity::Silicon,
        }
    }
}

/// The observable outcome of a run.
#[derive(Clone)]
pub struct RunResult {
    pub regs: [u32; 32],
    pub captured: Vec<u8>,
    pub cycles: u64,
    pub instret: u64,
    pub timing: TimingStats,
    pub running: bool,
}

/// Run `spec` in jsim and capture the result. The program is uploaded to the
/// target core's local SRAM (or its org), started, and run for the budget.
pub fn run(spec: &Spec) -> RunResult {
    run_with(spec, &[])
}

/// Like [`run`], but first writes each `(addr, bytes)` preset into the bus — a
/// *fixture*: the input state a kernel expects (param block, geometry blob,
/// camera, framebuffer…). Presets are applied after the program is loaded, so
/// a fixture may target any address (they must not overlap the code region).
/// This is what lets a real kernel run to a deterministic halt instead of
/// looping on zeroed memory.
pub fn run_with(spec: &Spec, pre: &[(u32, Vec<u8>)]) -> RunResult {
    let mut bus = Bus::new();
    for (i, b) in spec.bytes.iter().enumerate() {
        bus.write8(spec.org + i as u32, *b);
    }
    for (addr, blob) in pre {
        for (i, b) in blob.iter().enumerate() {
            bus.write8(addr + i as u32, *b);
        }
    }
    let (pc_addr, ctrl_addr) = match spec.target {
        RiscKind::Gpu => (mem::G_PC, mem::G_CTRL),
        RiscKind::Dsp => (mem::D_PC, mem::D_CTRL),
    };
    bus.write32(pc_addr, spec.org);
    bus.write32(ctrl_addr, mem::RISCGO);
    let mut core = Risc::new(spec.target);
    core.fidelity = spec.fidelity;
    core.run(&mut bus, spec.budget);

    let (addr, len) = spec.capture;
    let captured = (0..len).map(|i| bus.read8(addr + i)).collect();

    RunResult {
        regs: core.regs[0],
        captured,
        cycles: core.cycles,
        instret: core.instret,
        timing: core.pipe.stats.clone(),
        running: core.running,
    }
}

/// A structured difference between two runs.
#[derive(Debug, Default)]
pub struct Diff {
    /// First differing captured byte: (offset, a, b).
    pub mem: Option<(u32, u8, u8)>,
    /// Differing registers: (index, a, b).
    pub regs: Vec<(usize, u32, u32)>,
}

impl Diff {
    pub fn is_empty(&self) -> bool {
        self.mem.is_none() && self.regs.is_empty()
    }
}

/// Compare two results' captured region and register file.
pub fn compare(a: &RunResult, b: &RunResult) -> Diff {
    let mut d = Diff::default();
    for (i, (x, y)) in a.captured.iter().zip(&b.captured).enumerate() {
        if x != y {
            d.mem = Some((i as u32, *x, *y));
            break;
        }
    }
    for i in 0..32 {
        if a.regs[i] != b.regs[i] {
            d.regs.push((i, a.regs[i], b.regs[i]));
        }
    }
    d
}

/// Run a program under `silicon` and `bigpemu` and report divergence. Returns
/// (silicon, bigpemu, diff). A nonempty diff, or a nonzero
/// `bigpemu.timing.bigpemu_divergence`, means the code depends on behavior the
/// two model differently — a hardware-session risk surfaced deterministically.
pub fn profile_diff(spec: &Spec) -> (RunResult, RunResult, Diff) {
    let mut s = spec.clone();
    s.fidelity = Fidelity::Silicon;
    let sil = run(&s);
    s.fidelity = Fidelity::BigPEmu;
    let big = run(&s);
    let d = compare(&sil, &big);
    (sil, big, d)
}

/// Assemble JRISC source through jas (hazard-checked) into a `Spec`-ready byte
/// vector. Returns the bytes or the assembler diagnostics.
pub fn assemble(src: &str, target: RiscKind) -> Result<(Vec<u8>, u32), Vec<String>> {
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
    Ok((out.bytes, out.org))
}

#[cfg(test)]
mod tests {
    use super::*;

    const STOP: &str = "        movei #$00F02114,r30\n        moveq #0,r29\n        store r29,(r30)\n        nop\n";

    fn asm(body: &str) -> Vec<u8> {
        let src = format!("        .gpu\n{body}{STOP}");
        assemble(&src, RiscKind::Gpu).expect("assembles").0
    }

    #[test]
    fn identical_programs_have_no_diff() {
        let prog = asm("        moveq #7,r1\n        movei #$100000,r2\n        store r1,(r2)\n");
        let a = run(&Spec::gpu(prog.clone()));
        let b = run(&Spec::gpu(prog));
        assert!(compare(&a, &b).is_empty());
    }

    #[test]
    fn shadow_diff_catches_divergent_result() {
        let good = asm("        moveq #7,r1\n        movei #$100000,r2\n        store r1,(r2)\n");
        let bad = asm("        moveq #8,r1\n        movei #$100000,r2\n        store r1,(r2)\n");
        let d = compare(&run(&Spec::gpu(good)), &run(&Spec::gpu(bad)));
        assert!(!d.is_empty());
        assert_eq!(d.mem.map(|(_, a, b)| (a, b)), Some((7, 8)));
    }

    #[test]
    fn profile_diff_flags_bigpemu_divergence() {
        // External load consumed across a taken jump — silicon scoreboards it,
        // bigpemu profile does not. jsim counts the divergence.
        let prog = asm(
            "        movei #$00F1B000,r3\n\
             \x20       load (r3),r2\n\
             \x20       jr t,skip\n\
             \x20       nop\n\
             \x20       nop\n\
             skip:   move r2,r5\n\
             \x20       movei #$100000,r6\n\
             \x20       store r5,(r6)\n",
        );
        let (_sil, big, _d) = profile_diff(&Spec::gpu(prog));
        assert!(big.timing.bigpemu_divergence > 0, "expected a bigpemu divergence");
    }

    #[test]
    fn run_with_applies_memory_presets() {
        // A fixture preset is the input state a kernel reads. Here the program
        // loads a long from DRAM $2_0000 (which is zero unless preset) and stores
        // it to the capture region — so the observable result reflects the preset,
        // proving presets reach the bus before the run.
        let prog = asm(
            "        movei #$00020000,r1\n\
             \x20       load (r1),r2\n\
             \x20       nop\n\
             \x20       nop\n\
             \x20       movei #$00100000,r3\n\
             \x20       store r2,(r3)\n",
        );
        let spec = Spec { capture: (0x0010_0000, 4), ..Spec::gpu(prog) };
        // no preset ⇒ reads 0
        assert_eq!(run(&spec).captured, vec![0, 0, 0, 0]);
        // preset $DEADBEEF at the source address ⇒ that value is observed
        let pre = vec![(0x0002_0000u32, 0xDEAD_BEEFu32.to_be_bytes().to_vec())];
        assert_eq!(run_with(&spec, &pre).captured, vec![0xDE, 0xAD, 0xBE, 0xEF]);
    }

    #[test]
    fn golden_snapshot_round_trips() {
        let prog = asm("        moveq #31,r1\n        movei #$100000,r2\n        store r1,(r2)\n");
        let r = run(&Spec::gpu(prog));
        let golden = r.captured.clone();
        // a second run must match the golden
        let prog2 = asm("        moveq #31,r1\n        movei #$100000,r2\n        store r1,(r2)\n");
        let r2 = run(&Spec::gpu(prog2));
        assert_eq!(r2.captured, golden);
    }
}
