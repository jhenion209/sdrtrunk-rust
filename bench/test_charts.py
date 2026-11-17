import os
import tempfile
import unittest

import charts


class Charts(unittest.TestCase):
    def test_runs_averaged_per_second_skipping_smoke_test(self):
        with tempfile.TemporaryDirectory() as d:
            for name, cpu in [("0-x", [999, 999, 999]), ("1-x", [10, 20, 30]), ("2-x", [30, 40])]:
                with open(os.path.join(d, name + ".csv"), "w") as f:
                    f.write("t_s,cpu_pct,rss_mb,mp3_files,audio_s\n")
                    f.writelines(f"{i},{c},1,0,0\n" for i, c in enumerate(cpu))
            self.assertEqual(charts.average_runs(d, "x", "cpu_pct"), [20, 30])

    def test_axis_top_covers_the_peak(self):
        for peak, top in [(84, 100), (280, 300), (2624, 3000), (23.4, 25)]:
            got, step = charts.nice_top(peak)
            self.assertEqual(got, top, peak)
            self.assertLessEqual(got / step, 8)

    def test_one_line_and_label_per_program_and_axis_titles(self):
        s = charts.svg([("a", "#000", "#fff", [1, 2, 3]), ("b", "#111", "#eee", [3, 2, 1])], "%", "t", "CPU (%)")
        self.assertEqual(s.count("<path"), 2)
        self.assertIn(">a</text>", s)
        self.assertIn(">CPU (%)</text>", s)
        self.assertIn(">Time since measuring started</text>", s)
        self.assertIn("prefers-color-scheme:dark", s)


if __name__ == "__main__":
    unittest.main()
