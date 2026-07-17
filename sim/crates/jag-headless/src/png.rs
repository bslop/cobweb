//! A tiny, dependency-free PNG encoder (zlib *stored* blocks). Keeps the
//! emulator offline-buildable and deterministic — no external `png`/`flate2`.
//!
//! Output is a valid 8-bit RGBA PNG. We don't compress (stored blocks), which
//! is fine for screenshots: correctness and zero-dependency beat file size.

fn crc32(bytes: &[u8]) -> u32 {
    // Standard CRC-32 (IEEE), computed without a precomputed table.
    let mut crc: u32 = 0xFFFF_FFFF;
    for &b in bytes {
        crc ^= b as u32;
        for _ in 0..8 {
            let mask = (crc & 1).wrapping_neg();
            crc = (crc >> 1) ^ (0xEDB8_8320 & mask);
        }
    }
    !crc
}

fn adler32(bytes: &[u8]) -> u32 {
    let mut a: u32 = 1;
    let mut b: u32 = 0;
    for &x in bytes {
        a = (a + x as u32) % 65521;
        b = (b + a) % 65521;
    }
    (b << 16) | a
}

fn write_chunk(out: &mut Vec<u8>, kind: &[u8; 4], data: &[u8]) {
    out.extend_from_slice(&(data.len() as u32).to_be_bytes());
    let start = out.len();
    out.extend_from_slice(kind);
    out.extend_from_slice(data);
    let crc = crc32(&out[start..]);
    out.extend_from_slice(&crc.to_be_bytes());
}

/// Encode RGBA8888 (`width*height*4` bytes) into PNG bytes.
pub fn encode_rgba(width: u32, height: u32, rgba: &[u8]) -> Vec<u8> {
    assert_eq!(rgba.len(), (width * height * 4) as usize, "rgba size mismatch");

    let mut out = Vec::new();
    out.extend_from_slice(&[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A]);

    // IHDR: width, height, bit depth 8, color type 6 (RGBA), no interlace.
    let mut ihdr = Vec::with_capacity(13);
    ihdr.extend_from_slice(&width.to_be_bytes());
    ihdr.extend_from_slice(&height.to_be_bytes());
    ihdr.extend_from_slice(&[8, 6, 0, 0, 0]);
    write_chunk(&mut out, b"IHDR", &ihdr);

    write_chunk(&mut out, b"IDAT", &rgba_to_zlib(width, height, rgba));
    write_chunk(&mut out, b"IEND", &[]);
    out
}

/// Filter (none) + zlib-stored-DEFLATE a single RGBA frame into a zlib stream
/// (the payload of an IDAT or, with a sequence prefix, an APNG fdAT chunk).
fn rgba_to_zlib(width: u32, height: u32, rgba: &[u8]) -> Vec<u8> {
    let mut raw = Vec::with_capacity(((width * 4 + 1) * height) as usize);
    for y in 0..height {
        raw.push(0);
        let row = (y * width * 4) as usize;
        raw.extend_from_slice(&rgba[row..row + (width * 4) as usize]);
    }
    let mut zlib = Vec::with_capacity(raw.len() + raw.len() / 65535 + 16);
    zlib.extend_from_slice(&[0x78, 0x01]);
    let mut off = 0usize;
    while off < raw.len() {
        let take = (raw.len() - off).min(0xFFFF);
        let last = off + take >= raw.len();
        zlib.push(if last { 1 } else { 0 });
        zlib.extend_from_slice(&(take as u16).to_le_bytes());
        zlib.extend_from_slice(&(!(take as u16)).to_le_bytes());
        zlib.extend_from_slice(&raw[off..off + take]);
        off += take;
    }
    zlib.extend_from_slice(&adler32(&raw).to_be_bytes());
    zlib
}

/// Encode RGBA frames (all `width`×`height`) as an **animated PNG (APNG)** — a
/// real, scrubbable video file (full color, viewable in browsers/most viewers),
/// reusing the PNG encoder. `delay_num/delay_den` = per-frame delay in seconds
/// (e.g. 1/15 for 15 fps); `loops` = 0 for infinite.
pub fn encode_apng(
    width: u32,
    height: u32,
    frames: &[Vec<u8>],
    delay_num: u16,
    delay_den: u16,
    loops: u32,
) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(&[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A]);

    let mut ihdr = Vec::with_capacity(13);
    ihdr.extend_from_slice(&width.to_be_bytes());
    ihdr.extend_from_slice(&height.to_be_bytes());
    ihdr.extend_from_slice(&[8, 6, 0, 0, 0]);
    write_chunk(&mut out, b"IHDR", &ihdr);

    // acTL: number of frames + loop count.
    let mut actl = Vec::with_capacity(8);
    actl.extend_from_slice(&(frames.len() as u32).to_be_bytes());
    actl.extend_from_slice(&loops.to_be_bytes());
    write_chunk(&mut out, b"acTL", &actl);

    let mut seq: u32 = 0;
    let fctl = |seq: u32| -> Vec<u8> {
        let mut f = Vec::with_capacity(26);
        f.extend_from_slice(&seq.to_be_bytes()); // sequence number
        f.extend_from_slice(&width.to_be_bytes());
        f.extend_from_slice(&height.to_be_bytes());
        f.extend_from_slice(&0u32.to_be_bytes()); // x offset
        f.extend_from_slice(&0u32.to_be_bytes()); // y offset
        f.extend_from_slice(&delay_num.to_be_bytes());
        f.extend_from_slice(&delay_den.to_be_bytes());
        f.push(0); // dispose: none
        f.push(0); // blend: source
        f
    };

    for (i, frame) in frames.iter().enumerate() {
        write_chunk(&mut out, b"fcTL", &fctl(seq));
        seq += 1;
        let z = rgba_to_zlib(width, height, frame);
        if i == 0 {
            // First frame is the default image (IDAT).
            write_chunk(&mut out, b"IDAT", &z);
        } else {
            // Subsequent frames: fdAT = sequence number + zlib stream.
            let mut fdat = Vec::with_capacity(z.len() + 4);
            fdat.extend_from_slice(&seq.to_be_bytes());
            fdat.extend_from_slice(&z);
            write_chunk(&mut out, b"fdAT", &fdat);
            seq += 1;
        }
    }
    write_chunk(&mut out, b"IEND", &[]);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encodes_valid_png_header() {
        let png = encode_rgba(2, 2, &[0xFF; 16]);
        assert_eq!(&png[0..8], &[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A]);
        // IHDR chunk type appears right after the 8-byte signature + length.
        assert_eq!(&png[12..16], b"IHDR");
        // Ends with IEND.
        assert_eq!(&png[png.len() - 8..png.len() - 4], b"IEND");
    }

    #[test]
    fn crc_matches_known_value() {
        // CRC-32 of "IEND" (the IEND chunk with empty data) is a known constant.
        assert_eq!(crc32(b"IEND"), 0xAE42_6082);
    }
}
