//! Replays channelizer vectors from the original Java classes (fixtures/GenChan.java.txt).
//! Taps and extraction are compared bit-for-bit where the arithmetic is plain float math; the filter bank output
//! goes through an FFT (rustfft here, jtransforms in Java) so it is compared to a small relative tolerance.
use sdrtrunk_dsp::channel_calculator::ChannelCalculator;
use sdrtrunk_dsp::channelizer::{sinc_m2_channelizer_taps, sinc_m2_synthesizer_taps, ChannelExtractor, Channelizer, TwoChannelExtractor};

fn floats(s: &str) -> Vec<f32> {
    if s.is_empty() { return Vec::new(); }
    s.split(',').map(|h| f32::from_bits(u32::from_str_radix(h, 16).unwrap())).collect()
}

#[test]
fn designed_taps_match_java() {
    for line in include_str!("fixtures/java_chan_vectors.txt").lines().filter(|l| l.starts_with("TAPS|")) {
        let f: Vec<&str> = line.split('|').collect();
        let p: Vec<usize> = f[1].split(',').map(|v| v.parse().unwrap()).collect();
        let want = floats(f[2]);
        let got = sinc_m2_channelizer_taps(25000.0, p[0], p[1]).unwrap();
        assert_eq!(got.len(), want.len(), "channels {} length", p[0]);
        let mut exact = 0;
        let mut worst = 0.0f32;
        for (g, w) in got.iter().zip(&want) {
            if g.to_bits() == w.to_bits() { exact += 1; }
            worst = worst.max((g - w).abs());
        }
        println!("taps channels={}: {} of {} bit-identical, worst abs diff {:e}", p[0], exact, got.len(), worst);
        assert!(worst < 1e-9, "channels {}: worst diff {worst:e}", p[0]);
    }
}

#[test]
fn filter_bank_matches_java() {
    for line in include_str!("fixtures/java_chan_vectors.txt").lines().filter(|l| l.starts_with("CHAN|")) {
        let f: Vec<&str> = line.split('|').collect();
        let p: Vec<usize> = f[1].split(',').map(|v| v.parse().unwrap()).collect();
        let (channels, hops) = (p[0], p[1]);
        let (taps, input, want) = (floats(f[2]), floats(f[3]), floats(f[4]));
        let mut c = Channelizer::new(&taps, channels).unwrap();

        // Same uneven block schedule as the Java generator.
        let sizes = [channels + 2, 3 * channels / 2, 2, channels * 5 - 2, channels, 6];
        let (mut pos, mut step) = (0, 0);
        let mut got: Vec<f32> = Vec::new();
        while pos < input.len() {
            let n = sizes[step % sizes.len()].min(input.len() - pos);
            step += 1;
            c.receive(&input[pos..pos + n], |r| got.extend_from_slice(r));
            pos += n;
        }
        assert_eq!(got.len(), want.len(), "channels {channels}: result count (hops {hops})");
        let scale = want.iter().fold(0.0f32, |m, v| m.max(v.abs())).max(1.0);
        let worst = got.iter().zip(&want).fold(0.0f32, |m, (g, w)| m.max((g - w).abs()));
        println!("filter bank channels={channels}: {hops} hops, worst abs diff {worst:e} (peak {scale:e})");
        assert!(worst <= 2e-5 * scale, "channels {channels}: worst diff {worst:e}");
    }
}

#[test]
fn single_channel_extraction_matches_java() {
    let line = include_str!("fixtures/java_chan_vectors.txt").lines().find(|l| l.starts_with("EXTRACT|")).unwrap();
    let f: Vec<&str> = line.split('|').collect();
    let p: Vec<&str> = f[1].split(',').collect();
    let (rate, index, gain): (f64, usize, f32) = (p[0].parse().unwrap(), p[1].parse().unwrap(), p[2].parse().unwrap());
    let plan: Vec<(i64, usize)> = f[2].split(',').map(|s| { let (o, at) = s.split_once('@').unwrap(); (o.parse().unwrap(), at.parse().unwrap()) }).collect();
    let pairs = floats(f[3]);
    let expected: Vec<(Vec<f32>, Vec<f32>)> = f[4..].iter().map(|b| { let (i, q) = b.split_once(';').unwrap(); (floats(i), floats(q)) }).collect();

    let mut ex = ChannelExtractor::new(rate, index, gain);
    let mut got: Vec<(Vec<f32>, Vec<f32>)> = Vec::new();
    for n in 0..pairs.len() / 2 {
        for &(offset, at) in &plan {
            if at == n { ex.set_frequency_offset(offset); }
        }
        // Full result array with only the wanted channel populated; other bins must be ignored.
        let mut result = vec![0.0f32; 16];
        result[index * 2] = pairs[2 * n];
        result[index * 2 + 1] = pairs[2 * n + 1];
        ex.push(&result, |i, q| got.push((i.to_vec(), q.to_vec())));
    }
    assert_eq!(got.len(), expected.len(), "buffer count");
    let (mut exact, mut total) = (0, 0);
    for (b, (g, w)) in got.iter().zip(&expected).enumerate() {
        for (gv, wv) in g.0.iter().chain(&g.1).zip(w.0.iter().chain(&w.1)) {
            total += 1;
            if gv.to_bits() == wv.to_bits() { exact += 1; }
            assert!((gv - wv).abs() <= 1e-5, "buffer {b}: rust {gv:e} java {wv:e}");
        }
    }
    println!("extraction: {exact} of {total} floats bit-identical");
    assert_eq!(exact, total, "extraction path should be bit-identical (same float ops)");
}

fn line_of<'a>(text: &'a str, kind: &str) -> &'a str {
    text.lines().find(|l| l.starts_with(kind)).unwrap_or_else(|| panic!("no {kind} record"))
}

#[test]
fn synthesis_taps_match_java() {
    let text = include_str!("fixtures/java_chan_vectors.txt");
    let f: Vec<&str> = line_of(text, "SYN|").split('|').collect();
    let p: Vec<&str> = f[1].split(',').collect();
    let want = floats(f[2]);
    let got = sinc_m2_synthesizer_taps(p[0].parse().unwrap(), p[1].parse().unwrap(), p[2].parse().unwrap(), p[3].parse().unwrap()).unwrap();
    assert_eq!(got, want, "synthesis taps must be bit-identical");
}

#[test]
fn two_channel_extraction_matches_java() {
    let text = include_str!("fixtures/java_chan_vectors.txt");
    let syn = floats(line_of(text, "SYN|").split('|').nth(2).unwrap());
    let f: Vec<&str> = line_of(text, "EXTRACT2|").split('|').collect();
    let p: Vec<&str> = f[1].split(',').collect();
    let (rate, i1, i2, gain): (f64, usize, usize, f32) = (p[0].parse().unwrap(), p[1].parse().unwrap(), p[2].parse().unwrap(), p[3].parse().unwrap());
    let plan: Vec<(i64, usize)> = f[2].split(',').map(|s| { let (o, at) = s.split_once('@').unwrap(); (o.parse().unwrap(), at.parse().unwrap()) }).collect();
    let vals = floats(f[3]);
    let expected: Vec<(Vec<f32>, Vec<f32>)> = f[4..].iter().map(|b| { let (i, q) = b.split_once(';').unwrap(); (floats(i), floats(q)) }).collect();

    let mut ex = TwoChannelExtractor::new(rate, [i1, i2], &syn, gain);
    let mut got: Vec<(Vec<f32>, Vec<f32>)> = Vec::new();
    for n in 0..vals.len() / 4 {
        for &(offset, at) in &plan {
            if at == n { ex.set_frequency_offset(offset); }
        }
        let mut result = vec![0.0f32; 16];
        result[i1 * 2..i1 * 2 + 2].copy_from_slice(&vals[4 * n..4 * n + 2]);
        result[i2 * 2..i2 * 2 + 2].copy_from_slice(&vals[4 * n + 2..4 * n + 4]);
        ex.push(&result, |i, q| got.push((i.to_vec(), q.to_vec())));
    }
    assert_eq!(got.len(), expected.len(), "buffer count");
    let (mut exact, mut total) = (0, 0);
    for (b, (g, w)) in got.iter().zip(&expected).enumerate() {
        for (gv, wv) in g.0.iter().chain(&g.1).zip(w.0.iter().chain(&w.1)) {
            total += 1;
            if gv.to_bits() == wv.to_bits() { exact += 1; }
            assert!((gv - wv).abs() <= 1e-5, "buffer {b}: rust {gv:e} java {wv:e}");
        }
    }
    println!("two-channel extraction: {exact} of {total} floats bit-identical");
    assert_eq!(exact, total);
}

#[test]
fn channel_calculator_matches_java() {
    let (mut checked, mut errors, mut multi) = (0, 0, 0);
    let mut calcs: Vec<(String, ChannelCalculator)> = Vec::new();
    for line in include_str!("fixtures/java_chan_vectors.txt").lines().filter(|l| l.starts_with("CALC|")) {
        let f: Vec<&str> = line.split('|').collect();
        let key = f[1].to_string();
        if !calcs.iter().any(|(k, _)| *k == key) {
            let p: Vec<&str> = key.split(',').collect();
            calcs.push((key.clone(), ChannelCalculator::new(p[0].parse().unwrap(), p[1].parse().unwrap(), p[2].parse().unwrap(), 2.0)));
        }
        let calc = &calcs.iter().find(|(k, _)| *k == key).unwrap().1;
        let (freq, bw) = f[2].split_once(',').unwrap();
        let (freq, bw): (i64, i64) = (freq.parse().unwrap(), bw.parse().unwrap());
        let got = calc.channel_indexes(freq, bw);
        if f[3] == "ERR" {
            assert!(got.is_err(), "{line}: expected an error, got {got:?}");
            errors += 1;
        } else {
            let want: Vec<usize> = f[3].split(' ').map(|v| v.parse().unwrap()).collect();
            let got = got.unwrap_or_else(|e| panic!("{line}: unexpected error {e}"));
            assert_eq!(got, want, "{line}");
            assert_eq!(calc.center_frequency_for_indexes(&got), f[4].parse::<i64>().unwrap(), "{line}: center");
            if got.len() > 1 { multi += 1; }
        }
        checked += 1;
    }
    println!("calculator: {checked} cases ({errors} errors, {multi} multi-bin) match Java");
    assert!(checked > 10_000);
}

/// End to end: a tone at a known frequency, pushed through channelizer -> calculator -> extractor, must come out
/// at 0 Hz with steady amplitude. Covers both the one-bin and the two-bin (straddling) paths and the offset sign.
#[test]
fn tone_lands_at_baseband() {
    let (rate, center) = (2_500_000.0f64, 852_500_000i64);
    let mut calc = ChannelCalculator::new(rate, 100, center as f64, 2.0);
    calc.set_center_frequency(center as f64);
    // 80 kHz: inside bin 3 only. 84 kHz with 12.5 kHz bandwidth: straddles the bin 3/4 edge at 87.5 kHz.
    for (tone_offset, expect_bins) in [(80_000i64, 1usize), (84_000, 2)] {
        let wanted = center + tone_offset;
        let indexes = calc.channel_indexes(wanted, 12_500).unwrap();
        assert_eq!(indexes.len(), expect_bins, "tone +{tone_offset}");
        let offset = calc.frequency_offset(&indexes, wanted, 0);

        let mut chan = Channelizer::for_sample_rate(rate).unwrap();
        let channel_rate = calc.channel_sample_rate();
        let gain = chan.channels() as f32;
        let syn = sinc_m2_synthesizer_taps(channel_rate, calc.channel_bandwidth(), 2, 9).unwrap();
        enum Ex { One(ChannelExtractor), Two(TwoChannelExtractor) }
        let mut ex = if indexes.len() == 1 {
            let mut e = ChannelExtractor::new(channel_rate, indexes[0], gain);
            e.set_frequency_offset(offset);
            Ex::One(e)
        } else {
            let mut e = TwoChannelExtractor::new(channel_rate, [indexes[0], indexes[1]], &syn, gain);
            e.set_frequency_offset(offset);
            Ex::Two(e)
        };

        let samples = 2048 * 50 * 3; // enough hops for three output buffers
        let w = 2.0 * std::f64::consts::PI * tone_offset as f64 / rate;
        let input: Vec<f32> = (0..samples).flat_map(|n| { let p = w * n as f64; [p.cos() as f32, p.sin() as f32] }).collect();

        let mut buffers: Vec<(Vec<f32>, Vec<f32>)> = Vec::new();
        chan.receive(&input, |r| match &mut ex {
            Ex::One(e) => e.push(r, |i, q| buffers.push((i.to_vec(), q.to_vec()))),
            Ex::Two(e) => e.push(r, |i, q| buffers.push((i.to_vec(), q.to_vec()))),
        });
        assert!(buffers.len() >= 2, "need at least two output buffers, got {}", buffers.len());

        let (i, q) = &buffers[1]; // skip the filter warm-up buffer
        let mag: Vec<f64> = i.iter().zip(q).map(|(a, b)| ((*a as f64).powi(2) + (*b as f64).powi(2)).sqrt()).collect();
        let mean = mag.iter().sum::<f64>() / mag.len() as f64;
        let var = mag.iter().map(|m| (m - mean).powi(2)).sum::<f64>() / mag.len() as f64;
        let mut dphi = 0.0;
        for n in 1..i.len() {
            let (a, b, c, d) = (i[n - 1] as f64, q[n - 1] as f64, i[n] as f64, q[n] as f64);
            dphi += (d * a - c * b).atan2(c * a + d * b);
        }
        let hz = dphi / (i.len() - 1) as f64 * channel_rate / (2.0 * std::f64::consts::PI);
        println!("tone +{tone_offset} Hz via {} bin(s): mean |z| {mean:.3}, amplitude ripple {:.3}, residual {hz:.1} Hz", indexes.len(), var.sqrt() / mean);
        assert!(mean > 0.3, "tone lost: mean magnitude {mean}");
        assert!(var.sqrt() / mean < 0.1, "unsteady amplitude");
        assert!(hz.abs() < 100.0, "tone not at baseband: residual {hz} Hz");
    }
}
