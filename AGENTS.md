# For AI coding agents

This project is AI-friendly by design — it was largely written by one. If
you are an AI agent working in this repository, this file is your map.

## Build / test / measure

```sh
make sim      # build emulator -> sim/target/release/jagemu
make test     # full test suite (must stay green; ~35 tests, <1 min)
make bench    # calibration table in the simulator (needs calib ROMs built)
```

Every `jagemu` command prints exactly one JSON object on stdout (human
notes go to stderr). Use `run` for machine state, `peek` for memory,
`screenshot` for the true Object-Processor scan-out, `disasm` for 68k
disassembly, `serve`/`ctl` for persistent interactive sessions, `audio`
to capture a WAV, and `audiocheck <wav|rom> [--against <wav|rom>]` for
the audio counterpart of the pixel-diff: silence/DC/clipping/dropout/
spectral health, plus lag-aligned envelope+spectrum comparison against
an oracle capture (works on hardware WAVs too — same analyzer).

For live hardware sessions, `sim/tools/jagtap.py --device /dev/videoN`
splits the USB capture of the real Jaguar: the human watches
`http://localhost:8471/`, you fetch `/frame.jpg` (or Read the `--snap`
file), and `--audio hw:N` keeps a WAV ring that `audiocheck` can read.
Never open the V4L2 device directly — jagtap replaces that for everyone.

## Measuring performance (the point of this suite)

`--fidelity silicon` enables the hardware-calibrated timing model. In the
`run` JSON, `gpu.timing`/`dsp.timing` attribute every stalled cycle:
`stall_alu`, `stall_load`, `stall_div`, `stall_flags`, `jump_refill`,
`fetch_external`, `mem_external`, `contention`, plus hazard counters
(`waw_hazards`, `indexed_store_stale`, `slot_movei`, `slot_jump`,
`bigpemu_divergence`). Nonzero hazard counters in code you generated mean
your code is wrong on real silicon even if results look right.

## Hard-won rules (violating these produced real bugs here)

- **JRISC has exactly ONE delay slot** after JUMP/JR, always executed.
  Never place MOVEI or another jump in it.
- **Writes are not scoreboarded**: never write a register that has a
  pending load or DIV result without an intervening read (bug 13).
- **Indexed stores don't scoreboard their DATA register** — touch the
  register (`or rN,rN`) after a DIV/load before storing it via `(R14+n)`.
- **JR reaches only ±15 words** — long back-edges need `movei` + `jump (rN)`.
- rmac quirk: after `.68000` re-enter `.data`, or labels land in the wrong
  section and 68k-side copies silently copy nothing.
- GPU code executing from DRAM must follow the alignment rules in
  `calib/probes.s` (long-aligned jump sources, MOVEI immediately before,
  two NOPs after).
- All timing constants live in `sim/crates/jag-core/src/risc/timing.rs`
  with hardware provenance comments. Do not change them without a bench
  log; do not trust community folklore over the checked-in measurements.

## Conventions

- Commit author: `Claude Fable 5 <noreply@anthropic.com>`; committer:
  `Cobweb Maintainer <maintainer@cobweb.invalid>`. The maintainer is
  anonymous; no personal names anywhere in tree or history.
- The test suite is the gate: `make test` green before any commit.
- Determinism is a contract: no wall-clock, no RNG in `sim/crates/jag-core`.
- Every performance claim needs provenance: a bench log, a probe, or a
  simulator run someone else can reproduce with one command.
