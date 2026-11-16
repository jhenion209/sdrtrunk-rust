//! Decodes voice channels from a raw Airspy capture into WAV files (one per channel), to listen to the result.
//!
//! cargo run --release -p sdrtrunk-audio --example capture_to_wav -- FILE CENTER_HZ RATE_HZ CONTROL_HZ PPM c4fm|lsm OUTDIR VOICE_HZ...
//! SDRTRUNK_TRACE=1 prints when each LDU arrives.
use std::collections::BTreeMap;
use sdrtrunk_audio::{write_wav, Imbe, SAMPLE_RATE};
use sdrtrunk_p25::control::{ControlChannel, ControlEvent, Modulation};

struct Channel { hz: i64, dec: Imbe, pcm: Vec<i16>, raw: Vec<u8>, errs: Vec<i32>, ends: u32, clear_ess: u32, encrypted_ess: u32, algorithm: Option<u8> }

fn main() {
    let a: Vec<String> = std::env::args().collect();
    if a.len() < 9 { eprintln!("usage: capture_to_wav FILE CENTER_HZ RATE_HZ CONTROL_HZ PPM c4fm|lsm OUTDIR VOICE_HZ..."); std::process::exit(2); }
    let (center, rate, control, ppm) = (a[2].parse().unwrap(), a[3].parse().unwrap(), a[4].parse().unwrap(), a[5].parse().unwrap());
    let modulation = Modulation::parse(&a[6]).expect("modulation: c4fm or lsm");
    let out = std::path::PathBuf::from(&a[7]);
    let mut cc = ControlChannel::new(center, rate, control, ppm, 0, modulation).unwrap();
    let mut by_id: BTreeMap<u32, Channel> = BTreeMap::new();
    for f in &a[8..] {
        let hz: i64 = f.parse().unwrap();
        let ch = Channel { hz, dec: Imbe::new(), pcm: vec![], raw: vec![], errs: vec![], ends: 0, clear_ess: 0, encrypted_ess: 0, algorithm: None };
        by_id.insert(cc.add_voice(hz).unwrap(), ch);
    }
    let bytes = std::fs::read(&a[1]).unwrap();
    let iq: Vec<i16> = bytes.chunks_exact(2).map(|b| i16::from_le_bytes([b[0], b[1]])).collect();
    let trace = std::env::var_os("SDRTRUNK_TRACE").is_some();
    for (bi, block) in iq.chunks(2 * 65_536).enumerate() {
        let t = (bi * 65_536) as f64 / rate;
        cc.process(block, &mut |e| match e {
            ControlEvent::Voice { id, frames, ess, lc } => {
                if let (true, Some(lc)) = (trace, lc) { eprintln!("t={t:.3}s ch{id} LC {lc:?}"); }
                let ch = by_id.get_mut(&id).unwrap();
                if let Some(e) = ess {
                    if e.is_encrypted() { ch.encrypted_ess += 1 } else { ch.clear_ess += 1 }
                    ch.algorithm = Some(e.algorithm);
                }
                for f in &frames {
                    let (s, e) = ch.dec.decode(f);
                    ch.pcm.extend_from_slice(&s);
                    ch.errs.push(e);
                    // Raw frame packed MSB first (sdrtrunk's IMBE frame bytes), to decode with JMBE for comparison.
                    ch.raw.extend(f.chunks(8).map(|b| b.iter().fold(0u8, |a, &x| a << 1 | x as u8)));
                }
                if trace { eprintln!("t={t:.3}s ch{id} LDU errors {:?}", &ch.errs[ch.errs.len() - frames.len()..]); }
            }
            ControlEvent::VoiceEnd { id } => {
                if trace { eprintln!("t={t:.3}s ch{id} terminator"); }
                by_id.get_mut(&id).unwrap().ends += 1
            }
            ControlEvent::Call(g) if trace => eprintln!("t={t:.3}s grant {g:?}"),
            _ => {}
        });
    }
    for (id, ch) in &by_id {
        let path = out.join(format!("voice_{}.wav", ch.hz));
        write_wav(&path, &ch.pcm, SAMPLE_RATE).unwrap();
        std::fs::write(path.with_extension("imbe"), &ch.raw).unwrap();
        let clean = ch.errs.iter().filter(|&&e| e == 0).count();
        println!("channel {id} {} Hz: {} frames ({:.1} s), {clean} error-free, {} terminators, ESS clear {} encrypted {} (alg {:02X?}) -> {}",
            ch.hz, ch.errs.len(), ch.pcm.len() as f64 / SAMPLE_RATE as f64, ch.ends, ch.clear_ess, ch.encrypted_ess,
            ch.algorithm, path.display());
    }
}
