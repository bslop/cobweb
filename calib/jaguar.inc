; jaguar.inc — Tom/Jerry hardware register equates

; Memory controller
MEMCON1		equ	$F00000
MEMCON2		equ	$F00002

; Tom video registers
VMODE		equ	$F00028
HP		equ	$F00002
HBB		equ	$F00004
HBE		equ	$F00006
VS		equ	$F00008
VBB		equ	$F0000A
VBE		equ	$F0000C
VI		equ	$F0000E

; Object Processor
OBF		equ	$F00010
OLP		equ	$F00020

; Blitter
A1_BASE		equ	$F02100
A1_FLAGS	equ	$F02104
A1_CLIP		equ	$F02108
A1_PIXEL	equ	$F02110
A1_STEP		equ	$F02118
A2_BASE		equ	$F02200
A2_FLAGS	equ	$F02204
A2_PIXEL	equ	$F02210
B_CMD		equ	$F02238
B_COUNT		equ	$F02228

; Interrupt controller
INT1		equ	$F000E0
INT2		equ	$F000E2

; Tom GPU (RISC processor) control — G_PC overlaps A1_PIXEL; G_CTRL overlaps A1_STEP upper half
G_PC		equ	$F02110		; GPU program counter (write start address before GPUGO)
G_CTRL		equ	$F02114		; GPU control: bit 0 = GPUGO (write 1 to start, GPU clears when done)
GPU_RAM		equ	$F03000		; GPU local RAM, 4 KB, zero wait-states for GPU instruction fetch

; Joystick
JOYSTICK	equ	$F14000
JOYBUTS		equ	$F14002

; NTSC video constants (320×240 CRY16)
VMODE_VAL	equ	$6C1

; Framebuffer base
FB_BASE		equ	$1D0000
FB_WIDTH	equ	320
FB_HEIGHT	equ	240
