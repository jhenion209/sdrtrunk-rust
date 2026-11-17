# Benchmark: sdrtrunk, sdrtrunk-vce and sdrtrunk-rust

Live runs on one Raspberry Pi 5 (8 GB), one program at a time, same SDRs, same systems and talkgroups.
Each run: 60 s warm-up (longer until the first MP3), then 600 s of sampling once a second.
Three rounds in rotated order (A B C, B C A, C A B) so time-of-day traffic does not favour one program.

| File | What it does |
|---|---|
| `make_config.py` | sdrtrunk-rust database → sdrtrunk playlist (`playlist/default.xml`) and `tuners.txt` (PPM and gain to enter by hand) |
| `run.sh` | one run of one program in a fresh settings folder; writes `results/ROUND-PROGRAM.csv` and `.meta` |
| `measure.py` | samples CPU (% of one core, all threads), RSS and new MP3s of one process |
| `summarize.py` | `results/` → Markdown table; incomplete runs and runs with no calls are listed, not counted |
| `replay_db.py` | points the sdrtrunk-rust database copy at the baseband recordings for replay runs |
| `ppm_correct.py` | removes a tuner's PPM error from a baseband recording |
| `charts.py` | `results/` and `results-replay/` → the README's CPU and memory charts in `charts/` |

Tests: `python3 -m unittest bench/test_make_config.py bench/test_measure.py bench/test_summarize.py bench/test_replay_db.py bench/test_ppm_correct.py bench/test_charts.py`
(the sampler tests need Linux `/proc`).

## Setup (once)

1. Stop the service for the whole benchmark: `sudo systemctl stop sdrtrunk-rust`.
2. Build the release binary: `cargo build --release`.
3. Make the configs from a copy of the live database:
   `python3 bench/make_config.py --db ~/bench/live-copy.db --out ~/bench/config`, then
   `mkdir -p ~/bench/template && cp ~/bench/live-copy.db ~/bench/template/rust.db`.
4. sdrtrunk template: `mkdir -p ~/bench/template/sdrtrunk/SDRTrunk/playlist`, copy
   `~/bench/config/playlist/default.xml` into it, then start sdrtrunk once on the desktop with
   `JAVA_OPTS=-Duser.home=$HOME/bench/template/sdrtrunk` (Java ignores `HOME`) and enter each tuner's PPM and gain from `~/bench/config/tuners.txt`.
   Check both channels are enabled and decoding, then close it.
5. sdrtrunk-vce template: `ln -s <extracted sdrtrunk-vce folder> ~/bench/vce`, start it, choose
   "Import XML playlist" with `~/bench/config/playlist/default.xml`, enter the tuners the same way, close it, then
   `cp -a ~/bench/vce/data ~/bench/template/vce-data`.
6. Smoke test each program: `WARMUP_SECONDS=30 MEASURE_SECONDS=30 bench/run.sh sdrtrunk-rust 0` (and the other two),
   then delete `bench/results/0-*`.

## Runs

```
export RUST_VERSION=<commit>
bench/run.sh sdrtrunk 1;     bench/run.sh sdrtrunk-vce 1; bench/run.sh sdrtrunk-rust 1
bench/run.sh sdrtrunk-vce 2; bench/run.sh sdrtrunk-rust 2; bench/run.sh sdrtrunk 2
bench/run.sh sdrtrunk-rust 3; bench/run.sh sdrtrunk 3;    bench/run.sh sdrtrunk-vce 3
python3 bench/summarize.py > bench/results/summary.md
```

Then `sudo systemctl start sdrtrunk-rust`.

## Replay (same signal for all three)

Live runs hear different calls, so only CPU and memory compare. Replay plays one baseband recording to every program.

1. Record both SDRs at once, at the centre each tuner uses live (Tuners page), with the service stopped:
   `sdrtrunk-rust record --db ~/bench/live-copy.db --tuner 35AC63DC-2D40B04F --center HZ --seconds 175 --out ~/bench/replay`
   and the same for `rtl:00000001`, in parallel. Each prints its samples and must report 0 dropped.
   sdrtrunk plays WAV files up to 4 GB, which is 178 s for the Airspy at 6 MS/s (`record` refuses longer); every
   program loops the recording, so a 600 s run hears it about 3.4 times (4.2 GB Airspy, 1.7 GB RTL-SDR).
2. Remove each tuner's PPM error from its recording (sdrtrunk-vce has no PPM setting for recording tuners), keeping
   the raw files: `mkdir ~/bench/replay-raw && mv ~/bench/replay/*.wav ~/bench/replay-raw/`, then for each
   `python3 bench/ppm_correct.py --ppm PPM ~/bench/replay-raw/NAME.wav ~/bench/replay/NAME.wav` with the live tuner's PPM.
   Every program then plays the recordings at PPM 0 with automatic PPM off.
3. `~/bench/template-replay/sdrtrunk` and `~/bench/template-replay/vce-data`: copies of the live templates where each
   tuner is a Recording tuner on its WAV, at PPM 0, and each channel's preferred tuner set to
   it. sdrtrunk-vce plays only regular files in `data/recording_tuners` (symlinks refused): hard-link the WAVs there
   (`ln ~/bench/replay/*.wav ~/bench/vce/data/recording_tuners/`) before adding the tuners, and leave them out of the
   template; `run.sh` links them in for each run. sdrtrunk-rust needs no template: `run.sh` pins its database copy to
   the recordings with `replay_db.py`.
4. Run as above with `REPLAY_DIR=~/bench/replay` exported. Both Java programs start recording tuners disabled at
   every launch, so their runs are attended: when `run.sh` says so, enable both Recording tuners and start both
   channels in the program's window. Measuring starts 60 s after the first MP3, for all three programs. Each run
   starts at a different point in the loop; over about 3.4 loops that changes calls per minute only a little.
   Replay hears the same signal at any hour, so the order needs no rotation.
   Results go to `results-replay/`; summarize with `python3 bench/summarize.py --results bench/results-replay`.

## Caveats

- Live traffic differs between runs; compare CPU and memory, and read MP3s per minute as "kept up", not as a score.
- sdrtrunk and sdrtrunk-vce run with their desktop window open; sdrtrunk-rust runs headless with its web server idle.
- CPU is the whole process (all threads); 100% is one full core of four.
