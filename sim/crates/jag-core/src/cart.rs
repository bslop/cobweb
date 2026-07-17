//! Program loaders: COF (primary conveyor-belt output), raw binary, and
//! Alcyon ABS / `.jag`.
//!
//! ## COF (verified against a reference COF)
//!
//! Standard big-endian 68k COFF:
//! * 20-byte file header: `magic(2)=0x0150, nscns(2), timdat(4), symptr(4),
//!   nsyms(4), opthdr(2), flags(2)`.
//! * optional AOUTHDR (`opthdr` bytes, here 28): `magic(2), vstamp(2), tsize(4),
//!   dsize(4), bsize(4), entry(4), text_start(4), data_start(4)`.
//! * `nscns` × 40-byte section headers immediately after the AOUTHDR:
//!   `name(8), paddr(4), vaddr(4), size(4), scnptr(4), relptr(4), lnnoptr(4),
//!   nreloc(2), nlnno(2), flags(4)`.
//! * raw section data lives at file offset `scnptr`; `.bss` has none.
//!
//! ## Jaguar/BigPEmu COF quirks (porting notes, made configurable)
//!
//! * BigPEmu **refuses COF sections below vaddr `$2000`** — replicated via
//!   [`LoadOptions::min_section_vaddr`].
//! * BigPEmu **ignores the COF entry field**; execution starts at **text
//!   start**. We default `entry` to the lowest loaded `.text` vaddr.

use crate::bus::Bus;
use crate::mem;

#[derive(Debug, Clone)]
pub enum LoadError {
    TooShort,
    UnknownFormat,
    Malformed(&'static str),
}

impl std::fmt::Display for LoadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LoadError::TooShort => write!(f, "file too short"),
            LoadError::UnknownFormat => write!(f, "unrecognized program format"),
            LoadError::Malformed(s) => write!(f, "malformed file: {s}"),
        }
    }
}
impl std::error::Error for LoadError {}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    Cof,
    AbsAlcyon,
    Jag,
    /// Commercial cartridge dump: mapped at `$800000`, entry from the cart
    /// header at `$800404`. We HLE the boot ROM (skip the Atari signature check).
    Rom,
    Raw,
}

#[derive(Debug, Clone)]
pub struct Section {
    pub name: String,
    pub vaddr: u32,
    pub size: u32,
    pub loaded: bool,
}

#[derive(Debug, Clone)]
pub struct Cartridge {
    pub format: Format,
    pub entry: u32,
    pub sections: Vec<Section>,
}

#[derive(Debug, Clone)]
pub struct LoadOptions {
    /// Sections strictly below this vaddr are skipped (BigPEmu refuses < $2000).
    pub min_section_vaddr: u32,
    /// Override the entry point; otherwise derived per-format.
    pub entry_override: Option<u32>,
    /// Base address used when loading a raw binary.
    pub raw_base: u32,
}

impl Default for LoadOptions {
    fn default() -> Self {
        LoadOptions { min_section_vaddr: 0x2000, entry_override: None, raw_base: mem::USERRAM }
    }
}

#[inline]
fn be16(d: &[u8], o: usize) -> u16 {
    u16::from_be_bytes([d[o], d[o + 1]])
}
#[inline]
fn be32(d: &[u8], o: usize) -> u32 {
    u32::from_be_bytes([d[o], d[o + 1], d[o + 2], d[o + 3]])
}

/// Detect the container format from its header bytes.
pub fn detect(data: &[u8]) -> Format {
    if data.len() >= 2 {
        match be16(data, 0) {
            0x0150 | 0x0151 => return Format::Cof,
            0x601A | 0x601B => return Format::AbsAlcyon,
            _ => {}
        }
    }
    // `.jag` server format begins with the ASCII tag "JAGR".
    if data.len() >= 4 && &data[0..4] == b"JAGR" {
        return Format::Jag;
    }
    // Homebrew cart images built on the community "universal header"
    // (SubQMod's signed boot block — vbcc's Univ.bin, many releases): the
    // first 8 bytes are a fixed signature. Sizes vary (vbcc emits 64 KB+),
    // so detect by content, not size.
    const UNIV_SIG: [u8; 8] = [0xF6, 0x42, 0x23, 0x3C, 0x0D, 0xAC, 0x1D, 0x8D];
    if data.len() >= 0x408 && data[0..8] == UNIV_SIG {
        return Format::Rom;
    }
    // Commercial cartridge dumps (`.jag`/`.j64`) have no magic; they are whole
    // ROM images sized as a multiple of 1 MB (or the 128 KB Memory Track).
    let n = data.len();
    if n >= 0x800 && (n % (1 << 20) == 0 || n == 0x2_0000) {
        return Format::Rom;
    }
    Format::Raw
}

/// Load a program into `bus`, returning its description.
pub fn load(data: &[u8], bus: &mut Bus) -> Result<Cartridge, LoadError> {
    load_with(data, bus, &LoadOptions::default())
}

pub fn load_with(data: &[u8], bus: &mut Bus, opt: &LoadOptions) -> Result<Cartridge, LoadError> {
    match detect(data) {
        Format::Cof => load_cof(data, bus, opt),
        Format::AbsAlcyon => load_abs(data, bus, opt),
        Format::Jag => load_jag(data, bus, opt),
        Format::Rom => load_rom_cart(data, bus, opt),
        Format::Raw => load_raw(data, bus, opt),
    }
}

/// Commercial cartridge: map the whole image at `$800000` and take the entry
/// from the cart header long at `$800404` (file offset `$404`). We HLE the boot
/// ROM — skip the Atari signature check and jump straight to the cart entry.
fn load_rom_cart(data: &[u8], bus: &mut Bus, opt: &LoadOptions) -> Result<Cartridge, LoadError> {
    bus.load_cart(data.to_vec());
    let entry = opt.entry_override.unwrap_or_else(|| {
        if data.len() >= 0x408 {
            be32(data, 0x404)
        } else {
            mem::CART_START + 0x2000
        }
    });
    Ok(Cartridge {
        format: Format::Rom,
        entry,
        sections: vec![Section {
            name: "cart".into(),
            vaddr: mem::CART_START,
            size: data.len() as u32,
            loaded: true,
        }],
    })
}

fn load_cof(data: &[u8], bus: &mut Bus, opt: &LoadOptions) -> Result<Cartridge, LoadError> {
    if data.len() < 20 {
        return Err(LoadError::TooShort);
    }
    let nscns = be16(data, 2) as usize;
    let opthdr = be16(data, 16) as usize;
    let sec_base = 20 + opthdr;
    if data.len() < sec_base + nscns * 40 {
        return Err(LoadError::Malformed("section headers exceed file"));
    }

    // AOUTHDR text_start, if present. AOUTHDR starts at file offset 20; its
    // layout is magic(2) vstamp(2) tsize(4) dsize(4) bsize(4) entry(4)
    // text_start(4) data_start(4), so text_start is at file offset 20+0x14.
    let aout_text_start = if opthdr >= 28 { Some(be32(data, 20 + 0x14)) } else { None };

    let mut sections = Vec::with_capacity(nscns);
    let mut lowest_text: Option<u32> = None;

    for i in 0..nscns {
        let h = sec_base + i * 40;
        let name = cstr8(&data[h..h + 8]);
        let vaddr = be32(data, h + 12);
        let size = be32(data, h + 16);
        let scnptr = be32(data, h + 20) as usize;
        let flags = be32(data, h + 36);
        let is_bss = flags & 0x80 != 0 || (scnptr == 0 && name == ".bss");
        let is_text = flags & 0x20 != 0 || name == ".text";

        // BigPEmu refuses sections below the floor.
        let mut loaded = false;
        if vaddr >= opt.min_section_vaddr && size > 0 {
            if is_bss {
                zero_dram(bus, vaddr, size);
                loaded = true;
            } else if scnptr != 0 {
                let end = scnptr + size as usize;
                if end > data.len() {
                    return Err(LoadError::Malformed("section data exceeds file"));
                }
                copy_dram(bus, vaddr, &data[scnptr..end]);
                loaded = true;
            }
        }
        if is_text && loaded {
            lowest_text = Some(lowest_text.map_or(vaddr, |t| t.min(vaddr)));
        }
        sections.push(Section { name, vaddr, size, loaded });
    }

    // Entry: per the porting notes BigPEmu ignores the COF entry field and
    // starts at text start. Prefer the lowest loaded .text vaddr, fall back to
    // the AOUTHDR text_start, then the AOUTHDR entry.
    let entry = opt
        .entry_override
        .or(lowest_text)
        .or(aout_text_start)
        .unwrap_or(mem::USERRAM);

    Ok(Cartridge { format: Format::Cof, entry, sections })
}

/// Alcyon absolute (`$601A`/`$601B`). Header (28 bytes, big-endian):
/// `magic(2), tsize(4), dsize(4), bsize(4), symsize(4), reserved(4),
/// text_base(4)`. `$601B` carries explicit text+data bases; `$601A` implies
/// contiguous load at `text_base`.
fn load_abs(data: &[u8], bus: &mut Bus, opt: &LoadOptions) -> Result<Cartridge, LoadError> {
    if data.len() < 0x24 {
        return Err(LoadError::TooShort);
    }
    let magic = be16(data, 0);
    let tsize = be32(data, 2);
    let dsize = be32(data, 6);
    let bsize = be32(data, 10);
    let (text_base, data_base) = if magic == 0x601B {
        (be32(data, 0x18), be32(data, 0x1C))
    } else {
        let tb = be32(data, 0x16);
        (tb, tb + tsize)
    };
    let hdr = if magic == 0x601B { 0x20 } else { 0x1C };
    let tend = hdr + tsize as usize;
    let dend = tend + dsize as usize;
    if dend > data.len() {
        return Err(LoadError::Malformed("abs text/data exceed file"));
    }
    copy_dram(bus, text_base, &data[hdr..tend]);
    copy_dram(bus, data_base, &data[tend..dend]);
    zero_dram(bus, data_base + dsize, bsize);
    let entry = opt.entry_override.unwrap_or(text_base);
    Ok(Cartridge {
        format: Format::AbsAlcyon,
        entry,
        sections: vec![
            Section { name: ".text".into(), vaddr: text_base, size: tsize, loaded: true },
            Section { name: ".data".into(), vaddr: data_base, size: dsize, loaded: true },
            Section { name: ".bss".into(), vaddr: data_base + dsize, size: bsize, loaded: true },
        ],
    })
}

/// `.jag` server format: 4-byte "JAGR" tag, then `flags(4), load(4), size(4),
/// run(4)`, then raw payload. (Mirrors the Atari/skunkboard server header.)
fn load_jag(data: &[u8], bus: &mut Bus, opt: &LoadOptions) -> Result<Cartridge, LoadError> {
    if data.len() < 20 {
        return Err(LoadError::TooShort);
    }
    let load = be32(data, 8);
    let size = be32(data, 12);
    let run = be32(data, 16);
    let payload = 20;
    let end = payload + size as usize;
    if end > data.len() {
        return Err(LoadError::Malformed("jag payload exceeds file"));
    }
    copy_dram(bus, load, &data[payload..end]);
    let entry = opt.entry_override.unwrap_or(run);
    Ok(Cartridge {
        format: Format::Jag,
        entry,
        sections: vec![Section { name: "image".into(), vaddr: load, size, loaded: true }],
    })
}

fn load_raw(data: &[u8], bus: &mut Bus, opt: &LoadOptions) -> Result<Cartridge, LoadError> {
    copy_dram(bus, opt.raw_base, data);
    let entry = opt.entry_override.unwrap_or(opt.raw_base);
    Ok(Cartridge {
        format: Format::Raw,
        entry,
        sections: vec![Section {
            name: "raw".into(),
            vaddr: opt.raw_base,
            size: data.len() as u32,
            loaded: true,
        }],
    })
}

fn cstr8(b: &[u8]) -> String {
    let n = b.iter().position(|&c| c == 0).unwrap_or(b.len());
    String::from_utf8_lossy(&b[..n]).into_owned()
}

/// Copy bytes into DRAM (clamped to the 2 MB window).
fn copy_dram(bus: &mut Bus, vaddr: u32, src: &[u8]) {
    for (k, &b) in src.iter().enumerate() {
        let a = vaddr.wrapping_add(k as u32);
        if mem::is_dram(a) {
            bus.dram[a as usize] = b;
        }
    }
}

fn zero_dram(bus: &mut Bus, vaddr: u32, size: u32) {
    for k in 0..size {
        let a = vaddr.wrapping_add(k);
        if mem::is_dram(a) {
            bus.dram[a as usize] = 0;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a minimal valid COF with one .text section and assert it loads at
    /// the right vaddr with the right entry.
    #[test]
    fn cof_minimal_loads_text() {
        let opthdr: u16 = 28;
        let text_vaddr: u32 = 0x4000;
        let text: &[u8] = &[0x4E, 0x71, 0x4E, 0x75]; // NOP ; RTS
        let sec_base = 20 + opthdr as usize;
        let scnptr = sec_base + 40; // one section header, then raw data

        let mut d = vec![0u8; scnptr + text.len()];
        // file header
        d[0..2].copy_from_slice(&0x0150u16.to_be_bytes());
        d[2..4].copy_from_slice(&1u16.to_be_bytes()); // nscns
        d[16..18].copy_from_slice(&opthdr.to_be_bytes());
        // AOUTHDR (starts at file offset 20): entry @ +0x10, text_start @ +0x14.
        d[20 + 0x10..20 + 0x14].copy_from_slice(&text_vaddr.to_be_bytes()); // entry
        d[20 + 0x14..20 + 0x18].copy_from_slice(&text_vaddr.to_be_bytes()); // text_start
        // section header
        let h = sec_base;
        d[h..h + 5].copy_from_slice(b".text");
        d[h + 12..h + 16].copy_from_slice(&text_vaddr.to_be_bytes()); // vaddr
        d[h + 16..h + 20].copy_from_slice(&(text.len() as u32).to_be_bytes()); // size
        d[h + 20..h + 24].copy_from_slice(&(scnptr as u32).to_be_bytes()); // scnptr
        d[h + 36..h + 40].copy_from_slice(&0x20u32.to_be_bytes()); // STYP_TEXT
        d[scnptr..scnptr + text.len()].copy_from_slice(text);

        let mut bus = Bus::new();
        let cart = load(&d, &mut bus).unwrap();
        assert_eq!(cart.format, Format::Cof);
        assert_eq!(cart.entry, text_vaddr);
        assert_eq!(bus.read16(text_vaddr), 0x4E71); // NOP landed
        assert_eq!(bus.read16(text_vaddr + 2), 0x4E75); // RTS landed
    }

    #[test]
    fn detect_formats() {
        assert_eq!(detect(&[0x01, 0x50, 0, 0]), Format::Cof);
        assert_eq!(detect(&[0x60, 0x1A, 0, 0]), Format::AbsAlcyon);
        assert_eq!(detect(b"JAGR1234"), Format::Jag);
        assert_eq!(detect(&[0xDE, 0xAD]), Format::Raw);
    }
}
