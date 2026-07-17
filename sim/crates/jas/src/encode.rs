//! JRISC instruction encoding. One 16-bit word `opcode[15:10] reg1[9:5]
//! reg2[4:0]`, plus MOVEI's two immediate half-words. Opcode numbers and the
//! immediate-field quirks match `jag-core`'s executor exactly (the integration
//! tests enforce it by running assembled code in jsim).

use crate::{Assembler, EncodeErr, Target};

type R = Result<(u8, Vec<u16>), EncodeErr>;

fn word(op: u8, r1: u16, r2: u16) -> (u8, Vec<u16>) {
    (op, vec![((op as u16) << 10) | ((r1 & 0x1F) << 5) | (r2 & 0x1F)])
}

fn parse_reg(s: &str) -> Option<u16> {
    let s = s.trim();
    let rest = s.strip_prefix(['r', 'R'])?;
    let n: u16 = rest.parse().ok()?;
    (n < 32).then_some(n)
}

fn parse_cc(s: &str) -> Option<u16> {
    Some(match s.trim().to_ascii_lowercase().as_str() {
        "t" | "" | "always" => 0x00,
        "ne" | "nz" => 0x01,
        "eq" | "z" => 0x02,
        "cc" | "hs" | "nc" => 0x04,
        "hi" => 0x05,
        "cs" | "lo" | "c" => 0x08,
        "pl" => 0x14,
        "mi" => 0x18,
        "f" | "never" | "nvr" => 0x1F,
        _ => return None,
    })
}

fn msg(m: impl Into<String>) -> EncodeErr {
    EncodeErr::Message(m.into())
}
fn fix(m: impl Into<String>, f: impl Into<String>) -> EncodeErr {
    EncodeErr::Fix(m.into(), f.into())
}

fn args2(s: &str) -> Result<(String, String), EncodeErr> {
    let parts = crate::split_args(s);
    if parts.len() != 2 {
        return Err(msg(format!("expected 2 operands, found {}", parts.len())));
    }
    Ok((parts[0].clone(), parts[1].clone()))
}

fn reg2(s: &str) -> Result<(u16, u16), EncodeErr> {
    let (a, b) = args2(s)?;
    let r1 = parse_reg(&a).ok_or_else(|| msg(format!("expected register, found `{a}`")))?;
    let r2 = parse_reg(&b).ok_or_else(|| msg(format!("expected register, found `{b}`")))?;
    Ok((r1, r2))
}

/// `#expr` immediate.
fn imm(s: &str, asm: &Assembler) -> Result<u32, EncodeErr> {
    let s = s.trim();
    let e = s.strip_prefix('#').unwrap_or(s);
    asm.eval_pub(e).map_err(msg)
}

/// quick immediate 1..32 (32 encodes as field 0).
fn quick_1_32(n: u32) -> Result<u16, EncodeErr> {
    if n == 0 || n > 32 {
        return Err(fix(
            format!("quick immediate {n} out of range 1..32"),
            "split into two ops, or use a MOVEI + register op",
        ));
    }
    Ok((n & 0x1F) as u16) // 32 -> 0
}

fn dreg(s: &str) -> Result<u16, EncodeErr> {
    parse_reg(s).ok_or_else(|| msg(format!("expected register, found `{s}`")))
}

/// Parse a load/store paren operand: `(rN)`, `(r14+n)`, `(r15+n)`, `(r14+rN)`,
/// `(r15+rN)`. Returns a classified addressing mode.
enum Addr {
    Reg(u16),
    IdxImm(u16, u32), // base reg (14/15), immediate*—index
    IdxReg(u16, u16), // base reg (14/15), index reg
}

fn parse_addr(s: &str, asm: &Assembler) -> Result<Addr, EncodeErr> {
    let t = s.trim();
    let inner = t
        .strip_prefix('(')
        .and_then(|x| x.strip_suffix(')'))
        .ok_or_else(|| msg(format!("expected `(...)` address, found `{s}`")))?
        .trim();
    if let Some(plus) = inner.find('+') {
        let base = inner[..plus].trim();
        let idx = inner[plus + 1..].trim();
        let base_r = parse_reg(base)
            .ok_or_else(|| msg(format!("indexed base must be r14 or r15, found `{base}`")))?;
        if base_r != 14 && base_r != 15 {
            return Err(msg("indexed addressing only supports r14 or r15 as base"));
        }
        if let Some(ir) = parse_reg(idx) {
            Ok(Addr::IdxReg(base_r, ir))
        } else {
            let v = asm.eval_pub(idx).map_err(msg)?;
            Ok(Addr::IdxImm(base_r, v))
        }
    } else {
        let r = parse_reg(inner)
            .ok_or_else(|| msg(format!("expected register in `(...)`, found `{inner}`")))?;
        Ok(Addr::Reg(r))
    }
}

/// Encode one instruction. `mnem` is as-written; we lowercase for lookup.
pub(crate) fn encode(mnem: &str, args: &str, asm: &Assembler) -> R {
    let m = mnem.to_ascii_lowercase();
    let dsp = asm.cur_target() == Target::Dsp;

    // Two-register ALU ops.
    let rr = |op: u8| -> R {
        let (r1, r2) = reg2(args)?;
        Ok(word(op, r1, r2))
    };
    // quick #n, rD ops with a custom field encoder.
    let q = |op: u8, field: fn(u32) -> Result<u16, EncodeErr>| -> R {
        let (a, b) = args2(args)?;
        let n = imm(&a, asm)?;
        let r2 = dreg(&b)?;
        Ok(word(op, field(n)?, r2))
    };
    // single-register (reg1 = 0).
    let one = |op: u8| -> R {
        let r2 = dreg(args.trim())?;
        Ok(word(op, 0, r2))
    };

    match m.as_str() {
        "add" => rr(0),
        "addc" => rr(1),
        "addq" => q(2, quick_1_32),
        "addqt" => q(3, quick_1_32),
        "sub" => rr(4),
        "subc" => rr(5),
        "subq" => q(6, quick_1_32),
        "subqt" => q(7, quick_1_32),
        "neg" => one(8),
        "and" => rr(9),
        "or" => rr(10),
        "xor" => rr(11),
        "not" => one(12),
        "btst" => q(13, |n| Ok((n & 0x1F) as u16)),
        "bset" => q(14, |n| Ok((n & 0x1F) as u16)),
        "bclr" => q(15, |n| Ok((n & 0x1F) as u16)),
        "mult" => rr(16),
        "imult" => rr(17),
        "imultn" => rr(18),
        "resmac" => one(19),
        "imacn" => rr(20),
        "div" => rr(21),
        "abs" => one(22),
        "sh" => rr(23),
        "shlq" => q(24, |n| {
            if n == 0 || n > 32 {
                return Err(msg("shlq count out of range 1..32"));
            }
            Ok(((32 - n) & 0x1F) as u16)
        }),
        "shrq" => q(25, quick_1_32),
        "sha" => rr(26),
        "sharq" => q(27, quick_1_32),
        "ror" => rr(28),
        "rorq" => q(29, quick_1_32),
        "cmp" => rr(30),
        "cmpq" => q(31, |n| {
            let v = n as i32;
            if !(-16..=15).contains(&v) {
                return Err(msg("cmpq immediate out of range -16..15"));
            }
            Ok((v as u16) & 0x1F)
        }),
        // shared 32/33/42/48/62/63 opcodes split by target
        "sat8" if !dsp => one(32),
        "subqmod" if dsp => q(32, quick_1_32),
        "sat16" if !dsp => one(33),
        "sat16s" if dsp => one(33),
        "move" => encode_move(args, asm),
        "moveq" => q(35, |n| {
            if n > 31 {
                return Err(msg("moveq immediate out of range 0..31 (use movei)"));
            }
            Ok(n as u16)
        }),
        "moveta" => rr(36),
        "movefa" => rr(37),
        "movei" => {
            let (a, b) = args2(args)?;
            let v = imm(&a, asm)?;
            let r2 = dreg(&b)?;
            let w0 = (38u16 << 10) | (r2 & 0x1F);
            Ok((38, vec![w0, (v & 0xFFFF) as u16, (v >> 16) as u16]))
        }
        "loadb" => encode_load(39, args, asm),
        "loadw" => encode_load(40, args, asm),
        "load" => encode_load(41, args, asm),
        "loadp" if !dsp => encode_load(42, args, asm),
        "sat32s" if dsp => one(42),
        "storeb" => encode_store(45, args, asm),
        "storew" => encode_store(46, args, asm),
        "store" => encode_store(47, args, asm),
        "storep" if !dsp => encode_store(48, args, asm),
        "mirror" if dsp => one(48),
        "mtoi" => rr(55),
        "normi" => rr(56),
        "nop" => Ok(word(57, 0, 0)),
        "sat24" if !dsp => one(62),
        "pack" if !dsp => Ok(word(63, 0, dreg(args.trim())?)),
        "unpack" if !dsp => Ok(word(63, 1, dreg(args.trim())?)),
        "addqmod" if dsp => q(63, quick_1_32),
        "mmult" => rr(54),
        "jump" => encode_jump(args, asm),
        "jr" => encode_jr(args, asm),
        _ => Err(EncodeErr::Unknown),
    }
}

/// MOVE has three forms: `move rS,rD` (op34), `move PC,rD` (op51).
fn encode_move(args: &str, _asm: &Assembler) -> R {
    let (a, b) = args2(args)?;
    let r2 = dreg(&b)?;
    if a.trim().eq_ignore_ascii_case("pc") {
        return Ok(word(51, 0, r2));
    }
    let r1 = dreg(&a)?;
    Ok(word(34, r1, r2))
}

/// LOAD forms: `load (rS),rD` / `load (r14+n),rD` / `load (r14+rS),rD`.
/// `base_op` is the plain-`(rS)` opcode (39/40/41/42); indexed forms map to the
/// dedicated opcodes.
fn encode_load(base_op: u8, args: &str, asm: &Assembler) -> R {
    let (a, b) = args2(args)?;
    let dst = dreg(&b)?;
    match parse_addr(&a, asm)? {
        Addr::Reg(r) => Ok(word(base_op, r, dst)),
        Addr::IdxImm(base, v) => {
            if base_op != 41 {
                return Err(msg("indexed addressing only valid for `load` (32-bit)"));
            }
            if v == 0 || v > 32 {
                return Err(msg("indexed load offset out of range 1..32 longwords"));
            }
            let op = if base == 14 { 43 } else { 44 };
            Ok(word(op, (v & 0x1F) as u16, dst))
        }
        Addr::IdxReg(base, ir) => {
            if base_op != 41 {
                return Err(msg("register-indexed addressing only valid for `load`"));
            }
            let op = if base == 14 { 58 } else { 59 };
            Ok(word(op, ir, dst))
        }
    }
}

/// STORE forms: `store rData,(rAddr)` / `store rData,(r14+n)` / `(r14+rS)`.
fn encode_store(base_op: u8, args: &str, asm: &Assembler) -> R {
    let (a, b) = args2(args)?;
    let data = dreg(&a)?;
    match parse_addr(&b, asm)? {
        Addr::Reg(r) => Ok(word(base_op, r, data)),
        Addr::IdxImm(base, v) => {
            if base_op != 47 {
                return Err(msg("indexed addressing only valid for `store` (32-bit)"));
            }
            if v == 0 || v > 32 {
                return Err(msg("indexed store offset out of range 1..32 longwords"));
            }
            // op49/50: data=reg1, index quick=reg2
            let op = if base == 14 { 49 } else { 50 };
            Ok((op, vec![((op as u16) << 10) | ((data & 0x1F) << 5) | ((v & 0x1F) as u16)]))
        }
        Addr::IdxReg(base, ir) => {
            if base_op != 47 {
                return Err(msg("register-indexed addressing only valid for `store`"));
            }
            // op60/61: data=reg1, offset reg=reg2
            let op = if base == 14 { 60 } else { 61 };
            Ok((op, vec![((op as u16) << 10) | ((data & 0x1F) << 5) | (ir & 0x1F)]))
        }
    }
}

/// `jump cc,(rN)` or `jump (rN)`.
fn encode_jump(args: &str, asm: &Assembler) -> R {
    let parts = crate::split_args(args);
    let (cc, addr) = match parts.len() {
        1 => (0u16, parts[0].clone()),
        2 => (
            parse_cc(&parts[0]).ok_or_else(|| msg(format!("unknown condition `{}`", parts[0])))?,
            parts[1].clone(),
        ),
        _ => return Err(msg("jump expects `cc,(rN)` or `(rN)`")),
    };
    match parse_addr(&addr, asm)? {
        Addr::Reg(r) => Ok(word(52, r, cc)),
        _ => Err(msg("jump target must be a plain register `(rN)`")),
    }
}

/// `jr cc,label` or `jr label`. Encodes a signed 5-bit WORD displacement
/// relative to the delay-slot address (jr_addr + 2). Out of range → fix-it.
fn encode_jr(args: &str, asm: &Assembler) -> R {
    let parts = crate::split_args(args);
    let (cc, target) = match parts.len() {
        1 => (0u16, parts[0].clone()),
        2 => (
            parse_cc(&parts[0]).ok_or_else(|| msg(format!("unknown condition `{}`", parts[0])))?,
            parts[1].clone(),
        ),
        _ => return Err(msg("jr expects `cc,label` or `label`")),
    };
    let dest = asm.eval_pub(&target).map_err(msg)?;
    let here = asm.cur_pc();
    let delta = (dest as i64) - (here as i64 + 2);
    if delta % 2 != 0 {
        return Err(msg("jr target is not word-aligned"));
    }
    let words = delta / 2;
    if !(-16..=15).contains(&words) {
        // Only a hard error in pass 2 (pass 1 addresses may be provisional).
        if asm.in_pass2() {
            return Err(fix(
                format!("jr displacement {words} words out of range -16..15"),
                "use `movei #target,rT` then `jump cc,(rT)` for a far branch",
            ));
        }
    }
    Ok(word(53, (words as u16) & 0x1F, cc))
}
