//! `jag-core` — the deterministic Atari Jaguar machine.
//!
//! No I/O, no threads, no wall-clock, no RNG. Same ROM + same injected inputs
//! ⇒ identical frames, every run. This is what makes BigPEmu-oracle diffing
//! meaningful and AI-driven debugging (breakpoint → run → inspect) repeatable.
//!
//! See `docs/ARCHITECTURE.md` for the borrow model and `docs/spec/*` for the
//! implementation-grade hardware specifications this code is built against.

pub mod bios;
pub mod bus;
pub mod cart;
pub mod debug;
pub mod jerry;
pub mod m68k;
pub mod mem;
pub mod risc;
pub mod scheduler;
pub mod tom;

pub use bus::Bus;
pub use cart::{Cartridge, LoadError};
pub use debug::{Debugger, StopReason};
pub use m68k::M68k;
pub use risc::{Risc, RiscKind};
pub use scheduler::Scheduler;
pub use tom::Framebuffer;

/// The whole machine. One instance = one console = one thread.
pub struct Jaguar {
    pub cpu: M68k,
    pub gpu: Risc,
    pub dsp: Risc,
    pub bus: Bus,
    pub sched: Scheduler,
    pub dbg: Debugger,
}

impl Default for Jaguar {
    fn default() -> Self {
        Self::new()
    }
}

impl Jaguar {
    pub fn new() -> Self {
        Jaguar {
            cpu: M68k::new(),
            gpu: Risc::new(RiscKind::Gpu),
            dsp: Risc::new(RiscKind::Dsp),
            bus: Bus::new(),
            sched: Scheduler::ntsc(),
            dbg: Debugger::new(),
        }
    }

    /// Load a program from any supported container (`.cof`/`.abs`/`.jag`/`.rom`/
    /// raw) and reset the machine to run it.
    pub fn load(&mut self, data: &[u8]) -> Result<Cartridge, LoadError> {
        let cart = cart::load(data, &mut self.bus)?;
        // SSP = top of DRAM for all loads. (The "HLE cart SSP = $4000" value from
        // VJ/spec §3.3 B is WRONG for our carts — it regresses AvP and Trevor,
        // which rely on a top-of-DRAM stack; most carts set their own SSP anyway.)
        self.reset_to(cart.entry);
        Ok(cart)
    }

    /// Reset all processors and point the 68000 at `entry`.
    ///
    /// HLE boot (no BIOS image): replicate the boot ROM's observable post-boot
    /// state via [`bios::install`] — seed the reset vector (`[$0]=SSP`,
    /// `[$4]=entry`), fill the exception-vector table (with the default level-2
    /// interrupt dispatcher at vector 64 `$100`, so a game that enables the VI
    /// before installing its own handler doesn't wild-jump to `$0`), set the
    /// RISC engines big-endian, and idle the NTSC joypad.
    pub fn reset_to(&mut self, entry: u32) {
        self.reset_to_ssp(entry, mem::DRAM_END);
    }

    /// As [`reset_to`], but with an explicit initial supervisor stack pointer
    /// (cart boot uses a low stack — see [`load`]).
    pub fn reset_to_ssp(&mut self, entry: u32, ssp: u32) {
        bios::install(&mut self.bus, entry, ssp);
        self.cpu.reset(&mut self.bus);
        self.cpu.set_pc(entry);
        self.gpu.reset();
        self.dsp.reset();
        self.sched.reset();
        // Fresh, deterministic OP state: blank canvas, re-sized at first field.
        self.bus.tom.op = tom::OpState::default();
        self.bus.tom.fb = tom::Framebuffer::solid(320, 240, 0, 0, 0);
    }

    /// Execute exactly one 68000 instruction (plus the device work scheduled
    /// alongside it). Returns the 68k cycles consumed.
    #[inline]
    pub fn step_instruction(&mut self) -> u32 {
        let cycles = self.cpu.step(&mut self.bus, &mut self.dbg);
        self.sched.advance(cycles, &mut self.cpu, &mut self.gpu, &mut self.dsp, &mut self.bus);
        cycles
    }

    /// Run until the start of frame `target_frame` (absolute frame number) or
    /// until the debugger trips. Deterministic.
    pub fn run_to_frame(&mut self, target_frame: u64) -> StopReason {
        let mut illegal_seen = self.cpu.illegal_count;
        while self.sched.frame < target_frame {
            if self.dbg.is_breakpoint(self.cpu.pc) {
                return StopReason::Breakpoint(self.cpu.pc);
            }
            self.step_instruction();
            // GPU/DSP PC breakpoints: the RISC core stops mid-slice and records
            // the hit so registers are frozen at that instruction for inspection.
            if let Some(pc) = self.gpu.bp_hit.take() {
                return StopReason::GpuBreakpoint(pc);
            }
            if let Some(pc) = self.dsp.bp_hit.take() {
                return StopReason::DspBreakpoint(pc);
            }
            if let Some(reason) = self.dbg.take_stop() {
                return reason;
            }
            if self.dbg.stop_on_illegal && self.cpu.illegal_count != illegal_seen {
                return StopReason::Illegal {
                    pc: self.cpu.last_illegal.unwrap_or(0),
                    op: 0,
                };
            }
            illegal_seen = self.cpu.illegal_count;
        }
        StopReason::ReachedFrame(self.sched.frame)
    }

    /// Advance `n` whole frames from the current position.
    pub fn run_frames(&mut self, n: u64) -> StopReason {
        let target = self.sched.frame + n;
        self.run_to_frame(target)
    }

    /// The current absolute frame number.
    pub fn frame(&self) -> u64 {
        self.sched.frame
    }

    /// Single-step one RISC core (GPU if `is_dsp` false, else DSP) `n` times from
    /// its current state, returning the PC executed at each step. Breakpoints on
    /// that core are suspended for the trace (so it steps past the one it stopped
    /// on). The 68k/other core do not advance — this traces the core's own
    /// instruction flow (e.g. an interrupt handler) in isolation. Used by the
    /// debugger's post-breakpoint trace.
    pub fn trace_risc(&mut self, is_dsp: bool, n: usize) -> Vec<u32> {
        let mut out = Vec::with_capacity(n);
        let saved = if is_dsp {
            std::mem::take(&mut self.dsp.breakpoints)
        } else {
            std::mem::take(&mut self.gpu.breakpoints)
        };
        for _ in 0..n {
            if is_dsp {
                out.push(self.dsp.pc);
                self.dsp.run(&mut self.bus, 1);
            } else {
                out.push(self.gpu.pc);
                self.gpu.run(&mut self.bus, 1);
            }
        }
        if is_dsp {
            self.dsp.breakpoints = saved;
        } else {
            self.gpu.breakpoints = saved;
        }
        out
    }

    /// Return the **true Object-Processor scan-out** for the current frame — the
    /// actual displayed image, not the DRAM the 68000 wrote. The OP composites
    /// this one line at a time as the scheduler crosses each scanline (see
    /// `tom::op_render_line`), so this is just the accumulated framebuffer. This
    /// is the screenshot primitive that BigPEmu's headless path gets wrong.
    pub fn capture_frame(&self) -> Framebuffer {
        self.bus.tom.presented.clone()
    }

    /// Inject controller state for port `port` (0 or 1). See `jerry` for the
    /// button bit layout.
    pub fn set_pad(&mut self, port: usize, buttons: u32) {
        if port < self.bus.jerry.pads.len() {
            self.bus.jerry.pads[port] = buttons;
        }
    }

    /// Start capturing audio (the I2S/DAC output sampled at the I2S rate). Off
    /// by default — capture costs memory, so enable it only when pulling sound.
    pub fn enable_audio_capture(&mut self) {
        self.bus.audio_capture = true;
        self.bus.audio.clear();
    }

    /// Drain captured stereo audio. Returns `(sample_rate_hz, interleaved L,R)`.
    pub fn take_audio(&mut self) -> (u32, Vec<i16>) {
        (self.bus.audio_rate, std::mem::take(&mut self.bus.audio))
    }
}
