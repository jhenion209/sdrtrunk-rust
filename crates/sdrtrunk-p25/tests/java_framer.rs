//! Replays synthetic signals (fixtures/java_framer_vectors.txt, from GenFramer.java.txt) through the Rust
//! demodulator + framer and compares every message the framer reports with the original Java framer: TSBK blocks,
//! raw units, sync loss and dropped sample accounting, in order, with timestamps and bits.
use std::collections::HashMap;
use sdrtrunk_p25::demod::Demodulator;
use sdrtrunk_p25::framer::{FrameHandler, Framer, FramerEvent};
use sdrtrunk_p25::nid::DataUnitId;

const SPS: f32 = 25000.0 / 4800.0;
const REFERENCE_TIME: i64 = 1_000_000;

fn java_name(duid: DataUnitId) -> &'static str {
    match duid {
        DataUnitId::HeaderDataUnit => "HEADER_DATA_UNIT",
        DataUnitId::TerminatorDataUnit => "TERMINATOR_DATA_UNIT",
        DataUnitId::LogicalLinkDataUnit1 => "LOGICAL_LINK_DATA_UNIT_1",
        DataUnitId::TrunkingSignalingBlock1 => "TRUNKING_SIGNALING_BLOCK_1",
        DataUnitId::LogicalLinkDataUnit2 => "LOGICAL_LINK_DATA_UNIT_2",
        DataUnitId::PacketDataUnit => "PACKET_DATA_UNIT",
        DataUnitId::TerminatorDataUnitLinkControl => "TERMINATOR_DATA_UNIT_LINK_CONTROL",
        DataUnitId::PlaceHolder => "PLACE_HOLDER",
        DataUnitId::PacketDataUnitBlock1 => "PACKET_DATA_UNIT_BLOCK_1",
        DataUnitId::PacketDataUnitBlock2 => "PACKET_DATA_UNIT_BLOCK_2",
        DataUnitId::PacketDataUnitBlock3 => "PACKET_DATA_UNIT_BLOCK_3",
        DataUnitId::PacketDataUnitBlock4 => "PACKET_DATA_UNIT_BLOCK_4",
        DataUnitId::PacketDataUnitBlock5 => "PACKET_DATA_UNIT_BLOCK_5",
        DataUnitId::TrunkingSignalingBlock2 => "TRUNKING_SIGNALING_BLOCK_2",
        DataUnitId::TrunkingSignalingBlock3 => "TRUNKING_SIGNALING_BLOCK_3",
        DataUnitId::Unknown => "UNKNOWN",
    }
}

fn bits01(bits: &[bool]) -> String { bits.iter().map(|&b| if b { '1' } else { '0' }).collect() }

#[derive(Default)]
struct Collector {
    events: Vec<String>,
}

impl FrameHandler for Collector {
    fn handle(&mut self, event: FramerEvent) {
        self.events.push(match event {
            FramerEvent::Tsbk { duid, nac, timestamp, valid, last_block, corrected_bit_count, bits } => format!(
                "T,{},{nac},{timestamp},{},{},{corrected_bit_count},{}",
                java_name(duid), valid as u8, last_block as u8, bits01(&bits)
            ),
            FramerEvent::Unit { duid, nac, timestamp, sync_bit_errors, bits } => {
                // Java adds the sync bit errors to the corrected count of terminator data units only.
                let corrected = if duid == DataUnitId::TerminatorDataUnit { sync_bit_errors } else { 0 };
                format!("U,{},{nac},{timestamp},{corrected},{}", java_name(duid), bits01(&bits))
            }
            FramerEvent::SyncLoss { timestamp, bits } => format!("L,{timestamp},{bits}"),
            FramerEvent::DroppedSamples { timestamp, bits } => format!("D,{timestamp},{bits}"),
        });
    }
}

fn floats(s: &str) -> Vec<f32> {
    s.split(',').map(|h| f32::from_bits(u32::from_str_radix(h, 16).unwrap())).collect()
}

fn first_difference(got: &[String], want: &[&str]) -> String {
    for (n, (g, w)) in got.iter().zip(want).enumerate() {
        if g != w {
            let cut = |s: &str| s.chars().take(160).collect::<String>();
            return format!("event {n}:\n  rust {}\n  java {}", cut(g), cut(w));
        }
    }
    format!("event counts differ: rust {} java {}", got.len(), want.len())
}

#[test]
fn framer_matches_java() {
    let text = include_str!("fixtures/java_framer_vectors.txt");
    let mut signals: HashMap<&str, Vec<f32>> = HashMap::new();
    let (mut runs, mut events) = (0, 0);
    let mut kinds: HashMap<char, usize> = HashMap::new();

    for line in text.lines() {
        let f: Vec<&str> = line.splitn(4, '|').collect();
        match f[0] {
            "SIGNAL" => {
                signals.insert(f[1], floats(f[2]));
            }
            "FRAMER" => {
                let (id, mode) = (f[1], f[2]);
                let want: Vec<&str> = if f[3].is_empty() { Vec::new() } else { f[3].split(';').collect() };
                let samples = &signals[id];

                let mut framer = Framer::new(Collector::default());
                let mut demod_setup: Box<dyn Fn(&mut Demodulator<Framer<Collector>>)> = Box::new(|_| {});
                if let Some(nac) = mode.strip_prefix("expected:") {
                    let nac = i32::from_str_radix(nac, 16).unwrap();
                    framer.set_expected_nac(nac);
                    demod_setup = Box::new(move |d| d.set_expected_nac(nac));
                }
                if mode == "strict" {
                    framer.set_require_valid_nid(true);
                    demod_setup = Box::new(|d| d.set_require_valid_nid(true));
                }
                framer.set_timestamp(REFERENCE_TIME);
                framer.start();
                let mut demod = Demodulator::new(framer, SPS);
                demod_setup(&mut demod);

                let sizes = [1000usize, 513, 2048, 77, 300, 1024, 999];
                let (mut pos, mut step) = (0, 0);
                while pos < samples.len() {
                    let n = sizes[step % sizes.len()].min(samples.len() - pos);
                    step += 1;
                    demod.process(&samples[pos..pos + n]);
                    pos += n;
                }

                let got = demod.into_sink().into_handler().events;
                assert!(
                    got.len() == want.len() && got.iter().zip(&want).all(|(g, w)| g == w),
                    "{id} ({mode}): {}", first_difference(&got, &want)
                );
                for e in &got {
                    *kinds.entry(e.chars().next().unwrap()).or_default() += 1;
                }
                runs += 1;
                events += got.len();
            }
            other => panic!("unknown record {other}"),
        }
    }
    println!("framer: {runs} runs, {events} events identical to Java; by kind {kinds:?}");
    assert!(runs >= 15 && events > 100);
    for kind in ['T', 'U', 'L', 'D'] {
        assert!(kinds.get(&kind).copied().unwrap_or(0) > 0, "no {kind} events were exercised");
    }
}
