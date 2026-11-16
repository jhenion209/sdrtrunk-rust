#!/usr/bin/env python3
"""Turns an sdrtrunk-rust database into an sdrtrunk playlist, so sdrtrunk and sdrtrunk-vce receive and record the same
systems and talkgroups as sdrtrunk-rust for the benchmark.

    python3 bench/make_config.py --db COPY_OF_sdrtrunk-rust.db --out ~/bench/config

Writes OUT/playlist/default.xml (enabled systems as channels, talkgroup lists as aliases; a talkgroup set to record
gets a `record` id, unlisted talkgroups are not recorded) and OUT/tuners.txt (each SDR's PPM and gain, to enter by
hand: sdrtrunk names tuners by USB details it finds at run time, and sdrtrunk-vce's playlist import ignores tuner files).
"""
import argparse
import json
import os
import sqlite3
import xml.etree.ElementTree as ET


def load(db_path):
    """Enabled systems, the talkgroups of their lists, and the SDR settings, from a sdrtrunk-rust database."""
    c = sqlite3.connect(f"file:{db_path}?mode=ro", uri=True)
    systems = []
    for _, j in c.execute("SELECT id, json FROM systems ORDER BY id"):
        s = json.loads(j)
        if s.get("enabled", True):
            s["list"] = s.get("alias_list", "").strip() or s["system_name"]
            systems.append(s)
    lists = sorted({s["list"] for s in systems})
    talkgroups = {l: [(n, name, bool(rec)) for n, name, rec in
                      c.execute("SELECT number, name, record FROM talkgroups WHERE system = ? ORDER BY number", (l,))]
                  for l in lists}
    tuners = {i: json.loads(j) for i, j in c.execute("SELECT id, json FROM tuners")}
    c.close()
    return {"systems": systems, "talkgroups": talkgroups, "tuners": tuners}


def tuner_name(tuner_id):
    """sdrtrunk's name for a tuner (TunerClass / TunerType labels): "Airspy SERIAL" or "RTL-2832/R820T SERIAL"."""
    if tuner_id.startswith("rtl:"):
        return "RTL-2832/R820T " + tuner_id[4:]
    return "Airspy " + tuner_id


def playlist_xml(cfg):
    root = ET.Element("playlist", version="4")
    for l, tgs in cfg["talkgroups"].items():
        for number, name, record in tgs:
            a = ET.SubElement(root, "alias", color="0", group="", name=name or str(number), list=l)
            ET.SubElement(a, "id", type="talkgroup", value=str(number), protocol="APCO25")
            if record:
                ET.SubElement(a, "id", type="record")
    for order, s in enumerate(cfg["systems"], 1):
        ch = ET.SubElement(root, "channel", name=s["channel_name"], system=s["system_name"], enabled="true",
                           site=s["site_name"], order=str(order))
        ET.SubElement(ch, "event_log_configuration")
        src = ET.SubElement(ch, "source_configuration", type="sourceConfigTunerMultipleFrequency",
                            frequency_rotation_delay="400", source_type="TUNER_MULTIPLE_FREQUENCIES")
        if s.get("tuner"):
            src.set("preferred_tuner", tuner_name(s["tuner"]))
        for hz in s["control_hz"]:
            ET.SubElement(src, "frequency").text = str(hz)
        ET.SubElement(ch, "alias_list_name").text = s["list"]
        ET.SubElement(ch, "aux_decode_configuration")
        ET.SubElement(ch, "decode_configuration", type="decodeConfigP25Phase1",
                      modulation="CQPSK" if s["modulation"] == "lsm" else "C4FM", ignore_data_calls="false",
                      traffic_channel_pool_size="20")
        ET.SubElement(ch, "record_configuration")
    ET.indent(root)
    return ET.tostring(root, encoding="unicode") + "\n"


def tuners_text(cfg):
    lines = ["Enter these in sdrtrunk and sdrtrunk-vce for the benchmark (same as sdrtrunk-rust):"]
    for tid, t in sorted(cfg["tuners"].items()):
        if tid.startswith("rtl:"):
            gain = "automatic gain" if t.get("rtl_gain_db") is None else f"gain {t['rtl_gain_db']} dB"
        else:
            gain = f"gain {t.get('gain', 14)} of 21 (linearity)"
        lines.append(f"{tuner_name(tid)}: PPM {t.get('ppm', 0.0)}, {gain}")
    return "\n".join(lines) + "\n"


def main(argv=None):
    p = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    p.add_argument("--db", required=True)
    p.add_argument("--out", required=True)
    a = p.parse_args(argv)
    cfg = load(a.db)
    os.makedirs(os.path.join(a.out, "playlist"), exist_ok=True)
    with open(os.path.join(a.out, "playlist", "default.xml"), "w") as f:
        f.write(playlist_xml(cfg))
    with open(os.path.join(a.out, "tuners.txt"), "w") as f:
        f.write(tuners_text(cfg))
    print(f"{len(cfg['systems'])} systems, {sum(len(t) for t in cfg['talkgroups'].values())} talkgroups -> {a.out}")


if __name__ == "__main__":
    main()
