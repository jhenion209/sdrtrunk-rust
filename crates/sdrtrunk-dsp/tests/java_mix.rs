//! Replays oscillator and mixer vectors from the original Java classes (fixtures/GenMix.java.txt).
//! Line format: OSC|freq,rate,count|i;q  and  MIX|i;q|i;q  with floats as IEEE bit patterns in hex.
use sdrtrunk_dsp::mixer::{ComplexMixer, ComplexOscillator};

fn floats(s: &str) -> Vec<f32> {
    s.split(',').map(|h| f32::from_bits(u32::from_str_radix(h, 16).unwrap())).collect()
}

fn pair(s: &str) -> (Vec<f32>, Vec<f32>) {
    let (i, q) = s.split_once(';').unwrap();
    (floats(i), floats(q))
}

/// Returns the number of values that differ in any bit; panics if any differs by more than `tol`.
fn compare(ctx: &str, got: &[f32], want: &[f32], tol: f32) -> usize {
    assert_eq!(got.len(), want.len(), "{ctx}: length");
    let mut inexact = 0;
    for (n, (g, w)) in got.iter().zip(want).enumerate() {
        if g.to_bits() != w.to_bits() { inexact += 1; }
        assert!((g - w).abs() <= tol, "{ctx}[{n}]: rust {g:e} java {w:e}");
    }
    inexact
}

#[test]
fn matches_java_oracle() {
    let text = include_str!("fixtures/java_mix_vectors.txt");
    let mut oscs: Vec<((String, String), ComplexOscillator)> = Vec::new();
    let mut mixer = ComplexMixer::new(-250000.0, 10_000_000.0);
    let (mut inexact, mut total, mut mixes) = (0usize, 0usize, 0usize);
    for (n, line) in text.lines().enumerate() {
        let f: Vec<&str> = line.split('|').collect();
        let ctx = format!("line {} {}", n + 1, f[0]);
        match f[0] {
            "OSC" => {
                let p: Vec<&str> = f[1].split(',').collect();
                let key = (p[0].to_string(), p[1].to_string());
                let idx = match oscs.iter().position(|(k, _)| *k == key) {
                    Some(i) => i,
                    None => { oscs.push((key, ComplexOscillator::new(p[0].parse().unwrap(), p[1].parse().unwrap()))); oscs.len() - 1 }
                };
                let (gi, gq) = oscs[idx].1.generate(p[2].parse().unwrap());
                let (wi, wq) = pair(f[2]);
                inexact += compare(&ctx, &gi, &wi, 1e-6) + compare(&ctx, &gq, &wq, 1e-6);
                total += gi.len() * 2;
            }
            "MIX" => {
                mixes += 1;
                if mixes == 3 { mixer.set_frequency(412345.0); }
                let (xi, xq) = pair(f[1]);
                let (gi, gq) = mixer.mix(&xi, &xq);
                let (wi, wq) = pair(f[2]);
                inexact += compare(&ctx, &gi, &wi, 1e-5) + compare(&ctx, &gq, &wq, 1e-5);
                total += gi.len() * 2;
            }
            other => panic!("unknown record {other}"),
        }
    }
    println!("oscillator+mixer: {} of {} floats bit-identical", total - inexact, total);
    assert!(total > 0 && mixes == 4);
}

#[test]
fn interleaved_matches_deinterleaved() {
    let i: Vec<f32> = (0..300).map(|n| (n as f32 * 0.37).sin()).collect();
    let q: Vec<f32> = (0..300).map(|n| (n as f32 * 0.11).cos()).collect();
    let interleaved: Vec<f32> = i.iter().zip(&q).flat_map(|(a, b)| [*a, *b]).collect();
    let mut a = ComplexMixer::new(2500.0, 25_000.0);
    let mut b = ComplexMixer::new(2500.0, 25_000.0);
    assert_eq!(a.mix(&i, &q), b.mix_interleaved(&interleaved));
}
