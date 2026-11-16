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

Tests: `python3 -m unittest bench/test_make_config.py bench/test_measure.py bench/test_summarize.py`
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

## Caveats

- Live traffic differs between runs; compare CPU and memory, and read MP3s per minute as "kept up", not as a score.
- sdrtrunk and sdrtrunk-vce run with their desktop window open; sdrtrunk-rust runs headless with its web server idle.
- CPU is the whole process (all threads); 100% is one full core of four.
