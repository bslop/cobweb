//! Relocatable object format — the contract between jas and jln.
//!
//! A `.jo` object holds the assembled bytes, the symbols it defines (and which
//! are exported via `.globl`), and the relocations it needs the linker to patch
//! (a `movei #symbol` or `.long symbol` referencing something defined in
//! another object). Serialization is a small hand-rolled little-endian format —
//! std-only, no external crates — so the whole toolchain stays dependency-free.

/// What a relocation patches.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RelKind {
    /// MOVEI immediate: two 16-bit half-words, low-word-first, at `offset+2`
    /// (offset points at the opcode word).
    Movei,
    /// 32-bit big-endian longword at `offset`.
    Long,
    /// 16-bit big-endian word at `offset`.
    Word,
}

impl RelKind {
    fn tag(self) -> u8 {
        match self {
            RelKind::Movei => 0,
            RelKind::Long => 1,
            RelKind::Word => 2,
        }
    }
    fn from_tag(t: u8) -> Option<Self> {
        Some(match t {
            0 => RelKind::Movei,
            1 => RelKind::Long,
            2 => RelKind::Word,
            _ => return None,
        })
    }
}

/// A fixup the linker resolves: `value(symbol) + addend` written at `offset`.
#[derive(Debug, Clone)]
pub struct Reloc {
    pub offset: u32,
    pub kind: RelKind,
    pub symbol: String,
    pub addend: i64,
}

/// A symbol this object defines. `global` = exported (`.globl`).
#[derive(Debug, Clone)]
pub struct Symbol {
    pub name: String,
    pub value: u32,
    pub global: bool,
}

/// A relocatable object.
#[derive(Debug, Clone, Default)]
pub struct Object {
    /// Origin this object was assembled at (advisory; the linker places it).
    pub org: u32,
    pub bytes: Vec<u8>,
    pub symbols: Vec<Symbol>,
    pub relocs: Vec<Reloc>,
}

const MAGIC: &[u8; 4] = b"JOB1";

fn put_u32(o: &mut Vec<u8>, v: u32) {
    o.extend_from_slice(&v.to_le_bytes());
}
fn put_i64(o: &mut Vec<u8>, v: i64) {
    o.extend_from_slice(&v.to_le_bytes());
}
fn put_str(o: &mut Vec<u8>, s: &str) {
    put_u32(o, s.len() as u32);
    o.extend_from_slice(s.as_bytes());
}

struct Rd<'a> {
    b: &'a [u8],
    i: usize,
}
impl Rd<'_> {
    fn u32(&mut self) -> Option<u32> {
        let v = u32::from_le_bytes(self.b.get(self.i..self.i + 4)?.try_into().ok()?);
        self.i += 4;
        Some(v)
    }
    fn i64(&mut self) -> Option<i64> {
        let v = i64::from_le_bytes(self.b.get(self.i..self.i + 8)?.try_into().ok()?);
        self.i += 8;
        Some(v)
    }
    fn u8(&mut self) -> Option<u8> {
        let v = *self.b.get(self.i)?;
        self.i += 1;
        Some(v)
    }
    fn take(&mut self, n: usize) -> Option<&[u8]> {
        let s = self.b.get(self.i..self.i + n)?;
        self.i += n;
        Some(s)
    }
    fn string(&mut self) -> Option<String> {
        let n = self.u32()? as usize;
        Some(String::from_utf8_lossy(self.take(n)?).into_owned())
    }
}

impl Object {
    /// Serialize to the `.jo` byte format.
    pub fn serialize(&self) -> Vec<u8> {
        let mut o = Vec::new();
        o.extend_from_slice(MAGIC);
        put_u32(&mut o, self.org);
        put_u32(&mut o, self.bytes.len() as u32);
        o.extend_from_slice(&self.bytes);
        put_u32(&mut o, self.symbols.len() as u32);
        for s in &self.symbols {
            put_str(&mut o, &s.name);
            put_u32(&mut o, s.value);
            o.push(s.global as u8);
        }
        put_u32(&mut o, self.relocs.len() as u32);
        for r in &self.relocs {
            put_u32(&mut o, r.offset);
            o.push(r.kind.tag());
            put_i64(&mut o, r.addend);
            put_str(&mut o, &r.symbol);
        }
        o
    }

    /// Parse the `.jo` byte format.
    pub fn deserialize(data: &[u8]) -> Option<Object> {
        let mut r = Rd { b: data, i: 0 };
        if r.take(4)? != MAGIC {
            return None;
        }
        let org = r.u32()?;
        let nb = r.u32()? as usize;
        let bytes = r.take(nb)?.to_vec();
        let ns = r.u32()? as usize;
        let mut symbols = Vec::with_capacity(ns);
        for _ in 0..ns {
            let name = r.string()?;
            let value = r.u32()?;
            let global = r.u8()? != 0;
            symbols.push(Symbol { name, value, global });
        }
        let nr = r.u32()? as usize;
        let mut relocs = Vec::with_capacity(nr);
        for _ in 0..nr {
            let offset = r.u32()?;
            let kind = RelKind::from_tag(r.u8()?)?;
            let addend = r.i64()?;
            let symbol = r.string()?;
            relocs.push(Reloc { offset, kind, symbol, addend });
        }
        Some(Object { org, bytes, symbols, relocs })
    }
}
