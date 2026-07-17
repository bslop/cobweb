//! jtest — command-line differential verification harness.
//!
//!   jtest run      <prog> [opts]            run once, print result + stats
//!   jtest diff     <A> <B> [opts]           shadow diff two programs (exit≠0 if differ)
//!   jtest profiles <prog> [opts]            silicon vs bigpemu divergence
//!   jtest golden   <prog> --golden g [opts] compare/update a golden capture
//!
//! A <prog> is a flat .bin, or JRISC source with --assemble (hazard-checked
//! through jas). Options: --dsp, --org N, --budget N, --capture ADDR:LEN,
//! --fidelity silicon|bigpemu|functional.

use std::process::ExitCode;

use jag_core::risc::Fidelity;
use jag_core::RiscKind;
use jtest::{compare, profile_diff, run, Spec};

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.is_empty() {
        usage();
        return ExitCode::FAILURE;
    }
    let cmd = args[0].as_str();
    let rest = &args[1..];
    match cmd {
        "run" => cmd_run(rest),
        "diff" => cmd_diff(rest),
        "profiles" => cmd_profiles(rest),
        "golden" => cmd_golden(rest),
        "-h" | "--help" | "help" => {
            usage();
            ExitCode::SUCCESS
        }
        _ => {
            eprintln!("jtest: unknown command `{cmd}`");
            usage();
            ExitCode::FAILURE
        }
    }
}

struct Common {
    target: RiscKind,
    org: Option<u32>,
    budget: u32,
    capture: (u32, u32),
    fidelity: Fidelity,
    assemble: bool,
}

fn parse_common(args: &[String]) -> (Vec<String>, Common) {
    let mut positional = Vec::new();
    let mut c = Common {
        target: RiscKind::Gpu,
        org: None,
        budget: 100_000,
        capture: (0x0010_0000, 256),
        fidelity: Fidelity::Silicon,
        assemble: false,
    };
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--dsp" => c.target = RiscKind::Dsp,
            "--assemble" | "-S" => c.assemble = true,
            "--org" => c.org = it.next().and_then(|s| parse_num(s)),
            "--budget" => {
                if let Some(v) = it.next().and_then(|s| parse_num(s)) {
                    c.budget = v;
                }
            }
            "--capture" => {
                if let Some(s) = it.next() {
                    if let Some((a, l)) = s.split_once(':') {
                        if let (Some(a), Some(l)) = (parse_num(a), parse_num(l)) {
                            c.capture = (a, l);
                        }
                    }
                }
            }
            "--fidelity" => {
                c.fidelity = match it.next().map(|s| s.as_str()) {
                    Some("bigpemu") => Fidelity::BigPEmu,
                    Some("functional") => Fidelity::Functional,
                    _ => Fidelity::Silicon,
                }
            }
            s => positional.push(s.to_string()),
        }
    }
    (positional, c)
}

fn load(path: &str, c: &Common) -> Result<Spec, String> {
    let default_org = match c.target {
        RiscKind::Gpu => 0xF0_3000,
        RiscKind::Dsp => 0xF1_B000,
    };
    let org = c.org.unwrap_or(default_org);
    let bytes = if c.assemble {
        let src = std::fs::read_to_string(path).map_err(|e| format!("reading {path}: {e}"))?;
        match jtest::assemble(&src, c.target) {
            Ok((b, _)) => b,
            Err(diags) => {
                for d in &diags {
                    eprintln!("{path}:{d}");
                }
                return Err(format!("{} assembler error(s)", diags.len()));
            }
        }
    } else {
        std::fs::read(path).map_err(|e| format!("reading {path}: {e}"))?
    };
    Ok(Spec { bytes, target: c.target, org, budget: c.budget, capture: c.capture, fidelity: c.fidelity })
}

fn cmd_run(args: &[String]) -> ExitCode {
    let (pos, c) = parse_common(args);
    let Some(p) = pos.first() else {
        eprintln!("jtest run: need a program");
        return ExitCode::FAILURE;
    };
    let spec = match load(p, &c) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("jtest: {e}");
            return ExitCode::FAILURE;
        }
    };
    let r = run(&spec);
    let t = &r.timing;
    println!(
        "run {p}\n  cycles={} instret={} running={}\n  stalls: alu={} load={} div={} flags={} jump_refill={} contention={}\n  ext: fetch={} mem={}  hazards: waw={} idx_store_stale={} slot_movei={} slot_jump={} bigpemu_div={}",
        r.cycles, r.instret, r.running,
        t.stall_alu, t.stall_load, t.stall_div, t.stall_flags, t.jump_refill, t.contention,
        t.fetch_external, t.mem_external, t.waw_hazards, t.indexed_store_stale,
        t.slot_movei, t.slot_jump, t.bigpemu_divergence,
    );
    ExitCode::SUCCESS
}

fn cmd_diff(args: &[String]) -> ExitCode {
    let (pos, c) = parse_common(args);
    if pos.len() < 2 {
        eprintln!("jtest diff: need two programs");
        return ExitCode::FAILURE;
    }
    let (a, b) = match (load(&pos[0], &c), load(&pos[1], &c)) {
        (Ok(a), Ok(b)) => (a, b),
        _ => return ExitCode::FAILURE,
    };
    let d = compare(&run(&a), &run(&b));
    if d.is_empty() {
        println!("PASS: {} and {} produce identical capture + registers", pos[0], pos[1]);
        ExitCode::SUCCESS
    } else {
        if let Some((off, x, y)) = d.mem {
            println!("DIFF: capture[+0x{off:X}] {x:#04X} != {y:#04X}");
        }
        for (i, x, y) in &d.regs {
            println!("DIFF: r{i} {x:#010X} != {y:#010X}");
        }
        ExitCode::FAILURE
    }
}

fn cmd_profiles(args: &[String]) -> ExitCode {
    let (pos, c) = parse_common(args);
    let Some(p) = pos.first() else {
        eprintln!("jtest profiles: need a program");
        return ExitCode::FAILURE;
    };
    let spec = match load(p, &c) {
        Ok(s) => s,
        Err(_) => return ExitCode::FAILURE,
    };
    let (sil, big, d) = profile_diff(&spec);
    println!(
        "silicon: {} cycles   bigpemu: {} cycles   bigpemu_divergence sites: {}",
        sil.cycles, big.cycles, big.timing.bigpemu_divergence
    );
    if d.is_empty() && big.timing.bigpemu_divergence == 0 {
        println!("PASS: no silicon/bigpemu divergence — safe to trust the emulator here");
        ExitCode::SUCCESS
    } else {
        if let Some((off, x, y)) = d.mem {
            println!("DIVERGENT: capture[+0x{off:X}] silicon={x:#04X} bigpemu={y:#04X}");
        }
        if big.timing.bigpemu_divergence > 0 {
            println!(
                "WARNING: {} site(s) where BigPEmu fails to scoreboard an external load across a jump — hardware-correct code may read stale data under BigPEmu",
                big.timing.bigpemu_divergence
            );
        }
        ExitCode::FAILURE
    }
}

fn cmd_golden(args: &[String]) -> ExitCode {
    let (mut pos, c) = parse_common(args);
    // --golden <file> and --update are pulled from a re-scan.
    let mut golden_path = None;
    let mut update = false;
    let mut i = 0;
    // parse_common already consumed unknown flags into positional; recover them:
    pos.retain(|s| {
        if s == "--update" {
            update = true;
            false
        } else {
            true
        }
    });
    while i < pos.len() {
        if pos[i] == "--golden" && i + 1 < pos.len() {
            golden_path = Some(pos[i + 1].clone());
            pos.drain(i..i + 2);
        } else {
            i += 1;
        }
    }
    let Some(prog) = pos.first() else {
        eprintln!("jtest golden: need a program");
        return ExitCode::FAILURE;
    };
    let Some(golden) = golden_path else {
        eprintln!("jtest golden: --golden <file> required");
        return ExitCode::FAILURE;
    };
    let spec = match load(prog, &c) {
        Ok(s) => s,
        Err(_) => return ExitCode::FAILURE,
    };
    let cap = run(&spec).captured;
    if update {
        if let Err(e) = std::fs::write(&golden, &cap) {
            eprintln!("jtest: writing golden {golden}: {e}");
            return ExitCode::FAILURE;
        }
        println!("updated golden {golden} ({} bytes)", cap.len());
        return ExitCode::SUCCESS;
    }
    match std::fs::read(&golden) {
        Ok(g) if g == cap => {
            println!("PASS: {prog} matches golden {golden}");
            ExitCode::SUCCESS
        }
        Ok(g) => {
            let off = g.iter().zip(&cap).position(|(a, b)| a != b);
            println!("FAIL: {prog} diverges from golden {golden} at offset {off:?}");
            ExitCode::FAILURE
        }
        Err(_) => {
            eprintln!("jtest: golden {golden} not found (run with --update to create it)");
            ExitCode::FAILURE
        }
    }
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
        "jtest — differential verification harness for JRISC code\n\
         \n\
         COMMANDS:\n\
         \x20 jtest run      <prog> [opts]              run once; print result + stall/hazard stats\n\
         \x20 jtest diff     <A> <B> [opts]             shadow diff (exit≠0 if capture/regs differ)\n\
         \x20 jtest profiles <prog> [opts]              silicon vs bigpemu divergence check\n\
         \x20 jtest golden   <prog> --golden <f> [opts] compare a captured region to a golden (--update to write)\n\
         \n\
         OPTIONS:\n\
         \x20 --assemble        <prog> is JRISC source, assembled through jas (hazard-checked)\n\
         \x20 --dsp             target the DSP (default GPU)\n\
         \x20 --org <0xADDR>    load origin\n\
         \x20 --budget <N>      RISC-tick budget (default 100000)\n\
         \x20 --capture A:L     DRAM region to observe (default 0x100000:256)\n\
         \x20 --fidelity <F>    silicon | bigpemu | functional (default silicon)"
    );
}
