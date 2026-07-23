//! The jsim truth layer: a cycle-honest pipeline model for the Jaguar RISC
//! cores (Tom GPU / Jerry DSP).
//!
//! Sources of truth, in trust order:
//! 1. Jaguar Technical Reference v8 — scoreboard pp.34-35 ("no score-board
//!    protection applies to writes"), stall list p.62, instruction latency
//!    table pp.46-58, errata pp.133-141 (bugs 2, 13, 15, 25).
//! 2. the internal porting notes (jrisc-scheduling) — [HW]-verified corrections
//!    (ONE delay slot; shadow nops are padding not correctness; the 17-free-
//!    instruction div shadow; indexed stores don't scoreboard their DATA).
//!
//! Model summary:
//! - Reads stall until the producing write is ready (the scoreboard). Stalls
//!   are *attributed* (ALU / load / div / flags / divider-busy) so a profile
//!   answers WHY a kernel is slow, not just how slow.
//! - Writes are NOT protected. A fast write into a register with a pending
//!   slow write (load or div) is the bug-13 hazard: the slow write lands LAST,
//!   so the register ends up holding the first-issued (slow) value. `Silicon`
//!   reproduces that landing order; every profile counts the hazard.
//! - Indexed stores (ops 49/50/60/61) do not scoreboard their DATA register
//!   (TRM errata §2): storing a still-pending load/div result writes the STALE
//!   value under `Silicon`, and is counted under every timed profile.
//! - `BigPEmu` matches `Silicon` timing except its documented mismodel: it
//!   does not scoreboard external DRAM loads consumed across a taken jump
//!   (deterministic 76/76 repro, the pilot project 2026-07-17). We count each such
//!   site as a divergence rather than corrupt state — the counter IS the diff
//!   signal between profiles.
//!
//! Every latency constant lives in [`Lat`] and every DRAM constant in the
//! `DRAM_*`/`EXT_*` items. They are CALIBRATION KNOBS, not gospel: the
//! acceptance bar is stall predictions within measured error of Skunkboard
//! numbers on real silicon (two consoles + GameDrive + Skunkboard are on
//! hand). Open calibration questions are flagged `CAL:` inline.

/// How faithfully the RISC core models time.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Fidelity {
    /// One instruction = one cycle, no hazards. The pre-truth-layer behavior;
    /// remains the default until the model is hardware-calibrated.
    #[default]
    Functional,
    /// Full pipeline truth: scoreboard stalls, WAW landing order, indexed-store
    /// erratum, div shadow, jump refill, external fetch/page costs.
    ///
    /// GOAL (set 2026-07-23): be as close to real silicon as possible —
    /// including BEHAVIOR, not just timing. Where hardware serves garbage,
    /// Silicon should serve garbage. Two known gaps (OpenLara blitter-bug
    /// rounds 5-6, silicon-repro'd) where we currently serve a value-correct
    /// result hardware corrupts:
    ///   * a read of a DIV dest BEFORE the quotient is readable (no divider
    ///     interlock on hardware) — `read_stall` serves the settled value;
    ///   * an internal-SRAM load consumed across a taken jump (same erratum
    ///     as the BigPEmu-only path below, but REAL on silicon).
    /// Both need the true readable-latency threshold before modeling — a
    /// blind poison over-fires on correctly-scheduled code (their prototype:
    /// 70K false positives), which would be LESS faithful, not more. Blocked
    /// on `p_divlat` (calib/probes.s): the smallest K reading 0x55 is the
    /// threshold. Then: recalibrate `Lat::DIV`, poison early reads, and lift
    /// the load-across-jump check out of the BigPEmu-only gate below.
    Silicon,
    /// Silicon timing minus BigPEmu's documented mismodels; divergences from
    /// silicon semantics are counted, not applied.
    BigPEmu,
}

/// Result-ready and issue-cost constants, in RISC clock ticks.
///
/// Convention: an instruction issues at tick T and occupies `cost` ticks
/// (base issue + stalls). Result-ready latencies are anchored to the *last*
/// issue tick (`end - 1`): a 1-tick ALU op at [T, T+1) has its result ready at
/// T + ALU, so the very next instruction pays a 1-tick bubble — matching the
/// TRM's "writes at cycle 3" rows.
pub struct Lat;
impl Lat {
    /// ALU-class result (ADD/SUB/logic/shift/MULT/SAT…): written at cycle 3.
    pub const ALU: u64 = 2;
    /// Internal (local SRAM / own control regs) load result. TRM says cycle
    /// 3-4; HARDWARE (bench 2026-07-17, ldsram probe) says a load+consume
    /// pair is 3 cycles, i.e. ready at start+2. Indexed loads land one later
    /// (ldidx probe: pair = 6) — the +1 is added at the use site.
    pub const LOAD_INTERNAL: u64 = 2;
    /// DIV quotient: written at cycle 18 → the 17-instruction shadow.
    pub const DIV: u64 = 18;
    /// Extra issue ticks for an indexed load (address computation, TRM p.62).
    pub const IDX_LOAD_ISSUE: u32 = 2;
    /// Extra issue tick after an indexed store (TRM p.62).
    pub const IDX_STORE_ISSUE: u32 = 1;
    /// Taken JUMP/JR: pipeline refill after the delay slot. HARDWARE (jr
    /// probe): a taken JR costs 4 total = 1 issue + this refill, matching the
    /// TRM's "3 cycles after a taken jump" read literally.
    pub const JUMP_REFILL: u32 = 3;
    /// MOVEI: 3 instruction words, result usable at completion.
    pub const MOVEI_ISSUE: u32 = 3;
}

/// DRAM/bus model — HARDWARE-CALIBRATED (Skunkboard bench 2026-07-17,
/// calib suite v1, quiet-bus mode B unless noted; log in calib/).
/// The U-235 "2 ticks/MOVE" folklore was a JPIT clock artifact — the VC-timed
/// bench measured 1.00 cyc/instr local issue.
const DRAM_ROW_SHIFT: u32 = 11; // 2 KB pages
/// Issue-side bus occupancy of an external DRAM access (load or store),
/// page hit / miss. HARDWARE: lddram B pair = 4.1 (occupancy ~1), ldstride
/// B pair = 5.1 (~2), stdram identical — stores pay it too.
const DRAM_OCC_HIT: u32 = 1;
const DRAM_OCC_MISS: u32 = 2;
/// Additional result latency of a CONSUMED external DRAM load beyond
/// LOAD_INTERNAL. HARDWARE (session 2, lddramc B): consumed load-to-use is
/// ~15-16 cycles on a quiet bus — same order as the cross-chip EXT_OTHER
/// path (bus grant + access + return). MISS variant: provisional (CAL:
/// needs a consumed-strided probe).
const DRAM_LAT_HIT: u32 = 13;
const DRAM_LAT_MISS: u32 = 14;
/// Per-16-bit-word external instruction fetch, page hit / page miss, QUIET
/// bus. HARDWARE (session 2, mains mode B): 6.24 cyc/instr with the 68k
/// STOPped → ~5.2/word. (U-235's famous ~8.5x was measured with the 68k
/// "idling but not stopped" — folklore refined: truly quiet is 6.2x.)
const EXT_FETCH_HIT: u32 = 5;
const EXT_FETCH_MISS: u32 = 7;
/// Extra ticks a page-HIT external access pays while the 68000 is on the bus.
/// Mechanism (HARDWARE, bench 2026-07-17): row thrash + arbitration — the
/// 68k's interleaved traffic closes the GPU's open row, so sequential streams
/// pay extra while true page misses see none (ldstride A == B exactly).
/// Data accesses: ~+4 occupancy and ~+4 result latency (lddram/lddramc A-B).
/// Instruction fetch: ~+7/word (mains A 13.46 vs B 6.24 cyc/instr).
const CONTENTION_HIT_EXTRA: u32 = 4;
const CONTENTION_FETCH_EXTRA: u32 = 7;
/// Object Processor scan-out tax, in **milli-ticks per external DRAM access per
/// OP phrase-per-line**. The OP re-reads the display list's bitmaps every
/// visible line and outranks the GPU, so its traffic is a continuous background
/// load the RISCs arbitrate against.
///
/// HARDWARE-CALIBRATED (Skunkboard 2026-07-19, probe `lddramop`, mode B):
/// Tom's DRAM load stream ran 655 ticks with the OP parked on a STOP object and
/// 728 ticks with a full-screen 320x240 16bpp bitmap (80 phrases/line) — +11.1%,
/// i.e. **+0.46 cycles per external access at 80 phrases/line** → 5.75
/// milli-ticks per phrase. (The DSP, given the same treatment, moved Tom by
/// 0.0%: the GPU outranks Jerry, so Jerry is *not* a contention source. See
/// COBWEB_GAP_tom_jerry_contention.)
const OP_TAX_MILLI_NUM: u64 = 575; // 5.75 milli-ticks per phrase, per access
const OP_TAX_MILLI_DEN: u64 = 100;
/// Non-DRAM external data (cross-chip registers, cart): consumed-read cost
/// ~15-16 cycles. HARDWARE: derived from the null-probe overhead delta (a
/// consumed GPU read of Tom's VC); other cross-chip paths may differ. CAL.
const EXT_OTHER: u32 = 14;

/// Why a pending register write is slow — used for stall attribution and for
/// hazard semantics (only loads and DIV are slow enough to land out of order).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PendKind {
    Alu,
    Load,
    ExtLoad,
    Div,
}

impl PendKind {
    fn is_slow(self) -> bool {
        matches!(self, PendKind::Load | PendKind::ExtLoad | PendKind::Div)
    }
}

/// An in-flight register write the scoreboard knows about.
#[derive(Debug, Clone, Copy)]
struct Pending {
    /// bank*32 + index
    reg: u8,
    ready_at: u64,
    kind: PendKind,
    /// The producer's value (what the slow write will land). Captured because
    /// `isa::execute` applies results immediately; the truth layer re-lands
    /// this value at `ready_at` if a newer write raced it (bug 13).
    value: u32,
    /// Register value before the producer issued — what an indexed store's
    /// unprotected DATA read actually picks up (TRM errata §2).
    old_value: u32,
    /// A newer write raced this one (write-after-write hazard).
    dirty: bool,
    /// A taken jump occurred while this was in flight (BigPEmu divergence
    /// trigger for external loads).
    jumped: bool,
}

/// Stall/hazard accounting, per core. Cycle counts are RISC ticks.
#[derive(Debug, Default, Clone)]
pub struct TimingStats {
    /// Ticks stalled reading a not-yet-ready ALU result.
    pub stall_alu: u64,
    /// Ticks stalled reading a not-yet-ready load result.
    pub stall_load: u64,
    /// Ticks stalled reading a not-yet-ready DIV quotient.
    pub stall_div: u64,
    /// Ticks stalled waiting for flags (conditional jump / ADDC / SUBC).
    pub stall_flags: u64,
    /// Ticks stalled issuing a DIV while the divider is busy.
    pub stall_div_busy: u64,
    /// Ticks of taken-jump pipeline refill.
    pub jump_refill: u64,
    /// Ticks fetching instructions from external memory (GPU-in-main cost).
    pub fetch_external: u64,
    /// Ticks of external data-access latency observed by this core.
    pub mem_external: u64,
    /// Bug-13 write-after-write hazards (a write raced a pending load/div).
    /// Under `Silicon` the slow value lands last, as on hardware.
    pub waw_hazards: u64,
    /// Indexed stores that read a stale DATA register (TRM errata §2).
    pub indexed_store_stale: u64,
    /// MOVEI executed in a delay slot (forbidden by the scheduling handbook;
    /// unverified on hardware).
    pub slot_movei: u64,
    /// JUMP/JR executed in a delay slot ("results are not predictable").
    pub slot_jump: u64,
    /// Sites where BigPEmu semantics diverge from silicon (external load
    /// consumed across a taken jump without a scoreboard stall).
    pub bigpemu_divergence: u64,
    /// Ticks of 68k bus-contention (row-thrash) tax paid by external accesses.
    pub contention: u64,
    /// Blitter BUSY ticks (launch + transfer; asynchronous — busy time can
    /// exceed what the frame pays when the kernel overlaps compute).
    /// HARDWARE-CALIBRATED — see `tom::blit` BLIT_* constants. Split below
    /// per COBWEB_BUG_blitter_overcharged round 2, so each piece can be
    /// checked against subtractive silicon probes.
    pub blit: u64,
    /// Launch-overhead component of `blit` (BLIT_LAUNCH_TICKS per B_CMD).
    pub blit_launch: u64,
    /// Transfer component of `blit` (phrase-access ticks).
    pub blit_transfer: u64,
    /// Ticks this core spent executing loads of B_CMD that OBSERVED BUSY —
    /// the bwait spin, measured (not modeled) at the same granularity the
    /// kernel polls on hardware.
    pub blit_wait: u64,
}

impl TimingStats {
    /// All attributed stall ticks (excludes external fetch/data occupancy).
    pub fn total_stall(&self) -> u64 {
        self.stall_alu
            + self.stall_load
            + self.stall_div
            + self.stall_flags
            + self.stall_div_busy
            + self.jump_refill
    }
}

/// Per-core pipeline timing state.
#[derive(Debug, Default)]
pub struct Pipeline {
    pend: Vec<Pending>,
    flags_ready: u64,
    div_busy_until: u64,
    /// Fractional OP-tax carry (milli-ticks) so a sub-tick per-access cost
    /// accumulates instead of rounding to zero.
    op_tax_debt: u64,
    /// Last DRAM row touched by this core. CAL: per-core rows ignore
    /// cross-master page thrash (OP/Blitter/68k); calibration will decide
    /// whether a shared row + contention model is needed.
    last_dram_row: Option<u32>,
    /// Cycle of the most recent DRAM data access (density-regime model).
    last_dram_cycle: u64,
    /// Regime ticks charged at that access — subtracted from the next gap so
    /// the model keys on ISSUE density, not post-stall spacing (silicon
    /// charges a 1-per-4-instr stream steadily; measuring the gap after our
    /// own added wait made the model oscillate charge/no-charge).
    last_dram_extra: u64,
    pub stats: TimingStats,
}

impl Pipeline {
    pub fn reset(&mut self) {
        self.pend.clear();
        self.flags_ready = 0;
        self.div_busy_until = 0;
        self.op_tax_debt = 0;
        self.last_dram_row = None;
        self.last_dram_cycle = 0;
        self.last_dram_extra = 0;
        self.stats = TimingStats::default();
    }

    /// Land any pending writes that are due. Under `Silicon`, a dirty pending
    /// (raced by a newer write) re-lands the producer's value — the bug-13
    /// out-of-order landing: "the register holds the first value".
    pub fn settle(&mut self, now: u64, regs: &mut [[u32; 32]; 2], fidelity: Fidelity) {
        self.pend.retain(|p| {
            if p.ready_at <= now {
                if p.dirty && fidelity == Fidelity::Silicon {
                    regs[(p.reg >> 5) as usize][(p.reg & 31) as usize] = p.value;
                }
                false
            } else {
                true
            }
        });
    }

    /// Stall ticks to read the registers/flags `access` consumes at `now`.
    pub fn operand_stall(
        &mut self,
        access: &Access,
        bank: usize,
        now: u64,
        fidelity: Fidelity,
    ) -> u64 {
        let mut stall = 0u64;
        for r in access.reads.iter().flatten() {
            let b = if access.read_alt_bank { 1 - bank } else { bank };
            let id = (b * 32) as u8 + r;
            stall = stall.max(self.read_stall(id, now, fidelity));
        }
        if access.uses_flags && self.flags_ready > now {
            let wait = self.flags_ready - now;
            self.stats.stall_flags += wait;
            stall = stall.max(wait);
        }
        stall
    }

    fn read_stall(&mut self, reg: u8, now: u64, fidelity: Fidelity) -> u64 {
        let Some(p) = self.pend.iter().find(|p| p.reg == reg) else {
            return 0;
        };
        if p.ready_at <= now {
            return 0;
        }
        if fidelity == Fidelity::BigPEmu && p.kind == PendKind::ExtLoad && p.jumped {
            self.stats.bigpemu_divergence += 1;
            return 0;
        }
        let wait = p.ready_at - now;
        match p.kind {
            PendKind::Alu => self.stats.stall_alu += wait,
            PendKind::Load | PendKind::ExtLoad => self.stats.stall_load += wait,
            PendKind::Div => self.stats.stall_div += wait,
        }
        wait
    }

    /// Stall issuing a DIV while the divider is still busy (TRM bug 25 class).
    pub fn div_stall(&mut self, now: u64) -> u64 {
        if self.div_busy_until > now {
            let wait = self.div_busy_until - now;
            self.stats.stall_div_busy += wait;
            wait
        } else {
            0
        }
    }

    /// Charge one external DRAM access its share of Object Processor scan-out
    /// contention. `phrases` is the OP's per-line fetch demand (0 = idle list).
    /// The per-access cost is a fraction of a tick, so it accumulates in
    /// `op_tax_debt` and is released as whole ticks — attributed to `contention`.
    pub fn charge_op_tax(&mut self, phrases: u32) -> u32 {
        if phrases == 0 {
            return 0;
        }
        self.op_tax_debt += phrases as u64 * OP_TAX_MILLI_NUM / OP_TAX_MILLI_DEN;
        let whole = self.op_tax_debt / 1000;
        if whole > 0 {
            self.op_tax_debt -= whole * 1000;
            self.stats.contention += whole;
        }
        whole as u32
    }

    pub fn set_div_busy(&mut self, until: u64) {
        self.div_busy_until = until;
    }

    pub fn set_flags_ready(&mut self, at: u64) {
        self.flags_ready = at;
    }

    /// Record a slow (or ALU-latency) result entering the scoreboard.
    pub fn push_slow(&mut self, reg: u8, ready_at: u64, kind: PendKind, value: u32, old_value: u32) {
        self.pend.retain(|p| p.reg != reg);
        self.pend.push(Pending {
            reg,
            ready_at,
            kind,
            value,
            old_value,
            dirty: false,
            jumped: false,
        });
    }

    /// A write to `reg` is about to happen. If a slow write is still in flight
    /// this is the bug-13 WAW hazard: count it and mark the pending dirty so
    /// `settle` re-lands the slow value under `Silicon`.
    pub fn record_write(&mut self, reg: u8, now: u64) {
        if let Some(p) = self.pend.iter_mut().find(|p| p.reg == reg) {
            if p.ready_at > now && p.kind.is_slow() {
                p.dirty = true;
                self.stats.waw_hazards += 1;
            }
        }
    }

    /// A taken jump: charge the refill and mark in-flight loads (the BigPEmu
    /// consumed-across-a-jump trigger).
    pub fn taken_jump(&mut self) {
        self.stats.jump_refill += Lat::JUMP_REFILL as u64;
        for p in &mut self.pend {
            p.jumped = true;
        }
    }

    /// Stale value an indexed store's unprotected DATA read picks up, if its
    /// producer is still in flight. Counts the hazard.
    pub fn indexed_store_stale_value(&mut self, reg: u8, now: u64) -> Option<u32> {
        let p = self.pend.iter().find(|p| p.reg == reg).copied()?;
        if p.ready_at > now && p.kind.is_slow() {
            self.stats.indexed_store_stale += 1;
            Some(p.old_value)
        } else {
            None
        }
    }

    /// External data-access cost for one 32-bit access at `addr`, split into
    /// issue-side bus occupancy (charged to the instruction) and extra result
    /// latency (charged to the scoreboard for loads). `contended` = the 68000
    /// is on the bus (not STOPped): page-hit accesses pay the row-thrash tax.
    pub fn ext_access(&mut self, class: MemClass, addr: u32, contended: bool, now: u64) -> (u32, u32) {
        let (occ, lat) = match class {
            // Blitter-register block ($F022xx): HARDWARE (p_bcmdidle, bench
            // 2026-07-21 s2): a GPU load of B_CMD costs 2.0 cycles, not the
            // 1.0 of a local access — one extra bus cycle to cross into the
            // Blitter's register file. The core's own $F021xx control regs
            // stay free (unprobed, but G_FLAGS polling matching the nop
            // baseline anchors them). This is the bwait-poll under-charge:
            // the shaded kernels poll B_CMD thousands of times per frame.
            MemClass::Internal if (0x00F0_2200..0x00F0_2280).contains(&addr) => (1, 0),
            MemClass::Internal => (0, 0),
            MemClass::Dram => {
                let row = addr >> DRAM_ROW_SHIFT;
                if self.last_dram_row == Some(row) {
                    (DRAM_OCC_HIT, DRAM_LAT_HIT)
                } else {
                    self.last_dram_row = Some(row);
                    (DRAM_OCC_MISS, DRAM_LAT_MISS)
                }
            }
            MemClass::ExtOther => (1, EXT_OTHER),
        };
        // DENSITY REGIME — HARDWARE (p_dens2/6/14/30 + lddram, bench
        // 2026-07-21 s3). The flat model was wrong in both directions:
        //  * a QUIET-BUS access 5..10 cycles after the previous one pays
        //    ~+2 (dens2 B 8.20 vs 6.04 modeled) — yet back-to-back
        //    streaming (lddram B, gap ~4) pays nothing: page streaming
        //    holds the bus, a short idle gap forces re-arbitration.
        //    Empirical window; mechanism note in NEXT_BENCH.md.
        //  * the 68k tax exists ONLY inside that burst window: at game
        //    density silicon mode A == mode B exactly (dens30: 1.94 both)
        //    while the old flat tax charged every page-hit load.
        let mut occ = occ;
        if class == MemClass::Dram {
            let gap = now
                .saturating_sub(self.last_dram_cycle)
                .saturating_sub(self.last_dram_extra);
            let mut extra = 0u64;
            if (5..10).contains(&gap) {
                occ += 2; // quiet-bus re-arbitration window
                extra += 2;
                if contended {
                    occ += 6; // 68k steals the re-grant (dens2 A−B = 6.1)
                    self.stats.contention += 6;
                    extra += 6;
                }
            } else if gap < 5 && contended {
                occ += 4; // streaming under 68k pressure (lddram A−B = 4.3)
                self.stats.contention += 4;
                extra += 4;
            }
            self.last_dram_cycle = now;
            self.last_dram_extra = extra;
        }
        self.stats.mem_external += (occ + lat) as u64;
        (occ, lat)
    }

    /// Ticks added to the current DRAM access by OTHER models (OP tax): they
    /// stretch spacing without lowering issue density, so exclude them from
    /// the next regime-gap measurement (same reason as last_dram_extra).
    pub fn note_dram_stretch(&mut self, t: u64) {
        self.last_dram_extra += t;
    }

    /// Contention tax for a page-hit DRAM LOAD under a busy 68k (occupancy
    /// part; the caller adds the latency part to the scoreboard entry).
    pub fn charge_contention_load(&mut self) -> u32 {
        self.stats.contention += (2 * CONTENTION_HIT_EXTRA) as u64;
        CONTENTION_HIT_EXTRA
    }

    /// External instruction-fetch cost for `words` 16-bit words at `addr`.
    pub fn fetch_cost(&mut self, addr: u32, words: u32, in_dram: bool, contended: bool) -> u32 {
        let mut total = 0;
        for i in 0..words {
            let a = addr + i * 2;
            total += if !in_dram {
                EXT_OTHER
            } else {
                let row = a >> DRAM_ROW_SHIFT;
                if self.last_dram_row == Some(row) {
                    let extra = if contended { CONTENTION_FETCH_EXTRA } else { 0 };
                    self.stats.contention += extra as u64;
                    EXT_FETCH_HIT + extra
                } else {
                    self.last_dram_row = Some(row);
                    EXT_FETCH_MISS
                }
            };
        }
        self.stats.fetch_external += total as u64;
        total
    }
}

/// What one decoded instruction reads/writes, for the scoreboard.
/// Register operands are bank-relative indices; the caller maps to bank ids.
#[derive(Debug, Default, Clone, Copy)]
pub struct Access {
    /// Bank-relative register indices read (r1/r2/implicit R14/R15).
    pub reads: [Option<u8>; 3],
    /// Bank-relative destination register, if any.
    pub write: Option<u8>,
    /// Destination lives in the *other* bank (MOVETA).
    pub write_alt_bank: bool,
    /// Source registers read from the *other* bank (MOVEFA).
    pub read_alt_bank: bool,
    /// Consumes flags (conditional JUMP/JR, ADDC/SUBC).
    pub uses_flags: bool,
    /// Defines flags.
    pub sets_flags: bool,
}

/// Decode the operand/flag behavior of `iw`. Mirrors `isa::execute` — any
/// semantic change there must be reflected here.
pub fn classify(iw: u16, is_dsp: bool) -> Access {
    let op = ((iw >> 10) & 0x3F) as u8;
    let r1 = ((iw >> 5) & 0x1F) as u8;
    let r2 = iw as u8 & 0x1F;
    let mut a = Access::default();
    let mut ri = 0usize;
    let mut rd = |a: &mut Access, r: u8| {
        if ri < 3 {
            a.reads[ri] = Some(r);
            ri += 1;
        }
    };
    match op {
        // two-operand ALU: read r1,r2; write r2; set flags
        0 | 4 | 9 | 10 | 11 | 16 | 17 => {
            rd(&mut a, r1);
            rd(&mut a, r2);
            a.write = Some(r2);
            a.sets_flags = true;
        }
        // ADDC/SUBC also consume carry
        1 | 5 => {
            rd(&mut a, r1);
            rd(&mut a, r2);
            a.write = Some(r2);
            a.sets_flags = true;
            a.uses_flags = true;
        }
        // ADDQ/SUBQ (flags) — r1 is an immediate
        2 | 6 => {
            rd(&mut a, r2);
            a.write = Some(r2);
            a.sets_flags = true;
        }
        // ADDQT/SUBQT: flag-transparent
        3 | 7 => {
            rd(&mut a, r2);
            a.write = Some(r2);
        }
        // NEG/NOT/ABS
        8 | 12 | 22 => {
            rd(&mut a, r2);
            a.write = Some(r2);
            a.sets_flags = true;
        }
        // BTST: flags only
        13 => {
            rd(&mut a, r2);
            a.sets_flags = true;
        }
        // BSET/BCLR
        14 | 15 => {
            rd(&mut a, r2);
            a.write = Some(r2);
            a.sets_flags = true;
        }
        // IMULTN: MAC start (flags, no reg write)
        18 => {
            rd(&mut a, r1);
            rd(&mut a, r2);
            a.sets_flags = true;
        }
        // RESMAC: MAC → r2 (fast write, no flags)
        19 => {
            a.write = Some(r2);
        }
        // IMACN: MAC accumulate
        20 => {
            rd(&mut a, r1);
            rd(&mut a, r2);
        }
        // DIV: slow write, flag-transparent
        21 => {
            rd(&mut a, r1);
            rd(&mut a, r2);
            a.write = Some(r2);
        }
        // SH/SHA (register count)
        23 | 26 => {
            rd(&mut a, r1);
            rd(&mut a, r2);
            a.write = Some(r2);
            a.sets_flags = true;
        }
        // quick shifts/rotates
        24 | 25 | 27 | 29 => {
            rd(&mut a, r2);
            a.write = Some(r2);
            a.sets_flags = true;
        }
        // ROR (register count)
        28 => {
            rd(&mut a, r1);
            rd(&mut a, r2);
            a.write = Some(r2);
            a.sets_flags = true;
        }
        // CMP/CMPQ: flags only
        30 => {
            rd(&mut a, r1);
            rd(&mut a, r2);
            a.sets_flags = true;
        }
        31 => {
            rd(&mut a, r2);
            a.sets_flags = true;
        }
        // SAT/MOD family; GPU PACK/UNPACK (op 63) is flag-transparent
        32 | 33 | 62 | 63 => {
            rd(&mut a, r2);
            a.write = Some(r2);
            a.sets_flags = !(op == 63 && !is_dsp);
        }
        // MOVE / MOVEQ / MOVETA / MOVEFA / MOVEI / MOVE PC: fast writes
        34 => {
            rd(&mut a, r1);
            a.write = Some(r2);
        }
        35 => {
            a.write = Some(r2);
        }
        36 => {
            rd(&mut a, r1);
            a.write = Some(r2);
            a.write_alt_bank = true;
        }
        37 => {
            rd(&mut a, r1);
            a.write = Some(r2);
            a.read_alt_bank = true;
        }
        38 => {
            a.write = Some(r2);
        }
        // simple loads: (r1) → r2
        39 | 40 | 41 => {
            rd(&mut a, r1);
            a.write = Some(r2);
        }
        // GPU LOADP / DSP SAT32S
        42 => {
            if is_dsp {
                a.write = Some(r2);
                a.sets_flags = true;
            } else {
                rd(&mut a, r1);
                a.write = Some(r2);
            }
        }
        // indexed loads (R14+n)/(R15+n)
        43 => {
            rd(&mut a, 14);
            a.write = Some(r2);
        }
        44 => {
            rd(&mut a, 15);
            a.write = Some(r2);
        }
        // plain stores: addr r1, data r2 — data IS scoreboarded
        45 | 46 | 47 => {
            rd(&mut a, r1);
            rd(&mut a, r2);
        }
        // GPU STOREP / DSP MIRROR
        48 => {
            if is_dsp {
                rd(&mut a, r2);
                a.write = Some(r2);
                a.sets_flags = true;
            } else {
                rd(&mut a, r1);
                rd(&mut a, r2);
            }
        }
        // indexed stores: DATA (r1) is NOT scoreboarded — the erratum. Only
        // the base register is a protected read.
        49 => {
            rd(&mut a, 14);
        }
        50 => {
            rd(&mut a, 15);
        }
        51 => {
            a.write = Some(r2);
        }
        // JUMP cc,(r1) / JR cc,n — flags if conditional (cc field in r2)
        52 => {
            rd(&mut a, r1);
            a.uses_flags = r2 != 0;
        }
        53 => {
            a.uses_flags = r2 != 0;
        }
        // MMULT: bank-1 regs + local RAM → r2
        54 => {
            a.write = Some(r2);
            a.sets_flags = true;
        }
        // MTOI/NORMI
        55 | 56 => {
            rd(&mut a, r1);
            a.write = Some(r2);
            a.sets_flags = true;
        }
        57 => {} // NOP
        // register-indexed loads
        58 => {
            rd(&mut a, r1);
            rd(&mut a, 14);
            a.write = Some(r2);
        }
        59 => {
            rd(&mut a, r1);
            rd(&mut a, 15);
            a.write = Some(r2);
        }
        // register-indexed stores: data r1 unprotected; base+offset protected
        60 => {
            rd(&mut a, r2);
            rd(&mut a, 14);
        }
        61 => {
            rd(&mut a, r2);
            rd(&mut a, 15);
        }
        _ => {}
    }
    a
}

/// Indexed stores — the unprotected-DATA erratum ops.
pub fn is_indexed_store(op: u8) -> bool {
    matches!(op, 49 | 50 | 60 | 61)
}

/// Memory class of a data address, for latency purposes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MemClass {
    /// This core's local SRAM or its own control registers.
    Internal,
    /// Main DRAM.
    Dram,
    /// Everything else external (cart ROM, the other chip's registers…).
    ExtOther,
}

pub fn mem_class(addr: u32, sram_base: u32, sram_size: u32, ctrl_base: u32) -> MemClass {
    if (sram_base..sram_base + sram_size).contains(&addr)
        || (ctrl_base..ctrl_base + 0x200).contains(&addr)
    {
        MemClass::Internal
    } else if addr < crate::mem::DRAM_END {
        MemClass::Dram
    } else {
        MemClass::ExtOther
    }
}
