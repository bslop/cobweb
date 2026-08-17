//! C lexer. Turns source into a token stream for the parser. Handles the C
//! preprocessor's already-expanded output (no macro expansion here yet — that
//! lives in a preprocessing pass); comments and whitespace are skipped.

#[derive(Debug, Clone, PartialEq)]
pub enum Tok {
    // literals / identifiers
    /// Integer literal: (value, was it `u`-suffixed / too big for int).
    Num(i64, bool),
    /// Floating literal (kept as f64; lowered to 16.16 fixed-point by codegen).
    Float(f64),
    Str(Vec<u8>),
     Char(i64),
    Ident(String),
    Keyword(String),
    // punctuation / operators (kept as their source spelling)
    Punct(String),
    Eof,
}

#[derive(Debug, Clone)]
pub struct Token {
    pub tok: Tok,
    pub line: usize,
    pub col: usize,
    /// Source file this token came from (set by the preprocessor's `# N "file"`
    /// line markers; empty when the input carried none).
    pub file: std::rc::Rc<str>,
}

const KEYWORDS: &[&str] = &[
    "void", "char", "short", "int", "long", "unsigned", "signed", "float", "double",
    "struct", "union", "enum", "typedef", "static", "extern", "const", "volatile",
    "if", "else", "while", "for", "do", "switch", "case", "default", "break",
    "continue", "return", "goto", "sizeof", "register", "_Bool", "inline",
];

/// Multi-char punctuators, longest first so the greedy match is correct.
const PUNCTS: &[&str] = &[
    "<<=", ">>=", "...", "->", "++", "--", "<<", ">>", "<=", ">=", "==", "!=",
    "&&", "||", "+=", "-=", "*=", "/=", "%=", "&=", "|=", "^=", "##",
    "+", "-", "*", "/", "%", "&", "|", "^", "~", "!", "=", "<", ">", "?", ":",
    ";", ",", ".", "(", ")", "[", "]", "{", "}", "#",
];

pub fn lex(src: &str) -> Result<Vec<Token>, String> {
    let b = src.as_bytes();
    let mut i = 0usize;
    let mut line = 1usize;
    let mut col = 1usize;
    let mut file: std::rc::Rc<str> = std::rc::Rc::from("");
    let mut out = Vec::new();
    let mut bump = |i: &mut usize, line: &mut usize, col: &mut usize, n: usize| {
        for _ in 0..n {
            if b.get(*i) == Some(&b'\n') {
                *line += 1;
                *col = 1;
            } else {
                *col += 1;
            }
            *i += 1;
        }
    };
    while i < b.len() {
        let c = b[i];
        // whitespace
        if c == b' ' || c == b'\t' || c == b'\r' || c == b'\n' {
            bump(&mut i, &mut line, &mut col, 1);
            continue;
        }
        // line comment
        if c == b'/' && b.get(i + 1) == Some(&b'/') {
            while i < b.len() && b[i] != b'\n' {
                bump(&mut i, &mut line, &mut col, 1);
            }
            continue;
        }
        // block comment
        if c == b'/' && b.get(i + 1) == Some(&b'*') {
            bump(&mut i, &mut line, &mut col, 2);
            while i < b.len() && !(b[i] == b'*' && b.get(i + 1) == Some(&b'/')) {
                bump(&mut i, &mut line, &mut col, 1);
            }
            bump(&mut i, &mut line, &mut col, 2);
            continue;
        }
        // Preprocessor line marker (`# 12 "file"`): resync our line counter and
        // current file so every diagnostic names the true source position, not
        // a position in the expanded text. Any other `#...` line is skipped.
        if c == b'#' && (col == 1 || out.last().map(|t: &Token| matches!(&t.tok, Tok::Eof)).unwrap_or(true)) {
            let eol = b[i..].iter().position(|&x| x == b'\n').map(|n| i + n).unwrap_or(b.len());
            let rest = std::str::from_utf8(&b[i + 1..eol]).unwrap_or("").trim();
            if let Some((num, fname)) = parse_line_marker(rest) {
                i = (eol + 1).min(b.len()); // past the newline
                line = num; // the NEXT line is `num`
                col = 1;
                if let Some(f) = fname {
                    if &*file != f {
                        file = std::rc::Rc::from(f);
                    }
                }
                continue;
            }
            while i < b.len() && b[i] != b'\n' {
                bump(&mut i, &mut line, &mut col, 1);
            }
            continue;
        }
        let (sl, sc) = (line, col);
        // number: integer (hex/oct/dec) or float (has '.' or exponent)
        if c.is_ascii_digit() || (c == b'.' && b.get(i + 1).is_some_and(|d| d.is_ascii_digit())) {
            let (tok, n) = scan_number(&b[i..])?;
            bump(&mut i, &mut line, &mut col, n);
            out.push(Token { tok, line: sl, col: sc, file: file.clone() });
            continue;
        }
        // char literal
        if c == b'\'' {
            let (val, n) = lex_char(&b[i..])?;
            bump(&mut i, &mut line, &mut col, n);
            out.push(Token { tok: Tok::Char(val), line: sl, col: sc, file: file.clone() });
            continue;
        }
        // string literal
        if c == b'"' {
            let (bytes, n) = lex_string(&b[i..])?;
            bump(&mut i, &mut line, &mut col, n);
            out.push(Token { tok: Tok::Str(bytes), line: sl, col: sc, file: file.clone() });
            continue;
        }
        // identifier / keyword
        if c == b'_' || c.is_ascii_alphabetic() {
            let start = i;
            while i < b.len() && (b[i] == b'_' || b[i].is_ascii_alphanumeric()) {
                bump(&mut i, &mut line, &mut col, 1);
            }
            let s = std::str::from_utf8(&b[start..i]).unwrap().to_string();
            let tok = if KEYWORDS.contains(&s.as_str()) {
                Tok::Keyword(s)
            } else {
                Tok::Ident(s)
            };
            out.push(Token { tok, line: sl, col: sc, file: file.clone() });
            continue;
        }
        // punctuator (greedy longest match)
        let mut matched = None;
        for p in PUNCTS {
            if b[i..].starts_with(p.as_bytes()) {
                matched = Some(*p);
                break;
            }
        }
        match matched {
            Some(p) => {
                bump(&mut i, &mut line, &mut col, p.len());
                out.push(Token { tok: Tok::Punct(p.to_string()), line: sl, col: sc, file: file.clone() });
            }
            None => return Err(format!("{sl}:{sc}: stray '{}' in program", c as char)),
        }
    }
    out.push(Token { tok: Tok::Eof, line, col, file });
    Ok(out)
}

/// Parse the body of a line marker (everything after the `#`): `12 "file"` or
/// bare `12` (also accepts cpp's `line 12 "file"` spelling). Returns the line
/// number and the file name if present. Trailing flags (cpp's `1`/`2`…) are
/// ignored.
fn parse_line_marker(rest: &str) -> Option<(usize, Option<&str>)> {
    let rest = rest.strip_prefix("line").map(str::trim_start).unwrap_or(rest);
    let end = rest.find(|c: char| !c.is_ascii_digit()).unwrap_or(rest.len());
    if end == 0 {
        return None;
    }
    let num: usize = rest[..end].parse().ok()?;
    let after = rest[end..].trim_start();
    let fname = after
        .strip_prefix('"')
        .and_then(|a| a.split_once('"'))
        .map(|(name, _flags)| name);
    Some((num, fname))
}

/// Scan a numeric literal, distinguishing integers from floats (`.`/exponent).
fn scan_number(b: &[u8]) -> Result<(Tok, usize), String> {
    // hex / octal are always integers
    if b.len() >= 2 && b[0] == b'0' && (b[1] == b'x' || b[1] == b'X') {
        let (v, u, n) = lex_number(b)?;
        return Ok((Tok::Num(v, u), n));
    }
    let mut i = 0;
    let mut is_float = false;
    while i < b.len() && b[i].is_ascii_digit() {
        i += 1;
    }
    if i < b.len() && b[i] == b'.' {
        is_float = true;
        i += 1;
        while i < b.len() && b[i].is_ascii_digit() {
            i += 1;
        }
    }
    if i < b.len() && (b[i] == b'e' || b[i] == b'E') {
        is_float = true;
        i += 1;
        if i < b.len() && (b[i] == b'+' || b[i] == b'-') {
            i += 1;
        }
        while i < b.len() && b[i].is_ascii_digit() {
            i += 1;
        }
    }
    if is_float {
        let s = std::str::from_utf8(&b[..i]).unwrap();
        let f = s.parse::<f64>().map_err(|e| format!("bad float literal: {e}"))?;
        while i < b.len() && matches!(b[i], b'f' | b'F' | b'l' | b'L') {
            i += 1;
        }
        Ok((Tok::Float(f), i))
    } else {
        let (v, u, n) = lex_number(b)?;
        Ok((Tok::Num(v, u), n))
    }
}

fn lex_number(b: &[u8]) -> Result<(i64, bool, usize), String> {
    let mut i = 0;
    let val: i64;
    if b.len() >= 2 && b[0] == b'0' && (b[1] == b'x' || b[1] == b'X') {
        i = 2;
        let start = i;
        while i < b.len() && b[i].is_ascii_hexdigit() {
            i += 1;
        }
        val = i64::from_str_radix(std::str::from_utf8(&b[start..i]).unwrap(), 16)
            .map_err(|e| format!("bad hex literal: {e}"))?;
    } else if b[0] == b'0' && b.len() > 1 && b[1].is_ascii_digit() {
        i = 1;
        let start = i;
        while i < b.len() && (b'0'..=b'7').contains(&b[i]) {
            i += 1;
        }
        val = i64::from_str_radix(std::str::from_utf8(&b[start..i]).unwrap(), 8)
            .map_err(|e| format!("bad octal literal: {e}"))?;
    } else {
        while i < b.len() && b[i].is_ascii_digit() {
            i += 1;
        }
        val = std::str::from_utf8(&b[..i]).unwrap().parse::<i64>()
            .map_err(|e| format!("bad integer literal: {e}"))?;
    }
    // Consume integer suffixes u/U/l/L, and REMEMBER whether one made this
    // unsigned. Discarding that made `1u` a plain `int`, so `(0u - 1u) > 0u`
    // was false and `(1u - 6u) >> 8` folded as an arithmetic shift — the
    // literal path disagreed with the identical expression written through
    // `unsigned` variables. `long` is 32-bit here, so l/L changes nothing.
    let mut uns = false;
    while i < b.len() && matches!(b[i], b'u' | b'U' | b'l' | b'L') {
        if b[i] == b'u' || b[i] == b'U' {
            uns = true;
        }
        i += 1;
    }
    // A value too large for `int` is unsigned as well: `int` and `long` are both
    // 32-bit on this target and `long long` is unsupported, so unsigned int is
    // the only type left that can represent it.
    if val > i32::MAX as i64 || val < i32::MIN as i64 {
        uns = true;
    }
    Ok((val, uns, i))
}

fn escape(b: &[u8]) -> Result<(u8, usize), String> {
    // b starts right after the backslash
    Ok(match b.first() {
        Some(b'n') => (b'\n', 1),
        Some(b't') => (b'\t', 1),
        Some(b'r') => (b'\r', 1),
        Some(b'0') => (0, 1),
        Some(b'\\') => (b'\\', 1),
        Some(b'\'') => (b'\'', 1),
        Some(b'"') => (b'"', 1),
        Some(b'a') => (7, 1),
        Some(b'b') => (8, 1),
        Some(b'f') => (12, 1),
        Some(b'v') => (11, 1),
        Some(b'x') => {
            let mut i = 1;
            let mut v: u32 = 0;
            while i < b.len() && b[i].is_ascii_hexdigit() {
                v = v * 16 + (b[i] as char).to_digit(16).unwrap();
                i += 1;
            }
            (v as u8, i)
        }
        Some(&x) => (x, 1),
        None => return Err("bad escape".into()),
    })
}

fn lex_char(b: &[u8]) -> Result<(i64, usize), String> {
    // b[0] == '\''
    let mut i = 1;
    let val;
    if b.get(i) == Some(&b'\\') {
        let (v, n) = escape(&b[i + 1..])?;
        val = v as i64;
        i += 1 + n;
    } else {
        val = b[i] as i64;
        i += 1;
    }
    if b.get(i) != Some(&b'\'') {
        return Err("unterminated char literal".into());
    }
    Ok((val, i + 1))
}

fn lex_string(b: &[u8]) -> Result<(Vec<u8>, usize), String> {
    // b[0] == '"'
    let mut i = 1;
    let mut out = Vec::new();
    while i < b.len() && b[i] != b'"' {
        if b[i] == b'\\' {
            let (v, n) = escape(&b[i + 1..])?;
            out.push(v);
            i += 1 + n;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    if b.get(i) != Some(&b'"') {
        return Err("unterminated string literal".into());
    }
    out.push(0); // NUL terminator
    Ok((out, i + 1))
}
