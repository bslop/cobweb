//! ELF32 big-endian m68k relocatable-object writer — `jas --elf-obj`.
//!
//! This is the GNU-interop unlock from COBWEB_REQ_jcc68k_adoption item 1: a
//! `.o` GNU `ld` accepts, so a project whose image is laid out by a GNU linker
//! script (`jaguar.ld` memory regions, `.incbin` data objects, gcc-built
//! translation units) can migrate to jas/jcc68k one translation unit at a
//! time instead of porting its whole link to `jln` in one jump.
//!
//! The assembled blob is carved into `.text`/`.data`/`.bss` using the section
//! marks the assembler records; symbols become section-relative, and the
//! relocations map onto the standard m68k kinds (RELA):
//!
//! * `RelKind::Long` → `R_68K_32`   (absolute longword)
//! * `RelKind::Word` → `R_68K_16`   (absolute word, abs.w)
//! * `RelKind::Pc16` → `R_68K_PC16` (68k word-branch displacement)
//! * `RelKind::Movei` has no ELF representation (the swapped-halfword JRISC
//!   immediate): JRISC code destined for a GNU link ships as an `.incbin`
//!   blob or links with `jln` — attempting it here is a clear error.
//!
//! std-only, hand-rolled writer, like the rest of the toolchain.

use crate::object::RelKind;
use crate::{Assembled, Section};

const SHT_PROGBITS: u32 = 1;
const SHT_SYMTAB: u32 = 2;
const SHT_STRTAB: u32 = 3;
const SHT_RELA: u32 = 4;
const SHT_NOBITS: u32 = 8;

const SHF_WRITE: u32 = 1;
const SHF_ALLOC: u32 = 2;
const SHF_EXECINSTR: u32 = 4;

const SHN_UNDEF: u16 = 0;
const SHN_ABS: u16 = 0xFFF1;

const STB_GLOBAL: u8 = 1;

const R_68K_32: u8 = 1;
const R_68K_16: u8 = 2;
const R_68K_PC16: u8 = 5;

const EM_68K: u16 = 4;
const ET_REL: u16 = 1;
/// e_flags: plain 68000 (matches what `m68k-elf-gcc -m68000` stamps).
const EF_M68K_M68000: u32 = 0x0100_0000;

// section-header table indices (fixed layout)
const SH_TEXT: u16 = 1;
const SH_RELA_TEXT: u16 = 2;
const SH_DATA: u16 = 3;
const SH_RELA_DATA: u16 = 4;
const SH_BSS: u16 = 5;
const SH_SYMTAB: u16 = 6;
const SH_STRTAB: u16 = 7;
const SH_SHSTRTAB: u16 = 8;
const SH_COUNT: u16 = 9;

/// One carved span: which section, and its [start, end) byte range in the blob.
struct Span {
    sec: Section,
    start: u32,
    end: u32,
}

/// Carve the blob into at most one span per section, in blob order. The
/// assembler merges consecutive same-section marks; a section *re-entered*
/// after another section has intervened cannot be represented (concatenating
/// its pieces would change intra-section distances the assembler already
/// resolved), so it is an error.
fn carve(out: &Assembled) -> Result<Vec<Span>, String> {
    let total = out.bytes.len() as u32;
    let mut spans: Vec<Span> = Vec::new();
    for (i, &(sec, start)) in out.sections.iter().enumerate() {
        let end = out.sections.get(i + 1).map(|&(_, o)| o).unwrap_or(total);
        if end == start {
            continue;
        }
        if spans.iter().any(|s| s.sec == sec) {
            return Err(format!(
                "section `{}` is re-entered after another section — --elf-obj needs each \
                 section contiguous (emit all of {} in one run)",
                sec.name(),
                sec.name()
            ));
        }
        spans.push(Span { sec, start, end });
    }
    Ok(spans)
}

fn find_span<'a>(spans: &'a [Span], off: u32, total: u32) -> Option<&'a Span> {
    spans
        .iter()
        .find(|s| off >= s.start && off < s.end)
        .or_else(|| (off == total).then(|| spans.last()).flatten())
}

/// Serialize `out` (assembled with `object_mode + relocatable`) as an ELF32
/// big-endian m68k relocatable object.
pub fn write(out: &Assembled) -> Result<Vec<u8>, String> {
    let org = out.org;
    let total = out.bytes.len() as u32;
    let spans = carve(out)?;
    let span_of = |sec: Section| spans.iter().find(|s| s.sec == sec);
    let sec_size = |sec: Section| span_of(sec).map(|s| s.end - s.start).unwrap_or(0);
    let shndx = |sec: Section| match sec {
        Section::Text => SH_TEXT,
        Section::Data => SH_DATA,
        Section::Bss => SH_BSS,
    };

    // ── symbol table ─────────────────────────────────────────────────────────
    // Locals first (ELF requires it; sh_info = first global index). Every
    // defined symbol goes in: relocatable references may name any label.
    let is_global = |n: &str| out.globals.iter().any(|g| g == n);
    let mut defined: Vec<(&String, &u32)> = out.symbols.iter().collect();
    defined.sort_by(|a, b| (is_global(a.0), a.0).cmp(&(is_global(b.0), b.0)));

    // Undefined symbols: anything a relocation names that we don't define.
    let mut undef: Vec<&String> = out
        .relocs
        .iter()
        .map(|r| &r.symbol)
        .filter(|s| !out.symbols.contains_key(*s))
        .collect();
    undef.sort();
    undef.dedup();

    let mut strtab: Vec<u8> = vec![0];
    let mut sym_bytes: Vec<u8> = vec![0; 16]; // entry 0: null symbol
    let mut sym_index: std::collections::HashMap<String, u32> = std::collections::HashMap::new();
    let mut first_global = 1u32;

    let push_sym = |strtab: &mut Vec<u8>,
                    sym_bytes: &mut Vec<u8>,
                    sym_index: &mut std::collections::HashMap<String, u32>,
                    name: &str,
                    value: u32,
                    info: u8,
                    sh: u16|
     -> u32 {
        let name_off = strtab.len() as u32;
        strtab.extend_from_slice(name.as_bytes());
        strtab.push(0);
        let idx = (sym_bytes.len() / 16) as u32;
        sym_bytes.extend_from_slice(&name_off.to_be_bytes());
        sym_bytes.extend_from_slice(&value.to_be_bytes());
        sym_bytes.extend_from_slice(&0u32.to_be_bytes()); // st_size
        sym_bytes.push(info); // st_info (bind<<4 | type)
        sym_bytes.push(0); // st_other
        sym_bytes.extend_from_slice(&sh.to_be_bytes());
        sym_index.insert(name.to_string(), idx);
        idx
    };

    for &(name, &value) in &defined {
        let global = is_global(name);
        let (sval, sh) = if out.label_syms.contains(name) {
            let off = value.wrapping_sub(org);
            match find_span(&spans, off, total) {
                Some(sp) => (off - sp.start, shndx(sp.sec)),
                None => (value, SHN_ABS), // label outside the blob (org games)
            }
        } else {
            (value, SHN_ABS) // equ constant
        };
        let info = if global { STB_GLOBAL << 4 } else { 0 };
        let idx = push_sym(&mut strtab, &mut sym_bytes, &mut sym_index, name, sval, info, sh);
        if !global {
            first_global = idx + 1;
        }
    }
    for name in &undef {
        push_sym(&mut strtab, &mut sym_bytes, &mut sym_index, name, 0, STB_GLOBAL << 4, SHN_UNDEF);
    }

    // ── relocations, split per target section ────────────────────────────────
    let mut rela_text: Vec<u8> = Vec::new();
    let mut rela_data: Vec<u8> = Vec::new();
    for r in &out.relocs {
        let sp = find_span(&spans, r.offset, total)
            .ok_or_else(|| format!("relocation at offset {} outside any section", r.offset))?;
        let (buf, patch_off) = match sp.sec {
            Section::Text => (&mut rela_text, r.offset - sp.start),
            Section::Data => (&mut rela_data, r.offset - sp.start),
            Section::Bss => return Err("relocation inside .bss".into()),
        };
        let (ty, off) = match r.kind {
            RelKind::Long => (R_68K_32, patch_off),
            RelKind::Word => (R_68K_16, patch_off),
            RelKind::Pc16 => (R_68K_PC16, patch_off),
            RelKind::Movei => {
                return Err(format!(
                    "JRISC MOVEI relocation of `{}` has no ELF representation — keep JRISC \
                     code as an .incbin blob in the GNU link, or link it with jln",
                    r.symbol
                ));
            }
        };
        let sym = *sym_index
            .get(&r.symbol)
            .ok_or_else(|| format!("relocation names unknown symbol `{}`", r.symbol))?;
        buf.extend_from_slice(&off.to_be_bytes());
        buf.extend_from_slice(&((sym << 8) | ty as u32).to_be_bytes());
        buf.extend_from_slice(&(r.addend as i32).to_be_bytes());
    }

    // ── section-header string table ──────────────────────────────────────────
    let mut shstrtab: Vec<u8> = vec![0];
    let mut shname = |s: &str, shstrtab: &mut Vec<u8>| -> u32 {
        let off = shstrtab.len() as u32;
        shstrtab.extend_from_slice(s.as_bytes());
        shstrtab.push(0);
        off
    };
    let n_text = shname(".text", &mut shstrtab);
    let n_rela_text = shname(".rela.text", &mut shstrtab);
    let n_data = shname(".data", &mut shstrtab);
    let n_rela_data = shname(".rela.data", &mut shstrtab);
    let n_bss = shname(".bss", &mut shstrtab);
    let n_symtab = shname(".symtab", &mut shstrtab);
    let n_strtab = shname(".strtab", &mut shstrtab);
    let n_shstrtab = shname(".shstrtab", &mut shstrtab);

    // ── file layout ──────────────────────────────────────────────────────────
    let align4 = |v: usize| (v + 3) & !3;
    let ehsize = 52usize;
    let text_bytes = span_of(Section::Text).map(|s| &out.bytes[s.start as usize..s.end as usize]);
    let data_bytes = span_of(Section::Data).map(|s| &out.bytes[s.start as usize..s.end as usize]);

    let off_text = ehsize;
    let off_data = align4(off_text + text_bytes.map_or(0, |b| b.len()));
    let off_rela_text = align4(off_data + data_bytes.map_or(0, |b| b.len()));
    let off_rela_data = align4(off_rela_text + rela_text.len());
    let off_symtab = align4(off_rela_data + rela_data.len());
    let off_strtab = off_symtab + sym_bytes.len();
    let off_shstrtab = off_strtab + strtab.len();
    let off_sh = align4(off_shstrtab + shstrtab.len());

    let mut f: Vec<u8> = Vec::with_capacity(off_sh + SH_COUNT as usize * 40);
    // ELF header
    f.extend_from_slice(&[0x7F, b'E', b'L', b'F', 1, 2, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
    f.extend_from_slice(&ET_REL.to_be_bytes());
    f.extend_from_slice(&EM_68K.to_be_bytes());
    f.extend_from_slice(&1u32.to_be_bytes()); // e_version
    f.extend_from_slice(&0u32.to_be_bytes()); // e_entry
    f.extend_from_slice(&0u32.to_be_bytes()); // e_phoff
    f.extend_from_slice(&(off_sh as u32).to_be_bytes()); // e_shoff
    f.extend_from_slice(&EF_M68K_M68000.to_be_bytes()); // e_flags
    f.extend_from_slice(&(ehsize as u16).to_be_bytes()); // e_ehsize
    f.extend_from_slice(&0u16.to_be_bytes()); // e_phentsize
    f.extend_from_slice(&0u16.to_be_bytes()); // e_phnum
    f.extend_from_slice(&40u16.to_be_bytes()); // e_shentsize
    f.extend_from_slice(&SH_COUNT.to_be_bytes()); // e_shnum
    f.extend_from_slice(&SH_SHSTRTAB.to_be_bytes()); // e_shstrndx

    let pad_to = |f: &mut Vec<u8>, off: usize| f.resize(off, 0);
    if let Some(b) = text_bytes {
        f.extend_from_slice(b);
    }
    pad_to(&mut f, off_data);
    if let Some(b) = data_bytes {
        f.extend_from_slice(b);
    }
    pad_to(&mut f, off_rela_text);
    f.extend_from_slice(&rela_text);
    pad_to(&mut f, off_rela_data);
    f.extend_from_slice(&rela_data);
    pad_to(&mut f, off_symtab);
    f.extend_from_slice(&sym_bytes);
    f.extend_from_slice(&strtab);
    f.extend_from_slice(&shstrtab);
    pad_to(&mut f, off_sh);

    // ── section headers ──────────────────────────────────────────────────────
    let mut sh = |f: &mut Vec<u8>,
                  name: u32,
                  ty: u32,
                  flags: u32,
                  off: usize,
                  size: usize,
                  link: u32,
                  info: u32,
                  addralign: u32,
                  entsize: u32| {
        f.extend_from_slice(&name.to_be_bytes());
        f.extend_from_slice(&ty.to_be_bytes());
        f.extend_from_slice(&flags.to_be_bytes());
        f.extend_from_slice(&0u32.to_be_bytes()); // sh_addr
        f.extend_from_slice(&(off as u32).to_be_bytes());
        f.extend_from_slice(&(size as u32).to_be_bytes());
        f.extend_from_slice(&link.to_be_bytes());
        f.extend_from_slice(&info.to_be_bytes());
        f.extend_from_slice(&addralign.to_be_bytes());
        f.extend_from_slice(&entsize.to_be_bytes());
    };
    // 0: null
    sh(&mut f, 0, 0, 0, 0, 0, 0, 0, 0, 0);
    // 1: .text
    sh(
        &mut f,
        n_text,
        SHT_PROGBITS,
        SHF_ALLOC | SHF_EXECINSTR,
        off_text,
        sec_size(Section::Text) as usize,
        0,
        0,
        8,
        0,
    );
    // 2: .rela.text
    sh(
        &mut f,
        n_rela_text,
        SHT_RELA,
        0,
        off_rela_text,
        rela_text.len(),
        SH_SYMTAB as u32,
        SH_TEXT as u32,
        4,
        12,
    );
    // 3: .data
    sh(
        &mut f,
        n_data,
        SHT_PROGBITS,
        SHF_ALLOC | SHF_WRITE,
        off_data,
        sec_size(Section::Data) as usize,
        0,
        0,
        8,
        0,
    );
    // 4: .rela.data
    sh(
        &mut f,
        n_rela_data,
        SHT_RELA,
        0,
        off_rela_data,
        rela_data.len(),
        SH_SYMTAB as u32,
        SH_DATA as u32,
        4,
        12,
    );
    // 5: .bss (no file content)
    sh(
        &mut f,
        n_bss,
        SHT_NOBITS,
        SHF_ALLOC | SHF_WRITE,
        0,
        sec_size(Section::Bss) as usize,
        0,
        0,
        8,
        0,
    );
    // 6: .symtab
    sh(
        &mut f,
        n_symtab,
        SHT_SYMTAB,
        0,
        off_symtab,
        sym_bytes.len(),
        SH_STRTAB as u32,
        first_global,
        4,
        16,
    );
    // 7: .strtab
    sh(&mut f, n_strtab, SHT_STRTAB, 0, off_strtab, strtab.len(), 0, 0, 1, 0);
    // 8: .shstrtab
    sh(&mut f, n_shstrtab, SHT_STRTAB, 0, off_shstrtab, shstrtab.len(), 0, 0, 1, 0);

    Ok(f)
}
