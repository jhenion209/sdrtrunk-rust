//! RTL-SDR dongles through librtlsdr, loaded at run time: Debian/Raspberry Pi OS ship `librtlsdr0` (.so.0), Ubuntu 24.04
//! `librtlsdr2` (.so.2). A host without it still runs; it just lists no RTL-SDR. Samples come out as interleaved int16
//! IQ on the Airspy's 12-bit scale (±2048), so the decoder treats both alike.
use std::ffi::{c_char, c_int, c_void, CStr};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{sync_channel, Receiver, TrySendError};
use std::sync::{Arc, OnceLock};
use std::thread::JoinHandle;

/// Tuner ids of RTL-SDRs start with this ("rtl:00000001"); bare ids are Airspy serials.
pub const PREFIX: &str = "rtl:";
/// The RTL2832U runs reliably up to about 2.4 MS/s (sdrtrunk's default for it).
pub const RATE_HZ: u32 = 2_400_000;
/// Bytes per read: 64 KiB of IQ pairs, ~27 ms at 2.4 MS/s (a multiple of 512 as librtlsdr needs).
const READ_BYTES: usize = 131_072;

type Dev = c_void;

struct Lib {
    count: unsafe extern "C" fn() -> u32,
    strings: unsafe extern "C" fn(u32, *mut c_char, *mut c_char, *mut c_char) -> c_int,
    open: unsafe extern "C" fn(*mut *mut Dev, u32) -> c_int,
    close: unsafe extern "C" fn(*mut Dev) -> c_int,
    set_freq: unsafe extern "C" fn(*mut Dev, u32) -> c_int,
    set_rate: unsafe extern "C" fn(*mut Dev, u32) -> c_int,
    gain_mode: unsafe extern "C" fn(*mut Dev, c_int) -> c_int,
    set_gain: unsafe extern "C" fn(*mut Dev, c_int) -> c_int,
    reset: unsafe extern "C" fn(*mut Dev) -> c_int,
    read_sync: unsafe extern "C" fn(*mut Dev, *mut c_void, c_int, *mut c_int) -> c_int,
}

fn lib() -> Option<&'static Lib> {
    static LIB: OnceLock<Option<Lib>> = OnceLock::new();
    LIB.get_or_init(|| {
        // SAFETY: dlopen/dlsym with NUL-terminated names; each symbol is cast to librtlsdr's documented signature.
        unsafe {
            let h = [c"librtlsdr.so.2", c"librtlsdr.so.0", c"librtlsdr.so"].iter()
                .map(|n| libc::dlopen(n.as_ptr(), libc::RTLD_NOW)).find(|h| !h.is_null())?;
            macro_rules! sym {
                ($name:literal) => {{
                    let p = libc::dlsym(h, concat!($name, "\0").as_ptr() as *const c_char);
                    if p.is_null() { return None; }
                    std::mem::transmute::<*mut c_void, _>(p)
                }};
            }
            Some(Lib {
                count: sym!("rtlsdr_get_device_count"),
                strings: sym!("rtlsdr_get_device_usb_strings"),
                open: sym!("rtlsdr_open"),
                close: sym!("rtlsdr_close"),
                set_freq: sym!("rtlsdr_set_center_freq"),
                set_rate: sym!("rtlsdr_set_sample_rate"),
                gain_mode: sym!("rtlsdr_set_tuner_gain_mode"),
                set_gain: sym!("rtlsdr_set_tuner_gain"),
                reset: sym!("rtlsdr_reset_buffer"),
                read_sync: sym!("rtlsdr_read_sync"),
            })
        }
    }).as_ref()
}

/// USB serial of dongle `index` (empty if it has none).
fn serial(l: &Lib, index: u32) -> String {
    let (mut m, mut p, mut s) = ([0 as c_char; 256], [0 as c_char; 256], [0 as c_char; 256]);
    // SAFETY: librtlsdr writes at most 256 bytes (NUL-terminated) into each buffer.
    if unsafe { (l.strings)(index, m.as_mut_ptr(), p.as_mut_ptr(), s.as_mut_ptr()) } != 0 { return String::new(); }
    unsafe { CStr::from_ptr(s.as_ptr()) }.to_string_lossy().trim().to_string()
}

/// Tuner ids of the connected dongles ("rtl:00000001"). A dongle without a serial is listed by index ("rtl:#0").
// ponytail: two dongles with the same serial (many ship as 00000001) look like one; give them distinct serials with rtl_eeprom.
pub fn list() -> Vec<String> {
    let Some(l) = lib() else { return Vec::new() };
    // SAFETY: plain query.
    let n = unsafe { (l.count)() };
    (0..n).map(|i| { let s = serial(l, i); format!("{PREFIX}{}", if s.is_empty() { format!("#{i}") } else { s }) }).collect()
}

fn index_of(l: &Lib, id: &str) -> Option<u32> {
    let want = id.strip_prefix(PREFIX)?;
    if let Some(i) = want.strip_prefix('#') { return i.parse().ok(); }
    // SAFETY: plain query.
    (0..unsafe { (l.count)() }).find(|&i| serial(l, i) == want)
}

struct DevPtr(*mut Dev);
// SAFETY: librtlsdr allows retuning from one thread while another reads (rtl_tcp does the same).
unsafe impl Send for DevPtr {}
unsafe impl Sync for DevPtr {}

pub struct RtlSdr {
    dev: Arc<DevPtr>,
    stop: Arc<AtomicBool>,
    reader: Option<JoinHandle<()>>,
    pub blocks: Receiver<Vec<i16>>,
    /// IQ samples lost because the consumer fell behind.
    pub dropped: Arc<AtomicU64>,
}

impl RtlSdr {
    /// Opens the dongle with this tuner id ("rtl:SERIAL"), tunes it and starts reading at `RATE_HZ`.
    /// `gain_db`: tuner gain (librtlsdr picks the nearest step the tuner has), or None for automatic gain.
    pub fn open(id: &str, freq_hz: u32, gain_db: Option<f64>) -> Result<Self, String> {
        let l = lib().ok_or("librtlsdr is not installed (apt install librtlsdr0)")?;
        let index = index_of(l, id).ok_or_else(|| format!("{id} not found"))?;
        let mut dev = std::ptr::null_mut();
        // SAFETY: plain librtlsdr calls on a handle we own; it is closed on every error path.
        unsafe {
            let r = (l.open)(&mut dev, index);
            if r != 0 { return Err(format!("rtlsdr_open failed with code {r} (is the DVB driver blacklisted?)")); }
            let gain = match gain_db {
                None => (l.gain_mode)(dev, 0),
                Some(db) => match (l.gain_mode)(dev, 1) { 0 => (l.set_gain)(dev, (db * 10.0).round() as c_int), r => r },
            };
            let setup = [(l.set_rate)(dev, RATE_HZ), (l.set_freq)(dev, freq_hz), gain, (l.reset)(dev)];
            if let Some(r) = setup.iter().find(|&&r| r != 0) {
                (l.close)(dev);
                return Err(format!("RTL-SDR setup failed with code {r}"));
            }
        }
        let dev = Arc::new(DevPtr(dev));
        let stop = Arc::new(AtomicBool::new(false));
        let dropped = Arc::new(AtomicU64::new(0));
        // ponytail: 64 blocks (~1.7 s) of slack; samples beyond that are counted in `dropped`.
        let (tx, blocks) = sync_channel(64);
        let (d, s, dr) = (dev.clone(), stop.clone(), dropped.clone());
        let read = l.read_sync;
        let reader = std::thread::Builder::new().name("rtlsdr".into()).spawn(move || {
            let mut buf = vec![0u8; READ_BYTES];
            while !s.load(Ordering::Relaxed) {
                let mut n: c_int = 0;
                // SAFETY: buf holds READ_BYTES bytes; the device stays open until this thread is joined.
                if unsafe { read(d.0, buf.as_mut_ptr() as *mut c_void, READ_BYTES as c_int, &mut n) } != 0 { break; }
                let block: Vec<i16> = buf[..n.max(0) as usize].iter().map(|&b| (b as i16 - 128) * 16).collect();
                if let Err(TrySendError::Full(b)) = tx.try_send(block) { dr.fetch_add(b.len() as u64 / 2, Ordering::Relaxed); }
            }
        }).map_err(|e| e.to_string())?;
        Ok(RtlSdr { dev, stop, reader: Some(reader), blocks, dropped })
    }

    pub fn set_frequency(&self, freq_hz: u32) -> Result<(), String> {
        let l = lib().ok_or("librtlsdr is not installed")?;
        // SAFETY: valid open handle.
        match unsafe { (l.set_freq)(self.dev.0, freq_hz) } { 0 => Ok(()), r => Err(format!("rtlsdr_set_center_freq failed with code {r}")) }
    }
}

impl Drop for RtlSdr {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        // The reader finishes its current read (~27 ms), then the device is closed.
        if let Some(r) = self.reader.take() { let _ = r.join(); }
        // SAFETY: the reader has stopped; nothing else uses the handle.
        if let Some(l) = lib() { unsafe { (l.close)(self.dev.0); } }
    }
}
