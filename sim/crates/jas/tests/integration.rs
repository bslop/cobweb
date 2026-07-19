//! jas integration tests. Two kinds:
//!  1. Encoding is proven by ASSEMBLING a program and RUNNING it in jsim — the
//!     assembler and the emulator can never silently disagree about an opcode.
//!  2. The hazard pass is proven by asserting specific programs are rejected
//!     with the right diagnostic (and their fixed forms accepted).

use jag_core::{mem, Bus, Risc, RiscKind};
use jas::{assemble, Level, Options, Target};

/// Assemble GPU source, upload to GPU SRAM, run, return the bus.
fn run_gpu(src: &str) -> (Bus, jas::Assembled) {
    let out = assemble(src, &Options::default());
    assert_eq!(out.errors(), 0, "assembly errors: {:#?}", out.diags);
    let mut bus = Bus::new();
    for (i, b) in out.bytes.iter().enumerate() {
        bus.write8(mem::G_RAM + i as u32, *b);
    }
    bus.write32(mem::G_PC, mem::G_RAM);
    bus.write32(mem::G_CTRL, mem::RISCGO);
    let mut gpu = Risc::new(RiscKind::Gpu);
    gpu.run(&mut bus, 500);
    (bus, out)
}

fn errors_of(src: &str) -> Vec<String> {
    let out = assemble(src, &Options::default());
    out.diags
        .iter()
        .filter(|d| d.level == Level::Error)
        .map(|d| d.msg.clone())
        .collect()
}

#[test]
fn assembles_and_runs_arithmetic() {
    // moveq/add/shlq/store round-trip: 5+3=8, <<2 = 32, store to DRAM.
    let (mut bus, _) = run_gpu(
        "        .gpu\n\
         start:  moveq #5,r1\n\
         \x20       moveq #3,r2\n\
         \x20       add r1,r2\n\
         \x20       shlq #2,r2\n\
         \x20       movei #$00100000,r3\n\
         \x20       store r2,(r3)\n\
         \x20       movei #$00F02114,r4\n\
         \x20       moveq #0,r5\n\
         \x20       store r5,(r4)\n\
         \x20       nop\n",
    );
    assert_eq!(bus.read32(0x0010_0000), 32);
}

#[test]
fn assembles_jr_loop_with_labels() {
    // Sum 1..5 with a JR back-edge and a filled delay slot; forward + backward
    // label resolution both exercised.
    let src = "        .gpu\n\
        \x20       moveq #0,r1\n\
        \x20       moveq #5,r2\n\
        loop:   add r2,r1\n\
        \x20       subq #1,r2\n\
        \x20       cmpq #0,r2\n\
        \x20       jr ne,loop\n\
        \x20       nop\n\
        \x20       movei #$00100000,r3\n\
        \x20       store r1,(r3)\n\
        \x20       movei #$00F02114,r4\n\
        \x20       moveq #0,r5\n\
        \x20       store r5,(r4)\n\
        \x20       nop\n";
    let (mut bus, _) = run_gpu(src);
    assert_eq!(bus.read32(0x0010_0000), 15);
}

#[test]
fn movei_immediate_word_order() {
    // MOVEI must emit opcode, low16, high16 (each big-endian) so jsim loads the
    // full 32-bit constant.
    let out = assemble("        .gpu\n        movei #$CAFEBABE,r1\n", &Options::default());
    assert_eq!(out.errors(), 0);
    // bytes: [op_hi op_lo][BA BE][CA FE]
    assert_eq!(&out.bytes[2..6], &[0xBA, 0xBE, 0xCA, 0xFE]);
}

// ── hazard pass ──────────────────────────────────────────────────────────────

#[test]
fn rejects_waw_into_load_shadow() {
    // load into r2, then overwrite r2 before reading it = bug 13.
    let errs = errors_of(
        "        .gpu\n\
         \x20       movei #$100000,r3\n\
         \x20       load (r3),r2\n\
         \x20       moveq #3,r2\n",
    );
    assert!(errs.iter().any(|e| e.contains("bug 13")), "got: {errs:?}");
}

#[test]
fn accepts_waw_guarded_by_read() {
    // reading r2 (or r2,r2) settles the scoreboard before the overwrite.
    let out = assemble(
        "        .gpu\n\
         \x20       movei #$100000,r3\n\
         \x20       load (r3),r2\n\
         \x20       or r2,r2\n\
         \x20       moveq #3,r2\n",
        &Options::default(),
    );
    assert_eq!(out.errors(), 0, "{:#?}", out.diags);
}

#[test]
fn hazard_line_survives_conditional_removal() {
    // A `.if 0` block (removed by the preprocessor) before a hazard must NOT
    // shift the reported line: the diagnostic must name the real source line
    // (the write at line 8), not the post-collapse position (line 4).
    let src = "\t.gpu\n\
        \t.org $F03000\n\
        \t.if 0\n\
        \tnop\n\
        \tnop\n\
        \tnop\n\
        \t.endif\n\
        \tload (r0),r5\n\
        \tmoveq #1,r5\n\
        \tnop\n";
    let out = assemble(src, &Options::default());
    let bug13 = out.diags.iter().find(|d| d.msg.contains("bug 13")).expect("bug-13 diag");
    assert_eq!(bug13.line, 9, "write reported at wrong source line: {}", bug13.line);
    assert!(bug13.msg.contains("from line 8"), "producer line wrong: {}", bug13.msg);
}

#[test]
fn rejects_indexed_store_of_unsettled_reg() {
    // div into r2, then store r2 via (r14+n) without touching it = erratum.
    let errs = errors_of(
        "        .gpu\n\
         \x20       movei #$100000,r14\n\
         \x20       moveq #20,r2\n\
         \x20       moveq #3,r1\n\
         \x20       div r1,r2\n\
         \x20       store r2,(r14+1)\n",
    );
    assert!(errs.iter().any(|e| e.contains("errata")), "got: {errs:?}");
}

#[test]
fn rejects_movei_in_delay_slot() {
    let errs = errors_of(
        "        .gpu\n\
         loop:   jr loop\n\
         \x20       movei #1,r1\n",
    );
    assert!(errs.iter().any(|e| e.contains("MOVEI in a delay slot")), "got: {errs:?}");
}

#[test]
fn rejects_two_sequential_jumps() {
    let errs = errors_of(
        "        .gpu\n\
         a:      jr a\n\
         b:      jr b\n\
         \x20       nop\n",
    );
    assert!(errs.iter().any(|e| e.contains("two sequential jumps")), "got: {errs:?}");
}

#[test]
fn rejects_far_jr() {
    // a jr whose target is far past the 5-bit word range.
    let mut src = String::from("        .gpu\nstart:  jr far\n        nop\n");
    for _ in 0..40 {
        src.push_str("        nop\n");
    }
    src.push_str("far:    nop\n");
    let errs = errors_of(&src);
    assert!(errs.iter().any(|e| e.contains("out of range")), "got: {errs:?}");
}

#[test]
fn dsp_target_encodes_dsp_only_ops() {
    // subqmod/mirror are DSP-only and must not error under --dsp.
    let opts = Options { target: Target::Dsp, org: 0xF1_B000, ..Options::default() };
    let out = assemble("        .dsp\n        subqmod #4,r2\n        mirror r3\n", &opts);
    assert_eq!(out.errors(), 0, "{:#?}", out.diags);
}

#[test]
fn equ_and_expressions() {
    let out = assemble(
        "        .gpu\n\
         BASE    equ $F03000\n\
         \x20       movei #BASE+16*4,r1\n",
        &Options::default(),
    );
    assert_eq!(out.errors(), 0, "{:#?}", out.diags);
    // BASE + 64 = 0xF03040 -> low $3040, high $00F0
    assert_eq!(&out.bytes[2..6], &[0x30, 0x40, 0x00, 0xF0]);
}

// ── 68000 mode (validated in jag-core's interpreter) ─────────────────────────

use jag_core::{Debugger, M68k};

/// Assemble a 68000 program (org $4000 in DRAM), run it in the interpreter for
/// `steps`, return the bus.
fn run_68k(src: &str, steps: u32) -> Bus {
    let opts = Options { target: Target::Gpu, org: 0x4000, check_hazards: false, ..Default::default() };
    let out = assemble(src, &opts);
    assert_eq!(out.errors(), 0, "68k assembly errors: {:#?}", out.diags);
    let mut bus = Bus::new();
    for (i, b) in out.bytes.iter().enumerate() {
        bus.write8(0x4000 + i as u32, *b);
    }
    let mut cpu = M68k::new();
    cpu.reset(&mut bus);
    cpu.a[7] = 0x1F_0000; // stack
    cpu.set_pc(0x4000);
    let mut dbg = Debugger::new();
    for _ in 0..steps {
        cpu.step(&mut bus, &mut dbg);
    }
    bus
}

#[test]
fn m68k_moveq_and_store() {
    // moveq #7,d0 ; move.l d0,$100000
    let mut bus = run_68k("        .68000\n        moveq #7,d0\n        move.l d0,$100000\n        nop\n", 3);
    assert_eq!(bus.read32(0x0010_0000), 7);
}

#[test]
fn m68k_arithmetic_and_addressing() {
    // build 40 in d1 via adds, store through (a0)
    let mut bus = run_68k(
        "        .68000\n\
         \x20       movea.l #$100000,a0\n\
         \x20       moveq #10,d1\n\
         \x20       add.l d1,d1\n\
         \x20       addq.l #4,d1\n\
         \x20       move.l d1,(a0)\n\
         \x20       nop\n",
        5,
    );
    assert_eq!(bus.read32(0x0010_0000), 24); // 10+10+4
}

#[test]
fn m68k_branch_loop() {
    // sum 1..5 in d0 with a dbra loop
    let mut bus = run_68k(
        "        .68000\n\
         \x20       moveq #0,d0\n\
         \x20       moveq #5,d1\n\
         loop:   add.l d1,d0\n\
         \x20       subq.l #1,d1\n\
         \x20       bne loop\n\
         \x20       move.l d0,$100000\n\
         \x20       nop\n",
        40,
    );
    assert_eq!(bus.read32(0x0010_0000), 15);
}

#[test]
fn m68k_movem_roundtrip() {
    // save d0-d2 to stack, clobber, restore, store d1
    let mut bus = run_68k(
        "        .68000\n\
         \x20       moveq #11,d0\n\
         \x20       moveq #22,d1\n\
         \x20       moveq #33,d2\n\
         \x20       movem.l d0-d2,-(a7)\n\
         \x20       moveq #0,d1\n\
         \x20       movem.l (a7)+,d0-d2\n\
         \x20       move.l d1,$100000\n\
         \x20       nop\n",
        20,
    );
    assert_eq!(bus.read32(0x0010_0000), 22);
}

// ── COBWEB_ISSUES (aerodagger dogfooding) regressions ────────────────────────

#[test]
fn waw_window_does_not_flag_distant_write() {
    // Issue #1: a write far past the load's shadow must NOT be flagged (the
    // load has long since settled). Load into r2, 30 independent ops, write r2.
    let mut src = String::from("        .gpu\n        movei #$100000,r3\n        load (r3),r2\n");
    for _ in 0..30 {
        src.push_str("        nop\n");
    }
    src.push_str("        moveq #3,r2\n");
    assert_eq!(errors_of(&src).len(), 0, "distant write wrongly flagged (issue #1)");
}

#[test]
fn waw_window_still_flags_in_shadow() {
    // ...but a write a few instructions after the load IS still caught.
    let src = "        .gpu\n        movei #$100000,r3\n        load (r3),r2\n\
               \x20       nop\n        nop\n        moveq #3,r2\n";
    assert!(errors_of(src).iter().any(|e| e.contains("bug 13")), "close WAW must still fire");
}

#[test]
fn pragma_waives_hazard_on_its_line() {
    let src = "        .gpu\n        movei #$100000,r3\n        load (r3),r2\n\
               \x20       moveq #3,r2   ; jas:allow reviewed\n";
    assert_eq!(errors_of(src).len(), 0, "; jas:allow should waive the WAW on that line");
}

#[test]
fn m68k_start_flag_assembles_pure_68k() {
    // Issue #2: a pure-68k file with no .68000 directive assembles in 68k mode.
    let opts = Options { target: Target::Gpu, org: 0x4000, start_m68k: true, check_hazards: false, ..Default::default() };
    let out = assemble("        movem.l a1-a2,-(sp)\n        move.w #$4001,d0\n        rts\n", &opts);
    assert_eq!(out.errors(), 0, "pure-68k with --68000 must assemble: {:#?}", out.diags);
}
