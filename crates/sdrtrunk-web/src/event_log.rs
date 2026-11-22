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
use std::path::{Path, PathBuf};
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
    keep_days: u32,
    /// Date of the last file written; a new date prunes old files.
    last_day: Option<String>,
    pub last_error: Option<String>,
}

impl EventLog {
    /// `dir` None: logging is off and every method does nothing. `first_id` is the first EVENT_ID (the caller passes the
    /// current time in ms, so IDs keep growing across restarts). Daily files older than `keep_days` are deleted now and
    /// whenever a new day's file is written.
    pub fn new(dir: Option<PathBuf>, system: &str, policy: Arc<Mutex<RecordPolicy>>, first_id: u64, keep_days: u32) -> EventLog {
        let mut l = EventLog { dir, system: system.into(), policy, next_id: first_id, open: HashMap::new(), keep_days, last_day: None,
            last_error: None };
        l.prune_now();
        l
    }

    fn prune_now(&mut self) {
        let Some(dir) = self.dir.as_deref() else { return };
        if let Err(e) = prune(dir, Local::now().date_naive(), self.keep_days) {
            if e.kind() != std::io::ErrorKind::NotFound { self.fail(format!("event log: cannot delete old files in {}: {e}", dir.display())); }
        }
    }

    fn fail(&mut self, msg: String) {
        eprintln!("{msg}");
        self.last_error = Some(msg);
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
        let name_on_disk = file_name(&self.system, c.start_ms);
        let path = dir.join(&name_on_disk);
        let result = std::fs::create_dir_all(&dir).and_then(|_| {
            let mut f = std::fs::OpenOptions::new().append(true).create(true).open(&path)?;
            let mut out = String::new();
            if f.metadata()?.len() == 0 { out.push_str(HEADER); out.push('\n'); }
            out.push_str(&line);
            f.write_all(out.as_bytes())?;
            f.flush()
        });
        match result {
            Ok(()) => {
                self.last_error = None;
                let day = name_on_disk[..8].to_string();
                if self.last_day.as_ref() != Some(&day) {
                    self.last_day = Some(day);
                    self.prune_now();
                }
            }
            Err(e) => self.fail(format!("event log: {}: {e}", path.display())),
        }
    }
}

/// A stopping system (stop, failure, panic) writes the calls still open.
impl Drop for EventLog {
    fn drop(&mut self) { self.close_all(0); }
}

/// Deletes `<yyyyMMdd>_<system>_call_events.log` files (any system) dated before `today - keep_days`; returns how many.
/// Other files, and names whose date is not a real date, are left alone.
pub fn prune(dir: &Path, today: chrono::NaiveDate, keep_days: u32) -> std::io::Result<usize> {
    let cutoff = today - chrono::Days::new(keep_days as u64);
    let mut deleted = 0;
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let Ok(name) = entry.file_name().into_string() else { continue };
        let Some(date) = log_date(&name) else { continue };
        if date < cutoff && entry.file_type()?.is_file() {
            std::fs::remove_file(entry.path())?;
            deleted += 1;
        }
    }
    Ok(deleted)
}

/// The date of a daily event log file name, None for any other name.
pub fn log_date(name: &str) -> Option<chrono::NaiveDate> {
    let (date, rest) = (name.get(..8)?, name.get(8..)?);
    let system = rest.strip_prefix('_')?.strip_suffix("_call_events.log")?;
    if !date.bytes().all(|b| b.is_ascii_digit()) || system.is_empty() { return None; }
    chrono::NaiveDate::parse_from_str(date, "%Y%m%d").ok()
}

/// One logged call, read back from a file.
#[derive(Clone, Debug, PartialEq)]
pub struct Entry {
    pub system: String,
    pub start: chrono::NaiveDateTime,
    pub duration_ms: u64,
    pub event: String,
    pub from: Option<u32>,
    pub group: u16,
    pub name: Option<String>,
    pub channel: String,
    pub hz: u64,
    pub encrypted: bool,
    pub emergency: bool,
    pub id: u64,
}

/// What the Event Log page asks for. Empty `groups` / `radios`: any. `from` and `to` (inclusive) bound the call start.
#[derive(Clone, Debug, Default)]
pub struct Filter {
    pub system: Option<String>,
    pub event: Option<String>,
    pub encrypted: Option<bool>,
    pub emergency_only: bool,
    pub groups: Vec<u16>,
    pub radios: Vec<u32>,
    pub from: chrono::NaiveDateTime,
    pub to: chrono::NaiveDateTime,
}

/// One page of matches (newest first) and totals over every match.
#[derive(Clone, Debug, Default)]
pub struct Found {
    pub rows: Vec<Entry>,
    pub total: usize,
    pub encrypted: usize,
    pub emergency: usize,
    /// Distinct source radios.
    pub radios: usize,
    pub air_ms: u64,
    /// EVENT values of the calls in the time range, for the Call type choice.
    pub events: Vec<String>,
    /// Lines that are not call rows.
    pub skipped: usize,
}

/// Reads the files of the days `f.from`..=`f.to` and returns page `page` (0-based) of the matching calls.
/// A missing folder is an empty result.
// ponytail: scans the CSV files of the date range per request (~120 MB at 30 days); index them in SQLite if 30-day
// searches get slow.
pub fn search(dir: &Path, f: &Filter, page: usize, per_page: usize) -> std::io::Result<Found> {
    let mut found = Found::default();
    let read = match std::fs::read_dir(dir) {
        Ok(r) => r,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(found),
        Err(e) => return Err(e),
    };
    let want_system = f.system.as_deref().map(safe_name);
    let (mut matches, mut events, mut radios) = (Vec::new(), std::collections::BTreeSet::new(), std::collections::HashSet::new());
    for entry in read {
        let entry = entry?;
        let Ok(file) = entry.file_name().into_string() else { continue };
        let Some(date) = log_date(&file) else { continue };
        if date < f.from.date() || date > f.to.date() { continue; }
        let system = file[9..file.len() - "_call_events.log".len()].to_string();
        if want_system.as_ref().is_some_and(|w| *w != system) { continue; }
        for line in std::fs::read_to_string(entry.path())?.lines() {
            if line.is_empty() || line == HEADER { continue; }
            let Some(e) = parse_row(&system, line) else { found.skipped += 1; continue };
            if e.start < f.from || e.start > f.to { continue; }
            events.insert(e.event.clone());
            let keep = f.event.as_ref().is_none_or(|v| *v == e.event)
                && f.encrypted.is_none_or(|v| v == e.encrypted)
                && (!f.emergency_only || e.emergency)
                && (f.groups.is_empty() || f.groups.contains(&e.group))
                && (f.radios.is_empty() || e.from.is_some_and(|r| f.radios.contains(&r)));
            if !keep { continue; }
            found.encrypted += e.encrypted as usize;
            found.emergency += e.emergency as usize;
            found.air_ms += e.duration_ms;
            if let Some(r) = e.from { radios.insert(r); }
            matches.push(e);
        }
    }
    matches.sort_by(|a, b| b.start.cmp(&a.start).then(b.id.cmp(&a.id)));
    found.total = matches.len();
    found.radios = radios.len();
    found.events = events.into_iter().collect();
    found.rows = matches.into_iter().skip(page * per_page).take(per_page).collect();
    Ok(found)
}

/// A call row back into an `Entry`; None for anything that is not one.
fn parse_row(system: &str, line: &str) -> Option<Entry> {
    let v = crate::rr_csv::fields(line);
    if v.len() != 11 { return None; }
    let (group, name) = match v[5].rsplit_once(" (") {
        Some((n, g)) if n.starts_with('[') && n.ends_with(']') => (g.strip_suffix(')')?.parse().ok()?, Some(n[1..n.len() - 1].to_string())),
        _ => (v[5].parse().ok()?, None),
    };
    let mhz: f64 = v[7].parse().ok()?;
    Some(Entry {
        system: system.into(),
        start: chrono::NaiveDateTime::parse_from_str(&v[0], "%Y:%m:%d:%H:%M:%S").ok()?,
        duration_ms: v[1].parse().ok()?,
        event: v[3].clone(),
        from: if v[4].is_empty() { None } else { Some(v[4].parse().ok()?) },
        group,
        name,
        channel: v[6].clone(),
        hz: (mhz * 1e6).round() as u64,
        encrypted: v[9].contains(" ENCRYPTED"),
        emergency: v[9].contains(" EMERGENCY"),
        id: v[10].parse().ok()?,
    })
}

/// Numbers separated by commas or spaces (radio IDs, talkgroups).
pub fn parse_ids<T: std::str::FromStr>(s: &str) -> Result<Vec<T>, String> {
    s.split(|c: char| c == ',' || c.is_whitespace()).filter(|p| !p.is_empty())
        .map(|p| p.parse().map_err(|_| "Use numbers separated by commas or spaces.".to_string())).collect()
}

/// A system name as it appears in file names.
pub fn safe_name(system: &str) -> String {
    system.chars().map(|c| if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') { c } else { '_' }).collect()
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
    format!("{}_{}_call_events.log", local(start_ms).format("%Y%m%d"), safe_name(system))
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
    fn log(d: &Path, first_id: u64) -> EventLog { EventLog::new(Some(d.to_path_buf()), "SGCo", policy(), first_id, 30) }
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

    fn touch(d: &Path, names: &[&str]) {
        std::fs::create_dir_all(d).unwrap();
        for n in names { std::fs::write(d.join(n), b"x").unwrap(); }
    }
    fn present(d: &Path) -> Vec<String> {
        let mut v: Vec<String> = std::fs::read_dir(d).unwrap().map(|e| e.unwrap().file_name().into_string().unwrap()).collect();
        v.sort();
        v
    }
    fn day(y: i32, m: u32, d: u32) -> chrono::NaiveDate { chrono::NaiveDate::from_ymd_opt(y, m, d).unwrap() }

    #[test]
    fn prune_keeps_window() {
        let d = dir("prune-window");
        touch(&d, &["20261120_SGCo_call_events.log", "20261121_SGCo_call_events.log", "20261121_KSICS_call_events.log",
            "notes.txt", "20261340_SGCo_call_events.log"]);
        assert_eq!(prune(&d, day(2026, 12, 21), 30).unwrap(), 1);
        assert_eq!(present(&d), vec!["20261121_KSICS_call_events.log", "20261121_SGCo_call_events.log", "20261340_SGCo_call_events.log", "notes.txt"]);
    }

    #[test]
    fn prune_keep_one_day() {
        let d = dir("prune-one");
        touch(&d, &["20261119_SGCo_call_events.log", "20261120_SGCo_call_events.log", "20261121_SGCo_call_events.log"]);
        assert_eq!(prune(&d, day(2026, 11, 21), 1).unwrap(), 1);
        assert_eq!(present(&d), vec!["20261120_SGCo_call_events.log", "20261121_SGCo_call_events.log"]);
    }

    #[test]
    fn new_day_prunes() {
        let d = dir("prune-new");
        touch(&d, &["20200101_X_call_events.log"]);
        let _l = EventLog::new(Some(d.clone()), "SGCo", policy(), 1, 1);
        assert!(present(&d).is_empty(), "an old day's file is deleted when the system starts");
        let mut l = EventLog::new(Some(d.clone()), "SGCo", policy(), 1, 1);
        touch(&d, &["20200102_X_call_events.log"]);
        let t = Local::now().timestamp_millis();
        l.grant(&g(27, 911, HZ, false, false), t);
        l.close_all(t);
        assert!(!d.join("20200102_X_call_events.log").exists(), "and when a new day's file is opened");
    }

    fn write_call(l: &mut EventLog, start: i64, dur: i64, group: u16, source: u32, enc: bool, emer: bool) {
        l.grant(&g(group, source, HZ, enc, emer), start);
        if dur > 0 { l.grant(&g(group, source, HZ, enc, emer), start + dur); }
        l.close_all(start + dur);
    }
    fn dt(y: i32, mo: u32, d: u32, h: u32, mi: u32) -> chrono::NaiveDateTime { day(y, mo, d).and_hms_opt(h, mi, 0).unwrap() }
    fn sample(name: &str) -> PathBuf {
        let d = dir(name);
        let mut sg = EventLog::new(Some(d.clone()), "SGCo", policy(), 1, 3650);
        let mut ks = EventLog::new(Some(d.clone()), "KSICS", policy(), 100, 3650);
        write_call(&mut sg, ms(2026, 11, 19, 23, 59, 0), 1000, 27, 911, false, false);
        write_call(&mut sg, ms(2026, 11, 20, 10, 0, 0), 2000, 27, 911, false, false);
        write_call(&mut ks, ms(2026, 11, 20, 10, 2, 0), 3000, 495, 44599, false, false);
        write_call(&mut sg, ms(2026, 11, 20, 10, 5, 0), 1000, 27, 711035, true, false);
        write_call(&mut sg, ms(2026, 11, 20, 10, 10, 0), 500, 41, 911, false, true);
        write_call(&mut ks, ms(2026, 11, 20, 10, 20, 0), 0, 27, 911, false, false);
        d
    }
    fn day_filter() -> Filter { Filter { from: dt(2026, 11, 20, 0, 0), to: dt(2026, 11, 20, 23, 59), ..Filter::default() } }

    #[test]
    fn search_filters_each_field() {
        let d = sample("search");
        let n = |f: Filter| search(&d, &f, 0, 200).unwrap().total;
        let all = search(&d, &day_filter(), 0, 200).unwrap();
        assert_eq!((all.total, all.encrypted, all.emergency, all.radios, all.air_ms), (5, 1, 1, 3, 6500));
        assert_eq!(all.events, vec!["Group Call".to_string()]);
        assert_eq!(all.rows[0].group, 27, "newest first (KSICS 10:20)");
        assert_eq!((all.rows[0].system.as_str(), all.rows[0].from), ("KSICS", Some(911)));
        assert_eq!(all.rows.iter().find(|e| e.group == 27 && e.system == "SGCo").unwrap().name.as_deref(), Some("Sedgwick Co EMS"));
        assert_eq!(n(Filter { system: Some("SGCo".into()), ..day_filter() }), 3);
        assert_eq!(n(Filter { event: Some("Group Call".into()), ..day_filter() }), 5);
        assert_eq!(n(Filter { event: Some("Unit To Unit Call".into()), ..day_filter() }), 0);
        assert_eq!(n(Filter { encrypted: Some(true), ..day_filter() }), 1);
        assert_eq!(n(Filter { encrypted: Some(false), ..day_filter() }), 4);
        assert_eq!(n(Filter { emergency_only: true, ..day_filter() }), 1);
        assert_eq!(n(Filter { groups: vec![27], ..day_filter() }), 3);
        assert_eq!(n(Filter { radios: vec![911, 711035], ..day_filter() }), 4);
        assert_eq!(n(Filter { from: dt(2026, 11, 19, 0, 0), ..day_filter() }), 6);
        assert_eq!(n(Filter { from: dt(2026, 11, 20, 10, 3), to: dt(2026, 11, 20, 10, 10), ..day_filter() }), 2);
    }

    #[test]
    fn search_pages_newest_first() {
        let d = dir("pages");
        let mut l = EventLog::new(Some(d.clone()), "SGCo", policy(), 1, 3650);
        let t = ms(2026, 11, 20, 8, 0, 0);
        for i in 0..450 { write_call(&mut l, t + i * 1000, 0, 27, 911, false, false); }
        let p0 = search(&d, &day_filter(), 0, 200).unwrap();
        assert_eq!((p0.total, p0.rows.len()), (450, 200));
        assert_eq!(p0.rows[0].start, dt(2026, 11, 20, 8, 7) + chrono::Duration::seconds(29));
        assert_eq!(search(&d, &day_filter(), 2, 200).unwrap().rows.len(), 50);
    }

    #[test]
    fn search_skips_bad_lines() {
        let d = dir("bad-lines");
        let mut l = EventLog::new(Some(d.clone()), "SGCo", policy(), 1, 3650);
        write_call(&mut l, ms(2026, 11, 20, 9, 0, 0), 0, 27, 911, false, false);
        let f = d.join("20261120_SGCo_call_events.log");
        let mut text = std::fs::read_to_string(&f).unwrap();
        text.push_str("garbage\n");
        std::fs::write(&f, text).unwrap();
        let r = search(&d, &day_filter(), 0, 200).unwrap();
        assert_eq!((r.total, r.skipped), (1, 1));
    }

    #[test]
    fn search_missing_folder_is_empty() {
        let r = search(&dir("missing"), &day_filter(), 0, 200).unwrap();
        assert_eq!((r.total, r.rows.len()), (0, 0));
    }

    #[test]
    fn parse_ids_accepts_and_rejects() {
        assert_eq!(parse_ids::<u32>("911, 711035 4054").unwrap(), vec![911, 711035, 4054]);
        assert!(parse_ids::<u32>("  ").unwrap().is_empty());
        assert_eq!(parse_ids::<u32>("91a").unwrap_err(), "Use numbers separated by commas or spaces.");
        assert!(parse_ids::<u16>("70000").is_err(), "talkgroups are 16-bit");
    }

    #[test]
    fn off_does_nothing() {
        let d = dir("off");
        let mut l = EventLog::new(None, "SGCo", policy(), 1, 30);
        let t = ms(2026, 11, 20, 10, 0, 0);
        l.grant(&g(27, 911, HZ, false, false), t);
        l.close_all(t);
        assert!(!d.exists());
        assert_eq!(l.last_error, None);
    }
}
