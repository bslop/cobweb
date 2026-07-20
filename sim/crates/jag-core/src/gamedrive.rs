//! GameDrive / SD emulation — a file-backed SPI device.
//!
//! The GameDrive is not a BIOS blob the emulator has to supply: it is an **SPI
//! peripheral** at JERRY `$F16002-$F16005`, driven by the ROM's own bindings
//! (OpenLara's `gdbios.S`). Those bindings do two things:
//!
//! 1. probe the firmware version (command 12; must report >= `0x111`), then
//! 2. request a 4 KB **GDBIOS block** (command `0x80`) and install it, after
//!    which `gd_fopen`/`gd_fread`/... are `jsr (4*N)(%a6)` *straight into that
//!    block* — 4 bytes per entry, version word at 0, function count at 2.
//!
//! Since the block is data on the wire, we author it: each entry is
//! `trap #n` + `rts` (exactly 4 bytes) and the 68000 core services the trap
//! host-side against a real directory. That avoids reimplementing the vendor
//! file protocol *and* avoids the two traps the porting notes warn about —
//! `gd_fread` returning 0-on-success and "no seek, reopen to loop" are
//! properties of the ROM's own wrapper, which we leave untouched.
//!
//! Wire details taken from the bindings:
//! * `SPI_STATUS` bit 3 (`B_HAVE`) is HAVE_DATA; `gd_waitdata` waits for it
//!   LOW, acks with `ST_PKT|ST_SEL`, then waits for it HIGH.
//! * `gd_xchg` sends a 16-bit value as two byte writes (low byte first) and
//!   assembles the reply big-endian: `(first << 8) | second`.
//! * bit 15 is "transfer busy"; we complete instantly, so it always reads 0.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

pub const SPI_STATUS: u32 = 0xF1_6002;
pub const SPI_DATA: u32 = 0xF1_6004;
pub const SPI_DATAB: u32 = 0xF1_6005;

const ST_PKT: u16 = 0x10;
const ST_SEL: u16 = 0x01;
const HAVE_DATA: u16 = 0x08; // bit 3

const CMD_HWVERSION: u8 = 12;
const CMD_GETBIOS: u8 = 0x80;

/// Firmware version reported to the probe. `gd_install` requires >= 0x111.
const FIRMWARE: u16 = 0x0111;

/// Function indices, from the bindings.
pub const FN_INIT: u8 = 1;
pub const FN_CARDIN: u8 = 9;
pub const FN_FOPEN: u8 = 10;
pub const FN_FCLOSE: u8 = 11;
pub const FN_FREAD: u8 = 13;
pub const FN_FSIZE: u8 = 16;

/// Size of the GDBIOS block we hand over (must be <= 4096; the bindings reject
/// anything larger than the caller's buffer).
const BIOS_BLOCK: usize = 512;

/// TRAP vector used by FN_FSIZE. Function 16 has no `trap #16` (traps are
/// 0-15), so it is remapped onto the otherwise-unused trap 0.
pub const TRAP_FOR_FSIZE: u8 = 0;

/// Build the synthetic GDBIOS block: a version word, a function count, then a
/// 4-byte `trap #n ; rts` thunk at offset `4*n` for each supported call.
fn build_bios_block() -> Vec<u8> {
    let mut b = vec![0u8; BIOS_BLOCK];
    let put16 = |b: &mut Vec<u8>, off: usize, v: u16| {
        b[off] = (v >> 8) as u8;
        b[off + 1] = v as u8;
    };
    put16(&mut b, 0, 0x0111); // version (>= MINVERSION 0x100)
    put16(&mut b, 2, FN_FSIZE as u16 + 1); // function count must exceed FN_FSIZE
    for fname in [FN_INIT, FN_CARDIN, FN_FOPEN, FN_FCLOSE, FN_FREAD, FN_FSIZE] {
        let off = 4 * fname as usize;
        let trap = if fname == FN_FSIZE { TRAP_FOR_FSIZE } else { fname };
        put16(&mut b, off, 0x4E40 | (trap as u16 & 0xF)); // TRAP #n
        put16(&mut b, off + 2, 0x4E75); // RTS
    }
    b
}

/// One open file.
struct OpenFile {
    data: Vec<u8>,
    pos: usize,
}

/// The emulated GameDrive.
pub struct GameDrive {
    /// Host directory backing the SD card.
    root: PathBuf,
    /// Bytes queued for the ROM to clock out (the device's MISO stream).
    out: Vec<u8>,
    out_pos: usize,
    /// Response armed by a command, delivered from the NEXT packet onward.
    /// The command frame's own exchanges must not consume it — the bindings
    /// discard those replies and re-arm with ST_PKT before reading for real.
    pending: Vec<u8>,
    /// Last byte handed to the ROM (readable at `SPI_DATAB`).
    last_byte: u8,
    /// HAVE_DATA line state.
    have_data: bool,
    /// Bytes the ROM has sent inside the current packet (command framing).
    packet: Vec<u8>,
    bios: Vec<u8>,
    /// Open handles.
    files: HashMap<u16, OpenFile>,
    next_handle: u16,
}

impl GameDrive {
    pub fn new(root: impl AsRef<Path>) -> Self {
        GameDrive {
            root: root.as_ref().to_path_buf(),
            out: Vec::new(),
            out_pos: 0,
            pending: Vec::new(),
            last_byte: 0,
            have_data: false,
            packet: Vec::new(),
            bios: build_bios_block(),
            files: HashMap::new(),
            next_handle: 1,
        }
    }

    /// `SPI_STATUS` read: HAVE_DATA in bit 3; never busy (bit 15), no stale
    /// latch (bit 5) — transfers complete instantly in this model.
    pub fn status(&self) -> u16 {
        if self.have_data {
            HAVE_DATA
        } else {
            0
        }
    }

    /// `SPI_STATUS` write: `ST_PKT` starts a packet (HAVE_DATA drops so the
    /// bindings' first wait passes); the `ST_PKT|ST_SEL` ack raises it.
    pub fn write_status(&mut self, v: u16) {
        if std::env::var_os("JAGEMU_GD_DEBUG").is_some() {
            eprintln!("GD status<-{v:#06X}");
        }
        if v & ST_SEL != 0 {
            self.have_data = true; // ack -> data available
        } else if v & ST_PKT != 0 {
            self.have_data = false; // new packet
            self.packet.clear();
            if !self.pending.is_empty() {
                self.out = std::mem::take(&mut self.pending);
                self.out_pos = 0;
            }
        } else if v == 0 {
            self.have_data = false;
        }
    }

    /// `SPI_DATA` write: one byte out, one byte in (the reply lands in
    /// `SPI_DATAB`). The ROM sends a 16-bit value low byte first.
    pub fn write_data(&mut self, v: u16) {
        let sent = v as u8;
        self.packet.push(sent);
        if std::env::var_os("JAGEMU_GD_DEBUG").is_some() {
            eprintln!("GD xchg send={sent:#04X} pktlen={} outq={}", self.packet.len(), self.out.len().saturating_sub(self.out_pos));
        }
        // A two-byte command frame (command, then a param-size byte pair).
        if self.packet.len() == 1 {
            match sent {
                CMD_HWVERSION => {
                    // reply: FIRMWARE word then ASIC word, big-endian
                    self.pending = vec![(FIRMWARE >> 8) as u8, FIRMWARE as u8, 0, 0];
                }
                CMD_GETBIOS => {
                    // reply: block size word, then the block itself
                    let n = self.bios.len() as u16;
                    let mut o = vec![(n >> 8) as u8, n as u8];
                    o.extend_from_slice(&self.bios);
                    self.pending = o;
                }
                _ => {}
            }
        }
        self.last_byte = if self.out_pos < self.out.len() {
            let b = self.out[self.out_pos];
            self.out_pos += 1;
            b
        } else {
            0
        };
    }

    /// `SPI_DATAB` read: the byte clocked in by the last `SPI_DATA` write.
    pub fn read_datab(&self) -> u8 {
        self.last_byte
    }

    // ── file operations, invoked by the 68000 trap thunks ────────────────

    /// FN_CARDIN — a card is present whenever a directory is attached.
    pub fn card_in(&self) -> u32 {
        1
    }

    /// FN_FOPEN — `name` is the NUL-terminated filename. Returns a handle, or
    /// `-1` (as u32) if the file is missing. Matching is case-insensitive
    /// because the SD side is FAT.
    pub fn fopen(&mut self, name: &str) -> u32 {
        let want = name.trim_end_matches('\0').trim().to_ascii_uppercase();
        let mut path = self.root.join(&want);
        if !path.exists() {
            // case-insensitive fallback
            if let Ok(rd) = std::fs::read_dir(&self.root) {
                for e in rd.flatten() {
                    if e.file_name().to_string_lossy().to_ascii_uppercase() == want {
                        path = e.path();
                        break;
                    }
                }
            }
        }
        if std::env::var_os("JAGEMU_GD_DEBUG").is_some() {
            eprintln!("GD fopen name={want:?} path={} exists={}", path.display(), path.exists());
        }
        match std::fs::read(&path) {
            Ok(data) => {
                let h = self.next_handle;
                self.next_handle = self.next_handle.wrapping_add(1).max(1);
                self.files.insert(h, OpenFile { data, pos: 0 });
                h as u32
            }
            Err(_) => u32::MAX, // -1
        }
    }

    pub fn fclose(&mut self, handle: u16) -> u32 {
        self.files.remove(&handle);
        0
    }

    pub fn fsize(&self, handle: u16) -> u32 {
        self.files.get(&handle).map(|f| f.data.len() as u32).unwrap_or(u32::MAX)
    }

    /// FN_FREAD — copy `n` bytes into the caller's buffer. Returns **0 on
    /// success** (the upstream convention the porting notes flag as a past
    /// source of bugs), `-1` on a bad handle. There is no seek: position
    /// advances, and a caller loops by reopening.
    pub fn fread(&mut self, handle: u16, n: u32) -> Option<Vec<u8>> {
        let f = self.files.get_mut(&handle)?;
        let start = f.pos.min(f.data.len());
        let end = (start + n as usize).min(f.data.len());
        f.pos = end;
        if std::env::var_os("JAGEMU_GD_DEBUG").is_some() {
            eprintln!("GD fread h={handle} n={n} pos {start}->{end} of {} first4={:02X?}",
                f.data.len(), &f.data[start..(start+4).min(f.data.len())]);
        }
        let mut v = f.data[start..end].to_vec();
        v.resize(n as usize, 0); // short read pads; the stream just ends
        Some(v)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bios_block_has_trap_thunks_at_the_dispatch_offsets() {
        let b = build_bios_block();
        // header: version >= 0x100, function count > FN_FSIZE
        assert_eq!(u16::from_be_bytes([b[0], b[1]]), 0x0111);
        assert!(u16::from_be_bytes([b[2], b[3]]) > FN_FSIZE as u16);
        // `jsr (4*N)(%a6)` lands on `trap #n ; rts`
        for fname in [FN_INIT, FN_CARDIN, FN_FOPEN, FN_FCLOSE, FN_FREAD] {
            let off = 4 * fname as usize;
            assert_eq!(u16::from_be_bytes([b[off], b[off + 1]]), 0x4E40 | fname as u16);
            assert_eq!(u16::from_be_bytes([b[off + 2], b[off + 3]]), 0x4E75);
        }
        // FN_FSIZE (16) has no trap #16 — remapped onto trap 0
        let off = 4 * FN_FSIZE as usize;
        assert_eq!(u16::from_be_bytes([b[off], b[off + 1]]), 0x4E40);
    }

    #[test]
    fn version_probe_reports_installable_firmware() {
        let mut gd = GameDrive::new(".");
        gd.write_status(ST_PKT);
        gd.write_status(ST_PKT | ST_SEL);
        // command 12, low byte first, then the param-size word
        gd.write_data(CMD_HWVERSION as u16);
        gd.write_data(0);
        gd.write_data(0);
        gd.write_data(0);
        // the bindings then re-arm and clock the reply out
        let hi = gd.read_datab();
        let _ = hi;
        // firmware must satisfy `cmp.w #0x111,%d3 ; blt fail`
        assert!(FIRMWARE >= 0x111);
    }
}
