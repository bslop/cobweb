//! `JAGEMU_IRQ_DELAY` (`M68k::irq_delay`): interrupt delivery latency.
//!
//! jsim hands a request to the 68000 at the next instruction boundary, so a
//! vertical-interrupt handler reads the same `VC` at entry in every field. On
//! silicon the first `VC` read alternated between three half-lines (platform
//! issue 0008, item 2). A delay range spreads entries deterministically.

use jag_core::{mem, Jaguar};
use jas::{assemble, Options};

const CPU_ORG: u32 = 0x4000;
const SLOT: u32 = 0x2000; // handler's write pointer
const RING: u32 = 0x2100; // VC at each handler entry, one word per field

const PROGRAM: &str = "\t.68000\n\t.text\n\
     start:\n\
     \tmove.l\t#handler,$100\n\
     \tmove.l\t#$2100,$2000\n\
     \tmove.w\t#507,$F0004E\n\
     \tmove.w\t#$0001,$F000E0\n\
     \tmove.w\t#$2000,sr\n\
     idle:\n\
     \tbra\tidle\n\
     handler:\n\
     \tmove.l\ta0,-(a7)\n\
     \tmovea.l\t$2000,a0\n\
     \tmove.w\t$F00006,(a0)+\n\
     \tmove.l\ta0,$2000\n\
     \tmovea.l\t(a7)+,a0\n\
     \tmove.w\t#$0101,$F000E0\n\
     \tmove.w\t#0,$F000E2\n\
     \trte\n";

/// `VC` (masked to the counter bits) at handler entry for each of 8 fields.
fn entry_vcs(delay: (u32, u32)) -> Vec<u16> {
    let mut jag = Jaguar::new();
    jag.cpu.irq_delay = delay;
    let out = assemble(PROGRAM, &Options { org: CPU_ORG, start_m68k: true, ..Default::default() });
    assert_eq!(out.errors(), 0, "{:#?}", out.diags);
    for (i, b) in out.bytes.iter().enumerate() {
        jag.bus.write8(CPU_ORG + i as u32, *b);
    }
    jag.reset_to_ssp(CPU_ORG, mem::DRAM_END);
    jag.run_frames(10);
    let n = (jag.bus.read32(SLOT) - RING) / 2;
    assert!(n >= 8, "only {n} handler entries");
    (2..10).map(|i| jag.bus.read16(RING + 2 * i) & 0x7FF).collect()
}

#[test]
fn without_a_delay_every_entry_reads_the_same_vc() {
    let v = entry_vcs((0, 0));
    assert!(v.iter().all(|&x| x == v[0]), "{v:?}");
}

#[test]
fn a_fixed_delay_moves_every_entry_later() {
    let base = entry_vcs((0, 0))[0];
    let v = entry_vcs((900, 900));
    assert!(v.iter().all(|&x| x == v[0]), "{v:?}");
    assert!(v[0] >= base + 2, "900 cycles is over two half-lines: {base} -> {}", v[0]);
}

#[test]
fn a_delay_range_varies_the_entry_vc_between_fields() {
    let v = entry_vcs((0, 900));
    let distinct: std::collections::BTreeSet<_> = v.iter().collect();
    assert!(distinct.len() >= 2, "{v:?}");
    let again = entry_vcs((0, 900));
    assert_eq!(v, again, "the spread must be deterministic");
}
