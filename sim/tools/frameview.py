#!/usr/bin/env python3
"""frameview — render a jagemu run's timing telemetry as a frame-anatomy card.

A consumer of the one-JSON-per-run contract (`jagemu run … > run.json`); the
emitters carry no presentation logic. Produces ONE self-contained HTML file:
inline CSS/SVG, no external requests, light + dark, hover tooltips, and a
table view of every number drawn.

Why this exists: raw counters invite misattribution — the `blit` busy ledger
read as "54.2% of the frame" when the paid cost was 7.3%
(COBWEB_BUG_blitter_overcharged round 2). The card draws BUSY and PAID as
different things by construction.

    tools/frameview.py run.json -o card.html
        [--label NAME]                    name for the run
    tools/frameview.py a.json b.json -o card.html
        [--label A --label B]             pair-diff: two anatomies, shared axis
    [--fps  LABEL=JSIM[:SILICON]] ...     fps ladder rows (silicon optional)

Wall-clock basis: RISC ticks at 26.59 MHz; wall = frames/60 s.
"""

import argparse
import html
import json
import sys

RISC_HZ = 26_593_900
FIELD_HZ = 60.0

# GPU wall-anatomy segments, fixed order = fixed categorical slots (never
# cycled). Idle is NOT a series — it wears the de-emphasis gray.
SEGMENTS = [
    ("Execute", "s1"),
    ("Jump refill", "s2"),
    ("Scoreboard stalls", "s3"),
    ("External access", "s4"),
    ("Blitter wait", "s5"),
]
BUSY_SEGMENTS = [("Transfer (busy)", "s6"), ("Launch (busy)", "s7")]


def load_run(path):
    with open(path) as f:
        d = json.load(f)
    s = d.get("state", d)
    g = s["gpu"]
    t = g["timing"]
    frames = d.get("frames") or s.get("frame") or 0
    wall_ticks = frames / FIELD_HZ * RISC_HZ
    stalls = (
        t["stall_alu"] + t["stall_load"] + t["stall_div"]
        + t["stall_flags"] + t["stall_div_busy"]
    )
    external = t["fetch_external"] + t["mem_external"] + t["contention"]
    blit_wait = t.get("blit_wait", 0)
    cycles = g["cycles"]
    execute = max(0, cycles - stalls - t["jump_refill"] - external - blit_wait)
    return {
        "frames": frames,
        "wall_ticks": wall_ticks,
        "cycles": cycles,
        "segments": {
            "Execute": execute,
            "Jump refill": t["jump_refill"],
            "Scoreboard stalls": stalls,
            "External access": external,
            "Blitter wait": blit_wait,
        },
        "busy": {
            "Transfer (busy)": t.get("blit_transfer", 0),
            "Launch (busy)": t.get("blit_launch", 0),
        },
        "busy_total": t.get("blit", 0),
        "idle": max(0.0, wall_ticks - cycles),
    }


def pct(v, wall):
    return 100.0 * v / wall if wall else 0.0


ESC = html.escape


def svg_anatomy(runs, width=860):
    """Stacked GPU wall-share bars (one per run) + busy-ledger bars beneath,
    all on ONE percent-of-wall axis."""
    bar_h, gap, row_gap = 24, 2, 14
    label_w, pad_r, top = 150, 92, 8
    rows = []
    for r in runs:
        rows.append(("anatomy", r))
        rows.append(("busy", r))
    h = top + len(rows) * (bar_h + row_gap) + 30
    plot_w = width - label_w - pad_r
    out = [
        f'<svg viewBox="0 0 {width} {h}" role="img" '
        f'aria-label="GPU frame anatomy, percent of wall clock" '
        f'font-family="system-ui,sans-serif" font-size="12">'
    ]
    # x gridlines every 20%
    for gx in range(0, 101, 20):
        x = label_w + plot_w * gx / 100
        out.append(
            f'<line x1="{x:.1f}" y1="{top}" x2="{x:.1f}" y2="{h-26}" class="grid"/>'
        )
        out.append(
            f'<text x="{x:.1f}" y="{h-12}" text-anchor="middle" class="tick">{gx}%</text>'
        )
    y = top
    for kind, r in rows:
        wall = r["wall_ticks"]
        if kind == "anatomy":
            out.append(
                f'<text x="{label_w-8}" y="{y+bar_h/2+4}" text-anchor="end" '
                f'class="lab">{ESC(r["label"])}</text>'
            )
            x = label_w
            for name, slot in SEGMENTS:
                v = r["segments"][name]
                w = plot_w * pct(v, wall) / 100
                if w <= 0:
                    continue
                seg_w = max(0.0, w - gap)
                out.append(
                    f'<rect x="{x:.1f}" y="{y}" width="{seg_w:.1f}" height="{bar_h}" '
                    f'class="{slot}" data-tip="{ESC(r["label"])} — {ESC(name)}: '
                    f'{pct(v, wall):.1f}% of wall ({v:,} ticks)"/>'
                )
                x += w
            # idle: de-emphasis, to 100%
            idle_w = plot_w * pct(r["idle"], wall) / 100
            if idle_w > gap:
                out.append(
                    f'<rect x="{x:.1f}" y="{y}" width="{idle_w-gap:.1f}" height="{bar_h}" '
                    f'class="idle" data-tip="{ESC(r["label"])} — GPU idle/other: '
                    f'{pct(r["idle"], wall):.1f}% of wall"/>'
                )
            # direct label: the paid blit share (the number that was misread)
            bw = pct(r["segments"]["Blitter wait"], wall)
            out.append(
                f'<text x="{label_w+plot_w+6}" y="{y+bar_h/2+4}" class="val">'
                f'wait {bw:.1f}%</text>'
            )
        else:
            out.append(
                f'<text x="{label_w-8}" y="{y+bar_h/2+3}" text-anchor="end" '
                f'class="sublab">busy ledger</text>'
            )
            x = label_w
            for name, slot in BUSY_SEGMENTS:
                v = r["busy"][name]
                w = plot_w * pct(v, wall) / 100
                if w <= 0:
                    continue
                out.append(
                    f'<rect x="{x:.1f}" y="{y+4}" width="{max(0.0,w-gap):.1f}" '
                    f'height="{bar_h-8}" class="{slot}" '
                    f'data-tip="{ESC(r["label"])} — {ESC(name)}: '
                    f'{pct(v, wall):.1f}% of wall ({v:,} ticks). '
                    f'Asynchronous busy time — NOT frame cost."/>'
                )
                x += w
            bt = pct(r["busy_total"], wall)
            out.append(
                f'<text x="{label_w+plot_w+6}" y="{y+bar_h/2+3}" class="val">'
                f'busy {bt:.1f}%</text>'
            )
        y += bar_h + row_gap
    out.append("</svg>")
    return "".join(out)


def svg_ladder(rows, width=860):
    """Grouped horizontal bars: jsim vs silicon fps per build."""
    bar_h, pair_gap, group_gap = 14, 2, 16
    label_w, pad_r, top = 150, 90, 8
    plot_w = width - label_w - pad_r
    fmax = max(max(r["jsim"], r["silicon"] or 0) for r in rows) * 1.15
    h = top + len(rows) * (bar_h * 2 + pair_gap + group_gap) + 30
    out = [
        f'<svg viewBox="0 0 {width} {h}" role="img" aria-label="fps ladder, '
        f'jsim vs silicon" font-family="system-ui,sans-serif" font-size="12">'
    ]
    ticks = max(1, int(fmax // 2))
    for gx in range(0, int(fmax) + 1, max(1, ticks // 2) or 1):
        x = label_w + plot_w * gx / fmax
        out.append(f'<line x1="{x:.1f}" y1="{top}" x2="{x:.1f}" y2="{h-26}" class="grid"/>')
        out.append(f'<text x="{x:.1f}" y="{h-12}" text-anchor="middle" class="tick">{gx}</text>')
    y = top
    for r in rows:
        out.append(
            f'<text x="{label_w-8}" y="{y+bar_h+2}" text-anchor="end" class="lab">'
            f'{ESC(r["label"])}</text>'
        )
        for i, (src, cls) in enumerate((("jsim", "s1"), ("silicon", "s2"))):
            v = r[src]
            if v is None:
                continue
            w = plot_w * v / fmax
            yy = y + i * (bar_h + pair_gap)
            rr = min(4.0, w)
            out.append(
                f'<path d="M{label_w} {yy} h{w-rr:.1f} a{rr} {rr} 0 0 1 {rr} {rr} '
                f'v{bar_h-2*rr} a{rr} {rr} 0 0 1 -{rr} {rr} h-{w-rr:.1f} z" '
                f'class="{cls}" '
                f'data-tip="{ESC(r["label"])} — {src}: {v:.2f} fps"/>'
            )
            out.append(
                f'<text x="{label_w+w+6}" y="{yy+bar_h-3}" class="val">{v:.2f}</text>'
            )
        y += bar_h * 2 + pair_gap + group_gap
    out.append("</svg>")
    return "".join(out)


CSS = """
.viz-root {
  color-scheme: light;
  --surface-1:#fcfcfb; --text-primary:#0b0b0b; --text-secondary:#52514e;
  --text-muted:#8a887f; --grid:#e4e3de;
  --s1:#2a78d6; --s2:#008300; --s3:#e87ba4; --s4:#eda100; --s5:#1baf7a;
  --s6:#eb6834; --s7:#4a3aa7; --idle:#dddcd5;
  background:var(--surface-1); color:var(--text-primary);
  font:14px/1.5 system-ui,sans-serif; max-width:920px; margin:0 auto;
  padding:20px 16px 48px;
}
@media (prefers-color-scheme: dark) {
  :root:where(:not([data-theme="light"])) .viz-root {
    color-scheme: dark;
    --surface-1:#1a1a19; --text-primary:#ffffff; --text-secondary:#c3c2b7;
    --text-muted:#8b897e; --grid:#33322f;
    --s1:#3987e5; --s2:#008300; --s3:#d55181; --s4:#c98500; --s5:#199e70;
    --s6:#d95926; --s7:#9085e9; --idle:#2c2b29;
  }
}
:root[data-theme="dark"] .viz-root {
  color-scheme: dark;
  --surface-1:#1a1a19; --text-primary:#ffffff; --text-secondary:#c3c2b7;
  --text-muted:#8b897e; --grid:#33322f;
  --s1:#3987e5; --s2:#008300; --s3:#d55181; --s4:#c98500; --s5:#199e70;
  --s6:#d95926; --s7:#9085e9; --idle:#2c2b29;
}
.viz-root h1 { font-size:18px; margin:0 0 2px; }
.viz-root .sub { color:var(--text-secondary); margin:0 0 18px; font-size:13px; }
.viz-root h2 { font-size:14px; margin:26px 0 6px; }
.viz-root svg { width:100%; height:auto; display:block; }
.viz-root .grid { stroke:var(--grid); stroke-width:1; }
.viz-root .tick, .viz-root .sublab { fill:var(--text-muted); font-size:11px; }
.viz-root .lab { fill:var(--text-primary); }
.viz-root .val { fill:var(--text-secondary); font-size:11px; }
.viz-root rect.s1,.viz-root path.s1{fill:var(--s1)}
.viz-root rect.s2,.viz-root path.s2{fill:var(--s2)}
.viz-root rect.s3,.viz-root path.s3{fill:var(--s3)}
.viz-root rect.s4,.viz-root path.s4{fill:var(--s4)}
.viz-root rect.s5,.viz-root path.s5{fill:var(--s5)}
.viz-root rect.s6,.viz-root path.s6{fill:var(--s6)}
.viz-root rect.s7,.viz-root path.s7{fill:var(--s7)}
.viz-root rect.idle{fill:var(--idle)}
.viz-root .legend { display:flex; flex-wrap:wrap; gap:6px 16px; margin:8px 0 0;
  font-size:12px; color:var(--text-secondary); }
.viz-root .legend span { display:inline-flex; align-items:center; gap:6px; }
.viz-root .legend i { width:10px; height:10px; border-radius:2px; display:inline-block; }
.viz-root details { margin-top:14px; }
.viz-root summary { cursor:pointer; color:var(--text-secondary); font-size:13px; }
.viz-root table { border-collapse:collapse; margin-top:8px; font-size:12px;
  font-variant-numeric:tabular-nums; }
.viz-root th, .viz-root td { padding:3px 10px; text-align:right;
  border-bottom:1px solid var(--grid); }
.viz-root th:first-child, .viz-root td:first-child { text-align:left; }
.viz-root .note { color:var(--text-muted); font-size:12px; margin-top:6px; }
#tip { position:fixed; pointer-events:none; background:var(--text-primary);
  color:var(--surface-1); padding:5px 9px; border-radius:5px; font-size:12px;
  max-width:340px; opacity:0; transition:opacity .08s; z-index:9; }
"""

JS = """
const tip = document.getElementById('tip');
document.querySelectorAll('[data-tip]').forEach(el => {
  el.addEventListener('mousemove', e => {
    tip.textContent = el.dataset.tip;
    tip.style.opacity = 1;
    tip.style.left = Math.min(e.clientX + 12, innerWidth - 350) + 'px';
    tip.style.top = (e.clientY + 14) + 'px';
  });
  el.addEventListener('mouseleave', () => tip.style.opacity = 0);
});
"""


def legend(items):
    parts = [
        f'<span><i style="background:var(--{slot})"></i>{ESC(name)}</span>'
        for name, slot in items
    ]
    parts.append('<span><i style="background:var(--idle)"></i>GPU idle / other</span>')
    return '<div class="legend">' + "".join(parts) + "</div>"


def table_view(runs):
    heads = [n for n, _ in SEGMENTS] + [n for n, _ in BUSY_SEGMENTS] + [
        "Busy total", "GPU idle", "GPU cycles", "Wall ticks"]
    out = ["<details><summary>Table view (every number drawn)</summary><table>"]
    out.append("<tr><th>run</th>" + "".join(f"<th>{ESC(h)}</th>" for h in heads) + "</tr>")
    for r in runs:
        w = r["wall_ticks"]
        cells = [f"{pct(r['segments'][n], w):.2f}%" for n, _ in SEGMENTS]
        cells += [f"{pct(r['busy'][n], w):.2f}%" for n, _ in BUSY_SEGMENTS]
        cells += [f"{pct(r['busy_total'], w):.2f}%", f"{pct(r['idle'], w):.2f}%",
                  f"{r['cycles']:,}", f"{int(w):,}"]
        out.append(
            f"<tr><td>{ESC(r['label'])}</td>"
            + "".join(f"<td>{c}</td>" for c in cells) + "</tr>"
        )
    out.append("</table></details>")
    return "".join(out)


def main():
    p = argparse.ArgumentParser()
    p.add_argument("runs", nargs="+", help="jagemu run JSON file(s) (1 = card, 2 = pair-diff)")
    p.add_argument("--label", action="append", default=[], help="label per run, in order")
    p.add_argument("--fps", action="append", default=[],
                   help="ladder row: LABEL=JSIM[:SILICON]")
    p.add_argument("--title", default="Jaguar frame anatomy")
    p.add_argument("-o", "--out", required=True)
    a = p.parse_args()

    runs = []
    for i, path in enumerate(a.runs):
        r = load_run(path)
        r["label"] = a.label[i] if i < len(a.label) else path.rsplit("/", 1)[-1]
        runs.append(r)

    ladder = []
    for spec in a.fps:
        label, _, vals = spec.partition("=")
        jsim, _, sil = vals.partition(":")
        ladder.append({"label": label, "jsim": float(jsim),
                       "silicon": float(sil) if sil else None})

    body = [f'<div class="viz-root"><h1>{ESC(a.title)}</h1>']
    body.append(
        '<p class="sub">Percent of wall clock (26.59 MHz RISC ticks, 60 Hz fields). '
        'The <b>busy ledger</b> row is asynchronous Blitter busy time — under an '
        'overlapping kernel it exceeds what the frame pays; the paid cost is the '
        '<b>Blitter wait</b> segment.</p>'
    )
    body.append("<h2>GPU wall anatomy</h2>")
    body.append(svg_anatomy(runs))
    body.append(legend(SEGMENTS + BUSY_SEGMENTS))
    if len(runs) == 2:
        aw, bw = runs[0], runs[1]
        deltas = []
        for n, _ in SEGMENTS:
            d = pct(aw["segments"][n], aw["wall_ticks"]) - pct(bw["segments"][n], bw["wall_ticks"])
            if abs(d) >= 0.05:
                deltas.append(f"{n} {d:+.1f}pp")
        body.append(
            f'<p class="note">Δ ({ESC(aw["label"])} − {ESC(bw["label"])}): '
            + ", ".join(deltas) + "</p>"
        )
    if ladder:
        body.append("<h2>fps ladder — jsim vs silicon</h2>")
        body.append(svg_ladder(ladder))
        body.append(
            '<div class="legend"><span><i style="background:var(--s1)"></i>jsim</span>'
            '<span><i style="background:var(--s2)"></i>silicon</span></div>'
        )
    body.append(table_view(runs))
    body.append('<div id="tip"></div></div>')

    doc = (
        "<!doctype html><html><head><meta charset='utf-8'>"
        f"<title>{ESC(a.title)}</title><style>{CSS}</style></head><body>"
        + "".join(body)
        + f"<script>{JS}</script></body></html>"
    )
    with open(a.out, "w") as f:
        f.write(doc)
    print(f"frameview: wrote {a.out} ({len(runs)} run(s), {len(ladder)} ladder row(s))")


if __name__ == "__main__":
    main()
