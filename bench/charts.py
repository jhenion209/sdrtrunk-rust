#!/usr/bin/env python3
"""Draws the README's benchmark charts: CPU and memory over the 10 minutes, one line per program (its runs averaged
second by second), live and replay. Plain SVG that follows the reader's light or dark theme on GitHub.

    python3 bench/charts.py            # bench/results* -> bench/charts/{live,replay}-{cpu,memory}.svg
"""
import csv
import glob
import os
import sys

HERE = os.path.dirname(os.path.abspath(__file__))
PROGRAMS = [("sdrtrunk", "#2a78d6", "#3987e5"), ("sdrtrunk-vce", "#eb6834", "#d95926"), ("sdrtrunk-rust", "#1baf7a", "#199e70")]
SETS = [("live", "results"), ("replay", "results-replay")]
METRICS = [("cpu", "cpu_pct", "%", 10), ("memory", "rss_mb", " MB", 1)]
W, H, L, R, T, B = 760, 270, 64, 116, 18, 32


def average_runs(results, program, field):
    """Second-by-second mean of a program's runs (ROUND-PROGRAM.csv, round 0 = smoke test, skipped), cut to the
    shortest run."""
    runs = [[float(r[field]) for r in csv.DictReader(open(f))]
            for f in sorted(glob.glob(os.path.join(results, f"[0-9]*-{program}.csv")))
            if not os.path.basename(f).startswith("0-")]
    if not runs:
        raise SystemExit(f"{results}: no runs of {program}")
    n = min(map(len, runs))
    return [sum(r[i] for r in runs) / len(runs) for i in range(n)]


def rolling(ys, w):
    return [sum(ys[max(0, i - w + 1):i + 1]) / (i + 1 - max(0, i - w + 1)) for i in range(len(ys))]


def nice_top(peak):
    """Axis top and step: half, one, two or five times a power of ten, at most eight steps."""
    p = 10 ** (len(str(int(peak))) - 1) if peak >= 1 else 1
    for step in (p / 2, p, 2 * p, 5 * p):
        if peak / step <= 8:
            return -(-peak // step) * step, step
    return -(-peak // (10 * p)) * 10 * p, 10 * p


def label(v, unit):
    return (f"{v:,.0f}" if v >= 10 or v == int(v) else f"{v:.1f}") + unit


def svg(lines, unit, title):
    """lines: [(name, light colour, dark colour, values)]."""
    n = min(len(v) for *_, v in lines)
    top, step = nice_top(max(max(v[:n]) for *_, v in lines) * 1.05)
    iw, ih = W - L - R, H - T - B
    x = lambda i: L + i / (n - 1) * iw
    y = lambda v: T + ih - v / top * ih
    # Own background, same light/dark rule as the text, so it reads on any page theme.
    css = ["text{font:11px -apple-system,BlinkMacSystemFont,'Segoe UI',sans-serif;fill:#6e6d68}.bg{fill:#ffffff}",
           ".end{font-size:12px;font-weight:600;fill:#2b2b29}.grid{stroke:#e7e6e2}.axis{stroke:#c9c8c2}"]
    dark = ["text{fill:#9d9c94}.end{fill:#f0efec}.bg{fill:#0d1117}.grid{stroke:#2a2f36}.axis{stroke:#4a4f57}"]
    for k, (_, light, dk, _) in enumerate(lines):
        css.append(f".s{k}{{stroke:{light};fill:{light}}}")
        dark.append(f".s{k}{{stroke:{dk};fill:{dk}}}")
    out = [f'<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 {W} {H}" width="{W}" height="{H}" role="img">',
           f"<title>{title}</title>",
           "<style>" + "".join(css) + "@media (prefers-color-scheme:dark){" + "".join(dark) + "}</style>",
           f'<rect class="bg" width="{W}" height="{H}" rx="6"/>']
    v = 0.0
    while v <= top + 1e-9:
        out.append(f'<line class="{"axis" if v == 0 else "grid"}" x1="{L}" x2="{L + iw}" y1="{y(v):.1f}" y2="{y(v):.1f}"/>')
        out.append(f'<text x="{L - 8}" y="{y(v) + 4:.1f}" text-anchor="end">{label(v, unit)}</text>')
        v += step
    for t in range(0, 601, 120):
        out.append(f'<text x="{x(min(n - 1, t)):.1f}" y="{H - 8}" text-anchor="middle">{"0 s" if t == 0 else f"{t // 60} min"}</text>')
    for k, (*_, vals) in enumerate(lines):
        d = "".join(f'{"L" if i else "M"}{x(i):.1f},{y(val):.1f}' for i, val in enumerate(vals[:n]))
        out.append(f'<path class="s{k}" d="{d}" stroke-width="2" stroke-linejoin="round" style="fill:none"/>')
    # End labels, nudged apart so they never overlap.
    last = -1e9
    for ly, k, name in sorted((y(vals[n - 1]), k, name) for k, (name, *_, vals) in enumerate(lines)):
        ly = last = max(ly, last + 15)
        out.append(f'<circle class="s{k}" cx="{x(n - 1):.1f}" cy="{y(lines[k][3][n - 1]):.1f}" r="3"/>')
        out.append(f'<text class="end" x="{x(n - 1) + 8:.1f}" y="{ly + 4:.1f}">{name}</text>')
    out.append("</svg>")
    return "\n".join(out) + "\n"


def main(out_dir=os.path.join(HERE, "charts")):
    os.makedirs(out_dir, exist_ok=True)
    for set_name, results in SETS:
        for metric, field, unit, smooth in METRICS:
            lines = [(p, light, dark, rolling(average_runs(os.path.join(HERE, results), p, field), smooth))
                     for p, light, dark in PROGRAMS]
            path = os.path.join(out_dir, f"{set_name}-{metric}.svg")
            open(path, "w").write(svg(lines, unit, f"{set_name} {metric}"))
            print(path)


if __name__ == "__main__":
    sys.exit(main())
