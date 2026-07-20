//! fxrun — run a kernel against a jopt fixture and show where it actually went.
//!
//! `jopt` deliberately reports only accept/reject; when a fixture fails its
//! non-vacuity check ("fixture never wrote the capture region") this answers
//! the next question — *why*: where the kernel halted, whether it was still
//! running at budget exhaustion, what it wrote to its mailbox, and whether any
//! of the capture region was touched.
//!
//!   cargo run --release -p jtest --example fxrun -- \
//!       <kernel.s> <fixture.fx> [-d NAME=VAL]... [--watch ADDR]...

use jag_core::risc::Fidelity;
use jag_core::{mem, risc::Risc, Bus, RiscKind};

fn parse_int(s: &str) -> Option<u32> {
    let s = s.trim();
    if let Some(h) = s.strip_prefix("0x").or_else(|| s.strip_prefix("$")) {
        u32::from_str_radix(h, 16).ok()
    } else {
        s.parse().ok()
    }
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut pos = Vec::new();
    let mut defines = Vec::new();
    let mut watches = Vec::new();
    let mut fill: Option<u8> = None;
    let mut dump_capture: Option<String> = None;
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "-d" => defines.push(it.next().expect("-d NAME=VAL").clone()),
            "--watch" => watches.push(parse_int(it.next().expect("--watch ADDR")).unwrap()),
            "--fill" => fill = Some(0xAAu8),
            "--dump-capture" => dump_capture = Some(it.next().expect("--dump-capture FILE").clone()),
            _ => pos.push(a.clone()),
        }
    }
    let (src_path, fx_path) = (&pos[0], &pos[1]);

    // Assemble (hazard check off: fxrun is a debugger, not a gatekeeper).
    let src = std::fs::read_to_string(src_path).expect("read kernel");
    let opts = jas::Options {
        target: jas::Target::Gpu,
        org: mem::G_RAM,
        check_hazards: false,
        defines,
        ..Default::default()
    };
    let out = jas::assemble(&src, &opts);
    assert_eq!(out.errors(), 0, "kernel does not assemble");
    println!("assembled: {} bytes @ 0x{:06X}", out.bytes.len(), out.org);

    // Parse the fixture (same directives as jopt's CLI).
    let fxdir = std::path::Path::new(fx_path).parent().unwrap();
    let mut budget = 20_000_000u32;
    let mut capture = (0u32, 0u32);
    let mut pre: Vec<(u32, Vec<u8>)> = Vec::new();
    for line in std::fs::read_to_string(fx_path).expect("read fixture").lines() {
        let line = line.split('#').next().unwrap_or("").trim();
        let t: Vec<&str> = line.split_whitespace().collect();
        match t.as_slice() {
            ["budget", v] => budget = parse_int(v).unwrap(),
            ["capture", a, l] => capture = (parse_int(a).unwrap(), parse_int(l).unwrap()),
            ["long", a, v] => pre.push((parse_int(a).unwrap(), parse_int(v).unwrap().to_be_bytes().to_vec())),
            ["blob", a, f] => pre.push((parse_int(a).unwrap(), std::fs::read(fxdir.join(f)).expect("blob"))),
            [] => {}
            _ => panic!("bad fixture line: {line}"),
        }
    }

    // Mirror jtest::run_with, but keep the bus/core for post-mortem.
    let mut bus = Bus::new();
    for (i, b) in out.bytes.iter().enumerate() {
        bus.write8(out.org + i as u32, *b);
    }
    for (addr, blob) in &pre {
        for (i, b) in blob.iter().enumerate() {
            bus.write8(addr + i as u32, *b);
        }
    }
    // Optional canary: fill the capture region so "wrote zeros" (e.g. the
    // Blitter reading textures that are not in the fixture's address space)
    // is distinguishable from "wrote nothing".
    let (ca, cl) = capture;
    if let Some(v) = fill {
        for i in 0..cl {
            bus.write8(ca + i, v);
        }
    }
    let before: Vec<u8> = (0..cl).map(|i| bus.read8(ca + i)).collect();
    // Full-DRAM snapshot: find where the kernel REALLY writes, not where the
    // fixture assumed it would.
    let dram_before: Vec<u8> = (0..0x200000u32).map(|a| bus.read8(a)).collect();
    bus.write32(mem::G_PC, out.org);
    bus.write32(mem::G_CTRL, mem::RISCGO);
    let mut core = Risc::new(RiscKind::Gpu);
    core.fidelity = Fidelity::Silicon;
    // Run in slices, sampling PC each slice — a coarse profile that shows which
    // loop the budget actually went into (jopt only reports the endpoint).
    let mut hist: std::collections::HashMap<u32, u32> = std::collections::HashMap::new();
    let slices = 20_000u32;
    for _ in 0..slices {
        core.run(&mut bus, budget / slices);
        *hist.entry(core.pc).or_insert(0) += 1;
        if !core.running {
            break;
        }
    }
    let mut top: Vec<(u32, u32)> = hist.into_iter().collect();
    top.sort_by(|a, b| b.1.cmp(&a.1));
    println!("pc samples (top 8 of {} slices):", slices);
    for (pc, n) in top.iter().take(8) {
        println!("  0x{pc:06X} (org+0x{:X})  {n}", pc.wrapping_sub(out.org));
    }

    println!("after {} of {} budget ticks:", core.cycles, budget);
    println!("  running   {}   (false = halted itself; true = budget ran out)", core.running);
    println!("  pc        0x{:06X}  (org+0x{:X})", core.pc, core.pc.wrapping_sub(out.org));
    println!("  instret   {}", core.instret);
    let params = 0xF03F00u32;
    for i in 0..8 {
        let v = bus.read32(params + i * 4);
        if v != 0 {
            println!("  params[{i}] = 0x{v:08X}");
        }
    }
    let mailbox = bus.read32(params + 4);
    if mailbox != 0 {
        println!("  mailbox @0x{:06X} = 0x{:08X}", mailbox, bus.read32(mailbox));
    }
    let after: Vec<u8> = (0..cl).map(|i| bus.read8(ca + i)).collect();
    // Changed DRAM ranges (coalesced), so a mispointed capture is obvious.
    let mut ranges: Vec<(u32, u32)> = Vec::new();
    let mut a = 0u32;
    while a < 0x200000 {
        if bus.read8(a) != dram_before[a as usize] {
            let start = a;
            let mut last = a;
            while a < 0x200000 {
                if bus.read8(a) != dram_before[a as usize] {
                    last = a;
                } else if a - last > 0x100 {
                    break;
                }
                a += 1;
            }
            ranges.push((start, last));
        } else {
            a += 1;
        }
    }
    println!("  DRAM ranges changed by the run:");
    for (s0, e0) in ranges.iter().take(12) {
        println!("    0x{s0:06X}..0x{e0:06X}  ({} bytes span)", e0 - s0 + 1);
    }
    if ranges.is_empty() {
        println!("    none — the kernel wrote nothing anywhere in DRAM");
    }
    let changed = before.iter().zip(&after).filter(|(a, b)| a != b).count();
    let nonzero = after.iter().filter(|&&b| b != 0).count();
    println!("  capture 0x{ca:06X}+{cl}: {changed} bytes CHANGED by the run, {nonzero} non-zero");
    if let Some(f) = &dump_capture {
        std::fs::write(f, &after).expect("dump capture");
        println!("  capture written to {f}");
    }
    if changed > 0 {
        let first = before.iter().zip(&after).position(|(a, b)| a != b).unwrap();
        println!("    first change at +0x{first:X}: 0x{:02X} -> 0x{:02X}", before[first], after[first]);
    }
    for w in watches {
        println!("  watch 0x{w:06X} = 0x{:08X}", bus.read32(w));
    }
}
