//! SQLite storage: systems, global settings, talkgroups per system, audit log (accounts and sessions live in `auth.rs`).
use crate::streaming::StreamConfig;
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::path::Path;
use std::sync::{Mutex, MutexGuard};

/// Airspy tuning range.
pub const MIN_HZ: u64 = 24_000_000;
pub const MAX_HZ: u64 = 1_800_000_000;

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS settings (id INTEGER PRIMARY KEY CHECK (id = 1), json TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS systems (id INTEGER PRIMARY KEY, json TEXT NOT NULL);
-- Per-SDR settings, keyed by tuner id (Airspy serial or rtl:SERIAL); kept while the device is unplugged.
CREATE TABLE IF NOT EXISTS tuners (id TEXT PRIMARY KEY, json TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS audit (id INTEGER PRIMARY KEY, at INTEGER NOT NULL, username TEXT NOT NULL, action TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS users (id INTEGER PRIMARY KEY, username TEXT UNIQUE NOT NULL, role TEXT NOT NULL,
    pw_hash TEXT NOT NULL, external_id TEXT, created_at INTEGER NOT NULL);
CREATE TABLE IF NOT EXISTS sessions (token_hash TEXT PRIMARY KEY, user_id INTEGER NOT NULL, csrf TEXT NOT NULL,
    created_at INTEGER NOT NULL, last_seen INTEGER NOT NULL);
-- Control channel signal and decode quality, one row per system per minute (`quality::Summary` as JSON).
CREATE TABLE IF NOT EXISTS quality (system INTEGER NOT NULL, at_ms INTEGER NOT NULL, json TEXT NOT NULL, PRIMARY KEY (system, at_ms));
-- Icecast streams (`streaming::StreamConfig` as JSON, password included: the database file is readable only by the
-- service user).
CREATE TABLE IF NOT EXISTS streams (id INTEGER PRIMARY KEY, json TEXT NOT NULL);
-- Voice frequencies each system's grants have used: where its tuner window is placed.
CREATE TABLE IF NOT EXISTS voice_freqs (system INTEGER NOT NULL, hz INTEGER NOT NULL, PRIMARY KEY (system, hz));
";

/// Talkgroup lists are named alias lists (sdrtrunk's term); each system uses one, and several systems may share one.
/// The `system` columns hold the alias list name (the first lists were named after their system).
const TALKGROUP_TABLES: &str = "
CREATE TABLE IF NOT EXISTS alias_lists (name TEXT PRIMARY KEY);
CREATE TABLE IF NOT EXISTS talkgroups (system TEXT NOT NULL, number INTEGER NOT NULL, name TEXT NOT NULL,
    record INTEGER NOT NULL, PRIMARY KEY (system, number));
-- Talkgroups added by an import and not reviewed yet (pinned at the top of the list).
CREATE TABLE IF NOT EXISTS new_talkgroups (system TEXT NOT NULL, number INTEGER NOT NULL, PRIMARY KEY (system, number));
-- Record talkgroups not on the list; a system without a row records them.
CREATE TABLE IF NOT EXISTS unlisted (system TEXT PRIMARY KEY, record INTEGER NOT NULL);
";

/// Settings shared by all systems.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings { pub recording_dir: String }

impl Default for Settings {
    fn default() -> Self { Settings { recording_dir: "/var/lib/sdrtrunk-rust/recordings".into() } }
}

impl Settings {
    pub fn validate(&self) -> Result<(), Vec<FieldError>> {
        if self.recording_dir.starts_with('/') { Ok(()) } else { Err(vec![err("recording_dir", "Use a full path starting with /.")]) }
    }
}

/// One trunked system (one site): its own tuner, control channels and recording names.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct System {
    /// Database row id (not part of the stored JSON); 0 before the first save.
    #[serde(skip)]
    pub id: i64,
    pub enabled: bool,
    /// Airspy serial ("735863DC-2F67AFE3"), "rtl:" + RTL-SDR serial, or empty for Automatic.
    pub tuner: String,
    /// Primary control channel first, then backups.
    pub control_hz: Vec<u64>,
    pub failover_ms: u32,
    /// "lsm" (simulcast, sdrtrunk "CQPSK") or "c4fm".
    pub modulation: String,
    /// sdrtrunk alias-list System, Site and channel Name: written into MP3 file names and tags.
    pub system_name: String,
    pub site_name: String,
    pub channel_name: String,
    /// Talkgroup (alias) list this system uses; empty = the list named after the system.
    pub alias_list: String,
}

impl Default for System {
    fn default() -> Self {
        System {
            id: 0,
            enabled: true,
            tuner: String::new(),
            control_hz: vec![858_962_500],
            failover_ms: 1000,
            modulation: "lsm".into(),
            system_name: "P25".into(),
            site_name: "Site".into(),
            channel_name: "Control".into(),
            alias_list: String::new(),
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct FieldError { pub field: &'static str, pub message: String }

fn err(field: &'static str, message: &str) -> FieldError { FieldError { field, message: message.into() } }

impl System {
    pub fn validate(&self) -> Result<(), Vec<FieldError>> {
        let mut e = Vec::new();
        if self.control_hz.is_empty() {
            e.push(err("control_hz", "Enter at least one control channel frequency."));
        } else if self.control_hz.iter().any(|&f| !(MIN_HZ..=MAX_HZ).contains(&f)) {
            e.push(err("control_hz", "Frequencies must be between 24 and 1800 MHz."));
        }
        if !(200..=60_000).contains(&self.failover_ms) { e.push(err("failover_ms", "Use 200 to 60000 ms.")); }
        if !matches!(self.modulation.as_str(), "lsm" | "c4fm") { e.push(err("modulation", "Choose C4FM or LSM.")); }
        if !self.tuner.is_empty() && sdrtrunk_source::normalize_tuner(&self.tuner).is_none() {
            e.push(err("tuner", "Choose a connected SDR (Airspy serial such as 735863DC-2F67AFE3, or rtl:SERIAL), or Automatic."));
        }
        let name_ok = |s: &str| !s.trim().is_empty() && s.len() <= 60;
        if ![&self.system_name, &self.site_name, &self.channel_name].iter().all(|s| name_ok(s)) {
            e.push(err("names", "System, site and channel names must be 1 to 60 characters."));
        }
        if self.alias_list.len() > 60 { e.push(err("alias_list", "Alias list names are 1 to 60 characters.")); }
        if e.is_empty() { Ok(()) } else { Err(e) }
    }

    /// The alias list whose talkgroups this system names and records.
    pub fn list_name(&self) -> &str { if self.alias_list.trim().is_empty() { &self.system_name } else { &self.alias_list } }

    /// "SGCo / T-SGCo-Simulcast" for lists and status.
    pub fn label(&self) -> String { format!("{} / {}", self.system_name, self.channel_name) }
}

/// Live: systems may use the tuner. Setup: it runs at its fixed centre for adjusting (spectrum), no system uses it.
/// Disabled: never opened, not even by Automatic.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TunerMode { #[default] Live, Setup, Disabled }

impl TunerMode {
    pub fn as_str(self) -> &'static str { match self { TunerMode::Live => "live", TunerMode::Setup => "setup", TunerMode::Disabled => "disabled" } }
}

/// One SDR's settings (sdrtrunk keeps these per tuner, not per system: ppm is the device's clock error).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct TunerConfig {
    pub mode: TunerMode,
    /// Lock center: the window stays here. None: placed automatically to cover the control channel and the voice
    /// frequencies seen. A tuner in Setup mode sits here.
    pub center_hz: Option<u64>,
    /// Frequency limits: centres and channels stay inside.
    pub min_hz: Option<u64>,
    pub max_hz: Option<u64>,
    pub ppm: f64,
    /// Automatic PPM: follow the measured frequency error, within 3 PPM of the value it starts from (saved as it moves).
    pub auto_ppm: bool,
    /// Airspy combined linearity gain, 0 to 21.
    pub gain: u8,
    /// Airspy sample rate: 3 or 6 MS/s (an RTL-SDR always runs at 2.4 MS/s).
    pub rate_hz: u32,
    /// RTL-SDR tuner gain in dB; None = automatic.
    pub rtl_gain_db: Option<f64>,
}

impl Default for TunerConfig {
    fn default() -> Self {
        TunerConfig { mode: TunerMode::Live, center_hz: None, min_hz: None, max_hz: None, ppm: 0.0, auto_ppm: true, gain: 14,
            rate_hz: 6_000_000, rtl_gain_db: None }
    }
}

impl TunerConfig {
    pub fn validate(&self) -> Result<(), Vec<FieldError>> {
        let mut e = Vec::new();
        if !(-100.0..=100.0).contains(&self.ppm) { e.push(err("ppm", "Correction must be between -100 and 100 ppm.")); }
        if self.gain > 21 { e.push(err("gain", "Gain must be 0 to 21.")); }
        if self.rate_hz != 3_000_000 && self.rate_hz != 6_000_000 { e.push(err("rate_hz", "Sample rate must be 3 or 6 MS/s.")); }
        if self.rtl_gain_db.is_some_and(|g| !(0.0..=60.0).contains(&g)) { e.push(err("rtl_gain_db", "RTL-SDR gain must be 0 to 60 dB, or blank for automatic.")); }
        let in_range = |f: Option<u64>| f.is_none_or(|f| (MIN_HZ..=MAX_HZ).contains(&f));
        if !in_range(self.center_hz) { e.push(err("center_hz", "The center must be between 24 and 1800 MHz.")); }
        if !in_range(self.min_hz) || !in_range(self.max_hz) { e.push(err("limits", "Limits must be between 24 and 1800 MHz.")); }
        else if let (Some(lo), Some(hi)) = (self.min_hz, self.max_hz) {
            if lo >= hi { e.push(err("limits", "The minimum must be below the maximum.")); }
        }
        if let Some(c) = self.center_hz.filter(|&c| self.min_hz.is_some_and(|lo| c < lo) || self.max_hz.is_some_and(|hi| c > hi)) {
            e.push(err("center_hz", &format!("{} MHz is outside the frequency limits.", c as f64 / 1e6)));
        }
        if self.mode == TunerMode::Setup && self.center_hz.is_none() { e.push(err("center_hz", "Setup mode needs a center frequency to sit at.")); }
        if e.is_empty() { Ok(()) } else { Err(e) }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Talkgroup { pub number: u16, pub name: String, pub record: bool }

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AuditEntry { pub at: i64, pub username: String, pub action: String }

pub struct Db(Mutex<Connection>);

impl Db {
    /// Opens (or creates) the database file, readable and writable by its owner only: it holds password hashes,
    /// sessions and stream passwords.
    pub fn open(path: &Path) -> rusqlite::Result<Db> {
        let db = Self::init(Connection::open(path)?)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            if let Err(e) = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)) {
                eprintln!("cannot make {} private: {e}", path.display());
            }
        }
        Ok(db)
    }
    pub fn open_in_memory() -> rusqlite::Result<Db> { Self::init(Connection::open_in_memory()?) }

    fn init(mut c: Connection) -> rusqlite::Result<Db> {
        c.execute_batch(SCHEMA)?;
        migrate_to_systems(&mut c)?;
        c.execute_batch(TALKGROUP_TABLES)?;
        // Lists that existed before the alias_lists table: every list with talkgroups or a switch, and every system's.
        c.execute_batch("INSERT OR IGNORE INTO alias_lists (name) SELECT DISTINCT system FROM talkgroups;
                         INSERT OR IGNORE INTO alias_lists (name) SELECT system FROM unlisted;")?;
        let db = Db(Mutex::new(c));
        for s in db.systems() { db.create_alias_list(s.list_name())?; }
        Ok(db)
    }

    /// The connection; a panic while holding it does not make the database unusable.
    pub(crate) fn conn(&self) -> MutexGuard<'_, Connection> { self.0.lock().unwrap_or_else(|p| p.into_inner()) }

    pub fn settings(&self) -> Settings {
        let json: Option<String> = self.conn()
            .query_row("SELECT json FROM settings WHERE id = 1", [], |r| r.get(0)).optional().ok().flatten();
        json.and_then(|j| serde_json::from_str(&j).ok()).unwrap_or_default()
    }

    pub fn save_settings(&self, s: &Settings) -> rusqlite::Result<()> {
        let json = serde_json::to_string(s).expect("settings serialize");
        self.conn().execute("INSERT INTO settings (id, json) VALUES (1, ?1) ON CONFLICT(id) DO UPDATE SET json = ?1", [json])?;
        Ok(())
    }

    // ---------- systems ----------

    pub fn systems(&self) -> Vec<System> {
        let c = self.conn();
        let mut st = c.prepare("SELECT id, json FROM systems ORDER BY id").expect("query");
        st.query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?))).expect("query").filter_map(Result::ok)
            .filter_map(|(id, j)| serde_json::from_str::<System>(&j).ok().map(|s| System { id, ..s })).collect()
    }

    pub fn system(&self, id: i64) -> Option<System> { self.systems().into_iter().find(|s| s.id == id) }

    /// Inserts (id 0) or replaces; returns the id. Creates the system's alias list if it is new.
    pub fn save_system(&self, s: &System) -> rusqlite::Result<i64> {
        self.create_alias_list(s.list_name())?;
        let json = serde_json::to_string(s).expect("system serialize");
        let c = self.conn();
        if s.id == 0 {
            c.execute("INSERT INTO systems (json) VALUES (?1)", [json])?;
            Ok(c.last_insert_rowid())
        } else {
            c.execute("UPDATE systems SET json = ?2 WHERE id = ?1", params![s.id, json])?;
            Ok(s.id)
        }
    }

    /// Talkgroups stay: another site may share the System name. Its quality history goes.
    pub fn delete_system(&self, id: i64) -> rusqlite::Result<()> {
        let c = self.conn();
        c.execute("DELETE FROM systems WHERE id = ?1", [id])?;
        c.execute("DELETE FROM quality WHERE system = ?1", [id])?;
        c.execute("DELETE FROM voice_freqs WHERE system = ?1", [id])?;
        Ok(())
    }

    // ---------- quality history ----------

    /// Stores a minute's summary and drops history older than `quality::KEEP_MS`.
    pub fn add_quality(&self, system: i64, at_ms: i64, s: &crate::quality::Summary) -> rusqlite::Result<()> {
        let json = serde_json::to_string(s).expect("summary serialize");
        let c = self.conn();
        c.execute("INSERT OR REPLACE INTO quality (system, at_ms, json) VALUES (?1, ?2, ?3)", params![system, at_ms, json])?;
        c.execute("DELETE FROM quality WHERE at_ms < ?1", [at_ms - crate::quality::KEEP_MS])?;
        Ok(())
    }

    /// A system's samples from `since_ms` on, oldest first.
    pub fn quality(&self, system: i64, since_ms: i64) -> Vec<(i64, crate::quality::Summary)> {
        let c = self.conn();
        let mut st = c.prepare("SELECT at_ms, json FROM quality WHERE system = ?1 AND at_ms >= ?2 ORDER BY at_ms").expect("query");
        st.query_map(params![system, since_ms], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?))).expect("query")
            .filter_map(Result::ok).filter_map(|(at, j)| serde_json::from_str(&j).ok().map(|s| (at, s))).collect()
    }

    // ---------- alias lists ----------

    /// Alias list names, sorted.
    pub fn alias_lists(&self) -> Vec<String> {
        let c = self.conn();
        let mut st = c.prepare("SELECT name FROM alias_lists ORDER BY name COLLATE NOCASE").expect("query");
        st.query_map([], |r| r.get(0)).expect("query").filter_map(Result::ok).collect()
    }

    /// Adds an empty alias list (nothing happens if it exists).
    pub fn create_alias_list(&self, name: &str) -> rusqlite::Result<()> {
        self.conn().execute("INSERT OR IGNORE INTO alias_lists (name) VALUES (?1)", [name])?;
        Ok(())
    }

    // ---------- tuners ----------

    /// Stored settings by tuner id (devices never configured are absent: they use `TunerConfig::default()`).
    pub fn tuners(&self) -> Vec<(String, TunerConfig)> {
        let c = self.conn();
        let mut st = c.prepare("SELECT id, json FROM tuners ORDER BY id").expect("query");
        st.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))).expect("query").filter_map(Result::ok)
            .filter_map(|(id, j)| tuner_from_json(&j).map(|t| (id, t))).collect()
    }

    pub fn tuner(&self, id: &str) -> TunerConfig {
        self.tuners().into_iter().find(|(t, _)| t == id).map(|(_, c)| c).unwrap_or_default()
    }

    pub fn save_tuner(&self, id: &str, t: &TunerConfig) -> rusqlite::Result<()> {
        let json = serde_json::to_string(t).expect("tuner serialize");
        self.conn().execute("INSERT INTO tuners (id, json) VALUES (?1, ?2) ON CONFLICT(id) DO UPDATE SET json = ?2", params![id, json])?;
        Ok(())
    }

    // ---------- streams ----------

    pub fn streams(&self) -> Vec<StreamConfig> {
        let c = self.conn();
        let mut st = c.prepare("SELECT id, json FROM streams ORDER BY id").expect("query");
        st.query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?))).expect("query").filter_map(Result::ok)
            .filter_map(|(id, j)| serde_json::from_str::<StreamConfig>(&j).ok().map(|s| StreamConfig { id, ..s.upgraded() })).collect()
    }

    pub fn stream(&self, id: i64) -> Option<StreamConfig> { self.streams().into_iter().find(|s| s.id == id) }

    /// Inserts (id 0) or replaces; returns the id.
    pub fn save_stream(&self, s: &StreamConfig) -> rusqlite::Result<i64> {
        let json = serde_json::to_string(s).expect("stream serialize");
        let c = self.conn();
        if s.id == 0 {
            c.execute("INSERT INTO streams (json) VALUES (?1)", [json])?;
            Ok(c.last_insert_rowid())
        } else {
            c.execute("UPDATE streams SET json = ?2 WHERE id = ?1", params![s.id, json])?;
            Ok(s.id)
        }
    }

    pub fn delete_stream(&self, id: i64) -> rusqlite::Result<()> {
        self.conn().execute("DELETE FROM streams WHERE id = ?1", [id])?;
        Ok(())
    }

    // ---------- voice frequencies ----------

    /// Voice frequencies a system's grants have used, lowest first.
    pub fn voice_freqs(&self, system: i64) -> Vec<u64> {
        let c = self.conn();
        let mut st = c.prepare("SELECT hz FROM voice_freqs WHERE system = ?1 ORDER BY hz").expect("query");
        st.query_map([system], |r| r.get(0)).expect("query").filter_map(Result::ok).collect()
    }

    pub fn add_voice_freqs(&self, system: i64, hz: &[u64]) -> rusqlite::Result<()> {
        let mut c = self.conn();
        let tx = c.transaction()?;
        for f in hz { tx.execute("INSERT OR IGNORE INTO voice_freqs (system, hz) VALUES (?1, ?2)", params![system, f])?; }
        tx.commit()
    }

    /// Before the Tuners page, ppm, gain and sample rate were saved on each system. Moves them onto the tuner the
    /// system is pinned to, or for Automatic onto the first connected Airspy not pinned elsewhere. Runs while no tuner
    /// has settings yet, so it happens once (as soon as a device is connected).
    pub fn migrate_tuner_settings(&self, present: &[String]) -> rusqlite::Result<()> {
        if !self.tuners().is_empty() { return Ok(()); }
        let rows: Vec<Value> = {
            let c = self.conn();
            let mut st = c.prepare("SELECT json FROM systems ORDER BY id")?;
            let v = st.query_map([], |r| r.get::<_, String>(0))?.filter_map(Result::ok).filter_map(|j| serde_json::from_str(&j).ok()).collect();
            v
        };
        let pinned: Vec<String> = rows.iter().filter_map(|v| v.get("tuner")?.as_str().and_then(sdrtrunk_source::normalize_tuner)).collect();
        let mut done: Vec<String> = Vec::new();
        for v in &rows {
            if ["ppm", "gain", "rate_hz"].iter().all(|k| v.get(k).is_none()) { continue; }
            let pin = v.get("tuner").and_then(Value::as_str).and_then(sdrtrunk_source::normalize_tuner);
            let Some(id) = pin.or_else(|| present.iter().find(|p| !p.starts_with("rtl:") && !pinned.contains(p) && !done.contains(p)).cloned()) else { continue };
            if done.contains(&id) { continue; }
            let d = TunerConfig::default();
            let t = TunerConfig {
                ppm: v.get("ppm").and_then(Value::as_f64).unwrap_or(d.ppm),
                gain: v.get("gain").and_then(Value::as_u64).map_or(d.gain, |g| g.min(21) as u8),
                rate_hz: v.get("rate_hz").and_then(Value::as_u64).map_or(d.rate_hz, |r| r as u32),
                ..d
            };
            self.save_tuner(&id, &t)?;
            done.push(id);
        }
        Ok(())
    }

    // ---------- talkgroups (per System name) ----------

    pub fn talkgroups(&self, system: &str) -> Vec<Talkgroup> {
        let c = self.conn();
        let mut st = c.prepare("SELECT number, name, record FROM talkgroups WHERE system = ?1 ORDER BY number").expect("query");
        st.query_map([system], |r| Ok(Talkgroup { number: r.get(0)?, name: r.get(1)?, record: r.get(2)? }))
            .expect("query").filter_map(Result::ok).collect()
    }

    /// A manual save: also counts as reviewing an imported talkgroup.
    pub fn upsert_talkgroup(&self, system: &str, t: &Talkgroup) -> rusqlite::Result<()> {
        let c = self.conn();
        c.execute(
            "INSERT INTO talkgroups (system, number, name, record) VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(system, number) DO UPDATE SET name = ?3, record = ?4",
            params![system, t.number, t.name, t.record])?;
        c.execute("DELETE FROM new_talkgroups WHERE system = ?1 AND number = ?2", params![system, t.number])?;
        Ok(())
    }

    pub fn delete_talkgroup(&self, system: &str, number: u16) -> rusqlite::Result<()> {
        let c = self.conn();
        c.execute("DELETE FROM talkgroups WHERE system = ?1 AND number = ?2", params![system, number])?;
        c.execute("DELETE FROM new_talkgroups WHERE system = ?1 AND number = ?2", params![system, number])?;
        Ok(())
    }

    /// Adds imported talkgroups (all or none) and marks them new until reviewed.
    pub fn import_talkgroups(&self, system: &str, ts: &[Talkgroup]) -> rusqlite::Result<()> {
        let mut c = self.conn();
        let tx = c.transaction()?;
        for t in ts {
            tx.execute("INSERT INTO talkgroups (system, number, name, record) VALUES (?1, ?2, ?3, ?4)", params![system, t.number, t.name, t.record])?;
            tx.execute("INSERT OR IGNORE INTO new_talkgroups (system, number) VALUES (?1, ?2)", params![system, t.number])?;
        }
        tx.commit()
    }

    /// Sets Record calls on several talkgroups at once (all or none); unknown numbers are ignored.
    pub fn set_record(&self, system: &str, numbers: &[u16], record: bool) -> rusqlite::Result<()> {
        let mut c = self.conn();
        let tx = c.transaction()?;
        for n in numbers { tx.execute("UPDATE talkgroups SET record = ?3 WHERE system = ?1 AND number = ?2", params![system, n, record])?; }
        tx.commit()
    }

    /// Changes only the name (the record flag stays).
    pub fn rename_talkgroup(&self, system: &str, number: u16, name: &str) -> rusqlite::Result<()> {
        self.conn().execute("UPDATE talkgroups SET name = ?3 WHERE system = ?1 AND number = ?2", params![system, number, name])?;
        Ok(())
    }

    /// Imported talkgroups not reviewed yet, by number.
    pub fn new_talkgroups(&self, system: &str) -> Vec<u16> {
        let c = self.conn();
        let mut st = c.prepare("SELECT number FROM new_talkgroups WHERE system = ?1 ORDER BY number").expect("query");
        st.query_map([system], |r| r.get(0)).expect("query").filter_map(Result::ok).collect()
    }

    /// Clears the NEW mark of one talkgroup, or of all with None.
    pub fn mark_reviewed(&self, system: &str, number: Option<u16>) -> rusqlite::Result<()> {
        match number {
            Some(n) => self.conn().execute("DELETE FROM new_talkgroups WHERE system = ?1 AND number = ?2", params![system, n])?,
            None => self.conn().execute("DELETE FROM new_talkgroups WHERE system = ?1", [system])?,
        };
        Ok(())
    }

    /// Record talkgroups not on this system's list (on unless switched off).
    pub fn record_unlisted(&self, system: &str) -> bool {
        self.conn().query_row("SELECT record FROM unlisted WHERE system = ?1", [system], |r| r.get(0)).optional().ok().flatten().unwrap_or(true)
    }

    pub fn set_record_unlisted(&self, system: &str, on: bool) -> rusqlite::Result<()> {
        self.conn().execute("INSERT INTO unlisted (system, record) VALUES (?1, ?2) ON CONFLICT(system) DO UPDATE SET record = ?2", params![system, on])?;
        Ok(())
    }

    pub fn audit(&self, at: i64, username: &str, action: &str) {
        // ponytail: an audit write failure is logged, not fatal; the change itself already happened.
        if let Err(e) = self.conn().execute("INSERT INTO audit (at, username, action) VALUES (?1, ?2, ?3)", params![at, username, action]) {
            eprintln!("audit write failed: {e}");
        }
    }

    pub fn audit_log(&self, limit: usize) -> Vec<AuditEntry> {
        let c = self.conn();
        let mut st = c.prepare("SELECT at, username, action FROM audit ORDER BY id DESC LIMIT ?1").expect("query");
        st.query_map([limit as i64], |r| Ok(AuditEntry { at: r.get(0)?, username: r.get(1)?, action: r.get(2)? }))
            .expect("query").filter_map(Result::ok).collect()
    }
}

/// A stored tuner; settings saved before tuner modes have `"enabled": false` for Disabled.
fn tuner_from_json(j: &str) -> Option<TunerConfig> {
    let v: Value = serde_json::from_str(j).ok()?;
    let disabled = v.get("mode").is_none() && v.get("enabled") == Some(&Value::Bool(false));
    let t: TunerConfig = serde_json::from_value(v).ok()?;
    Some(if disabled { TunerConfig { mode: TunerMode::Disabled, ..t } } else { t })
}

/// Databases from before multiple systems: the one settings row becomes system 1 and its talkgroups move under its
/// System name. Runs once (the old talkgroups table has no `system` column); all or nothing.
fn migrate_to_systems(c: &mut Connection) -> rusqlite::Result<()> {
    let old_table = c.prepare("SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'talkgroups'")?.exists([])?;
    let has_system = c.prepare("SELECT 1 FROM pragma_table_info('talkgroups') WHERE name = 'system'")?.exists([])?;
    if !old_table || has_system { return Ok(()); }
    let tx = c.transaction()?;
    let old: Option<String> = tx.query_row("SELECT json FROM settings WHERE id = 1", [], |r| r.get(0)).optional()?;
    let v: Value = old.as_deref().and_then(|j| serde_json::from_str(j).ok()).unwrap_or(Value::Null);
    let name = v.get("system_name").and_then(Value::as_str).unwrap_or("P25").to_string();
    let unlisted = v.get("record_unlisted").and_then(Value::as_bool).unwrap_or(true);
    tx.execute_batch("ALTER TABLE talkgroups RENAME TO talkgroups_v1;")?;
    let had_new = tx.prepare("SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'new_talkgroups'")?.exists([])?;
    if had_new { tx.execute_batch("ALTER TABLE new_talkgroups RENAME TO new_talkgroups_v1;")?; }
    tx.execute_batch(TALKGROUP_TABLES)?;
    tx.execute("INSERT INTO talkgroups (system, number, name, record) SELECT ?1, number, name, record FROM talkgroups_v1", [&name])?;
    if had_new {
        tx.execute("INSERT INTO new_talkgroups (system, number) SELECT ?1, number FROM new_talkgroups_v1", [&name])?;
        tx.execute_batch("DROP TABLE new_talkgroups_v1;")?;
    }
    tx.execute_batch("DROP TABLE talkgroups_v1;")?;
    tx.execute("INSERT OR REPLACE INTO unlisted (system, record) VALUES (?1, ?2)", params![name, unlisted])?;
    if let Some(j) = old {
        // The old row's system and name fields are a System's fields; its ppm, gain and rate stay in the JSON until
        // `migrate_tuner_settings` moves them to a tuner.
        tx.execute("INSERT INTO systems (json) VALUES (?1)", [j])?;
    }
    tx.commit()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fields(s: &System) -> Vec<&'static str> {
        s.validate().err().unwrap_or_default().iter().map(|e| e.field).collect()
    }

    #[test]
    fn systems_roundtrip() {
        let db = Db::open_in_memory().unwrap();
        assert!(db.systems().is_empty(), "a fresh database has no systems");
        assert_eq!(db.settings(), Settings::default());
        let id = db.save_system(&System { system_name: "SGCo".into(), ..System::default() }).unwrap();
        let k = db.save_system(&System { system_name: "KSICS".into(), modulation: "c4fm".into(), tuner: "10A862DC-34348363".into(), ..System::default() }).unwrap();
        db.save_system(&System { id, system_name: "SGCo".into(), failover_ms: 2000, ..System::default() }).unwrap();
        let all = db.systems();
        assert_eq!((all.len(), all[0].id, all[0].failover_ms, all[1].id), (2, id, 2000, k));
        assert_eq!(db.system(k).unwrap().tuner, "10A862DC-34348363");
        db.save_system(&System { system_name: "KSICS".into(), site_name: "Butler".into(), ..System::default() }).unwrap();
        assert_eq!(db.alias_lists(), vec!["KSICS", "SGCo"], "each system's list exists");
        db.save_system(&System { system_name: "KSICS".into(), site_name: "Lyon".into(), alias_list: "KSICS Statewide".into(), ..System::default() }).unwrap();
        assert_eq!(db.alias_lists(), vec!["KSICS", "KSICS Statewide", "SGCo"]);
        assert_eq!(db.systems()[3].list_name(), "KSICS Statewide");
        assert_eq!(db.systems()[0].list_name(), "SGCo", "empty = named after the system");
        db.delete_system(id).unwrap();
        assert_eq!(db.systems().len(), 3);
    }

    #[test]
    fn validate_rejects() {
        assert!(System::default().validate().is_ok());
        let d = System::default;
        assert_eq!(fields(&System { control_hz: vec![], ..d() }), vec!["control_hz"]);
        assert_eq!(fields(&System { control_hz: vec![5], ..d() }), vec!["control_hz"]);
        assert_eq!(fields(&System { failover_ms: 50, ..d() }), vec!["failover_ms"]);
        assert_eq!(fields(&System { modulation: "qam".into(), ..d() }), vec!["modulation"]);
        assert_eq!(fields(&System { modulation: "c4fm".into(), ..d() }), Vec::<&str>::new());
        assert_eq!(fields(&System { site_name: " ".into(), ..d() }), vec!["names"]);
        assert_eq!(fields(&System { system_name: "x".repeat(61), ..d() }), vec!["names"]);
        assert_eq!(fields(&System { tuner: "not a serial".into(), ..d() }), vec!["tuner"]);
        assert!(Settings { recording_dir: "rec".into() }.validate().is_err());
        let t = TunerConfig::default;
        let tf = |c: TunerConfig| c.validate().err().unwrap_or_default().iter().map(|e| e.field).collect::<Vec<_>>();
        assert!(t().validate().is_ok());
        assert_eq!(tf(TunerConfig { gain: 30, ..t() }), vec!["gain"]);
        assert_eq!(tf(TunerConfig { rate_hz: 10_000_000, ..t() }), vec!["rate_hz"]);
        assert_eq!(tf(TunerConfig { ppm: 500.0, ..t() }), vec!["ppm"]);
        assert_eq!(tf(TunerConfig { rtl_gain_db: Some(99.0), ..t() }), vec!["rtl_gain_db"]);
        assert_eq!(tf(TunerConfig { mode: TunerMode::Setup, ..t() }), vec!["center_hz"], "Setup mode sits at a fixed center");
        assert_eq!(tf(TunerConfig { min_hz: Some(860_000_000), max_hz: Some(850_000_000), ..t() }), vec!["limits"]);
        assert_eq!(tf(TunerConfig { center_hz: Some(870_000_000), max_hz: Some(860_000_000), ..t() }), vec!["center_hz"]);
        assert_eq!(tf(TunerConfig { center_hz: Some(5), ..t() }), vec!["center_hz"]);
        assert!(TunerConfig { mode: TunerMode::Setup, center_hz: Some(853_000_000), min_hz: Some(851_000_000), ..t() }.validate().is_ok());
    }

    #[test]
    fn talkgroups_are_per_system() {
        let db = Db::open_in_memory().unwrap();
        db.upsert_talkgroup("SGCo", &Talkgroup { number: 85, name: "Fire".into(), record: true }).unwrap();
        db.upsert_talkgroup("SGCo", &Talkgroup { number: 3, name: "Law".into(), record: false }).unwrap();
        db.upsert_talkgroup("SGCo", &Talkgroup { number: 85, name: "Fire Dispatch".into(), record: false }).unwrap();
        db.upsert_talkgroup("KSICS", &Talkgroup { number: 85, name: "KHP".into(), record: true }).unwrap();
        let t = db.talkgroups("SGCo");
        assert_eq!(t.iter().map(|t| t.number).collect::<Vec<_>>(), vec![3, 85]);
        assert_eq!(t[1], Talkgroup { number: 85, name: "Fire Dispatch".into(), record: false });
        assert_eq!(db.talkgroups("KSICS")[0].name, "KHP", "same number, other system");
        db.delete_talkgroup("SGCo", 85).unwrap();
        assert_eq!((db.talkgroups("SGCo").len(), db.talkgroups("KSICS").len()), (1, 1));
        assert!(db.record_unlisted("SGCo"), "on by default");
        db.set_record_unlisted("SGCo", false).unwrap();
        assert!(!db.record_unlisted("SGCo") && db.record_unlisted("KSICS"));
    }

    #[test]
    fn import_marks_new_until_reviewed() {
        let db = Db::open_in_memory().unwrap();
        let s = "SGCo";
        let t = |n: u16, name: &str, record: bool| Talkgroup { number: n, name: name.into(), record };
        db.upsert_talkgroup(s, &t(1, "Mine", false)).unwrap();
        db.import_talkgroups(s, &[t(5, "North", true), t(85, "Fire", false)]).unwrap();
        assert_eq!(db.new_talkgroups(s), vec![5, 85]);
        assert!(db.new_talkgroups("KSICS").is_empty());
        db.rename_talkgroup(s, 1, "Wichita PD West").unwrap();
        assert_eq!(db.talkgroups(s)[0], t(1, "Wichita PD West", false), "rename keeps the record flag");
        db.set_record(s, &[1, 5], true).unwrap();
        assert!(db.talkgroups(s).iter().take(2).all(|t| t.record));
        db.mark_reviewed(s, Some(5)).unwrap();
        assert_eq!(db.new_talkgroups(s), vec![85]);
        db.upsert_talkgroup(s, &t(85, "Fire Ops", false)).unwrap();
        assert!(db.new_talkgroups(s).is_empty(), "saving a talkgroup counts as reviewing it");
        db.import_talkgroups(s, &[t(7, "A", true), t(9, "B", true)]).unwrap();
        db.delete_talkgroup(s, 7).unwrap();
        assert_eq!(db.new_talkgroups(s), vec![9], "deleting clears it");
        db.mark_reviewed(s, None).unwrap();
        assert!(db.new_talkgroups(s).is_empty());
    }

    #[test]
    fn old_database_becomes_system_one() {
        let c = Connection::open_in_memory().unwrap();
        c.execute_batch("
            CREATE TABLE settings (id INTEGER PRIMARY KEY CHECK (id = 1), json TEXT NOT NULL);
            CREATE TABLE talkgroups (number INTEGER PRIMARY KEY, name TEXT NOT NULL, record INTEGER NOT NULL);
            CREATE TABLE new_talkgroups (number INTEGER PRIMARY KEY);
            INSERT INTO talkgroups VALUES (27, 'Jail 1', 1), (85, 'Fire', 0);
            INSERT INTO new_talkgroups VALUES (85);").unwrap();
        c.execute("INSERT INTO settings VALUES (1, ?1)", [r#"{"control_hz":[858462500,858962500],"failover_ms":1000,"modulation":"lsm",
            "gain":14,"ppm":-3.8,"rate_hz":6000000,"recording_dir":"/srv/recordings","system_name":"SGCo",
            "site_name":"SGCo-P25","channel_name":"T-SGCo-Simulcast","record_unlisted":false}"#]).unwrap();
        let db = Db::init(c).unwrap();
        let s = db.systems();
        assert_eq!(s.len(), 1);
        assert_eq!((s[0].system_name.as_str(), s[0].control_hz.len(), s[0].tuner.as_str(), s[0].enabled), ("SGCo", 2, "", true));
        db.migrate_tuner_settings(&[]).unwrap();
        assert!(db.tuners().is_empty(), "no device connected yet: nothing to move them to");
        db.migrate_tuner_settings(&["rtl:00000001".into(), "735863DC-2F67AFE3".into()]).unwrap();
        assert_eq!(db.tuners(), vec![("735863DC-2F67AFE3".to_string(), TunerConfig { ppm: -3.8, ..TunerConfig::default() })],
            "Automatic system's settings land on the Airspy");
        db.save_tuner("735863DC-2F67AFE3", &TunerConfig { ppm: -4.0, ..TunerConfig::default() }).unwrap();
        db.migrate_tuner_settings(&["735863DC-2F67AFE3".into()]).unwrap();
        assert_eq!(db.tuner("735863DC-2F67AFE3").ppm, -4.0, "runs only once");
        assert_eq!(db.tuner("rtl:00000001"), TunerConfig::default(), "unconfigured tuners use defaults");
        assert_eq!(db.settings().recording_dir, "/srv/recordings");
        assert_eq!(db.talkgroups("SGCo").len(), 2);
        assert_eq!(db.new_talkgroups("SGCo"), vec![85]);
        assert!(!db.record_unlisted("SGCo"));
        assert_eq!(db.alias_lists(), vec!["SGCo"], "the old list became an alias list");
    }

    #[test]
    fn tuner_modes_and_voice_freqs() {
        let db = Db::open_in_memory().unwrap();
        db.conn().execute(r#"INSERT INTO tuners (id, json) VALUES ('rtl:1', '{"enabled":false,"ppm":2.0}'), ('rtl:2', '{"enabled":true}')"#, []).unwrap();
        assert_eq!((db.tuner("rtl:1").mode, db.tuner("rtl:1").ppm), (TunerMode::Disabled, 2.0), "switched off before modes existed");
        assert_eq!(db.tuner("rtl:2").mode, TunerMode::Live);
        let t = TunerConfig { mode: TunerMode::Setup, center_hz: Some(853_000_000), min_hz: Some(851_000_000), max_hz: Some(855_000_000), ..TunerConfig::default() };
        db.save_tuner("rtl:1", &t).unwrap();
        assert_eq!(db.tuner("rtl:1"), t);
        let id = db.save_system(&System::default()).unwrap();
        db.add_voice_freqs(id, &[853_900_000, 853_475_000]).unwrap();
        db.add_voice_freqs(id, &[853_475_000]).unwrap();
        assert_eq!(db.voice_freqs(id), vec![853_475_000, 853_900_000]);
        db.delete_system(id).unwrap();
        assert!(db.voice_freqs(id).is_empty(), "they go with the system");
    }

    #[test]
    fn audit_newest_first() {
        let db = Db::open_in_memory().unwrap();
        db.audit(1, "owner", "login");
        db.audit(2, "owner", "changed radio settings");
        let log = db.audit_log(10);
        assert_eq!(log[0].action, "changed radio settings");
        assert_eq!(log[1].at, 1);
        assert_eq!(db.audit_log(1).len(), 1);
    }
}
