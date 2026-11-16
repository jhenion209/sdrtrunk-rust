//! Complex oscillator and mixer, ported from sdrtrunk `dsp.oscillator.ScalarComplexOscillator`
//! and `dsp.mixer.ScalarComplexMixer`.
//!
//! The oscillator is a recursive rotator in f32 with a gain correction every 100 samples, so results
//! are only reproducible by following the Java float arithmetic exactly (no fused multiply-add).
use std::f64::consts::PI;

pub struct ComplexOscillator {
    frequency: f64,
    sample_rate: f64,
    angle_per_sample: f32,
    prev_i: f32,
    prev_q: f32,
}

impl ComplexOscillator {
    pub fn new(frequency: f64, sample_rate: f64) -> Self {
        let mut o = Self { frequency, sample_rate, angle_per_sample: 0.0, prev_i: 1.0, prev_q: 0.0 };
        o.update();
        o
    }

    fn update(&mut self) {
        // Java: (float)(TWO_PI * frequency / sampleRate)
        self.angle_per_sample = (2.0 * PI * self.frequency / self.sample_rate) as f32;
    }

    pub fn frequency(&self) -> f64 { self.frequency }
    pub fn has_frequency(&self) -> bool { self.frequency != 0.0 }

    pub fn set_frequency(&mut self, frequency: f64) {
        self.frequency = frequency;
        self.update();
    }

    pub fn set_sample_rate(&mut self, sample_rate: f64) {
        self.sample_rate = sample_rate;
        self.update();
    }

    /// Generates `count` samples, continuing from the previous call's phase.
    pub fn generate(&mut self, count: usize) -> (Vec<f32>, Vec<f32>) {
        let mut i = vec![0.0f32; count];
        let mut q = vec![0.0f32; count];
        let cos = (self.angle_per_sample as f64).cos() as f32;
        let sin = (self.angle_per_sample as f64).sin() as f32;
        let (mut pi, mut pq) = (self.prev_i, self.prev_q);
        let mut gain_pointer = 0u32;
        for n in 0..count {
            let mut ni = pi * cos - pq * sin;
            let mut nq = pi * sin + pq * cos;
            gain_pointer += 1;
            if gain_pointer % 100 == 0 {
                gain_pointer = 0;
                let gain = (3.0f32 - (pi * pi + pq * pq)) / 2.0f32;
                ni *= gain;
                nq *= gain;
            }
            i[n] = ni;
            q[n] = nq;
            pi = ni;
            pq = nq;
        }
        self.prev_i = pi;
        self.prev_q = pq;
        (i, q)
    }
}

/// Multiplies complex samples by the oscillator output (frequency shift).
pub struct ComplexMixer {
    osc: ComplexOscillator,
}

impl ComplexMixer {
    pub fn new(frequency: f64, sample_rate: f64) -> Self {
        Self { osc: ComplexOscillator::new(frequency, sample_rate) }
    }

    pub fn set_frequency(&mut self, frequency: f64) { self.osc.set_frequency(frequency); }
    pub fn has_frequency(&self) -> bool { self.osc.has_frequency() }
    pub fn frequency(&self) -> f64 { self.osc.frequency() }
    pub fn set_sample_rate(&mut self, sample_rate: f64) { self.osc.set_sample_rate(sample_rate); }

    pub fn mix(&mut self, i: &[f32], q: &[f32]) -> (Vec<f32>, Vec<f32>) {
        assert_eq!(i.len(), q.len());
        let (mut mi, mut mq) = self.osc.generate(i.len());
        for n in 0..i.len() {
            let inphase = mi[n] * i[n] - mq[n] * q[n];
            let quadrature = mq[n] * i[n] + mi[n] * q[n];
            mi[n] = inphase;
            mq[n] = quadrature;
        }
        (mi, mq)
    }

    /// Mixes interleaved I,Q samples (as delivered by the tuner drivers).
    pub fn mix_interleaved(&mut self, samples: &[f32]) -> (Vec<f32>, Vec<f32>) {
        let (mut mi, mut mq) = self.osc.generate(samples.len() / 2);
        for n in 0..mi.len() {
            let (si, sq) = (samples[2 * n], samples[2 * n + 1]);
            let inphase = mi[n] * si - mq[n] * sq;
            let quadrature = mq[n] * si + mi[n] * sq;
            mi[n] = inphase;
            mq[n] = quadrature;
        }
        (mi, mq)
    }
}
