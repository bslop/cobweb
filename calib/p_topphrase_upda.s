; p_topphrase_upda.s — two rig questions from COBWEB_REQ_rectshade_and_calibration
;
;   Q1 (§5.2): is the top phrase of GPU SRAM ($F03FF8-$F03FFF) writable and
;              stable on silicon? Nothing in the corpus ever proved it.
;   Q2 (§3):   under the DSTA2 role swap, which outer-loop update bit steps
;              the DESTINATION — UPDA2 (the A2 register set, as jsim models)
;              or UPDA1 (the "dest side" of the pipeline)?
;
; Self-contained 68k program, org $4000 (GameDrive load). No video, no
; console: verdicts land in DRAM for a skunk/JagGD memory read — or
; `jagemu peek --at 0x100 --len 16` when dogfooding in the simulator.
;
;   $100: Q1 verdict  $600D0001 = top phrase held both sentinel longs
;                     $BAD00001 = readback mismatched (top phrase NOT stable)
;   $104: Q1 readback of $F03FF8 (for the log)
;   $108: Q2 verdict  $600D0002 = row 1 landed re-homed one line below
;                                 (UPDA2 stepped the swapped dest = jsim)
;                     $BAD00002 = row 1 walked linearly past row 0
;                                 (dest never re-homed — the DRAM-corruption
;                                  scenario; jagemu would be wrong)
;                     $BAD0FFFF = neither pattern (inspect $110000 by hand)
;   $10C: magic $C0DED0NE when the probe is finished
;
; Assemble:  jas p_topphrase_upda.s --68000 --org 0x4000 -o p_topphrase_upda.bin
; (wrap to .cof with the usual makecof flow for the rig)

	.68000
	.org	$4000

BLIT_A1BASE	equ	$F02200
BLIT_A1FLAGS	equ	$F02204
BLIT_A1PIXEL	equ	$F0220C
BLIT_A1STEP	equ	$F02210
; NB: the A2 file has A2_MASK between FLAGS and PIXEL — its layout is NOT
; parallel to A1 (classic trap; caught by jsim's blit trace while dogfooding
; this very probe).
BLIT_A2BASE	equ	$F02224
BLIT_A2FLAGS	equ	$F02228
BLIT_A2MASK	equ	$F0222C
BLIT_A2PIXEL	equ	$F02230
BLIT_A2STEP	equ	$F02234
BLIT_BCMD	equ	$F02238
BLIT_BCOUNT	equ	$F0223C
BLIT_BSRCD	equ	$F02240		; 64-bit source data: TWO longs at $F02240/44
G_CTRL		equ	$F02114

; A2 flags: PITCH1 | PIXEL8 | WID320 | XADDPIX  (8bpp linear, 320 wide)
PIX8W320	equ	$00014218

start:
	movea.l	#$001F0000,a7

; ── Q1: top-phrase sentinel ──────────────────────────────────────────────────
; Write two sentinel longs, do unrelated bus work, read back. On the rig the
; interesting failure is a value that DECAYS or aliases; run the probe twice
; some seconds apart if the first pass is clean.
	move.l	#$C0DEF00D,$F03FF8
	move.l	#$5A5AA5A5,$F03FFC
	move.w	#$1000,d0		; unrelated bus traffic + settle delay
.settle:
	move.l	$4000,d1
	dbra	d0,.settle
	move.l	$F03FF8,d2
	move.l	$F03FFC,d3
	move.l	d2,$104			; raw readback for the log
	move.l	#$600D0001,d4
	cmp.l	#$C0DEF00D,d2
	bne.w	.q1bad
	cmp.l	#$5A5AA5A5,d3
	bne.w	.q1bad
	bra.w	.q1done
.q1bad:
	move.l	#$BAD00001,d4
.q1done:
	move.l	d4,$100

; ── Q2: DSTA2 + UPDA2-only, 2 rows x 4 px ────────────────────────────────────
; Canvas at $110000 (8bpp, 320-wide addressing), prefilled $EE for 8 lines.
; A2 = destination (via DSTA2), A1 = fill source side. B_SRCD pattern $77,
; A2_STEP = $20000-4 (down 1 line, X re-home over a 4px inner loop),
; B_COUNT = 2 rows x 4 px, command = fill | DSTA2 | UPDA2 (NO UPDA1).
;
; If UPDA2 steps the swapped destination (jsim semantics):
;   row 0: $110000..3 = $77, row 1: $110140..3 = $77 (320 = $140)
; If the dest is never re-homed and walks linearly:
;   $110004..7 = $77 instead (second row continues on line 0)
	lea	$110000,a0
	move.w	#(320*8/4)-1,d0		; clear 8 lines of 320, longs
	move.l	#$EEEEEEEE,d1
.clr:
	move.l	d1,(a0)+
	dbra	d0,.clr

	move.l	#$110000,BLIT_A2BASE
	move.l	#PIX8W320,BLIT_A2FLAGS
	move.l	#0,BLIT_A2PIXEL		; y=0, x=0
	; A2_STEP is (y<<16)|x as signed words: +1 row, x -= 4 (re-home)
	move.l	#$0001FFFC,BLIT_A2STEP
	move.l	#$110000+(320*16),BLIT_A1BASE	; parked well away
	move.l	#PIX8W320,BLIT_A1FLAGS
	move.l	#0,BLIT_A1PIXEL
	move.l	#$77777777,BLIT_BSRCD	; both halves of the 64-bit source data
	move.l	#$77777777,BLIT_BSRCD+4
	move.l	#(2<<16)|4,BLIT_BCOUNT
	; LFU = S (replace with B_SRCD pattern), DSTA2 role swap, UPDA2 only —
	; the question under test. Bits verbatim from JAGUAR.INC:
	; LFU_A|LFU_AN = $01800000, DSTA2 = $800, UPDA2 = $400.
	move.l	#$01800C00,BLIT_BCMD

	move.w	#$2000,d0		; let the blit drain
.bwait:
	dbra	d0,.bwait

	moveq	#0,d4
	move.b	$110000,d4		; row 0 px 0
	cmp.b	#$77,d4
	bne.w	.q2odd
	move.b	$110000+320,d4		; row 1 px 0 (re-homed)
	cmp.b	#$77,d4
	beq.w	.q2rehome
	move.b	$110004,d4		; linear continuation?
	cmp.b	#$77,d4
	beq.w	.q2linear
.q2odd:
	move.l	#$BAD0FFFF,$108
	bra.w	.done
.q2rehome:
	move.l	#$600D0002,$108
	bra.w	.done
.q2linear:
	move.l	#$BAD00002,$108
.done:
	move.l	#$C0DED04E,$10C
halt:
	bra.w	halt
