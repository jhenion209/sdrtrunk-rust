//! Where a tuner's receiving window sits, ported from the Java reference's `TunerCoveragePlanner`. Channels that must
//! be received (the control channel, calls in progress) decide which centres are legal; among those, the centre that
//! covers the most known voice frequencies wins, then the one that moves least, then the lower one.

/// Half a 12.5 kHz P25 channel: a channel is covered only when all of it is.
// ponytail: placement uses nominal frequencies; the tuner's PPM error shifts the whole band by ppm x centre (about
// 2 kHz per ppm at 850 MHz). The usable-width margin absorbs a few ppm; add the shift here if a badly-off RTL-SDR
// fails to open channels near the window edges.
pub const HALF_CHANNEL: i64 = 6_250;

/// A tuner's window: centre, usable width (the weak band edges left out), the half-width around the centre to avoid
/// (an RTL-SDR's DC spike) and the tuner's frequency limits (centres and channels must stay inside them).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Window { pub center: i64, pub usable: i64, pub gap_half: i64, pub min: i64, pub max: i64 }

impl Window {
    /// RTL-SDR: 98% of the sample rate, 5 kHz either side of the centre avoided; Airspy: 90%, no gap (the Java
    /// reference's R8x and Airspy figures). A recording (`file:`) keeps the window of the tuner it was made with: its
    /// name starts with that tuner's label ("RTL-SDR_..." from `sdrtrunk-rust record`). No limit set: the whole range.
    pub fn for_tuner(id: &str, rate_hz: u32, center: i64, min_hz: Option<u64>, max_hz: Option<u64>) -> Window {
        let rtl = id.starts_with("rtl:") || id.starts_with("file:RTL-SDR_");
        let (share, gap_half) = if rtl { (0.98, 5_000) } else { (0.90, 0) };
        Window {
            center, usable: (rate_hz as f64 * share) as i64, gap_half,
            min: min_hz.map_or(1, |m| m as i64), max: max_hz.map_or(i64::MAX / 2, |m| m as i64),
        }
    }

    pub fn at(self, center: i64) -> Window { Window { center, ..self } }

    /// Inside the tuner's limits.
    pub fn allowed(&self, hz: i64) -> bool { hz - HALF_CHANNEL >= self.min && hz + HALF_CHANNEL <= self.max }

    /// The whole channel at `hz` is inside the limits and the usable width, and clear of the centre gap.
    pub fn covers(&self, hz: i64) -> bool {
        let (lo, hi, half) = (hz - HALF_CHANNEL, hz + HALF_CHANNEL, self.usable / 2);
        self.allowed(hz) && lo >= self.center - half && hi <= self.center + half
            && (self.gap_half <= 0 || hi < self.center - self.gap_half || lo > self.center + self.gap_half)
    }

    /// Lowest and highest frequency received usefully.
    pub fn edges(&self) -> (i64, i64) { (self.center - self.usable / 2, self.center + self.usable / 2) }
}

/// The centre for `window` that covers every `required` channel and as many `known` ones as possible (ties: the
/// current centre, then the nearest, then the lower). The centre stays inside the limits. None when the required
/// channels cannot all fit.
pub fn choose_center(required: &[i64], known: &[i64], window: Window) -> Option<i64> {
    let half = window.usable / 2;
    let (mut low, mut high) = (window.min, window.max);
    for &hz in required {
        if !window.allowed(hz) { return None; }
        low = low.max(hz + HALF_CHANNEL - half);
        high = high.min(hz - HALF_CHANNEL + half);
    }
    if low > high { return None; }
    let useful: Vec<i64> = known.iter().copied().filter(|&hz| window.allowed(hz)).collect();
    // Candidates come from window edges and centre-gap edges around each channel, not a one-hertz search.
    let mut candidates = vec![window.center, low, high, low + (high - low) / 2];
    for &hz in useful.iter().chain(required) {
        candidates.extend([hz + HALF_CHANNEL - half, hz - HALF_CHANNEL + half,
            hz + HALF_CHANNEL + window.gap_half + 1, hz - HALF_CHANNEL - window.gap_half - 1]);
    }
    let mut best: Option<(usize, i64, i64)> = None;
    for c in candidates.into_iter().map(|c| c.clamp(low, high)) {
        let w = window.at(c);
        if !required.iter().all(|&hz| w.covers(hz)) { continue; }
        let count = useful.iter().filter(|&&hz| w.covers(hz)).count();
        let moved = (c - window.center).abs();
        let better = match best {
            None => true,
            Some((n, m, b)) => count > n || count == n && (moved < m || moved == m && c < b),
        };
        if better { best = Some((count, moved, c)); }
    }
    best.map(|(_, _, c)| c)
}

#[cfg(test)]
mod tests {
    use super::*;

    const RTL: &str = "rtl:00000001";
    const CONTROL: i64 = 852_112_500;

    fn rtl(center: i64) -> Window { Window::for_tuner(RTL, 2_400_000, center, None, None) }

    #[test]
    fn window_rules() {
        let w = rtl(853_000_000);
        assert_eq!((w.usable, w.gap_half), (2_352_000, 5_000));
        assert!(w.covers(851_831_000) && !w.covers(851_829_000), "usable edge: all of the channel inside");
        assert!(!w.covers(853_000_000) && !w.covers(853_010_000) && w.covers(853_012_000), "DC gap avoided");
        let a = Window::for_tuner("735863DC-2F67AFE3", 6_000_000, 859_000_000, Some(851_000_000), Some(860_000_000));
        assert_eq!((a.usable, a.gap_half), (5_400_000, 0));
        assert!(a.covers(859_000_000), "an Airspy has no centre gap");
        assert!(!a.covers(860_000_000) && !a.allowed(850_999_000), "frequency limits");
    }

    #[test]
    fn recordings_keep_their_tuners_window() {
        let r = Window::for_tuner("file:RTL-SDR_00000001_852730250_baseband_20261116_120000", 2_400_000, 852_730_250, None, None);
        assert_eq!((r.usable, r.gap_half), (2_352_000, 5_000), "an RTL-SDR recording: RTL width and centre gap");
        let a = Window::for_tuner("file:Airspy_35AC63DC-2D40B04F_858912500_baseband_20261116_120000", 6_000_000, 858_912_500, None, None);
        assert_eq!((a.usable, a.gap_half), (5_400_000, 0));
    }

    #[test]
    fn nothing_known_keeps_the_centre() {
        assert_eq!(choose_center(&[CONTROL], &[], rtl(CONTROL - 50_000)), Some(CONTROL - 50_000));
    }

    #[test]
    fn covers_control_and_known_voice() {
        // KSICS: control 852.1125 MHz, voice seen on 853.475, 853.625 and 853.9 MHz; the window starts 50 kHz below control.
        let known = [853_475_000, 853_625_000, 853_900_000];
        let c = choose_center(&[CONTROL], &known, rtl(CONTROL - 50_000)).unwrap();
        let w = rtl(c);
        assert!(w.covers(CONTROL) && known.iter().all(|&k| w.covers(k)), "centre {c}");
        // A frequency no window can add alongside control does not stop the rest being covered.
        let c = choose_center(&[CONTROL], &[853_900_000, 860_000_000], rtl(CONTROL - 50_000)).unwrap();
        assert!(rtl(c).covers(853_900_000));
    }

    #[test]
    fn required_channels_must_all_fit() {
        assert_eq!(choose_center(&[CONTROL, CONTROL + 3_000_000], &[], rtl(CONTROL)), None, "3 MHz apart on a 2.4 MS/s tuner");
        let c = choose_center(&[CONTROL, 853_900_000], &[], rtl(CONTROL - 50_000)).unwrap();
        assert!(rtl(c).covers(CONTROL) && rtl(c).covers(853_900_000));
        assert!(c - (CONTROL - 50_000) <= 1_800_000, "moves only as far as needed: {c}");
    }

    #[test]
    fn limits_respected() {
        let w = Window::for_tuner(RTL, 2_400_000, CONTROL - 50_000, Some(851_000_000), Some(852_500_000));
        assert_eq!(choose_center(&[853_900_000], &[], w), None, "outside the limits");
        let c = choose_center(&[CONTROL], &[853_500_000], w).unwrap();
        assert!(w.at(c).covers(CONTROL) && !w.at(c).covers(853_500_000) && c <= 852_500_000, "centre inside the limits: {c}");
    }
}
