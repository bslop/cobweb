//! jcc code generation: AST → auditable JRISC source.
//!
//! Static register allocation, no spills, no recursion. Variables live in
//! r1–r13; expression evaluation uses a depth-indexed scratch pool (r24–r27);
//! r16/r17 hold comparison operands; r20/r22 stage stores; r29/r30 are the
//! self-stop epilogue. Every jump gets an explicit `nop` delay slot — correct
//! by construction and left for jopt to fill. The output is fed straight back
//! through jas, which re-runs the hazard pass, so any codegen slip surfaces as
//! a compile error rather than silent wrong silicon.

use crate::parse::{Cond, Expr, Op, Rel, Stmt};
use std::collections::HashMap;

/// Compilation failure.
#[derive(Debug)]
pub enum CompileError {
    Parse(String),
    /// jcc emitted code jas rejected as a hazard (a compiler bug).
    Hazard(Vec<String>),
    Codegen(String),
}

impl std::fmt::Display for CompileError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CompileError::Parse(m) => write!(f, "parse error: {m}"),
            CompileError::Codegen(m) => write!(f, "codegen error: {m}"),
            CompileError::Hazard(ms) => {
                writeln!(f, "internal error: jcc emitted a hazard jas rejected:")?;
                for m in ms {
                    writeln!(f, "  {m}")?;
                }
                Ok(())
            }
        }
    }
}
impl std::error::Error for CompileError {}

const VAR_REGS: std::ops::RangeInclusive<u16> = 1..=13;
const SCRATCH: [u16; 4] = [24, 25, 26, 27];
const CMP_L: u16 = 16;
const CMP_R: u16 = 17;
const BR: u16 = 18; // branch-target register for movei+jump control flow
const ST_VAL: u16 = 22;
const ST_ADDR: u16 = 20;

struct Gen {
    out: String,
    vars: HashMap<String, u16>,
    next_var: u16,
    label: usize,
}

impl Gen {
    fn emit(&mut self, s: &str) {
        self.out.push_str("        ");
        self.out.push_str(s);
        self.out.push('\n');
    }
    fn label_line(&mut self, l: &str) {
        self.out.push_str(l);
        self.out.push_str(":\n");
    }
    fn new_label(&mut self) -> String {
        let l = format!("L{}", self.label);
        self.label += 1;
        l
    }

    fn var_reg(&mut self, name: &str) -> Result<u16, CompileError> {
        if let Some(&r) = self.vars.get(name) {
            return Ok(r);
        }
        Err(CompileError::Codegen(format!("use of undeclared variable `{name}`")))
    }

    fn declare(&mut self, name: &str) -> Result<u16, CompileError> {
        if self.vars.contains_key(name) {
            return Err(CompileError::Codegen(format!("variable `{name}` already declared")));
        }
        if !VAR_REGS.contains(&self.next_var) {
            return Err(CompileError::Codegen(
                "out of variable registers (v1 supports 13 live variables)".into(),
            ));
        }
        let r = self.next_var;
        self.next_var += 1;
        self.vars.insert(name.to_string(), r);
        Ok(r)
    }

    /// Load an integer literal into `dst`.
    fn load_imm(&mut self, v: u32, dst: u16) {
        if v <= 31 {
            self.emit(&format!("moveq #{v},r{dst}"));
        } else {
            self.emit(&format!("movei #${v:X},r{dst}"));
        }
    }

    /// Evaluate `e`, leaving the result in `dst`. `depth` indexes the scratch
    /// pool for sub-expressions.
    fn eval(&mut self, e: &Expr, dst: u16, depth: usize) -> Result<(), CompileError> {
        match e {
            Expr::Num(n) => {
                self.load_imm(*n, dst);
                Ok(())
            }
            Expr::Var(name) => {
                let r = self.var_reg(name)?;
                if r != dst {
                    self.emit(&format!("move r{r},r{dst}"));
                }
                Ok(())
            }
            Expr::Bin(op, l, r) => {
                // shift-by-constant uses the quick opcodes (no scratch needed)
                if matches!(op, Op::Shl | Op::Shr) {
                    if let Expr::Num(n) = **r {
                        self.eval(l, dst, depth)?;
                        if n == 0 {
                            return Ok(());
                        }
                        if n > 32 {
                            return Err(CompileError::Codegen("shift count > 32".into()));
                        }
                        let mn = if *op == Op::Shl { "shlq" } else { "shrq" };
                        self.emit(&format!("{mn} #{n},r{dst}"));
                        return Ok(());
                    }
                    return Err(CompileError::Codegen(
                        "shift amount must be a constant in v1".into(),
                    ));
                }
                self.eval(l, dst, depth)?;
                let sc = *SCRATCH.get(depth).ok_or_else(|| {
                    CompileError::Codegen("expression too deeply nested (v1 scratch depth 4)".into())
                })?;
                self.eval(r, sc, depth + 1)?;
                let mn = match op {
                    Op::Add => "add",
                    Op::Sub => "sub",
                    Op::And => "and",
                    Op::Or => "or",
                    Op::Xor => "xor",
                    Op::Mul => "mult", // 16x16 -> 32 unsigned
                    Op::Shl | Op::Shr => unreachable!(),
                };
                self.emit(&format!("{mn} r{sc},r{dst}"));
                Ok(())
            }
        }
    }

    /// Emit the compare for a condition; returns the `jr` condition that SKIPS
    /// the guarded code (exits the loop / skips the then-branch).
    fn compare(&mut self, c: &Cond) -> Result<&'static str, CompileError> {
        self.eval(&c.lhs, CMP_L, 0)?;
        self.eval(&c.rhs, CMP_R, 0)?;
        // cmp rS,rD computes rD - rS. Choose operand order + skip-cc per relop.
        let skip = match c.rel {
            Rel::Eq => {
                self.emit(&format!("cmp r{CMP_R},r{CMP_L}"));
                "ne"
            }
            Rel::Ne => {
                self.emit(&format!("cmp r{CMP_R},r{CMP_L}"));
                "eq"
            }
            Rel::Lt => {
                self.emit(&format!("cmp r{CMP_R},r{CMP_L}")); // L - R, CS if L<R
                "cc"
            }
            Rel::Ge => {
                self.emit(&format!("cmp r{CMP_R},r{CMP_L}"));
                "cs"
            }
            Rel::Gt => {
                self.emit(&format!("cmp r{CMP_L},r{CMP_R}")); // R - L, CS if R<L i.e. L>R
                "cc"
            }
            Rel::Le => {
                self.emit(&format!("cmp r{CMP_L},r{CMP_R}")); // R - L, CC if R>=L i.e. L<=R
                "cs"
            }
        };
        Ok(skip)
    }

    /// Conditional branch to `label` on condition `cc`. Uses movei+jump (no
    /// range limit); the movei sits between the cmp and the jump, which is also
    /// the correct flag-latency spacing. The jump's `nop` slot is explicit.
    fn branch_cond(&mut self, cc: &str, label: &str) {
        self.emit(&format!("movei #{label},r{BR}"));
        self.emit(&format!("jump {cc},(r{BR})"));
        self.emit("nop");
    }

    /// Unconditional branch to `label`.
    fn branch_always(&mut self, label: &str) {
        self.emit(&format!("movei #{label},r{BR}"));
        self.emit(&format!("jump t,(r{BR})"));
        self.emit("nop");
    }

    fn stmt(&mut self, s: &Stmt) -> Result<(), CompileError> {
        match s {
            Stmt::Decl(name, init) => {
                let r = self.declare(name)?;
                if let Some(e) = init {
                    self.eval(e, r, 0)?;
                } else {
                    self.emit(&format!("moveq #0,r{r}"));
                }
                Ok(())
            }
            Stmt::Assign(name, e) => {
                let r = self.var_reg(name)?;
                self.eval(e, r, 0)
            }
            Stmt::Store { val, addr } => {
                self.eval(val, ST_VAL, 0)?;
                self.eval(addr, ST_ADDR, 0)?;
                self.emit(&format!("store r{ST_VAL},(r{ST_ADDR})"));
                Ok(())
            }
            Stmt::If { cond, then, els } => {
                let skip = self.compare(cond)?;
                let l_else = self.new_label();
                self.branch_cond(skip, &l_else);
                for st in then {
                    self.stmt(st)?;
                }
                if let Some(els) = els {
                    let l_end = self.new_label();
                    self.branch_always(&l_end);
                    self.label_line(&l_else);
                    for st in els {
                        self.stmt(st)?;
                    }
                    self.label_line(&l_end);
                } else {
                    self.label_line(&l_else);
                }
                Ok(())
            }
            Stmt::While { cond, body } => {
                let l_start = self.new_label();
                let l_end = self.new_label();
                self.label_line(&l_start);
                let skip = self.compare(cond)?;
                self.branch_cond(skip, &l_end);
                for st in body {
                    self.stmt(st)?;
                }
                self.branch_always(&l_start);
                self.label_line(&l_end);
                Ok(())
            }
        }
    }
}

/// Generate JRISC source for a program. Returns (asm, variable→register map).
pub fn generate(prog: &[Stmt]) -> Result<(String, Vec<(String, u16)>), CompileError> {
    let mut g = Gen { out: String::new(), vars: HashMap::new(), next_var: *VAR_REGS.start(), label: 0 };
    g.out.push_str("; generated by jcc — auditable JRISC (re-checked by jas)\n");
    g.out.push_str("        .gpu\n");
    for s in prog {
        g.stmt(s)?;
    }
    // self-stop epilogue: clear G_CTRL GO so a jsim/hardware harness sees done.
    g.emit("movei #$00F02114,r30");
    g.emit("moveq #0,r29");
    g.emit("store r29,(r30)");
    g.emit("nop");

    let mut alloc: Vec<(String, u16)> = g.vars.iter().map(|(k, v)| (k.clone(), *v)).collect();
    alloc.sort_by_key(|(_, r)| *r);
    Ok((g.out, alloc))
}
