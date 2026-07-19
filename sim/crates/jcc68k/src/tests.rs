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
            res.symbols.get("main"), jag.bus.read32(0x100)
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

// ── preprocessor ─────────────────────────────────────────────────────────────
fn run_pp(src: &str) -> u32 {
    let dir = std::env::temp_dir().join(format!("jcc_pp_{}", std::process::id()));
    let _ = std::fs::create_dir_all(&dir);
    let main_c = dir.join("main.c");
    std::fs::write(&main_c, src).unwrap();
    let inc = vec![dir.to_string_lossy().to_string()];
    let user = crate::compile_file(src, &main_c, &inc).unwrap_or_else(|e| panic!("compile: {e}"));
    let asm = format!("{}\n{}\n{}", crate::startup(), user, crate::runtime());
    let opts = jas::Options { org: 0x4000, start_m68k: true, check_hazards: false, ..Default::default() };
    let res = jas::assemble(&asm, &opts);
    if res.errors() > 0 {
        panic!("asm errors:\n{:#?}\n{asm}", res.diags);
    }
    let mut jag = Jaguar::new();
    for (i, b) in res.bytes.iter().enumerate() {
        jag.bus.write8(0x4000 + i as u32, *b);
    }
    jag.cpu.set_pc(res.symbols.get("_start").copied().unwrap_or(0x4000));
    let mut prev = u32::MAX;
    for _ in 0..5_000_000 {
        let pc = jag.cpu.pc;
        if pc == prev { break; }
        prev = pc;
        jag.step_instruction();
    }
    jag.bus.read32(0x100)
}

#[test]
fn pp_object_macro() {
    assert_eq!(run_pp("#define N 5\n#define M 7\nint main(){ return N*M; }"), 35);
}

#[test]
fn pp_function_macro() {
    assert_eq!(run_pp("#define SQ(x) ((x)*(x))\nint main(){ return SQ(6) + SQ(2); }"), 40);
    assert_eq!(run_pp("#define MAX(a,b) ((a)>(b)?(a):(b))\nint main(){ return MAX(3,9); }"), 9);
}

#[test]
fn pp_conditionals() {
    let src = "#define FEATURE 1\n#if FEATURE\nint main(){ return 111; }\n#else\nint main(){ return 222; }\n#endif";
    assert_eq!(run_pp(src), 111);
    let src2 = "#ifdef NOPE\nint main(){ return 1; }\n#else\nint main(){ return 42; }\n#endif";
    assert_eq!(run_pp(src2), 42);
}

#[test]
fn ppinclude() {
    let dir = std::env::temp_dir().join(format!("jcc_pp_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("hdr.h"), "#define ANSWER 42\nint helper(int x){ return x + ANSWER; }\n").unwrap();
    assert_eq!(run_pp("#include \"hdr.h\"\nint main(){ return helper(8); }"), 50);
}

#[test]
fn pp_nested_macro() {
    let src = "#define A 2\n#define B (A+3)\n#define C (B*B)\nint main(){ return C; }";
    assert_eq!(run_pp(src), 25);
}

#[test]
fn structs_end_to_end() {
    let src = r#"
        struct Point { int x; int y; };
        int dist2(struct Point *a, struct Point *b) {
            int dx = a->x - b->x;
            int dy = a->y - b->y;
            return dx*dx + dy*dy;
        }
        int main() {
            struct Point p; p.x = 10; p.y = 20;
            struct Point q; q.x = 13; q.y = 24;
            return dist2(&p, &q);
        }
    "#;
    assert_eq!(run(src), 25);
}

#[test]
fn struct_array_and_typedef() {
    let src = r#"
        typedef struct { int lo; int hi; } Range;
        int main() {
            Range r[3];
            int i;
            int sum = 0;
            for (i = 0; i < 3; i++) { r[i].lo = i; r[i].hi = i * 10; }
            for (i = 0; i < 3; i++) sum += r[i].hi - r[i].lo;
            return sum;
        }
    "#;
    assert_eq!(run(src), (0 + 9 + 18));
}

#[test]
fn switch_stmt() {
    let f = |n: i32| format!("int main(){{ int x={n}; int r=0; switch(x){{ case 1: r=10; break; case 2: r=20; break; case 3: r=30; break; default: r=99; }} return r; }}");
    assert_eq!(run(&f(1)), 10);
    assert_eq!(run(&f(2)), 20);
    assert_eq!(run(&f(3)), 30);
    assert_eq!(run(&f(7)), 99);
}

#[test]
fn switch_fallthrough() {
    let src = "int main(){ int x=1; int r=0; switch(x){ case 1: r+=1; case 2: r+=2; case 3: r+=4; break; case 4: r+=8; } return r; }";
    assert_eq!(run(src), 7);
}

#[test]
fn array_initializer() {
    let src = "int main(){ int a[5] = {10,20,30}; return a[0]+a[1]+a[2]+a[3]+a[4]; }";
    assert_eq!(run(src), 60); // 10+20+30+0+0
}

#[test]
fn struct_initializer() {
    let src = "struct P{int x;int y;int z;}; int main(){ struct P p = {3,4,5}; return p.x*100+p.y*10+p.z; }";
    assert_eq!(run(src), 345);
}

#[test]
fn nested_array_init() {
    let src = "int main(){ int m[2][3] = {{1,2,3},{4,5,6}}; return m[0][0]+m[0][2]+m[1][1]+m[1][2]; }";
    assert_eq!(run(src), 1+3+5+6);
}

#[test]
fn goto_stmt() {
    let src = "int main(){ int i=0; int s=0; loop: if(i<5){ s+=i; i++; goto loop; } return s; }";
    assert_eq!(run(src), 10);
}

// ── fixed-point (16.16) ──────────────────────────────────────────────────────
#[test]
fn fixed_basic() {
    assert_eq!(run("int main(){ float f = 2.5; float g = 4.0; return (int)(f*g); }"), 10);
    assert_eq!(run("int main(){ float a = 1.5; float b = 2.25; return (int)((a+b)*100); }"), 375);
    assert_eq!(run("int main(){ float f = 3.0; float g = 2.0; return (int)(f/g*1000); }"), 1500);
}

#[test]
fn fixed_int_mix() {
    // int promoted to fixed
    assert_eq!(run("int main(){ float f = 3; return (int)(f * 2); }"), 6);
    assert_eq!(run("int main(){ float half = 1; half = half / 2; return (int)(half * 1000); }"), 500);
}

#[test]
fn fixed_raw_repr() {
    // 0.5 in 16.16 is 0x8000 = 32768
    assert_eq!(run("int main(){ float f = 0.5; return f; }"), 32768);
    // 1.0 is 0x10000 = 65536
    assert_eq!(run("int main(){ float f = 1.0; return f; }"), 65536);
}

#[test]
fn fixed_compare() {
    assert_eq!(run("int main(){ float a = 3.14; float b = 2.71; return a > b; }"), 1);
    assert_eq!(run("int main(){ float a = 1.5; return a < 1.0; }"), 0);
}

#[test]
fn fixed_negative() {
    assert_eq!(run("int main(){ float a = -2.5; float b = 4.0; return (int)(a*b); }"), (-10i32) as u32);
    assert_eq!(run("int main(){ float a = -6.0; float b = 2.0; return (int)(a/b); }"), (-3i32) as u32);
}

#[test]
fn union_overlap() {
    let src = r#"
        union U { int i; char c[4]; };
        int main() {
            union U u;
            u.i = 0x41424344;
            return u.c[0] + u.c[3];  // big-endian: c[0]=0x41, c[3]=0x44
        }
    "#;
    assert_eq!(run(src), 0x41 + 0x44);
}

#[test]
fn function_pointers() {
    let src = r#"
        int add(int a, int b) { return a + b; }
        int mul(int a, int b) { return a * b; }
        int apply(int (*f)(int,int), int x, int y) { return f(x, y); }
        int main() {
            int (*fp)(int,int) = add;
            int r1 = fp(3, 4);
            r1 = apply(mul, 5, 6);
            return fp(3,4) + r1;  // 7 + 30
        }
    "#;
    assert_eq!(run(src), 37);
}

#[test]
fn static_local() {
    let src = r#"
        int counter() { static int n = 0; n = n + 1; return n; }
        int main() { counter(); counter(); return counter(); }
    "#;
    assert_eq!(run(src), 3);
}

// ── end-to-end linking: C (jcc68k) + GAS asm (jas) → jln → jsim ──────────────

/// Compile a C unit that calls an external function, assemble a GAS-syntax 68k
/// helper separately, link the two relocatable objects with jln, and run the
/// linked image in jsim — the full Cobweb pipeline in one shot.
fn run_linked(main_src: &str, helper_gas: &str) -> u32 {
    // main unit (startup + user + runtime) as an object at $4000
    let main_asm = crate::compile_program(main_src).unwrap_or_else(|e| panic!("compile: {e}"));
    let main_opts = jas::Options {
        org: 0x4000,
        start_m68k: true,
        check_hazards: false,
        object_mode: true,
        ..Default::default()
    };
    let mr = jas::assemble(&main_asm, &main_opts);
    assert_eq!(mr.errors(), 0, "main asm errors: {:#?}", mr.diags);
    let main_obj = mr.object(0x4000);

    // helper unit (GAS dialect) as an object at $8000
    let help_opts = jas::Options {
        org: 0x8000,
        start_m68k: true,
        check_hazards: false,
        object_mode: true,
        gas: Some(true),
        ..Default::default()
    };
    let hr = jas::assemble(helper_gas, &help_opts);
    assert_eq!(hr.errors(), 0, "helper asm errors: {:#?}", hr.diags);
    let help_obj = hr.object(0x8000);

    let img = jln::link(&[main_obj, help_obj]).expect("link failed");
    let mut jag = Jaguar::new();
    for (i, b) in img.bytes.iter().enumerate() {
        jag.bus.write8(img.base + i as u32, *b);
    }
    let start = img.symbols.get("_start").copied().unwrap_or(img.base);
    jag.cpu.set_pc(start);
    let mut prev = u32::MAX;
    let mut steps = 0u64;
    loop {
        let pc = jag.cpu.pc;
        if pc == prev {
            break;
        }
        prev = pc;
        jag.step_instruction();
        steps += 1;
        if steps > 5_000_000 {
            break;
        }
    }
    jag.bus.read32(0x100)
}

#[test]
fn link_c_calls_gashelper() {
    // helper(x) = x * 2, written in GNU-as syntax (`%sp`, `|` comment).
    let helper = "\
        \t.text\n\
        \t.globl helper\n\
        helper:\n\
        \tmove.l 4(%sp),%d0  | argument\n\
        \tadd.l %d0,%d0      | x + x\n\
        \trts\n";
    let main = "extern int helper(int x); int main() { return helper(21); }";
    assert_eq!(run_linked(main, helper), 42);
}

#[test]
fn link_c_calls_gashelper_with_numeric_locals() {
    // sum(n) = n + (n-1) + ... + 1, via a GAS loop using a numeric local label.
    let helper = "\
        \t.text\n\
        \t.globl sumto\n\
        sumto:\n\
        \tmove.l 4(%sp),%d1  | n\n\
        \tmoveq #0,%d0\n\
        1:\ttst.l %d1\n\
        \tble.s 2f\n\
        \tadd.l %d1,%d0\n\
        \tsubq.l #1,%d1\n\
        \tbra.s 1b\n\
        2:\trts\n";
    let main = "extern int sumto(int n); int main() { return sumto(10); }";
    assert_eq!(run_linked(main, helper), 55);
}

/// Relocating link: assemble every object as position-independent (`relocatable`
/// at a nominal org 0) and let jln ASSIGN addresses (`Layout::base`), rebasing
/// each object's symbols and absolute relocations. No manual orgs.
fn run_autoplaced(main_src: &str, helpers: &[&str]) -> u32 {
    let asm_reloc = |src: &str, gas: bool| {
        let opts = jas::Options {
            org: 0,
            start_m68k: true,
            check_hazards: false,
            object_mode: true,
            relocatable: true,
            gas: gas.then_some(true),
            ..Default::default()
        };
        let r = jas::assemble(src, &opts);
        assert_eq!(r.errors(), 0, "asm errors: {:#?}", r.diags);
        r.object(0)
    };
    let main_asm = crate::compile_program(main_src).unwrap_or_else(|e| panic!("compile: {e}"));
    let mut objs = vec![asm_reloc(&main_asm, false)];
    for h in helpers {
        objs.push(asm_reloc(h, true));
    }
    let layout = jln::Layout { base: Some(0x4000), align: 4, entry: Some("_start".into()), ..Default::default() };
    let img = jln::link_with(&objs, &layout).expect("relocating link failed");
    let mut jag = Jaguar::new();
    for (i, b) in img.bytes.iter().enumerate() {
        jag.bus.write8(img.base + i as u32, *b);
    }
    jag.cpu.set_pc(img.entry);
    let mut prev = u32::MAX;
    let mut steps = 0u64;
    loop {
        let pc = jag.cpu.pc;
        if pc == prev {
            break;
        }
        prev = pc;
        jag.step_instruction();
        steps += 1;
        if steps > 5_000_000 {
            break;
        }
    }
    jag.bus.read32(0x100)
}

#[test]
fn relocating_link_assigns_addresses() {
    // main calls two separately-assembled GAS helpers; the linker places all
    // three objects and rebases every cross- and intra-object absolute reference.
    let dbl = "\t.text\n\t.globl dbl\ndbl:\n\tmove.l 4(%sp),%d0\n\tadd.l %d0,%d0\n\trts\n";
    let inc = "\t.text\n\t.globl inc\ninc:\n\tmove.l 4(%sp),%d0\n\taddq.l #1,%d0\n\trts\n";
    let main = "extern int dbl(int); extern int inc(int); \
                int main() { return dbl(20) + inc(1); }";
    assert_eq!(run_autoplaced(main, &[dbl, inc]), 42);
}

// ── register eval stack: deep nesting (spill) + call survival ────────────────

#[test]
fn deep_expression_spills_correctly() {
    // 10-deep left-nested sum forces the data eval stack past d2–d7 into the
    // memory spill path; the result must still be exact.
    let src = "int main() { int a=1,b=2,c=3,d=4,e=5,f=6,g=7,h=8,i=9,j=10;\
               return ((((((((a+b)+c)+d)+e)+f)+g)+h)+i)+j; }";
    assert_eq!(run(src), 55);
}

#[test]
fn calls_inside_expression_preserve_temps() {
    // Operands held in callee-saved temp registers must survive the calls in
    // sibling sub-expressions. id(x)=x, so this is 3*100 + 4*100 = 700 with the
    // held partial products interleaved with calls.
    let src = "int id(int x){ return x; }\
               int main(){ return id(3)*id(100) + id(4)*id(100); }";
    assert_eq!(run(src), 700);
}

#[test]
fn nested_assign_and_incdec_in_expr() {
    // Assign holds the dest address in an address temp across the rhs eval;
    // post-inc holds the old value across the store. Sequenced to avoid relying
    // on operand evaluation order.
    let src = "int main(){ int x=10, y=20; x = y + 3; int p = y++; return x*1000 + y*10 + p; }";
    // x = 23; p = 20 (old y), y = 21  →  23000 + 210 + 20 = 23230
    assert_eq!(run(src), 23 * 1000 + 21 * 10 + 20);
}

// ── local variable register allocation ──────────────────────────────────────

#[test]
fn many_register_locals() {
    // 8 hot scalar locals — more than the register pool; the overflow must fall
    // back to the frame and still compute correctly.
    let src = "int main(){ int a=1,b=2,c=3,d=4,e=5,f=6,g=7,h=8;\
               int i; for(i=0;i<3;i++){ a+=b; b+=c; c+=d; d+=e; e+=f; f+=g; g+=h; h+=a; }\
               return a+b+c+d+e+f+g+h; }";
    // mirror the loop in Rust
    let (mut a,mut b,mut c,mut d,mut e,mut f,mut g,mut h)=(1i32,2,3,4,5,6,7,8);
    for _ in 0..3 { a+=b; b+=c; c+=d; d+=e; e+=f; f+=g; g+=h; h+=a; }
    assert_eq!(run(src), (a+b+c+d+e+f+g+h) as u32);
}

#[test]
fn register_local_survives_call() {
    // A register-allocated accumulator must survive calls inside the loop.
    let src = "int inc(int x){ return x+1; }\
               int main(){ int s=0; int i; for(i=0;i<5;i++) s = s + inc(i); return s; }";
    // s = sum(inc(0..4)) = (1+2+3+4+5) = 15
    assert_eq!(run(src), 15);
}

#[test]
fn address_taken_local_stays_in_memory() {
    // `&x` forces x into a frame slot; writing through the pointer must be seen
    // by a later read of x (would fail if x were wrongly kept in a register).
    let src = "int main(){ int x=5; int i; int *p=&x;\
               for(i=0;i<10;i++) *p = *p + 1; return x; }";
    assert_eq!(run(src), 15);
}
