//! P25 Phase 1 voice: IMBE frames (144 bits from an LDU) -> 8 kHz 16-bit audio, using the bundled mbelib, with
//! JMBE's speech-gated automatic gain control.
pub mod agc;
pub mod mp3;

use agc::{Agc, FrameInfo};
use std::ffi::{c_char, c_int, c_void};
use std::io::Write;

/// csrc/shim.c `sdr_frame_info`.
#[repr(C)]
struct MbeFrameInfo { w0: f32, l: c_int, repeat: c_int, vl: [c_int; 57] }

extern "C" {
    fn sdr_mbe_new() -> *mut c_void;
    fn sdr_mbe_free(p: *mut c_void);
    fn sdr_mbe_imbe(p: *mut c_void, frame: *mut [c_char; 23], out: *mut f32, info: *mut MbeFrameInfo) -> c_int;
}

/// mbelib's own output gain (`mbe_floattoshort`), applied before the AGC.
const MBELIB_GAIN: f32 = 7.0;

/// Samples per IMBE frame (20 ms at 8 kHz).
pub const FRAME_SAMPLES: usize = 160;
pub const SAMPLE_RATE: u32 = 8000;

// P25 Phase 1 IMBE interleave schedule (dsd p25p1_const.h, ISC): received dibit n carries
// bit 1 -> frame[IW[n]][IX[n]] and bit 0 -> frame[IY[n]][IZ[n]].
const IW: [u8; 72] = [0, 2, 4, 1, 3, 5, 0, 2, 4, 1, 3, 6, 0, 2, 4, 1, 3, 6, 0, 2, 4, 1, 3, 6, 0, 2, 4, 1, 3, 6, 0, 2, 4, 1, 3, 6, 0, 2, 5, 1, 3, 6, 0, 2, 5, 1, 3, 6, 0, 2, 5, 1, 3, 7, 0, 2, 5, 1, 3, 7, 0, 2, 5, 1, 4, 7, 0, 3, 5, 2, 4, 7];
const IX: [u8; 72] = [22, 20, 10, 20, 18, 0, 20, 18, 8, 18, 16, 13, 18, 16, 6, 16, 14, 11, 16, 14, 4, 14, 12, 9, 14, 12, 2, 12, 10, 7, 12, 10, 0, 10, 8, 5, 10, 8, 13, 8, 6, 3, 8, 6, 11, 6, 4, 1, 6, 4, 9, 4, 2, 6, 4, 2, 7, 2, 0, 4, 2, 0, 5, 0, 13, 2, 0, 21, 3, 21, 11, 0];
const IY: [u8; 72] = [1, 3, 5, 0, 2, 4, 1, 3, 6, 0, 2, 4, 1, 3, 6, 0, 2, 4, 1, 3, 6, 0, 2, 4, 1, 3, 6, 0, 2, 4, 1, 3, 6, 0, 2, 5, 1, 3, 6, 0, 2, 5, 1, 3, 6, 0, 2, 5, 1, 3, 6, 0, 2, 5, 1, 3, 7, 0, 2, 5, 1, 4, 7, 0, 3, 5, 2, 4, 7, 1, 3, 5];
const IZ: [u8; 72] = [21, 19, 1, 21, 19, 9, 19, 17, 14, 19, 17, 7, 17, 15, 12, 17, 15, 5, 15, 13, 10, 15, 13, 3, 13, 11, 8, 13, 11, 1, 11, 9, 6, 11, 9, 14, 9, 7, 4, 9, 7, 12, 7, 5, 2, 7, 5, 10, 5, 3, 0, 5, 3, 8, 3, 1, 5, 3, 1, 6, 1, 14, 3, 1, 22, 4, 22, 12, 1, 22, 20, 2];

/// 144 received bits (72 dibits, MSB first) -> mbelib's `imbe_fr[8][23]` layout.
pub fn deinterleave(frame: &[bool; 144]) -> [[c_char; 23]; 8] {
    let mut fr = [[0 as c_char; 23]; 8];
    for n in 0..72 {
        fr[IW[n] as usize][IX[n] as usize] = frame[2 * n] as c_char;
        fr[IY[n] as usize][IZ[n] as usize] = frame[2 * n + 1] as c_char;
    }
    fr
}

/// Decoder state for one call (mbelib smooths across frames and the AGC tracks the speaker, so use one per call).
pub struct Imbe { state: *mut c_void, agc: Agc }

// SAFETY: the state is a plain heap block owned by this value; mbelib keeps no thread-local or global state for it.
unsafe impl Send for Imbe {}

impl Imbe {
    pub fn new() -> Imbe {
        // SAFETY: allocation only; checked for null.
        let state = unsafe { sdr_mbe_new() };
        assert!(!state.is_null(), "mbelib state allocation failed");
        Imbe { state, agc: Agc::new() }
    }

    /// Decodes one frame. Returns the audio and mbelib's corrected-error count (high values mean a bad frame).
    pub fn decode(&mut self, frame: &[bool; 144]) -> ([i16; FRAME_SAMPLES], i32) {
        let mut fr = deinterleave(frame);
        let mut pcm = [0f32; FRAME_SAMPLES];
        let mut info = MbeFrameInfo { w0: 0.0, l: 0, repeat: 0, vl: [0; 57] };
        // SAFETY: fr is 8x23 chars, pcm holds 160 floats and info matches sdr_frame_info, as sdr_mbe_imbe requires;
        // self.state is a live state.
        let errs = unsafe { sdr_mbe_imbe(self.state, fr.as_mut_ptr(), pcm.as_mut_ptr(), &mut info) };
        for s in &mut pcm { *s *= MBELIB_GAIN; }
        self.agc.process(&mut pcm, &frame_info(&info, errs));
        (pcm.map(|s| s.round() as i16), errs)
    }
}

/// JMBE's voicing test: the fraction of voiced bands in the lower half of the spectrum.
fn frame_info(m: &MbeFrameInfo, errors: i32) -> FrameInfo {
    let l = m.l.clamp(0, 56) as usize;
    let bands = (l / 2).max(1).min(l);
    let voiced = (1..=bands).filter(|&b| m.vl[b] != 0).count();
    FrameInfo {
        repeat: m.repeat > 0,
        errors,
        voiced_fraction: if bands == 0 { 0.0 } else { voiced as f32 / bands as f32 },
        fundamental: m.w0,
    }
}

impl Default for Imbe { fn default() -> Self { Self::new() } }

impl Drop for Imbe {
    // SAFETY: frees the block allocated in new(), exactly once.
    fn drop(&mut self) { unsafe { sdr_mbe_free(self.state) } }
}

/// Writes 16-bit mono PCM as a WAV file.
pub fn write_wav(path: &std::path::Path, samples: &[i16], rate: u32) -> std::io::Result<()> {
    let data = (samples.len() * 2) as u32;
    let mut w = std::io::BufWriter::new(std::fs::File::create(path)?);
    w.write_all(b"RIFF")?; w.write_all(&(36 + data).to_le_bytes())?; w.write_all(b"WAVEfmt ")?;
    w.write_all(&16u32.to_le_bytes())?; w.write_all(&1u16.to_le_bytes())?; w.write_all(&1u16.to_le_bytes())?;
    w.write_all(&rate.to_le_bytes())?; w.write_all(&(rate * 2).to_le_bytes())?;
    w.write_all(&2u16.to_le_bytes())?; w.write_all(&16u16.to_le_bytes())?;
    w.write_all(b"data")?; w.write_all(&data.to_le_bytes())?;
    for s in samples { w.write_all(&s.to_le_bytes())?; }
    w.flush()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn interleave_covers_every_frame_bit_once() {
        let rows = [23, 23, 23, 23, 15, 15, 15, 7];
        let mut seen = [[0u8; 23]; 8];
        for n in 0..72 {
            seen[IW[n] as usize][IX[n] as usize] += 1;
            seen[IY[n] as usize][IZ[n] as usize] += 1;
        }
        for (r, &len) in rows.iter().enumerate() {
            assert!(seen[r][..len].iter().all(|&c| c == 1), "row {r}: {:?}", &seen[r][..len]);
        }
    }

    #[test]
    fn decodes_a_frame_to_160_samples() {
        let mut d = Imbe::new();
        let (pcm, _errs) = d.decode(&[false; 144]);
        assert_eq!(pcm.len(), FRAME_SAMPLES);
    }

    #[test]
    fn wav_header() {
        let p = std::env::temp_dir().join("sdrtrunk_audio_test.wav");
        write_wav(&p, &[0, 1, -1], 8000).unwrap();
        let b = std::fs::read(&p).unwrap();
        assert_eq!(&b[0..4], b"RIFF");
        assert_eq!(b.len(), 44 + 6);
        assert_eq!(u32::from_le_bytes(b[24..28].try_into().unwrap()), 8000);
        let _ = std::fs::remove_file(p);
    }
}
