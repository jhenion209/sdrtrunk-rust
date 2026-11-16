//! C4FM symbol recovery, ported from sdrtrunk `P25P1DemodulatorC4FM`: turns the stream of demodulated phase values
//! into soft symbols and dibit decisions, with symbol timing, an equalizer/PLL, soft sync detection and NID
//! validation. Float and double arithmetic follows the Java source expression by expression.
//!
//! Left out on purpose: the packed bitstream recording (`DibitDelayLine`/`DibitToByteBufferAssembler`) and the UI
//! feedback callbacks (soft symbol broadcast, PLL error). The message framer is reached through [`SymbolSink`].
use crate::dibit::{Dibit, SOFT_SYMBOL_QUADRANT_BOUNDARY};
use crate::nid::{self, DataUnitId, NacTracker, NID_BITS, NO_EXPECTED_NAC, NO_TRACKED_NAC};
use crate::sync::{sync_pattern_dibits, sync_pattern_symbols, SoftSyncDetector, SYNC_SYMBOLS};
use std::f64::consts::PI;
use sdrtrunk_edac::bch::Bch63;

const EQUALIZER_LOOP_GAIN: f32 = 0.15;
const EQUALIZER_MAXIMUM_PLL: f32 = (PI / 3.0) as f32; // +/- 800 Hz
const EQUALIZER_MAXIMUM_GAIN: f32 = 1.25;
const EQUALIZER_RECALIBRATE_THRESHOLD: f32 = (PI / 8.0) as f32;
const SYNC_THRESHOLD_DETECTION: f32 = 80.0;
const SYNC_THRESHOLD_OPTIMIZED: f32 = 80.0;
const SYNC_THRESHOLD_EQUALIZED: f32 = 110.0;
const TWO_PI: f32 = (PI * 2.0) as f32;
const BUFFER_WORKSPACE_LENGTH: usize = 1024;
const DIBIT_LENGTH_NID: usize = 33; // 32 dibits (64 bits) + 1 status
const DIBIT_LENGTH_SYNC: usize = 24;

/// Receives the symbol decisions and sync notifications (implemented by the message framer).
pub trait SymbolSink {
    /// Every recovered symbol with its equalized soft value.
    fn process(&mut self, soft_symbol: f32, symbol: Dibit);
    /// A frame sync with a valid NID (or a high quality score) was found.
    fn sync_detected(&mut self);
    fn is_complete(&self) -> bool;
    fn is_assembling(&self) -> bool;
    /// Not in the Java original: called when a sync's NID validated (NAC, DUID, corrected bits). For diagnostics.
    fn nid_decoded(&mut self, _nac: i32, _duid: DataUnitId, _corrected_bits: i32) {}
}

/// Symbol timing and equalizer correction candidate.
#[derive(Clone, Debug)]
pub struct Correction {
    additional_offset: f64,
    pll_adjustment: f32,
    gain_adjustment: f32,
    detection_score: f32,
    optimization_score: f32,
    timing_adjustment: f64,
    pub nac: i32,
    pub data_unit_id: DataUnitId,
    corrected_bit_count: i32,
}

impl Correction {
    fn new(additional_offset: f64, timing: f64, pll: f32, gain: f32, detection: f32, optimization: f32, bit_errors: i32) -> Self {
        Self {
            additional_offset,
            pll_adjustment: pll,
            gain_adjustment: gain,
            detection_score: detection,
            optimization_score: optimization,
            timing_adjustment: timing,
            nac: 0,
            data_unit_id: DataUnitId::PlaceHolder,
            corrected_bit_count: bit_errors,
        }
    }

    pub fn timing_correction(&self) -> f64 { self.additional_offset + self.timing_adjustment }
    pub fn additional_offset(&self) -> f64 { self.additional_offset }
    pub fn detection_score(&self) -> f32 { self.detection_score }
    pub fn optimization_score(&self) -> f32 { self.optimization_score }
    pub fn pll_adjustment(&self) -> f32 { self.pll_adjustment }
    pub fn gain_adjustment(&self) -> f32 { self.gain_adjustment }
    pub fn corrected_bit_count(&self) -> i32 { self.corrected_bit_count }

    fn set_nid(&mut self, nac: i32, duid: DataUnitId) {
        self.nac = nac;
        self.data_unit_id = duid;
    }

    fn add_corrected_bit_count(&mut self, count: i32) { self.corrected_bit_count += count; }

    /// Valid NID decoded and the optimization score is above the equalizer threshold.
    pub fn is_valid(&self) -> bool {
        self.data_unit_id != DataUnitId::PlaceHolder && self.optimization_score > SYNC_THRESHOLD_EQUALIZED
    }

    pub fn has_high_quality_detection_score(&self) -> bool { self.detection_score > SYNC_THRESHOLD_EQUALIZED }

    pub fn has_high_quality_optimization_score(&self) -> bool {
        self.optimization_score >= self.detection_score && self.optimization_score > SYNC_THRESHOLD_EQUALIZED
    }
}

/// `LinearInterpolator.calculate(float, float, float)`.
fn interpolate(x1: f32, x2: f32, mu: f32) -> f32 {
    if mu < 0.0 {
        x1
    } else if mu > 1.0 {
        x2
    } else {
        x1 + ((x2 - x1) * mu)
    }
}

/// `LinearInterpolator.calculate(float, float, double)`.
fn interpolate_d(x1: f32, x2: f32, mu: f64) -> f32 { interpolate(x1, x2, mu as f32) }

pub struct Demodulator<S: SymbolSink> {
    sink: S,
    bch: Bch63,
    nac_tracker: NacTracker,
    sync_detector: SoftSyncDetector,
    sync_detector_lagging: SoftSyncDetector,
    sync_symbols: [f32; SYNC_SYMBOLS],
    sync_dibits: [Dibit; SYNC_SYMBOLS],
    // equalizer
    eq_initialized: bool,
    eq_pll: f32,
    eq_gain: f32,
    // timing
    fine_sync: bool,
    max_fine_sync_timing_adjustment: f64,
    noise_standard_deviation_threshold: f64,
    sample_point: f64,
    sample_point_adjustment: f64,
    sample_point_adjustment_increment: f64,
    sample_point_adjustment_max: f64,
    samples_per_symbol: f64,
    buffer: Vec<f32>,
    lagging_sync_offset: f32,
    optimize_fine_increment: f32,
    buffer_pointer: i64,
    buffer_reload_threshold: i64,
    symbols_since_last_sync: i32,
    expected_nac: i32,
    require_valid_nid: bool,
}

impl<S: SymbolSink> Demodulator<S> {
    pub fn new(sink: S, samples_per_symbol: f32) -> Self {
        let mut d = Self {
            sink,
            bch: Bch63::p25_nid(),
            nac_tracker: NacTracker::default(),
            sync_detector: SoftSyncDetector::new(),
            sync_detector_lagging: SoftSyncDetector::new(),
            sync_symbols: sync_pattern_symbols(),
            sync_dibits: sync_pattern_dibits(),
            eq_initialized: false,
            eq_pll: 0.0,
            eq_gain: 1.219,
            fine_sync: false,
            max_fine_sync_timing_adjustment: 0.0,
            noise_standard_deviation_threshold: 0.0,
            sample_point: 0.0,
            sample_point_adjustment: 0.0,
            sample_point_adjustment_increment: 0.0,
            sample_point_adjustment_max: 0.0,
            samples_per_symbol: 0.0,
            buffer: Vec::new(),
            lagging_sync_offset: 0.0,
            optimize_fine_increment: 0.0,
            buffer_pointer: 0,
            buffer_reload_threshold: 0,
            symbols_since_last_sync: 0,
            expected_nac: NO_EXPECTED_NAC,
            require_valid_nid: false,
        };
        d.set_samples_per_symbol(samples_per_symbol);
        d
    }

    pub fn sink(&self) -> &S { &self.sink }
    pub fn sink_mut(&mut self) -> &mut S { &mut self.sink }
    pub fn into_sink(self) -> S { self.sink }

    /// Equalizer balance (PLL) and gain, for diagnostics.
    pub fn equalizer(&self) -> (f32, f32) { (self.eq_pll, self.eq_gain) }

    /// Resets the equalizer PLL after a center frequency or PPM change.
    pub fn reset_pll(&mut self) {
        self.eq_initialized = false;
        self.eq_pll = 0.0;
    }

    pub fn reset_for_source_frequency_change(&mut self) {
        self.nac_tracker.reset();
        self.sync_detector.reset();
        self.sync_detector_lagging.reset();
        self.fine_sync = false;
        self.symbols_since_last_sync = 0;
        self.reset_pll();
    }

    /// Pins the NID validator to the NAC of the controlling channel (traffic channels).
    pub fn set_expected_nac(&mut self, nac: i32) {
        assert!((0..=0xFFF).contains(&nac), "Expected P25 Phase 1 traffic NAC must be a concrete 12-bit value");
        self.expected_nac = nac;
        self.nac_tracker.reset();
    }

    pub fn set_require_valid_nid(&mut self, require: bool) {
        self.require_valid_nid = require;
        self.nac_tracker.reset();
    }

    fn has_expected_nac(&self) -> bool { self.expected_nac != NO_EXPECTED_NAC }

    pub fn set_samples_per_symbol(&mut self, sps: f32) {
        self.samples_per_symbol = sps as f64;
        self.max_fine_sync_timing_adjustment = sps as f64 * 0.2; // 1/5th of a symbol period
        self.noise_standard_deviation_threshold = (Dibit::D01Plus3.ideal_phase() * 2.0f32) as f64 / sps as f64 * 1.2;
        self.sample_point = sps as f64;
        self.sample_point_adjustment_max = (sps / 2.0f32) as f64;
        self.sample_point_adjustment_increment = sps as f64 / 100.0;
        self.optimize_fine_increment = sps / 200.0f32;
        self.lagging_sync_offset = sps / 2.0f32;
        let span = ((DIBIT_LENGTH_SYNC + DIBIT_LENGTH_NID + 2) as f32 * sps) as f64;
        let buffer_length = BUFFER_WORKSPACE_LENGTH + span.ceil() as usize;
        self.buffer = vec![0.0; buffer_length];
        self.buffer_reload_threshold = buffer_length as i64 - ((sps * (DIBIT_LENGTH_NID + 1) as f32) as f64).ceil() as i64;
        self.buffer_pointer = self.buffer_reload_threshold;
    }

    /// Processes a block of demodulated samples into symbols.
    pub fn process(&mut self, samples: &[f32]) {
        // Shadow copies of the heap variables, as in Java: note that `optimize()` reads the heap `fine_sync`, which
        // only changes when this call returns.
        let mut fine_sync = self.fine_sync;
        let mut sample_point = self.sample_point;
        let samples_per_symbol = self.samples_per_symbol;
        let mut buffer_pointer = self.buffer_pointer;
        let buffer_reload_threshold = self.buffer_reload_threshold;
        let mut symbols_since_last_sync = self.symbols_since_last_sync;
        let mut samples_pointer = 0usize;

        while samples_pointer < samples.len() {
            if buffer_pointer >= buffer_reload_threshold {
                let len = self.buffer.len();
                let copy_length = BUFFER_WORKSPACE_LENGTH.min(samples.len() - samples_pointer);
                self.buffer.copy_within(copy_length.., 0);
                self.buffer[len - copy_length..].copy_from_slice(&samples[samples_pointer..samples_pointer + copy_length]);
                samples_pointer += copy_length;
                buffer_pointer -= copy_length as i64;

                // Unwrap phases
                for x in len - copy_length..len {
                    if self.buffer[x - 1] > 1.5 && self.buffer[x] < -1.5 {
                        self.buffer[x] += TWO_PI;
                    } else if self.buffer[x - 1] < -1.5 && self.buffer[x] > 1.5 {
                        self.buffer[x] -= TWO_PI;
                    }
                }
            }

            while buffer_pointer < buffer_reload_threshold {
                buffer_pointer += 1;
                sample_point -= 1.0;

                if sample_point < 1.0 {
                    symbols_since_last_sync += 1;
                    let bp = buffer_pointer as usize;
                    let soft_symbol = self.equalized_symbol(self.buffer[bp], self.buffer[bp + 1], sample_point);
                    let symbol = Dibit::from_soft(soft_symbol);
                    sample_point += self.adjustment(soft_symbol, symbol, bp);

                    self.sink.process(soft_symbol, symbol);

                    let score_primary = self.sync_detector.process(soft_symbol);
                    let mut candidate: Option<Correction> = None;
                    let buffer_offset = buffer_pointer as f64 + sample_point;

                    if fine_sync {
                        if symbols_since_last_sync > 1 && score_primary > SYNC_THRESHOLD_DETECTION {
                            candidate = self.optimize(0.0, score_primary, buffer_offset);
                        }
                    } else {
                        // Check for lagging sync pattern
                        let lag = (buffer_pointer as f64 + sample_point - self.lagging_sync_offset as f64) as f32;
                        let lag_integral = (lag as f64).floor() as i64;
                        let li = lag_integral as usize;
                        let soft_symbol_lag = self.equalized_symbol(self.buffer[li], self.buffer[li + 1], (lag - lag_integral as f32) as f64);
                        let score_lag = self.sync_detector_lagging.process(soft_symbol_lag);

                        if symbols_since_last_sync > 1 {
                            if score_primary > SYNC_THRESHOLD_DETECTION && score_primary > score_lag {
                                candidate = self.optimize(0.0, score_primary, buffer_offset);
                            }
                            if candidate.is_none() && score_lag > SYNC_THRESHOLD_DETECTION {
                                candidate = self.optimize(-(self.lagging_sync_offset as f64), score_lag, buffer_offset);
                            }
                        }
                    }

                    if let Some(mut correction) = candidate {
                        self.validate_nid(&mut correction, buffer_offset);

                        // A failed NID with a high quality original detection: retry with no timing adjustment.
                        if !correction.is_valid() && correction.has_high_quality_detection_score() {
                            correction = self.get_correction(correction.additional_offset(), 0.0,
                                correction.detection_score(), correction.detection_score(), buffer_offset);
                            self.validate_nid(&mut correction, buffer_offset);
                        }

                        if correction.is_valid() {
                            if fine_sync {
                                let mut adjustment = correction.timing_correction();
                                if adjustment < 1.0 {
                                    adjustment = adjustment.max(-self.max_fine_sync_timing_adjustment);
                                    adjustment = adjustment.min(self.max_fine_sync_timing_adjustment);
                                }
                                sample_point += adjustment;
                            } else {
                                sample_point += correction.timing_correction();
                            }
                            self.sample_point_adjustment = 0.0;
                            self.apply(&correction);
                            self.sink.nid_decoded(correction.nac, correction.data_unit_id, correction.corrected_bit_count());
                        }

                        if correction.is_valid() || correction.has_high_quality_detection_score()
                            || (correction.has_high_quality_optimization_score() && self.sink.is_complete())
                        {
                            self.sink.sync_detected();
                            fine_sync = true;
                            symbols_since_last_sync = 0;
                        }
                    }

                    if fine_sync ^ self.sink.is_assembling() {
                        fine_sync = self.sink.is_assembling();
                    }

                    // Add another symbol's worth of samples to the counter for processing
                    sample_point += samples_per_symbol;
                }
            }
        }

        self.buffer_pointer = buffer_pointer;
        self.fine_sync = fine_sync;
        self.sample_point = sample_point;
        self.symbols_since_last_sync = symbols_since_last_sync;
    }

    /// Resamples the NID after a sync using the candidate correction and validates it with BCH, updating the
    /// correction's NAC, DUID and corrected bit count.
    fn validate_nid(&mut self, correction: &mut Correction, buffer_offset: f64) {
        let mut pointer = buffer_offset + correction.timing_correction() + self.samples_per_symbol;
        let mut nid_bits = [false; NID_BITS];
        let mut bit = 0;

        for x in 0..DIBIT_LENGTH_NID {
            let integral = pointer.floor() as usize;
            let fractional = pointer - integral as f64;
            let soft_symbol = self.equalized_symbol_with(self.buffer[integral], self.buffer[integral + 1], fractional, correction);
            let symbol = Dibit::from_soft(soft_symbol);
            // Skip the status symbol at dibit 11
            if x != 11 {
                nid_bits[bit] = symbol.bit1();
                nid_bits[bit + 1] = symbol.bit2();
                bit += 2;
            }
            pointer += self.samples_per_symbol;
        }

        self.decode_nid(&mut nid_bits, correction);
    }

    /// Decodes and validates a captured NID, updating `correction` when it is acceptable.
    pub fn decode_nid(&mut self, nid_bits: &mut [bool; NID_BITS], correction: &mut Correction) {
        let strict = self.has_expected_nac() || self.require_valid_nid;
        let tracked_nac = if strict { NO_TRACKED_NAC } else { self.nac_tracker.tracked_nac() };
        let observed = if strict || tracked_nac < 0 { None } else { Some(tracked_nac) };
        let mut corrected = nid::decode_nid(&self.bch, nid_bits, observed);

        // If error correction fails, return the original correction candidate
        if corrected < 0 {
            correction.add_corrected_bit_count(corrected);
            return;
        }

        let nac = nid::nid_nac(nid_bits) as i32;
        let duid = DataUnitId::from_value(nid::nid_duid(nid_bits));

        if self.require_valid_nid && duid == DataUnitId::Unknown {
            return;
        }

        if self.has_expected_nac() {
            corrected = nid::validate_expected_nac(nid_bits, corrected, self.expected_nac);
        } else {
            corrected = nid::validate(nid_bits, corrected);
            if corrected < 0 {
                return;
            }
            // The BCH decoder can over-correct the NID and produce an invalid NAC. Compare against the tracked NID.
            self.nac_tracker.track(nac);
            if tracked_nac != NO_TRACKED_NAC && tracked_nac != nac {
                return;
            }
        }

        if corrected < 0 {
            return;
        }

        correction.set_nid(nac, duid);
        correction.add_corrected_bit_count(corrected);
    }

    /// True when the samples around a detected sync deviate more than a modulated signal would (a noise false detect).
    fn is_noisy(&self, offset: f64) -> bool {
        let start = (offset - (23.0 * self.samples_per_symbol)).floor() as i64;
        let end = (offset.ceil() as i64).min(self.buffer.len() as i64 - 1);

        // commons-math StandardDeviation: streaming first/second moments, bias corrected.
        let (mut n, mut m1, mut m2) = (0.0f64, 0.0f64, 0.0f64);
        for i in start..end {
            let i = i as usize;
            let d = (self.buffer[i] - self.buffer[i + 1]) as f64;
            n += 1.0;
            let dev = d - m1;
            let n_dev = dev / n;
            m1 += n_dev;
            m2 += (n - 1.0) * dev * n_dev;
        }
        let std_dev = if n == 0.0 { f64::NAN } else if n == 1.0 { 0.0 } else { (m2 / (n - 1.0)).sqrt() };
        std_dev > self.noise_standard_deviation_threshold
    }

    // ------------------------------------------------------------------ equalizer

    /// Finds the sync timing that maximizes the correlation score; None when it never reaches the threshold.
    fn optimize(&self, additional_offset: f64, detection_score: f32, buffer_offset: f64) -> Option<Correction> {
        let offset = buffer_offset + additional_offset;

        // Reject sync detections whose sample:sample deviation exceeds the noise threshold.
        if self.is_noisy(offset) {
            return None;
        }

        let fine_sync = self.fine_sync; // heap value, see process()
        let samples_per_symbol = self.samples_per_symbol;
        let mut step_size = samples_per_symbol / if fine_sync { 16.0 } else { 8.0 };
        let step_size_min = self.optimize_fine_increment as f64;
        let mut adjustment = 0.0f64;
        let adjustment_max = if fine_sync { samples_per_symbol } else { samples_per_symbol / 2.0 };

        let mut score_center = self.score(offset, samples_per_symbol);
        let mut score_left = self.score(offset - step_size, samples_per_symbol);
        let mut score_right = self.score(offset + step_size, samples_per_symbol);

        while step_size > step_size_min && adjustment.abs() <= adjustment_max {
            if score_left > score_right && score_left > score_center {
                adjustment -= step_size;
                score_right = score_center;
                score_center = score_left;
                score_left = self.score(offset + adjustment - step_size, samples_per_symbol);
            } else if score_right > score_left && score_right > score_center {
                adjustment += step_size;
                score_left = score_center;
                score_center = score_right;
                score_right = self.score(offset + adjustment + step_size, samples_per_symbol);
            } else {
                step_size *= 0.5f32 as f64;
                if step_size > step_size_min {
                    score_left = self.score(offset + adjustment - step_size, samples_per_symbol);
                    score_right = self.score(offset + adjustment + step_size, samples_per_symbol);
                }
            }
        }

        if score_center < SYNC_THRESHOLD_OPTIMIZED {
            return None;
        }

        Some(self.get_correction(additional_offset, adjustment, detection_score, score_center, buffer_offset))
    }

    fn equalize(&self, symbol: f32) -> f32 { (symbol + self.eq_pll) * self.eq_gain }

    /// Per-symbol timing nudge from the symbol error between sync detections.
    fn adjustment(&mut self, soft_symbol: f32, symbol: Dibit, buffer_pointer: usize) -> f64 {
        let rising = self.buffer[buffer_pointer] > self.buffer[buffer_pointer + 1];
        let step = self.sample_point_adjustment_increment;
        let adjustment = if soft_symbol < symbol.ideal_phase() {
            if rising { -step } else { step }
        } else if rising {
            step
        } else {
            -step
        };

        if (self.sample_point_adjustment + adjustment).abs() <= self.sample_point_adjustment_max {
            self.sample_point_adjustment += adjustment;
            return adjustment;
        }
        0.0
    }

    fn equalized_symbol(&self, sample1: f32, sample2: f32, mu: f64) -> f32 {
        interpolate_d(self.equalize(sample1), self.equalize(sample2), mu)
    }

    /// Equalizes with the candidate correction applied and wraps the phase into +/- PI.
    fn equalized_symbol_with(&self, sample1: f32, sample2: f32, mu: f64, c: &Correction) -> f32 {
        let gain = self.eq_gain + c.gain_adjustment;
        let mut s1 = (sample1 + self.eq_pll + c.pll_adjustment) * gain;
        let mut s2 = (sample2 + self.eq_pll + c.pll_adjustment) * gain;
        let pi = PI;
        if s1 as f64 > pi { s1 -= TWO_PI; } else if (s1 as f64) < -pi { s1 += TWO_PI; }
        if s2 as f64 > pi { s2 -= TWO_PI; } else if (s2 as f64) < -pi { s2 += TWO_PI; }
        interpolate_d(s1, s2, mu)
    }

    /// Sync correlation score at `offset` (the final sync symbol) using the current balance and gain.
    fn score(&self, offset: f64, samples_per_symbol: f64) -> f32 {
        self.score_with(offset, samples_per_symbol, self.eq_pll, self.eq_gain)
    }

    fn score_with(&self, offset: f64, samples_per_symbol: f64, balance: f32, gain: f32) -> f32 {
        let max_pointer = self.buffer.len() as i64 - 1;
        let mut pointer = offset - (samples_per_symbol * 23.0);
        let mut buffer_pointer = pointer.floor() as i64;
        let mut fractional = pointer - buffer_pointer as f64;
        let mut score = 0.0f32;

        for x in 0..SYNC_SYMBOLS {
            let soft_symbol = if buffer_pointer < max_pointer {
                let bp = buffer_pointer as usize;
                let s = interpolate_d(self.buffer[bp], self.buffer[bp + 1], fractional);
                (s + balance) * gain
            } else {
                0.0
            };
            score += soft_symbol * self.sync_symbols[x];
            pointer += samples_per_symbol;
            buffer_pointer = pointer.floor() as i64;
            fractional = pointer - buffer_pointer as f64;
        }
        score
    }

    /// Applies an equalizer correction, using the control loop gain after the first sync.
    fn apply(&mut self, c: &Correction) {
        if self.eq_initialized && c.pll_adjustment.abs() > EQUALIZER_RECALIBRATE_THRESHOLD {
            self.eq_initialized = false;
        }
        if self.eq_initialized {
            self.eq_pll += c.pll_adjustment * EQUALIZER_LOOP_GAIN;
            self.eq_gain += c.gain_adjustment * EQUALIZER_LOOP_GAIN;
        } else {
            self.eq_pll += c.pll_adjustment;
            self.eq_gain += c.gain_adjustment;
        }
        self.eq_pll = self.eq_pll.min(EQUALIZER_MAXIMUM_PLL);
        self.eq_pll = self.eq_pll.max(-EQUALIZER_MAXIMUM_PLL);
        self.eq_gain = self.eq_gain.min(EQUALIZER_MAXIMUM_GAIN);
        self.eq_gain = self.eq_gain.max(1.0);
        self.eq_initialized = true;
    }

    /// Resamples the sync symbols at the given timing and derives balance (PLL) and gain corrections.
    fn get_correction(&self, additional_offset: f64, timing_correction: f64, detection_score: f32,
                      mut optimization_score: f32, offset: f64) -> Correction {
        let sps = self.samples_per_symbol;
        let mut resample_start = offset + additional_offset + timing_correction;
        let mut integral = resample_start.floor() as i64;
        let mut symbol = self.sync_symbols[23];
        let mut soft = interpolate_d(self.buffer[integral as usize], self.buffer[integral as usize + 1], resample_start - integral as f64);
        soft = (soft + self.eq_pll) * self.eq_gain;

        let mut balance_plus3 = 0.0f32;
        let mut balance_minus3 = soft - symbol;
        let mut gain_accumulator = symbol.abs() - soft.abs();
        let mut bit_error_count = self.sync_dibits[23].bit_error_from(Dibit::from_soft(soft));

        resample_start -= 23.0 * sps;
        integral = resample_start.floor() as i64;

        for x in 0..23 {
            if integral >= 0 {
                symbol = self.sync_symbols[x];
                let i = integral as usize;
                soft = interpolate_d(self.buffer[i], self.buffer[i + 1], resample_start - integral as f64);
                soft = (soft + self.eq_pll) * self.eq_gain;
                bit_error_count += self.sync_dibits[x].bit_error_from(Dibit::from_soft(soft));

                if symbol > 0.0 {
                    balance_plus3 += soft - symbol;
                } else {
                    balance_minus3 += soft - symbol;
                }
                gain_accumulator += symbol.abs() - soft.abs();
            }
            resample_start += sps;
            integral = resample_start.floor() as i64;
        }

        balance_plus3 /= -11.0f32; // 11 x +3 and 13 x -3 symbols in the sync pattern
        balance_minus3 /= -13.0f32;
        let mut balance_correction = (balance_plus3 + balance_minus3) / 2.0f32;
        balance_correction = balance_correction.min(SOFT_SYMBOL_QUADRANT_BOUNDARY);
        balance_correction = balance_correction.max(-SOFT_SYMBOL_QUADRANT_BOUNDARY);
        gain_accumulator /= 24.0f32 * Dibit::D01Plus3.ideal_phase();

        // Recalculate the optimization score using the adjusted equalizer settings
        if !self.eq_initialized {
            optimization_score = self.score_with(offset + timing_correction, sps,
                self.eq_pll + balance_correction, self.eq_gain + gain_accumulator);
        }

        Correction::new(additional_offset, timing_correction, balance_correction, gain_accumulator,
            detection_score, optimization_score, bit_error_count)
    }
}
