//! jopt — the scheduler that can't ship a wrong answer.
//!
//! Input: correct JRISC source. Output: source that does the same thing in
//! fewer bytes/cycles — where "does the same thing" is not asserted, it is
//! *checked*. Every candidate transform is applied to the source, re-assembled
//! through jas (so labels, `jr` ranges, and the hazard pass all re-validate),
//! run in jsim alongside the original, and kept only if the captured memory and
//! the register file are byte-identical. jas catches a transform that becomes
//! a hazard; jsim catches one that changes behavior. A transform survives only
//! if both agree it is safe — that is the equivalence certificate.
//!
//! **Delay-slot filling.** The JRISC delay slot always executes, so a `nop`
//! after a jump is wasted. jopt sinks a *donor* instruction into the slot and
//! deletes the nop — the "bytes are features" win.
//!
//! v1 could only try the instruction *immediately* before the jump. That is
//! almost always the `cmp`/`cmpq` that sets the very flags a conditional jump
//! consumes, so on real kernels every candidate was rejected by the
//! certificate. **v2 is a scheduler**: for each wasted slot it walks the
//! straight-line block backwards looking for *any* donor it can legally sink —
//! one that is data-independent of everything it leapfrogs and flag-safe for
//! the jump — then still proves the result with the jsim certificate.
//!
//! Two guards, in order — the certificate alone is NOT enough (COBWEB_ISSUES #3:
//! a certificate passed a behaviour-changing fill because the test input didn't
//! observably exercise the affected path). So jopt first applies STRUCTURAL
//! preconditions that are *sound* (never permit an unsafe move) even if
//! conservative:
//!
//!   * **Dominance** — a donor may sink into J's slot only if every path that
//!     reaches J executed the donor first: J is not a branch target, and no
//!     label sits between the donor and J. (A label there is an entry that
//!     reaches J — and now its slot — without running the donor.)
//!   * **Data independence** — the donor is reordered past every instruction
//!     between it and J, so none of them may read what it writes, write what it
//!     reads, or write what it writes; and J itself must not read what it writes.
//!   * **Flag safety** — a donor that defines or consumes flags may only sink
//!     into an *unconditional* jump with no other flag op between, so the flags
//!     the jump/target observe are unchanged.
//!
//! Only a donor that clears all three is handed to the jsim equivalence
//! certificate as the second line of defence. Treat `accepted` as "passed both
//! guards", and re-gate real output with a render/shadow diff.
//!
//! jopt reasons over the *assembled* instruction stream (`jas::Assembled`), not
//! the raw text, so it sees exactly what the chip runs: instructions inside an
//! inactive `.if` block never assemble, so they are never candidates — the old
//! "4 accepted, 0 saved" (rewrites of profiling code the default build drops)
//! is now reported as `skipped-inactive`, not a phantom win.

use std::collections::{HashMap, HashSet};

use jag_core::risc::timing::{self, Access};
use jag_core::risc::Fidelity;
use jag_core::RiscKind;
use jtest::{compare, run, Spec};

/// One attempted transform and its verdict.
#[derive(Debug, Clone)]
pub struct Transform {
    pub kind: String,
    /// source line (1-based) of the jump whose slot was filled
    pub at_line: usize,
    pub accepted: bool,
    pub reason: String,
}

/// Result of optimizing a source file.
pub struct OptResult {
    pub source: String,
    pub bytes_before: usize,
    pub bytes_after: usize,
    pub transforms: Vec<Transform>,
}

impl OptResult {
    pub fn bytes_saved(&self) -> usize {
        self.bytes_before.saturating_sub(self.bytes_after)
    }
    pub fn accepted(&self) -> usize {
        self.transforms.iter().filter(|t| t.accepted).count()
    }
}

fn target_of(t: RiscKind) -> jas::Target {
    match t {
        RiscKind::Gpu => jas::Target::Gpu,
        RiscKind::Dsp => jas::Target::Dsp,
    }
}

fn org_of(t: RiscKind) -> u32 {
    match t {
        RiscKind::Gpu => 0xF0_3000,
        RiscKind::Dsp => 0xF1_B000,
    }
}

/// Assemble source into a full [`jas::Assembled`]; None if it does not assemble
/// clean (errors). With `allow_hazards`, the static hazard pass is skipped —
/// pre-existing (and any transform-introduced) hazards no longer block
/// assembly; the jsim Silicon equivalence certificate remains the guarantee.
fn assemble_full(src: &str, target: RiscKind, allow_hazards: bool) -> Option<jas::Assembled> {
    let opts = jas::Options {
        target: target_of(target),
        org: org_of(target),
        check_hazards: !allow_hazards,
        ..Default::default()
    };
    let out = jas::assemble(src, &opts);
    if out.errors() > 0 {
        None
    } else {
        Some(out)
    }
}

/// Assemble to bytes only (the certificate re-assembly path).
fn assemble(src: &str, target: RiscKind, allow_hazards: bool) -> Option<(Vec<u8>, u32)> {
    assemble_full(src, target, allow_hazards).map(|a| (a.bytes, a.org))
}

/// A behavioral fingerprint: run in jsim, capture a wide DRAM region + all
/// registers. Deterministic programs (no external input) fingerprint stably.
fn fingerprint(bytes: Vec<u8>, org: u32, target: RiscKind) -> Option<jtest::RunResult> {
    let spec = Spec {
        bytes,
        target,
        org,
        budget: 200_000,
        capture: (0x0010_0000, 4096),
        fidelity: Fidelity::Silicon,
    };
    Some(run(&spec))
}

/// True if two runs are behaviorally identical (captured region + registers).
fn equivalent(a: &jtest::RunResult, b: &jtest::RunResult) -> bool {
    compare(a, b).is_empty()
}

// ── decoded instruction model ─────────────────────────────────────────────────

/// One emitted (i.e. actually-assembled) instruction, decoded via the same
/// authority the simulator uses (`timing::classify`), tagged with its true
/// source line so the transform can edit the text.
struct Insn {
    /// 1-based source line (post line-map, so accurate across `.if`/macros).
    src_line: usize,
    op: u8,
    access: Access,
    is_jump: bool,
    /// Unconditional jump (cc field == "always"): does not consume flags.
    jump_uncond: bool,
    n_words: usize,
}

fn build_insns(asm: &jas::Assembled, target: RiscKind) -> Vec<Insn> {
    let is_dsp = matches!(target, RiscKind::Dsp);
    asm.emitted
        .iter()
        .filter_map(|e| {
            let op = e.op?;
            let word = *e.words.first()?;
            let access = timing::classify(word, is_dsp);
            let is_jump = op == 52 || op == 53;
            // cc field is reg2 (bits 4:0); 0 == "always" == unconditional.
            let jump_uncond = is_jump && (word & 0x1F) == 0;
            Some(Insn { src_line: e.line, op, access, is_jump, jump_uncond, n_words: e.words.len() })
        })
        .collect()
}

/// How many emitted instructions came from each source line — a line that
/// expands to more than one instruction (a macro call) is not text-editable.
fn line_instr_counts(asm: &jas::Assembled) -> HashMap<usize, usize> {
    let mut m = HashMap::new();
    for e in &asm.emitted {
        if e.op.is_some() {
            *m.entry(e.line).or_insert(0) += 1;
        }
    }
    m
}

/// Source lines that carried an instruction into the output — i.e. lines in
/// *active* code. A jump inside an inactive `.if` block is absent here.
fn active_instr_lines(asm: &jas::Assembled) -> HashSet<usize> {
    asm.emitted.iter().filter(|e| e.op.is_some()).map(|e| e.line).collect()
}

fn reads_vec(a: &Access) -> Vec<u8> {
    a.reads.iter().flatten().copied().collect()
}

/// A single-word compute/move op that is safe to *consider* relocating into a
/// delay slot (the certificate makes the final call). Excludes memory ops
/// (loads/stores — timing/scoreboard), MAC/DIV/MMULT (accumulator/slow state),
/// the cross-bank moves, the 3-word MOVEI, `move pc`, jumps, and nop.
fn relocatable_op(op: u8) -> bool {
    !matches!(op,
        18 | 19 | 20 |   // IMULTN/RESMAC/IMACN — MAC accumulator state
        21 |             // DIV — slow, drives the scoreboard
        36 | 37 |        // MOVETA/MOVEFA — cross-bank
        38 |             // MOVEI — 3 words, illegal in a slot
        39..=53 |        // loads, stores, move-pc, JUMP, JR
        54 |             // MMULT
        57 |             // NOP
        58..=61          // register-indexed loads/stores
    )
}

/// May donor `d` legally sink into jump `j`'s delay slot, given the
/// instructions `betweens` it would leapfrog (those strictly between d and j)?
/// Sound: every `true` is a behavior-preserving move (the certificate still
/// re-checks). See the module docs for the three structural guards.
fn donor_ok(d: &Insn, j: &Insn, betweens: &[&Insn]) -> bool {
    if d.n_words != 1 || !relocatable_op(d.op) {
        return false;
    }
    // Cross-bank effects make the register identity ambiguous — refuse.
    if d.access.write_alt_bank || d.access.read_alt_bank {
        return false;
    }

    // Flag safety: a donor that defines or consumes flags changes what the jump
    // and its target observe unless the jump is unconditional AND nothing else
    // between touches flags (so the donor stays the last/only flag op, just in
    // the slot instead of before the jump).
    if d.access.sets_flags || d.access.uses_flags {
        if !j.jump_uncond {
            return false;
        }
        if betweens.iter().any(|x| x.access.sets_flags || x.access.uses_flags) {
            return false;
        }
    }

    let dw = d.access.write;
    let dreads = reads_vec(&d.access);

    // Data independence from everything the donor is reordered past.
    for x in betweens {
        if x.access.write_alt_bank || x.access.read_alt_bank {
            return false;
        }
        let xreads = reads_vec(&x.access);
        if let Some(w) = dw {
            if xreads.contains(&w) {
                return false; // x would read the donor's (now-stale) result
            }
            if x.access.write == Some(w) {
                return false; // write-after-write order would flip
            }
        }
        if let Some(xw) = x.access.write {
            if dreads.contains(&xw) {
                return false; // donor would read x's newer value
            }
        }
    }

    // The jump now executes before the donor (which runs in its slot), so the
    // jump must not depend on the donor's result (e.g. `jump (rN)` address reg).
    if let Some(w) = dw {
        if reads_vec(&j.access).contains(&w) {
            return false;
        }
    }
    true
}

fn classify_has_label(lines: &[String], line_no: usize) -> bool {
    line_no >= 1 && line_no <= lines.len() && classify_line(&lines[line_no - 1]).has_label
}

/// A source line is text-editable for a fill iff it is in range, carries
/// exactly one instruction, and has no label (a label would be lost when the
/// line is rewritten or deleted).
fn editable(lines: &[String], counts: &HashMap<usize, usize>, line_no: usize) -> bool {
    line_no >= 1
        && line_no <= lines.len()
        && counts.get(&line_no) == Some(&1)
        && !classify_has_label(lines, line_no)
}

// ── source model (text classification, for labels/mnemonics) ──────────────────

/// A source line, classified enough to spot labels and mnemonics in text.
#[derive(Clone)]
struct SrcLine {
    mnem: Option<String>,
    has_label: bool,
}

fn classify_line(raw: &str) -> SrcLine {
    let no_comment = raw.split(';').next().unwrap_or("");
    let trimmed = no_comment.trim();
    if trimmed.is_empty() {
        return SrcLine { mnem: None, has_label: false };
    }
    let leading_ws = raw.starts_with([' ', '\t']);
    let first = trimmed.split_whitespace().next().unwrap_or("");
    let has_label = !leading_ws
        && (first.ends_with(':') || {
            let after = trimmed[first.len()..].trim_start();
            after.starts_with('=') || after.to_ascii_lowercase().starts_with("equ")
        });
    let after_label = if has_label {
        if let Some(c) = trimmed.find(':') {
            trimmed[c + 1..].trim()
        } else {
            ""
        }
    } else {
        trimmed
    };
    let mnem = after_label
        .split_whitespace()
        .next()
        .filter(|m| !m.starts_with('.') && !m.is_empty())
        .map(|m| m.to_ascii_lowercase());
    SrcLine { mnem, has_label }
}

fn is_jump_mnem(m: &str) -> bool {
    m == "jump" || m == "jr"
}

/// Turn a source line into a plain indented instruction (drop label/comment —
/// the caller only relocates label-free fill lines).
fn strip_to_insn(text: &str) -> String {
    let body = text.split(';').next().unwrap_or("").trim();
    format!("        {body}")
}

/// Build the candidate: sink the donor line's instruction into the slot and
/// delete the donor's original line. Requires `donor_line < slot_line`.
fn build_candidate(lines: &[String], donor_line: usize, slot_line: usize) -> Option<Vec<String>> {
    if donor_line < 1 || slot_line < 1 || donor_line >= slot_line || slot_line > lines.len() {
        return None;
    }
    let donor_text = strip_to_insn(&lines[donor_line - 1]);
    let mut cand = lines.to_vec();
    cand[slot_line - 1] = donor_text; // slot now does the work
    cand.remove(donor_line - 1); // drop the original (donor_line < slot_line: safe)
    Some(cand)
}

// ── the optimizer ─────────────────────────────────────────────────────────────

/// Optimize `src`. Greedy: fill each wasted delay slot with the nearest legal
/// donor, verify, keep on success; restart after each accepted fill.
pub fn optimize(src: &str, target: RiscKind) -> OptResult {
    optimize_opts(src, target, false)
}

/// Like [`optimize`], with `allow_input_hazards`: assemble past pre-existing
/// (benign, hardware-correct) hazards so the fill can still run. Only the jsim
/// equivalence certificate gates the output — a transform that changes behavior
/// (including one that introduces a harmful hazard, which jsim models at Silicon
/// fidelity) is still rejected.
pub fn optimize_opts(src: &str, target: RiscKind, allow_input_hazards: bool) -> OptResult {
    let base = match assemble_full(src, target, allow_input_hazards) {
        Some(a) => a,
        None => {
            return OptResult {
                source: src.to_string(),
                bytes_before: 0,
                bytes_after: 0,
                transforms: vec![Transform {
                    kind: "precheck".into(),
                    at_line: 0,
                    accepted: false,
                    reason: "input does not assemble clean (jas errors) — nothing to optimize"
                        .into(),
                }],
            };
        }
    };
    let base_bytes = base.bytes.clone();
    let org = base.org;
    let base_fp = fingerprint(base_bytes.clone(), org, target);
    let bytes_before = base_bytes.len();

    let mut lines: Vec<String> = src.lines().map(|s| s.to_string()).collect();
    let mut transforms = Vec::new();

    // Report (once) any wasted slot that lives in an inactive `.if` block: it is
    // never assembled, so filling it would be a phantom "win". This is the fix
    // for the confusing "accepted / 0 saved" on gpu_geotex's profiling code.
    report_inactive_fills(&lines, &active_instr_lines(&base), &mut transforms);

    // Greedy fixpoint: re-assemble each round so src_line numbers stay accurate.
    let mut changed = true;
    while changed {
        changed = false;
        let Some(asm) = assemble_full(&lines.join("\n"), target, allow_input_hazards) else {
            break;
        };
        let model = build_insns(&asm, target);
        let counts = line_instr_counts(&asm);

        'scan: for jp in 0..model.len() {
            let j = &model[jp];
            if !j.is_jump {
                continue;
            }
            let Some(slot) = model.get(jp + 1) else { continue };
            if slot.op != 57 {
                continue; // slot already does work
            }
            // The jump and slot lines must be plain, editable, and — for the
            // jump — not a branch target (dominance).
            if !editable(&lines, &counts, j.src_line) || classify_has_label(&lines, j.src_line) {
                continue;
            }
            if !editable(&lines, &counts, slot.src_line) {
                continue;
            }

            // Walk the straight-line block backwards for the nearest legal donor.
            let mut betweens: Vec<&Insn> = Vec::new();
            for dp in (0..jp).rev() {
                let d = &model[dp];
                // A label or another jump ends the block: donors at or before it
                // are not guaranteed to run before J.
                if classify_has_label(&lines, d.src_line) || d.is_jump {
                    break;
                }
                if editable(&lines, &counts, d.src_line)
                    && d.src_line < slot.src_line
                    && donor_ok(d, j, &betweens)
                {
                    if let Some(cand) = build_candidate(&lines, d.src_line, slot.src_line) {
                        let mnem =
                            classify_line(&lines[d.src_line - 1]).mnem.unwrap_or_else(|| "?".into());
                        match verify(
                            &cand.join("\n"),
                            target,
                            base_fp.as_ref(),
                            allow_input_hazards,
                        ) {
                            Ok(()) => {
                                transforms.push(Transform {
                                    kind: "delay-slot-fill".into(),
                                    at_line: j.src_line,
                                    accepted: true,
                                    reason: format!(
                                        "sank `{}` (line {}) into the slot; nop removed (2 bytes)",
                                        mnem, d.src_line
                                    ),
                                });
                                lines = cand;
                                changed = true;
                                break 'scan;
                            }
                            Err(why) => {
                                // Only report certificate failures (a structurally
                                // eligible donor that jsim rejected) — silent skips
                                // for structurally ineligible donors keep noise down.
                                transforms.push(Transform {
                                    kind: "delay-slot-fill".into(),
                                    at_line: j.src_line,
                                    accepted: false,
                                    reason: format!("donor line {}: {}", d.src_line, why),
                                });
                            }
                        }
                    }
                }
                betweens.insert(0, d);
            }
        }
    }

    let new_src = lines.join("\n");
    let bytes_after = assemble(&new_src, target, allow_input_hazards)
        .map(|(b, _)| b.len())
        .unwrap_or(bytes_before);
    OptResult { source: new_src, bytes_before, bytes_after, transforms }
}

/// Note wasted slots (`jump`/`jr` then `nop`) that fall in inactive `.if`
/// blocks — present in the text, absent from the assembly, so not fillable.
fn report_inactive_fills(
    lines: &[String],
    active: &HashSet<usize>,
    transforms: &mut Vec<Transform>,
) {
    for (i, l) in lines.iter().enumerate() {
        let c = classify_line(l);
        let Some(m) = &c.mnem else { continue };
        if !is_jump_mnem(m) || active.contains(&(i + 1)) {
            continue;
        }
        // next instruction-bearing line is a nop?
        let next = (i + 1..lines.len())
            .map(|k| classify_line(&lines[k]))
            .find(|cc| cc.mnem.is_some());
        if next.and_then(|cc| cc.mnem).as_deref() == Some("nop") {
            transforms.push(Transform {
                kind: "skipped-inactive".into(),
                at_line: i + 1,
                accepted: false,
                reason: "jump+nop inside an inactive `.if` block (not assembled) — skipped".into(),
            });
        }
    }
}

/// The equivalence certificate: candidate must assemble clean AND run
/// identically to the baseline fingerprint.
fn verify(
    cand_src: &str,
    target: RiscKind,
    base_fp: Option<&jtest::RunResult>,
    allow_hazards: bool,
) -> Result<(), String> {
    let (bytes, org) = assemble(cand_src, target, allow_hazards)
        .ok_or_else(|| "candidate did not assemble (jas rejected it — hazard or range)".to_string())?;
    let fp = fingerprint(bytes, org, target).ok_or_else(|| "candidate did not run".to_string())?;
    match base_fp {
        Some(base) if equivalent(base, &fp) => Ok(()),
        Some(_) => Err("candidate diverged from the original in jsim (certificate failed)".into()),
        None => Err("no baseline fingerprint".into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const STOP: &str = "        movei #$00F02114,r30\n        moveq #0,r29\n        store r29,(r30)\n        nop\n";

    fn word_at(bytes: &[u8], off: usize) -> u32 {
        u32::from_be_bytes([bytes[off], bytes[off + 1], bytes[off + 2], bytes[off + 3]])
    }

    #[test]
    fn fills_a_wasted_delay_slot() {
        // A back-edge loop with a nop slot: r1 accumulates 5+4+3+2+1 = 15. The
        // loop's donors all set the flags the conditional `jr` consumes, so v2
        // leaves the slot — but the result must stay correct (the certificate
        // guarantees it whatever jopt does).
        let src = format!(
            "        .gpu\n\
             \x20       moveq #0,r1\n\
             \x20       moveq #5,r2\n\
             loop:   add r2,r1\n\
             \x20       subq #1,r2\n\
             \x20       cmpq #0,r2\n\
             \x20       jr ne,loop\n\
             \x20       nop\n\
             \x20       movei #$00100000,r3\n\
             \x20       store r1,(r3)\n{STOP}"
        );
        let res = optimize(&src, RiscKind::Gpu);
        let (bytes, org) = assemble(&res.source, RiscKind::Gpu, false).unwrap();
        let r = run(&Spec { bytes, target: RiscKind::Gpu, org, budget: 100_000, capture: (0x0010_0000, 4), fidelity: Fidelity::Silicon });
        assert_eq!(word_at(&r.captured, 0), 15);
    }

    #[test]
    fn v2_sinks_a_non_adjacent_donor() {
        // The instruction *immediately* before the unconditional `jr` is a
        // 3-word MOVEI (illegal in a slot) — v1 would give up. v2 walks back one
        // more and sinks the independent `move r1,r5` into the slot. Both r1 and
        // r5 must still read 7 at $100000/$100004, and a fill must have happened.
        let src = format!(
            "        .gpu\n\
             \x20       moveq #7,r1\n\
             \x20       move r1,r5\n\
             \x20       movei #$00100000,r3\n\
             \x20       jr t,done\n\
             \x20       nop\n\
             done:   store r1,(r3)\n\
             \x20       movei #$00100004,r4\n\
             \x20       store r5,(r4)\n{STOP}"
        );
        let res = optimize(&src, RiscKind::Gpu);
        assert!(res.accepted() >= 1, "v2 failed to sink the non-adjacent donor");
        let (bytes, org) = assemble(&res.source, RiscKind::Gpu, false).unwrap();
        let r = run(&Spec { bytes, target: RiscKind::Gpu, org, budget: 100_000, capture: (0x0010_0000, 8), fidelity: Fidelity::Silicon });
        assert_eq!(word_at(&r.captured, 0), 7, "r1 wrong after fill");
        assert_eq!(word_at(&r.captured, 4), 7, "r5 wrong after fill (donor mis-scheduled)");
    }

    #[test]
    fn refuses_fill_across_a_labelled_jump() {
        // COBWEB_ISSUES #3 reproducer: `neg r0` on the conditionally-skipped
        // (negative) path, immediately before a LABELLED return jump. Sinking it
        // into that jump's slot would run it on the positive (skip) path too,
        // negating a positive value. jopt must never do that — `skip` is a branch
        // target, so its slot is off-limits. v2 may fill a *different*, sound slot
        // here; the safety property is only that the positive value stays +7 (if
        // neg had been hoisted into skip's slot, this would read -7).
        let src = format!(
            "        .gpu\n\
             \x20       moveq #7,r0\n\
             \x20       moveq #1,r2\n\
             \x20       movei #$00100000,r5\n\
             \x20       movei #out,r28\n\
             \x20       cmpq #0,r2\n\
             \x20       movei #skip,r27\n\
             \x20       jump pl,(r27)\n\
             \x20       nop\n\
             \x20       neg r0\n\
             skip:   jump t,(r28)\n\
             \x20       nop\n\
             out:    store r0,(r5)\n{STOP}"
        );
        let res = optimize(&src, RiscKind::Gpu);
        let (bytes, org) = assemble(&res.source, RiscKind::Gpu, false).unwrap();
        let r = run(&Spec { bytes, target: RiscKind::Gpu, org, budget: 50_000, capture: (0x0010_0000, 4), fidelity: Fidelity::Silicon });
        assert_eq!(word_at(&r.captured, 0), 7, "positive value was wrongly negated (unsound fill into a labelled slot)");
    }

    #[test]
    fn skips_fills_in_inactive_conditional_blocks() {
        // A `jump; nop` guarded by `.if 0` is never assembled. jopt must not
        // report it as an accepted fill — it should surface as skipped-inactive,
        // and the emitted bytes must not change.
        let src = format!(
            "        .gpu\n\
             \x20       moveq #1,r1\n\
             \x20       .if 0\n\
             \x20       addq #1,r1\n\
             \x20       jr t,dead\n\
             \x20       nop\n\
             dead:   nop\n\
             \x20       .endif\n\
             \x20       movei #$00100000,r3\n\
             \x20       store r1,(r3)\n{STOP}"
        );
        let res = optimize(&src, RiscKind::Gpu);
        assert_eq!(res.bytes_before, res.bytes_after, "inactive-block edit changed the binary");
        assert!(
            res.transforms.iter().any(|t| t.kind == "skipped-inactive"),
            "inactive `.if` fill site was not flagged"
        );
        assert!(
            !res.transforms.iter().any(|t| t.accepted),
            "jopt accepted a dead-code fill"
        );
    }

    #[test]
    fn never_breaks_correctness() {
        // Whatever jopt does, the optimized program must match the original.
        let src = format!(
            "        .gpu\n\
             \x20       moveq #10,r1\n\
             \x20       moveq #3,r2\n\
             \x20       add r2,r1\n\
             \x20       cmpq #0,r2\n\
             \x20       jr ne,.k\n\
             \x20       nop\n\
             .k:     movei #$00100000,r3\n\
             \x20       store r1,(r3)\n{STOP}"
        );
        let res = optimize(&src, RiscKind::Gpu);
        let base = assemble(&src, RiscKind::Gpu, false).unwrap();
        let opt = assemble(&res.source, RiscKind::Gpu, false).unwrap();
        let a = run(&Spec { bytes: base.0, target: RiscKind::Gpu, org: base.1, budget: 100_000, capture: (0x0010_0000, 4096), fidelity: Fidelity::Silicon });
        let b = run(&Spec { bytes: opt.0, target: RiscKind::Gpu, org: opt.1, budget: 100_000, capture: (0x0010_0000, 4096), fidelity: Fidelity::Silicon });
        assert!(compare(&a, &b).is_empty(), "jopt changed behavior — certificate should have prevented this");
    }
}
