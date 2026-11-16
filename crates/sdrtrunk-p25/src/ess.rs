//! LDU2 encryption sync word (ESS): message indicator, algorithm id and key id. Port of the decode in sdrtrunk
//! `LDU2Message.createEncryptionSyncParameters()` and `EncryptionSyncParameters`.
//!
//! 16 data + 8 Reed-Solomon parity hex words (6 bits each), every word Hamming(10,6,3) protected, sit between the
//! voice frames of the LDU2. Algorithm 0x80 means clear voice; anything else is encrypted.
use sdrtrunk_edac::bits::Outcome;
use sdrtrunk_edac::hamming::hamming10_check_and_correct;
use sdrtrunk_edac::rs::ReedSolomon;

/// Start of each 10-bit Hamming word in LDU2 message bits: 16 data words then 8 RS parity words.
const WORD_STARTS: [usize; 24] = [
    288, 298, 308, 318, 472, 482, 492, 502, 656, 666, 676, 686, 840, 850, 860, 870, 1024, 1034, 1044, 1054, 1208,
    1218, 1228, 1238,
];

/// `Encryption.UNENCRYPTED`.
pub const UNENCRYPTED: u8 = 0x80;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EncryptionSync {
    pub message_indicator: [u8; 9],
    pub algorithm: u8,
    pub key_id: u16,
}

impl EncryptionSync {
    pub fn is_encrypted(&self) -> bool { self.algorithm != UNENCRYPTED }
}

/// LDU1 link control word (sdrtrunk `LDU1Message.createLinkControlWord`): 72 bits, 12 data + 12 RS(24,12,13) parity
/// words at the same positions as the LDU2 ESS.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LinkControl { pub lco: u8, pub mfid: u8, pub service_options: u8, pub group: u16, pub source: u32 }

impl LinkControl {
    /// (talkgroup, source radio) for LCO 0 "group voice channel user" (standard MFID 0 or 1), else None.
    pub fn group_voice(&self) -> Option<(u16, u32)> { (self.lco == 0 && self.mfid <= 1).then_some((self.group, self.source)) }
    /// The "protected" service option of a group voice LC (`ServiceOptions.ENCRYPTION_FLAG`).
    pub fn is_encrypted(&self) -> bool { self.group_voice().is_some() && self.service_options & 0x40 != 0 }
}

/// Decodes the link control word from LDU1 message bits. None when the Reed-Solomon check fails.
pub fn link_control(ldu1: &[bool]) -> Option<LinkControl> {
    let v = hex_words(ldu1, 12, ReedSolomon::rs_24_12_13())?;
    Some(LinkControl {
        lco: (v >> 64) as u8 & 0x3F,
        mfid: (v >> 56) as u8,
        service_options: (v >> 48) as u8,
        group: (v >> 24) as u16,
        source: v as u32 & 0xFF_FFFF,
    })
}

/// Hamming-corrects the 24 hex words, Reed-Solomon-decodes them as a shortened RS(63, 39 + `data`) code (parity words
/// 23.. down at symbols 0.., data words `data-1`..0 after them, rest zero) and returns the data words MSB first.
fn hex_words(ldu: &[bool], data: usize, rs: ReedSolomon) -> Option<u128> {
    if ldu.len() < WORD_STARTS[23] + 10 { return None; }
    let mut bits = ldu.to_vec();
    for &s in &WORD_STARTS { hamming10_check_and_correct(&mut bits, s); }
    let word = |s: usize| bits[s..s + 6].iter().fold(0u8, |v, &b| (v << 1) | b as u8);
    let parity = 24 - data;
    let mut sym = [0u8; 63];
    for i in 0..parity { sym[i] = word(WORD_STARTS[23 - i]); }
    for i in 0..data { sym[parity + i] = word(WORD_STARTS[data - 1 - i]); }
    if rs.decode(&mut sym) == Outcome::Uncorrectable { return None; }
    Some((parity..24).rev().fold(0u128, |v, x| (v << 6) | sym[x] as u128))
}

/// Decodes the ESS from LDU2 message bits (as received after the NID, status symbols removed). None when the
/// Reed-Solomon check fails.
pub fn decode(ldu2: &[bool]) -> Option<EncryptionSync> {
    // 96 bits: 16 data words, RS(24,16,9).
    let v = hex_words(ldu2, 16, ReedSolomon::rs_24_16_9())?;
    let mut mi = [0u8; 9];
    for (n, b) in mi.iter_mut().enumerate() { *b = (v >> (88 - 8 * n)) as u8; }
    Some(EncryptionSync { message_indicator: mi, algorithm: (v >> 16) as u8, key_id: v as u16 })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Writes a 6-bit word plus the Hamming(10,6,3) parity that makes it a valid codeword.
    fn put_word(bits: &mut [bool], start: usize, value: u8) {
        for i in 0..6 { bits[start + i] = (value >> (5 - i)) & 1 == 1; }
        for p in 0..16u8 {
            for i in 0..4 { bits[start + 6 + i] = (p >> (3 - i)) & 1 == 1; }
            let mut probe = bits.to_vec();
            if hamming10_check_and_correct(&mut probe, start) == Outcome::Clean { return; }
        }
        panic!("no parity for {value}");
    }

    fn ldu2(algorithm: u8, key_id: u16, mi: [u8; 9]) -> Vec<bool> {
        let mut v = 0u128;
        for b in mi { v = (v << 8) | b as u128; }
        v = (v << 8) | algorithm as u128;
        v = (v << 16) | key_id as u128;
        let data: Vec<u8> = (0..16).map(|w| ((v >> (90 - 6 * w)) & 63) as u8).collect();
        // Encoder input: 55 data symbols, low index first: data words 15..0 then zero padding.
        let mut msg = [0u8; 55];
        for i in 0..16 { msg[i] = data[15 - i]; }
        let cw = ReedSolomon::rs_24_16_9().encode(&msg);
        let mut bits = vec![false; 1568];
        for w in 0..16 { put_word(&mut bits, WORD_STARTS[w], data[w]); }
        for i in 0..8 { put_word(&mut bits, WORD_STARTS[23 - i], cw[i]); }
        bits
    }

    #[test]
    fn decodes_clear_and_encrypted() {
        let mi = [1, 2, 3, 4, 5, 6, 7, 8, 9];
        let clear = decode(&ldu2(0x80, 0, [0; 9])).unwrap();
        assert!(!clear.is_encrypted());
        let enc = decode(&ldu2(0x84, 0x1234, mi)).unwrap();
        assert_eq!(enc, EncryptionSync { message_indicator: mi, algorithm: 0x84, key_id: 0x1234 });
        assert!(enc.is_encrypted());
    }

    #[test]
    fn corrects_bit_and_word_errors() {
        let mut bits = ldu2(0xAA, 0x0101, [9; 9]);
        bits[290] ^= true; // one bit in a word: Hamming fixes it
        for s in [472, 666, 1024] { for i in 0..4 { bits[s + i] ^= true; } } // three words: Reed-Solomon fixes them
        assert_eq!(decode(&bits).unwrap().algorithm, 0xAA);
    }

    /// LDU1 carrying `lc` (72 bits) as 12 data words plus RS(24,12,13) parity.
    fn ldu1(lc: u128) -> Vec<bool> {
        let data: Vec<u8> = (0..12).map(|w| ((lc >> (66 - 6 * w)) & 63) as u8).collect();
        let mut msg = [0u8; 51];
        for i in 0..12 { msg[i] = data[11 - i]; }
        let cw = ReedSolomon::rs_24_12_13().encode(&msg);
        let mut bits = vec![false; 1568];
        for w in 0..12 { put_word(&mut bits, WORD_STARTS[w], data[w]); }
        for i in 0..12 { put_word(&mut bits, WORD_STARTS[23 - i], cw[i]); }
        bits
    }

    #[test]
    fn link_control_group_voice_user() {
        // LCO 0, MFID 0, service options 0x40 (encrypted), reserved, group 27, source 703313.
        let lc = (0x40u128 << 48) | (27 << 24) | 703_313;
        let mut bits = ldu1(lc);
        for s in [298, 676] { for i in 0..5 { bits[s + i] ^= true; } } // two words: Reed-Solomon fixes them
        assert_eq!(link_control(&bits), Some(LinkControl { lco: 0, mfid: 0, service_options: 0x40, group: 27, source: 703_313 }));
        assert!(link_control(&bits).unwrap().is_encrypted());
        let clear = link_control(&ldu1((5 << 24) | 7)).unwrap();
        assert!(!clear.is_encrypted() && clear.group == 5 && clear.source == 7);
        assert_eq!(link_control(&ldu1(3u128 << 64)).unwrap().group_voice(), None, "other LCOs are not group voice");
    }

    #[test]
    fn garbage_is_rejected() {
        assert_eq!(link_control(&[false; 100]), None);
        let bits: Vec<bool> = (0..1568).map(|i| (i * 7 + i / 3) % 5 < 2).collect();
        assert_eq!(decode(&bits), None);
        assert_eq!(decode(&[false; 100]), None);
    }
}
