//! Deterministic timing: maps 68k cycles onto scanlines and frames, fires the
//! vertical interrupt, advances the VC counter, and gives the GPU/DSP their
//! cycle budget. See `docs/spec/VIDEO_TIMING.md`.
//!
//! NTSC: 68000 ≈ 13.295 MHz, RISC = 2× that. VP=523 → 524 half-lines → 262
//! lines/field at ≈59.94 Hz. VC counts **half-lines** (bit 11 = field); the VI
//! register is a half-line value the video interrupt fires on.

use crate::bus::Bus;
use crate::m68k::M68k;
use crate::mem;
use crate::risc::Risc;

pub struct Scheduler {
    /// Absolute frame (field) number since reset.
    pub frame: u64,
    /// Current half-line within the frame (0..half_lines_per_frame).
    pub half_line: u32,
    /// 68k cycles accumulated toward the next half-line boundary.
    cycle_acc: i64,

    pub cpu_clock_hz: u32,
    pub cpu_cycles_per_half_line: i64,
    pub half_lines_per_frame: u32,

    /// True once the VI has fired this frame (fire exactly once per field).
    vi_fired_this_frame: bool,

    /// Down-counters (in RISC-clock ticks) for the programmable timers. The
    /// Tom PIT raises INT1 bit 3; the two Jerry timers raise INT1 bit 4.
    pit_counter: i64,
    jtimer1_counter: i64,
    jtimer2_counter: i64,
    /// Audio sample down-counter (RISC-clock ticks until the next sample).
    audio_counter: i64,
}

impl Scheduler {
    pub fn ntsc() -> Self {
        // 13_295_453 Hz / 59.94 Hz ≈ 221_800 cyc/frame; /524 half-lines ≈ 423.
        Scheduler {
            frame: 0,
            half_line: 0,
            cycle_acc: 0,
            cpu_clock_hz: 13_295_453,
            cpu_cycles_per_half_line: 423,
            half_lines_per_frame: 524,
            vi_fired_this_frame: false,
            pit_counter: 0,
            jtimer1_counter: 0,
            jtimer2_counter: 0,
            audio_counter: 0,
        }
    }

    pub fn pal() -> Self {
        // 13_296_950 Hz / 50 Hz ≈ 265_939 cyc/frame; /624 half-lines ≈ 426.
        Scheduler {
            frame: 0,
            half_line: 0,
            cycle_acc: 0,
            cpu_clock_hz: 13_296_950,
            cpu_cycles_per_half_line: 426,
            half_lines_per_frame: 624,
            vi_fired_this_frame: false,
            pit_counter: 0,
            jtimer1_counter: 0,
            jtimer2_counter: 0,
            audio_counter: 0,
        }
    }

    pub fn reset(&mut self) {
        self.frame = 0;
        self.half_line = 0;
        self.cycle_acc = 0;
        self.vi_fired_this_frame = false;
        self.pit_counter = 0;
        self.jtimer1_counter = 0;
        self.jtimer2_counter = 0;
        self.audio_counter = 0;
    }

    /// Approximate scanline the OP/display would currently be on.
    pub fn line(&self) -> u32 {
        self.half_line / 2
    }

    /// Advance the timeline by `cpu_cycles`, firing video interrupts and giving
    /// the RISC engines their budget.
    pub fn advance(
        &mut self,
        cpu_cycles: u32,
        cpu: &mut M68k,
        gpu: &mut Risc,
        dsp: &mut Risc,
        bus: &mut Bus,
    ) {
        self.cycle_acc += cpu_cycles as i64;
        while self.cycle_acc >= self.cpu_cycles_per_half_line {
            self.cycle_acc -= self.cpu_cycles_per_half_line;
            self.half_line += 1;
            if self.half_line >= self.half_lines_per_frame {
                // Wrap BEFORE publishing VC: hardware counts 0..523 (the
                // calibration bench measured max VC = 523, modulus 524).
                self.half_line = 0;
                self.frame += 1;
                self.vi_fired_this_frame = false;
                // New field: the OP re-sizes/clears its canvas at the next
                // active line. The framebuffer persists for `capture_frame`.
                bus.tom.op.started = false;
            }

            // Update VC (half-line counter, bit 11 = field) for games that poll it.
            let field = if self.half_line >= self.half_lines_per_frame / 2 {
                0x800
            } else {
                0
            };
            bus.tom.win.w16(mem::VC, ((self.half_line & 0x7FF) | field) as u16);

            // Drive the Object Processor: composite one display line at scan-out
            // time. Run on even half-lines only (non-interlaced = one OP pass per
            // visible line), sampling the live object list as it is this instant.
            if self.half_line & 1 == 0 {
                crate::tom::op_render_line((self.half_line & 0x7FF) as u16, cpu, gpu, bus);
            }

            // Fire the vertical interrupt at the programmed half-line, once per
            // field. The pending latch (INT1 bit 0) is set regardless of the
            // enable — games with interrupts masked poll it for vblank. If
            // enabled, also raise the 68k level-2 interrupt.
            if !self.vi_fired_this_frame {
                let vi = bus.tom.win.r16(mem::VI);
                if vi != 0xFFFF && self.half_line >= vi as u32 {
                    bus.tom.int1_pending |= mem::C_VIDENA;
                    if bus.tom.int1_enable & mem::C_VIDENA != 0 {
                        cpu.request_interrupt(2);
                    }
                    self.vi_fired_this_frame = true;
                }
            }

        }

        // Programmable timers run on the RISC clock (~2× the 68k). Each is a
        // two-stage down-counter `(prescaler+1)*(divider+1)`; on expiry it
        // latches its interrupt pending and (if enabled) raises the 68k IRQ.
        let risc_ticks = (cpu_cycles as i64) * 2;
        self.tick_timers(risc_ticks, cpu, bus);

        // Audio: at the sample rate, fire the DSP I2S interrupt (so sound
        // engines compute the next sample) and capture the current output.
        if bus.audio_capture {
            self.tick_audio(risc_ticks, dsp, bus);
        }

        // The GPU/DSP run at ~2× the 68k clock. Give them their budget when the
        // RISCGO bit is set in their control register. A running (non-STOPped)
        // 68k occupies the main bus — external RISC accesses pay for it.
        bus.m68k_on_bus = !cpu.stopped;
        let budget = (cpu_cycles * 2).max(1);
        if bus.tom.win.r32(mem::G_CTRL) & mem::RISCGO != 0 {
            gpu.run(bus, budget);
        }
        if bus.jerry.win.r32(mem::D_CTRL) & mem::RISCGO != 0 {
            dsp.run(bus, budget);
        }
    }

    /// Advance the programmable timers by `ticks` RISC-clock ticks.
    fn tick_timers(&mut self, ticks: i64, cpu: &mut M68k, bus: &mut Bus) {
        // Tom PIT → INT1 bit 3.
        let pit0 = bus.tom.win.r16(mem::PIT0) as i64;
        let pit1 = bus.tom.win.r16(mem::PIT1) as i64;
        if pit0 != 0 || pit1 != 0 {
            let period = (pit0 + 1) * (pit1 + 1);
            if self.pit_counter <= 0 {
                self.pit_counter = period;
            }
            self.pit_counter -= ticks;
            if self.pit_counter <= 0 {
                self.pit_counter += period;
                bus.tom.int1_pending |= mem::C_PITENA;
                if bus.tom.int1_enable & mem::C_PITENA != 0 {
                    cpu.request_interrupt(2);
                }
            }
        }

        // Jerry timers 1 (JPIT1/JPIT2) and 2 (JPIT3/JPIT4) → INT1 bit 4 (Jerry).
        Self::tick_jerry(
            &mut self.jtimer1_counter,
            bus.jerry.win.r16(mem::JPIT1) as i64,
            bus.jerry.win.r16(mem::JPIT2) as i64,
            ticks,
            cpu,
            bus,
        );
        Self::tick_jerry(
            &mut self.jtimer2_counter,
            bus.jerry.win.r16(mem::JPIT3) as i64,
            bus.jerry.win.r16(mem::JPIT4) as i64,
            ticks,
            cpu,
            bus,
        );
    }

    /// Sample the audio output at the I2S frame rate. The rate is derived from
    /// SCLK (`sysclk / (64*(SCLK+1))` for 16-bit stereo I2S), defaulting to
    /// ~44.1 kHz when SCLK is unset. Captures L_I2S/R_I2S, falling back to the
    /// PWM DACs when I2S is silent.
    fn tick_audio(&mut self, ticks: i64, dsp: &mut Risc, bus: &mut Bus) {
        const SYSCLK: i64 = 26_590_906;
        let sclk = bus.jerry.win.r16(mem::SCLK) as i64;
        let period = if sclk > 3 { 64 * (sclk + 1) } else { 603 };
        bus.audio_rate = (SYSCLK / period.max(1)) as u32;

        self.audio_counter -= ticks;
        let mut guard = 0;
        while self.audio_counter <= 0 && guard < 8 {
            guard += 1;
            self.audio_counter += period;
            // Tick the DSP's per-sample I2S interrupt.
            dsp.raise_int(mem::DSP_INT_I2S);
            // Capture the current stereo sample.
            let mut l = bus.jerry.win.r16(mem::L_I2S) as i16;
            let mut r = bus.jerry.win.r16(mem::R_I2S) as i16;
            if l == 0 && r == 0 {
                l = bus.jerry.win.r16(mem::DAC1) as i16;
                r = bus.jerry.win.r16(mem::DAC2) as i16;
            }
            bus.audio.push(l);
            bus.audio.push(r);
        }
    }

    fn tick_jerry(counter: &mut i64, pre: i64, div: i64, ticks: i64, cpu: &mut M68k, bus: &mut Bus) {
        if pre == 0 && div == 0 {
            return; // timer not programmed
        }
        let period = (pre + 1) * (div + 1);
        if *counter <= 0 {
            *counter = period;
        }
        *counter -= ticks;
        if *counter <= 0 {
            *counter += period;
            bus.tom.int1_pending |= mem::C_JERENA;
            if bus.tom.int1_enable & mem::C_JERENA != 0 {
                cpu.request_interrupt(2);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::risc::{Risc, RiscKind};

    /// The audio sampler captures whatever the game leaves in the I2S registers
    /// at the sample rate — verify with a held value.
    #[test]
    fn audio_sampler_captures_i2s() {
        let mut bus = Bus::new();
        bus.audio_capture = true;
        bus.jerry.win.w16(mem::L_I2S, 1234u16);
        bus.jerry.win.w16(mem::R_I2S, 5678u16);
        let mut sched = Scheduler::ntsc();
        let mut cpu = M68k::new();
        let mut gpu = Risc::new(RiscKind::Gpu);
        let mut dsp = Risc::new(RiscKind::Dsp);
        // Advance ~200k cpu cycles (~one frame) — many sample periods.
        for _ in 0..2000 {
            sched.advance(100, &mut cpu, &mut gpu, &mut dsp, &mut bus);
        }
        assert!(!bus.audio.is_empty(), "no samples captured");
        // Default rate ~44.1 kHz over ~1 frame (~1/60 s) → ~735 stereo samples.
        assert!(bus.audio.len() >= 2 * 600, "too few samples: {}", bus.audio.len());
        assert!(bus.audio.iter().any(|&s| s == 1234), "left channel not captured");
        assert!(bus.audio.iter().any(|&s| s == 5678), "right channel not captured");
        assert_eq!(bus.audio_rate, 44_097);
    }
}
