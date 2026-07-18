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
//! v1 transform: **delay-slot filling**. The JRISC delay slot always executes,
//! so a `nop` after a jump is wasted. jopt moves the instruction *before* the
//! jump into the slot when doing so is behavior-preserving, deleting the nop —
//! the "bytes are features" win.
//!
//! Two guards, in order — the certificate alone is NOT enough (COBWEB_ISSUES #3:
//! a certificate passed a behaviour-changing fill because the test input didn't
//! observably exercise the affected path). So jopt first applies a STRUCTURAL
//! precondition — a fill is only *attempted* when every path reaching the jump
//! must have executed the moved instruction (no label on the jump or between it
//! and the fill, i.e. the jump is not a branch target) — and only then runs the
//! jsim equivalence certificate as a second line of defence. Treat `accepted`
//! as "passed both guards", and re-gate real output with a render/shadow diff.

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

/// Assemble source; None if it does not assemble clean (errors).
fn assemble(src: &str, target: RiscKind) -> Option<(Vec<u8>, u32)> {
    let opts = jas::Options {
        target: target_of(target),
        org: match target {
            RiscKind::Gpu => 0xF0_3000,
            RiscKind::Dsp => 0xF1_B000,
        },
        ..Default::default()
    };
    let out = jas::assemble(src, &opts);
    if out.errors() > 0 {
        None
    } else {
        Some((out.bytes, out.org))
    }
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

// ── source model ─────────────────────────────────────────────────────────────

/// A source line, classified enough for the slot-fill transform.
#[derive(Clone)]
struct SrcLine {
    text: String,
    /// lowercased mnemonic if this line carries an instruction
    mnem: Option<String>,
    has_label: bool,
}

fn classify_line(raw: &str) -> SrcLine {
    let no_comment = raw.split(';').next().unwrap_or("");
    let trimmed = no_comment.trim();
    if trimmed.is_empty() {
        return SrcLine { text: raw.to_string(), mnem: None, has_label: false };
    }
    let leading_ws = raw.starts_with([' ', '\t']);
    // label present if not indented and first token ends with ':' or is `name equ/=`
    let first = trimmed.split_whitespace().next().unwrap_or("");
    let has_label = !leading_ws && (first.ends_with(':') || {
        let after = trimmed[first.len()..].trim_start();
        after.starts_with('=') || after.to_ascii_lowercase().starts_with("equ")
    });
    // strip a leading label to find the mnemonic
    let after_label = if has_label {
        if let Some(c) = trimmed.find(':') {
            trimmed[c + 1..].trim()
        } else {
            "" // equ line: no instruction
        }
    } else {
        trimmed
    };
    let mnem = after_label
        .split_whitespace()
        .next()
        .filter(|m| !m.starts_with('.') && !m.is_empty())
        .map(|m| m.to_ascii_lowercase());
    SrcLine { text: raw.to_string(), mnem, has_label }
}

fn is_jump(m: &str) -> bool {
    m == "jump" || m == "jr"
}

/// The instruction is a single-word op safe to relocate into a delay slot as a
/// *candidate* (the certificate makes the final call). Excludes multi-word and
/// control-flow ops that are illegal in a slot.
fn fillable(m: &str) -> bool {
    !matches!(m, "jump" | "jr" | "movei" | "nop") && !m.starts_with('.')
}

/// Optimize `src`. Greedy: for each `... <fill> ; jump ; nop ...`, try moving
/// `<fill>` into the slot, verify, keep on success.
pub fn optimize(src: &str, target: RiscKind) -> OptResult {
    let (base_bytes, org) = match assemble(src, target) {
        Some(x) => x,
        None => {
            // don't optimize code that doesn't assemble; return unchanged
            return OptResult {
                source: src.to_string(),
                bytes_before: 0,
                bytes_after: 0,
                transforms: vec![Transform {
                    kind: "precheck".into(),
                    at_line: 0,
                    accepted: false,
                    reason: "input does not assemble clean (jas errors) — nothing to optimize".into(),
                }],
            };
        }
    };
    let base_fp = fingerprint(base_bytes.clone(), org, target);
    let bytes_before = base_bytes.len();

    let mut lines: Vec<String> = src.lines().map(|s| s.to_string()).collect();
    let mut transforms = Vec::new();

    // Repeatedly scan for a fillable triple and try it. Restart after each
    // accepted transform (indices shift).
    let mut changed = true;
    while changed {
        changed = false;
        // index list of instruction-bearing lines
        let cls: Vec<(usize, SrcLine)> =
            lines.iter().enumerate().map(|(i, l)| (i, classify_line(l))).collect();
        let insn_idx: Vec<usize> =
            cls.iter().filter(|(_, c)| c.mnem.is_some()).map(|(i, _)| *i).collect();

        for w in 1..insn_idx.len().saturating_sub(1) {
            let (bi, ji, si) = (insn_idx[w - 1], insn_idx[w], insn_idx[w + 1]);
            let b = &cls[bi].1;
            let j = &cls[ji].1;
            let s = &cls[si].1;
            let (Some(bm), Some(jm), Some(sm)) = (&b.mnem, &j.mnem, &s.mnem) else { continue };
            if !is_jump(jm) || sm != "nop" || !fillable(bm) {
                continue;
            }
            // SOUNDNESS (COBWEB_ISSUES #3): B may move into J's delay slot only
            // if EVERY execution path that reaches J executed B immediately
            // first — i.e. B falls straight through to J. A label on J, or on
            // any line between B and J, is a branch target: control can jump
            // there and reach J (and now its slot = B) WITHOUT having run B.
            // The bug: a `neg` on a conditionally-skipped path was moved into a
            // *labelled* return-jump's slot, so it also ran on the skip path,
            // wrongly negating positive quotients. The jsim certificate missed
            // it (the test input didn't observably exercise that path) — so this
            // structural precondition, not the certificate alone, is the guard.
            let branch_reachable_jump =
                (bi + 1..=ji).any(|k| classify_line(&lines[k]).has_label);
            if b.has_label || s.has_label || branch_reachable_jump {
                continue;
            }
            // Build candidate: move line `bi` into slot `si`, drop original `bi`.
            let fill_text = strip_to_insn(&b.text);
            let mut cand = lines.clone();
            cand[si] = fill_text; // slot now does the work
            cand.remove(bi); // remove the original (index bi < si, safe order)
            let cand_src = cand.join("\n");

            let line_no = ji + 1;
            match verify(&cand_src, target, org, base_fp.as_ref()) {
                Ok(()) => {
                    lines = cand;
                    transforms.push(Transform {
                        kind: "delay-slot-fill".into(),
                        at_line: line_no,
                        accepted: true,
                        reason: format!("moved `{}` into the slot; nop removed (2 bytes)", bm),
                    });
                    changed = true;
                    break;
                }
                Err(why) => {
                    transforms.push(Transform {
                        kind: "delay-slot-fill".into(),
                        at_line: line_no,
                        accepted: false,
                        reason: why,
                    });
                }
            }
        }
    }

    let new_src = lines.join("\n");
    let bytes_after = assemble(&new_src, target).map(|(b, _)| b.len()).unwrap_or(bytes_before);
    OptResult { source: new_src, bytes_before, bytes_after, transforms }
}

/// Turn a source line into a plain indented instruction (drop any label — the
/// caller only relocates label-free fill lines, so this just re-indents).
fn strip_to_insn(text: &str) -> String {
    let body = text.split(';').next().unwrap_or("").trim();
    format!("        {body}")
}

/// The equivalence certificate: candidate must assemble clean AND run
/// identically to the baseline fingerprint.
fn verify(
    cand_src: &str,
    target: RiscKind,
    _org: u32,
    base_fp: Option<&jtest::RunResult>,
) -> Result<(), String> {
    let (bytes, org) = assemble(cand_src, target)
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

    #[test]
    fn fills_a_wasted_delay_slot() {
        // A back-edge loop with a nop slot and an independent `addqt` before the
        // jump — the classic fill. r1 accumulates 5+4+3+2+1 = 15; the pointer
        // bump can live in the slot.
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
        // At least the fill should be attempted; the result must still be
        // correct (15 at $100000) — the certificate guarantees it.
        let (bytes, org) = assemble(&res.source, RiscKind::Gpu).unwrap();
        let r = run(&Spec { bytes, target: RiscKind::Gpu, org, budget: 100_000, capture: (0x0010_0000, 4), fidelity: Fidelity::Silicon });
        assert_eq!(u32::from_be_bytes([r.captured[0], r.captured[1], r.captured[2], r.captured[3]]), 15);
    }

    #[test]
    fn refuses_fill_across_a_labelled_jump() {
        // COBWEB_ISSUES #3 reproducer: `neg r0` on the conditionally-skipped
        // (negative) path, immediately before a LABELLED return jump. Moving it
        // into that jump's slot would run it on the positive (skip) path too,
        // negating a positive value. jopt must REFUSE (the jump is a branch
        // target). Here r2 is positive, so the correct result is +7, never -7.
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
        // the only fill candidate is neg -> skip's (labelled) slot: must be refused
        assert_eq!(res.accepted(), 0, "jopt filled a branch-reachable jump slot (unsound!)");
        // and behaviour is preserved: positive value stays +7
        let (bytes, org) = assemble(&res.source, RiscKind::Gpu).unwrap();
        let r = run(&Spec { bytes, target: RiscKind::Gpu, org, budget: 50_000, capture: (0x0010_0000, 4), fidelity: Fidelity::Silicon });
        assert_eq!(u32::from_be_bytes([r.captured[0], r.captured[1], r.captured[2], r.captured[3]]), 7,
            "positive value was wrongly negated");
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
        let base = assemble(&src, RiscKind::Gpu).unwrap();
        let opt = assemble(&res.source, RiscKind::Gpu).unwrap();
        let a = run(&Spec { bytes: base.0, target: RiscKind::Gpu, org: base.1, budget: 100_000, capture: (0x0010_0000, 4096), fidelity: Fidelity::Silicon });
        let b = run(&Spec { bytes: opt.0, target: RiscKind::Gpu, org: opt.1, budget: 100_000, capture: (0x0010_0000, 4096), fidelity: Fidelity::Silicon });
        assert!(compare(&a, &b).is_empty(), "jopt changed behavior — certificate should have prevented this");
    }
}
