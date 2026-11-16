//! Streaming real FIR filter. Port of sdrtrunk `RealFIRFilter` (scalar). History carries across calls.

pub struct RealFir {
    /// Coefficients reversed, as in Java, so the inner loop runs forward over the sample buffer.
    coefficients: Vec<f32>,
    buffer: Vec<f32>,
    overlap: usize,
}

impl RealFir {
    pub fn new(coefficients: &[f32]) -> Self {
        let mut c = coefficients.to_vec();
        c.reverse();
        let overlap = c.len() - 1;
        Self { buffer: vec![0.0; c.len()], coefficients: c, overlap }
    }

    pub fn filter(&mut self, samples: &[f32]) -> Vec<f32> {
        let n = samples.len();
        let want = n + self.overlap;
        if self.buffer.len() != want {
            let mut tmp = vec![0.0; want];
            tmp[..self.overlap].copy_from_slice(&self.buffer[self.buffer.len() - self.overlap..]);
            self.buffer = tmp;
        } else {
            self.buffer.copy_within(n.., 0);
        }
        self.buffer[self.overlap..].copy_from_slice(samples);
        (0..n)
            .map(|p| {
                let mut acc = 0f32;
                for (k, &c) in self.coefficients.iter().enumerate() {
                    acc += self.buffer[p + k] * c;
                }
                acc
            })
            .collect()
    }
}
