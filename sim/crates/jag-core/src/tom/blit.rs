//! The Tom Blitter — 2D block-move / fill / logic engine, per
//! `docs/spec/BLITTER.md` (Tech Reference v8 + JAGUAR.INC).
//!
//! Modeled **functionally**: a write to `B_CMD` runs the whole blit immediately
//! and `B_CMD` reads return idle. The spec endorses this (§7) — programs only
//! ever *wait* for idle (fire-and-forget; wait-before-setup), never observe the
//! blitter busy — so an instantaneous-at-`B_CMD` data path is hardware-faithful
//! for results. Cycle-accurate timing (§9) is the remaining accuracy item.
//!
//! Implements the full data path the games exercise: A1/A2 address generators
//! (WIDTH float, PIXEL size, PITCH), the two-level inner/outer loop, **memory
//! copies** (`SRCEN` reading through A2, `DSTA2` role swap), fills from `B_SRCD`,
//! every LFU boolean op, `PATDSEL`/`ADDDSEL` write data, the transparent-copy
//! data comparator (`DCOMPEN`/`CMPDST`), `CLIP_A1`, `BKGWREN`, and the
//! `UPDA1`/`UPDA2`/`UPDA1F` outer-step updates. Gouraud (`GOURD`) and Z
//! (`ZBUFF`/`ZMODE`) — the 16-bit 3D path — have their registers modeled but the
//! per-pixel computation is deferred (see the markers in `run`); the bit-expand
//! comparator (`BCOMPEN`) is likewise deferred. Both are flagged, never silently
//! wrong.

use crate::bus::Bus;
use crate::mem;

static WATCH_ADDR: std::sync::OnceLock<Option<u32>> = std::sync::OnceLock::new();

/// `B_CMD` read value indicating the blitter is idle (bit 0).
pub const BLIT_IDLE: u32 = 0x0000_0001;

/// Blitter timing — HARDWARE-CALIBRATED (Skunkboard 2026-07-19, probes
/// `p_blitsm`/`p_blitbg`: an SRCEN 8bpp *pixel-mode* copy of N pixels measured
/// `16 + 11.2*N` RISC ticks, i.e. ~5.6 ticks per DRAM phrase access — one for
/// the source read, one for the dest write — plus a ~16-tick launch. jsim
/// otherwise runs the whole blit at `B_CMD`-write time for free, so the GPU's
/// bwait-spin costs nothing; this is the ~12% "fill" term the fps model missed.
/// Accesses are counted in phrases: pixel-mode (XADDPIX/XADDINC) touches one
/// phrase per pixel; phrase-mode (XADDPHR) packs 64/bpp pixels per phrase.
const BLIT_LAUNCH_TICKS: u64 = 16;
/// Ticks per DRAM phrase access ×10 (5.6), kept integer for exactness.
const BLIT_ACCESS_TICKS_X10: u64 = 56;

/// Decode the 6-bit floating WIDTH field (4-bit exp, 2-bit mantissa + implied 1)
/// into a pixel count: `((4 + mant) << exp) >> 2` (spec §2.2).
fn decode_width(field: u32) -> u32 {
    let exp = (field >> 2) & 0xF;
    let mant = field & 3;
    (((4 + mant) << exp) >> 2).max(1)
}

/// Apply the LFU (4-bit logic function) per the selected min-terms (spec §5).
#[inline]
fn apply_lfu(lfu: u32, src: u32, dst: u32) -> u32 {
    let mut out = 0u32;
    if lfu & mem::BC_LFU_A != 0 {
        out |= src & dst;
    }
    if lfu & mem::BC_LFU_AN != 0 {
        out |= src & !dst;
    }
    if lfu & mem::BC_LFU_NA != 0 {
        out |= !src & dst;
    }
    if lfu & mem::BC_LFU_NAN != 0 {
        out |= !src & !dst;
    }
    out
}

#[inline]
fn sext16(v: u32) -> i32 {
    (v as u16) as i16 as i32
}

/// Saturating component-wise add of two 16-bit CRY/RGB pixels (`ADDDSEL`,
/// spec §4.2): 8-bit intensity (low byte) + two 4-bit chroma nibbles, each
/// clamped, no carry across fields.
#[inline]
fn sat_add16(a: u32, b: u32) -> u32 {
    let y = ((a & 0xFF) + (b & 0xFF)).min(0xFF);
    let cy = (((a >> 8) & 0xF) + ((b >> 8) & 0xF)).min(0xF);
    let cr = (((a >> 12) & 0xF) + ((b >> 12) & 0xF)).min(0xF);
    (cr << 12) | (cy << 8) | y
}

/// A blitter address generator (A1 or A2): a signed pixel pointer (X,Y) walking a
/// windowed, optionally pitched, linear pixel array (spec §2). Turns the current
/// pointer into a byte address + sub-byte bit offset, and advances per the
/// inner-loop XADD/YADD modes and the outer-loop STEP.
struct AddrGen {
    base: u32,
    width_px: u32,
    bpp: u32,
    pitch_phrases: u32, // 1 + inter-phrase gap
    x: i32,
    y: i32,
    xadd: u32, // 0=PHR, 1=PIX, 2=ZERO, 3=INC(DDA)
    yadd1: bool,
    xsignsub: bool,
    ysignsub: bool,
    // A2 pointer AND-mask (all-ones when masking disabled, so always applied).
    xmask: u32,
    ymask: u32,
    xstep: i32,
    ystep: i32,
    // 16.16 DDA state for XADDINC (A1 affine texture/line stepping): the current
    // pointer fraction and the per-inner-step increment (integer<<16 | fraction).
    xfrac: u32,
    yfrac: u32,
    xinc: i32,
    yinc: i32,
}

impl AddrGen {
    fn load(bus: &Bus, base_r: u32, flags_r: u32, pixel_r: u32, step_r: u32, is_a2: bool) -> Self {
        let w = &bus.tom.win;
        let flags = w.r32(flags_r);
        let pixel = w.r32(pixel_r);
        let step = w.r32(step_r);
        // PITCH (bits 0-1) → phrase gap 0/1/3/2 (spec §2.3); stride = (1+gap)·8.
        let gap = [0u32, 1, 3, 2][(flags & mem::AF_PITCH_MASK) as usize];
        let (xmask, ymask) = if is_a2 && flags & 0x8000 != 0 {
            let m = w.r32(mem::A2_MASK);
            (m & 0xFFFF, (m >> 16) & 0xFFFF)
        } else {
            (0xFFFF_FFFF, 0xFFFF_FFFF)
        };
        // The 16.16 DDA increment/fraction is A1-only (the source generator).
        // Combine the integer word (A1_INC) and the fraction word (A1_FINC) into a
        // signed 16.16 step; seed the pointer fraction from A1_FPIXEL.
        let (fpixel, inc, finc) = if is_a2 {
            (0, 0, 0)
        } else {
            (w.r32(mem::A1_FPIXEL), w.r32(mem::A1_INC), w.r32(mem::A1_FINC))
        };
        let xinc = (((inc & 0xFFFF) << 16) | (finc & 0xFFFF)) as i32;
        let yinc = ((((inc >> 16) & 0xFFFF) << 16) | ((finc >> 16) & 0xFFFF)) as i32;
        AddrGen {
            base: w.r32(base_r) & !7, // phrase-aligned
            width_px: decode_width((flags & mem::AF_WIDTH_MASK) >> mem::AF_WIDTH_SHIFT),
            bpp: 1u32 << ((flags & mem::AF_PIXEL_MASK) >> mem::AF_PIXEL_SHIFT),
            pitch_phrases: 1 + gap,
            x: sext16(pixel & 0xFFFF),
            y: sext16((pixel >> 16) & 0xFFFF),
            xadd: (flags & mem::AF_XADD_MASK) >> mem::AF_XADD_SHIFT,
            yadd1: flags & mem::AF_YADD1 != 0,
            xsignsub: flags & mem::AF_XSIGNSUB != 0,
            ysignsub: flags & mem::AF_YSIGNSUB != 0,
            xmask,
            ymask,
            xstep: sext16(step & 0xFFFF),
            ystep: sext16((step >> 16) & 0xFFFF),
            xfrac: fpixel & 0xFFFF,
            yfrac: (fpixel >> 16) & 0xFFFF,
            xinc,
            yinc,
        }
    }

    /// Linear pixel index of the current pointer (X + WIDTH·Y), with the A2 mask
    /// and the 12-bit Y range applied (spec §2.2, §2.4).
    #[inline]
    fn pixel_index(&self) -> u32 {
        let xx = (self.x as u32) & self.xmask;
        let yy = ((self.y as u32) & self.ymask) & 0xFFF; // Y is 12-bit at the generator
        xx.wrapping_add(self.width_px.wrapping_mul(yy))
    }

    /// Locate the current pixel: `(pixel_index, byte_addr, bit_in_byte)`,
    /// honouring PITCH phrase gaps (big-endian, MSB-first). Computed once per
    /// pixel and shared by the source/dest reads and the data-register lane.
    #[inline]
    fn locate(&self) -> (u32, u32, u32) {
        let idx = self.pixel_index();
        let ppp = (64 / self.bpp).max(1); // pixels per phrase
        let phrase = idx / ppp;
        let bit_in_phrase = (idx % ppp) * self.bpp;
        let a = self
            .base
            .wrapping_add(phrase.wrapping_mul(self.pitch_phrases * 8))
            .wrapping_add(bit_in_phrase / 8);
        (idx, a, bit_in_phrase % 8)
    }

    #[inline]
    fn read_at(&self, bus: &mut Bus, a: u32, bit: u32) -> u32 {
        match self.bpp {
            32 => bus.read32(a),
            16 => bus.read16(a) as u32,
            8 => bus.read8(a) as u32,
            b => (bus.read8(a) as u32 >> (8 - b - bit)) & ((1 << b) - 1), // sub-byte MSB-first
        }
    }

    #[inline]
    fn write_at(&self, bus: &mut Bus, a: u32, bit: u32, val: u32) {
        match self.bpp {
            32 => bus.write32(a, val),
            16 => bus.write16(a, val as u16),
            8 => bus.write8(a, val as u8),
            b => {
                let mask = ((1u32 << b) - 1) as u8;
                let shift = (8 - b - bit) as u8;
                let old = bus.read8(a);
                bus.write8(a, (old & !(mask << shift)) | (((val as u8) & mask) << shift));
            }
        }
    }

    /// Advance one inner-loop step (one pixel in this functional model).
    #[inline]
    fn step_inner(&mut self) {
        // XADDINC (3): the 16.16 DDA — add the fractional increment to X *and* Y
        // (XADDINC overrides YADD), carrying the fraction. This is the affine
        // texture/line sampler; approximating it as +1 scrambles textures.
        if self.xadd == 3 {
            let xp = (((self.x as i64) << 16) | self.xfrac as i64) + self.xinc as i64;
            self.x = (xp >> 16) as i32;
            self.xfrac = (xp as u32) & 0xFFFF;
            let yp = (((self.y as i64) << 16) | self.yfrac as i64) + self.yinc as i64;
            self.y = (yp >> 16) as i32;
            self.yfrac = (yp as u32) & 0xFFFF;
            return;
        }
        match self.xadd {
            1 => self.x += if self.xsignsub { -1 } else { 1 }, // XADDPIX
            2 => {}                                             // XADD0: hold
            _ => self.x += 1, // XADDPHR (per-pixel walk; phrase gaps via addr())
        }
        if self.yadd1 {
            self.y += if self.ysignsub { -1 } else { 1 };
        }
    }

    #[inline]
    fn step_outer(&mut self) {
        self.x += self.xstep;
        self.y += self.ystep;
    }
}

/// Extract one `bpp`-wide pixel lane from a 64-bit data register pair
/// `[hi, lo]` (big-endian: lane 0 = the most-significant bits = leftmost pixel).
#[inline]
fn lane(reg: [u32; 2], bpp: u32, idx: u32) -> u32 {
    let phrase = ((reg[0] as u64) << 32) | reg[1] as u64;
    let bits = bpp * (idx % (64 / bpp).max(1));
    ((phrase >> (64 - bpp - bits)) & ((1u64 << bpp) - 1)) as u32
}

/// Execute the blit described by the current A1/A2/B registers (latched here at
/// the `B_CMD` write). `cmd` is the value just written to `B_CMD`. Follows the
/// reference inner/outer-loop model in spec §8.
pub fn run(bus: &mut Bus, cmd: u32) {
    let count = bus.tom.win.r32(mem::B_COUNT);
    // B_COUNT: outer (rows) high, inner (line length) low; 0 ⇒ 65536 each half.
    let decode = |v: u32| if v == 0 { 0x1_0000 } else { v };
    let outer = decode((count >> 16) & 0xFFFF);
    let inner = decode(count & 0xFFFF);

    // Latch the 64-bit data registers (high long at the equate, low at +4).
    let w = &bus.tom.win;
    let srcd = [w.r32(mem::B_SRCD), w.r32(mem::B_SRCD + 4)];
    let patd = [w.r32(mem::B_PATD), w.r32(mem::B_PATD + 4)];
    let dstd = [w.r32(mem::B_DSTD), w.r32(mem::B_DSTD + 4)];
    let a1_clip = w.r32(mem::A1_CLIP);

    // Two address generators; DSTA2 swaps which is destination vs source.
    let mut gens = [
        AddrGen::load(bus, mem::A1_BASE, mem::A1_FLAGS, mem::A1_PIXEL, mem::A1_STEP, false),
        AddrGen::load(bus, mem::A2_BASE, mem::A2_FLAGS, mem::A2_PIXEL, mem::A2_STEP, true),
    ];
    let (dst, src) = if cmd & mem::BC_DSTA2 != 0 { (1usize, 0usize) } else { (0usize, 1usize) };

    // Env-gated blit trace (AI-eyes debugging): JAGEMU_BLIT_TRACE=1 prints each
    // blit's command + both address generators. Optional JAGEMU_BLIT_LO/HI bound
    // the dst base to a region of interest (hex), e.g. the framebuffer.
    if std::env::var_os("JAGEMU_BLIT_TRACE").is_some() {
        let lo = std::env::var("JAGEMU_BLIT_LO").ok()
            .and_then(|s| u32::from_str_radix(s.trim_start_matches("0x"), 16).ok()).unwrap_or(0);
        let hi = std::env::var("JAGEMU_BLIT_HI").ok()
            .and_then(|s| u32::from_str_radix(s.trim_start_matches("0x"), 16).ok()).unwrap_or(0xFFFF_FFFF);
        let db = gens[dst].base;
        if db >= lo && db <= hi {
            let d = &gens[dst];
            let s = &gens[src];
            eprintln!(
                "BLIT cmd={:08X} O={} I={} | DST base={:06X} x={} y={} w={} bpp={} pitch={} xadd={} yadd1={} step=({},{}) | \
                 SRC base={:06X} x={} y={} w={} bpp={} pitch={} xadd={} yadd1={} step=({},{})",
                cmd, outer, inner,
                d.base, d.x, d.y, d.width_px, d.bpp, d.pitch_phrases, d.xadd, d.yadd1, d.xstep, d.ystep,
                s.base, s.x, s.y, s.width_px, s.bpp, s.pitch_phrases, s.xadd, s.yadd1, s.xstep, s.ystep,
            );
        }
    }

    let lfu = cmd & mem::BC_LFU_MASK;
    let srcen = cmd & mem::BC_SRCEN != 0;
    let dsten = cmd & mem::BC_DSTEN != 0;
    let patdsel = cmd & mem::BC_PATDSEL != 0;
    let adddsel = cmd & mem::BC_ADDDSEL != 0;
    let gourd = cmd & mem::BC_GOURD != 0;
    let clip_a1 = cmd & mem::BC_CLIP_A1 != 0;
    let dcompen = cmd & mem::BC_DCOMPEN != 0;
    let cmpdst = cmd & mem::BC_CMPDST != 0;
    let bkgwren = cmd & mem::BC_BKGWREN != 0;
    let pixel_mode = gens[dst].xadd == 1; // XADDPIX
    let clip_w = (a1_clip & 0x7FFF) as i32;
    let clip_h = ((a1_clip >> 16) & 0x7FFF) as i32;
    // Read the destination pixel only when something consumes it: an LFU that
    // depends on D (REPLACE/CLEAR don't), the signed-add blend, a CMPDST
    // transparent compare, or a phrase-mode inhibit (which writes the read dest
    // back, spec §4.5). DSTEN forces the read regardless.
    let need_dst = dsten
        || adddsel
        || (dcompen && cmpdst)
        || (!patdsel && !gourd && lfu != mem::LFU_REPLACE && lfu != mem::LFU_CLEAR)
        || (!pixel_mode && (clip_a1 || dcompen));

    let dbpp = gens[dst].bpp;
    let dmask = if dbpp >= 32 { 0xFFFF_FFFF } else { (1u32 << dbpp) - 1 };

    // `inner` is the line length in PIXELS regardless of XADD mode — phrase mode
    // (XADDPHR) is just a 4-px/cycle hardware optimisation, not 4× the pixels
    // (spec §1.1, §10.2: B_COUNT inner = RENDER_W in pixels). Each iteration is
    // one pixel; the X pointer advances one pixel per step (see step_inner).
    // Safety bound: B_COUNT encodes 0 as 65536, so a garbage count could mean
    // ~4 billion synchronous iterations. No real blit exceeds the 2 MB DRAM as
    // pixels (~1M); cap above any legitimate blit so only pathological counts trip.
    let mut budget: u64 = 4_000_000;
    'rows: for _ in 0..outer {
        // Capture the line's starting pointers for phrase-mode realignment (6b).
        let dx0 = gens[dst].x;
        let sx0 = gens[src].x;
        for _ in 0..inner {
            if budget == 0 {
                break 'rows;
            }
            budget -= 1;
            // 1. Source pixel: read through A2/A1 when SRCEN, else the B_SRCD reg.
            let (lane_idx, da, dbit) = gens[dst].locate();
            let s = if srcen {
                let (_, sa, sbit) = gens[src].locate();
                gens[src].read_at(bus, sa, sbit)
            } else {
                lane(srcd, dbpp, lane_idx)
            };
            // 2. Destination read (for LFU/compare/inhibit restore).
            let d = if need_dst { gens[dst].read_at(bus, da, dbit) } else { 0 };
            // 3. Write data select (spec §4.2).
            let wd = if patdsel || gourd {
                lane(patd, dbpp, lane_idx) // GOURD: static B_PATD (computation deferred)
            } else if adddsel {
                sat_add16(s, d)
            } else {
                apply_lfu(lfu, s, d)
            } & dmask;
            // 4. Write-inhibit decisions (spec §4.5, §2.5).
            let mut inhibit = false;
            if clip_a1 {
                let g = &gens[0]; // CLIP always tests A1's window
                if g.x < 0 || g.y < 0 || g.x >= clip_w || g.y >= clip_h {
                    inhibit = true;
                }
            }
            if dcompen {
                // Transparent copy: inhibit when the compared pixel equals B_PATD.
                let cmpval = if cmpdst { d } else { s };
                if cmpval == (lane(patd, dbpp, lane_idx) & dmask) {
                    inhibit = true;
                }
            }
            // 5. Write (or background/restore on inhibit, spec §4.5).
            if !inhibit {
                gens[dst].write_at(bus, da, dbit, wd);
            } else if pixel_mode {
                if bkgwren {
                    gens[dst].write_at(bus, da, dbit, lane(dstd, dbpp, lane_idx) & dmask);
                }
            } else {
                // Phrase mode: an inhibited pixel still writes back the (read) dest.
                gens[dst].write_at(bus, da, dbit, d & dmask);
            }
            if let Some(wa) = WATCH_ADDR.get_or_init(|| {
                std::env::var("JAGEMU_WATCH").ok()
                    .and_then(|s| u32::from_str_radix(s.trim_start_matches("0x"), 16).ok())
            }) {
                if da == *wa {
                    eprintln!(
                        "WATCH {:06X} <- {:04X} (inhibit={}) by cmd={:08X} dst(x={},y={},w={},pitch={},xadd={}) O={} I={}",
                        da, wd, inhibit, cmd, gens[dst].x, gens[dst].y, gens[dst].width_px,
                        gens[dst].pitch_phrases, gens[dst].xadd, outer, inner
                    );
                }
            }
            // 6. Advance inner pointers.
            gens[dst].step_inner();
            if srcen {
                gens[src].step_inner();
            }
        }
        // 6b. Phrase-mode (XADDPHR) end-of-line realignment. The hardware walks
        // a whole number of PHRASES per line, sized by the destination: a line of
        // `inner` pixels starting at sub-phrase offset `off` spans
        // ceil((off+inner)/pps) phrases. After the line both pointers jump to that
        // phrase boundary before the outer STEP. Two consequences the per-pixel
        // inner loop misses:
        //   • The destination X rounds up to its next phrase boundary (a 12-px
        //     glyph at X=85 ends at 100, not 97 — so the game's STEP=-15 returns
        //     to 85 instead of drifting to 82).
        //   • The SOURCE advances by the SAME phrase count as the destination, not
        //     by `inner` pixels. A 12-px glyph whose destination spans 4 phrases
        //     reads 16 source pixels (the masked edge pixels are still consumed),
        //     so a 12-pixel-pitch font with SRC STEP=-4 nets +12/line. Driving the
        //     phrase count from the source instead would short-read and shear.
        // Pixel mode (XADDPIX) advances exactly per pixel and needs neither.
        if gens[dst].xadd == 0 {
            let dpps = (64 / gens[dst].bpp.max(1)).max(1) as i32;
            let doff = dx0.rem_euclid(dpps);
            let nphrases = (doff + inner as i32 + dpps - 1) / dpps; // ceil
            gens[dst].x = (dx0 - doff) + nphrases * dpps;
            if srcen && gens[src].xadd == 0 {
                let spps = (64 / gens[src].bpp.max(1)).max(1) as i32;
                let soff = sx0.rem_euclid(spps);
                gens[src].x = (sx0 - soff) + nphrases * spps;
            }
        }
        // 7. Outer-loop pointer updates (spec §5.3).
        if cmd & mem::BC_UPDA1 != 0 {
            gens[0].step_outer();
        }
        if cmd & mem::BC_UPDA2 != 0 {
            gens[1].step_outer();
        }
    }

    // Charge the blit its DRAM bus time (see BLIT_* constants). Counted in
    // phrase accesses: every line writes ceil(inner/ppp_dst) dest phrases, and
    // with SRCEN reads ceil(inner/ppp_src) source phrases. The launching
    // `B_CMD` store picks this up in the timed RISC step.
    let ppp = |g: &AddrGen| -> u64 {
        if g.xadd == 0 { (64 / g.bpp.max(1)).max(1) as u64 } else { 1 }
    };
    let per_line = |g: &AddrGen| -> u64 { (inner as u64).div_ceil(ppp(g)) };
    let dst_phrases = outer as u64 * per_line(&gens[dst]);
    let src_phrases = if srcen { outer as u64 * per_line(&gens[src]) } else { 0 };
    // DSTEN is a read-modify-write: every dest phrase is READ before the
    // logic op and the write-back, so it pays twice. This was uncharged —
    // invisible in the Caves/NOFILL anchors (no DSTEN in those paths) but a
    // systematic under-charge on OpenLara's shade pass, whose per-span
    // DSTEN|LFU(S|D) blits DOUBLE the launch count
    // (COBWEB_REQ_rectshade_and_calibration §2: jsim +30% optimistic on the
    // SHADED build only). Physics, not tuning: the constant is unchanged,
    // the access count now includes the reads the hardware performs.
    let dst_reads = if dsten { dst_phrases } else { 0 };
    let transfer = (dst_phrases + dst_reads + src_phrases) * BLIT_ACCESS_TICKS_X10 / 10;
    bus.tom.last_blit_launch = BLIT_LAUNCH_TICKS;
    bus.tom.last_blit_ticks = BLIT_LAUNCH_TICKS + transfer;
    bus.tom.blit_busy += BLIT_LAUNCH_TICKS + transfer; // asynchronous: drains as wall time passes
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mem;

    fn setup_fill_regs(bus: &mut Bus, fb: u32, x: u32, y: u32, n: u32, rows: u32, color: u16, upda1: bool) {
        bus.tom.win.w32(mem::A1_BASE, fb);
        bus.tom.win.w32(mem::A1_FLAGS, mem::AF_XADDPIX | 0x0000_4200 | (4 << mem::AF_PIXEL_SHIFT)); // PIX16|WID320|XPIX
        bus.tom.win.w32(mem::A1_PIXEL, (y << 16) | x);
        bus.tom.win.w32(mem::A1_STEP, (1 << 16) | ((-(320i32)) as u32 & 0xFFFF));
        let cc = ((color as u32) << 16) | color as u32;
        bus.tom.win.w32(mem::B_SRCD, cc);
        bus.tom.win.w32(mem::B_SRCD + 4, cc);
        bus.tom.win.w32(mem::B_COUNT, (rows << 16) | n);
        let _ = upda1;
    }

    #[test]
    fn blitter_reads_idle() {
        let mut bus = Bus::new();
        assert_eq!(bus.read32(mem::B_CMD) & BLIT_IDLE, BLIT_IDLE);
    }

    #[test]
    fn blit_cost_matches_hardware_probe() {
        // Mirrors the Skunkboard p_blitsm/p_blitbg probes (bench 2026-07-19):
        // SRCEN|LFU_REPLACE|DSTA2, 8bpp pixel-mode copy. Each pixel is one source
        // read phrase + one dest write phrase, so cost = 16 + 2*N*5.6 ticks.
        let mut bus = Bus::new();
        let setup = |bus: &mut Bus, n: u32| {
            bus.tom.win.w32(mem::A1_BASE, 0x14_0000);
            bus.tom.win.w32(mem::A1_FLAGS, 0x0001_4218); // PITCH1|PIXEL8|WID320|XADDPIX
            bus.tom.win.w32(mem::A1_PIXEL, 0);
            bus.tom.win.w32(mem::A2_BASE, 0x18_0000);
            bus.tom.win.w32(mem::A2_FLAGS, 0x0001_4218);
            bus.tom.win.w32(mem::A2_PIXEL, 0);
            bus.tom.win.w32(mem::B_COUNT, (1 << 16) | n);
        };
        // 256-px copy → 16 + (256+256)*56/10 = 2883 ticks (measured ~2890).
        setup(&mut bus, 256);
        bus.write32(mem::B_CMD, 0x0180_0801);
        assert_eq!(bus.tom.last_blit_ticks, 16 + (256 + 256) * 56 / 10);
        // 8-px copy → far cheaper, matching p_blitsm.
        setup(&mut bus, 8);
        bus.write32(mem::B_CMD, 0x0180_0801);
        assert_eq!(bus.tom.last_blit_ticks, 16 + (8 + 8) * 56 / 10);
    }

    #[test]
    fn solid_span_fill() {
        let mut bus = Bus::new();
        let fb = 0x10_0000u32;
        setup_fill_regs(&mut bus, fb, 5, 2, 10, 1, 0xF800, false);
        // Writing B_CMD triggers the blit (LFU replace).
        bus.write32(mem::B_CMD, mem::LFU_REPLACE);
        // 10 pixels at (5..15, row 2), width 320 → offset (2*320+5)*2.
        for i in 0..10u32 {
            let addr = fb + (2 * 320 + 5 + i) * 2;
            assert_eq!(bus.read16(addr), 0xF800, "pixel {i}");
        }
        // Pixel just before the span is untouched.
        assert_eq!(bus.read16(fb + (2 * 320 + 4) * 2), 0x0000);
    }

    #[test]
    fn watchpoint_attributes_blitter_writes() {
        // Write-watch on the fill target: hits must be attributed to the
        // BLITTER, not to the master that stored B_CMD ("who wrote this
        // byte" — COBWEB_REQ_rectshade_and_calibration §5.1).
        let mut bus = Bus::new();
        let fb = 0x10_0000u32;
        bus.add_watch(fb, fb + 0x1000);
        setup_fill_regs(&mut bus, fb, 5, 2, 10, 1, 0xF800, false);
        bus.write32(mem::B_CMD, mem::LFU_REPLACE);
        assert!(bus.watch_total >= 10, "fill writes logged: {}", bus.watch_total);
        assert!(
            bus.watch_log.iter().all(|h| h.master == crate::bus::Master::Blitter),
            "all hits blitter-attributed: {:?}",
            bus.watch_log.first()
        );
        // ...and a direct CPU-side store attributes to the current master.
        bus.cur_master = crate::bus::Master::Cpu;
        bus.cur_master_pc = 0x4242;
        let before = bus.watch_total;
        bus.write16(fb + 0x800, 0xBEEF);
        assert_eq!(bus.watch_total, before + 1);
        let last = *bus.watch_log.last().unwrap();
        assert_eq!(last.master, crate::bus::Master::Cpu);
        assert_eq!(last.pc, 0x4242);
        assert_eq!(last.size, 16);
    }

    #[test]
    fn band_fill_with_upda1() {
        let mut bus = Bus::new();
        let fb = 0x10_0000u32;
        // 3 rows of 320 px starting at y=1, x=0, color green ($003F).
        setup_fill_regs(&mut bus, fb, 0, 1, 320, 3, 0x003F, true);
        bus.write32(mem::B_CMD, mem::BC_UPDA1 | mem::LFU_REPLACE);
        for row in 1..4u32 {
            for col in [0u32, 100, 319] {
                assert_eq!(bus.read16(fb + (row * 320 + col) * 2), 0x003F, "row {row} col {col}");
            }
        }
        // Row 0 (above the band) stays clear.
        assert_eq!(bus.read16(fb + 100 * 2), 0x0000);
    }

    /// Memory-to-memory copy via SRCEN + the A2 source generator — the path
    /// commercial games use to load GPU/DSP programs. This is what was missing.
    #[test]
    fn srcen_memory_copy() {
        let mut bus = Bus::new();
        let (src, dst) = (0x10_0000u32, 0x12_0000u32);
        // Seed 16 distinct 16-bit words at the source.
        for i in 0..16u32 {
            bus.write16(src + i * 2, 0x1000 + i as u16);
        }
        // A2 = source, A1 = destination, both PIX16 | WID320 | XADDPIX.
        let flags = mem::AF_XADDPIX | 0x0000_4200 | (4 << mem::AF_PIXEL_SHIFT);
        bus.tom.win.w32(mem::A2_BASE, src);
        bus.tom.win.w32(mem::A2_FLAGS, flags);
        bus.tom.win.w32(mem::A2_PIXEL, 0);
        bus.tom.win.w32(mem::A1_BASE, dst);
        bus.tom.win.w32(mem::A1_FLAGS, flags);
        bus.tom.win.w32(mem::A1_PIXEL, 0);
        bus.tom.win.w32(mem::B_COUNT, (1 << 16) | 16); // 1 row, 16 pixels
        bus.write32(mem::B_CMD, mem::BC_SRCEN | mem::LFU_REPLACE);
        for i in 0..16u32 {
            assert_eq!(bus.read16(dst + i * 2), 0x1000 + i as u16, "copied word {i}");
        }
        // The word past the copy is untouched.
        assert_eq!(bus.read16(dst + 16 * 2), 0x0000);
    }

    /// XADDINC (A1 affine DDA): the source pointer advances by the 16.16
    /// increment (`A1_INC`/`A1_FINC`), sampling the texture at fractional steps.
    /// With `Xinc=2.5`, dest pixel i reads source texel floor(i·2.5) — not i
    /// (which the old `≈+1` approximation produced).
    #[test]
    fn xaddinc_affine_dda_sampling() {
        let mut bus = Bus::new();
        let (src, dst) = (0x10_0000u32, 0x12_0000u32);
        // Source texture: texel i holds value i (8bpp).
        for i in 0..64u32 {
            bus.write8(src + i, i as u8);
        }
        // A1 = source, XADDINC, 8bpp; increment 2.5 texels/pixel (int 2 + 0x8000).
        let sflags = 0x0003_0000 | 0x0000_4200 | (3 << mem::AF_PIXEL_SHIFT);
        bus.tom.win.w32(mem::A1_BASE, src);
        bus.tom.win.w32(mem::A1_FLAGS, sflags);
        bus.tom.win.w32(mem::A1_PIXEL, 0);
        bus.tom.win.w32(mem::A1_FPIXEL, 0);
        bus.tom.win.w32(mem::A1_INC, 0x0000_0002); // Xinc=2, Yinc=0
        bus.tom.win.w32(mem::A1_FINC, 0x0000_8000); // Xfinc=0.5
        // A2 = dest, linear (XADDPIX), 8bpp.
        let dflags = mem::AF_XADDPIX | 0x0000_4200 | (3 << mem::AF_PIXEL_SHIFT);
        bus.tom.win.w32(mem::A2_BASE, dst);
        bus.tom.win.w32(mem::A2_FLAGS, dflags);
        bus.tom.win.w32(mem::A2_PIXEL, 0);
        bus.tom.win.w32(mem::B_COUNT, (1 << 16) | 6); // 1 row, 6 pixels
        bus.write32(mem::B_CMD, mem::BC_DSTA2 | mem::BC_SRCEN | mem::LFU_REPLACE);
        // floor(i*2.5): 0, 2, 5, 7, 10, 12
        for (i, &exp) in [0u8, 2, 5, 7, 10, 12].iter().enumerate() {
            assert_eq!(bus.read8(dst + i as u32), exp, "dda pixel {i}");
        }
    }

    /// Transparent copy: DCOMPEN inhibits writes whose source equals B_PATD.
    #[test]
    fn dcompen_transparent_copy() {
        let mut bus = Bus::new();
        let (src, dst) = (0x10_0000u32, 0x12_0000u32);
        // Source: [AAAA, 0000(transparent), BBBB, 0000]; dest pre-filled 0xFFFF.
        let srcpx = [0xAAAAu16, 0x0000, 0xBBBB, 0x0000];
        for (i, &p) in srcpx.iter().enumerate() {
            bus.write16(src + i as u32 * 2, p);
        }
        for i in 0..4u32 {
            bus.write16(dst + i * 2, 0xFFFF);
        }
        let flags = mem::AF_XADDPIX | 0x0000_4200 | (4 << mem::AF_PIXEL_SHIFT);
        bus.tom.win.w32(mem::A2_BASE, src);
        bus.tom.win.w32(mem::A2_FLAGS, flags);
        bus.tom.win.w32(mem::A2_PIXEL, 0);
        bus.tom.win.w32(mem::A1_BASE, dst);
        bus.tom.win.w32(mem::A1_FLAGS, flags);
        bus.tom.win.w32(mem::A1_PIXEL, 0);
        bus.tom.win.w32(mem::B_PATD, 0); // transparent colour = 0 in all lanes
        bus.tom.win.w32(mem::B_PATD + 4, 0);
        bus.tom.win.w32(mem::B_COUNT, (1 << 16) | 4);
        bus.write32(mem::B_CMD, mem::BC_SRCEN | mem::BC_DCOMPEN | mem::LFU_REPLACE);
        assert_eq!(bus.read16(dst), 0xAAAA); // opaque copied
        assert_eq!(bus.read16(dst + 2), 0xFFFF); // transparent: dest preserved
        assert_eq!(bus.read16(dst + 4), 0xBBBB); // opaque copied
        assert_eq!(bus.read16(dst + 6), 0xFFFF); // transparent: dest preserved
    }

    /// Phrase-mode (`XADDPHR`) end-of-line pointer realignment (spec §3.4 / §8:
    /// "add phrase width and truncate X to the next phrase boundary"). A glyph-like
    /// multi-row fill that starts at a sub-phrase X offset with a width that is NOT
    /// a phrase multiple must have its X pointer rounded UP to the next phrase
    /// boundary at end of line, *before* the UPDA1 STEP — otherwise successive rows
    /// drift, which is exactly how a phrase-mode font shears. Here: 16bpp ⇒ 4 px /
    /// phrase, start X=2 (sub-phrase), inner=5 px. Each line spans
    /// ceil((2+5)/4)=2 phrases, so X ends at the phrase boundary 8 (not 2+5=7), and
    /// a phrase-aware game's STEP=-6 returns it to X=2 every row. Without the
    /// realignment the per-pixel pointer ends at 7 and STEP=-6 drifts it left by one
    /// column per row.
    #[test]
    fn phrase_mode_line_realigns_to_phrase_boundary() {
        let mut bus = Bus::new();
        let fb = 0x10_0000u32;
        let w = 320u32;
        let color = 0x07FFu16;
        // PIX16 | WID320 | XADDPHR (phrase mode = XADD bits 00).
        let flags = mem::AF_XADDPHR | 0x0000_4200 | (4 << mem::AF_PIXEL_SHIFT);
        bus.tom.win.w32(mem::A1_BASE, fb);
        bus.tom.win.w32(mem::A1_FLAGS, flags);
        bus.tom.win.w32(mem::A1_PIXEL, 2); // X=2, Y=0
        // Ystep=+1, Xstep=-6 (phrase-aware: returns X=8 back to X=2).
        bus.tom.win.w32(mem::A1_STEP, (1 << 16) | ((-6i32) as u32 & 0xFFFF));
        let cc = ((color as u32) << 16) | color as u32;
        bus.tom.win.w32(mem::B_SRCD, cc);
        bus.tom.win.w32(mem::B_SRCD + 4, cc);
        bus.tom.win.w32(mem::B_COUNT, (3 << 16) | 5); // 3 rows, 5 px each
        bus.write32(mem::B_CMD, mem::BC_UPDA1 | mem::LFU_REPLACE);
        // Every row paints exactly columns 2..=6 — no drift.
        for y in 0..3u32 {
            let px = |bus: &mut Bus, x: u32| bus.read16(fb + (y * w + x) * 2);
            assert_eq!(px(&mut bus, 0), 0, "row {y} col 0 must stay clear (drift guard)");
            assert_eq!(px(&mut bus, 1), 0, "row {y} col 1 must stay clear (drift guard)");
            for x in 2..=6u32 {
                assert_eq!(px(&mut bus, x), color, "row {y} col {x} should be painted");
            }
            assert_eq!(px(&mut bus, 7), 0, "row {y} col 7 must stay clear");
        }
    }
}
