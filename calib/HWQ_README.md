# Hardware-question probes (hwq)

Two open questions from `jaguar-shared/COBWEB_NEEDS.md` that only real silicon
can settle. Build: `make hwq` (Docker SDK image). Flash + read:

```sh
jcp -c calib/build/hwq_skunk.cof | tee hwq_$(date +%Y%m%d).log
```

Output lines (raw hex; interpretation inline):

- **`HWQ XJUMP val=…`** — the headline question. A DRAM load's ONLY consumer
  sits across a taken jump, while the load is still in flight (~8 cyc out).
  - `[GOOD: scoreboard held]` (val=$600D600D) → hardware stalls correctly
    across the jump; shadow-nops are padding, not correctness; jsim `Silicon`
    is faithful as-is.
  - `[SENTINEL: STALE read]` (val=$BADBAD00) → the scoreboard is bypassed
    across the jump (as BigPEmu does); shadow-nops are MANDATORY, and jsim
    `Silicon` needs the same value-divergence modeling BigPEmu has, and `jas`
    must forbid a consume across a jump within the load shadow.
- **`HWQ CTRL val=…`** — control: the same load+consume STRAIGHT-LINE. Must be
  `[GOOD]`. If CTRL=GOOD but XJUMP=SENTINEL, the jump is proven to be the cause.
- **`HWQ VCFULL max=…`** — unmasked VC maximum; modulus = max+1. Confirms the
  524-vs-2571 reconciliation ($20B/523 masked halfline count vs $A0B/2571
  field-bit-inclusive max). jsim reproduces both.

Results also land in DRAM at `0x100000` (XJUMP), `0x100010` (CTRL),
`0x100020` (VCFULL) for `jagemu peek` on the silent `hwq_sim.cof` build.
