# the seed emulator — Architecture

A from-scratch, **instrumentation-first** Atari Jaguar emulator written in Rust,
purpose-built to replace BigPEmu in the Jaguar game-porting conveyor belt that
lives across `~/Documents/Git/{jag_*,proj_*}`.

## Why this exists (the 7 BigPEmu pains it kills)

1. **One global lock** → every Claude instance serialized through a single
   Wine'd emulator. We are **true multi-instance**: each emulator is an
   ordinary, fully-isolated process. No global lock, ever.
2. **Wine** Windows binary → fragile. We are **native Linux**, statically
   linkable, no Wine, no `wineserver`.
3. **Display gymnastics** (Xvfb/weston/Xwayland) just to get pixels → we render
   the framebuffer **off-screen in-process** and emit PNG directly. No X server.
4. **Screenshots lie**: BigPEmu's headless dumps read *the DRAM the 68000 wrote,
   not the OP scan-out*, so they PASS while the screen is wrong. We capture the
   **true Object-Processor-composited frame** — what the display DAC would emit.
5. **Config overwritten on exit** → we have no global mutable config to fight;
   everything is per-invocation CLI/JSON.
6. **Awkward introspection** (per-game `.c` CVM scripts, no `goto`, binary file
   dumps) → a **native debug API**: peek/poke, registers, breakpoints,
   watchpoints, single-step, disassembly, trace, state snapshots — over JSON.
7. **Closed source, threaded approximation** → we are open, deterministic, and
   single-threaded-per-instance for **reproducible** runs (same ROM + same
   inputs ⇒ identical frames, every time).

## v1 success bar

Match **BigPEmu as a regression oracle**: run the same ROM in both, diff the
framebuffer and key machine state. The oracle harness (`jagemu oracle`) drives
both and reports divergence. First target is the **homebrew subset** the
conveyor belt actually uses (RGB16 direct-color OP bitmaps, GPU SRAM kernels,
blitter spans) — see `docs/spec/HOMEBREW_SUBSET.md`.

## Crate layout

```
crates/
  jag-core/      The deterministic machine. No I/O, no threads. Embeddable.
    bus / mem    Memory map, 16- vs 32-bit register access, documented
                 unmapped-read values (NO bus errors — Jaguar-correct).
    m68k         Motorola 68000 interpreter (big-endian, IPL/interrupts).
    risc         The shared Jaguar RISC ISA, instantiated as Tom GPU + Jerry DSP.
    tom          Video: Object Processor (true scan-out compositor) + Blitter +
                 video registers/timing.
    jerry        Timers, interrupt routing, joypad, audio (I2S) stub.
    cart         COF / ABS / JAG / ROM loaders (with the documented quirks).
    scheduler    Deterministic cycle/scanline/frame stepping.
    debug        Breakpoints, watchpoints, trace hooks the machine calls into.
  jag-debug/     Disassemblers (68k + RISC) + higher-level debug helpers.
  jag-headless/  Headless runner: run-to-frame-N, true-OP framebuffer → PNG.
  jag-instance/  Multi-instance isolation: per-instance dirs, registry, no lock.
  jagemu/        The Claude-native CLI. JSON in/out. Drop-in BigPEmu wrapper.
```

## The borrow model (Rust emulator structure)

The classic emulator aliasing problem is solved by keeping the CPUs **separate
from** the `Bus`:

```
struct Jaguar {
    cpu:  M68k,     // steps with &mut Bus
    gpu:  Risc,     // Tom GPU; steps with &mut Bus
    dsp:  Risc,     // Jerry DSP; steps with &mut Bus
    bus:  Bus,      // DRAM + Tom regs + Jerry regs + OP/Blitter device state
    sched: Scheduler,
    dbg:  Debugger,
}
```

`Bus` owns all *memory and memory-mapped device state* but **not** the processor
register files. A processor step borrows `&mut bus` for the duration of one
instruction, reads/writes memory through it, then releases it — so no two
processors alias the bus simultaneously (we step them cooperatively on the
scheduler's timeline, which is also what makes runs deterministic).

## Determinism contract

* Single instance = single thread. No wall-clock, no RNG in the core.
* `run_to_frame(n)` advances an exact, reproducible number of cycles.
* All nondeterminism (input, time) is injected explicitly via the control API.
* This is what makes BigPEmu-oracle diffing meaningful and makes AI-driven
  debugging (set breakpoint, run, inspect) repeatable.

## Multi-instance model

No `/tmp/bigpemu-shared/.lock`. Each instance:
* runs as its own process with its own memory image and state directory
  (`$JAGEMU_HOME/instances/<id>/`),
* exposes its control socket at `<state-dir>/control.sock` (or pure stdio in
  one-shot mode),
* never touches global mutable state, so N Claude instances run N emulators
  concurrently with zero contention.

See `docs/spec/*` for the implementation-grade hardware specs (mined from the
official Jaguar Technical Reference + `JAGUAR.INC` + the proven reference
backend).
