//! Automatic frequency correction as the Java reference does it (`ChannelFrequencyErrorManager` + `TunerFrequencyErrorManager`).
//! The decoder's carrier PLL measures how far the control channel sits from where it is decoded. Every 500 ms the
//! channel moves at most 10 Hz toward the average of those measurements. Every 5 s, when that channel correction
//! is above 50 Hz and Automatic PPM is on, the tuner PPM takes over up to 10 Hz of it, staying within 3 PPM of the
//! PPM it started from (so a quiet or noisy tuner cannot run away).

/// How often `tick` is called.
pub const TICK_MS: i64 = 500;
const CHANNEL_STEP_HZ: f64 = 10.0;
const TUNER_STEP_HZ: f64 = 10.0;
const TUNER_THRESHOLD_HZ: f64 = 50.0;
const TUNER_EVERY_TICKS: u32 = 10;
const MAX_EXCURSION_PPM: f64 = 3.0;
const AVERAGE_OVER: usize = 6;

#[derive(Clone, Debug)]
pub struct Correction {
    /// Tuner error in PPM (decode at f - ppm*f/1e6).
    pub ppm: f64,
    /// Channel correction in Hz (added to where the channel is decoded).
    pub channel_hz: f64,
    /// Frequency error measured over the last 5 s (the channel correction), None without a decoded signal.
    pub measured_hz: Option<f64>,
    pub auto: bool,
    baseline: f64,
    requests: Vec<f64>,
    pending: bool,
    fresh: bool,
    ticks: u32,
}

impl Correction {
    pub fn new(ppm: f64, auto: bool) -> Correction {
        Correction { ppm, channel_hz: 0.0, measured_hz: None, auto, baseline: ppm, requests: Vec::new(), pending: false, fresh: false, ticks: 0 }
    }

    /// A PLL measurement (Hz the signal sits above where it is decoded), taken when frames decoded.
    pub fn measure(&mut self, hz: f64) {
        if self.requests.len() == AVERAGE_OVER { self.requests.remove(0); }
        self.requests.push(hz);
        self.pending = true;
    }

    /// The channel changed (failover): its correction starts over; the tuner PPM stays.
    pub fn reset_channel(&mut self) {
        self.channel_hz = 0.0;
        self.requests.clear();
        (self.pending, self.fresh, self.measured_hz) = (false, false, None);
    }

    /// Every `TICK_MS`, with the frequency being decoded. True when `ppm` or `channel_hz` changed.
    pub fn tick(&mut self, freq_hz: f64) -> bool {
        let mut changed = false;
        if self.pending {
            let average = self.requests.iter().sum::<f64>() / self.requests.len() as f64;
            let step = average.round().clamp(-CHANNEL_STEP_HZ, CHANNEL_STEP_HZ);
            if step != 0.0 {
                self.channel_hz += step;
                self.requests.clear();
                changed = true;
            }
            (self.pending, self.fresh) = (false, true);
        }
        self.ticks += 1;
        if self.ticks < TUNER_EVERY_TICKS { return changed; }
        self.ticks = 0;
        if !self.fresh {
            self.measured_hz = None;
            return changed;
        }
        self.fresh = false;
        self.measured_hz = Some(self.channel_hz);
        if self.auto && self.channel_hz.abs() > TUNER_THRESHOLD_HZ && freq_hz > 0.0 {
            let step = self.channel_hz.clamp(-TUNER_STEP_HZ, TUNER_STEP_HZ);
            let ppm = (self.ppm - step * 1e6 / freq_hz).clamp(self.baseline - MAX_EXCURSION_PPM, self.baseline + MAX_EXCURSION_PPM);
            if ppm != self.ppm { self.ppm = ppm; changed = true; }
        }
        changed
    }

    /// Where the channel at `freq_hz` is decoded.
    pub fn decode_hz(&self, freq_hz: i64) -> i64 { crate::control::decode_hz(freq_hz, self.ppm, self.channel_hz) }
}

#[cfg(test)]
mod tests {
    use super::*;

    const F: f64 = 852_112_500.0;

    /// Runs `secs` of ticks against a signal `error_hz` above nominal; the PLL reads what is left.
    fn run(c: &mut Correction, error_hz: f64, secs: u32) {
        for _ in 0..secs * 2 {
            let residual = F + error_hz - c.decode_hz(F as i64) as f64;
            c.measure(residual);
            c.tick(F);
        }
    }

    #[test]
    fn auto_ppm_takes_over_the_error() {
        let mut c = Correction::new(0.0, true);
        run(&mut c, 400.0, 600);
        let left = F + 400.0 - c.decode_hz(F as i64) as f64;
        assert!(left.abs() <= 10.0, "channel locked on: {left}");
        assert!((c.ppm + 400.0 / F * 1e6).abs() < 0.07, "tuner absorbed it: {} ppm", c.ppm);
        assert!(c.channel_hz.abs() <= 60.0, "channel correction handed to the tuner: {}", c.channel_hz);
        assert!(c.measured_hz.is_some());
    }

    #[test]
    fn auto_ppm_limited_to_three_ppm() {
        let mut c = Correction::new(-1.0, true);
        // The tuner moves 10 Hz per 5 s: 3 PPM (2556 Hz) takes about 21 minutes.
        run(&mut c, 5000.0, 1800);
        assert!((c.ppm - (-4.0)).abs() < 1e-9, "stops 3 PPM from where it started: {}", c.ppm);
        let left = F + 5000.0 - c.decode_hz(F as i64) as f64;
        assert!(left.abs() <= 10.0, "channel correction covers the rest: {left}");
    }

    #[test]
    fn manual_ppm_never_changes() {
        let mut c = Correction::new(1.5, false);
        run(&mut c, 300.0, 120);
        assert_eq!(c.ppm, 1.5);
        let want = 300.0 + 1.5 * F / 1e6;
        assert!((c.channel_hz - want).abs() <= 10.0, "the channel still follows: {} (want {want})", c.channel_hz);
    }

    #[test]
    fn no_signal_no_change() {
        let mut c = Correction::new(0.5, true);
        for _ in 0..40 { assert!(!c.tick(F)); }
        assert_eq!((c.ppm, c.channel_hz, c.measured_hz), (0.5, 0.0, None));
        c.measure(30.0);
        c.reset_channel();
        for _ in 0..20 { c.tick(F); }
        assert_eq!(c.channel_hz, 0.0, "a reset drops pending measurements");
    }
}
