//! Platform issue 0016: two GPU/DSP behaviours measured on silicon (platform
//! bench `bench/gpuhaz`, jobs 395-400) that jsim used to run in program order.
//!
//! 1. A MOVE-class write right behind an ALU-class write of the same register
//!    is dropped: the register keeps the ALU result.
//! 2. Every GPU read outside the GPU's own RAM and registers copies the upper
//!    half of the 64-bit bus into G_HIDATA, a byte lane at a time.
//!
//! `--fidelity silicon` models both; every fidelity counts them.

use jag_core::risc::Fidelity;
use jag_core::{mem, Bus, Risc, RiscKind};
use jas::{assemble, Options, Target};

const DATA: u32 = 0x18_0000;
const RES: u32 = 0x19_0000;

/// Assemble `src` (no hazard check: the cases are the hazards), run it on
/// the core to completion, and return the bus and the core.
fn run(src: &str, dsp: bool, fidelity: Fidelity) -> (Bus, Risc) {
    let (target, org) = if dsp { (Target::Dsp, mem::D_RAM) } else { (Target::Gpu, mem::G_RAM) };
    let out = assemble(src, &Options { target, org, check_hazards: false, ..Default::default() });
    assert_eq!(out.errors(), 0, "assembly errors: {:#?}", out.diags);
    let mut bus = Bus::new();
    // the bench's DATA pattern: the word at byte offset b is $8000 + b
    for o in (0..0x400u32).step_by(4) {
        bus.write32(DATA + o, ((0x8000 + o) << 16) | (0x8002 + o));
    }
    for (i, b) in out.bytes.iter().enumerate() {
        bus.write8(org + i as u32, *b);
    }
    let (pc, ctrl) = if dsp { (mem::D_PC, mem::D_CTRL) } else { (mem::G_PC, mem::G_CTRL) };
    bus.write32(pc, org);
    bus.write32(ctrl, mem::RISCGO);
    let mut core = Risc::new(if dsp { RiscKind::Dsp } else { RiscKind::Gpu });
    core.fidelity = fidelity;
    core.run(&mut bus, 20_000);
    (bus, core)
}

fn results(bus: &mut Bus, n: u32) -> Vec<u32> {
    (0..n).map(|i| bus.read32(RES + 4 * i)).collect()
}

/// The write-order cases: r8 = $88, r7 = $700, r6 = $6000 before each.
const WAW: &str = "
        .macro  W
        movei   #$88,r8
        movei   #$700,r7
        movei   #$6000,r6
        cmp     r0,r0           ; Z: ne falls through, eq is taken
        nop
        nop
        .endm
        .macro  OUT
        nop
        nop
        nop
        store   r8,(r21)
        addq    #4,r21
        .endm
        moveq   #0,r0
        movei   #$190000,r21
        W
        add     r7,r8           ; 0: straight line
        move    r6,r8
        OUT
        W
        shlq    #1,r8           ; 1: moveq
        moveq   #5,r8
        OUT
        W
        jr      ne,a2           ; 2: a fall-through delay slot
        add     r7,r8
        move    r6,r8
a2:     OUT
        W
        jr      eq,a3           ; 3: taken: the target is not adjacent
        add     r7,r8
        moveq   #9,r8
a3:     move    r6,r8
        OUT
        W
        add     r7,r8           ; 4: one instruction between
        nop
        move    r6,r8
        OUT
        W
        add     r7,r8           ; 5: MOVEI is safe
        movei   #$1234,r8
        OUT
        W
        movei   #$180380,r1     ; 6: the bug-13 guard, then the overwrite
        nop
        load    (r1),r8
        or      r8,r8
        moveq   #5,r8
        OUT
        W
        movei   #$180380,r1     ; 7: a MOVE as the guard
        nop
        load    (r1),r8
        move    r8,r8
        moveq   #5,r8
        OUT
        moveq   #0,r1
        movei   #CTRL,r2
        store   r1,(r2)
        nop
        nop
";

#[test]
fn a_move_right_behind_an_alu_write_is_dropped_on_silicon() {
    // program order, then what bench jobs 395-400 read on the GPU and the DSP
    let program = [0x6000, 5, 0x6000, 0x6000, 0x6000, 0x1234, 5, 5];
    let silicon = [0x788, 0x110, 0x788, 0x6000, 0x6000, 0x1234, 0x8380_8382, 5];
    for dsp in [false, true] {
        let src = format!(
            "        {}\nCTRL .equ ${:X}\n{WAW}",
            if dsp { ".dsp" } else { ".gpu" },
            if dsp { mem::D_CTRL } else { mem::G_CTRL }
        );
        for (fid, want) in [(Fidelity::Functional, &program), (Fidelity::Silicon, &silicon)] {
            let (mut bus, core) = run(&src, dsp, fid);
            assert!(!core.running, "the kernel stops itself");
            assert_eq!(results(&mut bus, 8), want.to_vec(), "dsp={dsp} {fid:?}");
            let t = &core.pipe.stats;
            assert_eq!(t.alu_move_waw, 4, "dsp={dsp} {fid:?}: cases 0, 1, 2 and 6");
        }
    }
}

/// G_HIDATA cases, each read back to RES. PRIME = LOADP of DATA+$380:
/// $83808382 on the upper half of the bus and in G_HIDATA.
const HI: &str = "
        .gpu
        .macro  PRIME
        loadp   (r24),r4
        or      r4,r4
        .endm
        .macro  MARK
        movei   #$EEEE0000,r2
        store   r2,(r22)
        nop
        .endm
        .macro  HI
        load    (r22),r2
        or      r2,r2
        store   r2,(r21)
        addq    #4,r21
        .endm
        movei   #$190000,r21
        movei   #$F02118,r22
        movei   #$180380,r24
        movei   #$180200,r11
        movei   #$180202,r12
        movei   #$180204,r14
        movei   #$180201,r18
        movei   #$180304,r28
        movei   #$F03F00,r13
        movei   #$180100,r10
        movei   #$51525354,r5
        movei   #$61616161,r6
        movei   #$180308,r9
        PRIME
        load    (r11),r3        ; 0: a long at A%8 = 0 drives all four lanes
        or      r3,r3
        HI
        PRIME
        load    (r14),r3        ; 1: at A%8 = 4 none: G_HIDATA keeps the lanes
        or      r3,r3
        HI
        PRIME
        loadw   (r12),r3        ; 2: a word: its two lanes
        or      r3,r3
        HI
        PRIME
        loadb   (r18),r3        ; 3: a byte: its lane
        or      r3,r3
        HI
        PRIME
        store   r5,(r28)        ; 4: a store at A%8 = 4 drives the lanes
        load    (r14),r3
        or      r3,r3
        HI
        MARK
        load    (r13),r3        ; 5: our own RAM does not latch
        or      r3,r3
        HI
        MARK
        store   r5,(r28)        ; 6: a store does not latch
        HI
        loadp   (r10),r1        ; 7: the issue's case
        load    (r11),r3
        or      r1,r1
        or      r3,r3
        HI
        PRIME
        movei   #$53535353,r2   ; 8: STOREP takes a load still in flight
        store   r2,(r22)
        load    (r11),r3
        storep  r6,(r9)
        or      r3,r3
        nop
        nop
        load    (r9),r3
        or      r3,r3
        store   r3,(r21)
        addq    #4,r21
        moveq   #0,r1
        movei   #$F02114,r2
        store   r1,(r2)
        nop
        nop
";

#[test]
fn gpu_reads_latch_g_hidata_a_byte_lane_at_a_time_on_silicon() {
    let (mut bus, core) = run(HI, false, Fidelity::Silicon);
    assert!(!core.running);
    assert_eq!(
        results(&mut bus, 9),
        vec![
            0x8200_8202, // bench job 397, C01
            0x8380_8382, // C02 (silicon: the undriven lanes may hold other masters' bytes)
            0x8380_8202, // C04
            0x8300_8382, // C08
            0x5152_5354, // C32
            0xEEEE_0000, // C19
            0xEEEE_0000, // C21
            0x8200_8202, // C24
            0x8200_8202, // P03
        ]
    );
    let t = &core.pipe.stats;
    assert_eq!(t.hidata_clobbers, 7, "every use after a latching read, MARK or LOADP first");
}

#[test]
fn other_fidelities_keep_g_hidata_but_count_the_use() {
    let (mut bus, core) = run(HI, false, Fidelity::Functional);
    assert_eq!(
        results(&mut bus, 9),
        vec![
            0x8380_8382, 0x8380_8382, 0x8380_8382, 0x8380_8382, 0x8380_8382,
            0xEEEE_0000, 0xEEEE_0000, 0x8100_8102, 0x5353_5353,
        ]
    );
    assert_eq!(core.pipe.stats.hidata_clobbers, 7);
}

#[test]
fn strict_stops_at_both() {
    let src = format!("        .gpu\nCTRL .equ ${:X}\n{WAW}", mem::G_CTRL);
    let (mut bus, mut core) = (Bus::new(), Risc::new(RiscKind::Gpu));
    let out = assemble(&src, &Options { check_hazards: false, ..Default::default() });
    for (i, b) in out.bytes.iter().enumerate() {
        bus.write8(mem::G_RAM + i as u32, *b);
    }
    bus.write32(mem::G_PC, mem::G_RAM);
    bus.write32(mem::G_CTRL, mem::RISCGO);
    core.strict = jag_core::Strict::parse("alu-move").unwrap();
    core.run(&mut bus, 20_000);
    let f = core.strict_fault.as_ref().expect("alu-move trips");
    assert_eq!((f.kind, f.addr), ("alu_move_waw", 8));

    let out = assemble(HI, &Options { check_hazards: false, ..Default::default() });
    let (mut bus, mut core) = (Bus::new(), Risc::new(RiscKind::Gpu));
    for (i, b) in out.bytes.iter().enumerate() {
        bus.write8(mem::G_RAM + i as u32, *b);
    }
    bus.write32(mem::G_PC, mem::G_RAM);
    bus.write32(mem::G_CTRL, mem::RISCGO);
    core.strict = jag_core::Strict::parse("hidata").unwrap();
    core.run(&mut bus, 20_000);
    let f = core.strict_fault.as_ref().expect("hidata trips");
    assert_eq!(f.kind, "hidata_clobbered");
}
