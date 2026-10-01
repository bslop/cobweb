//! The INT2 bus-priority drop (platform issue 0008, item 1).
//!
//! On silicon, taking a 68000 interrupt lowers the GPU's and Blitter's bus
//! priority until `INT2` is written. A level-2 handler that acknowledged INT1
//! and returned with `rte` but never wrote `INT2` left every GPU DRAM load
//! stalled for good. These tests run a 68000 with a vertical-interrupt handler
//! (with and without the `INT2` write) next to a GPU kernel that counts in
//! DRAM, and check what `--fidelity silicon` and `--strict=int2` make of it.

use jag_core::risc::timing::Fidelity;
use jag_core::{mem, Jaguar, Strict};
use jas::{assemble, Options, Target};

const CPU_ORG: u32 = 0x4000;
/// DRAM long the GPU kernel increments forever.
const GPU_COUNT: u32 = 0x2000;
/// DRAM long the 68000 handler increments once per vertical interrupt.
const VI_COUNT: u32 = 0x2010;

fn cpu_program(write_int2: bool) -> String {
    let int2 = if write_int2 { "\tmove.w\t#0,$F000E2\n" } else { "" };
    format!(
        "\t.68000\n\t.text\n\
         start:\n\
         \tmove.l\t#handler,$100\n\
         \tmove.w\t#$0100,$F0004E\n\
         \tmove.w\t#$0001,$F000E0\n\
         \tmove.w\t#$2000,sr\n\
         idle:\n\
         \tbra\tidle\n\
         handler:\n\
         \taddq.l\t#1,${VI_COUNT:X}\n\
         \tmove.w\t#$0101,$F000E0\n\
         {int2}\
         \trte\n"
    )
}

const GPU_KERNEL: &str = "\
    movei #$2000,r1\n\
loop:\n\
    load (r1),r2\n\
    or r2,r2\n\
    addq #1,r2\n\
    store r2,(r1)\n\
    jr loop\n\
    nop\n";

/// Boot both programs and run `frames` fields; return the machine.
fn boot(write_int2: bool, fid: Fidelity, strict: Strict, frames: u64) -> Jaguar {
    let mut jag = Jaguar::new();
    jag.gpu.fidelity = fid;
    jag.dsp.fidelity = fid;
    jag.set_strict(strict);

    let cpu = assemble(&cpu_program(write_int2), &Options { org: CPU_ORG, start_m68k: true, ..Default::default() });
    assert_eq!(cpu.errors(), 0, "68000 assembly errors: {:#?}", cpu.diags);
    for (i, b) in cpu.bytes.iter().enumerate() {
        jag.bus.write8(CPU_ORG + i as u32, *b);
    }
    let gpu = assemble(GPU_KERNEL, &Options { target: Target::Gpu, org: mem::G_RAM, ..Default::default() });
    assert_eq!(gpu.errors(), 0, "GPU assembly errors: {:#?}", gpu.diags);
    for (i, b) in gpu.bytes.iter().enumerate() {
        jag.bus.write8(mem::G_RAM + i as u32, *b);
    }
    jag.bus.write32(mem::G_PC, mem::G_RAM);
    jag.bus.write32(mem::G_CTRL, mem::RISCGO);

    jag.reset_to_ssp(CPU_ORG, mem::DRAM_END);
    jag.run_frames(frames);
    jag
}

/// The GPU counter's progress over fields 4..8, after several interrupts.
fn gpu_progress_after_interrupts(write_int2: bool, fid: Fidelity) -> (u32, Jaguar) {
    let mut jag = boot(write_int2, fid, Strict::default(), 4);
    assert!(jag.bus.read32(VI_COUNT) >= 2, "the vertical interrupt never fired");
    let before = jag.bus.read32(GPU_COUNT);
    jag.run_frames(4);
    (jag.bus.read32(GPU_COUNT).wrapping_sub(before), jag)
}

#[test]
fn silicon_gpu_starves_after_a_handler_that_never_writes_int2() {
    let (progress, jag) = gpu_progress_after_interrupts(false, Fidelity::Silicon);
    assert_eq!(progress, 0, "the GPU kept loading from DRAM with its priority lowered");
    assert!(jag.gpu.pipe.stats.int2_starved > 0);
    assert!(jag.bus.tom.int2_lowered);
    // Held before the load, so G_PC names it, as it did on the bench.
    assert_eq!(jag.gpu.pc, mem::G_RAM + 6, "the GPU should be held at its DRAM load");
}

#[test]
fn silicon_gpu_runs_when_the_handler_writes_int2() {
    let (progress, jag) = gpu_progress_after_interrupts(true, Fidelity::Silicon);
    assert!(progress > 1000, "the GPU should keep counting, got {progress}");
    assert_eq!(jag.bus.tom.rte_without_int2, 0);
}

#[test]
fn functional_fidelity_only_counts_the_missing_int2() {
    // Not modelled outside silicon fidelity, but always reported.
    let (progress, jag) = gpu_progress_after_interrupts(false, Fidelity::Functional);
    assert!(progress > 1000);
    assert!(jag.bus.tom.rte_without_int2 >= 2);
}

#[test]
fn strict_int2_stops_at_the_rte() {
    let mut jag = boot(false, Fidelity::Functional, Strict { int2: true, ..Default::default() }, 8);
    let f = jag.strict_fault().cloned().expect("--strict=int2 should have stopped the run");
    assert_eq!(f.kind, "rte_without_int2");
    assert_eq!(f.master, "68k");
    assert_eq!(jag.bus.read16(f.pc), 0x4E73, "the fault PC should be the rte");
    assert_eq!(jag.bus.read32(VI_COUNT), 1, "it should stop at the first handler's rte");
}

#[test]
fn strict_int2_is_quiet_when_the_handler_writes_int2() {
    let jag = boot(true, Fidelity::Silicon, Strict { int2: true, ..Default::default() }, 8);
    assert!(jag.strict_fault().is_none());
}

#[test]
fn strict_exempt_by_pc_keeps_running() {
    let probe = boot(false, Fidelity::Functional, Strict { int2: true, ..Default::default() }, 8);
    let rte_pc = probe.strict_fault().unwrap().pc;
    let mut s = Strict { int2: true, ..Default::default() };
    s.add_exempt(&format!("int2:pc={rte_pc:X}")).unwrap();
    let jag = boot(false, Fidelity::Functional, s, 8);
    assert!(jag.strict_fault().is_none(), "the exempted rte stopped the run");
    assert!(jag.bus.tom.rte_without_int2 >= 2, "an exempt fault is still counted");
}

#[test]
fn a_long_write_to_int1_also_writes_int2() {
    let mut jag = Jaguar::new();
    jag.bus.tom.int2_lowered = true;
    jag.bus.write32(mem::INT1, 0x0101_0000);
    assert!(!jag.bus.tom.int2_lowered);
    assert_eq!(jag.bus.tom.int1_enable, 0x0001);
}

#[test]
fn a_gpu_looping_on_dram_does_not_starve_the_68000() {
    // The GPU's DRAM cycles used to be billed to the 68000 through the OP tax;
    // under functional fidelity the 68000 then ran ~10 instructions a field and
    // the vertical interrupt handler took several fields to finish.
    let jag = boot(true, Fidelity::Functional, Strict::default(), 8);
    assert!(jag.cpu.instret > 10_000, "68000 ran only {} instructions in 8 fields", jag.cpu.instret);
}
