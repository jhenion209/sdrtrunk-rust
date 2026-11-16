"""Tests for make_config.py: run with `python3 -m unittest bench/test_make_config.py`."""
import json
import os
import sqlite3
import sys
import tempfile
import unittest
import xml.etree.ElementTree as ET

sys.path.insert(0, os.path.dirname(__file__))
import make_config  # noqa: E402


def make_db(path):
    c = sqlite3.connect(path)
    c.executescript("""
        CREATE TABLE systems (id INTEGER PRIMARY KEY, json TEXT NOT NULL);
        CREATE TABLE talkgroups (system TEXT NOT NULL, number INTEGER NOT NULL, name TEXT NOT NULL, record INTEGER NOT NULL);
        CREATE TABLE tuners (id TEXT PRIMARY KEY, json TEXT NOT NULL);
    """)
    sgco = {"enabled": True, "tuner": "35AC63DC-2D40B04F", "control_hz": [858962500], "modulation": "lsm",
            "system_name": "SGCo", "site_name": "SGCo-P25", "channel_name": "T-SGCo-Simulcast", "alias_list": ""}
    ksics = {"enabled": True, "tuner": "rtl:00000001", "control_hz": [852112500, 852412500], "modulation": "c4fm",
             "system_name": "KSICS", "site_name": "KSICS-Sedgwick", "channel_name": "T-KSICS-Wichita", "alias_list": ""}
    off = dict(sgco, enabled=False, system_name="Off", channel_name="Off")
    for s in (sgco, ksics, off):
        c.execute("INSERT INTO systems (json) VALUES (?)", (json.dumps(s),))
    c.execute("INSERT INTO talkgroups VALUES ('SGCo', 23, 'WPD West', 1)")
    c.execute("INSERT INTO talkgroups VALUES ('SGCo', 85, 'Fire & EMS', 0)")
    c.execute("INSERT INTO tuners VALUES ('35AC63DC-2D40B04F', ?)", (json.dumps({"ppm": -3.9, "gain": 14}),))
    c.execute("INSERT INTO tuners VALUES ('rtl:00000001', ?)", (json.dumps({"ppm": 1.5, "rtl_gain_db": 32.8}),))
    c.commit()
    c.close()


class MakeConfig(unittest.TestCase):
    def setUp(self):
        self.dir = tempfile.mkdtemp()
        self.db = os.path.join(self.dir, "x.db")
        make_db(self.db)
        self.cfg = make_config.load(self.db)

    def test_playlist(self):
        root = ET.fromstring(make_config.playlist_xml(self.cfg))
        self.assertEqual(root.tag, "playlist")
        channels = root.findall("channel")
        self.assertEqual([c.get("name") for c in channels], ["T-SGCo-Simulcast", "T-KSICS-Wichita"], "enabled only")
        self.assertTrue(all(c.get("enabled") == "true" for c in channels))
        sg, ks = channels
        self.assertEqual([f.text for f in sg.iter("frequency")], ["858962500"])
        self.assertEqual([f.text for f in ks.iter("frequency")], ["852112500", "852412500"])
        self.assertEqual(sg.find("decode_configuration").get("modulation"), "CQPSK")
        self.assertEqual(ks.find("decode_configuration").get("modulation"), "C4FM")
        self.assertEqual(sg.find("source_configuration").get("preferred_tuner"), "Airspy 35AC63DC-2D40B04F")
        self.assertEqual(ks.find("source_configuration").get("preferred_tuner"), "RTL-2832/R820T 00000001")
        self.assertEqual(sg.findtext("alias_list_name"), "SGCo")
        aliases = {a.get("name"): a for a in root.findall("alias")}
        self.assertEqual(aliases["WPD West"].get("list"), "SGCo")
        ids = lambda a: [(i.get("type"), i.get("value")) for i in a.findall("id")]
        self.assertIn(("talkgroup", "23"), ids(aliases["WPD West"]))
        self.assertIn(("record", None), ids(aliases["WPD West"]))
        self.assertNotIn(("record", None), ids(aliases["Fire & EMS"]), "not recorded")

    def test_tuners_text(self):
        text = make_config.tuners_text(self.cfg)
        self.assertIn("Airspy 35AC63DC-2D40B04F: PPM -3.9, gain 14 of 21 (linearity)", text)
        self.assertIn("RTL-2832/R820T 00000001: PPM 1.5, gain 32.8 dB", text)

    def test_cli_writes_files(self):
        out = os.path.join(self.dir, "out")
        make_config.main(["--db", self.db, "--out", out])
        self.assertTrue(os.path.isfile(os.path.join(out, "playlist", "default.xml")))
        self.assertTrue(os.path.isfile(os.path.join(out, "tuners.txt")))


if __name__ == "__main__":
    unittest.main()
