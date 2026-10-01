//! Byte/word accesses to a RISC core's own local RAM (platform issue 0004,
//! item 1). Silicon takes 32-bit accesses only there: a narrow write never
//! lands and a narrow read is undefined. Under `--fidelity silicon` jsim now
//! drops the write and returns `Risc::NARROW_POISON`; other fidelities keep
//! the forgiving byte-addressable behaviour, and every fidelity counts it.

use jag_core::risc::timing::Fidelity;
use jag_core::{mem, Bus, Risc, RiscKind};
use jas::{assemble, Options};

/// Store $11223344 to $F03800 (32-bit), then STOREB $99 and STOREW $7777
/// into it, read back LOADB/LOADW/LOAD into r10-r12 and halt.
const KERNEL: &str = "\
    movei #$F03800,r1\n\
    movei #$11223344,r2\n\
    store r2,(r1)\n\
    movei #$99,r3\n\
    storeb r3,(r1)\n\
    movei #$F03802,r4\n\
    movei #$7777,r5\n\
    storew r5,(r4)\n\
    loadb (r1),r10\n\
    loadw (r1),r11\n\
    load (r1),r12\n\
    or r10,r10\n\
    or r11,r11\n\
    or r12,r12\n\
    movei #$F02114,r6\n\
    moveq #0,r7\n\
    store r7,(r6)\n\
    nop\n\
    nop\n";

fn run(fid: Fidelity) -> (Risc, Bus) {
    let out = assemble(KERNEL, &Options::default());
    assert_eq!(out.errors(), 0, "{:#?}", out.diags);
    let mut bus = Bus::new();
    for (i, b) in out.bytes.iter().enumerate() {
        bus.write8(mem::G_RAM + i as u32, *b);
    }
    bus.write32(mem::G_PC, mem::G_RAM);
    bus.write32(mem::G_CTRL, mem::RISCGO);
    let mut gpu = Risc::new(RiscKind::Gpu);
    gpu.fidelity = fid;
    gpu.run(&mut bus, 2000);
    (gpu, bus)
}

#[test]
fn silicon_drops_narrow_writes_and_poisons_narrow_reads() {
    let (gpu, mut bus) = run(Fidelity::Silicon);
    assert_eq!(bus.read32(0xF03800), 0x1122_3344, "narrow writes must not land");
    assert_eq!(gpu.regs[0][10], Risc::NARROW_POISON & 0xFF);
    assert_eq!(gpu.regs[0][11], Risc::NARROW_POISON & 0xFFFF);
    assert_eq!(gpu.regs[0][12], 0x1122_3344, "32-bit access is unaffected");
    assert_eq!(gpu.pipe.stats.narrow_sram, 4);
}

#[test]
fn functional_keeps_the_forgiving_model_but_counts() {
    let (gpu, mut bus) = run(Fidelity::Functional);
    assert_eq!(bus.read32(0xF03800), 0x9922_7777);
    assert_eq!(gpu.regs[0][10], 0x99);
    assert_eq!(gpu.pipe.stats.narrow_sram, 4);
}
