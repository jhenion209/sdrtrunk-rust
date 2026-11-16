//! Replays half-band decimator and C4FM front-end vectors from the original Java classes
//! (fixtures/GenFront.java.txt). Floats are IEEE bit patterns in hex.
use sdrtrunk_dsp::decimate::{half_band_taps, HalfBandDecimator};
use sdrtrunk_dsp::front_end::C4fmFrontEnd;

fn floats(s: &str) -> Vec<f32> {
    if s.is_empty() { return Vec::new(); }
    s.split(',').map(|h| f32::from_bits(u32::from_str_radix(h, 16).unwrap())).collect()
}

/// Number of bit-identical values; panics when any value differs by more than `tol`.
fn compare(ctx: &str, got: &[f32], want: &[f32], tol: f32) -> usize {
    assert_eq!(got.len(), want.len(), "{ctx}: length");
    let mut exact = 0;
    for (n, (g, w)) in got.iter().zip(want).enumerate() {
        if g.to_bits() == w.to_bits() { exact += 1; }
        assert!((g - w).abs() <= tol, "{ctx}[{n}]: rust {g:e} java {w:e}");
    }
    exact
}

#[test]
fn half_band_taps_and_decimation_match_java() {
    let text = include_str!("fixtures/java_front_vectors.txt");
    let taps_line = text.lines().find(|l| l.starts_with("HBTAPS|")).unwrap();
    let want = floats(taps_line.split('|').nth(2).unwrap());
    let exact = compare("HBTAPS", &half_band_taps(63), &want, 1e-9);
    println!("half-band taps: {exact} of {} bit-identical", want.len());
    assert_eq!(exact, want.len());

    let mut dec = HalfBandDecimator::x2();
    let (mut exact, mut total) = (0, 0);
    for (n, line) in text.lines().filter(|l| l.starts_with("DEC|")).enumerate() {
        let f: Vec<&str> = line.split('|').collect();
        let want = floats(f[2]);
        let got = dec.decimate(&floats(f[1]));
        exact += compare(&format!("DEC block {n}"), &got, &want, 1e-6);
        total += want.len();
    }
    println!("decimation: {exact} of {total} bit-identical");
    assert_eq!(exact, total);
}

#[test]
fn front_end_matches_java() {
    let text = include_str!("fixtures/java_front_vectors.txt");
    let info: Vec<&str> = text.lines().find(|l| l.starts_with("FRONTINFO|")).unwrap().split('|').nth(1).unwrap().split(',').collect();
    let mut fe = C4fmFrontEnd::new(50_000.0).unwrap();
    assert_eq!(fe.decimated_sample_rate(), info[0].parse::<f32>().unwrap());
    assert_eq!(fe.samples_per_symbol(), info[1].parse::<f32>().unwrap());

    let (mut exact, mut total) = (0, 0);
    for (n, line) in text.lines().filter(|l| l.starts_with("FRONT|")).enumerate() {
        let f: Vec<&str> = line.split('|').collect();
        let (i, q) = f[1].split_once(';').unwrap();
        let want = floats(f[2]);
        let got = fe.process(&floats(i), &floats(q));
        exact += compare(&format!("FRONT block {n}"), &got, &want, 1e-5);
        total += want.len();
    }
    println!("front end: {exact} of {total} bit-identical");
    assert_eq!(exact, total, "front end should be bit-identical");
}

#[test]
fn unsupported_rates_are_errors() {
    assert!(C4fmFrontEnd::new(9_600.0).is_err());
    assert!(C4fmFrontEnd::new(200_000.0).is_err()); // would need decimation by 8
    assert!(C4fmFrontEnd::new(25_000.0).is_ok()); // no decimation
}
