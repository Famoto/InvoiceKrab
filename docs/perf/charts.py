#!/usr/bin/env python3
"""Regenerates the SVG charts in docs/perf/ from the measurements below.

Usage: python3 docs/perf/charts.py   (writes docs/perf/*.svg next to itself)

The numbers are the ones reported in docs/PERFORMANCE.md. After re-measuring,
update the data tables here and in the doc together, then re-run this script.
Standard library only; the SVGs carry their own light/dark color tokens.
"""

import math
from pathlib import Path

OUT = Path(__file__).resolve().parent

# --------------------------------------------------------------------------
# Measurements (release build, fat LTO, 4 vCPU — see docs/PERFORMANCE.md)
# --------------------------------------------------------------------------

# Input sizes in bytes of the UBL documents (3, 100, 1k, 10k, 100k lines).
SIZES = [9_065, 95_154, 895_855, 8_911_856, 89_161_857]

# CLI wall time, median ms, UBL source.
CLI_MS = {
    "→ UBL": [4.43, 5.90, 21.94, 175.32, 1992.73],
    "→ Factur-X": [6.05, 6.29, 22.51, 210.41, 2185.22],
    "→ FatturaPA": [4.11, 4.90, 15.49, 125.38, 1220.09],
}

# CLI peak RSS, MB, UBL source.
CLI_RSS = {
    "→ UBL": [7.4, 7.3, 9.0, 39.3, 339.5],
    "→ Factur-X": [7.4, 7.4, 10.8, 56.9, 419.1],
    "→ FatturaPA": [7.4, 7.3, 7.3, 26.4, 219.4],
}

# Server POST /transform req/s at 32 connections, UBL source.
SERVER_SIZES = [("9 KB", 9_065), ("95 KB", 95_154), ("0.9 MB", 895_855)]
SERVER_RPS = {
    "→ UBL": [12_144, 1_787, 216],
    "→ Factur-X": [10_145, 1_558, 179],
}

# 9 KB UBL→UBL latency (ms) at 8 connections, alone vs. while 8 connections
# upload 8.9 MB documents.
HOL = {
    "Alone": {"p50": 0.70, "p99": 2.17},
    "During large uploads": {"p50": 223.39, "p99": 319.82},
}

# Callgrind share of instructions, krab-cli UBL→UBL on a 1k-line invoice.
PROFILE = [
    ("Parse XML into typed model", "quick-xml", 56.90),
    ("Serialize (excl. name checks)", "quick-xml", 18.89),
    ("Element-name validation", "quick-xml", 14.21),
    ("Generated writer (hub → model)", "Generated mapping code", 6.88),
    ("Generated reader (model → hub)", "Generated mapping code", 2.74),
    ("Start-up, I/O, other", "Other", 0.38),
]

# --------------------------------------------------------------------------
# Styling: validated categorical slots 1-3 (all-pairs, light + dark).
# --------------------------------------------------------------------------

STYLE = """
<style>
  svg { --surface:#fcfcfb; --ink:#0b0b0b; --ink2:#52514e; --muted:#898781;
        --grid:#e1e0d9; --axis:#c3c2b7;
        --s1:#2a78d6; --s2:#eb6834; --s3:#1baf7a; --s0:#898781; }
  @media (prefers-color-scheme: dark) {
    svg { --surface:#1a1a19; --ink:#ffffff; --ink2:#c3c2b7; --muted:#898781;
          --grid:#2c2c2a; --axis:#383835;
          --s1:#3987e5; --s2:#d95926; --s3:#199e70; --s0:#898781; }
  }
  text { font-family: system-ui, -apple-system, "Segoe UI", sans-serif; }
  .title { font-size:16px; font-weight:600; fill:var(--ink); }
  .sub { font-size:12px; fill:var(--ink2); }
  .tick { font-size:11px; fill:var(--muted); font-variant-numeric:tabular-nums; }
  .axlab { font-size:11px; fill:var(--ink2); }
  .lab { font-size:12px; fill:var(--ink); }
  .val { font-size:11px; fill:var(--ink2); font-variant-numeric:tabular-nums; }
  .grid { stroke:var(--grid); stroke-width:1; }
  .base { stroke:var(--axis); stroke-width:1; }
  .ref { stroke:var(--muted); stroke-width:1; stroke-dasharray:4 4; fill:none; }
</style>
"""

SLOTS = ["var(--s1)", "var(--s2)", "var(--s3)"]
W, H = 760, 420


def svg(body, title, sub, h=H, label=""):
    return (
        f'<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 {W} {h}" '
        f'width="{W}" height="{h}" role="img" aria-label="{label or title}">\n'
        f"{STYLE}"
        f'<rect width="{W}" height="{h}" rx="8" fill="var(--surface)"/>\n'
        f'<text class="title" x="24" y="32">{title}</text>\n'
        f'<text class="sub" x="24" y="52">{sub}</text>\n'
        f"{body}</svg>\n"
    )


def legend(items, x, y):
    out, cx = [], x
    for name, color in items:
        out.append(f'<rect x="{cx}" y="{y - 9}" width="10" height="10" rx="2" fill="{color}"/>')
        out.append(f'<text class="lab" x="{cx + 16}" y="{y}">{name}</text>')
        cx += 16 + 7.2 * len(name) + 24
    return "\n".join(out) + "\n"


def fmt_bytes(b):
    if b >= 1e6:
        return f"{b / 1e6:g} MB"
    if b >= 1e3:
        return f"{b / 1e3:g} KB"
    return f"{b} B"


def loglog(series, ylab, yticks, yfmt, title, sub, ref=None, fname=""):
    """Log-log line chart, x = input size, one line per series."""
    L, R, T, B = 72, 150, 96, 360
    xmin, xmax = math.log10(5e3), math.log10(1.5e8)
    ymin, ymax = math.log10(yticks[0]), math.log10(yticks[-1])
    sx = lambda v: L + (math.log10(v) - xmin) / (xmax - xmin) * (W - L - R)
    sy = lambda v: B - (math.log10(v) - ymin) / (ymax - ymin) * (B - T)
    b = []
    for t in yticks:
        b.append(f'<line class="grid" x1="{L}" x2="{W - R}" y1="{sy(t):.1f}" y2="{sy(t):.1f}"/>')
        b.append(f'<text class="tick" x="{L - 8}" y="{sy(t) + 4:.1f}" text-anchor="end">{yfmt(t)}</text>')
    for t in [1e4, 1e5, 1e6, 1e7, 1e8]:
        b.append(f'<text class="tick" x="{sx(t):.1f}" y="{B + 18}" text-anchor="middle">{fmt_bytes(int(t))}</text>')
    b.append(f'<line class="base" x1="{L}" x2="{W - R}" y1="{B}" y2="{B}"/>')
    b.append(f'<text class="axlab" x="{(L + W - R) / 2}" y="{B + 40}" text-anchor="middle">Input size (log scale)</text>')
    b.append(f'<text class="axlab" transform="translate(18 {(T + B) / 2}) rotate(-90)" text-anchor="middle">{ylab}</text>')
    if ref:
        pts, text, tx, ty = ref
        b.append('<polyline class="ref" points="' + " ".join(f"{sx(x):.1f},{sy(y):.1f}" for x, y in pts) + '"/>')
        b.append(f'<text class="val" x="{sx(tx):.1f}" y="{sy(ty) + 16:.1f}">{text}</text>')
    ends = []
    for (name, ys), color in zip(series.items(), SLOTS):
        pts = " ".join(f"{sx(x):.1f},{sy(y):.1f}" for x, y in zip(SIZES, ys))
        b.append(f'<polyline points="{pts}" fill="none" stroke="{color}" stroke-width="2" stroke-linejoin="round"/>')
        for x, y in zip(SIZES, ys):
            b.append(
                f'<circle cx="{sx(x):.1f}" cy="{sy(y):.1f}" r="4" fill="{color}" '
                f'stroke="var(--surface)" stroke-width="2"><title>{name}, {fmt_bytes(x)}: {yfmt(y)}</title></circle>'
            )
        ends.append([sy(ys[-1]), name, yfmt(ys[-1]), color])
    # Direct end labels, nudged apart so they never collide.
    ends.sort()
    for i in range(1, len(ends)):
        ends[i][0] = max(ends[i][0], ends[i - 1][0] + 30)
    for y, name, val, color in ends:
        b.append(f'<rect x="{W - R + 12}" y="{y - 12:.1f}" width="10" height="10" rx="2" fill="{color}"/>')
        b.append(f'<text class="lab" x="{W - R + 28}" y="{y - 3:.1f}">{name}</text>')
        b.append(f'<text class="val" x="{W - R + 28}" y="{y + 11:.1f}">{val}</text>')
    b.append(legend(list(zip(series.keys(), SLOTS)), 24, 78))
    (OUT / fname).write_text(svg("\n".join(b) + "\n", title, sub))


def bar_path(x0, y, x1, h):
    """Horizontal bar with 4px rounded data end (right), square at baseline."""
    r = min(4, h / 2, max(x1 - x0, 0))
    return (
        f"M{x0:.1f},{y:.1f} H{x1 - r:.1f} Q{x1:.1f},{y:.1f} {x1:.1f},{y + r:.1f} "
        f"V{y + h - r:.1f} Q{x1:.1f},{y + h:.1f} {x1 - r:.1f},{y + h:.1f} H{x0:.1f} Z"
    )


def col_path(x, y0, w, y1):
    """Vertical bar with 4px rounded data end (top), square at baseline."""
    r = min(4, w / 2, max(y0 - y1, 0))
    return (
        f"M{x:.1f},{y0:.1f} V{y1 + r:.1f} Q{x:.1f},{y1:.1f} {x + r:.1f},{y1:.1f} "
        f"H{x + w - r:.1f} Q{x + w:.1f},{y1:.1f} {x + w:.1f},{y1 + r:.1f} V{y0:.1f} Z"
    )


def server_throughput():
    L, R, T, B = 72, 24, 96, 360
    ymax = 250
    sy = lambda v: B - v / ymax * (B - T)
    b = []
    for t in range(0, ymax + 1, 50):
        b.append(f'<line class="grid" x1="{L}" x2="{W - R}" y1="{sy(t):.1f}" y2="{sy(t):.1f}"/>')
        b.append(f'<text class="tick" x="{L - 8}" y="{sy(t) + 4:.1f}" text-anchor="end">{t}</text>')
    b.append(f'<text class="axlab" transform="translate(18 {(T + B) / 2}) rotate(-90)" text-anchor="middle">Aggregate input throughput (MB/s)</text>')
    group_w = (W - L - R) / len(SERVER_SIZES)
    bw, gap = 64, 2
    for gi, (label, size) in enumerate(SERVER_SIZES):
        cx = L + group_w * (gi + 0.5)
        x = cx - bw - gap / 2
        for (name, rps), color in zip(SERVER_RPS.items(), SLOTS):
            mbps = rps[gi] * size / 1e6
            y1 = sy(mbps)
            b.append(
                f'<path d="{col_path(x, B, bw, y1)}" fill="{color}">'
                f"<title>{name}, {label}: {mbps:.0f} MB/s ({rps[gi]:,} req/s)</title></path>"
            )
            b.append(f'<text class="val" x="{x + bw / 2:.1f}" y="{y1 - 20:.1f}" text-anchor="middle">{mbps:.0f} MB/s</text>')
            b.append(f'<text class="val" x="{x + bw / 2:.1f}" y="{y1 - 6:.1f}" text-anchor="middle">{rps[gi]:,}/s</text>')
            x += bw + gap
        b.append(f'<text class="lab" x="{cx:.1f}" y="{B + 20}" text-anchor="middle">{label} invoice</text>')
    b.append(f'<line class="base" x1="{L}" x2="{W - R}" y1="{B}" y2="{B}"/>')
    b.append(legend(list(zip(SERVER_RPS.keys(), SLOTS)), 24, 78))
    (OUT / "server-throughput.svg").write_text(
        svg("\n".join(b) + "\n",
            "krab-server: POST /transform throughput",
            "32 connections, 4 workers, UBL source. Bar labels: MB/s of input and requests/s.",
            h=400)
    )


def head_of_line():
    L, R, T = 150, 40, 96
    h = 300
    vmin, vmax = 0.1, 1000
    sx = lambda v: L + (math.log10(v) - math.log10(vmin)) / (math.log10(vmax) - math.log10(vmin)) * (W - L - R)
    b = []
    for t in [0.1, 1, 10, 100, 1000]:
        b.append(f'<line class="grid" x1="{sx(t):.1f}" x2="{sx(t):.1f}" y1="{T - 6}" y2="{h - 56}"/>')
        b.append(f'<text class="tick" x="{sx(t):.1f}" y="{h - 40}" text-anchor="middle">{t:g} ms</text>')
    b.append(f'<text class="axlab" x="{(L + W - R) / 2}" y="{h - 18}" text-anchor="middle">Latency of a 9 KB transform (log scale)</text>')
    y, bh = T + 4, 22
    for metric in ["p50", "p99"]:
        b.append(f'<text class="lab" x="{L - 12}" y="{y + bh + 5}" text-anchor="end">{metric}</text>')
        for (name, vals), color in zip(HOL.items(), SLOTS):
            v = vals[metric]
            b.append(
                f'<path d="{bar_path(sx(vmin), y, sx(v), bh)}" fill="{color}">'
                f"<title>{name}, {metric}: {v:g} ms</title></path>"
            )
            b.append(f'<text class="val" x="{sx(v) + 6:.1f}" y="{y + 15}">{v:g} ms</text>')
            y += bh + 2
        y += 22
    b.append(f'<line class="base" x1="{sx(vmin):.1f}" x2="{sx(vmin):.1f}" y1="{T - 6}" y2="{h - 56}"/>')
    b.append(legend(list(zip(HOL.keys(), SLOTS)), 24, 78))
    (OUT / "server-head-of-line.svg").write_text(
        svg("\n".join(b) + "\n",
            "Small requests stall behind large uploads",
            "9 KB UBL→UBL at 8 connections, alone vs. while 8 connections send 8.9 MB documents (issue #35).",
            h=h)
    )


def profile():
    owners = {"quick-xml": SLOTS[0], "Generated mapping code": SLOTS[1], "Other": "var(--s0)"}
    L, R, T = 230, 70, 96
    bh, gap = 24, 2
    h = T + len(PROFILE) * (bh + gap) + 60
    sx = lambda v: L + v / 60 * (W - L - R)
    b = []
    for t in range(0, 61, 10):
        b.append(f'<line class="grid" x1="{sx(t):.1f}" x2="{sx(t):.1f}" y1="{T - 6}" y2="{h - 52}"/>')
        b.append(f'<text class="tick" x="{sx(t):.1f}" y="{h - 36}" text-anchor="middle">{t}%</text>')
    b.append(f'<text class="axlab" x="{(L + W - R) / 2}" y="{h - 14}" text-anchor="middle">Share of executed instructions</text>')
    y = T
    for name, owner, pct in PROFILE:
        b.append(f'<text class="lab" x="{L - 10}" y="{y + 16}" text-anchor="end">{name}</text>')
        b.append(
            f'<path d="{bar_path(sx(0), y, sx(pct), bh)}" fill="{owners[owner]}">'
            f"<title>{name} ({owner}): {pct:.1f}%</title></path>"
        )
        b.append(f'<text class="val" x="{sx(pct) + 6:.1f}" y="{y + 16}">{pct:.1f}%</text>')
        y += bh + gap
    b.append(f'<line class="base" x1="{sx(0):.1f}" x2="{sx(0):.1f}" y1="{T - 6}" y2="{h - 52}"/>')
    b.append(legend(list(owners.items()), 24, 78))
    (OUT / "cpu-profile.svg").write_text(
        svg("\n".join(b) + "\n",
            "Where transform time goes",
            "Callgrind, krab-cli UBL→UBL on a 1k-line (0.9 MB) invoice.",
            h=h)
    )


if __name__ == "__main__":
    loglog(CLI_MS, "Wall time (log scale)", [1, 10, 100, 1000, 10000],
           lambda v: f"{v:,.0f} ms" if v >= 10 else f"{v:g} ms",
           "krab-cli: wall time vs. input size",
           "UBL source, median of 5 runs. Below ~100 KB, process start-up (~3 ms) dominates.",
           ref=([(5e3, 3), (1.5e8, 3)], "process start-up ≈ 3 ms", 1.5e7, 3),
           fname="cli-time.svg")
    loglog(CLI_RSS, "Peak RSS (log scale)", [1, 10, 100, 1000],
           lambda v: f"{v:,.0f} MB" if v >= 10 else f"{v:g} MB",
           "krab-cli: peak memory vs. input size",
           "UBL source, peak RSS. Dashed line = the input document's own size (1×).",
           ref=([(1e6, 1), (1.5e8, 150)], "input size (1×)", 1.2e7, 9),
           fname="cli-memory.svg")
    server_throughput()
    head_of_line()
    profile()
