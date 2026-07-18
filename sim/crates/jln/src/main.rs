//! jln — command-line Cobweb linker.
//!
//!   jln a.jo b.jo ... -o image.bin [--map]
//!
//! Resolves cross-object symbols and relocations, lays objects out at their
//! orgs, and writes a loadable image. `--map` prints the resolved symbol table.

use std::process::ExitCode;

use jln::{link, parse_objects};

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.is_empty() || args.iter().any(|a| a == "-h" || a == "--help") {
        eprintln!(
            "jln — the Cobweb linker\n\
             \n\
             USAGE: jln <a.jo> <b.jo> ... -o <image.bin> [--map]\n\
             \n\
             Resolves cross-object symbols and relocations from jas objects and\n\
             writes one loadable image (objects placed at their assembled orgs)."
        );
        return if args.is_empty() { ExitCode::FAILURE } else { ExitCode::SUCCESS };
    }

    let mut inputs = Vec::new();
    let mut output = None;
    let mut map = false;
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "-o" => output = it.next().cloned(),
            "--map" => map = true,
            s if s.starts_with('-') => {
                eprintln!("jln: unknown option `{s}`");
                return ExitCode::FAILURE;
            }
            s => inputs.push(s.to_string()),
        }
    }

    if inputs.is_empty() {
        eprintln!("jln: no input objects");
        return ExitCode::FAILURE;
    }

    let mut blobs = Vec::new();
    for path in &inputs {
        match std::fs::read(path) {
            Ok(b) => blobs.push(b),
            Err(e) => {
                eprintln!("jln: cannot read {path}: {e}");
                return ExitCode::FAILURE;
            }
        }
    }

    let objects = match parse_objects(&blobs) {
        Ok(o) => o,
        Err(e) => {
            eprintln!("jln: {e}");
            return ExitCode::FAILURE;
        }
    };
    let img = match link(&objects) {
        Ok(i) => i,
        Err(e) => {
            eprintln!("jln: {e}");
            return ExitCode::FAILURE;
        }
    };

    if map {
        let mut syms: Vec<_> = img.symbols.iter().collect();
        syms.sort_by_key(|(_, v)| **v);
        eprintln!("jln: symbol map:");
        for (name, addr) in syms {
            eprintln!("  ${addr:06X}  {name}");
        }
    }

    if let Some(out) = output {
        if let Err(e) = std::fs::write(&out, &img.bytes) {
            eprintln!("jln: cannot write {out}: {e}");
            return ExitCode::FAILURE;
        }
        eprintln!(
            "jln: linked {} object(s) -> {} ({} bytes at 0x{:06X})",
            inputs.len(),
            out,
            img.bytes.len(),
            img.base
        );
    } else {
        eprintln!(
            "jln: linked {} object(s) OK ({} bytes at 0x{:06X}) — no -o, nothing written",
            inputs.len(),
            img.bytes.len(),
            img.base
        );
    }
    ExitCode::SUCCESS
}
