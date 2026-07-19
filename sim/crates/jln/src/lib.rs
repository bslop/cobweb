//! jln — the Cobweb linker, the 8th component and the successor to rln.
//!
//! Takes the relocatable objects jas emits (`.jo`), builds one global symbol
//! table, resolves every relocation, patches the bytes, and lays the objects
//! out into a single loadable image. This is what turns a pile of separately
//! assembled GPU/DSP/data fragments into something that runs — and it is where
//! the wishlist's headline linker feature lives (SRAM **overlay groups** as
//! first-class objects; v1 does the base link, overlays are the next step).
//!
//! v1 layout model: each object is placed at its own assembled `.org` (Jaguar
//! GPU/DSP code is written for a fixed local-RAM address), so local labels stay
//! valid; the linker's job is to resolve the *cross-object* references. The
//! image spans from the lowest org to the highest end, gaps zero-filled.

use std::collections::HashMap;

use jas::object::{Object, RelKind};

/// A linker error.
#[derive(Debug)]
pub enum LinkError {
    BadObject(String),
    DuplicateSymbol(String),
    Undefined { symbol: String, in_obj: usize },
    RelocOutOfRange(u32),
}

impl std::fmt::Display for LinkError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LinkError::BadObject(s) => write!(f, "not a valid .jo object: {s}"),
            LinkError::DuplicateSymbol(s) => write!(f, "symbol `{s}` defined in more than one object"),
            LinkError::Undefined { symbol, in_obj } => {
                write!(f, "undefined reference to `{symbol}` (from object #{in_obj})")
            }
            LinkError::RelocOutOfRange(o) => write!(f, "relocation at offset {o} is out of bounds"),
        }
    }
}
impl std::error::Error for LinkError {}

/// A linked image.
pub struct Image {
    /// Address the image loads at (the lowest placed address).
    pub base: u32,
    pub bytes: Vec<u8>,
    /// Every global symbol and its final address.
    pub symbols: HashMap<String, u32>,
    /// Entry point: the `entry` symbol's address, or the lowest placed address.
    pub entry: u32,
}

/// How the linker assigns addresses to objects.
#[derive(Debug, Clone)]
pub struct Layout {
    /// `Some(addr)` places objects sequentially from `addr` (a relocating link —
    /// each object's symbols and absolute relocations are rebased). `None` keeps
    /// every object at its own assembled `.org` (the fixed-address model, for
    /// GPU/DSP local-RAM code).
    pub base: Option<u32>,
    /// Alignment (bytes) applied before each sequentially-placed object.
    pub align: u32,
    /// Entry-point symbol; falls back to the lowest placed address.
    pub entry: Option<String>,
    /// Link-script symbol definitions (`--defsym NAME=ADDR`), e.g. the section
    /// boundary markers (`__bss_start`) that startup code references. The literal
    /// value `"@end"` resolves to the end of the placed image.
    pub defsyms: Vec<(String, DefVal)>,
}

/// Value of a `--defsym`: a fixed address or a symbolic image position.
#[derive(Debug, Clone)]
pub enum DefVal {
    Addr(u32),
    /// The address just past the last placed byte (for `__bss_*`/`__end`).
    ImageEnd,
}

impl Default for Layout {
    fn default() -> Self {
        // Back-compat: honor each object's assembled org.
        Layout { base: None, align: 2, entry: None, defsyms: Vec::new() }
    }
}

/// Parse `.jo` bytes into objects.
pub fn parse_objects(blobs: &[Vec<u8>]) -> Result<Vec<Object>, LinkError> {
    blobs
        .iter()
        .enumerate()
        .map(|(i, b)| Object::deserialize(b).ok_or_else(|| LinkError::BadObject(format!("#{i}"))))
        .collect()
}

/// Link objects into an image at each object's assembled org (back-compat).
pub fn link(objects: &[Object]) -> Result<Image, LinkError> {
    link_with(objects, &Layout::default())
}

/// Link objects into an image using `layout`. When `layout.base` is set the link
/// is *relocating*: objects are placed sequentially and their symbols/relocations
/// are rebased by the delta between the placed address and the assembled org.
pub fn link_with(objects: &[Object], layout: &Layout) -> Result<Image, LinkError> {
    // 1. assign a placement base to each object.
    let align = layout.align.max(1);
    let mut placed: Vec<u32> = Vec::with_capacity(objects.len());
    if let Some(start) = layout.base {
        let mut cursor = start;
        for obj in objects {
            cursor = (cursor + align - 1) / align * align;
            placed.push(cursor);
            cursor += obj.bytes.len() as u32;
        }
    } else {
        placed.extend(objects.iter().map(|o| o.org));
    }
    // per-object rebase delta (placed address − assembled org).
    let delta = |i: usize| placed[i] as i64 - objects[i].org as i64;

    // image extent (needed to resolve `@end` defsyms below).
    let img_base = placed.iter().copied().min().unwrap_or(0);
    let img_end = placed
        .iter()
        .zip(objects)
        .map(|(&p, o)| p + o.bytes.len() as u32)
        .max()
        .unwrap_or(img_base);

    // 2. global symbol table, rebased. Globals must be unique.
    let mut globals: HashMap<String, u32> = HashMap::new();
    for (i, obj) in objects.iter().enumerate() {
        for s in &obj.symbols {
            if s.global {
                let addr = (s.value as i64 + delta(i)) as u32;
                if globals.insert(s.name.clone(), addr).is_some() {
                    return Err(LinkError::DuplicateSymbol(s.name.clone()));
                }
            }
        }
    }
    // link-script symbol definitions (section markers etc.), lowest precedence.
    for (name, val) in &layout.defsyms {
        let addr = match val {
            DefVal::Addr(a) => *a,
            DefVal::ImageEnd => img_end,
        };
        globals.entry(name.clone()).or_insert(addr);
    }
    // per-object local symbol tables (rebased), used to resolve a reference to a
    // non-exported symbol within its own object before falling back to globals.
    let locals: Vec<HashMap<String, u32>> = objects
        .iter()
        .enumerate()
        .map(|(i, obj)| {
            obj.symbols
                .iter()
                .map(|s| (s.name.clone(), (s.value as i64 + delta(i)) as u32))
                .collect()
        })
        .collect();

    // 3. patch relocations against the rebased addresses. Collect every
    //    unresolved symbol first so the user sees the full list, not just one.
    let mut patched: Vec<Object> = objects.to_vec();
    let mut missing: Vec<(String, usize)> = Vec::new();
    for (oi, obj) in objects.iter().enumerate() {
        for r in &obj.relocs {
            if locals[oi].get(&r.symbol).or_else(|| globals.get(&r.symbol)).is_none()
                && !missing.iter().any(|(n, _)| n == &r.symbol)
            {
                missing.push((r.symbol.clone(), oi));
            }
        }
    }
    if let Some((symbol, in_obj)) = missing.first() {
        if missing.len() > 1 {
            eprintln!("jln: {} undefined symbols:", missing.len());
            for (n, o) in &missing {
                eprintln!("  `{n}` (from object #{o})");
            }
        }
        return Err(LinkError::Undefined { symbol: symbol.clone(), in_obj: *in_obj });
    }
    for (oi, obj) in patched.iter_mut().enumerate() {
        for r in &obj.relocs {
            // prefer this object's own definition, then the global table.
            let target = locals[oi]
                .get(&r.symbol)
                .or_else(|| globals.get(&r.symbol))
                .copied()
                .ok_or_else(|| LinkError::Undefined { symbol: r.symbol.clone(), in_obj: oi })?;
            let val = (target as i64 + r.addend) as u32;
            let off = r.offset as usize;
            match r.kind {
                RelKind::Movei => {
                    put_be16(&mut obj.bytes, off, (val & 0xFFFF) as u16)?;
                    put_be16(&mut obj.bytes, off + 2, (val >> 16) as u16)?;
                }
                RelKind::Long => {
                    put_be16(&mut obj.bytes, off, (val >> 16) as u16)?;
                    put_be16(&mut obj.bytes, off + 2, (val & 0xFFFF) as u16)?;
                }
                RelKind::Word => put_be16(&mut obj.bytes, off, (val & 0xFFFF) as u16)?,
            }
        }
    }

    // 4. lay out at the placed addresses into one sparse image.
    let base = placed.iter().copied().min().unwrap_or(0);
    let end = placed
        .iter()
        .zip(&patched)
        .map(|(&p, o)| p + o.bytes.len() as u32)
        .max()
        .unwrap_or(base);
    let mut bytes = vec![0u8; (end - base) as usize];
    for (i, obj) in patched.iter().enumerate() {
        let at = (placed[i] - base) as usize;
        bytes[at..at + obj.bytes.len()].copy_from_slice(&obj.bytes);
    }

    let entry = layout
        .entry
        .as_ref()
        .and_then(|e| globals.get(e).copied())
        .unwrap_or(base);
    Ok(Image { base, bytes, symbols: globals, entry })
}

fn put_be16(b: &mut [u8], off: usize, v: u16) -> Result<(), LinkError> {
    if off + 2 > b.len() {
        return Err(LinkError::RelocOutOfRange(off as u32));
    }
    b[off..off + 2].copy_from_slice(&v.to_be_bytes());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use jag_core::risc::Fidelity;
    use jag_core::{mem, Bus, Risc, RiscKind};

    fn asm_obj(src: &str, org: u32) -> Object {
        let opts = jas::Options {
            target: jas::Target::Gpu,
            org,
            object_mode: true,
            check_hazards: false,
            ..Default::default()
        };
        let out = jas::assemble(src, &opts);
        assert_eq!(out.errors(), 0, "assembly errors: {:#?}", out.diags);
        out.object(org)
    }

    #[test]
    fn resolves_cross_object_movei() {
        // Object A (at $F03000) exports `target`. Object B (at $F03400)
        // loads its address via `movei #target` and stores it — if the
        // relocation resolved, the stored value is $F03000.
        let a = asm_obj(
            "        .gpu\n        .globl target\ntarget:\n        nop\n",
            0xF03000,
        );
        let b = asm_obj(
            "        .gpu\n        .extern target\n\
             \x20       movei #target,r1\n\
             \x20       movei #$00100000,r2\n\
             \x20       store r1,(r2)\n\
             \x20       movei #$00F02114,r3\n\
             \x20       moveq #0,r4\n\
             \x20       store r4,(r3)\n        nop\n",
            0xF03400,
        );
        let img = link(&[a, b]).expect("links");
        assert_eq!(img.symbols.get("target"), Some(&0xF03000));

        // load the image and run object B in jsim
        let mut bus = Bus::new();
        for (i, byte) in img.bytes.iter().enumerate() {
            bus.write8(img.base + i as u32, *byte);
        }
        bus.write32(mem::G_PC, 0xF03400);
        bus.write32(mem::G_CTRL, mem::RISCGO);
        let mut gpu = Risc::new(RiscKind::Gpu);
        gpu.fidelity = Fidelity::Silicon;
        gpu.run(&mut bus, 5000);
        // B stored target's resolved address
        assert_eq!(bus.read32(0x0010_0000), 0xF03000);
    }

    #[test]
    fn undefined_reference_errors() {
        let b = asm_obj(
            "        .gpu\n        .extern nowhere\n        movei #nowhere,r1\n        nop\n",
            0xF03000,
        );
        assert!(matches!(link(&[b]), Err(LinkError::Undefined { .. })));
    }

    #[test]
    fn duplicate_global_errors() {
        let a = asm_obj("        .gpu\n        .globl dup\ndup:\n        nop\n", 0xF03000);
        let b = asm_obj("        .gpu\n        .globl dup\ndup:\n        nop\n", 0xF03400);
        assert!(matches!(link(&[a, b]), Err(LinkError::DuplicateSymbol(_))));
    }
}
