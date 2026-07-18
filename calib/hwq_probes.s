; hwq_probes.s — Cobweb hardware-question probes (GPU).
;
; Two open questions from COBWEB_NEEDS that only real silicon can answer:
;
;  Q1 (headline): does the GPU scoreboard an external DRAM load whose ONLY
;      consumer sits ACROSS a taken jump — delivering the correct (stalled)
;      value — or is the scoreboard bypassed across the jump, reading STALE?
;      (BigPEmu reads stale, deterministically; jsim's Silicon model assumes
;      hardware stalls correctly; nobody has benched it.)
;
;  Q2: the VC (video counter, $F00006) wrap modulus. The timing-calib probe
;      masked VC with $7FF (11 bits); if the real counter is wider, that
;      truncated it. This reads VC UNMASKED to settle 524 vs 2571.
;
; Each probe writes 32-bit longs to a DRAM result block (pointer in r18) and
; stops itself. All values are chosen non-zero so 0 means "didn't run".

GCTRL		equ	$F02114
VCADDR		equ	$F00006
PARAM_RES	equ	$F03F84		; 68k puts the result pointer here
GOODVAL		equ	$600D600D
SENTVAL		equ	$BADBAD00
MAGICD		equ	$C0DED04E

	.data
	.even

; ── Q1a: load consumed ACROSS a taken jump ─────────────────────────────────
; result[0] = r1 read back across the jump (GOOD if scoreboarded, SENT if stale)

	.globl	_p_xjump_s
	.globl	_p_xjump_e
_p_xjump_s:
	.gpu
	movei	#PARAM_RES,r16
	load	(r16),r18		; r18 = result block pointer
	movei	#$00140000,r2		; r2 = DRAM addr holding GOODVAL (seeded by 68k)
	movei	#SENTVAL,r1		; r1 = known sentinel (the "stale" value)
	load	(r2),r1			; external DRAM load into r1 — in flight ~16 cyc
	movei	#xj_tgt,r3		; set up the jump target (flag-transparent)
	jump	(r3)			; TAKEN jump; r1 not read anywhere yet
	nop				; delay slot — does NOT touch r1
	movei	#0,r1			; (skipped by the jump — never executes)
xj_tgt:
	; FIRST read of r1 is here, across the jump, load still in flight:
	store	r1,(r18)		; [0] GOOD => scoreboard held; SENT => stale
	movei	#GOODVAL,r4
	addqt	#4,r18
	store	r4,(r18)		; [1] GOOD reference
	movei	#SENTVAL,r5
	addqt	#4,r18
	store	r5,(r18)		; [2] SENT reference
	movei	#MAGICD,r6
	addqt	#4,r18
	store	r6,(r18)		; [3] done magic (written LAST)
	movei	#GCTRL,r7
	moveq	#0,r8
	store	r8,(r7)			; stop self
	nop
	nop
	.68000
	.data
_p_xjump_e:

; ── Q1b: CONTROL — same load+consume STRAIGHT-LINE (no jump between) ────────
; hardware definitely scoreboards this; if control=GOOD but xjump=SENT, the
; jump is proven to be what breaks the scoreboard.

	.even
	.globl	_p_ctrl_s
	.globl	_p_ctrl_e
_p_ctrl_s:
	.gpu
	movei	#PARAM_RES,r16
	load	(r16),r18
	movei	#$00140000,r2
	movei	#SENTVAL,r1
	load	(r2),r1			; external load
	nop				; straight line — no jump
	nop
	store	r1,(r18)		; [0] should be GOOD (normal scoreboard)
	movei	#MAGICD,r6
	addqt	#4,r18
	addqt	#4,r18
	addqt	#4,r18
	store	r6,(r18)		; [3] done magic
	movei	#GCTRL,r7
	moveq	#0,r8
	store	r8,(r7)
	nop
	nop
	.68000
	.data
_p_ctrl_e:

; ── Q2: unmasked VC maximum (modulus - 1) ──────────────────────────────────

	.even
	.globl	_p_vcfull_s
	.globl	_p_vcfull_e
_p_vcfull_s:
	.gpu
	movei	#PARAM_RES,r16
	load	(r16),r18
	movei	#$00100000,r17		; loop count (spans many fields)
	movei	#VCADDR,r20
	moveq	#0,r25			; max seen (UNMASKED full 16-bit)
	movei	#vc_loop,r21
vc_loop:
	loadw	(r20),r24		; full 16-bit VC, NO mask
	or	r24,r24			; settle the load before comparing
	cmp	r24,r25			; r25 - r24: borrow(C) set => r24 is new max
	jr	cc,vc_nomax		; C clear (r25 >= r24) => not a new max
	nop
	move	r24,r25			; record new max
vc_nomax:
	subq	#1,r17
	jump	ne,(r21)		; loop
	nop
	store	r25,(r18)		; [0] max VC seen (modulus - 1)
	addqt	#4,r18
	addqt	#4,r18
	addqt	#4,r18
	movei	#MAGICD,r6
	store	r6,(r18)		; [3] done magic
	movei	#GCTRL,r7
	moveq	#0,r8
	store	r8,(r7)			; stop self
	nop
	nop
	.68000
	.data
_p_vcfull_e:
