#!/usr/bin/env python3
"""Summarizes the benchmark runs in bench/results (ROUND-PROGRAM.csv + .meta) as a Markdown table.

    python3 bench/summarize.py [--results bench/results] [--expected 600] > bench/results/summary.md

A run counts toward the table unless it is incomplete (under 95% of the expected samples) or recorded no calls;
flagged runs are listed under the table with the reason.
"""
import argparse
import csv
import glob
import os
import re
import sys

PROGRAMS = ["sdrtrunk", "sdrtrunk-vce", "sdrtrunk-rust"]


def _mean(xs):
    return round(sum(xs) / len(xs), 1) if xs else 0.0


def _meta(path):
    out = {}
    with open(path) as f:
        for line in f:
            k, _, v = line.rstrip("\n").partition("=")
            out[k] = v
    return out


def load(results, expected=600):
    runs = []
    for path in sorted(glob.glob(os.path.join(results, "*.csv"))):
        m = re.fullmatch(r"(\d+)-(.+)\.csv", os.path.basename(path))
        if not m:
            continue
        with open(path) as f:
            rows = [{k: float(v) for k, v in r.items()} for r in csv.DictReader(f)]
        meta_path = path[:-4] + ".meta"
        meta = _meta(meta_path) if os.path.exists(meta_path) else {}
        n = len(rows)
        minutes = n / 60 or 1
        last = rows[-1] if rows else {"mp3_files": 0, "audio_s": 0}
        r = {"round": int(m[1]), "program": m[2], "samples": n, "version": meta.get("version", ""),
             "cpu_mean": _mean([x["cpu_pct"] for x in rows]), "cpu_max": max((x["cpu_pct"] for x in rows), default=0.0),
             "rss_mean": _mean([x["rss_mb"] for x in rows]), "rss_max": max((x["rss_mb"] for x in rows), default=0.0),
             "mp3_per_min": round(last["mp3_files"] / minutes, 1), "audio_s_per_min": round(last["audio_s"] / minutes, 1),
             "first_mp3_s": float(meta["first_mp3_s"]) if meta.get("first_mp3_s") else None}
        if n < 0.95 * expected:
            r["note"] = f"incomplete ({n} of {expected} samples)"
        elif last["mp3_files"] == 0:
            r["note"] = "no calls"
        else:
            r["note"] = ""
        runs.append(r)
    return runs


def markdown(runs):
    lines = ["| Program | Runs used | CPU mean % | CPU peak % | RSS mean MB | RSS peak MB | MP3s / min | Audio s / min |",
             "|---|---|---|---|---|---|---|---|"]
    for p in PROGRAMS + sorted({r["program"] for r in runs} - set(PROGRAMS)):
        mine = [r for r in runs if r["program"] == p]
        if not mine:
            continue
        ok = [r for r in mine if not r["note"]]
        if ok:
            cells = [_mean([r[k] for r in ok]) for k in
                     ("cpu_mean", "cpu_max", "rss_mean", "rss_max", "mp3_per_min", "audio_s_per_min")]
            cells = [f"{c:.1f}" for c in cells]
        else:
            cells = ["-"] * 6
        lines.append(f"| {p} | {len(ok)} of {len(mine)} | " + " | ".join(cells) + " |")
    flagged = [r for r in runs if r["note"]]
    if flagged:
        lines += ["", "Not counted:"]
        lines += [f"- round {r['round']} {r['program']}: {r['note']}" for r in flagged]
    return "\n".join(lines) + "\n"


def main(argv=None):
    p = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    p.add_argument("--results", default=os.path.join(os.path.dirname(__file__), "results"))
    p.add_argument("--expected", type=int, default=600)
    a = p.parse_args(argv)
    sys.stdout.write(markdown(load(a.results, a.expected)))


if __name__ == "__main__":
    main()
