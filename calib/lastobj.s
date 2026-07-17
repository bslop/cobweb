; lastobj.s — BSS/text end sentinels for rln linker

	.globl	__bss_end
	.globl	__text_end
	.globl	__bss_start

	.bss
__bss_start:
	.long	0
__bss_end:
	.long	0

	.text
__text_end:
