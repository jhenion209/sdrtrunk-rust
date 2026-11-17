//! sdrtrunk's baseband recordings: WAV, PCM signed 16-bit little-endian, 2 channels (I, Q), sample rate = tuner rate,
//! named `<prefix>_<centerHz>_baseband_<yyyyMMdd>_<HHmmss>.wav` (the Java reference's `RecordingTunerFileCatalog`).
use std::fs::File;
use std::io::{self, BufWriter, Read, Seek, SeekFrom, Write};
use std::path::Path;

/// Where the I/Q samples of a recording are: the sample rate, and the byte offset and length of the data chunk.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WavInfo { pub rate_hz: u32, pub data_start: u64, pub data_len: u64 }

/// Reads a recording's header, walking its chunks; anything but PCM 16-bit stereo is an error.
pub fn read_header<R: Read + Seek>(r: &mut R) -> Result<WavInfo, String> {
    let mut head = [0u8; 12];
    r.read_exact(&mut head).map_err(|e| format!("not a WAV file: {e}"))?;
    if &head[0..4] != b"RIFF" || &head[8..12] != b"WAVE" { return Err("not a WAV file".into()); }
    let mut rate = None;
    loop {
        let mut ch = [0u8; 8];
        r.read_exact(&mut ch).map_err(|_| "no data chunk".to_string())?;
        let size = u32::from_le_bytes(ch[4..8].try_into().unwrap()) as u64;
        let at = r.stream_position().map_err(|e| e.to_string())?;
        match &ch[0..4] {
            b"fmt " => {
                let mut f = [0u8; 16];
                r.read_exact(&mut f).map_err(|_| "short fmt chunk".to_string())?;
                let (format, channels, bits) = (u16::from_le_bytes([f[0], f[1]]), u16::from_le_bytes([f[2], f[3]]), u16::from_le_bytes([f[14], f[15]]));
                if format != 1 || channels != 2 || bits != 16 {
                    return Err(format!("need 16-bit stereo PCM I/Q, found format {format}, {channels} channels, {bits}-bit"));
                }
                rate = Some(u32::from_le_bytes(f[4..8].try_into().unwrap()));
            }
            b"data" => {
                let rate_hz = rate.ok_or("data chunk before fmt chunk")?;
                // A size never filled in (recording cut short) or past the end of the file: the rest of the file.
                let end = r.seek(SeekFrom::End(0)).map_err(|e| e.to_string())?;
                let rest = end.saturating_sub(at);
                let data_len = if size == 0 || size > rest { rest } else { size };
                return Ok(WavInfo { rate_hz, data_start: at, data_len });
            }
            _ => {}
        }
        r.seek(SeekFrom::Start(at + size + (size & 1))).map_err(|e| e.to_string())?;
    }
}

/// Writes a recording: the header first with zero sizes, filled in by `finish`.
pub struct WavWriter { out: BufWriter<File>, bytes: u64 }

impl WavWriter {
    pub fn create(path: &Path, rate_hz: u32) -> io::Result<WavWriter> {
        let mut out = BufWriter::with_capacity(1 << 20, File::create(path)?);
        let mut h = Vec::with_capacity(44);
        h.extend_from_slice(b"RIFF\0\0\0\0WAVEfmt ");
        for v in [16u32] { h.extend_from_slice(&v.to_le_bytes()); }
        for v in [1u16, 2] { h.extend_from_slice(&v.to_le_bytes()); }
        for v in [rate_hz, rate_hz * 4] { h.extend_from_slice(&v.to_le_bytes()); }
        for v in [4u16, 16] { h.extend_from_slice(&v.to_le_bytes()); }
        h.extend_from_slice(b"data\0\0\0\0");
        out.write_all(&h)?;
        Ok(WavWriter { out, bytes: 0 })
    }

    /// Appends interleaved I/Q samples.
    pub fn write(&mut self, iq: &[i16]) -> io::Result<()> {
        let mut b = Vec::with_capacity(iq.len() * 2);
        for s in iq { b.extend_from_slice(&s.to_le_bytes()); }
        self.out.write_all(&b)?;
        self.bytes += b.len() as u64;
        Ok(())
    }

    /// Fills in the RIFF and data sizes; returns the complex samples written.
    pub fn finish(self) -> io::Result<u64> {
        let bytes = self.bytes;
        let mut f = self.out.into_inner().map_err(|e| e.into_error())?;
        f.seek(SeekFrom::Start(4))?;
        f.write_all(&((36 + bytes) as u32).to_le_bytes())?;
        f.seek(SeekFrom::Start(40))?;
        f.write_all(&(bytes as u32).to_le_bytes())?;
        f.sync_all()?;
        Ok(bytes / 4)
    }
}

/// sdrtrunk's name for a recording; `stamp` is `yyyyMMdd_HHmmss`.
pub fn baseband_file_name(prefix: &str, center_hz: u32, stamp: &str) -> String {
    format!("{prefix}_{center_hz}_baseband_{stamp}.wav")
}

/// The centre frequency in a recording's name, as the Java catalog reads it: `.*_(\d+)_baseband_\d{8}_\d{6}\.wav`.
pub fn center_from_name(name: &str) -> Option<u32> {
    let lower = name.to_ascii_lowercase();
    let stem = lower.strip_suffix(".wav")?;
    let (head, stamp) = stem.rsplit_once("_baseband_")?;
    let (date, time) = stamp.split_once('_')?;
    let digits = |s: &str, n: usize| s.len() == n && s.bytes().all(|b| b.is_ascii_digit());
    if !digits(date, 8) || !digits(time, 6) { return None; }
    let (_, hz) = head.rsplit_once('_')?;
    if hz.is_empty() || !hz.bytes().all(|b| b.is_ascii_digit()) { return None; }
    hz.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn tmp(name: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!("{name}_{}_{:?}.wav", std::process::id(), std::thread::current().id()))
    }

    /// A WAV built by hand: `fmt` with these fields, then `extra` chunks, then a data chunk of `data` bytes.
    fn wav(format: u16, channels: u16, bits: u16, extra: &[(&[u8; 4], &[u8])], data: usize) -> Vec<u8> {
        let mut body = Vec::new();
        body.extend_from_slice(b"WAVE");
        body.extend_from_slice(b"fmt ");
        body.extend_from_slice(&16u32.to_le_bytes());
        body.extend_from_slice(&format.to_le_bytes());
        body.extend_from_slice(&channels.to_le_bytes());
        body.extend_from_slice(&48_000u32.to_le_bytes());
        body.extend_from_slice(&(48_000u32 * channels as u32 * bits as u32 / 8).to_le_bytes());
        body.extend_from_slice(&(channels * bits / 8).to_le_bytes());
        body.extend_from_slice(&bits.to_le_bytes());
        for (id, bytes) in extra {
            body.extend_from_slice(*id);
            body.extend_from_slice(&(bytes.len() as u32).to_le_bytes());
            body.extend_from_slice(bytes);
        }
        body.extend_from_slice(b"data");
        body.extend_from_slice(&(data as u32).to_le_bytes());
        body.extend(std::iter::repeat(0u8).take(data));
        let mut out = b"RIFF".to_vec();
        out.extend_from_slice(&(body.len() as u32).to_le_bytes());
        out.extend(body);
        out
    }

    #[test]
    fn round_trip() {
        let p = tmp("wav_rt");
        let mut w = WavWriter::create(&p, 2_400_000).unwrap();
        w.write(&[1, -1, 2, -2]).unwrap();
        w.write(&[3, -3]).unwrap();
        assert_eq!(w.finish().unwrap(), 3);
        let info = read_header(&mut std::fs::File::open(&p).unwrap()).unwrap();
        assert_eq!((info.rate_hz, info.data_start, info.data_len), (2_400_000, 44, 12));
        assert_eq!(std::fs::metadata(&p).unwrap().len(), 56);
        let bytes = std::fs::read(&p).unwrap();
        assert_eq!(u32::from_le_bytes(bytes[4..8].try_into().unwrap()), 48, "RIFF size = file length - 8");
        assert_eq!(&bytes[44..48], &[1, 0, 0xFF, 0xFF], "I then Q, little-endian");
        std::fs::remove_file(p).ok();
    }

    #[test]
    fn skips_extra_chunks() {
        let info = read_header(&mut Cursor::new(wav(1, 2, 16, &[(b"LIST", &[1, 2, 3, 4])], 8))).unwrap();
        assert_eq!((info.rate_hz, info.data_start, info.data_len), (48_000, 56, 8));
    }

    #[test]
    fn data_size_unset_or_too_big_means_rest_of_file() {
        // A recording cut short (Ctrl-C) never got its sizes filled in; a damaged one claims more than it holds.
        for claimed in [0u32, 1_000] {
            let mut b = wav(1, 2, 16, &[], 8);
            b[40..44].copy_from_slice(&claimed.to_le_bytes());
            let info = read_header(&mut Cursor::new(b)).unwrap();
            assert_eq!((info.data_start, info.data_len), (44, 8), "claimed {claimed}");
        }
    }

    #[test]
    fn rejects_other_formats() {
        for (bytes, why) in [
            (wav(1, 1, 16, &[], 8), "mono"),
            (wav(1, 2, 8, &[], 8), "8-bit"),
            (wav(3, 2, 16, &[], 8), "float"),
            (wav(1, 2, 16, &[], 8)[..36].to_vec(), "no data chunk"),
            (b"RIFX\0\0\0\0WAVE".to_vec(), "not RIFF"),
        ] {
            assert!(read_header(&mut Cursor::new(bytes)).is_err(), "{why}");
        }
    }

    #[test]
    fn names() {
        assert_eq!(baseband_file_name("Airspy_35AC63DC-2D40B04F", 851_012_500, "20261116_120000"),
            "Airspy_35AC63DC-2D40B04F_851012500_baseband_20261116_120000.wav");
        assert_eq!(center_from_name("x_851012500_baseband_20261116_120000.WAV"), Some(851_012_500));
        assert_eq!(center_from_name("RTL-SDR_00000001_769500000_baseband_20261116_120000.wav"), Some(769_500_000));
        for bad in ["x_851012500_baseband_2026111_120000.wav", "x_baseband_20261116_120000.wav",
            "x_851012500_baseband_20261116_120000.mp3", "851012500_baseband_20261116_120000.wav"] {
            assert_eq!(center_from_name(bad), None, "{bad}");
        }
    }
}
