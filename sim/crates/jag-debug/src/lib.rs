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

const M68K_CC: [&str; 16] = [
    "t", "f", "hi", "ls", "cc", "cs", "ne", "eq", "vc", "vs", "pl", "mi", "ge", "lt", "gt", "le",
];

fn m68k_word(bus: &Bus, addr: u32) -> u16 {
    let mut b = [0u8; 2];
    bus.peek(addr, &mut b);
    u16::from_be_bytes(b)
}

/// Decode an effective address. `base` points at the opcode word; `ext` counts
/// extension words consumed so far and is advanced as this EA reads more.
/// `size` (1/2/4 bytes) sets the width of `#imm`.
fn m68k_ea(bus: &Bus, base: u32, mode: u16, reg: u16, size: u32, ext: &mut u32) -> String {
    let mut next = || {
        let w = m68k_word(bus, base + 2 + *ext * 2);
        *ext += 1;
        w
    };
    let idx = |w: u16| {
        let rn = (w >> 12) & 0x7;
        let da = if w & 0x8000 != 0 { 'a' } else { 'd' };
        let sz = if w & 0x0800 != 0 { "l" } else { "w" };
        (rn, da, sz)
    };
    match mode {
        0 => format!("d{reg}"),
        1 => format!("a{reg}"),
        2 => format!("(a{reg})"),
        3 => format!("(a{reg})+"),
        4 => format!("-(a{reg})"),
        5 => format!("({},a{reg})", next() as i16),
        6 => {
            let w = next();
            let (rn, da, sz) = idx(w);
            format!("({},a{reg},{da}{rn}.{sz})", w as i8)
        }
        7 => match reg {
            0 => format!("(${:04X}).w", next()),
            1 => {
                let hi = next() as u32;
                let lo = next() as u32;
                format!("(${:08X}).l", (hi << 16) | lo)
            }
            2 => format!("({},pc)", next() as i16),
            3 => {
                let w = next();
                let (rn, da, sz) = idx(w);
                format!("({},pc,{da}{rn}.{sz})", w as i8)
            }
            4 => {
                if size == 4 {
                    let hi = next() as u32;
                    let lo = next() as u32;
                    format!("#${:08X}", (hi << 16) | lo)
                } else {
                    let w = next();
                    if size == 1 {
                        format!("#${:02X}", w & 0xFF)
                    } else {
                        format!("#${w:04X}")
                    }
                }
            }
            _ => "?".to_string(),
        },
        _ => "?".to_string(),
    }
}

/// Disassemble one 68000 instruction at `addr`. Returns the rendered text and
/// the instruction length in **words** (opcode + extension words), so a listing
/// stays aligned. Covers the instruction set jsim's interpreter runs; genuinely
/// unknown words fall back to `dc.w` (1 word).
pub fn disasm_68k_len(bus: &Bus, addr: u32) -> (String, u32) {
    let op = m68k_word(bus, addr);
    let mode = (op >> 3) & 7;
    let reg = op & 7;
    let sz_bits = (op >> 6) & 3;
    let sz_bytes = match sz_bits {
        0 => 1,
        1 => 2,
        _ => 4,
    };
    let sz_suf = |n: u32| match n {
        1 => "b",
        2 => "w",
        _ => "l",
    };
    let mut ext = 0u32;
    // Fixed single-word opcodes.
    match op {
        0x4E71 => return ("nop".into(), 1),
        0x4E75 => return ("rts".into(), 1),
        0x4E73 => return ("rte".into(), 1),
        0x4E77 => return ("rtr".into(), 1),
        0x4E76 => return ("trapv".into(), 1),
        0x4E70 => return ("reset".into(), 1),
        0x4E72 => {
            let w = m68k_word(bus, addr + 2);
            return (format!("stop #${w:04X}"), 2);
        }
        0x4AFC => return ("illegal".into(), 1),
        _ => {}
    }
    let text = match op >> 12 {
        // MOVE.b/l/w (and MOVEA when dest mode = 1).
        0x1 | 0x2 | 0x3 => {
            let size = match op >> 12 {
                0x1 => 1,
                0x2 => 4,
                _ => 2,
            };
            let dmode = (op >> 6) & 7;
            let dreg = (op >> 9) & 7;
            let src = m68k_ea(bus, addr, mode, reg, size, &mut ext);
            let dst = m68k_ea(bus, addr, dmode, dreg, size, &mut ext);
            let m = if dmode == 1 { "movea" } else { "move" };
            format!("{m}.{} {src},{dst}", sz_suf(size))
        }
        0x7 if op & 0x0100 == 0 => format!("moveq #{},d{}", (op & 0xFF) as i8, (op >> 9) & 7),
        0x6 => {
            let cc = (op >> 8) & 0xF;
            let m = if cc == 1 { "bsr".to_string() } else { format!("b{}", M68K_CC[cc as usize]) };
            let disp8 = op & 0xFF;
            if disp8 == 0 {
                let d = m68k_word(bus, addr + 2) as i16;
                ext = 1;
                format!("{m}.w ${:06X}", addr.wrapping_add(2).wrapping_add(d as u32))
            } else {
                format!("{m}.s ${:06X}", addr.wrapping_add(2).wrapping_add(disp8 as i8 as u32))
            }
        }
        // ADDQ / SUBQ / Scc / DBcc.
        0x5 => {
            if sz_bits == 3 {
                let cc = (op >> 8) & 0xF;
                if mode == 1 {
                    let d = m68k_word(bus, addr + 2) as i16;
                    ext = 1;
                    format!("db{} d{reg},${:06X}", M68K_CC[cc as usize], addr.wrapping_add(2).wrapping_add(d as u32))
                } else {
                    let ea = m68k_ea(bus, addr, mode, reg, 1, &mut ext);
                    format!("s{} {ea}", M68K_CC[cc as usize])
                }
            } else {
                let data = { let d = (op >> 9) & 7; if d == 0 { 8 } else { d } };
                let ea = m68k_ea(bus, addr, mode, reg, sz_bytes, &mut ext);
                let m = if op & 0x0100 != 0 { "subq" } else { "addq" };
                format!("{m}.{} #{data},{ea}", sz_suf(sz_bytes))
            }
        }
        // Immediates + static bit ops.
        0x0 => {
            let opc = (op >> 9) & 7;
            if op & 0xFF00 == 0x0800 {
                // static BTST/BCHG/BCLR/BSET #n,ea
                let bit = m68k_word(bus, addr + 2) & 0xFF;
                ext = 1;
                let m = ["btst", "bchg", "bclr", "bset"][((op >> 6) & 3) as usize];
                let ea = m68k_ea(bus, addr, mode, reg, 1, &mut ext);
                format!("{m} #{bit},{ea}")
            } else if op & 0x0100 != 0 {
                // dynamic bit ops btst/... Dn,ea
                let dn = (op >> 9) & 7;
                let m = ["btst", "bchg", "bclr", "bset"][((op >> 6) & 3) as usize];
                let ea = m68k_ea(bus, addr, mode, reg, 1, &mut ext);
                format!("{m} d{dn},{ea}")
            } else {
                let m = ["ori", "andi", "subi", "addi", "?", "eori", "cmpi", "?"][opc as usize];
                // immediate first (its width = operation size)
                let imm = m68k_ea(bus, addr, 7, 4, sz_bytes, &mut ext);
                let ea = m68k_ea(bus, addr, mode, reg, sz_bytes, &mut ext);
                format!("{m}.{} {imm},{ea}", sz_suf(sz_bytes))
            }
        }
        // Misc (group 4).
        0x4 => disasm_group4(bus, addr, op, mode, reg, sz_bits, sz_bytes, &mut ext),
        // OR / DIV / SBCD.
        0x8 => {
            let dn = (op >> 9) & 7;
            if sz_bits == 3 {
                let m = if op & 0x0100 != 0 { "divs" } else { "divu" };
                let ea = m68k_ea(bus, addr, mode, reg, 2, &mut ext);
                format!("{m}.w {ea},d{dn}")
            } else {
                let ea = m68k_ea(bus, addr, mode, reg, sz_bytes, &mut ext);
                if op & 0x0100 != 0 {
                    format!("or.{} d{dn},{ea}", sz_suf(sz_bytes))
                } else {
                    format!("or.{} {ea},d{dn}", sz_suf(sz_bytes))
                }
            }
        }
        // SUB / SUBX / SUBA.
        0x9 | 0xD => {
            let m = if op >> 12 == 9 { "sub" } else { "add" };
            let dn = (op >> 9) & 7;
            if sz_bits == 3 {
                let asz = if op & 0x0100 != 0 { 4 } else { 2 };
                let ea = m68k_ea(bus, addr, mode, reg, asz, &mut ext);
                format!("{m}a.{} {ea},a{dn}", sz_suf(asz))
            } else {
                let ea = m68k_ea(bus, addr, mode, reg, sz_bytes, &mut ext);
                if op & 0x0100 != 0 {
                    format!("{m}.{} d{dn},{ea}", sz_suf(sz_bytes))
                } else {
                    format!("{m}.{} {ea},d{dn}", sz_suf(sz_bytes))
                }
            }
        }
        // CMP / CMPA / EOR / CMPM.
        0xB => {
            let dn = (op >> 9) & 7;
            if sz_bits == 3 {
                let asz = if op & 0x0100 != 0 { 4 } else { 2 };
                let ea = m68k_ea(bus, addr, mode, reg, asz, &mut ext);
                format!("cmpa.{} {ea},a{dn}", sz_suf(asz))
            } else if op & 0x0100 != 0 {
                let ea = m68k_ea(bus, addr, mode, reg, sz_bytes, &mut ext);
                format!("eor.{} d{dn},{ea}", sz_suf(sz_bytes))
            } else {
                let ea = m68k_ea(bus, addr, mode, reg, sz_bytes, &mut ext);
                format!("cmp.{} {ea},d{dn}", sz_suf(sz_bytes))
            }
        }
        // AND / MUL / EXG / ABCD.
        0xC => {
            let dn = (op >> 9) & 7;
            if sz_bits == 3 {
                let m = if op & 0x0100 != 0 { "muls" } else { "mulu" };
                let ea = m68k_ea(bus, addr, mode, reg, 2, &mut ext);
                format!("{m}.w {ea},d{dn}")
            } else {
                let ea = m68k_ea(bus, addr, mode, reg, sz_bytes, &mut ext);
                if op & 0x0100 != 0 {
                    format!("and.{} d{dn},{ea}", sz_suf(sz_bytes))
                } else {
                    format!("and.{} {ea},d{dn}", sz_suf(sz_bytes))
                }
            }
        }
        // Shifts / rotates.
        0xE => {
            if sz_bits == 3 {
                let ea = m68k_ea(bus, addr, mode, reg, 2, &mut ext);
                let dir = if op & 0x0100 != 0 { "l" } else { "r" };
                let ty = ["as", "ls", "rox", "ro"][((op >> 9) & 3) as usize];
                format!("{ty}{dir}.w {ea}")
            } else {
                let dir = if op & 0x0100 != 0 { "l" } else { "r" };
                let ty = ["as", "ls", "rox", "ro"][((op >> 3) & 3) as usize];
                let cnt = (op >> 9) & 7;
                let rd = op & 7;
                if op & 0x0020 != 0 {
                    format!("{ty}{dir}.{} d{cnt},d{rd}", sz_suf(sz_bytes))
                } else {
                    let c = if cnt == 0 { 8 } else { cnt };
                    format!("{ty}{dir}.{} #{c},d{rd}", sz_suf(sz_bytes))
                }
            }
        }
        _ => format!("dc.w ${op:04X}"),
    };
    (text, 1 + ext)
}

fn disasm_group4(
    bus: &Bus,
    addr: u32,
    op: u16,
    mode: u16,
    reg: u16,
    sz_bits: u16,
    sz_bytes: u32,
    ext: &mut u32,
) -> String {
    let sz_suf = |n: u32| match n {
        1 => "b",
        2 => "w",
        _ => "l",
    };
    // 0x4E40-0x4E4F TRAP, 0x4E50/58 LINK/UNLK, 0x4E60/68 MOVE USP, JMP/JSR.
    if op & 0xFFF0 == 0x4E40 {
        return format!("trap #{}", op & 0xF);
    }
    if op & 0xFFF8 == 0x4E50 {
        let d = m68k_word(bus, addr + 2) as i16;
        *ext = 1;
        return format!("link a{},#{d}", op & 7);
    }
    if op & 0xFFF8 == 0x4E58 {
        return format!("unlk a{}", op & 7);
    }
    if op & 0xFFC0 == 0x4EC0 {
        let ea = m68k_ea(bus, addr, mode, reg, 4, ext);
        return format!("jmp {ea}");
    }
    if op & 0xFFC0 == 0x4E80 {
        let ea = m68k_ea(bus, addr, mode, reg, 4, ext);
        return format!("jsr {ea}");
    }
    if op & 0xFFC0 == 0x41C0 {
        // handled in main table? LEA is group 4 with bit pattern 0100 ddd 111.
    }
    // LEA An: 0100 aaa1 11mmm rrr
    if op & 0xF1C0 == 0x41C0 {
        let an = (op >> 9) & 7;
        let ea = m68k_ea(bus, addr, mode, reg, 4, ext);
        return format!("lea {ea},a{an}");
    }
    // CHK: 0100 ddd1 10mmm rrr
    if op & 0xF1C0 == 0x4180 {
        let dn = (op >> 9) & 7;
        let ea = m68k_ea(bus, addr, mode, reg, 2, ext);
        return format!("chk.w {ea},d{dn}");
    }
    // MOVEM: 0100 1D00 1Ssmmm rrr  (D=dir bit10, S size bit6)
    if op & 0xFB80 == 0x4880 {
        let list = m68k_word(bus, addr + 2);
        *ext = 1;
        let size = if op & 0x0040 != 0 { 4 } else { 2 };
        let to_mem = op & 0x0400 == 0;
        let ea = m68k_ea(bus, addr, mode, reg, size, ext);
        let regs = movem_list(list, mode == 4);
        if to_mem {
            return format!("movem.{} {regs},{ea}", sz_suf(size));
        } else {
            return format!("movem.{} {ea},{regs}", sz_suf(size));
        }
    }
    // Single-EA group-4 ops by bits 11-8.
    let ea_op = |m: &str, size: u32, ext: &mut u32| {
        let ea = m68k_ea(bus, addr, mode, reg, size, ext);
        format!("{m}.{} {ea}", sz_suf(size))
    };
    match (op >> 8) & 0xF {
        0x0 if sz_bits != 3 => return ea_op("negx", sz_bytes, ext),
        0x2 if sz_bits != 3 => return ea_op("clr", sz_bytes, ext),
        0x4 if sz_bits != 3 => return ea_op("neg", sz_bytes, ext),
        0x6 if sz_bits != 3 => return ea_op("not", sz_bytes, ext),
        0xA if sz_bits == 3 => {
            let ea = m68k_ea(bus, addr, mode, reg, 1, ext);
            return format!("tas {ea}");
        }
        0xA if sz_bits != 3 => return ea_op("tst", sz_bytes, ext),
        0x8 => {
            // NBCD / PEA / SWAP / EXT
            if op & 0xFFF8 == 0x4840 {
                return format!("swap d{}", op & 7);
            }
            if op & 0xFFB8 == 0x4880 {
                let m = if op & 0x0040 != 0 { "ext.l" } else { "ext.w" };
                return format!("{m} d{}", op & 7);
            }
            if op & 0xFFC0 == 0x4840 {
                let ea = m68k_ea(bus, addr, mode, reg, 4, ext);
                return format!("pea {ea}");
            }
        }
        0xE => {
            // 0x4E xx handled above; MOVE to/from SR/CCR are 0x40C0/0x44C0/0x46C0.
        }
        _ => {}
    }
    if op & 0xFFC0 == 0x40C0 {
        let ea = m68k_ea(bus, addr, mode, reg, 2, ext);
        return format!("move sr,{ea}");
    }
    if op & 0xFFC0 == 0x44C0 {
        let ea = m68k_ea(bus, addr, mode, reg, 2, ext);
        return format!("move {ea},ccr");
    }
    if op & 0xFFC0 == 0x46C0 {
        let ea = m68k_ea(bus, addr, mode, reg, 2, ext);
        return format!("move {ea},sr");
    }
    format!("dc.w ${op:04X}")
}

fn movem_list(mask: u16, predec: bool) -> String {
    // In -(An) mode the mask is reversed (a7..d0); otherwise d0..a7.
    let names: Vec<String> = (0..16)
        .filter(|i| {
            let bit = if predec { 15 - i } else { *i };
            mask & (1 << bit) != 0
        })
        .map(|i| if i < 8 { format!("d{i}") } else { format!("a{}", i - 8) })
        .collect();
    if names.is_empty() {
        "-".to_string()
    } else {
        names.join("/")
    }
}

/// Disassemble one 68000 instruction (word length inferred). Kept for callers
/// that only need the text; steps a single word's worth of `bytes`.
pub fn disasm_68k(bus: &Bus, addr: u32) -> Insn {
    let (text, len) = disasm_68k_len(bus, addr);
    let mut bytes = Vec::with_capacity(len as usize * 2);
    for i in 0..len {
        bytes.extend_from_slice(&m68k_word(bus, addr.wrapping_add(i * 2)).to_be_bytes());
    }
    Insn { addr, bytes, text }
}

/// Disassemble `count` instructions starting at `addr`, following each
/// instruction's real length so the listing stays aligned.
pub fn disasm_range(bus: &Bus, addr: u32, count: usize) -> Vec<Insn> {
    let mut out = Vec::with_capacity(count);
    let mut pc = addr;
    for _ in 0..count {
        let (text, len) = disasm_68k_len(bus, pc);
        let mut bytes = Vec::with_capacity(len as usize * 2);
        for i in 0..len {
            bytes.extend_from_slice(&m68k_word(bus, pc.wrapping_add(i * 2)).to_be_bytes());
        }
        out.push(Insn { addr: pc, bytes, text });
        pc = pc.wrapping_add(len * 2);
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
