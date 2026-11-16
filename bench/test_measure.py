"""Tests for measure.py: run with `python3 -m unittest bench/test_measure.py` (the sampler test needs Linux /proc)."""
import os
import subprocess
import sys
import tempfile
import unittest

sys.path.insert(0, os.path.dirname(__file__))
import measure  # noqa: E402

# A /proc/<pid>/stat line whose command name contains a space and a ")": fields after it are counted from the last ")".
STAT = "1234 (java (x) y) S 1 1234 1234 0 -1 4194560 100 0 0 0 250 75 0 0 20 0 42 0 98765 1000 500"


class Parse(unittest.TestCase):
    def test_cpu_ticks_and_start(self):
        self.assertEqual(measure.cpu_ticks(STAT), 250 + 75)
        self.assertEqual(measure.start_time(STAT), 98765)

    def test_rss(self):
        self.assertEqual(measure.rss_mb("Name:\tjava\nVmRSS:\t  524288 kB\nThreads:\t42\n"), 512.0)
        self.assertEqual(measure.rss_mb("Name:\tzombie\n"), 0.0)

    def test_audio_seconds(self):
        id3 = b"ID3\x04\x00\x00\x00\x00\x00\x0a" + b"\x00" * 10  # 10-byte header, tag body of 10 bytes
        self.assertAlmostEqual(measure.audio_seconds(id3 + b"\xff" * 2000), 1.0)
        self.assertAlmostEqual(measure.audio_seconds(b"\xff" * 4000), 2.0, msg="no tag")


@unittest.skipUnless(os.path.isdir("/proc/self"), "needs Linux /proc")
class Sample(unittest.TestCase):
    def test_stops_when_the_process_ends(self):
        child = subprocess.Popen([sys.executable, "-c", "import time; time.sleep(2)"])
        d = tempfile.mkdtemp()
        out = os.path.join(d, "run.csv")
        rows = measure.sample(child.pid, seconds=30, recordings=d, out=out, interval=1.0)
        child.wait()
        self.assertLessEqual(len(rows), 3)
        with open(out) as f:
            self.assertEqual(f.readline().strip(), "t_s,cpu_pct,rss_mb,mp3_files,audio_s")

    def test_counts_only_new_mp3s(self):
        d = tempfile.mkdtemp()
        with open(os.path.join(d, "old.mp3"), "wb") as f:
            f.write(b"\xff" * 2000)
        child = subprocess.Popen([sys.executable, "-c",
                                  f"import time; time.sleep(1.5); open({os.path.join(d, 'new.mp3')!r}, 'wb').write(b'\\xff' * 4000); time.sleep(1.5)"])
        rows = measure.sample(child.pid, seconds=30, recordings=d, out=os.path.join(d, "r.csv"), interval=1.0)
        child.wait()
        self.assertEqual((rows[-1]["mp3_files"], round(rows[-1]["audio_s"], 1)), (1, 2.0))


if __name__ == "__main__":
    unittest.main()
