# sdrtrunk-rust

A headless P25 Phase 1 trunking recorder for Linux, written in Rust: a port of the receive, decode and record path of
[sdrtrunk](https://github.com/DSheirer/sdrtrunk), with a web interface for setup and monitoring. It runs well on a
Raspberry Pi 5 and on x86_64.

Inspired by and built against two Java projects: [sdrtrunk](https://github.com/DSheirer/sdrtrunk) by Dennis Sheirer,
whose decoders this ports, and [sdrtrunk-vce](https://github.com/tylerwatt12/sdrtrunk-vce), an enhanced fork of it whose
headless web interface, tuner placement, signal-quality and streaming behaviour sdrtrunk-rust follows.

It follows a P25 control channel, tunes voice channels as calls are granted, decodes the IMBE voice and writes one MP3
per transmission, named and tagged the way sdrtrunk does, so tools that ingest sdrtrunk recordings take them unchanged.
Calls can also be played live to Icecast streams.

## Features

- P25 Phase 1 control and voice channels, C4FM and LSM (simulcast, sdrtrunk's "CQPSK"). DSP, error correction and
  decoding were checked against vectors generated from the Java code.
- Airspy and RTL-SDR tuners; several systems at once, each on its own SDR (pinned or picked automatically).
- Tuner window placed automatically to cover the control channel and the voice frequencies a system uses, moved when a
  call falls outside it; optional lock center and frequency limits; Setup mode to watch a tuner's spectrum.
- Automatic frequency correction (PPM) from the control channel's measured error.
- Recording: clear calls only (encrypted transmissions are never written), per talkgroup on/off, talkgroup lists with
  RadioReference CSV import.
- Icecast streaming: each stream plays the calls of chosen talkgroups (from any alias list) in order, with silence in
  between, now-playing titles, and import of the streams in an sdrtrunk playlist.
- Web UI: dashboard with live calls and signal readouts, tuner spectrum and waterfall, per-channel spectrum, signal and
  decode-quality history, user accounts (owner and admins).

Not supported: P25 Phase 2 (TDMA), DMR, NXDN, analog, and decryption.

## Build

Linux with a current stable Rust toolchain (`rustup`), a C compiler, and the Airspy library:

    sudo apt install build-essential libairspy0 librtlsdr2   # librtlsdr0 on older Debian
    cargo build --release -p sdrtrunk-cli

The program is `target/release/sdrtrunk-rust`. The RTL-SDR library is loaded at run time, so it is only needed when an
RTL-SDR is used.

## Run

As a service (the database lives in `/var/lib/sdrtrunk-rust`):

    sudo install -m755 target/release/sdrtrunk-rust /usr/local/bin/sdrtrunk-rust
    sudo install -m644 packaging/sdrtrunk-rust.service /etc/systemd/system/sdrtrunk-rust.service
    # set User= in the unit file to the account that should run it (in the plugdev group for USB access)
    sudo systemctl daemon-reload
    sudo systemctl enable --now sdrtrunk-rust
    journalctl -u sdrtrunk-rust | grep "setup code"

Open `http://<host>:8080/setup`, enter the setup code from the log and create the owner account. Then add a system
(control channel frequency, modulation, names used in file names) on the Systems page, check the SDR's frequency
correction on the Tuners page, and choose a recordings folder.

Options: `sdrtrunk-rust serve [--listen 0.0.0.0:8080] [--db PATH] [--secure-cookies]` (use `--secure-cookies`
behind an HTTPS reverse proxy). `sdrtrunk-rust --freq HZ ...` runs a console monitor without the web UI.

## License

GPL-3.0-or-later (see `LICENSE`), as a derived work of sdrtrunk and sdrtrunk-vce (both GPL-3.0). Bundled: mbelib
(`crates/sdrtrunk-audio/mbelib`, ISC licence, see its `COPYRIGHT`); LAME through the `mp3lame-encoder` crate (LGPL).

IMBE voice decoding may be covered by patents in some countries; check what applies where you use it. Listening to or
recording radio traffic is regulated differently in different places; you are responsible for using this lawfully.
