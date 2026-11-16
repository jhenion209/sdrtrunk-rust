//! Power spectrum of the tuner's IQ for the spectrum/waterfall display: Hann-windowed FFT, averaged over consecutive
//! segments, DC in the middle, reduced to a fixed width by keeping each group's peak (narrow signals stay visible).
use rustfft::num_complex::Complex32;
use rustfft::{Fft, FftPlanner};
use std::sync::Arc;

pub struct Spectrum {
    n: usize,
    width: usize,
    fft: Arc<dyn Fft<f32>>,
    window: Vec<f32>,
    buf: Vec<Complex32>,
}

impl Spectrum {
    /// `n`-point FFT (power of two), output `width` points (`n` must be a multiple of `width`).
    pub fn new(n: usize, width: usize) -> Spectrum {
        assert!(n % width == 0, "FFT size must be a multiple of the output width");
        let window = (0..n).map(|i| 0.5 - 0.5 * (2.0 * std::f32::consts::PI * i as f32 / n as f32).cos()).collect();
        Spectrum { n, width, fft: FftPlanner::new().plan_fft_forward(n), window, buf: vec![Complex32::new(0.0, 0.0); n] }
    }

    /// dB (relative to full scale, 0 dB = a full-scale tone) from interleaved int16 IQ on the ±2048 scale, averaging up to
    /// `max_segments` consecutive `n`-sample segments. Lowest frequency first. None if the block is shorter than one segment.
    pub fn frame(&mut self, iq: &[i16], max_segments: usize) -> Option<Vec<f32>> {
        self.frame_with(iq.len(), max_segments, |k| iq[k] as f32 / 2048.0)
    }

    /// As `frame`, from interleaved f32 IQ where full scale is 1.0 (a channel's samples).
    pub fn frame_f32(&mut self, iq: &[f32], max_segments: usize) -> Option<Vec<f32>> {
        self.frame_with(iq.len(), max_segments, |k| iq[k])
    }

    fn frame_with(&mut self, len: usize, max_segments: usize, sample: impl Fn(usize) -> f32) -> Option<Vec<f32>> {
        let segments = (len / 2 / self.n).min(max_segments);
        if segments == 0 { return None; }
        let mut power = vec![0f32; self.n];
        for seg in 0..segments {
            let base = seg * self.n * 2;
            for (i, c) in self.buf.iter_mut().enumerate() {
                let w = self.window[i];
                *c = Complex32::new(sample(base + 2 * i) * w, sample(base + 2 * i + 1) * w);
            }
            self.fft.process(&mut self.buf);
            for (p, c) in power.iter_mut().zip(&self.buf) { *p += c.norm_sqr(); }
        }
        // Hann coherent gain is 1/2: a full-scale tone gives (n/2)^2 in its bin.
        let full = (self.n as f32 / 2.0).powi(2) * segments as f32;
        let half = self.n / 2;
        let shifted = power[half..].iter().chain(&power[..half]);
        let db: Vec<f32> = shifted.map(|&p| 10.0 * (p / full).max(1e-20).log10()).collect();
        let group = self.n / self.width;
        Some(db.chunks(group).map(|c| c.iter().copied().fold(f32::MIN, f32::max)).collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tone(n: usize, cycles_per_sample: f64, amp: f64) -> Vec<i16> {
        (0..n).flat_map(|i| {
            let ph = 2.0 * std::f64::consts::PI * cycles_per_sample * i as f64;
            [(amp * ph.cos()) as i16, (amp * ph.sin()) as i16]
        }).collect()
    }

    #[test]
    fn tone_lands_in_the_right_place() {
        let mut s = Spectrum::new(2048, 1024);
        // +fs/4: three quarters across (DC in the middle); a full-scale tone reads about 0 dB.
        let f = s.frame(&tone(8192, 0.25, 2047.0), 4).unwrap();
        assert_eq!(f.len(), 1024);
        let peak = f.iter().enumerate().max_by(|a, b| a.1.total_cmp(b.1)).unwrap();
        assert_eq!(peak.0, 768);
        assert!(peak.1.abs() < 0.5, "{}", peak.1);
        // -fs/8: three eighths across, 20 dB lower for a tenth of the amplitude.
        let f = s.frame(&tone(4096, -0.125, 204.7), 4).unwrap();
        let peak = f.iter().enumerate().max_by(|a, b| a.1.total_cmp(b.1)).unwrap();
        assert_eq!(peak.0, 384);
        assert!((peak.1 + 20.0).abs() < 0.5, "{}", peak.1);
        assert!(f[100] < -60.0, "far from the tone is quiet: {}", f[100]);
        assert!(s.frame(&tone(1000, 0.1, 100.0), 4).is_none(), "shorter than one FFT");
        // The same tone as f32 (full scale 1.0) reads the same.
        let f32iq: Vec<f32> = tone(8192, 0.25, 2047.0).iter().map(|&v| v as f32 / 2048.0).collect();
        let f = s.frame_f32(&f32iq, 4).unwrap();
        let peak = f.iter().enumerate().max_by(|a, b| a.1.total_cmp(b.1)).unwrap();
        assert_eq!(peak.0, 768);
        assert!(peak.1.abs() < 0.5, "{}", peak.1);
    }
}
