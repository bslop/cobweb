; probes.s — Cobweb jsim calibration probes (Tom GPU kernels).
;
; Each probe is a self-contained GPU kernel, copied whole into GPU SRAM by
; main.c and run to completion. The kernel times itself against the VC
; half-line counter (self-calibrating clock: probe `vcmod` measures the wrap
; modulus on the actual rig — the folklore value has been wrong before), then
; writes {start, end, wraps, magic} to a DRAM result slot and stops itself
; (bug 23: only the local processor may clear its own GO bit).
;
; Wrap detection happens once per outer repetition; every body is far shorter
; than half a field, so wraps cannot be missed.
;
; Register plan (harness): r15 mask, r16 tmp, r17 reps, r18 result, r19 aux,
; r20 VC addr, r21 start, r22 prev, r23 wraps, r24 tick, r26-r28 epilogue.
; Bodies use r0-r14 freely.

PRMREPS		equ	$F03F80
PRMRESULT	equ	$F03F84
PRMAUX		equ	$F03F88
GRAMBASE	equ	$F03000
OUTERPC		equ	GRAMBASE+50	; .outer offset after PROBE_PRO (50 bytes);
				; JR can't span a 1KB body, so the outer
				; back-edge is `jump ne,(r25)` with r25
				; preloaded to this fixed kernel address
VCADDR		equ	$F00006
GCTRL		equ	$F02114
MAGICD		equ	$C0DED04E
DRAMCODE	equ	$60000		; long-aligned staging base for main-RAM bodies

	.data
	.even

; ── measurement harness ─────────────────────────────────────────────────────

	.macro	PROBE_PRO
	movei	#PRMREPS,r16
	load	(r16),r17		; outer repetitions
	movei	#PRMRESULT,r16
	load	(r16),r18		; result block pointer
	movei	#PRMAUX,r16
	load	(r16),r19		; aux (buffer / staged-body address)
	movei	#VCADDR,r20
	movei	#$7FF,r15		; VC mask (strip the field bit)
	loadw	(r20),r21
	and	r15,r21			; start tick
	move	r21,r22			; prev tick (wrap detection)
	moveq	#0,r23			; wrap count
	movei	#OUTERPC,r25		; outer-loop top (kernel runs at GRAMBASE)
.outer:
	.endm

	.macro	PROBE_EPI
	loadw	(r20),r24
	and	r15,r24
	cmp	r22,r24			; r24 - r22: borrow set ⇒ VC wrapped
	jr	cc,.nowrap
	move	r24,r22			; slot: prev update needed on both paths
	addq	#1,r23
.nowrap:
	subq	#1,r17
	jump	ne,(r25)
	nop
	loadw	(r20),r24
	and	r15,r24
	store	r21,(r18)		; [0] start tick
	addqt	#4,r18
	store	r24,(r18)		; [4] end tick
	addqt	#4,r18
	store	r23,(r18)		; [8] wraps
	addqt	#4,r18
	movei	#MAGICD,r26
	store	r26,(r18)		; [12] done magic — written LAST
	movei	#GCTRL,r27
	moveq	#0,r28
	store	r28,(r27)		; stop self
	nop
	nop
	.endm

; ── p_vcmod: VC wrap modulus discovery (max masked VC over many fields) ─────

	.even
	.globl	_p_vcmod_s
	.globl	_p_vcmod_e
_p_vcmod_s:
	.gpu
	movei	#PRMREPS,r16
	load	(r16),r17
	movei	#PRMRESULT,r16
	load	(r16),r18
	movei	#VCADDR,r20
	movei	#$7FF,r15
	moveq	#0,r25			; max seen
.mspin:
	loadw	(r20),r24
	and	r15,r24
	cmp	r24,r25			; r25 - r24: borrow ⇒ new max
	jr	cc,.nomax
	nop
	move	r24,r25
.nomax:
	subq	#1,r17
	jr	ne,.mspin
	nop
	store	r25,(r18)		; [0] max VC (modulus - 1)
	addqt	#4,r18
	store	r25,(r18)		; [4] repeat (unused)
	addqt	#4,r18
	moveq	#0,r23
	store	r23,(r18)		; [8] zero
	addqt	#4,r18
	movei	#MAGICD,r26
	store	r26,(r18)
	movei	#GCTRL,r27
	moveq	#0,r28
	store	r28,(r27)
	nop
	nop
	.68000
	.data
_p_vcmod_e:

; ── p_null: empty body — measures harness overhead per repetition ───────────

	.even
	.globl	_p_null_s
	.globl	_p_null_e
_p_null_s:
	.gpu
	PROBE_PRO
	PROBE_EPI
	.68000
	.data
_p_null_e:

; ── p_nop: 512 NOPs — baseline local-SRAM issue rate ────────────────────────

	.even
	.globl	_p_nop_s
	.globl	_p_nop_e
_p_nop_s:
	.gpu
	PROBE_PRO
	.rept	512
	nop
	.endr
	PROBE_EPI
	.68000
	.data
_p_nop_e:

; ── p_move: 512 reg-to-reg MOVEs (the U-235 "SCIENCE" replication) ──────────

	.even
	.globl	_p_move_s
	.globl	_p_move_e
_p_move_s:
	.gpu
	PROBE_PRO
	.rept	512
	move	r0,r1
	.endr
	PROBE_EPI
	.68000
	.data
_p_move_e:

; ── p_moveq: 512 MOVEQs — pure fast-write stream ────────────────────────────

	.even
	.globl	_p_moveq_s
	.globl	_p_moveq_e
_p_moveq_s:
	.gpu
	PROBE_PRO
	.rept	512
	moveq	#1,r1
	.endr
	PROBE_EPI
	.68000
	.data
_p_moveq_e:

; ── p_adddep: 512 dependent ADDs — every op reads the previous result ───────
; jsim predicts one bubble each (result written at cycle 3).

	.even
	.globl	_p_adddep_s
	.globl	_p_adddep_e
_p_adddep_s:
	.gpu
	PROBE_PRO
	.rept	256
	add	r1,r2
	add	r2,r1
	.endr
	PROBE_EPI
	.68000
	.data
_p_adddep_e:

; ── p_addind: 512 interleaved ADDs — two independent chains, no bubbles ─────

	.even
	.globl	_p_addind_s
	.globl	_p_addind_e
_p_addind_s:
	.gpu
	PROBE_PRO
	.rept	256
	add	r1,r2
	add	r3,r4
	.endr
	PROBE_EPI
	.68000
	.data
_p_addind_e:

; ── p_ldsram: 256 dependent internal loads (load + consume) ─────────────────

	.even
	.globl	_p_ldsram_s
	.globl	_p_ldsram_e
_p_ldsram_s:
	.gpu
	PROBE_PRO
	.rept	256
	load	(r19),r1
	or	r1,r1
	.endr
	PROBE_EPI
	.68000
	.data
_p_ldsram_e:

; ── p_ldidx: 256 dependent indexed internal loads — the +2 issue overhead ───

	.even
	.globl	_p_ldidx_s
	.globl	_p_ldidx_e
_p_ldidx_s:
	.gpu
	PROBE_PRO
	move	r19,r14
	.rept	256
	load	(r14+1),r1
	or	r1,r1
	.endr
	PROBE_EPI
	.68000
	.data
_p_ldidx_e:

; ── p_lddram: 256 sequential DRAM loads — page-hit data cost ────────────────

	.even
	.globl	_p_lddram_s
	.globl	_p_lddram_e
_p_lddram_s:
	.gpu
	PROBE_PRO
	move	r19,r10
	; destinations rotate r1-r4: consecutive loads into ONE register would
	; themselves be the bug-13 WAW hazard — we're measuring the bus here.
	.rept	64
	load	(r10),r1
	addqt	#4,r10
	load	(r10),r2
	addqt	#4,r10
	load	(r10),r3
	addqt	#4,r10
	load	(r10),r4
	addqt	#4,r10
	.endr
	PROBE_EPI
	.68000
	.data
_p_lddram_e:

; ── p_ldstride: 128 DRAM loads striding 2KB — page-miss data cost ───────────

	.even
	.globl	_p_ldstride_s
	.globl	_p_ldstride_e
_p_ldstride_s:
	.gpu
	PROBE_PRO
	move	r19,r10
	movei	#2048,r11
	; rotating destinations, as in p_lddram
	.rept	32
	load	(r10),r1
	add	r11,r10
	load	(r10),r2
	add	r11,r10
	load	(r10),r3
	add	r11,r10
	load	(r10),r4
	add	r11,r10
	.endr
	PROBE_EPI
	.68000
	.data
_p_ldstride_e:

; ── p_stdram: 256 sequential DRAM stores — write-buffer throughput ──────────

	.even
	.globl	_p_stdram_s
	.globl	_p_stdram_e
_p_stdram_s:
	.gpu
	PROBE_PRO
	move	r19,r10
	.rept	256
	store	r1,(r10)
	addqt	#4,r10
	.endr
	PROBE_EPI
	.68000
	.data
_p_stdram_e:

; ── p_blitsm / p_blitbg: Blitter textured-copy cost (launch + bwait) ─────────
; OpenLara's span fill: SRCEN|LFU_REPLACE|DSTA2, 8bpp, reading a source and
; writing the framebuffer. Each rep programs a copy of N pixels, launches it,
; and spins in bwait until B_CMD reads idle — exactly the GPU's real pattern.
; jsim currently models the blit as free (B_CMD reads idle instantly), so
; hardware-minus-sim on the two sizes (8 vs 256 px) pins base + per-pixel cost.
BLITSRC		equ	$00140000	; seeded DRAM buffer (source)
BLITDST		equ	$00180000	; scratch DRAM (dest)
BB_A1BASE	equ	$F02200
BB_A1FLAGS	equ	$F02204
BB_A1PIX	equ	$F0220C
BB_A2BASE	equ	$F02224
BB_A2FLAGS	equ	$F02228
BB_A2PIX	equ	$F02230
BB_BCOUNT	equ	$F0223C
BB_BCMD		equ	$F02238
BB_FLAGS8	equ	$00014218	; PITCH1|PIXEL8|WID320|XADDPIX (OpenLara A2)
BB_CMDTEX	equ	$01800801	; SRCEN|LFU_REPLACE|DSTA2 (OpenLara span)

	.macro	BLITSETUP
	movei	#BB_A1BASE,r0
	movei	#BLITSRC,r1
	store	r1,(r0)
	movei	#BB_A1FLAGS,r0
	movei	#BB_FLAGS8,r1
	store	r1,(r0)
	movei	#BB_A1PIX,r0
	moveq	#0,r1
	store	r1,(r0)
	movei	#BB_A2BASE,r0
	movei	#BLITDST,r1
	store	r1,(r0)
	movei	#BB_A2FLAGS,r0
	movei	#BB_FLAGS8,r1
	store	r1,(r0)
	movei	#BB_A2PIX,r0
	moveq	#0,r1
	store	r1,(r0)
	.endm

	.even
	.globl	_p_blitsm_s
	.globl	_p_blitsm_e
_p_blitsm_s:
	.gpu
	PROBE_PRO
	BLITSETUP
	movei	#BB_BCOUNT,r0
	movei	#$00010008,r1		; 1 row x 8 pixels
	store	r1,(r0)
	movei	#BB_BCMD,r2
	movei	#BB_CMDTEX,r1
	store	r1,(r2)			; launch
.bwsm:
	load	(r2),r1
	btst	#0,r1
	jr	eq,.bwsm		; spin until idle bit set
	nop
	PROBE_EPI
	.68000
	.data
_p_blitsm_e:

	.even
	.globl	_p_blitbg_s
	.globl	_p_blitbg_e
_p_blitbg_s:
	.gpu
	PROBE_PRO
	BLITSETUP
	movei	#BB_BCOUNT,r0
	movei	#$00010100,r1		; 1 row x 256 pixels
	store	r1,(r0)
	movei	#BB_BCMD,r2
	movei	#BB_CMDTEX,r1
	store	r1,(r2)			; launch
.bwbg:
	load	(r2),r1
	btst	#0,r1
	jr	eq,.bwbg
	nop
	PROBE_EPI
	.68000
	.data
_p_blitbg_e:

; ── p_blittex1 / p_blittexq: TEXTURED (XADDINC) span cost ───────────────────
; The cost model was calibrated from a LINEAR XADDPIX copy (p_blitsm/p_blitbg)
; and the per-access constant extrapolated to the textured case. OpenLara's real
; span uses A1 in XADDINC mode — a 16.16 fractional sampler — where consecutive
; destination pixels can land in the SAME source phrase. If hardware reuses a
; latched phrase, the model's "one source access per pixel" over-charges, which
; is the 2.4x fill over-charge reported against 8ca3fc0.
;   blittex1 : du = 1.0   (a fresh texel per pixel, minimal reuse)
;   blittexq : du = 0.25  (4 pixels per texel, heavy reuse)
; Same 256-px span, same everything else — the delta isolates source coalescing.
BT_A1FPIX	equ	$F02218
BT_A1INC	equ	$F0221C
BT_A1FINC	equ	$F02220
BT_TEXFLAGS	equ	$00034018	; PIXEL8|XADDINC|WID256 (OpenLara A1_FIXED|WID)

	.macro	BLITTEX
	movei	#BB_A1BASE,r0
	movei	#BLITSRC,r1
	store	r1,(r0)
	movei	#BB_A1FLAGS,r0
	movei	#BT_TEXFLAGS,r1
	store	r1,(r0)
	movei	#BB_A1PIX,r0
	moveq	#0,r1
	store	r1,(r0)
	movei	#BT_A1FPIX,r0
	moveq	#0,r1
	store	r1,(r0)
	movei	#BT_A1INC,r0
	movei	#\1,r1
	store	r1,(r0)
	movei	#BT_A1FINC,r0
	movei	#\2,r1
	store	r1,(r0)
	movei	#BB_A2BASE,r0
	movei	#BLITDST,r1
	store	r1,(r0)
	movei	#BB_A2FLAGS,r0
	movei	#BB_FLAGS8,r1
	store	r1,(r0)
	movei	#BB_A2PIX,r0
	moveq	#0,r1
	store	r1,(r0)
	movei	#BB_BCOUNT,r0
	movei	#$00010100,r1		; 1 row x 256 px
	store	r1,(r0)
	movei	#BB_BCMD,r2
	movei	#BB_CMDTEX,r1
	store	r1,(r2)			; launch
	.endm

	.even
	.globl	_p_blittex1_s
	.globl	_p_blittex1_e
_p_blittex1_s:
	.gpu
	PROBE_PRO
	BLITTEX	1, 0			; du = 1.0
.bwt1:
	load	(r2),r1
	btst	#0,r1
	jr	eq,.bwt1
	nop
	PROBE_EPI
	.68000
	.data
_p_blittex1_e:

	.even
	.globl	_p_blittexq_s
	.globl	_p_blittexq_e
_p_blittexq_s:
	.gpu
	PROBE_PRO
	BLITTEX	0, $4000		; du = 0.25 -> 4 px per source texel
.bwtq:
	load	(r2),r1
	btst	#0,r1
	jr	eq,.bwtq
	nop
	PROBE_EPI
	.68000
	.data
_p_blittexq_e:

; ── p_dsphammer: resident DSP DRAM read-hammer (concurrent bus-noise source) ─
; main.c starts this on Jerry (DSP) before a Tom probe and stops it after, so
; the timed Tom stream runs against real Jerry↔Tom DRAM contention on the one
; shared 64-bit bus — the term the fps model misses during render (68k STOPped,
; so its contention tax is off, but Jerry/OP still hammer). Uses only relative
; `jr` (no absolute label), so it runs wherever main.c stages it (D_RAM).
DHAMMER_BUF	equ	$001C0000
DHAMMER_END	equ	$001E0000
	.even
	.globl	_p_dsphammer_s
	.globl	_p_dsphammer_e
; BOUNDED: a free-running hammer starves the shared bus so hard the 68k's
; mode-A busy-poll never retires and the suite wedges (observed on hardware —
; the run stopped dead at this probe). It now runs a fixed number of passes,
; sized to outlast the Tom probe, then clears its OWN GO bit (TRM bug 23: only
; the local processor may stop itself) so the machine always recovers.
DHAMMER_PASSES	equ	64
D_CTRL_R	equ	$F1A114
; Loop-top address once staged in D_RAM. The setup ahead of dh_loop is
; 7 movei (3 words each) + store + move = 20 words = 40 bytes, so the body
; starts at D_RAM+$28. (Same fixed-address trick as the GPU probes' OUTERPC —
; the unrolled body is far past jr's ±16-word reach.)
DH_LOOPPC	equ	$F1B028
DHAMMER_MARK	equ	$001B0000	; "the DSP actually ran" witness
_p_dsphammer_s:
	.dsp
	movei	#DHAMMER_MARK,r8	; prove we started (main.c prints this)
	movei	#$D50D50D5,r9
	store	r9,(r8)
	movei	#DHAMMER_BUF,r1
	movei	#DHAMMER_END,r3
	movei	#DHAMMER_PASSES,r5
	movei	#DH_LOOPPC,r10		; unrolled body outruns jr's ±16 words
	move	r1,r2
dh_loop:
	; DENSE stream: 8 back-to-back loads with rotating destinations (same shape
	; as p_lddram) so Jerry demands as much DRAM bandwidth as it possibly can —
	; a sparse hammer would under-state contention and could fake a null result.
	load	(r2),r0
	addqt	#4,r2
	load	(r2),r4
	addqt	#4,r2
	load	(r2),r6
	addqt	#4,r2
	load	(r2),r7
	addqt	#4,r2
	load	(r2),r0
	addqt	#4,r2
	load	(r2),r4
	addqt	#4,r2
	load	(r2),r6
	addqt	#4,r2
	load	(r2),r7
	addqt	#4,r2
	cmp	r3,r2			; r2 - r3
	jump	cs,(r10)		; borrow ⇒ r2 < end ⇒ keep hammering
	nop
	move	r1,r2			; wrap to buffer start
	subq	#1,r5			; one pass done
	jump	ne,(r10)
	nop
	movei	#D_CTRL_R,r6		; passes exhausted — stop self
	moveq	#0,r7
	store	r7,(r6)
	nop
	nop
	.68000
	.data
_p_dsphammer_e:

; ── p_lddramc: CONSUMED sequential DRAM loads — full load-to-use latency ────
; (Session 2: pins DRAM_LAT_HIT, which session 1 could not measure.)

	.even
	.globl	_p_lddramc_s
	.globl	_p_lddramc_e
_p_lddramc_s:
	.gpu
	PROBE_PRO
	move	r19,r10
	.rept	128
	load	(r10),r1
	or	r1,r1
	addqt	#4,r10
	.endr
	PROBE_EPI
	.68000
	.data
_p_lddramc_e:

; ── p_divhot: DIV with immediate consumption — the full divide shadow ───────

	.even
	.globl	_p_divhot_s
	.globl	_p_divhot_e
_p_divhot_s:
	.gpu
	PROBE_PRO
	moveq	#3,r5
	.rept	64
	moveq	#29,r2
	div	r5,r2
	or	r2,r2
	.endr
	PROBE_EPI
	.68000
	.data
_p_divhot_e:

; ── p_divsh: DIV with 17 instructions of shadow work before consumption ─────
; jsim predicts the divide becomes ~free vs p_divhot.

	.even
	.globl	_p_divsh_s
	.globl	_p_divsh_e
_p_divsh_s:
	.gpu
	PROBE_PRO
	moveq	#3,r5
	.rept	32
	moveq	#29,r2
	div	r5,r2
	nop
	nop
	nop
	nop
	nop
	nop
	nop
	nop
	nop
	nop
	nop
	nop
	nop
	nop
	nop
	nop
	nop
	or	r2,r2
	.endr
	PROBE_EPI
	.68000
	.data
_p_divsh_e:

; ── p_jr: 512-iteration tight JR loop — taken-jump refill cost ──────────────

	.even
	.globl	_p_jr_s
	.globl	_p_jr_e
_p_jr_s:
	.gpu
	PROBE_PRO
	movei	#512,r5
.inner:
	subq	#1,r5
	jr	ne,.inner
	nop
	PROBE_EPI
	.68000
	.data
_p_jr_e:

; ── p_main: GPU-in-main harness — body staged to DRAM by the 68k ────────────
; SRAM-resident harness jumps into a DRAM-staged body and back, following the
; Owl/Scavone rules (MOVEI immediately before each cross-memory jump; the
; DRAM-side jump source LONG-aligned; two NOPs after the DRAM-side jump).
; EXPERIMENTAL on hardware: runs last; a hang needs a power cycle.

	.even
	.globl	_p_main_s
	.globl	_p_main_e
_p_main_s:
	.gpu
	PROBE_PRO
	move	pc,r13			; a      → r13 = a
	addq	#14,r13			; a+2    → return point = a+14
	movei	#DRAMCODE,r12		; a+4    target; MOVEI directly before the
	jump	(r12)			; a+10   jump settles the prefetch (Owl rule 2)
	nop				; a+12   delay slot
	; a+14: body returns here via jump (r13)
	PROBE_EPI
	.68000
	.data
_p_main_e:

; ── DRAM bodies for p_main (staged to long-aligned DRAM by main.c) ──────────
; Layout: 512 x 2-byte body ops = 1024 bytes, nop pad (2), movei (6) — so the
; jump source sits at +1032, long-aligned when the staging base is aligned.

	.even
	.globl	_pm_bodymov_s
	.globl	_pm_bodymov_e
_pm_bodymov_s:
	.gpu
	.rept	512
	move	r0,r1
	.endr
	nop				; alignment pad → jump lands on a long boundary
	movei	#0,r11		; settle prefetch (r11: body scratch, NOT r22 - harness prev-tick)
	jump	(r13)			; back to SRAM harness
	nop
	nop
	.68000
	.data
_pm_bodymov_e:

	.even
	.globl	_pm_bodynop_s
	.globl	_pm_bodynop_e
_pm_bodynop_s:
	.gpu
	.rept	512
	nop
	.endr
	nop
	movei	#0,r11
	jump	(r13)
	nop
	nop
	.68000
	.data
_pm_bodynop_e:
