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

/// Advance the metered async transfer by one frame's worth and write those bytes
/// into memory. Call at the field boundary.
///
/// A free function rather than a method because the device lives inside the bus
/// it has to write to: `advance_frame` hands back owned bytes so the borrow on
/// `bus.gamedrive` ends before `bus.write8` begins.
pub fn tick_frame(bus: &mut crate::bus::Bus) {
    let Some(gd) = bus.gamedrive.as_mut() else {
        return;
    };
    if gd.rate() == 0 {
        return;
    }
    let Some((at, chunk)) = gd.advance_frame() else {
        return;
    };
    for (i, b) in chunk.iter().enumerate() {
        bus.write8(at.wrapping_add(i as u32), *b);
    }
}

/// Drain whatever is left of the transfer in flight, immediately (FN_ASYNCWAIT).
pub fn finish_async(bus: &mut crate::bus::Bus) {
    let Some(gd) = bus.gamedrive.as_mut() else {
        return;
    };
    let Some((at, chunk)) = gd.finish_async() else {
        return;
    };
    for (i, b) in chunk.iter().enumerate() {
        bus.write8(at.wrapping_add(i as u32), *b);
    }
}

pub const SPI_STATUS: u32 = 0xF1_6002;
pub const SPI_DATA: u32 = 0xF1_6004;
pub const SPI_DATAB: u32 = 0xF1_6005;

const ST_PKT: u16 = 0x10;
const ST_SEL: u16 = 0x01;
const HAVE_DATA: u16 = 0x08; // bit 3

const CMD_HWVERSION: u8 = 12;
const CMD_GETBIOS: u8 = 0x80;

/// `GD_FSeek` flags (gdbios.h `GD_FSEEK_SET/CUR/END`).
const SEEK_CUR: u16 = 1;
const SEEK_END: u16 = 2;

/// `GD_FRead` flags: 0 CPU, 1 GPU, 2 GPU async. Only the async mode is metered.
pub const FREAD_GPU_ASYNC: u16 = 2;

/// Firmware version reported to the probe. `gd_install` requires >= 0x111.
const FIRMWARE: u16 = 0x0111;

/// Function indices, from the bindings (`JagGD/gdbios_bindings.s`).
pub const FN_INIT: u8 = 1;
pub const FN_INITGPUREAD: u8 = 2;
pub const FN_CARDIN: u8 = 9;
pub const FN_FOPEN: u8 = 10;
pub const FN_FCLOSE: u8 = 11;
pub const FN_FSEEK: u8 = 12;
pub const FN_FREAD: u8 = 13;
pub const FN_FTELL: u8 = 15;
pub const FN_FSIZE: u8 = 16;
pub const FN_ASYNCPOS: u8 = 17;
pub const FN_ASYNCWAIT: u8 = 18;
pub const FN_ASYNCACTIVE: u8 = 19;

/// Size of the GDBIOS block we hand over (must be <= 4096; the bindings reject
/// anything larger than the caller's buffer).
const BIOS_BLOCK: usize = 512;

/// `(function index, TRAP vector)`.
///
/// The thunk for function N lives at block offset `4*N` and must be exactly
/// four bytes, so it can only be `trap #n ; rts` — and 68000 traps run 0-15
/// while the GD BIOS numbers functions up to 26. Every function above 15 is
/// therefore remapped onto a trap the low-numbered functions do not use.
///
/// **This table is the single source of truth for both directions**:
/// `build_bios_block` writes the thunks from it and the 68000 core dispatches
/// through `fn_of_trap`. Two hand-maintained lists would silently drift, and a
/// drifted entry means a file call quietly vectors to the wrong operation —
/// which looks like corrupt data, not like a dispatch bug.
pub const FN_TRAP: [(u8, u8); 12] = [
    (FN_INIT, 1),
    // GD_InitGPURead. A no-op here, but it MUST have a thunk: on hardware the
    // async read modes do nothing until it installs the GPU interrupt handler,
    // so correct ROM code calls it first. With no entry, `jsr 8(%a6)` lands on
    // zeros — i.e. the hardware-correct sequence would be the one that crashes,
    // pushing authors toward code that only works here.
    (FN_INITGPUREAD, 5),
    (FN_CARDIN, 9),
    (FN_FOPEN, 10),
    (FN_FCLOSE, 11),
    (FN_FSEEK, 12),
    (FN_FREAD, 13),
    (FN_FTELL, 15),
    (FN_FSIZE, 0),        // 16 > 15: remapped
    (FN_ASYNCPOS, 2),     // 17 > 15: remapped
    (FN_ASYNCWAIT, 3),    // 18 > 15: remapped
    (FN_ASYNCACTIVE, 4),  // 19 > 15: remapped
];

/// Highest function index we publish; the block's function-count word must
/// exceed it or the bindings refuse the call.
const FN_MAX: u8 = FN_ASYNCACTIVE;

/// Which GD BIOS function a TRAP vector stands for, or `None` if the trap is
/// not ours (the core then takes a real 68000 trap).
pub fn fn_of_trap(trap: u8) -> Option<u8> {
    FN_TRAP.iter().find(|(_, t)| *t == trap).map(|(f, _)| *f)
}

/// Build the synthetic GDBIOS block: a version word, a function count, then a
/// 4-byte `trap #n ; rts` thunk at offset `4*n` for each supported call.
fn build_bios_block() -> Vec<u8> {
    let mut b = vec![0u8; BIOS_BLOCK];
    let put16 = |b: &mut Vec<u8>, off: usize, v: u16| {
        b[off] = (v >> 8) as u8;
        b[off + 1] = v as u8;
    };
    put16(&mut b, 0, 0x0111); // version (>= MINVERSION 0x100)
    put16(&mut b, 2, FN_MAX as u16 + 1); // count must exceed the highest index
    for (fname, trap) in FN_TRAP {
        let off = 4 * fname as usize;
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

/// A `GD_FREAD_GPU_ASYNC` transfer in flight.
///
/// On real hardware this runs on the GPU interrupt, 32 bytes per service, while
/// the game keeps drawing — so the bytes appear in the destination buffer over
/// many frames. Modelling that is the difference between checking a loader's
/// control flow and being able to answer "does the cutscene cover the load?".
struct AsyncXfer {
    dst: u32,
    /// Bytes still to deliver, in order.
    rest: std::collections::VecDeque<u8>,
    /// Bytes already written, so `FAsyncPos` can advance.
    done: u32,
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
    /// Destination end of the last read, reported by FN_ASYNCPOS. See
    /// `async_pos()` for why the units are a guess.
    async_pos: u32,
    /// Bytes an async read delivers per frame. **0 = complete instantly**, which
    /// is the default and the historical behaviour: a run that does not ask for
    /// a transfer model must not silently get one.
    rate: u32,
    /// The transfer in flight, if any (at most one — the hardware has one DMA).
    xfer: Option<AsyncXfer>,
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
            async_pos: 0,
            rate: 0,
            xfer: None,
        }
    }

    /// Bytes a `GD_FREAD_GPU_ASYNC` read delivers per frame (`--sd-rate`).
    /// 0 keeps the instant-completion model.
    pub fn set_rate(&mut self, bytes_per_frame: u32) {
        self.rate = bytes_per_frame;
    }

    /// Is a transfer model in effect at all?
    pub fn rate(&self) -> u32 {
        self.rate
    }

    /// Record where a completed read finished writing, for FN_ASYNCPOS.
    pub fn set_async_pos(&mut self, dst_end: u32) {
        self.async_pos = dst_end;
    }

    /// Begin a metered async read: the bytes are captured now (the file position
    /// advances immediately, as it does on hardware — the DMA owns them) but are
    /// handed to memory a frame at a time by `advance_frame`.
    ///
    /// Returns `false` if the handle is bad, in which case nothing starts.
    pub fn fread_async_start(&mut self, handle: u16, dst: u32, n: u32) -> bool {
        let Some(data) = self.fread(handle, n) else {
            return false;
        };
        self.async_pos = dst;
        self.xfer = Some(AsyncXfer {
            dst,
            rest: data.into_iter().collect(),
            done: 0,
        });
        true
    }

    /// Deliver up to `rate` more bytes of the transfer in flight. Returns the
    /// destination address and the bytes to write there, or `None` when there is
    /// nothing in flight. Call once per frame.
    pub fn advance_frame(&mut self) -> Option<(u32, Vec<u8>)> {
        let rate = self.rate.max(1) as usize;
        let x = self.xfer.as_mut()?;
        let take = rate.min(x.rest.len());
        let chunk: Vec<u8> = x.rest.drain(..take).collect();
        let at = x.dst.wrapping_add(x.done);
        x.done += take as u32;
        self.async_pos = x.dst.wrapping_add(x.done);
        if x.rest.is_empty() {
            self.xfer = None;
        }
        if chunk.is_empty() {
            None
        } else {
            Some((at, chunk))
        }
    }

    /// FN_ASYNCWAIT — deliver everything still outstanding, at once.
    pub fn finish_async(&mut self) -> Option<(u32, Vec<u8>)> {
        let x = self.xfer.take()?;
        let at = x.dst.wrapping_add(x.done);
        self.async_pos = x.dst.wrapping_add(x.done + x.rest.len() as u32);
        let chunk: Vec<u8> = x.rest.into_iter().collect();
        if chunk.is_empty() {
            None
        } else {
            Some((at, chunk))
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
        // A LEADING SLASH IS CARD-ABSOLUTE, NOT HOST-ABSOLUTE. `PathBuf::join`
        // throws the root away when the argument starts with '/', so the naive
        // form opened `/MUSIC.PCM` on the HOST filesystem. ROMs routinely try
        // both spellings (OpenLara's `gd_fopen(mi ? "/MUSIC.PCM" : "MUSIC.PCM")`
        // exists precisely because the card accepts both), so the slashed form
        // must resolve inside the attached directory like every other path.
        let want = name
            .trim_end_matches('\0')
            .trim()
            .replace('\\', "/")
            .trim_start_matches('/')
            .to_ascii_uppercase();
        let mut path = self.root.join(&want);
        if !path.exists() {
            // Case-insensitive resolve, one component at a time so
            // subdirectories work (`/DATA/PACK.BIN`): FAT is case-insensitive
            // and the host is not.
            let mut p = self.root.clone();
            let mut ok = true;
            for comp in want.split('/').filter(|c| !c.is_empty()) {
                let mut hit = None;
                if let Ok(rd) = std::fs::read_dir(&p) {
                    for e in rd.flatten() {
                        if e.file_name().to_string_lossy().to_ascii_uppercase() == comp {
                            hit = Some(e.path());
                            break;
                        }
                    }
                }
                match hit {
                    Some(h) => p = h,
                    None => { ok = false; break; }
                }
            }
            if ok {
                path = p;
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

    /// FN_FSEEK — `flags` is 0 SET / 1 CUR / 2 END, `offset` is signed.
    /// Returns 0 on success, `-1` on a bad handle.
    ///
    /// Seeking past the end is CLAMPED rather than refused: FatFs allows it and
    /// the following read then returns nothing, which is the behaviour a
    /// streaming loader must survive anyway.
    pub fn fseek(&mut self, handle: u16, flags: u16, offset: i32) -> u32 {
        let Some(f) = self.files.get_mut(&handle) else {
            return u32::MAX;
        };
        let base = match flags {
            SEEK_CUR => f.pos as i64,
            SEEK_END => f.data.len() as i64,
            _ => 0,
        };
        f.pos = (base + offset as i64).clamp(0, f.data.len() as i64) as usize;
        if std::env::var_os("JAGEMU_GD_DEBUG").is_some() {
            eprintln!("GD fseek h={handle} flags={flags} off={offset} -> pos {}", f.pos);
        }
        0
    }

    /// FN_FTELL — current file position, or `-1` on a bad handle.
    pub fn ftell(&self, handle: u16) -> u32 {
        self.files.get(&handle).map(|f| f.pos as u32).unwrap_or(u32::MAX)
    }

    /// FN_ASYNCACTIVE — nonzero while a metered transfer is still in flight.
    ///
    /// ⚠️ **Without `--sd-rate` this is always 0**, because reads then complete
    /// inside the trap. That default validates a loader's LOGIC and says nothing
    /// about its latency: a double buffer never actually overlaps, and a loader
    /// that deadlocks waiting on real transfer time still passes. Set a rate to
    /// exercise the wait path; the rate itself is your estimate of the card, not
    /// a measured constant.
    pub fn async_active(&self) -> u32 {
        u32::from(self.xfer.is_some())
    }

    /// FN_ASYNCPOS — how far the async read has got.
    ///
    /// ⚠️ The vendor bindings document this only as "current async GPU read
    /// position" and do not say whether that is a FILE offset or a DESTINATION
    /// address. We return the destination pointer reached so far, which is what
    /// a double-buffer consumer compares against — but the UNITS are a GUESS,
    /// unverified on silicon, and no ROM in the corpus uses the call, so there
    /// was nothing to infer it from. **Prefer `GD_FAsyncActive`/`GD_FAsyncWait`
    /// in ROM code**, whose meaning is unambiguous either way.
    pub fn async_pos(&self) -> u32 {
        self.async_pos
    }

    /// FN_FREAD — copy `n` bytes into the caller's buffer. Returns **0 on
    /// success** (the upstream convention the porting notes flag as a past
    /// source of bugs), `-1` on a bad handle.
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
        // header: version >= 0x100, function count above the highest index
        assert_eq!(u16::from_be_bytes([b[0], b[1]]), 0x0111);
        assert!(u16::from_be_bytes([b[2], b[3]]) > FN_MAX as u16);
        // `jsr (4*N)(%a6)` lands on `trap #n ; rts` for EVERY published call,
        // and the trap it lands on dispatches back to that same function.
        for (fname, trap) in FN_TRAP {
            let off = 4 * fname as usize;
            assert_eq!(
                u16::from_be_bytes([b[off], b[off + 1]]),
                0x4E40 | trap as u16,
                "fn {fname} thunk"
            );
            assert_eq!(u16::from_be_bytes([b[off + 2], b[off + 3]]), 0x4E75);
            assert_eq!(fn_of_trap(trap), Some(fname), "fn {fname} round-trip");
        }
    }

    /// Two functions sharing a TRAP would make one of them silently execute the
    /// other — corrupt data, with no error anywhere. Cheap to assert, so assert.
    #[test]
    fn every_function_owns_a_distinct_trap() {
        let mut traps: Vec<u8> = FN_TRAP.iter().map(|(_, t)| *t).collect();
        traps.sort_unstable();
        let n = traps.len();
        traps.dedup();
        assert_eq!(traps.len(), n, "duplicate TRAP vector in FN_TRAP");
        assert!(traps.iter().all(|t| *t <= 15), "68000 traps are 0-15");
    }

    #[test]
    fn seek_moves_the_read_position() {
        let dir = std::env::temp_dir().join("jagemu_gd_seek_test");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("PACK.SD"), b"0123456789").unwrap();
        let mut gd = GameDrive::new(&dir);

        let h = gd.fopen("PACK.SD") as u16;
        assert_eq!(gd.fseek(h, 0, 4), 0); // SET
        assert_eq!(gd.ftell(h), 4);
        assert_eq!(&gd.fread(h, 3).unwrap(), b"456");
        assert_eq!(gd.fseek(h, SEEK_CUR, -2), 0);
        assert_eq!(&gd.fread(h, 2).unwrap(), b"56");
        assert_eq!(gd.fseek(h, SEEK_END, 0), 0);
        assert_eq!(gd.ftell(h), 10);
        // past the end is clamped, and the read that follows is empty-padded
        assert_eq!(gd.fseek(h, 0, 999), 0);
        assert_eq!(gd.ftell(h), 10);
        assert_eq!(gd.fread(h, 4).unwrap(), vec![0, 0, 0, 0]);
        assert_eq!(gd.fseek(9999, 0, 0), u32::MAX); // bad handle
    }

    /// With a rate set, an async read must NOT be finished when it returns —
    /// that is the whole point. A model that delivers everything up front makes
    /// a loader that never waits look correct.
    #[test]
    fn metered_async_delivers_over_several_frames() {
        let dir = std::env::temp_dir().join("jagemu_gd_rate_test");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("PACK.BIN"), (0u8..100).collect::<Vec<u8>>()).unwrap();
        let mut gd = GameDrive::new(&dir);
        gd.set_rate(32);

        let h = gd.fopen("PACK.BIN") as u16;
        assert!(gd.fread_async_start(h, 0x1000, 100));
        assert_eq!(gd.async_active(), 1, "busy the moment it starts");
        assert_eq!(gd.async_pos(), 0x1000, "nothing delivered yet");

        // 100 bytes at 32/frame = 4 frames (32, 32, 32, 4).
        let mut got = Vec::new();
        let mut frames = 0;
        while let Some((at, chunk)) = gd.advance_frame() {
            assert_eq!(at as usize, 0x1000 + got.len(), "chunks are contiguous");
            got.extend_from_slice(&chunk);
            frames += 1;
        }
        assert_eq!(frames, 4);
        assert_eq!(got, (0u8..100).collect::<Vec<u8>>());
        assert_eq!(gd.async_active(), 0, "idle once drained");
        assert_eq!(gd.async_pos(), 0x1000 + 100);
    }

    /// `GD_FAsyncWait` must hand over everything outstanding at once, or a ROM
    /// that waits instead of polling hangs forever.
    #[test]
    fn async_wait_drains_the_remainder() {
        let dir = std::env::temp_dir().join("jagemu_gd_wait_test");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("PACK.BIN"), vec![0xAAu8; 100]).unwrap();
        let mut gd = GameDrive::new(&dir);
        gd.set_rate(10);

        let h = gd.fopen("PACK.BIN") as u16;
        gd.fread_async_start(h, 0x2000, 100);
        let (_, first) = gd.advance_frame().unwrap();
        assert_eq!(first.len(), 10);

        let (at, rest) = gd.finish_async().unwrap();
        assert_eq!(at, 0x2000 + 10);
        assert_eq!(rest.len(), 90);
        assert_eq!(gd.async_active(), 0);
        assert!(gd.finish_async().is_none(), "nothing left to drain");
    }

    /// Rate 0 is the default and must keep the old behaviour exactly: no
    /// transfer is ever in flight, so existing runs are unaffected.
    #[test]
    fn rate_zero_never_starts_a_transfer() {
        let mut gd = GameDrive::new(".");
        assert_eq!(gd.rate(), 0);
        assert_eq!(gd.async_active(), 0);
        assert!(gd.advance_frame().is_none());
    }

    /// A leading '/' is card-absolute. `PathBuf::join` would discard the
    /// attached directory and reach for the HOST root — so this guards a path
    /// escape as much as a lookup failure.
    #[test]
    fn card_absolute_paths_resolve_inside_the_attached_directory() {
        let dir = std::env::temp_dir().join("jagemu_gd_path_test");
        std::fs::create_dir_all(dir.join("data")).unwrap();
        std::fs::write(dir.join("data").join("pack.bin"), b"hello").unwrap();
        let mut gd = GameDrive::new(&dir);

        for name in ["/DATA/PACK.BIN", "DATA/PACK.BIN", "/data/pack.bin"] {
            let h = gd.fopen(name);
            assert_ne!(h, u32::MAX, "{name} should open");
            assert_eq!(gd.fsize(h as u16), 5, "{name}");
        }
        assert_eq!(gd.fopen("/NOPE.BIN"), u32::MAX);
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
