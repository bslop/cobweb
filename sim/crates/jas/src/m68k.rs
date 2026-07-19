//! 68000 assembler — the `.68000` mode of jas.
//!
//! Real Jaguar projects interleave a 68k host section (the "manager" CPU) with
//! GPU/DSP RISC code in one source file. To be the successor to rmac/rln, jas
//! has to assemble both. This module encodes the 68000 subset the corpus
//! actually uses — move/moveq/movea/movem, lea, arithmetic/logic (register,
//! immediate, and quick forms), shifts, the bit ops, all the branches plus
//! dbcc, jmp/jsr, link/unlk, and the returns — with the full effective-address
//! mode set. Encodings are validated by assembling and running in jag-core's
//! 68k interpreter, the same way the JRISC encoder is checked against jsim.
//!
//! Two-pass: branch displacements and `.extern` absolute references resolve on
//! pass 2; externs referenced through an absolute address record a `Long`
//! relocation for jln.

use crate::object::RelKind;
use crate::{Assembler, EncodeErr};

/// Result of encoding: the instruction words (opcode + extensions), plus an
/// optional relocation `(word-index, kind, symbol, addend)`.
pub(crate) struct M68kEnc {
    pub(crate) words: Vec<u16>,
    pub(crate) reloc: Option<(u32, RelKind, String, i64)>,
}

fn msg(m: impl Into<String>) -> EncodeErr {
    EncodeErr::Message(m.into())
}

/// Operand size.
#[derive(Clone, Copy, PartialEq)]
enum Sz {
    B,
    W,
    L,
}

impl Sz {
    /// two-bit size field used by most ops (byte=00 word=01 long=10)
    fn field(self) -> u16 {
        match self {
            Sz::B => 0,
            Sz::W => 1,
            Sz::L => 2,
        }
    }
    /// move size field (byte=01 word=11 long=10)
    fn move_field(self) -> u16 {
        match self {
            Sz::B => 1,
            Sz::W => 3,
            Sz::L => 2,
        }
    }
}

/// A parsed effective address: mode/reg (6-bit EA) + extension words, and an
/// optional symbol relocation carried by an absolute long.
struct Ea {
    mode: u16,
    reg: u16,
    ext: Vec<u16>,
    reloc: Option<(String, i64)>, // for abs.l of an external symbol
    /// index (in ext) of the reloc's first word
    reloc_ext_off: usize,
}

impl Ea {
    fn field(&self) -> u16 {
        (self.mode << 3) | self.reg
    }
}

fn dreg(s: &str) -> Option<u16> {
    let s = s.trim();
    let n = s.strip_prefix(['d', 'D'])?;
    let r: u16 = n.parse().ok()?;
    (r < 8).then_some(r)
}
fn areg(s: &str) -> Option<u16> {
    let s = s.trim();
    if s.eq_ignore_ascii_case("sp") {
        return Some(7);
    }
    let n = s.strip_prefix(['a', 'A'])?;
    let r: u16 = n.parse().ok()?;
    (r < 8).then_some(r)
}
/// Index of the `(` that matches the final `)` of `s` (which must end in `)`),
/// scanning right-to-left with paren depth. Lets `EXPR(An)` split correctly even
/// when `EXPR` itself contains parentheses, e.g. `(640-16)(a1)` → open at the
/// `(a1)` group.
fn matching_open(s: &str) -> Option<usize> {
    let bytes = s.as_bytes();
    if bytes.last() != Some(&b')') {
        return None;
    }
    let mut depth = 0i32;
    for i in (0..bytes.len()).rev() {
        match bytes[i] {
            b')' => depth += 1,
            b'(' => {
                depth -= 1;
                if depth == 0 {
                    return Some(i);
                }
            }
            _ => {}
        }
    }
    None
}

fn anyreg_idx(s: &str) -> Option<u16> {
    // index register token for d8(An,Xn): Dn -> 0nnn, An -> 1nnn, size .w/.l
    let s = s.trim();
    let (base, long) = match s.rsplit_once('.') {
        Some((b, sfx)) => (b.trim(), sfx.eq_ignore_ascii_case("l")),
        None => (s, false),
    };
    let (da, n) = if let Some(d) = dreg(base) {
        (0u16, d)
    } else if let Some(a) = areg(base) {
        (1u16, a)
    } else {
        return None;
    };
    // brief extension word: D/A(1) | reg(3) | W/L(1) | 0 0 0 | disp8
    Some((da << 15) | (n << 12) | ((long as u16) << 11))
}

fn parse_size(mnem: &str, default: Sz) -> (String, Sz) {
    if let Some((base, sfx)) = mnem.rsplit_once('.') {
        let sz = match sfx.to_ascii_lowercase().as_str() {
            "b" => Sz::B,
            "w" => Sz::W,
            "l" => Sz::L,
            _ => return (mnem.to_string(), default),
        };
        (base.to_string(), sz)
    } else {
        (mnem.to_string(), default)
    }
}

/// Parse an effective address operand. `sz` sizes an immediate.
fn parse_ea(op: &str, sz: Sz, asm: &Assembler) -> Result<Ea, EncodeErr> {
    let s = op.trim();
    let none = |mode, reg| Ea { mode, reg, ext: vec![], reloc: None, reloc_ext_off: 0 };

    if let Some(d) = dreg(s) {
        return Ok(none(0, d));
    }
    if let Some(a) = areg(s) {
        return Ok(none(1, a));
    }
    // immediate
    if let Some(imm) = s.strip_prefix('#') {
        let imm = imm.trim();
        // A long immediate that is the address of an external symbol relocates
        // (e.g. `move.l #_vi_isr,vec` installing a handler address).
        if sz == Sz::L {
            if let Some((sym, addend)) = asm.reloc_symbol_abs_pub(imm) {
                return Ok(Ea {
                    mode: 7,
                    reg: 4,
                    ext: vec![(addend >> 16) as u16, addend as u16],
                    reloc: Some((sym, addend)),
                    reloc_ext_off: 0,
                });
            }
        }
        let v = asm.eval_pub(imm).map_err(msg)?;
        let ext = match sz {
            Sz::L => vec![(v >> 16) as u16, v as u16],
            _ => vec![v as u16],
        };
        return Ok(Ea { mode: 7, reg: 4, ext, reloc: None, reloc_ext_off: 0 });
    }
    // -(An)
    if let Some(inner) = s.strip_prefix("-(").and_then(|x| x.strip_suffix(')')) {
        if let Some(a) = areg(inner) {
            return Ok(none(4, a));
        }
    }
    // (An)+ / (An) / (d,An,Xn) / (d,An) — only when the whole operand is ONE
    // parenthesized group (so `(640-16)(a1)` falls through to the d16(An) form
    // below, where the leading `(` belongs to the displacement expression).
    let whole_group = {
        let body = s.trim_end_matches('+');
        body.starts_with('(') && body.ends_with(')') && matching_open(body) == Some(0)
    };
    if whole_group || s.ends_with(")+") {
        let postinc = s.ends_with(")+");
        let body = s.trim_end_matches('+');
        let inner = &body[1..body.len() - 1];
        let parts: Vec<&str> = inner.splitn(3, ',').map(|x| x.trim()).collect();
        match parts.as_slice() {
            [one] => {
                if let Some(a) = areg(one) {
                    return Ok(none(if postinc { 3 } else { 2 }, a));
                }
            }
            [an, xn] if areg(an).is_some() && anyreg_idx(xn).is_some() => {
                // (An, Xn) — base + index, zero displacement (GAS drops the 0)
                let a = areg(an).unwrap();
                let brief = anyreg_idx(xn).unwrap();
                return Ok(Ea { mode: 6, reg: a, ext: vec![brief], reloc: None, reloc_ext_off: 0 });
            }
            [d, an] => {
                // (d16, An)
                if let Some(a) = areg(an) {
                    let disp = asm.eval_pub(d).map_err(msg)? as u16;
                    return Ok(Ea { mode: 5, reg: a, ext: vec![disp], reloc: None, reloc_ext_off: 0 });
                }
            }
            [d, an, xn] => {
                if let Some(a) = areg(an) {
                    let disp = asm.eval_pub(d).map_err(msg)? as i8 as u8;
                    let brief = anyreg_idx(xn).ok_or_else(|| msg(format!("bad index reg `{xn}`")))?;
                    return Ok(Ea {
                        mode: 6,
                        reg: a,
                        ext: vec![brief | (disp as u16)],
                        reloc: None,
                        reloc_ext_off: 0,
                    });
                }
            }
            _ => {}
        }
        return Err(msg(format!("unsupported addressing `{s}`")));
    }
    // d16(An) / d8(An,Xn) / d16(PC) / d8(PC,Xn) — classic paren-suffix form. The
    // register group is the LAST parenthesized group; the displacement (which may
    // itself be a parenthesized expression, e.g. `(640-16)(a1)`) precedes it.
    if s.ends_with(')') {
        if let Some(open) = matching_open(s) {
            let disp_str = &s[..open];
            let inner = &s[open + 1..s.len() - 1];
            let parts: Vec<&str> = inner.splitn(2, ',').map(|x| x.trim()).collect();
            let disp_val = if disp_str.trim().is_empty() {
                0
            } else {
                asm.eval_pub(disp_str.trim()).map_err(msg)? as i64
            };
            match parts.as_slice() {
                [base] if base.eq_ignore_ascii_case("pc") => {
                    return Ok(Ea { mode: 7, reg: 2, ext: vec![disp_val as u16], reloc: None, reloc_ext_off: 0 });
                }
                [base] => {
                    if let Some(a) = areg(base) {
                        return Ok(Ea { mode: 5, reg: a, ext: vec![disp_val as u16], reloc: None, reloc_ext_off: 0 });
                    }
                }
                [base, xn] => {
                    let brief = anyreg_idx(xn).ok_or_else(|| msg(format!("bad index reg `{xn}`")))?;
                    let d8 = (disp_val as i8) as u8 as u16;
                    if base.eq_ignore_ascii_case("pc") {
                        return Ok(Ea { mode: 7, reg: 3, ext: vec![brief | d8], reloc: None, reloc_ext_off: 0 });
                    }
                    if let Some(a) = areg(base) {
                        return Ok(Ea { mode: 6, reg: a, ext: vec![brief | d8], reloc: None, reloc_ext_off: 0 });
                    }
                }
                _ => {}
            }
        }
    }

    // absolute — number or symbol, with an optional explicit size override:
    // `EXPR.w` forces abs.w (one sign-extended word), `EXPR.l` forces abs.l.
    let (s, forced) = if let Some(b) = s.strip_suffix(".w").or_else(|| s.strip_suffix(".W")) {
        (b.trim_end(), Some(false))
    } else if let Some(b) = s.strip_suffix(".l").or_else(|| s.strip_suffix(".L")) {
        (b.trim_end(), Some(true))
    } else {
        (s, None)
    };
    // Symbol may be extern (reloc) or defined. Prefer abs.l so externs relocate
    // cleanly (unless `.w` was requested explicitly).
    if let Some((sym, addend)) = asm.reloc_symbol_abs_pub(s) {
        if forced == Some(false) {
            return Ok(Ea { mode: 7, reg: 0, ext: vec![addend as u16], reloc: Some((sym, addend)), reloc_ext_off: 0 });
        }
        return Ok(Ea {
            mode: 7,
            reg: 1,
            ext: vec![(addend >> 16) as u16, addend as u16],
            reloc: Some((sym, addend)),
            reloc_ext_off: 0,
        });
    }
    let v = asm.eval_pub(s).map_err(msg)?;
    // A symbolic operand's address isn't fixed until link, so it must be abs.l
    // (unless `.w` was requested) — this keeps its size identical across passes
    // even when the value currently resolves small. Pure numbers size by value.
    let use_word = match forced {
        Some(w) => !w, // forced .w → true, .l → false
        None if asm.is_symbolic(s) => false,
        None => v <= 0x7FFF || v >= 0xFFFF_8000,
    };
    if use_word {
        // abs.w (sign-extended)
        Ok(Ea { mode: 7, reg: 0, ext: vec![v as u16], reloc: None, reloc_ext_off: 0 })
    } else {
        Ok(Ea { mode: 7, reg: 1, ext: vec![(v >> 16) as u16, v as u16], reloc: None, reloc_ext_off: 0 })
    }
}

fn cc_field(name: &str) -> Option<u16> {
    Some(match name.to_ascii_lowercase().as_str() {
        "ra" | "t" => 0, // BRA uses 0; "t" for dbt
        "f" | "sr" => 1, // BSR handled separately; DBF/DBRA cc=1
        "hi" => 2,
        "ls" => 3,
        "cc" | "hs" => 4,
        "cs" | "lo" => 5,
        "ne" => 6,
        "eq" => 7,
        "vc" => 8,
        "vs" => 9,
        "pl" => 10,
        "mi" => 11,
        "ge" => 12,
        "lt" => 13,
        "gt" => 14,
        "le" => 15,
        _ => return None,
    })
}

fn split2(args: &str) -> Result<(String, String), EncodeErr> {
    let p = crate::split_args(args);
    if p.len() != 2 {
        return Err(msg(format!("expected 2 operands, found {}", p.len())));
    }
    Ok((p[0].clone(), p[1].clone()))
}

/// Encode one 68000 instruction. `here` is the address of this instruction
/// (for PC-relative branch displacement).
pub(crate) fn encode(mnem: &str, args: &str, here: u32, asm: &Assembler) -> Result<M68kEnc, EncodeErr> {
    let low = mnem.to_ascii_lowercase();

    // ── returns / simple ────────────────────────────────────────────────────
    let simple = match low.as_str() {
        "rts" => Some(0x4E75),
        "rte" => Some(0x4E73),
        "rtr" => Some(0x4E77),
        "nop" => Some(0x4E71),
        "reset" => Some(0x4E70),
        "trapv" => Some(0x4E76),
        "illegal" => Some(0x4AFC),
        _ => None,
    };
    if let Some(w) = simple {
        return Ok(one(w));
    }

    // ── branches: bra/bsr/bcc (+ .b/.s/.w) ───────────────────────────────────
    // Strip a trailing size (`.b`/`.s`/`.w`/`.l`) for the mnemonic base; the
    // short (`.s`/`.b`) vs word (`.w`) choice is re-read from `low` below.
    let bbase: String = match low.rsplit_once('.') {
        Some((b, s)) if matches!(s, "b" | "s" | "w" | "l") => b.to_string(),
        _ => low.clone(),
    };
    if bbase == "bra" || bbase == "bsr" || bbase.starts_with('b') && cc_field(&bbase[1..]).is_some() {
        let target = args.trim();
        // extern target -> reloc via absolute? 68000 branches are PC-relative;
        // for an undefined/extern target we fall back to a 16-bit disp of 0 with
        // a reloc note (jln patches). For local labels, compute the displacement.
        let opbase: u16 = if bbase == "bsr" {
            0x6100
        } else if bbase == "bra" {
            0x6000
        } else {
            0x6000 | (cc_field(&bbase[1..]).unwrap() << 8)
        };
        // The form (and thus the size) is fixed by the suffix, independent of the
        // displacement: `.s`/`.b` → short (1 word), `.w` or no suffix → 16-bit
        // (2 words). Not auto-relaxing keeps sizing identical across passes, so a
        // forward reference — unbound (0) in pass 1 — never changes byte count.
        let force_short = low.ends_with(".s") || low.ends_with(".b");
        // A short branch can only reach a same-object label (±127 bytes), so it
        // is never a cross-object relocation — always resolve it PC-relative.
        // Word-form branches to an *external* symbol relocate (jln patches the
        // displacement); same-object word branches resolve PC-relative too.
        if !force_short {
            if let Some((sym, addend)) = asm.reloc_symbol_pub(target) {
                return Ok(M68kEnc {
                    words: vec![opbase, addend as u16],
                    reloc: Some((1, RelKind::Word, sym, addend)),
                });
            }
        }
        let dest = asm.eval_pub(target).map_err(msg)?;
        let disp = dest as i64 - (here as i64 + 2);
        // Pass 1 only sizes: emit right-sized placeholders, never range-check a
        // displacement whose target isn't bound yet.
        if asm.pass() == 1 {
            return Ok(if force_short {
                one(opbase)
            } else {
                M68kEnc { words: vec![opbase, 0], reloc: None }
            });
        }
        if force_short {
            if disp == 0 {
                return Err(msg("short branch to next instruction (disp 0) — use .w"));
            }
            if !(-128..=127).contains(&disp) {
                return Err(msg("short branch displacement out of range — use .w"));
            }
            return Ok(one(opbase | (disp as u8 as u16)));
        }
        // 16-bit form
        if (-32768..=32767).contains(&disp) {
            return Ok(M68kEnc { words: vec![opbase, disp as u16], reloc: None });
        }
        return Err(msg("branch displacement out of 16-bit range (68000 has no long branch)"));
    }

    // ── dbcc / dbra ──────────────────────────────────────────────────────────
    if low.starts_with("db") {
        let cc = if &low == "dbra" || &low == "dbf" {
            1
        } else {
            cc_field(&low[2..]).ok_or_else(|| msg(format!("unknown dbcc `{low}`")))?
        };
        let (dn_s, tgt) = split2(args)?;
        let dn = dreg(&dn_s).ok_or_else(|| msg("dbcc needs a data register"))?;
        let dest = asm.eval_pub(tgt.trim()).map_err(msg)?;
        let disp = dest as i64 - (here as i64 + 2);
        return Ok(M68kEnc { words: vec![0x50C8 | (cc << 8) | dn, disp as u16], reloc: None });
    }

    // ── moveq ────────────────────────────────────────────────────────────────
    if low == "moveq" {
        let (imm, dn_s) = split2(args)?;
        let v = asm.eval_pub(imm.trim().trim_start_matches('#')).map_err(msg)? as i32;
        if !(-128..=127).contains(&v) {
            return Err(msg("moveq immediate out of range -128..127"));
        }
        let dn = dreg(&dn_s).ok_or_else(|| msg("moveq destination must be a data register"))?;
        return Ok(one(0x7000 | (dn << 9) | (v as u8 as u16)));
    }

    // ── move / movea ──────────────────────────────────────────────────────────
    let (base, sz) = parse_size(&low, Sz::W);
    if base == "move" || base == "movea" {
        let (src, dst) = split2(args)?;
        let (srt, dtt) = (src.trim(), dst.trim());
        // special-register moves (SR/CCR/USP) — the operand order is source,dest
        let is_sr = |x: &str| x.eq_ignore_ascii_case("sr");
        let is_ccr = |x: &str| x.eq_ignore_ascii_case("ccr");
        let is_usp = |x: &str| x.eq_ignore_ascii_case("usp");
        if is_sr(dtt) {
            // MOVE <ea>,SR  (0x46C0 | ea) — word
            let s = parse_ea(srt, Sz::W, asm)?;
            return Ok(with_src(0x46C0 | s.field(), &s));
        }
        if is_sr(srt) {
            // MOVE SR,<ea>  (0x40C0 | ea)
            let d = parse_ea(dtt, Sz::W, asm)?;
            return Ok(with_src(0x40C0 | d.field(), &d));
        }
        if is_ccr(dtt) {
            // MOVE <ea>,CCR (0x44C0 | ea)
            let s = parse_ea(srt, Sz::W, asm)?;
            return Ok(with_src(0x44C0 | s.field(), &s));
        }
        if is_usp(dtt) {
            // MOVE An,USP (0x4E60 | An)
            let a = areg(srt).ok_or_else(|| msg("move to USP needs an address register"))?;
            return Ok(one(0x4E60 | a));
        }
        if is_usp(srt) {
            // MOVE USP,An (0x4E68 | An)
            let a = areg(dtt).ok_or_else(|| msg("move from USP needs an address register"))?;
            return Ok(one(0x4E68 | a));
        }
        let s = parse_ea(srt, sz, asm)?;
        let d = parse_ea(dtt, sz, asm)?;
        let op = (sz.move_field() << 12) | (d.reg << 9) | (d.mode << 6) | s.field();
        return Ok(assemble_words(op, &s, &d));
    }

    // ── lea / pea ─────────────────────────────────────────────────────────────
    if base == "lea" {
        let (src, an_s) = split2(args)?;
        let an = areg(&an_s).ok_or_else(|| msg("lea destination must be an address register"))?;
        let s = parse_ea(&src, Sz::L, asm)?;
        let op = 0x41C0 | (an << 9) | s.field();
        return Ok(with_src(op, &s));
    }
    if base == "pea" {
        let s = parse_ea(args.trim(), Sz::L, asm)?;
        return Ok(with_src(0x4840 | s.field(), &s));
    }

    // ── clr / tst / neg / not / swap / ext ────────────────────────────────────
    if let Some(opbase) = match base.as_str() {
        "clr" => Some(0x4200u16),
        "tst" => Some(0x4A00),
        "neg" => Some(0x4400),
        "not" => Some(0x4600),
        _ => None,
    } {
        let s = parse_ea(args.trim(), sz, asm)?;
        return Ok(with_src(opbase | (sz.field() << 6) | s.field(), &s));
    }
    if base == "swap" {
        let dn = dreg(args.trim()).ok_or_else(|| msg("swap needs a data register"))?;
        return Ok(one(0x4840 | dn));
    }
    if base == "ext" {
        let dn = dreg(args.trim()).ok_or_else(|| msg("ext needs a data register"))?;
        let w = if sz == Sz::L { 0x48C0 } else { 0x4880 };
        return Ok(one(w | dn));
    }

    // ── mulu / muls / divu / divs (word form: <ea>,Dn) ────────────────────────
    if let Some(opbase) = match base.as_str() {
        "mulu" => Some(0xC0C0u16),
        "muls" => Some(0xC1C0),
        "divu" => Some(0x80C0),
        "divs" => Some(0x81C0),
        _ => None,
    } {
        let (src, dn_s) = split2(args)?;
        let dn = dreg(&dn_s).ok_or_else(|| msg("mul/div destination must be a data register"))?;
        let s = parse_ea(&src, Sz::W, asm)?;
        return Ok(with_src(opbase | (dn << 9) | s.field(), &s));
    }

    // ── addx / subx (data-register form: Dy,Dx) ──────────────────────────────
    if let Some(opbase) = match base.as_str() {
        "addx" => Some(0xD100u16),
        "subx" => Some(0x9100),
        _ => None,
    } {
        let (dy_s, dx_s) = split2(args)?;
        let dy = dreg(&dy_s).ok_or_else(|| msg("addx/subx source must be a data register"))?;
        let dx = dreg(&dx_s).ok_or_else(|| msg("addx/subx dest must be a data register"))?;
        return Ok(one(opbase | (dx << 9) | (sz.field() << 6) | dy));
    }

    // ── Scc <ea> (set a byte to $FF/$00 on the condition) ─────────────────────
    // `s` + a valid condition suffix. `sub`/`suba`/`subq`/`swap` carry invalid
    // suffixes, so they fall through to their own handlers below.
    if base.len() >= 2 && base.starts_with('s') {
        if let Some(cc) = cc_field(&base[1..]) {
            let s = parse_ea(args.trim(), Sz::B, asm)?;
            return Ok(with_src(0x50C0 | (cc << 8) | s.field(), &s));
        }
    }

    // ── jmp / jsr ─────────────────────────────────────────────────────────────
    if base == "jmp" || base == "jsr" {
        let s = parse_ea(args.trim(), Sz::L, asm)?;
        let opbase = if base == "jsr" { 0x4E80 } else { 0x4EC0 };
        return Ok(with_src(opbase | s.field(), &s));
    }

    // ── link / unlk ───────────────────────────────────────────────────────────
    if base == "link" {
        let (an_s, disp) = split2(args)?;
        let an = areg(&an_s).ok_or_else(|| msg("link needs an address register"))?;
        let d = asm.eval_pub(disp.trim().trim_start_matches('#')).map_err(msg)? as u16;
        return Ok(M68kEnc { words: vec![0x4E50 | an, d], reloc: None });
    }
    if base == "unlk" {
        let an = areg(args.trim()).ok_or_else(|| msg("unlk needs an address register"))?;
        return Ok(one(0x4E58 | an));
    }

    // ── addq / subq ───────────────────────────────────────────────────────────
    if base == "addq" || base == "subq" {
        let (imm, ea_s) = split2(args)?;
        let mut n = asm.eval_pub(imm.trim().trim_start_matches('#')).map_err(msg)?;
        if n == 0 || n > 8 {
            return Err(msg("addq/subq count out of range 1..8"));
        }
        if n == 8 {
            n = 0;
        }
        let ea = parse_ea(&ea_s, sz, asm)?;
        let opbase = if base == "subq" { 0x5100 } else { 0x5000 };
        return Ok(with_src(opbase | ((n as u16) << 9) | (sz.field() << 6) | ea.field(), &ea));
    }

    // ── immediate arithmetic/logic: addi subi andi ori eori cmpi ──────────────
    // GAS spells these as plain `add`/`sub`/`and`/`or`/`eor`/`cmp` with an
    // immediate source; route them here unless the destination is an address
    // register (which needs adda/suba/cmpa, handled below).
    let (imm_src, areg_dst) = {
        let p = crate::split_args(args);
        let imm = p.first().map(|a| a.trim_start().starts_with('#')).unwrap_or(false);
        let ad = p.get(1).map(|d| areg(d.trim()).is_some()).unwrap_or(false);
        (imm, ad)
    };
    if let Some(opbase) = match base.as_str() {
        "ori" => Some(0x0000u16),
        "andi" => Some(0x0200),
        "subi" => Some(0x0400),
        "addi" => Some(0x0600),
        "eori" => Some(0x0A00),
        "cmpi" => Some(0x0C00),
        "or" if imm_src && !areg_dst => Some(0x0000),
        "and" if imm_src && !areg_dst => Some(0x0200),
        "sub" if imm_src && !areg_dst => Some(0x0400),
        "add" if imm_src && !areg_dst => Some(0x0600),
        "eor" if imm_src && !areg_dst => Some(0x0A00),
        "cmp" if imm_src && !areg_dst => Some(0x0C00),
        _ => None,
    } {
        let (imm, ea_s) = split2(args)?;
        let v = asm.eval_pub(imm.trim().trim_start_matches('#')).map_err(msg)?;
        let ea = parse_ea(&ea_s, sz, asm)?;
        let mut words = vec![opbase | (sz.field() << 6) | ea.field()];
        match sz {
            Sz::L => {
                words.push((v >> 16) as u16);
                words.push(v as u16);
            }
            _ => words.push(v as u16),
        }
        words.extend_from_slice(&ea.ext);
        return Ok(M68kEnc { words, reloc: shift_reloc(&ea, /*after*/ 1 + imm_words(sz)) });
    }

    // ── register/EA arithmetic+logic: add sub and or eor cmp ──────────────────
    if let Some((opbase, ea_to_dn, dn_to_ea)) = match base.as_str() {
        "add" => Some((0xD000u16, true, true)),
        "sub" => Some((0x9000, true, true)),
        "and" => Some((0xC000, true, true)),
        "or" => Some((0x8000, true, true)),
        "cmp" => Some((0xB000, true, false)),
        "eor" => Some((0xB000, false, true)),
        _ => None,
    } {
        let (a, b) = split2(args)?;
        // Dn is one side. Determine direction.
        if let Some(dn) = dreg(&b) {
            if ea_to_dn {
                // <ea>,Dn
                let ea = parse_ea(&a, sz, asm)?;
                let op = opbase | (dn << 9) | (sz.field() << 6) | ea.field();
                return Ok(with_src(op, &ea));
            }
        }
        if let Some(dn) = dreg(&a) {
            if dn_to_ea {
                // Dn,<ea>
                let ea = parse_ea(&b, sz, asm)?;
                let op = opbase | (dn << 9) | (0b100 << 6) | (sz.field() << 6) | ea.field();
                return Ok(with_src(op, &ea));
            }
        }
        // `add`/`sub`/`cmp` with an address-register destination is really the
        // `adda`/`suba`/`cmpa` form (GAS spells them all `add`/`sub`/`cmp`).
        if let Some(an) = areg(&b) {
            if matches!(base.as_str(), "add" | "sub" | "cmp") {
                let ea = parse_ea(&a, sz, asm)?;
                let opmode = if sz == Sz::L { 0b111 } else { 0b011 };
                return Ok(with_src(opbase | (an << 9) | (opmode << 6) | ea.field(), &ea));
            }
        }
        return Err(msg(format!("`{base}` needs a data register on one side (v1)")));
    }

    // ── adda / suba / cmpa ────────────────────────────────────────────────────
    if let Some(opbase) = match base.as_str() {
        "adda" => Some(0xD000u16),
        "suba" => Some(0x9000),
        "cmpa" => Some(0xB000),
        _ => None,
    } {
        let (src, an_s) = split2(args)?;
        let an = areg(&an_s).ok_or_else(|| msg("`*a` destination must be an address register"))?;
        let ea = parse_ea(&src, sz, asm)?;
        // opmode: word=011, long=111
        let opmode = if sz == Sz::L { 0b111 } else { 0b011 };
        return Ok(with_src(opbase | (an << 9) | (opmode << 6) | ea.field(), &ea));
    }

    // ── shifts: as/ls/rox/ro l/r ─────────────────────────────────────────────
    if let Some((ty, dir)) = shift_kind(&base) {
        // forms: `LSL #n,Dy` / `LSL Dx,Dy` / `LSL <ea>` (by 1)
        let parts = crate::split_args(args);
        if parts.len() == 2 {
            let (cnt, dy_s) = (parts[0].trim(), parts[1].trim());
            let dy = dreg(dy_s).ok_or_else(|| msg("shift register form needs Dy"))?;
            let (cr, ir) = if let Some(imm) = cnt.strip_prefix('#') {
                let n = asm.eval_pub(imm.trim()).map_err(msg)?;
                ((if n == 8 { 0 } else { n as u16 & 7 }), 0)
            } else if let Some(dx) = dreg(cnt) {
                (dx, 1)
            } else {
                return Err(msg("shift count must be #imm or a data register"));
            };
            let op = 0xE000 | (cr << 9) | (dir << 8) | (sz.field() << 6) | (ir << 5) | (ty << 3) | dy;
            return Ok(one(op));
        } else if parts.len() == 1 {
            let ea = parse_ea(parts[0].trim(), Sz::W, asm)?;
            let op = 0xE0C0 | (ty << 9) | (dir << 8) | ea.field();
            return Ok(with_src(op, &ea));
        }
        return Err(msg("bad shift operands"));
    }

    // ── bit ops: btst bset bclr bchg (dynamic Dn / static #) ──────────────────
    if let Some(ty) = match base.as_str() {
        "btst" => Some(0u16),
        "bchg" => Some(1),
        "bclr" => Some(2),
        "bset" => Some(3),
        _ => None,
    } {
        let (bit, ea_s) = split2(args)?;
        let ea = parse_ea(&ea_s, Sz::B, asm)?;
        if let Some(imm) = bit.trim().strip_prefix('#') {
            let n = asm.eval_pub(imm.trim()).map_err(msg)? as u16;
            let mut words = vec![0x0800 | (ty << 6) | ea.field(), n];
            words.extend_from_slice(&ea.ext);
            return Ok(M68kEnc { words, reloc: None });
        }
        let dn = dreg(bit.trim()).ok_or_else(|| msg("bit op needs #n or Dn"))?;
        return Ok(with_src(0x0100 | (dn << 9) | (ty << 6) | ea.field(), &ea));
    }

    // ── movem ─────────────────────────────────────────────────────────────────
    if base == "movem" {
        return encode_movem(args, sz, asm);
    }

    Err(EncodeErr::Unknown)
}

fn imm_words(sz: Sz) -> usize {
    if sz == Sz::L {
        2
    } else {
        1
    }
}

/// If the EA carries a reloc, shift its word offset by `base_words` (the words
/// emitted before the EA extension).
fn shift_reloc(ea: &Ea, base_words: usize) -> Option<(u32, RelKind, String, i64)> {
    ea.reloc
        .as_ref()
        .map(|(s, a)| ((base_words + ea.reloc_ext_off) as u32, RelKind::Long, s.clone(), *a))
}

fn one(w: u16) -> M68kEnc {
    M68kEnc { words: vec![w], reloc: None }
}

/// opcode + a single source EA's extension words.
fn with_src(op: u16, ea: &Ea) -> M68kEnc {
    let mut words = vec![op];
    words.extend_from_slice(&ea.ext);
    M68kEnc { words, reloc: shift_reloc(ea, 1) }
}

/// MOVE: opcode + source ext + dest ext (source first).
fn assemble_words(op: u16, s: &Ea, d: &Ea) -> M68kEnc {
    let mut words = vec![op];
    words.extend_from_slice(&s.ext);
    let reloc = s
        .reloc
        .as_ref()
        .map(|(sym, a)| (1u32, RelKind::Long, sym.clone(), *a))
        .or_else(|| {
            d.reloc
                .as_ref()
                .map(|(sym, a)| ((1 + s.ext.len()) as u32, RelKind::Long, sym.clone(), *a))
        });
    words.extend_from_slice(&d.ext);
    M68kEnc { words, reloc }
}

fn shift_kind(base: &str) -> Option<(u16, u16)> {
    // returns (type[1:0], direction dir bit): AS=00 LS=01 ROX=10 RO=11, dir 1=left
    let (ty, rest) = if let Some(r) = base.strip_prefix("as") {
        (0, r)
    } else if let Some(r) = base.strip_prefix("ls") {
        (1, r)
    } else if let Some(r) = base.strip_prefix("rox") {
        (2, r)
    } else if let Some(r) = base.strip_prefix("ro") {
        (3, r)
    } else {
        return None;
    };
    let dir = match rest {
        "l" => 1,
        "r" => 0,
        _ => return None,
    };
    Some((ty, dir))
}

/// Parse a register list like `d0-d7/a0-a3` into a movem mask (bit i set = reg
/// in list; order d0..d7,a0..a7 for the control/postinc form).
fn reglist_mask(list: &str) -> Option<u16> {
    let mut mask = 0u16;
    let bit = |tok: &str| -> Option<u16> {
        if let Some(d) = dreg(tok) {
            Some(d)
        } else {
            areg(tok).map(|a| 8 + a)
        }
    };
    for grp in list.split('/') {
        let grp = grp.trim();
        if let Some((a, b)) = grp.split_once('-') {
            let (lo, hi) = (bit(a.trim())?, bit(b.trim())?);
            for i in lo..=hi {
                mask |= 1 << i;
            }
        } else {
            mask |= 1 << bit(grp)?;
        }
    }
    Some(mask)
}

fn encode_movem(args: &str, sz: Sz, asm: &Assembler) -> Result<M68kEnc, EncodeErr> {
    let (a, b) = split2(args)?;
    let long = sz == Sz::L;
    // reg->mem if first operand is a register list, mem->reg otherwise
    if let Some(mask) = reglist_mask(&a) {
        // reg -> mem
        let ea = parse_ea(&b, if long { Sz::L } else { Sz::W }, asm)?;
        // for -(An) predecrement the mask bit order is reversed
        let m = if ea.mode == 4 { mask.reverse_bits() } else { mask };
        let op = 0x4880 | ((long as u16) << 6) | ea.field();
        let mut words = vec![op, m];
        words.extend_from_slice(&ea.ext);
        return Ok(M68kEnc { words, reloc: None });
    }
    if let Some(mask) = reglist_mask(&b) {
        // mem -> reg
        let ea = parse_ea(&a, if long { Sz::L } else { Sz::W }, asm)?;
        let op = 0x4C80 | ((long as u16) << 6) | ea.field();
        let mut words = vec![op, mask];
        words.extend_from_slice(&ea.ext);
        return Ok(M68kEnc { words, reloc: None });
    }
    Err(msg("movem needs a register list on one side"))
}
