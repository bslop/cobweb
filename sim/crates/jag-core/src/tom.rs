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
/// The OP re-walks the object list **every display line**; the only state that
/// must persist between lines is the chosen canvas geometry and the screen
/// anchor (the base object's origin). Multi-line bitmaps advance their source
/// pointer *statelessly* — at half-line `vc` an object draws source line
/// `(vc - ypos)/2` — so we never mutate the game's DRAM list (which it rebuilds
/// each vblank) and a static homebrew list can never be corrupted.
pub struct OpState {
    /// Has the canvas been sized/cleared for the current field yet?
    pub started: bool,
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

    // First active call of the field: size/clear the canvas from the list.
    if !bus.tom.op.started {
        op_begin_field(bus, fmt);
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

    let w = width.min(LINE_W as u32);
    for x in 0..w {
        let i = x as usize;
        if !written[i] {
            continue;
        }
        let (r, g, b) = decode_pixel(line[i], fmt);
        bus.tom.fb.put(x, row, r, g, b);
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

    let (width, height, anchor_x, anchor_y);
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
                // Active on this line iff the scanline is within the object's
                // vertical span. Source line = (vc - ypos)/2 (stateless — no
                // header write-back, so a static list can never be corrupted).
                if vc32 >= o.ypos {
                    let src_line = (vc32 - o.ypos) / 2;
                    if src_line < o.height {
                        draw_object_line(bus, &o, src_line, anchor_x, line, written);
                    }
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
