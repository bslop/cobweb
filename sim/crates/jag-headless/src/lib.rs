//! Headless runner: load a ROM, run a deterministic number of frames with no
//! display server, and capture the **true OP-composited** frame as PNG.
//!
//! This is the conveyor belt's boot-test primitive — the honest replacement for
//! BigPEmu's headless capture (which reads DRAM, not the scan-out).

pub mod png;
pub mod wav;

use jag_core::{Framebuffer, Jaguar, StopReason};

/// Load a program and run it for `frames` fields. Returns the machine so the
/// caller can inspect state / capture further frames.
pub fn boot_and_run(rom: &[u8], frames: u64) -> Result<Jaguar, String> {
    let mut jag = Jaguar::new();
    jag.load(rom).map_err(|e| e.to_string())?;
    jag.run_frames(frames);
    Ok(jag)
}

/// Capture the current displayed frame as PNG bytes.
pub fn screenshot_png(jag: &Jaguar) -> Vec<u8> {
    let fb: Framebuffer = jag.capture_frame();
    png::encode_rgba(fb.width, fb.height, &fb.rgba)
}

/// Convenience: boot, run `frames`, return PNG bytes of the resulting frame.
pub fn boot_and_screenshot(rom: &[u8], frames: u64) -> Result<Vec<u8>, String> {
    let jag = boot_and_run(rom, frames)?;
    Ok(screenshot_png(&jag))
}

/// Run until a stop condition (frame target / breakpoint), reporting why.
pub fn run_reporting(jag: &mut Jaguar, target_frame: u64) -> StopReason {
    jag.run_to_frame(target_frame)
}

/// Per-frame metrics used by the playtester to flag "parts that don't look
/// right" without AI vision (Claude reviews the filmstrip for the rest).
#[derive(Debug, Clone)]
pub struct FrameMetrics {
    pub nonblack_pct: f32,
    pub distinct_colors: usize,
    /// Fraction of pixels that changed vs the previous captured frame (0 if none).
    pub changed_pct: f32,
    /// Heuristic anomaly flags (empty = looks plausible).
    pub flags: Vec<&'static str>,
}

/// Compute metrics for `fb` (optionally vs the previous captured frame). Flags:
/// `black` (≈all black), `near_black`, `low_color` (<4 colors), `frozen`
/// (identical to previous), `uniform` (single color).
pub fn frame_metrics(fb: &Framebuffer, prev: Option<&Framebuffer>) -> FrameMetrics {
    use std::collections::HashSet;
    let total = (fb.width * fb.height).max(1) as f32;
    let mut nonblack = 0u32;
    let mut colors: HashSet<u32> = HashSet::new();
    for px in fb.rgba.chunks_exact(4) {
        let c = (px[0] as u32) << 16 | (px[1] as u32) << 8 | px[2] as u32;
        if c != 0 {
            nonblack += 1;
        }
        if colors.len() < 4096 {
            colors.insert(c);
        }
    }
    let changed_pct = match prev {
        Some(p) if p.rgba.len() == fb.rgba.len() => {
            let mut diff = 0u32;
            for (a, b) in fb.rgba.chunks_exact(4).zip(p.rgba.chunks_exact(4)) {
                if a[0..3] != b[0..3] {
                    diff += 1;
                }
            }
            diff as f32 / total
        }
        _ => 1.0,
    };
    let nonblack_pct = nonblack as f32 / total;
    let distinct_colors = colors.len();

    let mut flags = Vec::new();
    if nonblack_pct == 0.0 {
        flags.push("black");
    } else if nonblack_pct < 0.01 {
        flags.push("near_black");
    }
    if distinct_colors <= 1 {
        flags.push("uniform");
    } else if distinct_colors < 4 {
        flags.push("low_color");
    }
    if prev.is_some() && changed_pct == 0.0 {
        flags.push("frozen");
    }
    FrameMetrics { nonblack_pct, distinct_colors, changed_pct, flags }
}

/// Compose a sequence of frames into one **filmstrip montage** image (a grid),
/// so a single image read shows motion over time. Frames are laid out
/// left-to-right, top-to-bottom in `cols` columns with a `gap`-px separator.
/// Returns `(width, height, rgba)`.
pub fn filmstrip(frames: &[Framebuffer], cols: u32, gap: u32) -> (u32, u32, Vec<u8>) {
    if frames.is_empty() {
        return (1, 1, vec![0, 0, 0, 255]);
    }
    let cols = cols.max(1);
    let fw = frames.iter().map(|f| f.width).max().unwrap_or(1);
    let fh = frames.iter().map(|f| f.height).max().unwrap_or(1);
    let rows = (frames.len() as u32).div_ceil(cols);
    let w = cols * fw + (cols + 1) * gap;
    let h = rows * fh + (rows + 1) * gap;
    // Dark-grey background so frame edges are visible.
    let mut out = vec![0u8; (w * h * 4) as usize];
    for px in out.chunks_exact_mut(4) {
        px[0] = 32;
        px[1] = 32;
        px[2] = 32;
        px[3] = 255;
    }
    for (i, f) in frames.iter().enumerate() {
        let c = (i as u32) % cols;
        let r = (i as u32) / cols;
        let ox = gap + c * (fw + gap);
        let oy = gap + r * (fh + gap);
        for y in 0..f.height {
            for x in 0..f.width {
                let si = ((y * f.width + x) * 4) as usize;
                let dx = ox + x;
                let dy = oy + y;
                let di = ((dy * w + dx) * 4) as usize;
                out[di..di + 4].copy_from_slice(&f.rgba[si..si + 4]);
            }
        }
    }
    (w, h, out)
}

/// Run `frames` frames with audio capture on, returning `(sample_rate, samples,
/// wav_bytes)`. Optionally inject `buttons` after `press_after` frames.
pub fn capture_audio(
    jag: &mut Jaguar,
    frames: u64,
    buttons: u32,
    press_after: u64,
) -> (u32, Vec<i16>, Vec<u8>) {
    jag.enable_audio_capture();
    if buttons != 0 && press_after < frames {
        jag.run_frames(press_after);
        jag.set_pad(0, buttons);
        jag.run_frames(frames - press_after);
    } else {
        jag.run_frames(frames);
    }
    let (rate, samples) = jag.take_audio();
    let wav = wav::encode_pcm16(rate, 2, &samples);
    (rate, samples, wav)
}

/// Capture `count` frames spaced `every` frames apart, starting after
/// `start_frame`, optionally holding `buttons`. Returns the captured frames.
pub fn capture_sequence(
    jag: &mut Jaguar,
    start_frame: u64,
    count: u32,
    every: u64,
    buttons: u32,
) -> Vec<Framebuffer> {
    if start_frame > 0 {
        jag.run_frames(start_frame);
    }
    if buttons != 0 {
        jag.set_pad(0, buttons);
    }
    let mut frames = Vec::with_capacity(count as usize);
    for _ in 0..count {
        frames.push(jag.capture_frame());
        jag.run_frames(every.max(1));
    }
    frames
}
