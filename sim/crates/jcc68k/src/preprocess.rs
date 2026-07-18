//! A C preprocessor: `#include`, `#define` (object- and function-like macros
//! with argument substitution), `#undef`, and the conditional family
//! (`#if`/`#ifdef`/`#ifndef`/`#elif`/`#else`/`#endif`) with `defined()` and
//! constant-expression evaluation. Line continuations and comments are handled
//! up front. Output is expanded C text ready for the lexer.
//!
//! Scope note: macro expansion is token-based with a per-expansion hide set to
//! stop self-recursion; function-macro invocations may span lines. This covers
//! the vast majority of real C headers; the hairier corners (`#`/`##` operators,
//! variadic macros, `_Pragma`) are follow-ups.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

#[derive(Clone)]
struct Macro {
    params: Option<Vec<String>>, // None = object-like
    body: Vec<Tk>,
}

/// A minimal preprocessing token: identifiers, numbers, strings/chars kept
/// verbatim, punctuation, and everything else as `Other`.
#[derive(Clone, Debug, PartialEq)]
enum Tk {
    Id(String),
    Punct(String),
    Str(String), // includes quotes
    Other(String),
    Hash,   // #
    HashHash, // ##
}

pub struct Pp {
    macros: HashMap<String, Macro>,
    include_dirs: Vec<PathBuf>,
    active_stack: Vec<CondFrame>,
    out: String,
    included: HashSet<PathBuf>,
    depth: usize,
}

#[derive(Clone)]
struct CondFrame {
    /// Are we currently emitting (this frame's branch is active AND all parents)?
    active: bool,
    /// Has any branch in this if/elif/else chain been taken yet?
    taken: bool,
    /// Was the parent active (so an else/elif can re-activate)?
    parent_active: bool,
}

pub fn preprocess(src: &str, path: &Path, include_dirs: &[String]) -> Result<String, String> {
    let mut pp = Pp {
        macros: builtin_macros(),
        include_dirs: include_dirs.iter().map(PathBuf::from).collect(),
        active_stack: Vec::new(),
        out: String::new(),
        included: HashSet::new(),
        depth: 0,
    };
    pp.run(src, path)?;
    Ok(pp.out)
}

fn builtin_macros() -> HashMap<String, Macro> {
    let mut m = HashMap::new();
    let obj = |s: &str| Macro { params: None, body: lex_pp(s) };
    m.insert("__STDC__".into(), obj("1"));
    m.insert("__JAGUAR__".into(), obj("1"));
    m.insert("__jcc68k__".into(), obj("1"));
    m
}

impl Pp {
    fn active(&self) -> bool {
        self.active_stack.iter().all(|f| f.active)
    }

    fn run(&mut self, src: &str, path: &Path) -> Result<(), String> {
        let joined = splice_lines(src);
        let lines = logical_lines(&joined);
        let mut i = 0;
        while i < lines.len() {
            let raw = &lines[i];
            let trimmed = raw.trim_start();
            if trimmed.starts_with('#') {
                self.directive(&trimmed[1..], path)?;
            } else if self.active() {
                // Expand macros and emit. A function-macro call may need more
                // lines to close its parenthesis; gather them.
                let mut text = raw.clone();
                while needs_more_lines(&text) && i + 1 < lines.len() {
                    i += 1;
                    text.push('\n');
                    text.push_str(&lines[i]);
                }
                let toks = lex_pp(&text);
                let expanded = self.expand(toks, &mut HashSet::new());
                self.out.push_str(&render(&expanded));
                self.out.push('\n');
            }
            i += 1;
        }
        Ok(())
    }

    fn directive(&mut self, d: &str, path: &Path) -> Result<(), String> {
        let d = d.trim();
        let (word, rest) = split_word(d);
        match word {
            "define" if self.active() => self.do_define(rest),
            "undef" if self.active() => {
                let (name, _) = split_word(rest);
                self.macros.remove(name);
                Ok(())
            }
            "include" if self.active() => self.do_include(rest, path),
            "ifdef" => {
                let (name, _) = split_word(rest);
                let cond = self.macros.contains_key(name);
                self.push_cond(cond);
                Ok(())
            }
            "ifndef" => {
                let (name, _) = split_word(rest);
                let cond = !self.macros.contains_key(name);
                self.push_cond(cond);
                Ok(())
            }
            "if" => {
                let cond = self.eval_cond(rest)?;
                self.push_cond(cond);
                Ok(())
            }
            "elif" => {
                self.do_elif(rest)?;
                Ok(())
            }
            "else" => {
                if let Some(f) = self.active_stack.last_mut() {
                    f.active = f.parent_active && !f.taken;
                    f.taken = true;
                }
                Ok(())
            }
            "endif" => {
                self.active_stack.pop();
                Ok(())
            }
            "error" if self.active() => Err(format!("#error {}", rest.trim())),
            "pragma" | "line" | "warning" | "error" | "define" | "undef" | "include" => Ok(()),
            _ => Ok(()), // unknown / inactive directive
        }
    }

    fn push_cond(&mut self, cond: bool) {
        let parent = self.active();
        self.active_stack.push(CondFrame {
            active: parent && cond,
            taken: cond,
            parent_active: parent,
        });
    }

    fn do_elif(&mut self, rest: &str) -> Result<(), String> {
        // Evaluate only if the parent is active and no branch taken yet.
        let (parent_active, already) = match self.active_stack.last() {
            Some(f) => (f.parent_active, f.taken),
            None => return Ok(()),
        };
        let cond = if parent_active && !already {
            self.eval_cond(rest)?
        } else {
            false
        };
        if let Some(f) = self.active_stack.last_mut() {
            f.active = parent_active && !already && cond;
            f.taken = already || cond;
        }
        Ok(())
    }

    fn do_define(&mut self, rest: &str) -> Result<(), String> {
        let rest = rest.trim_start();
        let (name, after) = split_ident(rest);
        if name.is_empty() {
            return Ok(());
        }
        // function-like iff '(' immediately follows the name (no space)
        if after.starts_with('(') {
            let close = after.find(')').ok_or("unterminated macro parameter list")?;
            let params: Vec<String> = after[1..close]
                .split(',')
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect();
            let body = lex_pp(after[close + 1..].trim());
            self.macros.insert(name.to_string(), Macro { params: Some(params), body });
        } else {
            let body = lex_pp(after.trim());
            self.macros.insert(name.to_string(), Macro { params: None, body });
        }
        Ok(())
    }

    fn do_include(&mut self, rest: &str, cur: &Path) -> Result<(), String> {
        let rest = rest.trim();
        // Expand macros in a computed include (#include SOME_MACRO).
        let (fname, angle) = if let Some(inner) = rest.strip_prefix('"') {
            (inner.trim_end_matches('"').to_string(), false)
        } else if let Some(inner) = rest.strip_prefix('<') {
            (inner.trim_end_matches('>').to_string(), true)
        } else {
            let toks = lex_pp(rest);
            let e = self.expand(toks, &mut HashSet::new());
            let s = render(&e);
            let s = s.trim();
            if let Some(inner) = s.strip_prefix('"') {
                (inner.trim_end_matches('"').to_string(), false)
            } else if let Some(inner) = s.strip_prefix('<') {
                (inner.trim_end_matches('>').to_string(), true)
            } else {
                return Ok(());
            }
        };
        let resolved = self.resolve_include(&fname, cur, angle);
        let Some(p) = resolved else {
            // Missing header: skip rather than fail, so system headers we don't
            // ship don't stop compilation (their declarations must be provided
            // another way). A stricter mode can turn this into an error.
            return Ok(());
        };
        if self.included.contains(&p) || self.depth > 64 {
            return Ok(()); // include-once-ish guard + recursion cap
        }
        self.included.insert(p.clone());
        let text = std::fs::read_to_string(&p).map_err(|e| format!("{}: {e}", p.display()))?;
        self.depth += 1;
        self.run(&text, &p)?;
        self.depth -= 1;
        Ok(())
    }

    fn resolve_include(&self, fname: &str, cur: &Path, angle: bool) -> Option<PathBuf> {
        if !angle {
            if let Some(dir) = cur.parent() {
                let p = dir.join(fname);
                if p.exists() {
                    return Some(p);
                }
            }
        }
        for dir in &self.include_dirs {
            let p = dir.join(fname);
            if p.exists() {
                return Some(p);
            }
        }
        None
    }

    // ── macro expansion ───────────────────────────────────────────────────────
    fn expand(&self, toks: Vec<Tk>, hide: &mut HashSet<String>) -> Vec<Tk> {
        let mut out = Vec::new();
        let mut i = 0;
        while i < toks.len() {
            let t = toks[i].clone();
            if let Tk::Id(name) = &t {
                if !hide.contains(name) {
                    if let Some(m) = self.macros.get(name) {
                        match &m.params {
                            None => {
                                // object-like
                                let mut h2 = hide.clone();
                                h2.insert(name.clone());
                                let sub = self.expand(m.body.clone(), &mut h2);
                                out.extend(sub);
                                i += 1;
                                continue;
                            }
                            Some(params) => {
                                // function-like: need a '(' next
                                if let Some((args, consumed)) = gather_args(&toks, i + 1) {
                                    let body = substitute(&m.body, params, &args, self, hide);
                                    let mut h2 = hide.clone();
                                    h2.insert(name.clone());
                                    let sub = self.expand(body, &mut h2);
                                    out.extend(sub);
                                    // Advance past `name ( … )`: name is at i, the
                                    // '(' at i+1, and `consumed` is the ')' offset
                                    // from that '('. Skip one past the ')'.
                                    i += consumed + 2;
                                    continue;
                                }
                            }
                        }
                    }
                }
            }
            out.push(t);
            i += 1;
        }
        out
    }

    // ── #if constant expression ───────────────────────────────────────────────
    fn eval_cond(&self, expr: &str) -> Result<bool, String> {
        // Replace `defined X` / `defined(X)` first, then macro-expand, then
        // evaluate remaining identifiers as 0.
        let toks = lex_pp(expr);
        let toks = self.apply_defined(toks);
        let toks = self.expand(toks, &mut HashSet::new());
        let text = render(&toks);
        let val = eval_int_expr(&text)?;
        Ok(val != 0)
    }

    fn apply_defined(&self, toks: Vec<Tk>) -> Vec<Tk> {
        let mut out = Vec::new();
        let mut i = 0;
        while i < toks.len() {
            if let Tk::Id(k) = &toks[i] {
                if k == "defined" {
                    // defined X  or  defined ( X )
                    let mut j = i + 1;
                    let mut name = None;
                    if matches!(toks.get(j), Some(Tk::Punct(p)) if p == "(") {
                        j += 1;
                        if let Some(Tk::Id(n)) = toks.get(j) {
                            name = Some(n.clone());
                            j += 1;
                        }
                        if matches!(toks.get(j), Some(Tk::Punct(p)) if p == ")") {
                            j += 1;
                        }
                    } else if let Some(Tk::Id(n)) = toks.get(j) {
                        name = Some(n.clone());
                        j += 1;
                    }
                    let v = name.map(|n| self.macros.contains_key(&n)).unwrap_or(false);
                    out.push(Tk::Other(if v { "1".into() } else { "0".into() }));
                    i = j;
                    continue;
                }
            }
            out.push(toks[i].clone());
            i += 1;
        }
        out
    }
}

/// Substitute macro parameters in a function-macro body with the actual args
/// (each arg is itself expanded).
fn substitute(body: &[Tk], params: &[String], args: &[Vec<Tk>], pp: &Pp, hide: &HashSet<String>) -> Vec<Tk> {
    let mut out = Vec::new();
    for t in body {
        if let Tk::Id(name) = t {
            if let Some(idx) = params.iter().position(|p| p == name) {
                if let Some(arg) = args.get(idx) {
                    let mut h = hide.clone();
                    out.extend(pp.expand(arg.clone(), &mut h));
                    continue;
                }
            }
        }
        out.push(t.clone());
    }
    out
}

/// Gather `( arg, arg, … )` starting at `start` (which should be `(`). Returns
/// the argument token lists and the number of tokens consumed (including parens).
fn gather_args(toks: &[Tk], start: usize) -> Option<(Vec<Vec<Tk>>, usize)> {
    if !matches!(toks.get(start), Some(Tk::Punct(p)) if p == "(") {
        return None;
    }
    let mut args: Vec<Vec<Tk>> = Vec::new();
    let mut cur: Vec<Tk> = Vec::new();
    let mut depth = 0i32;
    let mut i = start;
    loop {
        let t = toks.get(i)?;
        match t {
            Tk::Punct(p) if p == "(" => {
                if depth > 0 {
                    cur.push(t.clone());
                }
                depth += 1;
            }
            Tk::Punct(p) if p == ")" => {
                depth -= 1;
                if depth == 0 {
                    if !cur.is_empty() || !args.is_empty() {
                        args.push(std::mem::take(&mut cur));
                    }
                    return Some((args, i - start));
                }
                cur.push(t.clone());
            }
            Tk::Punct(p) if p == "," && depth == 1 => {
                args.push(std::mem::take(&mut cur));
            }
            other => cur.push(other.clone()),
        }
        i += 1;
    }
}

fn needs_more_lines(text: &str) -> bool {
    // Heuristic: an unbalanced '(' outside strings suggests a macro call spans
    // more lines. Cheap and good enough for gathering function-macro args.
    let mut depth = 0i32;
    let mut in_str = false;
    let mut in_ch = false;
    let mut prev = ' ';
    for c in text.chars() {
        match c {
            '"' if !in_ch && prev != '\\' => in_str = !in_str,
            '\'' if !in_str && prev != '\\' => in_ch = !in_ch,
            '(' if !in_str && !in_ch => depth += 1,
            ')' if !in_str && !in_ch => depth -= 1,
            _ => {}
        }
        prev = c;
    }
    depth > 0
}

// ── small helpers ────────────────────────────────────────────────────────────

fn splice_lines(src: &str) -> String {
    // Join backslash-newline; strip comments (block + line) so directives and
    // macro bodies don't get confused by them.
    let no_comments = strip_comments(src);
    no_comments.replace("\\\n", "")
}

fn strip_comments(src: &str) -> String {
    let b = src.as_bytes();
    let mut out = String::with_capacity(src.len());
    let mut i = 0;
    let mut in_str = false;
    let mut in_ch = false;
    while i < b.len() {
        let c = b[i];
        if in_str {
            out.push(c as char);
            if c == b'\\' && i + 1 < b.len() {
                out.push(b[i + 1] as char);
                i += 2;
                continue;
            }
            if c == b'"' {
                in_str = false;
            }
            i += 1;
            continue;
        }
        if in_ch {
            out.push(c as char);
            if c == b'\\' && i + 1 < b.len() {
                out.push(b[i + 1] as char);
                i += 2;
                continue;
            }
            if c == b'\'' {
                in_ch = false;
            }
            i += 1;
            continue;
        }
        if c == b'"' {
            in_str = true;
            out.push('"');
            i += 1;
            continue;
        }
        if c == b'\'' {
            in_ch = true;
            out.push('\'');
            i += 1;
            continue;
        }
        if c == b'/' && b.get(i + 1) == Some(&b'/') {
            while i < b.len() && b[i] != b'\n' {
                i += 1;
            }
            continue;
        }
        if c == b'/' && b.get(i + 1) == Some(&b'*') {
            i += 2;
            while i + 1 < b.len() && !(b[i] == b'*' && b[i + 1] == b'/') {
                if b[i] == b'\n' {
                    out.push('\n'); // keep line count roughly
                }
                i += 1;
            }
            i += 2;
            out.push(' ');
            continue;
        }
        out.push(c as char);
        i += 1;
    }
    out
}

fn logical_lines(src: &str) -> Vec<String> {
    src.split('\n').map(|s| s.to_string()).collect()
}

fn split_word(s: &str) -> (&str, &str) {
    let s = s.trim_start();
    let end = s.find(|c: char| c.is_whitespace()).unwrap_or(s.len());
    (&s[..end], &s[end..])
}

fn split_ident(s: &str) -> (&str, &str) {
    let end = s
        .find(|c: char| !(c == '_' || c.is_ascii_alphanumeric()))
        .unwrap_or(s.len());
    (&s[..end], &s[end..])
}

/// Tokenize preprocessing text (identifiers / numbers / strings / punctuation).
fn lex_pp(s: &str) -> Vec<Tk> {
    let b = s.as_bytes();
    let mut i = 0;
    let mut out = Vec::new();
    while i < b.len() {
        let c = b[i];
        if c.is_ascii_whitespace() {
            i += 1;
            continue;
        }
        if c == b'_' || c.is_ascii_alphabetic() {
            let start = i;
            while i < b.len() && (b[i] == b'_' || b[i].is_ascii_alphanumeric()) {
                i += 1;
            }
            out.push(Tk::Id(s[start..i].to_string()));
            continue;
        }
        if c.is_ascii_digit() {
            let start = i;
            while i < b.len()
                && (b[i].is_ascii_alphanumeric() || b[i] == b'.' || b[i] == b'x' || b[i] == b'X')
            {
                i += 1;
            }
            out.push(Tk::Other(s[start..i].to_string()));
            continue;
        }
        if c == b'"' || c == b'\'' {
            let q = c;
            let start = i;
            i += 1;
            while i < b.len() && b[i] != q {
                if b[i] == b'\\' {
                    i += 1;
                }
                i += 1;
            }
            i += 1;
            out.push(Tk::Str(s[start..i.min(b.len())].to_string()));
            continue;
        }
        if c == b'#' {
            if b.get(i + 1) == Some(&b'#') {
                out.push(Tk::HashHash);
                i += 2;
            } else {
                out.push(Tk::Hash);
                i += 1;
            }
            continue;
        }
        // multi-char punctuators
        let two = if i + 1 < b.len() { &s[i..i + 2] } else { "" };
        const P2: &[&str] = &[
            "<<", ">>", "<=", ">=", "==", "!=", "&&", "||", "->", "++", "--", "+=", "-=",
        ];
        if P2.contains(&two) {
            out.push(Tk::Punct(two.to_string()));
            i += 2;
            continue;
        }
        out.push(Tk::Punct((c as char).to_string()));
        i += 1;
    }
    out
}

fn render(toks: &[Tk]) -> String {
    let mut out = String::new();
    for (i, t) in toks.iter().enumerate() {
        if i > 0 {
            out.push(' ');
        }
        match t {
            Tk::Id(s) | Tk::Other(s) | Tk::Punct(s) | Tk::Str(s) => out.push_str(s),
            Tk::Hash => out.push('#'),
            Tk::HashHash => out.push_str("##"),
        }
    }
    out
}

/// Evaluate a constant integer expression for `#if` (after macro expansion).
/// Supports the usual C operators; unknown identifiers are treated as 0.
fn eval_int_expr(s: &str) -> Result<i64, String> {
    let toks = lex_pp(s);
    let mut p = EParser { toks, pos: 0 };
    let v = p.ternary()?;
    Ok(v)
}

struct EParser {
    toks: Vec<Tk>,
    pos: usize,
}
impl EParser {
    fn peek(&self) -> Option<&Tk> {
        self.toks.get(self.pos)
    }
    fn eat(&mut self, p: &str) -> bool {
        if matches!(self.peek(), Some(Tk::Punct(x)) if x == p) {
            self.pos += 1;
            true
        } else {
            false
        }
    }
    fn ternary(&mut self) -> Result<i64, String> {
        let c = self.bin(0)?;
        if self.eat("?") {
            let a = self.ternary()?;
            self.eat(":");
            let b = self.ternary()?;
            Ok(if c != 0 { a } else { b })
        } else {
            Ok(c)
        }
    }
    fn bin(&mut self, min: u8) -> Result<i64, String> {
        let mut lhs = self.unary()?;
        while let Some(op) = self.peek_binop() {
            let (prec, _) = binprec(&op);
            if prec < min {
                break;
            }
            self.pos += 1;
            let rhs = self.bin(prec + 1)?;
            lhs = apply(&op, lhs, rhs);
        }
        Ok(lhs)
    }
    fn peek_binop(&self) -> Option<String> {
        if let Some(Tk::Punct(p)) = self.peek() {
            if binprec(p).0 > 0 {
                return Some(p.clone());
            }
        }
        None
    }
    fn unary(&mut self) -> Result<i64, String> {
        if self.eat("!") {
            return Ok((self.unary()? == 0) as i64);
        }
        if self.eat("-") {
            return Ok(-self.unary()?);
        }
        if self.eat("~") {
            return Ok(!self.unary()?);
        }
        if self.eat("+") {
            return self.unary();
        }
        if self.eat("(") {
            let v = self.ternary()?;
            self.eat(")");
            return Ok(v);
        }
        match self.peek().cloned() {
            Some(Tk::Other(n)) => {
                self.pos += 1;
                parse_pp_int(&n)
            }
            Some(Tk::Id(_)) => {
                self.pos += 1;
                Ok(0) // unknown identifier → 0
            }
            _ => Ok(0),
        }
    }
}

fn binprec(op: &str) -> (u8, bool) {
    match op {
        "||" => (1, false),
        "&&" => (2, false),
        "|" => (3, false),
        "^" => (4, false),
        "&" => (5, false),
        "==" | "!=" => (6, false),
        "<" | ">" | "<=" | ">=" => (7, false),
        "<<" | ">>" => (8, false),
        "+" | "-" => (9, false),
        "*" | "/" | "%" => (10, false),
        _ => (0, false),
    }
}

fn apply(op: &str, a: i64, b: i64) -> i64 {
    match op {
        "||" => ((a != 0) || (b != 0)) as i64,
        "&&" => ((a != 0) && (b != 0)) as i64,
        "|" => a | b,
        "^" => a ^ b,
        "&" => a & b,
        "==" => (a == b) as i64,
        "!=" => (a != b) as i64,
        "<" => (a < b) as i64,
        ">" => (a > b) as i64,
        "<=" => (a <= b) as i64,
        ">=" => (a >= b) as i64,
        "<<" => a << b,
        ">>" => a >> b,
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
        "%" => {
            if b == 0 {
                0
            } else {
                a % b
            }
        }
        _ => 0,
    }
}

fn parse_pp_int(s: &str) -> Result<i64, String> {
    let s = s.trim_end_matches(|c| c == 'u' || c == 'U' || c == 'l' || c == 'L');
    if let Some(h) = s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")) {
        i64::from_str_radix(h, 16).map_err(|e| e.to_string())
    } else if s.len() > 1 && s.starts_with('0') && s.bytes().all(|b| (b'0'..=b'7').contains(&b)) {
        i64::from_str_radix(s, 8).map_err(|e| e.to_string())
    } else {
        s.parse::<i64>().map_err(|e| e.to_string())
    }
}
