//! Hardware CRY16 → RGB decode. CRY = Cyan/Red chroma (high byte) + intensity
//! (low byte). The three 16×16 modifier ROM tables are the Jaguar's, transcribed
//! from the Technical Reference (TRM p.28). This is the decode the OP/DAC does —
//! independent of any game-specific palette — so it matches BigPEmu.
//!
//! Nibble order, MEASURED on real silicon (jag_ascii, 2026-08-15): **red is the
//! HIGH nibble** `px[15:12]`, cyan the LOW nibble `px[11:8]`, intensity
//! `y = px[7:0]`; `R = CRY_RED[red][cyan] * y / 255`, likewise G/B.
//!
//! This was the other way round until a 16x16 sweep of all 256 chroma values was
//! rendered on a Jaguar and captured alongside the same sweep from here. Against
//! that capture the order below has a median hue error of 3.1 deg (worst 10.8,
//! which is composite-capture rendition); the previous order had a median of
//! 94.7 deg. Concrete: $0Fyy is CYAN on hardware, not the "pure red ramp" the
//! old anchors claimed, and $50yy is violet, not blue.
//!
//! The old note cited BigPEmu and the Quake port's E1M1 scene as verification.
//! Whatever that established, it was not this: an eyeball on a scene whose
//! palette was itself authored against this decode cannot separate the two
//! orders, because both render *a* plausible city. Only a sweep can.

#[rustfmt::skip]
const CRY_RED: [[u8; 16]; 16] = [
    [  0,  0,  0,  0,  0,  0,  0,  0,  0,  0,  0,  0,  0,  0,  0,  0],
    [ 34, 34, 34, 34, 34, 34, 34, 34, 34, 34, 34, 34, 34, 34, 19,  0],
    [ 68, 68, 68, 68, 68, 68, 68, 68, 68, 68, 68, 68, 64, 43, 21,  0],
    [102,102,102,102,102,102,102,102,102,102,102, 95, 71, 47, 23,  0],
    [135,135,135,135,135,135,135,135,135,135,130,104, 78, 52, 26,  0],
    [169,169,169,169,169,169,169,169,169,170,141,113, 85, 56, 28,  0],
    [203,203,203,203,203,203,203,203,203,183,153,122, 91, 61, 30,  0],
    [237,237,237,237,237,237,237,237,230,197,164,131, 98, 65, 32,  0],
    [255,255,255,255,255,255,255,255,247,214,181,148,115, 82, 49, 17],
    [255,255,255,255,255,255,255,255,255,235,204,173,143,112, 81, 51],
    [255,255,255,255,255,255,255,255,255,255,227,198,170,141,113, 85],
    [255,255,255,255,255,255,255,255,255,255,249,223,197,171,145,119],
    [255,255,255,255,255,255,255,255,255,255,255,248,224,200,177,153],
    [255,255,255,255,255,255,255,255,255,255,255,255,252,230,208,187],
    [255,255,255,255,255,255,255,255,255,255,255,255,255,255,240,221],
    [255,255,255,255,255,255,255,255,255,255,255,255,255,255,255,255],
];

#[rustfmt::skip]
const CRY_GREEN: [[u8; 16]; 16] = [
    [  0, 17, 34, 51, 68, 85,102,119,136,153,170,187,204,221,238,255],
    [  0, 19, 38, 57, 77, 96,115,134,154,173,192,211,231,250,255,255],
    [  0, 21, 43, 64, 86,107,129,150,172,193,215,236,255,255,255,255],
    [  0, 23, 47, 71, 95,119,142,166,190,214,238,255,255,255,255,255],
    [  0, 26, 52, 78,104,130,156,182,208,234,255,255,255,255,255,255],
    [  0, 28, 56, 85,113,141,170,198,226,255,255,255,255,255,255,255],
    [  0, 30, 61, 91,122,153,183,214,244,255,255,255,255,255,255,255],
    [  0, 32, 65, 98,131,164,197,230,255,255,255,255,255,255,255,255],
    [  0, 32, 65, 98,131,164,197,230,255,255,255,255,255,255,255,255],
    [  0, 30, 61, 91,122,153,183,214,244,255,255,255,255,255,255,255],
    [  0, 28, 56, 85,113,141,170,198,226,255,255,255,255,255,255,255],
    [  0, 26, 52, 78,104,130,156,182,208,234,255,255,255,255,255,255],
    [  0, 23, 47, 71, 95,119,142,166,190,214,238,255,255,255,255,255],
    [  0, 21, 43, 64, 86,107,129,150,172,193,215,236,255,255,255,255],
    [  0, 19, 38, 57, 77, 96,115,134,154,173,192,211,231,250,255,255],
    [  0, 17, 34, 51, 68, 85,102,119,136,153,170,187,204,221,238,255],
];

#[rustfmt::skip]
const CRY_BLUE: [[u8; 16]; 16] = [
    [255,255,255,255,255,255,255,255,255,255,255,255,255,255,255,255],
    [255,255,255,255,255,255,255,255,255,255,255,255,255,255,240,221],
    [255,255,255,255,255,255,255,255,255,255,255,255,252,230,208,187],
    [255,255,255,255,255,255,255,255,255,255,255,248,224,200,177,153],
    [255,255,255,255,255,255,255,255,255,255,249,223,197,171,145,119],
    [255,255,255,255,255,255,255,255,255,255,227,198,170,141,113, 85],
    [255,255,255,255,255,255,255,255,255,235,204,173,143,112, 81, 51],
    [255,255,255,255,255,255,255,255,247,214,181,148,115, 82, 49, 17],
    [237,237,237,237,237,237,237,237,230,197,164,131, 98, 65, 32,  0],
    [203,203,203,203,203,203,203,203,203,183,153,122, 91, 61, 30,  0],
    [169,169,169,169,169,169,169,169,169,170,141,113, 85, 56, 28,  0],
    [135,135,135,135,135,135,135,135,135,135,130,104, 78, 52, 26,  0],
    [102,102,102,102,102,102,102,102,102,102,102, 95, 71, 47, 23,  0],
    [ 68, 68, 68, 68, 68, 68, 68, 68, 68, 68, 68, 68, 64, 43, 21,  0],
    [ 34, 34, 34, 34, 34, 34, 34, 34, 34, 34, 34, 34, 34, 34, 19,  0],
    [  0,  0,  0,  0,  0,  0,  0,  0,  0,  0,  0,  0,  0,  0,  0,  0],
];

/// Decode a 16-bit CRY pixel to 8-bit RGB.
#[inline]
pub fn cry16_to_rgb(px: u16) -> (u8, u8, u8) {
    let cr = ((px >> 12) & 0xF) as usize; // RED nibble (row) — the high one
    let cy = ((px >> 8) & 0xF) as usize; // cyan nibble (column)
    let y = (px & 0xFF) as u32;
    let scale = |m: u8| ((m as u32 * y) / 255) as u8;
    (scale(CRY_RED[cr][cy]), scale(CRY_GREEN[cr][cy]), scale(CRY_BLUE[cr][cy]))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cry_intensity_zero_is_black() {
        // Y is the low byte; a zero low byte is black for any chroma.
        assert_eq!(cry16_to_rgb(0xFF00), (0, 0, 0)); // y=0, any chroma
    }

    #[test]
    fn cry_primary_corners() {
        // Corners as MEASURED on hardware, not as the TRM prose reads.
        // $F0FF: red=15, cyan=0, y=255 → pure red.
        assert_eq!(cry16_to_rgb(0xF0FF), (255, 0, 0));
        // $0FFF: red=0, cyan=15, y=255 → cyan.
        assert_eq!(cry16_to_rgb(0x0FFF), (0, 255, 255));
        // $00FF: no chroma, y=255 → blue corner of the CRY cube.
        assert_eq!(cry16_to_rgb(0x00FF), (0, 0, 255));
        // $88FF: centre chroma, full intensity ≈ white.
        assert_eq!(cry16_to_rgb(0x88FF), (247, 255, 230));
    }
}
