# Benchmark: sdrtrunk, sdrtrunk-vce and sdrtrunk-rust

Three P25 recorders, run live one at a time on the same Raspberry Pi 5, with the same SDRs, systems and talkgroups.
Each one decoded both control channels and recorded calls to MP3 for 10 minutes, three times.

## Result

| Program | Runs used | CPU mean % | CPU peak % | RSS mean MB | RSS peak MB | MP3s / min | Audio s / min |
|---|---|---|---|---|---|---|---|
| sdrtrunk | 3 of 3 | 83.8 | 207.1 | 1342.2 | 2039.1 | 6.8 | 29.1 |
| sdrtrunk-vce | 3 of 3 | 101.5 | 229.4 | 757.7 | 776.0 | 7.8 | 30.2 |
| sdrtrunk-rust | 3 of 3 | 67.6 | 84.0 | 22.0 | 23.4 | 6.9 | 24.5 |

CPU is the whole process, all threads: 100% is one of the Pi's four cores. Peaks and means are averaged over the
three runs. No run was flagged as incomplete or as having no calls.

- **CPU:** sdrtrunk-rust used about 19% less CPU on average than sdrtrunk and about 33% less than sdrtrunk-vce. Its
  peak stayed under one core; both Java programs peaked above two cores.
- **Memory:** sdrtrunk-rust stayed between 22 and 24 MB. sdrtrunk used about 60 times as much on average and grew to
  2.4 GB in two runs; sdrtrunk-vce held steady near 760 MB.
- **Calls:** all three kept up with the traffic. Call counts move with live traffic far more than with the program
  (see the per-run table), so read them as "kept up", not as a score.

## Per run

| Round | Program | Started | CPU mean % | CPU peak % | RSS mean MB | RSS peak MB | MP3s / min | Audio s / min | First MP3 (s) |
|---|---|---|---|---|---|---|---|---|---|
| 1 | sdrtrunk | 01:26 | 82.9 | 163.1 | 1143.0 | 1369.2 | 8.0 | 39.0 | 27 |
| 1 | sdrtrunk-vce | 01:37 | 103.8 | 226.0 | 793.6 | 808.3 | 9.3 | 36.3 | 41 |
| 1 | sdrtrunk-rust | 01:48 | 67.8 | 86.0 | 22.3 | 24.0 | 8.3 | 30.4 | 6 |
| 2 | sdrtrunk-vce | 01:59 | 103.1 | 229.0 | 745.1 | 767.6 | 6.3 | 21.3 | 15 |
| 2 | sdrtrunk-rust | 02:10 | 67.0 | 84.0 | 21.8 | 22.9 | 6.7 | 22.4 | 53 |
| 2 | sdrtrunk | 02:21 | 83.6 | 269.7 | 1538.2 | 2386.9 | 2.4 | 8.9 | 26 |
| 3 | sdrtrunk-rust | 02:33 | 67.9 | 82.0 | 21.9 | 23.2 | 5.6 | 20.6 | 14 |
| 3 | sdrtrunk | 02:44 | 84.8 | 188.6 | 1345.4 | 2361.3 | 10.1 | 39.5 | 39 |
| 3 | sdrtrunk-vce | 02:55 | 97.7 | 233.1 | 734.4 | 752.2 | 7.8 | 33.0 | 13 |

"First MP3" is seconds from launch to the first recorded call, which depends mostly on when the next call happened.
The raw once-a-second samples are in [`bench/results/`](bench/results/).

## Setup

| | |
|---|---|
| Computer | Raspberry Pi 5 Model B, 8 GB, 4 cores, `ondemand` governor |
| OS | Debian GNU/Linux 13 (trixie), kernel 6.18.60+rpt-rpi-2712 |
| SDRs | Airspy (one system), RTL-SDR (second system), second RTL-SDR available as a helper |
| sdrtrunk | v1.0.0, linux-aarch64 release |
| sdrtrunk-vce | nightly build, Java 25.0.1 |
| sdrtrunk-rust | commit b86b03b, `cargo build --release` |
| Date | 2026-11-16, 01:26 to 03:07 local time |

## Method

- Rounds rotate the order (A B C, B C A, C A B) so time of night does not favour one program.
- Each run starts in a fresh copy of the same settings, waits at least 60 s and until the first MP3, then samples
  `/proc` once a second for 600 s, then stops the program.
- The sdrtrunk and sdrtrunk-vce playlists were made from the sdrtrunk-rust database by `bench/make_config.py`, so all
  three followed the same systems and recorded the same talkgroups. sdrtrunk-rust recorded only listed talkgroups set
  to record and streamed nothing.
- `bench/README.md` has the full steps and scripts to repeat it.

## Caveats

- **Live traffic.** Each run heard different calls. CPU and memory are comparable; call counts are not precise.
- **Audio seconds are estimated** from MP3 file size at 16 kbps. sdrtrunk-rust's calls average about 3.6 s per file
  against about 4.3 s for sdrtrunk and 3.9 s for sdrtrunk-vce. This could be how each program ends a call or the
  encoder's framing; the benchmark does not tell which.
- **Desktop vs headless.** sdrtrunk and sdrtrunk-vce ran with their desktop windows open; sdrtrunk-rust ran headless
  with its web server idle. Part of the Java programs' CPU and memory is their user interface.
- **Tuner gain.** The Java programs used gain entered by hand (RTL at 32.8 dB); sdrtrunk-rust used its own tuner
  settings. sdrtrunk may also have used the helper RTL-SDR for traffic channels.
- **Java memory** follows the JVM's default heap sizing on an 8 GB machine; a smaller `-Xmx` would lower it.
- One computer, one night, three runs each. Treat differences under about 10% as noise.
