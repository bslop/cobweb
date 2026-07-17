//! jdbg — command-line source-level JRISC debugger (emulator backend).
//!
//!   jdbg <prog.s> [--dsp] [--break LINE]... [--trace N] [--max N]
//!
//! Without --trace: runs to the first breakpoint / halt / crash and prints a
//! source-level report. With --trace N: single-steps N instructions, printing
//! the source line at each step. Deterministic — same program, same trace.

use std::process::ExitCode;

use jag_core::RiscKind;
use jdbg::{Session, Stop};

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.is_empty() || args.iter().any(|a| a == "-h" || a == "--help") {
        eprintln!(
            "jdbg — source-level JRISC debugger (over jsim)\n\
             \n\
             USAGE: jdbg <prog.s> [--dsp] [--break LINE].. [--trace N] [--max N]\n\
             \n\
             Runs to the first breakpoint / halt / wild-jump and prints a report\n\
             that names the SOURCE LINE and live registers. --trace N single-steps\n\
             N instructions. Deterministic."
        );
        return if args.is_empty() { ExitCode::FAILURE } else { ExitCode::SUCCESS };
    }

    let mut input = None;
    let mut target = RiscKind::Gpu;
    let mut breaks = Vec::new();
    let mut trace = None;
    let mut max = 1_000_000u64;
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--dsp" => target = RiscKind::Dsp,
            "--break" | "-b" => {
                if let Some(l) = it.next().and_then(|s| s.parse().ok()) {
                    breaks.push(l);
                }
            }
            "--trace" => trace = it.next().and_then(|s| s.parse().ok()),
            "--max" => {
                if let Some(v) = it.next().and_then(|s| s.parse().ok()) {
                    max = v;
                }
            }
            s if s.starts_with('-') => {
                eprintln!("jdbg: unknown option `{s}`");
                return ExitCode::FAILURE;
            }
            s => input = Some(s.to_string()),
        }
    }

    let Some(input) = input else {
        eprintln!("jdbg: no input file");
        return ExitCode::FAILURE;
    };
    let src = match std::fs::read_to_string(&input) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("jdbg: cannot read {input}: {e}");
            return ExitCode::FAILURE;
        }
    };

    let mut sess = match Session::load(&src, target) {
        Ok(s) => s,
        Err(diags) => {
            for d in &diags {
                eprintln!("{input}:{d}");
            }
            return ExitCode::FAILURE;
        }
    };

    if let Some(n) = trace {
        for _ in 0..n {
            let line = sess.pc_line();
            let text = line.and_then(|l| sess.source_line(l)).unwrap_or("").trim().to_string();
            println!("${:06X}  line {:<4}  {}", sess.pc(), line.unwrap_or(0), text);
            if !sess.step() {
                println!("(halted after {} steps)", sess.steps());
                break;
            }
        }
        return ExitCode::SUCCESS;
    }

    let stop = sess.run(&breaks, max);
    print!("{}", sess.report(&stop));
    match stop {
        Stop::Escaped(_) | Stop::Budget => ExitCode::FAILURE,
        _ => ExitCode::SUCCESS,
    }
}
