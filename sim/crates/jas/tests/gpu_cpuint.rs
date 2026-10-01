//! GPU -> 68000 interrupt (G_CTRL bit 1, `CPUINT`): the STOP-sync wake-up.
//!
//! The 68000 starts a GPU kernel and sleeps in `stop #$2000`; the kernel
//! stores `CPUINT` with GO clear (halting itself), which latches INT1 source 1
//! and, when that source is enabled, wakes the 68000 through its level-2
//! handler. jsim used to ignore `CPUINT`, so the 68000 slept forever.

use jag_core::{mem, Jaguar};
use jas::{assemble, Options, Target};

const CPU_ORG: u32 = 0x4000;
const WOKE: u32 = 0x2000;
const HANDLED: u32 = 0x2004;
const PENDING_SEEN: u32 = 0x2008;

fn cpu_program(int1_enable: u16) -> String {
    format!(
        "\t.68000\n\t.text\n\
         start:\n\
         \tmove.l\t#handler,$100\n\
         \tmove.w\t#${int1_enable:04X},$F000E0\n\
         \tmove.l\t#$F03000,$F02110\n\
         \tmove.l\t#1,$F02114\n\
         \tstop\t#$2000\n\
         \tmove.l\t#1,${WOKE:X}\n\
         idle:\n\
         \tbra\tidle\n\
         handler:\n\
         \tmove.w\t$F000E0,${PENDING_SEEN:X}\n\
         \taddq.l\t#1,${HANDLED:X}\n\
         \tmove.w\t#$0202,$F000E0\n\
         \tmove.w\t#0,$F000E2\n\
         \trte\n"
    )
}

/// Halt with CPUINT set: the idiom from the platform notes (`moveq #2`).
const GPU_KERNEL: &str = "\
    movei #$F02114,r1\n\
    moveq #2,r2\n\
    store r2,(r1)\n\
    nop\n\
    nop\n";

fn boot(int1_enable: u16) -> Jaguar {
    let mut jag = Jaguar::new();
    let cpu = assemble(&cpu_program(int1_enable), &Options { org: CPU_ORG, start_m68k: true, ..Default::default() });
    assert_eq!(cpu.errors(), 0, "68000 assembly errors: {:#?}", cpu.diags);
    for (i, b) in cpu.bytes.iter().enumerate() {
        jag.bus.write8(CPU_ORG + i as u32, *b);
    }
    let gpu = assemble(GPU_KERNEL, &Options { target: Target::Gpu, org: mem::G_RAM, ..Default::default() });
    assert_eq!(gpu.errors(), 0, "GPU assembly errors: {:#?}", gpu.diags);
    for (i, b) in gpu.bytes.iter().enumerate() {
        jag.bus.write8(mem::G_RAM + i as u32, *b);
    }
    jag.reset_to_ssp(CPU_ORG, mem::DRAM_END);
    jag.run_frames(2);
    jag
}

#[test]
fn gpu_cpuint_wakes_a_stopped_68000() {
    let mut jag = boot(mem::C_GPUENA as u16);
    assert_eq!(jag.bus.read32(HANDLED), 1, "the GPU interrupt handler should run once");
    assert_eq!(jag.bus.read32(WOKE), 1, "the 68000 should wake from STOP");
    assert_eq!(jag.bus.read16(PENDING_SEEN) & mem::C_GPUENA as u16, mem::C_GPUENA as u16,
        "INT1 should read the GPU source pending in the handler");
    assert!(!jag.gpu.running, "the kernel halted itself");
    assert_eq!(jag.bus.read32(mem::G_CTRL) & mem::CPUINT, 0, "CPUINT reads back 0");
}

#[test]
fn gpu_cpuint_latches_but_does_not_interrupt_when_disabled() {
    let mut jag = boot(0);
    assert_eq!(jag.bus.read32(HANDLED), 0);
    assert_eq!(jag.bus.read32(WOKE), 0, "nothing should wake the 68000");
    assert_ne!(jag.bus.tom.int1_pending & mem::C_GPUENA as u16, 0, "the source still latches");
}
