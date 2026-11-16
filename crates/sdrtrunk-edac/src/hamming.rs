//! Short Hamming codes used in P25 link control / HDU / LDU. Port of sdrtrunk `Hamming10/13/15/16/17.java`.
//! Each codeword is data bits followed by check bits; check = XOR of `CHECKSUMS[i]` over set data bits i.
//! The Java `int[] indices` (non-contiguous) variant of Hamming13 is not ported yet.

use crate::bits::{get_uint, Outcome};

/// Result of an error-index lookup.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HammingError {
    NoErrors,
    /// Index of the single bit in error (absolute, i.e. includes the offset).
    Bit(usize),
    Multiple,
}

fn syndrome(bits: &[bool], offset: usize, checksums: &[u8], data_bits: usize, check_bits: usize) -> u8 {
    let calc = (0..data_bits).filter(|&i| bits[offset + i]).fold(0u8, |s, i| s ^ checksums[i]);
    calc ^ get_uint(bits, offset + data_bits, offset + data_bits + check_bits - 1) as u8
}

// ---- Hamming(10,6,3) ----
const H10: [u8; 6] = [0xE, 0xD, 0xB, 0x7, 0x3, 0xC];
/// Syndrome -> bit index within the 10-bit word; `None` = uncorrectable (2 errors).
const H10_INDEX: [Option<usize>; 16] = [
    None, Some(9), Some(8), Some(4), Some(7), None, None, Some(3), Some(6), None, None, Some(2), Some(5),
    Some(1), Some(0), None,
];

pub fn hamming10_check_and_correct(bits: &mut [bool], start: usize) -> Outcome {
    let s = syndrome(bits, start, &H10, 6, 4);
    if s == 0 {
        return Outcome::Clean;
    }
    match H10_INDEX[s as usize] {
        Some(i) => {
            bits[start + i] = !bits[start + i];
            Outcome::Corrected(1)
        }
        None => Outcome::Uncorrectable,
    }
}

// ---- Hamming(13,9,3) ----
const H13: [u8; 9] = [0xF, 0xE, 0x7, 0xA, 0x5, 0xB, 0xC, 0x6, 0x3];
const H13_INDEX: [i8; 16] = [-1, 12, 11, 8, 10, 4, 7, 2, 9, -1, 3, 5, 6, -1, 1, 0];

pub fn hamming13_error_index(bits: &[bool], offset: usize) -> HammingError {
    index_lookup(syndrome(bits, offset, &H13, 9, 4), &H13_INDEX, offset)
}

// ---- Hamming(15,11,3) ----
const H15: [u8; 11] = [0x9, 0xD, 0xF, 0xE, 0x7, 0xA, 0x5, 0xB, 0xC, 0x6, 0x3];
const H15_INDEX: [i8; 16] = [-1, 14, 13, 10, 12, 6, 9, 4, 11, 0, 5, 7, 8, 1, 3, 2];

pub fn hamming15_error_index(bits: &[bool], offset: usize) -> HammingError {
    index_lookup(syndrome(bits, offset, &H15, 11, 4), &H15_INDEX, offset)
}

fn index_lookup(s: u8, table: &[i8; 16], offset: usize) -> HammingError {
    if s == 0 {
        HammingError::NoErrors
    } else if table[s as usize] >= 0 {
        HammingError::Bit(offset + table[s as usize] as usize)
    } else {
        // Java would return `offset - 1` here; that is a bug, not a position.
        HammingError::Multiple
    }
}

// ---- Hamming(16,11,4) and (17,12,4): extended with a final parity-style check ----
const H16: [u8; 16] = [0x13, 0x1A, 0x1F, 0x1C, 0x0E, 0x15, 0x0B, 0x16, 0x19, 0x0D, 0x07, 0x10, 0x08, 0x04, 0x02, 0x01];
const H17: [u8; 17] = [
    0x1B, 0x1F, 0x1D, 0x1C, 0x0E, 0x07, 0x11, 0x1A, 0x0D, 0x14, 0x0A, 0x05, 0x10, 0x08, 0x04, 0x02, 0x01,
];

fn extended_error_index(bits: &[bool], offset: usize, checksums: &[u8], data: usize) -> HammingError {
    let len = checksums.len();
    let s = syndrome(bits, offset, checksums, data, 5);
    if s == 0 {
        return HammingError::NoErrors;
    }
    let odd = bits[offset..offset + len].iter().filter(|&&b| b).count() % 2 == 1;
    if s == 1 && odd {
        return HammingError::Multiple;
    }
    match checksums.iter().position(|&c| c == s) {
        Some(i) => HammingError::Bit(offset + i),
        None => HammingError::Multiple,
    }
}

pub fn hamming16_error_index(bits: &[bool], offset: usize) -> HammingError {
    extended_error_index(bits, offset, &H16, 11)
}

pub fn hamming17_error_index(bits: &[bool], offset: usize) -> HammingError {
    extended_error_index(bits, offset, &H17, 12)
}

/// Hamming16 `checkAndCorrect` from Java: flips on any syndrome match, no parity guard.
pub fn hamming16_check_and_correct(bits: &mut [bool], start: usize) -> Outcome {
    let s = syndrome(bits, start, &H16, 11, 5);
    if s == 0 {
        return Outcome::Clean;
    }
    match H16.iter().position(|&c| c == s) {
        Some(i) => {
            bits[start + i] = !bits[start + i];
            Outcome::Corrected(1)
        }
        None => Outcome::Uncorrectable,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn word(checksums: &[u8], data_bits: usize, check_bits: usize, data: u32) -> Vec<bool> {
        let mut w: Vec<bool> = (0..data_bits).map(|i| data >> (data_bits - 1 - i) & 1 == 1).collect();
        let check = (0..data_bits).filter(|&i| w[i]).fold(0u8, |s, i| s ^ checksums[i]);
        w.extend((0..check_bits).map(|i| check >> (check_bits - 1 - i) & 1 == 1));
        w
    }

    #[test]
    fn hamming10_fixes_every_single_error() {
        for data in [0u32, 0x3F, 0x15, 0x2A] {
            let cw = word(&H10, 6, 4, data);
            let mut c = cw.clone();
            assert_eq!(hamming10_check_and_correct(&mut c, 0), Outcome::Clean);
            for i in 0..10 {
                let mut w = cw.clone();
                w[i] = !w[i];
                assert_eq!(hamming10_check_and_correct(&mut w, 0), Outcome::Corrected(1));
                assert_eq!(w, cw);
            }
        }
    }

    fn single_error_located(checksums: &[u8], data: usize, check: usize, f: fn(&[bool], usize) -> HammingError) {
        let cw = word(checksums, data, check, 0x2B5 & ((1 << data) - 1));
        assert_eq!(f(&cw, 0), HammingError::NoErrors);
        for i in 0..cw.len() {
            let mut w = cw.clone();
            w[i] = !w[i];
            assert_eq!(f(&w, 0), HammingError::Bit(i), "bit {i}");
        }
    }

    #[test]
    fn hamming13_and_15_locate_single_errors() {
        single_error_located(&H13, 9, 4, hamming13_error_index);
        single_error_located(&H15, 11, 4, hamming15_error_index);
    }

    #[test]
    fn hamming16_and_17_locate_single_errors() {
        // Parity-guard: a flipped final bit gives syndrome 1 with *odd* weight only if the word started even.
        // Build valid words with an even overall weight so single errors always show odd weight; skip the
        // last (syndrome 1) bit, which Java deliberately reports as Multiple.
        for (cs, data, f) in [
            (&H16[..], 11usize, hamming16_error_index as fn(&[bool], usize) -> HammingError),
            (&H17[..], 12usize, hamming17_error_index),
        ] {
            let cw = word(cs, data, 5, 0x5A5 & ((1 << data) - 1));
            for i in 0..cs.len() - 1 {
                let mut w = cw.clone();
                w[i] = !w[i];
                assert_eq!(f(&w, 0), HammingError::Bit(i), "bit {i} len {}", cs.len());
            }
        }
    }
}
