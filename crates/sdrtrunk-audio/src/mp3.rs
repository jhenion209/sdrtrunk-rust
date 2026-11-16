//! Call audio to MP3 the way sdrtrunk writes it for ScannerPulse: mono, 8 kHz decoded audio resampled by LAME to
//! 16 kHz, 16 kbps CBR, preceded by an ID3v2.4 tag.
use mp3lame_encoder::{Bitrate, Builder, FlushNoGap, Mode, MonoPcm, Quality};
use std::num::NonZeroU32;

/// One text frame (frame id such as "TIT2", value) or a comment ("COMM", value).
pub type Tag<'a> = (&'a str, &'a str);

/// An ID3v2.4 tag holding `tags` (UTF-8 text). "COMM" becomes a comment frame with language "eng" and no description.
pub fn id3(tags: &[Tag]) -> Vec<u8> {
    let mut frames = Vec::new();
    for &(id, value) in tags {
        assert!(id.len() == 4, "ID3 frame id must be 4 characters: {id}");
        let mut body = vec![3u8]; // UTF-8
        if id == "COMM" { body.extend_from_slice(b"eng\0"); }
        body.extend_from_slice(value.as_bytes());
        frames.extend_from_slice(id.as_bytes());
        frames.extend_from_slice(&syncsafe(body.len()));
        frames.extend_from_slice(&[0, 0]);
        frames.extend_from_slice(&body);
    }
    let mut tag = b"ID3\x04\x00\x00".to_vec();
    tag.extend_from_slice(&syncsafe(frames.len()));
    tag.extend_from_slice(&frames);
    tag
}

fn syncsafe(n: usize) -> [u8; 4] {
    assert!(n < 1 << 28, "ID3 size too large");
    [(n >> 21) as u8 & 0x7F, (n >> 14) as u8 & 0x7F, (n >> 7) as u8 & 0x7F, n as u8 & 0x7F]
}

/// Encodes 8 kHz mono audio to a complete MP3 file (ID3 tag first).
pub fn encode(pcm: &[i16], tags: &[Tag]) -> Result<Vec<u8>, String> {
    let e = |x: &dyn std::fmt::Display| x.to_string();
    let mut b = Builder::new().ok_or("LAME init failed")?;
    b.set_num_channels(1).map_err(|x| e(&x))?;
    b.set_sample_rate(8_000).map_err(|x| e(&x))?;
    b.set_output_sample_rate(NonZeroU32::new(16_000)).map_err(|x| e(&x))?;
    b.set_brate(Bitrate::Kbps16).map_err(|x| e(&x))?;
    b.set_mode(Mode::Mono).map_err(|x| e(&x))?;
    b.set_quality(Quality::Good).map_err(|x| e(&x))?;
    b.set_to_write_vbr_tag(false).map_err(|x| e(&x))?;
    let mut enc = b.build().map_err(|x| e(&x))?;
    let mut out = id3(tags);
    out.reserve(mp3lame_encoder::max_required_buffer_size(pcm.len()));
    enc.encode_to_vec(MonoPcm(pcm), &mut out).map_err(|x| e(&x))?;
    out.reserve(7200);
    enc.flush_to_vec::<FlushNoGap>(&mut out).map_err(|x| e(&x))?;
    Ok(out)
}

/// One continuous MP3 stream (Icecast): 8 kHz mono audio in, `sample_rate` and `kbps` CBR out, never flushed, so
/// calls and silence follow each other without gaps.
pub struct StreamEncoder(mp3lame_encoder::Encoder);

impl StreamEncoder {
    /// `sample_rate`: 8000 or 16000; `kbps`: 8, 16, 24, 32, 48 or 64.
    pub fn new(sample_rate: u32, kbps: u16) -> Result<StreamEncoder, String> {
        let e = |x: &dyn std::fmt::Display| x.to_string();
        let rate = match kbps {
            8 => Bitrate::Kbps8, 16 => Bitrate::Kbps16, 24 => Bitrate::Kbps24, 32 => Bitrate::Kbps32, 48 => Bitrate::Kbps48,
            64 => Bitrate::Kbps64, _ => return Err(format!("unsupported bitrate {kbps} kbps")),
        };
        let mut b = Builder::new().ok_or("LAME init failed")?;
        b.set_num_channels(1).map_err(|x| e(&x))?;
        b.set_sample_rate(8_000).map_err(|x| e(&x))?;
        b.set_output_sample_rate(NonZeroU32::new(sample_rate)).map_err(|x| e(&x))?;
        b.set_brate(rate).map_err(|x| e(&x))?;
        b.set_mode(Mode::Mono).map_err(|x| e(&x))?;
        b.set_quality(Quality::Good).map_err(|x| e(&x))?;
        b.set_to_write_vbr_tag(false).map_err(|x| e(&x))?;
        Ok(StreamEncoder(b.build().map_err(|x| e(&x))?))
    }

    /// MP3 bytes for the next 8 kHz samples (LAME holds back a frame or so until more audio arrives).
    pub fn encode(&mut self, pcm: &[i16]) -> Result<Vec<u8>, String> {
        let mut out = Vec::with_capacity(mp3lame_encoder::max_required_buffer_size(pcm.len()));
        self.0.encode_to_vec(MonoPcm(pcm), &mut out).map_err(|x| x.to_string())?;
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stream_8k_16kbps() {
        let mut s = StreamEncoder::new(8_000, 16).unwrap();
        let tone: Vec<i16> = (0..8_000).map(|i| ((i as f32 * 0.3).sin() * 8000.0) as i16).collect();
        let mut out = Vec::new();
        for _ in 0..3 { out.extend(s.encode(&tone).unwrap()); out.extend(s.encode(&[0; 8_000]).unwrap()); }
        assert_eq!((out[0], out[1] & 0xE0), (0xFF, 0xE0), "starts on an MPEG frame, no ID3 tag");
        assert_eq!((out[1] >> 3) & 3, 0, "MPEG 2.5 (8 kHz)");
        assert!((10_000..14_000).contains(&out.len()), "6 s at 16 kbps is ~12000 bytes, got {}", out.len());
        assert!(StreamEncoder::new(8_000, 20).is_err());
    }

    #[test]
    fn id3_layout() {
        let t = id3(&[("TIT2", "23\"Jail 1\""), ("COMM", "Date:x;")]);
        assert_eq!(&t[..6], b"ID3\x04\x00\x00");
        assert_eq!(t[6..10], syncsafe(t.len() - 10));
        // TIT2: 4 id + 4 size + 2 flags + encoding byte + 10 bytes of text.
        assert_eq!(&t[10..14], b"TIT2");
        assert_eq!(t[14..18], [0, 0, 0, 11]);
        assert_eq!(t[20], 3);
        assert_eq!(&t[21..31], b"23\"Jail 1\"");
        assert_eq!(&t[31..35], b"COMM");
        assert_eq!(&t[41..53], b"\x03eng\0Date:x;");
        assert_eq!(t.len(), 53);
        assert_eq!(syncsafe(200), [0, 0, 1, 72]);
    }

    #[test]
    fn mpeg2_16k_mono_16kbps() {
        let pcm: Vec<i16> = (0..8_000).map(|i| ((i as f32 * 0.3).sin() * 8000.0) as i16).collect();
        let tag = id3(&[("TIT2", "1")]);
        let mp3 = encode(&pcm, &[("TIT2", "1")]).unwrap();
        assert_eq!(&mp3[..tag.len()], &tag[..]);
        let h = &mp3[tag.len()..tag.len() + 4];
        assert_eq!((h[0], h[1]), (0xFF, 0xF3), "MPEG-2 layer III without CRC");
        assert_eq!(h[2] >> 4, 2, "16 kbps");
        assert_eq!((h[2] >> 2) & 3, 2, "16 kHz");
        assert_eq!(h[3] >> 6, 3, "mono");
        let audio = mp3.len() - tag.len();
        assert!((1_800..2_600).contains(&audio), "1 s at 16 kbps is ~2000 bytes, got {audio}");
    }
}
