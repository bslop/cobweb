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
        bus.write8_dma(at.wrapping_add(i as u32), *b);
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
        bus.write8_dma(at.wrapping_add(i as u32), *b);
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
/// Cartridge-SDRAM control (`JagGD/gdbios_bindings.s`):
/// `GD_ROMWriteEnable .equ 4`, `GD_ROMSetPage .equ 5`, `GD_ROMSetPages .equ 6`.
pub const FN_ROMWEN: u8 = 4;
pub const FN_ROMPAGE: u8 = 5;
pub const FN_ROMPAGES: u8 = 6;
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
pub const FN_TRAP: [(u8, u8); 15] = [
    (FN_INIT, 1),
    // GD_InitGPURead. A no-op here, but it MUST have a thunk: on hardware the
    // async read modes do nothing until it installs the GPU interrupt handler,
    // so correct ROM code calls it first. With no entry, `jsr 8(%a6)` lands on
    // zeros — i.e. the hardware-correct sequence would be the one that crashes,
    // pushing authors toward code that only works here.
    (FN_INITGPUREAD, 5),
    // The cartridge SDRAM's three control calls. Traps 6/7/8 are the ones no
    // file call uses; every function still owns a distinct vector (asserted).
    (FN_ROMWEN, 6),
    (FN_ROMPAGE, 7),
    (FN_ROMPAGES, 8),
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

/// A GPU-mode `GD_FRead` (GPU or GPU async) moves its data in the GPU's
/// interrupt handler. Here it completes without one; on silicon a read issued
/// while the GPU is halted never completes, and the GameDrive is left
/// mid-transfer until a power cycle (vendor `JagGD/README.md`; notes "the
/// GameDrive GPU-async read", item 4). Warn once when the GPU isn't running.
/// Hosts that work on silicon: platform `lib/gd` README, "GPU-mode reads"
/// (bench jobs 190-200, platform issue 0012). Returns whether the GPU runs.
pub fn check_gpu_read_host(bus: &crate::bus::Bus) -> bool {
    let mut ctrl = [0u8; 4];
    bus.peek(crate::mem::G_CTRL, &mut ctrl);
    if u32::from_be_bytes(ctrl) & crate::mem::RISCGO != 0 {
        return true;
    }
    static WARNED: std::sync::Once = std::sync::Once::new();
    WARNED.call_once(|| {
        eprintln!("jsim WARNING: a GPU-mode gd_fread was issued with the GPU halted. \
                   It completes here, but on silicon the GPU's interrupt handler \
                   moves the data: the read never finishes and the GameDrive needs \
                   a power cycle. Start a GPU host first (GD_InitGPURead, G_DSPENA, \
                   J_EXTENA, the GPU running).");
    });
    false
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
            Err(_) => {
                // ☠☠ ON REAL HARDWARE THIS SUCCEEDS. Measured by `jag_quake`,
                // 2026-08-19: `gd_fopen` with READ | OPEN_EXISTING returns a
                // VALID HANDLE for a file that is not on the card, and the
                // following `gd_fread` then reports success while delivering
                // nothing. The failure presents as "the read worked and my data
                // is garbage", never as "file not found".
                //
                // This model returns -1, which is friendlier and catches typos —
                // but it means a missing asset is a clean error here and a silent
                // corruption on the cart. That difference cost `jag_quake` a
                // diagnosis: jsim said BLOB_ERR_OPEN while the console loaded
                // nothing and rendered black.
                //
                // The return is left as -1 (peers rely on it, and matching the
                // hardware would hide real typos); the warning carries the fact.
                // ⭐ The lesson for ROM authors: validate CONTENT — a magic word
                // — never the return code.
                static WARNED: std::sync::Once = std::sync::Once::new();
                WARNED.call_once(|| {
                    eprintln!("jsim WARNING: gd_fopen({want:?}) missing -> -1 here, \
                               but REAL HARDWARE RETURNS A VALID HANDLE and the \
                               read then silently delivers nothing (jag_quake, \
                               2026-08-19). Validate a magic word, not the \
                               return code.");
                });
                u32::MAX // -1
            }
        }
    }

    pub fn fclose(&mut self, handle: u16) -> u32 {
        self.files.remove(&handle);
        0
    }

    /// ☠☠☠ FSIZE HANGS THE 68000 ON REAL HARDWARE — measured on silicon by
    /// `jag_quake`, 2026-08-19, where it was the entire "the cart does not boot"
    /// blocker. `gd_install`, `gd_card_in` and `gd_fopen` all return; `gd_fsize`
    /// is entered and never comes back, with no error and no exception.
    ///
    /// This model returns the length, so a ROM that calls it looks perfectly
    /// healthy here and dies on the cart — exactly the divergence a simulator
    /// exists to prevent. The return is deliberately NOT changed (eight projects
    /// build against this and a hang would be a hostile default), so the model
    /// WARNS instead, once, unconditionally.
    ///
    /// ⚠ Note the bounded SPI waits in `gdbios.S` do not protect callers: those
    /// guard the binding's own probe, while `gdfunc` does `jsr (4*n)(%a6)` into
    /// RetroHQ's installed BIOS, whose spins are outside any timeout.
    /// ⭐ Get the length from your own container instead.
    pub fn fsize(&self, handle: u16) -> u32 {
        static WARNED: std::sync::Once = std::sync::Once::new();
        WARNED.call_once(|| {
            eprintln!("jsim WARNING: gd_fsize() is modelled as returning a length, \
                       but it HANGS the 68000 on real hardware (jag_quake, \
                       measured 2026-08-19). Take the size from your own file \
                       header instead — this call will pass here and hang the cart.");
        });
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


// ── THE CARTRIDGE SDRAM ──────────────────────────────────────────────────────
//
// The GameDrive carries **16 MB of SDRAM in sixteen 1 MB banks**, six of them
// mapped at a time into the cartridge window `$800000..$DFFFFF` (page p at
// `$8p0000`), and the window is READ-ONLY until `GD_ROMWriteEnable(1)`.
//
// ABI, read from `JagGD/gdbios_bindings.s` (not inferred):
// ```
// GD_ROMWriteEnable  .equ 4    void GD_ROMWriteEnable(u16 flags)   d0.w = flags
// GD_ROMSetPage      .equ 5    void ROMSetPage(u16 page, u16 bank) d0 = page<<16|bank
// GD_ROMSetPages     .equ 6    void ROMSetPages(u32 banks)         d0 = one nibble per page
//   Page 0:$8xxxxx 1:$9xxxxx 2:$axxxxx 3:$bxxxxx 4:$cxxxxx 5:$dxxxxx
//   Bank 0-15 is 1MB pages of the onboard 16MB SDRAM
//   "Lowest significant nibble is page 0, upto nibble 5. Data above is ignored."
// ```
//
// ☠ **A BYTE WRITE FILLS THE WHOLE 16-BIT WORD** — measured on silicon,
// jag_resident run 242. `make SDRAMTEST=10` (64 KB pattern written in
// LONGWORDS, verified in longwords then in bytes) reads GREEN on both passes;
// `SDRAMTEST=11`, the same pattern written in BYTES and verified in longwords,
// reads RED. Four earlier probe runs (modes 5/7/8/9) had panicked with >= 65535
// of the hero's 186 KB differing after a byte-loop `memcpy` into the window
// before modes 10/11 named the cause. The cart bus is 16 bits wide with no byte
// enables reaching the SDRAM, so the odd byte of a pair takes the even byte's
// data — a `memcpy` corrupts every other byte and `mode 4`'s byte-copied plate
// carried the smear (mean-abs-diff 1.8 against the DRAM copy's 1.1).
//
// A model that omitted this would bless a ROM that dies on the cart, which is
// the exact structural blindness this project keeps paying for. It is modelled
// here, and `Bus::write8` only applies it to a **genuine** byte store: a
// `write16`/`write32` decomposed into bytes raises `watch_suppress`, and that
// is what separates the two (same discipline as `risc_ram_narrow_writes`).

// ⚠ WHAT THIS MODEL DOES **NOT** CARRY, measured rather than guessed:
//
// * **The cart bus is SLOW and this model charges DRAM-ish cycles.** Silicon
//   (run 242, mode 1) copies 512 KB from the window to DRAM with a 68000
//   longword loop in **2.83 s = 181 KB/s** - every access is a 16-bit cart-bus
//   transaction. The same ROM under jsim inverts the screen every **60 fields
//   = 1.00 s = 512 KB/s**, i.e. **2.83x too fast**. So jsim can tell you a
//   cart-resident design is CORRECT and cannot tell you it is fast enough;
//   run 242's rule "nothing the 68000 touches per frame lives in the cart" is
//   not enforceable here. Charging the real cost would also re-time every
//   commercial cart ROM in the corpus (their code FETCHES from this window),
//   so it wants its own calibrated pass, not a constant bolted on here.
// * **Power-up contents.** Real SDRAM comes up arbitrary; this fills zero, so
//   a read-before-write reads a benign 0 here and noise on the cart - the same
//   class `JAGEMU_SRAM_POISON` exists for on Tom's and Jerry's SRAM. No poison
//   option is implemented for the cart window.
// * **Byte writes by a RISC or the Blitter.** The quirk is applied to every
//   genuine 8-bit store, on the mechanism (a 16-bit bus with no byte enables
//   reaching the SDRAM). Only the 68000's byte stores and `gd_fread`'s copy
//   were actually measured on silicon.
// * **The default page map.** Page p selects bank p until a ROM says
//   otherwise. `SDRAMTEST=1` writes through page 1 before touching the page
//   table and then requires `ROMSetPage(1,1)` to show the same data, which is
//   GREEN on silicon - so page 1's default IS bank 1. The other five are an
//   inference from that, not a measurement.

/// 1 MB per bank, sixteen banks.
pub const SDRAM_BANK_BYTES: u32 = 1024 * 1024;
pub const SDRAM_BANKS: u32 = 16;
pub const SDRAM_BYTES: u32 = SDRAM_BANKS * SDRAM_BANK_BYTES;
/// Pages of the cartridge window: `$800000..$DFFFFF` is exactly six.
pub const CART_PAGES: usize = 6;

/// The GameDrive's 16 MB of cartridge SDRAM, its six-entry page table and the
/// write-enable latch.
pub struct CartSdram {
    mem: Vec<u8>,
    /// Which bank each page of the window selects.
    page: [u8; CART_PAGES],
    /// `GD_ROMWriteEnable` — the window is read-only until a ROM sets this.
    write_enable: bool,
    /// Fault injection (`JAGEMU_CARTSDRAM_RO=1`): hold the latch off whatever
    /// the ROM asks for. This is the NEGATIVE CONTROL for the whole model — a
    /// probe that passes with the latch forced off is not measuring anything.
    force_read_only: bool,
    /// Counters, so a run can say whether this model was used at all rather
    /// than leaving "the probe passed" ambiguous between right and untouched.
    pub writes: u64,
    pub writes_refused: u64,
    pub byte_writes: u64,
    pub page_sets: u64,
}

impl Default for CartSdram {
    fn default() -> Self {
        Self::new()
    }
}

impl CartSdram {
    pub fn new() -> Self {
        CartSdram {
            // Zero fill. Real SDRAM powers up arbitrary, like Tom's and
            // Jerry's SRAM (see JAGEMU_SRAM_POISON) — a poison option for the
            // cart window is NOT implemented, so a read-before-write of the
            // window still reads a benign 0 here and noise on silicon.
            mem: vec![0u8; SDRAM_BYTES as usize],
            // Identity: page p selects bank p. This is what the window looks
            // like to a ROM that has not called ROMSetPage — jag_resident's
            // SDRAMTEST=1 writes 'JAG1' through page 1 BEFORE touching the page
            // table, switches page 1 to bank 7 and back, and requires 'JAG1' to
            // still be there; that probe reads GREEN on silicon, so page 1 maps
            // to a bank that ROMSetPage(1,1) also selects.
            page: [0, 1, 2, 3, 4, 5],
            write_enable: false,
            force_read_only: std::env::var_os("JAGEMU_CARTSDRAM_RO").is_some(),
            writes: 0,
            writes_refused: 0,
            byte_writes: 0,
            page_sets: 0,
        }
    }

    /// FN 4 `GD_ROMWriteEnable(flags)` — nonzero opens the window for writes.
    pub fn set_write_enable(&mut self, flags: u16) {
        self.write_enable = flags != 0;
        if self.force_read_only && flags != 0 {
            static WARNED: std::sync::Once = std::sync::Once::new();
            WARNED.call_once(|| {
                eprintln!(
                    "jsim: JAGEMU_CARTSDRAM_RO set - GD_ROMWriteEnable(1) is being \
                     IGNORED, every cart-window write will be dropped (fault arm)."
                );
            });
        }
    }

    pub fn write_enabled(&self) -> bool {
        self.write_enable && !self.force_read_only
    }

    /// FN 5 `GD_ROMSetPage(page, bank)`. Out-of-range arguments are ignored:
    /// the vendor documents page 0-5 and bank 0-15 and says nothing about what
    /// the firmware does with anything else, so this model refuses rather than
    /// inventing an aliasing rule a ROM could come to depend on.
    pub fn set_page(&mut self, page: u32, bank: u32) {
        if (page as usize) < CART_PAGES && bank < SDRAM_BANKS {
            self.page[page as usize] = bank as u8;
            self.page_sets += 1;
        }
    }

    /// FN 6 `GD_ROMSetPages(nibbles)` — nibble 0 is page 0, up to nibble 5;
    /// "data above is ignored".
    pub fn set_pages(&mut self, nibbles: u32) {
        for p in 0..CART_PAGES {
            self.page[p] = ((nibbles >> (4 * p)) & 0xF) as u8;
        }
        self.page_sets += 1;
    }

    /// Which bank a page currently selects (for tests and diagnostics).
    pub fn bank_of_page(&self, page: usize) -> u8 {
        self.page[page.min(CART_PAGES - 1)]
    }

    /// Map a cartridge-window address to an offset in the 16 MB array.
    /// `addr` must already be masked to 24 bits.
    #[inline]
    fn offset(&self, addr: u32) -> Option<usize> {
        if !(crate::mem::CART_START..crate::mem::CART_END).contains(&addr) {
            return None;
        }
        let off = addr - crate::mem::CART_START;
        let page = (off / SDRAM_BANK_BYTES) as usize;
        let within = off % SDRAM_BANK_BYTES;
        Some(self.page[page] as usize * SDRAM_BANK_BYTES as usize + within as usize)
    }

    #[inline]
    pub fn read8(&self, addr: u32) -> Option<u8> {
        self.offset(addr).map(|i| self.mem[i])
    }

    /// A write through the window. `narrow` is true for a REAL 8-bit store —
    /// the case that fills both halves of the 16-bit word (run 242). Returns
    /// false if the latch is closed, in which case nothing landed.
    #[inline]
    pub fn write8(&mut self, addr: u32, v: u8, narrow: bool) -> bool {
        if !self.write_enabled() {
            self.writes_refused += 1;
            return false;
        }
        let Some(i) = self.offset(addr) else {
            return false;
        };
        self.writes += 1;
        self.mem[i] = v;
        if narrow {
            // ☠ the silicon quirk: no byte enables, so the neighbour in the
            // same 16-bit word takes the same data.
            self.byte_writes += 1;
            let pair = i ^ 1;
            self.mem[pair] = v;
        }
        true
    }

    /// Host-side access for tests and tooling (bypasses the latch).
    pub fn poke_raw(&mut self, bank: u32, off: u32, v: u8) {
        let i = bank as usize * SDRAM_BANK_BYTES as usize + off as usize;
        self.mem[i] = v;
    }
    pub fn peek_raw(&self, bank: u32, off: u32) -> u8 {
        self.mem[bank as usize * SDRAM_BANK_BYTES as usize + off as usize]
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

    // ── the cartridge SDRAM (run 242) ───────────────────────────────────

    /// Attach a bus with the SDRAM present and the window already open, which
    /// is the state a ROM is in after `GD_ROMWriteEnable(1)`.
    fn sdram_bus() -> crate::bus::Bus {
        let mut bus = crate::bus::Bus::new();
        let mut s = CartSdram::new();
        s.set_write_enable(1);
        bus.cart_sdram = Some(Box::new(s));
        bus
    }

    /// Page p of `$800000..$DFFFFF` must address bank p by default and a
    /// DIFFERENT 1 MB of silicon after `GD_ROMSetPage`. This is the assertion
    /// `SDRAMTEST=1` makes on the rig (WHITE -> YELLOW -> GREEN): write through
    /// page 1, switch page 1 to bank 7, write there, switch back, and the first
    /// value must still be there.
    #[test]
    fn paging_selects_distinct_banks() {
        let mut bus = sdram_bus();
        let p1 = 0x90_0000; // page 1

        bus.write32(p1, 0x4A41_4731); // 'JAG1'
        assert_eq!(bus.read32(p1), 0x4A41_4731, "the window must read back");

        bus.cart_sdram.as_mut().unwrap().set_page(1, 7);
        // A different bank: it has never been written, so it cannot hold 'JAG1'.
        assert_ne!(bus.read32(p1), 0x4A41_4731, "paging changed nothing");
        bus.write32(p1, 0x4A41_4737); // 'JAG7'

        bus.cart_sdram.as_mut().unwrap().set_page(1, 1);
        assert_eq!(bus.read32(p1), 0x4A41_4731, "bank 1 did not survive");
        bus.cart_sdram.as_mut().unwrap().set_page(1, 7);
        assert_eq!(bus.read32(p1), 0x4A41_4737, "bank 7 did not survive");

        // and the two banks really are two places in the 16 MB array
        let s = bus.cart_sdram.as_ref().unwrap();
        assert_eq!(s.peek_raw(1, 0), 0x4A);
        assert_eq!(s.peek_raw(1, 3), 0x31);
        assert_eq!(s.peek_raw(7, 3), 0x37);
    }

    /// All six pages, and `GD_ROMSetPages`' one-nibble-per-page packing:
    /// "Lowest significant nibble is page 0, upto nibble 5. Data above is
    /// ignored." Every page must reach its own bank, or a build that maps six
    /// banks at once silently aliases two of them.
    #[test]
    fn all_six_pages_map_and_set_pages_packs_one_nibble_each() {
        let mut bus = sdram_bus();
        // banks 15,14,13,12,11,10 for pages 0..5, with junk above nibble 5
        bus.cart_sdram.as_mut().unwrap().set_pages(0xAB_CD_EF);
        let want = [0xF, 0xE, 0xD, 0xC, 0xB, 0xA];
        for (p, w) in want.iter().enumerate() {
            assert_eq!(bus.cart_sdram.as_ref().unwrap().bank_of_page(p), *w as u8);
        }
        // write a distinct word through each page, then read them all back
        for p in 0..CART_PAGES as u32 {
            bus.write32(0x80_0000 + p * SDRAM_BANK_BYTES, 0x1000_0000 + p);
        }
        for p in 0..CART_PAGES as u32 {
            assert_eq!(
                bus.read32(0x80_0000 + p * SDRAM_BANK_BYTES),
                0x1000_0000 + p,
                "page {p}"
            );
            // ... and it landed in the bank the nibble named
            assert_eq!(
                bus.cart_sdram.as_ref().unwrap().peek_raw(want[p as usize], 3),
                p as u8
            );
        }
        // an illegal page or bank is refused, not aliased
        bus.cart_sdram.as_mut().unwrap().set_page(6, 3);
        bus.cart_sdram.as_mut().unwrap().set_page(0, 16);
        assert_eq!(bus.cart_sdram.as_ref().unwrap().bank_of_page(0), 0xF);
    }

    /// Longword and word writes must round-trip EXACTLY — this is the arm that
    /// silicon reads GREEN (`SDRAMTEST=10`), and it is also the control for the
    /// byte-quirk test below: without it, a model that corrupted everything
    /// would pass the quirk test.
    #[test]
    fn longword_and_word_writes_round_trip() {
        let mut bus = sdram_bus();
        let a = 0xA0_0000; // page 2
        // the probe's own pattern: x = x*1664525 + 1013904223
        let mut x: u32 = 0x1234_5678;
        for k in 0..64u32 {
            x = x.wrapping_mul(1664525).wrapping_add(1013904223);
            bus.write32(a + k * 4, x);
        }
        let mut x: u32 = 0x1234_5678;
        for k in 0..64u32 {
            x = x.wrapping_mul(1664525).wrapping_add(1013904223);
            assert_eq!(bus.read32(a + k * 4), x, "longword {k}");
            // byte reads of a longword-written word are exact (silicon: the
            // second, GREEN pass of SDRAMTEST=10)
            for b in 0..4u32 {
                assert_eq!(bus.read8(a + k * 4 + b), (x >> (24 - 8 * b)) as u8);
            }
        }
        bus.write16(a + 0x100, 0xBEEF);
        assert_eq!(bus.read16(a + 0x100), 0xBEEF, "a word write is not narrow");
        assert_eq!(bus.read8(a + 0x100), 0xBE);
        assert_eq!(bus.read8(a + 0x101), 0xEF);
    }

    /// ☠ THE SILICON QUIRK (run 242): a BYTE write to the cart window fills the
    /// whole 16-bit word. `SDRAMTEST=11` writes a 64 KB pattern in bytes and
    /// verifies it in longwords: RED on the rig. A byte-loop `memcpy` into the
    /// window therefore corrupts every other byte, which is what cost four
    /// probe runs before modes 10/11 named it.
    #[test]
    fn a_byte_write_fills_the_whole_16_bit_word() {
        let mut bus = sdram_bus();
        let a = 0xB0_0000; // page 3
        bus.write32(a, 0x0000_0000);

        bus.write8(a, 0xAA); // even byte -> both halves of the word
        assert_eq!(bus.read16(a), 0xAAAA, "even byte did not fill the word");

        bus.write8(a + 3, 0x55); // odd byte -> both halves of ITS word
        assert_eq!(bus.read16(a + 2), 0x5555, "odd byte did not fill the word");

        // and the failure a ROM actually sees: a byte-loop copy of a longword
        // pattern comes back wrong, while the same bytes written as longwords
        // come back right.
        let pat: [u8; 8] = [0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08];
        for (i, b) in pat.iter().enumerate() {
            bus.write8(a + 0x40 + i as u32, *b);
        }
        let got: Vec<u8> = (0..8).map(|i| bus.read8(a + 0x40 + i)).collect();
        assert_ne!(got, pat.to_vec(), "the byte-loop copy must NOT survive");
        assert_eq!(got, vec![0x02, 0x02, 0x04, 0x04, 0x06, 0x06, 0x08, 0x08]);

        for (i, b) in pat.chunks(4).enumerate() {
            bus.write32(a + 0x80 + 4 * i as u32, u32::from_be_bytes(b.try_into().unwrap()));
        }
        let got: Vec<u8> = (0..8).map(|i| bus.read8(a + 0x80 + i)).collect();
        assert_eq!(got, pat.to_vec(), "the longword copy must survive");
    }

    /// The window is READ-ONLY until `GD_ROMWriteEnable(1)`. The control is in
    /// the same test: the identical store must land once the latch is open, or
    /// "nothing happened" would also pass for a model that never writes at all.
    #[test]
    fn writes_with_the_latch_off_do_not_land() {
        let mut bus = crate::bus::Bus::new();
        bus.cart_sdram = Some(Box::new(CartSdram::new())); // latch OFF, as at boot
        let a = 0xC0_0000; // page 4

        bus.write32(a, 0xDEAD_BEEF);
        assert_eq!(bus.read32(a), 0, "a write with the latch closed landed");
        assert_eq!(bus.cart_sdram.as_ref().unwrap().writes, 0);
        assert!(bus.cart_sdram.as_ref().unwrap().writes_refused > 0);
        // it is still reported as a write that went nowhere, as cart writes
        // always were
        assert!(bus.m68k_stray_write.is_some());

        // POSITIVE CONTROL: the same store, latch open.
        bus.cart_sdram.as_mut().unwrap().set_write_enable(1);
        bus.write32(a, 0xDEAD_BEEF);
        assert_eq!(bus.read32(a), 0xDEAD_BEEF, "the latch does not open");

        // and closing it again stops the next one
        bus.cart_sdram.as_mut().unwrap().set_write_enable(0);
        bus.write32(a, 0x0BAD_0BAD);
        assert_eq!(bus.read32(a), 0xDEAD_BEEF, "the latch does not close");
    }

    /// `JAGEMU_CARTSDRAM_RO` is the fault arm for the end-to-end probe: with it
    /// set, `GD_ROMWriteEnable(1)` must NOT open the window. A model that
    /// passes the probe both ways is not modelling anything.
    #[test]
    fn the_fault_arm_holds_the_latch_closed() {
        let mut s = CartSdram::new();
        s.set_write_enable(1);
        assert!(s.write_enabled(), "control: the latch opens normally");

        let mut s = CartSdram::new();
        s.force_read_only = true;
        s.set_write_enable(1);
        assert!(!s.write_enabled(), "the fault arm did not hold it closed");
        assert!(!s.write8(0x80_0000, 0xFF, false));
    }

    /// With no GameDrive attached the cart window must behave EXACTLY as it did
    /// before this model existed: reads 0, writes vanish into the stray-write
    /// diagnostic. Every ROM in the fleet that does not use the SDRAM depends
    /// on this.
    #[test]
    fn without_a_gamedrive_the_window_is_unchanged() {
        let mut bus = crate::bus::Bus::new();
        assert!(bus.cart_sdram.is_none());
        bus.write32(0x90_0000, 0xDEAD_BEEF);
        assert_eq!(bus.read32(0x90_0000), 0);
        assert!(bus.m68k_stray_write.is_some());
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
