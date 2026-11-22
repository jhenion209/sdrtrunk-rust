//! Receiver health for Uptime Kuma: what is wrong with one system right now, as short problem lines (see `/health`).
use crate::quality::Summary;
use crate::receiver::Status;

/// Quality is averaged over this long, and failures counted over this long (ms).
pub const QUALITY_WINDOW_MS: i64 = 300_000;
pub const FLAP_WINDOW_MS: i64 = 900_000;
/// Grace before a system that is not running counts (a starting one gets longer), and before a silent control channel does.
const DOWN_MS: i64 = 30_000;
const STARTING_MS: i64 = 60_000;
const MIN_QUALITY_PCT: f64 = 80.0;
const MAX_FAILURES: usize = 3;

/// What is known about one enabled system now.
pub struct Facts<'a> {
    /// None: the system is not running at all.
    pub status: Option<&'a Status>,
    /// Its stored quality samples of the last `QUALITY_WINDOW_MS`.
    pub quality: &'a [Summary],
    /// Tuners connected now.
    pub connected: &'a [String],
    /// The tuner the system is set to (None: Automatic).
    pub configured_tuner: Option<String>,
    /// Times it failed and was restarted in the last `FLAP_WINDOW_MS`.
    pub failures: usize,
}

/// The system's problems, in a fixed order; empty when healthy.
pub fn problems(now_ms: i64, f: &Facts) -> Vec<String> {
    let mut out = Vec::new();
    let Some(st) = f.status else { return vec!["not running (stopped)".into()] };
    if st.state != "running" {
        let (since, grace) = match st.state.as_str() {
            "starting" => (st.started_ms, STARTING_MS),
            "error" => (st.error_ms, DOWN_MS),
            _ => (st.started_ms, DOWN_MS),
        };
        if now_ms - since > grace {
            out.push(match &st.error { Some(e) => format!("not running ({}: {e})", st.state), None => format!("not running ({})", st.state) });
        }
    } else if now_ms - st.last_control_ms > DOWN_MS {
        out.push(format!("control channel lost for {} s", (now_ms - st.last_control_ms) / 1000));
    }
    let q = Summary::merge(f.quality);
    if let Some(p) = q.decode_pct().filter(|p| q.attempted >= 1.0 && *p < MIN_QUALITY_PCT) {
        out.push(format!("decode quality {p:.0} % (5 min)"));
    }
    if let Some(e) = &st.recording_error { out.push(format!("recording error: {e}")); }
    let held = f.configured_tuner.as_deref().or(st.tuner.as_deref());
    let gone = |id: &str| !id.starts_with(sdrtrunk_source::FILE_PREFIX) && !f.connected.iter().any(|c| c == id);
    if held.is_some_and(gone) || st.helpers.iter().any(|(id, _)| gone(id)) { out.push("tuner missing".into()); }
    if f.failures >= MAX_FAILURES { out.push(format!("tuner dropped {} times in 15 min", f.failures)); }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::quality::Summary;
    use crate::receiver::Status;

    const NOW: i64 = 1_000_000;

    fn status(state: &str, started_ms: i64, error_ms: i64, last_control_ms: i64) -> Status {
        Status { state: state.into(), started_ms, error_ms, last_control_ms, tuner: Some("A".into()), ..Status::default() }
    }
    fn pct(p: f64) -> Summary { Summary { valid: (p * 10.0) as u64, attempted: 1000.0, ..Summary::default() } }
    fn facts<'a>(st: Option<&'a Status>, quality: &'a [Summary], connected: &'a [String]) -> Facts<'a> {
        Facts { status: st, quality, connected, configured_tuner: Some("A".into()), failures: 0 }
    }
    fn conn(ids: &[&str]) -> Vec<String> { ids.iter().map(|s| s.to_string()).collect() }

    #[test]
    fn healthy_running_system_has_no_problems() {
        let st = status("running", 0, 0, NOW - 1000);
        assert_eq!(problems(NOW, &facts(Some(&st), &[pct(99.0)], &conn(&["A"]))), Vec::<String>::new());
    }

    #[test]
    fn starting_gets_60_s_then_not_running() {
        let c = conn(&["A"]);
        let st = status("starting", NOW - 59_000, 0, NOW - 59_000);
        assert!(problems(NOW, &facts(Some(&st), &[], &c)).is_empty());
        let st = status("starting", NOW - 61_000, 0, NOW - 61_000);
        assert_eq!(problems(NOW, &facts(Some(&st), &[], &c)), vec!["not running (starting)"]);
    }

    #[test]
    fn error_after_30_s_names_error() {
        let c = conn(&["A"]);
        let mut st = status("error", 0, NOW - 31_000, 0);
        st.error = Some("airspy: no device".into());
        assert_eq!(problems(NOW, &facts(Some(&st), &[], &c)), vec!["not running (error: airspy: no device)"]);
        st.error_ms = NOW - 29_000;
        assert!(problems(NOW, &facts(Some(&st), &[], &c)).is_empty());
    }

    #[test]
    fn never_started_is_stopped() {
        assert_eq!(problems(NOW, &facts(None, &[], &conn(&["A"]))), vec!["not running (stopped)"]);
    }

    #[test]
    fn control_lost_after_30_s() {
        let c = conn(&["A"]);
        let st = status("running", 0, 0, NOW - 45_000);
        assert_eq!(problems(NOW, &facts(Some(&st), &[], &c)), vec!["control channel lost for 45 s"]);
        let st = status("running", 0, 0, NOW - 30_000);
        assert!(problems(NOW, &facts(Some(&st), &[], &c)).is_empty());
    }

    #[test]
    fn quality_under_80_over_window() {
        let c = conn(&["A"]);
        let st = status("running", 0, 0, NOW);
        let low = [pct(60.0), pct(64.8)];
        assert!(problems(NOW, &facts(Some(&st), &low, &c)).contains(&"decode quality 62 % (5 min)".to_string()));
        assert!(problems(NOW, &facts(Some(&st), &[pct(80.0)], &c)).is_empty());
        assert!(problems(NOW, &facts(Some(&st), &[], &c)).is_empty());
    }

    #[test]
    fn recording_error_reported() {
        let mut st = status("running", 0, 0, NOW);
        st.recording_error = Some("disk full".into());
        assert!(problems(NOW, &facts(Some(&st), &[], &conn(&["A"]))).contains(&"recording error: disk full".to_string()));
    }

    #[test]
    fn tuner_missing_configured_or_helper() {
        let missing = |p: Vec<String>| p.iter().filter(|s| *s == "tuner missing").count();
        let mut st = status("running", 0, 0, NOW);
        assert_eq!(missing(problems(NOW, &facts(Some(&st), &[], &conn(&["B"])))), 1, "configured tuner gone");
        let auto = |st: &Status, c: &[String]| problems(NOW, &Facts { configured_tuner: None, ..facts(Some(st), &[], c) });
        st.tuner = Some("B".into());
        assert_eq!(missing(auto(&st, &conn(&["B"]))), 0, "Automatic: the held tuner is there");
        st.tuner = None;
        assert_eq!(missing(auto(&st, &conn(&[]))), 0, "Automatic, nothing held: not missing");
        let mut st = status("running", 0, 0, NOW);
        st.helpers = vec![("C".into(), 851_000_000)];
        assert_eq!(missing(problems(NOW, &facts(Some(&st), &[], &conn(&["A"])))), 1, "helper gone");
        st.helpers.push(("D".into(), 852_000_000));
        assert_eq!(missing(problems(NOW, &facts(Some(&st), &[], &conn(&["B"])))), 1, "said once");
    }

    #[test]
    fn replay_file_is_never_missing() {
        let mut st = status("running", 0, 0, NOW);
        st.tuner = Some("file:capture.wav".into());
        let f = Facts { configured_tuner: Some("file:capture.wav".into()), ..facts(Some(&st), &[], &[]) };
        assert!(problems(NOW, &f).is_empty());
    }

    #[test]
    fn flapping_at_3_failures() {
        let st = status("running", 0, 0, NOW);
        let c = conn(&["A"]);
        assert!(problems(NOW, &Facts { failures: 2, ..facts(Some(&st), &[], &c) }).is_empty());
        assert_eq!(problems(NOW, &Facts { failures: 3, ..facts(Some(&st), &[], &c) }), vec!["tuner dropped 3 times in 15 min"]);
    }

    #[test]
    fn problems_in_table_order() {
        let mut st = status("running", 0, 0, NOW - 40_000);
        st.recording_error = Some("x".into());
        let (q, c) = ([pct(10.0)], conn(&[]));
        let f = Facts { failures: 4, ..facts(Some(&st), &q, &c) };
        assert_eq!(problems(NOW, &f), vec!["control channel lost for 40 s", "decode quality 10 % (5 min)", "recording error: x",
            "tuner missing", "tuner dropped 4 times in 15 min"]);
    }
}
