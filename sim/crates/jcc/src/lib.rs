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
//!   cond    := ['signed'] expr ('=='|'!='|'<'|'>'|'<='|'>=') expr
//!   expr    := term (('+'|'-'|'&'|'|'|'^') term)*
//!   term    := factor (('*'|'/'|'<<'|'>>') factor)*
//!   factor  := NUMBER | IDENT | '(' expr ')' | '-' factor
//!            | ('load'|'loadw'|'loadb') '(' expr ')'   // 32/16/8-bit, zero-extended
//!            | ('abs'|'neg') '(' expr ')'
//!            | ('imult'|'sdiv') '(' expr ',' expr ')'
//!            | 'sar' '(' expr ',' NUMBER ')'           // arithmetic shift right
//! ```
//! Integers are 32-bit. Comparisons are unsigned unless prefixed `signed`
//! (`if (signed x < 0)`). `*` is JRISC `mult`, the UNSIGNED 16×16→32 multiply
//! (low halves of both operands); `imult` is the signed one. `/` is JRISC
//! `div`, unsigned 32/32; `sdiv` divides signed, truncating toward zero. A
//! program that divides first clears G_DIVCTRL (integer mode). Shift counts
//! are constants. Loads and divides are followed by a read of their result,
//! so nothing can overwrite a register while its load or divide is in flight.
//! A byte/word load from a constant GPU/DSP local-RAM address is refused:
//! silicon takes 32-bit accesses only there. Indexed access is address
//! arithmetic: `load(table + (i << 2))`.

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
        run_with(src, &[], addr)
    }

    /// `run_read` with DRAM longs preset, asserting the silicon hazard
    /// counters stay at zero (jcc output must be right on hardware, not
    /// just in jsim).
    fn run_with(src: &str, preset: &[(u32, u32)], addr: u32) -> u32 {
        let c = compile(src).expect("compiles");
        let opts = jas::Options { target: jas::Target::Gpu, org: mem::G_RAM, ..Default::default() };
        let out = jas::assemble(&c.asm, &opts);
        assert_eq!(out.errors(), 0, "jas rejected jcc output:\n{}\n{:#?}", c.asm, out.diags);
        let mut bus = Bus::new();
        for &(a, v) in preset {
            bus.write32(a, v);
        }
        for (i, b) in out.bytes.iter().enumerate() {
            bus.write8(mem::G_RAM + i as u32, *b);
        }
        bus.write32(mem::G_PC, mem::G_RAM);
        bus.write32(mem::G_CTRL, mem::RISCGO);
        let mut gpu = Risc::new(RiscKind::Gpu);
        gpu.fidelity = Fidelity::Silicon;
        gpu.run(&mut bus, 500_000);
        let t = &gpu.pipe.stats;
        assert_eq!(
            (t.waw_hazards, t.indexed_store_stale, t.slot_movei, t.slot_jump),
            (0, 0, 0, 0),
            "silicon hazard counters (waw, indexed_store_stale, slot_movei, slot_jump):\n{}",
            c.asm
        );
        bus.read32(addr)
    }

    const OUT: u32 = 0x100000;

    #[test]
    fn loads_of_each_width() {
        // a parameter block in DRAM: read it at 32, 16 and 8 bits
        let p = [(0x180000, 0x1234_5678), (0x180004, 0xCAFE_BEEF)];
        assert_eq!(run_with("store load(0x180004), 0x100000;", &p, OUT), 0xCAFE_BEEF);
        assert_eq!(run_with("store loadw(0x180002), 0x100000;", &p, OUT), 0x5678);
        assert_eq!(run_with("store loadb(0x180004), 0x100000;", &p, OUT), 0xCA);
        // indexed access: sum a 4-entry table
        let t = [(0x180000, 1), (0x180004, 20), (0x180008, 300), (0x18000C, 4000)];
        let sum = "int s = 0; int i = 0; while (i < 4) { s = s + load(0x180000 + (i << 2)); i = i + 1; } \
                   store s, 0x100000;";
        assert_eq!(run_with(sum, &t, OUT), 4321);
    }

    #[test]
    fn load_result_is_never_overwritten_in_flight() {
        // the register is rewritten right after the load: without the
        // compiler's read, that write races the load (bug 13 class)
        let src = "int x = load(0x180000); x = 5; store x, 0x100000;";
        assert_eq!(run_with(src, &[(0x180000, 77)], OUT), 5);
        let src = "int x = load(0x180000); int y = x + 1; store y, 0x100000;";
        assert_eq!(run_with(src, &[(0x180000, 77)], OUT), 78);
    }

    #[test]
    fn narrow_load_from_local_ram_is_refused() {
        for src in ["store loadb($F03100), 0x100000;", "store loadw($F1B000), 0x100000;"] {
            let e = compile(src).err().expect("must be refused").to_string();
            assert!(e.contains("32-bit accesses only"), "{e}");
        }
        assert!(compile("store load($F03100), 0x100000;").is_ok(), "32-bit is fine");
    }

    #[test]
    fn signed_comparisons() {
        let check = |rel: &str, a: i32, b: i32| {
            let src = format!(
                "int a = {}; int b = {}; int r = 0; if (signed a {rel} b) {{ r = 1; }} store r, 0x100000;",
                a as u32, b as u32
            );
            run_read(&src, OUT) == 1
        };
        for &(a, b) in &[(-3, 2), (2, -3), (-3, -3), (0, -1), (i32::MIN, i32::MAX), (i32::MAX, i32::MIN), (5, 9)] {
            assert_eq!(check("<", a, b), a < b, "{a} < {b}");
            assert_eq!(check("<=", a, b), a <= b, "{a} <= {b}");
            assert_eq!(check(">", a, b), a > b, "{a} > {b}");
            assert_eq!(check(">=", a, b), a >= b, "{a} >= {b}");
        }
        // unsigned stays the default: -3 is a huge number
        let src = "int a = 0; a = a - 3; int r = 0; if (a < 2) { r = 1; } store r, 0x100000;";
        assert_eq!(run_read(src, OUT), 0);
    }

    #[test]
    fn unsigned_divide() {
        assert_eq!(run_read("int a = 1000; int b = 7; store a / b, 0x100000;", OUT), 142);
        // the quotient is consumed and its register rewritten straight away
        let src = "int a = 100000; int q = a / 3; q = q + 1; store q, 0x100000;";
        assert_eq!(run_read(src, OUT), 33334);
        // a stale integer/fraction mode from a previous kernel is cleared
        let src = "store $FFFFFFFF / 16, 0x100000;";
        assert_eq!(run_with(src, &[(jag_core::mem::G_DIVCTRL, 1)], OUT), 0x0FFF_FFFF);
    }

    #[test]
    fn signed_divide_and_helpers() {
        let sdiv = |a: i32, b: i32| {
            run_read(&format!("store sdiv({}, {}), 0x100000;", a as u32, b as u32), OUT) as i32
        };
        for &(a, b) in &[(-7, 2), (7, -2), (-7, -2), (7, 2), (0, -5), (i32::MIN, 1), (-100, 7)] {
            assert_eq!(sdiv(a, b), a.wrapping_div(b), "sdiv({a}, {b})");
        }
        assert_eq!(run_read("int a = 0; a = a - 9; store abs(a), 0x100000;", OUT), 9);
        assert_eq!(run_read("int a = 9; store neg(a), 0x100000;", OUT) as i32, -9);
        assert_eq!(run_read("int a = 9; store -a + 2, 0x100000;", OUT) as i32, -7);
        assert_eq!(run_read("int a = 0; a = a - 3; store imult(a, 5), 0x100000;", OUT) as i32, -15);
        assert_eq!(run_read("int a = 3; int b = 5; store a * b, 0x100000;", OUT), 15);
        assert_eq!(run_read("int a = 0; a = a - 16; store sar(a, 2), 0x100000;", OUT) as i32, -4);
        assert_eq!(run_read("int a = 0; a = a - 16; store a >> 2, 0x100000;", OUT), 0x3FFF_FFFC);
    }

    #[test]
    fn assignment_reading_its_own_target() {
        // `x = y - x` used to build y in x's register first, reading back 0
        let src = "int x = 3; int y = 10; x = y - x; store x, 0x100000;";
        assert_eq!(run_read(src, OUT), 7);
        let src = "int x = 3; int y = 10; x = y - (x << 1); store x, 0x100000;";
        assert_eq!(run_read(src, OUT), 4);
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
