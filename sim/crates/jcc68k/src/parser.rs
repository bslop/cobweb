//! Recursive-descent C parser. Produces a typed AST. Variables are resolved to
//! unique names during parsing (so shadowing across scopes just works), and
//! expressions are typed as they are built (pointer arithmetic scaled, arrays
//! decayed, usual arithmetic conversions applied at codegen).

use crate::ast::*;
use crate::lexer::{Tok, Token};
use std::collections::HashMap;
use std::rc::Rc;

pub struct Parser {
    toks: Vec<Token>,
    /// `__attribute__((aligned(N)))` per source declarator name.
    aligns: HashMap<String, u32>,
    pos: usize,
    // scope stack: source name -> resolved (unique_name_or_global, type, is_global)
    scopes: Vec<HashMap<String, VarRef>>,
    typedefs: Vec<HashMap<String, Type>>,
    structs: HashMap<String, Type>,
    // current function's locals (unique names)
    cur_locals: Vec<(String, Type, bool)>, // (unique name, type, is_volatile)
    uid: usize,
    globals: Vec<Global>,
    functions: Vec<Function>,
    strings: Vec<Vec<u8>>,
    /// Names captured by the most recent function declarator (a Type can't carry
    /// parameter names, so they're stashed here for `function()` to pick up).
    pending_params: Vec<String>,
    /// enum constant name → value, visible as integer constants.
    enum_consts: HashMap<String, i64>,
    /// Stack of switch statements being parsed, collecting their case labels.
    cur_switch: Vec<SwitchBuild>,
}

#[derive(Default)]
struct SwitchBuild {
    cases: Vec<(i64, u32)>,
    default: Option<u32>,
}

#[derive(Clone)]
struct VarRef {
    name: String, // unique (locals) or plain (globals)
    ty: Type,
    is_global: bool,
}

type PResult<T> = Result<T, String>;

/// Normalize GNU C extensions the parser doesn't model but must not choke on:
/// drop `__attribute__((…))` (capturing `aligned(N)` per declarator),
/// `__extension__`, and `__restrict`; fold `__inline__`/`__const__`/
/// `__volatile__`/`__signed__` to their keywords. Inline-asm *statements* are
/// passed through for the parser to handle — dropping them silently deleted
/// interrupt enables and STOP sleeps (adoption report round 2, item 1); only
/// the asm-*label* form (`TYPE name __asm__("link")`) is consumed here.
fn strip_gnu(toks: Vec<Token>) -> (Vec<Token>, HashMap<String, u32>) {
    use std::collections::HashMap;
    let mut out: Vec<Token> = Vec::with_capacity(toks.len());
    let mut i = 0;
    let kw = |s: &str, t: &Token| {
        Token { tok: Tok::Keyword(s.into()), line: t.line, col: t.col, file: t.file.clone() }
    };
    // GCC asm labels: `TYPE name __asm__("linkname")` renames `name`'s linkage
    // symbol to `linkname`. Collected here, applied to every use in a second pass.
    let mut renames: HashMap<String, String> = HashMap::new();
    // `__attribute__((aligned(N)))` per declarator name (postfix position).
    let mut aligns: HashMap<String, u32> = HashMap::new();
    while i < toks.len() {
        if let Tok::Ident(s) = &toks[i].tok {
            match s.as_str() {
                // An `asm`/`__asm__("linkname")` following a declarator (`name`,
                // `name(params)`, or `name[]`) is an asm label — a symbol rename,
                // not an inline-asm statement. Record the rename and drop it.
                "__asm__" | "__asm" | "asm"
                    if matches!(toks.get(i + 1).map(|t| &t.tok), Some(Tok::Punct(p)) if p == "(")
                        && matches!(toks.get(i + 2).map(|t| &t.tok), Some(Tok::Str(_)))
                        && matches!(toks.get(i + 3).map(|t| &t.tok), Some(Tok::Punct(p)) if p == ")")
                        && declarator_name(&out).is_some() =>
                {
                    if let (Some(cname), Some(Tok::Str(bytes))) =
                        (declarator_name(&out), toks.get(i + 2).map(|t| t.tok.clone()))
                    {
                        let link: String =
                            bytes.iter().take_while(|&&b| b != 0).map(|&b| b as char).collect();
                        renames.insert(cname, link);
                    }
                    i += 4; // skip `asm ( "str" )`
                    continue;
                }
                "__attribute__" | "__attribute" => {
                    i += 1;
                    if matches!(toks.get(i).map(|t| &t.tok), Some(Tok::Punct(p)) if p == "(") {
                        let start = i;
                        let mut depth = 0i32;
                        while i < toks.len() {
                            match &toks[i].tok {
                                Tok::Punct(p) if p == "(" => depth += 1,
                                Tok::Punct(p) if p == ")" => {
                                    depth -= 1;
                                    i += 1;
                                    if depth == 0 {
                                        break;
                                    }
                                    continue;
                                }
                                _ => {}
                            }
                            i += 1;
                        }
                        // Capture `aligned(N)` for the declarator this attribute
                        // trails (GPU-shared buffers depend on it).
                        let mut j = start;
                        while j < i {
                            if matches!(&toks[j].tok, Tok::Ident(a) if a == "aligned" || a == "__aligned__") {
                                if let (Some(Tok::Punct(o)), Some(Tok::Num(n))) = (
                                    toks.get(j + 1).map(|t| &t.tok),
                                    toks.get(j + 2).map(|t| &t.tok),
                                ) {
                                    if o == "(" && *n > 0 {
                                        if let Some(name) = declarator_name(&out) {
                                            aligns.insert(name, *n as u32);
                                        }
                                    }
                                }
                            }
                            j += 1;
                        }
                    }
                    continue;
                }
                "__extension__" | "__restrict__" | "__restrict" | "restrict" => {
                    i += 1;
                    continue;
                }
                "__inline__" | "__inline" | "__forceinline" => {
                    out.push(kw("inline", &toks[i]));
                    i += 1;
                    continue;
                }
                "__const__" | "__const" => {
                    out.push(kw("const", &toks[i]));
                    i += 1;
                    continue;
                }
                "__volatile__" | "__volatile" => {
                    out.push(kw("volatile", &toks[i]));
                    i += 1;
                    continue;
                }
                "__signed__" => {
                    out.push(kw("signed", &toks[i]));
                    i += 1;
                    continue;
                }
                _ => {}
            }
        }
        out.push(toks[i].clone());
        i += 1;
    }
    // Apply asm-label renames to every identifier use (the C name links under
    // the asm name, so a call/reference must emit the asm symbol).
    if !renames.is_empty() {
        for t in &mut out {
            if let Tok::Ident(name) = &t.tok {
                if let Some(link) = renames.get(name) {
                    t.tok = Tok::Ident(link.clone());
                }
            }
        }
    }
    (out, aligns)
}

/// The declarator name preceding a trailing asm label: skip any balanced
/// `(params)` / `[dims]` declarator suffixes, then return the identifier.
fn declarator_name(out: &[Token]) -> Option<String> {
    let mut j = out.len();
    loop {
        let last = out.get(j.checked_sub(1)?)?;
        match &last.tok {
            Tok::Punct(p) if p == ")" || p == "]" => {
                let (close, open) = if p == ")" { (")", "(") } else { ("]", "[") };
                let mut depth = 0i32;
                loop {
                    j = j.checked_sub(1)?;
                    match &out.get(j)?.tok {
                        Tok::Punct(x) if x == close => depth += 1,
                        Tok::Punct(x) if x == open => {
                            depth -= 1;
                            if depth == 0 {
                                break;
                            }
                        }
                        _ => {}
                    }
                }
            }
            Tok::Ident(name) => return Some(name.clone()),
            _ => return None,
        }
    }
}

pub fn parse(toks: Vec<Token>) -> PResult<Program> {
    let (toks, aligns) = strip_gnu(toks);
    let mut p = Parser {
        toks,
        aligns,
        pos: 0,
        scopes: vec![HashMap::new()],
        typedefs: vec![HashMap::new()],
        structs: HashMap::new(),
        cur_locals: Vec::new(),
        uid: 0,
        globals: Vec::new(),
        functions: Vec::new(),
        strings: Vec::new(),
        pending_params: Vec::new(),
        enum_consts: HashMap::new(),
        cur_switch: Vec::new(),
    };
    p.program()?;
    Ok(Program { functions: p.functions, globals: p.globals, strings: p.strings })
}

impl Parser {
    // ── token helpers ───────────────────────────────────────────────────────
    fn peek(&self) -> &Tok {
        &self.toks[self.pos].tok
    }
    fn line(&self) -> usize {
        self.toks[self.pos].line
    }
    /// Source position for diagnostics: `file:line` when the preprocessor's
    /// line markers named the file, bare `line` otherwise.
    fn loc(&self) -> String {
        let t = &self.toks[self.pos];
        if t.file.is_empty() {
            t.line.to_string()
        } else {
            format!("{}:{}", t.file, t.line)
        }
    }
    fn at_punct(&self, s: &str) -> bool {
        matches!(self.peek(), Tok::Punct(p) if p == s)
    }
    fn at_kw(&self, s: &str) -> bool {
        matches!(self.peek(), Tok::Keyword(k) if k == s)
    }
    fn eat_punct(&mut self, s: &str) -> bool {
        if self.at_punct(s) {
            self.pos += 1;
            true
        } else {
            false
        }
    }
    fn eat_kw(&mut self, s: &str) -> bool {
        if self.at_kw(s) {
            self.pos += 1;
            true
        } else {
            false
        }
    }
    fn expect(&mut self, s: &str) -> PResult<()> {
        if self.eat_punct(s) {
            Ok(())
        } else {
            Err(format!("{}: expected '{}', found {:?}", self.loc(), s, self.peek()))
        }
    }
    fn ident(&mut self) -> PResult<String> {
        match self.peek().clone() {
            Tok::Ident(s) => {
                self.pos += 1;
                Ok(s)
            }
            other => Err(format!("{}: expected identifier, found {:?}", self.loc(), other)),
        }
    }

    fn push_scope(&mut self) {
        self.scopes.push(HashMap::new());
        self.typedefs.push(HashMap::new());
    }
    fn pop_scope(&mut self) {
        self.scopes.pop();
        self.typedefs.pop();
    }
    fn new_local(&mut self, name: &str, ty: Type, is_volatile: bool) -> String {
        self.uid += 1;
        let uniq = format!("{name}${}", self.uid);
        self.cur_locals.push((uniq.clone(), ty.clone(), is_volatile));
        self.scopes
            .last_mut()
            .unwrap()
            .insert(name.to_string(), VarRef { name: uniq.clone(), ty, is_global: false });
        uniq
    }
    fn add_global_scope(&mut self, name: &str, ty: Type) {
        self.scopes
            .first_mut()
            .unwrap()
            .insert(name.to_string(), VarRef { name: name.to_string(), ty, is_global: true });
    }
    fn resolve(&self, name: &str) -> Option<VarRef> {
        for s in self.scopes.iter().rev() {
            if let Some(v) = s.get(name) {
                return Some(v.clone());
            }
        }
        None
    }
    fn find_typedef(&self, name: &str) -> Option<Type> {
        for s in self.typedefs.iter().rev() {
            if let Some(t) = s.get(name) {
                return Some(t.clone());
            }
        }
        None
    }

    fn is_typename(&self) -> bool {
        match self.peek() {
            Tok::Keyword(k) => matches!(
                k.as_str(),
                "void" | "char" | "short" | "int" | "long" | "unsigned" | "signed"
                    | "struct" | "union" | "enum" | "float" | "double" | "_Bool"
                    | "const" | "volatile" | "static" | "extern" | "typedef" | "register"
                    | "inline"
            ),
            Tok::Ident(s) => self.find_typedef(s).is_some(),
            _ => false,
        }
    }

    // ── top level ───────────────────────────────────────────────────────────
    fn program(&mut self) -> PResult<()> {
        while !matches!(self.peek(), Tok::Eof) {
            self.top_level()?;
        }
        Ok(())
    }

    fn top_level(&mut self) -> PResult<()> {
        let (base, sc) = self.declspec()?;
        // typedef: `typedef <type> <name>;`
        if sc.typedef {
            loop {
                let (name, ty) = self.declarator(base.clone())?;
                self.typedefs.last_mut().unwrap().insert(name, ty);
                if !self.eat_punct(",") {
                    break;
                }
            }
            self.expect(";")?;
            return Ok(());
        }
        // bare `struct Foo { ... };`
        if self.at_punct(";") {
            self.pos += 1;
            return Ok(());
        }
        let (name, ty) = self.declarator(base.clone())?;
        if let TypeK::Func { .. } = &*ty {
            if self.at_punct("{") {
                return self.function(name, ty, sc);
            }
            // function prototype
            self.add_global_scope(&name, ty.clone());
            self.expect(";")?;
            return Ok(());
        }
        // global variable(s)
        self.global_var(name, ty, &sc)?;
        loop {
            if self.eat_punct(",") {
                let (n2, t2) = self.declarator(base.clone())?;
                self.global_var(n2, t2, &sc)?;
            } else {
                break;
            }
        }
        self.expect(";")?;
        Ok(())
    }

    fn global_var(&mut self, name: String, ty: Type, sc: &Storage) -> PResult<()> {
        self.add_global_scope(&name, ty.clone());
        let mut init = None;
        if self.eat_punct("=") {
            init = Some(self.global_initializer(&ty)?);
        }
        let align = self.aligns.get(&name).copied().unwrap_or(0);
        self.globals.push(Global {
            name,
            ty,
            init,
            is_static: sc.is_static,
            is_extern: sc.is_extern,
            align,
        });
        Ok(())
    }

    fn global_initializer(&mut self, ty: &Type) -> PResult<Vec<InitByte>> {
        let mut out = Vec::new();
        self.global_init_into(ty, &mut out)?;
        Ok(out)
    }

    /// Parse one initializer for `ty`, appending its big-endian image to `out`
    /// and padding to `ty.size()`. Handles nested braces (arrays/structs),
    /// string literals into char arrays, scalar constants, and address-of /
    /// bare-array-name pointer initializers (emitted as relocations).
    fn global_init_into(&mut self, ty: &Type, out: &mut Vec<InitByte>) -> PResult<()> {
        // `char buf[] = "…"` / `char buf[N] = "…"`.
        if let TypeK::Array(el, n) = &**ty {
            if el.size() == 1 {
                if let Tok::Str(bytes) = self.peek().clone() {
                    self.pos += 1;
                    let cap = if *n == 0 { bytes.len() as u32 } else { *n };
                    for i in 0..cap {
                        out.push(InitByte::Byte(*bytes.get(i as usize).unwrap_or(&0)));
                    }
                    return Ok(());
                }
            }
        }
        if self.eat_punct("{") {
            match &**ty {
                TypeK::Array(el, n) => {
                    let esz = el.size().max(1);
                    let mut count = 0u32;
                    while !self.at_punct("}") {
                        self.global_init_into(el, out)?;
                        count += 1;
                        if !self.eat_punct(",") {
                            break;
                        }
                    }
                    self.expect("}")?;
                    // zero-fill the remaining declared elements
                    let total = if *n == 0 { count } else { *n };
                    for _ in count..total {
                        for _ in 0..esz {
                            out.push(InitByte::Byte(0));
                        }
                    }
                }
                TypeK::Struct { members, size, .. } => {
                    let start = out.len();
                    let mut mi = 0usize;
                    while !self.at_punct("}") {
                        if mi >= members.len() {
                            return Err(format!("{}: too many struct initializers", self.loc()));
                        }
                        // pad to this member's offset
                        let target = start + members[mi].offset as usize;
                        while out.len() < target {
                            out.push(InitByte::Byte(0));
                        }
                        self.global_init_into(&members[mi].ty, out)?;
                        mi += 1;
                        if !self.eat_punct(",") {
                            break;
                        }
                    }
                    self.expect("}")?;
                    while out.len() < start + *size as usize {
                        out.push(InitByte::Byte(0));
                    }
                }
                _ => {
                    // scalar wrapped in braces: `int x = { 5 };`
                    self.global_init_into(ty, out)?;
                    self.eat_punct(",");
                    self.expect("}")?;
                }
            }
            return Ok(());
        }
        // Scalar. Try a symbol address first (pointer initializers), else a
        // constant expression.
        let sz = ty.size().max(1);
        if ty.is_ptr() {
            if let Some((sym, addend)) = self.try_global_addr()? {
                out.push(InitByte::Addr(sym, addend));
                return Ok(());
            }
        }
        let loc = self.loc();
        let e = self.assign()?;
        let v = const_eval(&e).map_err(|m| format!("{loc}: {m} in initializer"))?;
        for i in (0..sz).rev() {
            out.push(InitByte::Byte(((v >> (i * 8)) & 0xFF) as u8));
        }
        Ok(())
    }

    /// Recognize a pointer initializer that is the address of a global:
    /// a bare array/global name, `&global`, `&global[k]`, or `name + k`.
    /// Returns `(mangled-less symbol, byte addend)` or None if it isn't one.
    fn try_global_addr(&mut self) -> PResult<Option<(String, i64)>> {
        let save = self.pos;
        let took_amp = self.eat_punct("&");
        if let Tok::Ident(name) = self.peek().clone() {
            if let Some(v) = self.resolve(&name) {
                if v.is_global {
                    self.pos += 1;
                    let mut addend = 0i64;
                    // &global[k]  → addend = k * elem_size
                    if self.eat_punct("[") {
                        let idx = const_eval(&self.assign()?)?;
                        self.expect("]")?;
                        let esz = v.ty.base().map(|e| e.size()).unwrap_or(1) as i64;
                        addend += idx * esz;
                    }
                    // name + k / name - k  → addend scaled by pointee size
                    let scale = v.ty.base().map(|e| e.size().max(1)).unwrap_or(1) as i64;
                    loop {
                        if self.eat_punct("+") {
                            addend += const_eval(&self.assign()?)? * scale;
                        } else if self.eat_punct("-") {
                            addend -= const_eval(&self.assign()?)? * scale;
                        } else {
                            break;
                        }
                    }
                    return Ok(Some((v.name, addend)));
                }
            }
        }
        // not a symbol address — restore and let the caller const-eval it
        self.pos = save;
        let _ = took_amp;
        Ok(None)
    }

    fn function(&mut self, name: String, ty: Type, sc: Storage) -> PResult<()> {
        self.add_global_scope(&name, ty.clone());
        self.cur_locals.clear();
        self.push_scope();
        let (ret, params) = match &*ty {
            TypeK::Func { ret, params, .. } => (ret.clone(), params.clone()),
            _ => unreachable!(),
        };
        // Re-parse parameter names from the declarator scope: we stored param types
        // but need names; the declarator recorded them into `pending_params`.
        let param_names = std::mem::take(&mut self.pending_params);
        let mut param_locals = Vec::new();
        for (pname, pty) in param_names.iter().zip(params.iter()) {
            let uniq = self.new_local(pname, pty.clone(), false);
            param_locals.push((uniq, pty.clone()));
        }
        self.expect("{")?;
        let body = self.block_items()?;
        self.pop_scope();
        let locals = std::mem::take(&mut self.cur_locals);
        self.functions.push(Function {
            name,
            ret,
            params: param_locals,
            body,
            locals: locals
                .into_iter()
                .map(|(n, t, v)| Local { name: n, ty: t, offset: 0, is_volatile: v })
                .collect(),
            stack_size: 0,
            is_static: sc.is_static,
        });
        Ok(())
    }

    // ── declaration specifiers ────────────────────────────────────────────────
    fn declspec(&mut self) -> PResult<(Type, Storage)> {
        let mut sc = Storage::default();
        let mut signed: Option<bool> = None;
        let mut longs = 0;
        let mut shorts = 0;
        let mut base_kw: Option<&str> = None;
        let mut ty: Option<Type> = None;
        loop {
            match self.peek().clone() {
                Tok::Keyword(k) => match k.as_str() {
                    "typedef" => {
                        sc.typedef = true;
                        self.pos += 1;
                    }
                    "static" => {
                        sc.is_static = true;
                        self.pos += 1;
                    }
                    "extern" => {
                        sc.is_extern = true;
                        self.pos += 1;
                    }
                    "const" | "volatile" | "register" | "inline" | "signed" => {
                        if k == "signed" {
                            signed = Some(true);
                        }
                        if k == "volatile" {
                            sc.is_volatile = true;
                        }
                        self.pos += 1;
                    }
                    "unsigned" => {
                        signed = Some(false);
                        self.pos += 1;
                    }
                    "float" | "double" => {
                        // No FPU on the Jaguar: C floating types are 16.16 fixed.
                        ty = Some(t_fixed());
                        self.pos += 1;
                    }
                    "void" | "char" | "int" | "_Bool" => {
                        base_kw = Some(match k.as_str() {
                            "void" => "void",
                            "char" => "char",
                            "_Bool" => "char",
                            _ => "int",
                        });
                        self.pos += 1;
                    }
                    "short" => {
                        shorts += 1;
                        self.pos += 1;
                    }
                    "long" => {
                        longs += 1;
                        self.pos += 1;
                    }
                    "struct" | "union" => {
                        let is_union = k == "union";
                        self.pos += 1;
                        ty = Some(self.struct_decl(is_union)?);
                    }
                    "enum" => {
                        self.pos += 1;
                        ty = Some(self.enum_decl()?);
                    }
                    _ => break,
                },
                Tok::Ident(s) => {
                    if ty.is_none() && base_kw.is_none() && signed.is_none() && longs == 0 && shorts == 0 {
                        if let Some(t) = self.find_typedef(&s) {
                            ty = Some(t);
                            self.pos += 1;
                            continue;
                        }
                    }
                    break;
                }
                _ => break,
            }
        }
        // `long long` would be a 64-bit integer — silently sizing it at 32
        // bits made OpenLara's frustum cull overflow and discard every room
        // (a wrong-render, not even a crash). No 64-bit support on the 68000
        // yet, so this must be a hard error, never a silent wrap.
        if longs >= 2 {
            return Err(format!(
                "{}: `long long` (64-bit) is not supported on the 68000 target — \
                 restructure with 32-bit math (e.g. pre-shift operands) or 16.16 fix helpers",
                self.loc()
            ));
        }
        let final_ty = if let Some(t) = ty {
            t
        } else {
            let signed = signed.unwrap_or(true);
            match base_kw {
                Some("void") => t_void(),
                Some("char") => Rc::new(TypeK::Int { size: 1, signed: signed }),
                _ => {
                    let size = if shorts > 0 {
                        2
                    } else {
                        4 // int and long are both 4 (LP32)
                    };
                    Rc::new(TypeK::Int { size, signed })
                }
            }
        };
        Ok((final_ty, sc))
    }

    fn struct_decl(&mut self, is_union: bool) -> PResult<Type> {
        let tag = if let Tok::Ident(s) = self.peek().clone() {
            self.pos += 1;
            Some(s)
        } else {
            None
        };
        if !self.at_punct("{") {
            // reference to an existing tag
            if let Some(t) = tag.as_ref().and_then(|t| self.structs.get(t).cloned()) {
                return Ok(t);
            }
            let name = tag.unwrap_or_default();
            // forward decl — treat as opaque 0-size for now
            return Ok(Rc::new(TypeK::Struct { name, members: vec![], size: 0, align: 1 }));
        }
        self.expect("{")?;
        let mut members = Vec::new();
        let mut offset = 0u32;
        let mut align = 1u32;
        while !self.at_punct("}") {
            let (base, _) = self.declspec()?;
            loop {
                let (mname, mty) = self.declarator(base.clone())?;
                let a = mty.align();
                if is_union {
                    // All union members overlap at offset 0; size is the max.
                    members.push(Member { name: mname, ty: mty.clone(), offset: 0 });
                    offset = offset.max(mty.size());
                } else {
                    offset = align_to(offset, a);
                    members.push(Member { name: mname, ty: mty.clone(), offset });
                    offset += mty.size();
                }
                align = align.max(a);
                if !self.eat_punct(",") {
                    break;
                }
            }
            self.expect(";")?;
        }
        self.expect("}")?;
        let size = align_to(offset, align);
        let name = tag.clone().unwrap_or_default();
        let ty = Rc::new(TypeK::Struct { name: name.clone(), members, size, align });
        if let Some(t) = tag {
            self.structs.insert(t, ty.clone());
        }
        Ok(ty)
    }

    fn enum_decl(&mut self) -> PResult<Type> {
        // enum { A, B=5, C } — values as int constants in scope.
        if let Tok::Ident(_) = self.peek() {
            self.pos += 1;
        }
        if self.eat_punct("{") {
            let mut val = 0i64;
            while !self.at_punct("}") {
                let name = self.ident()?;
                if self.eat_punct("=") {
                    let e = self.assign()?;
                    val = const_eval(&e)?;
                }
                // register the enumerator as a global constant via a fake typedef-like binding:
                self.enum_consts.insert(name, val);
                val += 1;
                if !self.eat_punct(",") {
                    break;
                }
            }
            self.expect("}")?;
        }
        Ok(t_int())
    }

    // parameter names captured during the most recent function declarator
    // (declarator can't return names through Type, so stash them here)
    fn declarator(&mut self, base: Type) -> PResult<(String, Type)> {
        let mut ty = base;
        while self.eat_punct("*") {
            ty = t_ptr(ty);
            while self.at_kw("const") || self.at_kw("volatile") {
                self.pos += 1;
            }
        }
        // (declarator) grouping
        if self.at_punct("(") && self.is_grouped_declarator() {
            self.expect("(")?;
            // parse inner declarator with a placeholder base, then apply suffix
            let save = self.pos;
            let _ = save;
            let (name, inner) = self.declarator(t_void())?; // placeholder
            self.expect(")")?;
            let suffixed = self.type_suffix(ty)?;
            // substitute the placeholder (void) leaf in `inner` with `suffixed`
            let final_ty = substitute_leaf(&inner, &suffixed);
            return Ok((name, final_ty));
        }
        let name = match self.peek().clone() {
            Tok::Ident(s) => {
                self.pos += 1;
                s
            }
            _ => String::new(), // abstract declarator (for casts / params)
        };
        let ty = self.type_suffix(ty)?;
        Ok((name, ty))
    }

    fn is_grouped_declarator(&self) -> bool {
        // '(' followed by '*' or an identifier that isn't a typename → grouping
        if !self.at_punct("(") {
            return false;
        }
        match &self.toks[self.pos + 1].tok {
            Tok::Punct(p) if p == "*" => true,
            Tok::Punct(p) if p == "(" => true,
            _ => false,
        }
    }

    fn type_suffix(&mut self, ty: Type) -> PResult<Type> {
        if self.at_punct("(") {
            return self.func_params(ty);
        }
        if self.eat_punct("[") {
            let n = if self.at_punct("]") {
                0
            } else {
                let e = self.assign()?;
                const_eval(&e)? as u32
            };
            self.expect("]")?;
            let inner = self.type_suffix(ty)?;
            return Ok(Rc::new(TypeK::Array(inner, n)));
        }
        Ok(ty)
    }

    fn func_params(&mut self, ret: Type) -> PResult<Type> {
        self.expect("(")?;
        let mut params = Vec::new();
        let mut names = Vec::new();
        let mut variadic = false;
        // `(void)` → no params
        if self.at_kw("void") && matches!(&self.toks[self.pos + 1].tok, Tok::Punct(p) if p == ")") {
            self.pos += 1;
        } else {
            while !self.at_punct(")") {
                if self.eat_punct("...") {
                    variadic = true;
                    break;
                }
                let (base, _) = self.declspec()?;
                let (pname, mut pty) = self.declarator(base)?;
                pty = pty.decay();
                names.push(pname);
                params.push(pty);
                if !self.eat_punct(",") {
                    break;
                }
            }
        }
        self.expect(")")?;
        self.pending_params = names;
        Ok(Rc::new(TypeK::Func { ret, params, variadic }))
    }

    // ── statements ────────────────────────────────────────────────────────────
    fn block_items(&mut self) -> PResult<Vec<Stmt>> {
        let mut out = Vec::new();
        while !self.at_punct("}") && !matches!(self.peek(), Tok::Eof) {
            if self.is_typename() {
                self.local_decl(&mut out)?;
            } else {
                out.push(self.stmt()?);
            }
        }
        self.expect("}")?;
        Ok(out)
    }

    fn local_decl(&mut self, out: &mut Vec<Stmt>) -> PResult<()> {
        let (base, sc) = self.declspec()?;
        if sc.typedef {
            let (name, ty) = self.declarator(base)?;
            self.typedefs.last_mut().unwrap().insert(name, ty);
            self.expect(";")?;
            return Ok(());
        }
        loop {
            if self.at_punct(";") {
                break;
            }
            let (name, ty) = self.declarator(base.clone())?;
            if sc.is_extern {
                // `extern T x;` inside a function refers to the file-scope /
                // other-unit symbol `x` — bind to the real name, don't mint a
                // fresh one. Register it as a known (external) global so codegen
                // emits `_x`, unless a definition for it already exists here.
                self.scopes.last_mut().unwrap().insert(
                    name.clone(),
                    VarRef { name: name.clone(), ty: ty.clone(), is_global: true },
                );
                if !self.globals.iter().any(|g| g.name == name) {
                    self.globals.push(Global {
                        name: name.clone(),
                        ty: ty.clone(),
                        init: None,
                        is_static: false,
                        is_extern: true,
                        align: 0,
                    });
                }
                // An extern declaration can't have an initializer; move on.
                if !self.eat_punct(",") {
                    break;
                }
                continue;
            }
            if sc.is_static {
                // A static local lives in static storage, not the frame: give it
                // a unique global (internal linkage) and bind it in this scope.
                self.uid += 1;
                let uniq = format!("{name}__s{}", self.uid);
                self.scopes.last_mut().unwrap().insert(
                    name.clone(),
                    VarRef { name: uniq.clone(), ty: ty.clone(), is_global: true },
                );
                let mut init = None;
                if self.eat_punct("=") {
                    init = Some(self.global_initializer(&ty)?);
                }
                let align = self.aligns.get(&name).copied().unwrap_or(0);
                self.globals.push(Global {
                    name: uniq,
                    ty,
                    init,
                    is_static: true,
                    is_extern: false,
                    align,
                });
                if !self.eat_punct(",") {
                    break;
                }
                continue;
            }
            let uniq = self.new_local(&name, ty.clone(), sc.is_volatile);
            let init = if self.eat_punct("=") {
                Some(self.initializer()?)
            } else {
                None
            };
            out.push(Stmt::Decl(uniq, ty, init));
            if !self.eat_punct(",") {
                break;
            }
        }
        self.expect(";")?;
        Ok(())
    }

    fn initializer(&mut self) -> PResult<Init> {
        if self.eat_punct("{") {
            let mut items = Vec::new();
            while !self.at_punct("}") {
                items.push(self.initializer()?);
                if !self.eat_punct(",") {
                    break;
                }
            }
            self.expect("}")?;
            Ok(Init::List(items))
        } else {
            Ok(Init::Scalar(self.assign()?))
        }
    }

    /// `asm [volatile|goto] ( "text" [: outputs [: inputs [: clobbers]]] );`
    /// Basic asm passes through to the output. Extended asm supports the
    /// minimal subset the corpus uses — ≤1 data-register output, ≤2 operands
    /// total (`%0`/`%1`), clobbers accepted and ignored (this codegen holds
    /// nothing live in d0/d1/flags across statements). Anything richer is a
    /// hard error — silently dropping asm deleted a `move #$2000,sr`
    /// interrupt enable from a shipped port (adoption report round 2, item 1).
    fn asm_stmt(&mut self) -> PResult<Stmt> {
        let loc = self.loc();
        self.pos += 1; // asm / __asm__ / __asm
        while self.eat_kw("volatile") || self.eat_kw("goto") || self.eat_kw("inline") {}
        self.expect("(")?;
        let mut text = Vec::new();
        while let Tok::Str(bytes) = self.peek().clone() {
            self.pos += 1;
            text.extend(bytes.iter().take_while(|&&b| b != 0).copied());
        }
        let template = String::from_utf8_lossy(&text).into_owned();
        if self.eat_punct(")") {
            self.expect(";")?;
            return Ok(Stmt::Asm(template));
        }
        if !self.eat_punct(":") {
            return Err(format!(
                "{loc}: expected string literal or ':' in asm(...), found {:?}",
                self.peek()
            ));
        }
        let outputs = self.asm_operands()?;
        let inputs = if self.eat_punct(":") { self.asm_operands()? } else { Vec::new() };
        if self.eat_punct(":") {
            // clobber list: strings, accepted and ignored
            while matches!(self.peek(), Tok::Str(_)) {
                self.pos += 1;
                if !self.eat_punct(",") {
                    break;
                }
            }
        }
        self.expect(")")?;
        self.expect(";")?;

        if outputs.len() > 1 || inputs.len() > 1 || outputs.len() + inputs.len() > 2 {
            return Err(format!(
                "{loc}: extended inline asm supports at most one output and one input \
                 (%0/%1) — hoist richer asm to a .S file"
            ));
        }
        let output = match outputs.into_iter().next() {
            Some((cons, e)) => {
                let ok = (cons.starts_with('=') || cons.starts_with('+'))
                    && cons[1..].chars().all(|c| matches!(c, 'd' | 'r' | 'g'));
                if !ok {
                    return Err(format!(
                        "{loc}: unsupported asm output constraint \"{cons}\" — only \
                         \"=d\"/\"+d\" (data register) is supported"
                    ));
                }
                Some((cons.starts_with('+'), e))
            }
            None => None,
        };
        let input = match inputs.into_iter().next() {
            Some((cons, e)) => {
                if !cons.chars().all(|c| matches!(c, 'd' | 'r' | 'g' | 'i')) {
                    return Err(format!(
                        "{loc}: unsupported asm input constraint \"{cons}\" — only \
                         \"d\"/\"r\"/\"g\" (data register) is supported"
                    ));
                }
                Some(e)
            }
            None => None,
        };
        Ok(Stmt::AsmExt { template, output, input })
    }

    /// One `"constraint" (expr)` list section of an extended asm statement.
    fn asm_operands(&mut self) -> PResult<Vec<(String, Expr)>> {
        let mut out = Vec::new();
        while let Tok::Str(bytes) = self.peek().clone() {
            self.pos += 1;
            let cons: String =
                bytes.iter().take_while(|&&b| b != 0).map(|&b| b as char).collect();
            self.expect("(")?;
            let e = self.assign()?;
            self.expect(")")?;
            out.push((cons, e));
            if !self.eat_punct(",") {
                break;
            }
        }
        Ok(out)
    }

    fn stmt(&mut self) -> PResult<Stmt> {
        // labeled statement:  IDENT ':' stmt   (goto target)
        if let Tok::Ident(name) = self.peek().clone() {
            if matches!(&self.toks[self.pos + 1].tok, Tok::Punct(p) if p == ":") {
                self.pos += 2;
                let s = self.stmt()?;
                return Ok(Stmt::Label(name, Box::new(s)));
            }
            // inline asm statement: `asm [volatile] ("text");`
            if matches!(name.as_str(), "asm" | "__asm__" | "__asm") {
                return self.asm_stmt();
            }
        }
        if self.eat_kw("switch") {
            self.expect("(")?;
            let cond = self.expr()?;
            self.expect(")")?;
            self.cur_switch.push(SwitchBuild::default());
            let body = self.stmt()?;
            let sw = self.cur_switch.pop().unwrap();
            return Ok(Stmt::Switch(cond, Box::new(body), sw.cases, sw.default));
        }
        if self.eat_kw("case") {
            let e = self.conditional()?;
            let val = const_eval(&e)?;
            self.expect(":")?;
            let id = self.uid as u32;
            self.uid += 1;
            if let Some(sw) = self.cur_switch.last_mut() {
                sw.cases.push((val, id));
            }
            // The statement after the label is a separate block item; a bare
            // `case N:` before `}` is allowed (empty).
            return Ok(Stmt::Case(id));
        }
        if self.eat_kw("default") {
            self.expect(":")?;
            let id = self.uid as u32;
            self.uid += 1;
            if let Some(sw) = self.cur_switch.last_mut() {
                sw.default = Some(id);
            }
            return Ok(Stmt::Default(id));
        }
        if self.eat_kw("goto") {
            let name = self.ident()?;
            self.expect(";")?;
            return Ok(Stmt::Goto(name));
        }
        if self.eat_kw("return") {
            if self.eat_punct(";") {
                return Ok(Stmt::Return(None));
            }
            let e = self.expr()?;
            self.expect(";")?;
            return Ok(Stmt::Return(Some(e)));
        }
        if self.eat_kw("if") {
            self.expect("(")?;
            let c = self.expr()?;
            self.expect(")")?;
            let then = Box::new(self.stmt()?);
            let els = if self.eat_kw("else") {
                Some(Box::new(self.stmt()?))
            } else {
                None
            };
            return Ok(Stmt::If(c, then, els));
        }
        if self.eat_kw("while") {
            self.expect("(")?;
            let c = self.expr()?;
            self.expect(")")?;
            let body = Box::new(self.stmt()?);
            return Ok(Stmt::While(c, body));
        }
        if self.eat_kw("do") {
            let body = Box::new(self.stmt()?);
            if !self.eat_kw("while") {
                return Err(format!("{}: expected 'while' after do-body", self.loc()));
            }
            self.expect("(")?;
            let c = self.expr()?;
            self.expect(")")?;
            self.expect(";")?;
            return Ok(Stmt::DoWhile(body, c));
        }
        if self.eat_kw("for") {
            self.expect("(")?;
            self.push_scope();
            let init: Option<Box<Stmt>> = if self.eat_punct(";") {
                None
            } else if self.is_typename() {
                let mut v = Vec::new();
                self.local_decl(&mut v)?;
                Some(Box::new(Stmt::Block(v)))
            } else {
                let e = self.expr()?;
                self.expect(";")?;
                Some(Box::new(Stmt::Expr(e)))
            };
            let cond = if self.at_punct(";") {
                None
            } else {
                Some(self.expr()?)
            };
            self.expect(";")?;
            let step = if self.at_punct(")") {
                None
            } else {
                Some(self.expr()?)
            };
            self.expect(")")?;
            let body = Box::new(self.stmt()?);
            self.pop_scope();
            return Ok(Stmt::For(init, cond, step, body));
        }
        if self.eat_kw("break") {
            self.expect(";")?;
            return Ok(Stmt::Break);
        }
        if self.eat_kw("continue") {
            self.expect(";")?;
            return Ok(Stmt::Continue);
        }
        if self.at_punct("{") {
            self.pos += 1;
            self.push_scope();
            let items = self.block_items()?;
            self.pop_scope();
            return Ok(Stmt::Block(items));
        }
        if self.eat_punct(";") {
            return Ok(Stmt::Null);
        }
        let e = self.expr()?;
        self.expect(";")?;
        Ok(Stmt::Expr(e))
    }

    // ── expressions (precedence climbing by explicit levels) ──────────────────
    fn expr(&mut self) -> PResult<Expr> {
        let mut e = self.assign()?;
        while self.eat_punct(",") {
            let rhs = self.assign()?;
            let ty = rhs.ty.clone();
            e = Expr { kind: ExprK::Comma(Box::new(e), Box::new(rhs)), ty, line: self.line() };
        }
        Ok(e)
    }

    fn assign(&mut self) -> PResult<Expr> {
        let lhs = self.conditional()?;
        let op = match self.peek() {
            Tok::Punct(p) if matches!(p.as_str(), "=" | "+=" | "-=" | "*=" | "/=" | "%=" | "&=" | "|=" | "^=" | "<<=" | ">>=") => p.clone(),
            _ => return Ok(lhs),
        };
        self.pos += 1;
        let rhs = self.assign()?;
        let ty = lhs.ty.clone();
        if op == "=" {
            return Ok(Expr { kind: ExprK::Assign(Box::new(lhs), Box::new(rhs)), ty, line: self.line() });
        }
        // compound assignment: lhs op= rhs  =>  lhs = lhs op rhs
        let binop = match op.as_str() {
            "+=" => BinOp::Add,
            "-=" => BinOp::Sub,
            "*=" => BinOp::Mul,
            "/=" => BinOp::Div,
            "%=" => BinOp::Mod,
            "&=" => BinOp::And,
            "|=" => BinOp::Or,
            "^=" => BinOp::Xor,
            "<<=" => BinOp::Shl,
            ">>=" => BinOp::Shr,
            _ => unreachable!(),
        };
        let combined = self.make_binary(binop, lhs.clone(), rhs)?;
        Ok(Expr { kind: ExprK::Assign(Box::new(lhs), Box::new(combined)), ty, line: self.line() })
    }

    fn conditional(&mut self) -> PResult<Expr> {
        let c = self.logor()?;
        if self.eat_punct("?") {
            let t = self.expr()?;
            self.expect(":")?;
            let e = self.conditional()?;
            let ty = t.ty.clone();
            return Ok(Expr { kind: ExprK::Cond(Box::new(c), Box::new(t), Box::new(e)), ty, line: self.line() });
        }
        Ok(c)
    }

    fn bin_level(
        &mut self,
        ops: &[(&str, BinOp)],
        next: fn(&mut Self) -> PResult<Expr>,
    ) -> PResult<Expr> {
        let mut lhs = next(self)?;
        'outer: loop {
            for (s, op) in ops {
                if self.at_punct(s) {
                    self.pos += 1;
                    let rhs = next(self)?;
                    lhs = self.make_binary(*op, lhs, rhs)?;
                    continue 'outer;
                }
            }
            break;
        }
        Ok(lhs)
    }

    fn logor(&mut self) -> PResult<Expr> {
        self.bin_level(&[("||", BinOp::LogOr)], Self::logand)
    }
    fn logand(&mut self) -> PResult<Expr> {
        self.bin_level(&[("&&", BinOp::LogAnd)], Self::bitor)
    }
    fn bitor(&mut self) -> PResult<Expr> {
        self.bin_level(&[("|", BinOp::Or)], Self::bitxor)
    }
    fn bitxor(&mut self) -> PResult<Expr> {
        self.bin_level(&[("^", BinOp::Xor)], Self::bitand)
    }
    fn bitand(&mut self) -> PResult<Expr> {
        self.bin_level(&[("&", BinOp::And)], Self::equality)
    }
    fn equality(&mut self) -> PResult<Expr> {
        self.bin_level(&[("==", BinOp::Eq), ("!=", BinOp::Ne)], Self::relational)
    }
    fn relational(&mut self) -> PResult<Expr> {
        self.bin_level(
            &[("<=", BinOp::Le), (">=", BinOp::Ge), ("<", BinOp::Lt), (">", BinOp::Gt)],
            Self::shift,
        )
    }
    fn shift(&mut self) -> PResult<Expr> {
        self.bin_level(&[("<<", BinOp::Shl), (">>", BinOp::Shr)], Self::add)
    }
    fn add(&mut self) -> PResult<Expr> {
        self.bin_level(&[("+", BinOp::Add), ("-", BinOp::Sub)], Self::mul)
    }
    fn mul(&mut self) -> PResult<Expr> {
        self.bin_level(
            &[("*", BinOp::Mul), ("/", BinOp::Div), ("%", BinOp::Mod)],
            Self::cast,
        )
    }

    fn cast(&mut self) -> PResult<Expr> {
        // (type) cast
        if self.at_punct("(") && self.next_is_typename() {
            self.expect("(")?;
            let (base, _) = self.declspec()?;
            let (_, ty) = self.declarator(base)?;
            self.expect(")")?;
            let inner = self.cast()?;
            let ty = ty.decay();
            return Ok(Expr { kind: ExprK::Cast(Box::new(inner)), ty, line: self.line() });
        }
        self.unary()
    }

    fn next_is_typename(&self) -> bool {
        // we're at '(' ; peek the token after it
        match &self.toks[self.pos + 1].tok {
            Tok::Keyword(k) => matches!(
                k.as_str(),
                "void" | "char" | "short" | "int" | "long" | "unsigned" | "signed"
                    | "struct" | "union" | "enum" | "float" | "double" | "_Bool" | "const" | "volatile"
            ),
            Tok::Ident(s) => self.find_typedef(s).is_some(),
            _ => false,
        }
    }

    fn unary(&mut self) -> PResult<Expr> {
        let line = self.line();
        if self.eat_punct("+") {
            return self.cast();
        }
        if self.eat_punct("-") {
            let e = self.cast()?;
            let ty = e.ty.clone();
            return Ok(Expr { kind: ExprK::Unary(UnOp::Neg, Box::new(e)), ty, line });
        }
        if self.eat_punct("~") {
            let e = self.cast()?;
            let ty = e.ty.clone();
            return Ok(Expr { kind: ExprK::Unary(UnOp::Not, Box::new(e)), ty, line });
        }
        if self.eat_punct("!") {
            let e = self.cast()?;
            return Ok(Expr { kind: ExprK::Unary(UnOp::LogNot, Box::new(e)), ty: t_int(), line });
        }
        if self.eat_punct("*") {
            let e = self.cast()?;
            let base = e.ty.decay().base().ok_or_else(|| format!("{line}: dereferencing non-pointer"))?;
            return Ok(Expr { kind: ExprK::Unary(UnOp::Deref, Box::new(e)), ty: base, line });
        }
        if self.eat_punct("&") {
            let e = self.cast()?;
            let ty = t_ptr(e.ty.clone());
            return Ok(Expr { kind: ExprK::Unary(UnOp::Addr, Box::new(e)), ty, line });
        }
        if self.eat_punct("++") {
            let e = self.unary()?;
            // ++e => e = e + 1
            let one = Expr { kind: ExprK::Num(1), ty: t_int(), line };
            let sum = self.make_binary(BinOp::Add, e.clone(), one)?;
            let ty = e.ty.clone();
            return Ok(Expr { kind: ExprK::Assign(Box::new(e), Box::new(sum)), ty, line });
        }
        if self.eat_punct("--") {
            let e = self.unary()?;
            let one = Expr { kind: ExprK::Num(1), ty: t_int(), line };
            let sum = self.make_binary(BinOp::Sub, e.clone(), one)?;
            let ty = e.ty.clone();
            return Ok(Expr { kind: ExprK::Assign(Box::new(e), Box::new(sum)), ty, line });
        }
        if self.eat_kw("sizeof") {
            if self.at_punct("(") && self.next_is_typename() {
                self.expect("(")?;
                let (base, _) = self.declspec()?;
                let (_, ty) = self.declarator(base)?;
                self.expect(")")?;
                return Ok(Expr { kind: ExprK::Num(ty.size() as i64), ty: t_uint(), line });
            }
            let e = self.unary()?;
            return Ok(Expr { kind: ExprK::Num(e.ty.size() as i64), ty: t_uint(), line });
        }
        self.postfix()
    }

    fn postfix(&mut self) -> PResult<Expr> {
        let mut e = self.primary()?;
        loop {
            let line = self.line();
            if self.eat_punct("[") {
                let idx = self.expr()?;
                self.expect("]")?;
                // e[i] => *(e + i)
                let sum = self.make_binary(BinOp::Add, e, idx)?;
                let base = sum.ty.decay().base().ok_or_else(|| format!("{line}: indexing non-pointer"))?;
                e = Expr { kind: ExprK::Unary(UnOp::Deref, Box::new(sum)), ty: base, line };
                continue;
            }
            if self.eat_punct("(") {
                let mut args = Vec::new();
                while !self.at_punct(")") {
                    args.push(self.assign()?);
                    if !self.eat_punct(",") {
                        break;
                    }
                }
                self.expect(")")?;
                let ret = match &*e.ty.decay() {
                    TypeK::Ptr(inner) => match &**inner {
                        TypeK::Func { ret, .. } => ret.clone(),
                        _ => t_int(),
                    },
                    TypeK::Func { ret, .. } => ret.clone(),
                    _ => t_int(),
                };
                e = Expr { kind: ExprK::Call(Box::new(e), args), ty: ret, line };
                continue;
            }
            if self.eat_punct(".") {
                let name = self.ident()?;
                e = self.member(e, &name, false, line)?;
                continue;
            }
            if self.eat_punct("->") {
                let name = self.ident()?;
                e = self.member(e, &name, true, line)?;
                continue;
            }
            if self.eat_punct("++") {
                let ty = e.ty.clone();
                let delta = ptr_scale(&ty);
                e = Expr { kind: ExprK::PostIncDec(Box::new(e), delta), ty, line };
                continue;
            }
            if self.eat_punct("--") {
                let ty = e.ty.clone();
                let delta = -ptr_scale(&ty);
                e = Expr { kind: ExprK::PostIncDec(Box::new(e), delta), ty, line };
                continue;
            }
            break;
        }
        Ok(e)
    }

    fn member(&mut self, base: Expr, name: &str, arrow: bool, line: usize) -> PResult<Expr> {
        // For `a.b`, take address of a then member; for `a->b`, a is already a ptr.
        let struct_ty = if arrow {
            base.ty.decay().base().ok_or_else(|| format!("{line}: -> on non-pointer"))?
        } else {
            base.ty.clone()
        };
        let (mty, off) = match &*struct_ty {
            TypeK::Struct { members, .. } => {
                let m = members
                    .iter()
                    .find(|m| m.name == name)
                    .ok_or_else(|| format!("{line}: no member '{name}'"))?;
                (m.ty.clone(), m.offset)
            }
            _ => return Err(format!("{line}: member access on non-struct")),
        };
        // address of the struct
        let addr = if arrow {
            base
        } else {
            Expr { kind: ExprK::Unary(UnOp::Addr, Box::new(base)), ty: t_ptr(struct_ty.clone()), line }
        };
        // Member(addr, off) yields an lvalue of member type; codegen loads it.
        Ok(Expr { kind: ExprK::Member(Box::new(addr), off), ty: mty, line })
    }

    fn primary(&mut self) -> PResult<Expr> {
        let line = self.line();
        match self.peek().clone() {
            Tok::Num(n) => {
                self.pos += 1;
                Ok(Expr { kind: ExprK::Num(n), ty: t_int(), line })
            }
            Tok::Float(f) => {
                self.pos += 1;
                // 16.16 fixed-point literal (round to nearest).
                let fixed = (f * 65536.0).round() as i64;
                Ok(Expr { kind: ExprK::Num(fixed), ty: t_fixed(), line })
            }
            Tok::Char(n) => {
                self.pos += 1;
                Ok(Expr { kind: ExprK::Num(n), ty: t_char(), line })
            }
            Tok::Str(bytes) => {
                self.pos += 1;
                let idx = self.strings.len();
                let len = bytes.len() as u32;
                self.strings.push(bytes);
                Ok(Expr { kind: ExprK::StrLit(idx), ty: Rc::new(TypeK::Array(t_char(), len)), line })
            }
            Tok::Ident(s) => {
                self.pos += 1;
                if let Some(&v) = self.enum_consts.get(&s) {
                    return Ok(Expr { kind: ExprK::Num(v), ty: t_int(), line });
                }
                if let Some(vr) = self.resolve(&s) {
                    Ok(Expr { kind: ExprK::Var(vr.name), ty: vr.ty, line })
                } else {
                    // implicit function/global — assume int() or extern; used for
                    // calls to not-yet-declared functions.
                    Ok(Expr { kind: ExprK::Var(s), ty: Rc::new(TypeK::Func { ret: t_int(), params: vec![], variadic: true }), line })
                }
            }
            Tok::Punct(p) if p == "(" => {
                self.pos += 1;
                let e = self.expr()?;
                self.expect(")")?;
                Ok(e)
            }
            other => Err(format!("{line}: unexpected token {:?}", other)),
        }
    }

    // ── typed binary constructor (pointer arithmetic + conversions) ───────────
    fn make_binary(&mut self, op: BinOp, lhs: Expr, rhs: Expr) -> PResult<Expr> {
        let line = lhs.line;
        let lt = lhs.ty.decay();
        let rt = rhs.ty.decay();
        // pointer arithmetic
        if matches!(op, BinOp::Add) {
            if lt.is_ptr() && rt.is_integer() {
                let scaled = scale(rhs, lt.base().unwrap().size(), line);
                return Ok(Expr { kind: ExprK::Binary(op, Box::new(lhs), Box::new(scaled)), ty: lt, line });
            }
            if lt.is_integer() && rt.is_ptr() {
                let scaled = scale(lhs, rt.base().unwrap().size(), line);
                return Ok(Expr { kind: ExprK::Binary(op, Box::new(rhs), Box::new(scaled)), ty: rt, line });
            }
        }
        if matches!(op, BinOp::Sub) {
            if lt.is_ptr() && rt.is_integer() {
                let scaled = scale(rhs, lt.base().unwrap().size(), line);
                return Ok(Expr { kind: ExprK::Binary(op, Box::new(lhs), Box::new(scaled)), ty: lt, line });
            }
            if lt.is_ptr() && rt.is_ptr() {
                let esz = lt.base().unwrap().size().max(1) as i64;
                let diff = Expr { kind: ExprK::Binary(BinOp::Sub, Box::new(lhs), Box::new(rhs)), ty: t_int(), line };
                let szc = Expr { kind: ExprK::Num(esz), ty: t_int(), line };
                return Ok(Expr { kind: ExprK::Binary(BinOp::Div, Box::new(diff), Box::new(szc)), ty: t_int(), line });
            }
        }
        // fixed-point: if either operand is fixed, promote both to fixed. The
        // result is fixed for arithmetic, int for comparisons.
        if lt.is_fixed() || rt.is_fixed() {
            let to_fixed = |e: Expr| -> Expr {
                if e.ty.is_fixed() {
                    e
                } else {
                    Expr { kind: ExprK::Cast(Box::new(e)), ty: t_fixed(), line }
                }
            };
            let l = to_fixed(lhs);
            let r = to_fixed(rhs);
            let ty = match op {
                BinOp::Eq | BinOp::Ne | BinOp::Lt | BinOp::Le | BinOp::Gt | BinOp::Ge
                | BinOp::LogAnd | BinOp::LogOr => t_int(),
                _ => t_fixed(),
            };
            return Ok(Expr { kind: ExprK::Binary(op, Box::new(l), Box::new(r)), ty, line });
        }
        // comparisons and logicals yield int
        let ty = match op {
            BinOp::Eq | BinOp::Ne | BinOp::Lt | BinOp::Le | BinOp::Gt | BinOp::Ge
            | BinOp::LogAnd | BinOp::LogOr => t_int(),
            _ => {
                // usual arithmetic conversion: result is the wider (>=int) type;
                // unsigned if either operand is unsigned.
                let signed = lt.is_signed() && rt.is_signed();
                Rc::new(TypeK::Int { size: 4, signed })
            }
        };
        // Constant-fold two literals. Cheap here, and it feeds the code
        // generator's address folding: an offset that is still a `Binary` node
        // at codegen time has to be computed in registers at runtime.
        if let (ExprK::Num(a), ExprK::Num(b)) = (&lhs.kind, &rhs.kind) {
            let signed = lt.is_signed() && rt.is_signed();
            if let Some(v) = fold_const_binary(op, *a, *b, signed) {
                return Ok(Expr { kind: ExprK::Num(v), ty, line });
            }
        }
        Ok(Expr { kind: ExprK::Binary(op, Box::new(lhs), Box::new(rhs)), ty, line })
    }

    // extra parser state that doesn't fit the struct fields above
    // (declared via a small side table to avoid churn)
    // NB: kept as fields for clarity:
    // pending_params, enum_consts
    // (added below via impl blocks reusing Parser)
}

fn scale(e: Expr, sz: u32, line: usize) -> Expr {
    if sz <= 1 {
        return e;
    }
    // A constant index scales at compile time. Without this, `m->m[2]` emitted
    // the multiplication `2 * 4` as *runtime* instructions, which also hid the
    // constant from the code generator's address-folding — seven instructions
    // for what is one `move.l 8(a2),d0`.
    if let ExprK::Num(k) = &e.kind {
        return Expr { kind: ExprK::Num(k.wrapping_mul(sz as i64)), ty: e.ty.clone(), line };
    }
    let szc = Expr { kind: ExprK::Num(sz as i64), ty: t_int(), line };
    let ty = e.ty.clone();
    Expr { kind: ExprK::Binary(BinOp::Mul, Box::new(e), Box::new(szc)), ty, line }
}

/// Evaluate `a op b` at compile time when it is safe to do so.
///
/// Only operations whose result is independent of signedness are folded
/// unconditionally; division, remainder, right shift and the comparisons
/// depend on it, so those fold only when both operands are signed. Values are
/// kept as `i64` and truncated by the code generator exactly as a runtime
/// 32-bit computation would wrap.
fn fold_const_binary(op: BinOp, a: i64, b: i64, signed: bool) -> Option<i64> {
    let r = match op {
        BinOp::Add => a.wrapping_add(b),
        BinOp::Sub => a.wrapping_sub(b),
        BinOp::Mul => a.wrapping_mul(b),
        BinOp::And => a & b,
        BinOp::Or => a | b,
        BinOp::Xor => a ^ b,
        BinOp::Shl if (0..32).contains(&b) => a.wrapping_shl(b as u32),
        BinOp::Shr if signed && (0..32).contains(&b) => a >> b,
        BinOp::Div if signed && b != 0 => a.wrapping_div(b),
        BinOp::Mod if signed && b != 0 => a.wrapping_rem(b),
        BinOp::Eq => (a == b) as i64,
        BinOp::Ne => (a != b) as i64,
        BinOp::Lt if signed => (a < b) as i64,
        BinOp::Le if signed => (a <= b) as i64,
        BinOp::Gt if signed => (a > b) as i64,
        BinOp::Ge if signed => (a >= b) as i64,
        BinOp::LogAnd => ((a != 0) && (b != 0)) as i64,
        BinOp::LogOr => ((a != 0) || (b != 0)) as i64,
        _ => return None,
    };
    Some(r)
}

fn ptr_scale(ty: &Type) -> i64 {
    match &**ty {
        TypeK::Ptr(b) => b.size().max(1) as i64,
        _ => 1,
    }
}

fn align_to(n: u32, a: u32) -> u32 {
    let a = a.max(1);
    (n + a - 1) / a * a
}

/// Replace the placeholder Void leaf produced during grouped-declarator parsing.
fn substitute_leaf(shell: &Type, leaf: &Type) -> Type {
    match &**shell {
        TypeK::Void => leaf.clone(),
        TypeK::Ptr(b) => t_ptr(substitute_leaf(b, leaf)),
        TypeK::Array(b, n) => Rc::new(TypeK::Array(substitute_leaf(b, leaf), *n)),
        TypeK::Func { ret, params, variadic } => Rc::new(TypeK::Func {
            ret: substitute_leaf(ret, leaf),
            params: params.clone(),
            variadic: *variadic,
        }),
        _ => shell.clone(),
    }
}

pub fn const_eval(e: &Expr) -> Result<i64, String> {
    Ok(match &e.kind {
        ExprK::Num(n) => *n,
        ExprK::Unary(UnOp::Neg, a) => -const_eval(a)?,
        ExprK::Unary(UnOp::Not, a) => !const_eval(a)?,
        ExprK::Unary(UnOp::LogNot, a) => (const_eval(a)? == 0) as i64,
        ExprK::Binary(op, a, b) => {
            // Short-circuit operators must not eval the dead side.
            match op {
                BinOp::LogAnd => {
                    return Ok((const_eval(a)? != 0 && const_eval(b)? != 0) as i64);
                }
                BinOp::LogOr => {
                    return Ok((const_eval(a)? != 0 || const_eval(b)? != 0) as i64);
                }
                _ => {}
            }
            let (x, y) = (const_eval(a)?, const_eval(b)?);
            match op {
                BinOp::Add => x + y,
                BinOp::Sub => x - y,
                BinOp::Mul => x * y,
                BinOp::Div => x / y,
                BinOp::Mod => x % y,
                BinOp::Shl => x << y,
                BinOp::Shr => x >> y,
                BinOp::And => x & y,
                BinOp::Or => x | y,
                BinOp::Xor => x ^ y,
                BinOp::Eq => (x == y) as i64,
                BinOp::Ne => (x != y) as i64,
                BinOp::Lt => (x < y) as i64,
                BinOp::Le => (x <= y) as i64,
                BinOp::Gt => (x > y) as i64,
                BinOp::Ge => (x >= y) as i64,
                BinOp::LogAnd | BinOp::LogOr => unreachable!(),
            }
        }
        ExprK::Cond(c, t, f) => {
            if const_eval(c)? != 0 {
                const_eval(t)?
            } else {
                const_eval(f)?
            }
        }
        ExprK::Cast(a) => const_eval(a)?,
        _ => return Err("non-constant expression".into()),
    })
}

#[derive(Default, Clone)]
struct Storage {
    typedef: bool,
    is_static: bool,
    is_extern: bool,
    is_volatile: bool,
}
