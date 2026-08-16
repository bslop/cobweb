//! Regression: an UNCONDITIONAL jump ends linear fall-through.
//!
//! The hazard pass is straight-line, and that produced FALSE ERRORS on real
//! code: a routine that loads r24..r27 down one branch, jumps away, and loads
//! the same registers again in the ALTERNATIVE branch was reported as four
//! bug-13 write-after-write races - for two blocks that can never both run.
//! It refused to assemble gpu_bltex.gas and gpu_textured.gas outright, which
//! forced those kernels onto another assembler that checks nothing.

use jas::{assemble, Options};

#[test]
fn unconditional_jump_ends_the_shadow() {
    let src = "\
.gpu
        movei   #$00F03000,r28
        load    (r28),r24
        movei   #done,r22
        jump    T,(r22)
        nop
alt:
        load    (r28),r24
        nop
done:
        nop
";
    let out = assemble(src, &Options::default());
    assert_eq!(out.errors(), 0,
        "an unconditional jump must end the shadow window: {:#?}", out.diags);
}

#[test]
fn conditional_jump_still_reports_the_race() {
    // A CONDITIONAL jump may fall through, so the shadow must survive it and
    // the genuine bug-13 race still has to be caught.
    let src = "\
.gpu
        movei   #$00F03000,r28
        load    (r28),r24
        movei   #done,r22
        jump    NE,(r22)
        nop
        moveq   #1,r24
done:
        nop
";
    let out = assemble(src, &Options::default());
    assert!(out.errors() > 0,
        "a conditional jump must NOT clear the shadow: {:#?}", out.diags);
}

/// ☠️ A READ DOES NOT ALWAYS SETTLE THE SCOREBOARD — the one hazard in this
/// corpus that has actually been observed on silicon, and the one jas used to
/// report as clean.
///
/// `div rX,r0` then `neg r0` inside the shadow drew a stray polygon edge on a
/// Skunkboard (296 px across 7 columns) that no emulator reproduced. `neg`
/// reads r0, so the read-settles rule cleared the pending divide and nothing
/// was reported — but the operand of a single-operand op is the DESTINATION
/// field, which carries no interlock, so the divide's late write discarded the
/// negate.
#[test]
fn warns_on_dst_field_rmw_inside_the_divide_shadow() {
    let out = assemble(
        "        .gpu\n\
         \x20       div r3,r0\n\
         \x20       nop\n\
         \x20       nop\n\
         \x20       neg r0\n",
        &Options::default(),
    );
    assert_eq!(out.errors(), 0, "must stay a WARNING, not an error: {:#?}", out.diags);
    let msgs: Vec<String> = out.diags.iter().map(|d| format!("{d:?}")).collect();
    assert!(
        msgs.iter().any(|m| m.contains("DIVIDE shadow")),
        "expected the divide-shadow RMW warning, got: {msgs:#?}"
    );
}

/// A real source-field read (`move r0,r5`) DOES interlock — that is the fix the
/// bench confirmed, so it must come back clean.
#[test]
fn no_warning_when_the_quotient_is_read_into_a_scratch_first() {
    let out = assemble(
        "        .gpu\n\
         \x20       div r3,r0\n\
         \x20       move r0,r5\n\
         \x20       neg r5\n",
        &Options::default(),
    );
    let msgs: Vec<String> = out.diags.iter().map(|d| format!("{d:?}")).collect();
    assert!(!msgs.iter().any(|m| m.contains("DIVIDE shadow")), "got: {msgs:#?}");
}

/// Deliberately silent for a LOAD shadow: the silicon evidence is the divide.
/// Inventing a load case would be the over-reporting the flow rules removed.
#[test]
fn no_divide_warning_for_a_load_shadow_rmw() {
    let out = assemble(
        "        .gpu\n\
         \x20       movei #$100000,r3\n\
         \x20       load (r3),r0\n\
         \x20       neg r0\n",
        &Options::default(),
    );
    let msgs: Vec<String> = out.diags.iter().map(|d| format!("{d:?}")).collect();
    assert!(!msgs.iter().any(|m| m.contains("DIVIDE shadow")), "got: {msgs:#?}");
}
