//! jas — command-line JRISC assembler.
//!
//!   jas input.s -o out.bin [--dsp] [--org 0xADDR] [--no-hazard-check] [-Werror]
//!
//! Emits a flat binary at the origin. Diagnostics go to stderr with fix-its;
//! a nonzero exit means errors (nothing is written). The hazard pass is on by
//! default — that is the whole point.

use std::process::ExitCode;

use jas::{assemble, Level, Options, Target};

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.is_empty() || args.iter().any(|a| a == "-h" || a == "--help") {
        usage();
        return if args.is_empty() { ExitCode::FAILURE } else { ExitCode::SUCCESS };
    }

    let mut input = None;
    let mut output = None;
    let mut opts = Options::default();
    let mut org_set = false;

    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "-o" => output = it.next().cloned(),
            "--dsp" => {
                opts.target = Target::Dsp;
                if !org_set {
                    opts.org = 0xF1_B000; // DSP local RAM base
                }
            }
            "--gpu" => opts.target = Target::Gpu,
            "--68000" | "--m68k" => opts.start_m68k = true,
            "--org" => {
                if let Some(v) = it.next().and_then(|s| parse_num(s)) {
                    opts.org = v;
                    org_set = true;
                } else {
                    eprintln!("jas: --org needs a number");
                    return ExitCode::FAILURE;
                }
            }
            "-c" => opts.object_mode = true,   // emit a relocatable object (.jo)
            "--no-hazard-check" => opts.check_hazards = false,
            "-Werror" => opts.warnings_as_errors = true,
            "-I" => {
                if let Some(d) = it.next() {
                    opts.include_dirs.push(d.clone());
                }
            }
            s if s.starts_with('-') => {
                eprintln!("jas: unknown option `{s}`");
                return ExitCode::FAILURE;
            }
            s => input = Some(s.to_string()),
        }
    }

    let Some(input) = input else {
        eprintln!("jas: no input file");
        return ExitCode::FAILURE;
    };
    // default include search path: the input file's own directory
    if let Some(parent) = std::path::Path::new(&input).parent() {
        if !parent.as_os_str().is_empty() {
            opts.include_dirs.push(parent.to_string_lossy().into_owned());
        }
    }
    let src = match std::fs::read_to_string(&input) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("jas: cannot read {input}: {e}");
            return ExitCode::FAILURE;
        }
    };

    let result = assemble(&src, &opts);

    for d in &result.diags {
        eprintln!("{input}:{d}");
    }

    let errs = result.errors();
    let warns = result.warnings();
    if errs > 0 {
        eprintln!("jas: {errs} error(s), {warns} warning(s) — no output written");
        return ExitCode::FAILURE;
    }

    if let Some(out) = output {
        let data = if opts.object_mode {
            result.object(result.org).serialize()
        } else {
            result.bytes.clone()
        };
        if let Err(e) = std::fs::write(&out, &data) {
            eprintln!("jas: cannot write {out}: {e}");
            return ExitCode::FAILURE;
        }
        eprintln!(
            "jas: wrote {} ({} {} at 0x{:06X}, {} relocs), {warns} warning(s)",
            out,
            if opts.object_mode { data.len() } else { result.bytes.len() },
            if opts.object_mode { "object bytes" } else { "bytes" },
            result.org,
            result.relocs.len(),
        );
    } else {
        // no -o: just report (a syntax/hazard check run)
        let _ = Level::Warning;
        eprintln!(
            "jas: {} bytes assembled at 0x{:06X}, {warns} warning(s) (no -o, nothing written)",
            result.bytes.len(),
            result.org
        );
    }
    ExitCode::SUCCESS
}

fn parse_num(s: &str) -> Option<u32> {
    let s = s.trim();
    if let Some(h) = s.strip_prefix("0x").or_else(|| s.strip_prefix('$')) {
        u32::from_str_radix(h, 16).ok()
    } else {
        s.parse().ok()
    }
}

fn usage() {
    eprintln!(
        "jas — the JRISC assembler that refuses to assemble hazards\n\
         \n\
         USAGE:\n\
         \x20 jas <input.s> [-o out.bin] [options]\n\
         \n\
         OPTIONS:\n\
         \x20 -o <file>            write flat binary output\n\
         \x20 --gpu | --dsp       target core (default gpu; sets default org)\n\
         \x20 --68000            start in 68000 mode (pure-68k files w/o a .68000 directive)\n\
         \x20 --org <0xADDR>      origin address (default GPU $F03000 / DSP $F1B000)\n\
         \x20 --no-hazard-check   emit even known silicon hazards (not recommended)\n\
         \x20 -Werror             treat warnings as errors\n\
         \n\
         The hazard pass reports write-after-write into a load/divide shadow,\n\
         indexed stores of unsettled registers, JUMP/MOVEI in delay slots, and\n\
         out-of-range branches — as errors with fix-its, by default."
    );
}
