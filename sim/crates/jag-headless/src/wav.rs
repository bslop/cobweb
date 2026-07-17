//! A tiny, dependency-free WAV (RIFF/PCM) encoder for the captured audio.
//! 16-bit signed PCM. Stereo if `channels == 2` and samples are interleaved.

/// Encode interleaved 16-bit PCM `samples` into a WAV file.
pub fn encode_pcm16(sample_rate: u32, channels: u16, samples: &[i16]) -> Vec<u8> {
    let bits = 16u16;
    let block_align = channels * bits / 8;
    let byte_rate = sample_rate * block_align as u32;
    let data_len = (samples.len() * 2) as u32;

    let mut out = Vec::with_capacity(44 + data_len as usize);
    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&(36 + data_len).to_le_bytes());
    out.extend_from_slice(b"WAVE");
    // fmt chunk
    out.extend_from_slice(b"fmt ");
    out.extend_from_slice(&16u32.to_le_bytes()); // PCM fmt chunk size
    out.extend_from_slice(&1u16.to_le_bytes()); // audio format = PCM
    out.extend_from_slice(&channels.to_le_bytes());
    out.extend_from_slice(&sample_rate.to_le_bytes());
    out.extend_from_slice(&byte_rate.to_le_bytes());
    out.extend_from_slice(&block_align.to_le_bytes());
    out.extend_from_slice(&bits.to_le_bytes());
    // data chunk
    out.extend_from_slice(b"data");
    out.extend_from_slice(&data_len.to_le_bytes());
    for &s in samples {
        out.extend_from_slice(&s.to_le_bytes());
    }
    out
}

/// Simple peak/RMS stats of captured audio — lets Claude tell silence from
/// sound without "listening".
pub fn stats(samples: &[i16]) -> (i16, f64) {
    let peak = samples.iter().map(|s| s.unsigned_abs()).max().unwrap_or(0) as i16;
    let rms = if samples.is_empty() {
        0.0
    } else {
        let sum: f64 = samples.iter().map(|&s| (s as f64) * (s as f64)).sum();
        (sum / samples.len() as f64).sqrt()
    };
    (peak, rms)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wav_header_is_valid() {
        let w = encode_pcm16(44100, 2, &[0, 0, 1000, -1000]);
        assert_eq!(&w[0..4], b"RIFF");
        assert_eq!(&w[8..12], b"WAVE");
        assert_eq!(&w[12..16], b"fmt ");
        assert_eq!(&w[36..40], b"data");
        // channels @ offset 22, sample rate @ 24
        assert_eq!(u16::from_le_bytes([w[22], w[23]]), 2);
        assert_eq!(u32::from_le_bytes([w[24], w[25], w[26], w[27]]), 44100);
    }

    #[test]
    fn stats_detects_signal() {
        let (peak, rms) = stats(&[0, 0, 0, 0]);
        assert_eq!(peak, 0);
        assert_eq!(rms, 0.0);
        let (peak, rms) = stats(&[1000, -1000, 1000, -1000]);
        assert_eq!(peak, 1000);
        assert!((rms - 1000.0).abs() < 1.0);
    }
}
