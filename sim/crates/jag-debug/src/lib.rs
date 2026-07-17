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
