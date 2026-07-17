//! jprof — command-line JRISC profiler.
//!
//!   jprof <prog> [--assemble] [--dsp] [--budget N]
//!   jprof diff <A> <B> [--assemble] ...
//!
//! Prints the cycle breakdown by cause, the IPC, and a plain-language diagnosis
//! of the bottleneck. `diff` shows the per-cause delta between two builds — the
//! way to prove an optimization moved the number that matters.

use std::process::ExitCode;

use jag_core::RiscKind;
use jprof::{assemble, profile, Profile};

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.is_empty() || args.iter().any(|a| a == "-h" || a == "--help") {
        usage();
        return if args.is_empty() { ExitCode::FAILURE } else { ExitCode::SUCCESS };
    }

    let diff_mode = args[0] == "diff";
    let rest = if diff_mode { &args[1..] } else { &args[..] };

    let mut assemble_src = false;
    let mut target = RiscKind::Gpu;
    let mut budget = 500_000u32;
    let mut files = Vec::new();
    let mut it = rest.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--assemble" | "-S" => assemble_src = true,
            "--dsp" => target = RiscKind::Dsp,
            "--budget" => {
                if let Some(v) = it.next().and_then(|s| s.parse().ok()) {
                    budget = v;
                }
            }
            s if s.starts_with('-') => {
                eprintln!("jprof: unknown option `{s}`");
                return ExitCode::FAILURE;
            }
            s => files.push(s.to_string()),
        }
    }

    let load = |path: &str| -> Option<Profile> {
        let bytes_org = if assemble_src {
            let src = std::fs::read_to_string(path).ok()?;
            match assemble(&src, target) {
                Ok(x) => x,
                Err(ds) => {
                    for d in ds {
                        eprintln!("{path}:{d}");
                    }
                    return None;
                }
            }
        } else {
            let b = std::fs::read(path).ok()?;
            let org = match target {
                RiscKind::Gpu => 0xF0_3000,
                RiscKind::Dsp => 0xF1_B000,
            };
            (b, org)
        };
        Some(profile(bytes_org.0, bytes_org.1, target, budget))
    };

    if diff_mode {
        if files.len() < 2 {
            eprintln!("jprof diff: need two programs");
            return ExitCode::FAILURE;
        }
        let (Some(a), Some(b)) = (load(&files[0]), load(&files[1])) else {
            return ExitCode::FAILURE;
        };
        print_diff(&files[0], &a, &files[1], &b);
    } else {
        let Some(p) = files.first().and_then(|f| load(f)) else {
            return ExitCode::FAILURE;
        };
        print_profile(&files[0], &p);
    }
    ExitCode::SUCCESS
}

fn print_profile(name: &str, p: &Profile) {
    println!("profile: {name}");
    println!("  cycles {}   instret {}   IPC {:.3}", p.cycles, p.instret, p.ipc());
    println!("  cost breakdown:");
    for b in p.buckets() {
        if b.cycles == 0 {
            continue;
        }
        let pct = 100.0 * b.cycles as f64 / p.cycles.max(1) as f64;
        println!("    {:<32} {:>10}  {:5.1}%", b.name, b.cycles, pct);
    }
    println!("  bottleneck: {}", p.bottleneck());
}

fn print_diff(na: &str, a: &Profile, nb: &str, b: &Profile) {
    println!("diff: {na} -> {nb}");
    println!("  cycles {} -> {} ({:+})", a.cycles, b.cycles, b.cycles as i64 - a.cycles as i64);
    println!("  IPC {:.3} -> {:.3}", a.ipc(), b.ipc());
    let ba = a.buckets();
    let bb = b.buckets();
    for (x, y) in ba.iter().zip(&bb) {
        let d = y.cycles as i64 - x.cycles as i64;
        if d != 0 {
            println!("    {:<32} {:>10} -> {:<10} ({:+})", x.name, x.cycles, y.cycles, d);
        }
    }
}

fn usage() {
    eprintln!(
        "jprof — where did the cycles go?\n\
         \n\
         USAGE:\n\
         \x20 jprof <prog> [--assemble] [--dsp] [--budget N]     profile one build\n\
         \x20 jprof diff <A> <B> [--assemble] ...                per-cause delta between two builds\n\
         \n\
         Reports cycles by cause (issue / stalls / bus), IPC, and a plain-language\n\
         diagnosis. --assemble treats inputs as JRISC source (assembled via jas)."
    );
}
