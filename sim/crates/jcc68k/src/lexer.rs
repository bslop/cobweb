//! C lexer. Turns source into a token stream for the parser. Handles the C
//! preprocessor's already-expanded output (no macro expansion here yet — that
//! lives in a preprocessing pass); comments and whitespace are skipped.

#[derive(Debug, Clone, PartialEq)]
pub enum Tok {
    // literals / identifiers
    Num(i64),
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
        // preprocessor line directive (`# 12 "file"`) or a stray `#...` — skip to EOL.
        // (Real macros are handled by the preprocessor before us; a `#` here is a
        // line marker from the preprocessor.)
        if c == b'#' && (col == 1 || out.last().map(|t: &Token| matches!(&t.tok, Tok::Eof)).unwrap_or(true)) {
            while i < b.len() && b[i] != b'\n' {
                bump(&mut i, &mut line, &mut col, 1);
            }
            continue;
        }
        let (sl, sc) = (line, col);
        // number (int; hex/oct/dec, with optional u/l suffixes)
        if c.is_ascii_digit() {
            let start = i;
            let (val, n) = lex_number(&b[i..])?;
            bump(&mut i, &mut line, &mut col, n);
            let _ = start;
            out.push(Token { tok: Tok::Num(val), line: sl, col: sc });
            continue;
        }
        // char literal
        if c == b'\'' {
            let (val, n) = lex_char(&b[i..])?;
            bump(&mut i, &mut line, &mut col, n);
            out.push(Token { tok: Tok::Char(val), line: sl, col: sc });
            continue;
        }
        // string literal
        if c == b'"' {
            let (bytes, n) = lex_string(&b[i..])?;
            bump(&mut i, &mut line, &mut col, n);
            out.push(Token { tok: Tok::Str(bytes), line: sl, col: sc });
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
            out.push(Token { tok, line: sl, col: sc });
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
                out.push(Token { tok: Tok::Punct(p.to_string()), line: sl, col: sc });
            }
            None => return Err(format!("{sl}:{sc}: stray '{}' in program", c as char)),
        }
    }
    out.push(Token { tok: Tok::Eof, line, col });
    Ok(out)
}

fn lex_number(b: &[u8]) -> Result<(i64, usize), String> {
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
    // consume integer suffixes u/U/l/L
    while i < b.len() && matches!(b[i], b'u' | b'U' | b'l' | b'L') {
        i += 1;
    }
    Ok((val, i))
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
