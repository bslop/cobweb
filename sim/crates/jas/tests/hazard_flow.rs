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
