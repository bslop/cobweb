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
pub mod elf;
pub mod gas;
pub mod hazard;
pub mod m68k;
pub mod object;
pub mod preprocess;

use object::{Object, RelKind, Reloc, Symbol};

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
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

/// Section identity, tracked so object writers (ELF) can carve the assembled
/// blob into real `.text`/`.data`/`.bss` sections. In flat-binary and `.jo`
/// modes the switch directives stay advisory, exactly as before.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Section {
    Text,
    Data,
    Bss,
}

impl Section {
    pub fn name(self) -> &'static str {
        match self {
            Section::Text => ".text",
            Section::Data => ".data",
            Section::Bss => ".bss",
        }
    }
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
    pub externs: Vec<String>,
    pub relocs: Vec<Reloc>,
    /// source lines carrying a `;jas:allow` pragma (hazards there are waived)
    pub suppressed: Vec<usize>,
    pub diags: Vec<Diag>,
    /// Section marks: (section, byte offset where it starts). Consecutive marks
    /// of the same section are merged; always begins with (Text, 0).
    pub sections: Vec<(Section, u32)>,
    /// Names bound by *labels* (addresses in a section) as opposed to `equ`
    /// constants — object writers need the distinction (ELF `SHN_ABS`).
    pub label_syms: HashSet<String>,
}

impl Assembled {
    /// Build a relocatable object from this assembly (for `jln`).
    pub fn object(&self, org: u32) -> Object {
        let syms = self
            .symbols
            .iter()
            .map(|(name, &value)| Symbol {
                name: name.clone(),
                value,
                global: self.globals.iter().any(|g| g == name),
            })
            .collect();
        Object { org, bytes: self.bytes.clone(), symbols: syms, relocs: self.relocs.clone() }
    }
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
    /// Object mode: undefined symbols become relocations for jln instead of
    /// errors (turned on when emitting a `.jo` object with `-c`).
    pub object_mode: bool,
    /// Start in 68000 mode (for pure-68k source files with no `.68000`
    /// directive — rmac's default CPU is the 68k).
    pub start_m68k: bool,
    /// GAS dialect: `None` = rmac/Motorola (default), `Some(true)` = force the
    /// GNU-`as` frontend, `Some(false)` = never (even if it looks like GAS). The
    /// CLI maps `--gas`/`--no-gas`; the default auto-detects per file.
    pub gas: Option<bool>,
    /// Command-line `-D` preprocessor defines (`NAME` or `NAME=VALUE`), seeding
    /// the `#if`/`#ifdef` symbol table for cpp-style `.S` sources.
    pub defines: Vec<String>,
    /// Relocatable output: emit a relocation for *every* absolute reference to a
    /// defined symbol (not just externs), so jln can place the object at any
    /// address. PC-relative branches to same-object labels stay resolved (they
    /// are position-independent). Implies `object_mode`. Off by default (an
    /// object then pins to its assembled `.org`).
    pub relocatable: bool,
}

impl Default for Options {
    fn default() -> Self {
        Options {
            target: Target::Gpu,
            org: 0xF03000,
            check_hazards: true,
            warnings_as_errors: false,
            include_dirs: Vec::new(),
            object_mode: false,
            start_m68k: false,
            gas: None,
            defines: Vec::new(),
            relocatable: false,
        }
    }
}

/// Assemble `source`. Returns the emitted bytes plus diagnostics; the caller
/// decides whether to write output when `errors() > 0` (usually: don't).
pub fn assemble(source: &str, opts: &Options) -> Assembled {
    // GAS dialect: rewrite GNU-`as` syntax to jas-native. The lexical half
    // (comments, `%` registers) runs first so the front pass sees clean tokens;
    // numeric-label resolution runs *after* expansion so each macro expansion's
    // `9:` becomes a distinct label. `gas: Some(_)` forces; `None` auto-detects.
    let use_gas = opts.gas.unwrap_or_else(|| gas::looks_like_gas(source));
    let lexed;
    let source = if use_gas {
        lexed = gas::normalize_lexical(source);
        lexed.as_str()
    } else {
        source
    };
    // Front pass: expand includes / macros / rept / conditionals.
    let mut inc = preprocess::FsIncludes {
        dirs: opts.include_dirs.iter().map(PathBuf::from).collect(),
    };
    let (expanded, line_map) = match preprocess::run_mapped(source, &mut inc, &opts.defines) {
        Ok(pair) => pair,
        Err(diags) => {
            return Assembled { diags, ..Default::default() };
        }
    };
    // gas::resolve_numeric is line-preserving, so `line_map` still aligns 1:1.
    let expanded = if use_gas { gas::resolve_numeric(&expanded) } else { expanded };
    let mut asm = Assembler::new(opts);
    asm.run(&expanded, &line_map);
    let mut out = asm.finish();
    // (The GPU-SRAM top-phrase lint that lived here was RETIRED 2026-07-21:
    // the hwq TOPPHR sentinel probe proved $F03FF8-$F03FFF writable and
    // stable on silicon — calib/hwq_20260721.log. The top phrase is usable.)
    if opts.check_hazards {
        let suppressed: std::collections::HashSet<usize> = out.suppressed.iter().copied().collect();
        let mut hz = hazard::check(&out.emitted);
        hz.retain(|d| !suppressed.contains(&d.line));
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
    /// `::` double-colon label (auto-exported global)
    label_global: bool,
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
    /// condition-code aliases from `.ccdef` (name -> 5-bit cc)
    ccaliases: HashMap<String, u16>,
    /// symbols declared `.extern` (always relocatable)
    externs: HashSet<String>,
    /// relocations recorded in pass 2
    relocs: Vec<Reloc>,
    /// set by the encoder when a movei immediate is relocatable
    pending_reloc: RefCell<Option<(u32, RelKind, String, i64)>>,
    globals: Vec<String>,
    emitted: Vec<Emitted>,
    bytes: Vec<u8>,
    diags: Vec<Diag>,
    /// current scope for `.local` labels (last global label seen)
    scope: String,
    /// true inside a `.68000` section (route instructions to the 68k encoder)
    m68k_mode: bool,
    /// source lines with a `;jas:allow` pragma (hazard diagnostics waived)
    suppressed: std::collections::HashSet<usize>,
    /// section switch points (pass 2): (section, byte offset)
    sec_marks: Vec<(Section, u32)>,
    /// symbols defined as labels (vs `equ` constants)
    label_syms: HashSet<String>,
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
            ccaliases: HashMap::new(),
            externs: HashSet::new(),
            relocs: Vec::new(),
            pending_reloc: RefCell::new(None),
            globals: Vec::new(),
            emitted: Vec::new(),
            bytes: Vec::new(),
            diags: Vec::new(),
            scope: String::new(),
            m68k_mode: opts.start_m68k,
            suppressed: HashSet::new(),
            sec_marks: vec![(Section::Text, 0)],
            label_syms: HashSet::new(),
            pass: 0,
        }
    }

    /// Two passes: pass 1 binds every label to an address (forward references
    /// resolve), pass 2 emits with all symbols known.
    fn run(&mut self, source: &str, line_map: &[usize]) {
        // Report the *original* source line for each expanded line (the map is
        // parallel to `source.lines()`); fall back to the expanded position.
        let src_line = |i: usize| line_map.get(i).copied().unwrap_or(i + 1);
        for pass in 1..=2 {
            self.pass = pass;
            self.target = self.opts.target;
            self.pc = self.org;
            self.scope.clear();
            self.regaliases.clear();
            self.ccaliases.clear();
            self.relocs.clear();
            self.m68k_mode = self.opts.start_m68k;
            if pass == 2 {
                self.emitted.clear();
                self.bytes.clear();
                self.sec_marks = vec![(Section::Text, 0)];
            }
            for (i, raw) in source.lines().enumerate() {
                let n = src_line(i);
                if pass == 1 && raw.contains("jas:allow") {
                    self.suppressed.insert(n);
                }
                if let Some(line) = parse_line(raw, n) {
                    self.handle(&line);
                }
            }
        }
    }

    fn finish(self) -> Assembled {
        // merge consecutive same-section marks and drop empty spans
        let mut sections: Vec<(Section, u32)> = Vec::new();
        for (sec, off) in self.sec_marks {
            if let Some(last) = sections.last_mut() {
                if last.0 == sec {
                    continue; // still in the same section
                }
                if last.1 == off {
                    *last = (sec, off); // previous span was empty: replace it
                    // replacing may rejoin the span before it (.data/.text with
                    // nothing between switches back): merge those too
                    let n = sections.len();
                    if n >= 2 && sections[n - 2].0 == sec {
                        sections.pop();
                    }
                    continue;
                }
            }
            sections.push((sec, off));
        }
        Assembled {
            org: self.org,
            bytes: self.bytes,
            emitted: self.emitted,
            symbols: self.symbols,
            globals: self.globals,
            externs: self.externs.into_iter().collect(),
            relocs: self.relocs,
            suppressed: self.suppressed.into_iter().collect(),
            diags: self.diags,
            sections,
            label_syms: self.label_syms,
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
        if line.op == Some(".ccdef") {
            if let Some(name) = line.label {
                if let Some(v) = self.eval_or_err(line.args, line.n) {
                    self.ccaliases.insert(name.to_string(), (v & 0x1F) as u16);
                }
            }
            return;
        }
        if let Some(label) = line.label {
            self.define_label(label, line.n);
            if line.label_global && self.pass == 2 && !label.starts_with('.') {
                self.globals.push(label.to_string());
            }
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
        self.label_syms.insert(name.clone());
        if self.pass == 1 {
            if self.symbols.contains_key(&name) {
                // redefinition caught in pass 1
                self.diags.push(Diag::error(n, format!("duplicate label `{label}`")));
            }
            self.symbols.insert(name.clone(), self.pc);
        } else {
            self.symbols.insert(name.clone(), self.pc);
        }
        // GAS numeric locals (normalized to the reserved `L__gasnum_` prefix)
        // are file-global but must NOT reset the `.local` scope — a `3:` sitting
        // between other code shouldn't rebind the surrounding `.name` labels.
        if !label.starts_with('.') && !label.starts_with("L__gasnum_") {
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
            return self.emit_data_checked(line, sz, rest == "i", &opl);
        }
        if let Some(rest) = opl.strip_prefix(".dcb") {
            if rest.is_empty() || rest.starts_with('.') {
                return self.emit_dcb(line, suffix_size(rest.trim_start_matches('.')));
            }
        }
        if let Some(rest) = opl.strip_prefix(".ds").or_else(|| opl.strip_prefix("ds")) {
            // guard: don't swallow `.dsp`
            if rest.is_empty() || rest.starts_with('.') {
                return self.emit_ds(line, suffix_size(rest.trim_start_matches('.')));
            }
        }
        match opl.as_str() {
            ".gpu" => {
                self.target = Target::Gpu;
                self.m68k_mode = self.opts.start_m68k;
            }
            ".dsp" => {
                self.target = Target::Dsp;
                self.m68k_mode = self.opts.start_m68k;
            }
            ".68000" | ".68k" | ".m68k" => self.m68k_mode = true,
            ".org" | "org" => {
                if let Some(v) = self.eval_or_err(line.args, line.n) {
                    self.pc = v;
                    if !self.org_set {
                        self.org = v;
                        self.org_set = true;
                    }
                }
            }
            ".globl" | ".global" => {
                for name in line.args.split(',') {
                    let name = name.trim();
                    if !name.is_empty() && self.pass == 2 {
                        self.globals.push(name.to_string());
                    }
                }
            }
            ".extern" | ".xdef" | ".xref" => {
                for name in line.args.split(',') {
                    let name = name.trim();
                    if !name.is_empty() {
                        self.externs.insert(name.to_string());
                    }
                }
            }
            // rmac's `.long` with NO operands is an *alignment* directive (align
            // to a longword boundary); with operands it is GAS-style longword
            // data. Silently emitting nothing for the bare form cost a real
            // debug cycle (a 2-misaligned GPU data table read garbage on
            // silicon), so both meanings are honored by operand count.
            ".long" | "dc.l" | ".dc.l" => {
                if line.args.trim().is_empty() && opl == ".long" {
                    self.align_to(4, line);
                } else {
                    self.emit_data_checked(line, 4, false, &opl);
                }
            }
            ".word" | "dc.w" | ".dc.w" => self.emit_data_checked(line, 2, false, &opl),
            "dc.i" | ".dc.i" => self.emit_data(line, 4, true), // JRISC swapped-long
            ".byte" | "dc.b" | ".dc.b" => self.emit_data_checked(line, 1, false, &opl),
            ".align" => {
                if let Some(v) = self.eval_or_err(line.args, line.n) {
                    let a = v.max(1);
                    while self.pc % a != 0 {
                        self.put_byte(0, line);
                    }
                }
            }
            // GAS `.balign N` aligns the PC to an N-*byte* boundary (same as our
            // `.align` — jas's align is byte-granular, not power-of-two-exponent).
            ".balign" => {
                if let Some(v) = self.eval_or_err(line.args, line.n) {
                    let a = v.max(1);
                    while self.pc % a != 0 {
                        self.put_byte(0, line);
                    }
                }
            }
            // GAS `.section NAME [,flags]` — mapped onto the three base sections
            // when the name is (or starts with) one of them; other names fold
            // into `.data`.
            ".section" => {
                let name = line.args.split(',').next().unwrap_or("").trim().trim_matches('"');
                let sec = if name.starts_with(".text") {
                    Section::Text
                } else if name.starts_with(".bss") {
                    Section::Bss
                } else {
                    Section::Data
                };
                self.switch_section(sec);
            }
            // `.incbin "file"[,skip[,count]]` — splice a binary blob into the image.
            ".incbin" => self.emit_incbin(line),
            ".ascii" | ".asciz" | ".string" => self.emit_ascii(line, op != ".ascii"),
            ".space" | ".skip" | ".zero" => {
                // `.space N[,fill]` / `.zero N` — N fill bytes (default 0).
                let parts = split_args(line.args);
                if let Some(n) = parts.first().and_then(|p| self.eval_or_err(p, line.n)) {
                    let fill = parts.get(1).and_then(|p| self.eval_or_err(p, line.n)).unwrap_or(0) as u8;
                    for _ in 0..n {
                        self.put_byte(fill, line);
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
            ".ccdef" => {
                let parts = split_args(line.args);
                if parts.len() == 2 {
                    if let Some(v) = self.eval_or_err(&parts[1], line.n) {
                        self.ccaliases.insert(parts[0].trim().to_string(), (v & 0x1F) as u16);
                    }
                }
            }
            ".ccundef" => {
                for name in line.args.split(',') {
                    self.ccaliases.remove(name.trim());
                }
            }
            ".dc" => self.emit_data_checked(line, 2, false, &opl),
            ".phrase" => self.align_to(8, line),
            ".dphrase" => self.align_to(16, line),
            ".qphrase" => self.align_to(32, line),
            ".text" => self.switch_section(Section::Text),
            ".data" => self.switch_section(Section::Data),
            ".bss" => self.switch_section(Section::Bss),
            ".abs" => { /* absolute section: advisory in single-file mode */ }
            ".print" => { /* assembler-time message: ignored in batch */ }
            ".farskip" | ".wait" => {
                self.err_fix(line.n,
                    format!("`{op}` looks like a project macro — not defined here"),
                    "define it with .macro, or jas will expand it once macro support lands");
            }
            _ => self.err(line.n, format!("unknown directive `{op}`")),
        }
    }

    /// Record a section switch (pass 2; earlier passes only need sizes, which
    /// section identity does not affect — the blob layout is unchanged).
    fn switch_section(&mut self, sec: Section) {
        if self.pass == 2 {
            self.sec_marks.push((sec, self.bytes.len() as u32));
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

    /// `.incbin "file"[,skip[,count]]` — splice raw bytes from `file` into the
    /// output. The path is resolved against the include directories (which the
    /// CLI seeds with the source file's own directory).
    fn emit_incbin(&mut self, line: &Line) {
        let parts = split_args(line.args);
        let Some(raw) = parts.first() else {
            self.err(line.n, "`.incbin` expects a filename");
            return;
        };
        let name = raw.trim().trim_matches('"');
        let skip = parts.get(1).and_then(|p| self.eval_or_err(p, line.n)).unwrap_or(0) as usize;
        let count = parts.get(2).and_then(|p| self.eval_or_err(p, line.n)).map(|c| c as usize);
        // resolve against include dirs, then the current directory
        let mut found = None;
        for d in &self.opts.include_dirs {
            let cand = PathBuf::from(d).join(name);
            if cand.is_file() {
                found = Some(cand);
                break;
            }
        }
        let path = found.unwrap_or_else(|| PathBuf::from(name));
        match std::fs::read(&path) {
            Ok(data) => {
                let start = skip.min(data.len());
                let end = count.map(|c| (start + c).min(data.len())).unwrap_or(data.len());
                for &b in &data[start..end] {
                    self.put_byte(b, line);
                }
            }
            Err(e) => self.err(line.n, format!("`.incbin`: cannot read {}: {e}", path.display())),
        }
    }

    /// `.ascii "str"` / `.asciz "str"` (NUL-terminated) — emit the string bytes.
    fn emit_ascii(&mut self, line: &Line, nul_terminate: bool) {
        for item in split_args(line.args) {
            let s = item.trim();
            let Some(inner) = s.strip_prefix('"').and_then(|x| x.strip_suffix('"')) else {
                self.err(line.n, "`.ascii` expects a quoted string");
                continue;
            };
            for b in unescape_str(inner) {
                self.put_byte(b, line);
            }
            if nul_terminate {
                self.put_byte(0, line);
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

    /// [`emit_data`] plus an empty-operand check: a data directive with no
    /// operands emits nothing, which is never what the author meant (rmac's
    /// bare `.long` is the alignment directive; a bare `dc.w` is a typo).
    fn emit_data_checked(&mut self, line: &Line, size: u32, swapped: bool, opl: &str) {
        if line.args.trim().is_empty() {
            self.warn(
                line.n,
                format!("`{opl}` with no operands emits nothing — if you meant rmac's alignment directive, use `.align {size}`"),
            );
            return;
        }
        self.emit_data(line, size, swapped);
    }

    fn emit_data(&mut self, line: &Line, size: u32, swapped: bool) {
        for item in split_args(line.args) {
            // string literal in a `.byte`/`dc.b` list → raw bytes
            let t = item.trim();
            if t.starts_with('"') && t.ends_with('"') && t.len() >= 2 {
                for b in unescape_str(&t[1..t.len() - 1]) {
                    self.put_byte(b, line);
                }
                continue;
            }
            // relocatable longword (a table of external addresses)
            if size == 4 {
                if let Some((sym, addend)) = self.reloc_symbol_abs(item.trim()) {
                    if self.pass == 2 {
                        let off = self.bytes.len() as u32;
                        self.relocs.push(Reloc { offset: off, kind: RelKind::Long, symbol: sym, addend });
                    }
                    self.put_word((addend >> 16) as u16, line);
                    self.put_word(addend as u16, line);
                    continue;
                }
            }
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
    fn emit_insn(
        &mut self,
        op: u8,
        words: Vec<u16>,
        line: usize,
        reloc: Option<(u32, RelKind, String, i64)>,
    ) {
        if self.pass == 2 {
            let base = self.bytes.len() as u32;
            for w in &words {
                self.bytes.extend_from_slice(&w.to_be_bytes());
            }
            if let Some((woff, kind, symbol, addend)) = reloc {
                self.relocs.push(Reloc { offset: base + woff * 2, kind, symbol, addend });
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

    fn emit_m68k(&mut self, enc: m68k::M68kEnc, line: usize) {
        let n = enc.words.len() as u32;
        if self.pass == 2 {
            let base = self.bytes.len() as u32;
            for w in &enc.words {
                self.bytes.extend_from_slice(&w.to_be_bytes());
            }
            if let Some((woff, kind, symbol, addend)) = enc.reloc {
                self.relocs.push(Reloc { offset: base + woff * 2, kind, symbol, addend });
            }
            self.emitted.push(Emitted {
                addr: self.pc,
                words: enc.words,
                line,
                op: None,
                target: self.target,
            });
        }
        self.pc += n * 2;
    }

    fn instruction(&mut self, mnem: &str, line: &Line) {
        if self.m68k_mode {
            match m68k::encode(mnem, line.args, self.pc, self) {
                Ok(enc) => self.emit_m68k(enc, line.n),
                Err(EncodeErr::Unknown) => {
                    self.err(line.n, format!("unknown 68000 instruction `{mnem}`"))
                }
                Err(EncodeErr::Message(m)) => self.err(line.n, m),
                Err(EncodeErr::Fix(m, f)) => self.err_fix(line.n, m, f),
            }
            return;
        }
        *self.pending_reloc.borrow_mut() = None;
        match encode::encode(mnem, line.args, self) {
            Ok((op, words)) => {
                let reloc = self.pending_reloc.borrow_mut().take();
                self.emit_insn(op, words, line.n, reloc);
            }
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

    /// Resolve a condition operand: a builtin cc name, a `.ccdef` alias, or a
    /// constant expression (masked to 5 bits).
    pub(crate) fn resolve_cc(&self, s: &str) -> Option<u16> {
        let s = s.trim();
        if let Some(cc) = encode::builtin_cc(s) {
            return Some(cc);
        }
        if let Some(&cc) = self.ccaliases.get(s) {
            return Some(cc);
        }
        self.eval(s).ok().map(|v| (v & 0x1F) as u16)
    }

    /// If `expr` is a relocatable symbol reference (`SYM`, `SYM+C`, `SYM-C`,
    /// `C+SYM`) for a symbol this object does not define, return (symbol, addend).
    /// Only fires in pass 2, and only for externs (or any undefined symbol in
    /// object mode).
    fn reloc_symbol(&self, expr: &str) -> Option<(String, i64)> {
        self.reloc_symbol_impl(expr, false)
    }

    /// Like [`reloc_symbol`], for an *absolute* context (abs.l/abs.w, immediate
    /// address, `dc.l`/`movei`). In `relocatable` mode this also relocates
    /// references to symbols defined in this object, so the linker can rebase
    /// them — PC-relative branches keep using the non-abs form and stay resolved.
    fn reloc_symbol_abs(&self, expr: &str) -> Option<(String, i64)> {
        self.reloc_symbol_impl(expr, true)
    }

    fn reloc_symbol_impl(&self, expr: &str, abs: bool) -> Option<(String, i64)> {
        // Runs in BOTH passes: a reloc target is a full 32-bit address, so it must
        // be *sized* as abs.l/word-branch in pass 1 too (else the layout shifts
        // between passes and branch displacements come out wrong). The reloc is
        // only *recorded* in pass 2 (gated at the emit sites).
        let relocatable = |sym: &str| -> bool {
            // Local labels (`.name`) are always intra-object: they live in the
            // symbol table under their scope-qualified name and resolve at
            // assembly time, so they must never be deferred to the linker.
            if sym.starts_with('.') || !ident_ok(sym) {
                return false;
            }
            // Absolute references in relocatable mode relocate label (address)
            // symbols so the object can be placed anywhere, plus anything not
            // defined here (externs / forward refs). `equ` CONSTANTS must fold
            // at assembly time instead: they are values, not addresses, and
            // deferring one to the linker emits a reloc against a non-address
            // symbol that resolves to 0 (caught on Quake: `move.l #_vi_isr,USER0`
            // with `USER0 .equ $100` wrote the VI handler to vector $0 — the
            // console then died in the exception catcher on the first VI).
            if abs && self.opts.relocatable && self.opts.object_mode {
                return self.label_syms.contains(sym) || !self.symbols.contains_key(sym);
            }
            !self.symbols.contains_key(sym) && (self.opts.object_mode || self.externs.contains(sym))
        };
        let e = expr.trim();
        if relocatable(e) {
            return Some((e.to_string(), 0));
        }
        for (op, sign) in [('+', 1i64), ('-', -1i64)] {
            if let Some(pos) = e.rfind(op) {
                let l = e[..pos].trim();
                let r = e[pos + 1..].trim();
                if relocatable(l) {
                    if let Ok(c) = self.eval(r) {
                        return Some((l.to_string(), sign * c as i64));
                    }
                }
                if op == '+' && relocatable(r) {
                    if let Ok(c) = self.eval(l) {
                        return Some((r.to_string(), c as i64));
                    }
                }
            }
        }
        None
    }

    /// Relocation predicate for PC-relative contexts (branches): externs only.
    pub(crate) fn reloc_symbol_pub(&self, expr: &str) -> Option<(String, i64)> {
        self.reloc_symbol(expr)
    }

    /// Relocation predicate for absolute contexts (abs EA, immediate address).
    pub(crate) fn reloc_symbol_abs_pub(&self, expr: &str) -> Option<(String, i64)> {
        self.reloc_symbol_abs(expr)
    }

    pub(crate) fn movei_imm(&self, expr: &str) -> Result<u32, String> {
        // A MOVEI loads a full 32-bit address immediate — an absolute context.
        let e = expr.strip_prefix('#').unwrap_or(expr).trim();
        if let Some((sym, addend)) = self.reloc_symbol_abs(e) {
            *self.pending_reloc.borrow_mut() = Some((1, RelKind::Movei, sym, addend));
            return Ok(addend as u32);
        }
        self.eval(e)
    }

    fn lookup(&self, name: &str) -> Option<u32> {
        let q = if name.starts_with('.') {
            format!("{}{}", self.scope, name)
        } else {
            name.to_string()
        };
        self.symbols.get(&q).copied()
    }

    /// Which pass is running (1 = sizing, 2 = emitting). The m68k encoder uses
    /// this to size a forward branch without range-checking a displacement whose
    /// target isn't bound yet.
    pub(crate) fn pass(&self) -> u8 {
        self.pass
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
    // strip comment (`;` or `//` anywhere; also a leading `*` comment line,
    // MadMac style). `//` covers cpp'd `.S` sources that use C++ comments.
    let cut = [raw.find(';'), raw.find("//")].into_iter().flatten().min();
    let no_comment = match cut {
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
    let mut label_global = false;

    // Label: `name:` (any column) or a bare name in column 0.
    if !leading_ws {
        if let Some(colon) = find_label_colon(rest) {
            label = Some(rest[..colon].trim());
            if rest[colon..].starts_with("::") {
                label_global = true;
                rest = rest[colon + 2..].trim_start();
            } else {
                rest = rest[colon + 1..].trim_start();
            }
        } else {
            // possibly `name equ expr` / `name = expr`
            let first = rest.split_whitespace().next().unwrap_or("");
            let after = rest[first.len()..].trim_start();
            let al = after.to_ascii_lowercase();
            if after.starts_with('=') || al.starts_with("equ") || al.starts_with(".equ ")
                || al.starts_with(".equ\t") || al.starts_with(".set")
            {
                // symbol definition: op "=", args = value
                return Some(Line { n, label: Some(first), label_global: false, op: Some("="), args: kw_value(after) });
            }
            if al.starts_with(".equr") {
                // register alias: NAME .equr rN
                return Some(Line { n, label: Some(first), label_global: false, op: Some(".equr"), args: kw_value(after) });
            }
            if al.starts_with(".ccdef") {
                // condition-code alias: NAME .ccdef $15
                return Some(Line { n, label: Some(first), label_global: false, op: Some(".ccdef"), args: ccdef_value(after) });
            }
        }
    } else {
        rest = rest.trim_start();
    }

    let rest = rest.trim();
    if rest.is_empty() {
        return label.map(|l| Line { n, label: Some(l), label_global, op: None, args: "" });
    }
    let (op, args) = split_op(rest);
    Some(Line { n, label, label_global, op: Some(op), args })
}

fn ccdef_value(after: &str) -> &str {
    let a = after.trim_start();
    a.strip_prefix(".ccdef").map(|r| r.trim_start_matches([',', ' ', '\t']).trim()).unwrap_or(a)
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
        "dc.w" | "dc.l" | "dc.i" | "dc.b" | "dcb.w" | "dcb.l" | "dcb.b" | "ds.w" | "ds.l" | "ds.b" | "equ" | "org" | "="
    )
}

/// Decode C-style escapes in a `.ascii`/`.asciz` string body into raw bytes.
pub(crate) fn unescape_str(s: &str) -> Vec<u8> {
    let mut out = Vec::new();
    let b: Vec<char> = s.chars().collect();
    let mut i = 0;
    while i < b.len() {
        if b[i] == '\\' && i + 1 < b.len() {
            match b[i + 1] {
                'n' => out.push(b'\n'),
                't' => out.push(b'\t'),
                'r' => out.push(b'\r'),
                '0' => out.push(0),
                '\\' => out.push(b'\\'),
                '"' => out.push(b'"'),
                other => out.push(other as u8),
            }
            i += 2;
        } else {
            out.push(b[i] as u8);
            i += 1;
        }
    }
    out
}

pub(crate) fn split_args(s: &str) -> Vec<String> {
    // split on commas not inside parens or a quoted string
    let mut out = Vec::new();
    let mut depth = 0i32;
    let mut in_str = false;
    let mut prev = ' ';
    let mut cur = String::new();
    for c in s.chars() {
        match c {
            '"' if prev != '\\' => {
                in_str = !in_str;
                cur.push(c);
            }
            '(' if !in_str => {
                depth += 1;
                cur.push(c);
            }
            ')' if !in_str => {
                depth -= 1;
                cur.push(c);
            }
            ',' if depth == 0 && !in_str => {
                out.push(cur.trim().to_string());
                cur.clear();
            }
            _ => cur.push(c),
        }
        prev = c;
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

fn ident_ok(s: &str) -> bool {
    !s.is_empty()
        && s.chars().next().map(is_ident_start).unwrap_or(false)
        && s.chars().all(is_ident_char)
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
                } else if c == '0' && i + 1 < b.len() && (b[i + 1] == 'b' || b[i + 1] == 'B')
                    && i + 2 < b.len() && (b[i + 2] == '0' || b[i + 2] == '1')
                {
                    // `0b1010` binary literal (GAS spelling; `%` is the reg prefix there)
                    let mut j = i + 2;
                    while j < b.len() && (b[j] == '0' || b[j] == '1') {
                        j += 1;
                    }
                    let v = u32::from_str_radix(&b[i + 2..j].iter().collect::<String>(), 2)
                        .map_err(|_| "bad binary literal".to_string())?;
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
    /// Does `expr` reference a symbol (as opposed to being a pure numeric
    /// constant)? A symbolic operand has an address unknown until link, so an
    /// absolute reference to it must be sized `abs.l` regardless of any value it
    /// currently resolves to — keeping pass-1 and pass-2 sizes identical.
    pub(crate) fn is_symbolic(&self, expr: &str) -> bool {
        match expr_lex(expr) {
            Ok(toks) => toks.iter().any(|t| matches!(t, ETok::Sym(_))),
            Err(_) => true,
        }
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
