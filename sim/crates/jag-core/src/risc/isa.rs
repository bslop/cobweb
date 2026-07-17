//! Jaguar RISC opcode decode + execution (Tom GPU + Jerry DSP).
//! Implements `docs/spec/RISC_ISA.md` §4 (the 64-opcode table), §5 (condition
//! codes), §6 (MOVEI), §7 (MAC), §8 (DIV). Descendant module of `risc`.

use super::Risc;
use crate::bus::Bus;

/// quick "1..32" field: raw 0 decodes as 32.
#[inline]
fn quick(r1: usize) -> u32 {
    if r1 == 0 {
        32
    } else {
        r1 as u32
    }
}

/// sign-extend a 5-bit field to i32.
#[inline]
fn sext5(v: usize) -> i32 {
    ((v as i32) << 27) >> 27
}

/// Evaluate a JRISC condition code against the current flags (spec §5).
#[inline]
fn cond(core: &Risc, cc: usize) -> bool {
    let sel = if cc & 0x10 != 0 { core.n() } else { core.c() };
    let mut ok = true;
    if cc & 0x01 != 0 {
        ok &= !core.z();
    }
    if cc & 0x02 != 0 {
        ok &= core.z();
    }
    if cc & 0x04 != 0 {
        ok &= !sel;
    }
    if cc & 0x08 != 0 {
        ok &= sel;
    }
    ok
}

pub(super) fn execute(core: &mut Risc, bus: &mut Bus, iw: u16) {
    let op = ((iw >> 10) & 0x3F) as usize;
    let r1 = ((iw >> 5) & 0x1F) as usize;
    let r2 = (iw & 0x1F) as usize;
    let b = core.cur_bank();
    let s = core.reg(b, r1);
    let d = core.reg(b, r2);

    match op {
        0 => {
            // ADD
            let (res, carry) = d.overflowing_add(s);
            core.set_reg(b, r2, res);
            core.set_c(carry);
            core.set_zn(res);
        }
        1 => {
            // ADDC
            let cin = core.c() as u64;
            let r = d as u64 + s as u64 + cin;
            let res = r as u32;
            core.set_reg(b, r2, res);
            core.set_c(r > 0xFFFF_FFFF);
            core.set_zn(res);
        }
        2 | 3 => {
            // ADDQ (2: flags) / ADDQT (3: transparent)
            let n = quick(r1);
            let (res, carry) = d.overflowing_add(n);
            core.set_reg(b, r2, res);
            if op == 2 {
                core.set_c(carry);
                core.set_zn(res);
            }
        }
        4 => {
            // SUB
            let (res, borrow) = d.overflowing_sub(s);
            core.set_reg(b, r2, res);
            core.set_c(borrow);
            core.set_zn(res);
        }
        5 => {
            // SUBC
            let cin = core.c() as u64;
            let r = (d as u64).wrapping_sub(s as u64).wrapping_sub(cin);
            let res = r as u32;
            core.set_reg(b, r2, res);
            core.set_c(r > 0xFFFF_FFFF);
            core.set_zn(res);
        }
        6 | 7 => {
            // SUBQ (6) / SUBQT (7: transparent)
            let n = quick(r1);
            let (res, borrow) = d.overflowing_sub(n);
            core.set_reg(b, r2, res);
            if op == 6 {
                core.set_c(borrow);
                core.set_zn(res);
            }
        }
        8 => {
            // NEG
            let (res, borrow) = 0u32.overflowing_sub(d);
            core.set_reg(b, r2, res);
            core.set_c(borrow);
            core.set_zn(res);
        }
        9 => {
            let res = d & s;
            core.set_reg(b, r2, res);
            core.set_zn(res);
        }
        10 => {
            let res = d | s;
            core.set_reg(b, r2, res);
            core.set_zn(res);
        }
        11 => {
            let res = d ^ s;
            core.set_reg(b, r2, res);
            core.set_zn(res);
        }
        12 => {
            let res = !d;
            core.set_reg(b, r2, res);
            core.set_zn(res);
        }
        13 => {
            // BTST n,Rd — Z = (bit n of D) == 0
            core.set_z(d & (1 << (r1 & 31)) == 0);
        }
        14 => {
            let res = d | (1 << (r1 & 31));
            core.set_reg(b, r2, res);
            core.set_zn(res);
        }
        15 => {
            let res = d & !(1 << (r1 & 31));
            core.set_reg(b, r2, res);
            core.set_zn(res);
        }
        16 => {
            // MULT (unsigned 16×16)
            let res = (d & 0xFFFF).wrapping_mul(s & 0xFFFF);
            core.set_reg(b, r2, res);
            core.set_zn(res);
        }
        17 => {
            // IMULT (signed 16×16)
            let res = ((d as i16 as i32) * (s as i16 as i32)) as u32;
            core.set_reg(b, r2, res);
            core.set_zn(res);
        }
        18 => {
            // IMULTN — start MAC (no write-back)
            core.mac = (d as i16 as i64) * (s as i16 as i64);
            let res = core.mac as u32;
            core.set_zn(res);
        }
        19 => {
            // RESMAC — write accumulator
            core.set_reg(b, r2, core.mac as u32);
        }
        20 => {
            // IMACN — accumulate (no write-back, no flags)
            core.mac = core.mac.wrapping_add((d as i16 as i64) * (s as i16 as i64));
        }
        21 => {
            // DIV (unsigned; 16.16 if div_offset)
            if s == 0 {
                core.set_reg(b, r2, 0xFFFF_FFFF);
                core.div_remainder = d;
            } else if core.div_offset {
                let num = (d as u64) << 16;
                core.set_reg(b, r2, (num / s as u64) as u32);
                core.div_remainder = (num % s as u64) as u32;
            } else {
                core.set_reg(b, r2, d / s);
                core.div_remainder = d % s;
            }
        }
        22 => {
            // ABS
            let neg = d & 0x8000_0000 != 0;
            let res = if neg { 0u32.wrapping_sub(d) } else { d };
            core.set_reg(b, r2, res);
            core.set_c(neg);
            core.set_z(res == 0);
            core.set_n(false);
        }
        23 => {
            // SH — S>=0 right by S, else left by -S
            let sh = s as i32;
            let (res, carry) = if sh >= 0 {
                let k = (sh as u32).min(32);
                let c = if k > 0 { d & 1 != 0 } else { core.c() };
                (if k >= 32 { 0 } else { d >> k }, c)
            } else {
                let k = ((-sh) as u32).min(32);
                let c = if k > 0 { d & 0x8000_0000 != 0 } else { core.c() };
                (if k >= 32 { 0 } else { d << k }, c)
            };
            core.set_reg(b, r2, res);
            core.set_c(carry);
            core.set_zn(res);
        }
        24 => {
            // SHLQ — count = 32 - r1 (r1=0 ⇒ 32)
            let n = 32 - r1 as u32;
            let res = if n >= 32 { 0 } else { d << n };
            core.set_reg(b, r2, res);
            core.set_c(d & 0x8000_0000 != 0);
            core.set_zn(res);
        }
        25 => {
            // SHRQ — logical right by quick n
            let n = quick(r1);
            let res = if n >= 32 { 0 } else { d >> n };
            core.set_reg(b, r2, res);
            core.set_c(d & 1 != 0);
            core.set_zn(res);
        }
        26 => {
            // SHA — arithmetic SH
            let sh = s as i32;
            let (res, carry) = if sh >= 0 {
                let k = (sh as u32).min(32);
                let c = if k > 0 { d & 1 != 0 } else { core.c() };
                (if k >= 32 { ((d as i32) >> 31) as u32 } else { ((d as i32) >> k) as u32 }, c)
            } else {
                let k = ((-sh) as u32).min(32);
                let c = if k > 0 { d & 0x8000_0000 != 0 } else { core.c() };
                (if k >= 32 { 0 } else { d << k }, c)
            };
            core.set_reg(b, r2, res);
            core.set_c(carry);
            core.set_zn(res);
        }
        27 => {
            // SHARQ — arithmetic right by quick n
            let n = quick(r1);
            let res = if n >= 32 { ((d as i32) >> 31) as u32 } else { ((d as i32) >> n) as u32 };
            core.set_reg(b, r2, res);
            core.set_c(d & 1 != 0);
            core.set_zn(res);
        }
        28 => {
            // ROR by S & 31
            let k = s & 31;
            let res = d.rotate_right(k);
            core.set_reg(b, r2, res);
            core.set_c(res & 0x8000_0000 != 0);
            core.set_zn(res);
        }
        29 => {
            // RORQ by quick n
            let n = quick(r1) & 31;
            let res = d.rotate_right(n);
            core.set_reg(b, r2, res);
            core.set_c(res & 0x8000_0000 != 0);
            core.set_zn(res);
        }
        30 => {
            // CMP (flags of D - S)
            let (res, borrow) = d.overflowing_sub(s);
            core.set_c(borrow);
            core.set_zn(res);
        }
        31 => {
            // CMPQ (flags of D - sext5(n))
            let n = sext5(r1) as u32;
            let (res, borrow) = d.overflowing_sub(n);
            core.set_c(borrow);
            core.set_zn(res);
        }
        32 => {
            if core.kind.is_dsp() {
                // SUBQMOD — SUBQ then modulo-mask via D_MOD
                let n = quick(r1);
                let r = d.wrapping_sub(n) & core.modulo;
                core.set_reg(b, r2, r);
                core.set_zn(r);
            } else {
                // SAT8
                let res = (d as i32).clamp(0, 255) as u32;
                core.set_reg(b, r2, res);
                core.set_z(res == 0);
                core.set_n(false);
            }
        }
        33 => {
            if core.kind.is_dsp() {
                // SAT16S signed → [-32768, 32767]
                let res = (d as i32).clamp(-32768, 32767) as u32;
                core.set_reg(b, r2, res);
                core.set_z(res == 0);
            } else {
                // SAT16 → [0, 65535]
                let res = (d as i32).clamp(0, 65535) as u32;
                core.set_reg(b, r2, res);
                core.set_z(res == 0);
                core.set_n(false);
            }
        }
        34 => {
            core.set_reg(b, r2, s);
        }
        35 => {
            core.set_reg(b, r2, r1 as u32);
        }
        36 => {
            // MOVETA — to other bank
            let v = core.reg(b, r1);
            core.set_reg(1 - b, r2, v);
        }
        37 => {
            // MOVEFA — from other bank
            let v = core.reg(1 - b, r1);
            core.set_reg(b, r2, v);
        }
        38 => {
            // MOVEI — 32-bit immediate, little-endian word order
            let w1 = core.fetch16(bus) as u32;
            let w2 = core.fetch16(bus) as u32;
            core.set_reg(b, r2, (w2 << 16) | w1);
        }
        39 => {
            let v = bus.read8(s) as u32;
            core.set_reg(b, r2, v);
        }
        40 => {
            let v = bus.read16(s) as u32;
            core.set_reg(b, r2, v);
        }
        41 => {
            let v = core.dread32(bus, s);
            core.set_reg(b, r2, v);
        }
        42 => {
            if core.kind.is_dsp() {
                // SAT32S — saturate 40-bit MAC to signed 32
                let res = core.mac.clamp(i32::MIN as i64, i32::MAX as i64) as u32;
                core.set_reg(b, r2, res);
                core.set_zn(res);
            } else {
                // LOADP — 64-bit: low long → Rd, high long → hidata
                core.set_reg(b, r2, bus.read32(s));
                core.hidata = bus.read32(s.wrapping_add(4));
            }
        }
        43 => {
            // LOAD (R14+n)
            let addr = core.reg(b, 14).wrapping_add(quick(r1) * 4);
            let v = core.dread32(bus, addr);
            core.set_reg(b, r2, v);
        }
        44 => {
            // LOAD (R15+n)
            let addr = core.reg(b, 15).wrapping_add(quick(r1) * 4);
            let v = core.dread32(bus, addr);
            core.set_reg(b, r2, v);
        }
        45 => {
            // STOREB — addr=reg1, data=reg2
            bus.write8(s, d as u8);
        }
        46 => {
            bus.write16(s, d as u16);
        }
        47 => {
            // STORE — addr=reg1, data=reg2
            core.dwrite32(bus, s, d);
        }
        48 => {
            if core.kind.is_dsp() {
                // MIRROR — bit-reverse Rd
                let res = d.reverse_bits();
                core.set_reg(b, r2, res);
                core.set_zn(res);
            } else {
                // STOREP — 64-bit: low from Rd, high from hidata
                bus.write32(s, d);
                bus.write32(s.wrapping_add(4), core.hidata);
            }
        }
        49 => {
            // STORE Rs,(R14+n) — data=reg1, index=reg2
            let addr = core.reg(b, 14).wrapping_add(quick(r2) * 4);
            core.dwrite32(bus, addr, s);
        }
        50 => {
            let addr = core.reg(b, 15).wrapping_add(quick(r2) * 4);
            core.dwrite32(bus, addr, s);
        }
        51 => {
            // MOVE PC,Rd — return this instruction's address (pc already +2)
            let v = core.pc.wrapping_sub(2);
            core.set_reg(b, r2, v);
        }
        52 => {
            // JUMP cc,(Rs) — cc in reg2, address reg in reg1; delay slot
            if cond(core, r2) {
                core.pending_jump = Some(s);
            }
        }
        53 => {
            // JR cc,n — cc in reg2, signed offset in reg1; delay slot
            if cond(core, r2) {
                let off = sext5(r1) * 2;
                core.pending_jump = Some(core.pc.wrapping_add(off as u32));
            }
        }
        54 => {
            // MMULT — systolic matrix multiply (bank-1 operand × local RAM)
            mmult(core, bus, r1, r2, b);
        }
        55 => {
            // MTOI — mantissa-to-integer from IEEE-754 float
            let res = mtoi(s);
            core.set_reg(b, r2, res);
            core.set_zn(res);
        }
        56 => {
            // NORMI — normalization shift count of S
            let res = normi(s);
            core.set_reg(b, r2, res);
            core.set_zn(res);
        }
        57 => { /* NOP */ }
        58 => {
            // LOAD (R14+Rn) — offset reg = reg1, dst = reg2
            let addr = core.reg(b, 14).wrapping_add(s);
            let v = core.dread32(bus, addr);
            core.set_reg(b, r2, v);
        }
        59 => {
            let addr = core.reg(b, 15).wrapping_add(s);
            let v = core.dread32(bus, addr);
            core.set_reg(b, r2, v);
        }
        60 => {
            // STORE Rs,(R14+Rd) — data=reg1, offset=reg2
            let addr = core.reg(b, 14).wrapping_add(d);
            core.dwrite32(bus, addr, s);
        }
        61 => {
            let addr = core.reg(b, 15).wrapping_add(d);
            core.dwrite32(bus, addr, s);
        }
        62 => {
            if core.kind.is_dsp() {
                // reserved on DSP — treat as NOP
            } else {
                // SAT24 → [0, 0xFFFFFF]
                let res = (d as i32).clamp(0, 0xFF_FFFF) as u32;
                core.set_reg(b, r2, res);
                core.set_z(res == 0);
                core.set_n(false);
            }
        }
        63 => {
            if core.kind.is_dsp() {
                // ADDQMOD
                let n = quick(r1);
                let r = d.wrapping_add(n) & core.modulo;
                core.set_reg(b, r2, r);
                core.set_zn(r);
            } else if r1 == 0 {
                // PACK: [25:22]→[15:12], [16:13]→[11:8], [7:0]→[7:0]
                let res = ((d >> 10) & 0xF000) | ((d >> 5) & 0x0F00) | (d & 0x00FF);
                core.set_reg(b, r2, res);
            } else {
                // UNPACK: [15:12]→[25:22], [11:8]→[16:13], [7:0]→[7:0]
                let res = ((d & 0xF000) << 10) | ((d & 0x0F00) << 5) | (d & 0x00FF);
                core.set_reg(b, r2, res);
            }
        }
        _ => {}
    }
}

/// Systolic matrix multiply. One operand is two packed 16-bit elements per
/// bank-1 register starting at `Rs`; the other is in local RAM at MTXADDR,
/// traversed by row or column per MTXC. MWIDTH terms accumulate into Rd.
fn mmult(core: &mut Risc, bus: &mut Bus, r1: usize, r2: usize, b: usize) {
    let width = (core.mtxc & 0xF) as usize; // MWIDTH 3..15
    let by_column = core.mtxc & 0x10 != 0;
    let mtxaddr = ((core.mtxa >> 2) & 0x3FF) << 2; // address into local RAM (bytes)
    let sram = core.kind.sram_base();
    let mut acc: i64 = 0;
    for i in 0..width {
        // bank-1 register operand: two 16-bit elements packed per reg.
        let reg_idx = r1 + i / 2;
        let regw = core.reg(1, reg_idx & 0x1F);
        let a = if i & 1 == 0 { regw & 0xFFFF } else { regw >> 16 } as i16 as i64;
        // local-RAM operand.
        let stride = if by_column { (width as u32) * 4 } else { 4 };
        let addr = sram + mtxaddr + (i as u32) * stride;
        let m = bus.read16(addr) as i16 as i64;
        acc += a * m;
    }
    core.mac = acc;
    let res = acc as u32;
    core.set_reg(b, r2, res);
    core.set_zn(res);
}

/// MTOI: mantissa-to-integer — extract the 24-bit mantissa of an IEEE-754
/// single in S, sign-extended from bit 23 (per TRM p.51, approximate).
fn mtoi(s: u32) -> u32 {
    let mant = s & 0x007F_FFFF | 0x0080_0000; // implied leading 1
    let signed = (s & 0x8000_0000) != 0;
    let v = ((mant << 8) as i32) >> 8; // sign-extend from bit 23
    if signed {
        (-(v as i64)) as u32
    } else {
        v as u32
    }
}

/// NORMI: normalization integer — the negative shift count to normalize S
/// (bring its most-significant set bit to bit 31).
fn normi(s: u32) -> u32 {
    if s == 0 {
        return 0;
    }
    let lead = s.leading_zeros() as i32;
    (-lead) as u32
}
