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

; ── p_blit1 / p_blit2 / p_blit4: SHORT spans — the launch-overhead region ───
; The cost model was calibrated at 8 and 256 px and matches silicon within 1%
; there (blitbg exact, blitsm +7%) — but real geometry spends most of its spans
; far below 8 px, where the fixed launch cost dominates, and the whole-frame
; fill charge disagrees with hardware's NOFILL delta (jsim 24% vs hw ~10% of
; frame) while the per-blit probes agree. If that discrepancy is real per-blit
; cost, it must live here, in the region the calibration extrapolated through.
; Same config as blitsm (SRCEN|LFU_REPLACE|DSTA2, 8bpp, XADDPIX), only npix
; varies: 1/2/4 px complete the 1-2-4-8-256 curve.
	.macro	BLITN
	BLITSETUP
	movei	#BB_BCOUNT,r0
	movei	#\1,r1			; 1 row x N pixels
	store	r1,(r0)
	movei	#BB_BCMD,r2
	movei	#BB_CMDTEX,r1
	store	r1,(r2)			; launch
	.endm

	.even
	.globl	_p_blit1_s
	.globl	_p_blit1_e
_p_blit1_s:
	.gpu
	PROBE_PRO
	BLITN	$00010001
.bw1:
	load	(r2),r1
	btst	#0,r1
	jr	eq,.bw1
	nop
	PROBE_EPI
	.68000
	.data
_p_blit1_e:

	.even
	.globl	_p_blit2_s
	.globl	_p_blit2_e
_p_blit2_s:
	.gpu
	PROBE_PRO
	BLITN	$00010002
.bw2:
	load	(r2),r1
	btst	#0,r1
	jr	eq,.bw2
	nop
	PROBE_EPI
	.68000
	.data
_p_blit2_e:

	.even
	.globl	_p_blit4_s
	.globl	_p_blit4_e
_p_blit4_s:
	.gpu
	PROBE_PRO
	BLITN	$00010004
.bw4:
	load	(r2),r1
	btst	#0,r1
	jr	eq,.bw4
	nop
	PROBE_EPI
	.68000
	.data
_p_blit4_e:

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

; ── p_blitrmw: 256-px DSTEN read-modify-write OR fill — prices the dest READ ─
; The rect-shade pass is DSTEN|LFU(S|D): every dest phrase is read before the
; OR and the write-back. jsim charges that read at one access per pixel (the
; conservative reading of the blitbg calibration); if silicon coalesces or
; pipelines the RMW read, this probe reads BELOW blitbg+its-delta and the
; charge comes down. Delta vs p_blitbg isolates exactly the read term:
; blitbg = srcread+dstwrite per px; blitrmw = dstread+dstwrite per px.
; (COBWEB_BUG_blitter_overcharged round 2, suspect #1.)
BB_CMDRMW	equ	$01C00808	; DSTEN|LFU(S|D)|DSTA2, no SRCEN
BB_SRCD		equ	$F02240		; 64-bit source-data pair ($F02240/44)

	.even
	.globl	_p_blitrmw_s
	.globl	_p_blitrmw_e
_p_blitrmw_s:
	.gpu
	PROBE_PRO
	BLITSETUP
	movei	#BB_SRCD,r0		; OR pattern k, both phrase halves
	movei	#$5A5A5A5A,r1
	store	r1,(r0)
	movei	#BB_SRCD+4,r0
	store	r1,(r0)
	movei	#BB_BCOUNT,r0
	movei	#$00010100,r1		; 1 row x 256 pixels (same as blitbg)
	store	r1,(r0)
	movei	#BB_BCMD,r2
	movei	#BB_CMDRMW,r1
	store	r1,(r2)			; launch
.bwrmw:
	load	(r2),r1
	btst	#0,r1
	jr	eq,.bwrmw
	nop
	PROBE_EPI
	.68000
	.data
_p_blitrmw_e:


; ── p_densN: DRAM-load DENSITY SWEEP — the mode-A regime question ──────────
; lddram (back-to-back loads) pays 2x the model under a busy 68k; whole-game
; anchors (sparse access) fit within 1%. No flat constant fits both. One load
; per (2+N) instructions, N filler ALU ops: the curve locates the bus-grant
; transition so the charge can become density-aware with measured knees.

	.even
	.globl	_p_dens2_s
	.globl	_p_dens2_e
_p_dens2_s:
	.gpu
	PROBE_PRO
	move	r19,r10
	.rept	64
	load	(r10),r1
	addqt	#4,r10
	.rept	2
	or	r2,r2
	.endr
	.endr
	PROBE_EPI
	.68000
	.data
_p_dens2_e:

	.even
	.globl	_p_dens6_s
	.globl	_p_dens6_e
_p_dens6_s:
	.gpu
	PROBE_PRO
	move	r19,r10
	.rept	64
	load	(r10),r1
	addqt	#4,r10
	.rept	6
	or	r2,r2
	.endr
	.endr
	PROBE_EPI
	.68000
	.data
_p_dens6_e:

	.even
	.globl	_p_dens14_s
	.globl	_p_dens14_e
_p_dens14_s:
	.gpu
	PROBE_PRO
	move	r19,r10
	.rept	64
	load	(r10),r1
	addqt	#4,r10
	.rept	14
	or	r2,r2
	.endr
	.endr
	PROBE_EPI
	.68000
	.data
_p_dens14_e:

	.even
	.globl	_p_dens30_s
	.globl	_p_dens30_e
_p_dens30_s:
	.gpu
	PROBE_PRO
	move	r19,r10
	.rept	32
	load	(r10),r1
	addqt	#4,r10
	.rept	30
	or	r2,r2
	.endr
	.endr
	PROBE_EPI
	.68000
	.data
_p_dens30_e:


; ── p_ldcunderb: CONSUMED DRAM loads WHILE a long blit holds the bus ─────────
; THE remaining dense-geometry suspect (2026-07-22 discrimination: ALLCULL =
; staging + Jerry + no blits = EXACT; full builds = same + busy Blitter =
; +28%). ldunderb proved unconsumed loads ride through a blit (+3%); lddramc
; proved consumed loads cost 6.11 B on an idle-Blitter bus. This is the
; missing cell: geotex staging CONSUMES its loads while the async fill runs.
; Body = lddramc's consumed pairs, launched under a 2048-px blit.
	.even
	.globl	_p_ldcunderb_s
	.globl	_p_ldcunderb_e
_p_ldcunderb_s:
	.gpu
	PROBE_PRO
	BLITSETUP
	movei	#BB_BCOUNT,r0
	movei	#$00010080,r1		; 128 px (~1.4k cyc): the 384-instr consumed
					; run (~2.4k cyc quiet) OUTLASTS it, so the
					; probe is load-bound with the blit covering
					; the first ~60% — the overlap under test.
					; (2048/1024 px were blit-bound: drain only.)
	store	r1,(r0)
	movei	#BB_BCMD,r2
	movei	#BB_CMDTEX,r1
	store	r1,(r2)			; launch; do NOT bwait
	move	r19,r10
	.rept	128
	load	(r10),r1
	or	r1,r1			; consume — the stall under test
	addqt	#8,r10
	.endr
.bwlcu:
	load	(r2),r1			; drain for a clean next rep
	btst	#0,r1
	jr	eq,.bwlcu
	nop
	PROBE_EPI
	.68000
	.data
_p_ldcunderb_e:


; ── p_fireintobusy: B_CMD store while the Blitter is BUSY — held or queued? ──
; The ~8ms fill-slice gap (flag ladder 2026-07-22). Two 64-px blits fired
; back-to-back (2nd store lands mid-blit-1), then 1600 nops, then bwait.
; If silicon QUEUES the store (jsim model): nops overlap both blits →
; rep ≈ nops (~1600cyc). If silicon HOLDS the writer until blit 1 idles:
; rep ≈ blit1 + max(blit2,nops) (~2400cyc). 40% separation, sign decides.
	.even
	.globl	_p_fib_s
	.globl	_p_fib_e
_p_fib_s:
	.gpu
	PROBE_PRO
	BLITSETUP
	movei	#BB_BCOUNT,r0
	movei	#$00010040,r1		; 64 px
	store	r1,(r0)
	movei	#BB_BCMD,r2
	movei	#BB_CMDTEX,r1
	store	r1,(r2)			; launch 1
	movei	#BB_BCOUNT,r0		; re-arm count (blit consumed it)
	movei	#$00010040,r3
	store	r3,(r0)
	store	r1,(r2)			; launch 2 — INTO the running blit
	.rept	800
	nop
	.endr
.bwfib:
	load	(r2),r1
	btst	#0,r1
	jr	eq,.bwfib
	nop
	PROBE_EPI
	.68000
	.data
_p_fib_e:

; ── p_divext: DIV interleaved with consumed staging loads (geotex per-face) ──
; NODIV ladder slice: silicon saves 5.8ms removing divides, jsim saves 0.
; divhot/divsh (isolated) match — the in-kernel shape is div + staging
; loads consumed inside the shadow. Unit: div, 2 consumed DRAM loads,
; then consume the quotient.
	.even
	.globl	_p_divext_s
	.globl	_p_divext_e
_p_divext_s:
	.gpu
	PROBE_PRO
	move	r19,r10
	movei	#$10001,r5
	movei	#3,r6
	.rept	64
	div	r6,r5
	load	(r10),r1
	or	r1,r1
	addqt	#8,r10
	load	(r10),r2
	or	r2,r2
	addqt	#8,r10
	or	r5,r5			; consume quotient (in/after shadow)
	movei	#$10001,r5		; re-seed dividend
	.endr
	PROBE_EPI
	.68000
	.data
_p_divext_e:


; ── p_divoff: divext with DIV_OFFSET (16.16) — the NODIV-slice hypothesis ───
; geotex's perspective divides run in 16.16 mode; every prior div probe was
; integer mode and jsim prices both at 18 cycles. If 16.16 is slower on
; silicon, this reads above divext's 4.76 and the 5.8ms slice is named.
	.even
	.globl	_p_divoff_s
	.globl	_p_divoff_e
_p_divoff_s:
	.gpu
	PROBE_PRO
	movei	#$F0211C,r7		; G_DIVCTRL: DIV_OFFSET=1 (16.16 mode)
	moveq	#1,r8
	store	r8,(r7)
	move	r19,r10
	movei	#$10001,r5
	movei	#3,r6
	.rept	64
	div	r6,r5
	load	(r10),r1
	or	r1,r1
	addqt	#8,r10
	load	(r10),r2
	or	r2,r2
	addqt	#8,r10
	or	r5,r5
	movei	#$10001,r5
	.endr
	moveq	#0,r8			; restore integer mode for later probes
	store	r8,(r7)
	PROBE_EPI
	.68000
	.data
_p_divoff_e:


; ── p_divlat: DIV quotient READABLE-LATENCY by correctness (round 6.2 ask) ──
; The keystone the poison tool and the div calibration both need. 255/3=0x55
; (low 8 bits computed LAST, MSB-first divider). For K=0..15 instructions
; after the div, capture r5 as-read into result[K]. Host reads: smallest K
; with value 0x55 = true readable latency. jsim serves 0x55 at ALL K (its
; dest value is correct-with-stall) — so this probe DOUBLES as the silicon-
; vs-jsim correctness demonstrator. Writes 16 longs then magic at [64].
	; two operands per K: small (0xFF/3=0x55, settles early) then LARGE
	; (0x7FFFFFF0/3=0x2AAAAAA5, MSB-first significant bits computed LAST —
	; the actual garbage-window case round 5/6 point at).
	.macro	DIVLAT k
	movei	#255,r5
	movei	#3,r6
	div	r6,r5
	.rept	\k
	nop
	.endr
	move	r5,r0
	store	r0,(r18)		; result[K*8]     = small quotient as-read
	addqt	#4,r18
	movei	#$7FFFFFF0,r5
	movei	#3,r6
	div	r6,r5
	.rept	\k
	nop
	.endr
	move	r5,r0
	store	r0,(r18)		; result[K*8+4]   = LARGE quotient as-read
	addqt	#4,r18
	.endm

	.even
	.globl	_p_divlat_s
	.globl	_p_divlat_e
_p_divlat_s:
	.gpu
	movei	#PRMRESULT,r16
	load	(r16),r18		; result block base
	DIVLAT	0
	DIVLAT	1
	DIVLAT	2
	DIVLAT	3
	DIVLAT	4
	DIVLAT	5
	DIVLAT	6
	DIVLAT	7
	DIVLAT	8
	DIVLAT	9
	DIVLAT	10
	DIVLAT	11
	DIVLAT	12
	DIVLAT	13
	DIVLAT	14
	DIVLAT	15
	movei	#MAGICD,r26
	store	r26,(r18)		; magic at result+128, written LAST
	movei	#GCTRL,r27
	moveq	#0,r28
	store	r28,(r27)		; stop self
	nop
	nop
	.68000
	.data
_p_divlat_e:


; ── p_ldjump: load consumed across a TAKEN JUMP — scoreboard held or dropped? ──
; OpenLara round 5.2: a load in flight, then a taken jump, then the consume at
; the target. jsim (Silicon) scoreboards -> stalls -> correct value. Hardware
; reportedly DROPS the scoreboard across the basic-block boundary -> the consume
; reads stale/garbage. Tests both an internal SRAM load (their claim) and a slow
; DRAM load (unambiguous: ~15cyc latency vs ~5cyc jump overhead, still in flight
; at the target). Seeds known truths first, settles them, then load+jump+consume.
; result[0] = DRAM readback (truth 0xABCD1234), result[4] = SRAM readback
; (truth 0x5678DEF0), magic at [8]. Correct == truth; garbage != truth.
	.even
	.globl	_p_ldjump_s
	.globl	_p_ldjump_e
_p_ldjump_s:
	.gpu
	movei	#PRMRESULT,r16
	load	(r16),r18		; result base
	; seed a DRAM cell and an SRAM cell with known truths, let them settle
	movei	#$00160000,r12		; DRAM scratch
	movei	#$ABCD1234,r13
	store	r13,(r12)
	movei	#$F03E00,r14		; GPU SRAM scratch (below the param block)
	movei	#$5678DEF0,r7
	store	r7,(r14)
	.rept	20
	nop				; settle both stores
	.endr
	; --- DRAM load across a taken jump ---
	load	(r12),r1		; DRAM load into r1 (in flight ~15cyc)
	jr	t,.tgtD			; PC-relative taken jump (position-independent)
	nop				; delay slot
.tgtD:
	move	r1,r0			; consume at target — load still in flight
	store	r0,(r18)		; result[0] = DRAM readback
	addqt	#4,r18
	.rept	20
	nop
	.endr
	; --- SRAM load across a taken jump ---
	load	(r14),r2		; SRAM load into r2
	jr	t,.tgtS			; PC-relative taken jump
	nop				; delay slot
.tgtS:
	move	r2,r0			; consume at target
	store	r0,(r18)		; result[4] = SRAM readback
	addqt	#4,r18
	movei	#MAGICD,r26
	store	r26,(r18)		; magic at result+8
	movei	#GCTRL,r27
	moveq	#0,r28
	store	r28,(r27)		; stop self
	nop
	nop
	.68000
	.data
_p_ldjump_e:

; ── p_bcmdidle / p_bcmdbusy: what does a bwait POLL actually cost? ──────────
; The +30% geometry-build optimism suspect (bench 2026-07-21): a bwait spin
; is a stream of B_CMD reads — a Tom REGISTER read from the GPU, a shape no
; probe ever priced. Idle variant = the register-read baseline; busy variant
; = the same polls while a 2048-px blit owns the bus. The shaded builds add
; thousands of these spins per frame; jsim prices the shade pass ~free while
; silicon pays 23%.

	.even
	.globl	_p_bcmdidle_s
	.globl	_p_bcmdidle_e
_p_bcmdidle_s:
	.gpu
	PROBE_PRO
	movei	#BB_BCMD,r0
	.rept	256
	load	(r0),r1
	.endr
	PROBE_EPI
	.68000
	.data
_p_bcmdidle_e:

	.even
	.globl	_p_bcmdbusy_s
	.globl	_p_bcmdbusy_e
_p_bcmdbusy_s:
	.gpu
	PROBE_PRO
	BLITSETUP
	movei	#BB_BCOUNT,r0
	movei	#$00010800,r1		; 1 row x 2048 px — long bus-holder
	store	r1,(r0)
	movei	#BB_BCMD,r2
	movei	#BB_CMDTEX,r1
	store	r1,(r2)			; launch, then poll THROUGH the blit
	.rept	256
	load	(r2),r1
	.endr
.bwbc:
	load	(r2),r1			; drain so the next rep starts clean
	btst	#0,r1
	jr	eq,.bwbc
	nop
	PROBE_EPI
	.68000
	.data
_p_bcmdbusy_e:

; ── p_ldunderb: 256 DRAM loads WHILE a long blit holds the bus ───────────────
; The staging-under-blit contention term: jsim currently lets GPU external
; loads proceed as if the Blitter weren't on the DRAM bus (contention ~0.1%
; on the rect-shade run). Silicon-minus-(p_lddram + long-blit-alone) is the
; coefficient, measured directly. Launch first, do NOT bwait, load through
; the blit, then drain. (COBWEB_BUG_blitter_overcharged round 2, the
; under-charge side.)
	.even
	.globl	_p_ldunderb_s
	.globl	_p_ldunderb_e
_p_ldunderb_s:
	.gpu
	PROBE_PRO
	BLITSETUP
	movei	#BB_BCOUNT,r0
	movei	#$00010800,r1		; 1 row x 2048 px — a long bus-holder
	store	r1,(r0)
	movei	#BB_BCMD,r2
	movei	#BB_CMDTEX,r1
	store	r1,(r2)			; launch, no bwait — loads fight the blit
	move	r19,r0			; DRAM buffer base (PRMAUX)
	.rept	256
	load	(r0),r1
	addqt	#8,r0			; phrase stride, page-friendly like lddram
	.endr
.bwub:
	load	(r2),r1			; drain so the next rep starts clean
	btst	#0,r1
	jr	eq,.bwub
	nop
	PROBE_EPI
	.68000
	.data
_p_ldunderb_e:

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
