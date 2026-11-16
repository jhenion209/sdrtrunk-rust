//! Replays a synthetic pi/4-DQPSK signal through the Rust LSM (CQPSK) chain and compares every soft symbol and
//! dibit decision with the original Java `P25P1DecoderLSM` filter chain + `P25P1DemodulatorLSM`
//! (fixtures/GenLsm.java.txt). The framer is a stub that records symbols on both sides.
use sdrtrunk_dsp::baseband::lsm_baseband_taps;
use sdrtrunk_dsp::front_end::LsmFrontEnd;
use sdrtrunk_dsp::rrc::root_raised_cosine;
use sdrtrunk_p25::demod::SymbolSink;
use sdrtrunk_p25::demod_lsm::LsmDemodulator;
use sdrtrunk_p25::dibit::Dibit;

#[derive(Default)]
struct Log(String);

impl SymbolSink for Log {
    fn process(&mut self, soft_symbol: f32, symbol: Dibit) {
        self.0.push_str(&format!("{:x},{};", soft_symbol.to_bits(), symbol.value()));
    }
    fn sync_detected(&mut self) {}
    fn is_complete(&self) -> bool { false }
    fn is_assembling(&self) -> bool { false }
}

fn floats(s: &str) -> Vec<f32> {
    s.split(',').map(|h| f32::from_bits(u32::from_str_radix(h, 16).unwrap())).collect()
}

fn first_difference(got: &str, want: &str) -> String {
    let (g, w): (Vec<&str>, Vec<&str>) = (got.split(';').collect(), want.split(';').collect());
    for (n, (a, b)) in g.iter().zip(&w).enumerate() {
        if a != b { return format!("symbol {n}: rust `{a}` java `{b}`"); }
    }
    format!("symbol counts differ: rust {} java {}", g.len(), w.len())
}

#[test]
fn lsm_chain_matches_java() {
    let text = include_str!("fixtures/java_lsm_vectors.txt");
    let mut front = LsmFrontEnd::new(50_000.0).unwrap();
    let mut demod = LsmDemodulator::new(Log::default(), front.samples_per_symbol());
    let (mut blocks, mut symbols) = (0, 0);
    for line in text.lines() {
        let f: Vec<&str> = line.split('|').collect();
        match (f[0], f[1]) {
            ("TAPS", "baseband") => assert_eq!(lsm_baseband_taps(25_000).unwrap(), floats(f[2]), "baseband taps"),
            ("TAPS", "rrc") => assert_eq!(root_raised_cosine(25_000.0 / 4800.0, 16, 0.2), floats(f[2]), "rrc taps"),
            ("BLOCK", _) => {
                let (i, q) = front.process(&floats(f[1]), &floats(f[2]));
                demod.sink_mut().0.clear();
                demod.process(&i, &q);
                let got = &demod.sink_mut().0;
                assert!(got == f[3], "block {blocks}: {}", first_difference(got, f[3]));
                blocks += 1;
                symbols += f[3].matches(';').count();
            }
            _ => panic!("unknown line {}", f[0]),
        }
    }
    assert_eq!((blocks, symbols), (12, 12 * 192));
}

/// Shrinking blocks must not lose the carried-over samples (the Java original truncates its buffer first).
#[test]
fn lsm_handles_varying_block_sizes() {
    let mut front = LsmFrontEnd::new(50_000.0).unwrap();
    let mut demod = LsmDemodulator::new(Log::default(), front.samples_per_symbol());
    let tone: Vec<f32> = (0..9000).map(|k| (k as f32 * 0.3).cos() * 0.2).collect();
    let quad: Vec<f32> = (0..9000).map(|k| (k as f32 * 0.3).sin() * 0.2).collect();
    let mut pos = 0;
    for blk in [4000, 500, 2000, 100, 2400] {
        let (i, q) = front.process(&tone[pos..pos + blk], &quad[pos..pos + blk]);
        demod.process(&i, &q);
        pos += blk;
    }
    // 9000 samples at 50 kHz = 864 symbols, minus the one-symbol reserve.
    let n = demod.sink_mut().0.matches(';').count();
    assert!((860..=865).contains(&n), "{n} symbols");
}
