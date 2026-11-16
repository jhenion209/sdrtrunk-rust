//! Builds synthetic P25 Phase 1 dibit streams (sync, BCH-protected NID, trellis-coded TSBKs with CRC, voice and
//! terminator units, status symbols) for the framer vector tests.
//!
//!   cargo test -p sdrtrunk-p25 --test gen_streams -- --ignored   writes tests/fixtures/p25_streams.txt
//!
//! The streams are only inputs; the Java framer decides what they decode to (see GenFramer.java.txt).
use sdrtrunk_edac::bch::Bch63;
use sdrtrunk_edac::bits::{set_uint, Outcome};
use sdrtrunk_edac::crc::correct_ccitt80;
use sdrtrunk_p25::dibit::Dibit;
use sdrtrunk_p25::symbols::SymbolMessage;
use sdrtrunk_p25::sync::sync_pattern_dibits;
use sdrtrunk_p25::trellis::{encode_half_rate, interleave_data_dibits};
use sdrtrunk_p25::tsbk::deinterleave_viterbi_and_crc;

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u32 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        (self.0 >> 16) as u32
    }
    fn dibit(&mut self) -> u8 { (self.next() & 3) as u8 }
    fn dibits(&mut self, n: usize) -> Vec<u8> { (0..n).map(|_| self.dibit()).collect() }
}

fn nid_bits(bch: &Bch63, nac: u64, duid: u64) -> [bool; 64] {
    let mut data = [false; 16];
    set_uint(&mut data, 0, 12, nac);
    set_uint(&mut data, 12, 4, duid);
    let mut nid = [false; 64];
    nid[..63].copy_from_slice(&bch.encode(&data));
    nid[63] = ((duid & 2) != 0) ^ ((duid & 1) != 0);
    nid
}

/// 96 TSBK bits: 80 data bits followed by the CRC-CCITT that makes the block valid.
fn tsbk_bits(data: &[bool; 80]) -> Vec<bool> {
    for crc in 0..=0xFFFFu32 {
        let mut bits = data.to_vec();
        bits.extend((0..16).map(|i| (crc >> (15 - i)) & 1 == 1));
        if correct_ccitt80(&mut bits.clone(), 0, 80) == Outcome::Clean {
            return bits;
        }
    }
    unreachable!("a CRC always exists")
}

/// 98 interleaved dibits for one TSBK block.
fn tsbk_dibits(bits: &[bool]) -> Vec<u8> { interleave_data_dibits(&encode_half_rate(bits)) }

fn tsbk_payload(rng: &mut Rng, blocks: usize, corrupt_block: Option<usize>) -> Vec<u8> {
    let mut out = Vec::new();
    for b in 0..blocks {
        let mut data = [false; 80];
        for bit in data.iter_mut() { *bit = rng.next() & 1 == 1; }
        data[0] = b == blocks - 1; // last block flag
        let mut bits = tsbk_bits(&data);
        if corrupt_block == Some(b) {
            bits[10] = !bits[10];
            bits[40] = !bits[40];
        }
        out.extend(tsbk_dibits(&bits));
    }
    out
}

/// Sync + NID + payload with a status symbol after every 35 dibits.
fn frame(rng: &mut Rng, nid: &[bool; 64], payload: &[u8]) -> Vec<u8> {
    let mut content: Vec<u8> = sync_pattern_dibits().iter().map(|d| d.value()).collect();
    content.extend(nid.chunks(2).map(|p| ((p[0] as u8) << 1) | p[1] as u8));
    content.extend_from_slice(payload);
    let mut out = Vec::new();
    for (n, d) in content.into_iter().enumerate() {
        out.push(d);
        if (n + 1) % 35 == 0 {
            out.push(rng.dibit());
        }
    }
    out
}

#[test]
fn trellis_round_trip_decodes_with_valid_crc() {
    let mut rng = Rng(0x9E3779B97F4A7C15);
    for _ in 0..20 {
        let mut data = [false; 80];
        for bit in data.iter_mut() { *bit = rng.next() & 1 == 1; }
        let bits = tsbk_bits(&data);
        let dibits = tsbk_dibits(&bits);
        assert_eq!(dibits.len(), 98);
        let phases: Vec<f32> = dibits.iter().map(|&d| Dibit::from_value(d).ideal_phase()).collect();
        let block = deinterleave_viterbi_and_crc(&SymbolMessage::from_slice(&phases));
        assert_eq!(block.bits, bits);
        assert_eq!(block.corrected_bit_count, 0);
        assert!(block.is_valid());
    }
}

#[test]
#[ignore = "writes tests/fixtures/p25_streams.txt"]
fn write_streams() {
    let bch = Bch63::p25_nid();
    let mut rng = Rng(0x2545F4914F6CDD1D);
    let nac = 0x293;
    let n = |duid: u64| nid_bits(&bch, nac, duid);
    let mut streams: Vec<(&str, Vec<u8>)> = Vec::new();

    // Control channel: single and multi block TSBKs, a block with a failed CRC, idle gaps between.
    let mut s = rng.dibits(150);
    for (blocks, corrupt) in [(3, None), (1, None), (2, None), (1, Some(0)), (3, Some(1)), (1, None), (2, None)] {
        let payload = tsbk_payload(&mut rng, blocks, corrupt);
        s.extend(frame(&mut rng, &n(7), &payload));
        let gap = 20 + (rng.next() % 40) as usize;
        s.extend(rng.dibits(gap));
    }
    s.extend(rng.dibits(300));
    streams.push(("control", s));

    // Voice call: HDU, alternating LDU1/LDU2, terminators.
    let mut s = rng.dibits(100);
    let hdu = rng.dibits(329);
    s.extend(frame(&mut rng, &n(0), &hdu));
    for duid in [5u64, 10, 5, 10, 5] {
        let ldu = rng.dibits(784);
        s.extend(frame(&mut rng, &n(duid), &ldu));
    }
    let tdu = rng.dibits(14);
    s.extend(frame(&mut rng, &n(3), &tdu));
    s.extend(rng.dibits(60));
    let tdulc = rng.dibits(154);
    s.extend(frame(&mut rng, &n(15), &tdulc));
    s.extend(rng.dibits(400));
    streams.push(("voice", s));

    // Units cut short by the next sync, to exercise forced completion and dropped-symbol accounting.
    let mut s = rng.dibits(80);
    let ldu = rng.dibits(300);
    s.extend(frame(&mut rng, &n(5), &ldu));
    let payload = tsbk_payload(&mut rng, 1, None);
    s.extend(frame(&mut rng, &n(7), &payload));
    let ldu = rng.dibits(700);
    s.extend(frame(&mut rng, &n(10), &ldu));
    let ldu = rng.dibits(784);
    s.extend(frame(&mut rng, &n(5), &ldu));
    let tdu = rng.dibits(5);
    s.extend(frame(&mut rng, &n(3), &tdu));
    let payload = tsbk_payload(&mut rng, 3, None);
    s.extend(frame(&mut rng, &n(7), &payload));
    s.extend(rng.dibits(400));
    streams.push(("truncated", s));

    // A long quiet stretch to trigger the periodic sync loss accounting.
    let mut s = rng.dibits(6000);
    let payload = tsbk_payload(&mut rng, 1, None);
    s.extend(frame(&mut rng, &n(7), &payload));
    s.extend(rng.dibits(5200));
    streams.push(("quiet", s));

    let text: String = streams
        .iter()
        .map(|(name, d)| format!("STREAM|{name}|{}\n", d.iter().map(|v| char::from(b'0' + v)).collect::<String>()))
        .collect();
    std::fs::write(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/p25_streams.txt"), text).unwrap();
}
