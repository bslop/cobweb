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

// ── runtime-helper ABI ───────────────────────────────────────────────────────

#[test]
fn runtime_calls_use_libgcc_stack_abi() {
    // jcc68k emits calls to libgcc-NAMED helpers, so a project may satisfy
    // them with libgcc itself or a drop-in (OpenLara's divmod68k.S). Those
    // read operands from the STACK. Link compiled code against a foreign
    // stack-ABI __mulsi3 (NOT our runtime) — if codegen passed args in
    // registers this returns garbage, which is exactly the gpu.c/jerry.c
    // black-screen miscompile from the adoption report round 2.
    let user = crate::compile("int mul(int a, int b) { return a * b; }").unwrap();
    let foreign_mulsi3 = "\
	.68000\n\
	.text\n\
	.globl __mulsi3\n\
__mulsi3:\n\
	move.w	4(a7),d0\n\
	mulu.w	10(a7),d0\n\
	move.w	6(a7),d1\n\
	mulu.w	8(a7),d1\n\
	add.w	d1,d0\n\
	swap	d0\n\
	clr.w	d0\n\
	move.w	6(a7),d1\n\
	mulu.w	10(a7),d1\n\
	add.l	d1,d0\n\
	rts\n";
    let asm = format!(
        "{}\n{}\n{}",
        "\t.68000\n\t.text\n\t.globl _start\n_start:\n\tmovea.l #$001F0000,a7\n\
         \tmove.l #7,-(a7)\n\tmove.l #-6,-(a7)\n\tjsr mul\n\taddq.l #8,a7\n\
         \tmove.l d0,$100\nhalt:\n\tbra.w halt\n",
        user, foreign_mulsi3
    );
    let opts = jas::Options { org: 0x4000, start_m68k: true, check_hazards: false, ..Default::default() };
    let res = jas::assemble(&asm, &opts);
    assert_eq!(res.errors(), 0, "asm errors:\n{:#?}\n{asm}", res.diags);
    let mut jag = Jaguar::new();
    for (i, b) in res.bytes.iter().enumerate() {
        jag.bus.write8(0x4000 + i as u32, *b);
    }
    jag.cpu.set_pc(res.symbols.get("_start").copied().unwrap_or(0x4000));
    let mut prev = u32::MAX;
    for _ in 0..100_000 {
        if jag.cpu.pc == prev {
            break;
        }
        prev = jag.cpu.pc;
        jag.step_instruction();
    }
    assert_eq!(jag.bus.read32(0x100) as i32, -42, "-6 * 7 through the foreign helper");
}

// ── inline asm (adoption report round 2, item 1) ─────────────────────────────

#[test]
fn basic_asm_passes_through() {
    // A basic asm statement must reach the output (GNU % prefixes normalized)
    // — it was silently dropped, deleting an interrupt-enable from a port.
    let asm = crate::compile("int f(void) { __asm__ volatile (\"moveq #77,%d0\"); }").unwrap();
    assert!(asm.contains("moveq #77,d0"), "asm text missing/unnormalized:\n{asm}");
    // and it executes: d0 is the return register, so f() == 77
    assert_eq!(run("int f(void){ asm(\"moveq #77,%d0\"); }\nint main(){ return f(); }"), 77);
}

#[test]
fn extended_asm_muls_idiom_works() {
    // The corpus hot-path idiom: hardware 16x16 multiply via "+d"/"d"
    // operands (OpenLara main.c mul16 — the pose loop's dominant cost).
    let src = "\
static int mul16(int a, int b) {\n\
    __asm__(\"muls.w %1,%0\" : \"+d\"(a) : \"d\"(b));\n\
    return a;\n\
}\n\
int main() { return mul16(-320, 100); }\n";
    assert_eq!(run(src) as i32, -32000);
}

#[test]
fn unsupported_extended_asm_is_a_hard_error() {
    // More operands than the supported %0/%1 subset must error, not drop.
    let err = crate::compile(
        "int f(int x,int y,int z){ asm(\"add %2,%0\" : \"+d\"(x) : \"d\"(y), \"d\"(z)); return x; }",
    )
    .unwrap_err();
    assert!(err.contains("at most one output"), "got: {err}");
    // memory-constraint outputs are not supported — error, not drop
    let err2 = crate::compile("int f(int x){ asm(\"clr.l %0\" : \"=m\"(x)); return x; }")
        .unwrap_err();
    assert!(err2.contains("constraint"), "got: {err2}");
}

#[test]
fn asm_label_still_renames() {
    // The declarator form is a symbol rename, not a statement — must keep working.
    let asm = crate::compile(
        "extern int counter __asm__(\"hw_counter\");\nint get(void){ return counter; }",
    )
    .unwrap();
    assert!(asm.contains("hw_counter"), "asm label lost:\n{asm}");
}

// ── volatile locals (register promotion must not cache them) ─────────────────

#[test]
fn volatile_local_stays_in_memory() {
    // A volatile delay-loop counter promoted to a data register collapses the
    // delay the code was written for — it must live in the frame.
    let asm = crate::compile(
        "void delay(void){ volatile int d; for (d = 0; d < 40; d++) ; }",
    )
    .unwrap();
    assert!(asm.contains("(a6)"), "volatile local was register-promoted:\n{asm}");
}

// ── globals: .bss vs .data (round 2, item 3) ─────────────────────────────────

#[test]
fn zero_globals_land_in_bss() {
    let asm = crate::compile(
        "int hot = 5;\nstatic int cold_zero = 0;\nint cold_uninit[64];\n\
         int use(void){ return hot + cold_zero + cold_uninit[0]; }",
    )
    .unwrap();
    let bss = asm.split("\t.bss").nth(1).expect("no .bss section");
    assert!(bss.contains("cold_zero"), "zero-initialized static not in .bss:\n{asm}");
    assert!(bss.contains("cold_uninit"), "uninitialized global not in .bss:\n{asm}");
    assert!(!bss.contains("hot:"), "initialized global leaked into .bss:\n{asm}");
    let data = asm.split("\t.data").nth(1).expect("no .data section");
    assert!(data.contains("hot:"), "nonzero global missing from .data:\n{asm}");
    // zero-init values still read as 0 end to end
    assert_eq!(
        run("static int z = 0; int u[4]; int main(){ return z + u[2] + 9; }"),
        9
    );
}

#[test]
fn unreferenced_statics_are_eliminated() {
    // gcc -O2 discards unreferenced statics; main.c carries ~570KB of static
    // buffers for compiled-out render paths, which must not reach the image.
    let asm = crate::compile(
        "static int dead_buf[1000];\n\
         static int dead_fn(void){ return dead_buf[0]; }\n\
         static int live_buf[4];\n\
         int use(void){ return live_buf[1]; }\n",
    )
    .unwrap();
    assert!(!asm.contains("dead_buf"), "dead static buffer emitted:\n{asm}");
    assert!(!asm.contains("dead_fn"), "dead static function emitted:\n{asm}");
    assert!(asm.contains("live_buf"), "live static buffer missing:\n{asm}");
    // transitively-live statics survive (fn -> fn -> buffer)
    let asm2 = crate::compile(
        "static int buf[8];\n\
         static int inner(void){ return buf[0]; }\n\
         static int outer(void){ return inner(); }\n\
         int root(void){ return outer(); }\n",
    )
    .unwrap();
    assert!(asm2.contains("buf") && asm2.contains("inner"), "live chain dropped:\n{asm2}");
}

#[test]
fn aligned_attribute_is_honored() {
    let asm = crate::compile(
        "static volatile unsigned int mailbox[4] __attribute__((aligned(16)));\n\
         unsigned int read(void){ return mailbox[0]; }",
    )
    .unwrap();
    let bss = asm.split("\t.bss").nth(1).expect("mailbox should be bss");
    assert!(bss.contains(".align 16"), "aligned(16) dropped:\n{asm}");
}

// ── diagnostic line attribution ──────────────────────────────────────────────
// The preprocessor removes/inserts lines (#include splicing, dead #if blocks,
// gathered macro calls). Errors must still name the ORIGINAL source line —
// COBWEB_REQ_jcc68k_adoption item 4 was a "non-constant expression in
// initializer" pointing at an unrelated line 1200 lines away.

#[test]
fn error_line_survives_dead_conditional() {
    // 5 lines vanish inside `#if 0`; the bad initializer sits on source
    // line 9 and must be reported there, not at the post-collapse position.
    let src = "\
int ok;\n\
#if 0\n\
int a;\n\
int b;\n\
int c;\n\
int d;\n\
#endif\n\
int f(void);\n\
int bad = f();\n";
    let dir = std::env::temp_dir().join(format!("jcc_line_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let main_c = dir.join("main.c");
    std::fs::write(&main_c, src).unwrap();
    let err = crate::compile_file(src, &main_c, &[]).unwrap_err();
    assert!(
        err.contains(":9:") || err.starts_with("9:"),
        "error must point at source line 9, got: {err}"
    );
    assert!(err.contains("initializer"), "unexpected error: {err}");
}

#[test]
fn error_line_survives_include() {
    // An #include splices ~3 lines into the stream; the error after it must
    // still carry the including file's own line number (line 3) and its name.
    let dir = std::env::temp_dir().join(format!("jcc_line2_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("three.h"), "int h1;\nint h2;\nint h3;\n").unwrap();
    let src = "#include \"three.h\"\nint f(void);\nint bad = f();\n";
    let main_c = dir.join("main.c");
    std::fs::write(&main_c, src).unwrap();
    let err = crate::compile_file(src, &main_c, &[]).unwrap_err();
    assert!(
        err.contains("main.c:3:"),
        "error must point at main.c:3, got: {err}"
    );
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

// ─── address-register allocation, EA folding, copy folding ──────────────────
// These cover the paths added when pointers moved into A2-A4 and field offsets
// started folding into the instruction. Every one is an execution test: a
// wrong displacement or a mis-folded copy shows up as a wrong number, not a
// diff against expected assembly.

#[test]
fn struct_ptr_dot_product() {
    let src = "struct V { int x, y, z; };\
               int dot(struct V *a, struct V *b){ return a->x*b->x + a->y*b->y + a->z*b->z; }\
               int main(){ struct V p, q; p.x=1; p.y=2; p.z=3; q.x=4; q.y=5; q.z=6;\
                           return dot(&p,&q); }";
    assert_eq!(run(src), 32); // 4 + 10 + 18
}

#[test]
fn three_pointer_params_cross_product() {
    // Exercises all three address registers live at once, with stores through
    // the third while the first two are still being read.
    let src = "struct V { int x, y, z; };\
               void cross(struct V*a, struct V*b, struct V*o){\
                 o->x = a->y*b->z - a->z*b->y;\
                 o->y = a->z*b->x - a->x*b->z;\
                 o->z = a->x*b->y - a->y*b->x; }\
               int main(){ struct V a,b,o; a.x=1;a.y=0;a.z=0; b.x=0;b.y=1;b.z=0;\
                           cross(&a,&b,&o); return o.x*100 + o.y*10 + o.z; }";
    assert_eq!(run(src), 1); // (1,0,0) x (0,1,0) = (0,0,1)
}

#[test]
fn more_pointers_than_address_registers() {
    // Five pointer params: the surplus must fall back correctly.
    let src = "int f(int*a,int*b,int*c,int*d,int*e){ return *a + *b*2 + *c*3 + *d*4 + *e*5; }\
               int main(){ int v1=1,v2=1,v3=1,v4=1,v5=1; return f(&v1,&v2,&v3,&v4,&v5); }";
    assert_eq!(run(src), 15);
}

#[test]
fn pointer_post_increment_walk() {
    // `*p++` on a register-allocated pointer goes through the ADDA path.
    let src = "int main(){ int t[4]; t[0]=1;t[1]=2;t[2]=3;t[3]=4;\
                int *p=t; int s=0; int i; for(i=0;i<4;i++) s += *p++; return s; }";
    assert_eq!(run(src), 10);
}

#[test]
fn byte_and_short_through_pointers() {
    // Sub-word loads must keep their own width when the base is an A-register.
    let src = "int main(){ short s[2]; s[0]=1000; s[1]=-500; short *p=s;\
                return p[0] + p[1]; }";
    assert_eq!(run(src), 500);
}

#[test]
fn unsigned_char_zero_extends_through_pointer() {
    let src = "int main(){ unsigned char b[2]; b[0]=200; b[1]=100;\
                unsigned char *p=b; return p[0] + p[1]; }";
    assert_eq!(run(src), 300);
}

#[test]
fn signed_char_sign_extends_through_pointer() {
    let src = "int main(){ signed char b[2]; b[0]=-5; b[1]=-10;\
                signed char *p=b; return p[0] + p[1] + 100; }";
    assert_eq!(run(src), 85);
}

#[test]
fn logical_ops_on_pointer_loaded_values() {
    // AND/OR reject an address register as source; the operands here come from
    // A-register-based loads, so the fallback path has to be taken.
    let src = "struct S { int a, b; };\
               int f(struct S*p){ return (p->a & 0xF0) | (p->b & 0x0F); }\
               int main(){ struct S s; s.a=0xAB; s.b=0xCD; return f(&s); }";
    assert_eq!(run(src), 0xAD);
}

#[test]
fn null_pointer_compare() {
    let src = "int f(int*p){ if (p == 0) return 7; return *p; }\
               int main(){ int v=9; return f(0)*10 + f(&v); }";
    assert_eq!(run(src), 79);
}

#[test]
fn function_pointer_call() {
    // A function pointer is a pointer local — it must not be allocated somewhere
    // that breaks the indirect `jsr`.
    let src = "int add(int a,int b){ return a+b; }\
               int main(){ int (*fp)(int,int) = add; return fp(3,4); }";
    assert_eq!(run(src), 7);
}

#[test]
fn nested_member_chain_through_pointer() {
    let src = "struct In { int v; }; struct Out { struct In i; int w; };\
               int main(){ struct Out o; o.i.v=11; o.w=22;\
                           struct Out *p=&o; return p->i.v + p->w; }";
    assert_eq!(run(src), 33);
}

#[test]
fn store_through_pointer_then_read_alias() {
    // Two pointers to the same object: a fold that dropped the store would
    // return the stale value.
    let src = "int main(){ int v=1; int *p=&v; int *q=&v; *p = 5; return *q; }";
    assert_eq!(run(src), 5);
}

#[test]
fn assignment_rhs_uses_the_destination_pointer() {
    let src = "struct V { int x, y; };\
               int main(){ struct V a; struct V *p=&a; p->x = 3; p->y = p->x * 7;\
                           return p->y; }";
    assert_eq!(run(src), 21);
}

#[test]
fn global_array_constant_index_folds() {
    let src = "int g[4] = {5,6,7,8};\
               int main(){ return g[0] + g[3]; }";
    assert_eq!(run(src), 13);
}

#[test]
fn copy_fold_across_calls_respects_caller_saved() {
    // D0/D1 die at a `jsr`; the fold's liveness rule must not carry a value
    // across one.
    let src = "int id(int x){ return x; }\
               int main(){ int a=3, b=4; return id(a) + id(b)*2; }";
    assert_eq!(run(src), 11);
}

#[test]
fn matrix_transform_fixed_point() {
    // The shape the whole exercise is aimed at: pointer-heavy fixed-point math.
    let src = "struct V { int x, y, z; }; struct M { int m[9]; };\
               void xf(struct M*m, struct V*v, struct V*o){\
                 o->x = (m->m[0]*v->x + m->m[1]*v->y + m->m[2]*v->z) >> 8;\
                 o->y = (m->m[3]*v->x + m->m[4]*v->y + m->m[5]*v->z) >> 8;\
                 o->z = (m->m[6]*v->x + m->m[7]*v->y + m->m[8]*v->z) >> 8; }\
               int main(){ struct M m; struct V v, o; int i;\
                 for(i=0;i<9;i++) m.m[i]=0;\
                 m.m[0]=256; m.m[4]=256; m.m[8]=256;\
                 v.x=7; v.y=8; v.z=9; xf(&m,&v,&o);\
                 return o.x*100 + o.y*10 + o.z; }";
    assert_eq!(run(src), 789); // identity matrix in 24.8 fixed point
}

#[test]
fn multiply_by_constant_strength_reduction_is_exact() {
    // Constants the shift/add decomposition accepts (sums and differences of
    // two powers of two), plus ones it must reject and hand to __mulsi3.
    // Both paths have to produce the exact 32-bit product, for negative
    // multiplicands as well as positive ones.
    let consts: &[i64] = &[
        // sum-of-two-powers
        3, 5, 6, 9, 10, 12, 17, 18, 20, 24, 33, 36, 40, 48, 66, 72, 96, 132, 160, 264, 320,
        // difference-of-two-powers
        7, 14, 15, 28, 30, 31, 56, 60, 62, 63, 112, 124, 127, 224, 254, 255,
        // must fall back to the helper (three or more set bits)
        11, 13, 19, 21, 23, 100, 1000, 12345,
        // powers of two and identities still handled by the original path
        1, 2, 4, 8, 256, 1024,
    ];
    let vals: &[i64] = &[0, 1, 3, -3, 1234, -1234, 32767, -32768, 65535];
    for &n in consts {
        for &x in vals {
            let src = format!("int main(){{ int x = {x}; return x * {n}; }}");
            let got = run(&src) as i32;
            let want = (x as i32).wrapping_mul(n as i32);
            assert_eq!(got, want, "wrong product for {x} * {n}");
        }
    }
}

#[test]
fn struct_array_index_walk() {
    // `p[i].field` on a 12-byte struct is the case that made index scaling a
    // __mulsi3 call; the values must survive the strength reduction.
    let src = "struct V { int x, y, z; };\
               int walk(struct V *p, int n){ int acc=0; int i;\
                 for(i=0;i<n;i++) acc += p[i].x + p[i].z; return acc; }\
               int main(){ struct V a[3];\
                 a[0].x=1; a[0].y=99; a[0].z=2;\
                 a[1].x=3; a[1].y=99; a[1].z=4;\
                 a[2].x=5; a[2].y=99; a[2].z=6;\
                 return walk(a,3); }";
    assert_eq!(run(src), 21); // (1+2)+(3+4)+(5+6)
}

#[test]
fn constant_shift_counts_are_exact() {
    // Counts above 8 are now split into successive immediate shifts; every
    // count must still match C semantics for signed (arithmetic) and unsigned
    // (logical) operands, including negative values.
    for n in 1..=31i64 {
        for &x in &[1i64, -1, 0x1234_5678, -0x1234_5678, 0x7FFF_FFFF] {
            let s = format!("int main(){{ int x = {x}; return x >> {n}; }}");
            assert_eq!(run(&s) as i32, (x as i32) >> n, "signed {x} >> {n}");
            let s = format!("int main(){{ int x = {x}; return x << {n}; }}");
            assert_eq!(run(&s) as i32, (x as i32).wrapping_shl(n as u32), "{x} << {n}");
            let s = format!("int main(){{ unsigned x = {}u; return x >> {n}; }}",
                            x as i32 as u32);
            assert_eq!(run(&s), (x as i32 as u32) >> n, "unsigned {x} >> {n}");
        }
    }
}

#[test]
fn constant_folding_matches_runtime() {
    // Folded constant arithmetic must agree with what the unfolded form
    // computes, including wraparound and the comparison results.
    let cases: &[(&str, i32)] = &[
        ("2 + 3 * 4 - 1", 13),
        ("(7 & 12) | (1 << 5)", 36),
        ("100 / 7", 14),
        ("100 % 7", 2),
        ("(5 > 3) + (2 >= 9)", 1),
        ("1000000 * 3000", 1000000i32.wrapping_mul(3000)),
        ("-8 >> 2", -2),
        ("(3 && 0) + (0 || 4)", 1),
    ];
    for (expr, want) in cases {
        let src = format!("int main(){{ return {expr}; }}");
        assert_eq!(run(&src) as i32, *want, "constant expression `{expr}`");
    }
}

#[test]
fn constant_index_addressing_still_reads_the_right_slot() {
    // Constant subscripts now fold into the address; each must land on its own
    // element, not a neighbour.
    let src = "struct M { int m[9]; };\
               int pick(struct M *p){ return p->m[0]*1 + p->m[4]*10 + p->m[8]*100; }\
               int main(){ struct M m; int i; for(i=0;i<9;i++) m.m[i]=i;\
                           return pick(&m); }";
    assert_eq!(run(src), 0 + 4 * 10 + 8 * 100);
}

#[test]
fn store_target_pointer_reassigned_by_its_own_rhs() {
    // The destination address must be the value `p` had *before* the RHS ran.
    // This is the case that stops a pointer local's register from being treated
    // as a stable base for the store.
    let src = "struct V { int x, y; };\
               int main(){ struct V a, b; struct V *p = &a; struct V *q = &b;\
                 a.x = 0; a.y = 11; b.x = 0; b.y = 22;\
                 p->x = (p = q)->y;\
                 return a.x*100 + b.x; }";
    // `p->x` resolves against &a, while the RHS reads b.y (22) after p=q.
    assert_eq!(run(src), 2200);
}

#[test]
fn store_through_pointer_incremented_in_rhs() {
    let src = "int main(){ int t[3]; t[0]=0; t[1]=0; t[2]=0;\
                 int *p = t; *p = (int)(p++ , 7);\
                 return t[0]*10 + t[1]; }";
    assert_eq!(run(src), 70); // the store lands on t[0], not t[1]
}

// ─── string-literal initializers, and directives inside parentheses ─────────

#[test]
fn string_literal_in_static_initializer() {
    // The address of a string-pool entry is a link-time constant. const_eval
    // works in i64 and cannot represent one, so these used to be rejected as
    // "non-constant expression in initializer".
    let src = "static const char *s = \"Hi\";\
               int main(){ return s[0] + s[1]; }";
    assert_eq!(run(src), ('H' as u32) + ('i' as u32));
}

#[test]
fn string_literal_table_initializer() {
    let src = "static const char *const T[2] = { \"AB\", \"CD\" };\
               int main(){ return T[0][0] + T[1][1]*2; }";
    assert_eq!(run(src), ('A' as u32) + ('D' as u32) * 2);
}

#[test]
fn char_array_initializer_still_copies_bytes() {
    // A `char[]` destination copies the string; only a pointer takes its
    // address. This must not have been changed by the pointer case.
    let src = "static char buf[8] = \"Hi\";\
               int main(){ return buf[0] + buf[1] + buf[2]; }";
    assert_eq!(run(src), ('H' as u32) + ('i' as u32)); // buf[2] is the NUL
}

#[test]
fn directive_inside_parenthesized_expression() {
    // An unbalanced '(' used to make the preprocessor swallow the next line as
    // function-macro argument text, leaving the '#' in the token stream and
    // macro-expanding the condition's own name.
    let src = "int g(int x){ return x; }\n\
               int main(void){\n\
                 int a=0,b=0;\n\
                 if (g(1)\n\
               #ifdef FEAT\n\
                     && g(0)\n\
               #endif\n\
                    ) a=1;\n\
                 if (g(1)\n\
               #ifndef FEAT\n\
                     && g(0)\n\
               #endif\n\
                    ) b=1;\n\
                 return a*10 + b;\n\
               }\n";
    assert_eq!(run_pp(src), 10); // FEAT undefined: a=1, b=0
}

#[test]
fn directive_inside_call_arguments() {
    let src = "int h(int a,int b){ return a*10+b; }\n\
               int main(void){ return h(1,\n\
               #ifdef FEAT\n\
                 2\n\
               #else\n\
                 3\n\
               #endif\n\
                 ); }\n";
    assert_eq!(run_pp(src), 13);
}

#[test]
fn multiline_macro_call_still_gathers_lines() {
    // The directive guard must not break the reason line-gathering exists.
    let src = "#define ADD(a,b) ((a)+(b))\n\
               int main(void){ return ADD(20,\n\
                 3); }\n";
    assert_eq!(run_pp(src), 23);
}

#[test]
fn jerry_pose_angle_marshal_loop() {
    // The shape of jerry_pose_kick's angle marshal: byte source, 32-bit
    // volatile destination, unsigned loop bound `mcount*3`. The reported
    // symptom was the LAST angle being corrupted, so every element is checked.
    let src = "static unsigned char SRC[12] = {10,20,30,40,50,60,70,80,90,100,110,120};\
               static volatile unsigned DST[12];\
               void marshal(volatile unsigned *ab, const void *angles, unsigned mcount){\
                 const unsigned char *s = (const unsigned char *)angles;\
                 unsigned i2;\
                 for (i2 = 0; i2 < mcount*3; i2++) ab[i2] = s[i2]; }\
               int main(){ unsigned i; int sum=0;\
                 for (i=0;i<12;i++) DST[i]=0;\
                 marshal(DST, SRC, 4);\
                 for (i=0;i<12;i++) sum += (int)DST[i] * (int)(i+1);\
                 return sum; }";
    // weighted so a wrong value in ANY slot (including the last) changes the sum
    let want: u32 = (0..12u32).map(|i| (10 * (i + 1)) * (i + 1)).sum();
    assert_eq!(run(src), want);
}

#[test]
fn fifteen_parameters_all_arrive() {
    // jerry_pose_kick takes 15 args with the loop bound LAST; a wrong offset
    // for the tail arguments corrupts exactly the end of its output.
    let src = "int f(int a,int b,int c,int d,int e,int g,int h,int i,\
                     int j,int k,int l,int m,int n,int o,int p){\
                 return a*1+b*2+c*3+d*4+e*5+g*6+h*7+i*8+j*9+k*10\
                      + l*11+m*12+n*13+o*14+p*15; }\
               int main(){ return f(1,1,1,1,1,1,1,1,1,1,1,1,1,1,1); }";
    assert_eq!(run(src), (1..=15).sum::<u32>());
}

#[test]
fn fifteen_parameters_mixed_pointers_and_scalars() {
    // The real signature: pointers first (they claim address registers), then
    // scalars, with an unsigned count last.
    let src = "unsigned g_last;\
               void kick(const void *a,const void *b,const void *c,const void *d,\
                         const void *e,void *f,int r0,int r1,int r2,int r3,\
                         int r4,int r5,int r6,int r7,unsigned cnt){\
                 g_last = cnt + (unsigned)r7*1000; }\
               static int buf[4];\
               int main(){ kick(buf,buf,buf,buf,buf,buf,0,0,0,0,0,0,0,7,12);\
                           return (int)g_last; }";
    assert_eq!(run(src), 7012);
}

#[test]
fn jerry_pose_kick_full_param_block() {
    // A faithful copy of jerry_pose_kick: 15 parameters, a 16-long parameter
    // block, then the byte->long angle marshal at +0x40. Every written long is
    // checked, weighted by slot, so a wrong value anywhere (especially the
    // last angle, the reported symptom) changes the result.
    let src = "static volatile unsigned PB[64];\
               static unsigned char ANG[12] = {3,1,4,1,5,9,2,6,5,3,5,8};\
               void kick(const void *skv,const void *skvl,const void *sknode,\
                         const void *angles,const void *sintab,void *out,\
                         int rootx,int rooty,int rootz,int laC,int laS,\
                         int rx0,int rz0,int base_y,unsigned mcount){\
                 volatile unsigned *p = (volatile unsigned *)PB;\
                 p[0]=(unsigned)skv; p[1]=(unsigned)skvl; p[2]=(unsigned)sknode;\
                 p[3]=(unsigned)angles; p[4]=(unsigned)sintab; p[5]=(unsigned)out;\
                 p[6]=(unsigned)rootx; p[7]=(unsigned)rooty; p[8]=(unsigned)rootz;\
                 p[9]=(unsigned)laC; p[10]=(unsigned)laS; p[11]=(unsigned)rx0;\
                 p[12]=(unsigned)rz0; p[13]=(unsigned)base_y; p[14]=mcount;\
                 if (mcount <= 32) {\
                   volatile unsigned *ab = (volatile unsigned *)((unsigned)PB + 0x40);\
                   const unsigned char *s = (const unsigned char *)angles;\
                   unsigned i2;\
                   for (i2 = 0; i2 < mcount*3; i2++) ab[i2] = s[i2]; } }\
               int main(){ unsigned i; int sum=0;\
                 for (i=0;i<64;i++) PB[i]=0;\
                 kick(0,0,0,ANG,0,0, 11,22,33,44,55,66,77,88, 4);\
                 for (i=6;i<15;i++) sum += (int)PB[i]*(int)(i+1);\
                 for (i=0;i<12;i++) sum += (int)PB[16+i]*(int)(i+1)*100;\
                 return sum; }";
    let params = [11u32, 22, 33, 44, 55, 66, 77, 88, 4];
    let angles = [3u32, 1, 4, 1, 5, 9, 2, 6, 5, 3, 5, 8];
    let want: u32 = params.iter().enumerate().map(|(k, v)| v * (k as u32 + 7))
        .chain(angles.iter().enumerate().map(|(k, v)| v * (k as u32 + 1) * 100))
        .sum();
    assert_eq!(run(src), want);
}
