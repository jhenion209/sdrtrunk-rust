//! Runs a raw Airspy capture through the control channel and the call recorder, writing the MP3s a live receiver would.
//!
//! cargo run --release -p sdrtrunk-web --example record_capture -- FILE CENTER_HZ RATE_HZ CONTROL_HZ PPM c4fm|lsm OUTDIR
use std::sync::Arc;
use sdrtrunk_p25::control::{ControlChannel, Modulation};
use sdrtrunk_web::db::System;
use sdrtrunk_web::recorder::{Names, Recorder, RELEASE_MS};

fn main() {
    let a: Vec<String> = std::env::args().collect();
    if a.len() != 8 { eprintln!("usage: record_capture FILE CENTER_HZ RATE_HZ CONTROL_HZ PPM c4fm|lsm OUTDIR"); std::process::exit(2); }
    let (center, rate, control, ppm): (f64, f64, i64, f64) = (a[2].parse().unwrap(), a[3].parse().unwrap(), a[4].parse().unwrap(), a[5].parse().unwrap());
    let mut cc = ControlChannel::new(center, rate, control, ppm, 0, Modulation::parse(&a[6]).expect("c4fm or lsm")).unwrap();
    let d = System::default();
    let names = Names { system: d.system_name, site: d.site_name, channel: d.channel_name };
    let mut rec = Recorder::new(std::path::Path::new(&a[7]), names, Arc::default());
    let bytes = std::fs::read(&a[1]).unwrap();
    let iq: Vec<i16> = bytes.chunks_exact(2).map(|b| i16::from_le_bytes([b[0], b[1]])).collect();
    // Capture time, starting now, so file names carry today's date.
    let t0 = sdrtrunk_web::receiver::now_ms();
    let mut events = Vec::new();
    let mut now = t0;
    for (bi, block) in iq.chunks(2 * 65_536).enumerate() {
        now = t0 + ((bi * 65_536) as f64 / rate * 1000.0) as i64;
        cc.process(block, &mut |e| events.push(e));
        for e in events.drain(..) { rec.handle(&e, now, &mut cc); }
        rec.tick(now, &mut cc);
    }
    rec.tick(now + RELEASE_MS + 1, &mut cc);
    println!("{} calls recorded, {} voice channels still open, last error {:?}", rec.recorded, cc.voice_count(), rec.last_error);
}
