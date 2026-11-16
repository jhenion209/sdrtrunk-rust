//! 8-tap polyphase interpolator with 128 fractional steps. Port of sdrtrunk `InterpolatorScalar`.

use crate::interp_taps::TAPS;

pub const NTAPS: usize = 8;
pub const NSTEPS: usize = 128;

/// Interpolate between `samples[offset + 3]` and `samples[offset + 4]` at fractional position `mu` in [0, 1).
pub fn interpolate(samples: &[f32], offset: usize, mu: f32) -> f32 {
    assert!(samples.len() >= offset + NTAPS);
    let t = &TAPS[(NSTEPS as f32 * mu) as usize];
    // Java accumulates sequentially in this exact order (taps reversed against samples); keep it for bit-parity.
    let mut acc = t[7] * samples[offset];
    for k in 1..NTAPS {
        acc += t[7 - k] * samples[offset + k];
    }
    acc
}
