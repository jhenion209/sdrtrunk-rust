//! Differential phase demodulator. Port of sdrtrunk `DifferentialDemodulatorFloatScalar`.
//! Output per input sample is the phase change over ~one symbol: atan2 of conj(prev) * interpolated(current).

use crate::interpolator::interpolate;

pub struct DifferentialDemodulator {
    samples_per_symbol: f32,
    mu: f32,
    interpolation_offset: usize,
    buffer_overlap: usize,
    i_buf: Vec<f32>,
    q_buf: Vec<f32>,
}

impl DifferentialDemodulator {
    pub fn new(sample_rate: f64, symbol_rate: u32) -> Self {
        let sample_rate = sample_rate as f32;
        let samples_per_symbol = sample_rate / symbol_rate as f32;
        let mu = samples_per_symbol % 1.0;
        let mut interpolation_offset = samples_per_symbol.floor() as i32 - 4;
        let mut buffer_overlap = samples_per_symbol.floor() as i32 + 4;
        while interpolation_offset < 0 {
            interpolation_offset += 1;
            buffer_overlap += 1;
        }
        Self {
            samples_per_symbol,
            mu,
            interpolation_offset: interpolation_offset as usize,
            buffer_overlap: buffer_overlap as usize,
            i_buf: vec![0.0; 20],
            q_buf: vec![0.0; 20],
        }
    }

    pub fn samples_per_symbol(&self) -> f32 {
        self.samples_per_symbol
    }

    /// Samples between the two points each phase difference is taken from (the interpolator's value sits 3 + mu
    /// past its offset, so this is one sample short of a symbol, as in the Java original).
    pub fn delay_samples(&self) -> f32 {
        self.interpolation_offset as f32 + 3.0 + self.mu
    }

    pub fn demodulate(&mut self, i: &[f32], q: &[f32]) -> Vec<f32> {
        assert_eq!(i.len(), q.len());
        let n = i.len();
        let overlap = self.buffer_overlap;
        // Keep the tail of the previous buffer as history, then append the new block.
        for buf in [&mut self.i_buf, &mut self.q_buf] {
            let len = buf.len();
            buf.copy_within(len - overlap.., 0);
            buf.resize(n + overlap, 0.0);
        }
        self.i_buf[overlap..].copy_from_slice(i);
        self.q_buf[overlap..].copy_from_slice(q);
        (0..n)
            .map(|x| {
                let (ip, qpc) = (self.i_buf[x], -self.q_buf[x]);
                let off = self.interpolation_offset + x;
                let ic = interpolate(&self.i_buf, off, self.mu);
                let qc = interpolate(&self.q_buf, off, self.mu);
                let di = ip * ic - qpc * qc;
                let dq = ip * qc + ic * qpc;
                (dq as f64).atan2(di as f64) as f32
            })
            .collect()
    }
}
