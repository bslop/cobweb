# Retail compatibility report

**Generated 2026-07-17** by the full-corpus sweep (`make compat`). This
report exists so the project never claims more accuracy than it can show.
Regenerate it yourself: every number below comes from two commands anyone
can run against their own ROM dumps.

## The three accuracy tiers, honestly

1. **JRISC timing** — **hardware-calibrated.** 32/32 probes match a real
   console (mean error 0.059 cyc/instr). See `calib/`.
2. **Homebrew functional compatibility** — **good.** The modern-homebrew
   corpus the emulator grew up on boots and renders (GPU-rasterized 3D,
   sprites, text; see `sim/README.md` for the verified list).
3. **Retail functional compatibility** — **not there yet, measured.**
   Of 68 retail cart images: 67 boot to a running machine, 47 execute with
   zero illegal opcodes, 58 drive the GPU — **but none draw a recognizable
   scene** (12 reach a solid cleared screen; the rest stay black), at both
   6.7s and 25s of emulated time. The machines run; the retail display
   path does not. That is the next campaign, and this table is its
   regression baseline.

## Why (known gaps, already spec'd in `sim/docs/spec/`)

- **Boot-ROM state**: retail titles inherit video/CLUT/memory state from
  the real boot ROM that the HLE boot does not yet replicate.
- **Object Processor**: scaled-bitmap objects and other OP features the
  homebrew corpus never exercised.
- **CRY16**: exact color table has open items (also blocks two homebrew
  titles).
- **8bpp CLUT path**: at least one runtime (vbcc's) programs the CLUT in a
  way our OP renders black — found during compiler benchmarking.
- **CD titles (21 images)**: out of scope entirely — no CD hardware yet.

## Method

```sh
make compat            # sweeps every image in $JAGUAR_ROMS, writes the table
```

Per title: boot 400 frames (retry black screens at 1500), record load
format, 68k illegal-opcode count, GPU/DSP instruction counts, and analyze
the true OP scan-out screenshot (non-black %, distinct colors). "Scene"
requires >2% non-black AND more than one color — a cleared screen doesn't
count as rendering.

## Per-title results (68 cart images)

| Title | Execution | GPU | DSP | Display |
|---|---|---|---|---|
| Aircars_(1995).jag | runs clean | yes | yes | black |
| Alien vs Predator (1994).jag | runs clean | yes | yes | black |
| Alien vs Predator (Alpha).rom | runs clean | yes | yes | black |
| ARENA_Football_'95_(1995).jag | runs (356275 illegal) | yes | yes | cleared screen |
| Atari Karts (1995).jag | runs clean | yes | yes | black |
| Attack of the Mutant Penguins (1996).jag | runs clean | yes | yes | black |
| Battle Sphere Gold (World).j64 | runs clean | yes | yes | black |
| Brett_Hull_Hockey_(1995).jag | runs clean | yes | no | black |
| Brutal Sports Football (1994) (Telegames).jag | runs (25 illegal) | no | no | black |
| Bubsy - Fractured Furry Tails (1994).jag | runs (14 illegal) | yes | no | black |
| Cannon Fodder (1995) (Computer West).jag | runs clean | yes | no | black |
| Checkered Flag (1994).jag | runs clean | yes | yes | black |
| Club Drive (1994).jag | runs clean | yes | no | black |
| Cybermorph (1993).jag | runs clean | yes | yes | black |
| Defender 2000 (1996).jag | runs clean | yes | yes | black |
| Doom - Evil Unleashed (1994).jag | runs (50 illegal) | yes | yes | black |
| Double Dragon V (1995) (Williams).jag | runs clean | yes | yes | black |
| Dragon - The Bruce Lee Story (1994).jag | runs clean | yes | yes | cleared screen |
| Evolution - Dino Dudes (1993).jag | runs clean | yes | no | black |
| Fever Pitch Soccer (1995).jag | runs (347664 illegal) | yes | yes | cleared screen |
| Fight For Your Life (1996) [a1].jag | runs (1974202 illegal) | yes | no | cleared screen |
| Fight For Your Life (1996) [a2].jag | runs (33329 illegal) | no | no | black |
| Fight For Your Life (1996).jag | runs (1974202 illegal) | yes | no | cleared screen |
| Flashback (1995) (U.S. Gold).jag | runs clean | yes | yes | black |
| Flip Out (1995).jag | runs (3145732 illegal) | yes | no | black |
| Hover Strike (1995).jag | runs (851789 illegal) | yes | yes | cleared screen |
| Hyper Force (World).j64 | runs clean | no | yes | black |
| I-War (1995).jag | runs (1 illegal) | yes | no | black |
| International Sensible Soccer (1995).jag | runs clean | yes | yes | black |
| Iron Soldier (1994) [a1].jag | runs (298822 illegal) | yes | yes | cleared screen |
| Iron Soldier (1994).jag | runs (298822 illegal) | yes | yes | cleared screen |
| Iron Soldier 2 (World).j64 | LOAD FAIL | — | — | none |
| Kasumi Ninja (1994) [a1].jag | runs (9 illegal) | no | no | black |
| Kasumi Ninja (1994).jag | runs clean | yes | yes | black |
| Missile Command 3D (1995).jag | runs clean | yes | no | black |
| Music Demo (2002) (ScatoLOGIC).jag | runs clean | no | no | black |
| Native Demo (bin) (1997).jag | runs clean | yes | no | black |
| Native Demo (jag) (1997).jag | runs (128 illegal) | yes | no | black |
| NBA Jam TE (1996).jag | runs clean | yes | yes | black |
| Pinball Fantasies (1995) (Computer West).jag | runs clean | yes | yes | cleared screen |
| Pitfall - The Mayan Adventure (1995).jag | runs (794 illegal) | yes | yes | black |
| Power Drive Rally (1995) (TWI).jag | runs (3 illegal) | yes | yes | black |
| Protector - Special Edition (World).j64 | runs clean | yes | yes | black |
| Raiden (1994).jag | runs clean | yes | yes | black |
| Rayman (1995) (UBI Soft).jag | runs clean | yes | yes | black |
| Rayman Demo (1995) (UBI Soft).rom | runs (1518105 illegal) | no | no | black |
| Ruiner Pinball (1995).jag | runs clean | yes | yes | black |
| Skyhammer_(1999).jag | runs (10640 illegal) | yes | yes | black |
| Soccer Kid (World).j64 | runs clean | no | yes | black |
| Super Burnout (1995).jag | runs clean | yes | yes | black |
| Super Cross 3D (1995) [a1].rom | runs clean | yes | yes | cleared screen |
| Super Cross 3D (1995).jag | runs clean | yes | yes | cleared screen |
| Syndicate (1995) (Ocean).jag | runs clean | yes | no | black |
| Tempest 2000 (1994).jag | runs clean | yes | yes | black |
| Theme Park (1995) (Ocean).jag | runs clean | yes | no | black |
| Total Carnage (World).j64 | runs clean | yes | yes | black |
| Towers_II_(1996).jag | runs clean | yes | yes | black |
| Trevor McFur in the Crescent Galaxy (1993).jag | runs (538731 illegal) | yes | no | black |
| Troy Aikman NFL Football (1995) (Williams).jag | runs clean | yes | no | black |
| Ultra Vortek (1995).jag | runs clean | yes | no | black |
| Ultra Vortek (Beta) (1995).rom | runs clean | yes | no | black |
| Val D'Isere Skiing & Snowboarding (1994).jag | runs clean | yes | yes | black |
| White Men Can't Jump (1995).jag | runs (5998102 illegal) | yes | no | black |
| Wolfenstein 3D (1994).jag | runs clean | yes | yes | black |
| Worms (World).j64 | runs (3642484 illegal) | yes | no | cleared screen |
| Zero 5 (World).j64 | runs clean | yes | yes | black |
| Zool 2 (1994).jag | runs clean | yes | no | cleared screen |
| Zoop! (1996).jag | runs clean | yes | yes | black |

## Reading this table

"Runs clean" means the 68k executed hundreds of thousands to millions of
instructions with zero illegal opcodes — the CPU core, loaders, and memory
map hold up across nearly the whole library. Illegal-opcode counts point at
missing 68k/CPU-adjacent behavior; "black" with an active GPU points at the
display path. Both failure classes are exactly what the differential oracle
(vs BigPEmu, `sim/docs/spec/ACCURACY_ORACLE.md`) is designed to bisect.
