//! Polyphase channelizer, ported from sdrtrunk `dsp.filter.channelizer`:
//! `ComplexPolyphaseChannelizerM2` (2x-oversampled non-maximally decimated filter bank),
//! `FilterFactory.getSincM2Channelizer` (prototype filter design) and `OneChannelOutputProcessor`
//! (single-channel extraction, frequency translation and gain).
//!
//! Not ported yet: two-channel synthesis (needed when a signal straddles two bins) and the channel calculator.
//! The Java threading (dispatchers, arena of result buffers, warm-up) is dropped; processing is synchronous.
use crate::mixer::ComplexMixer;
use rustfft::num_complex::Complex32;
use rustfft::{Fft, FftPlanner};
use std::f64::consts::PI;
use std::sync::Arc;

const PERFECT_RECONSTRUCTION_GAIN_AT_BAND_EDGE: f64 = -6.020599842071533; // decibel(0.5, 0.0)
const MARGIN_OF_ERROR: f64 = 0.0003;
/// Taps per polyphase sub-channel used by sdrtrunk's PolyphaseChannelManager.
pub const TAPS_PER_CHANNEL: usize = 9;
/// Buffer size of the Java SampleAssembler; mixer oscillator state is per-buffer so it affects output bits.
const ASSEMBLY_BUFFER: usize = 2048;

// ---------------------------------------------------------------- prototype filter design

fn bessel_zeroth_order(x: f32) -> f32 {
    let mut f = 1.0f32;
    let x2 = x * x * 0.25f32;
    let mut xc = x2;
    let mut v = 1.0f32 + x2;
    for i in 2..100 {
        f *= i as f32;
        xc *= x2;
        let a = xc / (f * f);
        v += a;
        if (a as f64) < 1e-20 {
            break;
        }
    }
    v
}

fn kaiser_beta(attenuation: f32) -> f32 {
    if attenuation > 50.0 {
        0.1102f32 * (attenuation - 8.7f32)
    } else if attenuation >= 21.0 {
        (0.5842f32 * ((attenuation - 21.0) as f64).powf(0.4) as f32) + (0.07886f32 * (attenuation - 21.0f32))
    } else {
        0.0
    }
}

fn kaiser_window(length: usize, attenuation: f32) -> Vec<f32> {
    let beta = kaiser_beta(attenuation);
    let beta_bessel = bessel_zeroth_order(beta);
    (0..length)
        .map(|x| {
            let r = 2.0 * (x as f64) / ((length - 1) as f64) - 1.0;
            let temp = beta * ((1.0 - r * r).sqrt() as f32);
            bessel_zeroth_order(temp) / beta_bessel
        })
        .collect()
}

/// Kaiser-windowed sinc low-pass; `length` must be odd, `cutoff` is 0..0.5 of the sample rate.
fn kaiser_sinc(length: usize, cutoff: f64, attenuation: f32) -> Result<Vec<f32>, String> {
    if length % 2 == 0 {
        return Err("Sinc filters must be odd-length".into());
    }
    let half = length / 2;
    let window = kaiser_window(length, attenuation);
    let scalor = 2.0 * cutoff;
    let pi_scalor = PI * scalor;
    let mut c = vec![0.0f32; length];
    c[half] = (1.0 * scalor * window[half] as f64) as f32;
    for x in 1..=half {
        let a = pi_scalor * x as f64;
        let coefficient = (scalor * a.sin() / a) * window[half + x] as f64;
        c[half + x] = coefficient as f32;
        c[half - x] = coefficient as f32;
    }
    Ok(c)
}

/// Magnitude response in dB at `frequency` (0..0.5 of the sample rate).
fn evaluate(filter: &[f32], frequency: f64) -> f64 {
    let (mut re, mut im) = (0.0f64, 0.0f64);
    for (x, &t) in filter.iter().enumerate() {
        let angle = PI * frequency * x as f64;
        re += t as f64 * angle.cos();
        im += t as f64 * angle.sin();
    }
    ((10.0 * (re * re + im * im).log10()) as f32) as f64
}

fn matches_objective(a: f64) -> bool {
    (a - PERFECT_RECONSTRUCTION_GAIN_AT_BAND_EDGE).abs() <= MARGIN_OF_ERROR
}

/// Designs the perfect-reconstruction prototype filter (-6 dB at the channel edge) for the channelizer.
/// `channel_bandwidth` is sample_rate / channels. Returns `channels * taps_per_channel` taps (a leading
/// zero is prepended to the odd-length design).
pub fn sinc_m2_channelizer_taps(channel_bandwidth: f64, channels: usize, taps_per_channel: usize) -> Result<Vec<f32>, String> {
    let mut current_taps_per_channel = taps_per_channel;
    let mut filter_length = channels * current_taps_per_channel - 1;
    let sample_rate = channel_bandwidth * channels as f64;
    let band_edge = channel_bandwidth / sample_rate;
    let mut cutoff = band_edge / 2.0;
    let mut increment = cutoff * 0.1;

    let mut taps = kaiser_sinc(filter_length, cutoff, 80.0)?;
    let mut response = evaluate(&taps, band_edge);
    let increment_threshold = 1.0 / sample_rate;

    while increment > increment_threshold {
        if matches_objective(response) && cutoff + increment <= band_edge {
            let higher = kaiser_sinc(filter_length, cutoff + increment, 80.0)?;
            let higher_response = evaluate(&higher, band_edge);
            if matches_objective(higher_response) {
                cutoff += increment;
                taps = higher;
                response = higher_response;
            } else {
                increment /= 2.0;
            }
        } else if matches_objective(response) {
            increment /= 2.0;
        } else {
            cutoff -= increment;
            if cutoff <= 0.0 {
                current_taps_per_channel += 1;
                if current_taps_per_channel > taps_per_channel + 10 {
                    return Err(format!(
                        "Couldn't design filter with taps per channel count in the range of {} - {} Sample Rate:{} Channels:{}",
                        taps_per_channel, taps_per_channel + 10, sample_rate, channels
                    ));
                }
                filter_length = channels * current_taps_per_channel - 1;
                cutoff = channel_bandwidth / sample_rate;
                increment = cutoff * 0.1;
            }
            taps = kaiser_sinc(filter_length, cutoff, 80.0)?;
            response = evaluate(&taps, band_edge);
        }
    }

    if !matches_objective(response) {
        return Err("Cannot design filter to specifications".into());
    }
    let mut extended = Vec::with_capacity(taps.len() + 1);
    extended.push(0.0);
    extended.extend_from_slice(&taps);
    Ok(extended)
}

/// Number of (even) channels of at least 25 kHz that fit in `sample_rate`.
pub fn channel_count(sample_rate: f64) -> usize {
    let mut channels = (sample_rate / 25_000.0) as usize;
    if channels % 2 != 0 {
        channels -= 1;
    }
    channels
}

// ---------------------------------------------------------------- filter bank

fn top_block_map(channels: usize) -> Vec<usize> {
    let mut map = vec![0usize; channels * 2];
    let block = channels / 2;
    for channel in 0..block {
        let new_index = 2 * channel;
        let original = 2 * (block - channel - 1);
        let offset = 2 * block;
        map[original] = new_index;
        map[original + 1] = new_index + 1;
        map[offset + original] = offset + new_index;
        map[offset + original + 1] = offset + new_index + 1;
    }
    map
}

fn middle_block_map(channels: usize) -> Vec<usize> {
    let mut map = vec![0usize; channels * 2];
    let block = channels / 2;
    for channel in 0..block {
        let new_index = 2 * channel;
        let original = 2 * (block - channel - 1);
        let offset = 2 * block;
        map[offset + original] = new_index;
        map[offset + original + 1] = new_index + 1;
        map[original] = offset + new_index;
        map[original + 1] = offset + new_index + 1;
    }
    map
}

/// Interleaved I/Q prototype filter, rearranged to align with the inline sample buffer.
fn aligned_filter(coefficients: &[f32], channels: usize, taps_per_channel: usize) -> Vec<f32> {
    let mut filter = vec![0.0f32; channels * taps_per_channel * 2];
    for (n, &c) in coefficients.iter().enumerate() {
        filter[2 * n] = c;
        filter[2 * n + 1] = c;
    }
    let block = channels;
    for x in (0..filter.len()).step_by(block) {
        for y in 0..block / 2 {
            filter.swap(x + y, x + (block - y - 1));
        }
    }
    filter
}

/// 2x-oversampled polyphase filter bank. Every `channels / 2` complex input samples it emits one result of
/// `channels` complex values (interleaved I,Q), already inverse-FFT'd so index k holds channel k.
pub struct Channelizer {
    channels: usize,
    taps_per_channel: usize,
    inline_samples: Vec<f32>,
    inline_filter: Vec<f32>,
    top_map: Vec<usize>,
    middle_map: Vec<usize>,
    pointer: usize,
    top_block: bool,
    accumulator: Vec<f32>,
    fft: Arc<dyn Fft<f32>>,
    fft_buffer: Vec<Complex32>,
    fft_scratch: Vec<Complex32>,
    result: Vec<f32>,
}

impl Channelizer {
    pub fn new(taps: &[f32], channels: usize) -> Result<Self, String> {
        if channels == 0 || channels % 2 != 0 {
            return Err("Channel count must be an even multiple of the over-sample rate (2x)".into());
        }
        let taps_per_channel = taps.len().div_ceil(channels);
        let fft = FftPlanner::<f32>::new().plan_fft_inverse(channels);
        let scratch = vec![Complex32::new(0.0, 0.0); fft.get_inplace_scratch_len()];
        Ok(Self {
            channels,
            taps_per_channel,
            inline_samples: vec![0.0; channels * 2 * taps_per_channel],
            inline_filter: aligned_filter(taps, channels, taps_per_channel),
            top_map: top_block_map(channels),
            middle_map: middle_block_map(channels),
            pointer: 0,
            top_block: true,
            accumulator: vec![0.0; channels * 2],
            fft,
            fft_buffer: vec![Complex32::new(0.0, 0.0); channels],
            fft_scratch: scratch,
            result: vec![0.0; channels * 2],
        })
    }

    /// Designs the standard prototype filter (9 taps per channel) for `sample_rate`.
    pub fn for_sample_rate(sample_rate: f64) -> Result<Self, String> {
        let channels = channel_count(sample_rate);
        let taps = sinc_m2_channelizer_taps(sample_rate / channels as f64, channels, TAPS_PER_CHANNEL)?;
        Self::new(&taps, channels)
    }

    pub fn channels(&self) -> usize { self.channels }
    /// Output sample rate of each channel for a given input rate (2x oversampled).
    pub fn output_sample_rate(&self, input_sample_rate: f64) -> f64 { input_sample_rate / self.channels as f64 * 2.0 }

    /// Filter step: one output value per sub-channel (I and Q separately).
    fn process(&mut self) {
        let sub = self.channels * 2;
        self.accumulator.fill(0.0);
        for tap in 0..self.taps_per_channel {
            let off = tap * sub;
            for ch in 0..sub {
                self.accumulator[ch] += self.inline_samples[off + ch] * self.inline_filter[off + ch];
            }
        }
        let map = if self.top_block { &self.top_map } else { &self.middle_map };
        for x in 0..sub {
            self.result[x] = self.accumulator[map[x]];
        }
        self.top_block = !self.top_block;
    }

    /// Feeds interleaved I,Q samples; calls `sink` with each completed result (2 * channels floats).
    pub fn receive(&mut self, samples: &[f32], mut sink: impl FnMut(&[f32])) {
        let block = self.channels; // floats per hop = channels / 2 complex samples
        let mut at = 0;
        while at < samples.len() {
            if self.pointer < block {
                let n = (block - self.pointer).min(samples.len() - at);
                self.inline_samples[self.pointer..self.pointer + n].copy_from_slice(&samples[at..at + n]);
                self.pointer += n;
                at += n;
            }
            if self.pointer >= block {
                self.process();
                self.inverse_fft();
                sink(&self.result);
                let len = self.inline_samples.len();
                self.inline_samples.copy_within(0..len - block, block);
                self.pointer = 0;
            }
        }
    }

    /// In-place scaled inverse FFT of `result` (jtransforms `complexInverse(a, true)`).
    fn inverse_fft(&mut self) {
        for (k, c) in self.fft_buffer.iter_mut().enumerate() {
            *c = Complex32::new(self.result[2 * k], self.result[2 * k + 1]);
        }
        self.fft.process_with_scratch(&mut self.fft_buffer, &mut self.fft_scratch);
        let scale = 1.0 / self.channels as f32;
        for (k, c) in self.fft_buffer.iter().enumerate() {
            self.result[2 * k] = c.re * scale;
            self.result[2 * k + 1] = c.im * scale;
        }
    }
}

// ---------------------------------------------------------------- two channel synthesis

/// Synthesis filter that rejoins two adjacent bins (`FilterFactory.getSincM2Synthesizer`).
/// `channel_sample_rate` is the 2x-oversampled bin rate, `channel_bandwidth` the bin width.
pub fn sinc_m2_synthesizer_taps(channel_sample_rate: f64, channel_bandwidth: f64, channels: usize, taps_per_channel: usize) -> Result<Vec<f32>, String> {
    let filter_length = channels * taps_per_channel - 1;
    let cutoff = (channel_bandwidth * 1.10) / (channel_sample_rate * channels as f64);
    let taps = kaiser_sinc(filter_length, cutoff, 80.0)?;
    let mut extended = Vec::with_capacity(taps.len() + 1);
    extended.push(0.0);
    extended.extend_from_slice(&taps);
    Ok(extended)
}

/// Recombines the two bins that a signal straddles into one channel (`TwoChannelSynthesizerM2`).
pub struct TwoChannelSynthesizer {
    filter: Vec<f32>,
    buffer: Vec<f32>,
    top_block: bool,
}

impl TwoChannelSynthesizer {
    pub fn new(taps: &[f32]) -> Self {
        let taps_per_channel = taps.len().div_ceil(2);
        let mut filter = vec![0.0f32; 2 * taps_per_channel * 2];
        for (n, &c) in taps.iter().enumerate() {
            filter[2 * n] = c;
            filter[2 * n + 1] = c;
        }
        Self { buffer: vec![0.0; filter.len()], filter, top_block: true }
    }

    pub fn process(&mut self, i1: &[f32], q1: &[f32], i2: &[f32], q2: &[f32]) -> (Vec<f32>, Vec<f32>) {
        assert!(i1.len() == i2.len() && q1.len() == q2.len() && i1.len() == q1.len());
        let len = self.buffer.len();
        let (mut i_out, mut q_out) = (vec![0.0f32; i1.len()], vec![0.0f32; i1.len()]);
        for x in 0..i1.len() {
            self.buffer.copy_within(0..len - 4, 4);
            // The normalized two-point inverse FFT is a sum and difference.
            let sum = if self.top_block { 0 } else { 2 };
            let diff = 2 - sum;
            self.buffer[sum] = (i1[x] + i2[x]) * 0.5f32;
            self.buffer[sum + 1] = (q1[x] + q2[x]) * 0.5f32;
            self.buffer[diff] = (i1[x] - i2[x]) * 0.5f32;
            self.buffer[diff + 1] = (q1[x] - q2[x]) * 0.5f32;
            let (mut ia, mut qa) = (0.0f32, 0.0f32);
            for y in (0..len).step_by(2) {
                ia += self.buffer[y] * self.filter[y];
                qa += self.buffer[y + 1] * self.filter[y + 1];
            }
            i_out[x] = ia;
            q_out[x] = qa;
            self.top_block = !self.top_block;
        }
        (i_out, q_out)
    }
}

/// Mixes by -Fs/4 using sign swaps only (`FS4DownConverter`); the phase persists across calls.
#[derive(Default)]
pub struct Fs4DownConverter {
    pointer: usize,
}

impl Fs4DownConverter {
    pub fn mix(&mut self, i: &mut [f32], q: &mut [f32]) {
        for x in 0..i.len() {
            match self.pointer {
                1 => { let t = i[x]; i[x] = q[x]; q[x] = -t; }
                2 => { i[x] = -i[x]; q[x] = -q[x]; }
                3 => { let t = i[x]; i[x] = -q[x]; q[x] = t; }
                _ => {}
            }
            self.pointer = (self.pointer + 1) % 4;
        }
    }
}

// ---------------------------------------------------------------- single channel output

/// Extracts one polyphase channel, applies the frequency translation mixer and gain, and delivers
/// 2048-sample I/Q buffers (as the Java `OneChannelOutputProcessor` does).
pub struct ChannelExtractor {
    offset: usize,
    mixer: ComplexMixer,
    gain: f32,
    i: Vec<f32>,
    q: Vec<f32>,
}

impl ChannelExtractor {
    /// `channel_sample_rate` is the 2x-oversampled output rate; `gain` is normally the channel count.
    pub fn new(channel_sample_rate: f64, channel_index: usize, gain: f32) -> Self {
        let mut mixer = ComplexMixer::new(0.0, 0.0);
        mixer.set_sample_rate(channel_sample_rate);
        Self { offset: channel_index * 2, mixer, gain, i: Vec::with_capacity(ASSEMBLY_BUFFER), q: Vec::with_capacity(ASSEMBLY_BUFFER) }
    }

    pub fn set_channel_index(&mut self, channel_index: usize) { self.offset = channel_index * 2; }

    /// Mixing frequency in Hz: index center frequency - wanted frequency + tuner frequency correction.
    pub fn set_frequency_offset(&mut self, hz: i64) { self.mixer.set_frequency(hz as f64); }

    pub fn push(&mut self, result: &[f32], mut sink: impl FnMut(&[f32], &[f32])) {
        self.i.push(result[self.offset]);
        self.q.push(result[self.offset + 1]);
        if self.i.len() >= ASSEMBLY_BUFFER {
            let (mut i, mut q) = (std::mem::take(&mut self.i), std::mem::take(&mut self.q));
            if self.mixer.has_frequency() {
                (i, q) = self.mixer.mix(&i, &q);
            }
            for v in i.iter_mut() { *v *= self.gain; }
            for v in q.iter_mut() { *v *= self.gain; }
            sink(&i, &q);
            self.i.reserve(ASSEMBLY_BUFFER);
            self.q.reserve(ASSEMBLY_BUFFER);
        }
    }
}

/// Extracts two adjacent bins, synthesizes them into one channel, shifts by -Fs/4, then applies the frequency
/// offset mixer and gain (`TwoChannelOutputProcessor` / `TwoChannelMixerAssembler`).
pub struct TwoChannelExtractor {
    offset1: usize,
    offset2: usize,
    synthesizer: TwoChannelSynthesizer,
    fs4: Fs4DownConverter,
    mixer: ComplexMixer,
    gain: f32,
    i1: Vec<f32>,
    q1: Vec<f32>,
    i2: Vec<f32>,
    q2: Vec<f32>,
}

impl TwoChannelExtractor {
    pub fn new(channel_sample_rate: f64, indexes: [usize; 2], synthesis_taps: &[f32], gain: f32) -> Self {
        let mut mixer = ComplexMixer::new(0.0, 0.0);
        mixer.set_sample_rate(channel_sample_rate);
        let cap = || Vec::with_capacity(ASSEMBLY_BUFFER);
        Self {
            offset1: indexes[0] * 2,
            offset2: indexes[1] * 2,
            synthesizer: TwoChannelSynthesizer::new(synthesis_taps),
            fs4: Fs4DownConverter::default(),
            mixer,
            gain,
            i1: cap(), q1: cap(), i2: cap(), q2: cap(),
        }
    }

    pub fn set_frequency_offset(&mut self, hz: i64) { self.mixer.set_frequency(hz as f64); }

    pub fn push(&mut self, result: &[f32], mut sink: impl FnMut(&[f32], &[f32])) {
        self.i1.push(result[self.offset1]);
        self.q1.push(result[self.offset1 + 1]);
        self.i2.push(result[self.offset2]);
        self.q2.push(result[self.offset2 + 1]);
        if self.i1.len() >= ASSEMBLY_BUFFER {
            let (i1, q1) = (std::mem::take(&mut self.i1), std::mem::take(&mut self.q1));
            let (i2, q2) = (std::mem::take(&mut self.i2), std::mem::take(&mut self.q2));
            let (mut i, mut q) = self.synthesizer.process(&i1, &q1, &i2, &q2);
            self.fs4.mix(&mut i, &mut q);
            if self.mixer.has_frequency() {
                (i, q) = self.mixer.mix(&i, &q);
            }
            for v in i.iter_mut() { *v *= self.gain; }
            for v in q.iter_mut() { *v *= self.gain; }
            sink(&i, &q);
            for v in [&mut self.i1, &mut self.q1, &mut self.i2, &mut self.q2] {
                v.reserve(ASSEMBLY_BUFFER);
            }
        }
    }
}
