#!/usr/bin/env python3
"""Regenerates the SVG charts in docs/perf/ from the measurements below.

Usage: python3 docs/perf/charts.py   (writes docs/perf/*.svg next to itself)

The numbers are the ones reported in docs/PERFORMANCE.md. After re-measuring,
update the data here and the tables in the doc together, then re-run this
script. Standard library only; the SVGs carry their own light/dark colors.
"""

import math
from pathlib import Path

OUT = Path(__file__).resolve().parent

# --------------------------------------------------------------------------
# Measurements (release build, 4 vCPU — see docs/PERFORMANCE.md)
# --------------------------------------------------------------------------

# UBL invoice sizes in bytes: 3, 100, 1k, 10k and 100k invoice lines.
SIZES = [9_065, 95_154, 895_855, 8_911_856, 89_161_857]

# Server: invoices per second on 4 workers, 32 connections, by invoice size.
SERVER_SIZES = SIZES[:3]
SERVER_RPS = {
    "To UBL / XRechnung": [12_144, 1_787, 216],
    "To Factur-X": [10_145, 1_558, 179],
}

# Server: invoices per second by worker count (= CPU cores), 16 connections,
# UBL → XRechnung. The load generator shares the 4 cores, so the 4-core
# point understates the server (most visibly for the small invoice).
CORES = [1, 2, 3, 4]
CORE_RPS = {
    "Typical invoice (9 KB)": [4_409, 8_798, 12_555, 11_683],
    "100-line invoice (95 KB)": [510, 1_047, 1_507, 1_840],
}

# Single invoice, end-to-end time (command line, includes start-up), ms.
ONE_MS = {
    "To UBL": [4.43, 5.90, 21.94, 175.32, 1992.73],
    "To Factur-X": [6.05, 6.29, 22.51, 210.41, 2185.22],
    "To FatturaPA": [4.11, 4.90, 15.49, 125.38, 1220.09],
}

# Single invoice, peak memory (command line), MB.
ONE_MB = {
    "To UBL": [7.4, 7.3, 9.0, 39.3, 339.5],
    "To Factur-X": [7.4, 7.4, 10.8, 56.9, 419.1],
    "To FatturaPA": [7.4, 7.3, 7.3, 26.4, 219.4],
}

# Server peak memory (MB) with N large (8.9 MB) invoices sent at once,
# 4 workers. Idle: 3 MB.
CONCURRENT = [(1, 47), (2, 72), (4, 136), (8, 174)]

# Server peak memory per request as a multiple of the invoice's file size,
# worst output format, invoices of 1 MB and up, one request on a fresh
# server (idle memory excluded). glibc = Linux build, musl = Docker image.
FORMAT_BLOWUP = {
    "glibc (Linux build)": {"FatturaPA": 11.5, "UBL, XRechnung, Peppol": 5.9, "Factur-X": 3.8},
    "musl (Docker image)": {"FatturaPA": 9.0, "UBL, XRechnung, Peppol": 4.5, "Factur-X": 3.1},
}
BLOWUP_DEFAULT = 12

# Typical invoice latency (ms), alone vs. while large invoices are processed.
HOL = {
    "Typical invoices only": {"Median": 0.70, "Slowest 1%": 2.17},
    "While large invoices run": {"Median": 223.39, "Slowest 1%": 319.82},
}

# --------------------------------------------------------------------------
# Styling: validated categorical slots 1-3 (all-pairs, light + dark).
# --------------------------------------------------------------------------

STYLE = """
<style>
  svg { --surface:#fcfcfb; --ink:#0b0b0b; --ink2:#52514e; --muted:#898781;
        --grid:#e1e0d9; --axis:#c3c2b7;
        --s1:#2a78d6; --s2:#eb6834; --s3:#1baf7a; }
  @media (prefers-color-scheme: dark) {
    svg { --surface:#1a1a19; --ink:#ffffff; --ink2:#c3c2b7; --muted:#898781;
          --grid:#2c2c2a; --axis:#383835;
          --s1:#3987e5; --s2:#d95926; --s3:#199e70; }
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
W = 760


def svg(body, title, sub, h, label=""):
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


def num(v):
    return f"{v:,.0f}" if v >= 10 else f"{v:g}"


def line_chart(fname, title, sub, series, xs, *, xlog, ylog, xticks, yticks,
               xfmt, yfmt, xlab, ylab, ref=None, h=420, end_labels=True):
    """Line chart with markers, direct end labels and an optional dashed
    reference line `ref = (points, text, (tx, ty))`."""
    L, R, T, B = 80, 200 if end_labels else 32, 96, h - 60
    tx = (lambda v: math.log10(v)) if xlog else (lambda v: v)
    ty = (lambda v: math.log10(v)) if ylog else (lambda v: v)
    x0, x1 = tx(xticks[0]), tx(xticks[-1])
    y0, y1 = ty(yticks[0]), ty(yticks[-1])
    sx = lambda v: L + (tx(v) - x0) / (x1 - x0) * (W - L - R)
    sy = lambda v: B - (ty(v) - y0) / (y1 - y0) * (B - T)
    b = []
    for t in yticks:
        b.append(f'<line class="grid" x1="{L}" x2="{W - R}" y1="{sy(t):.1f}" y2="{sy(t):.1f}"/>')
        b.append(f'<text class="tick" x="{L - 8}" y="{sy(t) + 4:.1f}" text-anchor="end">{yfmt(t)}</text>')
    for t in xticks:
        b.append(f'<text class="tick" x="{sx(t):.1f}" y="{B + 18}" text-anchor="middle">{xfmt(t)}</text>')
    b.append(f'<line class="base" x1="{L}" x2="{W - R}" y1="{B}" y2="{B}"/>')
    b.append(f'<text class="axlab" x="{(L + W - R) / 2}" y="{B + 40}" text-anchor="middle">{xlab}</text>')
    b.append(f'<text class="axlab" transform="translate(18 {(T + B) / 2}) rotate(-90)" text-anchor="middle">{ylab}</text>')
    if ref:
        pts, text, (lx, ly) = ref
        b.append('<polyline class="ref" points="' + " ".join(f"{sx(x):.1f},{sy(y):.1f}" for x, y in pts) + '"/>')
        b.append(f'<text class="val" x="{sx(lx):.1f}" y="{sy(ly):.1f}">{text}</text>')
    ends = []
    for (name, ys), color in zip(series.items(), SLOTS):
        pts = " ".join(f"{sx(x):.1f},{sy(y):.1f}" for x, y in zip(xs, ys))
        b.append(f'<polyline points="{pts}" fill="none" stroke="{color}" stroke-width="2" stroke-linejoin="round"/>')
        for x, y in zip(xs, ys):
            b.append(
                f'<circle cx="{sx(x):.1f}" cy="{sy(y):.1f}" r="4" fill="{color}" stroke="var(--surface)" '
                f'stroke-width="2"><title>{name}, {xfmt(x)}: {yfmt(y)}</title></circle>'
            )
        ends.append([sy(ys[-1]), name, yfmt(ys[-1]), color])
    if end_labels:
        ends.sort()
        for i in range(1, len(ends)):
            ends[i][0] = max(ends[i][0], ends[i - 1][0] + 30)
        for y, name, val, color in ends:
            b.append(f'<rect x="{W - R + 12}" y="{y - 12:.1f}" width="10" height="10" rx="2" fill="{color}"/>')
            b.append(f'<text class="lab" x="{W - R + 28}" y="{y - 3:.1f}">{name}</text>')
            b.append(f'<text class="val" x="{W - R + 28}" y="{y + 11:.1f}">{val}</text>')
    b.append(legend(list(zip(series.keys(), SLOTS)), 24, 78))
    (OUT / fname).write_text(svg("\n".join(b) + "\n", title, sub, h))


def col_path(x, y0, w, y1):
    """Vertical bar with 4px rounded data end (top), square at the baseline."""
    r = min(4, w / 2, max(y0 - y1, 0))
    return (
        f"M{x:.1f},{y0:.1f} V{y1 + r:.1f} Q{x:.1f},{y1:.1f} {x + r:.1f},{y1:.1f} "
        f"H{x + w - r:.1f} Q{x + w:.1f},{y1:.1f} {x + w:.1f},{y1 + r:.1f} V{y0:.1f} Z"
    )


def bar_path(x0, y, x1, h):
    """Horizontal bar with 4px rounded data end (right), square at the baseline."""
    r = min(4, h / 2, max(x1 - x0, 0))
    return (
        f"M{x0:.1f},{y:.1f} H{x1 - r:.1f} Q{x1:.1f},{y:.1f} {x1:.1f},{y + r:.1f} "
        f"V{y + h - r:.1f} Q{x1:.1f},{y + h:.1f} {x1 - r:.1f},{y + h:.1f} H{x0:.1f} Z"
    )


def capacity():
    line_chart(
        "capacity.svg",
        "Invoices per second by invoice size",
        "HTTP service on a 4-core server. Larger invoices take proportionally longer.",
        SERVER_RPS, SERVER_SIZES, xlog=True, ylog=True,
        xticks=[5e3, 1e4, 1e5, 1e6, 2e6], yticks=[100, 1_000, 10_000, 100_000],
        xfmt=lambda v: fmt_bytes(int(v)) if v in (1e4, 1e5, 1e6) else "",
        yfmt=lambda v: f"{num(v)}/s",
        xlab="Invoice file size (log scale)", ylab="Invoices per second (log scale)",
    )


def scaling():
    speedup = {k: [v / vs[0] for v in vs] for k, vs in CORE_RPS.items()}
    line_chart(
        "scaling-cores.svg",
        "Throughput grows with CPU cores",
        "Speed-up vs. one core. Dashed line = perfect scaling. At 4 cores the load generator shared the machine.",
        speedup, CORES, xlog=False, ylog=False,
        xticks=[1, 2, 3, 4], yticks=[0, 1, 2, 3, 4],
        xfmt=lambda v: f"{v:g} core" + ("s" if v > 1 else ""),
        yfmt=lambda v: f"{v:.1f}×" if v % 1 else f"{v:g}×",
        xlab="CPU cores (workers)", ylab="Throughput vs. 1 core",
        ref=([(1, 1), (4, 4)], "perfect scaling", (3.05, 3.6)),
    )


def single_invoice_time():
    line_chart(
        "invoice-time.svg",
        "Time to convert one invoice",
        "End to end, command line, from a UBL invoice. Small invoices are dominated by the ~3 ms program start.",
        ONE_MS, SIZES, xlog=True, ylog=True,
        xticks=[5e3, 1e4, 1e5, 1e6, 1e7, 1e8, 1.5e8], yticks=[1, 10, 100, 1_000, 10_000],
        xfmt=lambda v: fmt_bytes(int(v)) if v in (1e4, 1e5, 1e6, 1e7, 1e8) else "",
        yfmt=lambda v: (f"{v / 1000:g} s" if v % 1000 == 0 else f"{v / 1000:.1f} s") if v >= 1000 else f"{v:,.0f} ms" if v >= 10 else f"{v:g} ms",
        xlab="Invoice file size (log scale)", ylab="Time (log scale)",
    )


def single_invoice_memory():
    line_chart(
        "invoice-memory.svg",
        "Memory needed for one invoice",
        "Peak memory, command line, from a UBL invoice. Dashed line = the invoice's own file size.",
        ONE_MB, SIZES, xlog=True, ylog=True,
        xticks=[5e3, 1e4, 1e5, 1e6, 1e7, 1e8, 1.5e8], yticks=[1, 10, 100, 1_000],
        xfmt=lambda v: fmt_bytes(int(v)) if v in (1e4, 1e5, 1e6, 1e7, 1e8) else "",
        yfmt=lambda v: f"{v / 1000:g} GB" if v >= 1000 else f"{v:,.0f} MB" if v >= 10 else f"{v:g} MB",
        xlab="Invoice file size (log scale)", ylab="Peak memory (log scale)",
        ref=([(1e6, 1), (1.5e8, 150)], "file size (1×)", (1.3e7, 8)),
    )


def concurrency_memory():
    L, R, T, h = 80, 32, 96, 380
    B = h - 60
    ymax = 200
    sy = lambda v: B - v / ymax * (B - T)
    b = []
    for t in range(0, ymax + 1, 50):
        b.append(f'<line class="grid" x1="{L}" x2="{W - R}" y1="{sy(t):.1f}" y2="{sy(t):.1f}"/>')
        b.append(f'<text class="tick" x="{L - 8}" y="{sy(t) + 4:.1f}" text-anchor="end">{t} MB</text>')
    b.append(f'<text class="axlab" transform="translate(18 {(T + B) / 2}) rotate(-90)" text-anchor="middle">Server peak memory</text>')
    slot = (W - L - R) / len(CONCURRENT)
    bw = 72
    for i, (n, mb) in enumerate(CONCURRENT):
        x = L + slot * (i + 0.5) - bw / 2
        b.append(
            f'<path d="{col_path(x, B, bw, sy(mb))}" fill="var(--s1)">'
            f"<title>{n} large invoices at once: {mb} MB</title></path>"
        )
        b.append(f'<text class="val" x="{x + bw / 2:.1f}" y="{sy(mb) - 8:.1f}" text-anchor="middle">{mb} MB</text>')
        b.append(f'<text class="lab" x="{x + bw / 2:.1f}" y="{B + 20}" text-anchor="middle">{n} at once</text>')
    b.append(f'<line class="base" x1="{L}" x2="{W - R}" y1="{B}" y2="{B}"/>')
    b.append(f'<text class="axlab" x="{(L + W - R) / 2}" y="{B + 44}" text-anchor="middle">Large (8.9 MB) invoices sent at the same time — 4 workers</text>')
    (OUT / "concurrency-memory.svg").write_text(
        svg("\n".join(b) + "\n",
            "Memory stays bounded under load",
            "Idle: 3 MB. Beyond 4 at once (one per worker) extra invoices wait in line instead of using more memory.",
            h)
    )


def format_memory():
    L, R, T, h = 250, 40, 116, 350
    vmax = 14
    sx = lambda v: L + v / vmax * (W - L - R)
    formats = list(next(iter(FORMAT_BLOWUP.values())).keys())
    b = []
    for t in range(0, vmax + 1, 2):
        b.append(f'<line class="grid" x1="{sx(t):.1f}" x2="{sx(t):.1f}" y1="{T - 6}" y2="{h - 56}"/>')
        b.append(f'<text class="tick" x="{sx(t):.1f}" y="{h - 40}" text-anchor="middle">{t}×</text>')
    b.append(f'<text class="axlab" x="{(L + W - R) / 2}" y="{h - 18}" text-anchor="middle">Peak memory per request ÷ invoice file size</text>')
    y, bh = T + 4, 18
    for fmt in formats:
        b.append(f'<text class="lab" x="{L - 12}" y="{y + bh + 5}" text-anchor="end">{fmt} input</text>')
        for (name, vals), color in zip(FORMAT_BLOWUP.items(), SLOTS):
            v = vals[fmt]
            b.append(
                f'<path d="{bar_path(sx(0), y, sx(v), bh)}" fill="{color}">'
                f"<title>{fmt} input, {name}: {v:g}×</title></path>"
            )
            # Surface-colored halo keeps the label legible over the reference line.
            b.append(
                f'<text class="val" x="{sx(v) + 6:.1f}" y="{y + 13}" stroke="var(--surface)" '
                f'stroke-width="4" paint-order="stroke">{v:g}×</text>'
            )
            y += bh + 2
        y += 18
    x = sx(BLOWUP_DEFAULT)
    b.append(f'<line class="ref" x1="{x:.1f}" x2="{x:.1f}" y1="{T - 6}" y2="{h - 56}"/>')
    b.append(f'<text class="val" x="{x:.1f}" y="{T - 12}" text-anchor="middle">default booking {BLOWUP_DEFAULT}×</text>')
    b.append(f'<line class="base" x1="{sx(0):.1f}" x2="{sx(0):.1f}" y1="{T - 6}" y2="{h - 56}"/>')
    b.append(legend(list(zip(FORMAT_BLOWUP.keys(), SLOTS)), 24, 78))
    (OUT / "format-memory.svg").write_text(
        svg("\n".join(b) + "\n",
            "Memory needed depends on the input format",
            "Worst output format, invoices of 1 MB and up. Compact FatturaPA expands the most.",
            h)
    )


def mixed_workload():
    L, R, T, h = 150, 40, 96, 300
    vmin, vmax = 0.1, 1000
    sx = lambda v: L + (math.log10(v) - math.log10(vmin)) / (math.log10(vmax) - math.log10(vmin)) * (W - L - R)
    b = []
    for t in [0.1, 1, 10, 100, 1000]:
        b.append(f'<line class="grid" x1="{sx(t):.1f}" x2="{sx(t):.1f}" y1="{T - 6}" y2="{h - 56}"/>')
        b.append(f'<text class="tick" x="{sx(t):.1f}" y="{h - 40}" text-anchor="middle">{t:g} ms</text>')
    b.append(f'<text class="axlab" x="{(L + W - R) / 2}" y="{h - 18}" text-anchor="middle">Response time for a typical (9 KB) invoice (log scale)</text>')
    y, bh = T + 4, 22
    for metric in ["Median", "Slowest 1%"]:
        b.append(f'<text class="lab" x="{L - 12}" y="{y + bh + 5}" text-anchor="end">{metric}</text>')
        for (name, vals), color in zip(HOL.items(), SLOTS):
            v = vals[metric]
            b.append(
                f'<path d="{bar_path(sx(vmin), y, sx(v), bh)}" fill="{color}">'
                f"<title>{name}, {metric}: {v:g} ms</title></path>"
            )
            b.append(f'<text class="val" x="{sx(v) + 6:.1f}" y="{y + 15}">{v:.1f} ms</text>')
            y += bh + 2
        y += 22
    b.append(f'<line class="base" x1="{sx(vmin):.1f}" x2="{sx(vmin):.1f}" y1="{T - 6}" y2="{h - 56}"/>')
    b.append(legend(list(zip(HOL.keys(), SLOTS)), 24, 78))
    (OUT / "mixed-workload.svg").write_text(
        svg("\n".join(b) + "\n",
            "Known limitation: large invoices delay small ones",
            "4-core server. Typical invoices alone vs. while eight 8.9 MB invoices are being converted.",
            h)
    )


if __name__ == "__main__":
    capacity()
    scaling()
    single_invoice_time()
    single_invoice_memory()
    concurrency_memory()
    format_memory()
    mixed_workload()
