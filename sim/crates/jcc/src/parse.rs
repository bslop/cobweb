//! Tokenizer + recursive-descent parser for the jcc v1 language.

#[derive(Debug, Clone)]
pub enum Expr {
    Num(u32),
    Var(String),
    Bin(Op, Box<Expr>, Box<Expr>),
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Op {
    Add,
    Sub,
    And,
    Or,
    Xor,
    Mul,
    Shl,
    Shr,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Rel {
    Eq,
    Ne,
    Lt,
    Gt,
    Le,
    Ge,
}

#[derive(Debug, Clone)]
pub struct Cond {
    pub lhs: Expr,
    pub rel: Rel,
    pub rhs: Expr,
}

#[derive(Debug, Clone)]
pub enum Stmt {
    Decl(String, Option<Expr>),
    Assign(String, Expr),
    Store { val: Expr, addr: Expr },
    If { cond: Cond, then: Vec<Stmt>, els: Option<Vec<Stmt>> },
    While { cond: Cond, body: Vec<Stmt> },
}

#[derive(Debug, Clone, PartialEq)]
enum Tok {
    Ident(String),
    Num(u32),
    Sym(String),
}

fn lex(src: &str) -> Result<Vec<Tok>, String> {
    let b: Vec<char> = src.chars().collect();
    let mut i = 0;
    let mut out = Vec::new();
    while i < b.len() {
        let c = b[i];
        if c.is_whitespace() {
            i += 1;
            continue;
        }
        // line comment // and /* */
        if c == '/' && i + 1 < b.len() && b[i + 1] == '/' {
            while i < b.len() && b[i] != '\n' {
                i += 1;
            }
            continue;
        }
        if c == '/' && i + 1 < b.len() && b[i + 1] == '*' {
            i += 2;
            while i + 1 < b.len() && !(b[i] == '*' && b[i + 1] == '/') {
                i += 1;
            }
            i += 2;
            continue;
        }
        if c == '$' {
            let mut j = i + 1;
            while j < b.len() && b[j].is_ascii_hexdigit() {
                j += 1;
            }
            let v = u32::from_str_radix(&b[i + 1..j].iter().collect::<String>(), 16)
                .map_err(|_| "bad hex")?;
            out.push(Tok::Num(v));
            i = j;
            continue;
        }
        if c.is_ascii_digit() {
            if c == '0' && i + 1 < b.len() && b[i + 1].to_ascii_lowercase() == 'x' {
                let mut j = i + 2;
                while j < b.len() && b[j].is_ascii_hexdigit() {
                    j += 1;
                }
                let v = u32::from_str_radix(&b[i + 2..j].iter().collect::<String>(), 16)
                    .map_err(|_| "bad hex")?;
                out.push(Tok::Num(v));
                i = j;
            } else {
                let mut j = i;
                while j < b.len() && b[j].is_ascii_digit() {
                    j += 1;
                }
                let v: u32 = b[i..j].iter().collect::<String>().parse().map_err(|_| "bad number")?;
                out.push(Tok::Num(v));
                i = j;
            }
            continue;
        }
        if c.is_ascii_alphabetic() || c == '_' {
            let mut j = i;
            while j < b.len() && (b[j].is_ascii_alphanumeric() || b[j] == '_') {
                j += 1;
            }
            out.push(Tok::Ident(b[i..j].iter().collect()));
            i = j;
            continue;
        }
        // multi-char operators
        let two: String = b[i..(i + 2).min(b.len())].iter().collect();
        if ["==", "!=", "<=", ">=", "<<", ">>"].contains(&two.as_str()) {
            out.push(Tok::Sym(two));
            i += 2;
            continue;
        }
        if "+-*&|^=<>(){};,".contains(c) {
            out.push(Tok::Sym(c.to_string()));
            i += 1;
            continue;
        }
        return Err(format!("unexpected character `{c}`"));
    }
    Ok(out)
}

struct P {
    t: Vec<Tok>,
    i: usize,
}

impl P {
    fn peek(&self) -> Option<&Tok> {
        self.t.get(self.i)
    }
    fn next(&mut self) -> Option<Tok> {
        let t = self.t.get(self.i).cloned();
        self.i += 1;
        t
    }
    fn eat_sym(&mut self, s: &str) -> Result<(), String> {
        match self.next() {
            Some(Tok::Sym(x)) if x == s => Ok(()),
            other => Err(format!("expected `{s}`, found {other:?}")),
        }
    }
    fn is_sym(&self, s: &str) -> bool {
        matches!(self.peek(), Some(Tok::Sym(x)) if x == s)
    }
    fn is_kw(&self, k: &str) -> bool {
        matches!(self.peek(), Some(Tok::Ident(x)) if x == k)
    }

    fn program(&mut self) -> Result<Vec<Stmt>, String> {
        let mut v = Vec::new();
        while self.peek().is_some() {
            v.push(self.stmt()?);
        }
        Ok(v)
    }

    fn block(&mut self) -> Result<Vec<Stmt>, String> {
        self.eat_sym("{")?;
        let mut v = Vec::new();
        while !self.is_sym("}") {
            if self.peek().is_none() {
                return Err("unterminated block".into());
            }
            v.push(self.stmt()?);
        }
        self.eat_sym("}")?;
        Ok(v)
    }

    fn stmt(&mut self) -> Result<Stmt, String> {
        if self.is_kw("int") {
            self.next();
            let name = self.ident()?;
            let init = if self.is_sym("=") {
                self.next();
                Some(self.expr()?)
            } else {
                None
            };
            self.eat_sym(";")?;
            return Ok(Stmt::Decl(name, init));
        }
        if self.is_kw("store") {
            self.next();
            let val = self.expr()?;
            self.eat_sym(",")?;
            let addr = self.expr()?;
            self.eat_sym(";")?;
            return Ok(Stmt::Store { val, addr });
        }
        if self.is_kw("if") {
            self.next();
            self.eat_sym("(")?;
            let cond = self.cond()?;
            self.eat_sym(")")?;
            let then = self.block()?;
            let els = if self.is_kw("else") {
                self.next();
                Some(self.block()?)
            } else {
                None
            };
            return Ok(Stmt::If { cond, then, els });
        }
        if self.is_kw("while") {
            self.next();
            self.eat_sym("(")?;
            let cond = self.cond()?;
            self.eat_sym(")")?;
            let body = self.block()?;
            return Ok(Stmt::While { cond, body });
        }
        // assignment
        let name = self.ident()?;
        self.eat_sym("=")?;
        let e = self.expr()?;
        self.eat_sym(";")?;
        Ok(Stmt::Assign(name, e))
    }

    fn cond(&mut self) -> Result<Cond, String> {
        let lhs = self.expr()?;
        let rel = match self.next() {
            Some(Tok::Sym(s)) => match s.as_str() {
                "==" => Rel::Eq,
                "!=" => Rel::Ne,
                "<" => Rel::Lt,
                ">" => Rel::Gt,
                "<=" => Rel::Le,
                ">=" => Rel::Ge,
                _ => return Err(format!("expected comparison, found `{s}`")),
            },
            other => return Err(format!("expected comparison, found {other:?}")),
        };
        let rhs = self.expr()?;
        Ok(Cond { lhs, rel, rhs })
    }

    fn expr(&mut self) -> Result<Expr, String> {
        let mut lhs = self.term()?;
        while let Some(Tok::Sym(s)) = self.peek() {
            let op = match s.as_str() {
                "+" => Op::Add,
                "-" => Op::Sub,
                "&" => Op::And,
                "|" => Op::Or,
                "^" => Op::Xor,
                _ => break,
            };
            self.next();
            let rhs = self.term()?;
            lhs = Expr::Bin(op, Box::new(lhs), Box::new(rhs));
        }
        Ok(lhs)
    }

    fn term(&mut self) -> Result<Expr, String> {
        let mut lhs = self.factor()?;
        while let Some(Tok::Sym(s)) = self.peek() {
            let op = match s.as_str() {
                "*" => Op::Mul,
                "<<" => Op::Shl,
                ">>" => Op::Shr,
                _ => break,
            };
            self.next();
            let rhs = self.factor()?;
            lhs = Expr::Bin(op, Box::new(lhs), Box::new(rhs));
        }
        Ok(lhs)
    }

    fn factor(&mut self) -> Result<Expr, String> {
        match self.next() {
            Some(Tok::Num(n)) => Ok(Expr::Num(n)),
            Some(Tok::Ident(id)) => Ok(Expr::Var(id)),
            Some(Tok::Sym(s)) if s == "(" => {
                let e = self.expr()?;
                self.eat_sym(")")?;
                Ok(e)
            }
            other => Err(format!("expected value, found {other:?}")),
        }
    }

    fn ident(&mut self) -> Result<String, String> {
        match self.next() {
            Some(Tok::Ident(id)) => Ok(id),
            other => Err(format!("expected identifier, found {other:?}")),
        }
    }
}

pub fn parse(src: &str) -> Result<Vec<Stmt>, String> {
    let t = lex(src)?;
    let mut p = P { t, i: 0 };
    let prog = p.program()?;
    if p.peek().is_some() {
        return Err("trailing tokens after program".into());
    }
    Ok(prog)
}
