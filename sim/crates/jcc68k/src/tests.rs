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

// ── redundant-load elimination: the cases that must NOT be optimized ─────────
//
// `elim_redundant_loads` reuses a register that already holds a memory value.
// Every test below is a situation where doing so would be wrong; each one
// returns a different answer if the pass forgets to invalidate. They execute
// in jsim rather than pattern-matching the asm, so they check semantics, not
// spelling.

#[test]
fn rle_store_through_alias_invalidates() {
    // `a` and `b` point at the same object, so the store must be observed by
    // the reload. Reusing the first load's register yields 14 instead of 106.
    let src = r#"
        int main() {
            int v = 7;
            int *a = &v;
            int *b = &v;
            int x = *a;
            *b = 99;
            return x + *a;
        }
    "#;
    assert_eq!(run(src), 106);
}

#[test]
fn rle_call_invalidates_memory() {
    // An ordinary call may write anything; only the arithmetic helpers are
    // exempt. Reusing the pre-call value yields 14 instead of 106.
    let src = r#"
        int g;
        void setg(void) { g = 99; }
        int main() { g = 7; int x = g; setg(); return x + g; }
    "#;
    assert_eq!(run(src), 106);
}

#[test]
fn rle_base_register_change_invalidates() {
    // `*p` is the same *spelling* before and after `p++` but a different
    // address. Reusing it yields 6 instead of 11.
    let src = r#"
        int main() {
            int arr[2];
            arr[0] = 3; arr[1] = 8;
            int *p = arr;
            int x = *p;
            p = p + 1;
            return x + *p;
        }
    "#;
    assert_eq!(run(src), 11);
}

#[test]
fn rle_loop_body_invalidates() {
    // A label is a branch target: values available on the fall-in path are not
    // available on the back edge. Reusing them here would return 3, not 7.
    let src = r#"
        int main() {
            int arr[3];
            arr[0] = 1; arr[1] = 2; arr[2] = 4;
            int i; int s = 0;
            for (i = 0; i < 3; i++) { s += arr[i]; arr[i] = 0; }
            return s + arr[0] + arr[1] + arr[2];
        }
    "#;
    assert_eq!(run(src), 7);
}

#[test]
fn rle_pure_helper_preserves_memory() {
    // `__mulsi3` is whitelisted as memory-clean, so the reload after it may be
    // folded — this pins that the folding is still *correct*.
    let src = r#"
        int main() { int v = 6; int *p = &v; int m = *p * *p; return m + *p; }
    "#;
    assert_eq!(run(src), 42);
}

#[test]
fn rle_genuine_redundant_load_is_correct() {
    // The case the pass exists for: same address, nothing in between.
    let src = r#"
        struct P { int x, y, w, h; };
        int main() {
            struct P p;
            p.x = 1; p.y = 2; p.w = 10; p.h = 20;
            struct P *q = &p;
            return (q->w - q->x) + (q->h - q->y) + q->w + q->h;
        }
    "#;
    assert_eq!(run(src), 9 + 18 + 10 + 20);
}

// ── semantics battery ────────────────────────────────────────────────────────
// Edge cases where a 68000 backend plausibly goes wrong. The sub-word parameter
// bug was found this way; these probe the neighbouring ground.

#[test]
fn sem_sub_word_return_values() {
    assert_eq!(run("unsigned char f(void){return 0x1FF;} int main(void){return f();}"), 0xFF);
    assert_eq!(run("short f(void){return -2;} int main(void){return f();}"), (-2i32) as u32);
    assert_eq!(
        run("unsigned short f(void){return -2;} int main(void){return (int)f();}"),
        0xFFFE
    );
}

#[test]
fn sem_char_sign_extension() {
    assert_eq!(run("int main(void){ signed char c = -5; int i = c; return i; }"), (-5i32) as u32);
    assert_eq!(run("int main(void){ unsigned char c = 200; int i = c; return i; }"), 200);
    assert_eq!(run("int main(void){ char a[2]; a[0]=-3; a[1]=7; return a[0]+a[1]; }"), 4u32);
}

#[test]
fn sem_unsigned_comparisons() {
    // The classic: 0x80000000 is "greater" unsigned, "less" signed.
    assert_eq!(run("int main(void){ unsigned a=0x80000000u,b=1u; return a>b; }"), 1);
    assert_eq!(run("int main(void){ int a=(int)0x80000000,b=1; return a<b; }"), 1);
    assert_eq!(run("int main(void){ unsigned a=0xFFFFFFFFu; return a/3u; }"), 0xFFFF_FFFFu32 / 3);
    assert_eq!(run("int main(void){ unsigned a=0xFFFFFFFFu; return a%7u; }"), 0xFFFF_FFFFu32 % 7);
}

#[test]
fn sem_shifts() {
    assert_eq!(run("int main(void){ unsigned v=0x12345678u; return v>>16; }"), 0x1234);
    assert_eq!(run("int main(void){ int v=-16; return v>>2; }"), (-4i32) as u32);
    assert_eq!(run("int main(void){ unsigned v=1u; return v<<31; }"), 0x8000_0000);
    // shift counts above 8 need a register count on the 68000
    assert_eq!(run("int main(void){ unsigned v=0xFFu; int n=12; return v<<n; }"), 0xFF000);
    assert_eq!(run("int main(void){ unsigned v=0xFF000000u; int n=20; return v>>n; }"), 0xFF0);
}

#[test]
fn sem_signed_division_rounds_toward_zero() {
    assert_eq!(run("int main(void){ int a=-7,b=2; return a/b; }"), (-3i32) as u32);
    assert_eq!(run("int main(void){ int a=-7,b=2; return a%b; }"), (-1i32) as u32);
    assert_eq!(run("int main(void){ int a=7,b=-2; return a/b; }"), (-3i32) as u32);
    assert_eq!(run("int main(void){ int a=-8,b=2; return a/b; }"), (-4i32) as u32);
}

#[test]
fn sem_sub_word_pointer_arithmetic() {
    let src = r#"
        int main(void) {
            unsigned char b[6]; short h[4]; int i;
            for (i = 0; i < 6; i++) b[i] = (unsigned char)(i * 3);
            for (i = 0; i < 4; i++) h[i] = (short)(i * 1000 - 1500);
            unsigned char *pb = b + 4;
            short *ph = h + 3;
            return (int)*pb + (int)*ph + (int)b[5] + (int)h[0];
        }
    "#;
    assert_eq!(run(src), (12i32 + 1500 + 15 - 1500) as u32);
}

#[test]
fn sem_struct_field_widths() {
    let src = r#"
        struct S { unsigned char c; short s; int i; unsigned char c2; };
        int main(void) {
            struct S v;
            v.c = 250; v.s = -300; v.i = 100000; v.c2 = 7;
            return (int)v.c + (int)v.s + v.i + (int)v.c2;
        }
    "#;
    assert_eq!(run(src), (250i32 - 300 + 100000 + 7) as u32);
}

#[test]
fn sem_many_args_deep_slots() {
    // The joypad bug lived in the 15th argument slot; make sure deep slots and
    // mixed widths both land correctly.
    let src = r#"
        int f(int a, short b, int c, unsigned char d, int e, short f2,
              int g, int h, int i, int j, short k, int l, unsigned char m, int n, short o) {
            return a + (int)b + c + (int)d + e + (int)f2 + g + h + i + j
                 + (int)k + l + (int)m + n + (int)o;
        }
        int main(void) {
            return f(1, -2, 3, 4, 5, -6, 7, 8, 9, 10, -11, 12, 13, 14, -15);
        }
    "#;
    assert_eq!(run(src), (1i32 - 2 + 3 + 4 + 5 - 6 + 7 + 8 + 9 + 10 - 11 + 12 + 13 + 14 - 15) as u32);
}

/// Byte offset of `field` within `Ty`, computed the portable way (no offsetof).
fn offset_probe(decl: &str, ty: &str, field: &str) -> u32 {
    run(&format!(
        "{decl}\nint main(void){{ {ty} v; char *b = (char*)&v; char *f = (char*)&v.{field}; \
         return (int)(f - b); }}"
    ))
}

#[test]
fn sem_struct_layout_never_misaligns_a_word() {
    // On the 68000 a .w or .l access at an ODD address raises an Address Error
    // (exception 3). So a struct that places a short/int/pointer at an odd
    // offset is not merely non-conforming, it faults on real silicon. These
    // pin the padding rather than trusting it.
    //
    // Alignment here is 2 for everything >= 2 bytes and 1 for char, which is
    // the correct rule for this chip: the 68000 needs EVEN addresses, not
    // natural 4-byte alignment. So `struct{char;int;}` is 6 bytes, not 8.
    let d = "struct A { char c; short s; };";
    assert_eq!(offset_probe(d, "struct A", "c"), 0, "char first");
    assert_eq!(offset_probe(d, "struct A", "s"), 2, "short must skip the pad byte");
    assert_eq!(run(&format!("{d}int main(void){{return (int)sizeof(struct A);}}")), 4);

    let d = "struct B { char c; int i; };";
    assert_eq!(offset_probe(d, "struct B", "i"), 2, "int aligns to 2 on 68k, not 4");
    assert_eq!(run(&format!("{d}int main(void){{return (int)sizeof(struct B);}}")), 6);

    // A char array of ODD length before a word field is the case most likely
    // to be mislaid: the field must still land even.
    let d = "struct C { char a[3]; short s; };";
    assert_eq!(offset_probe(d, "struct C", "s"), 4, "odd char[] must pad to even");
    assert_eq!(run(&format!("{d}int main(void){{return (int)sizeof(struct C);}}")), 6);

    // Nested aggregate inherits the inner alignment.
    let d = "struct In { short s; }; struct D { char c; struct In in; };";
    assert_eq!(offset_probe(d, "struct D", "in"), 2, "nested struct aligns to 2");

    // A pointer field must be even too.
    let d = "struct E { char c; int *p; };";
    assert_eq!(offset_probe(d, "struct E", "p"), 2, "pointer aligns to 2");
}

#[test]
fn sem_struct_array_stride_keeps_alignment() {
    // Element stride must be a multiple of the struct's alignment, or element
    // 1 onward misaligns even though element 0 is fine. `struct{short;char;}`
    // is 3 bytes of content and MUST be padded to 4.
    let src = r#"
        struct S { short s; char c; };
        int main(void) {
            struct S a[3];
            char *b = (char*)&a[0];
            int stride = (int)((char*)&a[1] - b);
            int off2   = (int)((char*)&a[2].s - b);
            /* prove every element's short is actually usable */
            int i; for (i = 0; i < 3; i++) { a[i].s = (short)(i * 1000); a[i].c = (char)i; }
            int sum = 0; for (i = 0; i < 3; i++) sum += a[i].s + a[i].c;
            return stride * 100000 + off2 * 1000 + sum;
        }
    "#;
    // stride 4, a[2].s at 8, sum = 0+1000+2000 + 0+1+2 = 3003
    assert_eq!(run(src), (4 * 100000 + 8 * 1000 + 3003) as u32);
}

#[test]
fn sem_local_and_global_word_fields_land_even() {
    // The frame allocator and the .data emitter each place objects; both must
    // keep word fields even. A misplaced local is the sneakier of the two
    // because the frame offset is chosen at compile time per function.
    let src = r#"
        struct W { char c; short s; int i; };
        static struct W g;
        int main(void) {
            char pad0;          /* push the next local to an odd raw offset */
            struct W l;
            char pad1;
            pad0 = 1; pad1 = 2;
            l.c = 3; l.s = 300; l.i = 70000;
            g.c = 4; g.s = 400; g.i = 80000;
            int lo = (int)((char*)&l.s - (char*)&l) + (int)((char*)&l.i - (char*)&l) * 10;
            int go = (int)((char*)&g.s - (char*)&g) + (int)((char*)&g.i - (char*)&g) * 10;
            return (lo == go) * 1000000 + lo * 1000 + (l.s + g.s) + (int)(pad0 + pad1);
        }
    "#;
    // s at 2, i at 4  -> lo = 2 + 40 = 42, and layout must match the global's
    assert_eq!(run(src), (1_000_000 + 42 * 1000 + 700 + 3) as u32);
}

// ── preprocessor boundary ────────────────────────────────────────────────────
// The builtin-header bug lived here, so the neighbouring ground is worth
// sweeping: expansion order, recursion guards, and the operators that build
// tokens rather than consume them.

// ── found by differential testing against the host cc ────────────────────────

#[test]
fn diff_narrowing_cast_actually_truncates() {
    // A cast is a VALUE conversion with no store to truncate it, and `cast`
    // never narrows — so `(short)v` and `(unsigned char)v` were complete
    // no-ops and the full 32-bit value flowed onward.
    //
    // NOTE the absence of an outer cast. `sem_cast_narrowing_roundtrips` wrote
    // `(int)(unsigned char)i` and passed throughout, because the OUTER widening
    // cast emits the mask as a side effect of zero-extending. The scaffolding
    // was doing the work the test claimed to be checking.
    assert_eq!(run("int main(void){ unsigned v=2560654684u; return (unsigned char)v; }"), 92);
    assert_eq!(run("int main(void){ unsigned v=2560654684u; return (signed char)v; }"), 92);
    assert_eq!(run("int main(void){ unsigned v=2560654684u; return (short)v; }"), 32092);
    // and the narrowed value must be what the surrounding expression sees
    assert_eq!(run("int main(void){ unsigned v=2560654684u; return (signed char)v + 1; }"), 93);
    assert_eq!(run("int main(void){ unsigned v=0xFF80u; return (signed char)v; }"), (-128i32) as u32);
    assert_eq!(run("int main(void){ int i=-1; return (unsigned short)i; }"), 0xFFFF);
}

#[test]
fn diff_narrow_unsigned_promotes_to_signed_int() {
    // The usual arithmetic conversions run integer PROMOTION first: on a
    // 32-bit-int target, unsigned char/short promote to SIGNED int, so a
    // subtraction that goes negative really is negative. Typing the result as
    // unsigned made `(a - b) < 0` false for every narrow unsigned operand —
    // and, downstream, made the comparison itself unsigned.
    assert_eq!(run("int main(void){ unsigned short a=1; int b=5; return (a-b) < 0; }"), 1);
    assert_eq!(run("int main(void){ unsigned char a=1; int b=5; return (a-b) < 0; }"), 1);
    assert_eq!(run("int main(void){ unsigned short a=1; short b=-1; return (a^b) < 0; }"), 1);
    assert_eq!(run("int main(void){ unsigned short a=2; signed char b=-3; return (a*b) < 0; }"), 1);
    // an unsigned operand of int rank or wider DOES make it unsigned
    assert_eq!(run("int main(void){ unsigned a=1; int b=5; return (a-b) < 0; }"), 0);
    assert_eq!(run("int main(void){ unsigned short a=1; int b=5; return (int)(a-b); }"), (-4i32) as u32);
}

#[test]
fn struct_assignment_copies_the_object() {
    // Whole-struct assignment fell through the scalar path and stored four
    // bytes — the source's ADDRESS — over the destination's first field, so
    // `y = x` produced pointer-shaped garbage. Now a block copy.
    let src = r#"
        struct P { int a; short b; unsigned char c; };
        int main(void) {
            struct P x, y;
            x.a = 30000; x.b = 200; x.c = 1;
            y = x;
            return y.a + y.b + y.c;
        }
    "#;
    assert_eq!(run(src), 30201);

    // initialization from another struct
    assert_eq!(
        run("struct P{int a;int b;};int main(void){struct P x;x.a=7;x.b=9;struct P y=x;return y.a*10+y.b;}"),
        79
    );
    // array elements
    assert_eq!(
        run("struct P{int a;int b;};int main(void){struct P v[2];v[0].a=333;v[0].b=444;v[1]=v[0];return v[1].a+v[1].b;}"),
        777
    );
    // a size that needs the odd-byte tail, not just the long loop
    assert_eq!(
        run("struct S{unsigned char c;};int main(void){struct S a,b;a.c=200;b=a;return b.c;}"),
        200
    );
    // large enough to exercise the dbra loop
    assert_eq!(
        run("struct B{int m[9];};int main(void){struct B x,y;int i;for(i=0;i<9;i++)x.m[i]=i*i;y=x;int s=0;for(i=0;i<9;i++)s+=y.m[i];return s;}"),
        204
    );
    // chained: the assignment's value is the destination object, so this must
    // propagate rather than copy garbage into the outer target
    assert_eq!(
        run("struct P{int a;short b;};int main(void){struct P x,y,z;x.a=5;x.b=6;z=y=x;return z.a*100+z.b;}"),
        506
    );
}

#[test]
fn diff_unary_ops_promote_to_int() {
    // `-` and `~` keep the operand's type unless integer promotion runs first,
    // so a narrow operand truncated the result: `~(unsigned char)13` came out
    // 242 instead of -14. Same family as the binary and `?:` typing bugs.
    assert_eq!(run("int main(void){ unsigned char c=13; return (int)(unsigned)(~c); }"), 4294967282);
    assert_eq!(run("int main(void){ unsigned short c=13; return (int)(unsigned)(~c); }"), 4294967282);
    assert_eq!(run("int main(void){ unsigned char c=13; return (int)(unsigned)(-c); }"), 4294967283);
    assert_eq!(run("int main(void){ signed char c=-5; return (int)(unsigned)(~c); }"), 4);
    // an unsigned int operand must STAY unsigned through the promotion
    assert_eq!(run("int main(void){ unsigned c=13; return (int)(unsigned)(-c); }"), 4294967283);
    assert_eq!(run("int main(void){ unsigned char c=13; return (~c) < 0; }"), 1);
}

#[test]
fn diff_conditional_applies_arithmetic_conversions() {
    // The `?:` result type is the usual arithmetic conversion of its two RESULT
    // operands, not simply the second operand's type. Typing it from one arm
    // meant a later widening cast masked the OTHER arm's value to that arm's
    // width.
    //
    // Note the `(unsigned)` — without a widening cast the wrong type produces
    // no instruction and the bug is invisible. My first three attempts to
    // reproduce this all omitted it and "passed".
    assert_eq!(
        run("int main(void){ unsigned char c=227; return (int)(unsigned)(0 ? c : (1u - 6u)); }"),
        4294967291
    );
    assert_eq!(
        run("int main(void){ short s=3; return (int)(unsigned)(0 ? s : (1u - 6u)); }"),
        4294967291
    );
    // the taken arm still converts correctly
    assert_eq!(
        run("int main(void){ unsigned char c=227; return (int)(unsigned)(1 ? c : (1u - 6u)); }"),
        227
    );
    // an unsigned-int arm makes the whole conditional unsigned
    assert_eq!(
        run("int main(void){ unsigned a=1u; int b=5; return (0 ? a : (a-b)) < 0; }"),
        0
    );
}

// ── fixed-point boundary ─────────────────────────────────────────────────────
// `float`/`double` here are signed 16.16 fixed in a 32-bit word — a convention
// this compiler invents, so it has no outside spec to have been checked
// against. 1.0 == 65536. Returning one through an `int` hands back the RAW
// word (see `fixed_raw_repr`), which is how these read the result.

#[test]
fn fx_literals_and_int_conversion() {
    assert_eq!(run("int main(){ float f = 1.0; return f; }"), 65536);
    assert_eq!(run("int main(){ float f = -1.5; return f; }"), (-98304i32) as u32);
    assert_eq!(run("int main(){ float f = 0.25; return f; }"), 16384);
    // int -> fixed on assignment
    assert_eq!(run("int main(){ int i = 3; float f = i; return f; }"), 3 * 65536);
    // fixed -> int truncates toward -inf via the arithmetic shift the cast uses
    assert_eq!(run("int main(){ float f = 2.75; int i = f; return i; }"), 2);
    assert_eq!(run("int main(){ float f = -2.75; int i = f; return i; }"), (-3i32) as u32);
}

#[test]
fn fx_add_and_subtract() {
    assert_eq!(run("int main(){ float a=1.5,b=2.25; float c=a+b; return c; }"), (3.75 * 65536.0) as u32);
    assert_eq!(run("int main(){ float a=1.5,b=2.25; float c=a-b; return c; }"), ((-0.75 * 65536.0) as i32) as u32);
    // mixing an int into fixed arithmetic must scale the int first
    assert_eq!(run("int main(){ float a=1.5; float c=a+2; return c; }"), (3.5 * 65536.0) as u32);
}

#[test]
fn fx_multiply_scales_back_down() {
    // 16.16 * 16.16 is 32.32, so the product must be shifted right by 16. A
    // plain 32-bit multiply gives 1.5*2.0 = 98304*131072 wrapped, not 3.0.
    assert_eq!(run("int main(){ float a=1.5,b=2.0; float c=a*b; return c; }"), (3.0 * 65536.0) as u32);
    assert_eq!(run("int main(){ float a=0.5,b=0.5; float c=a*b; return c; }"), (0.25 * 65536.0) as u32);
    assert_eq!(
        run("int main(){ float a=-2.5,b=3.0; float c=a*b; return c; }"),
        ((-7.5 * 65536.0) as i32) as u32
    );
    // scaling by an int operand must NOT double-shift
    assert_eq!(run("int main(){ float a=1.5; float c=a*3; return c; }"), (4.5 * 65536.0) as u32);
}

#[test]
fn fx_divide_scales_up_first() {
    // 16.16 / 16.16 needs the dividend shifted left by 16 before dividing, or
    // every quotient below 1.0 collapses to zero.
    assert_eq!(run("int main(){ float a=3.0,b=2.0; float c=a/b; return c; }"), (1.5 * 65536.0) as u32);
    assert_eq!(run("int main(){ float a=1.0,b=4.0; float c=a/b; return c; }"), (0.25 * 65536.0) as u32);
    assert_eq!(run("int main(){ float a=1.0,b=2.0; float c=a/b; return c; }"), 32768);
    // dividing by an int
    assert_eq!(run("int main(){ float a=5.0; float c=a/2; return c; }"), (2.5 * 65536.0) as u32);
}

#[test]
fn fx_comparisons_are_signed() {
    assert_eq!(run("int main(){ float a=-1.0,b=0.5; return a<b; }"), 1);
    assert_eq!(run("int main(){ float a=-1.0,b=0.5; return a>b; }"), 0);
    assert_eq!(run("int main(){ float a=2.5,b=2.5; return a==b; }"), 1);
    assert_eq!(run("int main(){ float a=0.25; return a>0; }"), 1);
}

// ── volatile / MMIO boundary ─────────────────────────────────────────────────
// The frontend DISCARDS `volatile` on pointed-to types (it keeps the qualifier
// only on locals, to bar register promotion), so codegen cannot tell an MMIO
// access from an ordinary load. These check whether that gap causes wrong code
// today. Counted in the ASM: a simulator cannot tell a dropped hardware read
// from a kept one, because the value does not change between them.

/// Count occurrences of `needle` inside function `f`'s emitted body.
fn count_in_fn(src: &str, f: &str, needle: &str) -> usize {
    let asm = crate::compile_program(src).expect("compile");
    asm.lines()
        .skip_while(|l| !l.trim_start().starts_with(&format!("{f}:")))
        .skip(1)
        .take_while(|l| !l.trim().starts_with("rts"))
        .filter(|l| l.contains(needle))
        .count()
}

#[test]
fn vol_every_write_reaches_the_bus() {
    // A pad strobe writes the same register repeatedly with different values;
    // none may be dropped or merged.
    let src = r#"
        void strobe4(volatile unsigned short *r) {
            *r = 0x81FE; *r = 0x81FD; *r = 0x81FB; *r = 0x81F7;
        }
    "#;
    assert_eq!(count_in_fn(src, "strobe4", "move.w"), 4, "all four strobes must be emitted");
}

#[test]
fn vol_write_then_read_keeps_both() {
    // The joypad idiom: select a column, then read it back. Neither may be
    // elided, and the read must not be answered from the written value.
    let src = r#"
        unsigned rw(volatile unsigned *r) { *r = 5; return *r; }
    "#;
    let asm = crate::compile_program(src).expect("compile");
    let body: String = asm
        .lines()
        .skip_while(|l| !l.trim_start().starts_with("rw:"))
        .take_while(|l| !l.trim().starts_with("rts"))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(body.contains("move.l d0,(a"), "the store must survive:\n{body}");
    assert!(
        body.matches("move.l (a").count() >= 1,
        "the read-back must not be answered from the stored value:\n{body}"
    );
}

#[test]
fn vol_loop_read_is_not_hoisted() {
    // Spin until a status register changes. Hoisting the load out of the loop
    // turns this into an infinite loop on hardware.
    let src = r#"
        void wait_ready(volatile unsigned *st) { while (*st & 1) { } }
    "#;
    // the load must appear inside the loop body, i.e. after a label
    let asm = crate::compile_program(src).expect("compile");
    let body: Vec<&str> = asm
        .lines()
        .skip_while(|l| !l.trim_start().starts_with("wait_ready:"))
        .take_while(|l| !l.trim().starts_with("rts"))
        .collect();
    let label_at = body.iter().position(|l| l.trim().starts_with(".L")).unwrap_or(0);
    let load_after_label = body[label_at..].iter().any(|l| l.contains("move.l (a"));
    assert!(load_after_label, "status load must stay inside the loop:\n{}", body.join("\n"));
}

// ── arithmetic runtime boundary ──────────────────────────────────────────────
// The 68000 has no 32-bit multiply or divide, so these lower to __mulsi3 /
// __divsi3 / __udivsi3 / __modsi3 / __umodsi3. DIVS/DIVU are only 32÷16, so a
// divisor that does not fit in 16 bits is the case a naive helper gets wrong.

#[test]
fn rt_division_with_wide_divisors() {
    // Divisor > 65535: cannot go through a single DIVU/DIVS.
    assert_eq!(run("int main(void){ int a=1000000000, b=100000; return a/b; }"), 10000);
    assert_eq!(run("int main(void){ int a=1000000000, b=100000; return a%b; }"), 0);
    assert_eq!(run("int main(void){ int a=999999999, b=123456; return a/b; }"), 999999999 / 123456);
    assert_eq!(run("int main(void){ int a=999999999, b=123456; return a%b; }"), 999999999 % 123456);
    assert_eq!(
        run("int main(void){ unsigned a=4000000000u, b=99991u; return a/b; }"),
        4000000000u32 / 99991
    );
    assert_eq!(
        run("int main(void){ unsigned a=4000000000u, b=99991u; return a%b; }"),
        4000000000u32 % 99991
    );
}

#[test]
fn rt_division_sign_combinations_wide() {
    // Every sign pairing with a wide divisor; C truncates toward zero.
    for (a, b) in [(1000000007i32, 100003i32), (-1000000007, 100003), (1000000007, -100003), (-1000000007, -100003)] {
        let src = format!("int main(void){{ int a={a}, b={b}; return a/b; }}");
        assert_eq!(run(&src), (a / b) as u32, "{a}/{b}");
        let src = format!("int main(void){{ int a={a}, b={b}; return a%b; }}");
        assert_eq!(run(&src), (a % b) as u32, "{a}%{b}");
    }
}

#[test]
fn rt_unsigned_high_bit_operands() {
    // Values with bit 31 set must be treated as magnitudes, not negatives.
    assert_eq!(run("int main(void){ unsigned a=0xFFFFFFFFu,b=0x10000u; return a/b; }"), 0xFFFF);
    assert_eq!(run("int main(void){ unsigned a=0x80000000u,b=3u; return a/b; }"), 0x80000000u32 / 3);
    assert_eq!(run("int main(void){ unsigned a=0x80000000u,b=3u; return a%b; }"), 0x80000000u32 % 3);
    assert_eq!(
        run("int main(void){ unsigned a=0xFFFFFFFFu,b=0xFFFFFFFEu; return a/b*10+a%b; }"),
        1 * 10 + 1
    );
}

#[test]
fn rt_multiply_wraps_like_c() {
    assert_eq!(run("int main(void){ int a=100000,b=100000; return a*b; }"), 100000u32.wrapping_mul(100000));
    assert_eq!(run("int main(void){ unsigned a=0xFFFFFFFFu,b=3u; return a*b; }"), 0xFFFFFFFFu32.wrapping_mul(3));
    assert_eq!(run("int main(void){ int a=-100000,b=100000; return a*b; }"), (-100000i32).wrapping_mul(100000) as u32);
    // a power-of-two multiply is strength-reduced; make sure it still wraps
    assert_eq!(run("int main(void){ int a=0x40000000; return a*4; }"), 0x40000000u32.wrapping_mul(4));
}

#[test]
fn rt_divide_by_zero_terminates() {
    // UB in C, but it must not hang the machine — the harness caps at 5M steps
    // and returns whatever is at $100, so a hang shows up as a wrong value
    // rather than a stuck test. This pins "terminates", not a specific result.
    let src = "int main(void){ int a=7,b=0; int q=a/b; return (q==q) ? 123 : 123; }";
    assert_eq!(run(src), 123);
}

// ── scoping / name binding ───────────────────────────────────────────────────
// The parenthesized-declarator bug was a name that never got bound. Sweep the
// rest of the binding rules: an unbound or wrongly-bound name is silent.

#[test]
fn sem_shadowing_local_over_global() {
    assert_eq!(run("int g = 111;\nint main(void){ int g = 9; return g; }"), 9);
    assert_eq!(run("int g = 111;\nint main(void){ int g = 9; g++; return g; }"), 10);
    // the global must still be reachable from a function that doesn't shadow it
    assert_eq!(
        run("int g = 111;\nstatic int peek(void){return g;}\nint main(void){int g=9;return peek()*10+g;}"),
        1119
    );
}

#[test]
fn sem_block_scope_shadowing() {
    let src = r#"
        int main(void) {
            int x = 1;
            int t = 0;
            { int x = 2; t += x * 10; { int x = 3; t += x * 100; } t += x; }
            t += x * 1000;
            return t;
        }
    "#;
    // inner-most 3*100, middle 2*10 and 2, outer 1*1000
    assert_eq!(run(src), 300 + 20 + 2 + 1000);
}

#[test]
fn sem_shadow_a_parameter() {
    let src = r#"
        static int f(int p) {
            int t = p * 100;
            { int p = 7; t += p; }
            return t + p;
        }
        int main(void) { return f(3); }
    "#;
    assert_eq!(run(src), 300 + 7 + 3);
}

#[test]
fn sem_for_scope_and_reuse() {
    let src = r#"
        int main(void) {
            int i = 99, t = 0;
            for (int i = 0; i < 3; i++) t += i;      /* loop i shadows outer */
            t += i * 100;                            /* outer i must survive */
            for (int i = 10; i < 13; i++) t += i;    /* a second, separate i */
            return t;
        }
    "#;
    assert_eq!(run(src), (0 + 1 + 2) + 9900 + (10 + 11 + 12));
}

#[test]
fn sem_separate_namespaces() {
    // Struct tags, members and ordinary identifiers live in different
    // namespaces; a name in one must not capture a reference meant for another.
    let src = r#"
        struct thing { int thing; };
        int main(void) {
            struct thing thing;
            thing.thing = 5;
            int count = 2;
            struct count { int x; };
            struct count c; c.x = 3;
            return thing.thing * 100 + count * 10 + c.x;
        }
    "#;
    assert_eq!(run(src), 500 + 20 + 3);
}

#[test]
fn sem_typedef_shadowed_by_variable() {
    // A local named like a typedef shadows it as an ordinary identifier.
    let src = r#"
        typedef int myty;
        int main(void) {
            myty a = 4;
            int myty = 7;
            return a * 10 + myty;
        }
    "#;
    assert_eq!(run(src), 47);
}

#[test]
fn sem_static_local_keeps_its_own_storage() {
    // A static local must not be re-initialized, and two statics with the same
    // source name in different functions must not share storage.
    let src = r#"
        static int a(void) { static int n = 100; n++; return n; }
        static int b(void) { static int n = 200; n++; return n; }
        int main(void) {
            a(); a(); b();
            return a() * 1000 + b();
        }
    "#;
    assert_eq!(run(src), 103 * 1000 + 202);
}

#[test]
fn sem_parenthesized_declarator_binds_the_name() {
    // `int (v) = 9;` is a redundant grouping, not a parameter list. The
    // declarator path used to drop the name: storage was allocated and the
    // initializer ran, but every later reference resolved elsewhere. With a
    // global of the same name in scope that is SILENT — the local is written
    // and the global is read.
    assert_eq!(run("int v = 111;\nint main(void){ int (v) = 9; return v; }"), 9);
    assert_eq!(run("int main(void){ int (v) = 9; return v; }"), 9);
    // the grouping must still not swallow real function-pointer declarators
    assert_eq!(
        run("static int g(int x){return x*3;}\nint main(void){ int (*fp)(int) = g; return fp(14); }"),
        42
    );
}

#[test]
fn pp_stringify_and_paste() {
    assert_eq!(run_pp("#define STR(x) #x\n#define LEN(x) (sizeof(STR(x))-1)\nint main(void){return (int)LEN(abcde);}"), 5);
    assert_eq!(run_pp("#define CAT(a,b) a##b\nint main(void){int xy=9;return CAT(x,y);}"), 9);
    // paste then expand: the result must be rescanned as a macro name
    assert_eq!(
        run_pp("#define PRE_val 41\n#define GET(n) PRE_##n\nint main(void){return GET(val)+1;}"),
        42
    );
    // stringify must not expand its argument first
    assert_eq!(
        run_pp("#define N 7\n#define STR(x) #x\n#define LEN(x) (sizeof(STR(x))-1)\nint main(void){return (int)LEN(N);}"),
        1
    );
}

#[test]
fn pp_no_infinite_recursion() {
    // A macro name must not re-expand inside its own expansion (C's "blue
    // paint" rule). Without the guard these hang the preprocessor; with it the
    // inner occurrence survives as a plain identifier, so the code still means
    // what it says.
    assert_eq!(run_pp("#define count count\nint main(void){int count=42;return count;}"), 42);
    // mutual recursion: X -> Y -> X, and the second X is painted
    assert_eq!(run_pp("#define X Y\n#define Y X\nint main(void){int X=5;return X;}"), 5);
    // self-reference with surrounding tokens still terminates
    assert_eq!(run_pp("#define v (v)\nint main(void){int v=9;return v;}"), 9);
}

#[test]
fn pp_conditional_arithmetic() {
    assert_eq!(run_pp("#if 2+3*4 == 14\nint main(void){return 1;}\n#else\nint main(void){return 0;}\n#endif"), 1);
    assert_eq!(run_pp("#define V 3\n#if defined(V) && V > 2\nint main(void){return 7;}\n#else\nint main(void){return 8;}\n#endif"), 7);
    assert_eq!(run_pp("#if UNDEFINED_THING\nint main(void){return 1;}\n#else\nint main(void){return 2;}\n#endif"), 2);
    // #elif chain
    assert_eq!(
        run_pp("#define K 2\n#if K==1\nint main(void){return 10;}\n#elif K==2\nint main(void){return 20;}\n#else\nint main(void){return 30;}\n#endif"),
        20
    );
}

#[test]
fn pp_multiline_and_nested_args() {
    // A macro whose invocation spans lines, and one macro as another's argument.
    assert_eq!(
        run_pp("#define ADD(a,b) ((a)+(b))\nint main(void){return ADD(\n  ADD(1,2),\n  ADD(3,4));}"),
        10
    );
    // an argument containing a comma inside parens must not split
    assert_eq!(
        run_pp("#define FIRST(a,b) (a)\n#define PAIR (1,2)\nint main(void){return FIRST((3,4),9);}"),
        4
    );
}

#[test]
fn sem_asm_clobbers_are_honored() {
    // A clobber list tells the compiler the asm DESTROYS those registers, so
    // nothing live may sit in them across it. Ignoring the list is silent
    // wrong code: `hot` gets allocated to d6, the asm zeroes d6, and the
    // function returns 0 instead of its value.
    let src = r#"
        int f(void) {
            int hot = 0x1234;
            int i;
            for (i = 0; i < 3; i++) hot += i;
            __asm__ __volatile__("moveq #0,d7\n\tmoveq #0,d6" ::: "d7", "d6");
            return hot;
        }
        int main(void) { return f(); }
    "#;
    assert_eq!(run(src), 0x1234 + 0 + 1 + 2);
}

#[test]
fn sem_asm_gas_register_sigils_normalize() {
    // GCC-style asm writes registers as `%%d0` in the C string (so the emitted
    // text is `%d0`). jas wants bare `d0`, so the sigil must be stripped AFTER
    // the `%%` escape is resolved — doing both in one pass lets `%%` win and
    // leaves `%d0` behind.
    let src = r#"
        int main(void) {
            int v = 0;
            __asm__ __volatile__("moveq #7,%%d0\n\tmove.l %%d0,%0" : "=r"(v));
            return v;
        }
    "#;
    assert_eq!(run(src), 7);
}

#[test]
fn sem_varargs_actually_work() {
    // The builtin <stdarg.h> was unusable: its `#define`s were spliced onto one
    // line by a Rust `\` continuation, so none was recognized as a directive
    // and the parser hit a bare `#`. Compiling is not enough — walk real args.
    let src = "#include <stdarg.h>\n\
        int sum(int n, ...) {\n\
            va_list ap; int i, t = 0;\n\
            va_start(ap, n);\n\
            for (i = 0; i < n; i++) t += va_arg(ap, int);\n\
            va_end(ap);\n\
            return t;\n\
        }\n\
        int main(void) { return sum(4, 10, 20, 30, 40) * 10 + sum(1, 7); }\n";
    assert_eq!(run_pp(src), (100 * 10 + 7) as u32);
}

#[test]
fn sem_builtin_headers_define_their_macros() {
    // Same splice bug: NULL and `true` sat on the typedef's line and never
    // became macros. `false` happened to work because it followed a \n.
    let src = "#include <stddef.h>\n#include <stdbool.h>\n\
        int main(void) {\n\
            int *p = NULL;\n\
            bool t = true, f = false;\n\
            return (p == NULL) * 100 + (t ? 10 : 0) + (f ? 1 : 0);\n\
        }\n";
    assert_eq!(run_pp(src), 110);
}

#[test]
fn sem_nested_calls_as_arguments() {
    // Every argument is itself a call, so the eval stack must survive callees
    // that clobber the caller-saved set. Ordering matters too: args push
    // right-to-left, but each call's result must land in its own slot.
    let src = r#"
        static int a(int x) { return x * 2; }
        static int b(int x) { return x + 3; }
        static int c(int x, int y, int z) { return x * 100 + y * 10 + z; }
        int main(void) {
            return c(a(b(1)), b(a(2)), a(a(1))) + c(1, 2, 3);
        }
    "#;
    let a = |x: i32| x * 2;
    let b = |x: i32| x + 3;
    let c = |x: i32, y: i32, z: i32| x * 100 + y * 10 + z;
    assert_eq!(run(src), (c(a(b(1)), b(a(2)), a(a(1))) + c(1, 2, 3)) as u32);
}

#[test]
fn sem_call_result_mixed_into_expression() {
    // A call in the middle of an arithmetic tree: the partial results held in
    // callee-saved registers must survive the call, and D0 must not be assumed
    // preserved across it.
    let src = r#"
        static int f(int x) { return x * 7; }
        int main(void) {
            int p = 3, q = 5;
            return (p + q) * f(p) - (q - p) * f(q) + f(p + q) * (p * q);
        }
    "#;
    let f = |x: i32| x * 7;
    let (p, q) = (3i32, 5i32);
    assert_eq!(run(src), ((p + q) * f(p) - (q - p) * f(q) + f(p + q) * (p * q)) as u32);
}

#[test]
fn sem_chained_and_compound_pointer_assign() {
    let src = r#"
        int main(void) {
            int v[6]; int i;
            for (i = 0; i < 6; i++) v[i] = i * i;
            int *p = v;
            p += 2;            /* -> v[2] */
            int a, b, c;
            a = b = c = *p;    /* 4 each */
            p -= 1;            /* -> v[1] */
            int d = *p;        /* 1 */
            int *q = v + 5;
            int span = (int)(q - p);   /* 4 */
            return a + b + c + d * 10 + span * 100;
        }
    "#;
    assert_eq!(run(src), (4 + 4 + 4 + 1 * 10 + 4 * 100) as u32);
}

#[test]
fn sem_large_frame_displacement() {
    // A frame deeper than a signed 16-bit displacement forces the fallback
    // that computes the address instead of folding it into the instruction.
    let src = r#"
        int main(void) {
            int big[9000];      /* 36000 bytes: past 32767 */
            int i;
            for (i = 0; i < 9000; i += 1000) big[i] = i;
            big[8999] = 42;
            return big[0] + big[4000] + big[8000] + big[8999];
        }
    "#;
    assert_eq!(run(src), (0 + 4000 + 8000 + 42) as u32);
}

#[test]
fn sem_sizeof_and_unary() {
    assert_eq!(run("int main(void){ return (int)sizeof(int)*1000 + (int)sizeof(char)*100 + (int)sizeof(short)*10; }"), 4 * 1000 + 100 + 2 * 10);
    assert_eq!(run("struct S{int a; short b; char c;};int main(void){ return (int)sizeof(struct S); }"), 8);
    assert_eq!(run("int main(void){ int a[7]; return (int)sizeof(a); }"), 28);
    assert_eq!(run("int main(void){ unsigned u=1u; return (int)(-u) == -1; }"), 1);
    assert_eq!(run("int main(void){ int x=5; return -(-x); }"), 5);
}

#[test]
fn sem_goto_out_of_nested_loops() {
    let src = r#"
        int main(void) {
            int i, j, found = 0;
            for (i = 0; i < 10; i++) {
                for (j = 0; j < 10; j++) {
                    if (i * j == 42) goto done;
                    found++;
                }
            }
        done:
            return found * 100 + i * 10 + j;
        }
    "#;
    let (mut i, mut j, mut found) = (0i32, 0i32, 0i32);
    'outer: loop {
        if i >= 10 { break }
        j = 0;
        loop {
            if j >= 10 { break }
            if i * j == 42 { break 'outer }
            found += 1;
            j += 1;
        }
        i += 1;
    }
    assert_eq!(run(src), (found * 100 + i * 10 + j) as u32);
}

#[test]
fn sem_continue_in_do_while_tests_condition() {
    // `continue` in a do-while must jump to the CONDITION, not the top of the
    // body. Jumping to the top skips the increment and hangs, or (if it skips
    // the test) runs the wrong number of iterations.
    let src = r#"
        int main(void) {
            int i = 0, n = 0, odd = 0;
            do {
                i++;
                if (i & 1) { odd++; continue; }
                n += i;
            } while (i < 10);
            return n * 100 + odd;
        }
    "#;
    assert_eq!(run(src), (2 + 4 + 6 + 8 + 10) * 100 + 5);
}

#[test]
fn sem_nested_loop_break_continue() {
    let src = r#"
        int main(void) {
            int i, j, hits = 0, skips = 0;
            for (i = 0; i < 5; i++) {
                for (j = 0; j < 5; j++) {
                    if (j == 3) break;
                    if ((i + j) & 1) { skips++; continue; }
                    hits++;
                }
            }
            return hits * 100 + skips;
        }
    "#;
    // inner runs j=0,1,2 for each of 5 i; (i+j) odd -> skip
    let (mut hits, mut skips) = (0, 0);
    for i in 0..5 {
        for j in 0..3 {
            if (i + j) % 2 == 1 { skips += 1 } else { hits += 1 }
        }
    }
    assert_eq!(run(src), (hits * 100 + skips) as u32);
}

#[test]
fn sem_deep_expression_nesting_spills() {
    // The evaluation stack holds operands in d2-d7 and a2-a5, spilling to the
    // machine stack past those depths. Nest deeper than the register pool to
    // exercise the spill path.
    let src = r#"
        int main(void) {
            int a=1,b=2,c=3,d=4,e=5,f=6,g=7,h=8;
            return ((((((a+b)*(c+d))-((e+f)*(g+h)))+(((a*b)+(c*d))-((e*f)+(g*h))))
                    * ((a+h)-(b+g))) + (((c*f)-(d*e)) * ((a+b+c+d)-(e+f+g+h))));
        }
    "#;
    let (a, b, c, d, e, f, g, h) = (1i32, 2, 3, 4, 5, 6, 7, 8);
    let want = ((((a + b) * (c + d)) - ((e + f) * (g + h)))
        + (((a * b) + (c * d)) - ((e * f) + (g * h))))
        * ((a + h) - (b + g))
        + (((c * f) - (d * e)) * ((a + b + c + d) - (e + f + g + h)));
    assert_eq!(run(src), want as u32);
}

#[test]
fn sem_division_signedness_after_promotion() {
    // The companion to the comparison fix: narrow unsigned operands promote to
    // signed int, so these are SIGNED divisions.
    assert_eq!(run("int main(void){ unsigned char c=10; int i=-3; return c/i; }"), (10i32 / -3) as u32);
    assert_eq!(run("int main(void){ unsigned char c=10; int i=-3; return c%i; }"), (10i32 % -3) as u32);
    // but an unsigned INT operand does force unsigned
    assert_eq!(run("int main(void){ unsigned u=10u; int i=-3; return u/i; }"), 10u32 / (-3i32 as u32));
    assert_eq!(run("int main(void){ short s=-9; int i=2; return s/i; }"), (-9i32 / 2) as u32);
}

#[test]
fn sem_cast_narrowing_roundtrips() {
    assert_eq!(run("int main(void){ int i=0x1234; return (int)(unsigned char)i; }"), 0x34);
    assert_eq!(run("int main(void){ int i=0x1234; return (int)(signed char)i; }"), 0x34);
    assert_eq!(run("int main(void){ int i=0x12F0; return (int)(signed char)i; }"), (-16i32) as u32);
    assert_eq!(run("int main(void){ int i=0x12345678; return (int)(short)i; }"), 0x5678);
    assert_eq!(run("int main(void){ int i=0x1234F678; return (int)(short)i; }"), 0xFFFFF678);
    assert_eq!(run("int main(void){ int i=-1; return (int)(unsigned short)i; }"), 0xFFFF);
}

#[test]
fn sem_switch_fallthrough_and_sparse() {
    let src = r#"
        int f(int x) {
            int r = 0;
            switch (x) {
                case 1:
                case 2: r += 1;          /* fallthrough */
                case 100: r += 10; break;
                case 1000: r += 100;
                default: r += 1000;
            }
            return r;
        }
        int main(void) {
            return f(1) * 1 + f(2) * 10 + f(100) * 100 + f(1000) * 1000 + f(7) * 10000;
        }
    "#;
    let f = |x: i32| -> i32 {
        match x {
            1 | 2 => 1 + 10,
            100 => 10,
            1000 => 100 + 1000,
            _ => 1000,
        }
    };
    let want = f(1) + f(2) * 10 + f(100) * 100 + f(1000) * 1000 + f(7) * 10000;
    assert_eq!(run(src), want as u32);
}

#[test]
fn sem_aggregate_initializers() {
    let src = r#"
        struct Pt { short x, y; };
        int main(void) {
            struct Pt a[3] = { {1,2}, {3,4}, {5,6} };
            int m[2][3] = { {1,2,3}, {4,5,6} };
            int partial[5] = { 7, 8 };          /* rest zero-filled */
            char s[6] = "abc";                  /* rest zero-filled */
            int t = 0, i, j;
            for (i = 0; i < 3; i++) t += a[i].x * 10 + a[i].y;
            for (i = 0; i < 2; i++) for (j = 0; j < 3; j++) t += m[i][j];
            for (i = 0; i < 5; i++) t += partial[i];
            for (i = 0; i < 6; i++) t += s[i];
            return t;
        }
    "#;
    let mut t = 0i32;
    for (x, y) in [(1, 2), (3, 4), (5, 6)] { t += x * 10 + y }
    t += 1 + 2 + 3 + 4 + 5 + 6;
    t += 7 + 8;
    t += 'a' as i32 + 'b' as i32 + 'c' as i32;

    // Isolate each initializer form first, so a failure names the culprit
    // rather than reporting one opaque sum.
    assert_eq!(
        run("struct Pt{short x,y;};int main(void){struct Pt a[3]={{1,2},{3,4},{5,6}};\
             int t=0,i;for(i=0;i<3;i++)t+=a[i].x*10+a[i].y;return t;}"),
        102,
        "array of structs"
    );
    assert_eq!(
        run("int main(void){int m[2][3]={{1,2,3},{4,5,6}};int t=0,i,j;\
             for(i=0;i<2;i++)for(j=0;j<3;j++)t+=m[i][j];return t;}"),
        21,
        "2D array"
    );
    assert_eq!(
        run("int main(void){int p[5]={7,8};int t=0,i;for(i=0;i<5;i++)t+=p[i];return t;}"),
        15,
        "partial init zero-fills"
    );
    assert_eq!(
        run("int main(void){char s[6]=\"abc\";int t=0,i;for(i=0;i<6;i++)t+=s[i];return t;}"),
        294,
        "string literal init of char array"
    );
    assert_eq!(run(src), t as u32);
}

#[test]
fn sem_integer_promotion() {
    // Narrow operands promote to int BEFORE the operation: the sum must not
    // wrap at the operand width. A backend that computes in the narrow type
    // returns 44 here, not 300.
    assert_eq!(
        run("int main(void){ unsigned char a=200,b=100; return a+b; }"),
        300
    );
    assert_eq!(
        run("int main(void){ unsigned short a=60000,b=10000; return a+b; }"),
        70000
    );
    // char*char promotes too: 200*200 = 40000, not 64.
    assert_eq!(
        run("int main(void){ unsigned char a=200,b=200; return a*b; }"),
        40000
    );
    // signed char arithmetic promotes to int, keeping the sign
    assert_eq!(
        run("int main(void){ signed char a=-100,b=-100; return a+b; }"),
        (-200i32) as u32
    );
}

#[test]
fn sem_mixed_width_comparison() {
    // An unsigned char compared against an int promotes to int — it does NOT
    // make the comparison unsigned.
    assert_eq!(run("int main(void){ unsigned char c=200; int i=-1; return c > i; }"), 1);
    assert_eq!(run("int main(void){ short s=-1; unsigned u=1u; return s > u; }"), 1); // s converts to unsigned
    assert_eq!(run("int main(void){ short s=-1; int i=1; return s < i; }"), 1);
}

#[test]
fn sem_struct_by_value_is_refused_not_miscompiled() {
    // By-value struct passing/returning is unimplemented, and what it used to
    // emit was silently WRONG: the caller pushed the struct's address, so the
    // callee mutated the caller's object, only 4 bytes were cleaned from the
    // stack, and the "returned struct" was a 4-byte address assigned over the
    // destination. A diagnostic is recoverable; that is not.
    let ret = r#"
        struct P { int a; int b; };
        struct P make(void) { struct P p; p.a = 1; p.b = 2; return p; }
    "#;
    let e = crate::compile_program(ret).expect_err("returning a struct by value must be refused");
    assert!(e.contains("returning a struct by value"), "unexpected: {e}");

    let param = r#"
        struct P { int a; int b; };
        int take(struct P p) { return p.a; }
    "#;
    let e = crate::compile_program(param).expect_err("by-value struct param must be refused");
    assert!(e.contains("struct by value"), "unexpected: {e}");

    // Passing a POINTER to a struct is the supported form and must still work.
    let ok = r#"
        struct P { int a; short b; unsigned char c; };
        static void bump(struct P *p) { p->a++; p->b++; p->c++; }
        int main(void) {
            struct P p; p.a = 10; p.b = -5; p.c = 254;
            bump(&p); bump(&p);
            return p.a * 1000 + (int)p.b * 10 + (int)p.c;
        }
    "#;
    assert_eq!(run(ok), (12i32 * 1000 + (-3) * 10 + 0) as u32);
}

#[test]
fn sem_function_pointer_mixed_widths() {
    let src = r#"
        static int add3(unsigned char a, short b, int c) { return (int)a + (int)b + c; }
        static int mul2(unsigned char a, short b, int c) { return ((int)a + (int)b) * c; }
        int main(void) {
            int (*f)(unsigned char, short, int);
            int t = 0;
            f = add3; t += f(200, -300, 7);
            f = mul2; t += f(200, -300, 7);
            return t;
        }
    "#;
    assert_eq!(run(src), ((200i32 - 300 + 7) + (200 - 300) * 7) as u32);
}

#[test]
fn sem_compound_assign_sub_word() {
    // Read-modify-write through a narrow lvalue must wrap at the lvalue's width.
    let src = r#"
        int main(void) {
            unsigned char c = 250; short s = 30000; unsigned short u = 65530;
            c += 10;      /* wraps to 4   */
            s += 10000;   /* wraps to -25536 */
            u += 10;      /* wraps to 4   */
            return (int)c * 1000000 + (int)s + (int)u;
        }
    "#;
    assert_eq!(run(src), (4i32 * 1000000 - 25536 + 4) as u32);
}

#[test]
fn sem_incdec_sub_word_wraps() {
    let src = r#"
        int main(void) {
            unsigned char c = 255; unsigned char d = 0;
            c++; d--;
            return (int)c * 1000 + (int)d;
        }
    "#;
    assert_eq!(run(src), 0 * 1000 + 255);
}

#[test]
fn sem_ternary_and_comma() {
    assert_eq!(run("int main(void){ int a=3,b=9; return a<b ? b-a : a-b; }"), 6);
    assert_eq!(run("int main(void){ int i=0,j; j=(i=4, i*3); return j; }"), 12);
    assert_eq!(run("int main(void){ unsigned char c=200; int x = c > 100 ? 7 : 9; return x; }"), 7);
}

#[test]
fn sem_nested_aggregate_access() {
    let src = r#"
        struct Inner { short a, b; };
        struct Outer { struct Inner in[3]; int tail; };
        int main(void) {
            struct Outer o; int i;
            for (i = 0; i < 3; i++) { o.in[i].a = (short)(i+1); o.in[i].b = (short)(-(i+1)); }
            o.tail = 77;
            return o.in[0].a + o.in[1].a + o.in[2].a
                 + o.in[0].b + o.in[1].b + o.in[2].b + o.tail;
        }
    "#;
    assert_eq!(run(src), 77);
}

#[test]
fn sem_do_while_and_switch_sub_word() {
    let src = r#"
        int pick(unsigned char k) {
            switch (k) { case 0: return 5; case 200: return 6; case 255: return 7; }
            return 8;
        }
        int main(void) {
            int n = 0, i = 0;
            do { n += i; i++; } while (i < 5);
            return n * 100 + pick(200) * 10 + pick(255);
        }
    "#;
    assert_eq!(run(src), 10 * 100 + 6 * 10 + 7);
}

#[test]
fn sem_pointer_compare_and_diff() {
    let src = r#"
        int main(void) {
            int a[8]; int *p = a + 2; int *q = a + 7;
            int d = (int)(q - p);
            int lt = p < q; int eq = (p == a + 2);
            return d * 100 + lt * 10 + eq;
        }
    "#;
    assert_eq!(run(src), 5 * 100 + 10 + 1);
}

#[test]
fn sem_global_sub_word_init_and_statics() {
    let src = r#"
        unsigned char gb[4] = {1, 2, 250, 255};
        short gs[3] = {-1, 1000, -1000};
        int counter(void) { static int n = 10; n++; return n; }
        int main(void) {
            int s = 0, i;
            for (i = 0; i < 4; i++) s += (int)gb[i];
            for (i = 0; i < 3; i++) s += (int)gs[i];
            counter(); counter();
            return s + counter();
        }
    "#;
    assert_eq!(run(src), (1 + 2 + 250 + 255 - 1 + 1000 - 1000 + 13) as u32);
}

#[test]
fn pose_marshal_full_mesh_count() {
    // jerry_pose_kick's angle marshal at the shipping mesh count (15 meshes ->
    // 45 angle bytes), not the 4 the existing regression test uses. The
    // reported symptom is the LAST angle corrupted, so this checks every index
    // and weights them distinctly. Fifteen parameters, so the loop bound
    // `mcount*3` is read from the deepest argument slot.
    let src = "\
        volatile unsigned *PB;\
        static unsigned char ANG[45];\
        static unsigned buf[64];\
        void kick(const void *a, const void *b, const void *c, const void *ang,\
                  const void *e, void *f, int r0, int r1, int r2,\
                  int laC, int laS, int rx0, int rz0, int by, unsigned mcount) {\
            volatile unsigned *p = PB;\
            p[0]=(unsigned)a; p[1]=(unsigned)b; p[2]=(unsigned)c; p[3]=(unsigned)ang;\
            p[4]=(unsigned)e; p[5]=(unsigned)f; p[6]=r0; p[7]=r1; p[8]=r2;\
            p[9]=laC; p[10]=laS; p[11]=rx0; p[12]=rz0; p[13]=by; p[14]=mcount;\
            if (mcount <= 32) {\
                volatile unsigned *ab = PB + 16;\
                const unsigned char *s = (const unsigned char *)ang;\
                unsigned i2;\
                for (i2 = 0; i2 < mcount*3; i2++) ab[i2] = s[i2];\
            }\
        }\
        int main(void) {\
            unsigned i; int sum = 0;\
            PB = buf;\
            for (i = 0; i < 64; i++) buf[i] = 0;\
            for (i = 0; i < 45; i++) ANG[i] = (unsigned char)(i + 1);\
            kick(0,0,0,ANG,0,0, 1,2,3,4,5,6,7,8, 15);\
            for (i = 0; i < 45; i++) sum += (int)buf[16+i] * (int)(i + 1);\
            return sum;\
        }";
    // every angle byte is (i+1), weighted by (i+1): sum of squares 1..45
    let want: i32 = (1..=45).map(|k| k * k).sum();
    assert_eq!(run(src), want as u32);
}

#[test]
fn sub_word_params_are_not_truncated() {
    // Arguments are pushed as 32-bit longs, so a `short` parameter's value sits
    // in the LOW word of its slot on this big-endian chip — at 10(a6), not
    // 8(a6). Reading the slot as a word from its base returns the zero padding.
    // This is the joypad strobe bug: `strobe(0x81FE)` saw sel == 0, so all four
    // scan columns wrote the same value and the pad read back nothing.
    let src = r#"
        unsigned short ident(unsigned short x) { return x; }
        int main(void) { return (int)ident(0x81FEu); }
    "#;
    assert_eq!(run(src), 0x81FE);
}

#[test]
fn sub_word_params_all_widths() {
    let src = r#"
        unsigned char  u8(unsigned char c)  { return c; }
        signed   char  s8(signed char c)    { return c; }
        short          s16(short s)         { return s; }
        int main(void) {
            return (int)u8(0xB7) + (int)s16(-1234) + (int)s8(-7);
        }
    "#;
    assert_eq!(run(src), (0xB7i32 - 1234 - 7) as u32);
}

#[test]
fn rle_never_folds_a_pointer_load() {
    // Two reads of a hardware register must both reach the bus. The frontend
    // throws `volatile` away on pointed-to types, so codegen cannot tell this
    // from an ordinary field load — which is exactly why the pass only folds
    // A6-relative (stack) addresses. Checked against the asm, because both
    // reads return the same value in a simulator and the bug would be silent.
    let src = r#"
        int poll2(volatile unsigned *r) {
            unsigned a = *r;
            unsigned b = *r;
            return (int)(a + b);
        }
    "#;
    let asm = crate::compile_program(src).expect("compile");
    let body: String = asm
        .lines()
        .skip_while(|l| !l.trim_start().starts_with("poll2:"))
        .take_while(|l| !l.trim().starts_with("rts"))
        .collect::<Vec<_>>()
        .join("\n");
    let reads = body.matches("move.l (a2),").count();
    assert_eq!(reads, 2, "both volatile reads must survive:\n{body}");
}

#[test]
fn rle_store_forwarding_is_correct() {
    // A store makes its value available at that address; the reload must see
    // the value just written, not a stale one.
    let src = r#"
        int main() {
            int a[2];
            a[0] = 5;
            a[1] = a[0] + 1;
            a[0] = a[1] * 2;
            return a[0] + a[1];
        }
    "#;
    assert_eq!(run(src), 12 + 6);
}
