//! GNU-`as` (GAS) 68k syntax → jas-native normalizer.
//!
//! The hand-written 68000 modules in real Jaguar C ports (startup, ISRs, BIOS
//! glue, data blobs) are written for `m68k-elf-as`, not rmac. Rather than teach
//! the validated jas core two dialects, this pass rewrites GAS source into the
//! Motorola/rmac spelling jas already parses — line-for-line, so diagnostics
//! keep pointing at the right source line. It handles the lexical differences:
//!
//!   * `/* … */` block comments and `|` line comments
//!   * `%`-prefixed registers (`%d0`, `%a6`, `%sp`→a7, `%fp`→a6, `%pc`, `%sr`)
//!   * numeric local labels (`1:` with `1f`/`1b` forward/backward references)
//!   * `.balign`/`.section` directive spellings (mapped or left for the core)
//!
//! Number bases (`0x`, `0b`), `.equ NAME, val`, `.incbin`, `.globl`/`.extern`,
//! `.long`/`.word`/`.byte`, and `.macro`/`.include` are understood by the core
//! directly, so they pass through unchanged.

/// Rewrite a GAS 68k source file into jas-native syntax. Line count is
/// preserved (each input line maps to exactly one output line). This is the
/// full pass; the assembler splits it around macro expansion (see
/// [`normalize_lexical`] and [`resolve_numeric`]).
pub fn normalize(src: &str) -> String {
    resolve_numeric(&normalize_lexical(src))
}

/// The lexical half: strip comments and de-prefix `%` registers. Safe to run
/// before macro/include expansion (macro bodies use `%a6`, `|` comments, etc.).
pub fn normalize_lexical(src: &str) -> String {
    let decommented = strip_comments(src);
    decommented
        .lines()
        .map(deprefix_registers)
        .collect::<Vec<_>>()
        .join("\n")
}

/// The numeric-label half: resolve `1:`/`1f`/`1b`. Must run *after* macro
/// expansion so each expansion's `9:` becomes a distinct label rather than a
/// duplicate of the single macro-body definition.
pub fn resolve_numeric(src: &str) -> String {
    let mut lines: Vec<String> = src.lines().map(str::to_string).collect();
    resolve_numeric_labels(&mut lines);
    lines.join("\n")
}

/// Heuristic: does this source look like GAS 68k rather than rmac? Any of the
/// GAS-only lexical tells is enough to switch dialects automatically.
pub fn looks_like_gas(src: &str) -> bool {
    src.contains("%d0")
        || src.contains("%a0")
        || src.contains("%sp")
        || src.contains("%pc")
        || src.contains(".balign")
        || src.contains("/*")
}

/// Remove `/* … */` block comments and `|` line comments, preserving newlines
/// and the contents of string/char literals.
fn strip_comments(src: &str) -> String {
    let b: Vec<char> = src.chars().collect();
    let mut out = String::with_capacity(src.len());
    let mut i = 0;
    let mut in_block = false;
    let mut in_str = false;
    let mut in_ch = false;
    let mut prev = '\0';
    while i < b.len() {
        let c = b[i];
        if in_block {
            if c == '*' && b.get(i + 1) == Some(&'/') {
                in_block = false;
                i += 2;
                out.push(' ');
                out.push(' ');
                continue;
            }
            // keep newlines so line numbers survive
            out.push(if c == '\n' { '\n' } else { ' ' });
            i += 1;
            continue;
        }
        if in_str {
            out.push(c);
            if c == '"' && prev != '\\' {
                in_str = false;
            }
            prev = if c == '\\' && prev == '\\' { '\0' } else { c };
            i += 1;
            continue;
        }
        if in_ch {
            out.push(c);
            if c == '\'' && prev != '\\' {
                in_ch = false;
            }
            prev = if c == '\\' && prev == '\\' { '\0' } else { c };
            i += 1;
            continue;
        }
        // not in any comment/literal
        if c == '/' && b.get(i + 1) == Some(&'*') {
            in_block = true;
            i += 2;
            out.push(' ');
            out.push(' ');
            continue;
        }
        if c == '|' {
            // line comment to EOL
            while i < b.len() && b[i] != '\n' {
                i += 1;
            }
            continue;
        }
        if c == '"' {
            in_str = true;
            out.push(c);
            prev = c;
            i += 1;
            continue;
        }
        if c == '\'' {
            in_ch = true;
            out.push(c);
            prev = c;
            i += 1;
            continue;
        }
        out.push(c);
        prev = c;
        i += 1;
    }
    out
}

/// Rewrite GAS numeric local labels: each `N:` definition becomes a unique
/// scoped label `.gasLN_<line>`, and each `Nf`/`Nb` reference resolves to the
/// nearest following / preceding definition of `N`.
fn resolve_numeric_labels(lines: &mut [String]) {
    // Collect definitions in source order: (line index, number).
    let mut defs: Vec<(usize, u32)> = Vec::new();
    for (i, line) in lines.iter().enumerate() {
        if let Some(n) = leading_numeric_label(line) {
            defs.push((i, n));
        }
    }
    if defs.is_empty() {
        return;
    }
    // Reserved non-dot prefix: GAS numeric locals aren't bound to the nearest
    // global label (a `foo:` between a `3f` and its `3:` must not break the ref),
    // so they can't be jas `.`-locals. jas special-cases this prefix to neither
    // scope-qualify it nor let it reset the local-label scope.
    let uniq = |n: u32, li: usize| format!("L__gasnum_{n}_{li}");

    for (i, line) in lines.iter_mut().enumerate() {
        // Replace a leading `N:` definition.
        if let Some(n) = leading_numeric_label(line) {
            let rest = line.trim_start();
            let after = &rest[rest.find(':').unwrap() + 1..];
            let indent = &line[..line.len() - rest.len()];
            *line = format!("{indent}{}:{after}", uniq(n, i));
        }
        // Replace `Nf` / `Nb` references anywhere in the (possibly rewritten) line.
        *line = replace_numeric_refs(line, i, &defs, &uniq);
    }
}

/// If `line` begins (after whitespace) with `<digits>:`, return the number.
fn leading_numeric_label(line: &str) -> Option<u32> {
    let t = line.trim_start();
    let digits: String = t.chars().take_while(|c| c.is_ascii_digit()).collect();
    if digits.is_empty() {
        return None;
    }
    if t[digits.len()..].starts_with(':') {
        digits.parse().ok()
    } else {
        None
    }
}

/// Replace `Nf`/`Nb` tokens with the resolved unique label. A token qualifies
/// only when the `f`/`b` is not followed by another identifier char (so `0b101`
/// binary and identifiers like `a1foo` are left alone).
fn replace_numeric_refs(
    line: &str,
    line_idx: usize,
    defs: &[(usize, u32)],
    uniq: &impl Fn(u32, usize) -> String,
) -> String {
    let b: Vec<char> = line.chars().collect();
    let mut out = String::with_capacity(line.len());
    let mut i = 0;
    while i < b.len() {
        // A numeric ref must not be preceded by an identifier char (else it's a
        // suffix, e.g. inside a hex/symbol) — require a boundary.
        let boundary = i == 0 || !is_ident_char(b[i - 1]);
        if boundary && b[i].is_ascii_digit() {
            let mut j = i;
            while j < b.len() && b[j].is_ascii_digit() {
                j += 1;
            }
            let dir = b.get(j).copied();
            let after_dir = b.get(j + 1).copied();
            let is_ref = matches!(dir, Some('f') | Some('b'))
                && after_dir.map(|c| !is_ident_char(c)).unwrap_or(true);
            if is_ref {
                let n: u32 = b[i..j].iter().collect::<String>().parse().unwrap();
                let forward = dir == Some('f');
                if let Some(target) = resolve_ref(n, line_idx, forward, defs) {
                    out.push_str(&uniq(n, target));
                    i = j + 1;
                    continue;
                }
            }
            // not a ref — copy the digits verbatim
            out.extend(&b[i..j]);
            i = j;
            continue;
        }
        out.push(b[i]);
        i += 1;
    }
    out
}

/// Find the defining line index for `Nf`/`Nb` from `from`: the nearest def of
/// `n` at/after `from` (forward) or at/before `from` (backward).
fn resolve_ref(n: u32, from: usize, forward: bool, defs: &[(usize, u32)]) -> Option<usize> {
    if forward {
        defs.iter().filter(|(li, d)| *d == n && *li >= from).map(|(li, _)| *li).min()
    } else {
        defs.iter().filter(|(li, d)| *d == n && *li <= from).map(|(li, _)| *li).max()
    }
}

/// Strip the `%` register prefix, mapping `%sp`→a7 and `%fp`→a6. A register name
/// is only rewritten when it stands as a full token after the `%`.
fn deprefix_registers(line: &str) -> String {
    let b: Vec<char> = line.chars().collect();
    let mut out = String::with_capacity(line.len());
    let mut i = 0;
    let mut in_str = false;
    let mut in_ch = false;
    while i < b.len() {
        let c = b[i];
        if in_str {
            out.push(c);
            if c == '"' {
                in_str = false;
            }
            i += 1;
            continue;
        }
        if in_ch {
            out.push(c);
            if c == '\'' {
                in_ch = false;
            }
            i += 1;
            continue;
        }
        if c == '"' {
            in_str = true;
            out.push(c);
            i += 1;
            continue;
        }
        if c == '\'' {
            in_ch = true;
            out.push(c);
            i += 1;
            continue;
        }
        if c == '%' {
            // read the register word following '%' (alphanumeric only — a `.w`/
            // `.l` index-size suffix like `%d0.l` must stay outside the name)
            let mut j = i + 1;
            while j < b.len() && b[j].is_ascii_alphanumeric() {
                j += 1;
            }
            let word: String = b[i + 1..j].iter().collect();
            if let Some(mapped) = map_register(&word) {
                out.push_str(mapped);
                i = j;
                continue;
            }
        }
        out.push(c);
        i += 1;
    }
    out
}

/// Map a GAS register name (sans `%`) to jas's spelling, or None if it isn't a
/// register (leave the `%` alone — it may be a binary literal in mixed source).
fn map_register(word: &str) -> Option<&'static str> {
    Some(match word {
        "d0" => "d0", "d1" => "d1", "d2" => "d2", "d3" => "d3",
        "d4" => "d4", "d5" => "d5", "d6" => "d6", "d7" => "d7",
        "a0" => "a0", "a1" => "a1", "a2" => "a2", "a3" => "a3",
        "a4" => "a4", "a5" => "a5", "a6" => "a6", "a7" => "a7",
        "sp" => "a7", "fp" => "a6",
        "pc" => "pc", "sr" => "sr", "ccr" => "ccr", "usp" => "usp",
        _ => return None,
    })
}

fn is_ident_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_' || c == '.' || c == '$'
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_comments_and_registers() {
        let s = "move.l %d0,%sp | tail\n/* block\n spanning */ nop\n";
        let n = normalize(s);
        assert!(n.contains("move.l d0,a7"));
        assert!(!n.contains('|'));
        assert!(!n.contains("/*"));
        assert!(n.contains("nop"));
        // line count preserved
        assert_eq!(n.lines().count(), s.lines().count());
    }

    #[test]
    fn numeric_locals() {
        let s = "1:\n  dbra %d0,1b\n  bra 2f\n2:\n  rts\n";
        let n = normalize(s);
        assert!(n.contains("L__gasnum_1_0:"));
        assert!(n.contains("dbra d0,L__gasnum_1_0"));
        assert!(n.contains("bra L__gasnum_2_3"));
        assert!(n.contains("L__gasnum_2_3:"));
    }

    #[test]
    fn leaves_binary_literals_alone() {
        let s = "move.l #0b1010,%d0\n";
        let n = normalize(s);
        assert!(n.contains("#0b1010"));
        assert!(n.contains("d0"));
    }
}
