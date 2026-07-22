# Jaguar CD backend — scope and roadmap

**Why:** cartridge address space caps at 6 MB; large games (full OpenLara,
anything gamestream-scale) currently reach only GameDrive owners. The Jaguar
CD (~790 MB/disc) is the large-content format the *rest* of the audience can
run: BigPEmu emulates the CD unit well, and real CD units exist. Target:
the same `gd_fopen`/`gd_fread` asset API over a `cdfs` backend, so one game
binary ships on GameDrive, CD, and (≤6 MB episodes) cartridge.

**Distribution matrix this completes:**
GameDrive = full game on real HW (done) · CD image = full game on
BigPEmu + real CD · `.j64` episode = everything else (jrom, done) ·
MiSTer big-content = future core contribution (jsim's `gamedrive.rs` is
the reference SPI implementation).

## Primary sources (all on this machine, none consulted-from-memory)

- `cubanismo/jaguar-sdk` docker: `/jaguar-sdk/jaguar/cdrom/` — CD BIOS
  (`cd_bios.20`, `cdbios45.*`), sample CD code (`cd_samp.s` family, DSA
  usage), `readme.txt`; `/jaguar-sdk/jaguar/cdboot/cdboot.txt` — the boot
  track format.
- BigPEmu as functional oracle for Butch behavior (same discipline as the
  cart-era work: BigPEmu answers "what", the rig answers "how fast" —
  except there is no CD rig here, so BigPEmu is the *only* oracle; every
  Butch register semantic must cite either the SDK docs or a BigPEmu
  probe, and timing stays explicitly unmodeled until someone benches a
  real unit).

## Work items, in order

1. **Extract + vendor the reference docs** from the SDK image into
   `sim/docs/spec/JAGCD.md` (register map for Butch at `$DFFF00`, DSA
   command set, I2S data path into Jerry, boot-track layout from
   `cdboot.txt`) — provenance-tagged like every other spec file.
2. **jsim: Butch device model** (`jag-core/src/butch.rs`): register window
   in cart space, DSA command FIFO, session/track table from an attached
   image, data delivery via the documented path. File-backed like
   `gamedrive.rs`: `jagemu --cd image/` (a directory of tracks first; real
   `.cdi`/`.cue` parsing after). Functional fidelity first; NO timing
   claims (no bench source).
3. **`jcd` image builder**: asset directory → boot track (per `cdboot.txt`)
   + data session; boots the same program jrom packages. Validation loop:
   build image → boot in jsim → pixel-diff vs the gamestream run; then the
   maintainer boots the same image in BigPEmu (the oracle check we cannot
   run headlessly).
4. **`cdfs` backend** for the game side: `gd_*`-signature calls over DSA
   reads, selected by boot-time probe (GameDrive absent + Butch present).
5. (Separate, MiSTer) offer `gamedrive.rs` as the spec for an HPS-side
   GameDrive service in the open-source core — tracked as an upstream
   conversation, not a cobweb work item.

## Non-goals (v1)

- CD audio (CDDA) playback modeling — data reads first; audio after the
  I2S path is proven against a BigPEmu comparison.
- Encryption/boot-signature specifics beyond what `cdboot.txt` documents —
  BigPEmu's homebrew handling is the compatibility bar.
- Any timing model. Butch timing is unbenchable without a CD rig; the
  fidelity tiers in `COMPATIBILITY.md` gain a "CD: functional-only" row.

## Status

- 2026-07-21: scope filed. Next session starts with item 1 (doc
  extraction) — deliberately BEFORE any code, per the no-constants-without-
  provenance rule.
