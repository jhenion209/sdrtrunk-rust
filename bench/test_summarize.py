"""Tests for summarize.py: run with `python3 -m unittest bench/test_summarize.py`."""
import os
import sys
import tempfile
import unittest

sys.path.insert(0, os.path.dirname(__file__))
import summarize  # noqa: E402


def write_run(d, round_, program, rows, mp3s, first="12"):
    with open(os.path.join(d, f"{round_}-{program}.csv"), "w") as f:
        f.write("t_s,cpu_pct,rss_mb,mp3_files,audio_s\n")
        for i in range(rows):
            cpu = 10.0 if i % 2 == 0 else 30.0  # mean 20, peak 30
            rss = 100.0 + i % 3  # mean 101, max 102
            files = mp3s * (i + 1) // rows
            f.write(f"{i + 1},{cpu},{rss},{files},{files * 4.0}\n")
    with open(os.path.join(d, f"{round_}-{program}.meta"), "w") as f:
        f.write(f"program={program}\nversion=v1\nstarted=2026-11-16T01:00:00-06:00\nfirst_mp3_s={first}\nexit=ok\n")


class Summarize(unittest.TestCase):
    def setUp(self):
        self.d = tempfile.mkdtemp()
        write_run(self.d, 1, "sdrtrunk-rust", 600, 30)
        write_run(self.d, 2, "sdrtrunk-rust", 300, 15)           # incomplete
        write_run(self.d, 1, "sdrtrunk", 600, 0, first="")       # no calls
        self.runs = summarize.load(self.d)

    def test_run_metrics(self):
        r = next(r for r in self.runs if r["round"] == 1 and r["program"] == "sdrtrunk-rust")
        self.assertEqual((r["samples"], r["cpu_mean"], r["cpu_max"], r["rss_mean"], r["rss_max"]),
                         (600, 20.0, 30.0, 101.0, 102.0))
        self.assertEqual((r["mp3_per_min"], r["audio_s_per_min"], r["first_mp3_s"]), (3.0, 12.0, 12.0))
        self.assertEqual(r["note"], "")

    def test_flags(self):
        notes = {(r["round"], r["program"]): r["note"] for r in self.runs}
        self.assertEqual(notes[(2, "sdrtrunk-rust")], "incomplete (300 of 600 samples)")
        self.assertEqual(notes[(1, "sdrtrunk")], "no calls")

    def test_table_excludes_flagged_runs(self):
        md = summarize.markdown(self.runs)
        rust = next(l for l in md.splitlines() if l.startswith("| sdrtrunk-rust |"))
        self.assertIn("| 1 of 2 |", rust, "only the full run counts")
        self.assertIn("20.0", rust)
        java = next(l for l in md.splitlines() if l.startswith("| sdrtrunk |"))
        self.assertIn("| 0 of 1 |", java)
        self.assertIn("incomplete (300 of 600 samples)", md)


if __name__ == "__main__":
    unittest.main()
