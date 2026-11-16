//! Decodes an sdrtrunk traffic-channel baseband recording (WAV, 2 x 16-bit, 50 kHz complex) to 8 kHz audio,
//! skipping the channelizer, so our demod + mbelib can be compared with sdrtrunk's MP3 of the same call.
//!
//! cargo run --release -p sdrtrunk-audio --example baseband_to_wav -- IN_baseband.wav OUT.wav
use std::cell::RefCell;
use std::rc::Rc;
use sdrtrunk_audio::{write_wav, Imbe, SAMPLE_RATE};
use sdrtrunk_dsp::front_end::{C4fmFrontEnd, LsmFrontEnd};
use sdrtrunk_p25::control::{ldu_frames, Modulation};
use sdrtrunk_p25::demod::Demodulator;
use sdrtrunk_p25::demod_lsm::LsmDemodulator;
use sdrtrunk_p25::framer::{FrameHandler, Framer, FramerEvent};
use sdrtrunk_p25::nid::DataUnitId;

#[derive(Default)]
struct Out { pcm: Vec<i16>, errs: Vec<i32>, ends: u32, sample: u64, at: Vec<f64> }

struct H { dec: Imbe, out: Rc<RefCell<Out>> }

impl FrameHandler for H {
    fn handle(&mut self, e: FramerEvent) {
        let mut o = self.out.borrow_mut();
        if std::env::var_os("SDRTRUNK_TRACE").is_some() {
            let t = o.sample as f64 / 50_000.0;
            match &e {
                FramerEvent::Unit { duid, sync_bit_errors, bits, nac, .. } => eprintln!("{t:.2} {duid:?} nac={nac:x} sync_err={sync_bit_errors} bits={}", bits.len()),
                FramerEvent::SyncLoss { bits, .. } => eprintln!("{t:.2} SyncLoss {bits}"),
                FramerEvent::DroppedSamples { bits, .. } => eprintln!("{t:.2} Dropped {bits}"),
                FramerEvent::Tsbk { .. } => eprintln!("{t:.2} Tsbk"),
            }
        }
        let FramerEvent::Unit { duid, bits, .. } = e else { return };
        match duid {
            DataUnitId::LogicalLinkDataUnit1 | DataUnitId::LogicalLinkDataUnit2 => {
                let Some(frames) = ldu_frames(&bits) else { return };
                let t = o.sample as f64 / 50_000.0;
                o.at.push(t);
                for f in &frames { let (s, e) = self.dec.decode(f); o.pcm.extend_from_slice(&s); o.errs.push(e); }
            }
            DataUnitId::TerminatorDataUnit | DataUnitId::TerminatorDataUnitLinkControl => o.ends += 1,
            _ => {}
        }
    }
}

fn main() {
    let a: Vec<String> = std::env::args().collect();
    if !(3..=4).contains(&a.len()) { eprintln!("usage: baseband_to_wav IN_baseband.wav OUT.wav [c4fm|lsm]"); std::process::exit(2); }
    let lsm = a.get(3).map_or(true, |m| Modulation::parse(m).expect("c4fm or lsm") == Modulation::Lsm);
    let b = std::fs::read(&a[1]).unwrap();
    let rate = u32::from_le_bytes(b[24..28].try_into().unwrap()) as f64;
    let data = b.windows(4).position(|w| w == b"data").expect("no data chunk") + 8;
    let s: Vec<f32> = b[data..].chunks_exact(2).map(|c| i16::from_le_bytes([c[0], c[1]]) as f32 / 32768.0).collect();
    let (i, q): (Vec<f32>, Vec<f32>) = s.chunks_exact(2).map(|c| (c[0], c[1])).unzip();
    let out = Rc::new(RefCell::new(Out::default()));
    let mut framer = Framer::new(H { dec: Imbe::new(), out: out.clone() });
    framer.start();
    if lsm {
        let mut front = LsmFrontEnd::new(rate).unwrap();
        let mut demod = LsmDemodulator::new(framer, front.samples_per_symbol());
        for (ci, cq) in i.chunks(1000).zip(q.chunks(1000)) {
            let (fi, fq) = front.process(ci, cq);
            demod.process(&fi, &fq);
            out.borrow_mut().sample += ci.len() as u64;
        }
    } else {
        let mut front = C4fmFrontEnd::new(rate).unwrap();
        let mut demod = Demodulator::new(framer, front.samples_per_symbol());
        for (ci, cq) in i.chunks(1000).zip(q.chunks(1000)) {
            demod.process(&front.process(ci, cq));
            out.borrow_mut().sample += ci.len() as u64;
        }
    }
    let o = out.borrow();
    write_wav(std::path::Path::new(&a[2]), &o.pcm, SAMPLE_RATE).unwrap();
    let clean = o.errs.iter().filter(|&&e| e == 0).count();
    let at: Vec<String> = o.at.iter().map(|t| format!("{t:.2}")).collect();
    println!("{:.2} s in, {} LDUs ({} frames, {clean} error-free), {} terminators; LDU at [{}]",
        i.len() as f64 / rate, o.at.len(), o.errs.len(), o.ends, at.join(" "));
}
