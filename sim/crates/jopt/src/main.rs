//! jopt — command-line JRISC scheduler with a jsim equivalence certificate.
//!
//!   jopt input.s [-o out.s] [--dsp]
//!
//! Reports every transform and its verdict (accepted transforms are proven
//! equivalent in jsim; rejected ones tell you why). Writes the optimized source
//! only if `-o` is given.

use std::process::ExitCode;

use jag_core::RiscKind;

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
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "-o" => output = it.next().cloned(),
            "--dsp" => target = RiscKind::Dsp,
            "--gpu" => target = RiscKind::Gpu,
            // optimize past pre-existing (benign) input hazards; the jsim
            // equivalence certificate still guarantees the output is safe.
            "--allow-input-hazards" | "--no-input-hazard-check" => allow_input_hazards = true,
            s if s.starts_with('-') => {
                eprintln!("jopt: unknown option `{s}`");
                return ExitCode::FAILURE;
            }
            s => input = Some(s.to_string()),
        }
    }

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

    let res = jopt::optimize_opts(&src, target, allow_input_hazards);

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
