"""Tests for replay_db.py: run with `python3 -m unittest bench/test_replay_db.py`."""
import json
import os
import sqlite3
import struct
import sys
import tempfile
import unittest

sys.path.insert(0, os.path.dirname(__file__))
import replay_db  # noqa: E402

AIRSPY, RTL = "35AC63DC-2D40B04F", "rtl:00000001"
A_WAV = "Airspy_35AC63DC-2D40B04F_851012500_baseband_20261116_120000.wav"
R_WAV = "RTL-SDR_00000001_769500000_baseband_20261116_120000.wav"


def wav(path, rate):
    """A 16-bit stereo WAV header with a LIST chunk before the data, and 8 bytes of samples."""
    fmt = struct.pack("<4sIHHIIHH", b"fmt ", 16, 1, 2, rate, rate * 4, 4, 16)
    body = b"WAVE" + fmt + b"LIST" + struct.pack("<I", 4) + b"abcd" + b"data" + struct.pack("<I", 8) + bytes(8)
    with open(path, "wb") as f:
        f.write(b"RIFF" + struct.pack("<I", len(body)) + body)


class ReplayDb(unittest.TestCase):
    def setUp(self):
        d = tempfile.mkdtemp()
        self.db, self.rec = os.path.join(d, "bench.db"), os.path.join(d, "rec")
        os.mkdir(self.rec)
        wav(os.path.join(self.rec, A_WAV), 6_000_000)
        wav(os.path.join(self.rec, R_WAV), 2_400_000)
        open(os.path.join(self.rec, "notes.txt"), "w").close()
        c = sqlite3.connect(self.db)
        c.execute("CREATE TABLE systems (id INTEGER PRIMARY KEY, json TEXT NOT NULL)")
        c.execute("CREATE TABLE tuners (id TEXT PRIMARY KEY, json TEXT NOT NULL)")
        for i, (name, tuner) in enumerate([("SGCo", AIRSPY), ("KSICS", RTL)], 1):
            c.execute("INSERT INTO systems VALUES (?, ?)", (i, json.dumps({"enabled": True, "tuner": tuner, "system_name": name})))
        for tid, ppm in [(AIRSPY, -4.2), (RTL, 1.5)]:
            c.execute("INSERT INTO tuners VALUES (?, ?)", (tid, json.dumps(
                {"mode": "live", "center_hz": None, "ppm": ppm, "auto_ppm": True, "gain": 14, "rate_hz": 6_000_000})))
        c.commit()
        c.close()

    def rows(self, table):
        c = sqlite3.connect(self.db)
        out = {k: json.loads(j) for k, j in c.execute(f"SELECT id, json FROM {table}")}
        c.close()
        return out

    def test_pins_systems_to_their_recordings(self):
        self.assertEqual(replay_db.apply(self.db, self.rec),
                         [("SGCo", "file:" + A_WAV[:-4]), ("KSICS", "file:" + R_WAV[:-4])])
        systems = self.rows("systems")
        self.assertEqual((systems[1]["tuner"], systems[2]["tuner"]), ("file:" + A_WAV[:-4], "file:" + R_WAV[:-4]))
        self.assertEqual(systems[1]["system_name"], "SGCo", "other fields kept")

    def test_file_tuners_take_centre_rate_and_no_ppm(self):
        # The recordings are PPM-corrected by ppm_correct.py, so every program plays them at PPM 0.
        replay_db.apply(self.db, self.rec)
        t = self.rows("tuners")
        a, r = t["file:" + A_WAV[:-4]], t["file:" + R_WAV[:-4]]
        self.assertEqual((a["center_hz"], a["rate_hz"], a["ppm"], a["auto_ppm"], a["mode"]), (851_012_500, 6_000_000, 0.0, False, "live"))
        self.assertEqual((r["center_hz"], r["rate_hz"], r["ppm"], r["auto_ppm"]), (769_500_000, 2_400_000, 0.0, False))

    def test_a_system_without_a_recording_is_an_error(self):
        os.remove(os.path.join(self.rec, R_WAV))
        with self.assertRaisesRegex(SystemExit, "KSICS"):
            replay_db.apply(self.db, self.rec)


if __name__ == "__main__":
    unittest.main()
