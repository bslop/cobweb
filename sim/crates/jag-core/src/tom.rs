//! Tom video: the Object Processor compositor that produces the **true display
//! scan-out** — the actual image the video DAC would emit, composited from the
//! object list. This is the screenshot primitive BigPEmu's headless path gets
//! wrong (it dumps the DRAM the 68000 wrote, which can PASS while the screen is
//! garbage). See `docs/spec/OBJECT_PROCESSOR.md`.
//!
//! v1 covers the homebrew subset: an RGB16 (and CRY16 / 8bpp-CLUT) BITMAP→STOP
//! object list. Multi-object sprite compositing (TRANS/REFLECT) and SCALED
//! bitmaps build on this. The Blitter lives in `tom::blit` (in progress).

use crate::bus::Bus;
use crate::m68k::M68k;
use crate::mem;
use crate::risc::Risc;

pub mod blit;
mod cry;

/// Hard caps on the composited canvas / per-object render extent. A garbage or
/// transient object list (huge IWIDTH/HEIGHT, e.g. 1bpp × 1023 phrases) must
/// never make us allocate gigabytes or loop forever — the Jaguar never faults,
/// so neither do we. These bounds comfortably exceed any real display mode +
/// overscan.
const MAX_FB_W: u32 = 2048;
const MAX_FB_H: u32 = 1024;

/// ⭐ `--full-window`: composite into the whole display window instead of the
/// bitmap bounding box, so **BGEN is in the capture**.
///
/// By default `op_begin_field` sizes the canvas to the bounding box of the
/// bitmap objects, which is what you want for comparing a rendered picture
/// against a reference. But `BG` (`$F00058`) is the Jaguar's diagnostic channel
/// of last resort — BGEN paints it wherever no object draws, so it stays legible
/// when the object list, the framebuffer and the blit are all broken, and at
/// least three projects run probe "ladders" on it. Under the default sizing that
/// band is **never in the PNG**: shorten the object to make room for it and the
/// canvas shrinks with the object (`jag_viewpoint` measured a 320x32 image from
/// an object shortened to 32 lines, and concluded from two such captures that a
/// BG ladder could not be validated in simulation at all).
///
/// With this set the canvas is the 320x240 window anchored at `VDB` — exactly
/// the default this function already uses for a field with no bitmap in the list
/// — and the bitmaps composite into it at their real `XPOS`/`YPOS`. Off by
/// default, so no existing capture, size assertion or golden image moves.
static FULL_WINDOW: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Set the `--full-window` capture mode (see [`FULL_WINDOW`]).
pub fn set_full_window(on: bool) {
    FULL_WINDOW.store(on, std::sync::atomic::Ordering::Relaxed);
}

/// Is `--full-window` capture mode on?
pub fn full_window() -> bool {
    FULL_WINDOW.load(std::sync::atomic::Ordering::Relaxed)
}

/// A composited frame in RGBA8888, ready for PNG.
#[derive(Clone)]
pub struct Framebuffer {
    pub width: u32,
    pub height: u32,
    /// `width * height * 4` bytes, row-major, RGBA.
    pub rgba: Vec<u8>,
}

impl Framebuffer {
    pub fn solid(width: u32, height: u32, r: u8, g: u8, b: u8) -> Self {
        let mut rgba = vec![0u8; (width * height * 4) as usize];
        for px in rgba.chunks_exact_mut(4) {
            px[0] = r;
            px[1] = g;
            px[2] = b;
            px[3] = 0xFF;
        }
        Framebuffer { width, height, rgba }
    }

    #[inline]
    fn put(&mut self, x: u32, y: u32, r: u8, g: u8, b: u8) {
        if x < self.width && y < self.height {
            let o = ((y * self.width + x) * 4) as usize;
            self.rgba[o] = r;
            self.rgba[o + 1] = g;
            self.rgba[o + 2] = b;
            self.rgba[o + 3] = 0xFF;
        }
    }

    /// Flood the whole buffer with one opaque color (the per-field background).
    fn fill(&mut self, r: u8, g: u8, b: u8) {
        for px in self.rgba.chunks_exact_mut(4) {
            px[0] = r;
            px[1] = g;
            px[2] = b;
            px[3] = 0xFF;
        }
    }
}

/// Decoded `VMODE` pixel format.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PixFmt {
    Cry16,
    Rgb24,
    Direct16,
    Rgb16,
}

impl PixFmt {
    fn from_vmode(vmode: u16) -> Self {
        match vmode & mem::VM_MODE_MASK {
            mem::VM_CRY16 => PixFmt::Cry16,
            mem::VM_RGB24 => PixFmt::Rgb24,
            mem::VM_DIRECT16 => PixFmt::Direct16,
            _ => PixFmt::Rgb16,
        }
    }
}

/// Per-field Object Processor state, persisted in `Tom` across the scanlines of
/// one field and reset at each frame boundary (`started = false`).
///
/// The OP re-walks the object list **every display line**, and DRAWING MUTATES
/// THE LIST: each displayed line advances that object's DATA by one DWIDTH,
/// counts HEIGHT down and steps YPOS with the beam, written back to DRAM.
///
/// ⚠️ This type used to call drawing *stateless* and say we "never mutate the
/// game's DRAM list (which it rebuilds each vblank)". That parenthesis was an
/// **assumption about the program**, not a property of the hardware, and nothing
/// enforced it — which left jsim blind to BOTH ways a real list dies:
///
///   * a **build-once** list, which silicon spends after a single field, and
///   * a **free-running rebuild**, which resets DATA on every scanline so
///     hardware draws source line 0 over the entire screen.
///
/// `jag_rr` hit both on 2026-08-17 and jsim rendered each perfectly, which is
/// worse than not modelling the OP at all: an emulator that shows a correct
/// picture for a ROM that is blank on silicon actively exonerates the bug.
pub struct OpState {
    /// Has the canvas been sized/cleared for the current field yet?
    pub started: bool,
    /// Has this field's list been consumed yet? Latched so the end-of-active
    /// -display write-back happens exactly once per field.
    pub consumed: bool,
    /// Display canvas size (pixels), chosen at field start.
    pub width: u32,
    pub height: u32,
    /// Screen origin: screen (0,0) maps to line-buffer x `anchor_x` and to the
    /// base object's top half-line `anchor_y`.
    pub anchor_x: i32,
    pub anchor_y: u16,
    /// Does this field's object list contain a GPU (TYPE 2) object? Only then do
    /// we walk the list on every scanline (to reach the GPU object even on lines
    /// outside the bitmap canvas). Bitmap-only lists — every game that renders
    /// today — keep the canvas-gated walk, byte-for-byte unchanged.
    pub has_gpu_object: bool,
    /// Phrases the OP fetches from DRAM per displayed line (sum of the active
    /// bitmaps' IWIDTH). This is real bus traffic every visible line and it
    /// outranks the GPU — HARDWARE (Skunkboard 2026-07-19, probe `lddramop`):
    /// a full-screen 320x240 16bpp object (80 phrases/line) slows Tom's DRAM
    /// stream by 11.1%, i.e. +0.46 cycles per external access. See
    /// `timing::OP_TAX_MILLI_PER_PHRASE`.
    pub phrases_per_line: u32,
}

impl Default for OpState {
    fn default() -> Self {
        OpState {
            started: false,
            consumed: false,
            width: 320,
            height: 240,
            anchor_x: 0,
            anchor_y: 0,
            has_gpu_object: false,
            phrases_per_line: 0,
        }
    }
}

/// One decoded object header (first + second phrase fields).
#[derive(Clone, Copy, Default)]
struct Obj {
    otype: u32,
    ypos: u32,   // half-lines
    height: u32, // source lines
    link: u32,   // next-object byte address
    data: u32,   // pixel-data byte address (line 0)
    xpos: i32,
    depth_bpp: u32,
    dwidth_phrases: u32,
    iwidth_phrases: u32,
    /// Inter-phrase stride within a line, in phrases (PITCH): 1 = contiguous,
    /// >1 skips embedded data (e.g. an interleaved Z buffer). Cybermorph's
    /// display object is PITCH=4 (one pixel phrase, three Z/pad phrases).
    pitch_phrases: u32,
    index: u32, // 7-bit INDEX, palette high bits for <8bpp
    reflect: bool,
    rmw: bool,
    trans: bool,
    firstpix: u32,
}

/// Convert an RGB16 word (`R5[15:11] B5[10:6] G6[5:0]` — blue in the middle,
/// per the porting notes) to 8-bit RGB.
#[inline]
fn rgb16_to_rgb(px: u16) -> (u8, u8, u8) {
    let r5 = ((px >> 11) & 0x1F) as u32;
    let b5 = ((px >> 6) & 0x1F) as u32;
    let g6 = (px & 0x3F) as u32;
    let r = (r5 << 3) | (r5 >> 2);
    let g = (g6 << 2) | (g6 >> 4);
    let b = (b5 << 3) | (b5 >> 2);
    (r as u8, g as u8, b as u8)
}

/// CRY16 → RGB via the hardware modifier tables (see `tom::cry`).
#[inline]
fn cry16_to_rgb(px: u16) -> (u8, u8, u8) {
    cry::cry16_to_rgb(px)
}

/// Max line-buffer width in 16-bit pixels (the hardware buffer is 360 × 32-bit
/// = 720 × 16-bit). All horizontal compositing clamps to this.
const LINE_W: usize = 720;

/// GPU interrupt source for the Object Processor (G_FLAGS enable bit `4+3`=7).
const OP_INT_SOURCE: u8 = 3;
/// Ceiling on RISC ticks a GPU object's ISR may run synchronously before the OP
/// gives up waiting for it — generous enough for a per-frame object ISR that
/// builds a display list or drives the co-processor handshake, but bounded so a
/// game that never clears the latch can't hang the field.
const GPU_OBJ_ISR_BUDGET: u32 = 2_000_000;

/// Render **one display scanline** of the Object Processor output into the
/// persistent framebuffer (`bus.tom.fb`).
///
/// The scheduler calls this once per visible line (every even half-line), so the
/// object list is sampled at the instant the line scans out. That is the whole
/// point: a game that rebuilds its display list every vblank composites
/// correctly here, whereas a single end-of-frame snapshot would catch a torn or
/// empty list. `vc` is the vertical count in half-lines.
pub fn op_render_line(vc: u16, cpu: &mut M68k, gpu: &mut Risc, bus: &mut Bus) {
    // Keep the bus-contention appetite current at LINE granularity. Sampling it
    // only at field start badly under-counts a load that changes mid-field (and
    // the calib probe is barely one field long). Ignore an implausible IWIDTH:
    // the 68k may be mid-write, and a torn list decodes as garbage.
    {
        let olp = ((bus.tom.win.r16(mem::OLPH) as u32) << 16) | bus.tom.win.r16(mem::OLP) as u32;
        let p: u32 = collect_bitmaps(bus, olp).iter().map(|b| b.iwidth_phrases).sum();
        if p <= (LINE_W as u32) {
            bus.tom.op.phrases_per_line = p;
        }
    }
    let vmode = bus.tom.win.r16(mem::VMODE);
    let fmt = PixFmt::from_vmode(vmode);

    // Size/clear the canvas from the list at the first ACTIVE line, not at
    // half-line 0.
    //
    // Canvas sizing is a jsim abstraction; the real OP has no such step, it just
    // draws an object when the beam reaches its YPOS. Doing it at half-line 0
    // demanded the list be intact at the very TOP of the field, which wrongly
    // fails a program that rebuilds during VERTICAL BLANK — the correct place,
    // and the one hardware gives it. Gating on VDB hands the program the same
    // window silicon does.
    let vdb = bus.tom.win.r16(mem::VDB);
    if !bus.tom.op.started && vc >= vdb {
        op_begin_field(bus, fmt);
        bus.tom.op.consumed = false;
    }
    if !bus.tom.op.started {
        return; // before the display window opens: nothing to composite yet
    }

    // End of active display: the real OP has now walked every header to its last
    // line, leaving them spent in DRAM. Do it HERE rather than at the field wrap
    // so a program still gets its whole vertical blank to rebuild — consuming at
    // the wrap would race `op_begin_field` on the very next half-line and blank
    // even a correct, rebuilding ROM.
    if !bus.tom.op.consumed
        && vc > bus.tom.op.anchor_y.saturating_add(2 * bus.tom.op.height as u16)
    {
        bus.tom.op.consumed = true;
        op_consume_list(bus);
    }
    let (width, height, anchor_x, anchor_y) =
        (bus.tom.op.width, bus.tom.op.height, bus.tom.op.anchor_x, bus.tom.op.anchor_y);

    // For lists containing a GPU (TYPE 2) object, walk on every scanline so the
    // OP reaches the object even on lines outside the bitmap canvas. Bitmap-only
    // lists keep the canvas-gated walk below, byte-for-byte unchanged.
    let in_canvas = vc >= anchor_y && ((vc - anchor_y) / 2) < height as u16;
    if bus.tom.op.has_gpu_object && !in_canvas {
        let mut line = [0u16; LINE_W];
        let mut written = [false; LINE_W];
        op_walk_line(vc, anchor_x, &mut line, &mut written, cpu, gpu, bus);
        return;
    }

    // Map this half-line to a screen row anchored on the base object's top.
    if vc < anchor_y {
        return;
    }
    let row = ((vc - anchor_y) / 2) as u32;
    if row >= height {
        return;
    }

    // Compose the scanline into a local line buffer of raw 16-bit physical
    // pixels, then convert via the VMODE pixel format and blit into the
    // persistent framebuffer. Unwritten pixels keep the field's background.
    let mut line = [0u16; LINE_W];
    let mut written = [false; LINE_W];
    op_walk_line(vc, anchor_x, &mut line, &mut written, cpu, gpu, bus);

    // ⭐ BGEN IS SAMPLED PER SCANLINE, NOT ONCE PER FIELD.
    // `op_begin_field` clears the whole canvas to BG, which is right for a ROM
    // that sets BG once. But BG ($F00058) is a live register: a program that
    // rewrites it during active display gets a different background on each
    // line -- that is how raster bars are done on this machine, and it is also
    // how a coprocessor reports on itself when the 68000 is asleep and no
    // object is drawing (jag_viewpoint, 2026-08-19: Tom writes the value it
    // reads to BG, so a live counter paints a gradient and a stuck one paints
    // a flat colour -- a distinction the field-start clear erased by
    // construction). Repaint this row from the CURRENT BG before compositing;
    // for the overwhelming case of a ROM that never touches BG mid-field the
    // pixels are identical to what the field-start clear already put there.
    let bgv = bus.tom.win.r16(mem::BG);
    let (br, bgc, bb) = decode_pixel(bgv, fmt);
    let w = width.min(LINE_W as u32);
    for x in 0..w {
        let i = x as usize;
        if written[i] {
            let (r, g, b) = decode_pixel(line[i], fmt);
            bus.tom.fb.put(x, row, r, g, b);
        } else {
            bus.tom.fb.put(x, row, br, bgc, bb);
        }
    }
}

/// At the first active line of a field, choose the display canvas from the
/// largest bitmap in the list (its pixel extent and screen origin) and clear the
/// framebuffer to the background colour. A list with no bitmap (sprite-only or
/// not-yet-built) defaults to 320×240 anchored at VDB.
fn op_begin_field(bus: &mut Bus, fmt: PixFmt) {
    let olp = ((bus.tom.win.r16(mem::OLPH) as u32) << 16) | bus.tom.win.r16(mem::OLP) as u32;
    let bitmaps = collect_bitmaps(bus, olp);

    let content_w = |b: &Obj| -> u32 {
        (b.iwidth_phrases * (64 / b.depth_bpp.max(1)).max(1)).clamp(1, MAX_FB_W)
    };
    let area = |b: &Obj| -> u64 { content_w(b) as u64 * b.height.clamp(1, MAX_FB_H) as u64 };

    let (mut width, mut height, mut anchor_x, mut anchor_y);
    if let Some(base) = bitmaps.iter().max_by_key(|b| area(b)) {
        // Anchor on the largest bitmap's top-left, then grow the canvas to the
        // bounding box of *every* bitmap so secondary objects (HUD, lower
        // sprites, a taller second bitmap) are not clipped by the base's extent.
        let (ax, ay) = (base.xpos, base.ypos as i32);
        let (mut w, mut h) = (1i32, 1i32);
        for b in &bitmaps {
            w = w.max((b.xpos - ax) + content_w(b) as i32);
            h = h.max((b.ypos as i32 - ay) / 2 + b.height as i32);
        }
        // The hardware line buffer is 720×16-bit; never exceed it horizontally.
        width = (w.max(1) as u32).min(LINE_W as u32);
        height = (h.max(1) as u32).clamp(1, MAX_FB_H);
        anchor_x = ax;
        anchor_y = base.ypos as u16;
    } else {
        // No bitmap (sprite-only or not-yet-built list): default window at VDB.
        width = 320;
        height = 240;
        anchor_x = 0;
        anchor_y = bus.tom.win.r16(mem::VDB);
    }

    // ⭐ --full-window: the whole display window, so the BGEN band around the
    // objects is in the capture. Same canvas this function already picks for a
    // field with no bitmap; the bitmaps just composite into it at their real
    // XPOS/YPOS instead of being anchored to the canvas origin.
    if full_window() {
        width = 320;
        height = 240;
        anchor_x = 0;
        anchor_y = bus.tom.win.r16(mem::VDB);
    }

    let bg = bus.tom.win.r16(mem::BG);
    let (br, bgc, bb) = decode_pixel(bg, fmt);
    if bus.tom.fb.width != width || bus.tom.fb.height != height {
        bus.tom.fb = Framebuffer::solid(width, height, br, bgc, bb);
    } else {
        bus.tom.fb.fill(br, bgc, bb);
    }
    bus.tom.op.started = true;
    bus.tom.op.width = width;
    bus.tom.op.height = height;
    bus.tom.op.anchor_x = anchor_x;
    bus.tom.op.anchor_y = anchor_y;
    bus.tom.op.has_gpu_object = list_has_gpu_object(bus, olp);
    // Per-line DRAM appetite of the display list: every bitmap re-reads IWIDTH
    // phrases on each line it covers. Drives the OP bus-contention tax.
    bus.tom.op.phrases_per_line =
        bitmaps.iter().map(|b| b.iwidth_phrases).sum::<u32>().min(4096);
}

/// Structural scan of the object graph for a GPU (TYPE 2) object (both BRANCH
/// paths explored). When present, the OP must walk the list every scanline so it
/// reaches the GPU object even on lines outside the bitmap canvas.
fn list_has_gpu_object(bus: &Bus, olp: u32) -> bool {
    use std::collections::HashSet;
    let mut seen: HashSet<u32> = HashSet::new();
    let mut stack = vec![olp];
    while let Some(addr) = stack.pop() {
        let addr8 = addr & !7;
        if !seen.insert(addr8) || seen.len() > 4096 {
            continue;
        }
        let o = decode_obj(bus, addr8);
        match o.otype {
            2 => return true,
            4 => {}
            3 => {
                if o.link != 0 {
                    stack.push(o.link);
                }
                stack.push(addr8 + 8);
            }
            0 | 1 => {
                if o.link != 0 {
                    stack.push(o.link);
                }
            }
            _ => stack.push(addr8 + 8),
        }
    }
    false
}

/// Decode an object header (first phrase always; second phrase for BITMAP/SCALED).
fn decode_obj(bus: &Bus, addr8: u32) -> Obj {
    let hi = peek32(bus, addr8);
    let lo = peek32(bus, addr8 + 4);
    let otype = lo & 7;
    // LINK is a 19-bit phrase index split across both longs (bits 42:24).
    let link = (((hi & 0x7FF) << 8) | ((lo >> 24) & 0xFF)) << 3;
    // DATA is a 21-bit phrase index (bits 63:43) → byte address.
    let data = ((hi >> 11) & 0x1FFFFF) << 3;
    let mut o = Obj {
        otype,
        ypos: (lo >> 3) & 0x7FF,
        height: (lo >> 14) & 0x3FF,
        link,
        data,
        ..Obj::default()
    };
    if otype == 0 || otype == 1 {
        let hi2 = peek32(bus, addr8 + 8);
        let lo2 = peek32(bus, addr8 + 12);
        let xpos_raw = lo2 & 0xFFF;
        o.xpos = ((xpos_raw << 20) as i32) >> 20; // sign-extend 12-bit
        o.depth_bpp = 1u32 << ((lo2 >> 12) & 7);
        o.pitch_phrases = ((lo2 >> 15) & 7).max(1); // PITCH bits 17:15 (0/1 → contiguous)
        o.dwidth_phrases = (lo2 >> 18) & 0x3FF;
        o.iwidth_phrases = ((((hi2 & 0x3F) << 4) | ((lo2 >> 28) & 0xF)) & 0x3FF).max(1);
        o.index = (hi2 >> 6) & 0x7F; // 7-bit INDEX (bits 44:38)
        o.reflect = (hi2 & 0x2000) != 0; // bit 45
        o.rmw = (hi2 & 0x4000) != 0; // bit 46
        o.trans = (hi2 & 0x8000) != 0; // bit 47
        o.firstpix = (hi2 >> 17) & 0x3F; // bits 54:49
    }
    o
}

/// Structural DFS of the object graph enumerating every reachable BITMAP/SCALED
/// (both BRANCH paths explored). Used only to size/anchor the canvas at field
/// start; per-line drawing uses the exact CC-evaluated walk in `op_walk_line`.
fn collect_bitmaps(bus: &Bus, olp: u32) -> Vec<Obj> {
    use std::collections::HashSet;
    let mut out = Vec::new();
    let mut seen: HashSet<u32> = HashSet::new();
    let mut stack = vec![olp];
    while let Some(addr) = stack.pop() {
        let addr8 = addr & !7;
        if !seen.insert(addr8) || seen.len() > 4096 {
            continue;
        }
        let o = decode_obj(bus, addr8);
        match o.otype {
            0 | 1 => {
                if o.link != 0 {
                    stack.push(o.link);
                }
                out.push(o);
            }
            4 => {} // STOP: end this path
            3 => {
                if o.link != 0 {
                    stack.push(o.link);
                }
                stack.push(addr8 + 8);
            }
            _ => stack.push(addr8 + 8),
        }
    }
    out
}

/// ☠ THE OP SELF-CONSUMES ITS OBJECT LIST — model it, or every build-once ROM
/// that is blank on silicon renders perfectly here.
///
/// Real hardware updates a BITMAP/SCALED header in DRAM as it renders: DATA
/// advances by DWIDTH phrases per displayed line and HEIGHT counts down, so at
/// the end of the field the stored header is spent (HEIGHT 0, DATA past the end
/// of the bitmap). A program must therefore rebuild the list EVERY field; one
/// that builds it once and spins draws exactly one field and then goes blank
/// permanently.
///
/// jsim used to re-read the list from DRAM each field and never write anything
/// back, so a build-once ROM looked correct forever. `jag_rr` lost most of a
/// hardware investigation to that gap on 2026-08-17: the emulator showed a
/// perfect test card while the Jaguar showed background, which sends you
/// hunting the object fields — alignment, IWIDTH, placement, colour — when the
/// list itself is simply gone. Same family as the narrow-RISC-RAM-write case:
/// modelling a hazard's existence without its consequence is what makes an
/// emulator *too forgiving*, and a too-forgiving emulator is worse than a
/// missing feature because it actively exonerates the bug.
///
/// Called once per field, at the field boundary, so within-field per-line reads
/// (the bus-contention appetite sampling) still see the live list.
pub fn op_consume_list(bus: &mut Bus) {
    use std::collections::HashSet;
    let olp = ((bus.tom.win.r16(mem::OLPH) as u32) << 16) | bus.tom.win.r16(mem::OLP) as u32;
    let mut seen: HashSet<u32> = HashSet::new();
    let mut stack = vec![olp];
    while let Some(addr) = stack.pop() {
        let addr8 = addr & !7;
        if !seen.insert(addr8) || seen.len() > 4096 {
            continue;
        }
        let o = decode_obj(bus, addr8);
        match o.otype {
            0 | 1 => {
                if o.link != 0 {
                    stack.push(o.link);
                }
                // DATA advances one DWIDTH per line drawn; HEIGHT counts to 0.
                let advanced = o
                    .data
                    .wrapping_add(o.height.wrapping_mul(o.dwidth_phrases).wrapping_mul(8));
                let hi = peek32(bus, addr8);
                let lo = peek32(bus, addr8 + 4);
                poke32_dram(bus, addr8, (hi & 0x7FF) | (((advanced >> 3) & 0x1F_FFFF) << 11));
                poke32_dram(bus, addr8 + 4, lo & !(0x3FF << 14));
            }
            4 => {} // STOP ends this path
            3 => {
                if o.link != 0 {
                    stack.push(o.link);
                }
                stack.push(addr8 + 8);
            }
            _ => stack.push(addr8 + 8),
        }
    }
}

/// Walk the live object list for one scanline (`vc` in half-lines), drawing each
/// active BITMAP/SCALED into `line`. BRANCH conditions are evaluated against the
/// current `vc`; STOP ends the line; a GPU object latches OB0–3 and suspends.
fn op_walk_line(
    vc: u16,
    anchor_x: i32,
    line: &mut [u16; LINE_W],
    written: &mut [bool; LINE_W],
    cpu: &mut M68k,
    gpu: &mut Risc,
    bus: &mut Bus,
) {
    let olp = ((bus.tom.win.r16(mem::OLPH) as u32) << 16) | bus.tom.win.r16(mem::OLP) as u32;
    // LINK replaces OLP bits 21:3; OLP's 4 MB bank bits (23:22) persist.
    let bank = olp & 0x00C0_0000;
    let mut addr = olp;
    let vc32 = vc as u32;
    // Only STOP terminates a list; bound the walk against malformed/cyclic lists
    // by breaking on a revisited phrase (mirrors `collect_bitmaps`).
    let mut visited: std::collections::HashSet<u32> = std::collections::HashSet::new();
    loop {
        let addr8 = addr & !7;
        if !visited.insert(addr8) || visited.len() > 4096 {
            break;
        }
        let o = decode_obj(bus, addr8);
        match o.otype {
            0 | 1 => {
                // Draw from the object's CURRENT DATA pointer and write the
                // header back, exactly as the OP does: DATA advances one DWIDTH
                // per displayed line, HEIGHT counts down, YPOS tracks the beam.
                //
                // This used to derive the source line as (vc - ypos)/2 and never
                // write back — "stateless, so a static list can never be
                // corrupted". That made jsim blind to BOTH ways a real list dies:
                // a build-once list (which silicon spends after one field) and a
                // FREE-RUNNING rebuild (which resets DATA every scanline, so
                // hardware draws line 0 over the whole screen). `jag_rr` hit both
                // on 2026-08-17 and jsim rendered each of them perfectly.
                if vc32 >= o.ypos && o.height > 0 {
                    draw_object_line(bus, &o, 0, anchor_x, line, written);
                    let stride = o.dwidth_phrases * 8;
                    let hi = peek32(bus, addr8);
                    let lo = peek32(bus, addr8 + 4);
                    let next_data = o.data.wrapping_add(stride);
                    poke32_dram(
                        bus,
                        addr8,
                        (hi & 0x7FF) | (((next_data >> 3) & 0x1F_FFFF) << 11),
                    );
                    let lo2 = (lo & !(0x3FF << 14) & !(0x7FF << 3))
                        | (((o.height - 1) & 0x3FF) << 14)
                        | (((o.ypos + 2) & 0x7FF) << 3);
                    poke32_dram(bus, addr8 + 4, lo2);
                }
                // The OP ALWAYS follows LINK (an inactive object still chains on).
                addr = bank | o.link;
            }
            2 => {
                // GPU object (TRM §3.3): hand the phrase to the GPU so it can act
                // on the OP's behalf (palette load, perspective, dynamic list
                // building). We (a) latch the phrase into OB0–3, (b) raise the GPU
                // "object" interrupt (source 3) — serviced by the scheduler's GPU
                // slice — then continue with the NEXT phrase in memory, not a LINK.
                //
                // A GPU object fires whenever the OP *reaches* it; the enclosing
                // list structure does the VC gating. jsim used to also gate on the
                // object's own YPOS (`vc==ypos`), but that is wrong: real lists
                // route to a GPU object via a BREQ BRANCH (e.g. Atari Karts fires
                // its object from a `vc==506` branch while the object's own YPOS is
                // 0) — the YPOS gate made the two conditions mutually exclusive so
                // the interrupt never fired. The TRM's per-object YPOS rule is
                // flagged UNVERIFIED in the spec; firing on reach matches real
                // display lists. (The remaining half of task #15 — the GPU
                // rendering each scanline into the line buffer and the OP reading
                // it back — still has to land before these games actually draw.)
                let hi = peek32(bus, addr8);
                let lo = peek32(bus, addr8 + 4);
                bus.tom.win.w16(mem::OB0, (hi >> 16) as u16);
                bus.tom.win.w16(mem::OB1, hi as u16);
                bus.tom.win.w16(mem::OB2, (lo >> 16) as u16);
                bus.tom.win.w16(mem::OB3, lo as u16);
                gpu.raise_int(OP_INT_SOURCE);
                // Suspend the OP and run the GPU's object ISR synchronously (TRM
                // §3.3): give the GPU cycles until it services the object
                // interrupt (clears the source-3 latch via INT_CLR) so the ISR's
                // side effects — OBF, dynamic list edits, the co-processor
                // handshake — are in place before the OP continues to the next
                // phrase. Bounded so an unhandled object can't hang the field.
                let op_enabled = gpu.running && gpu.flags & (1 << (4 + OP_INT_SOURCE)) != 0;
                if op_enabled {
                    let mut spent = 0u32;
                    while spent < GPU_OBJ_ISR_BUDGET
                        && gpu.int_latch & (1 << OP_INT_SOURCE) != 0
                    {
                        gpu.run(bus, 256);
                        spent += 256;
                    }
                }
                addr = addr8 + 8;
            }
            3 => {
                // BRANCH: 3-bit CC compared against the current vertical count.
                let cc = (peek32(bus, addr8 + 4) >> 14) & 7;
                let taken = match cc {
                    0 => o.ypos == vc32 || o.ypos == 0x7FF, // BREQ (+ wildcard)
                    1 => o.ypos > vc32,                     // BRGT
                    2 => o.ypos < vc32,                     // BRLT
                    3 => bus.tom.win.r16(mem::OBF) & 1 != 0, // BROP (OP flag)
                    4 => bus.tom.win.r16(mem::HC) & 0x400 != 0, // BRHALF
                    _ => false,
                };
                addr = if taken { bank | o.link } else { addr8 + 8 };
            }
            4 => {
                // STOP: end of list. Bit 3 of the first long gates the Object
                // interrupt (INT1 bit 2), mirroring the VI path in the scheduler.
                if peek32(bus, addr8 + 4) & 0x08 != 0 {
                    bus.tom.int1_pending |= mem::C_OPENA;
                    if bus.tom.int1_enable & mem::C_OPENA != 0 {
                        cpu.request_interrupt(2);
                    }
                }
                break;
            }
            _ => addr = addr8 + 8,
        }
    }
}

/// Draw one source line of a BITMAP object into the line buffer at screen X =
/// `xpos - anchor_x + i`. (SCALED is drawn at 1× for now; HSCALE/VSCALE land in
/// a later step.)
fn draw_object_line(
    bus: &Bus,
    o: &Obj,
    src_line: u32,
    anchor_x: i32,
    line: &mut [u16; LINE_W],
    written: &mut [bool; LINE_W],
) {
    let pps = (64 / o.depth_bpp.max(1)).max(1); // pixels per phrase
    let width_px = (o.iwidth_phrases * pps).min(MAX_FB_W); // bound the loop
    let stride = o.dwidth_phrases * 8; // source bytes per line
    let line_base = o.data.wrapping_add(src_line * stride);
    let base_x = o.xpos - anchor_x;

    // FIRSTPIX clips the left edge: the first displayed source pixel is
    // `firstpix`, placed at XPOS (so leading pixels are skipped, not shifted off
    // the right). firstpix == 0 for the common full-bitmap case.
    for x in o.firstpix..width_px {
        // REFLECT samples the source right→left while writing left→right.
        let src_x = if o.reflect { width_px - 1 - x } else { x };
        let (px, transparent) = sample_raw(bus, o, line_base, src_x);
        if transparent {
            continue;
        }
        let dst = base_x + (x - o.firstpix) as i32;
        if dst >= 0 && (dst as usize) < LINE_W {
            let d = dst as usize;
            // RMW (read-modify-write): add the source to the existing line-buffer
            // pixel in CRY space instead of overwriting (spec §6.3). Cybermorph's
            // HUD overlay is an RMW object with a zero source — adding zero must
            // leave the framebuffer visible, not paint black over it.
            line[d] = if o.rmw { rmw_blend(line[d], px) } else { px };
            written[d] = true;
        }
    }
}

/// RMW blend: component-wise saturating add of two CRY pixels — 8-bit intensity
/// (low byte) and two 4-bit chroma nibbles (high byte). (Spec §6.3; the exact
/// signed/clamp semantics are UNVERIFIED, but a zero source is a no-op either
/// way, which is the case the games rely on.)
#[inline]
fn rmw_blend(dst: u16, src: u16) -> u16 {
    let y = ((dst & 0xFF) + (src & 0xFF)).min(0xFF);
    let cyan = (((dst >> 8) & 0xF) + ((src >> 8) & 0xF)).min(0xF);
    let cred = (((dst >> 12) & 0xF) + ((src >> 12) & 0xF)).min(0xF);
    (cred << 12) | (cyan << 8) | y
}

/// Sample one source pixel as a raw 16-bit physical colour (post-CLUT for
/// indexed depths), plus whether it is transparent (TRANS + logical colour 0).
/// Honours the object's PITCH: each pixel-phrase advances `pitch_phrases`
/// phrases, skipping interleaved data (e.g. a Z buffer) within the line.
#[inline]
fn sample_raw(bus: &Bus, o: &Obj, line_base: u32, x: u32) -> (u16, bool) {
    let bpp = o.depth_bpp;
    let pps = (64 / bpp.max(1)).max(1); // pixels per phrase
    let phrase = x / pps;
    let bit_in_phrase = (x % pps) * bpp;
    let addr = line_base + phrase * o.pitch_phrases.max(1) * 8 + bit_in_phrase / 8;
    match bpp {
        16 => {
            let px = peek16(bus, addr);
            (px, o.trans && px == 0)
        }
        8 => {
            let idx = peek8(bus, addr) as u32;
            (bus.tom.win.r16(mem::CLUT + idx * 2), o.trans && idx == 0)
        }
        4 | 2 | 1 => {
            let shift = 8 - bpp - (bit_in_phrase % 8); // MSB-first within the byte
            let raw = (peek8(bus, addr) as u32 >> shift) & ((1 << bpp) - 1);
            // INDEX (7-bit) supplies the high palette bits, the pixel the low
            // `bpp` bits (bit-OR — INDEX is the CLUT bank). Per spec §6.2 the
            // bank uses the *top* 7/6/3 INDEX bits for 1/2/4 bpp.
            let idx = match bpp {
                1 => (o.index << 1) | raw,        // high7 << 1 | pix1
                2 => ((o.index >> 1) << 2) | raw, // high6 << 2 | pix2
                _ => ((o.index >> 4) << 4) | raw, // 4bpp: high3 << 4 | pix4
            };
            (bus.tom.win.r16(mem::CLUT + idx * 2), o.trans && raw == 0)
        }
        _ => (0xF81F, false), // RGB16 magenta sentinel for an unsupported depth
    }
}

/// Convert a 16-bit physical line-buffer pixel to host RGB per the VMODE format.
#[inline]
fn decode_pixel(px: u16, fmt: PixFmt) -> (u8, u8, u8) {
    match fmt {
        PixFmt::Cry16 => cry16_to_rgb(px),
        // DIRECT16 / RGB24 provisionally decode as RGB16 until modelled.
        _ => rgb16_to_rgb(px),
    }
}

/// Test/utility helper: run the OP over a whole NTSC field on the given machine
/// state and return the composited frame. Production capture reads the
/// accumulated `bus.tom.fb` directly (the scheduler drives `op_render_line`).
pub fn compose_frame(bus: &mut Bus) -> Framebuffer {
    let mut cpu = M68k::new();
    let mut gpu = Risc::new(crate::risc::RiscKind::Gpu);
    bus.tom.op.started = false;
    let mut hl = 0u16;
    while hl < 524 {
        op_render_line(hl, &mut cpu, &mut gpu, bus);
        hl += 2;
    }
    bus.tom.fb.clone()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a reference backend's exact RGB16 BITMAP→STOP object list, fill the
    /// framebuffer with a known color, and assert the OP composites it.
    fn setup_a3d_style(bus: &mut Bus, fb: u32, ol: u32, w_px: u32, h: u32, base_x: u32, base_y: u32) {
        let screen_pwidth = (w_px * 2) / 8; // phrases per line (16bpp)
        let link = (ol + 16) >> 3; // STOP object phrase address
        // First phrase (op_list[0..1]).
        bus.write32(ol, (fb << 8) | (link >> 8));
        bus.write32(ol + 4, (link << 24) | (h << 14) | (base_y << 4));
        // Second phrase (op_list[2..3]).
        bus.write32(ol + 8, screen_pwidth >> 4);
        bus.write32(
            ol + 12,
            (screen_pwidth << 28) | (screen_pwidth << 18) | (1 << 15) | (4 << 12) | base_x,
        );
        // STOP object.
        bus.write32(ol + 16, 0);
        bus.write32(ol + 20, 4);
        // VMODE = RGB16 enabled; OLP written word-swapped like the hardware path.
        bus.tom.win.w16(mem::VMODE, 0x06C7);
        bus.write32(mem::OLP, (ol >> 16) | (ol << 16));
    }

    #[test]
    fn op_composites_solid_rgb16() {
        let mut bus = Bus::new();
        let (fb, ol, w, h) = (0x10_0000u32, 0x1000u32, 320u32, 240u32);
        // Fill the framebuffer with RGB16 red ($F800 = R5=31).
        for i in 0..(w * h) {
            bus.write16(fb + i * 2, 0xF800);
        }
        setup_a3d_style(&mut bus, fb, ol, w, h, 16, 16);
        let frame = compose_frame(&mut bus);
        assert_eq!(frame.width, 320);
        assert_eq!(frame.height, 240);
        // Center pixel should be pure red.
        let o = ((120 * 320 + 160) * 4) as usize;
        assert_eq!(&frame.rgba[o..o + 4], &[255, 0, 0, 255]);
    }

    /// The OP SELF-CONSUMES its list: a ROM that builds the list once and never
    /// rebuilds draws exactly ONE field on silicon and is blank forever after.
    ///
    /// This is a REGRESSION GUARD, and the property it protects is jsim's
    /// ability to FAIL. Before this, a build-once list rendered perfectly here
    /// field after field, so the emulator actively exonerated a ROM that showed
    /// nothing on hardware — `jag_rr` lost most of a hardware investigation to
    /// it on 2026-08-17. An emulator that models a hazard's existence without
    /// its consequence is worse than one that omits it entirely.
    #[test]
    fn op_self_consumes_list_second_field_is_blank() {
        let mut bus = Bus::new();
        let (fb, ol, w, h) = (0x10_0000u32, 0x1000u32, 320u32, 240u32);
        for i in 0..(w * h) {
            bus.write16(fb + i * 2, 0xF800); // red
        }
        setup_a3d_style(&mut bus, fb, ol, w, h, 16, 16);

        // Field 1: the list is intact, so the full canvas composites.
        let first = compose_frame(&mut bus);
        assert_eq!((first.width, first.height), (320, 240), "first field should draw");
        let o = ((120 * 320 + 160) * 4) as usize;
        assert_eq!(&first.rgba[o..o + 4], &[255, 0, 0, 255], "first field should be red");

        // Field 2 with NO rebuild: the header is spent, so there is nothing to
        // size a canvas from. Height collapses — exactly the silicon symptom.
        let second = compose_frame(&mut bus);
        assert!(
            second.height < 240,
            "second field must NOT draw a full canvas from a consumed list \
             (got {}x{}) — jsim is exonerating a build-once ROM again",
            second.width,
            second.height
        );
    }

    #[test]
    fn op_composites_color_bands() {
        let mut bus = Bus::new();
        let (fb, ol, w, h) = (0x10_0000u32, 0x1000u32, 320u32, 240u32);
        // RGB16 (R5[15:11] B5[10:6] G6[5:0]): $F800=red, $003F=green, $07C0=blue.
        // Top third red, middle green, bottom blue.
        for y in 0..h {
            let c = if y < 80 { 0xF800 } else if y < 160 { 0x003F } else { 0x07C0 };
            for x in 0..w {
                bus.write16(fb + (y * w + x) * 2, c);
            }
        }
        setup_a3d_style(&mut bus, fb, ol, w, h, 16, 16);
        let frame = compose_frame(&mut bus);
        let at = |x: u32, y: u32| {
            let o = ((y * 320 + x) * 4) as usize;
            (frame.rgba[o], frame.rgba[o + 1], frame.rgba[o + 2])
        };
        assert_eq!(at(160, 40), (255, 0, 0)); // red band
        assert_eq!(at(160, 120), (0, 255, 0)); // green band ($003F)
        assert_eq!(at(160, 200), (0, 0, 255)); // blue band ($07C0)
    }

    #[test]
    fn video_disabled_is_black() {
        let mut bus = Bus::new(); // VMODE=0 → VIDEN clear
        let frame = compose_frame(&mut bus);
        assert!(frame.rgba.iter().all(|&b| b == 0 || b == 0xFF));
    }

    /// A garbage object list (huge 1bpp bitmap → ~65k×1023 px if unclamped)
    /// must NOT OOM-abort — it gets clamped. Regression test for a reference homebrew title.
    #[test]
    fn op_clamps_garbage_bitmap_no_oom() {
        let mut bus = Bus::new();
        let (ol, fb) = (0x1000u32, 0x10_0000u32);
        let link = (ol + 16) >> 3;
        let height = 0x3FFu32; // 1023 lines
        bus.write32(ol, (fb << 8) | (link >> 8));
        bus.write32(ol + 4, (link << 24) | (height << 14) | (16 << 4)); // BITMAP
        // Second phrase: DEPTH=0 (1bpp → 64 px/phrase), IWIDTH/DWIDTH maxed.
        let iwidth = 0x3FFu32;
        bus.write32(ol + 8, iwidth >> 4);
        bus.write32(ol + 12, (0xF << 28) | (iwidth << 18) | 16);
        bus.write32(ol + 16, 0);
        bus.write32(ol + 20, 4); // STOP
        bus.tom.win.w16(mem::VMODE, 0x06C7);
        bus.write32(mem::OLP, (ol >> 16) | (ol << 16));
        // Would allocate multi-GB unclamped; must return a clamped frame instead.
        let frame = compose_frame(&mut bus);
        assert!(frame.width <= MAX_FB_W, "width {} exceeds cap", frame.width);
        assert!(frame.height <= MAX_FB_H, "height {} exceeds cap", frame.height);
    }
}

/// Debug: walk the live object list from OLP and return a JSON array describing
/// every object (type, position, geometry, depth, PITCH, flags). Follows LINK for
/// BITMAP/SCALED, both arms for BRANCH, stops at STOP; bounded against cycles.
/// This is the AI-eyes window into *why* a screen composites the way it does.
pub fn dump_object_list(bus: &Bus) -> String {
    let olp = ((bus.tom.win.r16(mem::OLPH) as u32) << 16) | bus.tom.win.r16(mem::OLP) as u32;
    let bank = olp & 0x00C0_0000;
    // DFS over the whole reachable graph: both BRANCH arms, BITMAP/SCALED LINK,
    // and the next-phrase fall-through for GPU/BRANCH. Visit each phrase once.
    let mut seen: std::collections::HashSet<u32> = std::collections::HashSet::new();
    let mut stack = vec![olp];
    let mut nodes: Vec<(u32, Obj)> = Vec::new();
    while let Some(addr) = stack.pop() {
        let addr8 = addr & !7;
        if !seen.insert(addr8) || seen.len() > 4096 {
            continue;
        }
        let o = decode_obj(bus, addr8);
        match o.otype {
            0 | 1 => {
                if o.link != 0 {
                    stack.push(bank | o.link);
                }
            }
            3 => {
                if o.link != 0 {
                    stack.push(bank | o.link);
                }
                stack.push(addr8 + 8);
            }
            4 => {}
            _ => stack.push(addr8 + 8), // GPU / unknown: fall through
        }
        nodes.push((addr8, o));
    }
    nodes.sort_by_key(|(a, _)| *a);
    let mut out = String::from("[");
    for (i, (addr8, o)) in nodes.iter().enumerate() {
        let tname = match o.otype {
            0 => "BITMAP",
            1 => "SCALED",
            2 => "GPU",
            3 => "BRANCH",
            4 => "STOP",
            _ => "?",
        };
        // For BRANCH, decode the CC field for readability.
        let cc = (peek32(bus, addr8 + 4) >> 14) & 7;
        if i > 0 {
            out.push(',');
        }
        out.push_str(&format!(
            "{{\"addr\":\"0x{addr8:06X}\",\"type\":\"{tname}\",\"ypos\":{},\"height\":{},\
             \"xpos\":{},\"bpp\":{},\"iwidth\":{},\"dwidth\":{},\"pitch\":{},\"data\":\"0x{:06X}\",\
             \"index\":{},\"reflect\":{},\"rmw\":{},\"trans\":{},\"firstpix\":{},\"cc\":{},\"link\":\"0x{:06X}\"}}",
            o.ypos, o.height, o.xpos, o.depth_bpp, o.iwidth_phrases, o.dwidth_phrases,
            o.pitch_phrases, o.data, o.index, o.reflect, o.rmw, o.trans, o.firstpix, cc, o.link,
        ));
    }
    out.push(']');
    out
}

// Side-effect-free reads for compositing (no access counting, no device logic).
#[inline]
fn peek8(bus: &Bus, addr: u32) -> u8 {
    let mut b = [0u8; 1];
    bus.peek(addr, &mut b);
    b[0]
}
#[inline]
fn peek16(bus: &Bus, addr: u32) -> u16 {
    let mut b = [0u8; 2];
    bus.peek(addr, &mut b);
    u16::from_be_bytes(b)
}
#[inline]
fn peek32(bus: &Bus, addr: u32) -> u32 {
    let mut b = [0u8; 4];
    bus.peek(addr, &mut b);
    u32::from_be_bytes(b)
}

/// DRAM-only 32-bit poke that does NOT charge `m68k_bus_cycles`.
///
/// The Object Processor is not the 68000: its writes must not appear in the
/// CPU's bus accounting or every contention measurement built on that counter
/// shifts. `Bus::write32` charges two cycles, so it is the wrong tool here.
#[inline]
fn poke32_dram(bus: &mut Bus, addr: u32, v: u32) {
    let a = addr & crate::bus::ADDR_MASK;
    if mem::is_dram(a) && a + 3 < mem::DRAM_END {
        bus.dram[a as usize..a as usize + 4].copy_from_slice(&v.to_be_bytes());
    }
}
