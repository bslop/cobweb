//! Disassembly + higher-level debug helpers layered over `jag-core`.
//!
//! The 68000 and Jaguar-RISC disassemblers feed the debug API (so `disasm` over
//! the control protocol shows real mnemonics). v1 provides a compact 68k
//! disassembler skeleton; full coverage and the RISC disassembler build on it.

use jag_core::Bus;

/// One disassembled instruction.
#[derive(Debug, Clone)]
pub struct Insn {
    pub addr: u32,
    pub bytes: Vec<u8>,
    pub text: String,
}

/// Minimal 68000 disassembler: decodes the handful of opcodes that dominate
/// boot/ISR code so traces are readable. Unknown opcodes render as `dc.w`.
/// (Full table-driven coverage is a follow-up.)
pub fn disasm_68k(bus: &Bus, addr: u32) -> Insn {
    let mut b = [0u8; 2];
    bus.peek(addr, &mut b);
    let op = u16::from_be_bytes(b);
    let text = match op {
        0x4E71 => "nop".to_string(),
        0x4E75 => "rts".to_string(),
        0x4E73 => "rte".to_string(),
        0x4E77 => "rtr".to_string(),
        0x4E76 => "trapv".to_string(),
        0x4E70 => "reset".to_string(),
        0x4AFC => "illegal".to_string(),
        _ if op & 0xF000 == 0x6000 => {
            let cc = (op >> 8) & 0xF;
            let m = ["bra", "bsr", "bhi", "bls", "bcc", "bcs", "bne", "beq",
                     "bvc", "bvs", "bpl", "bmi", "bge", "blt", "bgt", "ble"];
            format!("{} .{:+}", m[cc as usize], (op & 0xFF) as i8)
        }
        _ if op & 0xF000 == 0x7000 => {
            format!("moveq #{}, d{}", (op & 0xFF) as i8, (op >> 9) & 7)
        }
        _ => format!("dc.w ${:04X}", op),
    };
    Insn { addr, bytes: b.to_vec(), text }
}

/// Disassemble `count` instructions starting at `addr` (word-stepping; this is
/// approximate until extension-word lengths are modeled per opcode).
pub fn disasm_range(bus: &Bus, addr: u32, count: usize) -> Vec<Insn> {
    let mut out = Vec::with_capacity(count);
    let mut pc = addr;
    for _ in 0..count {
        let insn = disasm_68k(bus, pc);
        pc = pc.wrapping_add(2);
        out.push(insn);
    }
    out
}

// ── JRISC (GPU / DSP) disassembler ──────────────────────────────────────────
// The inverse of jas's encoder; opcode meanings match `jag_core::risc::isa`
// exactly. Instruction word = `opcode[15:10] reg1[9:5] reg2[4:0]`; MOVEI is
// followed by a 32-bit immediate (low word, then high word).

/// JRISC condition-code name (jump/jr). Empty string = "always".
fn jrisc_cc(cc: u16) -> &'static str {
    match cc {
        0x00 => "",
        0x01 => "ne",
        0x02 => "eq",
        0x04 => "cc",
        0x05 => "hi",
        0x08 => "cs",
        0x14 => "pl",
        0x18 => "mi",
        0x1F => "f",
        _ => "?",
    }
}

/// Disassemble one JRISC instruction. `w0` is the opcode word; `w1`/`w2` are the
/// two words following it (only read for MOVEI). `pc` is the address of `w0`,
/// used to resolve `jr` targets. `is_dsp` selects the few DSP/GPU-divergent
/// opcodes (32/33/42/48/62/63). Returns `(text, length_in_words)` — 1 for most,
/// 3 for MOVEI.
pub fn disasm_jrisc(w0: u16, w1: u16, w2: u16, pc: u32, is_dsp: bool) -> (String, u32) {
    let op = (w0 >> 10) & 0x3F;
    let r1 = (w0 >> 5) & 0x1F;
    let r2 = w0 & 0x1F;
    // add/subq-family quick value: field 1..31 = itself, 0 = 32.
    let q = if r1 == 0 { 32 } else { r1 };
    let rr = |m: &str| format!("{m} r{r1},r{r2}");
    let one = |m: &str| format!("{m} r{r2}");
    let quick = |m: &str, n: i32| format!("{m} #{n},r{r2}");
    let text = match op {
        0 => rr("add"),
        1 => rr("addc"),
        2 => quick("addq", q as i32),
        3 => quick("addqt", q as i32),
        4 => rr("sub"),
        5 => rr("subc"),
        6 => quick("subq", q as i32),
        7 => quick("subqt", q as i32),
        8 => one("neg"),
        9 => rr("and"),
        10 => rr("or"),
        11 => rr("xor"),
        12 => one("not"),
        13 => quick("btst", r1 as i32),
        14 => quick("bset", r1 as i32),
        15 => quick("bclr", r1 as i32),
        16 => rr("mult"),
        17 => rr("imult"),
        18 => rr("imultn"),
        19 => one("resmac"),
        20 => rr("imacn"),
        21 => rr("div"),
        22 => one("abs"),
        23 => rr("sh"),
        24 => quick("shlq", (32 - r1) as i32 & 0x1F),
        25 => quick("shrq", q as i32),
        26 => rr("sha"),
        27 => quick("sharq", q as i32),
        28 => rr("ror"),
        29 => quick("rorq", q as i32),
        30 => rr("cmp"),
        31 => quick("cmpq", ((r1 as i32) << 27) >> 27), // sign-extend 5-bit
        32 if is_dsp => quick("subqmod", q as i32),
        32 => one("sat8"),
        33 if is_dsp => one("sat16s"),
        33 => one("sat16"),
        34 => rr("move"),
        35 => quick("moveq", r1 as i32),
        36 => rr("moveta"),
        37 => rr("movefa"),
        38 => {
            let imm = (w1 as u32) | ((w2 as u32) << 16);
            return (format!("movei #${imm:08X},r{r2}"), 3);
        }
        39 => format!("loadb (r{r1}),r{r2}"),
        40 => format!("loadw (r{r1}),r{r2}"),
        41 => format!("load (r{r1}),r{r2}"),
        42 if is_dsp => one("sat32s"),
        42 => format!("loadp (r{r1}),r{r2}"),
        43 => format!("load (r14+{q}),r{r2}"),
        44 => format!("load (r15+{q}),r{r2}"),
        45 => format!("storeb r{r2},(r{r1})"),
        46 => format!("storew r{r2},(r{r1})"),
        47 => format!("store r{r2},(r{r1})"),
        48 if is_dsp => one("mirror"),
        48 => format!("storep r{r2},(r{r1})"),
        49 => format!("store r{r1},(r14+{})", if r2 == 0 { 32 } else { r2 }),
        50 => format!("store r{r1},(r15+{})", if r2 == 0 { 32 } else { r2 }),
        51 => format!("move PC,r{r2}"),
        52 => {
            let cc = jrisc_cc(r2);
            if cc.is_empty() {
                format!("jump (r{r1})")
            } else {
                format!("jump {cc},(r{r1})")
            }
        }
        53 => {
            let words = ((r1 as i32) << 27) >> 27; // sign-extend 5-bit
            let target = (pc as i64 + 2 + words as i64 * 2) as u32;
            let cc = jrisc_cc(r2);
            if cc.is_empty() {
                format!("jr ${target:06X}")
            } else {
                format!("jr {cc},${target:06X}")
            }
        }
        54 => rr("mmult"),
        55 => rr("mtoi"),
        56 => rr("normi"),
        57 => "nop".to_string(),
        58 => format!("load (r14+r{r1}),r{r2}"),
        59 => format!("load (r15+r{r1}),r{r2}"),
        60 => format!("store r{r1},(r14+r{r2})"),
        61 => format!("store r{r1},(r15+r{r2})"),
        62 if !is_dsp => one("sat24"),
        63 if is_dsp => quick("addqmod", q as i32),
        63 if r1 == 0 => one("pack"),
        63 => one("unpack"),
        _ => format!("dc.w ${w0:04X}"),
    };
    (text, 1)
}

/// Disassemble `count` JRISC instructions from `addr`, following MOVEI's 3-word
/// length. `is_dsp` selects the DSP opcode variants.
pub fn disasm_jrisc_range(bus: &Bus, addr: u32, count: usize, is_dsp: bool) -> Vec<Insn> {
    let word = |a: u32| -> u16 {
        let mut b = [0u8; 2];
        bus.peek(a, &mut b);
        u16::from_be_bytes(b)
    };
    let mut out = Vec::with_capacity(count);
    let mut pc = addr;
    for _ in 0..count {
        let (w0, w1, w2) = (word(pc), word(pc.wrapping_add(2)), word(pc.wrapping_add(4)));
        let (text, len) = disasm_jrisc(w0, w1, w2, pc, is_dsp);
        let mut bytes = Vec::with_capacity(len as usize * 2);
        for i in 0..len {
            bytes.extend_from_slice(&word(pc.wrapping_add(i * 2)).to_be_bytes());
        }
        out.push(Insn { addr: pc, bytes, text });
        pc = pc.wrapping_add(len * 2);
    }
    out
}
