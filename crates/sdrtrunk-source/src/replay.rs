//! A baseband recording played back as a tuner: blocks of interleaved I/Q at the recording's own rate, paced by the
//! wall clock and looping at the end, like the Java reference's `ComplexWaveSource` with auto-replay.
use std::fs::File;
use std::io::{BufReader, Read, Seek, SeekFrom};
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{sync_channel, Receiver, TrySendError};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::wav::{center_from_name, read_header};

/// Blocks waiting for the reader, as with a USB tuner's queue; when it is full, the next due block is dropped.
const QUEUE_BLOCKS: usize = 64;

/// A recording being played. `blocks` are 10 ms of interleaved I/Q each; `dropped` counts complex samples the reader
/// did not take in time. Dropping the `Replay` stops playback.
pub struct Replay {
    pub blocks: Receiver<Vec<i16>>,
    pub dropped: Arc<AtomicU64>,
    pub rate_hz: u32,
    pub center_hz: u32,
    stop: Arc<AtomicBool>,
}

impl Replay {
    /// Starts playing `path`, whose name carries its centre frequency (sdrtrunk's baseband name).
    pub fn open(path: &Path) -> Result<Replay, String> {
        let name = path.file_name().and_then(|n| n.to_str()).unwrap_or_default();
        let center_hz = center_from_name(name)
            .ok_or_else(|| format!("{name}: no centre frequency in the name (expected NAME_HZ_baseband_yyyyMMdd_HHmmss.wav)"))?;
        let mut file = File::open(path).map_err(|e| format!("{}: {e}", path.display()))?;
        let info = read_header(&mut file).map_err(|e| format!("{name}: {e}"))?;
        let usable = info.data_len / 4 * 4;
        if usable == 0 || info.rate_hz == 0 { return Err(format!("{name}: no samples")); }
        let (tx, blocks) = sync_channel(QUEUE_BLOCKS);
        let (dropped, stop) = (Arc::new(AtomicU64::new(0)), Arc::new(AtomicBool::new(false)));
        let (d, s) = (dropped.clone(), stop.clone());
        let pairs = (info.rate_hz / 100).max(1) as usize;
        std::thread::Builder::new().name("replay".into()).spawn(move || {
            let mut r = BufReader::with_capacity(1 << 20, file);
            if r.seek(SeekFrom::Start(info.data_start)).is_err() { return; }
            let mut pos = 0u64;
            let mut bytes = vec![0u8; pairs * 4];
            let start = Instant::now();
            for k in 0u64.. {
                if s.load(Ordering::Relaxed) { return; }
                // Fill one block, going back to the start of the data at its end (whole pairs only).
                let mut got = 0;
                while got < bytes.len() {
                    if pos == usable {
                        if r.seek(SeekFrom::Start(info.data_start)).is_err() { return; }
                        pos = 0;
                    }
                    let n = ((bytes.len() - got) as u64).min(usable - pos) as usize;
                    if r.read_exact(&mut bytes[got..got + n]).is_err() { return; }
                    got += n;
                    pos += n as u64;
                }
                let block: Vec<i16> = bytes.chunks_exact(2).map(|b| i16::from_le_bytes([b[0], b[1]])).collect();
                let due = start + Duration::from_secs_f64(k as f64 * pairs as f64 / info.rate_hz as f64);
                if let Some(wait) = due.checked_duration_since(Instant::now()) { std::thread::sleep(wait); }
                match tx.try_send(block) {
                    Ok(()) => {}
                    Err(TrySendError::Full(_)) => { d.fetch_add(pairs as u64, Ordering::Relaxed); }
                    Err(TrySendError::Disconnected(_)) => return,
                }
            }
        }).map_err(|e| e.to_string())?;
        Ok(Replay { blocks, dropped, rate_hz: info.rate_hz, center_hz, stop })
    }
}

impl Drop for Replay {
    fn drop(&mut self) { self.stop.store(true, Ordering::Relaxed); }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    const NAME: &str = "t_851012500_baseband_20261116_120000.wav";

    fn dir(tag: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("replay_{tag}_{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    /// A 48 kHz recording of `pairs` samples with I = index, Q = -I, plus `extra` trailing bytes in the data chunk.
    fn recording(tag: &str, name: &str, pairs: usize, extra: &[u8]) -> std::path::PathBuf {
        let mut data = Vec::new();
        for i in 0..pairs as i16 { data.extend_from_slice(&i.to_le_bytes()); data.extend_from_slice(&(-i).to_le_bytes()); }
        data.extend_from_slice(extra);
        let mut b = b"RIFF".to_vec();
        b.extend_from_slice(&(36 + data.len() as u32).to_le_bytes());
        b.extend_from_slice(b"WAVEfmt ");
        b.extend_from_slice(&16u32.to_le_bytes());
        for v in [1u16, 2] { b.extend_from_slice(&v.to_le_bytes()); }
        for v in [48_000u32, 192_000] { b.extend_from_slice(&v.to_le_bytes()); }
        for v in [4u16, 16] { b.extend_from_slice(&v.to_le_bytes()); }
        b.extend_from_slice(b"data");
        b.extend_from_slice(&(data.len() as u32).to_le_bytes());
        b.extend(data);
        let p = dir(tag).join(name);
        std::fs::write(&p, b).unwrap();
        p
    }

    /// Everything received within `ms` milliseconds.
    fn collect(r: &Replay, ms: u64) -> Vec<Vec<i16>> {
        let end = Instant::now() + Duration::from_millis(ms);
        let mut out = Vec::new();
        while let Some(left) = end.checked_duration_since(Instant::now()) {
            if let Ok(b) = r.blocks.recv_timeout(left) { out.push(b); }
        }
        out
    }

    #[test]
    fn paced_and_looping() {
        let r = Replay::open(&recording("pace", NAME, 2_400, &[])).unwrap();
        assert_eq!((r.rate_hz, r.center_hz), (48_000, 851_012_500));
        let blocks = collect(&r, 300);
        let pairs: usize = blocks.iter().map(|b| b.len() / 2).sum();
        assert!((13_000..=16_000).contains(&pairs), "{pairs} samples in 300 ms at 48 kHz");
        let is: Vec<i16> = blocks.iter().flat_map(|b| b.iter().step_by(2).copied()).collect();
        let wraps = is.windows(2).filter(|w| w[1] < w[0]).count();
        assert!(wraps >= 4, "looped {wraps} times");
        assert!(is.windows(2).all(|w| w[1] == w[0] + 1 || (w[0] == 2_399 && w[1] == 0)), "no gaps or repeats");
    }

    #[test]
    fn slow_consumer_drops_whole_blocks() {
        let r = Replay::open(&recording("slow", NAME, 20_000, &[])).unwrap();
        std::thread::sleep(Duration::from_millis(1_000));
        let dropped = r.dropped.load(std::sync::atomic::Ordering::Relaxed);
        assert!(dropped > 0 && dropped % 480 == 0, "dropped {dropped}");
        while r.blocks.try_recv().is_ok() {}
        let pairs: usize = collect(&r, 200).iter().map(|b| b.len() / 2).sum();
        assert!((7_000..=12_000).contains(&pairs), "{pairs} samples in 200 ms after draining: no catch-up burst");
    }

    #[test]
    fn never_splits_a_pair() {
        let r = Replay::open(&recording("odd", NAME, 1_000, &[7, 7, 7])).unwrap();
        for b in collect(&r, 150) {
            assert_eq!(b.len() % 2, 0);
            assert!(b.chunks(2).all(|p| p[1] == -p[0]), "I/Q stay paired across the loop");
        }
    }

    #[test]
    fn needs_centre_and_samples() {
        let e = Replay::open(&recording("nocentre", "plain.wav", 100, &[])).err().unwrap();
        assert!(e.contains("centre"), "{e}");
        assert!(Replay::open(&recording("empty", NAME, 0, &[])).is_err());
        assert!(Replay::open(&dir("missing").join(NAME.replace("t_", "gone_"))).is_err());
    }
}
