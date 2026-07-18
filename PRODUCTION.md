# Road to production — dogfooding Cobweb against real projects

Cobweb is the **successor** to the rmac/rln + vbcc toolchain: production-grade,
standalone, and 100% open source. "Production" here has one honest test — can
it build real Jaguar projects? So we measure it against a real corpus (77
hand-written JRISC source files across shipping-grade homebrew) and let the
failures drive the backlog. No guessing.

## Current state (measured, 2026-07-17)

**jas assembles 36 / 77 real JRISC files** at the encoder level (no linker
yet), up from 0 before register aliases and the directive set landed. Two
representative files (`gpu_idle`, `gpu_tick`) verified byte-emitting and clean.

The remaining 46 files fail for a small number of *categorical* reasons —
these are the production backlog, in priority order:

| Blocker | ~count | What it needs |
|---|---|---|
| ~~Undefined symbols / cross-module `.extern`~~ | ~~done~~ | preprocessor + **jln linker SHIPPED** (jas `-c` objects, reloc resolution) ✓ |
| ~~`.if`/`.rept`/`.macro`/`.include`~~ | ~~done~~ | **preprocessor SHIPPED** (front pass) ✓ |
| 68k mnemonics (`move.l`, `movem`, `lsl`, `ori`, …) in mixed 68k/JRISC files | ~200 | a **68k assembler mode** (or section split) |
| Unknown condition codes | ~120 | audit the corpus's `jump`/`jr` condition spellings vs jas |
| `jr` displacement out of range | ~76 | mostly real far-branches (need movei+jump); verify a few |

## The production plan (build order)

1. **Preprocessor** (`jas` front pass): `.include`, `.macro`/`.endm`,
   `.rept`/`.endr`, `.if`/`.else`/`.endif`. Expands to a flat stream the
   existing two-pass assembler already handles. Unblocks the include-header
   undefined-symbols and the conditional/repeat/macro directives — the single
   biggest category.
2. ~~**jln — the linker**~~ **SHIPPED**: jas emits `.jo` relocatable objects
   (`-c`), jln resolves cross-object symbols + movei/long relocations into a
   loadable image. SRAM overlay groups (first-class) are the next jln step.
3. **68k assembler mode** in jas (or a sibling `jas68k`): the mixed files
   interleave a 68k host section (`.68000`) with GPU/DSP code. Real projects
   need both assembled and linked together. (jcc already needs a 68k backend
   for its boot shim — shared work.)
4. **Condition-code + range audit**: reconcile jas's `jump`/`jr` condition set
   and displacement math with the corpus.

## Migration order (eat our own dog food)

Migrate projects simplest-first, so each surfaces the next gap:
1. Single-file, self-contained JRISC kernels (many of the 31 already pass).
2. Projects with `.include`d equate headers (after the preprocessor).
3. Multi-object projects with `.extern` (after jln).
4. Mixed 68k/JRISC full builds (after the 68k mode) — full replacement of
   rmac/rln in the build.

Each migrated project is a regression test: once it builds with Cobweb, it
stays building (CI assembles the corpus on every change).

## Why measure this way

The whole thesis of the suite is that the Jaguar was limited by iteration
cost, not silicon — so the toolchain's job is to be *trustworthy*, and the
only proof of trustworthy is "it builds the real thing." A percentage against
a real corpus, moved by real commits, is the honest scoreboard. It only goes
up when working code makes it go up.
