# Benchmark: sdrtrunk, sdrtrunk-vce and sdrtrunk-rust

Three P25 recorders, run one at a time on the same Raspberry Pi 5, following the same systems and talkgroups.
Each one decoded both control channels and recorded calls to MP3 for 10 minutes, three times, in two setups:

- **Live:** the SDRs on the air. Each run hears different calls.
- **Same signal (replay):** one baseband recording of both SDRs, looped to every program, so each does the same work.

Call counts are not compared: no two runs recorded the same stretch of time.

## Live

| Program | Runs used | CPU mean % | CPU peak % | RSS mean MB | RSS peak MB |
|---|---|---|---|---|---|
| sdrtrunk | 3 of 3 | 83.8 | 207.1 | 1342.2 | 2039.1 |
| sdrtrunk-vce | 3 of 3 | 101.5 | 229.4 | 757.7 | 776.0 |
| sdrtrunk-rust | 3 of 3 | 67.6 | 84.0 | 22.0 | 23.4 |

CPU is the whole process, all threads: 100% is one of the Pi's four cores. Peaks and means are averaged over the
three runs. No run was flagged as incomplete or as having no calls.

- **CPU:** sdrtrunk-rust used about 19% less CPU on average than sdrtrunk and about 33% less than sdrtrunk-vce. Its
  peak stayed under one core; both Java programs peaked above two cores.
- **Memory:** sdrtrunk-rust stayed between 22 and 24 MB. sdrtrunk used about 60 times as much on average and grew to
  2.4 GB in two runs; sdrtrunk-vce held steady near 760 MB.
- **Calls:** all three kept up with the traffic.

### Per run

| Round | Program | Started | CPU mean % | CPU peak % | RSS mean MB | RSS peak MB | First MP3 (s) |
|---|---|---|---|---|---|---|---|
| 1 | sdrtrunk | 01:26 | 82.9 | 163.1 | 1143.0 | 1369.2 | 27 |
| 1 | sdrtrunk-vce | 01:37 | 103.8 | 226.0 | 793.6 | 808.3 | 41 |
| 1 | sdrtrunk-rust | 01:48 | 67.8 | 86.0 | 22.3 | 24.0 | 6 |
| 2 | sdrtrunk-vce | 01:59 | 103.1 | 229.0 | 745.1 | 767.6 | 15 |
| 2 | sdrtrunk-rust | 02:10 | 67.0 | 84.0 | 21.8 | 22.9 | 53 |
| 2 | sdrtrunk | 02:21 | 83.6 | 269.7 | 1538.2 | 2386.9 | 26 |
| 3 | sdrtrunk-rust | 02:33 | 67.9 | 82.0 | 21.9 | 23.2 | 14 |
| 3 | sdrtrunk | 02:44 | 84.8 | 188.6 | 1345.4 | 2361.3 | 39 |
| 3 | sdrtrunk-vce | 02:55 | 97.7 | 233.1 | 734.4 | 752.2 | 13 |

"First MP3" is seconds from launch to the first recorded call, which depends mostly on when the next call happened.
The raw once-a-second samples are in [`bench/results/`](bench/results/).

## Same signal (replay)

| Program | Runs used | CPU mean % | CPU peak % | RSS mean MB | RSS peak MB |
|---|---|---|---|---|---|
| sdrtrunk | 3 of 3 | 110.8 | 211.9 | 1569.8 | 1825.4 |
| sdrtrunk-vce | 3 of 3 | 117.6 | 230.0 | 1120.0 | 1204.7 |
| sdrtrunk-rust | 3 of 3 | 59.1 | 79.3 | 18.2 | 20.2 |

The recording was made on a weekday afternoon, so it carries about six times the live night-time traffic.

- **CPU:** sdrtrunk-rust used about half the CPU of either Java program, and never reached one core.
- **Memory:** sdrtrunk-rust stayed under 21 MB; the Java programs used 1.0 to 2.5 GB.

### Per run

| Round | Program | Started | CPU mean % | CPU peak % | RSS mean MB | RSS peak MB | First MP3 (s) |
|---|---|---|---|---|---|---|---|
| 1 | sdrtrunk | 23:12 | 105.6 | 183.3 | 1096.9 | 1263.4 | 69 |
| 1 | sdrtrunk-vce | 23:24 | 117.6 | 272.0 | 1259.0 | 1320.8 | 49 |
| 2 | sdrtrunk | 23:36 | 125.1 | 249.7 | 2187.3 | 2499.0 | 441 |
| 2 | sdrtrunk-vce | 23:54 | 113.7 | 200.0 | 1108.4 | 1172.1 | 36 |
| 3 | sdrtrunk | 00:06 | 101.8 | 202.7 | 1425.1 | 1713.7 | 46 |
| 3 | sdrtrunk-vce | 00:18 | 121.4 | 218.0 | 992.5 | 1121.3 | 224 |
| 1 | sdrtrunk-rust | 00:33 | 59.6 | 83.0 | 17.5 | 19.2 | 2 |
| 2 | sdrtrunk-rust | 00:44 | 58.8 | 79.0 | 18.5 | 20.6 | 2 |
| 3 | sdrtrunk-rust | 00:55 | 58.9 | 76.0 | 18.7 | 20.7 | 2 |

"First MP3" here is mostly how long it took to enable the Java programs' recording tuners by hand (see Method).
In sdrtrunk round 2 that took about 7 minutes, and that run's memory was the highest of the six Java runs.
The raw samples are in [`bench/results-replay/`](bench/results-replay/).

## Setup

| | |
|---|---|
| Computer | Raspberry Pi 5 Model B, 8 GB, 4 cores, `ondemand` governor |
| OS | Debian GNU/Linux 13 (trixie), kernel 6.18.60+rpt-rpi-2712 |
| SDRs | Airspy (one system), RTL-SDR (second system), second RTL-SDR available as a helper |
| sdrtrunk | v1.0.0, linux-aarch64 release |
| sdrtrunk-vce | nightly build, Java 25.0.1 |
| sdrtrunk-rust | live: commit b86b03b; replay: 8aa6e01 with the replay support; `cargo build --release` |
| Live runs | 2026-11-16, 01:26 to 03:07 local time |
| Recording | 2026-11-16, 13:18 local time, 175 s of both SDRs at once (Airspy 6 MS/s, RTL-SDR 2.4 MS/s) |
| Replay runs | 2026-11-16 23:12 to 2026-11-17 01:06 local time |

## Method

- Each run starts in a fresh copy of the same settings, then samples `/proc` once a second for 600 s, then stops the
  program. `bench/README.md` has the full steps and scripts to repeat it.
- The sdrtrunk and sdrtrunk-vce playlists were made from the sdrtrunk-rust database by `bench/make_config.py`, so all
  three followed the same systems and recorded the same talkgroups. sdrtrunk-rust recorded only listed talkgroups set
  to record and streamed nothing.
- **Live:** rounds rotate the order (A B C, B C A, C A B) so time of night does not favour one program. Sampling
  starts at least 60 s after launch and after the first MP3.
- **Replay:** `sdrtrunk-rust record` captured both SDRs at the centres they use live. `bench/ppm_correct.py` removed
  each tuner's PPM error from its recording, and every program played the recordings at PPM 0, looping them about 3.4
  times per run. Both Java programs start recording tuners disabled at every launch, so their tuners were enabled and
  channels started by hand at the start of each run. Sampling starts 60 s after the first MP3 for all three.
  The signal does not change with the hour, so the order was not rotated.

## Caveats

- **Desktop vs headless.** sdrtrunk and sdrtrunk-vce ran with their desktop windows open; sdrtrunk-rust ran headless
  with its web server idle. Part of the Java programs' CPU and memory is their user interface.
- **Live tuner gain.** The Java programs used gain entered by hand (RTL at 32.8 dB); sdrtrunk-rust used its own tuner
  settings. sdrtrunk may also have used the helper RTL-SDR for traffic channels. Replay removes both: every program
  played the same recordings.
- **Replay loops** a 175 s recording, so the 10-minute runs hear the same calls about 3.4 times.
- **Java memory** follows the JVM's default heap sizing on an 8 GB machine; a smaller `-Xmx` would lower it.
- One computer, three runs each per setup. Treat differences under about 10% as noise.
