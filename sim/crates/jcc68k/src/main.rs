//! jcc68k CLI: compile C to 68000 assembly, or straight to a binary via jas.
//!
//!   jcc68k input.c            # print 68000 assembly to stdout
//!   jcc68k input.c -o out.s   # write assembly
//!   jcc68k input.c -o out.bin --bin [--org 0xADDR]   # assemble to a raw binary
//!   jcc68k input.c --prog     # emit a complete program (startup + runtime)

use std::process::exit;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.is_empty() {
        eprintln!("usage: jcc68k <input.c> [-o out] [--bin] [--org 0xADDR] [--prog]");
        exit(2);
    }
    let mut input = None;
    let mut output = None;
    let mut to_bin = false;
    let mut whole_program = false;
    let mut org = 0x4000u32;
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "-o" => output = it.next().cloned(),
            "--bin" => to_bin = true,
            "--prog" => whole_program = true,
            "--org" => {
                org = it
                    .next()
                    .and_then(|s| parse_u32(s))
                    .unwrap_or_else(|| fail("--org needs a number"));
            }
            _ if a.starts_with('-') => fail(&format!("unknown flag {a}")),
            _ => input = Some(a.clone()),
        }
    }
    let input = input.unwrap_or_else(|| fail("no input file"));
    let src = std::fs::read_to_string(&input).unwrap_or_else(|e| fail(&format!("{input}: {e}")));

    let asm = if whole_program || to_bin {
        jcc68k::compile_program(&src)
    } else {
        jcc68k::compile(&src)
    }
    .unwrap_or_else(|e| fail(&format!("{input}: {e}")));

    if to_bin {
        let opts = jas::Options { org, start_m68k: true, ..Default::default() };
        let res = jas::assemble(&asm, &opts);
        if res.errors() > 0 {
            for d in &res.diags {
                eprintln!("{d:?}");
            }
            fail("assembly failed");
        }
        let out = output.unwrap_or_else(|| "a.bin".into());
        std::fs::write(&out, &res.bytes).unwrap_or_else(|e| fail(&format!("{out}: {e}")));
        eprintln!("jcc68k: wrote {out} ({} bytes at {:#X})", res.bytes.len(), org);
    } else {
        match output {
            Some(o) => std::fs::write(&o, asm).unwrap_or_else(|e| fail(&format!("{o}: {e}"))),
            None => print!("{asm}"),
        }
    }
}

fn parse_u32(s: &str) -> Option<u32> {
    let s = s.trim();
    if let Some(h) = s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")) {
        u32::from_str_radix(h, 16).ok()
    } else {
        s.parse().ok()
    }
}

fn fail(msg: &str) -> ! {
    eprintln!("jcc68k: {msg}");
    exit(1);
}
