//! Decoder threads, one per enabled system: each owns its tuner and control channel decoder and publishes status and
//! calls. `Receivers` starts and stops them and hands out tuners (pinned by serial, or Automatic). Tuners in Setup mode
//! get a thread of their own that only feeds the spectrum display.
pub use sdrtrunk_p25::control::center_for;
use crate::db::{Db, Settings, System, TunerConfig, TunerMode};
use crate::event_log::EventLog;
use crate::recorder::{Names, RecordPolicy, Recorder, Tuner};
use crate::streaming::Streams;
use crate::tuning::{self, choose_center};
use serde::Serialize;
use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering as AtomicOrdering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tokio::sync::broadcast;
use crate::quality::{Bucket, Window};
use sdrtrunk_p25::control::{ControlChannel, ControlEvent, FrameCounts, Modulation, VoiceBank};
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
    /// Last second a valid control channel message arrived (`started_ms` until the first), and when the last error began.
    pub last_control_ms: i64,
    pub error_ms: i64,
    /// When the system first failed, carried over the restarts that retry it; None once it runs.
    pub down_since_ms: Option<i64>,
    /// Tuner correction in use (moves with Automatic PPM) and the measured frequency error of the control channel.
    pub ppm: f64,
    pub auto_ppm: bool,
    pub freq_error_hz: Option<f64>,
    /// MP3 files written since the decoder started, and the last recording problem.
    pub recorded: u64,
    pub recording_error: Option<String>,
    /// Helper tuners following calls the main window cannot cover: (tuner id, centre Hz).
    pub helpers: Vec<(String, u64)>,
    /// (sample rate, usable width in Hz) of each of `helpers`, in the same order: what the Tuners page shows as the window.
    pub helper_bands: Vec<(u32, u32)>,
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
/// `next_block` waits up to `wait` (zero never blocks); it returns an empty block when nothing arrived in that time
/// (lets the thread notice a stop) and None at the end.
pub trait IqSource {
    fn next_block(&mut self, wait: Duration) -> Option<Vec<i16>>;
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
    use std::sync::mpsc::{Receiver as Rx, RecvTimeoutError, TryRecvError};
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
        fn next_block(&mut self, wait: Duration) -> Option<Vec<i16>> {
            let rx = self.parts().0;
            let got = if wait.is_zero() {
                rx.try_recv().map_err(|e| match e { TryRecvError::Empty => RecvTimeoutError::Timeout, TryRecvError::Disconnected => RecvTimeoutError::Disconnected })
            } else {
                rx.recv_timeout(wait)
            };
            block_or_end(got, || match &self.0 { Dev::Airspy(a, _) => a.is_streaming(), Dev::Rtl(_) => true })
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

/// What `next_block` gives for one receive: the block; an empty one when none came in time but the device still
/// streams; None when the reader ended or the device stopped streaming (an unplugged Airspy keeps its channel open, so
/// only `streaming` shows it; an RTL-SDR's reader ends).
fn block_or_end(got: Result<Vec<i16>, std::sync::mpsc::RecvTimeoutError>, streaming: impl FnOnce() -> bool) -> Option<Vec<i16>> {
    use std::sync::mpsc::RecvTimeoutError;
    match got {
        Ok(b) => Some(b),
        Err(RecvTimeoutError::Timeout) => streaming().then(Vec::new),
        Err(RecvTimeoutError::Disconnected) => None,
    }
}

#[cfg(not(target_os = "linux"))]
pub fn sdrs() -> Sdrs { Sdrs { list: Arc::new(Vec::new), open: Arc::new(|_, _, _| Err("SDRs are Linux only".into())) } }

/// Records `seconds` of `src` into `w` (sdrtrunk's baseband WAV); returns the complex samples written and those the
/// source dropped. The file is finished whatever happens, so a recording cut short is still a valid WAV.
fn record_baseband<S: Sink>(src: &mut dyn IqSource, seconds: u32, w: S) -> Result<(u64, u64), String> {
    too_long(seconds, src.rate_hz())?;
    let (result, w) = record_to(src, seconds, w);
    // Counted now: the tuner keeps streaming, unread, while the file is flushed (seconds on an SD card).
    let dropped = src.dropped();
    let written = w.finish().map_err(|e| e.to_string())?;
    result.map(|_| (written, dropped))
}

/// The largest WAV sdrtrunk writes or plays as one file (the Java reference's `WaveWriter.MAX_WAVE_SIZE`).
const MAX_WAV_BYTES: u64 = 2 * i32::MAX as u64;

/// Most whole seconds one recording can hold at `rate_hz` (4 bytes per complex sample, 44-byte header).
pub fn max_record_seconds(rate_hz: u32) -> u32 { ((MAX_WAV_BYTES - 44) / (rate_hz as u64 * 4)) as u32 }

fn too_long(seconds: u32, rate_hz: u32) -> Result<(), String> {
    let max = max_record_seconds(rate_hz);
    if seconds <= max { Ok(()) } else {
        Err(format!("one recording holds at most {max} s at {rate_hz} samples/s (sdrtrunk plays WAV files up to 4 GB)"))
    }
}

/// Where a recording goes: samples on the writer thread, then the final flush. A WAV file, or a stand-in in tests.
trait Sink: Send + 'static {
    fn put(&mut self, iq: &[i16]) -> std::io::Result<()>;
    /// Completes the recording; returns the complex samples it holds.
    fn finish(self) -> std::io::Result<u64>;
}

impl Sink for sdrtrunk_source::wav::WavWriter {
    fn put(&mut self, iq: &[i16]) -> std::io::Result<()> { self.write(iq) }
    fn finish(self) -> std::io::Result<u64> { sdrtrunk_source::wav::WavWriter::finish(self) }
}

/// Blocks waiting for the disk: an SD card can stall for seconds, far longer than a tuner's own queue (64 blocks,
/// under a second at 6 MS/s); 2048 Airspy blocks are about 23 s and 512 MB.
const WRITE_QUEUE_BLOCKS: usize = 2048;

/// Takes `seconds` of `src` and hands them to `sink` on a writer thread, so a slow disk never stops the tuner being
/// read. Returns the samples taken (or why it stopped) and the sink.
fn record_to<S: Sink>(src: &mut dyn IqSource, seconds: u32, sink: S) -> (Result<u64, String>, S) {
    let (tx, rx) = std::sync::mpsc::sync_channel::<Vec<i16>>(WRITE_QUEUE_BLOCKS);
    let writer = std::thread::spawn(move || {
        let (mut sink, mut err) = (sink, None);
        for b in rx {
            if err.is_none() { err = sink.put(&b).err(); }
        }
        (sink, err)
    });
    let want = seconds as u64 * src.rate_hz() as u64;
    let (mut have, mut last) = (0u64, Instant::now());
    let read = loop {
        if have >= want { break Ok(have); }
        match src.next_block(Duration::from_millis(500)) {
            None => break Err(format!("the tuner ended after {have} samples")),
            Some(b) if b.is_empty() => {
                if last.elapsed() > Duration::from_secs(5) { break Err(format!("no samples for 5 s after {have}")); }
            }
            Some(mut b) => {
                b.truncate(((want - have) as usize * 2).min(b.len() / 2 * 2));
                have += b.len() as u64 / 2;
                last = Instant::now();
                if tx.send(b).is_err() { break Err("the writer stopped".into()); }
            }
        }
    };
    drop(tx);
    let (sink, err) = writer.join().unwrap_or_else(|_| panic!("recording writer panicked"));
    match (read, err) {
        (Err(e), _) => (Err(e), sink),
        (Ok(_), Some(e)) => (Err(e.to_string()), sink),
        (Ok(n), None) => (Ok(n), sink),
    }
}

/// `sdrtrunk-rust record`: opens SDR `id` with its saved settings from database `db` at `center` Hz and records `seconds`
/// into `out` under sdrtrunk's baseband name. Returns the file, the samples written and the samples dropped.
pub fn record_tuner(db: &Path, id: &str, center: u32, seconds: u32, out: &Path) -> Result<(std::path::PathBuf, u64, u64), String> {
    let id = sdrtrunk_source::normalize_tuner(id).ok_or_else(|| format!("{id} is not an SDR id"))?;
    let cfg = Db::open(db).map_err(|e| format!("{}: {e}", db.display()))?.tuner(&id);
    let mut src = (sdrs().open)(&cfg, center, &id)?;
    too_long(seconds, src.rate_hz())?;
    let stamp = chrono::Local::now().format("%Y%m%d_%H%M%S").to_string();
    let name = sdrtrunk_source::wav::baseband_file_name(&sdrtrunk_source::tuner_label(&id).replace(' ', "_"), center, &stamp);
    let path = out.join(name);
    let w = sdrtrunk_source::wav::WavWriter::create(&path, src.rate_hz()).map_err(|e| format!("{}: {e}", path.display()))?;
    let (written, dropped) = record_baseband(src.as_mut(), seconds, w)?;
    Ok((path, written, dropped))
}

/// The baseband recordings in `dir` (sdrtrunk's `NAME_HZ_baseband_yyyyMMdd_HHmmss.wav`) as tuners "file:NAME", played
/// in real time and looping. A recording only opens and tunes at the centre it was recorded at.
pub fn replay_sdrs(dir: &Path) -> Result<Sdrs, String> {
    use sdrtrunk_source::{replay::Replay, wav::center_from_name, FILE_PREFIX};
    let entries = std::fs::read_dir(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    let mut files: BTreeMap<String, (std::path::PathBuf, u32)> = BTreeMap::new();
    for path in entries.filter_map(|e| e.ok().map(|e| e.path())) {
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else { continue };
        let Some(center) = center_from_name(name) else { continue };
        let stem = &name[..name.len() - 4];
        files.insert(format!("{FILE_PREFIX}{stem}"), (path.clone(), center));
    }
    if files.is_empty() { return Err(format!("{}: no baseband recordings (NAME_HZ_baseband_yyyyMMdd_HHmmss.wav)", dir.display())); }
    struct Playing { r: Replay, label: String }
    fn at_centre(label: &str, recorded: u32, asked: u32) -> Result<(), String> {
        if asked == recorded { Ok(()) } else { Err(format!("{label} was recorded at {recorded} Hz; it cannot tune to {asked} Hz")) }
    }
    impl IqSource for Playing {
        fn next_block(&mut self, wait: Duration) -> Option<Vec<i16>> {
            let got = if wait.is_zero() {
                self.r.blocks.try_recv().map_err(|e| match e {
                    std::sync::mpsc::TryRecvError::Empty => std::sync::mpsc::RecvTimeoutError::Timeout,
                    std::sync::mpsc::TryRecvError::Disconnected => std::sync::mpsc::RecvTimeoutError::Disconnected,
                })
            } else {
                self.r.blocks.recv_timeout(wait)
            };
            block_or_end(got, || true)
        }
        fn tune(&mut self, center_hz: u32) -> Result<(), String> { at_centre(&self.label, self.r.center_hz, center_hz) }
        fn dropped(&self) -> u64 { self.r.dropped.load(AtomicOrdering::Relaxed) }
        fn rate_hz(&self) -> u32 { self.r.rate_hz }
    }
    let files = Arc::new(files);
    let ids: Vec<String> = files.keys().cloned().collect();
    Ok(Sdrs {
        list: Arc::new(move || ids.clone()),
        open: Arc::new(move |_: &TunerConfig, center: u32, id: &str| {
            let label = sdrtrunk_source::tuner_label(id);
            let (path, recorded) = files.get(id).ok_or_else(|| format!("{label} not found"))?;
            at_centre(&label, *recorded, center)?;
            let r = Replay::open(path).map_err(|e| format!("{label}: {e}"))?;
            Ok(Box::new(Playing { r, label }) as Box<dyn IqSource>)
        }),
    })
}

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

/// System `s`'s main tuner by `pick_tuner`'s rules, among the connected SDRs (`present`) in Live mode; a pinned tuner in
/// Setup mode or switched off is an error.
fn main_tuner(s: &System, present: &[String], pool: &Pool, claimed: &HashSet<String>) -> Result<String, String> {
    if let Some(pin) = sdrtrunk_source::normalize_tuner(&s.tuner) {
        let label = sdrtrunk_source::tuner_label(&pin);
        match pool.config(&pin).mode {
            TunerMode::Live => {}
            TunerMode::Setup => return Err(format!("{label} is in Setup mode on the Tuners page")),
            TunerMode::Disabled => return Err(format!("{label} is switched off on the Tuners page")),
        }
    }
    let live: Vec<String> = present.iter().filter(|t| pool.config(t).mode == TunerMode::Live).cloned().collect();
    pick_tuner(&s.tuner, &live, claimed, &pool.reserved(s.id))
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
    /// The spectrum display of each helper tuner now open (tuner id, view), for `Receivers::tuner_view`.
    helper_views: HelperViews,
    /// Helper tuners `Receivers` asks the decoder to give up.
    release: Arc<Release>,
    /// The main tuner `Receivers` handed it at start (None: it got none).
    main: Option<String>,
    run: Run,
}

type HelperViews = Arc<Mutex<Vec<(String, Arc<Mutex<SpectrumView>>)>>>;

/// What the decoder thread shares with the rest of the program.
#[derive(Clone)]
struct Shared {
    status: Arc<Mutex<Status>>,
    calls: broadcast::Sender<CallEvent>,
    policy: Arc<Mutex<RecordPolicy>>,
    spectrum: Arc<Mutex<SpectrumView>>,
    quality: Arc<Mutex<Window>>,
    known: Arc<Mutex<BTreeSet<u64>>>,
    helper_views: HelperViews,
    streams: Arc<Streams>,
    release: Arc<Release>,
}

/// Tuners a decoder is asked to give up as helpers (an owner's save): `asked` is read every pass, `ids` only when set.
#[derive(Default)]
struct Release { asked: AtomicBool, ids: Mutex<Vec<String>> }

impl Receiver {
    #[allow(clippy::too_many_arguments)]
    fn start(system: System, settings: Settings, pool: SharedPool, sdrs: Sdrs, claimed: Claimed,
             calls: broadcast::Sender<CallEvent>, policy: RecordPolicy, known: Vec<u64>, streams: Arc<Streams>,
             pick: Result<String, String>) -> Receiver {
        let main = pick.as_ref().ok().cloned();
        let shared = Shared {
            status: Arc::new(Mutex::new(Status {
                state: "starting".into(), control_hz: system.control_hz.first().copied(), started_ms: now_ms(), last_control_ms: now_ms(), ..Status::default()
            })),
            calls,
            policy: Arc::new(Mutex::new(policy)),
            spectrum: Arc::default(),
            quality: Arc::default(),
            known: Arc::new(Mutex::new(known.into_iter().collect())),
            helper_views: Arc::default(),
            streams,
            release: Arc::default(),
        };
        let stop = Arc::new(AtomicBool::new(false));
        let (s, sh, flag) = (system.clone(), shared.clone(), stop.clone());
        let handle = std::thread::Builder::new().name(format!("decoder-{}", system.id))
            .spawn(move || decode(s, settings, pool, sdrs, claimed, flag, sh, pick)).expect("spawn decoder");
        let Shared { status, policy, spectrum, quality, known, helper_views, release, .. } = shared;
        Receiver { system, status, policy, spectrum, quality, known, helper_views, release, main, run: Mutex::new(Some((stop, handle))) }
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
        let status = Arc::new(Mutex::new(Status { state: "starting".into(), started_ms: now_ms(), last_control_ms: now_ms(), ..Status::default() }));
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

/// What decoders read when they look for a helper, kept current by `Receivers`: the SDRs the last scan saw, the
/// tuner settings as saved and the tuners pinned by enabled systems (system id, tuner id).
#[derive(Default)]
struct Pool { seen: Vec<String>, tuners: Vec<(String, TunerConfig)>, pins: Vec<(i64, String)> }

type SharedPool = Arc<Mutex<Pool>>;

impl Pool {
    fn config(&self, id: &str) -> TunerConfig { self.tuners.iter().find(|(t, _)| t == id).map(|(_, c)| c.clone()).unwrap_or_default() }

    /// Tuners pinned to enabled systems other than `me`.
    fn reserved(&self, me: i64) -> Vec<String> { self.pins.iter().filter(|(s, _)| *s != me).map(|(_, t)| t.clone()).collect() }

    /// Claims tuner `serial` as a helper of system `me`, checking under the pool lock that it is still Live and not
    /// pinned to another enabled system: a save may have changed either since `free` listed it.
    fn claim_helper(&self, claimed: &Claimed, me: i64, serial: &str) -> Result<Claim, String> {
        let label = sdrtrunk_source::tuner_label(serial);
        if self.config(serial).mode != TunerMode::Live { return Err(format!("{label} is not in Live mode")); }
        if self.reserved(me).iter().any(|t| t == serial) { return Err(format!("{label} is pinned to another system")); }
        if !claimed.lock().unwrap_or_else(|p| p.into_inner()).insert(serial.to_string()) { return Err(format!("{label} is in use")); }
        Ok(Claim(claimed.clone(), serial.to_string()))
    }

    /// Tuners system `me` may take as helpers now (no USB work: the list is the last scan's).
    fn free(&self, claimed: &Claimed, me: i64) -> Vec<String> {
        free_tuners(self.seen.clone(), &|t| self.config(t), &claimed.lock().unwrap_or_else(|p| p.into_inner()), &self.reserved(me))
    }
}

/// The running systems, by system id, and the tuners in Setup mode, by tuner id.
pub struct Receivers {
    sdrs: Sdrs,
    claimed: Claimed,
    pool: SharedPool,
    /// Connected tuners nobody held at the last `rescan` (None before the first).
    last_free: Mutex<Option<Vec<String>>>,
    /// Grants from every system.
    pub calls: broadcast::Sender<CallEvent>,
    running: Mutex<BTreeMap<i64, Arc<Receiver>>>,
    setups: Mutex<BTreeMap<String, Arc<SetupRun>>>,
    /// The Icecast streams; every system hands them the calls they carry.
    pub streams: Arc<Streams>,
    /// When each system was restarted after it failed holding a tuner (the last `health::FLAP_WINDOW_MS`).
    failed: Mutex<std::collections::HashMap<i64, std::collections::VecDeque<i64>>>,
}

fn policy_for(db: &Db, system: &str) -> RecordPolicy {
    let mut streamed: Vec<u16> = db.streams().into_iter().filter(|s| s.enabled).flat_map(|s| s.members)
        .filter(|m| m.list == system).map(|m| m.group).collect();
    streamed.sort_unstable();
    streamed.dedup();
    RecordPolicy { record_unlisted: db.record_unlisted(system), talkgroups: db.talkgroups(system), streamed,
        radios: db.radio_names(system).into_iter().map(|r| (r.radio, r.name)).collect() }
}

impl Receivers {
    pub fn new(sdrs: Sdrs) -> Receivers {
        let mut seen = (sdrs.list)();
        seen.sort();
        Receivers {
            pool: Arc::new(Mutex::new(Pool { seen, ..Pool::default() })), last_free: Mutex::new(None), sdrs, claimed: Arc::default(), calls: broadcast::channel(256).0, running: Mutex::new(BTreeMap::new()),
            setups: Mutex::new(BTreeMap::new()), streams: Arc::new(Streams::new(std::time::Duration::from_secs(30))),
            failed: Mutex::default(),
        }
    }

    /// Starts `systems`. Their main tuners are handed out here, in id order under the `claimed` lock, so when several
    /// want the same SDR the lowest-numbered system gets it (helpers stay first come, first served).
    fn start_many<'a>(&self, db: &Db, systems: impl Iterator<Item = &'a System>, run: &mut BTreeMap<i64, Arc<Receiver>>) {
        let mut systems: Vec<&System> = systems.collect();
        if systems.is_empty() { return; }
        systems.sort_by_key(|s| s.id);
        let present = (self.sdrs.list)();
        let picks: Vec<Result<String, String>> = {
            let pool = self.pool.lock().unwrap_or_else(|p| p.into_inner());
            let mut claimed = self.claimed.lock().unwrap_or_else(|p| p.into_inner());
            systems.iter().map(|s| {
                let pick = main_tuner(s, &present, &pool, &claimed);
                if let Ok(t) = &pick { claimed.insert(t.clone()); }
                pick
            }).collect()
        };
        for (s, pick) in systems.into_iter().zip(picks) {
            run.insert(s.id, Arc::new(Receiver::start(s.clone(), db.settings(), self.pool.clone(), self.sdrs.clone(),
                self.claimed.clone(), self.calls.clone(), policy_for(db, s.list_name()), db.voice_freqs(s.id), self.streams.clone(), pick)));
        }
    }

    /// Asks every running system holding one of `tuners` as a helper to give it up (its calls on it end; the system
    /// keeps running) and waits up to 2 s, without holding `running`, until every decoder asked has done so and the
    /// tuners no decoder may take again (not Live, or pinned) are free. A Live, unpinned one may be taken again at
    /// once by a decoder following a call, with its new settings. Tuners held otherwise (the main tuner of a system
    /// not in error, a Setup run) are left alone. Every decoder not in error is asked, not just those whose status
    /// lists the tuner: one may be opening it right now. Failed decoders have no thread left to ask.
    fn release_helpers(&self, tuners: &[String]) {
        let live: Vec<Arc<Receiver>> = self.running.lock().unwrap_or_else(|p| p.into_inner()).values()
            .filter(|r| r.state() != "error").cloned().collect();
        let held: Vec<String> = {
            let setups = self.setups.lock().unwrap_or_else(|p| p.into_inner());
            let claimed = self.claimed.lock().unwrap_or_else(|p| p.into_inner());
            tuners.iter().filter(|t| claimed.contains(*t) && !setups.contains_key(*t) && !live.iter().any(|r| r.main.as_ref() == Some(*t)))
                .cloned().collect()
        };
        if held.is_empty() { return; }
        let must_free: Vec<String> = {
            let p = self.pool.lock().unwrap_or_else(|p| p.into_inner());
            held.iter().filter(|t| p.config(t).mode != TunerMode::Live || p.pins.iter().any(|(_, pin)| pin == *t)).cloned().collect()
        };
        for r in &live {
            r.release.ids.lock().unwrap_or_else(|p| p.into_inner()).extend(held.iter().cloned());
            r.release.asked.store(true, AtomicOrdering::Release);
        }
        let since = Instant::now();
        while since.elapsed() < Duration::from_secs(2) {
            let handled = live.iter().all(|r| !r.release.asked.load(AtomicOrdering::Acquire) || matches!(r.state().as_str(), "error" | "stopped"));
            if handled && !must_free.iter().any(|t| self.claimed.lock().unwrap_or_else(|p| p.into_inner()).contains(t)) { return; }
            std::thread::sleep(Duration::from_millis(5));
        }
        eprintln!("receivers: helper tuners {held:?} not released within 2 s");
    }

    /// Reloads the tuner settings and pins every decoder reads.
    fn refresh(&self, db: &Db, all: &[System]) {
        let mut p = self.pool.lock().unwrap_or_else(|p| p.into_inner());
        p.tuners = db.tuners();
        p.pins = all.iter().filter(|s| s.enabled).filter_map(|s| sdrtrunk_source::normalize_tuner(&s.tuner).map(|t| (s.id, t))).collect();
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
        self.refresh(db, &all);
        self.start_many(db, all.iter().filter(|s| s.enabled), &mut run);
        drop(run);
        for (id, _) in db.tuners().into_iter().filter(|(_, c)| c.mode == TunerMode::Setup) { self.apply_setup(db, &id); }
    }

    /// After system `id` was saved or deleted: restarts it as stored (stops it if deleted or disabled), then
    /// retries systems that failed, since a tuner may have come free.
    pub fn apply(&self, db: &Db, id: i64) { self.restart(db, &[id]); }

    /// Restarts systems `ids` as stored (stops those deleted or disabled) together with every system that failed (a
    /// tuner may have come free). A tuner now pinned to a system is first given up by any system holding it as a
    /// helper (that system keeps running; the pinned system gets it). All restarted systems stop before any starts,
    /// so freed tuners are free for the starts.
    fn restart(&self, db: &Db, ids: &[i64]) {
        let all = db.systems();
        self.refresh(db, &all);
        let pinned: Vec<String> = all.iter().filter(|s| s.enabled).filter_map(|s| sdrtrunk_source::normalize_tuner(&s.tuner)).collect();
        self.release_helpers(&pinned);
        let mut run = self.running.lock().unwrap_or_else(|p| p.into_inner());
        let mut again: Vec<i64> = ids.to_vec();
        again.extend(run.iter().filter(|(sys, r)| !ids.contains(sys) && r.state() == "error").map(|(&sys, _)| sys));
        // A failed system stays down since its first failure across the restarts that retry it (see `health`).
        let mut down: Vec<(i64, i64)> = Vec::new();
        for sys in &again {
            if let Some(r) = run.remove(sys) {
                let st = r.status.lock().unwrap_or_else(|p| p.into_inner()).clone();
                if st.state == "error" {
                    down.push((*sys, st.down_since_ms.unwrap_or(st.error_ms)));
                    // It held a tuner and failed: a drop. One whose last restart found no tuner did not drop again.
                    if r.main.is_some() { self.note_drop(*sys); }
                }
                r.stop();
            }
        }
        self.start_many(db, all.iter().filter(|s| s.enabled && again.contains(&s.id)), &mut run);
        for (sys, since) in down {
            if let Some(r) = run.get(&sys) { update(&r.status, |st| st.down_since_ms = Some(since)); }
        }
    }

    /// After tuner `id`'s settings changed: stops its Setup run, releases it if a system holds it as a helper (that
    /// system keeps running), restarts the system using it as main tuner (it picks up the new settings, or another
    /// tuner if it left Live mode), retries systems that failed (it may be Live again), then starts its Setup run if it
    /// is in Setup mode.
    pub fn apply_tuner(&self, db: &Db, id: &str) {
        if let Some(r) = self.setups.lock().unwrap_or_else(|p| p.into_inner()).remove(id) { r.stop(); }
        // Before anything else: from here no decoder takes it as a helper under its old mode.
        self.refresh(db, &db.systems());
        self.release_helpers(&[id.to_string()]);
        // A failed system's `main` is stale (its claim is gone); `restart` retries it anyway.
        let main: Vec<i64> = self.running.lock().unwrap_or_else(|p| p.into_inner()).iter()
            .filter(|(_, r)| r.main.as_deref() == Some(id) && r.state() != "error").map(|(&sys, _)| sys).collect();
        self.restart(db, &main);
        self.apply_setup(db, id);
    }

    /// Device scan interval: how often `rescan` runs, in ms.
    pub const SCAN_MS: u64 = 5_000;

    /// Compares the connected SDRs, and those nobody holds, with the last scan. When either changed, retries systems
    /// that failed (an SDR may be free: plugged in, or a helper or a stopped system let it go); newly connected tuners
    /// in Setup mode get their Setup run. Otherwise does nothing. Running systems read this scan's list when they
    /// look for a helper.
    pub fn rescan(&self, db: &Db) {
        let mut now = (self.sdrs.list)();
        now.sort();
        let free: Vec<String> = {
            let claimed = self.claimed.lock().unwrap_or_else(|p| p.into_inner());
            now.iter().filter(|t| !claimed.contains(*t)).cloned().collect()
        };
        let freed = {
            let mut last = self.last_free.lock().unwrap_or_else(|p| p.into_inner());
            let changed = last.as_ref().is_some_and(|l| *l != free);
            *last = Some(free);
            changed
        };
        let added: Vec<String> = {
            let mut p = self.pool.lock().unwrap_or_else(|p| p.into_inner());
            let added = now.iter().filter(|t| !p.seen.contains(t)).cloned().collect();
            if p.seen == now && !freed { return; }
            p.seen = now;
            added
        };
        // `restart` retries failed systems and counts their drops.
        if self.running.lock().unwrap_or_else(|p| p.into_inner()).values().any(|r| r.state() == "error") { self.restart(db, &[]); }
        for id in added { self.apply_setup(db, &id); }
    }

    /// Tuner `id`'s Setup run status, while it is in Setup mode.
    pub fn setup_status(&self, id: &str) -> Option<Status> {
        self.setups.lock().unwrap_or_else(|p| p.into_inner()).get(id).map(|r| r.status.lock().unwrap_or_else(|p| p.into_inner()).clone())
    }

    /// The spectrum display of tuner `id`: the system using it (as main tuner or helper), or its Setup run.
    pub fn tuner_view(&self, id: &str) -> Option<Arc<Mutex<SpectrumView>>> {
        self.on_tuner(id).map(|r| r.spectrum.clone())
            .or_else(|| self.running.lock().unwrap_or_else(|p| p.into_inner()).values().find_map(|r|
                r.helper_views.lock().unwrap_or_else(|p| p.into_inner()).iter().find(|(t, _)| t == id).map(|(_, v)| v.clone())))
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

    /// Radio `radio`'s name in alias list `list`, from a running system's policy (no database read; per call).
    pub fn radio_name(&self, list: &str, radio: u32) -> Option<String> {
        self.running.lock().unwrap_or_else(|p| p.into_inner()).values().filter(|r| r.system.list_name() == list)
            .find_map(|r| r.policy.lock().unwrap_or_else(|p| p.into_inner()).radios.get(&radio).cloned())
    }

    /// The running system using tuner `id`, if any.
    pub fn on_tuner(&self, id: &str) -> Option<Arc<Receiver>> {
        self.running.lock().unwrap_or_else(|p| p.into_inner()).values()
            .find(|r| r.status.lock().unwrap_or_else(|p| p.into_inner()).tuner.as_deref() == Some(id)).cloned()
    }

    /// Notes that system `id` lost the tuner it held (for `failures`).
    fn note_drop(&self, id: i64) {
        let now = now_ms();
        let mut f = self.failed.lock().unwrap_or_else(|p| p.into_inner());
        let times = f.entry(id).or_default();
        times.push_back(now);
        while times.front().is_some_and(|&t| now - t > crate::health::FLAP_WINDOW_MS) { times.pop_front(); }
    }

    /// How often system `id` was restarted after it failed holding a tuner, since `since_ms` (up to `health::FLAP_WINDOW_MS` back).
    pub fn failures(&self, id: i64, since_ms: i64) -> usize {
        self.failed.lock().unwrap_or_else(|p| p.into_inner()).get(&id).map_or(0, |t| t.iter().filter(|&&t| t >= since_ms).count())
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
    update(status, |s| { s.state = "error".into(); s.error = Some(msg); s.error_ms = now_ms(); });
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

/// When the decoder ends however it ends, its status lists no helpers and no helper spectrum is offered.
struct HelperGuard(Arc<Mutex<Status>>, HelperViews);

impl Drop for HelperGuard {
    fn drop(&mut self) {
        update(&self.0, |st| (st.helpers, st.helper_bands) = (Vec::new(), Vec::new()));
        self.1.lock().unwrap_or_else(|p| p.into_inner()).clear();
    }
}

/// A tuner held by a decoder thread; released when the thread ends, however it ends.
struct Claim(Claimed, String);

impl Drop for Claim {
    fn drop(&mut self) { self.0.lock().unwrap_or_else(|p| p.into_inner()).remove(&self.1); }
}

/// Runs system `s` on main tuner `pick` (already claimed by `Receivers::start_many`, or why it got none).
#[allow(clippy::too_many_arguments)]
fn decode(s: System, settings: Settings, pool: SharedPool, sdrs: Sdrs, claimed: Claimed, stop: Arc<AtomicBool>, sh: Shared, pick: Result<String, String>) {
    // First, so the claim is released however this ends.
    let pick = pick.map(|sn| Claim(claimed.clone(), sn));
    let Shared { status, calls, policy, spectrum, quality, known, helper_views, streams, release } = sh;
    let _guard = PanicGuard(status.clone());
    let _helpers_guard = HelperGuard(status.clone(), helper_views.clone());
    let _claim = match pick { Ok(c) => c, Err(e) => return fail(&status, e) };
    let serial = _claim.1.clone();
    // The main tuner's settings as at start (a change restarts the system).
    let tuner = pool.lock().unwrap_or_else(|p| p.into_inner()).config(&serial);
    let mut idx = 0;
    let freq = |i: usize| s.control_hz[i] as i64;
    let modulation = Modulation::parse(&s.modulation).unwrap_or_default();
    let names = Names { system: s.system_name.clone(), site: s.site_name.clone(), channel: s.channel_name.clone() };
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
    let mut log = EventLog::new(settings.event_log_path(), &s.system_name, policy.clone(), now_ms() as u64, settings.event_log_days);
    let mut rec = Recorder::new(Path::new(&settings.recording_dir), names, policy.clone()).streaming(streams.clone(), s.list_name().to_string());
    update(&status, |st| { st.state = "running".into(); st.down_since_ms = None; });
    let (mut tick, mut tick_valid, mut tick_frames) = (Instant::now(), 0u64, FrameCounts::default());
    let mut last_correction_ms = now_ms();
    let mut events = Vec::new();
    let mut fft = sdrtrunk_dsp::spectrum::Spectrum::new(SPECTRUM_FFT, SPECTRUM_WIDTH);
    let (mut last_frame_ms, mut seq) = (0i64, 0u64);
    let (mut last_symbols_ms, mut symbols_seq) = (0i64, 0u64);
    let mut channel_fft = sdrtrunk_dsp::spectrum::Spectrum::new(CHANNEL_FFT, CHANNEL_WIDTH);
    let (mut last_channel_ms, mut channel_seq) = (0i64, 0u64);
    let mut helpers: Vec<Helper> = Vec::new();
    let mut shown_helpers: Vec<(String, u64)> = Vec::new();
    let mut lost = Vec::new();
    let free = || pool.lock().unwrap_or_else(|p| p.into_inner()).free(&claimed, s.id);
    let mut opener = |serial: &str, required: &[i64], known: &[i64], id_base: u32| {
        let cfg = pool.lock().unwrap_or_else(|p| p.into_inner()).config(serial);
        let claim = || pool.lock().unwrap_or_else(|p| p.into_inner()).claim_helper(&claimed, s.id, serial);
        open_helper(serial, &cfg, &sdrs.open, &claim, required, known, modulation, id_base)
    };

    // Known voice frequencies the main window leaves out: one helper, placed to cover as many as it can (more come on demand).
    // Nothing is required: `choose_center` places the window over as many left-out frequencies as the tuner's limits allow.
    let known_all = known_now();
    let left_out: Vec<i64> = known_all.iter().copied().filter(|&hz| !win.covers(hz)).collect();
    if !left_out.is_empty() {
        for serial in free() {
            match opener(&serial, &[], &left_out, free_id_base(&helpers)) {
                Ok(h) => { helpers.push(h); break; }
                Err(e) => eprintln!("decoder {}: no helper at start: {e}", s.label()),
            }
        }
    }

    while !stop.load(AtomicOrdering::Relaxed) {
        // With helpers, no source may hold up the others: read all without waiting, and rest only when none had data.
        let wait = if helpers.is_empty() { Duration::from_millis(200) } else { Duration::ZERO };
        // An error, not "stopped": `rescan` retries the system once the SDR is back (or another is free).
        let Some(block) = src.next_block(wait) else { return fail(&status, "SDR stopped delivering samples (unplugged?)".into()) };
        cc.process(&block, &mut |e| events.push(e));
        let helper_data = read_helpers(&mut helpers, &mut events, &mut lost);
        if release.asked.load(AtomicOrdering::Acquire) {
            // `asked` is cleared only once the helpers are gone, holding `ids` so no new request slips in between.
            let mut ids = release.ids.lock().unwrap_or_else(|p| p.into_inner());
            drop_helpers(&mut helpers, &ids, &mut lost);
            ids.clear();
            release.asked.store(false, AtomicOrdering::Release);
        }
        // Before this pass's grants, so one on a lost channel's frequency opens a fresh channel.
        for id in lost.drain(..) { rec.drop_channel(id); }
        if wait.is_zero() && block.is_empty() && !helper_data { std::thread::sleep(Duration::from_millis(5)); }
        if let Some(hz) = cc.take_frequency_error() { corr.measure(hz); }
        let now = now_ms();
        if now - last_correction_ms >= TICK_MS {
            last_correction_ms = now;
            if corr.tick(freq(idx) as f64) { cc.set_tuning(corr.ppm, corr.channel_hz); }
            update(&status, |st| { st.ppm = corr.ppm; st.freq_error_hz = corr.measured_hz; });
        }
        {
            let mut follow = Follow { cc: &mut cc, src: &mut src, win: &mut win, locked: tuner.center_hz.is_some(), control: freq(idx),
                known: &known, status: &status, helpers: &mut helpers, free: &free, opener: &mut opener };
            for e in events.drain(..) {
                match &e {
                    ControlEvent::Site { nac, wacn, system } => update(&status, |st| st.site = Some((*nac, *wacn, *system))),
                    ControlEvent::Call(g) => {
                        known.lock().unwrap_or_else(|p| p.into_inner()).insert(g.hz);
                        // No subscribers is normal (no page open).
                        let _ = calls.send(CallEvent { system: s.system_name.clone(), alias_list: s.list_name().to_string(), at_ms: now, group: g.group, source: g.source, hz: g.hz,
                            encrypted: g.encrypted, emergency: g.emergency });
                        log.grant(g, now);
                    }
                    ControlEvent::Voice { .. } | ControlEvent::VoiceEnd { .. } => {}
                }
                rec.handle(&e, now, &mut follow);
            }
            rec.tick(now, &mut follow);
            log.live(&rec.live(), now);
            log.tick(now);
        }
        release_idle(&mut helpers, now);
        let shown: Vec<(String, u64)> = helpers.iter().map(|h| (h.serial.clone(), h.win.center as u64)).collect();
        if shown != shown_helpers {
            update(&status, |st| {
                st.helpers = shown.clone();
                st.helper_bands = helpers.iter().map(|h| (h.src.rate_hz(), h.win.usable as u32)).collect();
            });
            *helper_views.lock().unwrap_or_else(|p| p.into_inner()) = helpers.iter().map(|h| (h.serial.clone(), h.view.clone())).collect();
            shown_helpers = shown;
        }
        draw_helpers(&mut helpers, || rec.following(), now);
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
            markers.extend(rec.following().into_iter().map(voice_marker));
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
                if rate > 0.0 { st.last_control_ms = now; }
                st.dropped = src.dropped();
                st.recorded = rec.recorded;
                st.recording_error = rec.last_error.clone().or_else(|| log.last_error());
            });
            (tick, tick_valid) = (Instant::now(), cc.valid_tsbk());
        }
        if s.control_hz.len() > 1 && failover_due(cc.last_valid_sample(), 0, cc.samples(), rate, s.failover_ms) {
            idx = (idx + 1) % s.control_hz.len();
            eprintln!("decoder {}: no control channel for {} ms, trying {} Hz", s.label(), s.failover_ms, freq(idx));
            win = match place(base, tuner.center_hz, freq(idx), &known_now()) { Ok(w) => w, Err(e) => return fail(&status, e) };
            if let Err(e) = src.tune(win.center as u32) { return fail(&status, e); }
            let next = match channel(idx, corr.ppm, win.center) { Ok(c) => c, Err(e) => return fail(&status, e) };
            switch_control(&mut cc, next, &mut rec);
            corr.reset_channel();
            tick_frames = FrameCounts::default();
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

/// The marker for a followed call: (Hz, talkgroup, name if listed, encrypted).
fn voice_marker((hz, tg, name, encrypted): (u64, u16, Option<String>, bool)) -> Marker {
    Marker { hz, kind: if encrypted { "encrypted" } else { "voice" }, label: name.map_or(format!("TG {tg}"), |n| format!("{tg} {n}")) }
}

/// A helper is released after this long without a voice channel.
const HELPER_IDLE_MS: i64 = 600_000;

/// A free Live tuner following voice channels the main tuner's window cannot cover. Its ids start at its `id_base`
/// (`(k + 1) << 24`), so they never clash with the main tuner's or another helper's.
struct Helper {
    serial: String,
    src: Box<dyn IqSource>,
    bank: VoiceBank,
    win: tuning::Window,
    locked: bool,
    /// Dropped after `src`, so the tuner is closed before another system can take it.
    _claim: Claim,
    /// When it was first seen without a voice channel.
    idle_since_ms: Option<i64>,
    id_base: u32,
    /// Its spectrum display, drawn from `last` (the newest block read) only while a page watches.
    view: Arc<Mutex<SpectrumView>>,
    fft: sdrtrunk_dsp::spectrum::Spectrum,
    last: Vec<i16>,
    last_frame_ms: i64,
    seq: u64,
}

/// The lowest helper id base, `(k + 1) << 24`, that no live helper uses (a released helper's base is reused).
fn free_id_base(helpers: &[Helper]) -> u32 {
    (1..=255u32).map(|k| k << 24).find(|b| helpers.iter().all(|h| h.id_base != *b)).expect("fewer than 255 helpers")
}

/// After a control channel failover: the main tuner's voice channels go with the old control channel (the recorder
/// forgets them); helpers and their calls carry on.
fn switch_control(cc: &mut ControlChannel, next: ControlChannel, rec: &mut Recorder) {
    for id in cc.voice_ids() { rec.drop_channel(id); }
    *cc = next;
}

/// Tuners a system may take as helpers: connected (`present`), Live, not in use, not pinned to another enabled system.
fn free_tuners(present: Vec<String>, config: &dyn Fn(&str) -> TunerConfig, claimed: &HashSet<String>, reserved: &[String]) -> Vec<String> {
    let reserved: Vec<String> = reserved.iter().filter_map(|r| sdrtrunk_source::normalize_tuner(r)).collect();
    present.into_iter().filter(|t| config(t).mode == TunerMode::Live && !claimed.contains(t) && !reserved.contains(t)).collect()
}

/// Claims tuner `serial` (with `claim`) and opens it as a helper whose window covers every `required` channel (and as
/// many `known` as it can), or at its locked centre if that covers them. Uses the tuner's saved PPM (no control channel
/// to measure).
#[allow(clippy::too_many_arguments)]
fn open_helper(serial: &str, cfg: &TunerConfig, open: &SourceFactory, claim: &dyn Fn() -> Result<Claim, String>, required: &[i64],
               known: &[i64], modulation: Modulation, id_base: u32) -> Result<Helper, String> {
    let label = sdrtrunk_source::tuner_label(serial);
    // Checked before opening (an open is slow USB work on the decode thread).
    if !required.iter().all(|&hz| tuning::Window::for_tuner(serial, 0, 0, cfg.min_hz, cfg.max_hz).allowed(hz)) {
        return Err(format!("{label}: outside its frequency limits"));
    }
    if let Some(c) = cfg.center_hz {
        let locked = tuning::Window::for_tuner(serial, expected_rate(serial, cfg), c as i64, cfg.min_hz, cfg.max_hz);
        if !required.iter().all(|&hz| locked.covers(hz)) { return Err(format!("{label}: its window cannot cover it")); }
    }
    let claim = claim()?;
    // With nothing required (start-up), the window is placed over whichever `known` channels fit this tuner's limits.
    let limits = tuning::Window::for_tuner(serial, 0, 0, cfg.min_hz, cfg.max_hz);
    let Some(&first) = required.first().or_else(|| known.iter().find(|&&hz| limits.allowed(hz))) else {
        return Err(format!("{label}: nothing within its frequency limits"));
    };
    let start = cfg.center_hz.map_or_else(|| center_for(first) as i64, |c| c as i64);
    // ponytail: opened on the decode thread, so decoding stalls while the SDR opens; open in the background if that
    // shows up as dropped samples.
    let mut src = (open)(cfg, start as u32, serial)?;
    let base = tuning::Window::for_tuner(serial, src.rate_hz(), start, cfg.min_hz, cfg.max_hz);
    let locked = cfg.center_hz.is_some();
    let center = if locked { required.iter().all(|&hz| base.covers(hz)).then_some(start) } else { choose_center(required, known, base) }
        .ok_or_else(|| format!("{label}: its window cannot cover it"))?;
    if required.is_empty() && !known.iter().any(|&hz| base.at(center).covers(hz)) {
        return Err(format!("{label}: its window covers none of them"));
    }
    if center != start { src.tune(center as u32)?; }
    let bank = VoiceBank::new(center as f64, src.rate_hz() as f64, cfg.ppm, modulation, now_ms(), id_base)?;
    Ok(Helper { serial: serial.to_string(), src, bank, win: base.at(center), locked, _claim: claim, idle_since_ms: None, id_base,
        view: Arc::default(), fft: sdrtrunk_dsp::spectrum::Spectrum::new(SPECTRUM_FFT, SPECTRUM_WIDTH), last: Vec::new(), last_frame_ms: 0, seq: 0 })
}

/// Samples per second tuner `id` delivers once open: an RTL-SDR always runs at its own rate (`rtlsdr::RATE_HZ`).
fn expected_rate(id: &str, cfg: &TunerConfig) -> u32 { if id.starts_with("rtl:") { 2_400_000 } else { cfg.rate_hz } }

/// Moves helper `h` so it also covers `hz` with its calls in progress; false when it is locked or they do not fit.
fn move_helper(h: &mut Helper, hz: i64, known: &[i64]) -> bool {
    if h.locked { return false; }
    let mut required = h.bank.voice_freqs();
    required.push(hz);
    let Some(center) = choose_center(&required, known, h.win) else { return false };
    if h.bank.set_center(center as f64).is_err() { return false; }
    if h.src.tune(center as u32).is_err() {
        let _ = h.bank.set_center(h.win.center as f64);
        return false;
    }
    h.win = h.win.at(center);
    true
}

/// Reads every helper without waiting and decodes its voice channels into `events`. A helper whose source ended is
/// dropped (its tuner released) and its channel ids go to `lost`, for the recorder to forget. True when any helper
/// delivered samples.
fn read_helpers(helpers: &mut Vec<Helper>, events: &mut Vec<ControlEvent>, lost: &mut Vec<u32>) -> bool {
    let mut got = false;
    helpers.retain_mut(|h| match h.src.next_block(Duration::ZERO) {
        Some(block) => {
            got |= !block.is_empty();
            h.bank.process(&block, &mut |e| events.push(e));
            if !block.is_empty() { h.last = block; }
            true
        }
        None => {
            eprintln!("decoder: helper {} stopped delivering samples; its calls end", sdrtrunk_source::tuner_label(&h.serial));
            lost.extend(h.bank.voice_ids());
            false
        }
    });
    got
}

/// Drops the helpers on tuners `ids` (an owner's save asked for them) as if their SDRs were lost: their tuners are
/// released and their channel ids go to `lost`, for the recorder to finish and forget.
fn drop_helpers(helpers: &mut Vec<Helper>, ids: &[String], lost: &mut Vec<u32>) {
    helpers.retain(|h| {
        let keep = !ids.contains(&h.serial);
        if !keep { lost.extend(h.bank.voice_ids()); }
        keep
    });
}

/// Draws a spectrum frame for each watched helper from its newest block, marking the calls (`following`, asked only
/// when a frame is drawn) inside its window.
fn draw_helpers(helpers: &mut [Helper], following: impl Fn() -> Vec<(u64, u16, Option<String>, bool)>, now: i64) {
    let mut calls = None;
    for h in helpers {
        let block = std::mem::take(&mut h.last);
        let watched = h.view.lock().unwrap_or_else(|p| p.into_inner()).watch_until_ms > now;
        if !watched || block.is_empty() || now - h.last_frame_ms < SPECTRUM_MS { continue; }
        let markers = calls.get_or_insert_with(&following).iter().filter(|f| h.win.covers(f.0 as i64)).cloned().map(voice_marker).collect();
        if let Some(frame) = tuner_frame(&mut h.fft, &block, h.seq + 1, &h.win, h.src.rate_hz(), markers) {
            (h.last_frame_ms, h.seq) = (now, h.seq + 1);
            h.view.lock().unwrap_or_else(|p| p.into_inner()).frame = Some(frame);
        }
    }
}

/// Drops helpers that have had no voice channel for `HELPER_IDLE_MS` (their tuners are free again).
fn release_idle(helpers: &mut Vec<Helper>, now_ms: i64) {
    helpers.retain_mut(|h| {
        if h.bank.voice_count() > 0 { h.idle_since_ms = None; return true; }
        now_ms - *h.idle_since_ms.get_or_insert(now_ms) < HELPER_IDLE_MS
    });
}

/// Opens voice channels for the recorder. A call outside the window moves the tuner first, but only when the control
/// channel and every call in progress stay inside, and never with Lock center (the Java reference's rule). A helper
/// already covering a call takes it before any tuner moves; a call no tuner covers moves the main tuner, else a helper,
/// else goes to a newly claimed free tuner.
struct Follow<'a> {
    cc: &'a mut ControlChannel,
    src: &'a mut Box<dyn IqSource>,
    win: &'a mut tuning::Window,
    locked: bool,
    control: i64,
    known: &'a Mutex<BTreeSet<u64>>,
    status: &'a Mutex<Status>,
    helpers: &'a mut Vec<Helper>,
    /// Tuners that may become helpers now.
    free: &'a dyn Fn() -> Vec<String>,
    /// `open_helper` for (tuner, required, known, id base), with the system's settings.
    opener: &'a mut dyn FnMut(&str, &[i64], &[i64], u32) -> Result<Helper, String>,
}

impl Tuner for Follow<'_> {
    fn open(&mut self, hz: i64) -> Result<u32, String> {
        if self.win.covers(hz) { return self.cc.add_voice(hz); }
        // A tuner already covering the call before moving one in use (a retune drops voice in progress). One that
        // fails leaves the call to the next option.
        for h in self.helpers.iter_mut().filter(|h| h.win.covers(hz)) {
            if let Ok(id) = h.bank.add_voice(hz) { return Ok(id); }
        }
        let why = match self.move_to_cover(hz) { Ok(()) => return self.cc.add_voice(hz), Err(e) => e };
        let known: Vec<i64> = self.known.lock().unwrap_or_else(|p| p.into_inner()).iter().map(|&k| k as i64)
            .filter(|&k| !self.win.covers(k)).collect();
        for h in self.helpers.iter_mut() {
            if move_helper(h, hz, &known) { return h.bank.add_voice(hz); }
        }
        let mut tried = Vec::new();
        for serial in (self.free)() {
            match (self.opener)(&serial, &[hz], &known, free_id_base(self.helpers)) {
                Ok(h) => { self.helpers.push(h); return self.helpers.last_mut().expect("just pushed").bank.add_voice(hz); }
                Err(e) => tried.push(e),
            }
        }
        let helpers = if tried.is_empty() { "no free SDR".to_string() } else { tried.join("; ") };
        Err(format!("no tuner can reach it: {why}; {helpers} (plug in another SDR or set limits)"))
    }
    /// Ids are unique across the main tuner and the helpers, so only the bank holding `id` changes.
    fn close(&mut self, id: u32) {
        self.cc.remove_voice(id);
        for h in self.helpers.iter_mut() { h.bank.remove_voice(id); }
    }
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
        let Some(block) = src.next_block(Duration::from_millis(200)) else { return update(&status, |st| st.state = "stopped".into()) };
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

    #[test]
    fn radio_names_follow_the_systems_list() {
        let db = Db::open_in_memory().unwrap();
        db.save_radio_name("A", &crate::db::RadioName { radio: 733_730, name: "K315".into() }).unwrap();
        assert_eq!(policy_for(&db, "A").radios.get(&733_730).map(String::as_str), Some("K315"));
        assert!(policy_for(&db, "B").radios.is_empty());
    }
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
        fn next_block(&mut self, _wait: Duration) -> Option<Vec<i16>> { Some(vec![0; 2 * 32_768]) }
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
        let rx = Receivers::new(sdrs);
        rx.restart_all(&db);
        assert!(wait_for(Duration::from_secs(2), || state(&rx, auto) == "running" && state(&rx, pinned) == "running"));
        assert_eq!(rx.status(pinned).unwrap().tuner.as_deref(), Some(A));
        assert_eq!(rx.status(auto).unwrap().tuner.as_deref(), Some(B), "automatic never takes a pinned tuner");
        // Added once the others run, so it is the one left without a tuner.
        let third = db.save_system(&sys("Extra", "")).unwrap();
        rx.apply(&db, third);
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
    fn plugged_in_sdr_retries_a_waiting_system() {
        let db = Db::open_in_memory().unwrap();
        let (sdrs, open, _) = zeros();
        let present: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(vec![A.into()]));
        let p = present.clone();
        let sdrs = Sdrs { list: Arc::new(move || p.lock().unwrap().clone()), open: sdrs.open };
        let first = db.save_system(&sys("SGCo", "")).unwrap();
        let rx = Receivers::new(sdrs);
        rx.restart_all(&db);
        assert!(wait_for(Duration::from_secs(2), || state(&rx, first) == "running"));
        // Added once the first runs, so it is the one left without a tuner.
        let second = db.save_system(&sys("Extra", "")).unwrap();
        rx.apply(&db, second);
        assert!(wait_for(Duration::from_secs(2), || state(&rx, first) == "running" && state(&rx, second) == "error"));
        assert!(rx.status(second).unwrap().error.unwrap().starts_with("no free SDR"));
        rx.rescan(&db);
        assert_eq!(state(&rx, second), "error", "nothing changed: nothing retried");
        present.lock().unwrap().push(B.into());
        rx.rescan(&db);
        assert!(wait_for(Duration::from_secs(2), || state(&rx, second) == "running"));
        assert_eq!(rx.status(second).unwrap().tuner.as_deref(), Some(B));
        let opened = open.load(Ordering::SeqCst);
        rx.rescan(&db);
        std::thread::sleep(Duration::from_millis(200));
        assert_eq!(open.load(Ordering::SeqCst), opened, "no change restarts nothing");
        rx.stop_all();
    }

    #[test]
    fn unsorted_list_is_no_change() {
        let db = Db::open_in_memory().unwrap();
        let (sdrs, open, _) = zeros();
        let sdrs = Sdrs { list: Arc::new(|| vec![A.into(), B.into()]), open: sdrs.open };
        // A sorts after B, so this list is unsorted.
        assert!(A > B);
        let first = db.save_system(&sys("SGCo", A)).unwrap();
        let rx = Receivers::new(sdrs);
        rx.restart_all(&db);
        assert!(wait_for(Duration::from_secs(2), || state(&rx, first) == "running"));
        // Added once the first runs, so it is the one left without a tuner.
        let second = db.save_system(&sys("Extra", A)).unwrap();
        rx.apply(&db, second);
        assert!(wait_for(Duration::from_secs(2), || state(&rx, first) == "running" && state(&rx, second) == "error"));
        let opened = open.load(Ordering::SeqCst);
        let before = rx.get(second).unwrap();
        rx.rescan(&db);
        assert!(Arc::ptr_eq(&before, &rx.get(second).unwrap()), "the first rescan restarted nothing");
        assert_eq!(open.load(Ordering::SeqCst), opened);
        rx.stop_all();
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

    #[test]
    fn rescan_counts_failures() {
        let db = Db::open_in_memory().unwrap();
        let id = db.save_system(&sys("SGCo", "")).unwrap();
        let list = Arc::new(Mutex::new(vec![A.to_string()]));
        let l = list.clone();
        let rx = Receivers::new(Sdrs { list: Arc::new(move || l.lock().unwrap().clone()), open: Arc::new(|_, _, _| Err("gone".into())) });
        rx.restart_all(&db);
        let first_error = Arc::new(Mutex::new(0));
        // Unplugged, back, unplugged again: two drops. The scan that sees it back restarts a system that never got a
        // tuner (not a drop of its own), and the error time carries over the restarts.
        for (i, plugged) in [vec![], vec![A.to_string()], vec![]].into_iter().enumerate() {
            assert!(wait_for(Duration::from_secs(1), || state(&rx, id) == "error"));
            if i == 0 { *first_error.lock().unwrap() = rx.status(id).unwrap().error_ms; }
            *list.lock().unwrap() = plugged;
            rx.rescan(&db);
        }
        assert_eq!(rx.failures(id, 0), 2, "each drop counts once");
        assert_eq!(rx.failures(id, now_ms() + 1), 0, "only failures since the given time");
        assert!(wait_for(Duration::from_secs(1), || state(&rx, id) == "error"));
        assert_eq!(rx.status(id).unwrap().down_since_ms, Some(*first_error.lock().unwrap()), "down since the first error");
    }

    #[test]
    fn retries_by_apply_count_drops_too() {
        let db = Db::open_in_memory().unwrap();
        let id = db.save_system(&sys("SGCo", "")).unwrap();
        let rx = Receivers::new(Sdrs { list: Arc::new(|| vec![A.to_string()]), open: Arc::new(|_, _, _| Err("gone".into())) });
        rx.restart_all(&db);
        for n in 1..=2 {
            assert!(wait_for(Duration::from_secs(1), || state(&rx, id) == "error"));
            rx.apply(&db, id);
            assert_eq!(rx.failures(id, 0), n, "a failed system that held a tuner, retried by a save, dropped it");
        }
    }

    struct Boom;
    impl IqSource for Boom {
        fn next_block(&mut self, _wait: Duration) -> Option<Vec<i16>> { panic!("simulated decoder fault") }
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
        fn next_block(&mut self, _wait: Duration) -> Option<Vec<i16>> { Some(Vec::new()) }
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
        let (mut helpers, free) = (Vec::new(), Vec::<String>::new);
        let mut opener = |_: &str, _: &[i64], _: &[i64], _: u32| -> Result<Helper, String> { Err("unused".into()) };
        let mut f = Follow { cc: &mut cc, src: &mut src, win: &mut win, locked: false, control, known: &known, status: &status,
            helpers: &mut helpers, free: &free, opener: &mut opener };
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
        let mut f = Follow { cc: &mut cc, src: &mut src, win: &mut win, locked: true, control, known: &known, status: &status,
            helpers: &mut helpers, free: &free, opener: &mut opener };
        assert!(f.open(853_950_000).unwrap_err().contains("locked window"));
        let limited = tuning::Window::for_tuner("rtl:00000001", 2_400_000, 0, None, Some(853_000_000));
        let mut win = place(limited, None, control, &[]).unwrap();
        let mut f = Follow { cc: &mut cc, src: &mut src, win: &mut win, locked: false, control, known: &known, status: &status,
            helpers: &mut helpers, free: &free, opener: &mut opener };
        assert!(f.open(853_475_000).unwrap_err().contains("frequency limits"));
        assert!(place(base, Some(855_000_000), control, &[]).unwrap_err().contains("outside this tuner's locked window"));
    }

    const RTL1: &str = "rtl:00000001";
    const RTL2: &str = "rtl:00000002";
    const KSICS: i64 = 852_112_500;

    /// Never delivers: the helper's SDR was unplugged.
    struct Gone;
    impl IqSource for Gone {
        fn next_block(&mut self, _wait: Duration) -> Option<Vec<i16>> { None }
        fn tune(&mut self, _: u32) -> Result<(), String> { Ok(()) }
        fn dropped(&self) -> u64 { 0 }
        fn rate_hz(&self) -> u32 { 2_400_000 }
    }

    /// KSICS on RTL-SDR 1 (claimed, the main tuner) with RTL-SDR 2 connected; `reserved` are pinned elsewhere.
    struct Rig {
        cc: ControlChannel, src: Box<dyn IqSource>, win: tuning::Window, known: Mutex<BTreeSet<u64>>, status: Mutex<Status>,
        helpers: Vec<Helper>, claimed: Claimed, reserved: Vec<String>, factory: SourceFactory, present: Vec<String>,
        /// Settings of every helper tuner.
        cfg: TunerConfig,
        main_tunes: Arc<Mutex<Vec<u32>>>, opens: Arc<Mutex<Vec<(String, u32)>>>, helper_tunes: Arc<Mutex<Vec<u32>>>,
    }

    impl Rig {
        fn new(reserved: &[&str], gone: bool) -> Rig {
            let (main_tunes, opens, helper_tunes) = (Arc::new(Mutex::new(Vec::new())), Arc::new(Mutex::new(Vec::new())), Arc::new(Mutex::new(Vec::new())));
            let (o, t) = (opens.clone(), helper_tunes.clone());
            let factory: SourceFactory = Arc::new(move |_, center, id| {
                o.lock().unwrap().push((id.to_string(), center));
                Ok(if gone { Box::new(Gone) as Box<dyn IqSource> } else { Box::new(Tunes(t.clone())) })
            });
            let base = tuning::Window::for_tuner(RTL1, 2_400_000, 0, None, None);
            let win = place(base, None, KSICS, &[]).unwrap();
            Rig {
                cc: ControlChannel::new(win.center as f64, 2.4e6, KSICS, 0.0, 0, Modulation::C4fm).unwrap(),
                src: Box::new(Tunes(main_tunes.clone())), win, known: Mutex::default(), status: Mutex::default(), helpers: Vec::new(),
                claimed: Arc::new(Mutex::new([RTL1.to_string()].into())), reserved: reserved.iter().map(|r| r.to_string()).collect(),
                factory, main_tunes, opens, helper_tunes, present: vec![RTL1.into(), RTL2.into()], cfg: TunerConfig::default(),
            }
        }

        fn free(&self) -> Vec<String> {
            free_tuners(self.present.clone(), &|_| self.cfg.clone(), &self.claimed.lock().unwrap(), &self.reserved)
        }

        fn follow<R>(&mut self, act: impl FnOnce(&mut Follow) -> R) -> R {
            let (claimed, reserved, factory, present, cfg) = (&self.claimed, &self.reserved, &self.factory, &self.present, &self.cfg);
            let free = || free_tuners(present.clone(), &|_| cfg.clone(), &claimed.lock().unwrap(), reserved);
            let mut opener = |serial: &str, required: &[i64], known: &[i64], id_base: u32| {
                let claim = || if claimed.lock().unwrap().insert(serial.to_string()) { Ok(Claim(claimed.clone(), serial.to_string())) } else { Err("in use".to_string()) };
                open_helper(serial, cfg, factory, &claim, required, known, Modulation::C4fm, id_base)
            };
            act(&mut Follow { cc: &mut self.cc, src: &mut self.src, win: &mut self.win, locked: false, control: KSICS, known: &self.known,
                status: &self.status, helpers: &mut self.helpers, free: &free, opener: &mut opener })
        }

        fn open(&mut self, hz: i64) -> Result<u32, String> { self.follow(|f| f.open(hz)) }
        fn close(&mut self, id: u32) { self.follow(|f| f.close(id)) }
        fn claims(&self, id: &str) -> bool { self.claimed.lock().unwrap().contains(id) }
    }

    #[test]
    fn helper_takes_a_call_the_main_window_cannot() {
        let mut rig = Rig::new(&[], false);
        assert!(rig.open(851_600_000).unwrap() < 1 << 24, "inside the main window");
        // 851.6 to 853.95 MHz is wider than one RTL-SDR window: the main tuner cannot move, RTL-SDR 2 takes it.
        let id = rig.open(853_950_000).unwrap();
        assert!(id >= 1 << 24, "helper id {id}");
        assert!(rig.main_tunes.lock().unwrap().is_empty(), "the main window stayed");
        assert_eq!(rig.helpers.len(), 1);
        let h = &rig.helpers[0];
        assert_eq!(h.serial, RTL2);
        let opens = rig.opens.lock().unwrap().clone();
        assert_eq!(opens.len(), 1);
        let at = rig.helper_tunes.lock().unwrap().last().copied().unwrap_or(opens[0].1);
        assert_eq!(at as i64, h.win.center, "the SDR sits where its window says");
        assert!(h.win.covers(853_950_000));
        assert!(rig.claims(RTL2), "claimed like a main tuner");

        // The main tuner could move to 853.8 MHz, but a helper already covering it is used first (no main retune).
        assert_eq!(rig.open(853_800_000).unwrap(), id + 1, "same helper");
        assert_eq!((rig.helpers.len(), rig.opens.lock().unwrap().len()), (1, 1), "no new claim");
        assert!(rig.main_tunes.lock().unwrap().is_empty(), "the main tuner was not retuned");
        assert_eq!(rig.helpers[0].bank.voice_freqs(), vec![853_950_000, 853_800_000]);
        assert_eq!(rig.cc.voice_freqs(), vec![851_600_000], "the call in progress stays on the main tuner");
    }

    #[test]
    fn no_free_tuner_is_an_error_not_a_crash() {
        let mut rig = Rig::new(&[RTL2], false);
        rig.open(851_600_000).unwrap();
        let e = rig.open(853_950_000).unwrap_err();
        assert!(e.contains("no tuner can reach it"), "{e}");
        assert!(!rig.claims(RTL2) && rig.opens.lock().unwrap().is_empty(), "the pinned tuner is not taken");
        assert!(rig.helpers.is_empty());
    }

    #[test]
    fn idle_helper_released() {
        let mut rig = Rig::new(&[], false);
        rig.open(851_600_000).unwrap();
        let id = rig.open(853_950_000).unwrap();
        release_idle(&mut rig.helpers, 1_000);
        assert_eq!(rig.helpers.len(), 1, "a call in progress");
        rig.close(id);
        assert!(rig.helpers[0].bank.voice_freqs().is_empty(), "closed on the helper");
        release_idle(&mut rig.helpers, 2_000);
        release_idle(&mut rig.helpers, 2_000 + HELPER_IDLE_MS - 1);
        assert_eq!(rig.helpers.len(), 1, "not idle long enough");
        assert!(rig.free().is_empty());
        release_idle(&mut rig.helpers, 2_000 + HELPER_IDLE_MS);
        assert!(rig.helpers.is_empty(), "released");
        assert!(!rig.claims(RTL2));
        assert_eq!(rig.free(), vec![RTL2.to_string()], "free again");
    }

    #[test]
    fn helper_source_lost() {
        let mut rig = Rig::new(&[], true);
        let names = Names { system: "KSICS".into(), site: "Site".into(), channel: "Control".into() };
        let mut rec = Recorder::new(&std::env::temp_dir().join("sdrtrunk-helper-lost"), names, Arc::default());
        let hz = 855_950_000;
        let grant = ControlEvent::Call(sdrtrunk_p25::trunk::CallGrant { group: 1, source: 0, hz, encrypted: false, emergency: false,
            channel: sdrtrunk_p25::tsbk_msg::Channel { band: 0, number: 1 } });
        rig.follow(|f| rec.handle(&grant, 0, f));
        let id = rig.helpers[0].bank.voice_ids()[0];
        assert!(id >= 1 << 24);
        let (mut events, mut lost) = (Vec::new(), Vec::new());
        read_helpers(&mut rig.helpers, &mut events, &mut lost);
        assert_eq!(lost, vec![id], "its channels are reported lost");
        assert!(rig.helpers.is_empty() && !rig.claims(RTL2), "dropped and released");
        for id in lost { rec.drop_channel(id); }
        assert!(rec.following().is_empty(), "the recorder forgot the channel");
        rig.follow(|f| rec.handle(&grant, 100, f));
        assert_eq!((rig.opens.lock().unwrap().len(), rec.following().len()), (2, 1), "a later grant there gets a new channel");
        assert!(rig.open(851_600_000).unwrap() < 1 << 24, "the main tuner still follows calls");
    }

    #[test]
    fn helper_ids_reused_without_collision() {
        let mut rig = Rig::new(&[], false);
        rig.present.push("rtl:00000003".into());
        rig.open(851_600_000).unwrap();
        let first = rig.open(853_950_000).unwrap();
        assert_eq!(first, 1 << 24);
        // RTL-SDR 2 cannot stretch from 853.95 to 857 MHz: RTL-SDR 3 takes it with the next base.
        let second = rig.open(857_000_000).unwrap();
        assert_eq!((second, rig.helpers.len()), (2 << 24, 2));
        rig.close(first);
        release_idle(&mut rig.helpers, 0);
        release_idle(&mut rig.helpers, HELPER_IDLE_MS);
        assert_eq!(rig.helpers.len(), 1, "RTL-SDR 2 released, RTL-SDR 3 still on its call");
        // 1 << 24 is free again; 2 << 24 is still RTL-SDR 3's.
        let again = rig.open(853_950_000).unwrap();
        assert_eq!(again, 1 << 24);
        let ids: Vec<Vec<u32>> = rig.helpers.iter().map(|h| h.bank.voice_ids()).collect();
        assert_eq!(ids, vec![vec![2 << 24], vec![1 << 24]], "no id in two banks");
    }

    #[test]
    fn failover_keeps_helpers_and_their_calls() {
        let mut rig = Rig::new(&[], false);
        let names = Names { system: "KSICS".into(), site: "Site".into(), channel: "Control".into() };
        let mut rec = Recorder::new(&std::env::temp_dir().join("sdrtrunk-failover-helpers"), names, Arc::default());
        let grant = |hz: u64| ControlEvent::Call(sdrtrunk_p25::trunk::CallGrant { group: 1, source: 0, hz, encrypted: false,
            emergency: false, channel: sdrtrunk_p25::tsbk_msg::Channel { band: 0, number: 1 } });
        rig.follow(|f| { rec.handle(&grant(851_600_000), 0, f); rec.handle(&grant(853_950_000), 0, f); });
        assert_eq!(rec.following().len(), 2);
        let next = ControlChannel::new(rig.win.center as f64, 2.4e6, 852_200_000, 0.0, 0, Modulation::C4fm).unwrap();
        switch_control(&mut rig.cc, next, &mut rec);
        let following: Vec<u64> = rec.following().iter().map(|f| f.0).collect();
        assert_eq!(following, vec![853_950_000], "only the main tuner's call is gone");
        assert_eq!(rig.helpers[0].bank.voice_freqs(), vec![853_950_000], "the helper keeps its call");
        assert!(rig.cc.voice_freqs().is_empty());
        // A grant on the dropped main frequency opens a fresh channel on the new control channel's bank.
        rig.follow(|f| rec.handle(&grant(851_600_000), 100, f));
        assert_eq!(rig.cc.voice_freqs(), vec![851_600_000]);
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
    fn known_frequencies_outside_the_main_window_get_a_helper_at_start() {
        for (list, helper) in [(vec![RTL1, RTL2], true), (vec![RTL1], false)] {
            let db = Db::open_in_memory().unwrap();
            let list: Vec<String> = list.into_iter().map(String::from).collect();
            let sdrs = Sdrs {
                list: Arc::new(move || list.clone()),
                open: Arc::new(|_, _, _| Ok(Box::new(Tunes(Arc::default())) as Box<dyn IqSource>)),
            };
            let id = db.save_system(&sys("SGCo", "")).unwrap();
            // Spread over 4 MHz: one RTL-SDR window cannot hold them all.
            db.add_voice_freqs(id, &[859_000_000, 860_000_000, 862_500_000, 863_000_000]).unwrap();
            let rx = Receivers::new(sdrs);
            rx.restart_all(&db);
            assert!(wait_for(Duration::from_secs(2), || state(&rx, id) == "running"));
            let st = rx.status(id).unwrap();
            let main = tuning::Window::for_tuner(RTL1, 2_400_000, st.center_hz.unwrap() as i64, None, None);
            assert!(main.covers(858_962_500) && !main.covers(863_000_000), "the main window leaves the far ones out");
            if helper {
                assert!(wait_for(Duration::from_secs(2), || !rx.status(id).unwrap().helpers.is_empty()));
                let st = rx.status(id).unwrap();
                assert_eq!(st.helpers.len(), 1);
                assert_eq!(st.helpers[0].0, RTL2);
                let w = tuning::Window::for_tuner(RTL2, 2_400_000, st.helpers[0].1 as i64, None, None);
                assert!(w.covers(862_500_000) && w.covers(863_000_000), "helper centre {}", st.helpers[0].1);
            } else {
                std::thread::sleep(Duration::from_millis(200));
                let st = rx.status(id).unwrap();
                assert!(st.helpers.is_empty() && st.state == "running", "alone on the main tuner");
            }
        }
    }

    #[test]
    fn helpers_vanish_from_status_when_the_decoder_stops() {
        let db = Db::open_in_memory().unwrap();
        let sdrs = Sdrs {
            list: Arc::new(|| vec![RTL1.into(), RTL2.into()]),
            open: Arc::new(|_, _, _| Ok(Box::new(Tunes(Arc::default())) as Box<dyn IqSource>)),
        };
        let id = db.save_system(&sys("SGCo", RTL1)).unwrap();
        db.add_voice_freqs(id, &[859_000_000, 862_500_000, 863_000_000]).unwrap();
        let rx = Receivers::new(sdrs);
        rx.restart_all(&db);
        assert!(wait_for(Duration::from_secs(2), || !rx.status(id).is_none_or(|s| s.helpers.is_empty())));
        assert!(rx.tuner_view(RTL2).is_some(), "the helper's own spectrum");
        assert!(rx.tuner_view(RTL1).is_some_and(|v| !Arc::ptr_eq(&v, &rx.tuner_view(RTL2).unwrap())), "a different view from the main tuner's");
        let r = rx.get(id).unwrap();
        assert_eq!(r.status.lock().unwrap().helper_bands.len(), 1);
        db.delete_system(id).unwrap();
        rx.apply(&db, id);
        let st = r.status.lock().unwrap().clone();
        assert!(st.helpers.is_empty() && st.helper_bands.is_empty() && st.state == "stopped", "{st:?}");
        assert!(rx.tuner_view(RTL2).is_none());
    }

    /// Known 859.0/862.5/863.0 MHz leave the main window (RTL-SDR 1 sits on the control channel + 859.0 side); the helper must
    /// take the two that fit together, not anchor on the outlier. `min_hz` is RTL-SDR 2's lower limit.
    fn helper_for_outliers(min_hz: Option<u64>, known: &[u64]) -> Vec<(String, u64)> {
        let db = Db::open_in_memory().unwrap();
        let sdrs = Sdrs {
            list: Arc::new(|| vec![RTL1.into(), RTL2.into()]),
            open: Arc::new(|_, _, _| Ok(Box::new(Tunes(Arc::default())) as Box<dyn IqSource>)),
        };
        db.save_tuner(RTL2, &TunerConfig { min_hz, ..TunerConfig::default() }).unwrap();
        let id = db.save_system(&sys("SGCo", RTL1)).unwrap();
        db.add_voice_freqs(id, known).unwrap();
        let rx = Receivers::new(sdrs);
        rx.restart_all(&db);
        assert!(wait_for(Duration::from_secs(2), || state(&rx, id) == "running"));
        wait_for(Duration::from_millis(500), || !rx.status(id).unwrap().helpers.is_empty());
        rx.status(id).unwrap().helpers
    }

    #[test]
    fn start_helper_covers_the_most_not_the_first() {
        let h = helper_for_outliers(None, &[865_000_000, 868_500_000, 869_000_000]);
        assert_eq!(h.len(), 1);
        let w = tuning::Window::for_tuner(RTL2, 2_400_000, h[0].1 as i64, None, None);
        assert!(w.covers(868_500_000) && w.covers(869_000_000), "the two that fit together, centre {}", h[0].1);
    }

    #[test]
    fn start_helper_skips_known_outside_its_limits() {
        // 855.0 MHz is first but under RTL-SDR 2's floor; the two near 863 still fit.
        let h = helper_for_outliers(Some(860_000_000), &[855_000_000, 863_000_000, 863_500_000]);
        assert_eq!(h.len(), 1);
        let w = tuning::Window::for_tuner(RTL2, 2_400_000, h[0].1 as i64, Some(860_000_000), None);
        assert!(w.covers(863_000_000) && w.covers(863_500_000), "centre {}", h[0].1);
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

    /// A fake SDR: blocks of zeros (empty ones when `silent`) until `end` is set, then None (unplugged).
    struct Fake { id: String, rate: u32, silent: bool, probe: Probe }
    impl IqSource for Fake {
        fn next_block(&mut self, _wait: Duration) -> Option<Vec<i16>> {
            if self.probe.end.load(Ordering::SeqCst) { return None; }
            *self.probe.reads.lock().unwrap().entry(self.id.clone()).or_default() += 1;
            Some(if self.silent { Vec::new() } else { vec![0; 2 * 4_096] })
        }
        fn tune(&mut self, _: u32) -> Result<(), String> { Ok(()) }
        fn dropped(&self) -> u64 { 0 }
        fn rate_hz(&self) -> u32 { self.rate }
    }
    impl Drop for Fake { fn drop(&mut self) { self.probe.open.fetch_sub(1, Ordering::SeqCst); } }

    /// What the fakes saw: open sources, reads per tuner, calls of `list`; `end` unplugs every open source.
    #[derive(Clone, Default)]
    struct Probe { open: Arc<AtomicUsize>, end: Arc<AtomicBool>, reads: Arc<Mutex<BTreeMap<String, usize>>>, lists: Arc<AtomicUsize> }
    impl Probe { fn reads(&self, id: &str) -> usize { self.reads.lock().unwrap().get(id).copied().unwrap_or(0) } }

    /// Fake SDRs `list`; those in `silent` deliver only empty blocks.
    fn fakes(list: &[&str], silent: &[&str]) -> (Sdrs, Probe) {
        let probe = Probe::default();
        let (list, silent): (Vec<String>, Vec<String>) = (list.iter().map(|s| s.to_string()).collect(), silent.iter().map(|s| s.to_string()).collect());
        let (p, q) = (probe.clone(), probe.clone());
        let sdrs = Sdrs {
            list: Arc::new(move || { p.lists.fetch_add(1, Ordering::SeqCst); list.clone() }),
            open: Arc::new(move |_, _, id| {
                q.open.fetch_add(1, Ordering::SeqCst);
                let rate = if id.starts_with("rtl:") { 2_400_000 } else { 3_000_000 };
                Ok(Box::new(Fake { id: id.into(), rate, silent: silent.iter().any(|s| s == id), probe: q.clone() }) as Box<dyn IqSource>)
            }),
        };
        (sdrs, probe)
    }

    /// A system on RTL-SDR 1 whose known voice frequencies need a helper (RTL-SDR 2), running with it.
    fn with_helper(db: &Db, rx: &Receivers) -> i64 {
        let id = db.save_system(&sys("SGCo", RTL1)).unwrap();
        db.add_voice_freqs(id, &[859_000_000, 862_500_000, 863_000_000]).unwrap();
        rx.restart_all(db);
        assert!(wait_for(Duration::from_secs(2), || rx.status(id).is_some_and(|s| s.helpers.iter().any(|h| h.0 == RTL2))), "{:?}", rx.status(id));
        id
    }

    #[test]
    fn main_source_end_is_an_error_and_retried() {
        let db = Db::open_in_memory().unwrap();
        let (sdrs, probe) = fakes(&[RTL1], &[]);
        let id = db.save_system(&sys("SGCo", "")).unwrap();
        let rx = Receivers::new(sdrs);
        rx.restart_all(&db);
        assert!(wait_for(Duration::from_secs(2), || state(&rx, id) == "running"));
        rx.rescan(&db);
        probe.end.store(true, Ordering::SeqCst);
        assert!(wait_for(Duration::from_secs(2), || state(&rx, id) == "error"), "{:?}", rx.status(id));
        assert!(rx.status(id).unwrap().error.unwrap().contains("stopped delivering samples"));
        probe.end.store(false, Ordering::SeqCst);
        rx.rescan(&db);
        assert!(wait_for(Duration::from_secs(2), || state(&rx, id) == "running"), "the SDR is there and free: retried {:?}", rx.status(id));
        assert_eq!(probe.open.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn helper_switched_to_setup_is_released() {
        let db = Db::open_in_memory().unwrap();
        let (sdrs, probe) = fakes(&[RTL1, RTL2], &[]);
        let rx = Receivers::new(sdrs);
        let id = with_helper(&db, &rx);
        let before = rx.get(id).unwrap();
        db.save_tuner(RTL2, &TunerConfig { mode: TunerMode::Setup, center_hz: Some(863_000_000), ..TunerConfig::default() }).unwrap();
        rx.apply_tuner(&db, RTL2);
        assert!(wait_for(Duration::from_secs(2), || rx.setup_status(RTL2).is_some_and(|s| s.state == "running")), "{:?}", rx.setup_status(RTL2));
        assert!(Arc::ptr_eq(&before, &rx.get(id).unwrap()) && state(&rx, id) == "running", "the system kept running, not restarted");
        assert_eq!(probe.open.load(Ordering::SeqCst), 2, "main tuner still open, plus the Setup run");
        std::thread::sleep(Duration::from_millis(200));
        assert!(rx.status(id).unwrap().helpers.is_empty(), "a Setup tuner is never a helper");
        assert!(rx.tuner_view(RTL2).is_some_and(|v| Arc::ptr_eq(&v, &rx.setups.lock().unwrap()[RTL2].spectrum)), "the spectrum is the Setup run's");
    }

    #[test]
    fn helper_saved_live_is_released_not_its_system() {
        let db = Db::open_in_memory().unwrap();
        let (sdrs, probe) = fakes(&[RTL1, RTL2], &[]);
        let rx = Receivers::new(sdrs);
        let id = with_helper(&db, &rx);
        let before = rx.get(id).unwrap();
        db.save_tuner(RTL2, &TunerConfig { rtl_gain_db: Some(20.0), ..TunerConfig::default() }).unwrap();
        rx.apply_tuner(&db, RTL2);
        assert!(!rx.claimed.lock().unwrap().contains(RTL2), "released before the save returns");
        assert!(Arc::ptr_eq(&before, &rx.get(id).unwrap()) && state(&rx, id) == "running", "not restarted");
        assert!(wait_for(Duration::from_secs(1), || rx.status(id).unwrap().helpers.is_empty()));
        assert_eq!(probe.open.load(Ordering::SeqCst), 1, "only the main tuner open");
    }

    #[test]
    fn newly_pinned_helper_goes_to_its_system() {
        let db = Db::open_in_memory().unwrap();
        let (sdrs, _) = fakes(&[RTL1, RTL2], &[]);
        let rx = Receivers::new(sdrs);
        let a = with_helper(&db, &rx);
        let before = rx.get(a).unwrap();
        let b = db.save_system(&sys("KSICS", RTL2)).unwrap();
        rx.apply(&db, b);
        assert!(wait_for(Duration::from_secs(2), || state(&rx, b) == "running"), "{:?}", rx.status(b));
        assert_eq!(rx.status(b).unwrap().tuner.as_deref(), Some(RTL2));
        assert!(Arc::ptr_eq(&before, &rx.get(a).unwrap()) && state(&rx, a) == "running", "the holder kept running, not restarted");
        std::thread::sleep(Duration::from_millis(200));
        assert!(rx.status(a).unwrap().helpers.is_empty(), "a pinned tuner is never a helper");
    }

    #[test]
    fn released_helper_calls_end_and_main_calls_stay() {
        let mut rig = Rig::new(&[], false);
        rig.present.push("rtl:00000003".into());
        let names = Names { system: "KSICS".into(), site: "Site".into(), channel: "Control".into() };
        let mut rec = Recorder::new(&std::env::temp_dir().join("sdrtrunk-helper-released"), names, Arc::default());
        let grant = |hz: u64| ControlEvent::Call(sdrtrunk_p25::trunk::CallGrant { group: 1, source: 0, hz, encrypted: false,
            emergency: false, channel: sdrtrunk_p25::tsbk_msg::Channel { band: 0, number: 1 } });
        rig.follow(|f| for hz in [851_600_000, 853_950_000, 857_000_000] { rec.handle(&grant(hz), 0, f) });
        assert_eq!((rec.following().len(), rig.helpers.len()), (3, 2));
        let mut lost = Vec::new();
        drop_helpers(&mut rig.helpers, &[RTL2.to_string()], &mut lost);
        assert_eq!(lost, vec![1 << 24], "RTL-SDR 2's call");
        assert!(!rig.claims(RTL2) && rig.claims("rtl:00000003"), "only RTL-SDR 2 released");
        for id in lost { rec.drop_channel(id); }
        let mut following: Vec<u64> = rec.following().iter().map(|f| f.0).collect();
        following.sort();
        assert_eq!(following, vec![851_600_000, 857_000_000], "the main tuner's and the other helper's calls go on");
    }

    #[test]
    fn tuner_being_opened_as_a_helper_is_released_on_save() {
        let db = Db::open_in_memory().unwrap();
        let (sdrs, _) = fakes(&[RTL1, RTL2], &[]);
        // RTL-SDR 2 opens slowly: the save arrives while it is claimed as the start-up helper but not yet in the status.
        let open = sdrs.open.clone();
        let sdrs = Sdrs { list: sdrs.list, open: Arc::new(move |c: &TunerConfig, hz, id: &str| {
            if id == RTL2 { std::thread::sleep(Duration::from_millis(300)); }
            open(c, hz, id)
        }) };
        let rx = Receivers::new(sdrs);
        let id = db.save_system(&sys("SGCo", RTL1)).unwrap();
        db.add_voice_freqs(id, &[859_000_000, 862_500_000, 863_000_000]).unwrap();
        rx.restart_all(&db);
        assert!(wait_for(Duration::from_secs(2), || rx.claimed.lock().unwrap().contains(RTL2)));
        assert!(rx.status(id).unwrap().helpers.is_empty(), "not listed yet");
        let before = rx.get(id).unwrap();
        db.save_tuner(RTL2, &TunerConfig { mode: TunerMode::Setup, center_hz: Some(863_000_000), ..TunerConfig::default() }).unwrap();
        rx.apply_tuner(&db, RTL2);
        assert!(wait_for(Duration::from_secs(2), || rx.setup_status(RTL2).is_some_and(|s| s.state == "running")), "{:?}", rx.setup_status(RTL2));
        assert!(Arc::ptr_eq(&before, &rx.get(id).unwrap()), "not restarted");
    }

    #[test]
    fn failed_systems_tuner_held_as_a_helper_is_released_on_save() {
        let db = Db::open_in_memory().unwrap();
        let (sdrs, _) = fakes(&[RTL1, RTL2], &[]);
        let (open, rtl2_opens) = (sdrs.open.clone(), Arc::new(AtomicUsize::new(0)));
        let n = rtl2_opens.clone();
        let sdrs = Sdrs { list: sdrs.list, open: Arc::new(move |c: &TunerConfig, hz, id: &str| {
            // RTL-SDR 2 fails its first open (as the failed system's main tuner) and opens fine as a helper.
            if id == RTL2 && n.fetch_add(1, Ordering::SeqCst) == 0 { return Err("RTL-SDR 00000002: usb_claim_interface error -6".into()); }
            // RTL-SDR 1 opens slowly, so the second system looks for a helper only after the first one's open failed.
            if id == RTL1 { std::thread::sleep(Duration::from_millis(200)); }
            open(c, hz, id)
        }) };
        let rx = Receivers::new(sdrs);
        // Automatic, lowest id: handed RTL-SDR 2 (RTL-SDR 1 is pinned to the other system).
        let a = db.save_system(&sys("SGCo", "")).unwrap();
        let b = db.save_system(&sys("KSICS", RTL1)).unwrap();
        db.add_voice_freqs(b, &[859_000_000, 862_500_000, 863_000_000]).unwrap();
        rx.restart_all(&db);
        assert!(wait_for(Duration::from_secs(3), || state(&rx, a) == "error" && rx.status(b).is_some_and(|s| s.helpers.iter().any(|h| h.0 == RTL2))),
            "a {:?} b {:?}", rx.status(a), rx.status(b));
        assert_eq!(rx.get(a).unwrap().main.as_deref(), Some(RTL2), "the failed system still names it as its main tuner");
        db.save_tuner(RTL2, &TunerConfig { mode: TunerMode::Setup, center_hz: Some(863_000_000), ..TunerConfig::default() }).unwrap();
        rx.apply_tuner(&db, RTL2);
        assert!(wait_for(Duration::from_secs(2), || rx.setup_status(RTL2).is_some_and(|s| s.state == "running")), "{:?}", rx.setup_status(RTL2));
        assert!(rx.status(b).unwrap().helpers.is_empty());
    }

    #[test]
    fn live_save_returns_promptly_when_the_helper_is_taken_again() {
        let db = Db::open_in_memory().unwrap();
        let (sdrs, _) = fakes(&[RTL1, RTL2], &[]);
        let rx = Receivers::new(sdrs);
        let id = with_helper(&db, &rx);
        // Stands in for the decoder following a grant right after the release: the Live tuner is claimed again at once.
        let claimed = rx.claimed.clone();
        let retake = std::thread::spawn(move || {
            let t = Instant::now();
            while t.elapsed() < Duration::from_secs(3) {
                let mut c = claimed.lock().unwrap();
                if !c.contains(RTL2) { c.insert(RTL2.into()); return true; }
            }
            false
        });
        db.save_tuner(RTL2, &TunerConfig { rtl_gain_db: Some(20.0), ..TunerConfig::default() }).unwrap();
        let t = Instant::now();
        rx.apply_tuner(&db, RTL2);
        assert!(t.elapsed() < Duration::from_secs(1), "the save waited {:?}", t.elapsed());
        assert!(retake.join().unwrap(), "the helper was released");
        assert_eq!(state(&rx, id), "running");
        rx.claimed.lock().unwrap().remove(RTL2);
    }

    #[test]
    fn failed_systems_get_no_release_requests() {
        let db = Db::open_in_memory().unwrap();
        let (sdrs, _) = fakes(&[RTL1, RTL2], &[]);
        let rx = Receivers::new(sdrs);
        let id = with_helper(&db, &rx);
        let failed = db.save_system(&sys("Extra", RTL1)).unwrap();
        rx.apply(&db, failed);
        assert!(wait_for(Duration::from_secs(2), || state(&rx, failed) == "error"));
        let f = rx.get(failed).unwrap();
        rx.apply_tuner(&db, RTL2);
        assert!(f.release.ids.lock().unwrap().is_empty(), "its thread is gone: nothing to pile up");
        assert_eq!(state(&rx, id), "running");
    }

    #[test]
    fn helper_claim_checks_mode_and_pins_under_the_pool_lock() {
        let claimed: Claimed = Arc::default();
        let pool = Pool { seen: vec![RTL1.into(), RTL2.into()],
            tuners: vec![(RTL1.into(), TunerConfig { mode: TunerMode::Setup, ..TunerConfig::default() })], pins: vec![(7, RTL2.into())] };
        assert!(pool.claim_helper(&claimed, 1, RTL1).is_err(), "Setup mode");
        assert!(pool.claim_helper(&claimed, 1, RTL2).is_err(), "pinned to system 7");
        assert!(claimed.lock().unwrap().is_empty(), "nothing claimed");
        let c = pool.claim_helper(&claimed, 7, RTL2).expect("its own pin is no obstacle");
        assert!(pool.claim_helper(&claimed, 7, RTL2).err().is_some_and(|e| e.contains("in use")));
        drop(c);
        assert!(claimed.lock().unwrap().is_empty());
    }

    #[test]
    fn lowest_numbered_system_wins_a_contested_sdr() {
        for (pin, why) in [("", "no free SDR"), (RTL1, "used by another system")] {
            for _ in 0..20 {
                let db = Db::open_in_memory().unwrap();
                let low = db.save_system(&sys("Low", pin)).unwrap();
                let high = db.save_system(&sys("High", pin)).unwrap();
                let (sdrs, _) = fakes(&[RTL1], &[]);
                // The lower system's decoder thread is slow to list SDRs: whoever claims first in the threads would lose it.
                let slow = format!("decoder-{low}");
                let sdrs = Sdrs { list: Arc::new(move || {
                    if std::thread::current().name() == Some(slow.as_str()) { std::thread::sleep(Duration::from_millis(50)); }
                    vec![RTL1.into()]
                }), open: sdrs.open };
                let rx = Receivers::new(sdrs);
                rx.restart_all(&db);
                assert!(wait_for(Duration::from_secs(2), || state(&rx, low) == "running" && state(&rx, high) == "error"),
                    "pin {pin:?}: low {:?} high {:?}", rx.status(low), rx.status(high));
                assert_eq!(rx.status(low).unwrap().tuner.as_deref(), Some(RTL1));
                assert!(rx.status(high).unwrap().error.unwrap().contains(why));
                rx.stop_all();
            }
        }
    }

    #[test]
    fn tuner_freed_without_a_device_change_retries_a_waiting_system() {
        let db = Db::open_in_memory().unwrap();
        let (sdrs, _) = fakes(&[RTL1], &[]);
        let rx = Receivers::new(sdrs);
        // Held as another system's helper.
        rx.claimed.lock().unwrap().insert(RTL1.into());
        let id = db.save_system(&sys("SGCo", "")).unwrap();
        rx.restart_all(&db);
        assert!(wait_for(Duration::from_secs(2), || state(&rx, id) == "error"));
        rx.rescan(&db);
        rx.claimed.lock().unwrap().remove(RTL1);
        rx.rescan(&db);
        assert!(wait_for(Duration::from_secs(2), || state(&rx, id) == "running"), "{:?}", rx.status(id));
    }

    #[test]
    fn stopping_a_system_releases_its_helpers() {
        let db = Db::open_in_memory().unwrap();
        let (sdrs, probe) = fakes(&[RTL1, RTL2], &[]);
        let rx = Receivers::new(sdrs);
        let id = with_helper(&db, &rx);
        assert_eq!(probe.open.load(Ordering::SeqCst), 2);
        db.delete_system(id).unwrap();
        rx.apply(&db, id);
        assert!(rx.claimed.lock().unwrap().is_empty(), "every claim dropped");
        assert_eq!(probe.open.load(Ordering::SeqCst), 0, "every source closed");
    }

    #[test]
    fn silent_main_does_not_stall_helpers() {
        let db = Db::open_in_memory().unwrap();
        let (sdrs, probe) = fakes(&[RTL1, RTL2], &[RTL1]);
        let rx = Receivers::new(sdrs);
        let id = with_helper(&db, &rx);
        let before = probe.reads(RTL2);
        assert!(wait_for(Duration::from_secs(2), || probe.reads(RTL2) > before + 20), "the helper keeps being read: {} then {}", before, probe.reads(RTL2));
        let view = rx.tuner_view(RTL2).expect("the helper's spectrum");
        view.lock().unwrap().watch_until_ms = now_ms() + 5000;
        assert!(wait_for(Duration::from_secs(2), || view.lock().unwrap().frame.is_some()), "its samples are decoded and drawn");
        assert_eq!(state(&rx, id), "running");
    }

    #[test]
    fn helper_search_reads_the_cached_device_list() {
        let db = Db::open_in_memory().unwrap();
        let (sdrs, probe) = fakes(&[RTL1], &[]);
        let id = db.save_system(&sys("SGCo", RTL1)).unwrap();
        db.add_voice_freqs(id, &[863_000_000]).unwrap();
        let rx = Receivers::new(sdrs);
        rx.restart_all(&db);
        assert!(wait_for(Duration::from_secs(2), || state(&rx, id) == "running"));
        std::thread::sleep(Duration::from_millis(200));
        assert_eq!(probe.lists.load(Ordering::SeqCst), 2, "start-up and the main tuner's pick; looking for a helper uses the last scan");
    }

    #[test]
    fn locked_helper_missing_the_call_is_never_opened() {
        let mut rig = Rig::new(&[], false);
        rig.cfg = TunerConfig { center_hz: Some(860_000_000), ..TunerConfig::default() };
        rig.open(851_600_000).unwrap();
        let e = rig.open(853_950_000).unwrap_err();
        assert!(e.contains("cannot cover"), "{e}");
        assert!(rig.opens.lock().unwrap().is_empty() && !rig.claims(RTL2), "not opened, not claimed");
    }

    #[test]
    fn stalled_airspy_ends() {
        use std::sync::mpsc::RecvTimeoutError;
        assert_eq!(block_or_end(Ok(vec![1, 2]), || unreachable!()), Some(vec![1, 2]));
        assert_eq!(block_or_end(Err(RecvTimeoutError::Timeout), || true), Some(Vec::new()), "streaming: just quiet");
        assert_eq!(block_or_end(Err(RecvTimeoutError::Timeout), || false), None, "stopped streaming (unplugged): the source ends");
        assert_eq!(block_or_end(Err(RecvTimeoutError::Disconnected), || true), None);
    }

    #[test]
    fn helper_spectrum_asks_for_calls_only_when_drawn() {
        let mut rig = Rig::new(&[], false);
        rig.open(851_600_000).unwrap();
        rig.open(853_950_000).unwrap();
        let asked = AtomicUsize::new(0);
        let following = || { asked.fetch_add(1, Ordering::SeqCst); Vec::new() };
        rig.helpers[0].last = vec![0; 2 * SPECTRUM_FFT];
        draw_helpers(&mut rig.helpers, following, 1_000);
        assert_eq!(asked.load(Ordering::SeqCst), 0, "nobody watching");
        rig.helpers[0].view.lock().unwrap().watch_until_ms = 5_000;
        rig.helpers[0].last = vec![0; 2 * SPECTRUM_FFT];
        draw_helpers(&mut rig.helpers, following, 1_000);
        assert_eq!(asked.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn helper_choice_follows_saved_settings() {
        let db = Db::open_in_memory().unwrap();
        let (sdrs, _) = fakes(&[RTL1, RTL2], &[]);
        let rx = Receivers::new(sdrs);
        let id = db.save_system(&sys("SGCo", RTL1)).unwrap();
        rx.restart_all(&db);
        assert!(wait_for(Duration::from_secs(2), || state(&rx, id) == "running"));
        let free = || rx.pool.lock().unwrap().free(&rx.claimed, id);
        assert_eq!(free(), vec![RTL2.to_string()]);
        db.save_tuner(RTL2, &TunerConfig { mode: TunerMode::Disabled, ..TunerConfig::default() }).unwrap();
        rx.apply_tuner(&db, RTL2);
        assert!(free().is_empty(), "switched off: no longer a helper");
        db.save_tuner(RTL2, &TunerConfig::default()).unwrap();
        rx.apply_tuner(&db, RTL2);
        let other = db.save_system(&System { enabled: false, ..sys("KSICS", RTL2) }).unwrap();
        rx.apply(&db, other);
        assert_eq!(free(), vec![RTL2.to_string()], "pinned to a disabled system: still free");
        db.save_system(&System { id: other, ..sys("KSICS", RTL2) }).unwrap();
        rx.apply(&db, other);
        assert!(rx.pool.lock().unwrap().reserved(id).contains(&RTL2.to_string()), "pinned to an enabled system");
        assert!(!free().contains(&RTL2.to_string()));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn expected_rtl_rate_is_the_driver_rate() {
        assert_eq!(expected_rate(RTL1, &TunerConfig::default()), sdrtrunk_source::rtlsdr::RATE_HZ);
        assert_eq!(expected_rate(A, &TunerConfig { rate_hz: 3_000_000, ..TunerConfig::default() }), 3_000_000);
    }

    #[test]
    fn failing_covering_helper_falls_through() {
        let mut rig = Rig::new(&[], false);
        rig.present.push("rtl:00000003".into());
        rig.open(851_600_000).unwrap();
        rig.open(853_950_000).unwrap();
        // Its window claims 857.2 MHz but its channels sit around 853.9 MHz: adding a channel there fails.
        rig.helpers[0].win = rig.helpers[0].win.at(857_000_000);
        assert!(rig.helpers[0].win.covers(857_200_000));
        let id = rig.open(857_200_000).expect("another tuner takes it");
        assert_eq!((id, rig.helpers.len()), (2 << 24, 2), "RTL-SDR 3");
    }

    const REC: &str = "a_851012500_baseband_20261116_120000";

    /// A folder with one 48 kHz recording named `REC`, plus files replay must ignore.
    fn replay_dir(tag: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("replay_sdrs_{tag}_{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        let mut w = sdrtrunk_source::wav::WavWriter::create(&d.join(format!("{REC}.wav")), 48_000).unwrap();
        w.write(&vec![1i16; 9_600]).unwrap();
        w.finish().unwrap();
        std::fs::write(d.join("notes.txt"), "x").unwrap();
        std::fs::write(d.join("b_baseband.wav"), "x").unwrap();
        d
    }

    #[test]
    fn replay_lists_recordings() {
        let sdrs = replay_sdrs(&replay_dir("list")).unwrap();
        assert_eq!((sdrs.list)(), vec![format!("file:{REC}")]);
    }

    #[test]
    fn replay_opens_only_at_recorded_centre() {
        let sdrs = replay_sdrs(&replay_dir("open")).unwrap();
        let id = format!("file:{REC}");
        let e = (sdrs.open)(&TunerConfig::default(), 851_000_000, &id).err().unwrap();
        assert!(e.contains("851012500") && e.contains("851000000"), "{e}");
        let mut src = (sdrs.open)(&TunerConfig::default(), 851_012_500, &id).unwrap();
        assert_eq!(src.rate_hz(), 48_000);
        assert!(src.next_block(Duration::from_millis(100)).is_some_and(|b| !b.is_empty()));
        assert!(src.tune(851_012_500).is_ok());
        assert!(src.tune(852_000_000).is_err());
        assert!((sdrs.open)(&TunerConfig::default(), 851_012_500, "file:other").is_err());
    }

    /// Delivers 1 000-sample blocks at 48 kHz; ends after `blocks` of them when set.
    struct Steady { left: Option<usize> }
    impl IqSource for Steady {
        fn next_block(&mut self, _: Duration) -> Option<Vec<i16>> {
            if let Some(n) = self.left.as_mut() { if *n == 0 { return None; } *n -= 1; }
            Some(vec![5i16; 2_000])
        }
        fn tune(&mut self, _: u32) -> Result<(), String> { Ok(()) }
        fn dropped(&self) -> u64 { 0 }
        fn rate_hz(&self) -> u32 { 48_000 }
    }

    fn rec_path(tag: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!("record_{tag}_{}.wav", std::process::id()))
    }

    #[test]
    fn records_exact_length() {
        let p = rec_path("exact");
        let w = sdrtrunk_source::wav::WavWriter::create(&p, 48_000).unwrap();
        assert_eq!(record_baseband(&mut Steady { left: None }, 1, w), Ok((48_000, 0)));
        let info = sdrtrunk_source::wav::read_header(&mut std::fs::File::open(&p).unwrap()).unwrap();
        assert_eq!(info.data_len, 192_000);
    }

    /// A sink that stalls 20 ms on every block, like an SD card flushing.
    struct Slow(usize);
    impl Sink for Slow {
        fn put(&mut self, iq: &[i16]) -> std::io::Result<()> { std::thread::sleep(Duration::from_millis(20)); self.0 += iq.len() / 2; Ok(()) }
        fn finish(self) -> std::io::Result<u64> { Ok(self.0 as u64) }
    }

    /// Steady, remembering when each block was taken.
    struct Timed { taken: Vec<Instant> }
    impl IqSource for Timed {
        fn next_block(&mut self, _: Duration) -> Option<Vec<i16>> { self.taken.push(Instant::now()); Some(vec![5i16; 2_000]) }
        fn tune(&mut self, _: u32) -> Result<(), String> { Ok(()) }
        fn dropped(&self) -> u64 { 0 }
        fn rate_hz(&self) -> u32 { 48_000 }
    }

    /// A sink whose `finish` (the file's final flush) raises `done`; the source reports drops only after that.
    struct Flushing(Arc<AtomicBool>);
    impl Sink for Flushing {
        fn put(&mut self, _: &[i16]) -> std::io::Result<()> { Ok(()) }
        fn finish(self) -> std::io::Result<u64> { self.0.store(true, Ordering::SeqCst); Ok(48_000) }
    }
    struct DropsLater(Arc<AtomicBool>);
    impl IqSource for DropsLater {
        fn next_block(&mut self, _: Duration) -> Option<Vec<i16>> { Some(vec![5i16; 2_000]) }
        fn tune(&mut self, _: u32) -> Result<(), String> { Ok(()) }
        fn dropped(&self) -> u64 { if self.0.load(Ordering::SeqCst) { 999 } else { 0 } }
        fn rate_hz(&self) -> u32 { 48_000 }
    }

    #[test]
    fn refuses_a_wav_too_big_for_sdrtrunk() {
        // 48 kHz x 4 bytes: 22 369 s is the most one WAV sdrtrunk plays can hold (4 294 967 294 bytes).
        let e = record_baseband(&mut Steady { left: None }, 22_370, Slow(0)).unwrap_err();
        assert!(e.contains("22369"), "{e}");
        assert_eq!(max_record_seconds(6_000_000), 178);
    }

    #[test]
    fn drops_after_the_recording_do_not_count() {
        let done = Arc::new(AtomicBool::new(false));
        let r = record_baseband(&mut DropsLater(done.clone()), 1, Flushing(done));
        assert_eq!(r, Ok((48_000, 0)), "the tuner keeps streaming while the file is flushed; those drops are not the recording's");
    }

    #[test]
    fn slow_disk_does_not_hold_up_the_tuner() {
        let mut src = Timed { taken: Vec::new() };
        let (got, sink) = record_to(&mut src, 1, Slow(0));
        assert_eq!((got, sink.0), (Ok(48_000), 48_000), "every sample still written");
        let span = *src.taken.last().unwrap() - src.taken[0];
        assert!(span < Duration::from_millis(300), "48 blocks taken in {span:?}; a 20 ms write each would take ~1 s");
    }

    #[test]
    fn device_end_keeps_a_valid_file() {
        let p = rec_path("ended");
        let w = sdrtrunk_source::wav::WavWriter::create(&p, 48_000).unwrap();
        let e = record_baseband(&mut Steady { left: Some(5) }, 1, w).unwrap_err();
        assert!(e.contains("ended"), "{e}");
        let info = sdrtrunk_source::wav::read_header(&mut std::fs::File::open(&p).unwrap()).unwrap();
        assert_eq!(info.data_len, 5 * 4_000);
        let bytes = std::fs::read(&p).unwrap();
        assert_eq!(u32::from_le_bytes(bytes[4..8].try_into().unwrap()) as usize, bytes.len() - 8);
    }

    #[test]
    fn replay_needs_a_recording() {
        let empty = std::env::temp_dir().join(format!("replay_sdrs_empty_{}", std::process::id()));
        std::fs::create_dir_all(&empty).unwrap();
        let e = replay_sdrs(&empty).err().unwrap();
        assert!(e.contains(&empty.display().to_string()), "{e}");
        assert!(replay_sdrs(&empty.join("missing")).is_err());
    }
}
