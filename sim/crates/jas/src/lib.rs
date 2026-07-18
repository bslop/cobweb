//! jas — the JRISC assembler that refuses to assemble hazards.
//!
//! rmac/MadMac-compatible syntax for the Atari Jaguar's Tom GPU and Jerry DSP
//! RISC cores. What sets it apart from rmac is not the encoder (that part is a
//! solved problem) but the [`hazard`] pass: the silicon traps that every other
//! Jaguar assembler emits silently — write-after-write into a load/divide
//! shadow (bug 13), an indexed store of an unsettled register (the TRM errata),
//! a JUMP/JR or MOVEI in a delay slot, an out-of-range `jr` — are reported as
//! errors with a fix-it, by default.
//!
//! Pipeline: [`assemble`] lexes → two-pass encodes (pass 1 sizes and binds
//! labels, pass 2 emits) → runs the hazard checker over the decoded stream →
//! returns the bytes plus a diagnostics list. The encoding is verified by the
//! integration tests, which assemble a program and *run it in jsim*, so the
//! assembler and the emulator can never silently disagree about an opcode.

mod encode;
pub mod hazard;
pub mod preprocess;

use std::collections::HashMap;
use std::path::PathBuf;

/// Which RISC core the code targets — it changes a handful of shared opcodes
/// (GPU SAT8/PACK vs DSP SAT16S/ADDQMOD) and the hazard rules.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Target {
    #[default]
    Gpu,
    Dsp,
}

/// Severity of a diagnostic.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Level {
    Error,
    Warning,
}

/// A diagnostic tied to a source line, with an optional fix-it hint — the thing
/// rmac never gives you ("undefined expression" at the wrong line).
#[derive(Debug, Clone)]
pub struct Diag {
    pub level: Level,
    pub line: usize,
    pub msg: String,
    pub fix: Option<String>,
}

impl Diag {
    fn error(line: usize, msg: impl Into<String>) -> Self {
        Diag { level: Level::Error, line, msg: msg.into(), fix: None }
    }
    fn warn(line: usize, msg: impl Into<String>) -> Self {
        Diag { level: Level::Warning, line, msg: msg.into(), fix: None }
    }
    fn with_fix(mut self, fix: impl Into<String>) -> Self {
        self.fix = Some(fix.into());
        self
    }
}

impl std::fmt::Display for Diag {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let tag = match self.level {
            Level::Error => "error",
            Level::Warning => "warning",
        };
        write!(f, "{}:{}: {}", self.line, tag, self.msg)?;
        if let Some(fix) = &self.fix {
            write!(f, "\n    fix: {fix}")?;
        }
        Ok(())
    }
}

/// One emitted instruction/datum, retained so the hazard pass can reason about
/// the decoded stream and so listings can map bytes back to source.
#[derive(Debug, Clone)]
pub struct Emitted {
    pub addr: u32,
    pub words: Vec<u16>,
    pub line: usize,
    /// Present for real instructions (not data directives) — drives hazards.
    pub op: Option<u8>,
    pub target: Target,
}

/// A successful (or partial) assembly.
#[derive(Debug, Default)]
pub struct Assembled {
    pub org: u32,
    pub bytes: Vec<u8>,
    pub emitted: Vec<Emitted>,
    pub symbols: HashMap<String, u32>,
    pub globals: Vec<String>,
    pub diags: Vec<Diag>,
}

impl Assembled {
    pub fn errors(&self) -> usize {
        self.diags.iter().filter(|d| d.level == Level::Error).count()
    }
    pub fn warnings(&self) -> usize {
        self.diags.iter().filter(|d| d.level == Level::Warning).count()
    }
}

/// Options controlling assembly.
#[derive(Debug, Clone)]
pub struct Options {
    pub target: Target,
    pub org: u32,
    /// Run the hazard pass (default true — the whole point).
    pub check_hazards: bool,
    /// Promote warnings to errors.
    pub warnings_as_errors: bool,
    /// Directories searched for `.include` files (plus the file's own dir).
    pub include_dirs: Vec<String>,
}

impl Default for Options {
    fn default() -> Self {
        Options {
            target: Target::Gpu,
            org: 0xF03000,
            check_hazards: true,
            warnings_as_errors: false,
            include_dirs: Vec::new(),
        }
    }
}

/// Assemble `source`. Returns the emitted bytes plus diagnostics; the caller
/// decides whether to write output when `errors() > 0` (usually: don't).
pub fn assemble(source: &str, opts: &Options) -> Assembled {
    // Front pass: expand includes / macros / rept / conditionals.
    let mut inc = preprocess::FsIncludes {
        dirs: opts.include_dirs.iter().map(PathBuf::from).collect(),
    };
    let expanded = match preprocess::run(source, &mut inc) {
        Ok(s) => s,
        Err(diags) => {
            return Assembled { diags, ..Default::default() };
        }
    };
    let mut asm = Assembler::new(opts);
    asm.run(&expanded);
    let mut out = asm.finish();
    if opts.check_hazards {
        let mut hz = hazard::check(&out.emitted);
        if opts.warnings_as_errors {
            for d in &mut hz {
                d.level = Level::Error;
            }
        }
        out.diags.extend(hz);
    }
    out
}

/// A parsed source line, comments and whitespace already stripped.
struct Line<'a> {
    n: usize,
    label: Option<&'a str>,
    /// mnemonic or directive (lowercased)
    op: Option<&'a str>,
    /// the raw operand text (everything after the mnemonic)
    args: &'a str,
}

struct Assembler<'a> {
    opts: &'a Options,
    target: Target,
    pc: u32,
    org: u32,
    org_set: bool,
    symbols: HashMap<String, u32>,
    /// register aliases from `.equr` (name -> register number)
    regaliases: HashMap<String, u16>,
    globals: Vec<String>,
    emitted: Vec<Emitted>,
    bytes: Vec<u8>,
    diags: Vec<Diag>,
    /// current scope for `.local` labels (last global label seen)
    scope: String,
    pass: u8,
}

impl<'a> Assembler<'a> {
    fn new(opts: &'a Options) -> Self {
        Assembler {
            opts,
            target: opts.target,
            pc: opts.org,
            org: opts.org,
            org_set: false,
            symbols: HashMap::new(),
            regaliases: HashMap::new(),
            globals: Vec::new(),
            emitted: Vec::new(),
            bytes: Vec::new(),
            diags: Vec::new(),
            scope: String::new(),
            pass: 0,
        }
    }

    /// Two passes: pass 1 binds every label to an address (forward references
    /// resolve), pass 2 emits with all symbols known.
    fn run(&mut self, source: &str) {
        for pass in 1..=2 {
            self.pass = pass;
            self.target = self.opts.target;
            self.pc = self.org;
            self.scope.clear();
            self.regaliases.clear();
            if pass == 2 {
                self.emitted.clear();
                self.bytes.clear();
            }
            for (i, raw) in source.lines().enumerate() {
                if let Some(line) = parse_line(raw, i + 1) {
                    self.handle(&line);
                }
            }
        }
    }

    fn finish(self) -> Assembled {
        Assembled {
            org: self.org,
            bytes: self.bytes,
            emitted: self.emitted,
            symbols: self.symbols,
            globals: self.globals,
            diags: self.diags,
        }
    }

    fn err(&mut self, line: usize, msg: impl Into<String>) {
        if self.pass == 2 {
            self.diags.push(Diag::error(line, msg));
        }
    }
    fn err_fix(&mut self, line: usize, msg: impl Into<String>, fix: impl Into<String>) {
        if self.pass == 2 {
            self.diags.push(Diag::error(line, msg).with_fix(fix));
        }
    }
    fn warn(&mut self, line: usize, msg: impl Into<String>) {
        if self.pass == 2 {
            self.diags.push(Diag::warn(line, msg));
        }
    }

    fn handle(&mut self, line: &Line) {
        // `NAME equ expr` / `NAME = expr`: bind the symbol to the VALUE, not the
        // PC. Must run before define_label (which would bind it to the PC).
        if line.op == Some("=") {
            if let Some(name) = line.label {
                if let Some(v) = self.eval_or_err(line.args, line.n) {
                    self.symbols.insert(name.to_string(), v);
                }
            }
            return;
        }
        if line.op == Some(".equr") {
            if let Some(name) = line.label {
                if let Some(r) = self.resolve_reg(line.args) {
                    self.regaliases.insert(name.to_string(), r);
                } else {
                    self.err(line.n, format!("`.equr {name}` needs a register, found `{}`", line.args));
                }
            }
            return;
        }
        if let Some(label) = line.label {
            self.define_label(label, line.n);
        }
        let Some(op) = line.op else { return };
        // Directive?
        if op.starts_with('.') || is_pseudo(op) {
            self.directive(op, line);
            return;
        }
        // `NAME equ expr` and `NAME = expr` arrive with the name as the label
        // via parse_line, so a bare instruction remains.
        self.instruction(op, line);
    }

    fn define_label(&mut self, label: &str, n: usize) {
        let name = self.qualify(label);
        if self.pass == 1 {
            if self.symbols.contains_key(&name) {
                // redefinition caught in pass 1
                self.diags.push(Diag::error(n, format!("duplicate label `{label}`")));
            }
            self.symbols.insert(name.clone(), self.pc);
        } else {
            self.symbols.insert(name.clone(), self.pc);
        }
        if !label.starts_with('.') {
            self.scope = label.to_string();
        }
    }

    /// Resolve a label reference through the local-scope rules (`.name` binds to
    /// the last global label).
    fn qualify(&self, label: &str) -> String {
        if label.starts_with('.') {
            format!("{}{}", self.scope, label)
        } else {
            label.to_string()
        }
    }

    fn directive(&mut self, op: &str, line: &Line) {
        let opl = op.to_ascii_lowercase();
        // size-suffixed data directives: .dc.X / .dcb.X / .ds.X
        if let Some(rest) = opl.strip_prefix(".dc.").or_else(|| opl.strip_prefix("dc.")) {
            let sz = suffix_size(rest);
            return self.emit_data(line, sz, rest == "i");
        }
        if let Some(rest) = opl.strip_prefix(".dcb") {
            if rest.is_empty() || rest.starts_with('.') {
                return self.emit_dcb(line, suffix_size(rest.trim_start_matches('.')));
            }
        }
        if let Some(rest) = opl.strip_prefix(".ds") {
            // guard: don't swallow `.dsp`
            if rest.is_empty() || rest.starts_with('.') {
                return self.emit_ds(line, suffix_size(rest.trim_start_matches('.')));
            }
        }
        match opl.as_str() {
            ".gpu" => self.target = Target::Gpu,
            ".dsp" => self.target = Target::Dsp,
            ".68000" | ".68k" => { /* leaving RISC scope — data/host, no encode */ }
            ".org" | "org" => {
                if let Some(v) = self.eval_or_err(line.args, line.n) {
                    self.pc = v;
                    if !self.org_set {
                        self.org = v;
                        self.org_set = true;
                    }
                }
            }
            ".globl" | ".global" | ".extern" => {
                for name in line.args.split(',') {
                    let name = name.trim();
                    if !name.is_empty() && self.pass == 2 {
                        self.globals.push(name.to_string());
                    }
                }
            }
            ".long" | "dc.l" | ".dc.l" => self.emit_data(line, 4, false),
            ".word" | "dc.w" | ".dc.w" => self.emit_data(line, 2, false),
            "dc.i" | ".dc.i" => self.emit_data(line, 4, true), // JRISC swapped-long
            ".byte" | "dc.b" | ".dc.b" => self.emit_data(line, 1, false),
            ".align" => {
                if let Some(v) = self.eval_or_err(line.args, line.n) {
                    let a = v.max(1);
                    while self.pc % a != 0 {
                        self.put_byte(0, line);
                    }
                }
            }
            ".even" => {
                if self.pc % 2 != 0 {
                    self.put_byte(0, line);
                }
            }
            // preprocessor directives are consumed by the front pass; any that
            // reach here (e.g. stray .endm) are harmless no-ops.
            ".include" | ".macro" | ".endm" | ".rept" | ".endr" | ".if" | ".ifdef"
            | ".ifndef" | ".else" | ".endif" => {}
            ".equ" | ".set" => {
                let parts = split_args(line.args);
                if parts.len() == 2 {
                    if let Some(v) = self.eval_or_err(&parts[1], line.n) {
                        self.symbols.insert(parts[0].trim().to_string(), v);
                    }
                } else {
                    self.err(line.n, "`.equ` expects `NAME, value`");
                }
            }
            ".equr" => {
                let parts = split_args(line.args);
                if parts.len() == 2 {
                    if let Some(r) = self.resolve_reg(&parts[1]) {
                        self.regaliases.insert(parts[0].trim().to_string(), r);
                    } else {
                        self.err(line.n, "`.equr` expects `NAME, rN`");
                    }
                }
            }
            ".equrundef" | ".equundef" => {
                for name in line.args.split(',') {
                    self.regaliases.remove(name.trim());
                }
            }
            ".dc" => self.emit_data(line, 2, false),
            ".phrase" => self.align_to(8, line),
            ".dphrase" => self.align_to(16, line),
            ".text" | ".data" | ".bss" | ".abs" => { /* section: advisory in single-file mode */ }
            ".print" => { /* assembler-time message: ignored in batch */ }
            ".farskip" | ".wait" => {
                self.err_fix(line.n,
                    format!("`{op}` looks like a project macro — not defined here"),
                    "define it with .macro, or jas will expand it once macro support lands");
            }
            _ => self.err(line.n, format!("unknown directive `{op}`")),
        }
    }

    fn align_to(&mut self, a: u32, line: &Line) {
        while self.pc % a != 0 {
            self.put_byte(0, line);
        }
    }

    /// `.dcb[.size] count, value` — `count` copies of `value`.
    fn emit_dcb(&mut self, line: &Line, size: u32) {
        let parts = split_args(line.args);
        if parts.len() != 2 {
            self.err(line.n, "`.dcb` expects `count, value`");
            return;
        }
        let (Some(count), Some(val)) =
            (self.eval_or_err(&parts[0], line.n), self.eval_or_err(&parts[1], line.n))
        else {
            return;
        };
        for _ in 0..count {
            match size {
                1 => self.put_byte(val as u8, line),
                4 => {
                    self.put_word((val >> 16) as u16, line);
                    self.put_word(val as u16, line);
                }
                _ => self.put_word(val as u16, line),
            }
        }
    }

    /// `.ds[.size] count` — reserve `count*size` zero bytes.
    fn emit_ds(&mut self, line: &Line, size: u32) {
        if let Some(count) = self.eval_or_err(line.args.trim(), line.n) {
            for _ in 0..(count * size) {
                self.put_byte(0, line);
            }
        }
    }

    fn emit_data(&mut self, line: &Line, size: u32, swapped: bool) {
        for item in split_args(line.args) {
            let Some(v) = self.eval_or_err(&item, line.n) else { continue };
            match size {
                1 => self.put_byte(v as u8, line),
                2 => self.put_word(v as u16, line),
                4 if swapped => {
                    // dc.i: low half-word first (MOVEI immediate convention)
                    self.put_word(v as u16, line);
                    self.put_word((v >> 16) as u16, line);
                }
                4 => {
                    self.put_word((v >> 16) as u16, line);
                    self.put_word(v as u16, line);
                }
                _ => {}
            }
        }
    }

    fn put_byte(&mut self, b: u8, line: &Line) {
        if self.pass == 2 {
            self.bytes.push(b);
            // track as raw datum (no op) for hazard adjacency
            self.emitted.push(Emitted {
                addr: self.pc,
                words: vec![],
                line: line.n,
                op: None,
                target: self.target,
            });
        }
        self.pc += 1;
    }

    fn put_word(&mut self, w: u16, line: &Line) {
        if self.pass == 2 {
            self.bytes.extend_from_slice(&w.to_be_bytes());
            self.emitted.push(Emitted {
                addr: self.pc,
                words: vec![w],
                line: line.n,
                op: None,
                target: self.target,
            });
        }
        self.pc += 2;
    }

    /// Emit one encoded instruction (its opcode word plus any MOVEI immediate).
    fn emit_insn(&mut self, op: u8, words: Vec<u16>, line: usize) {
        if self.pass == 2 {
            for w in &words {
                self.bytes.extend_from_slice(&w.to_be_bytes());
            }
            self.emitted.push(Emitted {
                addr: self.pc,
                words: words.clone(),
                line,
                op: Some(op),
                target: self.target,
            });
        }
        self.pc += (words.len() as u32) * 2;
    }

    fn instruction(&mut self, mnem: &str, line: &Line) {
        match encode::encode(mnem, line.args, self) {
            Ok((op, words)) => self.emit_insn(op, words, line.n),
            Err(EncodeErr::Message(m)) => self.err(line.n, m),
            Err(EncodeErr::Fix(m, fix)) => self.err_fix(line.n, m, fix),
            Err(EncodeErr::Unknown) => {
                self.err(line.n, format!("unknown instruction `{mnem}`"))
            }
        }
    }

    fn eval_or_err(&mut self, expr: &str, line: usize) -> Option<u32> {
        match self.eval(expr) {
            Ok(v) => Some(v),
            Err(m) => {
                self.err(line, m);
                None
            }
        }
    }

    /// Evaluate a constant expression. Supports symbols, `$hex`/`0x`/`%bin`/
    /// decimal literals, `*`/`.` (current PC), and `+ - * / << >> & | ^` with
    /// parentheses. Unknown symbols error only in pass 2 (pass 1 forward refs
    /// resolve to 0 so sizing is stable — all instruction sizes here are
    /// operand-independent, so this is safe).
    fn eval(&self, expr: &str) -> Result<u32, String> {
        let toks = expr_lex(expr)?;
        let mut p = ExprParser { toks: &toks, i: 0, asm: self };
        let v = p.parse(0)?;
        if p.i != p.toks.len() {
            return Err(format!("trailing tokens in expression `{expr}`"));
        }
        Ok(v)
    }

    /// Resolve a register operand: an `.equr` alias, or a plain `rN`/`RN`.
    pub(crate) fn resolve_reg(&self, s: &str) -> Option<u16> {
        let s = s.trim();
        if let Some(&r) = self.regaliases.get(s) {
            return Some(r);
        }
        let rest = s.strip_prefix(['r', 'R'])?;
        let n: u16 = rest.parse().ok()?;
        (n < 32).then_some(n)
    }

    fn lookup(&self, name: &str) -> Option<u32> {
        let q = if name.starts_with('.') {
            format!("{}{}", self.scope, name)
        } else {
            name.to_string()
        };
        self.symbols.get(&q).copied()
    }
}

/// Encoder error variants.
pub(crate) enum EncodeErr {
    Unknown,
    Message(String),
    Fix(String, String),
}

/// Parse a raw source line into label / mnemonic / args. Returns None for a
/// blank or comment-only line. Handles `name equ expr` and `name = expr` by
/// synthesizing a label + a fake directive so the assembler binds the symbol.
fn parse_line(raw: &str, n: usize) -> Option<Line<'_>> {
    // strip comment (`;` anywhere; also a leading `*` comment line, MadMac style)
    let no_comment = match raw.find(';') {
        Some(i) => &raw[..i],
        None => raw,
    };
    let trimmed = no_comment.trim_end();
    if trimmed.trim().is_empty() || trimmed.trim_start().starts_with('*') {
        return None;
    }

    let leading_ws = raw.starts_with([' ', '\t']);
    let mut rest = trimmed;
    let mut label = None;

    // Label: `name:` (any column) or a bare name in column 0.
    if !leading_ws {
        if let Some(colon) = find_label_colon(rest) {
            label = Some(rest[..colon].trim());
            rest = rest[colon + 1..].trim_start();
        } else {
            // possibly `name equ expr` / `name = expr`
            let first = rest.split_whitespace().next().unwrap_or("");
            let after = rest[first.len()..].trim_start();
            let al = after.to_ascii_lowercase();
            if after.starts_with('=') || al.starts_with("equ") || al.starts_with(".equ ")
                || al.starts_with(".equ\t") || al.starts_with(".set")
            {
                // symbol definition: op "=", args = value
                return Some(Line { n, label: Some(first), op: Some("="), args: kw_value(after) });
            }
            if al.starts_with(".equr") {
                // register alias: NAME .equr rN
                return Some(Line { n, label: Some(first), op: Some(".equr"), args: kw_value(after) });
            }
        }
    } else {
        rest = rest.trim_start();
    }

    let rest = rest.trim();
    if rest.is_empty() {
        return label.map(|l| Line { n, label: Some(l), op: None, args: "" });
    }
    let (op, args) = split_op(rest);
    Some(Line { n, label, op: Some(op), args })
}

fn kw_value(after: &str) -> &str {
    let a = after.trim_start();
    for kw in ["=", ".equrundef", ".equr", ".equ", ".set", "equ"] {
        if let Some(r) = a.strip_prefix(kw) {
            return r.trim_start_matches([',', ' ', '\t']).trim();
        }
    }
    a.trim()
}

/// Find the colon that terminates a leading label, ignoring `::` (global) and
/// not treating a colon inside later operands as a label.
fn find_label_colon(s: &str) -> Option<usize> {
    let bytes = s.as_bytes();
    // label must be the first token; scan the identifier then expect ':'
    let mut i = 0;
    if i >= bytes.len() || !is_ident_start(bytes[i] as char) {
        return None;
    }
    while i < bytes.len() && is_ident_char(bytes[i] as char) {
        i += 1;
    }
    if i < bytes.len() && bytes[i] == b':' {
        Some(i)
    } else {
        None
    }
}

fn split_op(s: &str) -> (&str, &str) {
    match s.find(|c: char| c.is_whitespace()) {
        Some(i) => (&s[..i], s[i..].trim_start()),
        None => (s, ""),
    }
}

/// Lowercase a mnemonic in place-free way by borrowing when already lowercase.
/// (Callers compare against lowercase tables; we lowercase at compare sites.)
fn is_pseudo(op: &str) -> bool {
    matches!(
        op.to_ascii_lowercase().as_str(),
        "dc.w" | "dc.l" | "dc.i" | "dc.b" | "equ" | "org" | "="
    )
}

pub(crate) fn split_args(s: &str) -> Vec<String> {
    // split on commas not inside parens
    let mut out = Vec::new();
    let mut depth = 0i32;
    let mut cur = String::new();
    for c in s.chars() {
        match c {
            '(' => {
                depth += 1;
                cur.push(c);
            }
            ')' => {
                depth -= 1;
                cur.push(c);
            }
            ',' if depth == 0 => {
                out.push(cur.trim().to_string());
                cur.clear();
            }
            _ => cur.push(c),
        }
    }
    if !cur.trim().is_empty() {
        out.push(cur.trim().to_string());
    }
    out
}

/// Map a data-directive size suffix (`b`/`w`/`l`/`i`) to a byte size.
fn suffix_size(sfx: &str) -> u32 {
    match sfx.trim_start_matches('.') {
        "b" => 1,
        "l" | "i" => 4,
        _ => 2, // w / empty / phrase-ish default to word
    }
}

fn is_ident_start(c: char) -> bool {
    c.is_ascii_alphabetic() || c == '_' || c == '.'
}
fn is_ident_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_' || c == '.' || c == '$'
}

// ── expression evaluator ─────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq)]
enum ETok {
    Num(u32),
    Sym(String),
    Pc,
    Op(char),
    Shl,
    Shr,
    LParen,
    RParen,
}

fn expr_lex(s: &str) -> Result<Vec<ETok>, String> {
    let mut out = Vec::new();
    let b: Vec<char> = s.chars().collect();
    let mut i = 0;
    while i < b.len() {
        let c = b[i];
        if c.is_whitespace() {
            i += 1;
            continue;
        }
        match c {
            '$' => {
                let mut j = i + 1;
                while j < b.len() && b[j].is_ascii_hexdigit() {
                    j += 1;
                }
                let v = u32::from_str_radix(&b[i + 1..j].iter().collect::<String>(), 16)
                    .map_err(|_| "bad hex literal".to_string())?;
                out.push(ETok::Num(v));
                i = j;
            }
            '%' => {
                let mut j = i + 1;
                while j < b.len() && (b[j] == '0' || b[j] == '1') {
                    j += 1;
                }
                let v = u32::from_str_radix(&b[i + 1..j].iter().collect::<String>(), 2)
                    .map_err(|_| "bad binary literal".to_string())?;
                out.push(ETok::Num(v));
                i = j;
            }
            '0'..='9' => {
                if c == '0' && i + 1 < b.len() && (b[i + 1] == 'x' || b[i + 1] == 'X') {
                    let mut j = i + 2;
                    while j < b.len() && b[j].is_ascii_hexdigit() {
                        j += 1;
                    }
                    let v = u32::from_str_radix(&b[i + 2..j].iter().collect::<String>(), 16)
                        .map_err(|_| "bad hex literal".to_string())?;
                    out.push(ETok::Num(v));
                    i = j;
                } else {
                    let mut j = i;
                    while j < b.len() && b[j].is_ascii_digit() {
                        j += 1;
                    }
                    let v = b[i..j]
                        .iter()
                        .collect::<String>()
                        .parse()
                        .map_err(|_| "bad decimal literal".to_string())?;
                    out.push(ETok::Num(v));
                    i = j;
                }
            }
            '*' | '.' if !(i + 1 < b.len() && is_ident_char(b[i + 1])) => {
                // `*` or `.` alone = current PC
                out.push(ETok::Pc);
                i += 1;
            }
            c if is_ident_start(c) => {
                let mut j = i;
                while j < b.len() && is_ident_char(b[j]) {
                    j += 1;
                }
                out.push(ETok::Sym(b[i..j].iter().collect()));
                i = j;
            }
            '<' if i + 1 < b.len() && b[i + 1] == '<' => {
                out.push(ETok::Shl);
                i += 2;
            }
            '>' if i + 1 < b.len() && b[i + 1] == '>' => {
                out.push(ETok::Shr);
                i += 2;
            }
            '+' | '-' | '*' | '/' | '&' | '|' | '^' => {
                out.push(ETok::Op(c));
                i += 1;
            }
            '(' => {
                out.push(ETok::LParen);
                i += 1;
            }
            ')' => {
                out.push(ETok::RParen);
                i += 1;
            }
            _ => return Err(format!("unexpected character `{c}` in expression")),
        }
    }
    Ok(out)
}

struct ExprParser<'a> {
    toks: &'a [ETok],
    i: usize,
    asm: &'a Assembler<'a>,
}

impl<'a> ExprParser<'a> {
    fn peek(&self) -> Option<&ETok> {
        self.toks.get(self.i)
    }
    fn bump(&mut self) -> Option<&ETok> {
        let t = self.toks.get(self.i);
        self.i += 1;
        t
    }

    fn prec(t: &ETok) -> Option<u8> {
        Some(match t {
            ETok::Op('|') | ETok::Op('^') => 1,
            ETok::Op('&') => 2,
            ETok::Shl | ETok::Shr => 3,
            ETok::Op('+') | ETok::Op('-') => 4,
            ETok::Op('*') | ETok::Op('/') => 5,
            _ => return None,
        })
    }

    fn parse(&mut self, min: u8) -> Result<u32, String> {
        let mut lhs = self.unary()?;
        while let Some(t) = self.peek() {
            let Some(p) = Self::prec(t) else { break };
            if p < min {
                break;
            }
            let opt = t.clone();
            self.i += 1;
            let rhs = self.parse(p + 1)?;
            lhs = apply(&opt, lhs, rhs)?;
        }
        Ok(lhs)
    }

    fn unary(&mut self) -> Result<u32, String> {
        let t = self.toks.get(self.i).cloned();
        self.i += 1;
        match t {
            Some(ETok::Num(v)) => Ok(v),
            Some(ETok::Pc) => Ok(self.asm.pc),
            Some(ETok::Sym(s)) => match self.asm.lookup(&s) {
                Some(v) => Ok(v),
                None => {
                    if self.asm.pass == 1 {
                        Ok(0) // forward ref; sizes are operand-independent
                    } else {
                        Err(format!("undefined symbol `{s}`"))
                    }
                }
            },
            Some(ETok::Op('-')) => Ok(0u32.wrapping_sub(self.unary()?)),
            Some(ETok::Op('+')) => self.unary(),
            Some(ETok::LParen) => {
                let v = self.parse(0)?;
                let close = self.toks.get(self.i).cloned();
                self.i += 1;
                match close {
                    Some(ETok::RParen) => Ok(v),
                    _ => Err("expected `)`".to_string()),
                }
            }
            other => Err(format!("expected value, found {other:?}")),
        }
    }
}

fn apply(op: &ETok, a: u32, b: u32) -> Result<u32, String> {
    Ok(match op {
        ETok::Op('+') => a.wrapping_add(b),
        ETok::Op('-') => a.wrapping_sub(b),
        ETok::Op('*') => a.wrapping_mul(b),
        ETok::Op('/') => {
            if b == 0 {
                return Err("division by zero in expression".into());
            }
            a / b
        }
        ETok::Op('&') => a & b,
        ETok::Op('|') => a | b,
        ETok::Op('^') => a ^ b,
        ETok::Shl => a.wrapping_shl(b),
        ETok::Shr => a.wrapping_shr(b),
        _ => return Err("bad operator".into()),
    })
}

// Re-export the evaluation entry the encoder uses.
impl<'a> Assembler<'a> {
    pub(crate) fn eval_pub(&self, expr: &str) -> Result<u32, String> {
        self.eval(expr)
    }
    pub(crate) fn cur_target(&self) -> Target {
        self.target
    }
    pub(crate) fn cur_pc(&self) -> u32 {
        self.pc
    }
    pub(crate) fn in_pass2(&self) -> bool {
        self.pass == 2
    }
}
