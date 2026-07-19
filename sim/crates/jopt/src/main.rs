//! jopt — command-line JRISC scheduler with a jsim equivalence certificate.
//!
//!   jopt input.s [-o out.s] [--dsp]
//!
//! Reports every transform and its verdict (accepted transforms are proven
//! equivalent in jsim; rejected ones tell you why). Writes the optimized source
//! only if `-o` is given.

use std::path::Path;
use std::process::ExitCode;

use jag_core::RiscKind;
use jopt::Fixture;

/// Parse an integer literal: decimal, `0x…`, or `$…` hex.
fn parse_int(s: &str) -> Option<u32> {
    let s = s.trim();
    if let Some(h) = s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")) {
        u32::from_str_radix(h, 16).ok()
    } else if let Some(h) = s.strip_prefix('$') {
        u32::from_str_radix(h, 16).ok()
    } else {
        s.parse().ok()
    }
}

/// Parse a fixture file. Directives, one per line (`#` comments, blanks ignored):
///
/// ```text
/// budget  <ticks>            RISC-tick budget (dec/hex)
/// capture <addr> <len>       observable region (the framebuffer, usually)
/// long    <addr> <value>     a 4-byte big-endian preset (param block, pointers)
/// blob    <addr> <file>      load a binary blob at addr (path relative to fixture)
/// ```
fn parse_fixture(path: &str) -> Result<Fixture, String> {
    let text = std::fs::read_to_string(path).map_err(|e| format!("cannot read {path}: {e}"))?;
    let dir = Path::new(path).parent().unwrap_or_else(|| Path::new("."));
    let mut fx = Fixture { pre: Vec::new(), budget: 20_000_000, capture: (0, 0) };
    let mut saw_capture = false;
    for (n, raw) in text.lines().enumerate() {
        let line = raw.split('#').next().unwrap_or("").trim();
        if line.is_empty() {
            continue;
        }
        let tok: Vec<&str> = line.split_whitespace().collect();
        let err = |m: &str| format!("{path}:{}: {m}", n + 1);
        match tok.as_slice() {
            ["budget", v] => fx.budget = parse_int(v).ok_or_else(|| err("bad budget"))?,
            ["capture", a, l] => {
                fx.capture = (
                    parse_int(a).ok_or_else(|| err("bad capture addr"))?,
                    parse_int(l).ok_or_else(|| err("bad capture len"))?,
                );
                saw_capture = true;
            }
            ["long", a, v] => {
                let addr = parse_int(a).ok_or_else(|| err("bad long addr"))?;
                let val = parse_int(v).ok_or_else(|| err("bad long value"))?;
                fx.pre.push((addr, val.to_be_bytes().to_vec()));
            }
            ["blob", a, file] => {
                let addr = parse_int(a).ok_or_else(|| err("bad blob addr"))?;
                let p = dir.join(file);
                let bytes = std::fs::read(&p)
                    .map_err(|e| err(&format!("cannot read blob {}: {e}", p.display())))?;
                fx.pre.push((addr, bytes));
            }
            _ => return Err(err("unknown directive")),
        }
    }
    if !saw_capture {
        return Err(format!("{path}: a `capture <addr> <len>` line is required"));
    }
    Ok(fx)
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.is_empty() || args.iter().any(|a| a == "-h" || a == "--help") {
        eprintln!(
            "jopt — JRISC scheduler; every transform is proven equivalent in jsim\n\
             \n\
             USAGE: jopt <input.s> [-o out.s] [--gpu|--dsp] [--allow-input-hazards]\n\
             \n\
             transform: delay-slot filling. For each wasted `nop` after a jump, jopt\n\
             walks the straight-line block backwards for a donor it can legally sink\n\
             into the slot (dominated by the jump, data-independent of everything it\n\
             leapfrogs, flag-safe), then proves the result equivalent in jsim.\n\
             \n\
             OPTIONS:\n\
             \x20 -o <file>              write the optimized source\n\
             \x20 --gpu | --dsp          target core (default gpu)\n\
             \x20 --allow-input-hazards  optimize past pre-existing (benign) input\n\
             \x20                        hazards; the jsim certificate still gates output\n\
             \x20 --fixture <file>       certify against a fixture (kernel input state)\n\
             \x20                        so a kernel that never halts in isolation runs\n\
             \x20                        to a real, observable end; captures its output\n\
             \n\
             Fixture file directives: `budget <ticks>`, `capture <addr> <len>`,\n\
             `long <addr> <value>`, `blob <addr> <file>` (# comments).\n\
             \n\
             Reasons over the assembled stream, so instructions in inactive `.if`\n\
             blocks are never touched (reported as `skipped-inactive`)."
        );
        return if args.is_empty() { ExitCode::FAILURE } else { ExitCode::SUCCESS };
    }

    let mut input = None;
    let mut output = None;
    let mut target = RiscKind::Gpu;
    let mut allow_input_hazards = false;
    let mut fixture_path = None;
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "-o" => output = it.next().cloned(),
            "--dsp" => target = RiscKind::Dsp,
            "--gpu" => target = RiscKind::Gpu,
            // optimize past pre-existing (benign) input hazards; the jsim
            // equivalence certificate still guarantees the output is safe.
            "--allow-input-hazards" | "--no-input-hazard-check" => allow_input_hazards = true,
            // certify against a fixture (kernel input state) so a kernel that
            // never halts in isolation runs to a real, observable end.
            "--fixture" => fixture_path = it.next().cloned(),
            s if s.starts_with('-') => {
                eprintln!("jopt: unknown option `{s}`");
                return ExitCode::FAILURE;
            }
            s => input = Some(s.to_string()),
        }
    }

    let fixture = match &fixture_path {
        Some(p) => match parse_fixture(p) {
            Ok(f) => Some(f),
            Err(e) => {
                eprintln!("jopt: {e}");
                return ExitCode::FAILURE;
            }
        },
        None => None,
    };

    let Some(input) = input else {
        eprintln!("jopt: no input file");
        return ExitCode::FAILURE;
    };
    let src = match std::fs::read_to_string(&input) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("jopt: cannot read {input}: {e}");
            return ExitCode::FAILURE;
        }
    };

    let res = jopt::optimize_fixture(&src, target, allow_input_hazards, fixture.as_ref());

    for t in &res.transforms {
        let mark = if t.accepted { "✓ accepted" } else { "· rejected" };
        eprintln!("  {mark}  {}:{}  {}", t.kind, t.at_line, t.reason);
    }
    let skipped = res.transforms.iter().filter(|t| t.kind == "skipped-inactive").count();
    eprintln!(
        "jopt: {} transform(s) accepted, {} bytes -> {} bytes ({} saved)",
        res.accepted(),
        res.bytes_before,
        res.bytes_after,
        res.bytes_saved()
    );
    if skipped > 0 {
        eprintln!(
            "jopt: {skipped} wasted slot(s) skipped — inside inactive `.if` blocks (not assembled)"
        );
    }

    if let Some(out) = output {
        if let Err(e) = std::fs::write(&out, &res.source) {
            eprintln!("jopt: cannot write {out}: {e}");
            return ExitCode::FAILURE;
        }
        eprintln!("jopt: wrote {out}");
    }
    ExitCode::SUCCESS
}
