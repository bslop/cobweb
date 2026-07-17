//! Jerry peripherals: joypad, timers, interrupt routing, audio (I2S).
//!
//! v1 implements the joypad read path (port 1 is what the conveyor belt uses);
//! timers/audio/DSP-interrupt routing build on this. See
//! `docs/spec/JERRY_AUDIO_IO.md`.

/// Logical Jaguar controller buttons. Values are the bit positions in the
/// 32-bit "joyedge" word documented in `JAGUAR.INC` (format
/// `xxApxxBx RLDU147* xxCxxxox 2580369#`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
pub enum Button {
    Up = 20,
    Down = 21,
    Left = 22,
    Right = 23,
    A = 29,
    B = 25,
    C = 13,
    Option = 9,
    Pause = 28,
    Star = 16,
    Hash = 0,
    K0 = 4,
    K1 = 19,
    K2 = 7,
    K3 = 3,
    K4 = 18,
    K5 = 6,
    K6 = 2,
    K7 = 17,
    K8 = 5,
    K9 = 1,
}

impl Button {
    #[inline]
    pub fn mask(self) -> u32 {
        1u32 << (self as u32)
    }
}

/// Build a pad word from a set of pressed buttons (logical "pressed = bit set"
/// convention used by the public API; the bus presents the hardware-correct
/// active-low multiplex when read through `$F14000/$F14002`).
pub fn pad_word(buttons: &[Button]) -> u32 {
    buttons.iter().fold(0u32, |acc, b| acc | b.mask())
}

/// The controller matrix: for each strobe column, the `(joy32_bit,
/// joyedge_button_bit)` pairs that the hardware drives **active-low**.
/// Derived from the shipped Jaguar Doom scan (a reference backend's joypad driver):
/// row data appears in JOY32 bits 27:24 (JOYSTICK[11:8]) + 1:0 (JOYBUTS[1:0]).
const COLS: [&[(u32, u32)]; 4] = [
    // strobe $81FE (col 0): Right,Left,Down,Up + A,Pause
    &[(27, 23), (26, 22), (25, 21), (24, 20), (1, 29), (0, 28)],
    // strobe $81FD (col 1): 7,4,1,* + B
    &[(27, 17), (26, 18), (25, 19), (24, 16), (1, 25)],
    // strobe $81FB (col 2): 2,5,8,0 + C
    &[(27, 7), (26, 6), (25, 5), (24, 4), (1, 13)],
    // strobe $81F7 (col 3): 3,6,9,# + Option
    &[(27, 3), (26, 2), (25, 1), (24, 0), (1, 9)],
];

/// Decode the strobe column from the value written to JOYSTICK: the active
/// column is the one whose low-nibble bit is pulled low ($FE→0, $FD→1, …).
fn strobe_col(strobe: u16) -> usize {
    match (!strobe) & 0xF {
        v if v & 0x1 != 0 => 0,
        v if v & 0x2 != 0 => 1,
        v if v & 0x4 != 0 => 2,
        v if v & 0x8 != 0 => 3,
        _ => 0,
    }
}

/// Compute the 32-bit `$F14000` read (JOYSTICK<<16 | JOYBUTS) for the given
/// strobe and injected (joyedge-format) pad word. Active-low: a pressed button
/// clears its matrix bit.
pub fn joy32(strobe: u16, pad: u32) -> u32 {
    let mut joy = 0xFFFF_FFFFu32;
    for &(mbit, bbit) in COLS[strobe_col(strobe)] {
        if pad & (1 << bbit) != 0 {
            joy &= !(1 << mbit);
        }
    }
    joy
}
