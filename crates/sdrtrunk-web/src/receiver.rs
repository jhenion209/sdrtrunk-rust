//! Decoder threads, one per enabled system: each owns its tuner and control channel decoder and publishes status and
//! calls. `Receivers` starts and stops them and hands out tuners (pinned by serial, or Automatic). Tuners in Setup mode
//! get a thread of their own that only feeds the spectrum display.
pub use sdrtrunk_p25::control::center_for;
use crate::db::{Db, System, TunerConfig, TunerMode};
use crate::recorder::{Names, RecordPolicy, Recorder, Tuner};
use crate::streaming::Streams;
use crate::tuning::{self, choose_center};
use serde::Serialize;
use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering as AtomicOrdering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Instant, SystemTime, UNIX_EPOCH};
use tokio::sync::broadcast;
use crate::quality::{Bucket, Window};
use sdrtrunk_p25::control::{ControlChannel, ControlEvent, FrameCounts, Modulation};
use sdrtrunk_p25::correction::{Correction, TICK_MS};

#[derive(Clone, Debug, Default, Serialize)]
pub struct Status {
    /// "starting" | "running" | "stopped" | "error"
    pub state: String,
    pub error: Option<String>,
    /// Id of the tuner in use, where it is tuned and how fast it samples.
    pub tuner: Option<String>,
    pub center_hz: Option<u64>,
    pub rate_hz: u32,
    /// Width received usefully around the centre (the weak band edges left out), and whether the centre is locked.
    pub usable_hz: u32,
    pub locked: bool,
    /// (NAC, WACN, system)
    pub site: Option<(i32, u32, u16)>,
    pub control_hz: Option<u64>,
    pub tsbk_per_s: f64,
    pub dropped: u64,
    pub started_ms: i64,
    /// Tuner correction in use (moves with Automatic PPM) and the measured frequency error of the control channel.
    pub ppm: f64,
    pub auto_ppm: bool,
    pub freq_error_hz: Option<f64>,
    /// MP3 files written since the decoder started, and the last recording problem.
    pub recorded: u64,
    pub recording_error: Option<String>,
}

/// A grant, with the System name it came from and the alias list its talkgroup names come from.
#[derive(Clone, Debug, Serialize)]
pub struct CallEvent { pub system: String, pub alias_list: String, pub at_ms: i64, pub group: u16, pub source: u32, pub hz: u64, pub encrypted: bool, pub emergency: bool }

/// One spectrum display frame: dB per point (lowest frequency first) across `center_hz ± rate_hz/2`, the width received
/// usefully (0: all of it) and what sits where.
#[derive(Clone, Debug, Serialize)]
pub struct SpectrumFrame { pub seq: u64, pub center_hz: u64, pub rate_hz: u32, pub usable_hz: u32, pub db: Vec<i16>, pub markers: Vec<Marker> }

/// A labelled frequency on the spectrum: the control channel or a voice channel being followed.
#[derive(Clone, Debug, Serialize)]
pub struct Marker { pub hz: u64, pub kind: &'static str, pub label: String }

/// Recent control channel soft symbols in milliradians (ideal: +/-785 and +/-2356), oldest first.
#[derive(Clone, Debug, Serialize)]
pub struct SymbolFrame { pub seq: u64, pub soft: Vec<i16> }

/// The tuner spectrum, the control channel's own spectrum (the Java reference's per-channel FFT: the channel's samples, centred where
/// it is decoded) and its symbols are only produced while someone watches (each `*_until_ms` is pushed ahead by the
/// page's stream).
#[derive(Default)]
pub struct SpectrumView {
    pub watch_until_ms: i64,
    pub frame: Option<SpectrumFrame>,
    pub channel_until_ms: i64,
    pub channel: Option<SpectrumFrame>,
    pub symbols_until_ms: i64,
    pub symbols: Option<SymbolFrame>,
}

/// Frames per second while watched, FFT size and points sent to the page.
const SPECTRUM_MS: i64 = 100;
const SPECTRUM_FFT: usize = 2048;
pub const SPECTRUM_WIDTH: usize = 1024;
/// Channel FFT: 1024 points at the channel rate (about 49 Hz each at 50 kHz), sent as 512.
const CHANNEL_FFT: usize = 1024;
pub const CHANNEL_WIDTH: usize = 512;

/// Interleaved int16 IQ blocks. Created on the decoder thread, so it need not be `Send`.
/// `next_block` returns an empty block when nothing arrived yet (lets the thread notice a stop) and None at the end.
pub trait IqSource {
    fn next_block(&mut self) -> Option<Vec<i16>>;
    fn tune(&mut self, center_hz: u32) -> Result<(), String>;
    fn dropped(&self) -> u64;
    /// Samples per second this source delivers (an RTL-SDR always runs at its own rate).
    fn rate_hz(&self) -> u32;
}

/// Opens the tuner with this id (Airspy serial or "rtl:SERIAL") and settings, tuned to `center` Hz.
pub type SourceFactory = Arc<dyn Fn(&TunerConfig, u32, &str) -> Result<Box<dyn IqSource>, String> + Send + Sync>;

/// The SDRs this host has: serials of those connected now, and how to open one.
#[derive(Clone)]
pub struct Sdrs { pub list: Arc<dyn Fn() -> Vec<String> + Send + Sync>, pub open: SourceFactory }

/// The Airspys and RTL-SDRs on this host.
#[cfg(target_os = "linux")]
pub fn sdrs() -> Sdrs {
    use std::sync::mpsc::{Receiver as Rx, RecvTimeoutError};
    use std::sync::atomic::AtomicU64;
    use sdrtrunk_source::{airspy::Airspy, rtlsdr::{self, RtlSdr}};
    enum Dev { Airspy(Airspy, u32), Rtl(RtlSdr) }
    struct Live(Dev);
    impl Live {
        fn parts(&self) -> (&Rx<Vec<i16>>, &AtomicU64) {
            match &self.0 { Dev::Airspy(a, _) => (&a.blocks, &a.dropped), Dev::Rtl(r) => (&r.blocks, &r.dropped) }
        }
    }
    impl IqSource for Live {
        fn next_block(&mut self) -> Option<Vec<i16>> {
            match self.parts().0.recv_timeout(std::time::Duration::from_millis(200)) {
                Ok(b) => Some(b),
                Err(RecvTimeoutError::Timeout) => Some(Vec::new()),
                Err(RecvTimeoutError::Disconnected) => None,
            }
        }
        fn tune(&mut self, center_hz: u32) -> Result<(), String> {
            match &self.0 { Dev::Airspy(a, _) => a.set_frequency(center_hz).map_err(|e| e.to_string()), Dev::Rtl(r) => r.set_frequency(center_hz) }
        }
        fn dropped(&self) -> u64 { self.parts().1.load(AtomicOrdering::Relaxed) }
        fn rate_hz(&self) -> u32 { match &self.0 { Dev::Airspy(_, rate) => *rate, Dev::Rtl(_) => rtlsdr::RATE_HZ } }
    }
    Sdrs {
        list: Arc::new(|| sdrtrunk_source::airspy::list().into_iter().chain(rtlsdr::list()).collect()),
        open: Arc::new(|s: &TunerConfig, center: u32, id: &str| {
            let label = sdrtrunk_source::tuner_label(id);
            let dev = if id.starts_with(rtlsdr::PREFIX) {
                Dev::Rtl(RtlSdr::open(id, center, s.rtl_gain_db).map_err(|e| format!("{label}: {e}"))?)
            } else {
                let sn = sdrtrunk_source::parse_serial(id).ok_or_else(|| format!("{id} is not an Airspy serial"))?;
                Dev::Airspy(Airspy::open(sn, center, s.rate_hz, s.gain).map_err(|e| format!("{label}: {e}"))?, s.rate_hz)
            };
            Ok(Box::new(Live(dev)) as Box<dyn IqSource>)
        }),
    }
}

#[cfg(not(target_os = "linux"))]
pub fn sdrs() -> Sdrs { Sdrs { list: Arc::new(Vec::new), open: Arc::new(|_, _, _| Err("SDRs are Linux only".into())) } }

/// Which tuner a system gets. `want` is its pinned tuner id or "" for Automatic; `claimed` are in use by running
/// systems; `reserved` are pinned by the other enabled systems (Automatic never takes those).
pub fn pick_tuner(want: &str, present: &[String], claimed: &HashSet<String>, reserved: &[String]) -> Result<String, String> {
    if !want.is_empty() {
        let sn = sdrtrunk_source::normalize_tuner(want).ok_or_else(|| format!("{want} is not an SDR id"))?;
        let label = sdrtrunk_source::tuner_label(&sn);
        if claimed.contains(&sn) { return Err(format!("{label} is used by another system")); }
        if !present.contains(&sn) { return Err(format!("{label} not found")); }
        return Ok(sn);
    }
    if present.is_empty() { return Err("no SDR found".into()); }
    let reserved: Vec<String> = reserved.iter().filter_map(|r| sdrtrunk_source::normalize_tuner(r)).collect();
    present.iter().find(|p| !claimed.contains(*p) && !reserved.contains(p)).cloned()
        .ok_or_else(|| "no free SDR: every connected SDR is in use or pinned to another system".into())
}

/// True when `failover_ms` of samples passed since the later of the last valid TSBK and the last retune.
pub fn failover_due(last_valid_sample: Option<u64>, since_tune_sample: u64, now_sample: u64, rate_hz: u32, failover_ms: u32) -> bool {
    let window = rate_hz as u64 * failover_ms as u64 / 1000;
    let from = last_valid_sample.unwrap_or(since_tune_sample).max(since_tune_sample);
    now_sample.saturating_sub(from) >= window
}

pub fn now_ms() -> i64 { SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_millis() as i64 }

type Claimed = Arc<Mutex<HashSet<String>>>;
type Run = Mutex<Option<(Arc<AtomicBool>, JoinHandle<()>)>>;

/// Stops and joins a thread; its tuner is closed and free again when this returns.
fn join(run: &Run, status: &Mutex<Status>) {
    if let Some((stop, handle)) = run.lock().unwrap_or_else(|p| p.into_inner()).take() {
        stop.store(true, AtomicOrdering::Relaxed);
        let _ = handle.join();
    }
    let mut st = status.lock().unwrap_or_else(|p| p.into_inner());
    if st.state != "error" { st.state = "stopped".into(); }
}

/// One system's decoder.
pub struct Receiver {
    pub system: System,
    pub status: Arc<Mutex<Status>>,
    /// What to record, read by the recorder on every call (no restart needed after an edit).
    pub policy: Arc<Mutex<RecordPolicy>>,
    pub spectrum: Arc<Mutex<SpectrumView>>,
    /// The last minute of control channel quality, one bucket per second.
    pub quality: Arc<Mutex<Window>>,
    /// Voice frequencies the system's grants used (from the database at start, plus new ones): where the tuner sits.
    pub known: Arc<Mutex<BTreeSet<u64>>>,
    run: Run,
}

/// What the decoder thread shares with the rest of the program.
#[derive(Clone)]
struct Shared {
    status: Arc<Mutex<Status>>,
    calls: broadcast::Sender<CallEvent>,
    policy: Arc<Mutex<RecordPolicy>>,
    spectrum: Arc<Mutex<SpectrumView>>,
    quality: Arc<Mutex<Window>>,
    known: Arc<Mutex<BTreeSet<u64>>>,
    streams: Arc<Streams>,
}

impl Receiver {
    #[allow(clippy::too_many_arguments)]
    fn start(system: System, dir: String, reserved: Vec<String>, tuners: Vec<(String, TunerConfig)>, sdrs: Sdrs, claimed: Claimed,
             calls: broadcast::Sender<CallEvent>, policy: RecordPolicy, known: Vec<u64>, streams: Arc<Streams>) -> Receiver {
        let shared = Shared {
            status: Arc::new(Mutex::new(Status {
                state: "starting".into(), control_hz: system.control_hz.first().copied(), started_ms: now_ms(), ..Status::default()
            })),
            calls,
            policy: Arc::new(Mutex::new(policy)),
            spectrum: Arc::default(),
            quality: Arc::default(),
            known: Arc::new(Mutex::new(known.into_iter().collect())),
            streams,
        };
        let stop = Arc::new(AtomicBool::new(false));
        let (s, sh, flag) = (system.clone(), shared.clone(), stop.clone());
        let handle = std::thread::Builder::new().name(format!("decoder-{}", system.id))
            .spawn(move || decode(s, dir, reserved, tuners, sdrs, claimed, flag, sh)).expect("spawn decoder");
        let Shared { status, policy, spectrum, quality, known, .. } = shared;
        Receiver { system, status, policy, spectrum, quality, known, run: Mutex::new(Some((stop, handle))) }
    }

    /// Stops and joins the decoder thread; its tuner is closed and free again when this returns.
    fn stop(&self) { join(&self.run, &self.status); }

    pub fn state(&self) -> String { self.status.lock().unwrap_or_else(|p| p.into_inner()).state.clone() }
}

impl Drop for Receiver { fn drop(&mut self) { self.stop(); } }

/// A tuner in Setup mode: open at its fixed centre so its spectrum can be watched; no system uses it.
pub struct SetupRun { pub status: Arc<Mutex<Status>>, pub spectrum: Arc<Mutex<SpectrumView>>, run: Run }

impl SetupRun {
    fn start(id: String, cfg: TunerConfig, sdrs: Sdrs, claimed: Claimed) -> SetupRun {
        let status = Arc::new(Mutex::new(Status { state: "starting".into(), started_ms: now_ms(), ..Status::default() }));
        let spectrum: Arc<Mutex<SpectrumView>> = Arc::default();
        let stop = Arc::new(AtomicBool::new(false));
        let (st, sp, flag) = (status.clone(), spectrum.clone(), stop.clone());
        let handle = std::thread::Builder::new().name(format!("setup-{id}"))
            .spawn(move || setup(id, cfg, sdrs, claimed, flag, st, sp)).expect("spawn setup");
        SetupRun { status, spectrum, run: Mutex::new(Some((stop, handle))) }
    }

    fn stop(&self) { join(&self.run, &self.status); }
}

impl Drop for SetupRun { fn drop(&mut self) { self.stop(); } }

/// The running systems, by system id, and the tuners in Setup mode, by tuner id.
pub struct Receivers {
    sdrs: Sdrs,
    claimed: Claimed,
    /// Grants from every system.
    pub calls: broadcast::Sender<CallEvent>,
    running: Mutex<BTreeMap<i64, Arc<Receiver>>>,
    setups: Mutex<BTreeMap<String, Arc<SetupRun>>>,
    /// The Icecast streams; every system hands them the calls they carry.
    pub streams: Arc<Streams>,
}

fn policy_for(db: &Db, system: &str) -> RecordPolicy {
    let mut streamed: Vec<u16> = db.streams().into_iter().filter(|s| s.enabled).flat_map(|s| s.members)
        .filter(|m| m.list == system).map(|m| m.group).collect();
    streamed.sort_unstable();
    streamed.dedup();
    RecordPolicy { record_unlisted: db.record_unlisted(system), talkgroups: db.talkgroups(system), streamed }
}

impl Receivers {
    pub fn new(sdrs: Sdrs) -> Receivers {
        Receivers {
            sdrs, claimed: Arc::default(), calls: broadcast::channel(256).0, running: Mutex::new(BTreeMap::new()),
            setups: Mutex::new(BTreeMap::new()), streams: Arc::new(Streams::new(std::time::Duration::from_secs(30))),
        }
    }

    fn start(&self, db: &Db, all: &[System], s: &System) -> Arc<Receiver> {
        let reserved = all.iter().filter(|o| o.enabled && o.id != s.id && !o.tuner.is_empty()).map(|o| o.tuner.clone()).collect();
        Arc::new(Receiver::start(s.clone(), db.settings().recording_dir, reserved, db.tuners(), self.sdrs.clone(), self.claimed.clone(),
            self.calls.clone(), policy_for(db, s.list_name()), db.voice_freqs(s.id), self.streams.clone()))
    }

    /// Stops tuner `id`'s Setup run, and starts one if the tuner is (now) in Setup mode and connected.
    fn apply_setup(&self, db: &Db, id: &str) {
        let mut setups = self.setups.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(r) = setups.remove(id) { r.stop(); }
        let cfg = db.tuner(id);
        if cfg.mode == TunerMode::Setup && (self.sdrs.list)().iter().any(|t| t == id) {
            setups.insert(id.to_string(), Arc::new(SetupRun::start(id.to_string(), cfg, self.sdrs.clone(), self.claimed.clone())));
        }
    }

    /// Stops every system and Setup run, then starts the enabled systems and the tuners in Setup mode (startup,
    /// recordings folder change).
    pub fn restart_all(&self, db: &Db) {
        let mut run = self.running.lock().unwrap_or_else(|p| p.into_inner());
        for (_, r) in std::mem::take(&mut *run) { r.stop(); }
        for (_, r) in std::mem::take(&mut *self.setups.lock().unwrap_or_else(|p| p.into_inner())) { r.stop(); }
        let all = db.systems();
        for s in all.iter().filter(|s| s.enabled) { run.insert(s.id, self.start(db, &all, s)); }
        drop(run);
        for (id, _) in db.tuners().into_iter().filter(|(_, c)| c.mode == TunerMode::Setup) { self.apply_setup(db, &id); }
    }

    /// After system `id` was saved or deleted: restarts it as stored (stops it if deleted or disabled), then
    /// retries systems that failed, since a tuner may have come free.
    pub fn apply(&self, db: &Db, id: i64) {
        let mut run = self.running.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(r) = run.remove(&id) { r.stop(); }
        let all = db.systems();
        if let Some(s) = all.iter().find(|s| s.id == id && s.enabled) { run.insert(id, self.start(db, &all, s)); }
        for s in all.iter().filter(|s| s.enabled && s.id != id) {
            if run.get(&s.id).is_some_and(|r| r.state() == "error") {
                if let Some(r) = run.remove(&s.id) { r.stop(); }
                run.insert(s.id, self.start(db, &all, s));
            }
        }
    }

    /// After tuner `id`'s settings changed: stops its Setup run, restarts the systems using it (they pick up the new
    /// settings, or another tuner if it left Live mode), retries systems that failed (it may be Live again), then
    /// starts its Setup run if it is in Setup mode.
    pub fn apply_tuner(&self, db: &Db, id: &str) {
        if let Some(r) = self.setups.lock().unwrap_or_else(|p| p.into_inner()).remove(id) { r.stop(); }
        let restart: Vec<i64> = self.running.lock().unwrap_or_else(|p| p.into_inner()).iter()
            .filter(|(_, r)| { let st = r.status.lock().unwrap_or_else(|p| p.into_inner()); st.tuner.as_deref() == Some(id) || st.state == "error" })
            .map(|(&sys, _)| sys).collect();
        for sys in restart { self.apply(db, sys); }
        self.apply_setup(db, id);
    }

    /// Tuner `id`'s Setup run status, while it is in Setup mode.
    pub fn setup_status(&self, id: &str) -> Option<Status> {
        self.setups.lock().unwrap_or_else(|p| p.into_inner()).get(id).map(|r| r.status.lock().unwrap_or_else(|p| p.into_inner()).clone())
    }

    /// The spectrum display of tuner `id`: the system using it, or its Setup run.
    pub fn tuner_view(&self, id: &str) -> Option<Arc<Mutex<SpectrumView>>> {
        self.on_tuner(id).map(|r| r.spectrum.clone())
            .or_else(|| self.setups.lock().unwrap_or_else(|p| p.into_inner()).get(id).map(|r| r.spectrum.clone()))
    }

    /// (system id, status) of every running system, for the Tuners page.
    pub fn statuses(&self) -> Vec<(i64, Status)> {
        self.running.lock().unwrap_or_else(|p| p.into_inner()).iter()
            .map(|(&id, r)| (id, r.status.lock().unwrap_or_else(|p| p.into_inner()).clone())).collect()
    }

    /// After a stream was added, changed or deleted: restarts the streams and tells every system which talkgroups
    /// are streamed now.
    pub fn apply_streams(&self, db: &Db) {
        self.streams.apply(db.streams());
        self.refresh_policy(db);
    }

    /// Talkgroup or "record unlisted" change: every running system reloads its list (no restart).
    pub fn refresh_policy(&self, db: &Db) {
        for r in self.running.lock().unwrap_or_else(|p| p.into_inner()).values() {
            *r.policy.lock().unwrap_or_else(|p| p.into_inner()) = policy_for(db, r.system.list_name());
        }
    }

    /// The running system using tuner `id`, if any.
    pub fn on_tuner(&self, id: &str) -> Option<Arc<Receiver>> {
        self.running.lock().unwrap_or_else(|p| p.into_inner()).values()
            .find(|r| r.status.lock().unwrap_or_else(|p| p.into_inner()).tuner.as_deref() == Some(id)).cloned()
    }

    pub fn get(&self, id: i64) -> Option<Arc<Receiver>> { self.running.lock().unwrap_or_else(|p| p.into_inner()).get(&id).cloned() }

    pub fn status(&self, id: i64) -> Option<Status> { self.get(id).map(|r| r.status.lock().unwrap_or_else(|p| p.into_inner()).clone()) }

    /// Connected tuners plus those this process has open, sorted (for the tuner picker).
    pub fn devices(&self) -> Vec<String> {
        let mut d = (self.sdrs.list)();
        d.extend(self.claimed.lock().unwrap_or_else(|p| p.into_inner()).iter().cloned());
        d.sort();
        d.dedup();
        d
    }

    /// Once a minute: stores each running system's last minute of quality and the voice frequencies it has seen, and
    /// saves a PPM that Automatic PPM moved (so they are remembered after a restart).
    pub fn record_minute(&self, db: &Db, now: i64) {
        let running: Vec<(i64, Arc<Receiver>)> = self.running.lock().unwrap_or_else(|p| p.into_inner())
            .iter().map(|(&id, r)| (id, r.clone())).collect();
        for (id, r) in running {
            let summary = r.quality.lock().unwrap_or_else(|p| p.into_inner()).summary(now, 60);
            if !summary.is_empty() { let _ = db.add_quality(id, now, &summary); }
            let known: Vec<u64> = r.known.lock().unwrap_or_else(|p| p.into_inner()).iter().copied().collect();
            let _ = db.add_voice_freqs(id, &known);
            save_auto_ppm(db, &r.status.lock().unwrap_or_else(|p| p.into_inner()).clone());
        }
    }

    pub fn stop_all(&self) {
        for (_, r) in std::mem::take(&mut *self.running.lock().unwrap_or_else(|p| p.into_inner())) { r.stop(); }
        for (_, r) in std::mem::take(&mut *self.setups.lock().unwrap_or_else(|p| p.into_inner())) { r.stop(); }
        self.streams.stop_all();
    }
}

impl Drop for Receivers { fn drop(&mut self) { self.stop_all(); } }

/// Saves the PPM a running system's Automatic PPM arrived at onto its tuner (rounded to 0.001), unless the tuner's
/// Automatic PPM was switched off meanwhile.
fn save_auto_ppm(db: &Db, st: &Status) {
    let Some(tuner) = st.tuner.as_deref().filter(|_| st.auto_ppm && st.state == "running") else { return };
    let mut cfg = db.tuner(tuner);
    let ppm = (st.ppm * 1000.0).round() / 1000.0;
    if cfg.auto_ppm && cfg.ppm != ppm {
        cfg.ppm = ppm;
        let _ = db.save_tuner(tuner, &cfg);
    }
}

fn update(status: &Mutex<Status>, f: impl FnOnce(&mut Status)) { f(&mut status.lock().unwrap_or_else(|p| p.into_inner())); }

fn fail(status: &Mutex<Status>, msg: String) {
    eprintln!("decoder: {msg}");
    update(status, |s| { s.state = "error".into(); s.error = Some(msg); });
}

/// Marks the status as an error if the decoder thread unwinds, so the Status page never shows a dead decoder as running.
struct PanicGuard(Arc<Mutex<Status>>);

impl Drop for PanicGuard {
    fn drop(&mut self) {
        if std::thread::panicking() {
            fail(&self.0, "decoder stopped unexpectedly; save the system to restart it (details in the log)".into());
        }
    }
}

/// A tuner held by a decoder thread; released when the thread ends, however it ends.
struct Claim(Claimed, String);

impl Drop for Claim {
    fn drop(&mut self) { self.0.lock().unwrap_or_else(|p| p.into_inner()).remove(&self.1); }
}

#[allow(clippy::too_many_arguments)]
fn decode(s: System, dir: String, reserved: Vec<String>, tuners: Vec<(String, TunerConfig)>, sdrs: Sdrs, claimed: Claimed,
          stop: Arc<AtomicBool>, sh: Shared) {
    let Shared { status, calls, policy, spectrum, quality, known, streams } = sh;
    let _guard = PanicGuard(status.clone());
    let config = |id: &str| tuners.iter().find(|(t, _)| t == id).map(|(_, c)| c.clone()).unwrap_or_default();
    if let Some(pin) = sdrtrunk_source::normalize_tuner(&s.tuner) {
        let label = sdrtrunk_source::tuner_label(&pin);
        match config(&pin).mode {
            TunerMode::Live => {}
            TunerMode::Setup => return fail(&status, format!("{label} is in Setup mode on the Tuners page")),
            TunerMode::Disabled => return fail(&status, format!("{label} is switched off on the Tuners page")),
        }
    }
    let present: Vec<String> = (sdrs.list)().into_iter().filter(|t| config(t).mode == TunerMode::Live).collect();
    let serial = {
        let mut c = claimed.lock().unwrap_or_else(|p| p.into_inner());
        match pick_tuner(&s.tuner, &present, &c, &reserved) {
            Ok(sn) => { c.insert(sn.clone()); sn }
            Err(e) => return fail(&status, e),
        }
    };
    let _claim = Claim(claimed.clone(), serial.clone());
    let tuner = config(&serial);
    let mut idx = 0;
    let freq = |i: usize| s.control_hz[i] as i64;
    let modulation = Modulation::parse(&s.modulation).unwrap_or_default();
    let names = Names { system: s.system_name.clone(), site: s.site_name.clone(), channel: s.channel_name.clone() };
    let recorder = || Recorder::new(Path::new(&dir), names.clone(), policy.clone()).streaming(streams.clone(), s.list_name().to_string());
    let known_now = || known.lock().unwrap_or_else(|p| p.into_inner()).iter().map(|&hz| hz as i64).collect::<Vec<i64>>();
    let opened_at = tuner.center_hz.unwrap_or(center_for(freq(0)) as u64) as u32;
    let mut src = match (sdrs.open)(&tuner, opened_at, &serial) { Ok(src) => src, Err(e) => return fail(&status, e) };
    let rate = src.rate_hz();
    let base = tuning::Window::for_tuner(&serial, rate, opened_at as i64, tuner.min_hz, tuner.max_hz);
    let mut win = match place(base, tuner.center_hz, freq(0), &known_now()) { Ok(w) => w, Err(e) => return fail(&status, e) };
    if win.center != opened_at as i64 {
        if let Err(e) = src.tune(win.center as u32) { return fail(&status, e); }
    }
    let mut corr = Correction::new(tuner.ppm, tuner.auto_ppm);
    update(&status, |st| {
        st.tuner = Some(serial.clone()); st.center_hz = Some(win.center as u64); st.rate_hz = rate;
        (st.usable_hz, st.locked) = (win.usable as u32, tuner.center_hz.is_some());
        (st.ppm, st.auto_ppm) = (corr.ppm, corr.auto);
    });
    let channel = |i: usize, ppm: f64, center: i64| ControlChannel::new(center as f64, rate as f64, freq(i), ppm, now_ms(), modulation);
    let mut cc = match channel(0, corr.ppm, win.center) { Ok(c) => c, Err(e) => return fail(&status, e) };
    let mut rec = recorder();
    let mut recorded_before = 0;
    update(&status, |st| st.state = "running".into());
    let (mut tick, mut tick_valid, mut tick_frames) = (Instant::now(), 0u64, FrameCounts::default());
    let mut last_correction_ms = now_ms();
    let mut events = Vec::new();
    let mut fft = sdrtrunk_dsp::spectrum::Spectrum::new(SPECTRUM_FFT, SPECTRUM_WIDTH);
    let (mut last_frame_ms, mut seq) = (0i64, 0u64);
    let (mut last_symbols_ms, mut symbols_seq) = (0i64, 0u64);
    let mut channel_fft = sdrtrunk_dsp::spectrum::Spectrum::new(CHANNEL_FFT, CHANNEL_WIDTH);
    let (mut last_channel_ms, mut channel_seq) = (0i64, 0u64);

    while !stop.load(AtomicOrdering::Relaxed) {
        let Some(block) = src.next_block() else {
            update(&status, |st| st.state = "stopped".into());
            return;
        };
        cc.process(&block, &mut |e| events.push(e));
        if let Some(hz) = cc.take_frequency_error() { corr.measure(hz); }
        let now = now_ms();
        if now - last_correction_ms >= TICK_MS {
            last_correction_ms = now;
            if corr.tick(freq(idx) as f64) { cc.set_tuning(corr.ppm, corr.channel_hz); }
            update(&status, |st| { st.ppm = corr.ppm; st.freq_error_hz = corr.measured_hz; });
        }
        {
            let mut follow = Follow { cc: &mut cc, src: &mut src, win: &mut win, locked: tuner.center_hz.is_some(), control: freq(idx),
                known: &known, status: &status };
            for e in events.drain(..) {
                match &e {
                    ControlEvent::Site { nac, wacn, system } => update(&status, |st| st.site = Some((*nac, *wacn, *system))),
                    ControlEvent::Call(g) => {
                        known.lock().unwrap_or_else(|p| p.into_inner()).insert(g.hz);
                        // No subscribers is normal (no page open).
                        let _ = calls.send(CallEvent { system: s.system_name.clone(), alias_list: s.list_name().to_string(), at_ms: now, group: g.group, source: g.source, hz: g.hz,
                            encrypted: g.encrypted, emergency: g.emergency });
                    }
                    ControlEvent::Voice { .. } | ControlEvent::VoiceEnd { .. } => {}
                }
                rec.handle(&e, now, &mut follow);
            }
            rec.tick(now, &mut follow);
        }
        let (watched, channel_watched, symbols_watched) = {
            let v = spectrum.lock().unwrap_or_else(|p| p.into_inner());
            (v.watch_until_ms > now, v.channel_until_ms > now, v.symbols_until_ms > now)
        };
        cc.capture_symbols(symbols_watched);
        cc.capture_iq(channel_watched);
        if channel_watched && now - last_channel_ms >= SPECTRUM_MS {
            if let Some(db) = channel_fft.frame_f32(&cc.take_iq(), 4) {
                last_channel_ms = now;
                channel_seq += 1;
                let frame = SpectrumFrame { seq: channel_seq, center_hz: cc.control_decode_hz() as u64, rate_hz: cc.channel_rate_hz() as u32,
                    usable_hz: 0, db: db.iter().map(|&d| d.round().clamp(-200.0, 50.0) as i16).collect(),
                    markers: vec![Marker { hz: s.control_hz[idx], kind: "control", label: "control".into() }] };
                spectrum.lock().unwrap_or_else(|p| p.into_inner()).channel = Some(frame);
            }
        }
        if symbols_watched && now - last_symbols_ms >= SPECTRUM_MS {
            last_symbols_ms = now;
            symbols_seq += 1;
            let soft = cc.take_symbols().iter().map(|&s| (s * 1000.0).round().clamp(-4000.0, 4000.0) as i16).collect();
            spectrum.lock().unwrap_or_else(|p| p.into_inner()).symbols = Some(SymbolFrame { seq: symbols_seq, soft });
        }
        if watched && now - last_frame_ms >= SPECTRUM_MS {
            let mut markers = vec![Marker { hz: s.control_hz[idx], kind: "control", label: "control".into() }];
            markers.extend(rec.following().into_iter().map(|(hz, tg, name, encrypted)| Marker {
                hz, kind: if encrypted { "encrypted" } else { "voice" }, label: name.map_or(format!("TG {tg}"), |n| format!("{tg} {n}")),
            }));
            if let Some(frame) = tuner_frame(&mut fft, &block, seq + 1, &win, rate, markers) {
                (last_frame_ms, seq) = (now, seq + 1);
                spectrum.lock().unwrap_or_else(|p| p.into_inner()).frame = Some(frame);
            }
        }
        if tick.elapsed().as_secs_f64() >= 1.0 {
            let f = cc.frames();
            let frames = FrameCounts { valid: f.valid - tick_frames.valid, invalid: f.invalid - tick_frames.invalid,
                lost_bits: f.lost_bits - tick_frames.lost_bits };
            tick_frames = f;
            quality.lock().unwrap_or_else(|p| p.into_inner()).push(Bucket { at_ms: now, signal_db: cc.take_signal_dbfs(), frames });
            let rate = (cc.valid_tsbk() - tick_valid) as f64 / tick.elapsed().as_secs_f64();
            update(&status, |st| {
                st.tsbk_per_s = rate;
                st.dropped = src.dropped();
                st.recorded = recorded_before + rec.recorded;
                st.recording_error = rec.last_error.clone();
            });
            (tick, tick_valid) = (Instant::now(), cc.valid_tsbk());
        }
        if s.control_hz.len() > 1 && failover_due(cc.last_valid_sample(), 0, cc.samples(), rate, s.failover_ms) {
            idx = (idx + 1) % s.control_hz.len();
            eprintln!("decoder {}: no control channel for {} ms, trying {} Hz", s.label(), s.failover_ms, freq(idx));
            win = match place(base, tuner.center_hz, freq(idx), &known_now()) { Ok(w) => w, Err(e) => return fail(&status, e) };
            if let Err(e) = src.tune(win.center as u32) { return fail(&status, e); }
            cc = match channel(idx, corr.ppm, win.center) { Ok(c) => c, Err(e) => return fail(&status, e) };
            corr.reset_channel();
            tick_frames = FrameCounts::default();
            // The new control channel has no voice channels: start the recorder afresh (open transmissions are lost).
            recorded_before += rec.recorded;
            rec = recorder();
            tick_valid = 0;
            update(&status, |st| { st.control_hz = Some(s.control_hz[idx]); st.center_hz = Some(win.center as u64); st.site = None; });
        }
    }
}

/// Where the tuner sits for control channel `control`: its locked centre (which must cover it), or the centre that
/// covers it and the most known voice frequencies, starting from 50 kHz below it.
fn place(base: tuning::Window, locked: Option<u64>, control: i64, known: &[i64]) -> Result<tuning::Window, String> {
    let mhz = |hz: i64| format!("{:.4} MHz", hz as f64 / 1e6);
    match locked {
        Some(c) => {
            let w = base.at(c as i64);
            let (lo, hi) = w.edges();
            if w.covers(control) { Ok(w) } else {
                Err(format!("control channel {} is outside this tuner's locked window ({} to {})", mhz(control), mhz(lo), mhz(hi)))
            }
        }
        None => choose_center(&[control], known, base.at(center_for(control) as i64)).map(|c| base.at(c))
            .ok_or_else(|| format!("control channel {} is outside this tuner's frequency limits", mhz(control))),
    }
}

/// Opens voice channels for the recorder. A call outside the window moves the tuner first, but only when the control
/// channel and every call in progress stay inside, and never with Lock center (the Java reference's rule).
struct Follow<'a> {
    cc: &'a mut ControlChannel,
    src: &'a mut Box<dyn IqSource>,
    win: &'a mut tuning::Window,
    locked: bool,
    control: i64,
    known: &'a Mutex<BTreeSet<u64>>,
    status: &'a Mutex<Status>,
}

impl Tuner for Follow<'_> {
    fn open(&mut self, hz: i64) -> Result<u32, String> {
        if !self.win.covers(hz) { self.move_to_cover(hz)?; }
        self.cc.add_voice(hz)
    }
    fn close(&mut self, id: u32) { self.cc.remove_voice(id) }
}

impl Follow<'_> {
    fn move_to_cover(&mut self, hz: i64) -> Result<(), String> {
        if !self.win.allowed(hz) { return Err("outside this tuner's frequency limits".into()); }
        if self.locked { return Err("outside this tuner's locked window".into()); }
        let mut required = vec![self.control, hz];
        required.extend(self.cc.voice_freqs());
        let known: Vec<i64> = self.known.lock().unwrap_or_else(|p| p.into_inner()).iter().map(|&k| k as i64).collect();
        let center = choose_center(&required, &known, *self.win)
            .ok_or("does not fit in one window with the control channel and the calls in progress")?;
        let old = self.win.center;
        self.cc.set_center(center as f64)?;
        if let Err(e) = self.src.tune(center as u32) {
            let _ = self.cc.set_center(old as f64);
            return Err(e);
        }
        *self.win = self.win.at(center);
        update(self.status, |st| st.center_hz = Some(center as u64));
        Ok(())
    }
}

/// A tuner spectrum frame from one block of samples, or None while the FFT collects samples.
fn tuner_frame(fft: &mut sdrtrunk_dsp::spectrum::Spectrum, block: &[i16], seq: u64, win: &tuning::Window, rate: u32, markers: Vec<Marker>)
    -> Option<SpectrumFrame> {
    let db = fft.frame(block, 4)?;
    Some(SpectrumFrame { seq, center_hz: win.center as u64, rate_hz: rate, usable_hz: win.usable as u32,
        db: db.iter().map(|&d| d.round().clamp(-200.0, 50.0) as i16).collect(), markers })
}

/// A tuner in Setup mode: open at its fixed centre, a spectrum frame about 10 a second while watched.
fn setup(id: String, cfg: TunerConfig, sdrs: Sdrs, claimed: Claimed, stop: Arc<AtomicBool>, status: Arc<Mutex<Status>>,
         spectrum: Arc<Mutex<SpectrumView>>) {
    let _guard = PanicGuard(status.clone());
    let label = sdrtrunk_source::tuner_label(&id);
    if !claimed.lock().unwrap_or_else(|p| p.into_inner()).insert(id.clone()) { return fail(&status, format!("{label} is in use")); }
    let _claim = Claim(claimed.clone(), id.clone());
    let center = cfg.center_hz.unwrap_or_default();
    let mut src = match (sdrs.open)(&cfg, center as u32, &id) { Ok(src) => src, Err(e) => return fail(&status, e) };
    let rate = src.rate_hz();
    let win = tuning::Window::for_tuner(&id, rate, center as i64, cfg.min_hz, cfg.max_hz);
    update(&status, |st| {
        st.state = "running".into(); st.tuner = Some(id.clone()); st.center_hz = Some(center); st.rate_hz = rate;
        (st.usable_hz, st.locked, st.ppm, st.auto_ppm) = (win.usable as u32, true, cfg.ppm, cfg.auto_ppm);
    });
    let mut fft = sdrtrunk_dsp::spectrum::Spectrum::new(SPECTRUM_FFT, SPECTRUM_WIDTH);
    let (mut last_frame_ms, mut seq) = (0i64, 0u64);
    while !stop.load(AtomicOrdering::Relaxed) {
        let Some(block) = src.next_block() else { return update(&status, |st| st.state = "stopped".into()) };
        let now = now_ms();
        let watched = spectrum.lock().unwrap_or_else(|p| p.into_inner()).watch_until_ms > now;
        if watched && now - last_frame_ms >= SPECTRUM_MS {
            if let Some(frame) = tuner_frame(&mut fft, &block, seq + 1, &win, rate, Vec::new()) {
                (last_frame_ms, seq) = (now, seq + 1);
                spectrum.lock().unwrap_or_else(|p| p.into_inner()).frame = Some(frame);
            }
        }
        update(&status, |st| st.dropped = src.dropped());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::{Duration, Instant};

    fn wait_for(limit: Duration, mut f: impl FnMut() -> bool) -> bool {
        let t = Instant::now();
        while t.elapsed() < limit { if f() { return true; } std::thread::sleep(Duration::from_millis(10)); }
        false
    }

    const A: &str = "735863DC-2F67AFE3";
    const B: &str = "10A862DC-34348363";

    #[test]
    fn tuner_rules() {
        let present = vec![A.to_string(), B.to_string()];
        let none = HashSet::new();
        assert_eq!(pick_tuner(B, &present, &none, &[]), Ok(B.into()), "pinned");
        assert_eq!(pick_tuner("0x10a862dc34348363", &present, &none, &[]), Ok(B.into()), "any serial form");
        assert_eq!(pick_tuner("", &present, &none, &[]), Ok(A.into()), "automatic: first free");
        assert_eq!(pick_tuner("", &present, &none, &[A.to_lowercase()]), Ok(B.into()), "automatic skips pinned ones");
        let used: HashSet<String> = [A.to_string()].into();
        assert_eq!(pick_tuner(A, &present, &used, &[]), Err(format!("Airspy {A} is used by another system")));
        assert_eq!(pick_tuner("", &present, &used, &[B.into()]).unwrap_err(), "no free SDR: every connected SDR is in use or pinned to another system");
        assert_eq!(pick_tuner("00000000-00000001", &present, &none, &[]), Err("Airspy 00000000-00000001 not found".into()));
        assert_eq!(pick_tuner("", &[], &none, &[]), Err("no SDR found".into()));
    let mixed = vec![A.to_string(), "rtl:00000001".to_string()];
    assert_eq!(pick_tuner("rtl:00000001", &mixed, &none, &[]), Ok("rtl:00000001".into()), "pinned RTL-SDR");
    assert_eq!(pick_tuner("", &mixed, &none, &[A.into()]), Ok("rtl:00000001".into()), "automatic falls back to the RTL-SDR");
    assert_eq!(pick_tuner("rtl:00000002", &mixed, &none, &[]), Err("RTL-SDR 00000002 not found".into()));
    }

    #[test]
    fn failover_rule() {
        let r = 3_000_000;
        assert!(!failover_due(None, 0, 2_999_999, r, 1000));
        assert!(failover_due(None, 0, 3_000_000, r, 1000));
        assert!(!failover_due(Some(1_000_000), 0, 1_000_000 + 2_997_000, r, 1000), "999 ms after a TSBK");
        assert!(failover_due(Some(1_000_000), 0, 1_000_000 + 3_003_000, r, 1000), "1001 ms after a TSBK");
        assert!(!failover_due(Some(10), 5_000_000, 5_000_000 + 2_000_000, r, 1000), "measured from the retune");
    }

    struct Zeros { open: Arc<AtomicUsize>, tunes: Arc<Mutex<Vec<u32>>> }
    impl IqSource for Zeros {
        fn next_block(&mut self) -> Option<Vec<i16>> { Some(vec![0; 2 * 32_768]) }
        fn tune(&mut self, center_hz: u32) -> Result<(), String> { self.tunes.lock().unwrap().push(center_hz); Ok(()) }
        fn dropped(&self) -> u64 { 0 }
        fn rate_hz(&self) -> u32 { 3_000_000 }
    }
    impl Drop for Zeros { fn drop(&mut self) { self.open.fetch_sub(1, Ordering::SeqCst); } }

    /// Two fake Airspys (A, B) that stream zeros; counts open sources and records retunes.
    fn zeros() -> (Sdrs, Arc<AtomicUsize>, Arc<Mutex<Vec<u32>>>) {
        let (open, tunes) = (Arc::new(AtomicUsize::new(0)), Arc::new(Mutex::new(Vec::new())));
        let (o, t) = (open.clone(), tunes.clone());
        let sdrs = Sdrs {
            list: Arc::new(|| vec![A.into(), B.into()]),
            open: Arc::new(move |_, _, _| {
                o.fetch_add(1, Ordering::SeqCst);
                Ok(Box::new(Zeros { open: o.clone(), tunes: t.clone() }) as Box<dyn IqSource>)
            }),
        };
        (sdrs, open, tunes)
    }

    fn sys(name: &str, tuner: &str) -> System { System { system_name: name.into(), tuner: tuner.into(), ..System::default() } }

    fn state(rx: &Receivers, id: i64) -> String { rx.status(id).map(|s| s.state).unwrap_or_default() }

    #[test]
    fn two_systems_two_tuners() {
        let db = Db::open_in_memory().unwrap();
        let (sdrs, open, _) = zeros();
        let auto = db.save_system(&sys("SGCo", "")).unwrap();
        let pinned = db.save_system(&sys("KSICS", A)).unwrap();
        let third = db.save_system(&sys("Extra", "")).unwrap();
        let rx = Receivers::new(sdrs);
        rx.restart_all(&db);
        assert!(wait_for(Duration::from_secs(2), || state(&rx, auto) == "running" && state(&rx, pinned) == "running"));
        assert_eq!(rx.status(pinned).unwrap().tuner.as_deref(), Some(A));
        assert_eq!(rx.status(auto).unwrap().tuner.as_deref(), Some(B), "automatic never takes a pinned tuner");
        assert!(wait_for(Duration::from_secs(1), || state(&rx, third) == "error"));
        assert!(rx.status(third).unwrap().error.unwrap().starts_with("no free SDR"));
        assert_eq!(open.load(Ordering::SeqCst), 2);

        // Disabling the pinned system frees A; the failed system is retried and gets it.
        db.save_system(&System { id: pinned, enabled: false, ..sys("KSICS", A) }).unwrap();
        rx.apply(&db, pinned);
        assert!(rx.get(pinned).is_none());
        assert!(wait_for(Duration::from_secs(2), || state(&rx, third) == "running"));
        assert_eq!(rx.status(third).unwrap().tuner.as_deref(), Some(A));
        rx.stop_all();
        assert_eq!(open.load(Ordering::SeqCst), 0, "stop closes every source");
        assert!(rx.claimed.lock().unwrap().is_empty(), "every tuner released");
    }

    #[test]
    fn disabled_tuner_never_used() {
        let db = Db::open_in_memory().unwrap();
        let (sdrs, _, _) = zeros();
        db.save_tuner(A, &TunerConfig { mode: TunerMode::Disabled, ..TunerConfig::default() }).unwrap();
        let auto = db.save_system(&sys("SGCo", "")).unwrap();
        let pinned = db.save_system(&sys("KSICS", A)).unwrap();
        let rx = Receivers::new(sdrs);
        rx.restart_all(&db);
        assert!(wait_for(Duration::from_secs(2), || state(&rx, auto) == "running" && state(&rx, pinned) == "error"));
        assert_eq!(rx.status(auto).unwrap().tuner.as_deref(), Some(B), "automatic skips the disabled tuner");
        assert_eq!(rx.status(pinned).unwrap().error.unwrap(), format!("Airspy {A} is switched off on the Tuners page"));
        db.save_tuner(A, &TunerConfig::default()).unwrap();
        rx.apply_tuner(&db, A);
        assert!(wait_for(Duration::from_secs(2), || state(&rx, pinned) == "running"), "switched back on: the failed system retried");
        assert_eq!(rx.status(pinned).unwrap().center_hz, Some(858_912_500));
    }

    #[test]
    fn spectrum_only_while_watched() {
        let db = Db::open_in_memory().unwrap();
        let (sdrs, _, _) = zeros();
        let id = db.save_system(&sys("SGCo", A)).unwrap();
        let rx = Receivers::new(sdrs);
        rx.restart_all(&db);
        assert!(wait_for(Duration::from_secs(2), || state(&rx, id) == "running"));
        let r = rx.on_tuner(A).expect("system on tuner A");
        std::thread::sleep(Duration::from_millis(300));
        assert!(r.spectrum.lock().unwrap().frame.is_none(), "nobody watching: no FFT work");
        r.spectrum.lock().unwrap().watch_until_ms = now_ms() + 5000;
        assert!(wait_for(Duration::from_secs(2), || r.spectrum.lock().unwrap().frame.is_some()));
        let f = r.spectrum.lock().unwrap().frame.clone().unwrap();
        assert_eq!((f.db.len(), f.center_hz, f.rate_hz), (SPECTRUM_WIDTH, 858_912_500, 3_000_000));
        assert_eq!((f.markers[0].hz, f.markers[0].kind), (858_962_500, "control"));
        assert!(rx.on_tuner(B).is_none());
    }

    #[test]
    fn quality_recorded_and_symbols_only_while_watched() {
        let db = Db::open_in_memory().unwrap();
        let (sdrs, _, _) = zeros();
        let id = db.save_system(&sys("SGCo", A)).unwrap();
        let rx = Receivers::new(sdrs);
        rx.restart_all(&db);
        assert!(wait_for(Duration::from_secs(2), || state(&rx, id) == "running"));
        let r = rx.get(id).unwrap();
        assert!(wait_for(Duration::from_secs(3), || r.quality.lock().unwrap().summary(now_ms(), 30).signal_db.is_some()));
        rx.record_minute(&db, now_ms());
        assert_eq!(db.quality(id, 0).len(), 1, "one sample per minute");
        let st = rx.status(id).unwrap();
        assert!(st.auto_ppm && st.ppm == 0.0 && st.freq_error_hz.is_none(), "no signal: nothing measured {st:?}");

        std::thread::sleep(Duration::from_millis(300));
        assert!(r.spectrum.lock().unwrap().symbols.is_none(), "nobody watching: no symbols");
        r.spectrum.lock().unwrap().symbols_until_ms = now_ms() + 5000;
        assert!(wait_for(Duration::from_secs(2), || r.spectrum.lock().unwrap().symbols.as_ref().is_some_and(|s| !s.soft.is_empty())));
        db.delete_system(id).unwrap();
        assert!(db.quality(id, 0).is_empty(), "history goes with the system");
    }

    #[test]
    fn auto_ppm_saved_on_its_tuner() {
        let db = Db::open_in_memory().unwrap();
        let running = |ppm: f64, auto: bool| Status { state: "running".into(), tuner: Some(A.into()), ppm, auto_ppm: auto, ..Status::default() };
        save_auto_ppm(&db, &running(-2.91234, true));
        assert_eq!(db.tuner(A).ppm, -2.912, "remembered for the next start");
        save_auto_ppm(&db, &running(5.0, false));
        assert_eq!(db.tuner(A).ppm, -2.912, "manual PPM is never overwritten");
        db.save_tuner(A, &TunerConfig { auto_ppm: false, ..db.tuner(A) }).unwrap();
        save_auto_ppm(&db, &running(1.0, true));
        assert_eq!(db.tuner(A).ppm, -2.912, "switched off on the Tuners page meanwhile");
    }

    #[test]
    fn restart_never_opens_twice() {
        let db = Db::open_in_memory().unwrap();
        let (sdrs, open, _) = zeros();
        let id = db.save_system(&sys("SGCo", A)).unwrap();
        let rx = Receivers::new(sdrs);
        for _ in 0..3 { rx.apply(&db, id); }
        assert!(wait_for(Duration::from_secs(2), || state(&rx, id) == "running"), "{:?}", rx.status(id));
        assert_eq!(open.load(Ordering::SeqCst), 1, "the old decoder released the tuner before the new one took it");
    }

    #[test]
    fn open_error_reported() {
        let db = Db::open_in_memory().unwrap();
        let id = db.save_system(&sys("SGCo", "")).unwrap();
        let rx = Receivers::new(Sdrs { list: Arc::new(|| vec![A.into()]), open: Arc::new(|_, _, _| Err("airspy_open_sn failed with libairspy code -5".into())) });
        rx.restart_all(&db);
        assert!(wait_for(Duration::from_secs(1), || state(&rx, id) == "error"));
        assert_eq!(rx.status(id).unwrap().error.as_deref(), Some("airspy_open_sn failed with libairspy code -5"));
        assert!(wait_for(Duration::from_secs(1), || rx.claimed.lock().unwrap().is_empty()), "a failed open releases the tuner");
    }

    struct Boom;
    impl IqSource for Boom {
        fn next_block(&mut self) -> Option<Vec<i16>> { panic!("simulated decoder fault") }
        fn tune(&mut self, _: u32) -> Result<(), String> { Ok(()) }
        fn dropped(&self) -> u64 { 0 }
        fn rate_hz(&self) -> u32 { 3_000_000 }
    }

    #[test]
    fn decoder_panic_reported() {
        let db = Db::open_in_memory().unwrap();
        let id = db.save_system(&sys("SGCo", "")).unwrap();
        let rx = Receivers::new(Sdrs { list: Arc::new(|| vec![A.into()]), open: Arc::new(|_, _, _| Ok(Box::new(Boom) as Box<dyn IqSource>)) });
        rx.restart_all(&db);
        assert!(wait_for(Duration::from_secs(2), || state(&rx, id) == "error"));
        assert!(rx.status(id).unwrap().error.unwrap().contains("decoder stopped unexpectedly"));
    }

    struct Tunes(Arc<Mutex<Vec<u32>>>);
    impl IqSource for Tunes {
        fn next_block(&mut self) -> Option<Vec<i16>> { Some(Vec::new()) }
        fn tune(&mut self, center_hz: u32) -> Result<(), String> { self.0.lock().unwrap().push(center_hz); Ok(()) }
        fn dropped(&self) -> u64 { 0 }
        fn rate_hz(&self) -> u32 { 2_400_000 }
    }

    #[test]
    fn call_outside_the_window_moves_the_tuner() {
        // KSICS on an RTL-SDR: control 852.1125 MHz, the window starts 50 kHz below it and ends near 853.2 MHz.
        let control = 852_112_500;
        let tunes = Arc::new(Mutex::new(Vec::new()));
        let mut src: Box<dyn IqSource> = Box::new(Tunes(tunes.clone()));
        let base = tuning::Window::for_tuner("rtl:00000001", 2_400_000, 0, None, None);
        let (known, status) = (Mutex::new(BTreeSet::new()), Mutex::new(Status::default()));
        let mut win = place(base, None, control, &[]).unwrap();
        assert_eq!(win.center, center_for(control) as i64);
        let mut cc = ControlChannel::new(win.center as f64, 2.4e6, control, 0.0, 0, Modulation::C4fm).unwrap();
        let mut f = Follow { cc: &mut cc, src: &mut src, win: &mut win, locked: false, control, known: &known, status: &status };
        f.open(852_600_000).unwrap();
        assert!(tunes.lock().unwrap().is_empty(), "inside the window: no move");
        f.open(853_900_000).unwrap();
        let moved = *tunes.lock().unwrap().last().expect("retuned");
        assert!(f.win.covers(control) && f.win.covers(852_600_000) && f.win.covers(853_900_000), "centre {moved}");
        assert_eq!(status.lock().unwrap().center_hz, Some(moved as u64));
        let e = f.open(850_000_000).unwrap_err();
        assert!(e.contains("does not fit"), "{e}");
        assert_eq!(f.cc.voice_freqs(), vec![852_600_000, 853_900_000], "calls in progress kept");

        let mut win = place(base, Some(852_062_500), control, &[]).unwrap();
        let mut f = Follow { cc: &mut cc, src: &mut src, win: &mut win, locked: true, control, known: &known, status: &status };
        assert!(f.open(853_950_000).unwrap_err().contains("locked window"));
        let limited = tuning::Window::for_tuner("rtl:00000001", 2_400_000, 0, None, Some(853_000_000));
        let mut win = place(limited, None, control, &[]).unwrap();
        let mut f = Follow { cc: &mut cc, src: &mut src, win: &mut win, locked: false, control, known: &known, status: &status };
        assert!(f.open(853_475_000).unwrap_err().contains("frequency limits"));
        assert!(place(base, Some(855_000_000), control, &[]).unwrap_err().contains("outside this tuner's locked window"));
    }

    #[test]
    fn known_voice_frequencies_place_the_window() {
        let db = Db::open_in_memory().unwrap();
        let (sdrs, _, tunes) = zeros();
        let id = db.save_system(&sys("SGCo", A)).unwrap();
        db.add_voice_freqs(id, &[861_000_000]).unwrap();
        let rx = Receivers::new(sdrs);
        rx.restart_all(&db);
        assert!(wait_for(Duration::from_secs(2), || state(&rx, id) == "running"));
        let st = rx.status(id).unwrap();
        let center = st.center_hz.unwrap() as i64;
        let w = tuning::Window::for_tuner(A, 3_000_000, center, None, None);
        assert!(w.covers(858_962_500) && w.covers(861_000_000) && center != 858_912_500, "control and the known voice channel: {center}");
        assert_eq!(st.usable_hz, 2_700_000);
        assert_eq!(tunes.lock().unwrap().last().copied(), Some(center as u32), "the tuner moved there");
        rx.get(id).unwrap().known.lock().unwrap().insert(861_100_000);
        rx.record_minute(&db, now_ms());
        assert_eq!(db.voice_freqs(id), vec![861_000_000, 861_100_000], "remembered for the next start");
    }

    #[test]
    fn setup_mode_runs_alone() {
        let db = Db::open_in_memory().unwrap();
        let (sdrs, open, _) = zeros();
        db.save_tuner(A, &TunerConfig { mode: TunerMode::Setup, center_hz: Some(853_000_000), ..TunerConfig::default() }).unwrap();
        let pinned = db.save_system(&sys("KSICS", A)).unwrap();
        let auto = db.save_system(&sys("SGCo", "")).unwrap();
        let rx = Receivers::new(sdrs);
        rx.restart_all(&db);
        assert!(wait_for(Duration::from_secs(2), || state(&rx, auto) == "running" && state(&rx, pinned) == "error"
            && rx.setup_status(A).is_some_and(|s| s.state == "running")));
        assert_eq!(rx.status(pinned).unwrap().error.unwrap(), format!("Airspy {A} is in Setup mode on the Tuners page"));
        assert_eq!(rx.status(auto).unwrap().tuner.as_deref(), Some(B), "Automatic leaves a tuner in Setup mode alone");
        let s = rx.setup_status(A).unwrap();
        assert_eq!((s.center_hz, s.locked), (Some(853_000_000), true));
        let view = rx.tuner_view(A).expect("its spectrum");
        view.lock().unwrap().watch_until_ms = now_ms() + 5000;
        assert!(wait_for(Duration::from_secs(2), || view.lock().unwrap().frame.as_ref().is_some_and(|f| f.center_hz == 853_000_000)));
        assert_eq!(open.load(Ordering::SeqCst), 2);

        db.save_tuner(A, &TunerConfig::default()).unwrap();
        rx.apply_tuner(&db, A);
        assert!(rx.setup_status(A).is_none(), "back to Live: the Setup run stops");
        assert!(wait_for(Duration::from_secs(2), || state(&rx, pinned) == "running"), "and the system pinned to it starts");
    }

    #[test]
    fn failover_retunes() {
        let db = Db::open_in_memory().unwrap();
        let (sdrs, _, tunes) = zeros();
        let id = db.save_system(&System { control_hz: vec![858_962_500, 859_962_500], ..sys("SGCo", "") }).unwrap();
        let rx = Receivers::new(sdrs);
        rx.restart_all(&db);
        let want = center_for(859_962_500) as u32;
        assert!(wait_for(Duration::from_secs(20), || tunes.lock().unwrap().contains(&want)), "tunes {:?}", tunes.lock().unwrap());
        assert!(wait_for(Duration::from_secs(1), || rx.status(id).unwrap().control_hz == Some(859_962_500)));
    }
}
