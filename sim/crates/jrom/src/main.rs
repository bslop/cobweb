//! jrom CLI: `jrom program.cof -o game.j64 [--rom game.rom]`
//!
//! Packages any Jaguar executable (COF/ABS/JAG/raw) as a bootable cartridge:
//! `.j64` (universal signed header — MiSTer, BigPEmu, flash carts, real
//! hardware) and optionally `.rom` (Alpine convention, loads at $802000).

use std::process::exit;

fn fail(msg: &str) -> ! {
    eprintln!("jrom: {msg}");
    exit(1);
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.is_empty() || args.iter().any(|a| a == "-h" || a == "--help") {
        eprintln!(
            "jrom — package a Jaguar executable as a cartridge image\n\n\
             USAGE:\n  jrom <program.cof|.abs|.jag|.bin> -o <game.j64> [--rom <game.rom>]\n\n\
             .j64: signed universal header + boot stub + RAM image, 1MB-multiple.\n\
             \x20     Boots MiSTer's Jaguar core, BigPEmu, Virtual Jaguar, flash carts,\n\
             \x20     and real hardware (the header passes cart authentication).\n\
             .rom: Alpine image (no header) — loads/executes at $802000."
        );
        exit(if args.is_empty() { 2 } else { 0 });
    }
    let mut input = None;
    let mut out_j64 = None;
    let mut out_rom = None;
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "-o" => out_j64 = it.next().cloned(),
            "--rom" => out_rom = it.next().cloned(),
            s if s.starts_with('-') => fail(&format!("unknown flag {s}")),
            s => input = Some(s.to_string()),
        }
    }
    let input = input.unwrap_or_else(|| fail("no input file"));
    let out_j64 = out_j64.unwrap_or_else(|| fail("-o <game.j64> required"));
    let data = std::fs::read(&input).unwrap_or_else(|e| fail(&format!("{input}: {e}")));

    let img = jrom::build(&data).unwrap_or_else(|e| fail(&e));
    std::fs::write(&out_j64, &img.j64).unwrap_or_else(|e| fail(&format!("{out_j64}: {e}")));
    eprintln!(
        "jrom: wrote {out_j64} ({} MB cart; restores {} bytes to 0x{:06X}, entry 0x{:06X})",
        img.j64.len() >> 20,
        img.span.1,
        img.span.0,
        img.entry
    );
    if let Some(rp) = out_rom {
        std::fs::write(&rp, &img.rom).unwrap_or_else(|e| fail(&format!("{rp}: {e}")));
        eprintln!("jrom: wrote {rp} ({} bytes, Alpine @ $802000)", img.rom.len());
    }
    println!(
        "{{\"ok\":true,\"j64\":\"{}\",\"size\":{},\"base\":\"0x{:06X}\",\"len\":{},\"entry\":\"0x{:06X}\"}}",
        out_j64,
        img.j64.len(),
        img.span.0,
        img.span.1,
        img.entry
    );
}
