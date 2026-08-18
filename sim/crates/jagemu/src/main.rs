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
    if let Some(r) = flag_val(rest, "--sd-rate").and_then(|s| s.parse::<u32>().ok()) {
        SD_RATE.store(r, std::sync::atomic::Ordering::Relaxed);
    }
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
         \x20 jagemu run <rom> --pc-histogram [--core 68k|gpu|dsp|all] [--map m.map]\n\
         \x20      [--gpu-map g.map] [--dsp-map d.map] [--start S] [--top K] [--bucket N]\n\
         \x20      [--prof-json p.json]      # full per-PC profile; diff two with profdiff.py\n\
         \x20 jagemu run <rom> --watchdog N   # warn if a core runs N frames without clearing GO\n\
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
         Input: --press <a,b,c,up,down,left,right,option,start>  --press-after <frame>\n\
         \n\
         GameDrive SD (any command that boots a ROM):\n\
         \x20 --sd <dir>          serve <dir> as the SD card; the ROM's own GDBIOS\n\
         \x20                     bindings drive it (fopen/fseek/fread/...)\n\
         \x20 --sd-rate <bytes>   bytes an ASYNC read delivers per frame. Default 0\n\
         \x20                     = complete instantly, which exercises a loader's\n\
         \x20                     logic but NOT its waiting. Set it to make a\n\
         \x20                     GD_FREAD_GPU_ASYNC transfer actually take time."
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

/// Parse `0xLO..0xHI` (inclusive range) or a single address.
fn parse_range(s: &str) -> Result<(u32, u32), String> {
    match s.split_once("..") {
        Some((a, b)) => Ok((parse_u32(a)?, parse_u32(b.trim_start_matches('='))?)),
        None => {
            let a = parse_u32(s)?;
            Ok((a, a))
        }
    }
}

/// JSON for the watch state: range, totals, and the logged hits.
fn watch_json(jag: &Jaguar, lo: u32, hi: u32) -> String {
    let hits: Vec<String> = jag
        .bus
        .watch_log
        .iter()
        .take(64)
        .map(|h| {
            format!(
                "{{\"addr\":\"0x{:06X}\",\"value\":\"0x{:X}\",\"size\":{},\"master\":\"{}\",\"pc\":\"0x{:06X}\",\"frame\":{}}}",
                h.addr,
                h.value,
                h.size,
                h.master.name(),
                h.pc,
                h.frame
            )
        })
        .collect();
    format!(
        "{{\"range\":\"0x{:06X}..0x{:06X}\",\"total\":{},\"logged\":{},\"hits\":[{}]}}",
        lo,
        hi,
        jag.bus.watch_total,
        jag.bus.watch_log.len(),
        hits.join(",")
    )
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

/// The name of a fidelity, for reporting it back to the caller.
///
/// Every subcommand echoes this now, because the default is silent and
/// **fidelity is part of the experiment, not a rendering option**. A
/// `run --fidelity silicon` profile and a bare `peek` of the same ROM are two
/// different timelines: a reporting project cross-checked a profiler finding
/// with `peek` and `break`, got a flat contradiction, and spent a session
/// concluding the profiler was misattributing cycles. It was not — the checks
/// were running the other timeline, in which the game had not even reached the
/// same state. See `COBWEB_ISSUES_RESIDENT.md`.
pub fn fidelity_name(f: Fidelity) -> &'static str {
    match f {
        Fidelity::Functional => "functional",
        Fidelity::Silicon => "silicon",
        Fidelity::BigPEmu => "bigpemu",
    }
}

/// Parse `--fidelity functional|silicon|bigpemu` (default functional — the
/// timed profiles are the jsim truth layer, opt-in until hardware-calibrated).
///
/// Warns on stderr when it falls back to the default, so the choice is never
/// invisible: an omitted flag and a deliberate `--fidelity functional` produce
/// the same run but very different confidence, and only one of them is a
/// decision somebody made.
fn fidelity_arg(args: &[String]) -> Result<Fidelity, String> {
    Ok(match flag_val(args, "--fidelity") {
        None => {
            eprintln!(
                "jagemu: fidelity=functional (default). Timing differs from \
                 --fidelity silicon, and so can the ROM's behaviour - compare \
                 only runs that used the SAME fidelity."
            );
            Fidelity::Functional
        }
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

/// `--sd-rate <bytes>`: bytes an async read delivers per frame. 0 = instant.
static SD_RATE: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

/// Attach the emulated GameDrive if `--sd` was given. Without it the SPI window
/// floats and `gd_install` fails its bounded waits — i.e. "no GameDrive", which
/// is exactly the state a ROM must already handle.
fn attach_sd(jag: &mut Jaguar) {
    if let Some(Some(dir)) = SD_DIR.get() {
        let mut gd = jag_core::gamedrive::GameDrive::new(dir);
        gd.set_rate(SD_RATE.load(std::sync::atomic::Ordering::Relaxed));
        jag.bus.gamedrive = Some(gd);
    }
}

/// `--watchdog <frames>`, stashed so it reaches every boot path without adding
/// a parameter to seven call sites for what is a debugging aid. 0 = disabled.
static WATCHDOG_FRAMES: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
/// Set by `--blit-histogram`; read after the run to print the per-shape
/// Blitter breakdown. Global for the same reason WATCHDOG_FRAMES is: the run
/// helpers do not take the arg vector.
static BLIT_HIST: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
/// `--blit-top N`: how many shape rows to print. 0 means all of them.
///
/// The default is a readable summary, not the answer. A renderer in a real
/// level produces hundreds of distinct shapes with a very long tail, and a
/// truncated table invites exactly one mistake: summing the printed rows and
/// concluding the big blits dominate, when the rows shown may be a minority of
/// the total cost. The footer below always states the coverage so that error
/// is impossible to make silently; `--blit-top 0` prints the tail itself.
static BLIT_TOP: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(20);

fn apply_watchdog(jag: &mut Jaguar) {
    let n = WATCHDOG_FRAMES.load(std::sync::atomic::Ordering::Relaxed);
    if n > 0 {
        jag.gpu.stuck_after_frames = Some(n);
        jag.dsp.stuck_after_frames = Some(n);
    }
}

/// Loud, unconditional diagnostics for the "renders here, black-screens on
/// silicon" class (COBWEB_BUG_jagemu_runs_code_that_hangs_silicon.md).
///
/// Unconditional on purpose. Both signals are free, and the failure they catch
/// costs a 195-second flash plus a physical power-cycle to discover the slow
/// way — an opt-in flag would be off precisely on the run that needed it.
fn report_hazard_diagnostics(jag: &Jaguar) {
    for (name, t) in [("Tom GPU", &jag.gpu.pipe.stats), ("Jerry DSP", &jag.dsp.pipe.stats)] {
        if t.div_by_zero > 0 {
            eprintln!(
                "jagemu: WARNING — {name} executed {} DIV(s) with a ZERO divisor. jsim \
                 returns 0xFFFFFFFF and continues; real silicon does NOT, and a kernel \
                 that divides by zero has black-screened a Jaguar while rendering fine \
                 here. Silicon's exact behaviour is unmeasured, so this is reported, not \
                 modelled — but treat a nonzero count as a hardware failure.",
                t.div_by_zero
            );
        }
    }
    for (name, c) in [("Tom GPU", &jag.gpu), ("Jerry DSP", &jag.dsp)] {
        if let Some((pc, frames)) = c.stuck_at {
            eprintln!(
                "jagemu: WARNING — {name} ran {frames} consecutive frames without clearing \
                 RISCGO (first seen at pc={pc:#010X})."
            );
        }
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
    apply_watchdog(&mut jag);
    if buttons != 0 && press_after < frames {
        jag.run_frames(press_after);
        jag.set_pad(0, buttons);
        jag.run_frames(frames - press_after);
    } else if frames > 0 {
        jag.run_frames(frames);
    }
    report_hazard_diagnostics(&jag);
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
    // --watchdog <frames>: warn if a core never clears RISCGO for that many
    // consecutive frames. Opt-in with no default, because a RESIDENT kernel
    // legitimately runs forever and a warning that always fires is one nobody
    // reads. Only the caller knows whether its kernel is per-frame.
    if let Some(n) = flag_val(args, "--watchdog").map(parse_u32).transpose()? {
        WATCHDOG_FRAMES.store(n, std::sync::atomic::Ordering::Relaxed);
    }
    BLIT_HIST.store(
        has_flag(args, "--blit-histogram"),
        std::sync::atomic::Ordering::Relaxed,
    );
    if let Some(n) = flag_val(args, "--blit-top").map(parse_u32).transpose()? {
        BLIT_TOP.store(n, std::sync::atomic::Ordering::Relaxed);
    }
    let prof = has_flag(args, "--pc-histogram") || has_flag(args, "--profile68k");
    // The histogram is printed from the profiled boot path, so on its own
    // `--blit-histogram` is silently inert — the worst way for a flag to fail.
    if has_flag(args, "--blit-histogram") && !prof {
        eprintln!(
            "jagemu: --blit-histogram needs a profiled boot: add --pc-histogram \
             (or --profile68k). It also needs --fidelity silicon — the default \
             functional mode runs no timing model, so every Blitter counter \
             reads 0."
        );
    }
    if prof {
        let top = flag_val(args, "--top").map(parse_u32).transpose()?.unwrap_or(25) as usize;
        let gran = flag_val(args, "--bucket").map(parse_u32).transpose()?.unwrap_or(0);
        // --start <frame>: run this many frames with the profiler DISARMED (boot /
        // level-load excluded), then arm it and accumulate for --frames. Without
        // it a short run ranks one-time boot loops as steady-state hotspots
        // (COBWEB_REQ_pchistogram_warmup_start.md).
        let start = flag_val(args, "--start").map(parse_u64).transpose()?.unwrap_or(0);
        let map = match flag_val(args, "--map") {
            Some(m) => load_map(m)?,
            None => Vec::new(),
        };
        // --core selects which masters to profile. Default `all`: the 68k
        // section is what it always was, GPU/DSP are additive. Naming one core
        // keeps the run cheap when only that core is under investigation.
        let core = flag_val(args, "--core").unwrap_or("all");
        let cores = match core {
            "all" => Cores { m68k: true, gpu: true, dsp: true },
            "68k" | "68000" | "m68k" => Cores { m68k: true, gpu: false, dsp: false },
            "gpu" | "tom" => Cores { m68k: false, gpu: true, dsp: false },
            "dsp" | "jerry" => Cores { m68k: false, gpu: false, dsp: true },
            other => return Err(format!("--core: expected 68k|gpu|dsp|all, got `{other}`")),
        };
        let gpu_map = match flag_val(args, "--gpu-map") {
            Some(m) => load_map(m)?,
            None => Vec::new(),
        };
        let dsp_map = match flag_val(args, "--dsp-map") {
            Some(m) => load_map(m)?,
            None => Vec::new(),
        };
        let maps = Maps { m68k: &map, gpu: &gpu_map, dsp: &dsp_map };
        let jag = boot_profiled(
            &data,
            start,
            frames,
            btn,
            after,
            fidelity_arg(args)?,
            gran,
            top,
            cores,
            maps,
            flag_val(args, "--prof-json"),
        )?;
        println!(
            "{{\"ok\":true,\"path\":{},\"frames\":{},\"state\":{}}}",
            jstr(&path),
            frames,
            state_json(&jag)
        );
        return Ok(());
    }
    // --watch 0xLO..0xHI (or a single address): log every write from any
    // master — 68k, GPU, DSP, Blitter — landing in the range. "Who wrote this
    // byte" is the first question when silicon and emulator disagree.
    let watch = flag_val(args, "--watch").map(parse_range).transpose()?;
    if let Some((lo, hi)) = watch {
        let mut jag = Jaguar::new();
        jag.load(&data).map_err(|e| e.to_string())?;
        attach_sd(&mut jag);
        let fid = fidelity_arg(args)?;
        jag.gpu.fidelity = fid;
        jag.dsp.fidelity = fid;
        jag.bus.add_watch(lo, hi);
        if btn != 0 && after < frames {
            jag.run_frames(after);
            jag.set_pad(0, btn);
            jag.run_frames(frames - after);
        } else {
            jag.run_frames(frames);
        }
        eprintln!(
            "jagemu: watch 0x{lo:06X}..0x{hi:06X}: {} write(s), first {} logged",
            jag.bus.watch_total,
            jag.bus.watch_log.len()
        );
        println!(
            "{{\"ok\":true,\"path\":{},\"frames\":{},\"watch\":{},\"state\":{}}}",
            jstr(&path),
            frames,
            watch_json(&jag, lo, hi),
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

/// Which masters to profile (`--core`).
#[derive(Clone, Copy)]
struct Cores {
    m68k: bool,
    gpu: bool,
    dsp: bool,
}

/// Symbol maps, one per master.
#[derive(Clone, Copy)]
struct Maps<'a> {
    m68k: &'a [(u32, String)],
    gpu: &'a [(u32, String)],
    dsp: &'a [(u32, String)],
}

/// Boot with the requested profilers armed, then print the histograms to stderr
/// (stdout stays a single JSON object, as every other command guarantees).
#[allow(clippy::too_many_arguments)]
fn boot_profiled(
    rom: &[u8],
    start: u64,
    frames: u64,
    buttons: u32,
    press_after: u64,
    fid: Fidelity,
    gran: u32,
    top: usize,
    cores: Cores,
    maps: Maps,
    prof_json: Option<&str>,
) -> Result<Jaguar, String> {
    let map = maps.m68k;
    let mut jag = Jaguar::new();
    jag.load(rom).map_err(|e| e.to_string())?;
    attach_sd(&mut jag);
    jag.gpu.fidelity = fid;
    jag.dsp.fidelity = fid;
    apply_watchdog(&mut jag);
    // Warmup: run to `start` with the profiler off, so one-time boot/level-load
    // loops don't count as steady-state. A button press scheduled inside the
    // warmup window still fires there; a later one fires in the armed window.
    if start > 0 {
        if buttons != 0 && press_after < start {
            jag.run_frames(press_after);
            jag.set_pad(0, buttons);
            jag.run_frames(start - press_after);
        } else {
            jag.run_frames(start);
        }
    }
    // Arm the profilers and accumulate over the [start, start+frames) window.
    // Each core's profiler is independent, so `--core gpu` pays nothing for the
    // 68k's per-instruction bookkeeping and vice versa.
    if cores.m68k {
        jag.dbg.prof = Some(Box::new(jag_core::debug::Profile::new()));
    }
    if cores.gpu {
        jag.gpu.arm_profiler();
    }
    if cores.dsp {
        jag.dsp.arm_profiler();
    }
    let press_in_window = press_after.saturating_sub(start);
    if buttons != 0 && press_after >= start && press_in_window < frames {
        jag.run_frames(press_in_window);
        jag.set_pad(0, buttons);
        jag.run_frames(frames - press_in_window);
    } else if frames > 0 {
        jag.run_frames(frames);
    }
    // ☠ `awake` stays 0 when the 68000 was not the profiled core. That is NOT
    // the same as "the 68000 slept", so the wall-clock line below must not
    // report it as 0.0% — see the guard there.
    let mut awake = 0u64;
    let m68k_profiled = jag.dbg.prof.is_some();
    if let Some(p) = jag.dbg.prof.as_ref() {
        awake = p.main_cycles + p.isr_cycles;
        let tot = p.total_cycles.max(1);
        if start > 0 {
            eprintln!(
                "=== 68k cycle profile ({frames} frames, armed window [{start}, {}) — boot excluded) ===",
                start + frames
            );
        } else {
            eprintln!("=== 68k cycle profile ({frames} frames) ===");
        }
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
    }
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
    // ☠☠ DO NOT PRINT A CONFIDENT ZERO FOR A CORE NOBODY MEASURED. This line
    // used to read "68000 awake 0.000 s 0.0%" whenever jagemu was run with
    // --core gpu or --core dsp, because `awake` is only accumulated by the
    // 68000 profiler. A jag_quake run read that as an optimisation having
    // driven the 68000 to sleep and put the number in a commit message; the
    // real figure, measured like-for-like with --core 68k, was 35.3%.
    // An unmeasured quantity must say so — a plausible number is worse than
    // no number, because nothing downstream can tell them apart.
    if m68k_profiled {
        eprintln!(
            "  68000 awake     {:>8.3} s  {:5.1}%",
            awake_wall,
            100.0 * awake_wall / wall
        );
    } else {
        eprintln!("  68000 awake          not profiled  (re-run with --core 68k)");
    }
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

    if jag.bus.risc_ram_narrow_writes > 0 {
        eprintln!(
            "\n  !! {} sub-32-bit write(s) into GPU/DSP internal RAM.\n\
             \x20    Silicon takes 32-BIT ACCESSES ONLY there - a byte/word store does not\n\
             \x20    land, so a kernel uploaded this way is corrupt and the core never starts.\n\
             \x20    jsim models byte-addressable memory and runs it fine; hardware will not.",
            jag.bus.risc_ram_narrow_writes
        );
    }

    // --blit-histogram: WHICH blits spend the Blitter's time. An aggregate
    // cannot distinguish three full-screen copies from ten thousand short
    // spans, and those call for opposite fixes.
    if BLIT_HIST.load(std::sync::atomic::Ordering::Relaxed) {
        let mut rows: Vec<_> = jag
            .bus
            .tom
            .blit_shapes
            .iter()
            .map(|(k, v)| (*k, *v))
            .collect();
        rows.sort_by(|a, b| b.1 .1.cmp(&a.1 .1));
        let total: u64 = rows.iter().map(|r| r.1 .1).sum();
        let fr = frames.max(1) as f64;
        // Who actually issued the blits. The per-core `gpu/dsp.timing.blit*`
        // counters answer a different question — they are drained by whichever
        // RISC core runs next, so a 68000-issued blit lands under Tom or Jerry
        // depending only on which was busier. Read this table for issuance and
        // those for per-core drain; do not mix them.
        eprintln!("\n=== blits by issuing master ===");
        eprintln!("  {:>8} {:>10} {:>14} {:>14}", "master", "count", "launch_ticks", "transfer_ticks");
        for (i, s) in jag.bus.tom.blit_by_master.iter().enumerate() {
            if s.0 == 0 {
                continue;
            }
            let name = match i {
                0 => "68000",
                1 => "Tom",
                2 => "Jerry",
                3 => "Blitter",
                _ => "host",
            };
            eprintln!("  {:>8} {:>10} {:>14} {:>14}", name, s.0, s.1, s.2);
        }
        eprintln!("\n=== blit shapes by transfer cost ({} distinct) ===", rows.len());
        eprintln!(
            "  {:>6} {:>6} {:>5} {:>6} {:>10} {:>14} {:>7} {:>12}",
            "inner", "outer", "srcen", "phrase", "count", "transfer_ticks", "% xfer", "ticks/frame"
        );
        let want = BLIT_TOP.load(std::sync::atomic::Ordering::Relaxed) as usize;
        let shown = if want == 0 { rows.len() } else { want.min(rows.len()) };
        for (k, v) in rows.iter().take(shown) {
            eprintln!(
                "  {:>6} {:>6} {:>5} {:>6} {:>10} {:>14} {:>6.1}% {:>12.0}",
                k.0,
                k.1,
                if k.2 { "yes" } else { "no" },
                if k.3 { "yes" } else { "no" },
                v.0,
                v.1,
                if total > 0 { 100.0 * v.1 as f64 / total as f64 } else { 0.0 },
                v.1 as f64 / fr
            );
        }
        // Always state what the printed rows actually cover. Reading a
        // truncated table as if it were the whole cost is the specific error
        // this footer exists to prevent.
        let covered: u64 = rows.iter().take(shown).map(|r| r.1 .1).sum();
        eprintln!("  total transfer ticks {total}  ({:.0}/frame)", total as f64 / fr);
        if shown < rows.len() {
            eprintln!(
                "  shown {shown} of {} shapes = {:.1}% of transfer; {} rows ({:.1}%) not printed \
                 — use --blit-top 0 for all",
                rows.len(),
                if total > 0 { 100.0 * covered as f64 / total as f64 } else { 0.0 },
                rows.len() - shown,
                if total > 0 { 100.0 * (total - covered) as f64 / total as f64 } else { 0.0 },
            );
        } else {
            eprintln!("  all {} shapes shown (100% of transfer)", rows.len());
        }
    }

    if let Some(p) = jag.dbg.prof.as_ref() {
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
    }
    report_hazard_diagnostics(&jag);
    risc_profile_report("Tom GPU", jag.gpu.prof.as_deref(), gran, top, maps.gpu);
    risc_profile_report("Jerry DSP", jag.dsp.prof.as_deref(), gran, top, maps.dsp);
    if let Some(path) = prof_json {
        let txt = profile_json(&jag, frames, start, maps);
        std::fs::write(path, &txt).map_err(|e| format!("writing {path}: {e}"))?;
        eprintln!("jagemu: wrote {path} ({} bytes)", txt.len());
    }
    Ok(jag)
}

/// The full per-PC profile as JSON — every executed PC, not just the top K.
///
/// This is what makes a *work move* priceable. "Did moving the pose to Jerry
/// help?" is a diff of two profiles, and a top-K table cannot answer it: the
/// routine that appeared is usually nowhere near the top, and the routine that
/// vanished leaves no row behind. `sim/tools/profdiff.py` consumes this.
fn profile_json(jag: &Jaguar, frames: u64, start: u64, maps: Maps) -> String {
    let mut s = String::from("{\"frames\":");
    s.push_str(&frames.to_string());
    s.push_str(",\"start\":");
    s.push_str(&start.to_string());
    if let Some(p) = jag.dbg.prof.as_ref() {
        s.push_str(",\"m68k\":{\"total_cycles\":");
        s.push_str(&p.total_cycles.to_string());
        s.push_str(",\"stopped_cycles\":");
        s.push_str(&p.stopped_cycles.to_string());
        s.push_str(",\"isr_cycles\":");
        s.push_str(&p.isr_cycles.to_string());
        s.push_str(",\"main_cycles\":");
        s.push_str(&p.main_cycles.to_string());
        s.push_str(",\"columns\":[\"pc\",\"cycles\",\"instrs\"],\"pcs\":[");
        // usize::MAX: every PC, not a top-K slice — see the doc comment.
        for (i, (pc, cyc, n)) in p.top(usize::MAX).into_iter().enumerate() {
            if i > 0 {
                s.push(',');
            }
            s.push_str(&format!("[{pc},{cyc},{n}]"));
        }
        s.push_str("],\"symbols\":");
        s.push_str(&sym_json(maps.m68k));
        s.push('}');
    }
    for (key, prof, map) in [
        ("gpu", jag.gpu.prof.as_deref(), maps.gpu),
        ("dsp", jag.dsp.prof.as_deref(), maps.dsp),
    ] {
        let Some(p) = prof else { continue };
        s.push_str(&format!(",\"{key}\":{{\"total_cycles\":{}", p.total.cycles));
        s.push_str(&format!(",\"total_instrs\":{}", p.total.instrs));
        s.push_str(
            ",\"columns\":[\"pc\",\"cycles\",\"instrs\",\"stall_alu\",\"stall_load\",\
             \"stall_div\",\"stall_flags\",\"stall_div_busy\",\"jump_refill\",\
             \"fetch_external\",\"mem_external\",\"blit_wait\",\"contention\"],\"pcs\":[",
        );
        let mut rows = p.all();
        rows.sort_unstable_by(|a, b| b.1.cycles.cmp(&a.1.cycles));
        for (i, (pc, r)) in rows.into_iter().enumerate() {
            if i > 0 {
                s.push(',');
            }
            s.push_str(&format!(
                "[{},{},{},{},{},{},{},{},{},{},{},{},{}]",
                pc,
                r.cycles,
                r.instrs,
                r.stall_alu,
                r.stall_load,
                r.stall_div,
                r.stall_flags,
                r.stall_div_busy,
                r.jump_refill,
                r.fetch_external,
                r.mem_external,
                r.blit_wait,
                r.contention
            ));
        }
        s.push_str("],\"symbols\":");
        s.push_str(&sym_json(map));
        s.push('}');
    }
    s.push('}');
    s
}

fn sym_json(map: &[(u32, String)]) -> String {
    let mut s = String::from("[");
    for (i, (a, n)) in map.iter().enumerate() {
        if i > 0 {
            s.push(',');
        }
        s.push_str(&format!("[{a},{}]", jstr(n)));
    }
    s.push(']');
    s
}

/// Print one JRISC core's per-PC profile.
///
/// The stall columns are the point of this table. A core-wide `jump_refill`
/// total tells you a kernel is refilling the pipe; it does not tell you which
/// jump, which is why chasing one previously meant reading the listing by hand.
/// Here every stalled tick is attributed to the instruction that paid it, so the
/// hot PC and the reason it is hot arrive together.
fn risc_profile_report(
    name: &str,
    prof: Option<&jag_core::debug::RiscProfile>,
    gran: u32,
    top: usize,
    map: &[(u32, String)],
) {
    let Some(p) = prof else { return };
    let t = &p.total;
    if t.cycles == 0 {
        eprintln!("\n=== {name} cycle profile ===\n  (core never ran)");
        return;
    }
    let tot = t.cycles.max(1) as f64;
    let pct = |v: u64| 100.0 * v as f64 / tot;
    eprintln!("\n=== {name} cycle profile ===");
    eprintln!("  cycles executed {:>12}   ({} instrs)", t.cycles, t.instrs);
    // These partition the core's cycles: issue + stalls + external fetch = 100%.
    eprintln!(
        "  issue           {:>12}  {:5.1}%   (executing, not stalled)",
        t.cycles.saturating_sub(t.total_stall() + t.fetch_external),
        pct(t.cycles.saturating_sub(t.total_stall() + t.fetch_external))
    );
    for (label, v) in [
        ("stall_load", t.stall_load),
        ("stall_alu", t.stall_alu),
        ("stall_div", t.stall_div),
        ("stall_div_busy", t.stall_div_busy),
        ("stall_flags", t.stall_flags),
        ("jump_refill", t.jump_refill),
        ("fetch_external", t.fetch_external),
    ] {
        if v > 0 {
            eprintln!("  {label:<15} {v:>12}  {:5.1}%", pct(v));
        }
    }
    // These do NOT partition the cycles above and must not be added to them.
    // `mem_external` is bus occupancy PLUS result latency: the occupancy half is
    // charged to the loading instruction, the latency half is paid later (and
    // only if a consumer is close enough) as `stall_load`. `blit_wait` is
    // already inside `issue` — the ticks are real B_CMD poll instructions.
    // `contention` is the tax portion already included in the costs above.
    if t.mem_external + t.blit_wait + t.contention > 0 {
        eprintln!("  -- overlapping measures (not a share of the cycles above) --");
        for (label, v) in [
            ("mem_external", t.mem_external),
            ("blit_wait", t.blit_wait),
            ("contention", t.contention),
        ] {
            if v > 0 {
                eprintln!("  {label:<15} {v:>12}");
            }
        }
    }
    let rows = if gran > 0 { p.top_buckets(gran, top) } else { p.top(top) };
    eprintln!(
        "\n  {:<10} {:>12} {:>7} {:>10} {:>9} {:>9} {:>9} {:>9}  {}",
        if gran > 0 { "bucket" } else { "pc" },
        "cycles",
        "% core",
        "instrs",
        "ld",
        "div",
        "refill",
        "mem",
        "symbol"
    );
    for (pc, r) in rows {
        eprintln!(
            "  0x{:06X}   {:>12} {:>6.2}% {:>10} {:>9} {:>9} {:>9} {:>9}  {}",
            pc,
            r.cycles,
            pct(r.cycles),
            r.instrs,
            r.stall_load,
            r.stall_div + r.stall_div_busy,
            r.jump_refill,
            r.mem_external + r.fetch_external,
            sym_for(map, pc)
        );
    }
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
        &["--against", "--frames", "--press", "--press-after", "--sd", "--sd-rate", "-o", "--out"];
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
    let fid = fidelity_arg(args)?;
    let jag = boot_input(&data, frames, btn, after, fid)?;
    let mut buf = vec![0u8; len as usize];
    jag.bus.peek(at, &mut buf);
    if let Some(path) = out {
        std::fs::write(path, &buf).map_err(|e| e.to_string())?;
        println!(
            "{{\"ok\":true,\"at\":{},\"at_hex\":{},\"len\":{},\"fidelity\":{},\"out\":{}}}",
            at, jstr(&format!("0x{at:06X}")), len, jstr(fidelity_name(fid)), jstr(path)
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
        "{{\"ok\":true,\"at\":{},\"at_hex\":{},\"len\":{},\"fidelity\":{},\"bytes\":[{}]}}",
        at,
        jstr(&format!("0x{at:06X}")),
        len,
        jstr(fidelity_name(fid)),
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
    // serve honors --fidelity like every one-shot command (it was silently
    // functional-only — COBWEB_REQ_rectshade_and_calibration §2).
    let fid = fidelity_arg(args)?;
    jag.gpu.fidelity = fid;
    jag.dsp.fidelity = fid;
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
        // watch 0xLO..0xHI  (or: watch 0xLO 0xHI / watch 0xADDR)
        "watch" => {
            let (lo, hi) = match (nth_pos(args, 1), nth_pos(args, 2)) {
                (Some(a), Some(b)) => (parse_u32(a)?, parse_u32(b)?),
                (Some(a), None) => parse_range(a)?,
                _ => return Err("watch needs <addr> or <lo>..<hi>".into()),
            };
            jag.bus.add_watch(lo, hi);
            Ok(format!("{{\"ok\":true,\"watch\":\"0x{lo:06X}..0x{hi:06X}\"}}"))
        }
        "unwatch" => {
            jag.bus.clear_watches();
            Ok("{\"ok\":true,\"watches\":0}".to_string())
        }
        // watchlog [--keep]: report hits; clears the log (not the watches)
        // unless --keep, so successive runs read cleanly.
        "watchlog" => {
            let (lo, hi) = jag
                .bus
                .watches
                .first()
                .copied()
                .unwrap_or((0, 0));
            let out = watch_json(jag, lo, hi);
            if !args.iter().any(|a| a == "--keep") {
                jag.bus.watch_log.clear();
                jag.bus.watch_total = 0;
            }
            Ok(out)
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
         \"flags\":\"0x{:08X}\",\"regs0\":[{}],\"regs1\":[{}]}},\
         \"blitter\":{{\"bcmd_busy_reads\":{},\"bcmd_poll_in_settle\":{}}},\"risc_ram_narrow_writes\":{},\
         \"d\":[{}],\"a\":[{}]}}",
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
        // Bus-level, not per-core: a B_CMD status read is one bus transaction
        // regardless of which master issued it, so these cannot live in
        // TimingStats alongside the per-core stall counters.
        jag.bus.bcmd_busy_reads.load(std::sync::atomic::Ordering::Relaxed),
        jag.bus.bcmd_poll_in_settle.load(std::sync::atomic::Ordering::Relaxed),
        jag.bus.risc_ram_narrow_writes,
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
         \"bigpemu_divergence\":{},\"contention\":{},\"blit\":{},\
         \"unaligned_risc32\":{},\"blit_count\":{},\"blit_launch\":{},\"blit_transfer\":{},\"blit_wait\":{},\"div_by_zero\":{}}}",
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
        t.blit,
        t.unaligned_risc32,
        t.blit_count,
        t.blit_launch,
        t.blit_transfer,
        t.blit_wait,
        t.div_by_zero
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
