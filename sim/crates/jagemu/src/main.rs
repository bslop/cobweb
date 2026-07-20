//! `jagemu` — the Claude-native Atari Jaguar emulator CLI.
//!
//! Every command prints a single JSON object to stdout (machine-readable for
//! Claude Code) and human notes to stderr. Deterministic, no display server, no
//! Wine, true multi-instance (no global lock).
//!
//! Commands:
//!   jagemu info <rom>                         describe a program
//!   jagemu run <rom> [--frames N]             boot, run N frames, dump state
//!   jagemu screenshot <rom> [--frames N] [-o p.png]   true-OP PNG capture
//!   jagemu disasm <rom> --at ADDR [--count N] [--frames N]
//!   jagemu instances [--prune]                list/prune isolated instances
//!   jagemu version

use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::process::ExitCode;

use jag_core::risc::{Fidelity, TimingStats};
use jag_core::Jaguar;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.is_empty() {
        usage();
        return ExitCode::FAILURE;
    }
    let cmd = args[0].as_str();
    let rest = &args[1..];
    let _ = SD_DIR.set(flag_val(rest, "--sd").map(|s| s.to_string()));
    let result = match cmd {
        "info" => cmd_info(rest),
        "run" => cmd_run(rest),
        "screenshot" | "shot" => cmd_screenshot(rest),
        "video" | "film" => cmd_video(rest),
        "audio" | "sound" => cmd_audio(rest),
        "audiocheck" | "audio-check" => cmd_audiocheck(rest),
        "playtest" => cmd_playtest(rest),
        "disasm" => cmd_disasm(rest),
        "peek" => cmd_peek(rest),
        "dump" => cmd_dump(rest),
        "objects" | "objlist" => cmd_objects(rest),
        "break" => cmd_break(rest),
        "serve" => cmd_serve(rest),
        "ctl" => cmd_ctl(rest),
        "oracle-dump" => cmd_oracle_dump(rest),
        "oracle-diff" => cmd_oracle_diff(rest),
        "instances" => cmd_instances(rest),
        "version" | "--version" | "-V" => {
            println!("{{\"name\":\"jagemu\",\"version\":\"{}\"}}", env!("CARGO_PKG_VERSION"));
            Ok(())
        }
        "help" | "--help" | "-h" => {
            usage();
            Ok(())
        }
        other => Err(format!("unknown command: {other}")),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("jagemu: error: {e}");
            println!("{{\"ok\":false,\"error\":{}}}", jstr(&e));
            ExitCode::FAILURE
        }
    }
}

fn usage() {
    eprintln!(
        "jagemu — Atari Jaguar emulator (Claude-native)\n\
         \n\
         USAGE:\n\
         \x20 jagemu info <rom>\n\
         \x20 jagemu run <rom> [--frames N] [--fidelity functional|silicon|bigpemu]\n\
         \x20 jagemu screenshot <rom> [--frames N] [-o out.png]\n\
         \x20 jagemu video <rom> [--count N] [--every K] [--start S] [--cols C] [--dir D] -o film.png\n\
         \x20 jagemu audio <rom> [--frames N] [--press a] -o out.wav\n\
         \x20 jagemu audiocheck <wav|rom> [--against <wav|rom>] [--frames N] [--press a]\n\
         \x20 jagemu disasm <rom> --at 0xADDR [--count N] [--frames N]\n\
         \x20 jagemu peek <rom> --at 0xADDR [--len N] [--frames N] [--press a]\n\
         \x20 jagemu dump <rom> --at 0xADDR --len N -o file.bin   # full-region export, no cap\n\
         \x20 jagemu break <rom> --at 0xADDR [--frames N] [--press a]\n\
         \x20 jagemu instances [--prune]\n\
         \x20 jagemu version\n\
         \n\
         Live headless session (Claude connects + pulls video/state):\n\
         \x20 jagemu serve --rom <path> [--instance <name>]      # long-running, isolated\n\
         \x20 jagemu ctl <instance> <cmd...>                     # drive it; state persists\n\
         \x20   cmds: ping state run N step N frame f.png video f.png audio f.wav\n\
         \x20         peek A [--len N] poke A b,b,b input a,up release break A continue N reset disasm A stop\n\
         \n\
         Input: --press <a,b,c,up,down,left,right,option,start>  --press-after <frame>"
    );
}

// ── argument helpers ────────────────────────────────────────────────────────

fn flag_val<'a>(args: &'a [String], name: &str) -> Option<&'a str> {
    args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).map(|s| s.as_str())
}

fn has_flag(args: &[String], name: &str) -> bool {
    args.iter().any(|a| a == name)
}

fn positional(args: &[String]) -> Option<&str> {
    args.iter().find(|a| !a.starts_with('-')).map(|s| s.as_str())
}

fn parse_u32(s: &str) -> Result<u32, String> {
    let s = s.trim();
    let v = if let Some(h) = s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")) {
        u32::from_str_radix(h, 16)
    } else if let Some(h) = s.strip_prefix('$') {
        u32::from_str_radix(h, 16)
    } else {
        s.parse()
    };
    v.map_err(|_| format!("invalid number: {s}"))
}

fn parse_u64(s: &str) -> Result<u64, String> {
    s.trim().parse().map_err(|_| format!("invalid number: {s}"))
}

fn load_rom(args: &[String]) -> Result<(String, Vec<u8>), String> {
    let path = positional(args).ok_or("missing <rom> path")?;
    let data = std::fs::read(path).map_err(|e| format!("reading {path}: {e}"))?;
    Ok((path.to_string(), data))
}

fn boot(rom: &[u8], frames: u64, fid: Fidelity) -> Result<Jaguar, String> {
    boot_input(rom, frames, 0, 0, fid)
}

/// Parse `--fidelity functional|silicon|bigpemu` (default functional — the
/// timed profiles are the jsim truth layer, opt-in until hardware-calibrated).
fn fidelity_arg(args: &[String]) -> Result<Fidelity, String> {
    Ok(match flag_val(args, "--fidelity") {
        None => Fidelity::Functional,
        Some(s) => match s.to_ascii_lowercase().as_str() {
            "functional" => Fidelity::Functional,
            "silicon" => Fidelity::Silicon,
            "bigpemu" => Fidelity::BigPEmu,
            other => {
                return Err(format!(
                    "unknown fidelity: {other} (functional|silicon|bigpemu)"
                ))
            }
        },
    })
}

/// Boot, optionally pressing `buttons` (joyedge bit word) on port 1 from frame
/// `press_after` onward (idle before, so edge-sensitive title screens trigger).
/// Host directory attached as the GameDrive SD card (`--sd <dir>`), parsed once
/// in `main` so every boot path picks it up.
static SD_DIR: std::sync::OnceLock<Option<String>> = std::sync::OnceLock::new();

/// Attach the emulated GameDrive if `--sd` was given. Without it the SPI window
/// floats and `gd_install` fails its bounded waits — i.e. "no GameDrive", which
/// is exactly the state a ROM must already handle.
fn attach_sd(jag: &mut Jaguar) {
    if let Some(Some(dir)) = SD_DIR.get() {
        jag.bus.gamedrive = Some(jag_core::gamedrive::GameDrive::new(dir));
    }
}

fn boot_input(
    rom: &[u8],
    frames: u64,
    buttons: u32,
    press_after: u64,
    fid: Fidelity,
) -> Result<Jaguar, String> {
    let mut jag = Jaguar::new();
    jag.load(rom).map_err(|e| e.to_string())?;
    attach_sd(&mut jag);
    jag.gpu.fidelity = fid;
    jag.dsp.fidelity = fid;
    if buttons != 0 && press_after < frames {
        jag.run_frames(press_after);
        jag.set_pad(0, buttons);
        jag.run_frames(frames - press_after);
    } else if frames > 0 {
        jag.run_frames(frames);
    }
    Ok(jag)
}

/// Parse a comma/plus/space-separated button list into a joyedge bit word.
fn parse_buttons(s: &str) -> Result<u32, String> {
    use jag_core::jerry::Button::*;
    let mut w = 0u32;
    for tok in s.split(|c| c == ',' || c == '+' || c == ' ').filter(|t| !t.is_empty()) {
        let bit = match tok.to_ascii_lowercase().as_str() {
            "up" => Up,
            "down" => Down,
            "left" => Left,
            "right" => Right,
            "a" => A,
            "b" => B,
            "c" => C,
            "option" => Option,
            "pause" | "start" => Pause,
            "star" | "*" => Star,
            "hash" | "#" => Hash,
            "0" => K0,
            "1" => K1,
            "2" => K2,
            "3" => K3,
            "4" => K4,
            "5" => K5,
            "6" => K6,
            "7" => K7,
            "8" => K8,
            "9" => K9,
            other => return Err(format!("unknown button: {other}")),
        };
        w |= bit.mask();
    }
    Ok(w)
}

fn press_args(args: &[String]) -> Result<(u32, u64), String> {
    let buttons = match flag_val(args, "--press") {
        Some(s) => parse_buttons(s)?,
        None => 0,
    };
    let after = flag_val(args, "--press-after").map(parse_u64).transpose()?.unwrap_or(0);
    Ok((buttons, after))
}

// ── commands ────────────────────────────────────────────────────────────────

fn cmd_info(args: &[String]) -> Result<(), String> {
    let (path, data) = load_rom(args)?;
    let mut jag = Jaguar::new();
    let cart = jag.load(&data).map_err(|e| e.to_string())?;
    attach_sd(&mut jag);
    let secs: Vec<String> = cart
        .sections
        .iter()
        .map(|s| {
            format!(
                "{{\"name\":{},\"vaddr\":{},\"size\":{},\"loaded\":{}}}",
                jstr(&s.name),
                s.vaddr,
                s.size,
                s.loaded
            )
        })
        .collect();
    println!(
        "{{\"ok\":true,\"path\":{},\"format\":{},\"entry\":{},\"entry_hex\":{},\"sections\":[{}]}}",
        jstr(&path),
        jstr(&format!("{:?}", cart.format)),
        cart.entry,
        jstr(&format!("0x{:06X}", cart.entry)),
        secs.join(",")
    );
    Ok(())
}

/// `symbol.map` / `rln -m` style map: lines with a hex address and a name.
/// Anything unparseable is skipped, so a map from any toolchain is safe to pass.
fn load_map(path: &str) -> Result<Vec<(u32, String)>, String> {
    let txt = std::fs::read_to_string(path).map_err(|e| format!("reading {path}: {e}"))?;
    let mut v: Vec<(u32, String)> = Vec::new();
    for line in txt.lines() {
        let mut addr: Option<u32> = None;
        let mut name: Option<String> = None;
        for tok in line.split_whitespace() {
            let t = tok.trim_start_matches("0x").trim_start_matches('$');
            if addr.is_none() && t.len() >= 4 && t.chars().all(|c| c.is_ascii_hexdigit()) {
                addr = u32::from_str_radix(t, 16).ok();
            } else if addr.is_some() && name.is_none() && !tok.is_empty() {
                name = Some(tok.to_string());
            }
        }
        if let (Some(a), Some(n)) = (addr, name) {
            v.push((a, n));
        }
    }
    v.sort_by_key(|e| e.0);
    Ok(v)
}

/// Nearest preceding symbol, as `name+0xNN`.
fn sym_for(map: &[(u32, String)], addr: u32) -> String {
    match map.binary_search_by_key(&addr, |e| e.0) {
        Ok(i) => map[i].1.clone(),
        // Err(0) means the address precedes every symbol: there is no preceding
        // entry to name it with, and `map[i - 1]` below would underflow.
        Err(0) => String::new(),
        Err(i) => {
            let (a, ref n) = map[i - 1];
            if addr - a > 0x2000 { String::new() } else { format!("{n}+0x{:X}", addr - a) }
        }
    }
}

fn cmd_run(args: &[String]) -> Result<(), String> {
    let (path, data) = load_rom(args)?;
    let frames = flag_val(args, "--frames").map(parse_u64).transpose()?.unwrap_or(60);
    let (btn, after) = press_args(args)?;
    let prof = has_flag(args, "--pc-histogram") || has_flag(args, "--profile68k");
    if prof {
        let top = flag_val(args, "--top").map(parse_u32).transpose()?.unwrap_or(25) as usize;
        let gran = flag_val(args, "--bucket").map(parse_u32).transpose()?.unwrap_or(0);
        let map = match flag_val(args, "--map") {
            Some(m) => load_map(m)?,
            None => Vec::new(),
        };
        let jag = boot_profiled(&data, frames, btn, after, fidelity_arg(args)?, gran, top, &map)?;
        println!(
            "{{\"ok\":true,\"path\":{},\"frames\":{},\"state\":{}}}",
            jstr(&path),
            frames,
            state_json(&jag)
        );
        return Ok(());
    }
    let jag = boot_input(&data, frames, btn, after, fidelity_arg(args)?)?;
    println!(
        "{{\"ok\":true,\"path\":{},\"frames\":{},\"state\":{}}}",
        jstr(&path),
        frames,
        state_json(&jag)
    );
    Ok(())
}

/// Boot with the 68k profiler armed, then print the histogram to stderr (stdout
/// stays a single JSON object, as every other command guarantees).
#[allow(clippy::too_many_arguments)]
fn boot_profiled(
    rom: &[u8],
    frames: u64,
    buttons: u32,
    press_after: u64,
    fid: Fidelity,
    gran: u32,
    top: usize,
    map: &[(u32, String)],
) -> Result<Jaguar, String> {
    let mut jag = Jaguar::new();
    jag.load(rom).map_err(|e| e.to_string())?;
    attach_sd(&mut jag);
    jag.gpu.fidelity = fid;
    jag.dsp.fidelity = fid;
    jag.dbg.prof = Some(Box::new(jag_core::debug::Profile::new()));
    if buttons != 0 && press_after < frames {
        jag.run_frames(press_after);
        jag.set_pad(0, buttons);
        jag.run_frames(frames - press_after);
    } else if frames > 0 {
        jag.run_frames(frames);
    }
    let p = jag.dbg.prof.as_ref().unwrap();
    let awake = p.main_cycles + p.isr_cycles;
    let tot = p.total_cycles.max(1);
    eprintln!("=== 68k cycle profile ({frames} frames) ===");
    eprintln!(
        "  total charged   {:>12}",
        p.total_cycles
    );
    eprintln!(
        "  asleep in STOP  {:>12}  {:5.1}%   (waiting on an interrupt — not frame cost)",
        p.stopped_cycles,
        100.0 * p.stopped_cycles as f64 / tot as f64
    );
    eprintln!(
        "  awake           {:>12}  {:5.1}%",
        awake,
        100.0 * awake as f64 / tot as f64
    );
    eprintln!(
        "    in vblank ISR {:>12}  {:5.1}% of awake   ({} instrs)",
        p.isr_cycles,
        100.0 * p.isr_cycles as f64 / awake.max(1) as f64,
        p.isr_instrs
    );
    eprintln!(
        "    main line     {:>12}  {:5.1}% of awake   ({} instrs)",
        p.main_cycles,
        100.0 * p.main_cycles as f64 / awake.max(1) as f64,
        p.main_instrs
    );
    // Wall-clock accounting (COBWEB_REQ_wall_clock_accounting.md): per-core
    // *cycles* cannot express "who was holding wall-clock time". These are
    // fractions of the SAME elapsed wall clock, so they legitimately sum past
    // 100% — the masters run concurrently, and that overlap is the point.
    let hz = 26_590_906.0_f64;
    let wall = frames as f64 / 59.94;
    let wall_cyc = wall * hz;
    let gpu_c = jag.gpu.cycles as f64;
    let dsp_c = jag.dsp.cycles as f64;
    let blit_c = jag.gpu.pipe.stats.blit as f64;
    let m68k_hz = 13_295_453.0_f64;
    let awake_wall = awake as f64 / m68k_hz;
    eprintln!("\n=== wall-clock accounting ({wall:.2} s simulated) ===");
    eprintln!(
        "  68000 awake     {:>8.3} s  {:5.1}%",
        awake_wall,
        100.0 * awake_wall / wall
    );
    eprintln!(
        "  Tom GPU busy    {:>8.3} s  {:5.1}%   (of which Blitter {:.3} s, {:.1}%)",
        gpu_c / hz,
        100.0 * gpu_c / wall_cyc,
        blit_c / hz,
        100.0 * blit_c / wall_cyc
    );
    eprintln!(
        "  Jerry DSP busy  {:>8.3} s  {:5.1}%",
        dsp_c / hz,
        100.0 * dsp_c / wall_cyc
    );

    let rows = if gran > 0 { p.top_buckets(gran, top) } else { p.top(top) };
    eprintln!(
        "\n  {:<10} {:>12} {:>7} {:>12}  {}",
        if gran > 0 { "bucket" } else { "pc" },
        "cycles",
        "% awake",
        "instrs",
        "symbol"
    );
    for (pc, cyc, n) in rows {
        eprintln!(
            "  0x{:06X}   {:>12} {:>6.2}% {:>12}  {}",
            pc,
            cyc,
            100.0 * cyc as f64 / awake.max(1) as f64,
            n,
            sym_for(map, pc)
        );
    }
    Ok(jag)
}

fn cmd_screenshot(args: &[String]) -> Result<(), String> {
    let (path, data) = load_rom(args)?;
    let frames = flag_val(args, "--frames").map(parse_u64).transpose()?.unwrap_or(60);
    let out = flag_val(args, "-o")
        .or_else(|| flag_val(args, "--out"))
        .map(|s| s.to_string())
        .unwrap_or_else(|| "screenshot.png".to_string());
    let (btn, after) = press_args(args)?;
    let jag = boot_input(&data, frames, btn, after, fidelity_arg(args)?)?;
    let fb = jag.capture_frame();
    let png = jag_headless::png::encode_rgba(fb.width, fb.height, &fb.rgba);
    std::fs::write(&out, &png).map_err(|e| format!("writing {out}: {e}"))?;
    eprintln!("jagemu: wrote {} ({}x{}, {} bytes)", out, fb.width, fb.height, png.len());
    println!(
        "{{\"ok\":true,\"path\":{},\"frames\":{},\"out\":{},\"width\":{},\"height\":{},\"png_bytes\":{},\"state\":{}}}",
        jstr(&path),
        frames,
        jstr(&out),
        fb.width,
        fb.height,
        png.len(),
        state_json(&jag)
    );
    Ok(())
}

fn cmd_video(args: &[String]) -> Result<(), String> {
    let (path, data) = load_rom(args)?;
    let count = flag_val(args, "--count").map(parse_u32).transpose()?.unwrap_or(16).clamp(1, 256);
    let every = flag_val(args, "--every").map(parse_u64).transpose()?.unwrap_or(8);
    let start = flag_val(args, "--start").map(parse_u64).transpose()?.unwrap_or(0);
    let cols = flag_val(args, "--cols").map(parse_u32).transpose()?.unwrap_or(4).max(1);
    let (btn, _after) = press_args(args)?;
    let out = flag_val(args, "-o").or_else(|| flag_val(args, "--out")).unwrap_or("video.png");
    let dir = flag_val(args, "--dir");

    let mut jag = Jaguar::new();
    jag.load(&data).map_err(|e| e.to_string())?;
    attach_sd(&mut jag);
    let frames = jag_headless::capture_sequence(&mut jag, start, count, every, btn);

    // Optionally write each frame individually.
    if let Some(d) = dir {
        std::fs::create_dir_all(d).map_err(|e| e.to_string())?;
        for (i, f) in frames.iter().enumerate() {
            let p = format!("{d}/frame_{i:04}.png");
            let png = jag_headless::png::encode_rgba(f.width, f.height, &f.rgba);
            std::fs::write(&p, &png).map_err(|e| e.to_string())?;
        }
    }

    // Optional: a real animated PNG (scrubbable video file).
    if let Some(anim) = flag_val(args, "--anim") {
        let fps = flag_val(args, "--fps").map(parse_u32).transpose()?.unwrap_or(12).max(1);
        if let Some(f0) = frames.first() {
            let (fw, fh) = (f0.width, f0.height);
            let bufs: Vec<Vec<u8>> =
                frames.iter().filter(|f| f.width == fw && f.height == fh).map(|f| f.rgba.clone()).collect();
            let apng = jag_headless::png::encode_apng(fw, fh, &bufs, 1, fps as u16, 0);
            std::fs::write(anim, &apng).map_err(|e| format!("writing {anim}: {e}"))?;
            eprintln!("jagemu: animated {anim} ({}x{}, {} frames @ {} fps)", fw, fh, bufs.len(), fps);
        }
    }

    // The filmstrip montage: one image showing motion across all captured frames.
    let (w, h, rgba) = jag_headless::filmstrip(&frames, cols, 2);
    let png = jag_headless::png::encode_rgba(w, h, &rgba);
    std::fs::write(out, &png).map_err(|e| format!("writing {out}: {e}"))?;
    eprintln!(
        "jagemu: filmstrip {out} ({}x{}, {} frames, every {} frames from {})",
        w, h, frames.len(), every, start
    );
    println!(
        "{{\"ok\":true,\"path\":{},\"out\":{},\"frames\":{},\"every\":{},\"start\":{},\"width\":{},\"height\":{},\"state\":{}}}",
        jstr(&path),
        jstr(out),
        frames.len(),
        every,
        start,
        w,
        h,
        state_json(&jag)
    );
    Ok(())
}

/// Autonomous playtest: drive the game on an input timeline, capture frames,
/// flag anomalies ("parts that don't look right"), and emit a report + filmstrip
/// for Claude to review and act on.
fn cmd_playtest(args: &[String]) -> Result<(), String> {
    let (path, data) = load_rom(args)?;
    // Default generously: slow loaders (large 3D and scrolling ports) only
    // reach their first rendered frame around ~500, so a shorter run
    // false-flags them.
    let frames = flag_val(args, "--frames").map(parse_u64).transpose()?.unwrap_or(540);
    let shots = flag_val(args, "--shots").map(parse_u32).transpose()?.unwrap_or(16).clamp(2, 64);
    let out = flag_val(args, "-o").or_else(|| flag_val(args, "--out")).unwrap_or("playtest.png");

    // Input timeline: "frame:buttons,frame:buttons". Default: press A at ~1s to
    // get past a title, then idle (most games animate or attract on their own).
    let mut events: Vec<(u64, u32)> = Vec::new();
    if let Some(script) = flag_val(args, "--script") {
        for tok in script.split(',').filter(|t| !t.is_empty()) {
            let (f, b) = tok.split_once(':').ok_or("script item must be frame:buttons")?;
            events.push((parse_u64(f)?, parse_buttons(b)?));
        }
    } else {
        events.push((60, parse_buttons("a").unwrap_or(0)));
        events.push((75, 0)); // release (edge press)
    }
    events.sort_by_key(|e| e.0);

    let mut jag = Jaguar::new();
    jag.load(&data).map_err(|e| e.to_string())?;
    attach_sd(&mut jag);

    // Capture `shots` frames evenly across the run, applying inputs on schedule.
    let mut frames_out: Vec<jag_core::Framebuffer> = Vec::new();
    let mut metrics: Vec<jag_headless::FrameMetrics> = Vec::new();
    let mut ev = 0usize;
    let mut prev: Option<jag_core::Framebuffer> = None;
    for s in 0..shots {
        let target = frames * (s as u64 + 1) / shots as u64;
        while jag.frame() < target {
            // Apply any input events due before the next frame.
            while ev < events.len() && events[ev].0 <= jag.frame() {
                jag.set_pad(0, events[ev].1);
                ev += 1;
            }
            jag.run_frames(1);
        }
        let fb = jag.capture_frame();
        let m = jag_headless::frame_metrics(&fb, prev.as_ref());
        metrics.push(m);
        prev = Some(fb.clone());
        frames_out.push(fb);
    }

    // Filmstrip for visual review.
    let (w, h, rgba) = jag_headless::filmstrip(&frames_out, 4, 2);
    let png = jag_headless::png::encode_rgba(w, h, &rgba);
    std::fs::write(out, &png).map_err(|e| format!("writing {out}: {e}"))?;

    // Aggregate verdict.
    let n = metrics.len().max(1);
    let black = metrics.iter().filter(|m| m.flags.contains(&"black")).count();
    let frozen = metrics.iter().filter(|m| m.flags.contains(&"frozen")).count();
    let any_motion = metrics.iter().any(|m| m.changed_pct > 0.001);
    let any_content = metrics.iter().any(|m| m.nonblack_pct > 0.02);
    let illegal = jag.cpu.illegal_count;

    let (verdict, mut issues): (&str, Vec<String>) = if black == n {
        ("broken_black", vec!["every captured frame is black — nothing renders".into()])
    } else if !any_content {
        ("broken_empty", vec!["frames are essentially empty (almost no non-black pixels)".into()])
    } else if !any_motion {
        ("static", vec!["content is present but never changes — possible freeze/hang or a static screen".into()])
    } else {
        ("plausible", Vec::new())
    };
    if illegal > 0 {
        issues.push(format!("CPU hit {illegal} illegal/unimplemented opcode(s)"));
    }
    if black > 0 && black < n {
        issues.push(format!("{black}/{n} captured frames are black"));
    }
    if frozen > n / 2 && verdict != "static" {
        issues.push(format!("{frozen}/{n} frames identical to the previous (stuttery/low motion)"));
    }

    let detail: Vec<String> = metrics
        .iter()
        .enumerate()
        .map(|(i, m)| {
            let flags: Vec<String> = m.flags.iter().map(|f| jstr(f)).collect();
            format!(
                "{{\"shot\":{},\"nonblack_pct\":{:.3},\"colors\":{},\"changed_pct\":{:.3},\"flags\":[{}]}}",
                i, m.nonblack_pct, m.distinct_colors, m.changed_pct, flags.join(",")
            )
        })
        .collect();
    let issues_j: Vec<String> = issues.iter().map(|s| jstr(s)).collect();

    eprintln!(
        "jagemu: playtest {} → verdict={} ({} shots, filmstrip {})",
        std::path::Path::new(&path).file_name().and_then(|s| s.to_str()).unwrap_or("?"),
        verdict, shots, out
    );
    for iss in &issues {
        eprintln!("  ⚠ {iss}");
    }
    println!(
        "{{\"ok\":true,\"path\":{},\"frames\":{},\"shots\":{},\"filmstrip\":{},\"verdict\":{},\
         \"issues\":[{}],\"detail\":[{}]}}",
        jstr(&path), frames, shots, jstr(out), jstr(verdict),
        issues_j.join(","), detail.join(",")
    );
    Ok(())
}

fn cmd_audio(args: &[String]) -> Result<(), String> {
    let (path, data) = load_rom(args)?;
    let frames = flag_val(args, "--frames").map(parse_u64).transpose()?.unwrap_or(180);
    let (btn, after) = press_args(args)?;
    let out = flag_val(args, "-o").or_else(|| flag_val(args, "--out")).unwrap_or("audio.wav");
    let mut jag = Jaguar::new();
    jag.load(&data).map_err(|e| e.to_string())?;
    attach_sd(&mut jag);
    let (rate, samples, wav) = jag_headless::capture_audio(&mut jag, frames, btn, after);
    std::fs::write(out, &wav).map_err(|e| format!("writing {out}: {e}"))?;
    let (peak, rms) = jag_headless::wav::stats(&samples);
    eprintln!(
        "jagemu: wrote {out} ({} stereo samples @ {} Hz, peak={}, rms={:.0})",
        samples.len() / 2, rate, peak, rms
    );
    println!(
        "{{\"ok\":true,\"path\":{},\"out\":{},\"sample_rate\":{},\"samples\":{},\"peak\":{},\"rms\":{:.1},\"silent\":{}}}",
        jstr(&path), jstr(out), rate, samples.len() / 2, peak, rms, peak == 0
    );
    Ok(())
}

/// Load `path` as an audio capture: a `.wav` is decoded directly; anything
/// else is treated as a ROM, booted, and captured with the shared
/// `--frames`/`--press`/`--press-after` arguments.
fn audio_source(path: &str, args: &[String]) -> Result<(u32, u16, Vec<i16>), String> {
    let bytes = std::fs::read(path).map_err(|e| format!("{path}: {e}"))?;
    if bytes.len() >= 4 && &bytes[0..4] == b"RIFF" {
        return jag_headless::wav::decode_pcm16(&bytes).map_err(|e| format!("{path}: {e}"));
    }
    let frames = flag_val(args, "--frames").map(parse_u64).transpose()?.unwrap_or(400);
    let (btn, after) = press_args(args)?;
    let mut jag = Jaguar::new();
    jag.load(&bytes).map_err(|e| format!("{path}: {e}"))?;
    attach_sd(&mut jag);
    let (rate, samples, _) = jag_headless::capture_audio(&mut jag, frames, btn, after);
    Ok((rate, 2, samples))
}

/// `jagemu audiocheck <wav|rom> [--against <wav|rom>]` — the audio counterpart
/// of the screenshot pixel-diff. Alone: a health report (silence, DC,
/// clipping, dropouts, spectral peaks). With `--against`: lag-aligned
/// comparison of loudness envelope + spectrum against a reference capture
/// (builds boot at different speeds; the lag is measured, not assumed).
fn cmd_audiocheck(args: &[String]) -> Result<(), String> {
    // first true positional: skip flags AND their values
    const VALUE_FLAGS: &[&str] =
        &["--against", "--frames", "--press", "--press-after", "--sd", "-o", "--out"];
    let mut target = None;
    let mut i = 0;
    while i < args.len() {
        let a = args[i].as_str();
        if VALUE_FLAGS.contains(&a) {
            i += 2;
            continue;
        }
        if a.starts_with('-') {
            i += 1;
            continue;
        }
        target = Some(args[i].clone());
        break;
    }
    let target = target.ok_or("audiocheck needs a .wav capture or a ROM")?;
    let (rate, ch, samples) = audio_source(&target, args)?;
    let a = jag_headless::wav::analyze(rate, ch, &samples);

    let peaks_json: Vec<String> = a
        .spectral_peaks
        .iter()
        .map(|(hz, db)| format!("{{\"hz\":{hz:.1},\"db\":{db:.1}}}"))
        .collect();
    let dc_json: Vec<String> = a.dc_offset.iter().map(|d| format!("{d:.4}")).collect();
    let mut json = format!(
        "{{\"ok\":true,\"capture\":{},\"sample_rate\":{},\"duration_s\":{:.2},\
         \"silent\":{},\"peak_dbfs\":{:.1},\"rms_dbfs\":{:.1},\"clipped_samples\":{},\
         \"dc_offset\":[{}],\"silence_ratio\":{:.3},\"leading_silence_s\":{:.2},\
         \"longest_gap_s\":{:.2},\"channel_correlation\":{},\"spectral_peaks\":[{}]",
        jstr(&target),
        rate,
        a.duration_s,
        a.silent,
        a.peak_dbfs,
        a.rms_dbfs,
        a.clipped,
        dc_json.join(","),
        a.silence_ratio,
        a.leading_silence_s,
        a.longest_gap_s,
        a.channel_correlation.map(|c| format!("{c:.4}")).unwrap_or("null".into()),
        peaks_json.join(","),
    );
    eprintln!(
        "jagemu: {} — {:.1}s @ {} Hz, peak {:.1} dBFS, rms {:.1} dBFS, {}% silent{}",
        target,
        a.duration_s,
        rate,
        a.peak_dbfs,
        a.rms_dbfs,
        (a.silence_ratio * 100.0).round(),
        if a.silent { " (NO AUDIO AT ALL)" } else { "" }
    );

    if let Some(refpath) = flag_val(args, "--against") {
        let (rr, rc, rsamples) = audio_source(refpath, args)?;
        let c = jag_headless::wav::compare((rate, ch, &samples), (rr, rc, &rsamples))?;
        json.push_str(&format!(
            ",\"against\":{},\"lag_s\":{:.2},\"envelope_correlation\":{:.4},\
             \"envelope_mae_db\":{:.2},\"spectral_mae_db\":{:.2},\"matches\":{}",
            jstr(refpath),
            c.lag_s,
            c.envelope_correlation,
            c.envelope_mae_db,
            c.spectral_mae_db,
            c.matches
        ));
        eprintln!(
            "jagemu: vs {} — lag {:+.2}s, envelope corr {:.3}, spectral MAE {:.1} dB → {}",
            refpath,
            c.lag_s,
            c.envelope_correlation,
            c.spectral_mae_db,
            if c.matches { "MATCH" } else { "MISMATCH" }
        );
    }
    json.push('}');
    println!("{json}");
    Ok(())
}

fn cmd_disasm(args: &[String]) -> Result<(), String> {
    let (_path, data) = load_rom(args)?;
    let frames = flag_val(args, "--frames").map(parse_u64).transpose()?.unwrap_or(0);
    let count = flag_val(args, "--count").map(parse_u32).transpose()?.unwrap_or(16) as usize;
    let jag = boot(&data, frames, fidelity_arg(args)?)?;
    let at = match flag_val(args, "--at") {
        Some(s) => parse_u32(s)?,
        None => jag.cpu.pc,
    };
    // --gpu / --dsp select the JRISC disassembler; default is 68000.
    let insns = if args.iter().any(|a| a == "--gpu") {
        jag_debug::disasm_jrisc_range(&jag.bus, at, count, false)
    } else if args.iter().any(|a| a == "--dsp") {
        jag_debug::disasm_jrisc_range(&jag.bus, at, count, true)
    } else {
        jag_debug::disasm_range(&jag.bus, at, count)
    };
    let items: Vec<String> = insns
        .iter()
        .map(|i| format!("{{\"addr\":{},\"text\":{}}}", i.addr, jstr(&i.text)))
        .collect();
    println!("{{\"ok\":true,\"at\":{},\"insns\":[{}]}}", at, items.join(","));
    Ok(())
}

fn cmd_peek(args: &[String]) -> Result<(), String> {
    let (_path, data) = load_rom(args)?;
    let frames = flag_val(args, "--frames").map(parse_u64).transpose()?.unwrap_or(0);
    let (btn, after) = press_args(args)?;
    let at = parse_u32(flag_val(args, "--at").ok_or("peek needs --at ADDR")?)?;
    let out = flag_val(args, "--out");
    // Raw-file dumps can be large; interactive hex dumps stay bounded at 4 KB.
    let cap = if out.is_some() { 0x20_0000 } else { 4096 };
    let len = flag_val(args, "--len").map(parse_u32).transpose()?.unwrap_or(64).min(cap);
    let jag = boot_input(&data, frames, btn, after, fidelity_arg(args)?)?;
    let mut buf = vec![0u8; len as usize];
    jag.bus.peek(at, &mut buf);
    if let Some(path) = out {
        std::fs::write(path, &buf).map_err(|e| e.to_string())?;
        println!(
            "{{\"ok\":true,\"at\":{},\"at_hex\":{},\"len\":{},\"out\":{}}}",
            at, jstr(&format!("0x{at:06X}")), len, jstr(path)
        );
        return Ok(());
    }
    // Human hex dump on stderr.
    for (i, chunk) in buf.chunks(16).enumerate() {
        let hex: Vec<String> = chunk.iter().map(|b| format!("{b:02X}")).collect();
        let ascii: String = chunk
            .iter()
            .map(|&b| if (0x20..0x7F).contains(&b) { b as char } else { '.' })
            .collect();
        eprintln!("  {:06X}  {:<48}  {}", at + (i * 16) as u32, hex.join(" "), ascii);
    }
    let bytes: Vec<String> = buf.iter().map(|b| b.to_string()).collect();
    println!(
        "{{\"ok\":true,\"at\":{},\"at_hex\":{},\"len\":{},\"bytes\":[{}]}}",
        at,
        jstr(&format!("0x{at:06X}")),
        len,
        bytes.join(",")
    );
    Ok(())
}

/// Boot N frames, then dump the live Object Processor list (decoded) plus a few
/// key TOM registers. The AI-eyes window into what the OP is being asked to draw.
/// Dump an arbitrary memory region to a file — no size cap (unlike the
/// interactive `peek` hex view), for full-framebuffer parity exports etc.
fn cmd_dump(args: &[String]) -> Result<(), String> {
    let (_path, data) = load_rom(args)?;
    let frames = flag_val(args, "--frames").map(parse_u64).transpose()?.unwrap_or(0);
    let (btn, after) = press_args(args)?;
    let at = parse_u32(flag_val(args, "--at").ok_or("dump needs --at ADDR")?)?;
    let len = parse_u32(flag_val(args, "--len").ok_or("dump needs --len N")?)?;
    let out = flag_val(args, "-o").or_else(|| flag_val(args, "--out")).ok_or("dump needs -o FILE")?;
    let jag = boot_input(&data, frames, btn, after, fidelity_arg(args)?)?;
    let mut buf = vec![0u8; len as usize];
    jag.bus.peek(at, &mut buf);
    std::fs::write(out, &buf).map_err(|e| e.to_string())?;
    println!(
        "{{\"ok\":true,\"at\":{},\"at_hex\":{},\"len\":{},\"out\":{}}}",
        at, jstr(&format!("0x{at:06X}")), len, jstr(out)
    );
    eprintln!("jagemu: dumped {len} bytes at 0x{at:06X} -> {out}");
    Ok(())
}

fn cmd_objects(args: &[String]) -> Result<(), String> {
    let (path, data) = load_rom(args)?;
    let frames = flag_val(args, "--frames").map(parse_u64).transpose()?.unwrap_or(0);
    let (btn, after) = press_args(args)?;
    let jag = boot_input(&data, frames, btn, after, fidelity_arg(args)?)?;
    let objs = jag_core::tom::dump_object_list(&jag.bus);
    let r16 = |a: u32| -> u16 {
        let mut b = [0u8; 2];
        jag.bus.peek(a, &mut b);
        u16::from_be_bytes(b)
    };
    let olp = ((r16(jag_core::mem::OLPH) as u32) << 16) | r16(jag_core::mem::OLP) as u32;
    println!(
        "{{\"ok\":true,\"path\":{},\"frames\":{},\"olp\":\"0x{:06X}\",\"vmode\":\"0x{:04X}\",\
         \"bg\":\"0x{:04X}\",\"vdb\":{},\"objects\":{}}}",
        jstr(&path),
        frames,
        olp,
        r16(jag_core::mem::VMODE),
        r16(jag_core::mem::BG),
        r16(jag_core::mem::VDB),
        objs
    );
    Ok(())
}

fn cmd_break(args: &[String]) -> Result<(), String> {
    let (path, data) = load_rom(args)?;
    let frames = flag_val(args, "--frames").map(parse_u64).transpose()?.unwrap_or(600);
    let (btn, after) = press_args(args)?;
    let at = parse_u32(flag_val(args, "--at").ok_or("break needs --at ADDR")?)?;
    // Which core to break on (default 68000).
    let core = if args.iter().any(|a| a == "--gpu") {
        "gpu"
    } else if args.iter().any(|a| a == "--dsp") {
        "dsp"
    } else {
        "68k"
    };
    let mut jag = Jaguar::new();
    jag.load(&data).map_err(|e| e.to_string())?;
    attach_sd(&mut jag);
    jag.gpu.fidelity = fidelity_arg(args)?;
    jag.dsp.fidelity = fidelity_arg(args)?;
    // Apply input timing: idle to `after`, press, then arm the breakpoint so we
    // stop at the first hit during real gameplay rather than during boot.
    if btn != 0 && after > 0 {
        jag.run_frames(after.min(frames));
        jag.set_pad(0, btn);
    } else if btn != 0 {
        jag.set_pad(0, btn);
    }
    match core {
        "gpu" => {
            jag.gpu.breakpoints.insert(at);
        }
        "dsp" => {
            jag.dsp.breakpoints.insert(at);
        }
        _ => jag.dbg.add_breakpoint(at),
    }
    let target = jag.frame() + frames;
    let reason = jag.run_to_frame(target);
    let (kind, hit) = match reason {
        jag_core::StopReason::Breakpoint(pc) => ("breakpoint", Some(pc)),
        jag_core::StopReason::GpuBreakpoint(pc) => ("gpu_breakpoint", Some(pc)),
        jag_core::StopReason::DspBreakpoint(pc) => ("dsp_breakpoint", Some(pc)),
        jag_core::StopReason::ReachedFrame(_) => ("frame_limit", None),
        _ => ("stopped", None),
    };
    // Disassemble a few instructions at the hit PC in the right ISA.
    let disasm = hit.map(|pc| {
        let insns = match core {
            "gpu" => jag_debug::disasm_jrisc_range(&jag.bus, pc, 6, false),
            "dsp" => jag_debug::disasm_jrisc_range(&jag.bus, pc, 6, true),
            _ => jag_debug::disasm_range(&jag.bus, pc, 6),
        };
        let items: Vec<String> = insns
            .iter()
            .map(|i| format!("{{\"addr\":{},\"text\":{}}}", i.addr, jstr(&i.text)))
            .collect();
        format!("[{}]", items.join(","))
    });
    // --trace N: after a RISC breakpoint, single-step the core N instructions and
    // record the PC path (with disassembly) to trace where control flows next.
    let trace = if hit.is_some() && core != "68k" {
        if let Some(n) = flag_val(args, "--trace").map(parse_u32).transpose()? {
            let is_dsp = core == "dsp";
            let pcs = jag.trace_risc(is_dsp, n as usize);
            let items: Vec<String> = pcs
                .iter()
                .map(|&pc| {
                    let (text, _) = jag_debug::disasm_jrisc(
                        m16(&jag.bus, pc),
                        m16(&jag.bus, pc.wrapping_add(2)),
                        m16(&jag.bus, pc.wrapping_add(4)),
                        pc,
                        is_dsp,
                    );
                    format!("{{\"pc\":\"0x{pc:06X}\",\"text\":{}}}", jstr(&text))
                })
                .collect();
            Some(format!("[{}]", items.join(",")))
        } else {
            None
        }
    } else {
        None
    };
    let _ = &trace;
    println!(
        "{{\"ok\":true,\"path\":{},\"core\":{},\"stop\":{},\"hit_pc\":{},\"disasm\":{},\"trace\":{},\"state\":{}}}",
        jstr(&path),
        jstr(core),
        jstr(kind),
        hit.map(|p| format!("\"0x{p:06X}\"")).unwrap_or_else(|| "null".to_string()),
        disasm.unwrap_or_else(|| "null".to_string()),
        trace.unwrap_or_else(|| "null".to_string()),
        state_json(&jag)
    );
    Ok(())
}

/// Read a big-endian 16-bit word from the bus (side-effect-free).
fn m16(bus: &jag_core::Bus, addr: u32) -> u16 {
    let mut b = [0u8; 2];
    bus.peek(addr, &mut b);
    u16::from_be_bytes(b)
}

// ── BigPEmu oracle: dump identical state for parity diffing ─────────────────

const ORACLE_MAGIC: u32 = 0x4A41_474F; // 'JAGO'
const ORACLE_CHUNKS: u32 = 64;

/// Produce the oracle dump (same binary layout as `oracle.c`): big-endian u32s —
/// magic, frame, line, 68k (PC,D0-7,A0-7), GPU (PC,CTRL,FLAGS,R0-31),
/// DSP (PC,CTRL,FLAGS,R0-31), num_chunks, then per-chunk FNV-1a hashes of DRAM.
fn oracle_dump_bytes(jag: &Jaguar) -> Vec<u8> {
    let mut w: Vec<u32> = Vec::with_capacity(160);
    w.push(ORACLE_MAGIC);
    w.push(jag.frame() as u32);
    w.push(jag.sched.line());
    // 68k
    w.push(jag.cpu.pc);
    w.extend_from_slice(&jag.cpu.d);
    w.extend_from_slice(&jag.cpu.a);
    // GPU
    w.push(jag.gpu.pc);
    w.push(jag.gpu.ctrl);
    w.push(jag.gpu.flags);
    let gb = jag.gpu.cur_bank();
    w.extend_from_slice(&jag.gpu.regs[gb]);
    // DSP
    w.push(jag.dsp.pc);
    w.push(jag.dsp.ctrl);
    w.push(jag.dsp.flags);
    let db = jag.dsp.cur_bank();
    w.extend_from_slice(&jag.dsp.regs[db]);
    // DRAM divergence map: FNV-1a of each 32 KB chunk.
    w.push(ORACLE_CHUNKS);
    let chunk = jag.bus.dram.len() / ORACLE_CHUNKS as usize;
    for c in 0..ORACLE_CHUNKS as usize {
        let mut h: u32 = 0x811c_9dc5;
        for &b in &jag.bus.dram[c * chunk..(c + 1) * chunk] {
            h ^= b as u32;
            h = h.wrapping_mul(16777619);
        }
        w.push(h);
    }
    let mut out = Vec::with_capacity(w.len() * 4);
    for v in w {
        out.extend_from_slice(&v.to_be_bytes());
    }
    out
}

fn cmd_oracle_dump(args: &[String]) -> Result<(), String> {
    let (path, data) = load_rom(args)?;
    let frames = flag_val(args, "--frames").map(parse_u64).transpose()?.unwrap_or(200);
    let (btn, after) = press_args(args)?;
    let out = flag_val(args, "-o").or_else(|| flag_val(args, "--out")).unwrap_or("jagemu_oracle.bin");
    let jag = boot_input(&data, frames, btn, after, fidelity_arg(args)?)?;
    let bytes = oracle_dump_bytes(&jag);
    std::fs::write(out, &bytes).map_err(|e| format!("writing {out}: {e}"))?;
    eprintln!("jagemu: oracle dump {out} ({} bytes, frame {})", bytes.len(), jag.frame());
    println!(
        "{{\"ok\":true,\"path\":{},\"out\":{},\"frame\":{},\"bytes\":{}}}",
        jstr(&path), jstr(out), jag.frame(), bytes.len()
    );
    Ok(())
}

#[allow(dead_code)] // parse DTO: some fields are informational, not diffed
struct OracleDump {
    frame: u32,
    line: u32,
    cpu_pc: u32,
    d: [u32; 8],
    a: [u32; 8],
    gpu_pc: u32,
    gpu_ctrl: u32,
    gpu_flags: u32,
    gpu_r: [u32; 32],
    dsp_pc: u32,
    dsp_ctrl: u32,
    dsp_flags: u32,
    dsp_r: [u32; 32],
    chunks: Vec<u32>,
}

fn parse_oracle(bytes: &[u8]) -> Result<OracleDump, String> {
    let mut w = Vec::with_capacity(bytes.len() / 4);
    for c in bytes.chunks_exact(4) {
        w.push(u32::from_be_bytes([c[0], c[1], c[2], c[3]]));
    }
    if w.len() < 91 || w[0] != ORACLE_MAGIC {
        return Err("not an oracle dump (bad magic/length)".into());
    }
    let mut i = 1usize;
    let take = |w: &[u32], i: &mut usize| -> u32 {
        let v = w[*i];
        *i += 1;
        v
    };
    let frame = take(&w, &mut i);
    let line = take(&w, &mut i);
    let cpu_pc = take(&w, &mut i);
    let mut d = [0u32; 8];
    for x in &mut d {
        *x = take(&w, &mut i);
    }
    let mut a = [0u32; 8];
    for x in &mut a {
        *x = take(&w, &mut i);
    }
    let gpu_pc = take(&w, &mut i);
    let gpu_ctrl = take(&w, &mut i);
    let gpu_flags = take(&w, &mut i);
    let mut gpu_r = [0u32; 32];
    for x in &mut gpu_r {
        *x = take(&w, &mut i);
    }
    let dsp_pc = take(&w, &mut i);
    let dsp_ctrl = take(&w, &mut i);
    let dsp_flags = take(&w, &mut i);
    let mut dsp_r = [0u32; 32];
    for x in &mut dsp_r {
        *x = take(&w, &mut i);
    }
    let nchunks = take(&w, &mut i) as usize;
    let avail = w.len().saturating_sub(i);
    let mut chunks = Vec::with_capacity(nchunks);
    for _ in 0..nchunks.min(avail) {
        chunks.push(take(&w, &mut i));
    }
    Ok(OracleDump {
        frame, line, cpu_pc, d, a, gpu_pc, gpu_ctrl, gpu_flags, gpu_r,
        dsp_pc, dsp_ctrl, dsp_flags, dsp_r, chunks,
    })
}

/// Is a RISC PC inside its SRAM (i.e. the core is actively executing)?
fn gpu_running(pc: u32) -> bool {
    (0xF0_3000..0xF0_4000).contains(&pc)
}
fn dsp_running(pc: u32) -> bool {
    (0xF1_B000..0xF1_D000).contains(&pc)
}

fn cmd_oracle_diff(args: &[String]) -> Result<(), String> {
    // oracle-diff <bigpemu.bin> <jagemu.bin>
    let pa = nth_pos(args, 0).ok_or("oracle-diff needs <bigpemu.bin> <jagemu.bin>")?;
    let pb = nth_pos(args, 1).ok_or("oracle-diff needs two dumps")?;
    let a = parse_oracle(&std::fs::read(pa).map_err(|e| format!("{pa}: {e}"))?)?;
    let b = parse_oracle(&std::fs::read(pb).map_err(|e| format!("{pb}: {e}"))?)?;

    let dreg_diffs: Vec<usize> = (0..8).filter(|&i| a.d[i] != b.d[i]).collect();
    let areg_diffs: Vec<usize> = (0..8).filter(|&i| a.a[i] != b.a[i]).collect();
    let gpu_reg_diffs: Vec<usize> = (0..32).filter(|&i| a.gpu_r[i] != b.gpu_r[i]).collect();
    let dsp_reg_diffs: Vec<usize> = (0..32).filter(|&i| a.dsp_r[i] != b.dsp_r[i]).collect();
    let n = a.chunks.len().min(b.chunks.len());
    let chunk_diffs: Vec<usize> = (0..n).filter(|&i| a.chunks[i] != b.chunks[i]).collect();
    let chunk_sz = 0x20_0000u32 / ORACLE_CHUNKS;

    // RISCGO (ctrl bit 0) is the authoritative "is the core started" signal.
    let gpu_go = |c: u32| c & 1 != 0;
    // Human report on stderr.
    eprintln!("=== oracle diff: BigPEmu (A) vs jagemu (B) ===");
    eprintln!("  frame      A={} (line {}) B={} (line {})", a.frame, a.line, b.frame, b.line);
    eprintln!("  68k PC     A=0x{:06X} B=0x{:06X} {}", a.cpu_pc, b.cpu_pc, mark(a.cpu_pc == b.cpu_pc));
    eprintln!(
        "  GPU        A: go={} pc=0x{:06X} flags=0x{:08X}   B: go={} pc=0x{:06X} flags=0x{:08X}",
        gpu_go(a.gpu_ctrl), a.gpu_pc, a.gpu_flags, gpu_go(b.gpu_ctrl), b.gpu_pc, b.gpu_flags
    );
    eprintln!(
        "  DSP        A: go={} pc=0x{:06X}   B: go={} pc=0x{:06X}   ctrl A=0x{:08X} B=0x{:08X}",
        gpu_go(a.dsp_ctrl), a.dsp_pc, gpu_go(b.dsp_ctrl), b.dsp_pc, a.dsp_ctrl, b.dsp_ctrl
    );
    eprintln!("  D regs differ: {:?}", dreg_diffs);
    eprintln!("  A regs differ: {:?}", areg_diffs);
    eprintln!("  GPU regs differ: {} of 32", gpu_reg_diffs.len());
    eprintln!("  DRAM chunks differ: {} of {}", chunk_diffs.len(), n);
    for &c in chunk_diffs.iter().take(8) {
        eprintln!("    chunk {:2}: 0x{:06X}-0x{:06X}", c, c as u32 * chunk_sz, (c as u32 + 1) * chunk_sz);
    }

    let cd: Vec<String> = chunk_diffs.iter().map(|c| format!("{{\"chunk\":{},\"addr\":{}}}", c, *c as u32 * chunk_sz)).collect();
    println!(
        "{{\"ok\":true,\"cpu_pc_match\":{},\"cpu_pc\":{{\"a\":\"0x{:06X}\",\"b\":\"0x{:06X}\"}},\
         \"gpu_running\":{{\"a\":{},\"b\":{}}},\"dsp_running\":{{\"a\":{},\"b\":{}}},\
         \"dreg_diffs\":{:?},\"areg_diffs\":{:?},\"gpu_reg_diffs\":{},\"dsp_reg_diffs\":{},\
         \"dram_chunks_total\":{},\"dram_chunks_diff\":{},\"dram_diff_regions\":[{}]}}",
        a.cpu_pc == b.cpu_pc, a.cpu_pc, b.cpu_pc,
        gpu_running(a.gpu_pc), gpu_running(b.gpu_pc),
        dsp_running(a.dsp_pc), dsp_running(b.dsp_pc),
        dreg_diffs, areg_diffs, gpu_reg_diffs.len(), dsp_reg_diffs.len(),
        n, chunk_diffs.len(), cd.join(",")
    );
    Ok(())
}

fn mark(ok: bool) -> &'static str {
    if ok {
        "✓"
    } else {
        "✗ DIVERGE"
    }
}

// ── persistent daemon (Claude connects + drives a live, isolated instance) ──

/// `jagemu serve --rom <path> [--instance <name>]` — a long-running, headless,
/// isolated emulator instance Claude connects to. State persists between
/// commands so you can run/step/inspect/inject input interactively, and pull
/// frames/video/audio on demand. Multi-instance: each serve = its own process,
/// state dir and control socket (no global lock).
fn cmd_serve(args: &[String]) -> Result<(), String> {
    let rom_path = flag_val(args, "--rom").or_else(|| positional(args)).ok_or("serve needs --rom <path>")?;
    let data = std::fs::read(rom_path).map_err(|e| format!("reading {rom_path}: {e}"))?;
    let project = flag_val(args, "--instance").map(|s| s.to_string()).unwrap_or_else(|| {
        std::path::Path::new(rom_path).file_stem().and_then(|s| s.to_str()).unwrap_or("jag").to_string()
    });
    let inst = jag_instance::Instance::create(&project).map_err(|e| e.to_string())?;
    let sock = inst.control_socket();
    let _ = std::fs::remove_file(&sock);
    let listener = UnixListener::bind(&sock).map_err(|e| format!("bind {}: {e}", sock.display()))?;

    let mut jag = Jaguar::new();
    let cart = jag.load(&data).map_err(|e| e.to_string())?;
    attach_sd(&mut jag);
    let entry = cart.entry;

    println!(
        "{{\"ok\":true,\"event\":\"serving\",\"instance\":{},\"socket\":{},\"pid\":{},\"rom\":{},\"entry\":{}}}",
        jstr(&inst.id),
        jstr(&sock.to_string_lossy()),
        std::process::id(),
        jstr(rom_path),
        entry
    );
    eprintln!("jagemu: serving instance '{}' on {} — `jagemu ctl {} <cmd>`", inst.id, sock.display(), inst.id);
    let _ = std::io::stdout().flush();

    for conn in listener.incoming() {
        let mut stream = match conn {
            Ok(s) => s,
            Err(_) => continue,
        };
        let mut argv: Vec<String> = Vec::new();
        {
            let reader = BufReader::new(&stream);
            for line in reader.lines() {
                match line {
                    Ok(l) => argv.push(l),
                    Err(_) => break,
                }
            }
        }
        if argv.first().map(|s| s.as_str()) == Some("stop") {
            let _ = stream.write_all(b"{\"ok\":true,\"event\":\"stopped\"}\n");
            break;
        }
        let resp = daemon_dispatch(&mut jag, entry, &argv);
        let _ = stream.write_all(resp.as_bytes());
        let _ = stream.write_all(b"\n");
    }
    let _ = std::fs::remove_file(&sock);
    let _ = inst.cleanup();
    Ok(())
}

/// `jagemu ctl <instance> <cmd> [args...]` — send one command to a running
/// instance and print its JSON reply.
fn cmd_ctl(args: &[String]) -> Result<(), String> {
    let inst = positional(args).ok_or("ctl needs <instance> <cmd...>")?;
    let sock = resolve_socket(inst)?;
    let idx = args.iter().position(|a| a == inst).unwrap();
    let cmd_args = &args[idx + 1..];
    if cmd_args.is_empty() {
        return Err("ctl needs a command (e.g. run 60, frame out.png, peek 0x3F00)".into());
    }
    let mut stream = UnixStream::connect(&sock).map_err(|e| format!("connect {}: {e}", sock.display()))?;
    for a in cmd_args {
        stream.write_all(a.as_bytes()).map_err(|e| e.to_string())?;
        stream.write_all(b"\n").map_err(|e| e.to_string())?;
    }
    stream.shutdown(std::net::Shutdown::Write).ok();
    let mut resp = String::new();
    stream.read_to_string(&mut resp).map_err(|e| e.to_string())?;
    print!("{resp}");
    Ok(())
}

fn resolve_socket(inst: &str) -> Result<std::path::PathBuf, String> {
    if inst.contains('/') {
        let p = std::path::PathBuf::from(inst);
        if p.exists() {
            return Ok(p);
        }
    }
    let direct = jag_instance::home().join("instances").join(inst).join("control.sock");
    if direct.exists() {
        return Ok(direct);
    }
    let list = jag_instance::list().map_err(|e| e.to_string())?;
    if let Some(i) = list.iter().find(|i| i.id == inst) {
        return Ok(i.dir.join("control.sock"));
    }
    if let Some(i) = list.iter().filter(|i| i.project == inst && i.alive).last() {
        return Ok(i.dir.join("control.sock"));
    }
    Err(format!("no instance matching '{inst}' (try `jagemu instances`)"))
}

fn nth_pos(args: &[String], n: usize) -> Option<&str> {
    args.iter().filter(|a| !a.starts_with('-')).nth(n).map(|s| s.as_str())
}
fn arg_u64(args: &[String], n: usize) -> Option<u64> {
    nth_pos(args, n).and_then(|s| parse_u64(s).ok())
}

/// Execute one daemon command against the live machine; returns a JSON reply.
fn daemon_dispatch(jag: &mut Jaguar, entry: u32, args: &[String]) -> String {
    let cmd = args.first().map(|s| s.as_str()).unwrap_or("");
    let res: Result<String, String> = (|| match cmd {
        "ping" => Ok("{\"ok\":true,\"pong\":true}".to_string()),
        "state" | "regs" => Ok(format!("{{\"ok\":true,\"state\":{}}}", state_json(jag))),
        "run" => {
            let n = arg_u64(args, 1).unwrap_or(1);
            jag.run_frames(n);
            Ok(format!("{{\"ok\":true,\"state\":{}}}", state_json(jag)))
        }
        "step" => {
            let n = arg_u64(args, 1).unwrap_or(1);
            for _ in 0..n {
                jag.step_instruction();
            }
            Ok(format!("{{\"ok\":true,\"state\":{}}}", state_json(jag)))
        }
        "frame" => {
            let out = nth_pos(args, 1).ok_or("frame needs <out.png>")?;
            let fb = jag.capture_frame();
            let png = jag_headless::png::encode_rgba(fb.width, fb.height, &fb.rgba);
            std::fs::write(out, &png).map_err(|e| e.to_string())?;
            Ok(format!(
                "{{\"ok\":true,\"out\":{},\"width\":{},\"height\":{},\"frame\":{}}}",
                jstr(out), fb.width, fb.height, jag.frame()
            ))
        }
        "video" => {
            let out = nth_pos(args, 1).ok_or("video needs <out.png>")?;
            let count = flag_val(args, "--count").map(parse_u32).transpose()?.unwrap_or(12).clamp(1, 256);
            let every = flag_val(args, "--every").map(parse_u64).transpose()?.unwrap_or(6);
            let cols = flag_val(args, "--cols").map(parse_u32).transpose()?.unwrap_or(4).max(1);
            let frames = jag_headless::capture_sequence(jag, 0, count, every, 0);
            let (w, h, rgba) = jag_headless::filmstrip(&frames, cols, 2);
            let png = jag_headless::png::encode_rgba(w, h, &rgba);
            std::fs::write(out, &png).map_err(|e| e.to_string())?;
            Ok(format!(
                "{{\"ok\":true,\"out\":{},\"frames\":{},\"width\":{},\"height\":{}}}",
                jstr(out), frames.len(), w, h
            ))
        }
        "audio" => {
            let out = nth_pos(args, 1).ok_or("audio needs <out.wav>")?;
            let n = flag_val(args, "--frames").map(parse_u64).transpose()?.unwrap_or(120);
            jag.enable_audio_capture();
            jag.run_frames(n);
            let (rate, samples) = jag.take_audio();
            let wav = jag_headless::wav::encode_pcm16(rate, 2, &samples);
            std::fs::write(out, &wav).map_err(|e| e.to_string())?;
            let (peak, rms) = jag_headless::wav::stats(&samples);
            Ok(format!(
                "{{\"ok\":true,\"out\":{},\"sample_rate\":{},\"samples\":{},\"peak\":{},\"rms\":{:.1},\"silent\":{}}}",
                jstr(out), rate, samples.len() / 2, peak, rms, peak == 0
            ))
        }
        "peek" => {
            let at = parse_u32(nth_pos(args, 1).ok_or("peek needs <addr>")?)?;
            let len = flag_val(args, "--len").map(parse_u32).transpose()?.unwrap_or(64).min(8192);
            let mut buf = vec![0u8; len as usize];
            jag.bus.peek(at, &mut buf);
            let bytes: Vec<String> = buf.iter().map(|b| b.to_string()).collect();
            Ok(format!("{{\"ok\":true,\"at\":{},\"at_hex\":{},\"bytes\":[{}]}}", at, jstr(&format!("0x{at:06X}")), bytes.join(",")))
        }
        "poke" => {
            let at = parse_u32(nth_pos(args, 1).ok_or("poke needs <addr> <byte,byte,...>")?)?;
            let bytes_s = nth_pos(args, 2).ok_or("poke needs bytes")?;
            let bytes: Vec<u8> = bytes_s.split(',').filter_map(|s| parse_u32(s).ok().map(|v| v as u8)).collect();
            jag.bus.poke(at, &bytes);
            Ok(format!("{{\"ok\":true,\"at\":{},\"wrote\":{}}}", at, bytes.len()))
        }
        "input" | "press" => {
            let btn = parse_buttons(nth_pos(args, 1).ok_or("input needs <buttons>")?)?;
            jag.set_pad(0, btn);
            Ok(format!("{{\"ok\":true,\"pad\":{}}}", btn))
        }
        "release" => {
            jag.set_pad(0, 0);
            Ok("{\"ok\":true,\"pad\":0}".to_string())
        }
        "break" => {
            let at = parse_u32(nth_pos(args, 1).ok_or("break needs <addr>")?)?;
            jag.dbg.add_breakpoint(at);
            Ok(format!("{{\"ok\":true,\"breakpoint\":\"0x{at:06X}\"}}"))
        }
        "continue" => {
            let n = arg_u64(args, 1).unwrap_or(600);
            let target = jag.frame() + n;
            let reason = jag.run_to_frame(target);
            let (k, hit) = match reason {
                jag_core::StopReason::Breakpoint(pc) => ("breakpoint", Some(pc)),
                _ => ("frame_limit", None),
            };
            Ok(format!(
                "{{\"ok\":true,\"stop\":{},\"hit_pc\":{},\"state\":{}}}",
                jstr(k),
                hit.map(|p| format!("\"0x{p:06X}\"")).unwrap_or_else(|| "null".into()),
                state_json(jag)
            ))
        }
        "reset" => {
            jag.reset_to(entry);
            Ok(format!("{{\"ok\":true,\"state\":{}}}", state_json(jag)))
        }
        "disasm" => {
            let at = match nth_pos(args, 1) {
                Some(s) => parse_u32(s)?,
                None => jag.cpu.pc,
            };
            let count = flag_val(args, "--count").map(parse_u32).transpose()?.unwrap_or(16) as usize;
            let insns = jag_debug::disasm_range(&jag.bus, at, count);
            let items: Vec<String> = insns.iter().map(|i| format!("{{\"addr\":{},\"text\":{}}}", i.addr, jstr(&i.text))).collect();
            Ok(format!("{{\"ok\":true,\"at\":{},\"insns\":[{}]}}", at, items.join(",")))
        }
        other => Err(format!("unknown command: {other}")),
    })();
    match res {
        Ok(s) => s,
        Err(e) => format!("{{\"ok\":false,\"error\":{}}}", jstr(&e)),
    }
}

fn cmd_instances(args: &[String]) -> Result<(), String> {
    if has_flag(args, "--prune") {
        let n = jag_instance::prune_stale().map_err(|e| e.to_string())?;
        println!("{{\"ok\":true,\"pruned\":{n}}}");
        return Ok(());
    }
    let list = jag_instance::list().map_err(|e| e.to_string())?;
    let items: Vec<String> = list
        .iter()
        .map(|i| {
            format!(
                "{{\"id\":{},\"project\":{},\"pid\":{},\"alive\":{}}}",
                jstr(&i.id),
                jstr(&i.project),
                i.pid,
                i.alive
            )
        })
        .collect();
    println!("{{\"ok\":true,\"instances\":[{}]}}", items.join(","));
    Ok(())
}

// ── JSON helpers ────────────────────────────────────────────────────────────

fn state_json(jag: &Jaguar) -> String {
    let cpu = &jag.cpu;
    let dregs: Vec<String> = cpu.d.iter().map(|v| v.to_string()).collect();
    let aregs: Vec<String> = cpu.a.iter().map(|v| v.to_string()).collect();
    let hexregs = |bank: &[u32; 32]| -> String {
        bank.iter().map(|v| format!("\"0x{v:08X}\"")).collect::<Vec<_>>().join(",")
    };
    format!(
        "{{\"frame\":{},\"pc\":{},\"pc_hex\":{},\"sr\":{},\"instret\":{},\"illegal\":{},\
         \"last_illegal_op\":\"0x{:04X}\",\
         \"gpu\":{{\"running\":{},\"pc_hex\":{},\"instret\":{},\"cycles\":{},\"granted\":{},\"timing\":{},\
         \"flags\":\"0x{:08X}\",\"regs0\":[{}],\"regs1\":[{}]}},\
         \"dsp\":{{\"running\":{},\"instret\":{},\"cycles\":{},\"timing\":{},\
         \"flags\":\"0x{:08X}\",\"regs0\":[{}],\"regs1\":[{}]}},\"d\":[{}],\"a\":[{}]}}",
        jag.frame(),
        cpu.pc,
        jstr(&format!("0x{:06X}", cpu.pc)),
        cpu.sr,
        cpu.instret,
        cpu.illegal_count,
        cpu.last_illegal_op,
        jag.gpu.running,
        jstr(&format!("0x{:06X}", jag.gpu.pc)),
        jag.gpu.instret,
        jag.gpu.cycles,
        jag.gpu.granted,
        timing_json(&jag.gpu.pipe.stats),
        jag.gpu.flags,
        hexregs(&jag.gpu.regs[0]),
        hexregs(&jag.gpu.regs[1]),
        jag.dsp.running,
        jag.dsp.instret,
        jag.dsp.cycles,
        timing_json(&jag.dsp.pipe.stats),
        jag.dsp.flags,
        hexregs(&jag.dsp.regs[0]),
        hexregs(&jag.dsp.regs[1]),
        dregs.join(","),
        aregs.join(",")
    )
}

/// Stall attribution + hazard counters from the jsim truth layer. All zeros
/// under the default `functional` fidelity.
fn timing_json(t: &TimingStats) -> String {
    format!(
        "{{\"stall_alu\":{},\"stall_load\":{},\"stall_div\":{},\"stall_flags\":{},\
         \"stall_div_busy\":{},\"jump_refill\":{},\"fetch_external\":{},\"mem_external\":{},\
         \"waw_hazards\":{},\"indexed_store_stale\":{},\"slot_movei\":{},\"slot_jump\":{},\
         \"bigpemu_divergence\":{},\"contention\":{},\"blit\":{}}}",
        t.stall_alu,
        t.stall_load,
        t.stall_div,
        t.stall_flags,
        t.stall_div_busy,
        t.jump_refill,
        t.fetch_external,
        t.mem_external,
        t.waw_hazards,
        t.indexed_store_stale,
        t.slot_movei,
        t.slot_jump,
        t.bigpemu_divergence,
        t.contention,
        t.blit
    )
}

/// JSON-escape a string (minimal: quotes, backslash, control chars).
fn jstr(s: &str) -> String {
    let mut o = String::with_capacity(s.len() + 2);
    o.push('"');
    for c in s.chars() {
        match c {
            '"' => o.push_str("\\\""),
            '\\' => o.push_str("\\\\"),
            '\n' => o.push_str("\\n"),
            '\r' => o.push_str("\\r"),
            '\t' => o.push_str("\\t"),
            c if (c as u32) < 0x20 => o.push_str(&format!("\\u{:04x}", c as u32)),
            c => o.push(c),
        }
    }
    o.push('"');
    o
}
