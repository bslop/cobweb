#!/usr/bin/env python3
"""framecheck — scored comparison of an emulator frame against a captured
hardware frame (the video counterpart of `jagemu audiocheck`).

The failure mode this exists for: "emulator clean, silicon streaked"
(COBWEB_REQ_rectshade_and_calibration §5.3) — today that verdict needs a
human eyeball; this makes it a number a background task can gate on.

    tools/framecheck.py <emu.png> <hw.jpg|png> [--tolerance N] [--diff out.png]

The hardware frame (720x480 USB capture with borders, NTSC gamma/levels) is
auto-cropped to its active region, rescaled to the emulator frame's size,
and luma-normalized before comparison, so only *structure* differences
count. Metrics:

  pct_bad        fraction of pixels whose |diff| exceeds --tolerance
  mean_diff      mean absolute luma difference (post-normalization)
  streak_score   fraction of pixel COLUMNS whose mean diff is anomalous —
                 the rig fault's signature is per-polygon VERTICAL streaks
  verdict        match / mismatch (+ streaky flag)

Exit code 0 = match, 1 = mismatch, 2 = usage/IO error. One JSON on stdout.

Self-test (no hardware): --selftest degrades a frame the way a capture
does (border, rescale, gamma, JPEG) and checks MATCH, then adds synthetic
streaks and checks MISMATCH+streaky.
"""

import argparse
import json
import sys

import numpy as np
from PIL import Image


def luma(img: np.ndarray) -> np.ndarray:
    return img[..., 0] * 0.299 + img[..., 1] * 0.587 + img[..., 2] * 0.114


def autocrop(img: np.ndarray, thresh: float = 12.0) -> np.ndarray:
    """Crop away the capture's dead borders: keep rows/cols with content."""
    y = luma(img)
    rows = np.where(y.max(axis=1) > thresh)[0]
    cols = np.where(y.max(axis=0) > thresh)[0]
    if len(rows) < 16 or len(cols) < 16:
        return img  # nearly-black frame: nothing to crop against
    return img[rows[0] : rows[-1] + 1, cols[0] : cols[-1] + 1]


def normalize_luma(a: np.ndarray, ref: np.ndarray) -> np.ndarray:
    """Match a's luma mean/std to ref's (capture gamma/levels differ)."""
    ya, yr = luma(a), luma(ref)
    sa = ya.std() or 1.0
    scaled = (a - ya.mean()) * (yr.std() / sa) + yr.mean()
    return np.clip(scaled, 0, 255)


def compare(emu: np.ndarray, hw: np.ndarray, tolerance: float):
    h, w = emu.shape[:2]
    hw_active = autocrop(hw)
    hw_scaled = np.asarray(
        Image.fromarray(hw_active.astype(np.uint8)).resize((w, h), Image.BILINEAR),
        dtype=float,
    )
    hw_norm = normalize_luma(hw_scaled, emu)
    diff = np.abs(luma(hw_norm) - luma(emu))
    # ignore a 4-px frame edge: capture ringing/overscan lives there
    core = diff[4 : h - 4, 4 : w - 4]
    pct_bad = float((core > tolerance).mean())
    mean_diff = float(core.mean())
    # vertical-streak signature: columns whose mean diff is way above the
    # frame's own median column
    col_mean = core.mean(axis=0)
    med = np.median(col_mean)
    streak_cols = col_mean > max(2.5 * med, med + tolerance)
    streak_score = float(streak_cols.mean())
    return diff, {
        "pct_bad": round(pct_bad, 4),
        "mean_diff": round(mean_diff, 2),
        "streak_score": round(streak_score, 4),
        "streaky": streak_score > 0.08,
        "matches": pct_bad < 0.10 and streak_score <= 0.08,
    }


def load(path: str) -> np.ndarray:
    return np.asarray(Image.open(path).convert("RGB"), dtype=float)


def degrade(emu: np.ndarray) -> np.ndarray:
    """Simulate the capture path: border, rescale, gamma, JPEG round-trip."""
    import io

    img = Image.fromarray(emu.astype(np.uint8)).resize((656, 440), Image.BILINEAR)
    framed = Image.new("RGB", (720, 480), (2, 2, 2))
    framed.paste(img, (32, 20))
    arr = np.asarray(framed, dtype=float)
    arr = 255.0 * (arr / 255.0) ** 1.15  # gamma shift
    buf = io.BytesIO()
    Image.fromarray(arr.astype(np.uint8)).save(buf, "JPEG", quality=80)
    return np.asarray(Image.open(buf).convert("RGB"), dtype=float)


def selftest(emu_path: str, tolerance: float) -> int:
    emu = load(emu_path)
    hw = degrade(emu)
    _, clean = compare(emu, hw, tolerance)
    streaked = hw.copy()
    for x in range(60, 660, 24):  # per-polygon vertical streaks
        streaked[100:420, x : x + 3] = 255.0
    _, bad = compare(emu, streaked, tolerance)
    ok = clean["matches"] and not bad["matches"] and bad["streaky"]
    print(json.dumps({"ok": ok, "clean": clean, "streaked": bad}))
    return 0 if ok else 1


def main() -> int:
    p = argparse.ArgumentParser()
    p.add_argument("emu", help="emulator frame (PNG)")
    p.add_argument("hw", nargs="?", help="hardware capture frame (JPG/PNG)")
    p.add_argument("--tolerance", type=float, default=40.0,
                   help="per-pixel luma diff considered 'bad' (default 40)")
    p.add_argument("--diff", help="write a diff heat image here")
    p.add_argument("--selftest", action="store_true")
    a = p.parse_args()

    if a.selftest:
        return selftest(a.emu, a.tolerance)
    if not a.hw:
        p.error("need <hw> frame (or --selftest)")

    try:
        emu, hw = load(a.emu), load(a.hw)
    except Exception as e:
        print(json.dumps({"ok": False, "error": str(e)}))
        return 2
    diff, m = compare(emu, hw, a.tolerance)
    if a.diff:
        heat = np.clip(diff * (255.0 / max(diff.max(), 1.0)), 0, 255).astype(np.uint8)
        Image.fromarray(heat).save(a.diff)
    out = {"ok": True, "emu": a.emu, "hw": a.hw, **m}
    print(json.dumps(out))
    print(
        f"framecheck: {'MATCH' if m['matches'] else 'MISMATCH'}"
        f"{' (VERTICAL STREAKS)' if m['streaky'] else ''} — "
        f"{m['pct_bad']*100:.1f}% pixels off, streak score {m['streak_score']:.3f}",
        file=sys.stderr,
    )
    return 0 if m["matches"] else 1


if __name__ == "__main__":
    sys.exit(main())
