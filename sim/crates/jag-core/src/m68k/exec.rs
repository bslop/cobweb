//! 68000 opcode decode + execution. Dispatched by the top nibble (the classic
//! 68000 encoding groups). Descendant module of `m68k`, so it can use the
//! parent's private helpers (`do_add`, `ea_*`, flags, …).

use super::*;

#[inline]
fn size_from_bits(b: u16) -> Option<Size> {
    match b {
        0 => Some(Size::Byte),
        1 => Some(Size::Word),
        2 => Some(Size::Long),
        _ => None,
    }
}

#[inline]
fn sign_extend(v: u32, size: Size) -> u32 {
    match size {
        Size::Byte => v as u8 as i8 as i32 as u32,
        Size::Word => v as u16 as i16 as i32 as u32,
        Size::Long => v,
    }
}

impl M68k {
    pub(super) fn execute(&mut self, bus: &mut Bus, op: u16) -> u32 {
        match op >> 12 {
            0x0 => self.group0(bus, op),
            0x1 => self.move_inst(bus, op, Size::Byte),
            0x2 => self.move_inst(bus, op, Size::Long),
            0x3 => self.move_inst(bus, op, Size::Word),
            0x4 => self.group4(bus, op),
            0x5 => self.group5(bus, op),
            0x6 => self.branch(bus, op),
            0x7 => self.moveq(op),
            0x8 => self.group8(bus, op),
            0x9 => self.group_addsub(bus, op, false),
            0xB => self.groupb(bus, op),
            0xC => self.groupc(bus, op),
            0xD => self.group_addsub(bus, op, true),
            0xE => self.group_shift(bus, op),
            _ => self.illegal(bus, op),
        }
    }

    fn illegal(&mut self, bus: &mut Bus, op: u16) -> u32 {
        self.last_illegal = Some(self.pc.wrapping_sub(2));
        self.last_illegal_op = op;
        self.illegal_count += 1;
        // Line-A / Line-F have dedicated vectors; everything else → vector 4.
        let vector = match op >> 12 {
            0xA => 10,
            0xF => 11,
            _ => 4,
        };
        self.exception(bus, vector, false)
    }

    // ── 0x0: immediates + bit operations ─────────────────────────────────────
    fn group0(&mut self, bus: &mut Bus, op: u16) -> u32 {
        let mode = (op >> 3) & 7;
        let reg = op & 7;
        // Bit ops with a static bit number, or the special CCR/SR immediates.
        let opc = (op >> 9) & 7;
        let sizebits = (op >> 6) & 3;

        // BTST/BCHG/BCLR/BSET with immediate bit number: 0000 1000 ssmmm rrr
        if (op & 0xFF00) == 0x0800 {
            let bitnum = self.fetch16(bus) & 0xFF;
            return self.bitop(bus, mode, reg, (op >> 6) & 3, bitnum as u32);
        }
        // Dynamic bit ops: 0000 ddd1 ttmmm rrr (handled in groupc style? no) —
        // bit ops with bit number in Dn have bit8 set.
        if op & 0x0100 != 0 && mode != 1 {
            let dn = ((op >> 9) & 7) as usize;
            let bitnum = self.d[dn];
            return self.bitop(bus, mode, reg, (op >> 6) & 3, bitnum);
        }

        let size = match size_from_bits(sizebits) {
            Some(s) => s,
            None => return self.illegal(bus, op),
        };

        // Immediate source.
        let imm = match size {
            Size::Byte => (self.fetch16(bus) & 0xFF) as u32,
            Size::Word => self.fetch16(bus) as u32,
            Size::Long => self.fetch32(bus),
        };

        // ANDI/ORI/EORI to CCR (#imm,CCR) / to SR (#imm,SR): mode=7 reg=4.
        if mode == 7 && reg == 4 {
            return self.imm_to_sr_ccr(opc, size, imm, bus, op);
        }

        let ea = self.decode_ea(bus, mode, reg, size);
        let d = self.ea_read(bus, ea, size);
        match opc {
            0 => {
                // ORI
                let r = (d | imm) & size.mask();
                self.do_logic_flags(r, size);
                self.ea_write(bus, ea, size, r);
            }
            1 => {
                // ANDI
                let r = (d & imm) & size.mask();
                self.do_logic_flags(r, size);
                self.ea_write(bus, ea, size, r);
            }
            2 => {
                // SUBI
                let r = self.do_sub(imm, d, size, false);
                self.ea_write(bus, ea, size, r);
            }
            3 => {
                // ADDI
                let r = self.do_add(imm, d, size, false);
                self.ea_write(bus, ea, size, r);
            }
            5 => {
                // EORI
                let r = (d ^ imm) & size.mask();
                self.do_logic_flags(r, size);
                self.ea_write(bus, ea, size, r);
            }
            6 => {
                // CMPI
                self.do_cmp(imm, d, size);
            }
            _ => return self.illegal(bus, op),
        }
        12
    }

    fn imm_to_sr_ccr(&mut self, opc: u16, size: Size, imm: u32, bus: &mut Bus, op: u16) -> u32 {
        let to_sr = size == Size::Word;
        if to_sr && !self.supervisor() {
            return self.exception(bus, 8, false); // privilege violation
        }
        let cur = if to_sr { self.sr } else { self.sr & 0x00FF } as u32;
        let r = match opc {
            0 => cur | imm,         // ORI
            1 => cur & imm,         // ANDI
            5 => cur ^ imm,         // EORI
            _ => return self.illegal(bus, op),
        };
        if to_sr {
            self.set_sr(r as u16);
        } else {
            self.sr = (self.sr & 0xFF00) | (r as u16 & 0x00FF);
        }
        20
    }

    fn bitop(&mut self, bus: &mut Bus, mode: u16, reg: u16, kind: u16, bitnum: u32) -> u32 {
        // On Dn the bit number is mod 32 (long); on memory mod 8 (byte).
        let size = if mode == 0 { Size::Long } else { Size::Byte };
        let bit = if mode == 0 { bitnum & 31 } else { bitnum & 7 };
        let ea = self.decode_ea(bus, mode, reg, size);
        let v = self.ea_read(bus, ea, size);
        let mask = 1u32 << bit;
        self.set_flag(FLAG_Z, v & mask == 0);
        let nv = match kind {
            0 => return 6, // BTST: test only
            1 => v ^ mask, // BCHG
            2 => v & !mask, // BCLR
            3 => v | mask, // BSET
            _ => v,
        };
        self.ea_write(bus, ea, size, nv);
        8
    }

    // ── MOVE / MOVEA ─────────────────────────────────────────────────────────
    fn move_inst(&mut self, bus: &mut Bus, op: u16, size: Size) -> u32 {
        let src_mode = (op >> 3) & 7;
        let src_reg = op & 7;
        let dst_mode = (op >> 6) & 7;
        let dst_reg = (op >> 9) & 7;

        let sea = self.decode_ea(bus, src_mode, src_reg, size);
        let v = self.ea_read(bus, sea, size);

        if dst_mode == 1 {
            // MOVEA: word source sign-extends to 32; no flags.
            let val = sign_extend(v, size);
            self.a[dst_reg as usize] = val;
            return 4;
        }
        let dea = self.decode_ea(bus, dst_mode, dst_reg, size);
        self.ea_write(bus, dea, size, v);
        self.do_logic_flags(v, size); // MOVE sets N,Z; clears V,C
        8
    }

    fn moveq(&mut self, op: u16) -> u32 {
        let reg = ((op >> 9) & 7) as usize;
        let v = (op & 0xFF) as i8 as i32 as u32;
        self.d[reg] = v;
        self.set_flag(FLAG_C, false);
        self.set_flag(FLAG_V, false);
        self.set_nz(v, Size::Long);
        4
    }

    // ── 0x4: misc (densely encoded — priority-ordered mask cascade) ──────────
    fn group4(&mut self, bus: &mut Bus, op: u16) -> u32 {
        // 0x4Exx control instructions (TRAP/LINK/JMP/JSR/RTS/RTE/...).
        if op & 0xFF00 == 0x4E00 {
            return self.group4_e(bus, op);
        }
        let mode = (op >> 3) & 7;
        let reg = op & 7;
        let sizebits = (op >> 6) & 3;

        // LEA An,<ea>
        if op & 0xF1C0 == 0x41C0 {
            let an = ((op >> 9) & 7) as usize;
            let ea = self.decode_ea(bus, mode, reg, Size::Long);
            if let Ea::Mem(a) = ea {
                self.a[an] = a;
            }
            return 4;
        }
        // CHK Dn,<ea> (word)
        if op & 0xF1C0 == 0x4180 {
            let dn = ((op >> 9) & 7) as usize;
            let ea = self.decode_ea(bus, mode, reg, Size::Word);
            let bound = self.ea_read(bus, ea, Size::Word) as i16 as i32;
            let v = self.d[dn] as i16 as i32;
            self.set_flag(FLAG_Z, v == 0);
            if v < 0 || v > bound {
                self.set_flag(FLAG_N, v < 0);
                return self.exception(bus, 6, false);
            }
            return 10;
        }
        // SWAP Dn (must precede PEA, which shares the 0x4840 prefix).
        if op & 0xFFF8 == 0x4840 {
            let r = (op & 7) as usize;
            let v = self.d[r].rotate_left(16);
            self.d[r] = v;
            self.set_flag(FLAG_C, false);
            self.set_flag(FLAG_V, false);
            self.set_nz(v, Size::Long);
            return 4;
        }
        // PEA <ea>
        if op & 0xFFC0 == 0x4840 {
            let ea = self.decode_ea(bus, mode, reg, Size::Long);
            if let Ea::Mem(a) = ea {
                self.push32(bus, a);
            }
            return 12;
        }
        // EXT.W / EXT.L (must precede MOVEM, which shares 0x4880/0x48C0).
        if op & 0xFFF8 == 0x4880 {
            let r = (op & 7) as usize;
            let ext = self.d[r] as u8 as i8 as i16 as u16 as u32;
            self.d[r] = (self.d[r] & 0xFFFF_0000) | (ext & 0xFFFF);
            self.set_flag(FLAG_C, false);
            self.set_flag(FLAG_V, false);
            self.set_nz(self.d[r], Size::Word);
            return 4;
        }
        if op & 0xFFF8 == 0x48C0 {
            let r = (op & 7) as usize;
            self.d[r] = self.d[r] as u16 as i16 as i32 as u32;
            self.set_flag(FLAG_C, false);
            self.set_flag(FLAG_V, false);
            self.set_nz(self.d[r], Size::Long);
            return 4;
        }
        // MOVEM regs→mem / mem→regs.
        if op & 0xFF80 == 0x4880 {
            return self.movem(bus, op, false);
        }
        if op & 0xFF80 == 0x4C80 {
            return self.movem(bus, op, true);
        }
        // MOVE from SR / to CCR / to SR.
        if op & 0xFFC0 == 0x40C0 || op & 0xFFC0 == 0x44C0 || op & 0xFFC0 == 0x46C0 {
            return self.move_sr_ccr(bus, op);
        }
        // ILLEGAL / TAS / TST.
        if op == 0x4AFC {
            return self.illegal(bus, op);
        }
        if op & 0xFFC0 == 0x4AC0 {
            let ea = self.decode_ea(bus, mode, reg, Size::Byte);
            let v = self.ea_read(bus, ea, Size::Byte);
            self.set_nz(v, Size::Byte);
            self.set_flag(FLAG_V, false);
            self.set_flag(FLAG_C, false);
            self.ea_write(bus, ea, Size::Byte, v | 0x80);
            return 10;
        }
        if op & 0xFF00 == 0x4A00 {
            let size = match size_from_bits(sizebits) {
                Some(s) => s,
                None => return self.illegal(bus, op),
            };
            let ea = self.decode_ea(bus, mode, reg, size);
            let v = self.ea_read(bus, ea, size);
            self.do_logic_flags(v, size);
            return 4;
        }
        // NEGX / CLR / NEG / NOT.
        let kind = (op >> 8) & 0xF;
        if matches!(kind, 0x0 | 0x2 | 0x4 | 0x6) {
            let size = match size_from_bits(sizebits) {
                Some(s) => s,
                None => return self.illegal(bus, op),
            };
            let ea = self.decode_ea(bus, mode, reg, size);
            match kind {
                0x0 => {
                    let d = self.ea_read(bus, ea, size);
                    let r = self.do_sub(d, 0, size, true);
                    self.ea_write(bus, ea, size, r);
                }
                0x2 => {
                    self.ea_write(bus, ea, size, 0);
                    self.set_flag(FLAG_N, false);
                    self.set_flag(FLAG_Z, true);
                    self.set_flag(FLAG_V, false);
                    self.set_flag(FLAG_C, false);
                }
                0x4 => {
                    let d = self.ea_read(bus, ea, size);
                    let r = self.do_sub(d, 0, size, false);
                    self.ea_write(bus, ea, size, r);
                }
                0x6 => {
                    let d = self.ea_read(bus, ea, size);
                    let r = (!d) & size.mask();
                    self.do_logic_flags(r, size);
                    self.ea_write(bus, ea, size, r);
                }
                _ => unreachable!(),
            }
            return 8;
        }
        self.illegal(bus, op)
    }

    fn move_sr_ccr(&mut self, bus: &mut Bus, op: u16) -> u32 {
        let mode = (op >> 3) & 7;
        let reg = op & 7;
        match (op >> 9) & 7 {
            0 => {
                // MOVE from SR (word)
                let ea = self.decode_ea(bus, mode, reg, Size::Word);
                let sr = self.sr;
                self.ea_write(bus, ea, Size::Word, sr as u32);
            }
            2 => {
                // MOVE to CCR (word source, low byte)
                let ea = self.decode_ea(bus, mode, reg, Size::Word);
                let v = self.ea_read(bus, ea, Size::Word) as u16;
                self.sr = (self.sr & 0xFF00) | (v & 0x00FF);
            }
            3 => {
                // MOVE to SR (privileged)
                if !self.supervisor() {
                    return self.exception(bus, 8, false);
                }
                let ea = self.decode_ea(bus, mode, reg, Size::Word);
                let v = self.ea_read(bus, ea, Size::Word) as u16;
                self.set_sr(v);
            }
            _ => return self.illegal(bus, op),
        }
        12
    }

    fn group4_e(&mut self, bus: &mut Bus, op: u16) -> u32 {
        // 0100 111x ...: TRAP, LINK, UNLK, MOVE USP, RESET, NOP, STOP, RTE,
        // RTS, TRAPV, RTR, JSR, JMP, MOVEM mem→reg.
        match op {
            0x4E70 => 132, // RESET (no-op on our model)
            0x4E71 => 4,   // NOP
            0x4E72 => {
                // STOP #imm
                if !self.supervisor() {
                    return self.exception(bus, 8, false);
                }
                let imm = self.fetch16(bus);
                self.set_sr(imm);
                self.stopped = true;
                4
            }
            0x4E73 => {
                // RTE (privileged)
                if !self.supervisor() {
                    return self.exception(bus, 8, false);
                }
                let sr = self.pop16(bus);
                let pc = self.pop32(bus);
                self.set_sr(sr);
                self.pc = pc;
                20
            }
            0x4E75 => {
                // RTS
                self.pc = self.pop32(bus);
                16
            }
            0x4E76 => {
                // TRAPV
                if self.flag(FLAG_V) {
                    return self.exception(bus, 7, false);
                }
                4
            }
            0x4E77 => {
                // RTR
                let ccr = self.pop16(bus);
                self.sr = (self.sr & 0xFF00) | (ccr & 0x00FF);
                self.pc = self.pop32(bus);
                20
            }
            _ => {
                if op & 0xFFF0 == 0x4E40 {
                    // TRAP #n
                    let n = (op & 0xF) as u32;
                    return self.exception(bus, 32 + n, false);
                }
                if op & 0xFFF8 == 0x4E50 {
                    // LINK An,#disp
                    let an = (op & 7) as usize;
                    let disp = self.fetch16(bus) as i16 as i32 as u32;
                    self.push32(bus, self.a[an]);
                    self.a[an] = self.a[7];
                    self.a[7] = self.a[7].wrapping_add(disp);
                    return 16;
                }
                if op & 0xFFF8 == 0x4E58 {
                    // UNLK An
                    let an = (op & 7) as usize;
                    self.a[7] = self.a[an];
                    self.a[an] = self.pop32(bus);
                    return 12;
                }
                if op & 0xFFF0 == 0x4E60 {
                    // MOVE USP (privileged)
                    if !self.supervisor() {
                        return self.exception(bus, 8, false);
                    }
                    let an = (op & 7) as usize;
                    if op & 0x8 != 0 {
                        self.a[an] = self.usp; // USP → An
                    } else {
                        self.usp = self.a[an]; // An → USP
                    }
                    return 4;
                }
                let mode = (op >> 3) & 7;
                let reg = op & 7;
                if op & 0xFFC0 == 0x4E80 {
                    // JSR
                    let ea = self.decode_ea(bus, mode, reg, Size::Long);
                    if let Ea::Mem(a) = ea {
                        self.push32(bus, self.pc);
                        self.pc = a;
                    }
                    return 16;
                }
                if op & 0xFFC0 == 0x4EC0 {
                    // JMP
                    let ea = self.decode_ea(bus, mode, reg, Size::Long);
                    if let Ea::Mem(a) = ea {
                        self.pc = a;
                    }
                    return 8;
                }
                // LEA (0100 ddd1 11mmm rrr)
                if op & 0xF1C0 == 0x41C0 {
                    let an = ((op >> 9) & 7) as usize;
                    let ea = self.decode_ea(bus, mode, reg, Size::Long);
                    if let Ea::Mem(a) = ea {
                        self.a[an] = a;
                    }
                    return 4;
                }
                // MOVEM memory→registers (0100 1100 1ssm mmrrr, dr=1)
                if op & 0xFB80 == 0x4880 {
                    return self.movem(bus, op, true);
                }
                self.illegal(bus, op)
            }
        }
    }

    fn movem(&mut self, bus: &mut Bus, op: u16, to_regs: bool) -> u32 {
        let size = if op & 0x40 != 0 { Size::Long } else { Size::Word };
        let mode = (op >> 3) & 7;
        let reg = (op & 7) as usize;
        let list = self.fetch16(bus);
        let step = size.bytes();
        let mut count = 0u32;

        if to_regs {
            // (An)+ or control mode; list order D0..D7,A0..A7 ascending.
            let mut addr = match mode {
                3 => self.a[reg],
                _ => match self.decode_ea(bus, mode, reg as u16, size) {
                    Ea::Mem(a) => a,
                    _ => return self.illegal(bus, op),
                },
            };
            for i in 0..16 {
                if list & (1 << i) != 0 {
                    let v = match size {
                        Size::Long => bus.read32(addr),
                        _ => bus.read16(addr) as i16 as i32 as u32,
                    };
                    if i < 8 {
                        self.d[i] = v;
                    } else {
                        self.a[i - 8] = v;
                    }
                    addr = addr.wrapping_add(step);
                    count += 1;
                }
            }
            if mode == 3 {
                self.a[reg] = addr;
            }
        } else if mode == 4 {
            // -(An): registers stored A7..A0,D7..D0 (reversed), predecrement.
            let mut addr = self.a[reg];
            for i in 0..16 {
                if list & (1 << i) != 0 {
                    // bit i (0..15) selects, in -(An) mode, register
                    // A7..A0,D7..D0 for i=0..15.
                    let val = if i < 8 { self.a[7 - i] } else { self.d[15 - i] };
                    addr = addr.wrapping_sub(step);
                    match size {
                        Size::Long => bus.write32(addr, val),
                        _ => bus.write16(addr, val as u16),
                    }
                    count += 1;
                }
            }
            self.a[reg] = addr;
        } else {
            // control mode, registers D0..D7,A0..A7 ascending.
            let mut addr = match self.decode_ea(bus, mode, reg as u16, size) {
                Ea::Mem(a) => a,
                _ => return self.illegal(bus, op),
            };
            for i in 0..16 {
                if list & (1 << i) != 0 {
                    let val = if i < 8 { self.d[i] } else { self.a[i - 8] };
                    match size {
                        Size::Long => bus.write32(addr, val),
                        _ => bus.write16(addr, val as u16),
                    }
                    addr = addr.wrapping_add(step);
                    count += 1;
                }
            }
        }
        8 + count * if size == Size::Long { 8 } else { 4 }
    }

    // ── 0x5: ADDQ/SUBQ, Scc, DBcc ────────────────────────────────────────────
    fn group5(&mut self, bus: &mut Bus, op: u16) -> u32 {
        let sizebits = (op >> 6) & 3;
        let mode = (op >> 3) & 7;
        let reg = op & 7;
        if sizebits == 3 {
            // Scc or DBcc
            let cc = (op >> 8) & 0xF;
            if mode == 1 {
                // DBcc Dn,disp
                let disp = self.fetch16(bus) as i16 as i32;
                if !self.cond(cc) {
                    let dn = reg as usize;
                    let lo = (self.d[dn] as u16).wrapping_sub(1);
                    self.d[dn] = (self.d[dn] & 0xFFFF0000) | lo as u32;
                    if lo != 0xFFFF {
                        self.pc = self.pc.wrapping_sub(2).wrapping_add(disp as u32);
                        return 10;
                    }
                }
                return 12;
            }
            // Scc
            let ea = self.decode_ea(bus, mode, reg, Size::Byte);
            let v = if self.cond(cc) { 0xFF } else { 0x00 };
            self.ea_write(bus, ea, Size::Byte, v);
            return 8;
        }
        // ADDQ / SUBQ
        let size = size_from_bits(sizebits).unwrap();
        let mut data = ((op >> 9) & 7) as u32;
        if data == 0 {
            data = 8;
        }
        let is_sub = op & 0x0100 != 0;
        // On An, the whole long is affected and no flags change.
        if mode == 1 {
            let an = reg as usize;
            self.a[an] = if is_sub {
                self.a[an].wrapping_sub(data)
            } else {
                self.a[an].wrapping_add(data)
            };
            return 8;
        }
        let ea = self.decode_ea(bus, mode, reg, size);
        let d = self.ea_read(bus, ea, size);
        let r = if is_sub {
            self.do_sub(data, d, size, false)
        } else {
            self.do_add(data, d, size, false)
        };
        self.ea_write(bus, ea, size, r);
        8
    }

    // ── 0x6: Bcc / BRA / BSR ─────────────────────────────────────────────────
    fn branch(&mut self, bus: &mut Bus, op: u16) -> u32 {
        let cc = (op >> 8) & 0xF;
        let disp8 = (op & 0xFF) as i8;
        let base = self.pc;
        let target = if disp8 == 0 {
            let d = self.fetch16(bus) as i16 as i32;
            base.wrapping_add(d as u32)
        } else {
            // Note: 68000 has no .l form; disp8 == -1 (0xFF) is a real -1 byte
            // displacement → odd target → address error (the `bsr.l` bug).
            base.wrapping_add(disp8 as i32 as u32)
        };
        match cc {
            0 => {
                // BRA
                self.pc = target;
                10
            }
            1 => {
                // BSR
                self.push32(bus, self.pc);
                self.pc = target;
                18
            }
            _ => {
                if self.cond(cc) {
                    self.pc = target;
                    10
                } else {
                    8
                }
            }
        }
    }

    // ── 0x8: OR / DIVU / DIVS ────────────────────────────────────────────────
    fn group8(&mut self, bus: &mut Bus, op: u16) -> u32 {
        let dn = ((op >> 9) & 7) as usize;
        let mode = (op >> 3) & 7;
        let reg = op & 7;
        let opmode = (op >> 6) & 7;
        match opmode {
            // DIVU.w (3) / DIVS.w (7): 32 ÷ 16 → quotient:remainder in Dn.
            3 | 7 => {
                let ea = self.decode_ea(bus, mode, reg, Size::Word);
                let divisor = self.ea_read(bus, ea, Size::Word) & 0xFFFF;
                if divisor == 0 {
                    return self.exception(bus, 5, false);
                }
                let dividend = self.d[dn];
                let signed = opmode == 7;
                let (quot, rem, overflow) = if signed {
                    let q = (dividend as i32) / (divisor as i16 as i32);
                    let r = (dividend as i32) % (divisor as i16 as i32);
                    (q as u32, r as u32, q < -32768 || q > 32767)
                } else {
                    let q = dividend / divisor;
                    let r = dividend % divisor;
                    (q, r, q > 0xFFFF)
                };
                if overflow {
                    self.set_flag(FLAG_V, true);
                    return 78;
                }
                self.d[dn] = ((rem & 0xFFFF) << 16) | (quot & 0xFFFF);
                self.set_flag(FLAG_C, false);
                self.set_flag(FLAG_V, false);
                self.set_nz(quot & 0xFFFF, Size::Word);
                140
            }
            _ => {
                // OR
                let size = match size_from_bits(opmode & 3) {
                    Some(s) => s,
                    None => return self.illegal(bus, op),
                };
                let to_ea = opmode & 4 != 0;
                let ea = self.decode_ea(bus, mode, reg, size);
                if to_ea {
                    let s = self.d[dn];
                    let d = self.ea_read(bus, ea, size);
                    let r = (s | d) & size.mask();
                    self.do_logic_flags(r, size);
                    self.ea_write(bus, ea, size, r);
                } else {
                    let s = self.ea_read(bus, ea, size);
                    let r = (self.d[dn] | s) & size.mask();
                    self.do_logic_flags(r, size);
                    self.d[dn] = (self.d[dn] & !size.mask()) | r;
                }
                6
            }
        }
    }

    // ── 0x9 (SUB) / 0xD (ADD), incl. An and X variants ───────────────────────
    fn group_addsub(&mut self, bus: &mut Bus, op: u16, is_add: bool) -> u32 {
        let dn = ((op >> 9) & 7) as usize;
        let mode = (op >> 3) & 7;
        let reg = op & 7;
        let opmode = (op >> 6) & 7;

        // ADDA/SUBA: opmode 3 (word) or 7 (long).
        if opmode == 3 || opmode == 7 {
            let size = if opmode == 3 { Size::Word } else { Size::Long };
            let ea = self.decode_ea(bus, mode, reg, size);
            let s = sign_extend(self.ea_read(bus, ea, size), size);
            self.a[dn] = if is_add {
                self.a[dn].wrapping_add(s)
            } else {
                self.a[dn].wrapping_sub(s)
            };
            return 8;
        }

        let size = match size_from_bits(opmode & 3) {
            Some(s) => s,
            None => return self.illegal(bus, op),
        };
        let to_ea = opmode & 4 != 0;
        let ea = self.decode_ea(bus, mode, reg, size);
        if to_ea {
            let s = self.d[dn];
            let d = self.ea_read(bus, ea, size);
            let r = if is_add {
                self.do_add(s, d, size, false)
            } else {
                self.do_sub(s, d, size, false)
            };
            self.ea_write(bus, ea, size, r);
        } else {
            let s = self.ea_read(bus, ea, size);
            let d = self.d[dn];
            let r = if is_add {
                self.do_add(s, d, size, false)
            } else {
                self.do_sub(s, d, size, false)
            };
            self.d[dn] = (self.d[dn] & !size.mask()) | (r & size.mask());
        }
        6
    }

    // ── 0xB: CMP / CMPA / CMPM / EOR ─────────────────────────────────────────
    fn groupb(&mut self, bus: &mut Bus, op: u16) -> u32 {
        let dn = ((op >> 9) & 7) as usize;
        let mode = (op >> 3) & 7;
        let reg = op & 7;
        let opmode = (op >> 6) & 7;

        // CMPA: opmode 3 (word) / 7 (long).
        if opmode == 3 || opmode == 7 {
            let size = if opmode == 3 { Size::Word } else { Size::Long };
            let ea = self.decode_ea(bus, mode, reg, size);
            let s = sign_extend(self.ea_read(bus, ea, size), size);
            self.do_cmp(s, self.a[dn], Size::Long);
            return 6;
        }
        let size = size_from_bits(opmode & 3).unwrap();
        if opmode & 4 != 0 {
            // EOR (to EA) or CMPM (mode 1).
            if mode == 1 {
                // CMPM (Ay)+,(Ax)+
                let ay = reg as usize;
                let ax = dn;
                let sea = self.decode_ea(bus, 3, ay as u16, size);
                let dea = self.decode_ea(bus, 3, ax as u16, size);
                let s = self.ea_read(bus, sea, size);
                let d = self.ea_read(bus, dea, size);
                self.do_cmp(s, d, size);
                return 12;
            }
            let ea = self.decode_ea(bus, mode, reg, size);
            let d = self.ea_read(bus, ea, size);
            let r = (d ^ self.d[dn]) & size.mask();
            self.do_logic_flags(r, size);
            self.ea_write(bus, ea, size, r);
            8
        } else {
            // CMP Dn
            let ea = self.decode_ea(bus, mode, reg, size);
            let s = self.ea_read(bus, ea, size);
            self.do_cmp(s, self.d[dn], size);
            6
        }
    }

    // ── 0xC: AND / MULU / MULS / EXG / ABCD ──────────────────────────────────
    fn groupc(&mut self, bus: &mut Bus, op: u16) -> u32 {
        let dn = ((op >> 9) & 7) as usize;
        let mode = (op >> 3) & 7;
        let reg = op & 7;
        let opmode = (op >> 6) & 7;

        match opmode {
            // MULU.w (3) / MULS.w (7): 16 × 16 → 32 in Dn.
            3 | 7 => {
                let ea = self.decode_ea(bus, mode, reg, Size::Word);
                let s = self.ea_read(bus, ea, Size::Word) & 0xFFFF;
                let d = self.d[dn] & 0xFFFF;
                let r = if opmode == 7 {
                    ((s as i16 as i32) * (d as i16 as i32)) as u32
                } else {
                    s * d
                };
                self.d[dn] = r;
                self.set_flag(FLAG_C, false);
                self.set_flag(FLAG_V, false);
                self.set_nz(r, Size::Long);
                74
            }
            _ => {
                // EXG (opmode 5 with specific sub-encodings) else AND.
                if opmode == 5 && (mode == 0 || mode == 1) {
                    // EXG Dx,Dy ($C140) / EXG Ax,Ay ($C148)
                    let rx = dn;
                    let ry = reg as usize;
                    if mode == 0 {
                        self.d.swap(rx, ry);
                    } else {
                        self.a.swap(rx, ry);
                    }
                    return 6;
                }
                if opmode == 6 && mode == 1 {
                    // EXG Dx,Ay ($C188)
                    let dx = dn;
                    let ay = reg as usize;
                    std::mem::swap(&mut self.d[dx], &mut self.a[ay]);
                    return 6;
                }
                let size = match size_from_bits(opmode & 3) {
                    Some(s) => s,
                    None => return self.illegal(bus, op),
                };
                let to_ea = opmode & 4 != 0;
                let ea = self.decode_ea(bus, mode, reg, size);
                if to_ea {
                    let s = self.d[dn];
                    let d = self.ea_read(bus, ea, size);
                    let r = (s & d) & size.mask();
                    self.do_logic_flags(r, size);
                    self.ea_write(bus, ea, size, r);
                } else {
                    let s = self.ea_read(bus, ea, size);
                    let r = (self.d[dn] & s) & size.mask();
                    self.do_logic_flags(r, size);
                    self.d[dn] = (self.d[dn] & !size.mask()) | r;
                }
                6
            }
        }
    }

    // ── 0xE: shifts / rotates ────────────────────────────────────────────────
    fn group_shift(&mut self, bus: &mut Bus, op: u16) -> u32 {
        let sizebits = (op >> 6) & 3;
        if sizebits == 3 {
            // Memory shift by 1 (word only).
            let mode = (op >> 3) & 7;
            let reg = op & 7;
            let kind = (op >> 9) & 7;
            let left = op & 0x0100 != 0;
            let ea = self.decode_ea(bus, mode, reg, Size::Word);
            let v = self.ea_read(bus, ea, Size::Word);
            let r = self.shift_one(v, Size::Word, kind & 3, left);
            self.ea_write(bus, ea, Size::Word, r);
            return 8;
        }
        let size = size_from_bits(sizebits).unwrap();
        let reg = (op & 7) as usize;
        let left = op & 0x0100 != 0;
        let kind = (op >> 3) & 3;
        let ir = (op >> 5) & 1; // 0 = immediate count, 1 = Dn count
        let cnt_field = ((op >> 9) & 7) as u32;
        let count = if ir == 0 {
            if cnt_field == 0 {
                8
            } else {
                cnt_field
            }
        } else {
            self.d[cnt_field as usize] & 63
        };
        let mut v = self.d[reg] & size.mask();
        for _ in 0..count {
            v = self.shift_one(v, size, kind, left);
        }
        if count == 0 {
            // Count 0: flags from value, C cleared (ASL/LSL etc.).
            self.set_flag(FLAG_C, false);
            self.set_flag(FLAG_V, false);
            self.set_nz(v, size);
        } else {
            self.set_nz(v, size);
        }
        self.d[reg] = (self.d[reg] & !size.mask()) | (v & size.mask());
        6 + 2 * count
    }

    /// One shift/rotate step (kind: 0=AS,1=LS,2=ROX,3=RO), updating C/X/V.
    fn shift_one(&mut self, v: u32, size: Size, kind: u16, left: bool) -> u32 {
        let m = size.mask();
        let msb = size.msb();
        let v = v & m;
        let (res, carry) = match (kind, left) {
            (0, true) | (1, true) => {
                // ASL / LSL
                let c = v & msb != 0;
                ((v << 1) & m, c)
            }
            (0, false) => {
                // ASR (arithmetic)
                let c = v & 1 != 0;
                let sign = v & msb;
                ((v >> 1) | sign, c)
            }
            (1, false) => {
                // LSR
                let c = v & 1 != 0;
                (v >> 1, c)
            }
            (3, true) => {
                // ROL
                let c = v & msb != 0;
                (((v << 1) | if c { 1 } else { 0 }) & m, c)
            }
            (3, false) => {
                // ROR
                let c = v & 1 != 0;
                ((v >> 1) | if c { msb } else { 0 }, c)
            }
            (2, true) => {
                // ROXL through X
                let xin = if self.flag(FLAG_X) { 1 } else { 0 };
                let c = v & msb != 0;
                (((v << 1) | xin) & m, c)
            }
            (2, false) => {
                // ROXR through X
                let xin = if self.flag(FLAG_X) { msb } else { 0 };
                let c = v & 1 != 0;
                ((v >> 1) | xin, c)
            }
            _ => (v, false),
        };
        self.set_flag(FLAG_C, carry);
        if kind != 3 {
            // ROx/ASx/LSx update X; pure rotates (kind 3) leave X unchanged.
            self.set_flag(FLAG_X, carry);
        }
        // V is set on ASL only when the sign bit changes; cleared otherwise.
        if kind == 0 && left {
            self.set_flag(FLAG_V, (res ^ v) & msb != 0);
        } else {
            self.set_flag(FLAG_V, false);
        }
        res
    }
}
