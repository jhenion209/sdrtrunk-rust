#!/usr/bin/env python3
"""Points a copy of the sdrtrunk-rust database at baseband recordings for `serve --replay DIR`.

    python3 bench/replay_db.py --db BENCH_DB --replay DIR

Each enabled system is pinned to the recording made from its live tuner (`sdrtrunk-rust record` names it after the
tuner: "Airspy_SERIAL_..." or "RTL-SDR_SERIAL_..."). The recording's tuner gets the recorded centre (locked), the WAV's
rate and PPM 0 (the recordings are corrected with ppm_correct.py), with automatic PPM off.
"""
import argparse
import json
import os
import re
import sqlite3
import struct
import sys

NAME = re.compile(r".*_(\d+)_baseband_\d{8}_\d{6}\.wav", re.IGNORECASE)


def wav_rate(path):
    """Sample rate from a WAV's fmt chunk (walks the chunks)."""
    with open(path, "rb") as f:
        if f.read(12)[8:12] != b"WAVE":
            raise SystemExit(f"{path}: not a WAV file")
        while True:
            head = f.read(8)
            if len(head) < 8:
                raise SystemExit(f"{path}: no fmt chunk")
            cid, size = head[:4], struct.unpack("<I", head[4:])[0]
            if cid == b"fmt ":
                return struct.unpack("<HHI", f.read(8))[2]
            f.seek(size + (size & 1), 1)


def label_prefix(tuner):
    """The file-name prefix `record` gives a tuner's recordings."""
    return ("RTL-SDR_" + tuner[4:] if tuner.startswith("rtl:") else "Airspy_" + tuner) + "_"


def apply(db_path, replay_dir):
    """Pins every enabled system to its recording; returns [(system name, file tuner id)]."""
    wavs = sorted(n for n in os.listdir(replay_dir) if NAME.fullmatch(n))
    c = sqlite3.connect(db_path)
    tuners = {k: json.loads(j) for k, j in c.execute("SELECT id, json FROM tuners")}
    done = []
    for sid, j in c.execute("SELECT id, json FROM systems ORDER BY id").fetchall():
        s = json.loads(j)
        if not s.get("enabled"):
            continue
        live = s.get("tuner", "")
        mine = [n for n in wavs if n.startswith(label_prefix(live))]
        if not mine:
            raise SystemExit(f"{s.get('system_name')}: no recording for tuner {live or '(automatic)'} in {replay_dir}")
        name = mine[-1]
        fid = "file:" + name[:-4]
        cfg = dict(tuners.get(live, {}))
        cfg.update(mode="live", center_hz=int(NAME.fullmatch(name)[1]), rate_hz=wav_rate(os.path.join(replay_dir, name)),
                   ppm=0.0, auto_ppm=False, min_hz=None, max_hz=None)
        c.execute("INSERT OR REPLACE INTO tuners (id, json) VALUES (?, ?)", (fid, json.dumps(cfg)))
        s["tuner"] = fid
        c.execute("UPDATE systems SET json = ? WHERE id = ?", (json.dumps(s), sid))
        done.append((s.get("system_name"), fid))
    c.commit()
    c.close()
    return done


def main(argv=None):
    p = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    p.add_argument("--db", required=True)
    p.add_argument("--replay", required=True)
    a = p.parse_args(argv)
    for name, fid in apply(a.db, a.replay):
        print(f"{name}: {fid}")


if __name__ == "__main__":
    sys.exit(main())
