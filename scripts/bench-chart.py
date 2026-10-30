#!/usr/bin/env python3
"""Draw the README benchmark chart from scripts/bench-agents.sh output.

Usage: scripts/bench-chart.py <results.csv> <out-prefix>

Writes <out-prefix>-light.svg and <out-prefix>-dark.svg: for every agent count,
the wall time until every agent finished, without and with Sluice (grouped
columns, the reduction above each group). Several rows for one agent count and
scenario are reduced to their median. Standard library only.
"""

import csv
import statistics
import sys
from xml.sax.saxutils import escape

# Validated with the dataviz palette checker against GitHub's page surfaces
# (#ffffff light, #0d1117 dark): categorical slots 1 and 2 of the reference
# palette, stepped per mode.
THEMES = {
    "light": {
        "with": "#2a78d6",
        "without": "#eb6834",
        "text": "#1f2328",
        "muted": "#59636e",
        "grid": "#d1d9e0",
    },
    "dark": {
        "with": "#3987e5",
        "without": "#d95926",
        "text": "#f0f6fc",
        "muted": "#9198a1",
        "grid": "#3d444d",
    },
}

W, H = 720, 380
LEFT, RIGHT, TOP, BOTTOM = 56, 24, 124, 48
BAR, GAP = 24, 2
FONT = "-apple-system,BlinkMacSystemFont,'Segoe UI',Helvetica,Arial,sans-serif"


def load(path):
    rows = {}
    meta = None
    with open(path, newline="") as f:
        for r in csv.DictReader(f):
            key = (int(r["agents"]), r["scenario"])
            rows.setdefault(key, []).append(int(r["wall_ms"]) / 1000)
            meta = (r["cpus"], r["mem_gib"])
    agents = sorted({a for a, _ in rows})
    data = []
    for a in agents:
        if (a, "with") in rows and (a, "without") in rows:
            data.append(
                (a, statistics.median(rows[(a, "without")]), statistics.median(rows[(a, "with")]))
            )
    return data, meta


def nice_max(v):
    for step in (0.5, 1, 2, 5, 10, 15, 20, 30, 60, 120, 300, 600):
        if v / step <= 5:
            return step * -(-v // step), step
    return v, v / 5


def column(x, y0, h, color):
    """A column `h` high from baseline `y0`, 4px rounded top, square base."""
    r = min(4, h)
    top = y0 - h
    return (
        f'<path d="M{x},{y0} V{top + r} Q{x},{top} {x + r},{top} H{x + BAR - r} '
        f'Q{x + BAR},{top} {x + BAR},{top + r} V{y0} Z" fill="{color}"/>'
    )


def svg(data, meta, t):
    # Seconds below five minutes, minutes above.
    if max(w for _, w, _ in data) >= 300:
        data = [(a, wo / 60, wi / 60) for a, wo, wi in data]
        unit, fmt = "min", "{:.1f}"
    else:
        unit, fmt = "s", "{:.0f}"
    ymax, step = nice_max(max(w for _, w, _ in data) * 1.05)
    plot_h = H - TOP - BOTTOM
    plot_w = W - LEFT - RIGHT
    y0 = TOP + plot_h
    sy = plot_h / ymax
    band = plot_w / len(data)
    cpus, mem = meta
    out = [
        f'<svg xmlns="http://www.w3.org/2000/svg" width="{W}" height="{H}" '
        f'viewBox="0 0 {W} {H}" role="img" aria-labelledby="t d" font-family="{FONT}">',
        '<title id="t">Time until every agent has committed, without and with Sluice</title>',
        '<desc id="d">'
        + escape(
            "; ".join(
                f"{a} agent(s): {fmt.format(wo)} {unit} without Sluice, "
                f"{fmt.format(wi)} {unit} with Sluice"
                for a, wo, wi in data
            )
        )
        + "</desc>",
        f'<text x="{LEFT}" y="28" font-size="17" font-weight="600" fill="{t["text"]}">'
        "Time until every agent has committed</text>",
        f'<text x="{LEFT}" y="50" font-size="13" fill="{t["muted"]}">'
        "Each agent makes 2 commits in its own worktree, running this repository's "
        "pre-commit gates (fmt, clippy).</text>",
        f'<text x="{LEFT}" y="68" font-size="13" fill="{t["muted"]}">'
        f"GitHub-hosted runner, {cpus} CPUs, {mem} GiB of RAM; median of the runs. Lower is better.</text>",
    ]
    # Legend: Sluice first (slot 1), then the baseline (slot 2).
    lx = LEFT
    for key, label in (("with", "With Sluice"), ("without", "Without Sluice")):
        out.append(f'<rect x="{lx}" y="86" width="12" height="12" rx="2" fill="{t[key]}"/>')
        out.append(
            f'<text x="{lx + 18}" y="96.5" font-size="13" fill="{t["text"]}">{label}</text>'
        )
        lx += 18 + len(label) * 7.4 + 24
    # Recessive grid and the y axis in minutes.
    v = 0.0
    while v <= ymax + 1e-9:
        y = y0 - v * sy
        out.append(
            f'<line x1="{LEFT}" x2="{W - RIGHT}" y1="{y:.1f}" y2="{y:.1f}" '
            f'stroke="{t["grid"]}" stroke-width="1"/>'
        )
        label = f"{v:g} {unit}" if v else "0"
        out.append(
            f'<text x="{LEFT - 8}" y="{y + 4:.1f}" font-size="12" text-anchor="end" '
            f'fill="{t["muted"]}">{label}</text>'
        )
        v += step
    for i, (a, wo, wi) in enumerate(data):
        cx = LEFT + band * (i + 0.5)
        x_with = cx - BAR - GAP / 2
        x_without = cx + GAP / 2
        for x, val, key in ((x_with, wi, "with"), (x_without, wo, "without")):
            h = val * sy
            out.append(column(round(x, 1), y0, h, t[key]))
            out.append(
                f'<text x="{x + BAR / 2:.1f}" y="{y0 - h - 6:.1f}" font-size="12" '
                f'text-anchor="middle" fill="{t["text"]}">{fmt.format(val)}</text>'
            )
        top = y0 - max(wo, wi) * sy - 26
        change = (wi - wo) / wo * 100 if wo else 0
        sign = "−" if change < 0 else "+"
        out.append(
            f'<text x="{cx:.1f}" y="{top:.1f}" font-size="13" font-weight="600" '
            f'text-anchor="middle" fill="{t["text"]}">{sign}{abs(change):.0f} %</text>'
        )
        noun = "agent" if a == 1 else "agents"
        out.append(
            f'<text x="{cx:.1f}" y="{y0 + 22}" font-size="13" text-anchor="middle" '
            f'fill="{t["text"]}">{a} {noun}</text>'
        )
    out.append("</svg>")
    return "\n".join(out) + "\n"


def main():
    if len(sys.argv) != 3:
        sys.exit("usage: bench-chart.py <results.csv> <out-prefix>")
    data, meta = load(sys.argv[1])
    if not data:
        sys.exit("no agent count has both scenarios")
    for mode, theme in THEMES.items():
        with open(f"{sys.argv[2]}-{mode}.svg", "w") as f:
            f.write(svg(data, meta, theme))


if __name__ == "__main__":
    main()
