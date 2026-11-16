//! P25 Phase 1 LSM (Linear Simulcast Modulation, CQPSK) demodulator. Port of sdrtrunk `P25P1DemodulatorLSM` plus
//! the soft sync detection of `P25P1MessageFramer.processWithSoftSyncDetect()`.
//!
//! Differential QPSK demodulation of filtered I/Q (from `sdrtrunk_dsp::front_end::LsmFrontEnd`) with automatic gain,
//! Gardner timing recovery and a carrier PLL. Simulcast sites need this instead of the C4FM demodulator.
//! Float/double types follow the Java so results match it bit for bit (tests/java_lsm.rs).
use crate::demod::SymbolSink;
use crate::dibit::Dibit;
use crate::sync::SoftSyncDetector;
use std::f64::consts::PI;

const HALF_PI: f32 = (PI / 2.0) as f32;
const MAX_PLL: f32 = (PI / 3.0) as f32; // +/- 800 Hz
const OBJECTIVE_MAGNITUDE: f32 = 1.0;
const PLL_GAIN: f32 = 0.1;
/// `P25P1MessageFramer.SYNC_DETECTION_THRESHOLD`.
const SYNC_DETECTION_THRESHOLD: f32 = 60.0;

pub struct LsmDemodulator<S: SymbolSink> {
    sink: S,
    sync: SoftSyncDetector,
    sample_point: f64,
    samples_per_symbol: f64,
    samples_per_half_symbol: f64,
    buffer_i: Vec<f32>,
    buffer_q: Vec<f32>,
    buffer_pointer: usize,
    buffer_reserve: usize,
    pll: f32,
    sample_gain: f32,
    previous_middle: (f32, f32),
    previous_current: (f32, f32),
    previous_symbol: (f32, f32),
}

impl<S: SymbolSink> LsmDemodulator<S> {
    pub fn new(sink: S, samples_per_symbol: f32) -> Self {
        let reserve = (samples_per_symbol as f64).ceil() as usize;
        Self {
            sink,
            sync: SoftSyncDetector::new(),
            sample_point: samples_per_symbol as f64,
            samples_per_symbol: samples_per_symbol as f64,
            samples_per_half_symbol: (samples_per_symbol / 2.0) as f64,
            buffer_i: vec![0.0; reserve],
            buffer_q: vec![0.0; reserve],
            buffer_pointer: 0,
            buffer_reserve: reserve,
            pll: 0.0,
            sample_gain: 1.0,
            previous_middle: (0.0, 0.0),
            previous_current: (0.0, 0.0),
            previous_symbol: (0.7, 0.7),
        }
    }

    pub fn sink_mut(&mut self) -> &mut S { &mut self.sink }

    /// Resets the carrier PLL after a tuner frequency or PPM change.
    pub fn reset_pll(&mut self) { self.pll = 0.0; }

    /// Carrier PLL: phase correction per symbol (radians).
    pub fn pll(&self) -> f32 { self.pll }

    /// Processes filtered, pulse-shaped I/Q into symbols for the sink.
    pub fn process(&mut self, i: &[f32], q: &[f32]) {
        let reserve = self.buffer_reserve;
        // Carry the reserve samples from the last call to the front, then append the new samples. (Java resizes the
        // buffer first, which loses samples when a block is shorter than the one before; identical otherwise.)
        let (bp, n) = (self.buffer_pointer, i.len().min(q.len()));
        self.buffer_i.copy_within(bp..bp + reserve, 0);
        self.buffer_q.copy_within(bp..bp + reserve, 0);
        self.buffer_i.truncate(reserve);
        self.buffer_q.truncate(reserve);
        self.buffer_i.extend_from_slice(&i[..n]);
        self.buffer_q.extend_from_slice(&q[..n]);

        let mut sample_point = self.sample_point;
        let sps = self.samples_per_symbol;
        let half = self.samples_per_half_symbol;
        let mut pll = self.pll;
        let mut gain = self.sample_gain;
        let (mut prev_sym_i, mut prev_sym_q) = self.previous_symbol;
        let (mut prev_mid_i, mut prev_mid_q) = self.previous_middle;
        let (mut prev_cur_i, mut prev_cur_q) = self.previous_current;
        let ted_gain = sps / 4.0;
        let max_timing_adjustment = sps / 25.0;
        let (bi, bq) = (&self.buffer_i, &self.buffer_q);

        let mut buffer_pointer = 0usize;
        let reload = bi.len() - reserve;
        while buffer_pointer < reload {
            buffer_pointer += 1;
            sample_point -= 1.0;
            if sample_point >= 1.0 {
                continue;
            }
            // Sample point is the middle sample, between the previous and current symbols.
            let mut i_mid = linear(bi[buffer_pointer], bi[buffer_pointer + 1], sample_point);
            let mut q_mid = linear(bq[buffer_pointer], bq[buffer_pointer + 1], sample_point);
            let pointer = buffer_pointer as f64 + sample_point + half;
            let offset = pointer.floor() as usize;
            let residual = pointer - offset as f64;
            let mut i_cur = linear(bi[offset], bi[offset + 1], residual);
            let mut q_cur = linear(bq[offset], bq[offset + 1], residual);

            // Adjust sample gain toward unit magnitude.
            let magnitude = ((i_cur as f64).powi(2) + (q_cur as f64).powi(2)).sqrt() as f32;
            if magnitude > 0.0 && !magnitude.is_infinite() {
                let required = constrain_f32(OBJECTIVE_MAGNITUDE / magnitude, 500.0);
                gain += (required - gain) * 0.05;
                gain = gain.min(required).min(500.0);
            }
            i_mid *= gain;
            q_mid *= gain;
            i_cur *= gain;
            q_cur *= gain;

            let pll_i = (pll as f64).cos() as f32;
            let pll_q = (pll as f64).sin() as f32;

            // Differential demodulation of the middle sample, rotated by the PLL.
            let mut i_mid_d = (prev_mid_i * i_mid) - (-prev_mid_q * q_mid);
            let mut q_mid_d = (prev_mid_i * q_mid) + (-prev_mid_q * i_mid);
            let t = (i_mid_d * pll_i) - (q_mid_d * pll_q);
            q_mid_d = (q_mid_d * pll_i) + (i_mid_d * pll_q);
            i_mid_d = t;

            // Differential demodulation of the symbol, rotated by the PLL.
            let mut i_sym = (prev_cur_i * i_cur) - (-prev_cur_q * q_cur);
            let mut q_sym = (prev_cur_i * q_cur) + (-prev_cur_q * i_cur);
            let t = (i_sym * pll_i) - (q_sym * pll_q);
            q_sym = (q_sym * pll_i) + (i_sym * pll_q);
            i_sym = t;

            let soft = (q_sym as f64).atan2(i_sym as f64) as f32;

            // Gardner timing error detection.
            let ted = ((prev_sym_i - i_sym) * i_mid_d) + ((prev_sym_q - q_sym) * q_mid_d);
            sample_point += constrain_f64(ted as f64, max_timing_adjustment) * ted_gain;

            // PLL (not adjusted when the soft symbol is exactly zero).
            let hard = if soft != 0.0 {
                let hard = to_dibit(soft);
                let phase_error = constrain_f32(soft - hard.ideal_phase(), 0.3);
                pll -= phase_error * PLL_GAIN;
                pll = constrain_f32(pll, MAX_PLL);
                hard
            } else {
                Dibit::D00Plus1
            };

            self.sink.process(soft, hard);
            if self.sync.process(soft) > SYNC_DETECTION_THRESHOLD {
                self.sink.sync_detected();
            }

            (prev_sym_i, prev_sym_q) = (i_sym, q_sym);
            (prev_mid_i, prev_mid_q) = (i_mid, q_mid);
            (prev_cur_i, prev_cur_q) = (i_cur, q_cur);
            sample_point += sps;
        }

        self.buffer_pointer = buffer_pointer;
        self.sample_point = sample_point;
        self.pll = pll;
        self.sample_gain = gain;
        self.previous_symbol = (prev_sym_i, prev_sym_q);
        self.previous_middle = (prev_mid_i, prev_mid_q);
        self.previous_current = (prev_cur_i, prev_cur_q);
    }
}

/// QPSK quadrant decision. Java `P25P1DemodulatorLSM.toDibit`.
pub fn to_dibit(soft: f32) -> Dibit {
    if soft > 0.0 {
        if soft > HALF_PI { Dibit::D01Plus3 } else { Dibit::D00Plus1 }
    } else if soft < -HALF_PI {
        Dibit::D11Minus3
    } else {
        Dibit::D10Minus1
    }
}

/// Java `LinearInterpolator.calculate(float, float, double)`: mu is cast to float and clamped to [0, 1].
fn linear(x1: f32, x2: f32, mu: f64) -> f32 {
    let mu = mu as f32;
    if mu < 0.0 {
        x1
    } else if mu > 1.0 {
        x2
    } else {
        x1 + ((x2 - x1) * mu)
    }
}

fn constrain_f64(v: f64, c: f64) -> f64 { if v.is_nan() || v.is_infinite() { 0.0 } else if v > c { c } else { v.max(-c) } }

fn constrain_f32(v: f32, c: f32) -> f32 { if v.is_nan() || v.is_infinite() { 0.0 } else if v > c { c } else { v.max(-c) } }
