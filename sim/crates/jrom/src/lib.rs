//! jrom — package a Jaguar executable as a cartridge image, so a build runs
//! everywhere: MiSTer's Jaguar core, BigPEmu, Virtual Jaguar, flash carts —
//! not just a Skunkboard/GameDrive rig.
//!
//! ## The cartridge layout (the community "universal header" convention)
//!
//! ```text
//! $800000  Univ.bin — SubQMod's signed 8 KB boot block. Passes the real boot
//!          ROM's cartridge authentication, so the image boots ORIGINAL
//!          hardware and cores that run the real BIOS (MiSTer). Its header
//!          fields: [$800400]=$04040404 (cart flags), [$800404]=$00802000
//!          (entry — right past the block).
//! $802000  boot stub, assembled by jas at build time: copies the payload
//!          from cart space into DRAM and jumps to the program entry.
//! $802000+stub  the payload — the program's RAM image, exactly as a COF
//!          load would place it.
//! …        zero-fill to a 1 MB multiple (dumps are detected by that shape,
//!          and flash carts/cores expect it).
//! ```
//!
//! The input is anything jag-core's loader accepts (COF/ABS/JAG/raw): the
//! file is loaded into a scratch [`Bus`] exactly as jsim would load it, and
//! the cart packages whatever landed in DRAM. One loader, no format drift.
//!
//! `.rom` (Alpine) output is the same stub+payload WITHOUT the header — the
//! Alpine/VJ `--alpine` convention loads it at `$802000` directly.

use jag_core::{cart, Bus};

/// SubQMod's universal signed boot block — sha256
/// 3f74561… (the copy every homebrew toolchain ships). Vendored so a cart
/// build needs nothing outside this repo.
pub const UNIV: &[u8] = include_bytes!("univ.bin");

/// Where the boot stub (and Alpine images) execute: right past the header.
pub const CART_CODE: u32 = 0x0080_2000;

pub struct CartImage {
    /// Full `.j64` image (header + stub + payload, 1 MB-multiple).
    pub j64: Vec<u8>,
    /// Alpine `.rom` image (stub + payload, loads at $802000).
    pub rom: Vec<u8>,
    /// RAM span the stub restores: (base, len).
    pub span: (u32, u32),
    /// Program entry the stub jumps to.
    pub entry: u32,
}

/// Package `program` (COF/ABS/JAG/raw bytes) as a cartridge.
pub fn build(program: &[u8]) -> Result<CartImage, String> {
    // 1. Load exactly as jsim would, into a scratch machine.
    let mut bus = Bus::new();
    let info = cart::load(program, &mut bus).map_err(|e| e.to_string())?;

    // 2. The RAM image: the span covering every loaded section.
    let loaded: Vec<_> = info.sections.iter().filter(|s| s.loaded && s.size > 0).collect();
    if loaded.is_empty() {
        return Err("no loadable sections".into());
    }
    let base = loaded.iter().map(|s| s.vaddr).min().unwrap();
    let end = loaded.iter().map(|s| s.vaddr + s.size).max().unwrap();
    if end > jag_core::mem::DRAM_END {
        return Err(format!("image end 0x{end:06X} beyond DRAM"));
    }
    let mut payload = bus.dram[base as usize..end as usize].to_vec();
    while payload.len() % 4 != 0 {
        payload.push(0);
    }

    // 3. The boot stub, assembled by jas with the real constants. Runs from
    //    cart space in supervisor mode right after the signed block.
    let stub_src = stub_source(base, payload.len() as u32, info.entry);
    let opts = jas::Options {
        org: CART_CODE,
        start_m68k: true,
        check_hazards: false,
        ..Default::default()
    };
    let out = jas::assemble(&stub_src, &opts);
    if out.errors() > 0 {
        return Err(format!("stub assembly failed: {:#?}", out.diags));
    }
    let stub = out.bytes;

    // 4. Alpine image: stub + payload at $802000.
    let mut rom = stub.clone();
    rom.extend_from_slice(&payload);

    // 5. .j64: header + the same, padded to a 1 MB multiple.
    let mut j64 = UNIV.to_vec();
    debug_assert_eq!(j64.len(), 0x2000);
    j64.extend_from_slice(&rom);
    let mb = 1usize << 20;
    let target = j64.len().div_ceil(mb) * mb;
    j64.resize(target, 0);

    Ok(CartImage { j64, rom, span: (base, payload.len() as u32), entry: info.entry })
}

/// 68k boot stub source. PC-relative source address so the same code works
/// in the headered ($802000+stub) and Alpine layouts without relinking.
fn stub_source(base: u32, len: u32, entry: u32) -> String {
    format!(
        "\t.68000\n\
         \t.org ${org:X}\n\
         start:\n\
         \tmove.w\t#$2700,sr\n\
         \tlea\tpayload(pc),a0\n\
         \tmovea.l\t#${base:X},a1\n\
         \tmove.l\t#${longs:X},d0\n\
         copy:\n\
         \tmove.l\t(a0)+,(a1)+\n\
         \tsubq.l\t#1,d0\n\
         \tbne.s\tcopy\n\
         \tmovea.l\t#$001F0000,a7\n\
         \tmove.l\t#${entry:X},a2\n\
         \tjmp\t(a2)\n\
         \t.even\n\
         payload:\n",
        org = CART_CODE,
        base = base,
        longs = len / 4,
        entry = entry,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use jag_core::Jaguar;

    /// A tiny program: write a magic long to $100 and spin. Assembled with
    /// jas as a RAM image at $4000, packaged, then BOOTED AS A CART in jsim —
    /// the loader path an emulator/MiSTer would take.
    #[test]
    fn cart_boots_and_runs_the_payload() {
        let prog_src = "\t.68000\n\t.org $4000\n\
             start:\n\tmove.l #$CA7B007,d0\n\tmove.l d0,$100\n\
             halt:\n\tbra.w halt\n";
        let opts = jas::Options {
            org: 0x4000,
            start_m68k: true,
            check_hazards: false,
            ..Default::default()
        };
        let out = jas::assemble(prog_src, &opts);
        assert_eq!(out.errors(), 0, "{:#?}", out.diags);

        let img = build(&out.bytes).expect("packages");
        assert_eq!(img.j64.len() % (1 << 20), 0, "1MB multiple");
        assert_eq!(&img.j64[0..8], &UNIV[0..8], "signed header present");
        assert_eq!(img.span.0, 0x4000);

        // Boot the .j64 as a cartridge.
        let mut jag = Jaguar::new();
        jag.load(&img.j64).expect("cart loads");
        let mut prev = u32::MAX;
        for _ in 0..200_000 {
            if jag.cpu.pc == prev {
                break;
            }
            prev = jag.cpu.pc;
            jag.step_instruction();
        }
        assert_eq!(jag.bus.read32(0x100), 0x0CA7_B007, "payload ran from DRAM");
    }

    #[test]
    fn alpine_rom_boots_at_802000() {
        let prog_src = "\t.68000\n\t.org $4000\n\
             start:\n\tmove.l #$A1B1FE,d0\n\tmove.l d0,$104\n\
             halt:\n\tbra.w halt\n";
        let opts = jas::Options {
            org: 0x4000,
            start_m68k: true,
            check_hazards: false,
            ..Default::default()
        };
        let out = jas::assemble(prog_src, &opts);
        assert_eq!(out.errors(), 0);
        let img = build(&out.bytes).expect("packages");

        // Alpine load: cart space with the image at offset $2000 (= $802000),
        // execute there (no header, exactly `jcp -f` / VJ --alpine).
        let mut jag = Jaguar::new();
        let mut cartimg = vec![0u8; 0x2000];
        cartimg.extend_from_slice(&img.rom);
        jag.bus.load_cart(cartimg);
        jag.cpu.set_pc(CART_CODE);
        let mut prev = u32::MAX;
        for _ in 0..200_000 {
            if jag.cpu.pc == prev {
                break;
            }
            prev = jag.cpu.pc;
            jag.step_instruction();
        }
        assert_eq!(jag.bus.read32(0x104), 0x00A1_B1FE, "alpine payload ran");
    }
}
