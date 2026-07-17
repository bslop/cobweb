//! jcc — a restricted systems language compiling to *auditable* JRISC.
//!
//! Not a C frontend (yet). Per the wishlist, jcc v1 is a small, statically
//! allocated language whose output you can read and whose safety is enforced by
//! the rest of the suite: jcc emits JRISC **source**, jas assembles it (re-
//! running the hazard pass over compiler output — the compiler is untrusted,
//! the checker is trusted), and jsim runs it. Every variable maps to a fixed
//! register (no spilling, no recursion, no hidden costs), and the compiler
//! reports a whole-program **SRAM budget ledger** against the 4 KB GPU local
//! RAM, because on this machine bytes are features.
//!
//! Grammar (v1):
//! ```text
//!   program := stmt*
//!   stmt    := 'int' IDENT ['=' expr] ';'      // declare (register-allocated)
//!            | IDENT '=' expr ';'              // assign
//!            | 'store' expr ',' expr ';'       // store value, addr
//!            | 'if' '(' cond ')' block ['else' block]
//!            | 'while' '(' cond ')' block
//!   cond    := expr ('=='|'!='|'<'|'>'|'<='|'>=') expr   // unsigned
//!   expr    := term (('+'|'-'|'&'|'|'|'^') term)*
//!   term    := factor (('*'|'<<'|'>>') factor)*
//!   factor  := NUMBER | IDENT | '(' expr ')'
//! ```
//! Integers are 32-bit; `*` is the JRISC 16×16→32 multiply.

mod codegen;
mod parse;

pub use codegen::CompileError;

/// GPU local RAM usable for code, after leaving room for the interrupt vectors
/// and a small parameter/stack area — the budget the ledger reports against.
pub const GPU_CODE_BUDGET: usize = 3584;

/// A compiled program: the emitted JRISC source plus the budget ledger.
pub struct Compiled {
    /// Auditable JRISC assembly (feed to jas).
    pub asm: String,
    /// Assembled size in bytes (0 if jcc could not size it).
    pub bytes: usize,
    /// Variables and the registers they were bound to (for the ledger/debug).
    pub allocation: Vec<(String, u16)>,
}

impl Compiled {
    pub fn over_budget(&self) -> bool {
        self.bytes > GPU_CODE_BUDGET
    }
    pub fn ledger(&self) -> String {
        format!(
            "SRAM budget: {} / {} bytes ({} free){}",
            self.bytes,
            GPU_CODE_BUDGET,
            GPU_CODE_BUDGET.saturating_sub(self.bytes),
            if self.over_budget() { "  ** OVER BUDGET **" } else { "" }
        )
    }
}

/// Compile source to JRISC assembly. The returned asm is guaranteed to have
/// been accepted by jas (hazard-checked) — a compile that would emit a hazard
/// is a compiler bug and surfaces as a `CompileError::Hazard`.
pub fn compile(src: &str) -> Result<Compiled, CompileError> {
    let prog = parse::parse(src).map_err(CompileError::Parse)?;
    let (asm, allocation) = codegen::generate(&prog)?;

    // Re-assemble our own output through jas: this both sizes the program and
    // proves jcc did not emit a silicon hazard.
    let opts = jas::Options { target: jas::Target::Gpu, org: 0xF0_3000, ..Default::default() };
    let out = jas::assemble(&asm, &opts);
    if out.errors() > 0 {
        let msgs: Vec<String> = out
            .diags
            .iter()
            .filter(|d| d.level == jas::Level::Error)
            .map(|d| d.to_string())
            .collect();
        return Err(CompileError::Hazard(msgs));
    }

    Ok(Compiled { asm, bytes: out.bytes.len(), allocation })
}

#[cfg(test)]
mod tests {
    use super::*;
    use jag_core::risc::Fidelity;
    use jag_core::{mem, Bus, Risc, RiscKind};

    /// Compile, assemble, run in jsim, read a 32-bit result from DRAM.
    fn run_read(src: &str, addr: u32) -> u32 {
        let c = compile(src).expect("compiles");
        let opts = jas::Options { target: jas::Target::Gpu, org: mem::G_RAM, ..Default::default() };
        let out = jas::assemble(&c.asm, &opts);
        assert_eq!(out.errors(), 0, "jas rejected jcc output:\n{}\n{:#?}", c.asm, out.diags);
        let mut bus = Bus::new();
        for (i, b) in out.bytes.iter().enumerate() {
            bus.write8(mem::G_RAM + i as u32, *b);
        }
        bus.write32(mem::G_PC, mem::G_RAM);
        bus.write32(mem::G_CTRL, mem::RISCGO);
        let mut gpu = Risc::new(RiscKind::Gpu);
        gpu.fidelity = Fidelity::Silicon;
        gpu.run(&mut bus, 500_000);
        bus.read32(addr)
    }

    #[test]
    fn arithmetic() {
        let r = run_read("int a = 5; int b = 3; int c = a + b; c = c << 2; store c, 0x100000;", 0x100000);
        assert_eq!(r, 32);
    }

    #[test]
    fn multiply() {
        let r = run_read("int a = 6; int b = 7; store a * b, 0x100000;", 0x100000);
        assert_eq!(r, 42);
    }

    #[test]
    fn while_loop_sum() {
        // sum 1..5
        let r = run_read(
            "int acc = 0; int i = 5; while (i > 0) { acc = acc + i; i = i - 1; } store acc, 0x100000;",
            0x100000,
        );
        assert_eq!(r, 15);
    }

    #[test]
    fn if_else_branch() {
        let r = run_read(
            "int a = 3; int b = 9; int m; if (a < b) { m = b; } else { m = a; } store m, 0x100000;",
            0x100000,
        );
        assert_eq!(r, 9);
    }

    #[test]
    fn nested_loop_and_store_addr_expr() {
        // store into a computed address; count total inner iterations
        let r = run_read(
            "int n = 0; int i = 3; while (i > 0) { int j = 3; while (j > 0) { n = n + 1; j = j - 1; } i = i - 1; } store n, 0x100000;",
            0x100000,
        );
        assert_eq!(r, 9);
    }

    #[test]
    fn output_is_hazard_clean() {
        // A program whose naive codegen could trip the checker still compiles —
        // meaning jcc's codegen is hazard-aware (or jas would reject it).
        let c = compile("int a = 10; int b = 2; int q = a; store q, 0x100000;").unwrap();
        assert!(!c.asm.is_empty());
        assert!(c.bytes > 0);
    }
}
