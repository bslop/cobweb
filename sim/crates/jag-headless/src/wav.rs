//! A tiny, dependency-free WAV (RIFF/PCM) encoder/decoder for the captured
//! audio — 16-bit signed PCM, interleaved when stereo — plus the analysis
//! behind `jagemu audiocheck`: the audio counterpart of the pixel-diff
//! (silence/DC/clipping/spectrum health, and lag-aligned comparison of a
//! build's capture against an oracle's). Everything hand-rolled, std-only,
//! like the rest of the toolchain.

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

/// Parse a 16-bit PCM WAV: `(sample_rate, channels, interleaved samples)`.
pub fn decode_pcm16(bytes: &[u8]) -> Result<(u32, u16, Vec<i16>), String> {
    if bytes.len() < 12 || &bytes[0..4] != b"RIFF" || &bytes[8..12] != b"WAVE" {
        return Err("not a RIFF/WAVE file".into());
    }
    let mut rate = 0u32;
    let mut channels = 0u16;
    let mut bits = 0u16;
    let mut data: Option<&[u8]> = None;
    let mut i = 12;
    while i + 8 <= bytes.len() {
        let id = &bytes[i..i + 4];
        let len = u32::from_le_bytes(bytes[i + 4..i + 8].try_into().unwrap()) as usize;
        let body = bytes.get(i + 8..i + 8 + len).ok_or("truncated chunk")?;
        match id {
            b"fmt " => {
                if len < 16 {
                    return Err("short fmt chunk".into());
                }
                let format = u16::from_le_bytes([body[0], body[1]]);
                if format != 1 {
                    return Err(format!("unsupported WAV format {format} (want PCM)"));
                }
                channels = u16::from_le_bytes([body[2], body[3]]);
                rate = u32::from_le_bytes(body[4..8].try_into().unwrap());
                bits = u16::from_le_bytes([body[14], body[15]]);
            }
            b"data" => data = Some(body),
            _ => {}
        }
        i += 8 + len + (len & 1); // chunks are word-aligned
    }
    if bits != 16 {
        return Err(format!("unsupported bit depth {bits} (want 16)"));
    }
    let data = data.ok_or("no data chunk")?;
    let samples = data
        .chunks_exact(2)
        .map(|c| i16::from_le_bytes([c[0], c[1]]))
        .collect();
    Ok((rate, channels.max(1), samples))
}

// ── analysis ─────────────────────────────────────────────────────────────────

/// Windowed-RMS envelope size (mono samples). At ~44.1kHz this is ~23ms — fine
/// enough to see dropouts, coarse enough to align captures cheaply.
const WIN: usize = 1024;
/// A window quieter than this (dBFS) counts as silence.
const SILENCE_DBFS: f64 = -60.0;

fn dbfs(x: f64) -> f64 {
    if x <= 0.0 {
        -120.0
    } else {
        (20.0 * (x / 32768.0).log10()).max(-120.0)
    }
}

/// Mix interleaved samples to mono f64.
fn mono(samples: &[i16], channels: u16) -> Vec<f64> {
    let ch = channels.max(1) as usize;
    samples
        .chunks_exact(ch)
        .map(|f| f.iter().map(|&s| s as f64).sum::<f64>() / ch as f64)
        .collect()
}

/// Windowed RMS envelope of a mono signal.
fn envelope(m: &[f64]) -> Vec<f64> {
    m.chunks(WIN)
        .map(|w| (w.iter().map(|s| s * s).sum::<f64>() / w.len().max(1) as f64).sqrt())
        .collect()
}

/// In-place iterative radix-2 FFT (interleaved re/im). `n` must be a power of 2.
fn fft(re: &mut [f64], im: &mut [f64]) {
    let n = re.len();
    debug_assert!(n.is_power_of_two() && im.len() == n);
    // bit-reversal permutation
    let mut j = 0usize;
    for i in 0..n {
        if i < j {
            re.swap(i, j);
            im.swap(i, j);
        }
        let mut m = n >> 1;
        while m >= 1 && j & m != 0 {
            j ^= m;
            m >>= 1;
        }
        j |= m;
    }
    let mut len = 2;
    while len <= n {
        let ang = -2.0 * std::f64::consts::PI / len as f64;
        let (wr, wi) = (ang.cos(), ang.sin());
        let mut i = 0;
        while i < n {
            let (mut cr, mut ci) = (1.0f64, 0.0f64);
            for k in 0..len / 2 {
                let (ur, ui) = (re[i + k], im[i + k]);
                let (vr, vi) = (
                    re[i + k + len / 2] * cr - im[i + k + len / 2] * ci,
                    re[i + k + len / 2] * ci + im[i + k + len / 2] * cr,
                );
                re[i + k] = ur + vr;
                im[i + k] = ui + vi;
                re[i + k + len / 2] = ur - vr;
                im[i + k + len / 2] = ui - vi;
                let ncr = cr * wr - ci * wi;
                ci = cr * wi + ci * wr;
                cr = ncr;
            }
            i += len;
        }
        len <<= 1;
    }
}

/// Average magnitude spectrum (dB) over Hann-windowed chunks of the LOUD part
/// of a mono signal. Returns `FFT_N/2` bins.
const FFT_N: usize = 4096;
fn avg_spectrum(m: &[f64]) -> Vec<f64> {
    let mut acc = vec![0.0f64; FFT_N / 2];
    let mut count = 0usize;
    let hann: Vec<f64> = (0..FFT_N)
        .map(|i| 0.5 - 0.5 * (2.0 * std::f64::consts::PI * i as f64 / FFT_N as f64).cos())
        .collect();
    for chunk in m.chunks_exact(FFT_N) {
        // skip windows that are essentially silent — they only blur the average
        let rms = (chunk.iter().map(|s| s * s).sum::<f64>() / FFT_N as f64).sqrt();
        if dbfs(rms) < SILENCE_DBFS {
            continue;
        }
        let mut re: Vec<f64> = chunk.iter().zip(&hann).map(|(s, h)| s * h).collect();
        let mut im = vec![0.0f64; FFT_N];
        fft(&mut re, &mut im);
        for k in 0..FFT_N / 2 {
            acc[k] += (re[k] * re[k] + im[k] * im[k]).sqrt();
        }
        count += 1;
    }
    if count > 0 {
        for a in &mut acc {
            *a /= count as f64;
        }
    }
    acc
}

/// Health report of one capture.
#[derive(Debug)]
pub struct Analysis {
    pub duration_s: f64,
    pub peak: i16,
    pub peak_dbfs: f64,
    pub rms_dbfs: f64,
    /// Mean sample value per channel, as a fraction of full scale.
    pub dc_offset: Vec<f64>,
    /// Samples at ±full scale (a run of them = hard clipping).
    pub clipped: usize,
    /// Fraction of windows below the silence threshold.
    pub silence_ratio: f64,
    /// Silence before the first audible window.
    pub leading_silence_s: f64,
    /// Longest silent run AFTER sound has started (dropout detector).
    pub longest_gap_s: f64,
    /// L/R Pearson correlation (1.0 = mono-identical; None for mono captures).
    pub channel_correlation: Option<f64>,
    /// Top spectral peaks of the loud part: (Hz, dB relative to the strongest).
    pub spectral_peaks: Vec<(f64, f64)>,
    pub silent: bool,
}

/// Analyze one capture.
pub fn analyze(rate: u32, channels: u16, samples: &[i16]) -> Analysis {
    let ch = channels.max(1) as usize;
    let nframes = samples.len() / ch;
    let duration_s = nframes as f64 / rate.max(1) as f64;
    let (peak, rms) = stats(samples);

    let mut dc_offset = Vec::with_capacity(ch);
    for c in 0..ch {
        let mut sum = 0.0f64;
        let mut n = 0usize;
        let mut idx = c;
        while idx < samples.len() {
            sum += samples[idx] as f64;
            n += 1;
            idx += ch;
        }
        dc_offset.push(if n == 0 { 0.0 } else { sum / n as f64 / 32768.0 });
    }
    let clipped = samples.iter().filter(|&&s| s == i16::MAX || s == i16::MIN).count();

    let m = mono(samples, channels);
    let env = envelope(&m);
    let loud = |e: &f64| dbfs(*e) >= SILENCE_DBFS;
    let n_loud = env.iter().filter(|e| loud(e)).count();
    let silence_ratio = if env.is_empty() {
        1.0
    } else {
        1.0 - n_loud as f64 / env.len() as f64
    };
    let win_s = WIN as f64 / rate.max(1) as f64;
    let first_loud = env.iter().position(loud);
    let leading_silence_s = first_loud.unwrap_or(env.len()) as f64 * win_s;
    let mut longest_gap = 0usize;
    if let Some(start) = first_loud {
        let last_loud = env.iter().rposition(loud).unwrap_or(start);
        let mut run = 0usize;
        for e in &env[start..=last_loud] {
            if loud(e) {
                longest_gap = longest_gap.max(run);
                run = 0;
            } else {
                run += 1;
            }
        }
        longest_gap = longest_gap.max(run);
    }

    let channel_correlation = (ch == 2).then(|| {
        let l: Vec<f64> = samples.iter().step_by(2).map(|&s| s as f64).collect();
        let r: Vec<f64> = samples.iter().skip(1).step_by(2).map(|&s| s as f64).collect();
        pearson(&l, &r)
    });

    let spec = avg_spectrum(&m);
    let bin_hz = rate as f64 / FFT_N as f64;
    let mut peaks: Vec<(f64, f64)> = Vec::new();
    let max_mag = spec.iter().cloned().fold(0.0f64, f64::max);
    if max_mag > 0.0 {
        // local maxima, strongest first, at least 3 bins apart, skipping DC
        let mut cands: Vec<(usize, f64)> = (2..spec.len() - 1)
            .filter(|&k| spec[k] > spec[k - 1] && spec[k] >= spec[k + 1])
            .map(|k| (k, spec[k]))
            .collect();
        cands.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap());
        for (k, mag) in cands {
            if peaks.len() >= 3 {
                break;
            }
            if peaks.iter().any(|&(hz, _)| (hz - k as f64 * bin_hz).abs() < 3.0 * bin_hz) {
                continue;
            }
            peaks.push((k as f64 * bin_hz, 20.0 * (mag / max_mag).log10()));
        }
    }

    Analysis {
        duration_s,
        peak,
        peak_dbfs: dbfs(peak as f64),
        rms_dbfs: dbfs(rms),
        dc_offset,
        clipped,
        silence_ratio,
        leading_silence_s,
        longest_gap_s: longest_gap as f64 * win_s,
        channel_correlation,
        spectral_peaks: peaks,
        silent: peak == 0,
    }
}

fn pearson(a: &[f64], b: &[f64]) -> f64 {
    let n = a.len().min(b.len());
    if n == 0 {
        return 0.0;
    }
    let (ma, mb) = (
        a[..n].iter().sum::<f64>() / n as f64,
        b[..n].iter().sum::<f64>() / n as f64,
    );
    let (mut num, mut da, mut db) = (0.0, 0.0, 0.0);
    for i in 0..n {
        let (x, y) = (a[i] - ma, b[i] - mb);
        num += x * y;
        da += x * x;
        db += y * y;
    }
    if da == 0.0 || db == 0.0 {
        // flat signals: identical flats correlate perfectly, else not at all
        return if da == db { 1.0 } else { 0.0 };
    }
    num / (da * db).sqrt()
}

/// Comparison of a capture against a reference (an oracle build's audio).
#[derive(Debug)]
pub struct Comparison {
    /// Lag applied to align the capture to the reference (seconds; positive =
    /// the capture's sound starts LATER than the reference's).
    pub lag_s: f64,
    /// Pearson correlation of the aligned loudness envelopes.
    pub envelope_correlation: f64,
    /// Mean |difference| of the aligned envelopes, in dB.
    pub envelope_mae_db: f64,
    /// Mean |difference| of the average spectra (loud parts), in dB.
    pub spectral_mae_db: f64,
    /// The bottom line: envelopes correlate and spectra agree.
    pub matches: bool,
}

/// Compare `(rate, channels, samples)` against a reference capture. Envelope
/// alignment first (builds boot at different speeds — the jcc68k flip boots
/// slower than the gcc oracle, same audio), then shape + spectrum.
pub fn compare(
    a: (u32, u16, &[i16]),
    b: (u32, u16, &[i16]),
) -> Result<Comparison, String> {
    if a.0 != b.0 {
        return Err(format!("sample rates differ ({} vs {})", a.0, b.0));
    }
    let rate = a.0;
    let ma = mono(a.2, a.1);
    let mb = mono(b.2, b.1);
    let ea = envelope(&ma);
    let eb = envelope(&mb);
    if ea.is_empty() || eb.is_empty() {
        return Err("empty capture".into());
    }
    // best lag by envelope cross-correlation over the full range
    let max_lag = ea.len().max(eb.len());
    let mut best = (0i64, f64::MIN);
    let lo = -(max_lag as i64);
    for lag in lo..=(max_lag as i64) {
        let mut num = 0.0;
        let mut n = 0usize;
        for i in 0..ea.len() {
            let j = i as i64 - lag;
            if j >= 0 && (j as usize) < eb.len() {
                num += ea[i] * eb[j as usize];
                n += 1;
            }
        }
        if n >= 8 && num > best.1 {
            best = (lag, num);
        }
    }
    let lag = best.0;
    // aligned overlap
    let mut xa = Vec::new();
    let mut xb = Vec::new();
    for i in 0..ea.len() {
        let j = i as i64 - lag;
        if j >= 0 && (j as usize) < eb.len() {
            xa.push(ea[i]);
            xb.push(eb[j as usize]);
        }
    }
    let envelope_correlation = pearson(&xa, &xb);
    let envelope_mae_db = if xa.is_empty() {
        120.0
    } else {
        xa.iter()
            .zip(&xb)
            .map(|(p, q)| (dbfs(*p) - dbfs(*q)).abs())
            .sum::<f64>()
            / xa.len() as f64
    };
    // spectral agreement over the loud parts (alignment-independent)
    let sa = avg_spectrum(&ma);
    let sb = avg_spectrum(&mb);
    // Clamp at -60 dB below each capture's own peak bin: below that is noise
    // floor, and log-of-noise differences would swamp the MAE with meaningless
    // dB across thousands of quiet bins.
    let norm = |s: &[f64]| -> Vec<f64> {
        let mx = s.iter().cloned().fold(0.0f64, f64::max).max(1e-12);
        s.iter()
            .map(|v| (20.0 * (v / mx).max(1e-6).log10()).max(-60.0))
            .collect()
    };
    let (na, nb) = (norm(&sa), norm(&sb));
    let spectral_mae_db =
        na.iter().zip(&nb).map(|(p, q)| (p - q).abs()).sum::<f64>() / na.len() as f64;

    let matches = envelope_correlation > 0.85 && spectral_mae_db < 6.0;
    Ok(Comparison {
        lag_s: lag as f64 * WIN as f64 / rate as f64,
        envelope_correlation,
        envelope_mae_db,
        spectral_mae_db,
        matches,
    })
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

    #[test]
    fn decode_roundtrips_encode() {
        let samples: Vec<i16> = vec![0, 100, -100, 32000, -32000, 7];
        let w = encode_pcm16(44097, 2, &samples);
        let (rate, ch, back) = decode_pcm16(&w).expect("decodes");
        assert_eq!((rate, ch), (44097, 2));
        assert_eq!(back, samples);
    }

    /// Interleaved stereo sine at `hz`, `secs` long, amplitude `amp`, starting
    /// after `lead` seconds of silence.
    fn sine(rate: u32, hz: f64, secs: f64, amp: f64, lead: f64) -> Vec<i16> {
        let n = (rate as f64 * (secs + lead)) as usize;
        let start = (rate as f64 * lead) as usize;
        let mut out = Vec::with_capacity(n * 2);
        for i in 0..n {
            let v = if i < start {
                0
            } else {
                let t = (i - start) as f64 / rate as f64;
                (amp * (2.0 * std::f64::consts::PI * hz * t).sin()) as i16
            };
            out.push(v);
            out.push(v);
        }
        out
    }

    #[test]
    fn analyze_finds_tone_and_silence() {
        let rate = 44100;
        let s = sine(rate, 1000.0, 1.0, 12000.0, 0.5);
        let a = analyze(rate, 2, &s);
        assert!(!a.silent);
        assert_eq!(a.clipped, 0);
        assert!((a.leading_silence_s - 0.5).abs() < 0.1, "lead {}", a.leading_silence_s);
        assert!(a.silence_ratio > 0.2 && a.silence_ratio < 0.5, "ratio {}", a.silence_ratio);
        let (hz, _) = a.spectral_peaks[0];
        assert!((hz - 1000.0).abs() < 25.0, "peak at {hz} Hz");
        assert!(a.channel_correlation.unwrap() > 0.999, "identical channels");
        assert!(a.dc_offset[0].abs() < 0.01);
    }

    #[test]
    fn analyze_flags_clipping_and_dc() {
        let rate = 44100;
        let clipped: Vec<i16> = (0..rate).map(|i| if i % 2 == 0 { i16::MAX } else { i16::MIN }).collect();
        let a = analyze(rate as u32, 1, &clipped);
        assert!(a.clipped > 1000);
        let dc: Vec<i16> = vec![8000; rate as usize];
        let a = analyze(rate as u32, 1, &dc);
        assert!(a.dc_offset[0] > 0.2, "dc {}", a.dc_offset[0]);
    }

    #[test]
    fn analyze_measures_dropout_gap() {
        let rate = 44100;
        // tone, 0.3s hole, tone again
        let mut s = sine(rate, 500.0, 0.5, 10000.0, 0.0);
        s.extend(std::iter::repeat(0i16).take((rate as f64 * 0.3) as usize * 2));
        s.extend(sine(rate, 500.0, 0.5, 10000.0, 0.0));
        let a = analyze(rate, 2, &s);
        assert!(
            (a.longest_gap_s - 0.3).abs() < 0.1,
            "gap {} (want ~0.3)",
            a.longest_gap_s
        );
    }

    #[test]
    fn compare_matches_delayed_same_audio() {
        let rate = 44100;
        let a = sine(rate, 800.0, 1.0, 10000.0, 0.2);
        let b = sine(rate, 800.0, 1.0, 10000.0, 0.7); // same sound, boots 0.5s later
        let c = compare((rate, 2, &a), (rate, 2, &b)).unwrap();
        assert!(c.matches, "should match: {c:?}");
        assert!((c.lag_s - (-0.5)).abs() < 0.1, "lag {} (want ~-0.5)", c.lag_s);
        assert!(c.envelope_correlation > 0.9);
    }

    #[test]
    fn compare_rejects_different_audio() {
        let rate = 44100;
        let a = sine(rate, 800.0, 1.0, 10000.0, 0.2);
        let b = sine(rate, 3100.0, 0.4, 4000.0, 0.1);
        let c = compare((rate, 2, &a), (rate, 2, &b)).unwrap();
        assert!(!c.matches, "must not match: {c:?}");
    }

    #[test]
    fn fft_locates_bin() {
        // a pure tone at bin 100 must dominate the spectrum
        let n = FFT_N;
        let mut re: Vec<f64> = (0..n)
            .map(|i| (2.0 * std::f64::consts::PI * 100.0 * i as f64 / n as f64).sin())
            .collect();
        let mut im = vec![0.0; n];
        fft(&mut re, &mut im);
        let mags: Vec<f64> = (0..n / 2)
            .map(|k| (re[k] * re[k] + im[k] * im[k]).sqrt())
            .collect();
        let top = mags
            .iter()
            .enumerate()
            .max_by(|a, b| a.1.partial_cmp(b.1).unwrap())
            .unwrap()
            .0;
        assert_eq!(top, 100);
    }
}
