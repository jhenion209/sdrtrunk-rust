#!/usr/bin/env python3
"""Removes a tuner's PPM error from a baseband recording, so every program plays it at PPM 0.

    python3 bench/ppm_correct.py --ppm P IN.wav OUT.wav

A tuner off by P ppm puts a channel at f where f - P*f/1e6 is expected (sdrtrunk-rust's `decode_hz`); the recording is
shifted by P * centre / 1e6 Hz. Over one tuner's band that term changes by a few hertz only, so one shift fits all.
The header is copied as is; the centre comes from the file name (`..._<centreHz>_baseband_...wav`).
"""
import argparse
import os
import re
import struct
import sys

import numpy as np

NAME = re.compile(r".*_(\d+)_baseband_\d{8}_\d{6}\.wav", re.IGNORECASE)


def data_chunk(f):
    """(sample rate, data start, data length) from a 16-bit stereo PCM WAV."""
    if f.read(12)[8:12] != b"WAVE":
        raise SystemExit("not a WAV file")
    rate = None
    while True:
        head = f.read(8)
        if len(head) < 8:
            raise SystemExit("no data chunk")
        cid, size = head[:4], struct.unpack("<I", head[4:])[0]
        if cid == b"fmt ":
            fmt, ch, rate, _, _, bits = struct.unpack("<HHIIHH", f.read(16))
            if (fmt, ch, bits) != (1, 2, 16):
                raise SystemExit("need 16-bit stereo PCM I/Q")
            f.seek(size - 16 + (size & 1), 1)
        elif cid == b"data":
            if rate is None:
                raise SystemExit("data chunk before fmt chunk")
            start = f.tell()
            rest = os.fstat(f.fileno()).st_size - start
            return rate, start, (rest if size == 0 or size > rest else size) // 4 * 4
        else:
            f.seek(size + (size & 1), 1)


def correct(src, out, ppm, chunk=1 << 22):
    """Writes `src` shifted by ppm * centre / 1e6 Hz to `out`; `chunk` is complex samples per step."""
    m = NAME.fullmatch(os.path.basename(src))
    if not m:
        raise SystemExit(f"{src}: no centre in the name")
    shift = ppm * int(m[1]) / 1e6
    with open(src, "rb") as f, open(out, "wb") as o:
        rate, start, length = data_chunk(f)
        f.seek(0)
        o.write(f.read(start))
        step = shift / rate  # cycles per sample
        n = 0
        while n * 4 < length:
            raw = f.read(min(chunk * 4, length - n * 4))
            s = np.frombuffer(raw, dtype="<i2").astype(np.float32)
            z = s[0::2] + 1j * s[1::2]
            # Phase from the absolute sample index, folded to one cycle, so chunks join exactly.
            cycles = (step * n) % 1.0 + step * np.arange(len(z))
            z = z * np.exp(2j * np.pi * cycles).astype(np.complex64)
            iq = np.empty(2 * len(z), dtype=np.float32)
            iq[0::2], iq[1::2] = z.real, z.imag
            o.write(np.clip(np.rint(iq), -32768, 32767).astype("<i2").tobytes())
            n += len(z)
        o.write(f.read())  # anything after the data chunk
    return shift


def main(argv=None):
    p = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    p.add_argument("--ppm", type=float, required=True)
    p.add_argument("src")
    p.add_argument("out")
    a = p.parse_args(argv)
    print(f"{a.out}: shifted {correct(a.src, a.out, a.ppm):+.1f} Hz")


if __name__ == "__main__":
    sys.exit(main())
