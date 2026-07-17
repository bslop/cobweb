//! The hazard pass — jas's reason to exist.
//!
//! Every other Jaguar assembler is a faithful translator: it emits exactly the
//! bytes you wrote, silicon traps and all. jas models the scoreboard over the
//! decoded instruction stream and reports, as errors with fix-its:
//!
//! * **Write-after-write into a load/divide shadow** (TRM bug 13): a register
//!   still pending from a slow producer, overwritten before any read, lands out
//!   of order. The canonical guard is a dependent read (`or rN,rN`).
//! * **Indexed store of an unsettled register** (TRM errata §2): ops 49/50/60/61
//!   don't scoreboard their DATA register, so storing a just-loaded/divided
//!   value writes the stale one.
//! * **A JUMP/JR or MOVEI in a delay slot** ("results are not predictable";
//!   a 3-word instruction in the single slot is unverified on hardware).
//! * **nop+nop after a jump** (warning): the old two-NOP convention wasting a
//!   real delay slot.
//!
//! The model is linear (one pass, straight-line) — it does not follow branches,
//! so it catches the local hazards that dominate hand and compiled JRISC. Its
//! rules are the same ones jsim's timing model enforces, so the assembler and
//! the simulator agree on what is dangerous.

use crate::{Diag, Emitted};
use std::collections::HashMap;

/// Decoded register behavior of one instruction (bank-agnostic — raw register
/// numbers; conservative for the two cross-bank moves).
struct Acc {
    reads: Vec<u8>,
    write: Option<u8>,
    /// This instruction's result lands late (DIV or a load) — a WAW/erratum source.
    slow: bool,
    /// If this is an indexed store, the DATA register it does NOT scoreboard.
    indexed_store_data: Option<u8>,
    op: u8,
}

fn classify(e: &Emitted) -> Option<Acc> {
    let op = e.op?;
    let w = *e.words.first()?;
    let r1 = ((w >> 5) & 0x1F) as u8;
    let r2 = (w & 0x1F) as u8;
    let mut reads = Vec::new();
    let mut write = None;
    let mut slow = false;
    let mut indexed_store_data = None;
    let dsp = e.target == crate::Target::Dsp;

    match op {
        // two-operand ALU: read r1,r2; write r2
        0 | 1 | 4 | 5 | 9 | 10 | 11 | 16 | 17 | 23 | 26 | 28 => {
            reads.push(r1);
            reads.push(r2);
            write = Some(r2);
        }
        // quick ALU / shifts: read r2; write r2 (r1 = immediate)
        2 | 3 | 6 | 7 | 14 | 15 | 24 | 25 | 27 | 29 => {
            reads.push(r2);
            write = Some(r2);
        }
        // neg/not/abs
        8 | 12 | 22 => {
            reads.push(r2);
            write = Some(r2);
        }
        13 => reads.push(r2),               // btst
        18 => {
            reads.push(r1);
            reads.push(r2);
        } // imultn (MAC, no reg write)
        19 => write = Some(r2),             // resmac
        20 => {
            reads.push(r1);
            reads.push(r2);
        } // imacn
        21 => {
            // DIV — slow
            reads.push(r1);
            reads.push(r2);
            write = Some(r2);
            slow = true;
        }
        30 => {
            reads.push(r1);
            reads.push(r2);
        } // cmp
        31 => reads.push(r2),               // cmpq
        // shared 32/33/62/63 + move family
        32 | 33 | 62 | 63 => {
            reads.push(r2);
            write = Some(r2);
        }
        34 => {
            reads.push(r1);
            write = Some(r2);
        } // move
        35 => write = Some(r2),             // moveq
        36 => {
            reads.push(r1);
            write = Some(r2);
        } // moveta (other bank dst — conservative)
        37 => {
            reads.push(r1);
            write = Some(r2);
        } // movefa
        38 => write = Some(r2),             // movei
        // simple loads: read r1 (addr); write r2 — slow
        39 | 40 | 41 => {
            reads.push(r1);
            write = Some(r2);
            slow = true;
        }
        42 => {
            if dsp {
                reads.push(r2);
                write = Some(r2);
            } else {
                reads.push(r1);
                write = Some(r2);
                slow = true; // LOADP
            }
        }
        43 => {
            reads.push(14);
            write = Some(r2);
            slow = true;
        }
        44 => {
            reads.push(15);
            write = Some(r2);
            slow = true;
        }
        // plain stores: read addr r1, data r2 — DATA is scoreboarded here
        45 | 46 | 47 => {
            reads.push(r1);
            reads.push(r2);
        }
        48 => {
            if dsp {
                reads.push(r2);
                write = Some(r2);
            } else {
                reads.push(r1);
                reads.push(r2);
            }
        }
        // indexed stores: DATA (r1) NOT scoreboarded; base read is protected
        49 => {
            reads.push(14);
            indexed_store_data = Some(r1);
        }
        50 => {
            reads.push(15);
            indexed_store_data = Some(r1);
        }
        51 => write = Some(r2),             // move pc
        52 => reads.push(r1),               // jump (addr reg)
        53 => {}                            // jr
        54 => write = Some(r2),             // mmult
        55 | 56 => {
            reads.push(r1);
            write = Some(r2);
        }
        57 => {} // nop
        58 => {
            reads.push(14);
            reads.push(r1);
            write = Some(r2);
            slow = true;
        }
        59 => {
            reads.push(15);
            reads.push(r1);
            write = Some(r2);
            slow = true;
        }
        60 => {
            reads.push(14);
            reads.push(r2);
            indexed_store_data = Some(r1);
        }
        61 => {
            reads.push(15);
            reads.push(r2);
            indexed_store_data = Some(r1);
        }
        _ => {}
    }
    Some(Acc { reads, write, slow, indexed_store_data, op })
}

fn is_jump(op: u8) -> bool {
    op == 52 || op == 53
}

/// Run the hazard pass over the emitted stream.
pub fn check(emitted: &[Emitted]) -> Vec<Diag> {
    let mut diags = Vec::new();
    // reg -> line of the pending slow producer (None = settled)
    let mut pending: HashMap<u8, usize> = HashMap::new();

    // Only instruction-bearing entries participate; keep their indices so we can
    // look at the delay slot (the next instruction).
    let insns: Vec<&Emitted> = emitted.iter().filter(|e| e.op.is_some()).collect();

    for (i, e) in insns.iter().enumerate() {
        let Some(acc) = classify(e) else { continue };

        // Delay-slot content check: look at what follows a jump.
        if is_jump(acc.op) {
            if let Some(next) = insns.get(i + 1) {
                let nop = next.op.unwrap();
                if is_jump(nop) {
                    diags.push(Diag::error(
                        next.line,
                        "JUMP/JR in a delay slot — two sequential jumps have unpredictable results",
                    ).with_fix("separate the jumps with a real instruction (or a NOP)"));
                } else if nop == 38 {
                    diags.push(Diag::error(
                        next.line,
                        "MOVEI in a delay slot — a 3-word instruction in the single slot is unverified on hardware",
                    ).with_fix("move the MOVEI before the jump and keep a 1-word instruction in the slot"));
                } else if nop == 57 {
                    // nop in slot: if the instruction AFTER is also nop, flag the
                    // wasteful old two-nop convention.
                    if let Some(after) = insns.get(i + 2) {
                        if after.op == Some(57) {
                            diags.push(Diag::warn(
                                next.line,
                                "nop+nop after a jump wastes the single delay slot (the old two-NOP rule)",
                            ).with_fix("fill the slot with useful work; the JRISC delay slot always executes"));
                        }
                    }
                }
            }
        }

        // Indexed store of an unsettled register (erratum) — check BEFORE reads
        // settle anything, and do NOT settle it (the read is unprotected).
        if let Some(data) = acc.indexed_store_data {
            if let Some(&pline) = pending.get(&data) {
                diags.push(Diag::error(
                    e.line,
                    format!(
                        "indexed store of r{data}, still pending from a load/divide at line {pline} \
                         — the DATA register is not scoreboarded (TRM errata), the STALE value is stored"
                    ),
                ).with_fix(format!("touch the register first, e.g. `or r{data},r{data}`, so the scoreboard settles it")));
            }
        }

        // A protected read settles the scoreboard for that register.
        for &r in &acc.reads {
            pending.remove(&r);
        }

        // Write-after-write into a shadow (bug 13): writing a register that is
        // still pending, when this instruction did NOT also read it.
        if let Some(w) = acc.write {
            let read_it = acc.reads.contains(&w);
            if !read_it {
                if let Some(&pline) = pending.get(&w) {
                    diags.push(Diag::error(
                        e.line,
                        format!(
                            "write to r{w} races a pending load/divide from line {pline} \
                             (TRM bug 13: writes are not scoreboarded — the slow value lands LAST)"
                        ),
                    ).with_fix(format!("read r{w} first (e.g. `or r{w},r{w}`) or reorder so the slow result is consumed before overwrite")));
                }
            }
            // Update pending for this write.
            if acc.slow {
                pending.insert(w, e.line);
            } else {
                pending.remove(&w);
            }
        }
    }

    diags
}
