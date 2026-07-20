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
    /// Scalar locals promoted to a callee-saved data register for this function
    /// (name → register). Read/written directly; never spilled to a frame slot.
    reg_of: HashMap<String, String>,
    /// Data registers available to the eval stack this function (the callee-saved
    /// set minus any [`reg_of`] has claimed for a local).
    dpool: Vec<&'static str>,
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
    let live = reachable(prog);
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
        reg_of: HashMap::new(),
        dpool: Vec::new(),
    };
    for gl in &prog.globals {
        g.globals.insert(gl.name.clone(), gl.ty.clone());
    }
    for f in &prog.functions {
        g.globals.insert(f.name.clone(), t_int());
    }
    g.emit_prelude();
    for f in &prog.functions {
        if f.is_static && !live.contains(&f.name) {
            continue; // unreferenced static: no text
        }
        g.gen_function(f)?;
    }
    g.emit_data(prog, &live);
    Ok(peephole(&g.out))
}

/// Names of statics reachable from this unit's externally-visible definitions
/// (non-static functions and globals). gcc -O2 discards unreferenced statics
/// — OpenLara's main.c carries ~570KB of static buffers belonging to compiled-
/// out render paths, and emitting them blew the 2MB console budget (adoption
/// report round 2). Non-static definitions are always emitted.
fn reachable(prog: &Program) -> std::collections::HashSet<String> {
    use std::collections::HashSet;
    // per-definition reference sets
    let mut fn_refs: HashMap<&str, HashSet<String>> = HashMap::new();
    for f in &prog.functions {
        let mut refs = HashSet::new();
        for s in live_prefix(&f.body) {
            collect_stmt_refs(s, &mut refs);
        }
        fn_refs.insert(&f.name, refs);
    }
    let mut gl_refs: HashMap<&str, HashSet<String>> = HashMap::new();
    for g in &prog.globals {
        let mut refs = HashSet::new();
        if let Some(init) = &g.init {
            for b in init {
                if let InitByte::Addr(sym, _) = b {
                    refs.insert(sym.clone());
                }
            }
        }
        gl_refs.insert(&g.name, refs);
    }
    let mut live: HashSet<String> = HashSet::new();
    let mut work: Vec<String> = prog
        .functions
        .iter()
        .filter(|f| !f.is_static)
        .map(|f| f.name.clone())
        .chain(
            prog.globals
                .iter()
                .filter(|g| !g.is_static && !g.is_extern)
                .map(|g| g.name.clone()),
        )
        .collect();
    while let Some(n) = work.pop() {
        if !live.insert(n.clone()) {
            continue;
        }
        if let Some(refs) = fn_refs.get(n.as_str()) {
            work.extend(refs.iter().cloned());
        }
        if let Some(refs) = gl_refs.get(n.as_str()) {
            work.extend(refs.iter().cloned());
        }
    }
    live
}

// ── unreachable-tail pruning ─────────────────────────────────────────────────
// OpenLara's main() ends its active render path in `for (;;)`; everything
// after it — a compiled-out path referencing 570KB of static buffers — is
// unreachable. gcc -O2 removes it; we must too, in BOTH the reachability walk
// and emission (emitting code that names eliminated statics would leave
// undefined references at link time).

/// Does control ever reach the statement AFTER `s`?
fn falls_through(s: &Stmt) -> bool {
    let const_true = |e: &Expr| matches!(&e.kind, ExprK::Num(n) if *n != 0);
    match s {
        Stmt::Return(_) | Stmt::Break | Stmt::Continue | Stmt::Goto(_) => false,
        Stmt::For(_, cond, _, body) => match cond {
            None => loop_escapes(body),
            Some(c) if const_true(c) => loop_escapes(body),
            Some(_) => true,
        },
        Stmt::While(c, body) if const_true(c) => loop_escapes(body),
        Stmt::DoWhile(body, c) if const_true(c) => loop_escapes(body),
        Stmt::If(_, t, Some(e)) => falls_through(t) || falls_through(e),
        Stmt::Label(_, inner) => falls_through(inner),
        Stmt::Block(items) => {
            let p = live_prefix(items);
            p.len() == items.len() && p.last().map(falls_through).unwrap_or(true)
        }
        _ => true,
    }
}

/// Can control escape this loop body to the loop's successor: a `break` bound
/// to THIS loop, or a `goto` whose label is defined outside the body.
fn loop_escapes(body: &Stmt) -> bool {
    if break_at_level(body) {
        return true;
    }
    let mut labels = std::collections::HashSet::new();
    let mut gotos = std::collections::HashSet::new();
    collect_labels_gotos(body, &mut labels, &mut gotos);
    gotos.iter().any(|g| !labels.contains(g))
}

/// A `break` binding to the current level (nested loops/switches own theirs).
fn break_at_level(s: &Stmt) -> bool {
    match s {
        Stmt::Break => true,
        Stmt::While(..) | Stmt::DoWhile(..) | Stmt::For(..) | Stmt::Switch(..) => false,
        Stmt::Block(v) => v.iter().any(break_at_level),
        Stmt::If(_, t, e) => {
            break_at_level(t) || e.as_deref().map(break_at_level).unwrap_or(false)
        }
        Stmt::Label(_, inner) => break_at_level(inner),
        _ => false,
    }
}

fn collect_labels_gotos(
    s: &Stmt,
    labels: &mut std::collections::HashSet<String>,
    gotos: &mut std::collections::HashSet<String>,
) {
    match s {
        Stmt::Label(n, inner) => {
            labels.insert(n.clone());
            collect_labels_gotos(inner, labels, gotos);
        }
        Stmt::Goto(n) => {
            gotos.insert(n.clone());
        }
        Stmt::Block(v) => v.iter().for_each(|s| collect_labels_gotos(s, labels, gotos)),
        Stmt::If(_, t, e) => {
            collect_labels_gotos(t, labels, gotos);
            if let Some(e) = e {
                collect_labels_gotos(e, labels, gotos);
            }
        }
        Stmt::While(_, b) | Stmt::DoWhile(b, _) | Stmt::Switch(_, b, _, _) => {
            collect_labels_gotos(b, labels, gotos)
        }
        Stmt::For(i, _, _, b) => {
            if let Some(i) = i {
                collect_labels_gotos(i, labels, gotos);
            }
            collect_labels_gotos(b, labels, gotos);
        }
        _ => {}
    }
}

/// Can control enter `s` from outside sequential flow (a goto label or a
/// switch case/default)? Tails containing one are never pruned.
fn has_entry_point(s: &Stmt) -> bool {
    match s {
        Stmt::Label(..) | Stmt::Case(_) | Stmt::Default(_) => true,
        Stmt::Block(v) => v.iter().any(has_entry_point),
        Stmt::If(_, t, e) => {
            has_entry_point(t) || e.as_deref().map(has_entry_point).unwrap_or(false)
        }
        Stmt::While(_, b) | Stmt::DoWhile(b, _) | Stmt::Switch(_, b, _, _) => has_entry_point(b),
        Stmt::For(i, _, _, b) => {
            i.as_deref().map(has_entry_point).unwrap_or(false) || has_entry_point(b)
        }
        _ => false,
    }
}

/// The reachable prefix of a statement list: cut after the first statement
/// control cannot fall out of, unless something later is a jump target.
fn live_prefix(items: &[Stmt]) -> &[Stmt] {
    for (i, s) in items.iter().enumerate() {
        if !falls_through(s) {
            let tail = &items[i + 1..];
            if tail.iter().any(has_entry_point) {
                return items; // a label/case makes the tail reachable — keep all
            }
            return &items[..=i];
        }
    }
    items
}

/// Collect every name a statement references (variables, calls, inline-asm
/// text) — the edges of the reachability graph. Inline asm is scanned as raw
/// text, so a symbol named there keeps its definition alive.
fn collect_stmt_refs(s: &Stmt, out: &mut std::collections::HashSet<String>) {
    fn expr(e: &Expr, out: &mut std::collections::HashSet<String>) {
        match &e.kind {
            ExprK::Var(n) => {
                out.insert(n.clone());
            }
            ExprK::Num(_) | ExprK::StrLit(_) => {}
            ExprK::Unary(_, a) | ExprK::Cast(a) | ExprK::Member(a, _) | ExprK::PostIncDec(a, _) => {
                expr(a, out)
            }
            ExprK::Binary(_, a, b) | ExprK::Assign(a, b) | ExprK::Comma(a, b) => {
                expr(a, out);
                expr(b, out);
            }
            ExprK::Cond(c, t, f) => {
                expr(c, out);
                expr(t, out);
                expr(f, out);
            }
            ExprK::Call(callee, args) => {
                expr(callee, out);
                args.iter().for_each(|a| expr(a, out));
            }
        }
    }
    fn init(i: &Init, out: &mut std::collections::HashSet<String>) {
        match i {
            Init::Scalar(e) => expr(e, out),
            Init::List(items) => items.iter().for_each(|it| init(it, out)),
        }
    }
    let asm_text = |t: &str, out: &mut std::collections::HashSet<String>| {
        // conservative: every identifier-looking token in the asm text
        for tok in t.split(|c: char| !(c.is_ascii_alphanumeric() || c == '_')) {
            if !tok.is_empty() && !tok.starts_with(|c: char| c.is_ascii_digit()) {
                out.insert(tok.to_string());
            }
        }
    };
    match s {
        Stmt::Expr(e) | Stmt::Return(Some(e)) => expr(e, out),
        Stmt::Return(None) | Stmt::Break | Stmt::Continue | Stmt::Goto(_)
        | Stmt::Case(_) | Stmt::Default(_) | Stmt::Null => {}
        Stmt::Asm(t) => asm_text(t, out),
        Stmt::AsmExt { template, output, input } => {
            asm_text(template, out);
            if let Some((_, e)) = output {
                expr(e, out);
            }
            if let Some(e) = input {
                expr(e, out);
            }
        }
        Stmt::If(c, t, e) => {
            expr(c, out);
            collect_stmt_refs(t, out);
            if let Some(e) = e {
                collect_stmt_refs(e, out);
            }
        }
        Stmt::While(c, b) | Stmt::DoWhile(b, c) => {
            expr(c, out);
            collect_stmt_refs(b, out);
        }
        Stmt::For(i, c, st, b) => {
            if let Some(i) = i {
                collect_stmt_refs(i, out);
            }
            if let Some(c) = c {
                expr(c, out);
            }
            if let Some(st) = st {
                expr(st, out);
            }
            collect_stmt_refs(b, out);
        }
        Stmt::Block(ss) => live_prefix(ss).iter().for_each(|s| collect_stmt_refs(s, out)),
        Stmt::Switch(e, b, _, _) => {
            expr(e, out);
            collect_stmt_refs(b, out);
        }
        Stmt::Label(_, b) => collect_stmt_refs(b, out),
        Stmt::Decl(_, _, Some(i)) => init(i, out),
        Stmt::Decl(_, _, None) => {}
    }
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
        let slot = self.dpool.get(self.dtemp).map(|r| r.to_string()).unwrap_or_else(|| "-(a7)".into());
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
        self.reg_of.clear();

        // Promote the hottest scalar-4, address-never-taken locals/params to
        // callee-saved data registers d7..d4 (the eval stack then uses whatever
        // remains). This keeps loop counters/accumulators out of memory.
        let mut refs = HashMap::new();
        let mut addr = std::collections::HashSet::new();
        for s in &f.body {
            analyze_stmt(s, &mut refs, &mut addr);
        }
        // Volatile locals are excluded: every access must be a real memory
        // access (a volatile delay-loop counter promoted to a register would
        // collapse the delay the code is written for).
        let volatile_names: std::collections::HashSet<&str> = f
            .locals
            .iter()
            .filter(|l| l.is_volatile)
            .map(|l| l.name.as_str())
            .collect();
        let mut cand: Vec<(&str, &Type)> = f
            .params
            .iter()
            .map(|(n, t)| (n.as_str(), t))
            .chain(f.locals.iter().map(|l| (l.name.as_str(), &l.ty)))
            .filter(|(n, t)| {
                is_scalar4(t) && !addr.contains(*n) && refs.contains_key(*n) && !volatile_names.contains(*n)
            })
            .collect();
        // hottest first; then drop duplicates (a param appears in both lists).
        cand.sort_by(|a, b| refs[b.0].cmp(&refs[a.0]).then(a.0.cmp(b.0)));
        cand.dedup_by(|a, b| a.0 == b.0);
        const LOCAL_REGS: &[&str] = &["d7", "d6", "d5", "d4"];
        let mut claimed: Vec<&str> = Vec::new();
        for ((n, _), &r) in cand.iter().zip(LOCAL_REGS) {
            self.reg_of.insert(n.to_string(), r.to_string());
            claimed.push(r);
        }
        self.dpool = DTEMP_REGS.iter().copied().filter(|r| !claimed.contains(r)).collect();

        let param_names: std::collections::HashSet<&str> =
            f.params.iter().map(|(n, _)| n.as_str()).collect();
        let mut poff = 8i32;
        for (pn, pt) in &f.params {
            // A register param still receives its arg in the stack slot; we copy
            // it to the register in the prologue below.
            self.frame.insert(pn.clone(), poff);
            self.types.insert(pn.clone(), pt.clone());
            poff += 4; // args are pushed as longs
        }
        let mut noff = 0i32;
        for loc in &f.locals {
            self.types.insert(loc.name.clone(), loc.ty.clone());
            if param_names.contains(loc.name.as_str()) || self.reg_of.contains_key(&loc.name) {
                continue; // register locals need no frame slot
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

        // Generate the body into a side buffer first, then wrap it in the
        // smallest correct prologue/epilogue: LINK/UNLK only when the frame is
        // actually used, and save/restore only the callee-saved registers the
        // body touches. A leaf like `blit_wait()` (a 3-instruction spin) gets
        // no prologue at all instead of a full link + movem of ten registers.
        let outer = std::mem::take(&mut self.out);
        for s in live_prefix(&f.body) {
            self.gen_stmt(s)?;
        }
        let body = std::mem::replace(&mut self.out, outer);

        let used = used_callee_saved(&body);
        let need_frame = frame_size > 0 || body.contains("(a6)");
        let save_bytes = used.len() as i32 * 4;

        if !f.is_static {
            self.line(&format!(".globl {name}"));
        }
        self.lbl(&name);
        if need_frame {
            self.line(&format!("link a6,#-{frame_size}"));
        }
        match used.as_slice() {
            [] => {}
            [r] => self.line(&format!("move.l {r},-(a7)")),
            _ => self.line(&format!("movem.l {},-(a7)", used.join("/"))),
        }
        // Copy register-allocated parameters out of their incoming stack slots.
        // Without a frame pointer the slots are A7-relative: past the saved
        // registers and the return address (the copies run before any body
        // push, so A7 is still at its post-save position).
        for (pn, _) in &f.params {
            if let Some(reg) = self.reg_of.get(pn).cloned() {
                let off = self.frame[pn];
                if need_frame {
                    self.line(&format!("move.l {off}(a6),{reg}"));
                } else {
                    self.line(&format!("move.l {}(a7),{reg}", off - 8 + 4 + save_bytes));
                }
            }
        }
        self.out.push_str(&body);
        // fall-through return (returns garbage in D0, like C without a return)
        let rl = self.ret_label.clone();
        self.lbl(&rl);
        match used.as_slice() {
            [] => {}
            [r] => self.line(&format!("move.l (a7)+,{r}")),
            _ => self.line(&format!("movem.l (a7)+,{}", used.join("/"))),
        }
        if need_frame {
            self.line("unlk a6");
        }
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
            Stmt::Asm(text) => {
                // Basic inline asm: emit the text verbatim, GNU `%` register
                // prefixes normalized to the jas spelling, one line per
                // newline-separated piece.
                for l in text.lines() {
                    let l = normalize_gas_asm(l);
                    let l = l.trim();
                    if !l.is_empty() {
                        self.line(l);
                    }
                }
            }
            Stmt::AsmExt { template, output, input } => {
                // Operand plan (GCC numbering, output first): %0 → d0, %1 → d1.
                // Evaluate the input, hold it, load a `+` output's old value,
                // emit the substituted template, store d0 back to the output.
                let slot = match input {
                    Some(inp) => {
                        self.gen_expr(inp)?;
                        Some(self.push_dtemp())
                    }
                    None => None,
                };
                if let Some((read_write, out_lv)) = output {
                    if *read_write {
                        self.gen_expr(out_lv)?; // old value in d0
                    }
                }
                let in_reg = if output.is_some() { "d1" } else { "d0" };
                if let Some(slot) = slot {
                    self.pop_dtemp_to(&slot, in_reg);
                }
                let subst = template.replace("%0", "__R0__").replace("%1", "__R1__");
                let (r0, r1) = if output.is_some() { ("d0", in_reg) } else { (in_reg, "") };
                for l in subst.lines() {
                    let l = normalize_gas_asm(l);
                    let l = l.replace("__R0__", r0).replace("__R1__", r1);
                    let l = l.trim();
                    if !l.is_empty() {
                        self.line(l);
                    }
                }
                if let Some((_, out_lv)) = output {
                    self.store_d0_to_lvalue(out_lv)?;
                }
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
                for it in live_prefix(items) {
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
                            if let Some(r) = self.reg_of.get(name).cloned() {
                                self.line(&format!("move.l d0,{r}"));
                            } else {
                                self.store_to_local(name, ty);
                            }
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
            ExprK::Var(name) if self.reg_of.contains_key(name) => {
                // register-allocated local → read the register directly
                let r = self.reg_of[name].clone();
                self.line(&format!("move.l {r},d0"));
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
                if let ExprK::Var(name) = &lhs.kind {
                    if let Some(r) = self.reg_of.get(name).cloned() {
                        // register-allocated local → assign the register directly
                        self.gen_expr(rhs)?;
                        self.cast(&rhs.ty, &lhs.ty);
                        self.line(&format!("move.l d0,{r}"));
                        return Ok(());
                    }
                }
                self.gen_addr(lhs)?;
                let slot = self.push_atemp(); // hold dest addr in a callee-saved areg
                self.gen_expr(rhs)?; // value in D0
                self.cast(&rhs.ty, &lhs.ty); // implicit conversion (int↔fixed, widen)
                self.pop_atemp_to(&slot, "a0"); // restore dest
                self.store(&lhs.ty);
                // result of assignment is the stored value (already in D0)
            }
            ExprK::PostIncDec(lhs, delta) => {
                if let ExprK::Var(name) = &lhs.kind {
                    if let Some(r) = self.reg_of.get(name).cloned() {
                        // register local: result is the old value, then adjust
                        self.line(&format!("move.l {r},d0"));
                        self.load_imm_into("d1", *delta as i32);
                        self.line(&format!("add.l d1,{r}"));
                        return Ok(());
                    }
                }
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
                // Fast path: a cheap rhs (a constant or a 4-byte scalar variable)
                // folds straight into the instruction, sparing the temp register
                // and the operand's load — `x + 5` → `add.l #5,d0`, not a push/pop.
                if let Some((src, is_imm)) = self.foldable_rhs(*op, b) {
                    self.gen_expr(a)?;
                    self.fold_binop(*op, &a.ty, &b.ty, &src, is_imm);
                } else {
                    // General path: rhs held on the register eval stack.
                    self.gen_expr(b)?;
                    let slot = self.push_dtemp();
                    self.gen_expr(a)?;
                    self.pop_dtemp_to(&slot, "d1"); // D0=lhs, D1=rhs
                    self.gen_binop(*op, &a.ty, &b.ty);
                }
            }
            ExprK::Call(callee, args) => {
                self.gen_call(callee, args)?;
            }
        }
        Ok(())
    }

    /// Store D0 into an lvalue (used by extended-asm output write-back).
    fn store_d0_to_lvalue(&mut self, lv: &Expr) -> Result<(), String> {
        if let ExprK::Var(name) = &lv.kind {
            if let Some(r) = self.reg_of.get(name).cloned() {
                self.line(&format!("move.l d0,{r}"));
                return Ok(());
            }
        }
        let slot = self.push_dtemp(); // save the value across gen_addr
        self.gen_addr(lv)?;
        self.pop_dtemp_to(&slot, "d0");
        self.store(&lv.ty);
        Ok(())
    }

    /// Compute the address of an lvalue into A0.
    fn gen_addr(&mut self, e: &Expr) -> Result<(), String> {
        match &e.kind {
            ExprK::Var(name) => {
                if self.reg_of.contains_key(name) {
                    // A register-allocated local has no address; reads/writes are
                    // intercepted before gen_addr, and `&x` locals are excluded
                    // from allocation, so reaching here is a compiler bug.
                    return Err(format!("{}: address of register-allocated `{name}`", e.line));
                }
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
        // Direct call to a named function, else indirect through D0. A register
        // (or frame) local named here holds a function *pointer* — call indirect.
        if let ExprK::Var(name) = &callee.kind {
            if !self.frame.contains_key(name) && !self.reg_of.contains_key(name) {
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

    /// Integer mul/div/mod by a power-of-two constant, as shifts (D0 in
    /// place). Returns false when the value can't be reduced (the caller
    /// falls back to the runtime helper). Signed division only reduces for
    /// n == 1 (an arithmetic shift rounds toward −∞, C truncates toward 0).
    fn fold_pow2(&mut self, op: BinOp, lt: &Type, rt: &Type, n: i64) -> bool {
        let unsigned = !(lt.is_signed() && rt.is_signed());
        if n <= 0 || (n & (n - 1)) != 0 {
            return false;
        }
        let k = n.trailing_zeros();
        match op {
            BinOp::Mul => {
                match k {
                    0 => {}
                    1..=8 => self.line(&format!("asl.l #{k},d0")),
                    _ => {
                        self.line(&format!("moveq #{k},d1"));
                        self.line("asl.l d1,d0");
                    }
                }
                true
            }
            BinOp::Div if unsigned || k == 0 => {
                match k {
                    0 => {}
                    1..=8 => self.line(&format!("lsr.l #{k},d0")),
                    _ => {
                        self.line(&format!("moveq #{k},d1"));
                        self.line("lsr.l d1,d0");
                    }
                }
                true
            }
            BinOp::Mod if unsigned => {
                self.line(&format!("and.l #{},d0", n - 1));
                true
            }
            _ => false,
        }
    }

    /// Call a runtime helper with the **libgcc calling convention**: both
    /// operands pushed (right-to-left, so `a` sits at 4(sp)), caller pops,
    /// result in D0. jcc68k emits calls to libgcc-NAMED symbols (`__mulsi3`
    /// …), so it must use libgcc's ABI — a project linking against libgcc or
    /// a drop-in like OpenLara's divmod68k.S would otherwise read stack
    /// garbage as operands and miscompile silently (the gpu.c/jerry.c
    /// black-screen boot from the adoption report, round 2).
    fn call_runtime(&mut self, name: &str) {
        self.line("move.l d1,-(a7)");
        self.line("move.l d0,-(a7)");
        self.line(&format!("jsr {name}"));
        self.line("addq.l #8,a7");
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
                    self.call_runtime("__mulfix");
                } else {
                    self.call_runtime("__mulsi3");
                }
            }
            BinOp::Div => {
                if lt.is_fixed() || rt.is_fixed() {
                    self.call_runtime("__divfix");
                } else if unsigned {
                    self.call_runtime("__udivsi3");
                } else {
                    self.call_runtime("__divsi3");
                }
            }
            BinOp::Mod => {
                if unsigned {
                    self.call_runtime("__umodsi3");
                } else {
                    self.call_runtime("__modsi3");
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

    /// A "cheap" operand that can be an instruction source without evaluation
    /// into a register: an integer constant (immediate) or a 4-byte scalar
    /// variable (a longword in memory). Returns `(source, is_immediate)`.
    fn cheap_operand(&self, e: &Expr) -> Option<(String, bool)> {
        match &e.kind {
            ExprK::Num(n) => Some((format!("#{}", *n as i32), true)),
            ExprK::Var(name) if self.reg_of.contains_key(name) => {
                // a register-allocated local is itself a valid instruction source
                Some((self.reg_of[name].clone(), false))
            }
            ExprK::Var(name) if is_scalar4(&e.ty) => {
                let src = match self.frame.get(name).copied() {
                    Some(off) => format!("{off}(a6)"),
                    None => mangle(name),
                };
                Some((src, false))
            }
            _ => None,
        }
    }

    /// If the right operand of `op` can be folded directly into the instruction,
    /// return its source. Some ops accept only a subset (eor needs an immediate;
    /// shifts need a 1–8 immediate count).
    fn foldable_rhs(&self, op: BinOp, b: &Expr) -> Option<(String, bool)> {
        let (src, is_imm) = self.cheap_operand(b)?;
        let ok = match op {
            // <ea>/immediate source works directly
            BinOp::Add | BinOp::Sub | BinOp::And | BinOp::Or
            | BinOp::Eq | BinOp::Ne | BinOp::Lt | BinOp::Le | BinOp::Gt | BinOp::Ge
            | BinOp::Mul | BinOp::Div | BinOp::Mod => true,
            // eor has no `<ea>,Dn` form — immediate only (eori)
            BinOp::Xor => is_imm,
            // immediate shift count, 1..8 only
            BinOp::Shl | BinOp::Shr => {
                matches!(&b.kind, ExprK::Num(n) if (1..=8).contains(n))
            }
            BinOp::LogAnd | BinOp::LogOr => false,
        };
        ok.then_some((src, is_imm))
    }

    /// Emit `op` with the right operand as a folded source (D0 already holds the
    /// left operand). Mirrors [`gen_binop`] but takes the rhs from `src`.
    fn fold_binop(&mut self, op: BinOp, lt: &Type, rt: &Type, src: &str, _is_imm: bool) {
        let unsigned = !(lt.is_signed() && rt.is_signed());
        match op {
            BinOp::Add => self.line(&format!("add.l {src},d0")),
            BinOp::Sub => self.line(&format!("sub.l {src},d0")),
            BinOp::And => self.line(&format!("and.l {src},d0")),
            BinOp::Or => self.line(&format!("or.l {src},d0")),
            BinOp::Xor => self.line(&format!("eori.l {src},d0")),
            BinOp::Shl => self.line(&format!("asl.l {src},d0")),
            BinOp::Shr => {
                self.line(&format!("{} {src},d0", if unsigned { "lsr.l" } else { "asr.l" }));
            }
            BinOp::Eq | BinOp::Ne | BinOp::Lt | BinOp::Le | BinOp::Gt | BinOp::Ge => {
                self.line(&format!("cmp.l {src},d0"));
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
            // multiply/divide/modulo: a power-of-two constant strength-reduces
            // to shifts/masks — pointer-index scaling made every array store a
            // __mulsi3 CALL (OpenLara's video_init: ~460k calls to clear three
            // framebuffers, ~8 seconds of boot). Otherwise the runtime helper.
            BinOp::Mul | BinOp::Div | BinOp::Mod => {
                if !lt.is_fixed() && !rt.is_fixed() {
                    if let Some(n) = parse_imm(src, _is_imm) {
                        if self.fold_pow2(op, lt, rt, n) {
                            return;
                        }
                    }
                }
                self.line(&format!("move.l {src},d1"));
                self.gen_binop(op, lt, rt);
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
    fn emit_data(&mut self, prog: &Program, live: &std::collections::HashSet<String>) {
        // an unreferenced static never earns bytes (matches the text side)
        let emit = |g: &Global| !g.is_extern && (!g.is_static || live.contains(&g.name));
        // strings
        if !prog.strings.is_empty() {
            self.out.push_str("\t.align 16\n\t.data\n");
            for (i, s) in prog.strings.iter().enumerate() {
                writeln!(self.out, "{}_{i}:", self.str_prefix).unwrap();
                self.out.push_str("\t.dc.b ");
                let parts: Vec<String> = s.iter().map(|b| format!("${b:02X}")).collect();
                self.out.push_str(&parts.join(","));
                self.out.push('\n');
            }
            self.out.push_str("\t.even\n");
        }
        // Globals split by content: only a nonzero initializer earns .data
        // bytes in the image. Uninitialized AND all-zero-initialized globals
        // go to .bss — C zero-initializes both, the loader/startup clears
        // .bss, and emitting literal zeros put 607KB of padding into a 2MB
        // console image (adoption report round 2, item 3). .bss is emitted
        // last so --elf-obj's section carve stays contiguous.
        let is_zero = |g: &Global| match &g.init {
            None => true,
            Some(bytes) => bytes.iter().all(|b| matches!(b, InitByte::Byte(0))),
        };
        let align_line = |out: &mut String, g: &Global| {
            if g.align > 2 {
                writeln!(out, "\t.align {}", g.align).unwrap();
            } else {
                out.push_str("\t.even\n");
            }
        };
        // Sections open on a 16-byte boundary so `aligned(N<=16)` members keep
        // their alignment after the linker places the section (the ELF section
        // addralign is 16 to match; a member's `.align` is blob-relative, so
        // start-of-section and member alignment must agree mod 16).
        // The `.align 16` BEFORE each section switch pads the *previous*
        // section, so the new one starts 16-aligned in the blob and
        // `aligned(N<=16)` members keep their alignment after the linker
        // places the section (ELF section addralign is 16 to match).
        let has_data = prog.globals.iter().any(|g| emit(g) && !is_zero(g));
        if has_data {
            self.out.push_str("\t.align 16\n\t.data\n");
            for g in &prog.globals {
                if !emit(g) || is_zero(g) {
                    continue;
                }
                align_line(&mut self.out, g);
                if !g.is_static {
                    writeln!(self.out, "\t.globl {}", mangle(&g.name)).unwrap();
                }
                writeln!(self.out, "{}:", mangle(&g.name)).unwrap();
                emit_init(&mut self.out, g.init.as_ref().unwrap());
            }
        }
        let has_bss = prog.globals.iter().any(|g| emit(g) && is_zero(g));
        if has_bss {
            self.out.push_str("\t.align 16\n\t.bss\n");
            for g in &prog.globals {
                if !emit(g) || !is_zero(g) {
                    continue;
                }
                let sz = ((g.ty.size().max(1) + 1) / 2) * 2;
                align_line(&mut self.out, g);
                if !g.is_static {
                    writeln!(self.out, "\t.globl {}", mangle(&g.name)).unwrap();
                }
                writeln!(self.out, "{}:", mangle(&g.name)).unwrap();
                writeln!(self.out, "\t.ds.b {sz}").unwrap();
            }
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

/// The integer value of a folded immediate operand (`#N`), if it is one.
fn parse_imm(src: &str, is_imm: bool) -> Option<i64> {
    if !is_imm {
        return None;
    }
    src.strip_prefix('#')?.parse().ok()
}

/// Normalize a GNU-as m68k line for jas: `%d0`/`%sr` → `d0`/`sr`, `%%` → `%`.
/// (Basic asm strings in the ports are written in gas syntax.)
fn normalize_gas_asm(line: &str) -> String {
    let b: Vec<char> = line.chars().collect();
    let mut out = String::with_capacity(line.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == '%' {
            if b.get(i + 1) == Some(&'%') {
                out.push('%');
                i += 2;
                continue;
            }
            if b.get(i + 1).map(|c| c.is_ascii_alphabetic()).unwrap_or(false) {
                i += 1; // drop the register prefix
                continue;
            }
        }
        out.push(b[i]);
        i += 1;
    }
    out
}

/// Which callee-saved registers a generated function body actually names, in
/// save order. Token-boundary matching so a symbol like `fixed2int` doesn't
/// count as a `d2` use (a false positive would only cost an extra save, but
/// the whole point here is not paying for registers a leaf never touches).
fn used_callee_saved(body: &str) -> Vec<&'static str> {
    const REGS: &[&str] = &["d2", "d3", "d4", "d5", "d6", "d7", "a2", "a3", "a4", "a5"];
    let b = body.as_bytes();
    let boundary = |c: u8| !(c.is_ascii_alphanumeric() || c == b'_');
    REGS.iter()
        .copied()
        .filter(|r| {
            body.match_indices(r).any(|(i, _)| {
                let before = i == 0 || boundary(b[i - 1]);
                let after = i + r.len() >= b.len() || boundary(b[i + r.len()]);
                before && after
            })
        })
        .collect()
}

/// A scalar type that occupies a full 4-byte longword (int, long, pointer,
/// fixed) — safe to read/write as a `.l` memory operand. Excludes sub-int types
/// (whose memory image is 1–2 bytes) and aggregates.
fn is_scalar4(ty: &Type) -> bool {
    ty.size() == 4 && !matches!(&**ty, TypeK::Array(..) | TypeK::Struct { .. } | TypeK::Func { .. })
}

/// Walk an expression, counting variable references and recording which
/// variables have their address taken (`&x`) — the latter can't live in a
/// register.
fn analyze_expr(e: &Expr, refs: &mut HashMap<String, usize>, addr: &mut std::collections::HashSet<String>) {
    match &e.kind {
        ExprK::Var(n) => *refs.entry(n.clone()).or_default() += 1,
        ExprK::Num(_) | ExprK::StrLit(_) => {}
        ExprK::Unary(UnOp::Addr, inner) => {
            if let ExprK::Var(n) = &inner.kind {
                addr.insert(n.clone());
            }
            analyze_expr(inner, refs, addr);
        }
        ExprK::Unary(_, a) | ExprK::Cast(a) | ExprK::Member(a, _) | ExprK::PostIncDec(a, _) => {
            analyze_expr(a, refs, addr)
        }
        ExprK::Binary(_, a, b) | ExprK::Assign(a, b) | ExprK::Comma(a, b) => {
            analyze_expr(a, refs, addr);
            analyze_expr(b, refs, addr);
        }
        ExprK::Cond(c, t, f) => {
            analyze_expr(c, refs, addr);
            analyze_expr(t, refs, addr);
            analyze_expr(f, refs, addr);
        }
        ExprK::Call(callee, args) => {
            analyze_expr(callee, refs, addr);
            for a in args {
                analyze_expr(a, refs, addr);
            }
        }
    }
}

/// Walk a statement (and any nested statements/initializers) for the analysis.
fn analyze_stmt(s: &Stmt, refs: &mut HashMap<String, usize>, addr: &mut std::collections::HashSet<String>) {
    let mut init = |i: &Init, refs: &mut _, addr: &mut _| {
        fn walk(i: &Init, refs: &mut HashMap<String, usize>, addr: &mut std::collections::HashSet<String>) {
            match i {
                Init::Scalar(e) => analyze_expr(e, refs, addr),
                Init::List(items) => items.iter().for_each(|it| walk(it, refs, addr)),
            }
        }
        walk(i, refs, addr);
    };
    match s {
        Stmt::Expr(e) | Stmt::Return(Some(e)) => analyze_expr(e, refs, addr),
        Stmt::Return(None) | Stmt::Break | Stmt::Continue | Stmt::Goto(_)
        | Stmt::Case(_) | Stmt::Default(_) | Stmt::Null | Stmt::Asm(_) => {}
        Stmt::AsmExt { output, input, .. } => {
            if let Some((_, e)) = output {
                analyze_expr(e, refs, addr);
            }
            if let Some(e) = input {
                analyze_expr(e, refs, addr);
            }
        }
        Stmt::If(c, t, e) => {
            analyze_expr(c, refs, addr);
            analyze_stmt(t, refs, addr);
            if let Some(e) = e {
                analyze_stmt(e, refs, addr);
            }
        }
        Stmt::While(c, b) | Stmt::DoWhile(b, c) => {
            analyze_expr(c, refs, addr);
            analyze_stmt(b, refs, addr);
        }
        Stmt::For(i, c, st, b) => {
            if let Some(i) = i {
                analyze_stmt(i, refs, addr);
            }
            if let Some(c) = c {
                analyze_expr(c, refs, addr);
            }
            if let Some(st) = st {
                analyze_expr(st, refs, addr);
            }
            analyze_stmt(b, refs, addr);
        }
        Stmt::Block(ss) => ss.iter().for_each(|s| analyze_stmt(s, refs, addr)),
        Stmt::Switch(e, b, _, _) => {
            analyze_expr(e, refs, addr);
            analyze_stmt(b, refs, addr);
        }
        Stmt::Label(_, b) => analyze_stmt(b, refs, addr),
        Stmt::Decl(_, _, Some(i)) => init(i, refs, addr),
        Stmt::Decl(_, _, None) => {}
    }
}

fn mangle(name: &str) -> String {
    // C linkage: no prefix, matching the m68k-elf (GCC/ELF) convention the real
    // Jaguar ports and their hand-written asm use — so a C `gpu_kernel` reference
    // resolves to the asm's `gpu_kernel` label at link time. (The a.out `_`
    // prefix would leave every cross-language symbol unresolved.)
    name.to_string()
}
