//! P25 CRCs. Port of sdrtrunk `CRC.java` / `CRCP25.java`. (`CRC16.java` is not ported: nothing in the
//! planned P25 Phase 1 path uses it.)
//!
//! All checks take bit messages as `&[bool]`, index 0 = first transmitted bit.

use crate::bits::{get_uint, Outcome};
use crate::crc_tables::{CRC_12_FACCH, CRC_12_SACCH, CRC_16_LCCH};
use crate::golay::golay24_check_and_correct;
use std::sync::OnceLock;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Crc {
    Passed,
    PassedInverted,
    FailedCrc,
    FailedParity,
    Corrected,
    Unknown,
}

impl Crc {
    pub fn passes(self) -> bool {
        matches!(self, Crc::Passed | Crc::PassedInverted | Crc::Corrected)
    }

    pub fn abbreviation(self) -> &'static str {
        match self {
            Crc::Passed => "*",
            Crc::PassedInverted => "#",
            Crc::FailedCrc => "f",
            Crc::FailedParity => "p",
            Crc::Corrected => "C",
            Crc::Unknown => "-",
        }
    }
}

const CCITT_MESSAGE_LENGTH: usize = 80;
const CCITT_CRC_LENGTH: usize = 16;
const CRC_16_POLYNOMIAL: u64 = 0x1021;
const CRC_9_POLYNOMIAL: u64 = 0x59;
const CRC_32_POLYNOMIAL: u64 = 0x04C1_1DB7;

/// Shift `bits[start..end]` through a non-reflected CRC register of `crc_length` bits.
fn update_crc(mut rem: u64, bits: &[bool], start: usize, end: usize, crc_length: u32, poly: u64) -> u64 {
    let high = 1u64 << (crc_length - 1);
    let mask = (1u64 << crc_length) - 1;
    for &b in &bits[start..end] {
        let feedback = (rem & high != 0) ^ b;
        rem = (rem << 1) & mask;
        if feedback {
            rem ^= poly;
        }
    }
    rem
}

/// CRC with zero initial fill and final inversion (`inversion` is the XOR-out mask).
fn calculate_crc(bits: &[bool], start: usize, end: usize, crc_length: u32, poly: u64, inversion: u64) -> u64 {
    update_crc(0, bits, start, end, crc_length, poly) ^ inversion
}

/// Syndrome (CRC residual) produced by a single-bit error at each position of an 80+16 bit CCITT frame.
fn ccitt80_syndromes() -> &'static [u16; CCITT_MESSAGE_LENGTH + CCITT_CRC_LENGTH] {
    static T: OnceLock<[u16; 96]> = OnceLock::new();
    T.get_or_init(|| {
        let mut t = [0u16; 96];
        for (bit, slot) in t.iter_mut().enumerate().take(CCITT_MESSAGE_LENGTH) {
            let mut rem: u16 = 1;
            for _ in 0..CCITT_CRC_LENGTH + CCITT_MESSAGE_LENGTH - bit - 1 {
                let high = rem & 0x8000 != 0;
                rem <<= 1;
                if high {
                    rem ^= CRC_16_POLYNOMIAL as u16;
                }
            }
            *slot = rem;
        }
        for bit in 0..CCITT_CRC_LENGTH {
            t[CCITT_MESSAGE_LENGTH + bit] = 1 << (CCITT_CRC_LENGTH - bit - 1);
        }
        t
    })
}

/// CRC-CCITT over 80 data bits at `message_start` followed by the 16 CRC bits at `crc_start`.
/// Corrects a single-bit error; anything else is `Uncorrectable`.
pub fn correct_ccitt80(bits: &mut [bool], message_start: usize, crc_start: usize) -> Outcome {
    assert!(
        crc_start - message_start == CCITT_MESSAGE_LENGTH && bits.len() >= crc_start + CCITT_CRC_LENGTH,
        "P25 CRC-CCITT requires 80 data bits followed by 16 CRC bits"
    );
    let calculated = calculate_crc(bits, message_start, crc_start, 16, CRC_16_POLYNOMIAL, 0xFFFF);
    let checksum = get_uint(bits, crc_start, crc_start + 15);
    let residual = (calculated ^ checksum) as u16;
    if residual == 0 {
        return Outcome::Clean;
    }
    match ccitt80_syndromes().iter().position(|&s| s == residual) {
        // Errors in the CRC bits themselves are located but need no flip of message data in Java either,
        // since it flips `errorLocation + messageStart`, which for CRC bits lands on the CRC bits.
        Some(loc) => {
            bits[loc + message_start] = !bits[loc + message_start];
            Outcome::Corrected(1)
        }
        None => Outcome::Uncorrectable,
    }
}

/// 9-bit CRC over a 144-bit PDU block (header bits 0..7 and 16..144; CRC in bits 7..16).
pub fn check_crc9(bits: &[bool], start: usize) -> Crc {
    if bits.len() < start + 144 {
        return Crc::FailedCrc;
    }
    let rem = calculate_crc(bits, start, start + 7, 9, CRC_9_POLYNOMIAL, 0);
    let rem = update_crc(rem, bits, start + 16, start + 144, 9, CRC_9_POLYNOMIAL);
    let calculated = rem ^ 0x1FF;
    // Java reads 9 bits as (start+7 ..= start+15), written here with the same inclusive bounds.
    let checksum = get_uint(bits, start + 7, start + 15);
    if calculated == checksum { Crc::Passed } else { Crc::FailedCrc }
}

pub fn check_crc32(bits: &[bool], message_start: usize, crc_start: usize) -> Crc {
    if crc_start <= message_start || bits.len() < crc_start + 32 {
        return Crc::FailedCrc;
    }
    let calculated = calculate_crc(bits, message_start, crc_start, 32, CRC_32_POLYNOMIAL, 0xFFFF_FFFF);
    if calculated == get_uint(bits, crc_start, crc_start + 31) { Crc::Passed } else { Crc::FailedCrc }
}

fn table_crc(bits: &[bool], table: &[u16], data_bits: usize, init: u16, checksum_end: usize) -> bool {
    let calculated = bits[..data_bits]
        .iter()
        .enumerate()
        .filter(|(_, &b)| b)
        .fold(init, |c, (i, _)| c ^ table[i]);
    calculated as u64 == get_uint(bits, data_bits, checksum_end)
}

/// P25 Phase 2 FACCH 12-bit CRC (144 data bits, CRC at 144..=155).
pub fn crc12_facch(bits: &[bool]) -> bool {
    table_crc(bits, &CRC_12_FACCH, 144, 0xFFF, 155)
}

/// P25 Phase 2 SACCH 12-bit CRC (168 data bits, CRC at 168..=179).
pub fn crc12_sacch(bits: &[bool]) -> bool {
    table_crc(bits, &CRC_12_SACCH, 168, 0xFFF, 179)
}

/// P25 Phase 2 LCCH 16-bit CRC (164 data bits, CRC at 164..=179).
pub fn crc16_lcch(bits: &[bool]) -> bool {
    table_crc(bits, &CRC_16_LCCH, 164, 0xFFFF, 179)
}

/// Golay(24) correction of a TDULC: codewords start at bit 64, every 24 bits. True if none were uncorrectable.
pub fn correct_golay24_tdulc(bits: &mut [bool]) -> bool {
    let mut x = 64;
    while x < bits.len() {
        if golay24_check_and_correct(bits, x) == Outcome::Uncorrectable {
            return false;
        }
        x += 24;
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bits::set_uint;

    fn bytes_to_bits(b: &[u8]) -> Vec<bool> {
        b.iter().flat_map(|&x| (0..8).rev().map(move |i| x >> i & 1 == 1)).collect()
    }

    #[test]
    fn crc16_matches_published_crc16_gsm_check_value() {
        // Zero init, poly 0x1021, no reflection, xor-out 0xFFFF == CRC-16/GSM; check("123456789") = 0xCE3C.
        let bits = bytes_to_bits(b"123456789");
        assert_eq!(calculate_crc(&bits, 0, bits.len(), 16, CRC_16_POLYNOMIAL, 0xFFFF), 0xCE3C);
    }

    #[test]
    fn crc32_matches_published_value_for_its_parameters() {
        // Poly 04C11DB7, zero init, xor-out FFFFFFFF, no reflection: bitwise reference computed independently.
        let bits = bytes_to_bits(b"123456789");
        let mut rem: u32 = 0;
        for &b in &bits {
            let fb = (rem >> 31 & 1 == 1) ^ b;
            rem <<= 1;
            if fb {
                rem ^= 0x04C1_1DB7;
            }
        }
        assert_eq!(calculate_crc(&bits, 0, bits.len(), 32, CRC_32_POLYNOMIAL, 0xFFFF_FFFF), !rem as u64);
    }

    fn ccitt_frame(seed: u64) -> Vec<bool> {
        let mut f = vec![false; 96];
        let mut x = seed;
        for b in f.iter_mut().take(80) {
            x = x.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            *b = x >> 40 & 1 == 1;
        }
        let crc = calculate_crc(&f, 0, 80, 16, CRC_16_POLYNOMIAL, 0xFFFF);
        set_uint(&mut f, 80, 16, crc);
        f
    }

    #[test]
    fn ccitt80_corrects_every_single_bit_error() {
        let good = ccitt_frame(7);
        let mut clean = good.clone();
        assert_eq!(correct_ccitt80(&mut clean, 0, 80), Outcome::Clean);
        for i in 0..96 {
            let mut f = good.clone();
            f[i] = !f[i];
            assert_eq!(correct_ccitt80(&mut f, 0, 80), Outcome::Corrected(1), "bit {i}");
            assert_eq!(f, good, "bit {i}");
        }
    }

    #[test]
    fn ccitt80_rejects_two_bit_errors() {
        let mut f = ccitt_frame(9);
        f[3] = !f[3];
        f[50] = !f[50];
        assert_eq!(correct_ccitt80(&mut f, 0, 80), Outcome::Uncorrectable);
    }

    #[test]
    fn table_sizes_match_message_geometry() {
        assert_eq!(CRC_12_FACCH.len(), 144);
        assert_eq!(CRC_12_SACCH.len(), 168);
        assert_eq!(CRC_16_LCCH.len(), 180); // 164 data + 16 CRC contributions
    }

    #[test]
    fn table_crcs_pass_when_checksum_is_appended() {
        for (data, table, init, width, f) in [
            (144usize, &CRC_12_FACCH[..], 0xFFFu16, 12usize, crc12_facch as fn(&[bool]) -> bool),
            (168, &CRC_12_SACCH[..], 0xFFF, 12, crc12_sacch),
            (164, &CRC_16_LCCH[..], 0xFFFF, 16, crc16_lcch),
        ] {
            let mut m = vec![false; data + width];
            for i in (0..data).step_by(3) {
                m[i] = true;
            }
            let crc = (0..data).filter(|&i| m[i]).fold(init, |c, i| c ^ table[i]);
            set_uint(&mut m, data, width, crc as u64);
            assert!(f(&m), "data bits {data}");
            m[1] = !m[1];
            assert!(!f(&m), "data bits {data} must fail after a flip");
        }
    }

    #[test]
    fn crc9_and_crc32_detect_corruption() {
        let mut m = vec![false; 144];
        for i in (0..144).step_by(5) {
            m[i] = true;
        }
        let rem = calculate_crc(&m, 0, 7, 9, CRC_9_POLYNOMIAL, 0);
        let rem = update_crc(rem, &m, 16, 144, 9, CRC_9_POLYNOMIAL);
        set_uint(&mut m, 7, 9, rem ^ 0x1FF);
        assert_eq!(check_crc9(&m, 0), Crc::Passed);
        m[100] = !m[100];
        assert_eq!(check_crc9(&m, 0), Crc::FailedCrc);

        let mut p = vec![false; 96 + 32];
        for i in (0..96).step_by(7) {
            p[i] = true;
        }
        let c = calculate_crc(&p, 0, 96, 32, CRC_32_POLYNOMIAL, 0xFFFF_FFFF);
        set_uint(&mut p, 96, 32, c);
        assert_eq!(check_crc32(&p, 0, 96), Crc::Passed);
        p[10] = !p[10];
        assert_eq!(check_crc32(&p, 0, 96), Crc::FailedCrc);
    }
}
