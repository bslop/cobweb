//! Hardware CRY16 → RGB decode. CRY = Cyan/Red chroma (high byte) + intensity
//! (low byte). The three 16×16 modifier ROM tables are the Jaguar's, transcribed
//! from the Technical Reference (TRM p.28). This is the decode the OP/DAC does —
//! independent of any game-specific palette — so it matches BigPEmu.
//!
//! Byte order per the TRM pixel format (C-R-Y, high to low): cyan `px[15:12]`,
//! red `px[11:8]`, intensity `y = px[7:0]`; `R = CRY_RED[red][cyan] * y / 255`,
//! likewise G/B. Verified against a CRY framebuffer that renders correctly on
//! BigPEmu *and* real silicon (the Quake port's E1M1 scene): this order
//! reproduces it; the swapped order (Y in the high byte, previously claimed
//! from a Cybermorph eyeball — white text survives either order, so it proved
//! nothing) renders chroma noise. Sanity anchors: `$0Fyy` = pure red ramp,
//! `$F0yy` = cyan ramp, `$88FF` ≈ white — all three hold only in this order.

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
///
/// The chroma index is the WHOLE HIGH BYTE, used flat: `base[(px >> 8) & 0xFF]`.
/// The tables above are that same 256-entry `tga2cry` array written as 16x16,
/// so the row is the high nibble and the column is the low nibble — the
/// opposite order to the one this used to apply, which read the transpose.
///
/// Two bugs, both silicon-adjudicated against a bubsy3d capture
/// (`COBWEB_BUG_cry16_decode.md`, four authored face colours):
///
///  1. Index order. `[low][high]` reads the transposed entry, and it is silent
///     because the transpose of a plausible colour is another plausible colour
///     — a cube rendered every face wrong still looks like a shaded cube. It
///     survived earlier verification because the anchors used ($0Fyy, $F0yy)
///     are symmetric-looking corners that were themselves derived from the
///     wrong convention rather than from silicon.
///  2. Intensity scale is `>> 8`, not `/ 255`. Off by at most one count, so it
///     hides completely behind bug 1 and cannot be found by eye.
///
/// The base table itself was never wrong: it is byte-for-byte the authentic
/// Atari `tga2cry` array (verified 256/256 against `crypal_tables.h`). The
/// filed report guessed the table and it was the one part that was correct.
#[inline]
pub fn cry16_to_rgb(px: u16) -> (u8, u8, u8) {
    let hi = ((px >> 12) & 0xF) as usize; // chroma index, high nibble = row
    let lo = ((px >> 8) & 0xF) as usize; // chroma index, low nibble = column
    let y = (px & 0xFF) as u32;
    let scale = |m: u8| (((m as u32) * y) >> 8) as u8;
    (scale(CRY_RED[hi][lo]), scale(CRY_GREEN[hi][lo]), scale(CRY_BLUE[hi][lo]))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cry_intensity_zero_is_black() {
        // Y is the low byte; a zero low byte is black for any chroma.
        assert_eq!(cry16_to_rgb(0xFF00), (0, 0, 0)); // y=0, any chroma
    }

    /// Corners, read off the `tga2cry` table by its flat high-byte index.
    ///
    /// These previously asserted the TRANSPOSED colours — $0FFF as pure red —
    /// because they were written from the same wrong convention as the decoder,
    /// so they confirmed it instead of catching it. `cry_base_rgb[0x0F]` is
    /// `(0,255,255)`: $0FFF is CYAN. Full intensity is 254, not 255, because
    /// the scale is `(m * 255) >> 8`.
    #[test]
    fn cry_primary_corners() {
        assert_eq!(cry16_to_rgb(0x0FFF), (0, 254, 254)); // base[0x0F] = cyan
        assert_eq!(cry16_to_rgb(0xF0FF), (254, 0, 0)); // base[0xF0] = red
        assert_eq!(cry16_to_rgb(0x00FF), (0, 0, 254)); // base[0x00] = blue
    }

    /// The adjudicating case: four authored face colours from bubsy3d, read off
    /// real silicon through a capture card. These are interior chroma values,
    /// not corners — the whole point, since a transposed table passes at the
    /// corners and fails everywhere a real renderer actually lives.
    #[test]
    fn cry_matches_silicon_bubsy3d_faces() {
        assert_eq!(cry16_to_rgb(0x1DDD), (29, 215, 220)); // top, cyan
        assert_eq!(cry16_to_rgb(0xECF2), (241, 218, 32)); // right, yellow
        assert_eq!(cry16_to_rgb(0x7ECA), (25, 201, 38)); // back, green
        assert_eq!(cry16_to_rgb(0xE2E3), (226, 33, 30)); // front, red
    }
}
