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
    pos: usize,
    // scope stack: source name -> resolved (unique_name_or_global, type, is_global)
    scopes: Vec<HashMap<String, VarRef>>,
    typedefs: Vec<HashMap<String, Type>>,
    structs: HashMap<String, Type>,
    // current function's locals (unique names)
    cur_locals: Vec<(String, Type)>,
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

pub fn parse(toks: Vec<Token>) -> PResult<Program> {
    let mut p = Parser {
        toks,
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
            Err(format!("{}: expected '{}', found {:?}", self.line(), s, self.peek()))
        }
    }
    fn ident(&mut self) -> PResult<String> {
        match self.peek().clone() {
            Tok::Ident(s) => {
                self.pos += 1;
                Ok(s)
            }
            other => Err(format!("{}: expected identifier, found {:?}", self.line(), other)),
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
    fn new_local(&mut self, name: &str, ty: Type) -> String {
        self.uid += 1;
        let uniq = format!("{name}${}", self.uid);
        self.cur_locals.push((uniq.clone(), ty.clone()));
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
        self.globals.push(Global {
            name,
            ty,
            init,
            is_static: sc.is_static,
            is_extern: sc.is_extern,
        });
        Ok(())
    }

    fn global_initializer(&mut self, ty: &Type) -> PResult<Vec<u8>> {
        // Only constant scalar / string initializers for now.
        if let Tok::Str(bytes) = self.peek().clone() {
            self.pos += 1;
            return Ok(bytes);
        }
        let e = self.assign()?;
        let v = const_eval(&e)?;
        let sz = ty.size().max(1);
        let mut out = Vec::new();
        for i in (0..sz).rev() {
            out.push(((v >> (i * 8)) & 0xFF) as u8); // big-endian
        }
        Ok(out)
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
            let uniq = self.new_local(pname, pty.clone());
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
                .map(|(n, t)| Local { name: n, ty: t, offset: 0 })
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
                        self.pos += 1;
                    }
                    "unsigned" => {
                        signed = Some(false);
                        self.pos += 1;
                    }
                    "void" | "char" | "int" | "float" | "double" | "_Bool" => {
                        base_kw = Some(match k.as_str() {
                            "void" => "void",
                            "char" => "char",
                            "int" => "int",
                            "_Bool" => "char",
                            _ => "int", // float/double → int placeholder (no FPU codegen yet)
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
                        self.pos += 1;
                        ty = Some(self.struct_decl()?);
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

    fn struct_decl(&mut self) -> PResult<Type> {
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
                offset = align_to(offset, a);
                members.push(Member { name: mname, ty: mty.clone(), offset });
                offset += mty.size();
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
            let uniq = self.new_local(&name, ty.clone());
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

    fn stmt(&mut self) -> PResult<Stmt> {
        // labeled statement:  IDENT ':' stmt   (goto target)
        if let Tok::Ident(name) = self.peek().clone() {
            if matches!(&self.toks[self.pos + 1].tok, Tok::Punct(p) if p == ":") {
                self.pos += 2;
                let s = self.stmt()?;
                return Ok(Stmt::Label(name, Box::new(s)));
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
                return Err(format!("{}: expected 'while' after do-body", self.line()));
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
                    | "struct" | "union" | "enum" | "float" | "double" | "_Bool" | "const"
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
    let szc = Expr { kind: ExprK::Num(sz as i64), ty: t_int(), line };
    let ty = e.ty.clone();
    Expr { kind: ExprK::Binary(BinOp::Mul, Box::new(e), Box::new(szc)), ty, line }
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
        ExprK::Binary(op, a, b) => {
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
                _ => return Err("non-constant expression".into()),
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
}
