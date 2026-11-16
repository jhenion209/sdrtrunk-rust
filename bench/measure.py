#!/usr/bin/env python3
"""Samples one process once a second: CPU (% of one core, all threads), memory (RSS) and the MP3s written to a
recordings folder since sampling started. Stops after `--seconds`, or early when the process ends.

    python3 bench/measure.py --pid PID --seconds 600 --recordings DIR --out run.csv

CSV columns: t_s, cpu_pct, rss_mb, mp3_files, audio_s (MP3 files and audio seconds are cumulative).
"""
import argparse
import csv
import os
import sys
import time

FIELDS = ["t_s", "cpu_pct", "rss_mb", "mp3_files", "audio_s"]
MP3_BITS_PER_SECOND = 16_000


def _after_comm(stat_line):
    """The fields after the command name, which may itself contain spaces and ')'."""
    return stat_line[stat_line.rindex(")") + 2:].split()


def cpu_ticks(stat_line):
    """utime + stime (clock ticks) from a /proc/<pid>/stat line: all threads of the process."""
    f = _after_comm(stat_line)
    return int(f[11]) + int(f[12])


def start_time(stat_line):
    """The process start time (clock ticks since boot): changes if the PID is reused."""
    return int(_after_comm(stat_line)[19])


def rss_mb(status_text):
    for line in status_text.splitlines():
        if line.startswith("VmRSS:"):
            return int(line.split()[1]) / 1024
    return 0.0


def audio_seconds(mp3_bytes):
    """Seconds of 16 kbps MP3 audio: the bytes after an ID3v2 tag."""
    skip = 0
    if mp3_bytes[:3] == b"ID3" and len(mp3_bytes) >= 10:
        size = 0
        for b in mp3_bytes[6:10]:
            size = (size << 7) | (b & 0x7F)
        skip = 10 + size + (10 if mp3_bytes[5] & 0x10 else 0)
    return max(0, len(mp3_bytes) - skip) * 8 / MP3_BITS_PER_SECOND


def _read(path):
    with open(path) as f:
        return f.read()


def _new_mp3s(folder, since):
    """(count, audio seconds) of MP3s under `folder` modified at or after `since`."""
    count, audio = 0, 0.0
    for root, _, files in os.walk(folder):
        for name in files:
            if not name.lower().endswith(".mp3"):
                continue
            path = os.path.join(root, name)
            try:
                if os.stat(path).st_mtime >= since:
                    with open(path, "rb") as f:
                        audio += audio_seconds(f.read())
                    count += 1
            except OSError:
                continue
    return count, audio


def sample(pid, seconds, recordings, out, interval=1.0):
    """Samples until `seconds` passed or the process ended; returns the rows (also written to `out`)."""
    tick = os.sysconf("SC_CLK_TCK")
    stat = f"/proc/{pid}/stat"
    started = start_time(_read(stat))
    wall0 = time.time()
    t0 = time.monotonic()
    last_t, last_ticks = t0, cpu_ticks(_read(stat))
    rows = []
    with open(out, "w", newline="") as f:
        w = csv.DictWriter(f, fieldnames=FIELDS)
        w.writeheader()
        while time.monotonic() - t0 < seconds:
            time.sleep(max(0.0, t0 + (len(rows) + 1) * interval - time.monotonic()))
            try:
                line = _read(stat)
                # Ended: a zombie (not yet collected by its parent) or a reused PID.
                if _after_comm(line)[0] in ("Z", "X") or start_time(line) != started:
                    break
                status = _read(f"/proc/{pid}/status")
            except (OSError, ValueError, IndexError):
                break
            now, ticks = time.monotonic(), cpu_ticks(line)
            files, audio = _new_mp3s(recordings, wall0)
            row = {"t_s": round(now - t0, 1), "cpu_pct": round((ticks - last_ticks) / tick / (now - last_t) * 100, 1),
                   "rss_mb": round(rss_mb(status), 1), "mp3_files": files, "audio_s": round(audio, 1)}
            last_t, last_ticks = now, ticks
            rows.append(row)
            w.writerow(row)
            f.flush()
    return rows


def main(argv=None):
    p = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    p.add_argument("--pid", type=int, required=True)
    p.add_argument("--seconds", type=float, default=600)
    p.add_argument("--recordings", required=True)
    p.add_argument("--out", required=True)
    a = p.parse_args(argv)
    rows = sample(a.pid, a.seconds, a.recordings, a.out)
    if not rows:
        print("no samples (process gone?)", file=sys.stderr)
        return 1
    mean = sum(r["cpu_pct"] for r in rows) / len(rows)
    print(f"{len(rows)} samples, CPU mean {mean:.1f}%, RSS max {max(r['rss_mb'] for r in rows):.0f} MB, "
          f"{rows[-1]['mp3_files']} MP3s / {rows[-1]['audio_s']:.0f} s audio")
    return 0


if __name__ == "__main__":
    sys.exit(main())
