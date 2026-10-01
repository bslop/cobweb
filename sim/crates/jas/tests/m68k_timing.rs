//! 68000 timing against silicon (platform timing bench `bench/perf.s`, jagq
//! job 47, NTSC Jaguar, code and data in DRAM, OP on a bare STOP list).
//! Per-iteration 68000 cycles on silicon, from fields x 221,812 cycles:
//!   R  16 taken DBRAs              63 fields / 60000 iterations = 232.9
//!   D  16 x move.l (a0)+,d1 + lea  74 fields / 48000             = 342.0
//!   C  64-byte bytewise copy       113 fields / 9000             = 2785
//! jsim must land within 6% of each (it was 16-49% fast before the refit).

use jag_core::{mem, Jaguar};
use jas::{assemble, Options};

const ORG: u32 = 0x4000;

/// Cycles per iteration of `body` (one loop iteration, d5 = count).
fn cycles_per_iter(body: &str, iters: u32) -> f64 {
    let src = format!(
        "\t.68000\n\t.text\nstart:\n\tmove.l\t#{iters},%d5\n\tlea\t$C0000,%a0\nloop:\n{body}\
         \tsubq.l\t#1,%d5\n\tbne.s\tloop\ndone:\n\tbra.s\tdone\n"
    );
    let out = assemble(&src.replace('%', ""), &Options { org: ORG, start_m68k: true, ..Default::default() });
    assert_eq!(out.errors(), 0, "{:#?}", out.diags);
    let done = out.symbols["done"];
    let loop_ = out.symbols["loop"];
    let mut jag = Jaguar::new();
    for (i, b) in out.bytes.iter().enumerate() {
        jag.bus.write8(ORG + i as u32, *b);
    }
    jag.reset_to_ssp(ORG, mem::DRAM_END);
    // The bench's display-off case: the OP on a bare STOP list.
    jag.bus.write32(0x1000, 0);
    jag.bus.write32(0x1004, 4);
    jag.bus.write32(mem::OLP, 0x1000 << 16);
    while jag.cpu.pc != loop_ {
        jag.step_instruction();
    }
    let c0 = jag.cpu.cycles;
    while jag.cpu.pc != done {
        jag.step_instruction();
    }
    (jag.cpu.cycles - c0) as f64 / iters as f64
}

fn check(name: &str, got: f64, silicon: f64) {
    let r = got / silicon;
    assert!((0.94..=1.06).contains(&r), "{name}: jsim {got:.1} vs silicon {silicon:.1} cycles ({r:.3}x)");
}

#[test]
fn fetch_only_dbra_loop() {
    let got = cycles_per_iter("\tmoveq\t#15,d0\ninner:\tdbra\td0,inner\n", 2000);
    check("R", got, 63.0 * 221_812.0 / 60_000.0);
}

#[test]
fn dram_read_stream() {
    let body = "\tmove.l\t(a0)+,d1\n".repeat(16) + "\tlea\t-64(a0),a0\n";
    let got = cycles_per_iter(&body, 2000);
    check("D", got, 74.0 * 221_812.0 / 48_000.0);
}

#[test]
fn bytewise_copy() {
    let body = "\tlea\t$C0000,a0\n\tlea\t$D0000,a1\n\tlea\t64(a0),a2\ninner:\tmove.b\t(a0)+,(a1)+\n\tcmpa.l\ta2,a0\n\tbne.s\tinner\n";
    let got = cycles_per_iter(body, 500);
    check("C", got, 113.0 * 221_812.0 / 9_000.0);
}
