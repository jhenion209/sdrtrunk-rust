//! Half-band decimate-by-2 filter, ported from sdrtrunk `FilterFactory.getHalfBand` (Hamming window) and
//! `halfband.RealHalfBandDecimationFilter`. This is the `RealDecimateX2Filter` the C4FM decoder uses to bring the
//! 50 kHz polyphase channel down to 25 kHz (63 taps). Higher decimation factors are not needed for P25.
use std::f64::consts::PI;

/// Hamming window, with the Java float `TWO_PI` constant and float arithmetic reproduced exactly.
fn hamming(length: usize) -> Vec<f32> {
    let two_pi = (PI as f32) * 2.0f32;
    (0..length)
        .map(|x| 0.54f32 - (0.46f32 * (((two_pi * x as f32) / (length - 1) as f32) as f64).cos() as f32))
        .collect()
}

/// Half-band low-pass taps: length must be 4m+3; odd-index taps are zero except the 0.5 center.
pub fn half_band_taps(length: usize) -> Vec<f32> {
    assert!((length - 3) % 4 == 0, "Half Band filter length (N) must be 4m+3");
    let window = hamming(length);
    let half = (length / 2) as i64;
    let mut taps = vec![0.0f32; length];
    for x in 0..length {
        let offset = x as i64 - half;
        if offset == 0 {
            taps[x] = 0.5;
        } else if x % 2 == 0 {
            taps[x] = (((offset as f64 * PI / 2.0).sin() / (offset as f64 * PI)) * window[x] as f64) as f32;
        }
    }
    taps
}

pub struct HalfBandDecimator {
    coefficients: Vec<f32>,
    buffer: Vec<f32>,
    overlap: usize,
}

impl HalfBandDecimator {
    /// `RealDecimateX2Filter`: 63-tap Hamming half-band.
    pub fn x2() -> Self { Self::new(half_band_taps(63)) }

    pub fn new(coefficients: Vec<f32>) -> Self {
        assert!((coefficients.len() + 1) % 4 == 0, "Half-band filter coefficients must be odd-length (4m+3)");
        let overlap = coefficients.len() - 1;
        Self { coefficients, buffer: Vec::new(), overlap }
    }

    /// Filters and decimates by two; `samples.len()` must be even. History carries across calls.
    pub fn decimate(&mut self, samples: &[f32]) -> Vec<f32> {
        assert!(samples.len() % 2 == 0, "Samples array length must be an integer multiple of 2");
        let want = samples.len() + self.overlap;
        if self.buffer.is_empty() {
            self.buffer = vec![0.0; want];
        } else if self.buffer.len() != want {
            let mut tmp = vec![0.0; want];
            tmp[..self.overlap].copy_from_slice(&self.buffer[self.buffer.len() - self.overlap..]);
            self.buffer = tmp;
        } else {
            self.buffer.copy_within(samples.len().., 0);
        }
        self.buffer[self.overlap..].copy_from_slice(samples);

        let half = self.overlap / 2;
        let mut out = Vec::with_capacity(samples.len() / 2);
        for bp in (0..samples.len()).step_by(2) {
            let mut acc = 0.0f32;
            for cp in (0..half).step_by(2) {
                acc += self.coefficients[cp] * (self.buffer[bp + cp] + self.buffer[bp + (self.overlap - cp)]);
            }
            acc += self.buffer[bp + half] * 0.5f32;
            out.push(acc);
        }
        out
    }
}
