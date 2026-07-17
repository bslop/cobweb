# the seed emulator

A from-scratch, **instrumentation-first** Atari Jaguar emulator in Rust, built to
replace BigPEmu in the Jaguar game-porting conveyor belt — native (no Wine), with
**true multi-instance** isolation, **real OP-composited screenshots**, a
first-class **debug API for Claude Code**, and **deterministic** execution for
regression-diffing against BigPEmu.

> **Why:** BigPEmu is accurate but, for an AI-driven porting pipeline, it fights
> you: one global lock serializes every Claude instance through a single Wine'd
> process; headless screenshots read *the DRAM the 68000 wrote, not the OP
> scan-out* (so they pass while the screen is wrong); there are no real
> breakpoints/watchpoints/trace; and it overwrites its own config on exit. This
> emulator fixes all of that.

## Built to give an AI eyes — not for humans

The primary user is **Claude**, driving the Jaguar port pipeline headless, in
parallel. So it is designed around what an AI needs to *see and debug* the
machine:

- **Live sessions you connect to.** `jagemu serve` runs a long-lived, isolated,
  headless instance; `jagemu ctl <id> <cmd>` drives it (run / step / inject
  input / inspect) with **state persisting between commands**. Multiple sessions
  run concurrently with **no global lock** — N Claude instances, N emulators
  (proven: two daemons driven independently at once).
- **Pull video as one image.** `video` renders a **filmstrip montage** (a grid
  of frames over time) so a *single* image read shows motion.
- **Pull pictures.** `frame` / `screenshot` emit the **true OP scan-out** PNG.
- **Deep debugging.** breakpoints, watchpoints, single-step, `peek`/`poke`,
  register + GPU/DSP state, disassembly — all over JSON.
- **Audio** capture: `audio` samples the I2S/DAC output → **WAV** + peak/RMS
  stats (so you can tell silence from sound without listening). DSP audio ISRs
  run via RISC interrupt handling.

```sh
jagemu serve --rom game.cof --instance dev &       # → instance id + control socket
jagemu ctl dev run 250                              # advance 250 frames (state persists)
jagemu ctl dev input a,up                           # hold buttons
jagemu ctl dev video shot.png --count 8 --every 4   # filmstrip of motion
jagemu ctl dev peek 0x3F00 --len 32                 # inspect memory
jagemu ctl dev break 0x4000 ; jagemu ctl dev continue 600
jagemu ctl dev stop
```

## Status (what works today)

It **boots real conveyor-belt ROMs and renders them correctly**, headless, under
input control — including full **GPU-rasterized 3D games**:

| ROM | Result |
|---|---|
| a reference backend | title screen → press A → **3D castle scene** (sky/sun/wall/portcullis), GPU-rasterized, 90 colors |
| a reference homebrew title | **pre-rendered room** with furniture + character, 90% non-black |
| a reference homebrew title | **3D room scene**, 100% non-black |
| a reference homebrew title | **renders the level** (platforms, ladders, sprites), 24 colors |
| a reference homebrew title | **title screen**, 144 colors |
| a reference homebrew title, a reference large-world port | render | 
| other reference homebrew titles | **boot cleanly, 0 illegal opcodes**, GPU active (remaining game-specific OP/CRY gaps) |

Verified subsystems:

- **Bus / memory map** — full Jaguar map with documented behavior: *no bus
  errors*, region-correct unmapped reads (`$FFFF`/`$0000`), width-sensitive
  register aliasing (the VMODE/BORD1 corruption bug reproduces).
- **68000 core** — P0+P1 ISA, all addressing modes, big-endian, level-2
  interrupts via vector 64 (`$100`), address-error on odd fetch (`bsr.l` bug).
  Boots real `m68k-aout-gcc` output (16M+ instructions, 0 illegal on the reference backend).
- **Jaguar RISC core** — the shared GPU/DSP ISA (all 64 opcodes, banked
  registers, delay slots, MAC, DIV, MOVEI). Tom GPU runs real rasterizer
  kernels from SRAM and feeds the Blitter.
- **Blitter** — pixel-addressed (XPIX) solid/pattern fills, LFU logic unit,
  A1 stepping (UPDA1), synchronous + always-idle (matches the fire-and-forget
  usage). This is what unblocked the GPU-rendered games.
- **Object Processor compositor** — the **true scan-out** (not a DRAM dump):
  word-swapped OLP, BITMAP geometry, RGB16 / **hardware CRY16 table** /
  8bpp-CLUT, TRANS + REFLECT.
- **Interrupts** — VI fires at the programmed scanline; INT1 pending/enable
  latches so both interrupt-driven *and* vblank-polling (`btst #0,INT1`) games
  work. (This unblocked two reference homebrew titles' GPUs.)
- **Joypad** — the 4-strobe controller matrix; inject input via `--press`.
- **COF loader** — verified vs a reference COF; ABS/JAG/raw; BigPEmu quirks honored.
- **Multi-instance** — no global lock; proven 4× concurrent.
- **Debug surface** — breakpoints, watchpoints, trace in core; CLI `peek`
  (memory inspect) and `break` (run-to-PC) over JSON.
- **CLI** (`jagemu`) — `info`/`run`/`screenshot`/`disasm`/`peek`/`break`/`instances`.

- **jsim truth layer (v1, HARDWARE-CALIBRATED)** — cycle-honest RISC timing via
  `--fidelity silicon|bigpemu` (default `functional` until hardware-calibrated).
  Models the scoreboard (reads stall, attributed by producer: ALU/load/div/flags),
  the bug-13 write-after-write landing order, the indexed-store unprotected-DATA
  erratum, the 17-instruction DIV shadow, taken-jump refill, and external
  fetch/data costs with a DRAM page model. Counts hazards as lints (WAW,
  MOVEI/jump in delay slot) and BigPEmu-vs-silicon divergences (the
  load-across-jump scoreboard mismodel). Stats surface per core in `run` JSON
  as `gpu.timing`/`dsp.timing`. Source: `crates/jag-core/src/risc/timing.rs`;
  golden tests derive from TRM v8 pp.34-62/errata and
  the internal porting notes (jrisc-scheduling). All latency/DRAM constants
  were calibrated against real silicon on 2026-07-17 (calib/ suite, Skunkboard
  bench): local issue/ALU-bubble/DIV-shadow/indexed-load/jump-refill all match
  hardware within 0.5%; DRAM occupancy and the GPU-in-main fetch tax within
  ~2%. Known-unmodeled (measured, documented): 68k bus contention on
  DRAM-bound GPU work (2.1x on load streams with a polling 68k) and
  consumed-external-load latency. VC modulus aligned to hardware (524).

In progress / next (scoped, spec'd in `docs/spec/`):

- **Jerry** timers + audio (I2S/DSP) — `docs/spec/JERRY_AUDIO_IO.md`.
- **CRY16** exact color table (unblocks reference homebrew titles).
- **Debug daemon** (socket protocol) + **oracle harness** (diff vs BigPEmu) —
  the v1 parity bar; also the tool to chase divergences like a reference homebrew title's.

## Build

```sh
cargo build --release        # → target/release/jagemu
cargo test                   # unit + integration tests
```

No external crates: the core is std-only, offline-buildable, deterministic.

## Use (Claude-native CLI)

Every command prints one JSON object to stdout (human notes go to stderr):

```sh
# Describe a program
jagemu info path/to/game.cof

# Boot and run N frames, dump CPU/machine state as JSON
jagemu run path/to/game.cof --frames 120

# Capture the TRUE Object-Processor frame as PNG (the honest screenshot)
jagemu screenshot path/to/game.cof --frames 120 -o shot.png

# Disassemble at an address after running
jagemu disasm path/to/game.cof --frames 120 --at 0x4000 --count 16

# List / prune isolated instances (no global lock — these never contend)
jagemu instances
jagemu instances --prune
```

`scripts/analyze_png.py shot.png` reports size / non-black% / color histogram.

## Conveyor-belt integration

Drop-in for the existing `tools/bigpemu` + `capture.sh` pattern, but native and
lock-free — see `tools/`:

```sh
tools/capture.sh path/to/game.cof            # headless boot-test → PNG + JSON, pass/fail
```

Multiple projects can run `capture.sh` **at the same time** with no queueing.

## Architecture

See `docs/ARCHITECTURE.md` for the crate layout and the borrow model, and
`docs/spec/*.md` for the implementation-grade hardware specifications (mined from
the Jaguar Technical Reference v8, the official `JAGUAR.INC`, and the proven
reference backend).

```
crates/
  jag-core/      deterministic machine: bus, m68k, risc, tom (OP+blitter), jerry, cart, scheduler, debug
  jag-debug/     68k + RISC disassemblers and debug helpers
  jag-headless/  headless runner + dependency-free PNG encoder
  jag-instance/  multi-instance isolation (no global lock)
  jagemu/        the Claude-native CLI
```
