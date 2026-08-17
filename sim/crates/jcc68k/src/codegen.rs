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
    /// The unit's string-literal pool, needed at statement level because
    /// `char buf[N] = "text"` must copy the CHARACTERS into the frame rather
    /// than store a pointer to the pool.
    strings: Vec<Vec<u8>>,
    ret_label: String,
    /// The current function's declared return type. `return e;` converts to it
    /// like any other assignment, so a `char`/`short` result is narrowed before
    /// it reaches D0 — otherwise a caller reading the full 32 bits sees the
    /// untruncated value.
    ret_ty: Type,
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
    /// Same, for held lvalue addresses (the callee-saved address registers minus
    /// any claimed by a pointer local).
    apool: Vec<&'static str>,
}

/// Whether a register name denotes an address register (A0–A7).
///
/// These are not interchangeable with data registers on the 68000: they are
/// illegal as the source of AND/OR/EOR, `tst`/byte ops don't accept them, and
/// arithmetic into one is spelled `adda`. Every site that emits an instruction
/// naming a [`Gen::reg_of`] register has to ask.
fn is_areg(r: &str) -> bool {
    r.as_bytes().first() == Some(&b'a') && r.len() == 2 && r.as_bytes()[1].is_ascii_digit()
}

/// Callee-saved data registers used as the expression eval stack (they survive
/// function calls and the runtime helpers, which all preserve d2–d7).
const DTEMP_REGS: &[&str] = &["d2", "d3", "d4", "d5", "d6", "d7"];
/// Callee-saved address registers for held lvalue addresses.
const ATEMP_REGS: &[&str] = &["a2", "a3", "a4", "a5"];

/// A 68000 effective address that a load/store can name directly.
///
/// The point of this type is that the 68000 encodes a 16-bit displacement in
/// the instruction for free, so a struct-field offset or a frame slot costs
/// nothing extra. Materializing every address into A0 first (`move.l d0,a0` +
/// `adda.l #off,a0` + `move.l (a0),d0`) spends three instructions on what
/// `move.l off(a0),d0` does in one. [`Gen::addr_ea`] builds these instead.
#[derive(Clone, Debug, PartialEq)]
enum Ea {
    /// `off(areg)`, rendered `(areg)` when the displacement is zero.
    Disp(i32, String),
    /// `sym+off` — absolute long.
    Abs(String, i32),
}

impl Ea {
    fn render(&self) -> String {
        match self {
            Ea::Disp(0, r) => format!("({r})"),
            Ea::Disp(o, r) => format!("{o}({r})"),
            Ea::Abs(s, 0) => s.clone(),
            Ea::Abs(s, o) if *o > 0 => format!("{s}+{o}"),
            Ea::Abs(s, o) => format!("{s}{o}"), // negative prints its own sign
        }
    }

    /// Whether this address survives evaluating an arbitrary subexpression.
    /// A6 is the frame pointer and absolutes are link-time constants; A0 is
    /// caller-saved scratch that any nested call or helper will clobber.
    fn is_stable(&self) -> bool {
        match self {
            Ea::Disp(_, r) => r == "a6",
            Ea::Abs(..) => true,
        }
    }

    /// The 68000 displacement field is a signed 16-bit word.
    fn disp_ok(off: i32) -> bool {
        (-32768..=32767).contains(&off)
    }

    /// This address plus a constant byte offset, when it still encodes.
    fn plus(&self, d: i32) -> Option<Ea> {
        match self {
            Ea::Disp(o, r) => {
                let n = o.checked_add(d)?;
                Ea::disp_ok(n).then(|| Ea::Disp(n, r.clone()))
            }
            Ea::Abs(s, o) => Some(Ea::Abs(s.clone(), o.checked_add(d)?)),
        }
    }
}

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
        strings: prog.strings.clone(),
        ret_label: String::new(),
        ret_ty: t_int(),
        break_labels: Vec::new(),
        cont_labels: Vec::new(),
        dtemp: 0,
        atemp: 0,
        reg_of: HashMap::new(),
        dpool: Vec::new(),
        apool: Vec::new(),
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
        Stmt::AsmExt { template, output, input, .. } => {
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
        let slot = self.apool.get(self.atemp).map(|r| r.to_string()).unwrap_or_else(|| "-(a7)".into());
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
        // Passing or returning a struct BY VALUE is not implemented, and the
        // code that used to come out was silently wrong rather than absent:
        // `gen_call` pushes an aggregate argument's ADDRESS, so the callee
        // mutated the caller's object (pass-by-reference), only 4 bytes were
        // cleaned from the stack, and a struct return assigned 4 bytes — the
        // address — into the destination. Refuse it instead, the way the
        // 64-bit types are refused: a diagnostic is recoverable, a silent
        // miscompile in a renderer is not.
        if matches!(&*f.ret, TypeK::Struct { .. }) {
            return Err(format!(
                "{}: returning a struct by value is not supported on this target — \
                 return it through an out-pointer parameter instead",
                f.name
            ));
        }
        if let Some((pn, _)) = f.params.iter().find(|(_, t)| matches!(&**t, TypeK::Struct { .. })) {
            return Err(format!(
                "{}: parameter `{pn}` passes a struct by value, which is not supported on \
                 this target — pass a pointer to it instead",
                f.name
            ));
        }
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
        // Pointers go to ADDRESS registers, everything else to data registers.
        // This is the difference between `move.l 8(a2),d0` and the three-step
        // `move.l d6,d0 / move.l d0,a0 / move.l 8(a0),d0` — a pointer parked in
        // a data register has to be ferried into A0 before every single
        // dereference, which is most of what struct-walking code does.
        const LOCAL_DREGS: &[&str] = &["d7", "d6", "d5", "d4"];
        // A5 is deliberately left out: the eval stack needs at least one address
        // register for held lvalue addresses before it starts spilling to A7.
        const LOCAL_AREGS: &[&str] = &["a2", "a3", "a4"];
        // Registers an inline asm says it destroys are off the table entirely —
        // for locals and for the evaluation stack alike.
        let banned = collect_asm_clobbers(&f.body);
        let usable = |set: &[&'static str]| -> Vec<&'static str> {
            set.iter().copied().filter(|r| !banned.contains(*r)).collect()
        };
        let (local_aregs, local_dregs) = (usable(LOCAL_AREGS), usable(LOCAL_DREGS));
        let mut claimed: Vec<&str> = Vec::new();
        let (ptrs, others): (Vec<_>, Vec<_>) =
            cand.iter().partition(|(_, t)| matches!(&***t, TypeK::Ptr(_)));
        for ((n, _), r) in ptrs.iter().zip(&local_aregs) {
            self.reg_of.insert(n.to_string(), r.to_string());
            claimed.push(r);
        }
        for ((n, _), r) in others.iter().zip(&local_dregs) {
            self.reg_of.insert(n.to_string(), r.to_string());
            claimed.push(r);
        }
        self.dpool = usable(DTEMP_REGS).into_iter().filter(|r| !claimed.contains(r)).collect();
        self.apool = usable(ATEMP_REGS).into_iter().filter(|r| !claimed.contains(r)).collect();

        let param_names: std::collections::HashSet<&str> =
            f.params.iter().map(|(n, _)| n.as_str()).collect();
        let mut poff = 8i32;
        for (pn, pt) in &f.params {
            // A register param still receives its arg in the stack slot; we copy
            // it to the register in the prologue below.
            //
            // Every argument occupies a full 32-bit slot, so a `char`/`short`
            // parameter's value sits in the LOW end of its slot on this
            // big-endian chip: at +3 for a byte, +2 for a word. Addressing the
            // slot from its base and reading a word there returns the zero (or
            // sign) padding instead of the argument. That was the joypad strobe
            // bug — `strobe(0x81FE)` read sel == 0, so all four scan columns
            // wrote the same value and the pad reported nothing pressed.
            //
            // Aggregates are exempt: `gen_call` pushes an aggregate argument's
            // *address*, so those slots really are 4-byte pointers. Registered
            // params are exempt by construction — `is_scalar4` only promotes
            // 4-byte types, so the prologue's `move.l off(a6)` never sees an
            // adjusted offset.
            let sz = pt.size().max(1) as i32;
            let scalar = !matches!(&**pt, TypeK::Array(..) | TypeK::Struct { .. } | TypeK::Func { .. });
            let lo = if scalar && sz < 4 { 4 - sz } else { 0 };
            self.frame.insert(pn.clone(), poff + lo);
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
        self.ret_ty = f.ret.clone(); // set before the body: `return` reads it

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
            Stmt::AsmExt { template, output, input, .. } => {
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
                    // Narrow a char/short result to its declared width, so a
                    // caller reading the full 32 bits of D0 cannot see the
                    // untruncated value (`unsigned char f(){return 0x1FF;}`
                    // must yield 255, not 511).
                    //
                    // Deliberately limited to sub-word *integer* returns rather
                    // than a general conversion to the return type. `float` here
                    // is raw 16.16 fixed, and this project's convention is that
                    // returning one through an `int` hands back the raw word —
                    // see `fixed_raw_repr`. A full C conversion would silently
                    // change that for every ported source file.
                    //
                    // `cast` never narrows — it relies on the destination store
                    // to truncate, and a return has no store. Casting *from* the
                    // narrow return type *to* int runs its widening path, which
                    // is exactly the mask-and-re-extend this needs.
                    let rt = self.ret_ty.clone();
                    if matches!(&*rt, TypeK::Int { size: 1 | 2, .. }) {
                        self.cast(&rt, &t_int());
                    }
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
                    if self.try_str_array_init(off, ty, init) {
                        return Ok(());
                    }
                    // `struct P y = x;` — an aggregate initialized from another
                    // aggregate is a copy, not a pointer store.
                    if matches!(&**ty, TypeK::Struct { .. }) {
                        if let Init::Scalar(e) = init {
                            self.gen_expr(e)?; // source address in D0
                            self.line("move.l d0,a0");
                            self.line(&format!("lea {off}(a6),a1"));
                            self.copy_block(ty.size());
                            return Ok(());
                        }
                    }
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
                let ea = self.addr_ea(e)?;
                self.load_ea(&e.ty, &ea);
            }
            ExprK::Unary(UnOp::Addr, inner) => {
                let ea = self.addr_ea(inner)?;
                self.ea_addr_to_d0(&ea);
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
                // An explicit cast is a VALUE conversion, and `cast` never
                // narrows — it leaves truncation to the destination store, of
                // which a cast expression has none. Without this `(short)v` and
                // `(unsigned char)v` were complete no-ops: the full 32-bit
                // value flowed straight into the surrounding expression.
                // Casting *from* the narrow type *to* int runs cast's widening
                // path, which is the mask-and-re-extend this needs. Same shape
                // as the sub-word `return` fix.
                if matches!(&*e.ty, TypeK::Int { size: 1 | 2, .. }) {
                    self.cast(&e.ty, &t_int());
                }
            }
            ExprK::Assign(lhs, rhs) => {
                // `y = x` on a struct copies the whole object. The RHS is an
                // aggregate rvalue, i.e. its *address* lands in D0, so hold
                // that across the destination's address computation and then
                // block-copy.
                if matches!(&*lhs.ty, TypeK::Struct { .. }) {
                    let sz = lhs.ty.size();
                    self.gen_expr(rhs)?; // source address in D0
                    let slot = self.push_dtemp();
                    let dst = self.addr_ea(lhs)?;
                    self.materialize_ea(&dst); // destination address in A0
                    self.line("move.l a0,a1");
                    self.pop_dtemp_to(&slot, "d0");
                    self.line("move.l d0,a0");
                    self.copy_block(sz);
                    // The value of the assignment is the destination object,
                    // and an aggregate rvalue *is* its address — so hand back
                    // where A1 started, not where the copy left it. Without
                    // this `z = y = x` silently produced garbage instead of
                    // chaining.
                    self.line("move.l a1,d0");
                    self.line(&format!("sub.l #{sz},d0"));
                    return Ok(());
                }
                if let ExprK::Var(name) = &lhs.kind {
                    if let Some(r) = self.reg_of.get(name).cloned() {
                        // register-allocated local → assign the register directly
                        self.gen_expr(rhs)?;
                        self.cast(&rhs.ty, &lhs.ty);
                        self.line(&format!("move.l d0,{r}"));
                        return Ok(());
                    }
                }
                let ea = self.addr_ea(lhs)?;
                if self.ea_stable_across(&ea, rhs) {
                    // A frame slot or absolute cannot be disturbed by evaluating
                    // the RHS, so it needs no address register held across it.
                    self.gen_expr(rhs)?; // value in D0
                    self.cast(&rhs.ty, &lhs.ty); // implicit conversion (int↔fixed, widen)
                    self.store_ea(&lhs.ty, &ea);
                } else {
                    self.materialize_ea(&ea);
                    let slot = self.push_atemp(); // hold dest addr in a callee-saved areg
                    self.gen_expr(rhs)?; // value in D0
                    self.cast(&rhs.ty, &lhs.ty);
                    self.pop_atemp_to(&slot, "a0"); // restore dest
                    self.store(&lhs.ty);
                }
                // result of assignment is the stored value (already in D0)
            }
            ExprK::PostIncDec(lhs, delta) => {
                if let ExprK::Var(name) = &lhs.kind {
                    if let Some(r) = self.reg_of.get(name).cloned() {
                        // register local: result is the old value, then adjust
                        self.line(&format!("move.l {r},d0"));
                        self.load_imm_into("d1", *delta as i32);
                        // adding into an address register is ADDA, not ADD
                        let add = if is_areg(&r) { "adda.l" } else { "add.l" };
                        self.line(&format!("{add} d1,{r}"));
                        return Ok(());
                    }
                }
                // The EA is computed once and reused for both the load and the
                // store: nothing between them touches A0, so the address never
                // needs parking in a callee-saved register.
                let ea = self.addr_ea(lhs)?;
                self.load_ea(&lhs.ty, &ea); // old value in D0
                let dslot = self.push_dtemp(); // hold old value (the result)
                self.load_imm_into("d1", *delta as i32);
                self.line("add.l d1,d0");
                self.store_ea(&lhs.ty, &ea);
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
                    // When the rhs is parked in a register it is already a legal
                    // instruction source, so the binop reads it where it sits;
                    // only a stack-spilled operand has to come back through D1.
                    let rhs = if slot == "-(a7)" {
                        self.pop_dtemp_to(&slot, "d1");
                        "d1".to_string()
                    } else {
                        self.dtemp -= 1; // release the slot without a copy
                        slot
                    };
                    self.gen_binop(*op, &a.ty, &b.ty, &rhs);
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
        let slot = self.push_dtemp(); // save the value across the address computation
        let ea = self.addr_ea(lv)?;
        self.pop_dtemp_to(&slot, "d0");
        self.store_ea(&lv.ty, &ea);
        Ok(())
    }

    /// The effective address of an lvalue, computing as little as possible.
    ///
    /// Frame slots and globals need no code at all — they are a displacement
    /// off A6 or an absolute. Only a genuinely computed pointer costs an A0
    /// materialization, and even then the field offset rides along in the
    /// displacement rather than in an `adda.l`.
    fn addr_ea(&mut self, e: &Expr) -> Result<Ea, String> {
        match &e.kind {
            ExprK::Var(name) => {
                if self.reg_of.contains_key(name) {
                    // A register-allocated local has no address; reads/writes are
                    // intercepted before gen_addr, and `&x` locals are excluded
                    // from allocation, so reaching here is a compiler bug.
                    return Err(format!("{}: address of register-allocated `{name}`", e.line));
                }
                if let Some(off) = self.frame.get(name).copied() {
                    if Ea::disp_ok(off) {
                        return Ok(Ea::Disp(off, "a6".into()));
                    }
                    // Frame deeper than a 16-bit displacement: compute it.
                    self.line(&format!("lea {off}(a6),a0"));
                    return Ok(Ea::Disp(0, "a0".into()));
                }
                // known global, or unknown → treat as extern global
                Ok(Ea::Abs(mangle(name), 0))
            }
            ExprK::Unary(UnOp::Deref, inner) => self.ptr_ea(inner, 0),
            ExprK::Member(base_addr, off) => self.ptr_ea(base_addr, *off as i32),
            _ => Err(format!("{}: not an lvalue", e.line)),
        }
    }

    /// The effective address of `*(ptr) + off`, where `ptr`'s *value* is the
    /// base address. Folds the two shapes that dominate real code: taking the
    /// address of an lvalue (`&s` → `s`'s own EA, so `s.field` needs no
    /// arithmetic at all) and adding a constant (already scaled to bytes by
    /// the parser, so `tab[3]` collapses to one absolute).
    fn ptr_ea(&mut self, ptr: &Expr, off: i32) -> Result<Ea, String> {
        // A pointer local already living in an address register *is* the base
        // register — `p->z` is one `move.l 8(a2),d0`, no address computation.
        if let ExprK::Var(name) = &ptr.kind {
            if let Some(r) = self.reg_of.get(name).cloned() {
                if is_areg(&r) && Ea::disp_ok(off) {
                    return Ok(Ea::Disp(off, r));
                }
            }
        }
        // An array lvalue *is* its own address (C's array-to-pointer decay), so
        // `m->m[k]` indexes straight off the base register. Without this the
        // aggregate load path hands the address through D0 and costs a
        // `movea.l` on every element access.
        if matches!(&*ptr.ty, TypeK::Array(..))
            && matches!(
                &ptr.kind,
                ExprK::Var(_) | ExprK::Member(..) | ExprK::Unary(UnOp::Deref, _)
            )
        {
            let base = self.addr_ea(ptr)?;
            if let Some(ea) = base.plus(off) {
                return Ok(ea);
            }
            self.materialize_ea(&base);
            return self.a0_plus(off);
        }
        match &ptr.kind {
            ExprK::Unary(UnOp::Addr, inner) => {
                let base = self.addr_ea(inner)?;
                if let Some(ea) = base.plus(off) {
                    return Ok(ea);
                }
                self.materialize_ea(&base);
                return self.a0_plus(off);
            }
            ExprK::Binary(BinOp::Add, a, b) => {
                if let ExprK::Num(n) = &b.kind {
                    if let Some(sum) = (*n as i32).checked_add(off) {
                        return self.ptr_ea(a, sum);
                    }
                }
            }
            _ => {}
        }
        self.gen_expr(ptr)?; // pointer value in D0
        self.line("move.l d0,a0");
        self.a0_plus(off)
    }

    /// An EA for `off(a0)`, spending an `adda.l` only if the displacement
    /// does not fit the instruction.
    fn a0_plus(&mut self, off: i32) -> Result<Ea, String> {
        if Ea::disp_ok(off) {
            Ok(Ea::Disp(off, "a0".into()))
        } else {
            self.line(&format!("adda.l #{off},a0"));
            Ok(Ea::Disp(0, "a0".into()))
        }
    }

    /// Whether `ea` still names the same address after `rhs` has been evaluated.
    ///
    /// Beyond the unconditionally-stable cases, an address based on a
    /// register-allocated pointer local is stable too: A2–A5 are callee-saved,
    /// so calls and helpers preserve them, and the only thing that can change
    /// one is an assignment to that very variable. This is what lets `o->x = …`
    /// store straight through the pointer instead of parking its address in
    /// another register across the right-hand side.
    fn ea_stable_across(&self, ea: &Ea, rhs: &Expr) -> bool {
        if ea.is_stable() {
            return true;
        }
        let Ea::Disp(_, r) = ea else { return false };
        match self.reg_of.iter().find(|(_, reg)| *reg == r) {
            Some((name, _)) => !assigns_to(rhs, name),
            None => false, // A0 scratch, or an eval-stack slot
        }
    }

    /// Force an EA into A0, for the paths that need a real address register.
    fn materialize_ea(&mut self, ea: &Ea) {
        if *ea == Ea::Disp(0, "a0".into()) {
            return; // already there
        }
        self.line(&format!("lea {},a0", ea.render()));
    }

    /// Leave the *address* denoted by `ea` in D0 (for `&x` and aggregate rvalues).
    fn ea_addr_to_d0(&mut self, ea: &Ea) {
        match ea {
            Ea::Disp(0, r) => self.line(&format!("move.l {r},d0")),
            Ea::Abs(s, 0) => self.line(&format!("move.l #{s},d0")),
            _ => {
                self.materialize_ea(ea);
                self.line("move.l a0,d0");
            }
        }
    }

    fn gen_call(&mut self, callee: &Expr, args: &[Expr]) -> Result<(), String> {
        // Catch a by-value struct argument at the call site too: an `extern`
        // callee never reaches `gen_function`, so the definition-side check
        // above cannot see it.
        if let Some(a) = args.iter().find(|a| matches!(&*a.ty, TypeK::Struct { .. })) {
            return Err(format!(
                "{}: passing a struct by value is not supported on this target — \
                 pass a pointer to it instead",
                a.line
            ));
        }
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
    /// Multiply D0 by a constant that is a sum or difference of two powers of
    /// two, using shifts instead of the `__mulsi3` helper.
    ///
    /// The 68000 has no 32x32 multiply, so every `i * sizeof(struct)` in an
    /// array subscript became a subroutine call — the single most expensive
    /// thing in a loop that walks an array of structs. Both identities need one
    /// scratch register and no call:
    ///
    /// ```text
    /// x * (2^a + 2^b) == ((x << (a-b)) + x) << b
    /// x * (2^a - 2^b) == ((x << (a-b)) - x) << b
    /// ```
    ///
    /// Signedness doesn't enter into it: the low 32 bits of a product are the
    /// same either way. Shift counts are capped at 8, the largest the immediate
    /// form encodes, which still covers every plausible element size.
    fn fold_mul_const(&mut self, n: i64) -> bool {
        if n <= 2 || n > 0xFFFF {
            return false;
        }
        let b = n.trailing_zeros();
        // n == 2^a + 2^b
        let sum = (n.count_ones() == 2).then(|| (63 - n.leading_zeros(), true));
        // n == 2^a - 2^b  ⇔  n + 2^b is a power of two
        let diff = {
            let m = n + (1i64 << b);
            (m.count_ones() == 1).then(|| (63 - m.leading_zeros(), false))
        };
        let Some((a, is_add)) = sum.or(diff) else {
            return false;
        };
        let (shift1, shift2) = (a - b, b);
        if shift1 < 1 || shift1 > 8 || shift2 > 8 {
            return false;
        }
        self.line("move.l d0,d1");
        self.line(&format!("asl.l #{shift1},d0"));
        self.line(if is_add { "add.l d1,d0" } else { "sub.l d1,d0" });
        if shift2 > 0 {
            self.line(&format!("asl.l #{shift2},d0"));
        }
        true
    }

    fn fold_pow2(&mut self, op: BinOp, lt: &Type, rt: &Type, n: i64) -> bool {
        let unsigned = forces_unsigned(lt) || forces_unsigned(rt);
        // A multiply by a non-power-of-two still beats the helper call when the
        // constant is two powers of two apart — which every odd-sized struct
        // index is (`p[i]` on a 12-byte struct is `i * 12`).
        if matches!(op, BinOp::Mul) && n > 0 && (n & (n - 1)) != 0 {
            return self.fold_mul_const(n);
        }
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
    fn call_runtime_rhs(&mut self, rhs: &str, name: &str) {
        self.line(&format!("move.l {rhs},-(a7)"));
        self.line("move.l d0,-(a7)");
        self.line(&format!("jsr {name}"));
        self.line("addq.l #8,a7");
    }

    // ── binops (D0 = D0 op RHS) ───────────────────────────────────────────────
    /// `rhs` names the right operand's location — normally a data register the
    /// eval stack parked it in, so the value is used where it already sits
    /// instead of being copied into D1 first.
    fn gen_binop(&mut self, op: BinOp, lt: &Type, rt: &Type, rhs: &str) {
        let unsigned = forces_unsigned(lt) || forces_unsigned(rt);
        match op {
            BinOp::Add => self.line(&format!("add.l {rhs},d0")),
            BinOp::Sub => self.line(&format!("sub.l {rhs},d0")),
            BinOp::And => self.line(&format!("and.l {rhs},d0")),
            BinOp::Or => self.line(&format!("or.l {rhs},d0")),
            BinOp::Xor => self.line(&format!("eor.l {rhs},d0")),
            BinOp::Shl => self.line(&format!("asl.l {rhs},d0")),
            BinOp::Shr => {
                if unsigned {
                    self.line(&format!("lsr.l {rhs},d0"));
                } else {
                    self.line(&format!("asr.l {rhs},d0"));
                }
            }
            BinOp::Mul => {
                if lt.is_fixed() || rt.is_fixed() {
                    self.call_runtime_rhs(rhs, "__mulfix");
                } else {
                    self.call_runtime_rhs(rhs, "__mulsi3");
                }
            }
            BinOp::Div => {
                if lt.is_fixed() || rt.is_fixed() {
                    self.call_runtime_rhs(rhs, "__divfix");
                } else if unsigned {
                    self.call_runtime_rhs(rhs, "__udivsi3");
                } else {
                    self.call_runtime_rhs(rhs, "__divsi3");
                }
            }
            BinOp::Mod => {
                if unsigned {
                    self.call_runtime_rhs(rhs, "__umodsi3");
                } else {
                    self.call_runtime_rhs(rhs, "__modsi3");
                }
            }
            BinOp::Eq | BinOp::Ne | BinOp::Lt | BinOp::Le | BinOp::Gt | BinOp::Ge => {
                self.line(&format!("cmp.l {rhs},d0")); // sets flags for D0 - D1
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
                // A register-allocated local is itself a valid instruction
                // source — except an address register, which the 68000 rejects
                // as the source of AND/OR/EOR. Not worth encoding per-op: let
                // those fall back to the generic path.
                let r = self.reg_of[name].clone();
                (!is_areg(&r)).then_some((r, false))
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
            // The immediate shift count encodes 1–8, but a larger constant is
            // still better done as a short run of immediate shifts than by
            // materializing the count into a register and going through the
            // eval stack (`x >> 14` on 16.16 fixed point is everywhere).
            BinOp::Shl | BinOp::Shr => {
                matches!(&b.kind, ExprK::Num(n) if (1..=31).contains(n))
            }
            BinOp::LogAnd | BinOp::LogOr => false,
        };
        ok.then_some((src, is_imm))
    }

    /// Shift D0 by `src`, splitting a constant count larger than 8 into
    /// successive immediate shifts (the 68000 encodes only 1–8 per instruction).
    fn emit_shift(&mut self, mnemonic: &str, src: &str) {
        let Some(mut n) = src.strip_prefix('#').and_then(|s| s.parse::<u32>().ok()) else {
            self.line(&format!("{mnemonic} {src},d0")); // register count
            return;
        };
        if n >= 32 {
            // Shifting a 32-bit value by its whole width is undefined in C;
            // produce zero for the logical forms, the sign bit for arithmetic.
            n = if mnemonic == "asr.l" { 31 } else { 32 };
            if n == 32 {
                self.line("moveq #0,d0");
                return;
            }
        }
        while n > 0 {
            let step = n.min(8);
            self.line(&format!("{mnemonic} #{step},d0"));
            n -= step;
        }
    }

    /// Emit `op` with the right operand as a folded source (D0 already holds the
    /// left operand). Mirrors [`gen_binop`] but takes the rhs from `src`.
    fn fold_binop(&mut self, op: BinOp, lt: &Type, rt: &Type, src: &str, _is_imm: bool) {
        let unsigned = forces_unsigned(lt) || forces_unsigned(rt);
        match op {
            BinOp::Add => self.line(&format!("add.l {src},d0")),
            BinOp::Sub => self.line(&format!("sub.l {src},d0")),
            BinOp::And => self.line(&format!("and.l {src},d0")),
            BinOp::Or => self.line(&format!("or.l {src},d0")),
            BinOp::Xor => self.line(&format!("eori.l {src},d0")),
            BinOp::Shl => self.emit_shift("asl.l", src),
            BinOp::Shr => self.emit_shift(if unsigned { "lsr.l" } else { "asr.l" }, src),
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
                self.gen_binop(op, lt, rt, "d1");
            }
            BinOp::LogAnd | BinOp::LogOr => unreachable!("handled in gen_expr"),
        }
    }

    // ── loads / stores by size (A0 = address) ─────────────────────────────────
    /// Load the value at `ea` into D0, sign- or zero-extended to 32 bits.
    fn load_ea(&mut self, ty: &Type, ea: &Ea) {
        match &**ty {
            TypeK::Array(..) | TypeK::Struct { .. } | TypeK::Func { .. } => {
                // aggregate lvalue → its address is the value
                self.ea_addr_to_d0(ea);
            }
            _ => {
                let sz = ty.size();
                let signed = ty.is_signed();
                let a = ea.render();
                match sz {
                    1 => {
                        if signed {
                            self.line(&format!("move.b {a},d0"));
                            self.line("ext.w d0");
                            self.line("ext.l d0");
                        } else {
                            self.line("moveq #0,d0");
                            self.line(&format!("move.b {a},d0"));
                        }
                    }
                    2 => {
                        if signed {
                            self.line(&format!("move.w {a},d0"));
                            self.line("ext.l d0");
                        } else {
                            self.line("moveq #0,d0");
                            self.line(&format!("move.w {a},d0"));
                        }
                    }
                    _ => self.line(&format!("move.l {a},d0")),
                }
            }
        }
    }

    fn store(&mut self, ty: &Type) {
        // A0 = dest, D0 = value
        let ea = Ea::Disp(0, "a0".into());
        self.store_ea(ty, &ea);
    }

    /// Store D0 into `ea`, truncated to the lvalue's width.
    fn store_ea(&mut self, ty: &Type, ea: &Ea) {
        let a = ea.render();
        match ty.size() {
            1 => self.line(&format!("move.b d0,{a}")),
            2 => self.line(&format!("move.w d0,{a}")),
            _ => self.line(&format!("move.l d0,{a}")),
        }
    }

    /// Zero `size` bytes at `off(a6)` (used before an aggregate initializer so
    /// unlisted elements read as 0, per C).
    /// Copy `size` bytes from the address in A0 to the address in A1.
    ///
    /// Whole-struct assignment copies the OBJECT. Before this existed the
    /// aggregate fell through the scalar path, which stored four bytes — the
    /// source's *address* — over the destination's first field, so `y = x`
    /// produced pointer-shaped garbage.
    ///
    /// Same shape as [`Gen::clear_frame`]: a `dbra` loop over longs then the
    /// odd tail. `dbra` counts a 16-bit register, so this handles up to 256KB
    /// per struct — far past anything declarable here.
    fn copy_block(&mut self, size: u32) {
        if size == 0 {
            return;
        }
        let longs = size / 4;
        if longs > 0 {
            let lbl = self.l();
            self.load_imm_into("d0", longs as i32 - 1);
            self.lbl(&format!(".Lcpy_{lbl}"));
            self.line("move.l (a0)+,(a1)+");
            self.line(&format!("dbra d0,.Lcpy_{lbl}"));
        }
        for _ in 0..(size % 4) {
            self.line("move.b (a0)+,(a1)+");
        }
    }

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

    /// `char buf[N] = "text"` — copy the CHARACTERS into the frame slot and
    /// zero-fill the tail. Returns false when the initializer isn't that shape.
    ///
    /// Without this the scalar path evaluates the literal to its pool ADDRESS
    /// and stores four bytes of pointer over the first elements, leaving the
    /// rest of the array uninitialized — `char s[6] = "abc"` then sums to
    /// whatever the pointer bytes happened to be. File-scope initializers
    /// already emit the characters (`.dc.b $61,$62,$63,$00,$00,$00`); this
    /// makes locals agree with them.
    fn try_str_array_init(&mut self, off: i32, ty: &Type, init: &Init) -> bool {
        let (TypeK::Array(el, n), Init::Scalar(e)) = (&**ty, init) else {
            return false;
        };
        if el.size() != 1 {
            return false;
        }
        let ExprK::StrLit(idx) = &e.kind else {
            return false;
        };
        let Some(bytes) = self.strings.get(*idx).cloned() else {
            return false;
        };
        let n = *n as usize;
        // Zero the whole slot, then write only the non-zero characters — the
        // same clear-then-fill shape the braced-list path uses. A literal
        // longer than the array is truncated (C lets the NUL drop when the
        // characters exactly fill it); a shorter one leaves zeros behind.
        self.clear_frame(off, n as u32);
        for i in 0..n.min(bytes.len()) {
            if bytes[i] != 0 {
                self.line(&format!("move.b #{},{}(a6)", bytes[i], off + i as i32));
            }
        }
        true
    }

    /// Emit an aggregate/scalar initializer into the frame slot at `off(a6)`.
    fn gen_local_init(&mut self, off: i32, ty: &Type, init: &Init) -> Result<(), String> {
        if self.try_str_array_init(off, ty, init) {
            return Ok(());
        }
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
        //
        // Only a genuine integer source has a *width* to widen from. An array's
        // `size()` is its aggregate footprint, so without this guard a
        // `char[2]` decaying to a pointer looked like a 16-bit integer and got
        // masked with `and.l #$FFFF` — silently truncating the pointer to
        // garbage. Pointers and aggregates are already 32-bit values; they need
        // no conversion at all.
        if to.size() >= 4 && from.is_integer() {
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
                let sp = self.str_prefix.clone(); // out is borrowed mutably
                emit_init(&mut self.out, g.init.as_ref().unwrap(), &sp);
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
        // Leave the location counter EVEN. Initialized globals emit their exact
        // bytes, so a single `unsigned char g = 231;` ends the unit on an odd
        // address — and `--prog` concatenates startup + this unit + the runtime
        // with no realignment between them. Every runtime helper then began at
        // an odd address, so the first `jsr __umodsi3` took an ADDRESS ERROR
        // and vanished into the boot ROM: a program that used a byte-sized
        // global *and* any 32-bit divide, multiply or modulo simply ran away,
        // with `illegal` still reading 0 because a fetch fault is not an
        // illegal opcode. (`.bss` already rounds each object up to even, and
        // each section starts `.align 16`, which is why only this path bled.)
        self.out.push_str("\t.even\n");
    }
}

/// Emit a global's initializer image: coalesce runs of literal bytes into
/// `.dc.b` directives, and emit each symbol address as `.dc.l _sym+addend`.
/// `str_prefix` spells the per-unit string-pool labels.
fn emit_init(out: &mut String, init: &[InitByte], str_prefix: &str) {
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
            InitByte::Str(idx) => {
                flush(out, &mut run);
                out.push_str("\t.even\n");
                writeln!(out, "\t.dc.l {str_prefix}_{idx}").unwrap();
            }
        }
    }
    flush(out, &mut run);
}

/// Post-process the emitted assembly. Currently: drop `bra L` when the very
/// next line is `L:` (a branch to the following instruction) — both a size
/// optimization and a workaround for the assembler's short/long branch
/// oscillation on a zero displacement.
/// Split an operand list at the top-level comma — the one not inside `(...)`,
/// so `8(a0,d1.l),d2` splits after the closing paren.
fn split_operands(ops: &str) -> Option<(&str, &str)> {
    let mut depth = 0i32;
    for (i, c) in ops.char_indices() {
        match c {
            '(' => depth += 1,
            ')' => depth -= 1,
            ',' if depth == 0 => return Some((ops[..i].trim(), ops[i + 1..].trim())),
            _ => {}
        }
    }
    None
}

/// `(mnemonic, operands)` for an instruction line, or `None` for anything that
/// is not one (blank, directive, label).
fn split_insn(line: &str) -> Option<(&str, &str)> {
    let t = line.trim();
    if t.is_empty() || t.starts_with('.') || t.starts_with('*') || t.ends_with(':') {
        return None;
    }
    Some(match t.find(char::is_whitespace) {
        Some(i) => (&t[..i], t[i..].trim()),
        None => (t, ""),
    })
}

/// Mnemonic without its size suffix (`move.l` → `move`).
fn base_mnemonic(m: &str) -> &str {
    m.split('.').next().unwrap_or(m)
}

/// Whole-word search for a register name inside an operand string.
fn mentions(ops: &str, r: &str) -> bool {
    let b = ops.as_bytes();
    let boundary = |c: u8| !(c.is_ascii_alphanumeric() || c == b'_');
    ops.match_indices(r).any(|(i, _)| {
        let before = i == 0 || boundary(b[i - 1]);
        let after = i + r.len() >= b.len() || boundary(b[i + r.len()]);
        before && after
    })
}

fn is_machine_reg(s: &str) -> bool {
    let b = s.as_bytes();
    s.len() == 2 && matches!(b[0], b'd' | b'a') && b[1].is_ascii_digit()
}

/// Whether `r` is never read again before being overwritten, starting at `from`.
///
/// Conservative by construction: anything it cannot reason about (a label, a
/// branch, an unrecognized mnemonic touching `r`) counts as *live*, which only
/// ever costs an optimization, never correctness. The one non-obvious rule is
/// the calling convention — D0/D1/A0/A1 are caller-saved in the jcc68k ABI, so
/// a `jsr` kills them.
fn dead_after(lines: &[&str], from: usize, r: &str) -> bool {
    let scratch = matches!(r, "d0" | "d1" | "a0" | "a1");
    for line in &lines[from.min(lines.len())..] {
        let t = line.trim();
        if t.is_empty() {
            continue;
        }
        if t.ends_with(':') {
            return false; // a label may be reached with r live
        }
        let Some((m, ops)) = split_insn(t) else { continue };
        let base = base_mnemonic(m);
        match base {
            "jsr" | "bsr" => {
                return !mentions(ops, r) && scratch;
            }
            "rts" | "rte" => return r != "d0",
            "jmp" | "bra" => return false,
            _ if base.starts_with('b') || base.starts_with("db") => return false,
            _ => {}
        }
        match split_operands(ops) {
            Some((src, dst)) => {
                if mentions(src, r) {
                    return false; // read
                }
                if mentions(dst, r) {
                    // Only a full-width overwrite of exactly this register kills it.
                    let pure_write = matches!(base, "move" | "movea" | "moveq" | "lea" | "clr");
                    return pure_write && dst == r && (m.ends_with(".l") || m == "moveq");
                }
            }
            None => {
                if !ops.is_empty() && mentions(ops, r) {
                    return base == "clr" && ops.trim() == r;
                }
            }
        }
    }
    false
}

/// Fold `move.l A,R` into the single following instruction that reads `R`,
/// when `R` is dead afterwards.
///
/// This is what turns `move.l 8(a3),d0 / move.l d0,d2` into `move.l 8(a3),d2`
/// and `move.l d3,d1 / add.l d1,d0` into `add.l d3,d0`. Only the immediately
/// adjacent instruction is considered, which keeps the safety argument simple:
/// nothing can invalidate `A` in between because there is no in-between.
/// Whether an operand carries an addressing side effect (`-(an)` / `(an)+`).
fn is_auto(op: &str) -> bool {
    op.contains("-(") || op.contains(")+")
}

/// Whether an effective address provably names ordinary stack memory.
///
/// This is the safety gate on [`elim_redundant_loads`], and it is deliberately
/// narrow. The frontend **discards `volatile` on pointed-to types** — it keeps
/// the qualifier only on locals, to bar them from register promotion — so by
/// the time codegen runs, a hardware register read through a `volatile
/// uint32_t *` is indistinguishable from an ordinary struct field load. Reusing
/// a register for the second of two MMIO reads is a miscompile of exactly the
/// kind that leaves a Jaguar pad strobe reading stale data.
///
/// A6-relative memory is this compiler's own frame. Nothing the program can
/// map to hardware lives there, so folding those loads is sound regardless of
/// what the frontend forgot. Widening this to pointer dereferences requires
/// carrying `volatile` in the type system first.
fn ea_is_frame(ea: &str) -> bool {
    ea.ends_with("(a6)") && !is_auto(ea)
}

/// Local redundant-load elimination — the available-expressions optimization,
/// scoped to a basic block.
///
/// A second `move.l <ea>,dN` naming an address whose value is already sitting
/// in a register becomes a register copy, which `fold_copies` then usually
/// deletes outright. Straight-line pointer code reloads the same field
/// constantly (`p->w` twice in one expression is two `move.l 8(a2)`s), so this
/// fires on ordinary game code, not just contrived cases.
///
/// The alias model is deliberately blunt: any store through a named memory
/// operand forgets every remembered load. The one exception is both sound and
/// load-bearing — `-(a7)`/`(a7)+` push and pop touch memory *below* the stack
/// pointer, which no object the program can name overlaps, so argument
/// marshalling doesn't wipe the table. It does move A7, though, so anything
/// addressed off A7 is dropped. Calls clear everything except the arithmetic
/// runtime helpers, which touch no memory and preserve all but D0/D1/A0/A1.
/// Anything this pass does not positively recognize clears the table.
fn elim_redundant_loads(lines: &[&str]) -> (Vec<String>, bool) {
    /// jcc68k's own 32-bit mul/div helpers: pure arithmetic, no memory traffic.
    const PURE_HELPERS: &[&str] = &["__mulsi3", "__divsi3", "__udivsi3", "__modsi3", "__umodsi3"];
    /// Reads its operands, writes neither.
    const NO_WRITE: &[&str] = &["cmp", "cmpa", "cmpi", "cmpm", "tst", "btst"];
    /// `op src,dst` — writes `dst`.
    const WRITES_DST: &[&str] = &[
        "add", "adda", "addq", "addi", "sub", "suba", "subq", "subi", "and", "andi", "or", "ori",
        "eor", "eori", "asl", "asr", "lsl", "lsr", "rol", "ror", "muls", "mulu", "divs", "divu",
        "moveq", "lea", "move", "movea",
    ];
    /// `op dst` — writes its single operand.
    const WRITES_ONE: &[&str] = &["neg", "not", "clr", "ext", "extb", "swap", "tas"];

    fn kill_reg(avail: &mut Vec<(String, String)>, r: &str) {
        avail.retain(|(ea, reg)| reg != r && !mentions(ea, r));
    }

    let mut avail: Vec<(String, String)> = Vec::new(); // (effective address, register holding it)
    let mut out: Vec<String> = Vec::with_capacity(lines.len());
    let mut changed = false;

    for line in lines {
        if line.trim().is_empty() {
            out.push(line.to_string());
            continue;
        }
        let Some((m, ops)) = split_insn(line) else {
            avail.clear(); // a label may be a branch target: nothing survives it
            out.push(line.to_string());
            continue;
        };
        let base = base_mnemonic(m);

        if base == "jsr" || base == "bsr" {
            if base == "jsr" && PURE_HELPERS.contains(&ops.trim()) {
                for r in ["d0", "d1", "a0", "a1"] {
                    kill_reg(&mut avail, r);
                }
            } else {
                avail.clear();
            }
            out.push(line.to_string());
            continue;
        }

        // A push or pop shifts A7, invalidating everything addressed off it.
        if is_auto(ops) && mentions(ops, "a7") {
            kill_reg(&mut avail, "a7");
        }

        if m == "move.l" || m == "movea.l" {
            if let Some((src, dst)) = split_operands(ops) {
                let src_imm = src.starts_with('#');
                let src_mem = !is_machine_reg(src) && !src_imm;
                if src_mem && is_machine_reg(dst) && ea_is_frame(src) {
                    if let Some(held) = avail.iter().find(|(ea, _)| ea == src).map(|(_, r)| r.clone())
                    {
                        changed = true;
                        if held == dst {
                            continue; // value already in this very register
                        }
                        out.push(format!("\t{m} {held},{dst}"));
                        kill_reg(&mut avail, dst);
                        avail.push((src.to_string(), dst.to_string()));
                        continue;
                    }
                    kill_reg(&mut avail, dst);
                    avail.push((src.to_string(), dst.to_string()));
                    out.push(line.to_string());
                    continue;
                }
                if src_mem && is_machine_reg(dst) {
                    // A load this pass may not reason about (it could be MMIO):
                    // it still writes its destination.
                    kill_reg(&mut avail, dst);
                    out.push(line.to_string());
                    continue;
                }
                if !is_machine_reg(dst) {
                    // a store; pushes alias nothing nameable, so they keep the table
                    if !is_auto(dst) {
                        avail.clear();
                        if is_machine_reg(src) && ea_is_frame(dst) {
                            avail.push((dst.to_string(), src.to_string()));
                        }
                    }
                    out.push(line.to_string());
                    continue;
                }
                // register/immediate → register: the value travels with the copy
                let carried = is_machine_reg(src)
                    .then(|| avail.iter().find(|(_, r)| r == src).map(|(ea, _)| ea.clone()))
                    .flatten();
                kill_reg(&mut avail, dst);
                if let Some(ea) = carried.filter(|ea| !mentions(ea, dst)) {
                    avail.push((ea, dst.to_string()));
                }
                out.push(line.to_string());
                continue;
            }
        }

        if NO_WRITE.contains(&base) {
            out.push(line.to_string());
            continue;
        }
        let is_scc = base.len() == 3 && base.starts_with('s');
        let written = match split_operands(ops) {
            Some((_, dst)) if WRITES_DST.contains(&base) => Some(dst),
            None if WRITES_ONE.contains(&base) || is_scc => Some(ops.trim()),
            _ => None,
        };
        match written {
            Some(dst) if is_machine_reg(dst) => kill_reg(&mut avail, dst),
            Some(dst) if !is_auto(dst) => avail.clear(), // store to memory
            Some(_) => {}
            None => avail.clear(), // unrecognized: assume the worst
        }
        out.push(line.to_string());
    }
    (out, changed)
}

fn fold_copies(lines: &[&str]) -> (Vec<String>, bool) {
    let mut out: Vec<String> = Vec::with_capacity(lines.len());
    let mut changed = false;
    let mut i = 0;
    while i < lines.len() {
        if i + 1 < lines.len() {
            if let Some(folded) = try_fold(lines, i) {
                out.push(folded);
                changed = true;
                i += 2;
                continue;
            }
        }
        out.push(lines[i].to_string());
        i += 1;
    }
    (out, changed)
}

fn try_fold(lines: &[&str], i: usize) -> Option<String> {
    let (m0, ops0) = split_insn(lines[i])?;
    if m0 != "move.l" && m0 != "movea.l" {
        return None;
    }
    let (a, r) = split_operands(ops0)?;
    if !is_machine_reg(r) {
        return None;
    }
    // Source with an addressing side effect can't be duplicated or delayed.
    if a.contains("-(") || a.contains(")+") {
        return None;
    }
    let (m1, ops1) = split_insn(lines[i + 1])?;
    if !m1.ends_with(".l") {
        return None; // keep widths identical; sub-word ops change semantics
    }
    let base1 = base_mnemonic(m1);
    let (src1, dst1) = split_operands(ops1)?;
    // `cmp` reads both operands and writes neither, so a copy feeding its
    // destination can be folded too — `move.l d7,d0 / cmp.l d5,d0` is just
    // `cmp.l d5,d7`. The result must land in a data register to stay encodable.
    if base_mnemonic(m1) == "cmp" && dst1 == r && src1 != r {
        if !is_machine_reg(a) || !a.starts_with('d') {
            return None;
        }
        if !dead_after(lines, i + 2, r) {
            return None;
        }
        return Some(format!("\t{m1} {src1},{a}"));
    }
    // R must be read exactly once, as the whole source, and not written here.
    if src1 != r || mentions(dst1, r) {
        return None;
    }
    // The folding instruction must not clobber anything A depends on.
    if is_machine_reg(dst1) && mentions(a, dst1) {
        return None;
    }
    let a_is_areg = is_machine_reg(a) && a.starts_with('a');
    let a_is_mem = !is_machine_reg(a) && !a.starts_with('#');
    match base1 {
        // An address register is illegal as the source of these.
        "and" | "or" | "eor" if a_is_areg => return None,
        // `<ea>,Dn` forms need a data-register destination.
        "add" | "sub" | "cmp" | "and" | "or" | "eor" if a_is_mem && !dst1.starts_with('d') => {
            return None
        }
        "move" | "movea" | "add" | "sub" | "cmp" | "and" | "or" | "eor" => {}
        _ => return None, // unknown consumer: leave it alone
    }
    if !dead_after(lines, i + 2, r) {
        return None;
    }
    // An immediate source needs the `#`-form mnemonic spelling left as-is;
    // `move` into an address register is `movea`.
    let mnem = if base1 == "move" && is_machine_reg(dst1) && dst1.starts_with('a') {
        "movea.l".to_string()
    } else {
        m1.to_string()
    };
    Some(format!("\t{mnem} {a},{dst1}"))
}

/// `(scc, branch-if-condition-true, branch-if-condition-false)`.
const SCC_TO_BRANCH: &[(&str, &str, &str)] = &[
    ("seq", "beq", "bne"),
    ("sne", "bne", "beq"),
    ("slt", "blt", "bge"),
    ("sle", "ble", "bgt"),
    ("sgt", "bgt", "ble"),
    ("sge", "bge", "blt"),
    ("scs", "bcs", "bcc"),
    ("sls", "bls", "bhi"),
    ("shi", "bhi", "bls"),
    ("scc", "bcc", "bcs"),
];

/// Is `r` dead at the instruction following `label`?
fn dead_at_label(lines: &[&str], label: &str, r: &str) -> bool {
    let want = format!("{label}:");
    lines
        .iter()
        .position(|l| l.trim() == want)
        .is_some_and(|i| dead_after(lines, i + 1, r))
}

/// Collapse "materialize a boolean, then test it" into a conditional branch.
///
/// Every `if`/`while` condition compiles to `cmp / s<cc> d0 / and.l #1,d0 /
/// tst.l d0 / b<eq|ne>` — five instructions to use flags the `cmp` already set.
/// The comparison's own flags can drive the branch directly, provided the
/// boolean in D0 is genuinely unused on *both* successors.
fn fuse_compare_branch(lines: &[&str]) -> (Vec<String>, bool) {
    let mut out: Vec<String> = Vec::with_capacity(lines.len());
    let mut changed = false;
    let mut i = 0;
    while i < lines.len() {
        if i + 4 < lines.len() {
            if let Some(fused) = try_fuse(lines, i) {
                out.push(lines[i].to_string()); // keep the cmp
                out.push(fused);
                changed = true;
                i += 5;
                continue;
            }
        }
        out.push(lines[i].to_string());
        i += 1;
    }
    (out, changed)
}

fn try_fuse(lines: &[&str], i: usize) -> Option<String> {
    if split_insn(lines[i])?.0 != "cmp.l" {
        return None;
    }
    let (m1, o1) = split_insn(lines[i + 1])?;
    let entry = SCC_TO_BRANCH.iter().find(|(s, _, _)| *s == m1)?;
    if o1.trim() != "d0" {
        return None;
    }
    let (m2, o2) = split_insn(lines[i + 2])?;
    if m2 != "and.l" || o2.replace(' ', "") != "#1,d0" {
        return None;
    }
    let (m3, o3) = split_insn(lines[i + 3])?;
    if m3 != "tst.l" || o3.trim() != "d0" {
        return None;
    }
    let (m4, target) = split_insn(lines[i + 4])?;
    let branch = match base_mnemonic(m4) {
        "beq" => entry.2, // the boolean was false ⇒ the condition was false
        "bne" => entry.1,
        _ => return None,
    };
    // The boolean must be unused however control flows from here.
    if !dead_after(lines, i + 5, "d0") || !dead_at_label(lines, target, "d0") {
        return None;
    }
    let suffix = if m4.ends_with(".s") { ".s" } else { ".w" };
    Some(format!("\t{branch}{suffix} {target}"))
}

fn peephole(asm: &str) -> String {
    let stage1 = peephole_branches(asm);
    // Copy folding exposes more copy folding (a three-instruction chain
    // collapses one link per pass), so run to a fixpoint.
    let mut cur = stage1;
    for _ in 0..8 {
        let lines: Vec<&str> = cur.lines().collect();
        let (fused, c1) = fuse_compare_branch(&lines);
        let refs: Vec<&str> = fused.iter().map(|s| s.as_str()).collect();
        let (loaded, c3) = elim_redundant_loads(&refs);
        let refs: Vec<&str> = loaded.iter().map(|s| s.as_str()).collect();
        let (folded, c2) = fold_copies(&refs);
        if !c1 && !c2 && !c3 {
            break;
        }
        cur = folded.join("\n");
        cur.push('\n');
    }
    cur
}

fn peephole_branches(asm: &str) -> String {
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
/// Whether evaluating `e` can assign to the local named `name`.
///
/// Only a direct assignment or increment counts. A call cannot reach the
/// variable indirectly: taking its address would have disqualified it from
/// register allocation in the first place.
fn assigns_to(e: &Expr, name: &str) -> bool {
    let target_is = |lv: &Expr| matches!(&lv.kind, ExprK::Var(n) if n == name);
    match &e.kind {
        ExprK::Assign(lv, rhs) => target_is(lv) || assigns_to(lv, name) || assigns_to(rhs, name),
        ExprK::PostIncDec(lv, _) => target_is(lv) || assigns_to(lv, name),
        ExprK::Binary(_, a, b) | ExprK::Comma(a, b) => assigns_to(a, name) || assigns_to(b, name),
        ExprK::Unary(_, a) | ExprK::Cast(a) | ExprK::Member(a, _) => assigns_to(a, name),
        ExprK::Cond(c, a, b) => {
            assigns_to(c, name) || assigns_to(a, name) || assigns_to(b, name)
        }
        ExprK::Call(callee, args) => {
            assigns_to(callee, name) || args.iter().any(|a| assigns_to(a, name))
        }
        ExprK::Num(_) | ExprK::StrLit(_) | ExprK::Var(_) => false,
    }
}

/// Whether an operand forces the *unsigned* form of a comparison or division.
///
/// C's usual arithmetic conversions run integer PROMOTION first, and on this
/// target `int` is 32-bit, so it represents every `unsigned char` and
/// `unsigned short` value: those promote to **signed** `int` and do NOT make
/// the operation unsigned. Only an unsigned integer of `int` rank or wider
/// does — plus pointers, which always compare unsigned.
///
/// Testing `!is_signed()` instead gets this wrong for exactly the narrow
/// unsigned types: `unsigned char c = 200; c > -1` is TRUE in C (200 > -1 as
/// ints) but compiles to a `shi` against 0xFFFFFFFF and yields 0.
fn forces_unsigned(ty: &Type) -> bool {
    match &**ty {
        TypeK::Int { size, signed } => !*signed && *size >= 4,
        // arrays and functions have already decayed to addresses by here
        TypeK::Ptr(_) | TypeK::Array(..) | TypeK::Func { .. } => true,
        // Fixed is signed 16.16; Void/Struct never reach an arithmetic operand
        _ => false,
    }
}

/// Every machine register named in an `asm` clobber list anywhere in `body`.
///
/// Scope is the whole function, not the statement: the allocator assigns a
/// local one register for its entire lifetime, so a register clobbered by an
/// asm *anywhere* is unusable *everywhere*. Coarse, but the alternative is
/// live-range splitting, and being coarse here costs a register while being
/// wrong costs the value — `hot` parked in D6 across `moveq #0,d6` is simply
/// gone, with nothing in the output to suggest it.
///
/// Non-register clobbers (`"cc"`, `"memory"`) are ignored: this compiler keeps
/// no value in the condition codes across a statement, and the peephole's
/// redundant-load pass already forgets memory at any instruction it does not
/// positively recognize, which every asm line is.
fn collect_asm_clobbers(body: &[Stmt]) -> std::collections::HashSet<String> {
    fn walk(s: &Stmt, out: &mut std::collections::HashSet<String>) {
        match s {
            Stmt::AsmExt { clobbers, .. } => {
                for c in clobbers {
                    let r = c.trim().trim_start_matches('%').to_ascii_lowercase();
                    if is_machine_reg(&r) {
                        out.insert(r);
                    }
                }
            }
            Stmt::If(_, t, e) => {
                walk(t, out);
                if let Some(e) = e {
                    walk(e, out);
                }
            }
            Stmt::While(_, b) | Stmt::DoWhile(b, _) | Stmt::Label(_, b) => walk(b, out),
            Stmt::For(i, _, _, b) => {
                if let Some(i) = i {
                    walk(i, out);
                }
                walk(b, out);
            }
            Stmt::Block(items) => {
                for it in items {
                    walk(it, out);
                }
            }
            Stmt::Switch(_, b, _, _) => walk(b, out),
            _ => {}
        }
    }
    let mut out = std::collections::HashSet::new();
    for s in body {
        walk(s, &mut out);
    }
    out
}

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
