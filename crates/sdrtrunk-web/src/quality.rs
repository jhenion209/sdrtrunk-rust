//! Control channel signal and decode quality, measured as the Java reference does: one bucket per second (control channel power in
//! dBFS, frame counts), a 30 s rolling summary for the Status page and one history sample per minute (kept 30 days).
//! Decode quality = valid / (valid + invalid + bits lost to sync loss or drops / 196), in percent.
use serde::{Deserialize, Serialize};
use std::collections::VecDeque;
use sdrtrunk_p25::control::{FrameCounts, FRAME_BITS};

/// History is kept this long.
pub const KEEP_MS: i64 = 30 * 86_400_000;
/// Seconds of buckets a receiver holds (one history sample).
const WINDOW_S: usize = 60;

/// One second of the control channel: its average power (None before samples arrive) and the frames in that second.
#[derive(Clone, Copy, Debug, Default)]
pub struct Bucket { pub at_ms: i64, pub signal_db: Option<f64>, pub frames: FrameCounts }

/// Signal (dBFS: average, lowest and highest second, latest second) and decode quality over a span.
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Summary {
    pub signal_db: Option<f64>,
    pub signal_min: Option<f64>,
    pub signal_max: Option<f64>,
    pub current_db: Option<f64>,
    pub valid: u64,
    /// valid + invalid + lost bits / 196 (frames that could have decoded).
    pub attempted: f64,
}

impl Summary {
    pub fn decode_pct(&self) -> Option<f64> {
        (self.attempted > 0.0).then(|| (100.0 * self.valid as f64 / self.attempted).clamp(0.0, 100.0))
    }

    pub fn is_empty(&self) -> bool { self.signal_db.is_none() && self.attempted == 0.0 }

    /// Several summaries as one: signal averaged in power, extremes kept, frames added.
    pub fn merge<'a>(all: impl IntoIterator<Item = &'a Summary>) -> Summary {
        let mut out = Summary::default();
        let (mut power, mut n) = (0.0, 0);
        for s in all {
            if let Some(db) = s.signal_db { power += 10f64.powf(db / 10.0); n += 1; }
            out.signal_min = min(out.signal_min, s.signal_min);
            out.signal_max = max(out.signal_max, s.signal_max);
            out.current_db = s.current_db.or(out.current_db);
            out.valid += s.valid;
            out.attempted += s.attempted;
        }
        out.signal_db = (n > 0).then(|| 10.0 * (power / n as f64).log10());
        out
    }
}

fn min(a: Option<f64>, b: Option<f64>) -> Option<f64> { [a, b].into_iter().flatten().reduce(f64::min) }
fn max(a: Option<f64>, b: Option<f64>) -> Option<f64> { [a, b].into_iter().flatten().reduce(f64::max) }

impl From<&Bucket> for Summary {
    fn from(b: &Bucket) -> Summary {
        let f = b.frames;
        Summary {
            signal_db: b.signal_db, signal_min: b.signal_db, signal_max: b.signal_db, current_db: b.signal_db,
            valid: f.valid, attempted: (f.valid + f.invalid) as f64 + f.lost_bits as f64 / FRAME_BITS,
        }
    }
}

/// The last minute of buckets of a running system.
#[derive(Default)]
pub struct Window { buckets: VecDeque<Bucket> }

impl Window {
    pub fn push(&mut self, b: Bucket) {
        self.buckets.push_back(b);
        while self.buckets.len() > WINDOW_S { self.buckets.pop_front(); }
    }

    /// The buckets of the last `secs` seconds before `now_ms`.
    pub fn summary(&self, now_ms: i64, secs: i64) -> Summary {
        let from = now_ms - secs * 1000;
        let recent: Vec<Summary> = self.buckets.iter().filter(|b| b.at_ms > from).map(Summary::from).collect();
        Summary::merge(&recent)
    }
}

/// History samples reduced to at most `points` evenly spaced ones between `from_ms` and `to_ms` (for a chart).
pub fn downsample(samples: &[(i64, Summary)], from_ms: i64, to_ms: i64, points: usize) -> Vec<(i64, Summary)> {
    let span = ((to_ms - from_ms) as f64 / points as f64).max(1.0);
    let centre = |slot: i64| from_ms + ((slot as f64 + 0.5) * span) as i64;
    let mut out: Vec<(i64, Summary)> = Vec::new();
    let mut group: Vec<Summary> = Vec::new();
    let mut slot = None;
    for &(at, s) in samples.iter().filter(|(at, _)| (from_ms..=to_ms).contains(at)) {
        let k = ((at - from_ms) as f64 / span) as i64;
        if let Some(p) = slot.filter(|&p| p != k) {
            out.push((centre(p), Summary::merge(&group)));
            group.clear();
        }
        slot = Some(k);
        group.push(s);
    }
    if let Some(p) = slot { out.push((centre(p), Summary::merge(&group))); }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bucket(at_s: i64, db: Option<f64>, valid: u64, invalid: u64, lost_bits: u64) -> Bucket {
        Bucket { at_ms: at_s * 1000, signal_db: db, frames: FrameCounts { valid, invalid, lost_bits } }
    }

    #[test]
    fn window_summary() {
        let mut w = Window::default();
        for s in 0..100 { w.push(bucket(s, Some(-40.0), 10, 0, 0)); }
        w.push(bucket(100, Some(-30.0), 8, 1, 196));
        w.push(bucket(101, None, 0, 0, 0));
        let s = w.summary(101_000, 30);
        assert_eq!((s.signal_min, s.signal_max, s.current_db), (Some(-40.0), Some(-30.0), Some(-30.0)));
        assert!((s.signal_db.unwrap() - 10.0 * ((28.0 * 1e-4 + 1e-3) / 29.0f64).log10()).abs() < 1e-9, "averaged in power");
        assert_eq!(s.valid, 28 * 10 + 8);
        assert!((s.decode_pct().unwrap() - 100.0 * 288.0 / 290.0).abs() < 1e-9, "one invalid frame and one lost frame");
        assert_eq!(w.summary(101_000, 3600).valid, 58 * 10 + 8, "only the last minute is kept");
        assert!(Window::default().summary(0, 30).is_empty());
        assert_eq!(Summary::default().decode_pct(), None, "nothing attempted: no percentage");
    }

    #[test]
    fn downsample_keeps_extremes() {
        let s = |db: f64, valid: u64| Summary { signal_db: Some(db), signal_min: Some(db - 5.0), signal_max: Some(db + 5.0),
            current_db: Some(db), valid, attempted: 10.0 };
        let samples: Vec<(i64, Summary)> = (0..60).map(|m| (m * 60_000, s(-50.0 + (m % 2) as f64 * 10.0, 10 - (m % 2) as u64))).collect();
        let d = downsample(&samples, 0, 3_600_000, 6);
        assert_eq!(d.len(), 6);
        assert_eq!(d[0].0, 300_000, "centre of each slot");
        assert_eq!((d[0].1.signal_min, d[0].1.signal_max), (Some(-55.0), Some(-35.0)));
        assert!((d[0].1.decode_pct().unwrap() - 95.0).abs() < 1e-9);
        assert_eq!(downsample(&samples, 0, 3_600_000, 300).len(), 60, "fewer samples than points: all of them");
        assert!(downsample(&samples, 4_000_000, 5_000_000, 10).is_empty());
    }
}
