//! Runs a raw Airspy IQ capture (airspy_rx -t 2, interleaved int16) through the Rust receive chain
//! (channelizer -> channel extractor -> C4FM front end -> demodulator -> framer) and reports what it finds.
//!
//! cargo run --release -p sdrtrunk-p25 --example analyze_capture -- FILE CENTER_HZ RATE_HZ WANTED_HZ [SCAN_HZ]
//!
//! WANTED_HZ is scanned +/- SCAN_HZ (default 5000) in 250 Hz steps to find the tuner's frequency error.
use std::collections::BTreeMap;
use std::io::Read;
use sdrtrunk_dsp::channel_calculator::ChannelCalculator;
use sdrtrunk_dsp::channelizer::{sinc_m2_synthesizer_taps, ChannelExtractor, Channelizer, TwoChannelExtractor};
use sdrtrunk_dsp::front_end::C4fmFrontEnd;
use sdrtrunk_p25::demod::Demodulator;
use sdrtrunk_p25::framer::{FrameHandler, Framer, FramerEvent};

#[derive(Default)]
struct Counts {
    tsbk_valid: usize,
    tsbk_invalid: usize,
    tsbk_last: usize,
    units: BTreeMap<String, usize>,
    nacs: BTreeMap<i32, usize>,
    messages: BTreeMap<String, usize>,
    sync_loss_bits: i64,
    dropped_bits: i64,
    first_valid_ms: Option<i64>,
    last_valid_ms: i64,
}

impl FrameHandler for Counts {
    fn handle(&mut self, event: FramerEvent) {
        match event {
            FramerEvent::Tsbk { nac, valid, last_block, timestamp, bits, .. } => {
                if valid {
                    let msg = format!("{:?}", sdrtrunk_p25::tsbk_msg::parse(&bits));
                    *self.messages.entry(msg).or_default() += 1;
                    self.tsbk_valid += 1;
                    *self.nacs.entry(nac).or_default() += 1;
                    self.first_valid_ms.get_or_insert(timestamp);
                    self.last_valid_ms = timestamp;
                } else {
                    self.tsbk_invalid += 1;
                }
                if last_block { self.tsbk_last += 1; }
            }
            FramerEvent::Unit { duid, .. } => *self.units.entry(format!("{duid:?}")).or_default() += 1,
            FramerEvent::SyncLoss { bits, .. } => self.sync_loss_bits += bits as i64,
            FramerEvent::DroppedSamples { bits, .. } => self.dropped_bits += bits as i64,
        }
    }
}

enum Extractor { One(ChannelExtractor), Two(TwoChannelExtractor) }

fn main() {
    let a: Vec<String> = std::env::args().collect();
    if a.len() < 5 {
        eprintln!("usage: analyze_capture FILE CENTER_HZ RATE_HZ WANTED_HZ [SCAN_HZ]");
        std::process::exit(2);
    }
    let path = &a[1];
    let center: f64 = a[2].parse().unwrap();
    let rate: f64 = a[3].parse().unwrap();
    let wanted: i64 = a[4].parse().unwrap();
    let scan: i64 = a.get(5).map(|s| s.parse().unwrap()).unwrap_or(5000);

    let mut chan = Channelizer::for_sample_rate(rate).expect("channelizer");
    let channels = chan.channels();
    let mut calc = ChannelCalculator::new(rate, channels, center, 2.0);
    calc.set_center_frequency(center);
    let channel_rate = calc.channel_sample_rate();
    println!("{channels} channels of {:.0} Hz, channel rate {channel_rate:.0} Hz", calc.channel_bandwidth());

    // Bins that can matter for the scan range.
    let mut keep: Vec<usize> = Vec::new();
    for shift in (-scan..=scan).step_by(250) {
        if let Ok(ix) = calc.channel_indexes(wanted + shift, 12_500) {
            for i in ix { if !keep.contains(&i) { keep.push(i); } }
        }
    }
    keep.sort();
    println!("channelizing bins {keep:?}");

    // Pass 1: channelize the whole capture, keeping only the wanted bins.
    let mut stored: BTreeMap<usize, Vec<f32>> = keep.iter().map(|&k| (k, Vec::new())).collect();
    let mut power = vec![0.0f64; channels];
    let mut results = 0u64;
    let mut file = std::fs::File::open(path).expect("open capture");
    let mut raw = vec![0u8; 4 * 1_000_000];
    let mut leftover: Vec<u8> = Vec::new();
    loop {
        let n = file.read(&mut raw).unwrap();
        if n == 0 { break; }
        leftover.extend_from_slice(&raw[..n]);
        let usable = leftover.len() / 4 * 4;
        let samples: Vec<f32> = leftover[..usable].chunks_exact(2).map(|b| i16::from_le_bytes([b[0], b[1]]) as f32 / 2048.0).collect();
        leftover.drain(..usable);
        chan.receive(&samples, |r| {
            results += 1;
            for (k, v) in stored.iter_mut() { v.push(r[2 * k]); v.push(r[2 * k + 1]); }
            for k in 0..channels { power[k] += (r[2 * k] as f64).powi(2) + (r[2 * k + 1] as f64).powi(2); }
        });
    }
    println!("{results} channelizer results ({:.2} s)", results as f64 / channel_rate);

    // Strongest bins as a sanity check on where the energy is.
    let mut order: Vec<usize> = (0..channels).collect();
    order.sort_by(|&x, &y| power[y].partial_cmp(&power[x]).unwrap());
    print!("strongest bins (index: freq MHz):");
    for &k in order.iter().take(8) {
        print!(" {k}:{:.4}", calc.index_center_frequency(k as i32, sdrtrunk_dsp::channel_calculator::Boundary::AdjustPositive) / 1e6);
    }
    println!();

    // Pass 2: scan the wanted frequency to find the one that decodes.
    let syn = sinc_m2_synthesizer_taps(channel_rate, calc.channel_bandwidth(), 2, 9).unwrap();
    let mut best: Option<(i64, Counts)> = None;
    println!("{:>7} {:>5} {:>10} {:>12} {:>6}", "shift", "bins", "tsbk ok", "tsbk bad", "units");
    for shift in (-scan..=scan).step_by(250) {
        let freq = wanted + shift;
        let Ok(indexes) = calc.channel_indexes(freq, 12_500) else { continue };
        let offset = calc.frequency_offset(&indexes, freq, 0);
        let gain = channels as f32;
        let mut ex = if indexes.len() == 1 {
            let mut e = ChannelExtractor::new(channel_rate, indexes[0], gain);
            e.set_frequency_offset(offset);
            Extractor::One(e)
        } else if indexes.len() == 2 {
            let mut e = TwoChannelExtractor::new(channel_rate, [indexes[0], indexes[1]], &syn, gain);
            e.set_frequency_offset(offset);
            Extractor::Two(e)
        } else { continue };

        let mut front = C4fmFrontEnd::new(channel_rate).unwrap();
        let mut framer = Framer::new(Counts::default());
        framer.set_timestamp(1_000_000);
        framer.start();
        let mut demod = Demodulator::new(framer, front.samples_per_symbol());
        let mut feed = |i: &[f32], q: &[f32]| { let d = front.process(i, q); demod.process(&d); };
        let mut full: Vec<f32> = vec![0.0; 2 * calc.channel_count()];
        let n = stored.values().next().unwrap().len() / 2;
        for r in 0..n {
            for &k in &indexes { full[2 * k] = stored[&k][2 * r]; full[2 * k + 1] = stored[&k][2 * r + 1]; }
            match &mut ex {
                Extractor::One(e) => e.push(&full, &mut feed),
                Extractor::Two(e) => e.push(&full, &mut feed),
            }
        }
        let c = demod.into_sink().into_handler();
        println!("{shift:>7} {:>5} {:>10} {:>12} {:>6}", indexes.len(), c.tsbk_valid, c.tsbk_invalid, c.units.values().sum::<usize>());
        if best.as_ref().map_or(true, |(_, b)| c.tsbk_valid > b.tsbk_valid) { best = Some((shift, c)); }
    }

    if let Some((shift, c)) = best {
        let span = (c.last_valid_ms - c.first_valid_ms.unwrap_or(0)) as f64 / 1000.0;
        println!("\nbest shift {shift} Hz (frequency error {:.2} ppm)", -(shift as f64) / wanted as f64 * 1e6);
        println!("valid TSBK blocks {} (last-block flags {}), invalid {}", c.tsbk_valid, c.tsbk_last, c.tsbk_invalid);
        if c.tsbk_valid > 1 {
            println!("first to last valid block spans {span:.2} s: {:.1} valid blocks per second", c.tsbk_valid as f64 / span);
        }
        println!("NAC counts (hex): {:?}", c.nacs.iter().map(|(k, v)| (format!("{k:03X}"), *v)).collect::<Vec<_>>());
        println!("other units: {:?}", c.units);
        println!("messages (count: parsed):");
        for (m, n) in &c.messages { println!("  {n:>3}: {m}"); }
        println!("sync loss bits {} dropped bits {}", c.sync_loss_bits, c.dropped_bits);
    }
}
