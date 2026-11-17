import os
import struct
import tempfile
import unittest

import numpy as np

import ppm_correct

RATE = 48_000


def write_wav(path, iq, rate=RATE):
    data = np.asarray(iq, dtype="<i2").tobytes()
    with open(path, "wb") as f:
        f.write(b"RIFF" + struct.pack("<I", 36 + len(data)) + b"WAVEfmt " + struct.pack("<IHHIIHH", 16, 1, 2, rate, rate * 4, 4, 16))
        f.write(b"data" + struct.pack("<I", len(data)) + data)


def tone(hz, n, amp=10_000):
    t = np.arange(n) / RATE
    z = amp * np.exp(2j * np.pi * hz * t)
    iq = np.empty(2 * n)
    iq[0::2], iq[1::2] = z.real, z.imag
    return np.round(iq)


def peak_hz(path):
    with open(path, "rb") as f:
        raw = f.read()[44:]
    s = np.frombuffer(raw, dtype="<i2").astype(float)
    z = s[0::2] + 1j * s[1::2]
    spec = np.abs(np.fft.fft(z))
    return np.fft.fftfreq(len(z), 1 / RATE)[np.argmax(spec)]


class PpmCorrectTest(unittest.TestCase):
    def setUp(self):
        self.d = tempfile.mkdtemp()
        self.src = os.path.join(self.d, "Airspy_X_850000000_baseband_20261116_120000.wav")

    def test_moves_the_band_by_ppm_times_centre(self):
        # A tuner at -4 ppm puts a channel ppm*f = 3400 Hz high: 1000 Hz shows up at 4400 Hz.
        write_wav(self.src, tone(4_400, RATE))
        out = os.path.join(self.d, "out.wav")
        ppm_correct.correct(self.src, out, -4.0)
        self.assertAlmostEqual(peak_hz(out), 1_000, delta=2)

    def test_chunks_join_without_a_phase_step(self):
        write_wav(self.src, tone(4_400, 5_000))
        whole, parts = os.path.join(self.d, "a.wav"), os.path.join(self.d, "b.wav")
        ppm_correct.correct(self.src, whole, -4.0)
        ppm_correct.correct(self.src, parts, -4.0, chunk=777)
        a, b = open(whole, "rb").read(), open(parts, "rb").read()
        self.assertEqual(a[:44], b[:44])
        diff = np.abs(np.frombuffer(a[44:], "<i2").astype(int) - np.frombuffer(b[44:], "<i2").astype(int))
        self.assertLessEqual(diff.max(), 1)

    def test_header_and_length_kept(self):
        write_wav(self.src, tone(0, 1_000))
        out = os.path.join(self.d, "out.wav")
        ppm_correct.correct(self.src, out, 1.0)
        self.assertEqual(os.path.getsize(out), os.path.getsize(self.src))
        self.assertEqual(open(out, "rb").read()[:44], open(self.src, "rb").read()[:44])

    def test_full_scale_clips_instead_of_wrapping(self):
        write_wav(self.src, np.full(2_000, 32_767))
        out = os.path.join(self.d, "out.wav")
        ppm_correct.correct(self.src, out, 5.0)
        s = np.frombuffer(open(out, "rb").read()[44:], "<i2").astype(int)
        self.assertGreater(np.abs(s).max(), 30_000, "no wrap to small or opposite values")
        self.assertTrue((s >= -32_768).all() and (s <= 32_767).all())


if __name__ == "__main__":
    unittest.main()
