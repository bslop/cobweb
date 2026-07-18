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

pub use parser::const_eval;

/// Compile C source to 68000 assembly (user code only — no runtime/startup).
pub fn compile(src: &str) -> Result<String, String> {
    let toks = lexer::lex(src)?;
    let prog = parser::parse(toks)?;
    codegen::generate(&prog)
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
"#;

#[cfg(test)]
mod tests;
