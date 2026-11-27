//! Airspy R2/Mini receiver through libairspy (Debian/Raspberry Pi OS package `libairspy0`, the same library
//! `airspy_rx` uses). Samples come out as interleaved int16 IQ, like `airspy_rx -t 2`.
//! Linux only: links `libairspy.so.0` (see build.rs), so no `-dev` package is needed.
use std::ffi::{c_int, c_void};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{sync_channel, Receiver, SyncSender, TrySendError};
use std::sync::Arc;

#[repr(C)]
struct Device { _private: [u8; 0] }

#[repr(C)]
struct Transfer {
    device: *mut Device,
    ctx: *mut c_void,
    samples: *mut c_void,
    sample_count: c_int,
    dropped_samples: u64,
    sample_type: c_int,
}

const SAMPLE_INT16_IQ: c_int = 2;

extern "C" {
    fn airspy_init() -> c_int;
    fn airspy_exit() -> c_int;
    fn airspy_open_sn(device: *mut *mut Device, serial: u64) -> c_int;
    fn airspy_list_devices(serials: *mut u64, count: c_int) -> c_int;
    fn airspy_close(device: *mut Device) -> c_int;
    fn airspy_set_sample_type(device: *mut Device, sample_type: c_int) -> c_int;
    fn airspy_set_samplerate(device: *mut Device, samplerate: u32) -> c_int;
    fn airspy_set_freq(device: *mut Device, freq_hz: u32) -> c_int;
    fn airspy_set_linearity_gain(device: *mut Device, value: u8) -> c_int;
    fn airspy_start_rx(device: *mut Device, cb: extern "C" fn(*mut Transfer) -> c_int, ctx: *mut c_void) -> c_int;
    fn airspy_stop_rx(device: *mut Device) -> c_int;
    fn airspy_is_streaming(device: *mut Device) -> c_int;
}

struct Ctx {
    tx: SyncSender<Vec<i16>>,
    /// IQ samples lost: dropped by the library, or because the consumer fell behind.
    dropped: Arc<AtomicU64>,
}

extern "C" fn on_samples(t: *mut Transfer) -> c_int {
    // SAFETY: libairspy passes a valid transfer whose ctx is the Ctx we registered, and `samples` holds
    // `sample_count` IQ pairs of int16 (INT16_IQ). The Ctx outlives streaming (stopped in Drop before it is freed).
    unsafe {
        let t = &*t;
        let ctx = &*(t.ctx as *const Ctx);
        let n = t.sample_count.max(0) as usize;
        if t.dropped_samples > 0 { ctx.dropped.fetch_add(t.dropped_samples, Ordering::Relaxed); }
        let block = std::slice::from_raw_parts(t.samples as *const i16, 2 * n).to_vec();
        if let Err(TrySendError::Full(_)) = ctx.tx.try_send(block) {
            ctx.dropped.fetch_add(n as u64, Ordering::Relaxed);
        }
    }
    0
}

#[derive(Debug)]
pub struct AirspyError { pub call: &'static str, pub code: i32 }

impl std::fmt::Display for AirspyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} failed with libairspy code {}", self.call, self.code)
    }
}
impl std::error::Error for AirspyError {}

fn check(call: &'static str, code: c_int) -> Result<(), AirspyError> {
    if code == 0 { Ok(()) } else { Err(AirspyError { call, code }) }
}

pub struct Airspy {
    dev: *mut Device,
    ctx: *mut Ctx,
    pub blocks: Receiver<Vec<i16>>,
    pub dropped: Arc<AtomicU64>,
}

/// Serial numbers of the connected Airspys, in sdrtrunk's form ("735863DC-2F67AFE3").
pub fn list() -> Vec<String> {
    let mut sn = [0u64; 16];
    // SAFETY: libairspy writes at most `count` serials into the buffer and returns how many it found.
    let n = unsafe { airspy_list_devices(sn.as_mut_ptr(), sn.len() as c_int) };
    sn[..n.clamp(0, sn.len() as c_int) as usize].iter().map(|&s| crate::format_serial(s)).collect()
}

impl Airspy {
    /// Opens the Airspy with this serial (`airspy_list_devices` form, see `list`) and starts streaming.
    /// `rate_hz`: one of `AIRSPY_RATES` (the R2 lists 10e6 and 2.5e6, the Mini 6e6 and 3e6; libairspy decides what else a
    /// device takes). `linearity_gain` is libairspy's 0..=21 gain.
    pub fn open(serial: u64, freq_hz: u32, rate_hz: u32, linearity_gain: u8) -> Result<Self, AirspyError> {
        // ponytail: 64 blocks (~0.7 s at 6 MS/s) of slack; samples beyond that are counted in `dropped`.
        let (tx, blocks) = sync_channel(64);
        let dropped = Arc::new(AtomicU64::new(0));
        let ctx = Box::into_raw(Box::new(Ctx { tx, dropped: dropped.clone() }));
        let mut dev = std::ptr::null_mut();
        // SAFETY: plain libairspy calls on a device handle we own; on any error the handle is closed below.
        unsafe {
            check("airspy_init", airspy_init())?;
            if let Err(e) = check("airspy_open_sn", airspy_open_sn(&mut dev, serial)) {
                airspy_exit();
                drop(Box::from_raw(ctx));
                return Err(e);
            }
            let me = Airspy { dev, ctx, blocks, dropped };
            check("airspy_set_sample_type", airspy_set_sample_type(dev, SAMPLE_INT16_IQ))?;
            check("airspy_set_samplerate", airspy_set_samplerate(dev, rate_hz))?;
            check("airspy_set_freq", airspy_set_freq(dev, freq_hz))?;
            check("airspy_set_linearity_gain", airspy_set_linearity_gain(dev, linearity_gain))?;
            check("airspy_start_rx", airspy_start_rx(dev, on_samples, ctx as *mut c_void))?;
            Ok(me)
        }
    }

    /// False once streaming stopped, e.g. the device was unplugged (libairspy stops on a failed USB transfer; the
    /// sample channel stays open, so only this shows it).
    pub fn is_streaming(&self) -> bool {
        // SAFETY: valid open handle; only reads the device's streaming flags.
        unsafe { airspy_is_streaming(self.dev) == 1 }
    }

    pub fn set_frequency(&self, freq_hz: u32) -> Result<(), AirspyError> {
        // SAFETY: valid open handle.
        check("airspy_set_freq", unsafe { airspy_set_freq(self.dev, freq_hz) })
    }
}

impl Drop for Airspy {
    fn drop(&mut self) {
        // SAFETY: stop streaming before freeing the callback context.
        unsafe {
            airspy_stop_rx(self.dev);
            airspy_close(self.dev);
            airspy_exit();
            drop(Box::from_raw(self.ctx));
        }
    }
}
