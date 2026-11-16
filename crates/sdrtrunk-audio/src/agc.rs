//! Speech-gated automatic gain control. Port of JMBE `jmbe.codec.AGC` (GPL-3.0, Dennis Sheirer), which sdrtrunk
//! applies to decoded P25 audio when "JMBE AGC" is on.
//!
//! Each 20 ms frame is classified as speech or not from the decoded voice parameters (repeat/error status, voicing,
//! pitch continuity) and its level against a tracked noise floor. Only speech frames move the speech level estimate;
//! the gain steers the average speech level toward -20 dBFS and is held during pauses and noise. A per-frame peak
//! limiter keeps the applied gain from clipping.

const MINIMUM_GAIN: f32 = 1.0;
const DEFAULT_GAIN: f32 = 1.0;
const MAXIMUM_GAIN: f32 = 30.0;
const MAXIMUM_AMPLITUDE: f32 = i16::MAX as f32 * 0.95;
const TARGET_SPEECH_LEVEL_DB: f32 = -20.0;
const MINIMUM_SPEECH_LEVEL_DB: f32 = -60.0;
const SPEECH_THRESHOLD_DB: f32 = 9.0;
const MINIMUM_LEVEL_DB: f32 = -100.0;
const NOISE_FLOOR_WINDOW_FRAMES: usize = 100;
const NOISE_FLOOR_PERCENTILE: f32 = 0.05;
const NOISE_FLOOR_INITIAL_MAXIMUM_DB: f32 = -50.0;
const FAST_CONVERGENCE_FRAMES: u32 = 5;
const SPEECH_LEVEL_FAST_RATE: f32 = 0.5;
const SPEECH_LEVEL_RISE_GAIN_RATE: f32 = 0.02;
const SPEECH_LEVEL_REDUCE_GAIN_RATE: f32 = 0.03;
const SPEECH_LEVEL_LOUD_RATE: f32 = 0.5;
const SPEECH_LEVEL_LOUD_THRESHOLD_DB: f32 = 6.0;
const MAXIMUM_FRAME_ERRORS: i32 = 4;
const MINIMUM_VOICED_FRACTION: f32 = 0.3;
const PITCH_CONTINUITY_TOLERANCE: f32 = 0.2;

/// What the AGC needs to know about the decoded frame.
#[derive(Clone, Copy, Debug)]
pub struct FrameInfo {
    /// Frame was a repeat of the previous parameters (decode failed or too many errors).
    pub repeat: bool,
    /// Corrected bit errors.
    pub errors: i32,
    /// Fraction of voiced bands in the lower half of the spectrum.
    pub voiced_fraction: f32,
    /// Fundamental frequency (any unit; only relative changes are used).
    pub fundamental: f32,
}

pub struct Agc {
    gain: f32,
    applied_gain: f32,
    noise_floor_db: f32,
    speech_level_db: f32,
    speech_frames: u32,
    previous_fundamental: f32,
    window: [f32; NOISE_FLOOR_WINDOW_FRAMES],
    window_count: usize,
}

impl Default for Agc {
    fn default() -> Self { Self::new() }
}

impl Agc {
    pub fn new() -> Self {
        Agc {
            gain: DEFAULT_GAIN,
            applied_gain: DEFAULT_GAIN,
            noise_floor_db: NOISE_FLOOR_INITIAL_MAXIMUM_DB,
            speech_level_db: MINIMUM_LEVEL_DB,
            speech_frames: 0,
            previous_fundamental: 0.0,
            window: [0.0; NOISE_FLOOR_WINDOW_FRAMES],
            window_count: 0,
        }
    }

    pub fn gain(&self) -> f32 { self.gain }

    /// Applies gain to one frame of samples scaled to the 16-bit range.
    pub fn process(&mut self, samples: &mut [f32], info: &FrameInfo) {
        let peak = samples.iter().fold(0.0f32, |p, s| p.max(s.abs()));
        let sum_squared: f64 = samples.iter().map(|&s| (s * s) as f64).sum();
        let level_db = to_db((sum_squared / samples.len() as f64).sqrt() as f32);
        self.update(level_db, info);

        let limit = if peak > 0.0 { MAXIMUM_AMPLITUDE / peak } else { MAXIMUM_GAIN };
        let start = self.applied_gain.min(limit);
        let end = self.gain.min(limit);
        let step = (end - start) / samples.len() as f32;
        for (x, s) in samples.iter_mut().enumerate() {
            *s = (*s * (start + step * (x + 1) as f32)).clamp(-MAXIMUM_AMPLITUDE, MAXIMUM_AMPLITUDE);
        }
        self.applied_gain = end;
    }

    fn update(&mut self, level_db: f32, info: &FrameInfo) {
        if info.repeat || info.errors > MAXIMUM_FRAME_ERRORS {
            self.previous_fundamental = 0.0;
            return;
        }
        self.update_noise_floor(level_db);
        let voiced = info.voiced_fraction >= MINIMUM_VOICED_FRACTION;
        let continuous = voiced && self.previous_fundamental > 0.0
            && (info.fundamental - self.previous_fundamental).abs() <= self.previous_fundamental * PITCH_CONTINUITY_TOLERANCE;
        self.previous_fundamental = if voiced { info.fundamental } else { 0.0 };
        let speech = continuous && level_db >= MINIMUM_SPEECH_LEVEL_DB && level_db >= self.noise_floor_db + SPEECH_THRESHOLD_DB;
        if speech {
            self.update_speech_level(level_db);
            let gain = 10f64.powf(((TARGET_SPEECH_LEVEL_DB - self.speech_level_db) / 20.0) as f64) as f32;
            self.gain = gain.clamp(MINIMUM_GAIN, MAXIMUM_GAIN);
        }
    }

    /// Noise floor: 5th percentile of the last 100 usable frame levels, capped at -50 dBFS until the window fills.
    fn update_noise_floor(&mut self, level_db: f32) {
        self.window[self.window_count % NOISE_FLOOR_WINDOW_FRAMES] = level_db;
        self.window_count += 1;
        let count = self.window_count.min(NOISE_FLOOR_WINDOW_FRAMES);
        let mut sorted = self.window;
        sorted[..count].sort_by(f32::total_cmp);
        let mut floor = sorted[(count as f32 * NOISE_FLOOR_PERCENTILE) as usize];
        if self.window_count < NOISE_FLOOR_WINDOW_FRAMES {
            floor = floor.min(NOISE_FLOOR_INITIAL_MAXIMUM_DB);
        }
        self.noise_floor_db = floor;
    }

    fn update_speech_level(&mut self, level_db: f32) {
        if self.speech_frames == 0 {
            self.speech_level_db = level_db;
        } else {
            let rate = if level_db > self.speech_level_db + SPEECH_LEVEL_LOUD_THRESHOLD_DB {
                SPEECH_LEVEL_LOUD_RATE
            } else if self.speech_frames < FAST_CONVERGENCE_FRAMES {
                SPEECH_LEVEL_FAST_RATE
            } else if level_db > self.speech_level_db {
                SPEECH_LEVEL_REDUCE_GAIN_RATE
            } else {
                SPEECH_LEVEL_RISE_GAIN_RATE
            };
            self.speech_level_db += (level_db - self.speech_level_db) * rate;
        }
        self.speech_frames += 1;
    }
}

fn to_db(rms: f32) -> f32 {
    if rms <= 0.0 { MINIMUM_LEVEL_DB } else { MINIMUM_LEVEL_DB.max((20.0 * ((rms / i16::MAX as f32) as f64).log10()) as f32) }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn speech() -> FrameInfo { FrameInfo { repeat: false, errors: 0, voiced_fraction: 1.0, fundamental: 0.1 } }

    fn tone(amplitude: f32) -> [f32; 160] {
        std::array::from_fn(|n| amplitude * (n as f32 * 0.3).sin())
    }

    fn rms_db(s: &[f32]) -> f32 { to_db((s.iter().map(|x| (x * x) as f64).sum::<f64>() / s.len() as f64).sqrt() as f32) }

    #[test]
    fn quiet_speech_is_raised_toward_target() {
        let mut agc = Agc::new();
        let mut last = [0.0; 160];
        // About -30 dBFS: above the speech threshold (noise floor capped at -50 dBFS, +9 dB) while the noise window
        // fills. (A constant level forever would become the noise floor itself and stop counting as speech.)
        for _ in 0..60 {
            last = tone(1500.0);
            agc.process(&mut last, &speech());
        }
        let level = rms_db(&last);
        assert!((level - TARGET_SPEECH_LEVEL_DB).abs() < 1.5, "{level} dBFS");
    }

    #[test]
    fn loud_speech_never_clips() {
        let mut agc = Agc::new();
        for _ in 0..50 {
            let mut f = tone(60_000.0); // would clip at unit gain
            agc.process(&mut f, &speech());
            assert!(f.iter().all(|s| s.abs() <= MAXIMUM_AMPLITUDE));
        }
    }

    #[test]
    fn repeats_errors_and_unvoiced_frames_hold_the_gain() {
        let mut agc = Agc::new();
        for _ in 0..20 { agc.process(&mut tone(1500.0), &speech()); }
        let g = agc.gain();
        assert!(g > 2.0, "speech set a gain: {g}");
        for info in [
            FrameInfo { repeat: true, ..speech() },
            FrameInfo { errors: 5, ..speech() },
            FrameInfo { voiced_fraction: 0.1, ..speech() },
        ] {
            for _ in 0..20 { agc.process(&mut tone(3000.0), &info); }
            assert_eq!(agc.gain(), g, "{info:?}");
        }
    }

    #[test]
    fn erratic_pitch_is_not_speech() {
        let mut agc = Agc::new();
        for n in 0..50 {
            let info = FrameInfo { fundamental: if n % 2 == 0 { 0.05 } else { 0.2 }, ..speech() };
            agc.process(&mut tone(1500.0), &info);
        }
        assert_eq!(agc.gain(), DEFAULT_GAIN);
    }
}
