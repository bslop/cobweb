//! jcc — command-line compiler for the restricted JRISC systems language.
//!
//!   jcc input.jc [-o out.s]
//!
//! Emits auditable JRISC assembly (feed to jas) and prints the SRAM budget
//! ledger. The output is guaranteed hazard-clean — jcc re-checks its own
//! output through jas before returning.

use std::process::ExitCode;

use jcc::compile;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.is_empty() || args.iter().any(|a| a == "-h" || a == "--help") {
        eprintln!(
            "jcc — restricted systems language -> auditable JRISC\n\
             \n\
             USAGE: jcc <input.jc> [-o out.s]\n\
             \n\
             Emits hazard-clean JRISC assembly (assemble with jas) and reports\n\
             the SRAM budget. Language: int vars, + - * << >> & | ^, if/else,\n\
             while, store <val>,<addr>. See the crate docs for the grammar."
        );
        return if args.is_empty() { ExitCode::FAILURE } else { ExitCode::SUCCESS };
    }

    let mut input = None;
    let mut output = None;
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "-o" => output = it.next().cloned(),
            s if s.starts_with('-') => {
                eprintln!("jcc: unknown option `{s}`");
                return ExitCode::FAILURE;
            }
            s => input = Some(s.to_string()),
        }
    }

    let Some(input) = input else {
        eprintln!("jcc: no input file");
        return ExitCode::FAILURE;
    };
    let src = match std::fs::read_to_string(&input) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("jcc: cannot read {input}: {e}");
            return ExitCode::FAILURE;
        }
    };

    let compiled = match compile(&src) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("jcc: {e}");
            return ExitCode::FAILURE;
        }
    };

    eprintln!("jcc: {}", compiled.ledger());
    for (name, reg) in &compiled.allocation {
        eprintln!("     {name} -> r{reg}");
    }
    if compiled.over_budget() {
        eprintln!("jcc: WARNING — code exceeds GPU local RAM budget");
    }

    if let Some(out) = output {
        if let Err(e) = std::fs::write(&out, &compiled.asm) {
            eprintln!("jcc: cannot write {out}: {e}");
            return ExitCode::FAILURE;
        }
        eprintln!("jcc: wrote {out}");
    } else {
        print!("{}", compiled.asm);
    }
    ExitCode::SUCCESS
}
