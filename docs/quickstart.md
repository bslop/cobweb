# Quickstart

Zero to a running Jaguar in two commands. Everything here is copy-paste.

## Prerequisites

- **Rust** (any recent stable): https://rustup.rs — one script, no config.
- **Python 3** (for result tables; almost certainly already installed).
- **Docker** — *only* if you want to rebuild the calibration/benchmark ROMs
  from source. Not needed to use the simulator.

## 1. Build and run something

```sh
make sim                  # builds the emulator (no dependencies beyond Rust)
make run ROM=game.cof     # boots it, prints machine state as JSON
make shot ROM=game.cof    # saves what the console actually displays to shot.png
```

`.cof`, `.abs`, `.jag`, universal-header `.j64`, and raw binaries all load.
Every command prints one JSON object — pleasant for humans, trivial for
scripts and AI agents.

## 2. Measure something (the part no other Jaguar tool does)

```sh
make run ROM=game.cof FIDELITY=silicon FRAMES=600
```

Look at `gpu.timing` in the output: every stalled GPU cycle attributed to
its cause — ALU bubbles, load latency, DIV shadow, taken-jump refill,
external fetch tax, 68k bus contention. The `silicon` timing model is
calibrated against a real console (see `calib/README.md`; mean error 0.059
cycles/instruction across 32 hardware probes). `--fidelity functional` is
a fast uncalibrated mode; `bigpemu` mimics that emulator's known timing
divergences so you can compare.

## 3. Verify our numbers yourself

No hardware:

```sh
make calib      # builds the probe ROMs (Docker, first pull is a few GB)
make bench      # runs all 18 timing probes in the simulator, prints the table
```

With a Skunkboard and a real Jaguar:

```sh
jcp -c calib/build/calib_skunk.cof | tee mybench.log
python3 calib/parse_results.py --console mybench.log
```

Compare against `calib/PREDICTIONS_silicon.txt` and the checked-in bench
logs. **If your console disagrees with ours, please open an issue — a
contradicting bench log is the most valuable contribution there is.**

## 4. Where things live

| Path | What |
|---|---|
| `sim/` | emulator + calibrated timing model (`sim/README.md`) |
| `calib/` | hardware calibration suite + bench logs (`calib/README.md`) |
| `bench/` | compiler benchmark harness |
| `docs/COMPARISON.md` | what this suite solves vs. existing tools |
| `RESEARCH.md` | the full ecosystem/hardware survey |

## Troubleshooting

- **`make calib` fails immediately** — Docker isn't installed or the daemon
  isn't running. The prebuilt ROMs may also be checked into releases; you
  only need Docker to rebuild from source.
- **A ROM boots to a black screen** — try `make shot` with more frames
  (`FRAMES=600`); some titles take many seconds to first draw. If it stays
  black but `gpu.instret` is climbing, the program runs and the display
  path is the gap — please file it with the ROM name.
- **Something disagrees with real hardware** — that's a bug we want more
  than any other. File it with your bench log.
