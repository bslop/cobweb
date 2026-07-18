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
    /// Address the image loads at (the lowest object org).
    pub base: u32,
    pub bytes: Vec<u8>,
    /// Every global symbol and its final address.
    pub symbols: HashMap<String, u32>,
    /// Entry point: the lowest object's org (best-effort; overrideable).
    pub entry: u32,
}

/// Parse `.jo` bytes into objects.
pub fn parse_objects(blobs: &[Vec<u8>]) -> Result<Vec<Object>, LinkError> {
    blobs
        .iter()
        .enumerate()
        .map(|(i, b)| Object::deserialize(b).ok_or_else(|| LinkError::BadObject(format!("#{i}"))))
        .collect()
}

/// Link objects into an image. Global symbols must be unique; every reloc must
/// resolve to a defined global (or a symbol local to some object).
pub fn link(objects: &[Object]) -> Result<Image, LinkError> {
    // 1. global symbol table (globals are exported; also index all defined
    //    symbols so intra-project references that happen to be non-global still
    //    resolve — pragmatic for hand-written Jaguar code).
    let mut globals: HashMap<String, u32> = HashMap::new();
    for obj in objects {
        for s in &obj.symbols {
            if s.global {
                if globals.insert(s.name.clone(), s.value).is_some() {
                    return Err(LinkError::DuplicateSymbol(s.name.clone()));
                }
            }
        }
    }
    // second-chance table: all defined symbols (non-global included), for
    // resolving references the author didn't bother to .globl.
    let mut anydef: HashMap<String, u32> = HashMap::new();
    for obj in objects {
        for s in &obj.symbols {
            anydef.entry(s.name.clone()).or_insert(s.value);
        }
    }

    // 2. patch relocations (work on owned copies of the bytes)
    let mut patched: Vec<Object> = objects.to_vec();
    for (oi, obj) in patched.iter_mut().enumerate() {
        for r in &obj.relocs {
            let target = globals
                .get(&r.symbol)
                .or_else(|| anydef.get(&r.symbol))
                .copied()
                .ok_or_else(|| LinkError::Undefined { symbol: r.symbol.clone(), in_obj: oi })?;
            let val = (target as i64 + r.addend) as u32;
            let off = r.offset as usize;
            match r.kind {
                RelKind::Movei => {
                    // low half-word then high half-word, each big-endian
                    let lo = (val & 0xFFFF) as u16;
                    let hi = (val >> 16) as u16;
                    put_be16(&mut obj.bytes, off, lo)?;
                    put_be16(&mut obj.bytes, off + 2, hi)?;
                }
                RelKind::Long => {
                    put_be16(&mut obj.bytes, off, (val >> 16) as u16)?;
                    put_be16(&mut obj.bytes, off + 2, (val & 0xFFFF) as u16)?;
                }
                RelKind::Word => put_be16(&mut obj.bytes, off, (val & 0xFFFF) as u16)?,
            }
        }
    }

    // 3. lay out at each object's org into one sparse image
    let base = patched.iter().map(|o| o.org).min().unwrap_or(0);
    let end = patched.iter().map(|o| o.org + o.bytes.len() as u32).max().unwrap_or(base);
    let mut bytes = vec![0u8; (end - base) as usize];
    for obj in &patched {
        let at = (obj.org - base) as usize;
        bytes[at..at + obj.bytes.len()].copy_from_slice(&obj.bytes);
    }

    Ok(Image { base, bytes, symbols: globals, entry: base })
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
