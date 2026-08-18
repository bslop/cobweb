//! jcc68k — a C compiler targeting the Motorola 68000 (the Jaguar's CPU).
//!
//! Pipeline: `lex` → `parse` (typed AST) → `codegen` (68000 assembly). The
//! emitted assembly is fed to **jas** (the trusted assembler), so the compiler
//! itself stays untrusted and auditable — you can read every instruction it
//! produces. The 68000 lacks 32-bit MUL/DIV, so those lower to the runtime
//! helpers in [`runtime`].

mod ast;
mod codegen;
mod lexer;
mod parser;
mod preprocess;

pub use parser::const_eval;

/// Compile C source to 68000 assembly (user code only — no runtime/startup).
/// `src` is assumed already preprocessed (see [`compile_file`] for the full
/// path that runs the preprocessor).
pub fn compile(src: &str) -> Result<String, String> {
    let toks = lexer::lex(src)?;
    let prog = parser::parse(toks)?;
    codegen::generate(&prog)
}

/// Preprocess then compile: runs `#include`/`#define`/`#if` against `src`
/// (whose on-disk location is `path`, for relative includes) with the given
/// `-I` include directories, then compiles the result.
pub fn compile_file(src: &str, path: &std::path::Path, include_dirs: &[String]) -> Result<String, String> {
    compile_file_with(src, path, include_dirs, &[])
}

/// Like [`compile_file`], with command-line `-D` macros (`NAME` or `NAME=BODY`).
pub fn compile_file_with(
    src: &str,
    path: &std::path::Path,
    include_dirs: &[String],
    defines: &[String],
) -> Result<String, String> {
    let pp = preprocess::preprocess_with(src, path, include_dirs, defines)?;
    compile(&pp)
}

/// Preprocess C source (exposed for tooling / `-E`).
pub fn preprocess_only(src: &str, path: &std::path::Path, include_dirs: &[String]) -> Result<String, String> {
    preprocess::preprocess(src, path, include_dirs)
}

/// Preprocess with command-line `-D` macros (for `-E` + `-D`).
pub fn preprocess_only_with(
    src: &str,
    path: &std::path::Path,
    include_dirs: &[String],
    defines: &[String],
) -> Result<String, String> {
    preprocess::preprocess_with(src, path, include_dirs, defines)
}

/// A complete, assemblable program: startup (`_start`) → the compiled unit →
/// the runtime helpers. `entry` is the label the startup calls (usually `_main`).
/// Assemble with `jas` in 68000 mode at the chosen org.
pub fn compile_program(src: &str) -> Result<String, String> {
    let user = compile(src)?;
    Ok(format!("{}\n{}\n{}", startup(), user, runtime()))
}

/// The C runtime: 32-bit multiply/divide/modulo helpers the 68000 can't do in
/// hardware (it only has 16×16 MUL and 32÷16 DIV). **libgcc calling
/// convention**: both operands on the stack (first at 4(sp)), result in D0,
/// caller pops — so these are drop-in interchangeable with libgcc's own
/// helpers (or a project's re-assembled copy) at link time, in both
/// directions. Only D0/D1 are clobbered.
pub fn runtime() -> &'static str {
    RUNTIME
}

/// A minimal startup: set the stack, call `_main`, stash its return value in D0
/// at a known address, and halt (spin). Suitable for the test harness and for a
/// bare-metal entry; a real cart would replace this.
pub fn startup() -> &'static str {
    STARTUP
}

const STARTUP: &str = r#"
	.68000
	.text
	.globl _start
_start:
	movea.l	#$001F0000,a7
	jsr	main
	move.l	d0,$100
_start_halt:
	bra	_start_halt
"#;

const RUNTIME: &str = r#"
; ── 32-bit runtime helpers ───────────────────────────────────────────────────
; libgcc calling convention: a at 4(sp), b at 8(sp), result in D0, caller
; pops. Public entries load the stack args then fall into register-based
; cores (the `_regs` labels, used for the internal calls).
	.68000
	.text
	.globl __mulsi3
	.globl __udivsi3
	.globl __umodsi3
	.globl __divsi3
	.globl __modsi3
	.globl __mulfix
	.globl __divfix

; D0 = a * b  (32×32→32, low 32 bits)
__mulsi3:
	move.l	4(a7),d0
	move.l	8(a7),d1
	movem.l	d2-d5,-(a7)
	move.l	d0,d2			; a
	move.l	d1,d3			; b
	move.w	d2,d0
	mulu.w	d3,d0			; a_lo * b_lo
	move.l	d2,d4
	swap	d4			; d4.w = a_hi
	mulu.w	d3,d4			; a_hi * b_lo
	move.l	d3,d5
	swap	d5			; d5.w = b_hi
	mulu.w	d2,d5			; a_lo * b_hi
	add.w	d5,d4			; (a_hi*b_lo + a_lo*b_hi) low16
	swap	d4
	clr.w	d4			; << 16
	add.l	d4,d0
	movem.l	(a7)+,d2-d5
	rts

; D0 = a / b (unsigned), remainder left in D1
__udivsi3:
	move.l	4(a7),d0
	move.l	8(a7),d1
__udivsi3_regs:
	movem.l	d2-d4,-(a7)
	move.l	d0,d2			; dividend
	move.l	d1,d3			; divisor
; ── fast path: a divisor that fits in 16 bits ───────────────────────────────
; The 68000 HAS a divide — DIVU.W, 32÷16 → 16 — it just cannot do 32÷32. Two
; DIVUs give the full 32-bit quotient whenever the divisor fits in a word, at
; ~340 cycles against the shift-subtract loop's ~1,450.
;
;   q_hi = hi16 / v          r_hi = hi16 % v        (q_hi < 2^16 since v >= 1)
;   q_lo = (r_hi:lo16) / v                          (< 2^16 since r_hi < v)
;   quotient = q_hi:q_lo     remainder = (r_hi:lo16) % v
;
; Neither DIVU can overflow: that is what `r_hi < v` buys, and it is why the
; second dividend is built from the FIRST division's remainder rather than
; from the dividend again.
	cmp.l	#$FFFF,d3
	bhi.w	__udiv_wide		; divisor >= 65536 — no 16-bit form
	tst.w	d3
	beq.w	__udiv_wide		; /0 is UB; keep the loop's terminating behaviour
	move.l	d2,d4
	clr.w	d4
	swap	d4			; d4 = hi16, zero-extended
	divu.w	d3,d4			; d4 = [r_hi : q_hi]
	move.l	d4,d1			; keep q_hi
	move.w	d2,d4			; d4 = [r_hi : lo16] — the rest of the dividend
	divu.w	d3,d4			; d4 = [rem : q_lo]
	move.l	d1,d0
	swap	d0			; d0 = [q_hi : r_hi]
	move.w	d4,d0			; d0 = [q_hi : q_lo] = quotient
	move.l	d4,d1
	clr.w	d1
	swap	d1			; d1 = remainder
	movem.l	(a7)+,d2-d4
	rts

__udiv_wide:
	moveq	#0,d0			; quotient
	moveq	#0,d4			; remainder
	moveq	#31,d1
__udiv_loop:
	add.l	d2,d2			; dividend <<= 1, MSB → X
	addx.l	d4,d4			; remainder = (remainder<<1) | X
	add.l	d0,d0			; quotient <<= 1
	cmp.l	d3,d4			; remainder >= divisor ?
	bcs.w	__udiv_skip
	sub.l	d3,d4
	addq.l	#1,d0
__udiv_skip:
	dbra	d1,__udiv_loop
	move.l	d4,d1			; remainder
	movem.l	(a7)+,d2-d4
	rts

; D0 = a % b (unsigned)
__umodsi3:
	move.l	4(a7),d0
	move.l	8(a7),d1
	bsr.w	__udivsi3_regs
	move.l	d1,d0
	rts

; D0 = a / b (signed)
__divsi3:
	move.l	4(a7),d0
	move.l	8(a7),d1
	movem.l	d5,-(a7)
	moveq	#0,d5
	tst.l	d0
	bpl.w	__div_p1
	neg.l	d0
	not.l	d5
__div_p1:
	tst.l	d1
	bpl.w	__div_p2
	neg.l	d1
	not.l	d5
__div_p2:
	bsr.w	__udivsi3_regs
	tst.l	d5
	beq.w	__div_done
	neg.l	d0
__div_done:
	movem.l	(a7)+,d5
	rts

; D0 = a % b (signed; result takes the dividend's sign)
__modsi3:
	move.l	4(a7),d0
	move.l	8(a7),d1
	movem.l	d5,-(a7)
	moveq	#0,d5
	tst.l	d0
	bpl.w	__mod_p1
	neg.l	d0
	not.l	d5
__mod_p1:
	tst.l	d1
	bpl.w	__mod_p2
	neg.l	d1
__mod_p2:
	bsr.w	__udivsi3_regs
	move.l	d1,d0
	tst.l	d5
	beq.w	__mod_done
	neg.l	d0
__mod_done:
	movem.l	(a7)+,d5
	rts

; ── 16.16 fixed-point helpers (same stack ABI) ───────────────────────────────
; D0 = (a * b) >> 16, signed 16.16. Builds the full 64-bit product from four
; 16×16 partials, then shifts right 16.
__mulfix:
	move.l	4(a7),d0
	move.l	8(a7),d1
	movem.l	d2-d7,-(a7)
	moveq	#0,d7			; sign
	tst.l	d0
	bpl.w	__mf_a
	neg.l	d0
	not.l	d7
__mf_a:
	tst.l	d1
	bpl.w	__mf_b
	neg.l	d1
	not.l	d7
__mf_b:
	move.w	d0,d3			; a0
	move.w	d1,d2			; b0
	mulu.w	d2,d3			; d3 = a0*b0 (pll)
	move.l	d0,d4
	swap	d4			; d4.w = a1
	move.w	d1,d5
	mulu.w	d4,d5			; d5 = a1*b0 (phl)
	move.l	d1,d6
	swap	d6			; d6.w = b1
	move.w	d0,d2
	mulu.w	d6,d2			; d2 = a0*b1 (plh)
	mulu.w	d6,d4			; d4 = a1*b1 (phh)
	; mid = plh + phl, carry → phh<<16
	add.l	d5,d2			; d2 = mid, X = carry
	moveq	#0,d0
	addx.l	d0,d0			; d0 = carry
	swap	d0			; carry << 16
	add.l	d0,d4			; phh += carry<<16
	; lo = pll + (mid_lo16 << 16)
	move.l	d2,d0
	swap	d0
	clr.w	d0			; (mid & $FFFF) << 16
	add.l	d0,d3			; lo(d3), X = carry_lo
	moveq	#0,d1
	addx.l	d1,d1			; d1 = carry_lo
	; hi = phh + (mid >> 16) + carry_lo
	move.l	d2,d0
	clr.w	d0
	swap	d0			; mid >> 16
	add.l	d0,d4			; hi(d4)
	add.l	d1,d4			; += carry_lo
	; result = (lo >> 16) | (hi << 16)
	move.l	d3,d0
	clr.w	d0
	swap	d0			; lo >> 16
	move.l	d4,d1
	swap	d1
	clr.w	d1			; hi << 16
	or.l	d1,d0
	tst.l	d7
	beq.w	__mf_done
	neg.l	d0
__mf_done:
	movem.l	(a7)+,d2-d7
	rts

; D0 = (a << 16) / b, signed 16.16. Shift-subtract with 48 iterations: after
; the 32 dividend bits are consumed the shift feeds zeros, producing the 16
; fractional quotient bits.
__divfix:
	move.l	4(a7),d0
	move.l	8(a7),d1
	movem.l	d2-d5,-(a7)
	moveq	#0,d5			; sign
	tst.l	d0
	bpl.w	__df_a
	neg.l	d0
	not.l	d5
__df_a:
	tst.l	d1
	bpl.w	__df_b
	neg.l	d1
	not.l	d5
__df_b:
	move.l	d0,d2			; dividend
	move.l	d1,d3			; divisor
	moveq	#0,d0			; quotient
	moveq	#0,d4			; remainder
	moveq	#47,d1			; 32 + 16 iterations
__df_loop:
	add.l	d2,d2			; dividend <<= 1, MSB → X (0 once exhausted)
	addx.l	d4,d4			; remainder = (remainder<<1)|X
	add.l	d0,d0			; quotient <<= 1
	cmp.l	d3,d4
	bcs.w	__df_skip
	sub.l	d3,d4
	addq.l	#1,d0
__df_skip:
	dbra	d1,__df_loop
	tst.l	d5
	beq.w	__df_done
	neg.l	d0
__df_done:
	movem.l	(a7)+,d2-d5
	rts
"#;

#[cfg(test)]
mod tests;
