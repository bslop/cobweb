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
    let pp = preprocess::preprocess(src, path, include_dirs)?;
    compile(&pp)
}

/// Preprocess C source (exposed for tooling / `-E`).
pub fn preprocess_only(src: &str, path: &std::path::Path, include_dirs: &[String]) -> Result<String, String> {
    preprocess::preprocess(src, path, include_dirs)
}

/// A complete, assemblable program: startup (`_start`) → the compiled unit →
/// the runtime helpers. `entry` is the label the startup calls (usually `_main`).
/// Assemble with `jas` in 68000 mode at the chosen org.
pub fn compile_program(src: &str) -> Result<String, String> {
    let user = compile(src)?;
    Ok(format!("{}\n{}\n{}", startup(), user, runtime()))
}

/// The C runtime: 32-bit multiply/divide/modulo helpers the 68000 can't do in
/// hardware (it only has 16×16 MUL and 32÷16 DIV). All take operands in D0/D1
/// and return in D0, C-clobbering only caller-saved registers.
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
	jsr	_main
	move.l	d0,$100
_start_halt:
	bra	_start_halt
"#;

const RUNTIME: &str = r#"
; ── 32-bit runtime helpers (operands D0,D1 → result D0) ──────────────────────
	.68000
	.text
	.globl __mulsi3
	.globl __udivsi3
	.globl __umodsi3
	.globl __divsi3
	.globl __modsi3
	.globl __mulfix
	.globl __divfix

; D0 = D0 * D1  (32×32→32, low 32 bits)
__mulsi3:
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

; D0 = D0 / D1 (unsigned), remainder left in D1
__udivsi3:
	movem.l	d2-d4,-(a7)
	move.l	d0,d2			; dividend
	move.l	d1,d3			; divisor
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

; D0 = D0 % D1 (unsigned)
__umodsi3:
	bsr.w	__udivsi3
	move.l	d1,d0
	rts

; D0 = D0 / D1 (signed)
__divsi3:
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
	bsr.w	__udivsi3
	tst.l	d5
	beq.w	__div_done
	neg.l	d0
__div_done:
	movem.l	(a7)+,d5
	rts

; D0 = D0 % D1 (signed; result takes the dividend's sign)
__modsi3:
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
	bsr.w	__udivsi3
	move.l	d1,d0
	tst.l	d5
	beq.w	__mod_done
	neg.l	d0
__mod_done:
	movem.l	(a7)+,d5
	rts

; ── 16.16 fixed-point helpers ────────────────────────────────────────────────
; D0 = (D0 * D1) >> 16, signed 16.16. Builds the full 64-bit product from four
; 16×16 partials, then shifts right 16.
__mulfix:
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

; D0 = (D0 << 16) / D1, signed 16.16. Shift-subtract with 48 iterations: after
; the 32 dividend bits are consumed the shift feeds zeros, producing the 16
; fractional quotient bits.
__divfix:
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
