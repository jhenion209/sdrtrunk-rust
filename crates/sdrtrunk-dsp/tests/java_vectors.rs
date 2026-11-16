//! Replays vectors from the original Java DSP classes (fixtures/GenDsp.java.txt) against the Rust port.
//! Floats are exact IEEE bit patterns in hex. Line format: KIND|params/input|expected.
use sdrtrunk_dsp::baseband::baseband_taps;
use sdrtrunk_dsp::diff_demod::DifferentialDemodulator;
use sdrtrunk_dsp::fir::RealFir;
use sdrtrunk_dsp::rrc::root_raised_cosine;

fn floats(s: &str) -> Vec<f32> {
    s.split(',').map(|h| f32::from_bits(u32::from_str_radix(h, 16).unwrap())).collect()
}

/// Max absolute difference; also reports how many values are bit-identical.
fn compare(ctx: &str, got: &[f32], want: &[f32], tol: f32) -> (usize, usize) {
    assert_eq!(got.len(), want.len(), "{ctx}: length");
    let mut exact = 0;
    for (i, (g, w)) in got.iter().zip(want).enumerate() {
        if g.to_bits() == w.to_bits() { exact += 1; }
        assert!((g - w).abs() <= tol, "{ctx}[{i}]: rust {g:e} java {w:e} diff {:e}", (g - w).abs());
    }
    (exact, got.len())
}

#[test]
fn matches_java_oracle() {
    let text = include_str!("fixtures/java_dsp_vectors.txt");
    let mut fir: Option<RealFir> = None;
    let mut demods: std::collections::HashMap<String, DifferentialDemodulator> = Default::default();
    let (mut exact, mut total) = (0usize, 0usize);
    let mut tally = |r: (usize, usize)| { exact += r.0; total += r.1; };
    for (n, line) in text.lines().enumerate() {
        let f: Vec<&str> = line.split('|').collect();
        let ctx = format!("line {} {}", n + 1, f[0]);
        match f[0] {
            "RRC" => {
                let p: Vec<&str> = f[1].split(',').collect();
                let got = root_raised_cosine(p[0].parse().unwrap(), p[1].parse().unwrap(), p[2].parse().unwrap());
                tally(compare(&ctx, &got, &floats(f[2]), 1e-6));
            }
            "FIR" => {
                let (taps, input) = f[1].split_once(';').unwrap();
                let filt = fir.get_or_insert_with(|| RealFir::new(&floats(taps)));
                tally(compare(&ctx, &filt.filter(&floats(input)), &floats(f[2]), 1e-6));
            }
            "DEMOD" => {
                let parts: Vec<&str> = f[1].split(';').collect();
                let d = demods.entry(parts[0].to_string()).or_insert_with(|| DifferentialDemodulator::new(parts[0].parse().unwrap(), 4800));
                tally(compare(&ctx, &d.demodulate(&floats(parts[1]), &floats(parts[2])), &floats(f[2]), 1e-5));
            }
            "BASEBAND" => {
                let sr: f64 = f[1].parse().unwrap();
                let got = baseband_taps(sr as u32).expect("rate");
                tally(compare(&ctx, &got, &floats(f[2]), 0.0));
            }
            k => panic!("unknown {k}"),
        }
    }
    eprintln!("bit-identical floats: {exact}/{total}");
}
