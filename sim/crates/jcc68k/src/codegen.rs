//! 68000 code generator. Stack-machine style: `gen_expr` leaves its result in
//! D0, `gen_addr` leaves an lvalue address in A0, binary ops push/pop the left
//! operand through the stack. Correct first, fast later (a register allocator
//! and peephole pass are the follow-up).
//!
//! Calling convention (jcc68k ABI): arguments pushed right-to-left as 32-bit
//! longs, caller cleans the stack; return value in D0; A6 is the frame pointer
//! (LINK/UNLK); D2-D7/A2-A5 are callee-saved; D0-D1/A0-A1 are caller-saved.
//! The 68000 lacks 32-bit MUL/DIV, so those lower to `__mulsi3`/`__divsi3`/
//! `__udivsi3`/`__modsi3`/`__umodsi3` runtime calls (see `runtime()`).

use crate::ast::*;
use std::collections::HashMap;
use std::fmt::Write;

pub struct Gen {
    out: String,
    label: usize,
    /// Per-translation-unit tag making string-pool labels unique across objects
    /// (they're referenced from code but defined in `.data`, so they can't be
    /// function-scoped `.L` locals). Derived from a hash of the unit's content.
    str_prefix: String,
    // per-function
    frame: HashMap<String, i32>,
    types: HashMap<String, Type>,
    globals: HashMap<String, Type>,
    ret_label: String,
    break_labels: Vec<String>,
    cont_labels: Vec<String>,
    /// Evaluation-stack depth for data temporaries (operands of an outer op held
    /// while an inner one is evaluated). Values live in callee-saved d2–d7 (which
    /// survive calls and runtime helpers), spilling to the stack past depth 6.
    dtemp: usize,
    /// Same, for address temporaries — callee-saved a2–a5, spilling past depth 4.
    atemp: usize,
}

/// Callee-saved data registers used as the expression eval stack (they survive
/// function calls and the runtime helpers, which all preserve d2–d7).
const DTEMP_REGS: &[&str] = &["d2", "d3", "d4", "d5", "d6", "d7"];
/// Callee-saved address registers for held lvalue addresses.
const ATEMP_REGS: &[&str] = &["a2", "a3", "a4", "a5"];

/// A short, stable per-unit tag from the program's strings and symbol names, so
/// two distinct translation units don't emit colliding string-pool labels.
fn unit_tag(prog: &Program) -> String {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    for s in &prog.strings {
        s.hash(&mut h);
    }
    for f in &prog.functions {
        f.name.hash(&mut h);
    }
    for g in &prog.globals {
        g.name.hash(&mut h);
    }
    format!("{:08x}", h.finish() as u32)
}

pub fn generate(prog: &Program) -> Result<String, String> {
    let mut g = Gen {
        out: String::new(),
        label: 0,
        str_prefix: format!("__str_{}", unit_tag(prog)),
        frame: HashMap::new(),
        types: HashMap::new(),
        globals: HashMap::new(),
        ret_label: String::new(),
        break_labels: Vec::new(),
        cont_labels: Vec::new(),
        dtemp: 0,
        atemp: 0,
    };
    for gl in &prog.globals {
        g.globals.insert(gl.name.clone(), gl.ty.clone());
    }
    for f in &prog.functions {
        g.globals.insert(f.name.clone(), t_int());
    }
    g.emit_prelude();
    for f in &prog.functions {
        g.gen_function(f)?;
    }
    g.emit_data(prog);
    Ok(peephole(&g.out))
}

impl Gen {
    fn l(&mut self) -> usize {
        self.label += 1;
        self.label
    }
    fn line(&mut self, s: &str) {
        self.out.push('\t');
        self.out.push_str(s);
        self.out.push('\n');
    }
    fn lbl(&mut self, s: &str) {
        writeln!(self.out, "{s}:").unwrap();
    }

    /// Save D0 (an operand of an outer expression) onto the register eval stack,
    /// returning the slot spelling. Uses a callee-saved data register while any
    /// remain, else the machine stack. Pair with [`pop_dtemp_to`].
    fn push_dtemp(&mut self) -> String {
        let slot = DTEMP_REGS.get(self.dtemp).map(|r| r.to_string()).unwrap_or_else(|| "-(a7)".into());
        self.line(&format!("move.l d0,{slot}"));
        self.dtemp += 1;
        slot
    }
    /// Restore a value saved by [`push_dtemp`] into `dst` (usually `d1`).
    fn pop_dtemp_to(&mut self, slot: &str, dst: &str) {
        self.dtemp -= 1;
        if slot == "-(a7)" {
            self.line(&format!("move.l (a7)+,{dst}"));
        } else {
            self.line(&format!("move.l {slot},{dst}"));
        }
    }
    /// Save A0 (a held lvalue address) onto the address eval stack.
    fn push_atemp(&mut self) -> String {
        let slot = ATEMP_REGS.get(self.atemp).map(|r| r.to_string()).unwrap_or_else(|| "-(a7)".into());
        self.line(&format!("move.l a0,{slot}"));
        self.atemp += 1;
        slot
    }
    /// Restore an address saved by [`push_atemp`] into `dst` (usually `a0`).
    fn pop_atemp_to(&mut self, slot: &str, dst: &str) {
        self.atemp -= 1;
        if slot == "-(a7)" {
            self.line(&format!("move.l (a7)+,{dst}"));
        } else {
            self.line(&format!("move.l {slot},{dst}"));
        }
    }

    fn emit_prelude(&mut self) {
        self.out.push_str("\t.68000\n");
        self.out.push_str("\t.text\n");
    }

    // ── functions ─────────────────────────────────────────────────────────────
    fn gen_function(&mut self, f: &Function) -> Result<(), String> {
        // Lay out the frame. Params get positive offsets (8,12,… above A6);
        // other locals get negative offsets below A6.
        self.frame.clear();
        self.types.clear();
        let param_names: std::collections::HashSet<&str> =
            f.params.iter().map(|(n, _)| n.as_str()).collect();
        let mut poff = 8i32;
        for (pn, pt) in &f.params {
            self.frame.insert(pn.clone(), poff);
            self.types.insert(pn.clone(), pt.clone());
            poff += 4; // args are pushed as longs
        }
        let mut noff = 0i32;
        for loc in &f.locals {
            self.types.insert(loc.name.clone(), loc.ty.clone());
            if param_names.contains(loc.name.as_str()) {
                continue;
            }
            let sz = loc.ty.size().max(1) as i32;
            let al = loc.ty.align().max(1) as i32;
            noff += sz;
            noff = (noff + al - 1) / al * al;
            self.frame.insert(loc.name.clone(), -noff);
        }
        let frame_size = ((noff + 1) / 2) * 2; // word-align the frame

        let name = mangle(&f.name);
        self.ret_label = format!(".Lret_{}", self.l());
        if !f.is_static {
            self.line(&format!(".globl {name}"));
        }
        self.lbl(&name);
        self.line(&format!("link a6,#-{frame_size}"));
        self.line("movem.l d2-d7/a2-a5,-(a7)");
        for s in &f.body {
            self.gen_stmt(s)?;
        }
        // fall-through return (returns garbage in D0, like C without a return)
        let rl = self.ret_label.clone();
        self.lbl(&rl);
        self.line("movem.l (a7)+,d2-d7/a2-a5");
        self.line("unlk a6");
        self.line("rts");
        self.out.push('\n');
        Ok(())
    }

    // ── statements ────────────────────────────────────────────────────────────
    fn gen_stmt(&mut self, s: &Stmt) -> Result<(), String> {
        match s {
            Stmt::Expr(e) => {
                self.gen_expr(e)?;
            }
            Stmt::Null => {}
            Stmt::Return(e) => {
                if let Some(e) = e {
                    self.gen_expr(e)?;
                }
                let rl = self.ret_label.clone();
                self.line(&format!("bra.w {rl}"));
            }
            Stmt::Block(items) => {
                for it in items {
                    self.gen_stmt(it)?;
                }
            }
            Stmt::Decl(name, ty, init) => {
                if let Some(init) = init {
                    let off = self.frame.get(name).copied().unwrap_or(0);
                    match init {
                        Init::Scalar(e) => {
                            self.gen_expr(e)?;
                            self.cast(&e.ty, ty); // implicit conversion
                            self.store_to_local(name, ty);
                        }
                        Init::List(_) => {
                            self.clear_frame(off, ty.size());
                            self.gen_local_init(off, ty, init)?;
                        }
                    }
                }
            }
            Stmt::Switch(cond, body, cases, default) => {
                self.gen_expr(cond)?; // D0 = switch value
                for (val, id) in cases {
                    self.line(&format!("cmpi.l #{val},d0"));
                    self.line(&format!("beq.w .Lsw_{id}"));
                }
                let brk = self.l();
                let after = default
                    .map(|d| format!(".Lsw_{d}"))
                    .unwrap_or_else(|| format!(".Lbrk_{brk}"));
                self.line(&format!("bra.w {after}"));
                self.break_labels.push(format!(".Lbrk_{brk}"));
                self.gen_stmt(body)?;
                self.break_labels.pop();
                self.lbl(&format!(".Lbrk_{brk}"));
            }
            Stmt::Case(id) => self.lbl(&format!(".Lsw_{id}")),
            Stmt::Default(id) => self.lbl(&format!(".Lsw_{id}")),
            Stmt::Goto(name) => self.line(&format!("bra.w .LuserL_{name}")),
            Stmt::Label(name, s) => {
                self.lbl(&format!(".LuserL_{name}"));
                self.gen_stmt(s)?;
            }
            Stmt::If(c, then, els) => {
                let lelse = self.l();
                let lend = self.l();
                self.gen_cond_branch(c, &format!(".Lelse_{lelse}"), false)?;
                self.gen_stmt(then)?;
                if let Some(els) = els {
                    self.line(&format!("bra.w .Lend_{lend}"));
                    self.lbl(&format!(".Lelse_{lelse}"));
                    self.gen_stmt(els)?;
                    self.lbl(&format!(".Lend_{lend}"));
                } else {
                    self.lbl(&format!(".Lelse_{lelse}"));
                }
            }
            Stmt::While(c, body) => {
                let ltop = self.l();
                let lend = self.l();
                self.lbl(&format!(".Ltop_{ltop}"));
                self.gen_cond_branch(c, &format!(".Lend_{lend}"), false)?;
                self.break_labels.push(format!(".Lend_{lend}"));
                self.cont_labels.push(format!(".Ltop_{ltop}"));
                self.gen_stmt(body)?;
                self.break_labels.pop();
                self.cont_labels.pop();
                self.line(&format!("bra.w .Ltop_{ltop}"));
                self.lbl(&format!(".Lend_{lend}"));
            }
            Stmt::DoWhile(body, c) => {
                let ltop = self.l();
                let lcont = self.l();
                let lend = self.l();
                self.lbl(&format!(".Ltop_{ltop}"));
                self.break_labels.push(format!(".Lend_{lend}"));
                self.cont_labels.push(format!(".Lcont_{lcont}"));
                self.gen_stmt(body)?;
                self.break_labels.pop();
                self.cont_labels.pop();
                self.lbl(&format!(".Lcont_{lcont}"));
                self.gen_cond_branch(c, &format!(".Ltop_{ltop}"), true)?;
                self.lbl(&format!(".Lend_{lend}"));
            }
            Stmt::For(init, cond, step, body) => {
                let ltop = self.l();
                let lcont = self.l();
                let lend = self.l();
                if let Some(init) = init {
                    self.gen_stmt(init)?;
                }
                self.lbl(&format!(".Ltop_{ltop}"));
                if let Some(cond) = cond {
                    self.gen_cond_branch(cond, &format!(".Lend_{lend}"), false)?;
                }
                self.break_labels.push(format!(".Lend_{lend}"));
                self.cont_labels.push(format!(".Lcont_{lcont}"));
                self.gen_stmt(body)?;
                self.break_labels.pop();
                self.cont_labels.pop();
                self.lbl(&format!(".Lcont_{lcont}"));
                if let Some(step) = step {
                    self.gen_expr(step)?;
                }
                self.line(&format!("bra.w .Ltop_{ltop}"));
                self.lbl(&format!(".Lend_{lend}"));
            }
            Stmt::Break => {
                let l = self.break_labels.last().cloned().ok_or("break outside loop")?;
                self.line(&format!("bra.w {l}"));
            }
            Stmt::Continue => {
                let l = self.cont_labels.last().cloned().ok_or("continue outside loop")?;
                self.line(&format!("bra.w {l}"));
            }
        }
        Ok(())
    }

    /// Branch to `target` when the condition is false (jump_if_true=false) or
    /// true (jump_if_true=true).
    fn gen_cond_branch(&mut self, c: &Expr, target: &str, jump_if_true: bool) -> Result<(), String> {
        self.gen_expr(c)?; // D0 = condition value
        self.line("tst.l d0");
        if jump_if_true {
            self.line(&format!("bne.w {target}"));
        } else {
            self.line(&format!("beq.w {target}"));
        }
        Ok(())
    }

    // ── expressions ───────────────────────────────────────────────────────────
    /// Evaluate `e`, result in D0.
    fn gen_expr(&mut self, e: &Expr) -> Result<(), String> {
        match &e.kind {
            ExprK::Num(n) => {
                self.load_imm(*n as i32);
            }
            ExprK::StrLit(idx) => {
                self.line(&format!("lea {}_{idx},a0", self.str_prefix));
                self.line("move.l a0,d0");
            }
            ExprK::Var(_) | ExprK::Unary(UnOp::Deref, _) | ExprK::Member(..) => {
                // lvalue → load its value
                self.gen_addr(e)?;
                self.load(&e.ty);
            }
            ExprK::Unary(UnOp::Addr, inner) => {
                self.gen_addr(inner)?;
                self.line("move.l a0,d0");
            }
            ExprK::Unary(UnOp::Neg, a) => {
                self.gen_expr(a)?;
                self.line("neg.l d0");
            }
            ExprK::Unary(UnOp::Not, a) => {
                self.gen_expr(a)?;
                self.line("not.l d0");
            }
            ExprK::Unary(UnOp::LogNot, a) => {
                self.gen_expr(a)?;
                self.line("tst.l d0");
                self.line("seq d0");
                self.line("and.l #1,d0");
            }
            ExprK::Cast(a) => {
                self.gen_expr(a)?;
                self.cast(&a.ty, &e.ty);
            }
            ExprK::Assign(lhs, rhs) => {
                self.gen_addr(lhs)?;
                let slot = self.push_atemp(); // hold dest addr in a callee-saved areg
                self.gen_expr(rhs)?; // value in D0
                self.cast(&rhs.ty, &lhs.ty); // implicit conversion (int↔fixed, widen)
                self.pop_atemp_to(&slot, "a0"); // restore dest
                self.store(&lhs.ty);
                // result of assignment is the stored value (already in D0)
            }
            ExprK::PostIncDec(lhs, delta) => {
                self.gen_addr(lhs)?;
                let aslot = self.push_atemp(); // hold dest addr
                self.load(&lhs.ty); // old value in D0
                let dslot = self.push_dtemp(); // hold old value (the result)
                self.load_imm_into("d1", *delta as i32);
                self.line("add.l d1,d0");
                self.pop_atemp_to(&aslot, "a0"); // dest addr
                self.store(&lhs.ty);
                self.pop_dtemp_to(&dslot, "d0"); // result = old value
            }
            ExprK::Comma(a, b) => {
                self.gen_expr(a)?;
                self.gen_expr(b)?;
            }
            ExprK::Cond(c, t, f) => {
                let lelse = self.l();
                let lend = self.l();
                self.gen_cond_branch(c, &format!(".Lelse_{lelse}"), false)?;
                self.gen_expr(t)?;
                self.line(&format!("bra.w .Lend_{lend}"));
                self.lbl(&format!(".Lelse_{lelse}"));
                self.gen_expr(f)?;
                self.lbl(&format!(".Lend_{lend}"));
            }
            ExprK::Binary(BinOp::LogAnd, a, b) => {
                let lfalse = self.l();
                let lend = self.l();
                self.gen_expr(a)?;
                self.line("tst.l d0");
                self.line(&format!("beq.w .Lfalse_{lfalse}"));
                self.gen_expr(b)?;
                self.line("tst.l d0");
                self.line(&format!("beq.w .Lfalse_{lfalse}"));
                self.line("moveq #1,d0");
                self.line(&format!("bra.w .Lend_{lend}"));
                self.lbl(&format!(".Lfalse_{lfalse}"));
                self.line("moveq #0,d0");
                self.lbl(&format!(".Lend_{lend}"));
            }
            ExprK::Binary(BinOp::LogOr, a, b) => {
                let ltrue = self.l();
                let lend = self.l();
                self.gen_expr(a)?;
                self.line("tst.l d0");
                self.line(&format!("bne.w .Ltrue_{ltrue}"));
                self.gen_expr(b)?;
                self.line("tst.l d0");
                self.line(&format!("bne.w .Ltrue_{ltrue}"));
                self.line("moveq #0,d0");
                self.line(&format!("bra.w .Lend_{lend}"));
                self.lbl(&format!(".Ltrue_{ltrue}"));
                self.line("moveq #1,d0");
                self.lbl(&format!(".Lend_{lend}"));
            }
            ExprK::Binary(op, a, b) => {
                // rhs first, held on the register eval stack; lhs into D0; rhs → D1
                self.gen_expr(b)?;
                let slot = self.push_dtemp();
                self.gen_expr(a)?;
                self.pop_dtemp_to(&slot, "d1"); // D0=lhs, D1=rhs
                self.gen_binop(*op, &a.ty, &b.ty);
            }
            ExprK::Call(callee, args) => {
                self.gen_call(callee, args)?;
            }
        }
        Ok(())
    }

    /// Compute the address of an lvalue into A0.
    fn gen_addr(&mut self, e: &Expr) -> Result<(), String> {
        match &e.kind {
            ExprK::Var(name) => {
                if let Some(off) = self.frame.get(name).copied() {
                    self.line(&format!("lea {off}(a6),a0"));
                } else if self.globals.contains_key(name) {
                    self.line(&format!("lea {},a0", mangle(name)));
                } else {
                    // unknown → treat as extern global
                    self.line(&format!("lea {},a0", mangle(name)));
                }
                Ok(())
            }
            ExprK::Unary(UnOp::Deref, inner) => {
                self.gen_expr(inner)?; // pointer value in D0
                self.line("move.l d0,a0");
                Ok(())
            }
            ExprK::Member(base_addr, off) => {
                self.gen_expr(base_addr)?; // struct address in D0
                self.line("move.l d0,a0");
                if *off != 0 {
                    self.line(&format!("adda.l #{off},a0"));
                }
                Ok(())
            }
            _ => Err(format!("{}: not an lvalue", e.line)),
        }
    }

    fn gen_call(&mut self, callee: &Expr, args: &[Expr]) -> Result<(), String> {
        // Push args right-to-left as longs.
        let nbytes = args.len() * 4;
        for a in args.iter().rev() {
            self.gen_expr(a)?;
            self.line("move.l d0,-(a7)");
        }
        // Direct call to a named function, else indirect through D0.
        if let ExprK::Var(name) = &callee.kind {
            if !self.frame.contains_key(name) {
                self.line(&format!("jsr {}", mangle(name)));
                if nbytes > 0 {
                    self.line(&format!("adda.l #{nbytes},a7"));
                }
                return Ok(());
            }
        }
        // indirect
        self.gen_expr(callee)?;
        self.line("move.l d0,a0");
        self.line("jsr (a0)");
        if nbytes > 0 {
            self.line(&format!("adda.l #{nbytes},a7"));
        }
        Ok(())
    }

    // ── binops (D0 = D0 op D1) ────────────────────────────────────────────────
    fn gen_binop(&mut self, op: BinOp, lt: &Type, rt: &Type) {
        let unsigned = !(lt.is_signed() && rt.is_signed());
        match op {
            BinOp::Add => self.line("add.l d1,d0"),
            BinOp::Sub => self.line("sub.l d1,d0"),
            BinOp::And => self.line("and.l d1,d0"),
            BinOp::Or => self.line("or.l d1,d0"),
            BinOp::Xor => self.line("eor.l d1,d0"),
            BinOp::Shl => self.line("asl.l d1,d0"),
            BinOp::Shr => {
                if unsigned {
                    self.line("lsr.l d1,d0");
                } else {
                    self.line("asr.l d1,d0");
                }
            }
            BinOp::Mul => {
                if lt.is_fixed() || rt.is_fixed() {
                    self.line("jsr __mulfix");
                } else {
                    self.line("jsr __mulsi3");
                }
            }
            BinOp::Div => {
                if lt.is_fixed() || rt.is_fixed() {
                    self.line("jsr __divfix");
                } else if unsigned {
                    self.line("jsr __udivsi3");
                } else {
                    self.line("jsr __divsi3");
                }
            }
            BinOp::Mod => {
                if unsigned {
                    self.line("jsr __umodsi3");
                } else {
                    self.line("jsr __modsi3");
                }
            }
            BinOp::Eq | BinOp::Ne | BinOp::Lt | BinOp::Le | BinOp::Gt | BinOp::Ge => {
                self.line("cmp.l d1,d0"); // sets flags for D0 - D1
                let cc = match (op, unsigned) {
                    (BinOp::Eq, _) => "seq",
                    (BinOp::Ne, _) => "sne",
                    (BinOp::Lt, false) => "slt",
                    (BinOp::Le, false) => "sle",
                    (BinOp::Gt, false) => "sgt",
                    (BinOp::Ge, false) => "sge",
                    (BinOp::Lt, true) => "scs",
                    (BinOp::Le, true) => "sls",
                    (BinOp::Gt, true) => "shi",
                    (BinOp::Ge, true) => "scc",
                    _ => unreachable!(),
                };
                self.line(&format!("{cc} d0"));
                self.line("and.l #1,d0");
            }
            BinOp::LogAnd | BinOp::LogOr => unreachable!("handled in gen_expr"),
        }
    }

    // ── loads / stores by size (A0 = address) ─────────────────────────────────
    fn load(&mut self, ty: &Type) {
        match &**ty {
            TypeK::Array(..) | TypeK::Struct { .. } | TypeK::Func { .. } => {
                // aggregate lvalue → its address is the value
                self.line("move.l a0,d0");
            }
            _ => {
                let sz = ty.size();
                let signed = ty.is_signed();
                match sz {
                    1 => {
                        if signed {
                            self.line("move.b (a0),d0");
                            self.line("ext.w d0");
                            self.line("ext.l d0");
                        } else {
                            self.line("moveq #0,d0");
                            self.line("move.b (a0),d0");
                        }
                    }
                    2 => {
                        if signed {
                            self.line("move.w (a0),d0");
                            self.line("ext.l d0");
                        } else {
                            self.line("moveq #0,d0");
                            self.line("move.w (a0),d0");
                        }
                    }
                    _ => self.line("move.l (a0),d0"),
                }
            }
        }
    }

    fn store(&mut self, ty: &Type) {
        // A0 = dest, D0 = value
        match ty.size() {
            1 => self.line("move.b d0,(a0)"),
            2 => self.line("move.w d0,(a0)"),
            _ => self.line("move.l d0,(a0)"),
        }
    }

    /// Zero `size` bytes at `off(a6)` (used before an aggregate initializer so
    /// unlisted elements read as 0, per C).
    fn clear_frame(&mut self, off: i32, size: u32) {
        if size == 0 {
            return;
        }
        self.line(&format!("lea {off}(a6),a0"));
        let longs = size / 4;
        if longs > 0 {
            let lbl = self.l();
            self.load_imm_into("d0", longs as i32 - 1);
            self.lbl(&format!(".Lclr_{lbl}"));
            self.line("clr.l (a0)+");
            self.line(&format!("dbra d0,.Lclr_{lbl}"));
        }
        for _ in 0..(size % 4) {
            self.line("clr.b (a0)+");
        }
    }

    /// Emit an aggregate/scalar initializer into the frame slot at `off(a6)`.
    fn gen_local_init(&mut self, off: i32, ty: &Type, init: &Init) -> Result<(), String> {
        match (init, &**ty) {
            (Init::Scalar(e), _) => {
                self.gen_expr(e)?;
                self.cast(&e.ty, ty);
                match ty.size() {
                    1 => self.line(&format!("move.b d0,{off}(a6)")),
                    2 => self.line(&format!("move.w d0,{off}(a6)")),
                    _ => self.line(&format!("move.l d0,{off}(a6)")),
                }
            }
            (Init::List(items), TypeK::Array(el, n)) => {
                let esz = el.size() as i32;
                for (i, it) in items.iter().enumerate() {
                    if *n != 0 && i as u32 >= *n {
                        break;
                    }
                    self.gen_local_init(off + i as i32 * esz, el, it)?;
                }
            }
            (Init::List(items), TypeK::Struct { members, .. }) => {
                for (it, m) in items.iter().zip(members.iter()) {
                    self.gen_local_init(off + m.offset as i32, &m.ty, it)?;
                }
            }
            (Init::List(items), _) => {
                if let Some(first) = items.first() {
                    self.gen_local_init(off, ty, first)?;
                }
            }
        }
        Ok(())
    }

    fn store_to_local(&mut self, name: &str, ty: &Type) {
        if let Some(off) = self.frame.get(name).copied() {
            match ty.size() {
                1 => self.line(&format!("move.b d0,{off}(a6)")),
                2 => self.line(&format!("move.w d0,{off}(a6)")),
                _ => self.line(&format!("move.l d0,{off}(a6)")),
            }
        }
    }

    fn cast(&mut self, from: &Type, to: &Type) {
        // int/fixed conversions (16.16 fixed-point).
        if to.is_fixed() && from.is_integer() {
            // int → fixed: value << 16 (swap words, clear the low word).
            self.line("swap d0");
            self.line("clr.w d0");
            return;
        }
        if to.is_integer() && from.is_fixed() {
            // fixed → int: signed value >> 16 (integer part in the high word).
            self.line("swap d0");
            self.line("ext.l d0");
            // narrow to the target int size happens implicitly on store
            return;
        }
        if to.is_fixed() || from.is_fixed() {
            return; // fixed↔fixed, or fixed↔pointer: bit-identical
        }
        // Narrowing/widening in D0.
        if to.size() >= 4 {
            // widen to 32 from smaller int
            match from.size() {
                1 => {
                    if from.is_signed() {
                        self.line("ext.w d0");
                        self.line("ext.l d0");
                    } else {
                        self.line("and.l #$FF,d0");
                    }
                }
                2 => {
                    if from.is_signed() {
                        self.line("ext.l d0");
                    } else {
                        self.line("and.l #$FFFF,d0");
                    }
                }
                _ => {}
            }
        }
        // narrowing is implicit (stores use the destination size)
    }

    fn load_imm(&mut self, v: i32) {
        self.load_imm_into("d0", v);
    }
    fn load_imm_into(&mut self, reg: &str, v: i32) {
        if (-128..=127).contains(&v) {
            self.line(&format!("moveq #{v},{reg}"));
        } else {
            self.line(&format!("move.l #{v},{reg}"));
        }
    }

    // ── data + rodata ─────────────────────────────────────────────────────────
    fn emit_data(&mut self, prog: &Program) {
        // strings
        if !prog.strings.is_empty() {
            self.out.push_str("\t.data\n");
            for (i, s) in prog.strings.iter().enumerate() {
                writeln!(self.out, "{}_{i}:", self.str_prefix).unwrap();
                self.out.push_str("\t.dc.b ");
                let parts: Vec<String> = s.iter().map(|b| format!("${b:02X}")).collect();
                self.out.push_str(&parts.join(","));
                self.out.push('\n');
            }
            self.out.push_str("\t.even\n");
        }
        // globals with initializers, then bss
        let has_data = prog.globals.iter().any(|g| g.init.is_some() && !g.is_extern);
        if has_data {
            self.out.push_str("\t.data\n\t.even\n");
            for g in &prog.globals {
                if g.is_extern {
                    continue;
                }
                if let Some(init) = &g.init {
                    if !g.is_static {
                        writeln!(self.out, "\t.globl {}", mangle(&g.name)).unwrap();
                    }
                    writeln!(self.out, "{}:", mangle(&g.name)).unwrap();
                    emit_init(&mut self.out, init);
                }
            }
        }
        for g in &prog.globals {
            if g.is_extern || g.init.is_some() {
                continue;
            }
            let sz = ((g.ty.size().max(1) + 1) / 2) * 2;
            if !g.is_static {
                writeln!(self.out, "\t.globl {}", mangle(&g.name)).unwrap();
            }
            writeln!(self.out, "{}:", mangle(&g.name)).unwrap();
            writeln!(self.out, "\t.ds.b {sz}").unwrap();
        }
    }
}

/// Emit a global's initializer image: coalesce runs of literal bytes into
/// `.dc.b` directives, and emit each symbol address as `.dc.l _sym+addend`.
fn emit_init(out: &mut String, init: &[InitByte]) {
    let mut run: Vec<u8> = Vec::new();
    let flush = |out: &mut String, run: &mut Vec<u8>| {
        if !run.is_empty() {
            out.push_str("\t.dc.b ");
            let parts: Vec<String> = run.iter().map(|b| format!("${b:02X}")).collect();
            out.push_str(&parts.join(","));
            out.push('\n');
            run.clear();
        }
    };
    for item in init {
        match item {
            InitByte::Byte(b) => run.push(*b),
            InitByte::Addr(sym, addend) => {
                flush(out, &mut run);
                out.push_str("\t.even\n");
                if *addend != 0 {
                    writeln!(out, "\t.dc.l {}+{}", mangle(sym), addend).unwrap();
                } else {
                    writeln!(out, "\t.dc.l {}", mangle(sym)).unwrap();
                }
            }
        }
    }
    flush(out, &mut run);
}

/// Post-process the emitted assembly. Currently: drop `bra L` when the very
/// next line is `L:` (a branch to the following instruction) — both a size
/// optimization and a workaround for the assembler's short/long branch
/// oscillation on a zero displacement.
fn peephole(asm: &str) -> String {
    let lines: Vec<&str> = asm.lines().collect();
    let mut out = String::with_capacity(asm.len());
    let mut i = 0;
    while i < lines.len() {
        let line = lines[i];
        let trimmed = line.trim();
        if let Some(target) = trimmed.strip_prefix("bra.w ").map(str::trim) {
            // A `bra L` where L labels the very next instruction (possibly
            // reached only through empty/label-only lines) is a fall-through:
            // drop it. This both optimizes and dodges the assembler's zero-
            // displacement branch-size oscillation.
            let want = format!("{target}:");
            let mut j = i + 1;
            let mut falls_through = false;
            while j < lines.len() {
                let t = lines[j].trim();
                if t.is_empty() {
                    j += 1;
                    continue;
                }
                if t.ends_with(':') && !t.contains(char::is_whitespace) {
                    if t == want {
                        falls_through = true;
                        break;
                    }
                    j += 1;
                    continue; // another label at the same address
                }
                break; // a real instruction before the target label
            }
            if falls_through {
                i += 1;
                continue;
            }
        }
        out.push_str(line);
        out.push('\n');
        i += 1;
    }
    out
}

fn mangle(name: &str) -> String {
    // C linkage: no prefix, matching the m68k-elf (GCC/ELF) convention the real
    // Jaguar ports and their hand-written asm use — so a C `gpu_kernel` reference
    // resolves to the asm's `gpu_kernel` label at link time. (The a.out `_`
    // prefix would leave every cross-language symbol unresolved.)
    name.to_string()
}
