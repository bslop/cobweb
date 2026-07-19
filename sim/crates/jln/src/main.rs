//! jln — command-line Cobweb linker.
//!
//!   jln a.jo b.jo ... -o image.bin [--map]
//!
//! Resolves cross-object symbols and relocations, lays objects out at their
//! orgs, and writes a loadable image. `--map` prints the resolved symbol table.

use std::process::ExitCode;

use jln::parse_objects;

fn parse_num(s: &str) -> Option<u32> {
    let s = s.trim();
    if let Some(h) = s.strip_prefix("0x").or_else(|| s.strip_prefix('$')) {
        u32::from_str_radix(h, 16).ok()
    } else {
        s.parse().ok()
    }
}

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
    let mut layout = jln::Layout::default();
    while let Some(a) = it.next() {
        match a.as_str() {
            "-o" => output = it.next().cloned(),
            "--map" => map = true,
            "--base" | "-Ttext" => {
                // relocating link: place objects sequentially from this address
                match it.next().and_then(|s| parse_num(s)) {
                    Some(v) => layout.base = Some(v),
                    None => {
                        eprintln!("jln: --base needs an address");
                        return ExitCode::FAILURE;
                    }
                }
            }
            "--align" => {
                if let Some(v) = it.next().and_then(|s| parse_num(s)) {
                    layout.align = v.max(1);
                }
            }
            "--entry" | "-e" => layout.entry = it.next().cloned(),
            "--defsym" => {
                // NAME=ADDR, or NAME=@end for the end of the placed image
                if let Some(def) = it.next() {
                    if let Some((name, val)) = def.split_once('=') {
                        let dv = if val.trim() == "@end" {
                            jln::DefVal::ImageEnd
                        } else {
                            jln::DefVal::Addr(parse_num(val).unwrap_or(0))
                        };
                        layout.defsyms.push((name.trim().to_string(), dv));
                    }
                }
            }
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
    let img = match jln::link_with(&objects, &layout) {
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
