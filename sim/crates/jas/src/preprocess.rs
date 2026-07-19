//! The jas preprocessor — the front pass that makes real multi-file projects
//! assemble.
//!
//! Expands, in one text pass, everything the two-pass assembler shouldn't have
//! to know about:
//!   * `.include "file"`      — textual inclusion (searched include dirs), with
//!                              recursion + depth guard.
//!   * `.macro name a,b` … `.endm` and invocation `name x,y` — positional
//!     (`\1`,`\2`) and named (`\a`,`\b`) parameter substitution, plus `\~`/`\@`
//!     for a per-expansion unique id (macro-local labels).
//!   * `.rept N` … `.endr`    — repeat a block N times.
//!   * `.if expr` / `.else` / `.endif` — conditional assembly, evaluated
//!     against `.equ`/`.set`/`=` symbols seen so far.
//!
//! Output is flat source with a preserved line count feel (blank lines stand in
//! for consumed directives) so downstream diagnostics still point somewhere
//! sensible. Errors are jas `Diag`s.

use crate::{Diag, Level};
use std::collections::HashMap;

/// Resolve an `.include` name to its text. Returns None if not found.
pub trait Includes {
    fn read(&mut self, name: &str) -> Option<String>;
}

/// A no-op resolver (`.include` becomes an error) — for sources without includes.
pub struct NoIncludes;
impl Includes for NoIncludes {
    fn read(&mut self, _: &str) -> Option<String> {
        None
    }
}

/// Filesystem resolver searching a list of directories, then the raw path.
pub struct FsIncludes {
    pub dirs: Vec<std::path::PathBuf>,
}
impl Includes for FsIncludes {
    fn read(&mut self, name: &str) -> Option<String> {
        for d in &self.dirs {
            let p = d.join(name);
            if let Ok(s) = std::fs::read_to_string(&p) {
                return Some(s);
            }
        }
        std::fs::read_to_string(name).ok()
    }
}

struct Macro {
    params: Vec<String>,
    body: Vec<String>,
}

struct Pp<'a> {
    inc: &'a mut dyn Includes,
    macros: HashMap<String, Macro>,
    syms: HashMap<String, i64>,
    out: Vec<String>,
    /// Original source line for each emitted line in `out` (parallel array), so
    /// diagnostics point at the real file line even after conditionals/macros
    /// add or remove lines. Best-effort for macro/include bodies.
    src: Vec<usize>,
    diags: Vec<Diag>,
    uniq: usize,
    depth: usize,
}

const MAX_DEPTH: usize = 64;

/// Preprocess `src`. `line0` is the 1-based line the source starts at (for
/// nested includes it's 1; kept simple — diagnostics use best-effort lines).
pub fn run(src: &str, inc: &mut dyn Includes, defines: &[String]) -> Result<String, Vec<Diag>> {
    run_mapped(src, inc, defines).map(|(s, _)| s)
}

/// Like [`run`], also returning the per-output-line source-line map (parallel to
/// the output lines) so the assembler can report original file line numbers.
pub fn run_mapped(
    src: &str,
    inc: &mut dyn Includes,
    defines: &[String],
) -> Result<(String, Vec<usize>), Vec<Diag>> {
    let mut syms = HashMap::new();
    for d in defines {
        match d.split_once('=') {
            Some((n, v)) => {
                let val = v.trim().parse::<i64>().unwrap_or(1);
                syms.insert(n.trim().to_string(), val);
            }
            None => {
                syms.insert(d.trim().to_string(), 1);
            }
        }
    }
    let mut pp = Pp {
        inc,
        macros: HashMap::new(),
        syms,
        out: Vec::new(),
        src: Vec::new(),
        diags: Vec::new(),
        uniq: 0,
        depth: 0,
    };
    let lines: Vec<String> = src.lines().map(|s| s.to_string()).collect();
    pp.process(&lines, 1);
    if pp.diags.iter().any(|d| d.level == Level::Error) {
        Err(pp.diags)
    } else {
        Ok((pp.out.join("\n"), pp.src))
    }
}

/// Strip a `;` comment for directive scanning (but we re-emit the original).
fn strip(line: &str) -> &str {
    line.split(';').next().unwrap_or("").trim()
}

/// Canonical directive name for a first-token, accepting both the rmac `.name`
/// and the C-preprocessor `#name` spellings. Returns `""` for non-directives.
fn norm_directive(tok: &str) -> &'static str {
    match tok.trim_start_matches(['#', '.']) {
        "macro" => ".macro",
        "rept" => ".rept",
        "if" => ".if",
        "ifdef" => ".ifdef",
        "ifndef" => ".ifndef",
        "elif" => ".elif",
        "else" => ".else",
        "endif" => ".endif",
        "include" => ".include",
        "define" => ".define",
        "undef" => ".undef",
        _ => "",
    }
}

/// First whitespace-separated token of a line's code part.
fn first_token(line: &str) -> &str {
    strip(line).split_whitespace().next().unwrap_or("")
}

/// Index of the label-terminating `:` (or the second `:` of a `::`) when `s`
/// begins with an assembler label `ident:`. Returns None otherwise.
fn find_label_colon(s: &str) -> Option<usize> {
    let b = s.as_bytes();
    if b.is_empty() || !(b[0].is_ascii_alphabetic() || b[0] == b'_' || b[0] == b'.') {
        return None;
    }
    let mut i = 0;
    while i < b.len() && (b[i].is_ascii_alphanumeric() || b[i] == b'_' || b[i] == b'.' || b[i] == b'$') {
        i += 1;
    }
    if i < b.len() && b[i] == b':' {
        if i + 1 < b.len() && b[i + 1] == b':' {
            Some(i + 1)
        } else {
            Some(i)
        }
    } else {
        None
    }
}

/// The argument part of a directive line: everything after the first token
/// (the `#if`/`.ifdef`/`#include`/... keyword).
fn directive_arg(code: &str) -> String {
    let mut it = code.splitn(2, |c: char| c.is_whitespace());
    let _kw = it.next();
    it.next().unwrap_or("").trim().to_string()
}

impl Pp<'_> {
    fn err(&mut self, line: usize, msg: impl Into<String>) {
        self.diags.push(Diag::error(line, msg));
    }

    /// Process a slice of lines, appending expanded output. `base` is the
    /// 1-based source line of `lines[0]` for diagnostics.
    /// Emit one output line, recording the source line it came from (for
    /// accurate diagnostics after the preprocessor adds/removes lines).
    fn emit(&mut self, line: String, src_line: usize) {
        self.out.push(line);
        self.src.push(src_line);
    }

    fn process(&mut self, lines: &[String], base: usize) {
        if self.depth > MAX_DEPTH {
            self.err(base, "preprocessor recursion too deep (include/macro cycle?)");
            return;
        }
        let mut i = 0;
        while i < lines.len() {
            let raw = &lines[i];
            let code = strip(raw);
            let tok = first_token(raw).to_ascii_lowercase();

            // block collectors need lookahead. Both rmac (`.if`) and cpp (`#if`)
            // spellings map to the same machinery via `norm_directive`.
            match norm_directive(&tok) {
                ".macro" => {
                    i = self.collect_macro(lines, i, base);
                    continue;
                }
                ".rept" => {
                    i = self.collect_rept(lines, i, base);
                    continue;
                }
                ".if" | ".ifdef" | ".ifndef" => {
                    i = self.collect_if(lines, i, base, norm_directive(&tok));
                    continue;
                }
                ".include" => {
                    self.do_include(code, base + i);
                    i += 1;
                    continue;
                }
                ".define" => {
                    self.do_define(code, base + i);
                    i += 1;
                    continue;
                }
                ".undef" => {
                    let name = code.trim_start_matches(['#', '.']).trim();
                    let name = name.strip_prefix("undef").unwrap_or(name).trim();
                    self.syms.remove(name);
                    self.macros.remove(name);
                    i += 1;
                    continue;
                }
                _ => {}
            }

            // record .equ/.set/= for later .if / .rept evaluation
            self.record_symbol(code);

            // macro invocation? The macro name is the first token, optionally
            // after a leading `label:` (which we emit before expanding).
            let trimmed = code.trim_start();
            let (label, after_label) = match find_label_colon(trimmed) {
                Some(c) => (Some(&trimmed[..=c]), trimmed[c + 1..].trim_start()),
                None => (None, trimmed),
            };
            let mac_name = after_label.split_whitespace().next().unwrap_or("");
            if self.macros.contains_key(mac_name) && !after_label.starts_with('.') {
                if let Some(lbl) = label {
                    self.emit(lbl.to_string(), base + i);
                }
                self.expand_macro(mac_name, after_label, base + i);
                i += 1;
                continue;
            }

            self.emit(raw.clone(), base + i);
            i += 1;
        }
    }

    fn collect_macro(&mut self, lines: &[String], start: usize, base: usize) -> usize {
        let header = strip(&lines[start]);
        // `.macro name p1,p2` or `.macro name p1 p2`
        let rest = header[".macro".len()..].trim();
        let mut it = rest.splitn(2, |c: char| c.is_whitespace());
        let name = it.next().unwrap_or("").trim().to_string();
        let params: Vec<String> = it
            .next()
            .map(|p| p.split(|c| c == ',' || c == ' ').filter(|s| !s.is_empty()).map(|s| s.trim().to_string()).collect())
            .unwrap_or_default();
        let mut body = Vec::new();
        let mut i = start + 1;
        while i < lines.len() {
            if first_token(&lines[i]).eq_ignore_ascii_case(".endm") {
                break;
            }
            body.push(lines[i].clone());
            i += 1;
        }
        if i >= lines.len() {
            self.err(base + start, format!("`.macro {name}` has no `.endm`"));
        }
        if name.is_empty() {
            self.err(base + start, "`.macro` needs a name");
        } else {
            self.macros.insert(name, Macro { params, body });
        }
        i + 1
    }

    fn expand_macro(&mut self, name: &str, callsite: &str, at: usize) {
        self.uniq += 1;
        let uid = self.uniq;
        // args are everything after the macro name on the call line
        let after = callsite.trim();
        let after = after.strip_prefix(name).unwrap_or(after).trim();
        let args: Vec<String> =
            crate::split_args(after).into_iter().map(|s| s.trim().to_string()).collect();
        let mac = self.macros.get(name).unwrap();
        // build substitution map — named params (`\name`) plus positional
        // (`\1`, `\2`, …). GAS macros may use positionals with no named params
        // declared, so cover every supplied argument, not just declared ones.
        let mut subs: Vec<(String, String)> = Vec::new();
        let n = mac.params.len().max(args.len());
        for idx in 0..n {
            let val = args.get(idx).cloned().unwrap_or_default();
            subs.push((format!("\\{}", idx + 1), val.clone()));
            if let Some(p) = mac.params.get(idx) {
                subs.push((format!("\\{p}"), val));
            }
        }
        // Highest-numbered positionals first so `\10` isn't clipped by `\1`.
        subs.sort_by(|a, b| b.0.len().cmp(&a.0.len()));
        let body = mac.body.clone();
        if self.depth > MAX_DEPTH {
            self.err(at, "macro expansion too deep");
            return;
        }
        let expanded: Vec<String> = body
            .iter()
            .map(|l| {
                let mut s = l.clone();
                for (k, v) in &subs {
                    s = s.replace(k, v);
                }
                // unique-id tokens for macro-local labels
                s = s.replace("\\~", &uid.to_string()).replace("\\@", &uid.to_string());
                s
            })
            .collect();
        self.depth += 1;
        self.process(&expanded, at);
        self.depth -= 1;
    }

    fn collect_rept(&mut self, lines: &[String], start: usize, base: usize) -> usize {
        let header = strip(&lines[start]);
        let expr = header[".rept".len()..].trim();
        let count = self.eval(expr).unwrap_or(0).max(0) as usize;
        let mut body = Vec::new();
        let mut depth = 1;
        let mut i = start + 1;
        while i < lines.len() {
            let t = first_token(&lines[i]).to_ascii_lowercase();
            if t == ".rept" {
                depth += 1;
            }
            if t == ".endr" {
                depth -= 1;
                if depth == 0 {
                    break;
                }
            }
            body.push(lines[i].clone());
            i += 1;
        }
        if i >= lines.len() {
            self.err(base + start, "`.rept` has no `.endr`");
        }
        self.depth += 1;
        for _ in 0..count {
            self.process(&body, base + start + 1);
        }
        self.depth -= 1;
        i + 1
    }

    fn collect_if(&mut self, lines: &[String], start: usize, base: usize, kind: &str) -> usize {
        // A branch guard: `Some(true/false)` for if/elif, `None` for else.
        struct Branch {
            guard: Option<bool>,
            body: Vec<String>,
        }
        let cond0 = directive_arg(strip(&lines[start]));
        let mut branches = vec![Branch { guard: Some(self.cond_true(kind, &cond0)), body: Vec::new() }];
        let mut depth = 1;
        let mut i = start + 1;
        while i < lines.len() {
            let raw_tok = first_token(&lines[i]).to_ascii_lowercase();
            let d = norm_directive(&raw_tok);
            if matches!(d, ".if" | ".ifdef" | ".ifndef") {
                depth += 1;
            } else if d == ".endif" {
                depth -= 1;
                if depth == 0 {
                    break;
                }
            } else if d == ".elif" && depth == 1 {
                let cond = directive_arg(strip(&lines[i]));
                branches.push(Branch { guard: Some(self.cond_true(".if", &cond)), body: Vec::new() });
                i += 1;
                continue;
            } else if d == ".else" && depth == 1 {
                branches.push(Branch { guard: None, body: Vec::new() });
                i += 1;
                continue;
            }
            branches.last_mut().unwrap().body.push(lines[i].clone());
            i += 1;
        }
        if i >= lines.len() {
            self.err(base + start, "`.if`/`#if` has no `.endif`");
        }
        // First branch whose guard is true; else the `.else` branch.
        let chosen = branches
            .iter()
            .find(|b| b.guard == Some(true))
            .or_else(|| branches.iter().find(|b| b.guard.is_none()));
        if let Some(b) = chosen {
            let body = b.body.clone();
            self.process(&body, base + start + 1);
        }
        i + 1
    }

    /// Evaluate a conditional's truth for `.if`/`.ifdef`/`.ifndef`.
    fn cond_true(&self, kind: &str, cond: &str) -> bool {
        match kind {
            ".ifdef" => self.is_defined(cond),
            ".ifndef" => !self.is_defined(cond),
            _ => self.eval(&self.expand_defined(cond)).unwrap_or(0) != 0,
        }
    }

    fn is_defined(&self, name: &str) -> bool {
        let name = name.trim();
        self.syms.contains_key(name) || self.macros.contains_key(name)
    }

    /// Replace `defined(NAME)` / `defined NAME` with `1`/`0` so the numeric
    /// expression evaluator can handle a C `#if defined(...)` condition.
    fn expand_defined(&self, cond: &str) -> String {
        let mut out = String::with_capacity(cond.len());
        let b: Vec<char> = cond.chars().collect();
        let mut i = 0;
        while i < b.len() {
            if cond[i..].starts_with("defined") && (i == 0 || !b[i - 1].is_alphanumeric()) {
                let mut j = i + "defined".len();
                while j < b.len() && b[j].is_whitespace() {
                    j += 1;
                }
                let paren = b.get(j) == Some(&'(');
                if paren {
                    j += 1;
                }
                while j < b.len() && b[j].is_whitespace() {
                    j += 1;
                }
                let mut k = j;
                while k < b.len() && (b[k].is_alphanumeric() || b[k] == '_') {
                    k += 1;
                }
                let name: String = b[j..k].iter().collect();
                if paren {
                    while k < b.len() && b[k].is_whitespace() {
                        k += 1;
                    }
                    if b.get(k) == Some(&')') {
                        k += 1;
                    }
                }
                out.push(if self.is_defined(&name) { '1' } else { '0' });
                i = k;
                continue;
            }
            out.push(b[i]);
            i += 1;
        }
        out
    }

    /// `#define NAME [VALUE]` / `.define NAME VALUE`. A bodyless define is `1`;
    /// an integer body is recorded for `#if`. Non-integer bodies define the name
    /// as present (value 1) so `#ifdef`/`defined()` still see it.
    fn do_define(&mut self, code: &str, _at: usize) {
        let rest = code.trim_start_matches(['#', '.']).trim();
        let rest = rest.strip_prefix("define").unwrap_or(rest).trim();
        let mut it = rest.splitn(2, |c: char| c.is_whitespace());
        let name = it.next().unwrap_or("").trim();
        if name.is_empty() || !is_ident(name) {
            return;
        }
        let val = it.next().map(str::trim).filter(|s| !s.is_empty());
        let v = val.and_then(|v| self.eval(v)).unwrap_or(1);
        self.syms.insert(name.to_string(), v);
    }

    fn do_include(&mut self, code: &str, at: usize) {
        let arg = directive_arg(code);
        let arg = arg.trim().trim_matches(['"', '<', '>', '\'']);
        match self.inc.read(arg) {
            Some(text) => {
                let lines: Vec<String> = text.lines().map(|s| s.to_string()).collect();
                self.depth += 1;
                self.process(&lines, 1);
                self.depth -= 1;
            }
            None => self.err(at, format!("cannot find include file `{arg}`")),
        }
    }

    /// Record a `NAME .equ V` / `.equ NAME,V` / `NAME = V` / `.set` so `.if`/
    /// `.rept` can use it. Best-effort integer only.
    fn record_symbol(&mut self, code: &str) {
        if code.is_empty() {
            return;
        }
        let low = code.to_ascii_lowercase();
        // `.equ NAME, V` / `.set NAME, V`
        if low.starts_with(".equ") || low.starts_with(".set") {
            let rest = code[4..].trim();
            if let Some((name, val)) = rest.split_once(',') {
                if let Some(v) = self.eval(val.trim()) {
                    self.syms.insert(name.trim().to_string(), v);
                }
            }
            return;
        }
        // `NAME .equ V` / `NAME = V` / `NAME equ V`
        let mut it = code.splitn(2, |c: char| c.is_whitespace());
        let name = it.next().unwrap_or("");
        let after = it.next().unwrap_or("").trim();
        let al = after.to_ascii_lowercase();
        let val = if let Some(r) = after.strip_prefix('=') {
            Some(r.trim())
        } else if al.starts_with(".equ") {
            Some(after[4..].trim())
        } else if al.starts_with("equ") {
            Some(after[3..].trim())
        } else {
            None
        };
        if let (true, Some(v)) = (is_ident(name), val.and_then(|v| self.eval(v))) {
            self.syms.insert(name.to_string(), v);
        }
    }

    /// Evaluate a preprocess-time integer expression: literals, recorded
    /// symbols, and `+ - * / & | << >> = == != < > <= >= ! ( )` (a lone `=` is
    /// equality, as in rmac's `.if NOFILL=0`). Best-effort.
    fn eval(&self, expr: &str) -> Option<i64> {
        let toks = lex(expr)?;
        let mut p = EP { t: &toks, i: 0, syms: &self.syms };
        let v = p.expr(0)?;
        (p.i == p.t.len()).then_some(v)
    }
}

fn is_ident(s: &str) -> bool {
    !s.is_empty()
        && s.chars().next().map(|c| c.is_ascii_alphabetic() || c == '_' || c == '.').unwrap_or(false)
        && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '.' || c == '$')
}

// ── tiny preprocess-time expression evaluator (i64) ──────────────────────────

#[derive(Clone, PartialEq)]
enum T {
    N(i64),
    S(String),
    Op(String),
    L,
    R,
}

fn lex(s: &str) -> Option<Vec<T>> {
    let b: Vec<char> = s.chars().collect();
    let mut i = 0;
    let mut out = Vec::new();
    while i < b.len() {
        let c = b[i];
        if c.is_whitespace() {
            i += 1;
            continue;
        }
        if c == '$' {
            let mut j = i + 1;
            while j < b.len() && b[j].is_ascii_hexdigit() {
                j += 1;
            }
            out.push(T::N(i64::from_str_radix(&b[i + 1..j].iter().collect::<String>(), 16).ok()?));
            i = j;
        } else if c.is_ascii_digit() {
            if c == '0' && i + 1 < b.len() && b[i + 1].to_ascii_lowercase() == 'x' {
                let mut j = i + 2;
                while j < b.len() && b[j].is_ascii_hexdigit() {
                    j += 1;
                }
                out.push(T::N(i64::from_str_radix(&b[i + 2..j].iter().collect::<String>(), 16).ok()?));
                i = j;
            } else {
                let mut j = i;
                while j < b.len() && b[j].is_ascii_digit() {
                    j += 1;
                }
                out.push(T::N(b[i..j].iter().collect::<String>().parse().ok()?));
                i = j;
            }
        } else if c.is_ascii_alphabetic() || c == '_' || c == '.' {
            let mut j = i;
            while j < b.len() && (b[j].is_ascii_alphanumeric() || b[j] == '_' || b[j] == '.' || b[j] == '$') {
                j += 1;
            }
            out.push(T::S(b[i..j].iter().collect()));
            i = j;
        } else if c == '(' {
            out.push(T::L);
            i += 1;
        } else if c == ')' {
            out.push(T::R);
            i += 1;
        } else {
            let two: String = b[i..(i + 2).min(b.len())].iter().collect();
            if ["==", "!=", "<=", ">=", "<<", ">>", "&&", "||"].contains(&two.as_str()) {
                out.push(T::Op(two));
                i += 2;
            } else if "+-*/&|^<>!".contains(c) {
                out.push(T::Op(c.to_string()));
                i += 1;
            } else if c == '=' {
                // rmac uses a single `=` as equality in `.if`/`.rept` conditions
                // (`.if NOFILL=0`). `==` is already handled above; a lone `=` is
                // the same comparison. Without this it failed to lex and the
                // whole conditional block was silently dropped.
                out.push(T::Op("==".to_string()));
                i += 1;
            } else {
                return None;
            }
        }
    }
    Some(out)
}

struct EP<'a> {
    t: &'a [T],
    i: usize,
    syms: &'a HashMap<String, i64>,
}

impl EP<'_> {
    fn prec(op: &str) -> u8 {
        match op {
            "||" => 1,
            "&&" => 2,
            "|" | "^" => 3,
            "&" => 4,
            "==" | "!=" => 5,
            "<" | ">" | "<=" | ">=" => 6,
            "<<" | ">>" => 7,
            "+" | "-" => 8,
            "*" | "/" => 9,
            _ => 0,
        }
    }

    fn expr(&mut self, min: u8) -> Option<i64> {
        let mut lhs = self.unary()?;
        while let Some(T::Op(op)) = self.t.get(self.i) {
            let p = Self::prec(op);
            if p == 0 || p < min {
                break;
            }
            let op = op.clone();
            self.i += 1;
            let rhs = self.expr(p + 1)?;
            lhs = apply(&op, lhs, rhs);
        }
        Some(lhs)
    }

    fn unary(&mut self) -> Option<i64> {
        let t = self.t.get(self.i).cloned()?;
        self.i += 1;
        match t {
            T::N(n) => Some(n),
            T::S(name) => Some(*self.syms.get(&name).unwrap_or(&0)),
            T::Op(o) if o == "-" => Some(-self.unary()?),
            T::Op(o) if o == "!" => Some((self.unary()? == 0) as i64),
            T::Op(o) if o == "+" => self.unary(),
            T::L => {
                let v = self.expr(0)?;
                matches!(self.t.get(self.i), Some(T::R)).then(|| self.i += 1)?;
                Some(v)
            }
            _ => None,
        }
    }
}

fn apply(op: &str, a: i64, b: i64) -> i64 {
    match op {
        "+" => a + b,
        "-" => a - b,
        "*" => a * b,
        "/" => {
            if b == 0 {
                0
            } else {
                a / b
            }
        }
        "&" => a & b,
        "|" => a | b,
        "^" => a ^ b,
        "<<" => a << b,
        ">>" => a >> b,
        "==" => (a == b) as i64,
        "!=" => (a != b) as i64,
        "<" => (a < b) as i64,
        ">" => (a > b) as i64,
        "<=" => (a <= b) as i64,
        ">=" => (a >= b) as i64,
        "&&" => ((a != 0) && (b != 0)) as i64,
        "||" => ((a != 0) || (b != 0)) as i64,
        _ => 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pp(src: &str) -> String {
        run(src, &mut NoIncludes, &[]).unwrap()
    }

    #[test]
    fn rept_expands() {
        let out = pp(".rept 3\n    nop\n.endr\n");
        assert_eq!(out.matches("nop").count(), 3);
    }

    #[test]
    fn macro_positional_and_named() {
        let out = pp(".macro ld val,reg\n    movei #\\val,\\reg\n.endm\n    ld $100,r5\n");
        assert!(out.contains("movei #$100,r5"), "got: {out}");
    }

    #[test]
    fn conditional_true_and_false() {
        let out = pp("DEBUG .equ 1\n.if DEBUG\n    nop\n.else\n    move r1,r2\n.endif\n");
        assert!(out.contains("nop"));
        assert!(!out.contains("move r1,r2"));
    }

    #[test]
    fn ifndef_guard() {
        let out = pp(".ifdef NOThere\n    bad\n.endif\n    good\n");
        assert!(!out.contains("bad"));
        assert!(out.contains("good"));
    }

    #[test]
    fn macro_unique_label() {
        let out = pp(".macro spin\n.l\\@:\n    jr .l\\@\n    nop\n.endm\n    spin\n    spin\n");
        // two expansions -> two distinct local labels
        assert!(out.contains(".l1"));
        assert!(out.contains(".l2"));
    }
}
