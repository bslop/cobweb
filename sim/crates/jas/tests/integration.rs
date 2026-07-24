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

/// Assemble a DSP program at Jerry's D_RAM and stage it started (RISCGO), so the
/// caller can `dsp.run(&mut bus, n)` in chunks and inject "68k" bus writes in
/// between — the shape of a resident poll-loop / mailbox handshake.
fn dsp_staged(src: &str) -> (Bus, Risc) {
    let opts = Options { target: Target::Dsp, org: mem::D_RAM, ..Default::default() };
    let out = assemble(src, &opts);
    assert_eq!(out.errors(), 0, "assembly errors: {:#?}", out.diags);
    let mut bus = Bus::new();
    for (i, b) in out.bytes.iter().enumerate() {
        bus.write8(mem::D_RAM + i as u32, *b);
    }
    bus.write32(mem::D_PC, mem::D_RAM);
    bus.write32(mem::D_CTRL, mem::RISCGO);
    (bus, Risc::new(RiscKind::Dsp))
}

/// A resident DSP poll loop over a mailbox at `cmd_addr`: spin until the word is
/// nonzero, then write it to the `mark` address (proving 68k→DSP visibility of
/// the command) and clear the mailbox to 0 (proving DSP→68k visibility of the
/// acknowledgement), then halt-spin. Mirrors dsp_pose.das's main_loop.
fn poll_kernel(cmd_addr: u32, mark: u32) -> String {
    format!(
        "        .dsp\n\
         main_loop:\n\
         \x20       movei #${cmd_addr:08X},r0\n\
         \x20       load (r0),r1\n\
         \x20       nop\n\
         \x20       nop\n\
         \x20       movei #main_loop,r2\n\
         \x20       cmpq #0,r1\n\
         \x20       jump EQ,(r2)\n\
         \x20       nop\n\
         \x20       nop\n\
         \x20       movei #${mark:08X},r3\n\
         \x20       store r1,(r3)\n\
         \x20       moveq #0,r4\n\
         \x20       store r4,(r0)\n\
         done:   movei #done,r5\n\
         \x20       jump T,(r5)\n\
         \x20       nop\n\
         \x20       nop\n"
    )
}

#[test]
fn loadp_hidata_is_stale_inside_the_load_shadow() {
    // COBWEB_GAP §"corroborating divergence": G_HIDATA is NOT scoreboarded on
    // silicon — reading it soon after LOADP does not stall, it returns the STALE
    // value (a kernel that did so rendered garbage on hardware while jsim, which
    // landed it instantly, looked correct). Needs a timed profile to observe.
    let src = "        .gpu\n\
         \x20       movei #$00F02118,r2\n\
         \x20       movei #$AAAAAAAA,r1\n\
         \x20       store r1,(r2)\n\
         \x20       movei #$00100020,r3\n\
         \x20       movei #$11112222,r4\n\
         \x20       store r4,(r3)\n\
         \x20       movei #$00100024,r5\n\
         \x20       movei #$33334444,r6\n\
         \x20       store r6,(r5)\n\
         \x20       loadp (r3),r7\n\
         \x20       load (r2),r8\n\
         \x20       nop\n\
         \x20       nop\n\
         \x20       movei #$00100000,r9\n\
         \x20       store r8,(r9)\n\
         \x20       .rept 24\n\
         \x20       nop\n\
         \x20       .endr\n\
         \x20       load (r2),r10\n\
         \x20       nop\n\
         \x20       nop\n\
         \x20       movei #$00100004,r11\n\
         \x20       store r10,(r11)\n";
    let out = assemble(src, &Options { check_hazards: false, ..Default::default() });
    assert_eq!(out.errors(), 0, "assembly errors: {:#?}", out.diags);
    let mut bus = Bus::new();
    for (i, b) in out.bytes.iter().enumerate() {
        bus.write8(mem::G_RAM + i as u32, *b);
    }
    bus.write32(mem::G_PC, mem::G_RAM);
    bus.write32(mem::G_CTRL, mem::RISCGO);
    let mut gpu = Risc::new(RiscKind::Gpu);
    gpu.fidelity = jag_core::risc::Fidelity::Silicon;
    gpu.run(&mut bus, 4000);

    assert_eq!(
        bus.read32(0x0010_0000),
        0xAAAA_AAAA,
        "G_HIDATA read inside the LOADP shadow must see the STALE value (silicon does not scoreboard it)"
    );
    assert_eq!(
        bus.read32(0x0010_0004),
        0x1111_2222,
        "G_HIDATA must settle to the phrase's high long once the load lands"
    );
}

#[test]
fn storep_loadp_phrase_byte_order() {
    // COBWEB_BUG_storep_loadp_byteorder (hardware-confirmed): the Jaguar is
    // big-endian, so a phrase's HIGH long (G_HIDATA) lands at the LOWER address
    // and the low long (Rn) at +4. jsim had them swapped — code round-tripped
    // fine in jsim but rendered transposed on silicon. Verify the actual memory
    // layout, not just a self-consistent round-trip.
    let (mut bus, _) = run_gpu(
        "        .gpu\n\
         \x20       movei #$AAAAAAAA,r1\n\
         \x20       movei #$00F02118,r2\n\
         \x20       store r1,(r2)\n\
         \x20       movei #$BBBBBBBB,r3\n\
         \x20       movei #$00100000,r4\n\
         \x20       storep r3,(r4)\n\
         \x20       loadp (r4),r6\n\
         \x20       nop\n\
         \x20       nop\n\
         \x20       movei #$00100010,r7\n\
         \x20       storep r6,(r7)\n\
         halt:   movei #halt,r0\n\
         \x20       jump T,(r0)\n\
         \x20       nop\n\
         \x20       nop\n",
    );
    // STOREP: high long (hidata) at [A], low long (Rn) at [A+4].
    assert_eq!(bus.read32(0x0010_0000), 0xAAAA_AAAA, "STOREP high long must land at the lower address");
    assert_eq!(bus.read32(0x0010_0004), 0xBBBB_BBBB, "STOREP low long must land at +4");
    // LOADP: hidata ← [A], Rd ← [A+4]; the re-STOREP echoes them unchanged.
    assert_eq!(bus.read32(0x0010_0010), 0xAAAA_AAAA, "LOADP must read the high long from the lower address");
    assert_eq!(bus.read32(0x0010_0014), 0xBBBB_BBBB, "LOADP must read the low long from +4");
}

#[test]
fn dsp_cmd_mailbox_jerry_sram() {
    // COBWEB_ISSUE_dsp_dcmd_unverified: the 68k↔DSP D_CMD mailbox in Jerry SRAM.
    // A resident DSP poll loop must observe a command the 68k writes into Jerry
    // SRAM ($F1C338, the real dsp_pose CMD_D), dispatch on it, and clear it
    // visibly back to the 68k — no stale/cached read in either direction.
    const CMD_D: u32 = 0x00F1_C338; // Jerry SRAM
    const MARK: u32 = 0x0000_1000; // DRAM marker
    let (mut bus, mut dsp) = dsp_staged(&poll_kernel(CMD_D, MARK));

    // The DSP polls with no command pending: it must NOT dispatch.
    dsp.run(&mut bus, 300);
    assert_eq!(bus.read32(MARK), 0, "DSP dispatched with no command pending");
    assert!(dsp.running, "resident DSP loop should still be running");

    // The 68k writes the command into Jerry SRAM while the DSP is mid-poll.
    bus.write32(CMD_D, 1);
    dsp.run(&mut bus, 800);

    assert_eq!(bus.read32(MARK), 1, "68k→DSP: the DSP never observed the command");
    assert_eq!(bus.read32(CMD_D), 0, "DSP→68k: the DSP's clear is not visible");
}

#[test]
fn dsp_cmd_mailbox_dram() {
    // Same handshake through a DRAM mailbox (some handshakes use dsp_mailbox in
    // DRAM); 68k↔DSP visibility must hold there too, not just in Jerry SRAM.
    const CMD: u32 = 0x0000_2000; // DRAM mailbox
    const MARK: u32 = 0x0000_1000; // DRAM marker
    let (mut bus, mut dsp) = dsp_staged(&poll_kernel(CMD, MARK));

    dsp.run(&mut bus, 300);
    assert_eq!(bus.read32(MARK), 0, "DSP dispatched with no command pending");

    bus.write32(CMD, 2);
    dsp.run(&mut bus, 800);

    assert_eq!(bus.read32(MARK), 2, "68k→DSP: DRAM command not observed");
    assert_eq!(bus.read32(CMD), 0, "DSP→68k: DRAM clear not visible");
}

#[test]
fn dsp_mailbox_serviced_while_68k_stopped() {
    // COBWEB_ISSUE_dsp_dcmd_unverified, the STOP scenario: the 68k writes the
    // command and parks itself in STOP; the resident DSP must service the mailbox
    // and clear it CONCURRENTLY, driven by the scheduler — not by the 68k running.
    // The scheduler gives the DSP its budget every step even while the CPU sleeps
    // (a stopped 68k still advances time at 4 cyc/step), so the co-transform makes
    // progress and the clear is visible without the 68k executing an instruction.
    const CMD_D: u32 = 0x00F1_C338;
    const MARK: u32 = 0x0000_1000;
    let mut jag = jag_core::Jaguar::new();
    jag.reset_to_ssp(0x4000, mem::DRAM_END);

    // Stage the resident DSP poll kernel and start it.
    let opts = Options { target: Target::Dsp, org: mem::D_RAM, ..Default::default() };
    let out = assemble(&poll_kernel(CMD_D, MARK), &opts);
    assert_eq!(out.errors(), 0, "assembly errors: {:#?}", out.diags);
    for (i, b) in out.bytes.iter().enumerate() {
        jag.bus.write8(mem::D_RAM + i as u32, *b);
    }
    jag.bus.write32(mem::D_PC, mem::D_RAM);
    jag.bus.write32(mem::D_CTRL, mem::RISCGO);

    // The 68k has issued the command and gone to sleep in STOP.
    jag.bus.write32(CMD_D, 1);
    jag.cpu.stopped = true;

    // Drive the scheduler; the CPU stays asleep, the DSP does the work.
    for _ in 0..4000 {
        jag.step_instruction();
        if jag.bus.read32(CMD_D) == 0 {
            break;
        }
    }

    assert!(jag.cpu.stopped, "the 68k should have stayed asleep in STOP");
    assert_eq!(jag.bus.read32(MARK), 1, "DSP did not service the mailbox while the 68k slept");
    assert_eq!(jag.bus.read32(CMD_D), 0, "DSP→68k: the clear is not visible after servicing");
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
fn if_single_equals_is_equality() {
    // rmac uses a lone `=` (not only `==`) for equality in `.if`, and an
    // undefined symbol is 0. So `.if NOFILL=0` must take the TRUE branch and
    // `.if NOFILL=1` the else branch. Before the lexer fix a lone `=` failed to
    // lex, the condition evaluated false, and the whole conditional block was
    // silently dropped — which is exactly how the gpu_geotex blit LAUNCH (inside
    // `.if NOFILL=0`) went missing and the kernel never rendered.
    let (mut bus, _) = run_gpu(
        "        .gpu\n\
         \x20       .if NOFILL=0\n\
         \x20       moveq #1,r0\n\
         \x20       .else\n\
         \x20       moveq #9,r0\n\
         \x20       .endif\n\
         \x20       .if NOFILL=1\n\
         \x20       moveq #8,r2\n\
         \x20       .else\n\
         \x20       moveq #3,r2\n\
         \x20       .endif\n\
         \x20       movei #$00100000,r1\n\
         \x20       store r0,(r1)\n\
         \x20       movei #$00100004,r3\n\
         \x20       store r2,(r3)\n",
    );
    assert_eq!(bus.read32(0x0010_0000), 1, "`.if NOFILL=0` must take the =0 branch");
    assert_eq!(bus.read32(0x0010_0004), 3, "`.if NOFILL=1` must take the else branch");
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
fn accepts_waw_guarded_by_move_touch() {
    // `move r2,r2` reads its source, so it settles the scoreboard exactly like
    // `or r2,r2` — both spellings must be credited (COBWEB_REQ_jcc68k_adoption
    // item 5 asked for parity between the two).
    let out = assemble(
        "        .gpu\n\
         \x20       movei #$100000,r3\n\
         \x20       load (r3),r2\n\
         \x20       move r2,r2\n\
         \x20       moveq #3,r2\n",
        &Options::default(),
    );
    assert_eq!(out.errors(), 0, "{:#?}", out.diags);
}

#[test]
fn accepts_indexed_store_guarded_by_move_touch() {
    let out = assemble(
        "        .gpu\n\
         \x20       movei #$100000,r14\n\
         \x20       moveq #20,r2\n\
         \x20       moveq #3,r1\n\
         \x20       div r1,r2\n\
         \x20       move r2,r2\n\
         \x20       store r2,(r14+1)\n",
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
fn rejects_adjacent_mmults() {
    // Two MMULTs with nothing between hard-wedge real Tom (bug 23); jas must
    // refuse them (calib p_mm_mm2 on silicon 2026-07-24).
    let errs = errors_of(
        "        .gpu\n\
         \x20       mmult r2,r4\n\
         \x20       mmult r2,r5\n",
    );
    assert!(errs.iter().any(|e| e.contains("adjacent MMULTs")), "got: {errs:?}");
}

#[test]
fn accepts_mmults_separated_by_a_settle() {
    // A single instruction between the MMULTs clears the wedge (p_mm_mm2s ran
    // clean with a gap); no adjacency error.
    let errs = errors_of(
        "        .gpu\n\
         \x20       mmult r2,r4\n\
         \x20       nop\n\
         \x20       mmult r2,r5\n",
    );
    assert!(!errs.iter().any(|e| e.contains("adjacent MMULTs")), "got: {errs:?}");
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
fn bare_dot_long_is_rmac_alignment() {
    // rmac's `.long` with no operands aligns to a longword boundary; jas used
    // to silently emit nothing, leaving the following table 2-misaligned (GPU
    // loads force-align on silicon, so it read garbage).
    let out = assemble(
        "        .gpu\n\
         \x20       dc.w 1\n\
         \x20       .long\n\
         tab:    dc.l $DEADBEEF\n",
        &Options::default(),
    );
    assert_eq!(out.errors(), 0, "{:#?}", out.diags);
    assert_eq!(out.symbols["tab"] % 4, 0, "table not long-aligned");
    assert_eq!(out.bytes.len(), 8); // 2 data + 2 pad + 4 data
    assert_eq!(&out.bytes[4..8], &[0xDE, 0xAD, 0xBE, 0xEF]);
}

#[test]
fn empty_data_directive_warns() {
    // `dc.w` with no operands emits nothing — that is never intended, so it
    // must at least warn instead of passing silently.
    let out = assemble("        .gpu\n        dc.w\n", &Options::default());
    assert_eq!(out.errors(), 0, "{:#?}", out.diags);
    assert!(
        out.diags.iter().any(|d| d.msg.contains("no operands")),
        "expected empty-operand warning, got: {:#?}",
        out.diags
    );
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

// ── ELF object output (--elf-obj) ────────────────────────────────────────────

/// Minimal ELF32-BE reader for validating our own output structurally.
struct Elf<'a> {
    b: &'a [u8],
}
impl<'a> Elf<'a> {
    fn u16(&self, o: usize) -> u16 {
        u16::from_be_bytes(self.b[o..o + 2].try_into().unwrap())
    }
    fn u32(&self, o: usize) -> u32 {
        u32::from_be_bytes(self.b[o..o + 4].try_into().unwrap())
    }
    /// (type, flags, offset, size) of section header `i`.
    fn sh(&self, i: usize) -> (u32, u32, usize, usize) {
        let off = self.u32(32) as usize + i * 40;
        (
            self.u32(off + 4),
            self.u32(off + 8),
            self.u32(off + 16) as usize,
            self.u32(off + 20) as usize,
        )
    }
}

#[test]
fn elf_obj_is_wellformed_m68k_rel() {
    let src = "\t.68000\n\
        \t.text\n\
        \t.globl entry\n\
        \t.extern outside\n\
        entry:\n\
        \tmove.l counter,d0\n\
        \tbsr.w outside\n\
        \trts\n\
        \t.data\n\
        counter:\n\
        \tdc.l 42\n";
    let opts = Options { org: 0x4000, start_m68k: true, object_mode: true, relocatable: true, check_hazards: false, ..Default::default() };
    let out = assemble(src, &opts);
    assert_eq!(out.errors(), 0, "{:#?}", out.diags);
    let bytes = jas::elf::write(&out).expect("elf");
    let e = Elf { b: &bytes };
    assert_eq!(&bytes[..4], b"\x7fELF");
    assert_eq!(bytes[4], 1, "ELFCLASS32");
    assert_eq!(bytes[5], 2, "big-endian");
    assert_eq!(e.u16(16), 1, "ET_REL");
    assert_eq!(e.u16(18), 4, "EM_68K");
    assert_eq!(e.u16(48), 9, "section count");
    // .text: PROGBITS, alloc+exec, 12 bytes (move.l abs.l 6 + bsr.w 4 + rts 2)
    let (ty, fl, _, sz) = e.sh(1);
    assert_eq!((ty, fl & 6, sz), (1, 6, 12), ".text header");
    // .data: PROGBITS, alloc+write, the dc.l
    let (ty, fl, doff, sz) = e.sh(3);
    assert_eq!((ty, fl & 3, sz), (1, 3, 4), ".data header");
    assert_eq!(&bytes[doff..doff + 4], &[0, 0, 0, 42]);
    // .rela.text: two RELA entries (counter abs32 + outside pc16)
    let (ty, _, roff, rsz) = e.sh(2);
    assert_eq!((ty, rsz), (4, 24), ".rela.text");
    let r_info = |i: usize| e.u32(roff + i * 12 + 4);
    let types: Vec<u8> = (0..2).map(|i| (r_info(i) & 0xFF) as u8).collect();
    assert!(types.contains(&1), "R_68K_32 present: {types:?}");
    assert!(types.contains(&5), "R_68K_PC16 present: {types:?}");
}

#[test]
fn elf_obj_folds_equ_constants_not_relocs() {
    // An `equ` CONSTANT used as an absolute destination must encode its VALUE.
    // Object mode used to relocate every identifier in an absolute context —
    // including equates — emitting a reloc against a non-address symbol that
    // resolved to 0. Caught on Quake: `move.l #_vi_isr,USER0` with
    // `USER0 .equ $100` wrote the VI handler to vector $0, and the console
    // died in the exception catcher on the first vertical interrupt.
    let src = "\t.68000\n\
        \t.text\n\
        USER0 .equ $100\n\
        entry:\n\
        \tmove.l #entry,USER0\n";
    let opts = Options { org: 0x4000, start_m68k: true, object_mode: true, relocatable: true, check_hazards: false, ..Default::default() };
    let out = assemble(src, &opts);
    assert_eq!(out.errors(), 0, "{:#?}", out.diags);
    let bytes = jas::elf::write(&out).expect("elf");
    let e = Elf { b: &bytes };
    // move.l #imm32,(abs).l = op 2 + imm 4 (relocated `entry`) + abs.l 4
    let (_, _, toff, tsz) = e.sh(1);
    assert_eq!(tsz, 10, ".text size");
    assert_eq!(&bytes[toff + 6..toff + 10], &[0, 0, 1, 0], "USER0 folded to $100, not relocated");
    // exactly ONE reloc (the #entry immediate) — none for the equate
    let (ty, _, _, rsz) = e.sh(2);
    assert_eq!((ty, rsz), (4, 12), "one RELA entry only");
}

#[test]
fn elf_obj_rejects_jrisc_movei_reloc() {
    // A JRISC MOVEI of an extern has no ELF relocation type — must be a clear
    // error, not silent corruption.
    let src = "\t.gpu\n\t.extern target\n\tmovei #target,r1\n\tnop\n";
    let opts = Options { object_mode: true, relocatable: true, ..Default::default() };
    let out = assemble(src, &opts);
    assert_eq!(out.errors(), 0, "{:#?}", out.diags);
    let err = jas::elf::write(&out).unwrap_err();
    assert!(err.contains("MOVEI"), "got: {err}");
}

#[test]
fn elf_obj_rejects_interleaved_sections() {
    let src = "\t.68000\n\t.text\n\tnop\n\t.data\n\tdc.w 1\n\t.text\n\tnop\n";
    let opts = Options { org: 0x4000, start_m68k: true, object_mode: true, relocatable: true, check_hazards: false, ..Default::default() };
    let out = assemble(src, &opts);
    assert_eq!(out.errors(), 0, "{:#?}", out.diags);
    let err = jas::elf::write(&out).unwrap_err();
    assert!(err.contains("re-entered"), "got: {err}");
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
