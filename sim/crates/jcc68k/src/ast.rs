//! AST + the C type system, sized for the 68000 (LP32: char=1, short=2,
//! int=long=pointer=4). Types are reference-counted so a node can share the
//! type of another without deep clones.

use std::rc::Rc;

pub type Type = Rc<TypeK>;

#[derive(Debug, Clone, PartialEq)]
pub enum TypeK {
    Void,
    /// Integer: (size in bytes, signed?).
    Int { size: u32, signed: bool },
    Ptr(Type),
    Array(Type, u32),
    Func { ret: Type, params: Vec<Type>, variadic: bool },
    Struct { name: String, members: Vec<Member>, size: u32, align: u32 },
}

#[derive(Debug, Clone, PartialEq)]
pub struct Member {
    pub name: String,
    pub ty: Type,
    pub offset: u32,
}

pub fn t_void() -> Type {
    Rc::new(TypeK::Void)
}
pub fn t_int() -> Type {
    Rc::new(TypeK::Int { size: 4, signed: true })
}
pub fn t_uint() -> Type {
    Rc::new(TypeK::Int { size: 4, signed: false })
}
pub fn t_char() -> Type {
    Rc::new(TypeK::Int { size: 1, signed: true })
}
pub fn t_uchar() -> Type {
    Rc::new(TypeK::Int { size: 1, signed: false })
}
pub fn t_short() -> Type {
    Rc::new(TypeK::Int { size: 2, signed: true })
}
pub fn t_ushort() -> Type {
    Rc::new(TypeK::Int { size: 2, signed: false })
}
pub fn t_ptr(to: Type) -> Type {
    Rc::new(TypeK::Ptr(to))
}

impl TypeK {
    pub fn size(&self) -> u32 {
        match self {
            TypeK::Void => 1,
            TypeK::Int { size, .. } => *size,
            TypeK::Ptr(_) => 4,
            TypeK::Array(el, n) => el.size() * n,
            TypeK::Func { .. } => 4,
            TypeK::Struct { size, .. } => *size,
        }
    }
    pub fn align(&self) -> u32 {
        match self {
            TypeK::Void => 1,
            TypeK::Int { size, .. } => (*size).min(2).max(1), // 68k: 16-bit alignment
            TypeK::Ptr(_) | TypeK::Func { .. } => 2,
            TypeK::Array(el, _) => el.align(),
            TypeK::Struct { align, .. } => *align,
        }
    }
    pub fn is_integer(&self) -> bool {
        matches!(self, TypeK::Int { .. })
    }
    pub fn is_signed(&self) -> bool {
        matches!(self, TypeK::Int { signed: true, .. })
    }
    pub fn is_ptr(&self) -> bool {
        matches!(self, TypeK::Ptr(_) | TypeK::Array(..))
    }
    /// The pointed-to / element type for pointers and arrays.
    pub fn base(&self) -> Option<Type> {
        match self {
            TypeK::Ptr(b) | TypeK::Array(b, _) => Some(b.clone()),
            _ => None,
        }
    }
    /// A value of array type decays to a pointer to its element in most contexts.
    pub fn decay(self: &Type) -> Type {
        match &**self {
            TypeK::Array(el, _) => t_ptr(el.clone()),
            TypeK::Func { .. } => t_ptr(self.clone()),
            _ => self.clone(),
        }
    }
}

// ── expressions ──────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct Expr {
    pub kind: ExprK,
    pub ty: Type,
    pub line: usize,
}

#[derive(Debug, Clone)]
pub enum ExprK {
    Num(i64),
    /// String literal: index into the program's string pool.
    StrLit(usize),
    /// A named object: (name, storage). Resolved to a stack offset or a global.
    Var(String),
    Assign(Box<Expr>, Box<Expr>),
    Binary(BinOp, Box<Expr>, Box<Expr>),
    Unary(UnOp, Box<Expr>),
    /// `&e`, `*e` are their own unary ops (Addr, Deref).
    Call(Box<Expr>, Vec<Expr>),
    /// struct member access after an address is computed: (base_addr_expr, offset)
    Member(Box<Expr>, u32),
    Cond(Box<Expr>, Box<Expr>, Box<Expr>),
    Comma(Box<Expr>, Box<Expr>),
    Cast(Box<Expr>),
    /// Post-increment/decrement: (lvalue, delta, result-is-old-value).
    PostIncDec(Box<Expr>, i64),
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum BinOp {
    Add,
    Sub,
    Mul,
    Div,
    Mod,
    Shl,
    Shr,
    And,
    Or,
    Xor,
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
    LogAnd,
    LogOr,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum UnOp {
    Neg,
    Not,     // bitwise ~
    LogNot,  // !
    Addr,    // &
    Deref,   // *
}

// ── statements ───────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub enum Stmt {
    Expr(Expr),
    Return(Option<Expr>),
    If(Expr, Box<Stmt>, Option<Box<Stmt>>),
    While(Expr, Box<Stmt>),
    DoWhile(Box<Stmt>, Expr),
    For(Option<Box<Stmt>>, Option<Expr>, Option<Expr>, Box<Stmt>),
    Block(Vec<Stmt>),
    Break,
    Continue,
    /// `switch (expr) body`, plus the collected `(case value, label id)` list and
    /// an optional default label id (assigned by the parser).
    Switch(Expr, Box<Stmt>, Vec<(i64, u32)>, Option<u32>),
    /// A `case N:` label — its unique id matches an entry in the enclosing Switch.
    Case(u32),
    /// A `default:` label.
    Default(u32),
    Goto(String),
    Label(String, Box<Stmt>),
    /// A local variable declaration with an optional initializer. The initializer
    /// list handles scalars, arrays, and structs (see `Init`).
    Decl(String, Type, Option<Init>),
    Null,
}

/// A local/global initializer: a scalar expression, or a brace list for
/// aggregates (arrays/structs), possibly nested.
#[derive(Debug, Clone)]
pub enum Init {
    Scalar(Expr),
    List(Vec<Init>),
}

// ── top level ────────────────────────────────────────────────────────────────

pub struct Function {
    pub name: String,
    pub ret: Type,
    pub params: Vec<(String, Type)>,
    pub body: Vec<Stmt>,
    /// All locals (params + block locals), name → (type, frame offset). Filled by
    /// codegen; the parser records declarations, codegen lays out the frame.
    pub locals: Vec<Local>,
    pub stack_size: u32,
    pub is_static: bool,
}

#[derive(Debug, Clone)]
pub struct Local {
    pub name: String,
    pub ty: Type,
    pub offset: i32, // relative to the frame pointer A6 (negative = locals)
}

pub struct Global {
    pub name: String,
    pub ty: Type,
    /// Initializer bytes (little/big handled by codegen); None = .bss (zeroed).
    pub init: Option<Vec<u8>>,
    pub is_static: bool,
    pub is_extern: bool,
}

pub struct Program {
    pub functions: Vec<Function>,
    pub globals: Vec<Global>,
    pub strings: Vec<Vec<u8>>,
}
