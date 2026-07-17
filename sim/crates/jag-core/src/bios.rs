//! High-level emulation (HLE) of the Jaguar boot ROM.
//!
//! Real hardware runs a boot ROM at `$E00000` that, before jumping to the game,
//! (1) configures the memory controller, (2) validates the cartridge
//! "encryption" header, (3) installs a default level-2 interrupt dispatcher plus
//! a full exception-vector table in low RAM, and (4) reads the cart entry from
//! the longword at `$800404` and jumps there. We have no boot-ROM image, so this
//! module replicates the boot ROM's *observable* post-boot state directly.
//!
//! The piece that actually unblocks commercial carts is the **default interrupt
//! dispatcher at vector 64 (`$100`)**. A game that enables the video interrupt
//! *before* installing its own handler (Alien vs Predator, Rayman, …) takes a
//! level-2 interrupt that vectors through `$100`; with no BIOS that longword is
//! zero, so the 68k jumps to `$0` and runs low RAM as garbage code — exactly the
//! "wild jump → cascade of illegal opcodes" we observe. Installing a real
//! handler there (ack the video interrupt, restore bus priority, `RTE`) makes
//! the boot continue until the game installs its own vectors.
//!
//! Handler code lives in a synthesized boot-ROM image mapped at `$E00000`; the
//! vectors in low DRAM point at it. A game that rewrites the vectors or the
//! MEMCON/endian registers simply overwrites these defaults — exactly as on
//! hardware. See `docs/spec/M68K_JAGUAR.md` §3.3 (B) and §4.

use crate::bus::Bus;
use crate::mem;

/// Boot-ROM offset of the single `RTE` exception stub (short 6-byte frame).
const OFF_RTE: u32 = 0x80;
/// Boot-ROM offset of the group-0 (bus/address error) cleanup stub.
const OFF_GROUP0: u32 = 0xA0;
/// Boot-ROM offset of the default level-2 interrupt dispatcher.
const OFF_IRQ: u32 = 0x100;
/// Boot-ROM offset of the catch-all halt loop (`BRA.S *`).
const OFF_HALT: u32 = 0x08;

/// Absolute address of the default interrupt dispatcher (vector 64 / `$100`).
pub const HLE_IRQ_HANDLER: u32 = mem::BOOTROM_START + OFF_IRQ;
/// Absolute address of the generic `RTE` exception stub.
pub const HLE_RTE_STUB: u32 = mem::BOOTROM_START + OFF_RTE;
/// Absolute address of the group-0 (bus/address error) cleanup stub.
pub const HLE_GROUP0_STUB: u32 = mem::BOOTROM_START + OFF_GROUP0;

#[inline]
fn put16(rom: &mut [u8], off: u32, w: u16) {
    let o = off as usize;
    rom[o] = (w >> 8) as u8;
    rom[o + 1] = w as u8;
}

#[inline]
fn put32(rom: &mut [u8], off: u32, l: u32) {
    rom[off as usize..off as usize + 4].copy_from_slice(&l.to_be_bytes());
}

/// Synthesize the HLE boot-ROM image (mapped at `$E00000`). It holds the handler
/// routines the low-RAM vectors point at, plus a reset-vector mirror in its
/// first 8 bytes (so a 68k that ever resets through the ROM still lands safely).
pub fn build_bootrom() -> Vec<u8> {
    let mut rom = vec![0xFFu8; 0x200];

    // Reset-vector mirror: SSP = top of DRAM, PC = the halt loop.
    put32(&mut rom, 0x00, mem::DRAM_END);
    put32(&mut rom, 0x04, mem::BOOTROM_START + OFF_HALT);

    // $08: catch-all halt loop — `BRA.S *` (branch to self).
    put16(&mut rom, OFF_HALT, 0x60FE);

    // $80: generic short-frame exception stub — `RTE`.
    put16(&mut rom, OFF_RTE, 0x4E73);

    // $A0: group-0 (bus/address error) cleanup. The 68000 pushes a 7-word frame
    // here; SR/PC sit 8 bytes in. Discard the 4 extra words, then `RTE`.
    //   LEA  8(sp),sp      ; 4FEF 0008
    //   RTE                ; 4E73
    put16(&mut rom, OFF_GROUP0, 0x4FEF);
    put16(&mut rom, OFF_GROUP0 + 2, 0x0008);
    put16(&mut rom, OFF_GROUP0 + 4, 0x4E73);

    // $100: default level-2 interrupt dispatcher. The Jaguar funnels every IRQ
    // source through one level-2 line; this acks the video time-base interrupt
    // (the only one a game enables before installing its own ISR) and returns.
    //   MOVE.W #$0101,$00F000E0  ; INT1: clear video pending (bit 8) + keep
    //                            ;       video enabled (bit 0)
    //   MOVE.W #$0000,$00F000E2  ; INT2: restore GPU/Blitter bus priority
    //   RTE
    let irq: [u16; 9] = [
        0x33FC, 0x0101, 0x00F0, 0x00E0, // move.w #$0101,$00F000E0
        0x33FC, 0x0000, 0x00F0, 0x00E2, // move.w #$0000,$00F000E2
        0x4E73, // rte
    ];
    for (i, w) in irq.iter().enumerate() {
        put16(&mut rom, OFF_IRQ + 2 * i as u32, *w);
    }

    rom
}

/// Seed the post-boot machine state a commercial cart expects: a full exception
/// vector table in low DRAM (vectors point at the synthesized boot ROM), the
/// default interrupt dispatcher at vector 64, big-endian GPU/DSP, and an idle
/// NTSC joypad. `entry` is the cart entry (from `[$800404]`); `ssp` is the
/// initial supervisor stack. Call after the program image is loaded.
pub fn install(bus: &mut Bus, entry: u32, ssp: u32) {
    bus.bootrom = build_bootrom();

    // Vectors 0/1: reset SSP and PC.
    bus.write32(0x0, ssp);
    bus.write32(0x4, entry);

    // Vectors 2..255 ($008..$3FF): safe defaults the BIOS would have installed.
    //  * 2,3  (bus / address error) → group-0 cleanup stub
    //  * 64   (Jaguar level-2 IRQ)  → default dispatcher
    //  * rest                        → short-frame RTE stub
    for vec in 2u32..256 {
        let handler = match vec {
            2 | 3 => HLE_GROUP0_STUB,
            64 => HLE_IRQ_HANDLER,
            _ => HLE_RTE_STUB,
        };
        bus.write32(vec * 4, handler);
    }

    // Post-BIOS hardware state. The console boots the RISC engines big-endian
    // (both halves) — games normally re-assert this, but some commercial carts
    // assume the BIOS already did. Joypad idle + NTSC (bit 4 of JOYBUTS).
    bus.write32(mem::G_END, 0x0007_0007);
    bus.write32(mem::D_END, 0x0007_0007);
    bus.tom.win.w16(mem::OBF, 0);
    bus.jerry.win.w16(mem::JOYSTICK, 0xFFFF);
    bus.jerry.win.w16(mem::JOYBUTS, 0xFFFF);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bootrom_has_handlers_at_expected_offsets() {
        let rom = build_bootrom();
        // IRQ dispatcher starts with `MOVE.W #$0101,$00F000E0`.
        assert_eq!(rom[OFF_IRQ as usize], 0x33);
        assert_eq!(rom[OFF_IRQ as usize + 1], 0xFC);
        assert_eq!(rom[OFF_IRQ as usize + 2], 0x01);
        assert_eq!(rom[OFF_IRQ as usize + 3], 0x01);
        // RTE stub.
        assert_eq!(rom[OFF_RTE as usize], 0x4E);
        assert_eq!(rom[OFF_RTE as usize + 1], 0x73);
    }

    #[test]
    fn install_seeds_vector_table() {
        let mut bus = Bus::new();
        install(&mut bus, 0x80_2000, mem::DRAM_END);
        assert_eq!(bus.read32(0x0), mem::DRAM_END); // SSP
        assert_eq!(bus.read32(0x4), 0x80_2000); // entry
        assert_eq!(bus.read32(64 * 4), HLE_IRQ_HANDLER); // $100 → dispatcher
        assert_eq!(bus.read32(4 * 4), HLE_RTE_STUB); // illegal → RTE stub
        assert_eq!(bus.read32(3 * 4), HLE_GROUP0_STUB); // addr error → cleanup
        // The dispatcher is fetchable through the boot-ROM window.
        assert_eq!(bus.read16(HLE_IRQ_HANDLER), 0x33FC);
        assert_eq!(bus.read16(HLE_IRQ_HANDLER + 16), 0x4E73); // trailing RTE
    }

    /// End-to-end: enabling the video IRQ and taking it must land in the BIOS
    /// dispatcher and return cleanly, not wild-jump to $0.
    #[test]
    fn taken_irq_lands_in_dispatcher_and_returns() {
        use crate::m68k::M68k;
        let mut bus = Bus::new();
        install(&mut bus, 0x4000, mem::DRAM_END);
        let mut cpu = M68k::new();
        cpu.reset(&mut bus);
        cpu.sr = 0x2000; // supervisor, IRQ mask 0 (interrupts allowed)
        cpu.set_pc(0x4000);
        // Simulate the scheduler raising a pending video interrupt.
        bus.tom.int1_pending |= mem::C_VIDENA;
        cpu.request_interrupt(2);
        let mut dbg = crate::debug::Debugger::new();
        // First step takes the interrupt → PC enters the dispatcher.
        cpu.step(&mut bus, &mut dbg);
        assert_eq!(cpu.pc, HLE_IRQ_HANDLER, "IRQ did not vector to the dispatcher");
        // Run the handler to completion (3 instructions: 2 moves + RTE).
        for _ in 0..3 {
            cpu.step(&mut bus, &mut dbg);
        }
        assert_eq!(cpu.pc, 0x4000, "RTE did not return to interrupted PC");
        assert_eq!(bus.tom.int1_pending & mem::C_VIDENA, 0, "video IRQ not acked");
    }
}
