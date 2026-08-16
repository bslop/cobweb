# Cobweb — public release plan

**Mission:** open-source the suite Atari should have shipped in 1993, for
everyone. Hasbro released the Jaguar platform into the public domain on
May 14, 1999; Cobweb's job is to finish that gesture — a fully open, fully
documented, hardware-honest toolchain with nothing held back and nothing
left undocumented.

## License

Recommendation: **MIT** (single, simple, maximally adoptable — matches
"everyone in the world"). Alternatives worth one decision: Apache-2.0 (adds
an explicit patent grant — largely moot here since Hasbro released the
platform patents) or MIT OR Apache-2.0 dual (Rust-ecosystem convention; the
emulator is Rust). GPL would conflict with the goal of homebrew devs
shipping commercial carts freely. **Decision pending — trivial to apply
before first publication; blocked on nothing else.**

## Provenance audit (what's ours to license)

| Component | Origin | Status |
|---|---|---|
| `sim/` (emulator + jsim truth layer) | authored in-project | ours — license freely |
| `calib/` probes, harness, parser, docs | authored in-project | ours |
| `calib/skunk.s` | tursilion (harmlesslion.com) | header: "licensed freely and may be used for any purpose, commercial or..." — redistributable with attribution kept |
| `calib/skunkglue.s` | user's own corpus (a reference sandbox project) | ours |
| `calib/jaguar.inc`, `lastobj.s`, `bootstub.s` | register facts / user's corpus patterns | ours (platform is public domain) |
| `RESEARCH.md` | authored in-project | ours |
| Build toolchain (rmac/rln/gcc via cubanismo/jaguar-sdk Docker) | external **build dependency**, not distributed | document as prerequisite; long-term replaced by jas |
| `sim/crates/jrom/src/univ.bin` | SubQMod's signed 8 KB universal header | ☠️ **third-party binary, REDISTRIBUTED** - `include_bytes!` puts it in the repo and in every `.j64` jrom emits. No formal licence exists for it; it is carried by most of the Jaguar homebrew toolchain. Called out in LICENSE and excluded from the MIT grant. |

☠️ **This table previously said "nothing in the tree carries a restrictive
license" while listing only `skunk.s` - it had missed `univ.bin` entirely.**
An audit that enumerates the files it already knows about is not an audit.
Nothing else in the tree is third-party: `Cargo.lock` resolves to 14 packages
and all 14 are this workspace's own crates - there are no external
dependencies at all.

## Authorship & attribution

This project is **AI-written, human-directed**: Claude (Anthropic) writes
the code and documentation; an anonymous human maintainer directs the work,
makes design decisions, operates the hardware rig, and reviews what ships.
This is stated openly in the README and in any announcement — no claim of
human authorship is made for work the AI did, and the maintainer claims no
personal credit.
The verification story is deliberately authorship-independent: calibration
ROMs and bench logs let anyone check every claim on their own console.

## Release checklist

1. **Version control**: `git init` + initial commit (the tree is not yet a
   repo). History from here on is public history — write commit messages
   for strangers.
2. **License files**: LICENSE at root once chosen; keep tursilion's header
   intact in `calib/skunk.s`.
3. **De-localize the docs**: README currently references the internal porting notes
   and the seed emulator (private sibling repos). Vendor the load-bearing
   knowledge (the hazard rulebook, the wishlist requirements) into
   `docs/` in-tree, with provenance tags kept, so the public repo stands
   alone.
4. **The developer documentation set** (core tenet: painfully easy, fully
   fleshed out, nothing untouched):
   - `docs/quickstart.md` — zero to running a ROM in jagemu in one command;
     zero to first GPU program in five minutes.
   - `docs/jagemu.md` — every command and flag, with copy-paste examples.
   - `docs/fidelity.md` — the timing model: every constant, its hardware
     provenance (which probe, which bench log), and the known-unmodeled list.
   - `docs/jrisc-handbook.md` — the ISA + hazard rulebook with [HW]/[TRM]
     provenance tags: the errata as rules, the delay-slot discipline, the
     GPU-in-main alignment law, the measured contention numbers.
   - `docs/calibration.md` — how to bench your own console (the whole point:
     anyone with a Skunkboard can verify every number we publish).
   - `docs/architecture.md` — crate map, borrow model, determinism contract.
5. **CI**: cargo test + the calib_sim suite under jsim as a regression gate
   (deterministic, no hardware needed) — green badge from day one.
6. **Benchmarks before claims**: publish the compiled-vs-hand-asm shootout
   (vbcc jrisc preview samples, Brainstorm GCC 2.6.3, corpus hand kernels —
   all measured in calibrated jsim + spot-checked on the bench) as
   `docs/benchmarks.md`. First-ever published numbers of this kind; they
   are also the bar Cobweb's own jcc must clear publicly.
7. **Naming/hosting**: repo name, org, and whether the seed emulator's git
   history gets grafted in (it exists in the private sibling repo) — user
   decisions at publish time.

## What "the suite Atari should have shipped" means, concretely

Atari's 1995 FAQ promised a compiler that transparently overlays code past
the 4KB SRAM limit. It never shipped. The 1995 dev kit had no cycle-honest
simulator, no hazard-checking assembler, and documentation whose errata
list ended with "none of these work-rounds are very satisfactory."
Cobweb ships, in order: the simulator with silicon-calibrated truth (done),
the assembler that refuses to assemble hazards (jas, next), verification as
a product (jtest), the superoptimizing scheduler (jopt), the overlay-managing
compiler (jcc), and debugging/profiling that don't require pointing a camera
at a TV (jdbg/jprof). Every timing claim traceable to a bench log anyone
can reproduce on their own console.
