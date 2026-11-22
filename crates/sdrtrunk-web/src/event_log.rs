//! Call event log: one sdrtrunk-style CSV row per call, appended to a daily `<yyyyMMdd>_<system>_call_events.log`.
//! A call is keyed by frequency: it opens at its first grant, stays open while grants for the same talkgroup repeat or
//! the recorder hears it, and is written when another talkgroup takes the frequency, after `IDLE_MS` of quiet, or when
//! the system stops.
use crate::recorder::RecordPolicy;
use chrono::{Local, TimeZone};
use sdrtrunk_p25::trunk::CallGrant;
use sdrtrunk_p25::tsbk_msg::Channel;
use std::collections::HashMap;
use std::io::Write;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

/// A call ends this long after its last grant or voice frame (ms).
pub const IDLE_MS: i64 = 3000;

pub const HEADER: &str = "TIMESTAMP,DURATION_MS,PROTOCOL,EVENT,FROM,TO,CHANNEL_NUMBER,FREQUENCY,TIMESLOT,DETAILS,EVENT_ID";

/// A transmission the recorder hears now (see `Recorder::live`).
#[derive(Clone, Debug, PartialEq)]
pub struct Live { pub hz: u64, pub group: u16, pub source: u32, pub encrypted: bool }

#[derive(Clone, Debug)]
struct OpenCall { start_ms: i64, last_ms: i64, group: u16, source: u32, channel: Channel, hz: u64, encrypted: bool, emergency: bool }

pub struct EventLog {
    dir: Option<PathBuf>,
    system: String,
    policy: Arc<Mutex<RecordPolicy>>,
    next_id: u64,
    open: HashMap<u64, OpenCall>,
    pub last_error: Option<String>,
}

impl EventLog {
    /// `dir` None: logging is off and every method does nothing. `first_id` is the first EVENT_ID (the caller passes the
    /// current time in ms, so IDs keep growing across restarts).
    pub fn new(dir: Option<PathBuf>, system: &str, policy: Arc<Mutex<RecordPolicy>>, first_id: u64) -> EventLog {
        EventLog { dir, system: system.into(), policy, next_id: first_id, open: HashMap::new(), last_error: None }
    }

    pub fn grant(&mut self, g: &CallGrant, now_ms: i64) {
        if self.dir.is_none() { return; }
        if let Some(c) = self.open.get_mut(&g.hz) {
            if c.group == g.group {
                c.last_ms = now_ms;
                if c.source == 0 { c.source = g.source; }
                c.encrypted |= g.encrypted;
                c.emergency |= g.emergency;
                return;
            }
            let ended = self.open.remove(&g.hz).expect("present");
            self.write(&ended);
        }
        self.open.insert(g.hz, OpenCall { start_ms: now_ms, last_ms: now_ms, group: g.group, source: g.source, channel: g.channel,
            hz: g.hz, encrypted: g.encrypted, emergency: g.emergency });
    }

    /// The recorder's live transmissions keep their calls open and add what the voice says (source, encryption).
    pub fn live(&mut self, live: &[Live], now_ms: i64) {
        for l in live {
            let Some(c) = self.open.get_mut(&l.hz).filter(|c| c.group == l.group) else { continue };
            c.last_ms = now_ms;
            if c.source == 0 { c.source = l.source; }
            c.encrypted |= l.encrypted;
        }
    }

    pub fn tick(&mut self, now_ms: i64) {
        let idle: Vec<u64> = self.open.iter().filter(|(_, c)| now_ms - c.last_ms > IDLE_MS).map(|(&hz, _)| hz).collect();
        self.close(idle);
    }

    pub fn close_all(&mut self, _now_ms: i64) {
        let all: Vec<u64> = self.open.keys().copied().collect();
        self.close(all);
    }

    fn close(&mut self, mut hzs: Vec<u64>) {
        let mut ended: Vec<OpenCall> = hzs.drain(..).filter_map(|hz| self.open.remove(&hz)).collect();
        ended.sort_by_key(|c| c.start_ms);
        for c in ended { self.write(&c); }
    }

    fn write(&mut self, c: &OpenCall) {
        let Some(dir) = self.dir.clone() else { return };
        let name = {
            let p = self.policy.lock().unwrap_or_else(|p| p.into_inner());
            p.talkgroups.iter().find(|t| t.number == c.group).map(|t| t.name.clone())
        };
        let line = row(c, name.as_deref(), self.next_id);
        self.next_id += 1;
        let path = dir.join(file_name(&self.system, c.start_ms));
        let result = std::fs::create_dir_all(&dir).and_then(|_| {
            let mut f = std::fs::OpenOptions::new().append(true).create(true).open(&path)?;
            let mut out = String::new();
            if f.metadata()?.len() == 0 { out.push_str(HEADER); out.push('\n'); }
            out.push_str(&line);
            f.write_all(out.as_bytes())?;
            f.flush()
        });
        match result {
            Ok(()) => self.last_error = None,
            Err(e) => {
                eprintln!("event log: {}: {e}", path.display());
                self.last_error = Some(format!("event log: {}: {e}", path.display()));
            }
        }
    }
}

/// A stopping system (stop, failure, panic) writes the calls still open.
impl Drop for EventLog {
    fn drop(&mut self) { self.close_all(0); }
}

fn local(ms: i64) -> chrono::DateTime<Local> { Local.timestamp_millis_opt(ms).single().unwrap_or_else(Local::now) }

fn quote(s: &str) -> String { format!("\"{}\"", s.replace('"', "\"\"")) }

/// One CSV line in sdrtrunk's call event format, `\n` included.
fn row(c: &OpenCall, name: Option<&str>, id: u64) -> String {
    let to = match name { Some(n) if !n.is_empty() => format!("[{n}] ({})", c.group), _ => c.group.to_string() };
    let mut details = String::from("PHASE 1 GROUP CALL");
    if c.encrypted { details.push_str(" ENCRYPTED"); }
    if c.emergency { details.push_str(" EMERGENCY"); }
    let fields = [
        local(c.start_ms).format("%Y:%m:%d:%H:%M:%S").to_string(),
        (c.last_ms - c.start_ms).max(0).to_string(),
        "APCO-25".into(),
        "Group Call".into(),
        if c.source == 0 { String::new() } else { c.source.to_string() },
        to,
        format!("{}-{}", c.channel.band, c.channel.number),
        format!("{:.6}", c.hz as f64 / 1e6),
        String::new(),
        details,
        id.to_string(),
    ];
    let mut line = fields.iter().map(|f| quote(f)).collect::<Vec<_>>().join(",");
    line.push('\n');
    line
}

/// `<yyyyMMdd>_<system>_call_events.log` for the local date of `start_ms`; characters outside `[A-Za-z0-9._-]` in the
/// system name become `_`, so the name never leaves the folder.
fn file_name(system: &str, start_ms: i64) -> String {
    let safe: String = system.chars().map(|c| if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') { c } else { '_' }).collect();
    format!("{}_{safe}_call_events.log", local(start_ms).format("%Y%m%d"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::Talkgroup;
    use crate::recorder::RecordPolicy;
    use chrono::{Local, TimeZone};
    use sdrtrunk_p25::trunk::CallGrant;
    use sdrtrunk_p25::tsbk_msg::Channel;
    use std::path::{Path, PathBuf};
    use std::sync::{Arc, Mutex};

    const HZ: u64 = 859_737_500;

    fn ms(y: i32, mo: u32, d: u32, h: u32, mi: u32, s: u32) -> i64 {
        Local.with_ymd_and_hms(y, mo, d, h, mi, s).single().unwrap().timestamp_millis()
    }
    fn dir(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("sdrtrunk-event-log-{name}"));
        let _ = std::fs::remove_dir_all(&d);
        let _ = std::fs::remove_file(&d);
        d
    }
    fn policy() -> Arc<Mutex<RecordPolicy>> {
        Arc::new(Mutex::new(RecordPolicy {
            talkgroups: vec![Talkgroup { number: 27, name: "Sedgwick Co EMS".into(), record: true }],
            ..RecordPolicy::default()
        }))
    }
    fn g(group: u16, source: u32, hz: u64, encrypted: bool, emergency: bool) -> CallGrant {
        CallGrant { group, source, channel: Channel { band: 0, number: 4054 }, hz, encrypted, emergency }
    }
    fn log(d: &Path, first_id: u64) -> EventLog { EventLog::new(Some(d.to_path_buf()), "SGCo", policy(), first_id) }
    fn lines(d: &Path, day: &str) -> Vec<String> {
        std::fs::read_to_string(d.join(format!("{day}_SGCo_call_events.log"))).unwrap().lines().map(str::to_string).collect()
    }
    fn call(start_ms: i64, last_ms: i64, group: u16, source: u32, encrypted: bool, emergency: bool) -> OpenCall {
        OpenCall { start_ms, last_ms, group, source, channel: Channel { band: 0, number: 4054 }, hz: HZ, encrypted, emergency }
    }
    const HEADER: &str = "TIMESTAMP,DURATION_MS,PROTOCOL,EVENT,FROM,TO,CHANNEL_NUMBER,FREQUENCY,TIMESLOT,DETAILS,EVENT_ID";

    #[test]
    fn row_quotes_fields() {
        let t = ms(2026, 11, 20, 13, 39, 30);
        assert_eq!(row(&call(t, t + 2862, 27, 911, false, false), Some("Sedgwick Co EMS"), 7),
            "\"2026:11:20:13:39:30\",\"2862\",\"APCO-25\",\"Group Call\",\"911\",\"[Sedgwick Co EMS] (27)\",\"0-4054\",\"859.737500\",\"\",\"PHASE 1 GROUP CALL\",\"7\"\n");
        let quoted = row(&call(t, t, 85, 0, true, true), Some("Fire, \"Ops\" 2"), 8);
        assert!(quoted.contains(",\"\",\"[Fire, \"\"Ops\"\" 2] (85)\","), "{quoted}");
        assert!(quoted.contains("\"PHASE 1 GROUP CALL ENCRYPTED EMERGENCY\""), "{quoted}");
        assert!(row(&call(t, t, 85, 0, false, false), None, 9).contains(",\"85\","));
    }

    #[test]
    fn file_name_is_safe() {
        let t = ms(2026, 11, 20, 12, 0, 0);
        assert_eq!(file_name("SGCo", t), "20261120_SGCo_call_events.log");
        assert_eq!(file_name("a/b c", t), "20261120_a_b_c_call_events.log");
    }

    #[test]
    fn repeated_grants_make_one_row() {
        let d = dir("repeated");
        let mut l = log(&d, 1);
        let t = ms(2026, 11, 20, 10, 0, 0);
        for i in 0..3 { l.grant(&g(27, 911, HZ, false, false), t + i * 1000); }
        l.tick(t + 2000 + IDLE_MS);
        assert!(!d.join("20261120_SGCo_call_events.log").exists(), "not ended before IDLE_MS has passed");
        l.tick(t + 2000 + IDLE_MS + 1);
        let f = lines(&d, "20261120");
        assert_eq!(f.len(), 2);
        assert_eq!(f[0], HEADER);
        assert!(f[1].starts_with("\"2026:11:20:10:00:00\",\"2000\","), "{}", f[1]);
    }

    #[test]
    fn other_talkgroup_on_frequency_ends_call() {
        let d = dir("other-tg");
        let mut l = log(&d, 1);
        let t = ms(2026, 11, 20, 10, 0, 0);
        l.grant(&g(27, 911, HZ, false, false), t);
        l.grant(&g(27, 911, HZ, false, false), t + 300);
        l.grant(&g(41, 911, HZ, false, false), t + 500);
        let f = lines(&d, "20261120");
        assert_eq!(f.len(), 2);
        assert!(f[1].contains("\"300\"") && f[1].contains("(27)"), "{}", f[1]);
        l.close_all(t + 600);
        let f = lines(&d, "20261120");
        assert_eq!(f.len(), 3);
        assert!(f[2].contains(",\"41\","), "{}", f[2]);
    }

    #[test]
    fn live_transmission_keeps_call_open_and_adds_facts() {
        let d = dir("live");
        let mut l = log(&d, 1);
        let t = ms(2026, 11, 20, 10, 0, 0);
        l.grant(&g(27, 0, HZ, false, false), t);
        l.live(&[Live { hz: HZ, group: 27, source: 711035, encrypted: true }], t + 4000);
        l.tick(t + 4000);
        assert!(!d.join("20261120_SGCo_call_events.log").exists(), "still open");
        l.tick(t + 4000 + IDLE_MS + 1);
        let f = lines(&d, "20261120");
        assert!(f[1].contains(",\"711035\",") && f[1].contains(",\"4000\",") && f[1].contains(" ENCRYPTED"), "{}", f[1]);
    }

    #[test]
    fn midnight_call_goes_to_start_day() {
        let d = dir("midnight");
        let mut l = log(&d, 1);
        let t = ms(2026, 11, 20, 23, 59, 59);
        l.grant(&g(27, 911, HZ, false, false), t);
        l.grant(&g(27, 911, HZ, false, false), t + 3000);
        l.close_all(t + 3000);
        assert_eq!(lines(&d, "20261120").len(), 2);
        assert!(!d.join("20261121_SGCo_call_events.log").exists());
    }

    #[test]
    fn reopen_appends_without_header() {
        let d = dir("reopen");
        let t = ms(2026, 11, 20, 10, 0, 0);
        let mut a = log(&d, 100);
        a.grant(&g(27, 911, HZ, false, false), t);
        a.close_all(t);
        drop(a);
        let mut b = log(&d, 200);
        b.grant(&g(27, 911, HZ, false, false), t + 60_000);
        b.close_all(t + 60_000);
        let f = lines(&d, "20261120");
        assert_eq!(f.iter().filter(|x| x.as_str() == HEADER).count(), 1);
        assert!(f[1].ends_with(",\"100\"") && f[2].ends_with(",\"200\""), "{f:?}");
    }

    #[test]
    fn unwritable_folder_sets_error_and_recovers() {
        let d = dir("unwritable");
        std::fs::write(&d, b"a file where the folder should be").unwrap();
        let mut l = log(&d, 1);
        let t = ms(2026, 11, 20, 10, 0, 0);
        l.grant(&g(27, 911, HZ, false, false), t);
        l.close_all(t);
        assert!(l.last_error.as_deref().is_some_and(|e| e.starts_with("event log: ")), "{:?}", l.last_error);
        std::fs::remove_file(&d).unwrap();
        l.grant(&g(27, 911, HZ, false, false), t + 10_000);
        l.close_all(t + 10_000);
        assert_eq!(l.last_error, None);
        assert_eq!(lines(&d, "20261120").len(), 2);
    }

    #[test]
    fn drop_writes_open_calls() {
        let d = dir("drop");
        let mut l = log(&d, 1);
        l.grant(&g(27, 911, HZ, false, false), ms(2026, 11, 20, 10, 0, 0));
        drop(l);
        assert_eq!(lines(&d, "20261120").len(), 2, "a stopping system writes its open calls");
    }

    #[test]
    fn off_does_nothing() {
        let d = dir("off");
        let mut l = EventLog::new(None, "SGCo", policy(), 1);
        let t = ms(2026, 11, 20, 10, 0, 0);
        l.grant(&g(27, 911, HZ, false, false), t);
        l.close_all(t);
        assert!(!d.exists());
        assert_eq!(l.last_error, None);
    }
}
