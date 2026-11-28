//! An LSM voice channel that decoded noise for a while (between transmissions) still decodes the next transmission.
//! A synthetic pi/4-DQPSK voice call (HDU, 5 LDUs, terminators; fixtures/p25_streams.txt) is preceded by noise.
use sdrtrunk_dsp::front_end::LsmFrontEnd;
use sdrtrunk_p25::demod_lsm::LsmDemodulator;
use sdrtrunk_p25::dibit::Dibit;
use sdrtrunk_p25::framer::{FrameHandler, Framer, FramerEvent};
use sdrtrunk_p25::nid::DataUnitId;
use std::cell::Cell;
use std::f64::consts::PI;
use std::rc::Rc;

const RATE: f64 = 50_000.0;

struct Ldus(Rc<Cell<usize>>);
impl FrameHandler for Ldus {
    fn handle(&mut self, e: FramerEvent) {
        if let FramerEvent::Unit { duid: DataUnitId::LogicalLinkDataUnit1 | DataUnitId::LogicalLinkDataUnit2, .. } = e {
            self.0.set(self.0.get() + 1);
        }
    }
}

struct Rng(u64);
impl Rng {
    fn unit(&mut self) -> f64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        ((self.0 >> 11) as f64 + 0.5) / (1u64 << 53) as f64
    }
    fn gauss(&mut self) -> f64 { (-2.0 * self.unit().ln()).sqrt() * (2.0 * PI * self.unit()).cos() }
}

fn voice() -> Vec<Dibit> {
    let line = include_str!("fixtures/p25_streams.txt").lines().find(|l| l.starts_with("STREAM|voice|")).unwrap();
    line.rsplit('|').next().unwrap().bytes().map(|b| Dibit::from_value(b - b'0')).collect()
}

/// GenLsm's signal: linear phase ramp across each symbol, amplitude 0.25, `offset_hz` carrier offset, noise `sigma`.
fn modulate(dibits: &[Dibit], rng: &mut Rng, offset_hz: f64, sigma: f64) -> (Vec<f32>, Vec<f32>) {
    let start = 2.0 * PI * rng.unit();
    let mut phase_at = vec![start];
    for d in dibits { phase_at.push(phase_at.last().unwrap() + d.ideal_phase() as f64); }
    let n = (dibits.len() as f64 / 4800.0 * RATE) as usize;
    let (mut i, mut q) = (Vec::with_capacity(n), Vec::with_capacity(n));
    for k in 0..n {
        let t = k as f64 / RATE * 4800.0;
        let s = t.floor() as usize;
        let ph = phase_at[s] + (phase_at[s + 1] - phase_at[s]) * (t - s as f64) + 2.0 * PI * offset_hz * k as f64 / RATE;
        i.push((0.25 * ph.cos() + rng.gauss() * sigma) as f32);
        q.push((0.25 * ph.sin() + rng.gauss() * sigma) as f32);
    }
    (i, q)
}

/// LDUs decoded from `noise_s` seconds of noise then the voice call; and the PLL (radians/symbol) when the call begins.
fn run(noise_s: f64, seed: u64) -> (usize, f32) {
    let mut rng = Rng(seed);
    let count = Rc::new(Cell::new(0));
    let mut front = LsmFrontEnd::new(RATE).unwrap();
    let mut framer = Framer::new(Ldus(count.clone()));
    framer.start();
    let mut demod = LsmDemodulator::new(framer, front.samples_per_symbol());
    let mut feed = |demod: &mut LsmDemodulator<Framer<Ldus>>, i: &[f32], q: &[f32]| {
        for (bi, bq) in i.chunks(2000).zip(q.chunks(2000)) { let (fi, fq) = front.process(bi, bq); demod.process(&fi, &fq); }
    };
    let n = (noise_s * RATE) as usize;
    let (ni, nq): (Vec<f32>, Vec<f32>) = (0..n).map(|_| ((rng.gauss() * 0.02) as f32, (rng.gauss() * 0.02) as f32)).unzip();
    feed(&mut demod, &ni, &nq);
    let pll = demod.pll();
    let (si, sq) = modulate(&voice(), &mut rng, 100.0, 0.02);
    feed(&mut demod, &si, &sq);
    (count.get(), pll)
}

/// A voice channel stays open between transmissions (recorder `RELEASE_MS`). Before the PLL held still on noise, it
/// wandered to its +/-60 degree limit there, and a transmission starting beyond 45 degrees locked a quadrant off and was
/// lost whole (about 1 in 5 after half a second of noise; SGCo unit replies went missing).
#[test]
fn voice_after_noise_decodes_like_a_fresh_channel() {
    for noise_s in [0.0, 0.5, 2.0, 5.0] {
        let runs: Vec<(usize, f32)> = (1..=40).map(|s| run(noise_s, 0x9E37_79B9_7F4A_7C15 ^ s)).collect();
        let complete = runs.iter().filter(|r| r.0 == 5).count();
        let wandered = runs.iter().filter(|r| r.1.abs() > (PI / 8.0) as f32).count();
        assert_eq!((complete, wandered), (40, 0), "after {noise_s} s of noise: calls with all 5 LDUs, PLL past 22.5 degrees");
    }
}
