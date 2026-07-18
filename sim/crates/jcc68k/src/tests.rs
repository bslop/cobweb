//! End-to-end tests: compile C → assemble with jas → run in jsim's 68000 →
//! check the result. The startup stashes `main`'s return value at address $100.

use jag_core::Jaguar;

/// Compile+assemble+run `src`, returning `main`'s 32-bit return value.
fn run(src: &str) -> u32 {
    let asm = crate::compile_program(src).unwrap_or_else(|e| panic!("compile error: {e}"));
    let org = 0x4000u32;
    let opts = jas::Options { org, start_m68k: true, check_hazards: false, ..Default::default() };
    let res = jas::assemble(&asm, &opts);
    if res.errors() > 0 {
        panic!("assembly errors:\n{:#?}\n--- asm ---\n{}", res.diags, asm);
    }
    let mut jag = Jaguar::new();
    for (i, b) in res.bytes.iter().enumerate() {
        jag.bus.write8(org + i as u32, *b);
    }
    let start = res.symbols.get("_start").copied().unwrap_or(org);
    jag.cpu.set_pc(start);
    // Run until the startup's halt spin (a bra to itself leaves PC unchanged).
    let trace = std::env::var("JCCTRACE").is_ok();
    let mut steps = 0u64;
    let mut prev = u32::MAX;
    loop {
        let pc = jag.cpu.pc;
        if pc == prev {
            break; // bra-to-self spin
        }
        prev = pc;
        if trace && (8..24).contains(&steps) {
            eprintln!(
                "  [{steps:3}] pc={:06X} d0={:08X} d1={:08X} d2={:08X} d3={:08X} d4={:08X}",
                pc, jag.cpu.d[0], jag.cpu.d[1], jag.cpu.d[2], jag.cpu.d[3], jag.cpu.d[4]
            );
        }
        jag.step_instruction();
        steps += 1;
        if steps > 5_000_000 {
            break;
        }
    }
    if std::env::var("JCCDBG").is_ok() {
        eprintln!(
            "DBG steps={steps} final_pc={:06X} d0={:08X} illegal={} sym_start={start:06X} sym_main={:?} $100={:08X}",
            jag.cpu.pc, jag.cpu.d[0], jag.cpu.illegal_count,
            res.symbols.get("_main"), jag.bus.read32(0x100)
        );
    }
    jag.bus.read32(0x100)
}

fn wrap(body: &str) -> String {
    format!("int main() {{ {body} }}")
}

#[test]
fn ret_const() {
    assert_eq!(run(&wrap("return 42;")), 42);
}

#[test]
fn arithmetic_precedence() {
    assert_eq!(run(&wrap("return 2 + 3 * 4;")), 14);
    assert_eq!(run(&wrap("return (2 + 3) * 4;")), 20);
    assert_eq!(run(&wrap("return 100 - 30 - 20;")), 50);
    assert_eq!(run(&wrap("return 17 % 5;")), 2);
    assert_eq!(run(&wrap("return 17 / 5;")), 3);
}

#[test]
fn locals_and_assign() {
    assert_eq!(run(&wrap("int x = 5; int y = 7; return x * y;")), 35);
    assert_eq!(run(&wrap("int a = 3; a = a + 10; a += 4; return a;")), 17);
}

#[test]
fn big_multiply() {
    // 32-bit multiply through __mulsi3
    assert_eq!(run(&wrap("return 12345 * 6789;")), 12345u32.wrapping_mul(6789));
    assert_eq!(run(&wrap("int a = 100000; int b = 40000; return a * b;")), 100000u32.wrapping_mul(40000));
}

#[test]
fn division_and_mod() {
    assert_eq!(run(&wrap("return 1000000 / 7;")), 142857);
    assert_eq!(run(&wrap("return 1000000 % 7;")), 1);
    assert_eq!(run(&wrap("int a = -20; int b = 3; return a / b;")), (-20i32 / 3) as u32);
    assert_eq!(run(&wrap("int a = -20; int b = 3; return a % b;")), (-20i32 % 3) as u32);
}

#[test]
fn comparisons() {
    assert_eq!(run(&wrap("return 5 < 10;")), 1);
    assert_eq!(run(&wrap("return 5 > 10;")), 0);
    assert_eq!(run(&wrap("return 5 == 5;")), 1);
    assert_eq!(run(&wrap("return 5 != 5;")), 0);
    assert_eq!(run(&wrap("return 5 >= 5;")), 1);
}

#[test]
fn control_flow_if() {
    assert_eq!(run(&wrap("int x = 7; if (x > 5) return 1; else return 2;")), 1);
    assert_eq!(run(&wrap("int x = 3; if (x > 5) return 1; else return 2;")), 2);
}

#[test]
fn control_flow_while() {
    assert_eq!(run(&wrap("int i = 0; int s = 0; while (i < 10) { s = s + i; i = i + 1; } return s;")), 45);
}

#[test]
fn control_flow_for() {
    assert_eq!(run(&wrap("int s = 0; int i; for (i = 1; i <= 100; i = i + 1) s = s + i; return s;")), 5050);
}

#[test]
fn functions_and_recursion() {
    let src = r#"
        int fact(int n) { if (n <= 1) return 1; return n * fact(n - 1); }
        int main() { return fact(5); }
    "#;
    assert_eq!(run(src), 120);
}

#[test]
fn fibonacci() {
    let src = r#"
        int fib(int n) { if (n < 2) return n; return fib(n-1) + fib(n-2); }
        int main() { return fib(10); }
    "#;
    assert_eq!(run(src), 55);
}

#[test]
fn pointers() {
    assert_eq!(run(&wrap("int x = 41; int *p = &x; *p = *p + 1; return x;")), 42);
}

#[test]
fn arrays() {
    let src = r#"
        int main() {
            int a[5];
            int i;
            for (i = 0; i < 5; i = i + 1) a[i] = i * i;
            return a[0] + a[1] + a[2] + a[3] + a[4];
        }
    "#;
    assert_eq!(run(src), 0 + 1 + 4 + 9 + 16);
}

#[test]
fn logical_ops() {
    assert_eq!(run(&wrap("return (1 && 1) + (1 && 0) + (0 || 1) + (0 || 0);")), 2);
    assert_eq!(run(&wrap("return !0 + !5;")), 1);
}

#[test]
fn globals() {
    let src = r#"
        int counter;
        int bump() { counter = counter + 1; return counter; }
        int main() { bump(); bump(); return bump(); }
    "#;
    assert_eq!(run(src), 3);
}

#[test]
fn shifts_and_bitops() {
    assert_eq!(run(&wrap("return (1 << 8) | 0xF;")), 271);
    assert_eq!(run(&wrap("return (0xFF00 >> 4) & 0xFF;")), 0xF0);
    assert_eq!(run(&wrap("return 0xAA ^ 0xFF;")), 0x55);
}

#[test]
fn div_small() {
    assert_eq!(run(&wrap("return 10 / 2;")), 5);
    assert_eq!(run(&wrap("return 100 / 7;")), 14);
    assert_eq!(run(&wrap("return 6 / 3;")), 2);
    assert_eq!(run(&wrap("return 7 / 1;")), 7);
}
