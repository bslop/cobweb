; bootstub.s — Cobweb calibration ROM: 68000 entry, VI handler, STOP helper.
;
; Init sequence follows the corpus's hardware-proven startup (a reference sandbox
; project's startup.S, [HW] on Skunkboard): NO MEMCON writes (the skunk/BJL boot
; environment already configured the bus — clobbering MEMCON kills the cart
; bus that skunklib's EZ-Host polling lives on), explicit GPU/DSP endianness,
; VI suppressed until main enables it, and a catch-all exception handler that
; leaves a crash breadcrumb at $820 instead of wild-jumping through zero.

	.extern	__bss_start,__bss_end,_cal_main

VIREG		equ	$F0004E		; vertical interrupt half-line (NOT $F0000E)
INT1REG		equ	$F000E0
INT2REG		equ	$F000E2

	.text

	.globl	_start
_start:
	move.w	#$2700,sr		; interrupts off during setup
	move.l	#$00070007,$F0210C	; G_END: GPU big-endian
	move.l	#$00050005,$F1A10C	; D_END: DSP big-endian
	lea	$200000,sp		; stack at top of 2MB DRAM
	move.w	#$FFFF,VIREG		; suppress vertical interrupts for now

	; Catch-all handler in vectors 2..255: a wild exception lands somewhere
	; observable (breadcrumb at $820) instead of jumping through zero.
	lea	exc_catch(pc),a1
	lea	$8,a0
	move.w	#256-2-1,d0
.vecs:
	move.l	a1,(a0)+
	dbra	d0,.vecs

	; All Jaguar 68k interrupts arrive at vector 64 / $100 (Tom bug 7).
	move.l	#vi_handler,$100.w

	; Zero BSS
	lea	__bss_start,a0
	lea	__bss_end,a1
	moveq	#0,d0
.zero:
	cmp.l	a1,a0
	bge.s	.zdone
	move.l	d0,(a0)+
	bra.s	.zero
.zdone:

	jsr	_cal_main
.halt:
	bra.s	.halt

; Crash breadcrumb: magic + SSP + top of the exception frame → $820, then spin.
exc_catch:
	move.l	#$EEEE0000,$820.w
	move.l	sp,$824.w
	move.l	(sp),$828.w
	move.l	4(sp),$82C.w
	move.l	8(sp),$830.w
	move.l	12(sp),$834.w
.espin:
	bra.s	.espin

vi_handler:
	move.w	#$0101,INT1REG		; keep VI enabled (bit 0) + clear latch (bit 8)
	move.w	#$0000,INT2REG		; restore GPU/Blitter bus priorities
	rte

; void cpu_stop(void) — sleep until the next interrupt (IPL 0, supervisor).
	.globl	_cpu_stop
_cpu_stop:
	stop	#$2000
	rts

; void irq_on(void) — supervisor mode, all interrupt levels enabled.
	.globl	_irq_on
_irq_on:
	move.w	#$2000,sr
	rts
