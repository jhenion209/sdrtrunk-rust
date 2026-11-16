//! Root-raised-cosine taps. Port of sdrtrunk `FilterFactory.getRootRaisedCosine(double, int, float)`.
//!
//! Quirk preserved on purpose: the function halves `samples_per_symbol` internally (callers of the "2x"
//! overload pass twice the rate). `P25P1DecoderC4FM` calls it directly with the true samples-per-symbol, so the
//! P25 filter is designed for half that value. Do not "fix" this without changing the Java behavior too.

use std::f64::consts::PI;

pub fn root_raised_cosine(samples_per_symbol: f64, symbol_count: usize, alpha: f32) -> Vec<f32> {
    let sps = samples_per_symbol / 2.0;
    let taps = (sps * symbol_count as f64).round() as usize;
    let alpha = alpha as f64;
    let mut c = vec![0f32; taps];
    let mut scale = 0f32;
    for x in 0..taps {
        let index = x as f64 - (taps as f32 / 2.0f32) as f64;
        let x1 = PI * index / sps;
        let mut x2 = 4.0 * alpha * index / sps;
        let mut x3 = x2 * x2 - 1.0;
        let (numerator, denominator);
        if x3.abs() >= 0.000001 {
            numerator = if x != taps / 2 {
                ((1.0 + alpha) * x1).cos() + ((1.0 - alpha) * x1).sin() / (4.0 * alpha * index / sps)
            } else {
                ((1.0 + alpha) * x1).cos() + (1.0 - alpha) * PI / (4.0 * alpha)
            };
            denominator = x3 * PI;
        } else {
            if alpha == 1.0 {
                c[x] = -1.0;
                continue;
            }
            x3 = (1.0 - alpha) * x1;
            x2 = (1.0 + alpha) * x1;
            numerator = x2.sin() * (1.0 + alpha) * PI - x3.cos() * ((1.0 - alpha) * PI * sps) / (4.0 * alpha * index)
                + x3.sin() * sps * sps / (4.0 * alpha * index * index);
            denominator = -32.0 * PI * alpha * alpha * index / sps;
        }
        c[x] = (4.0 * alpha * numerator / denominator) as f32;
        scale += c[x];
    }
    for v in &mut c {
        *v /= scale;
    }
    c
}
