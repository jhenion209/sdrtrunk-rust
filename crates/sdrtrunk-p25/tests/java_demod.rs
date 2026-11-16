//! Replays synthetic demodulated-phase signals through the Rust C4FM demodulator and compares every soft symbol,
//! dibit decision and sync notification with the original Java `P25P1DemodulatorC4FM`
//! (fixtures/GenDemod.java.txt). The message framer is replaced on both sides by the same small state machine.
use sdrtrunk_p25::demod::{Demodulator, SymbolSink};
use sdrtrunk_p25::dibit::Dibit;

const ASSEMBLE: i32 = 130;
const SPS: f32 = 25000.0 / 4800.0;

#[derive(Default)]
struct StubFramer {
    assembling: bool,
    complete: bool,
    remaining: i32,
    log: String,
}

impl SymbolSink for StubFramer {
    fn process(&mut self, soft_symbol: f32, symbol: Dibit) {
        self.log.push_str(&format!("p,{:x},{};", soft_symbol.to_bits(), symbol.value()));
        if self.assembling {
            self.remaining -= 1;
            if self.remaining <= 0 {
                self.assembling = false;
                self.complete = true;
            }
        }
    }
    fn sync_detected(&mut self) {
        self.log.push_str("s;");
        self.assembling = true;
        self.complete = false;
        self.remaining = ASSEMBLE;
    }
    fn is_complete(&self) -> bool { self.complete }
    fn is_assembling(&self) -> bool { self.assembling }
}

fn floats(s: &str) -> Vec<f32> {
    s.split(',').map(|h| f32::from_bits(u32::from_str_radix(h, 16).unwrap())).collect()
}

/// First differing event, for a readable failure message.
fn first_difference(got: &str, want: &str) -> String {
    let g: Vec<&str> = got.split(';').collect();
    let w: Vec<&str> = want.split(';').collect();
    for (n, (a, b)) in g.iter().zip(&w).enumerate() {
        if a != b {
            return format!("event {n}: rust `{a}` java `{b}`");
        }
    }
    format!("event counts differ: rust {} java {}", g.len(), w.len())
}

#[test]
fn demodulator_matches_java() {
    let text = include_str!("fixtures/java_demod_vectors.txt");
    let (mut runs, mut symbols, mut syncs) = (0, 0, 0);
    for (n, line) in text.lines().enumerate() {
        let f: Vec<&str> = line.split('|').collect();
        assert_eq!(f[0], "DEMOD");
        let mode = f[1];
        let sizes: Vec<usize> = f[2].split(',').map(|v| v.parse().unwrap()).collect();
        let samples = floats(f[3]);
        let want = f[4];

        let mut d = Demodulator::new(StubFramer::default(), SPS);
        if let Some(nac) = mode.strip_prefix("expected:") {
            d.set_expected_nac(i32::from_str_radix(nac, 16).unwrap());
        }
        if mode == "strict" {
            d.set_require_valid_nid(true);
        }
        let mut pos = 0;
        for size in sizes {
            d.process(&samples[pos..pos + size]);
            pos += size;
        }
        assert_eq!(pos, samples.len());

        let got = &d.sink().log;
        assert!(got == want, "run {} ({mode}): {}", n + 1, first_difference(got, want));
        runs += 1;
        symbols += want.matches("p,").count();
        syncs += want.matches("s;").count();
    }
    println!("demodulator: {runs} runs, {symbols} symbols and {syncs} sync events identical to Java");
    assert!(runs >= 7 && syncs > 10);
}
