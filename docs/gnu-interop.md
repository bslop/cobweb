# GNU toolchain interop: migrating a gcc/ld project one file at a time

Most shipped Jaguar homebrew links with GNU `ld` — a linker script
(`jaguar.ld`) with memory regions, gcc-built C objects, hand-written `.S`
files, and `.incbin` data objects. `jas --elf-obj` exists so such a project
can adopt `jcc68k`/`jas` **per translation unit**, keeping its GNU link
untouched. No flag-day port to `jln`.

## The flow

Replace one C file's compile rule; everything else stays gcc:

```sh
jcc68k unit.c -I. -DYOUR_FLAGS -o unit.s     # C → 68000 assembly (readable)
jas unit.s --68000 --elf-obj -o unit.o       # → ELF32 m68k relocatable object
m68k-elf-ld -T jaguar.ld ... unit.o ...      # links exactly like a gcc object
```

`--elf-obj` implies `-r`: every absolute reference to a defined symbol is
emitted as a relocation, so the GNU linker is free to place the sections
wherever the script says. Verify a migrated unit any time with the usual
oracle: build both ways, diff the linked image.

## The runtime, without libgcc

`jcc68k` lowers 32-bit multiply/divide/modulo (the 68000 only has 16×16 and
32÷16 hardware forms) and the 16.16 `fix` helpers to calls on `__mulsi3`,
`__udivsi3`, `__umodsi3`, `__divsi3`, `__modsi3`, `__mulfix`, `__divfix`.
When linking with `-nostdlib` (no libgcc), build them once as an object:

```sh
jcc68k --runtime -o jrt68k.s
jas jrt68k.s --68000 --elf-obj -o jrt68k.o   # add to your OBJS
```

(All helpers take operands in D0/D1, return in D0, and preserve d2–d7/a2–a5.)

## What lands in which section

`jas` maps `.text`/`.data`/`.bss` (and GAS `.section` spellings of the same)
onto real ELF sections; symbols are section-relative, relocations are RELA
(`R_68K_32`, `R_68K_16` for abs.w, `R_68K_PC16` for word branches to
externs). Two v1 constraints, both diagnosed with a clear error rather than
silent corruption:

- **Each section must be contiguous** within one source file — emit all of
  `.text`, then all of `.data`. (Re-entering a section would change
  intra-section distances the assembler already resolved.) `jcc68k` output
  always satisfies this.
- **JRISC `movei #extern` has no ELF relocation type** (the immediate is a
  swapped half-word pair no standard m68k reloc describes). Keep GPU/DSP
  code as assembled blobs (`.incbin` / `dsp_blob.S`, as the ports already
  do), or link the RISC side with `jln`.

## Symbol naming

`jcc68k` emits C symbols unprefixed (`main`, not `_main`), matching the
m68k-elf/GCC convention the Jaguar ports use — a C `gpu_kernel` reference
resolves against the asm label `gpu_kernel` at link time with no glue.
