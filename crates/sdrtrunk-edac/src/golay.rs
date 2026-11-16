//! Golay(23,12) / extended Golay(24,12) / shortened Golay(18,6). Port of sdrtrunk `Golay23/24/18.java`.
//!
//! Bits 0..12 are data, bits 12..23 are 11 check bits, bit 23 (24-bit form) is even-parity.
//! Java decodes Golay23 by cyclic rotation; the code is perfect (every syndrome belongs to exactly one
//! error pattern of weight <= 3), so a syndrome table gives identical results.

use crate::bits::{get_uint, set_uint, Outcome};
use std::sync::OnceLock;

const CHECKSUMS: [u16; 23] = [
    0x63A, 0x31D, 0x7B4, 0x3DA, 0x1ED, 0x6CC, 0x366, 0x1B3, 0x6E3, 0x54B, 0x49F, 0x475, 0x400, 0x200, 0x100,
    0x080, 0x040, 0x020, 0x010, 0x008, 0x004, 0x002, 0x001,
];

/// Syndrome of a pattern (bit i set = error at index i); only indices < 23 carry check information.
fn pattern_syndrome(pattern: u32) -> u16 {
    (0..23).filter(|i| pattern >> i & 1 == 1).fold(0, |s, i| s ^ CHECKSUMS[i])
}

/// All error patterns of weight <= 3 over `width` bits.
fn patterns(width: u32) -> impl Iterator<Item = u32> {
    let single = (0..width).map(|a| 1u32 << a);
    let double = (0..width).flat_map(move |a| (a + 1..width).map(move |b| 1u32 << a | 1 << b));
    let triple = (0..width).flat_map(move |a| {
        (a + 1..width).flat_map(move |b| (b + 1..width).map(move |c| 1u32 << a | 1 << b | 1 << c))
    });
    std::iter::once(0).chain(single).chain(double).chain(triple)
}

fn syndrome(bits: &[bool], start: usize) -> u16 {
    let calc = bits[start..start + 12]
        .iter()
        .enumerate()
        .filter(|(_, &b)| b)
        .fold(0u16, |s, (i, _)| s ^ CHECKSUMS[i]);
    calc ^ get_uint(bits, start + 12, start + 22) as u16
}

fn flip_pattern(bits: &mut [bool], start: usize, mut pattern: u32) -> usize {
    let n = pattern.count_ones() as usize;
    while pattern != 0 {
        let i = pattern.trailing_zeros() as usize;
        bits[start + i] = !bits[start + i];
        pattern &= pattern - 1;
    }
    n
}

fn table23() -> &'static [u32; 2048] {
    static T: OnceLock<[u32; 2048]> = OnceLock::new();
    T.get_or_init(|| {
        let mut t = [u32::MAX; 2048];
        for p in patterns(23) {
            let s = pattern_syndrome(p) as usize;
            assert_eq!(t[s], u32::MAX, "duplicate Golay23 syndrome");
            t[s] = p;
        }
        t
    })
}

/// Index = (syndrome << 1) | overall-parity-odd.
fn table24() -> &'static [u32; 4096] {
    static T: OnceLock<[u32; 4096]> = OnceLock::new();
    T.get_or_init(|| {
        let mut t = [u32::MAX; 4096];
        for p in patterns(24) {
            let idx = (pattern_syndrome(p & 0x7F_FFFF) as usize) << 1 | (p.count_ones() as usize & 1);
            assert_eq!(t[idx], u32::MAX, "duplicate Golay24 syndrome");
            t[idx] = p;
        }
        t
    })
}

/// 23-bit Golay at `start`. Always repairs (the code is perfect); up to 3 bit errors are meaningful.
pub fn golay23_check_and_correct(bits: &mut [bool], start: usize) -> Outcome {
    let s = syndrome(bits, start);
    if s == 0 {
        return Outcome::Clean;
    }
    Outcome::Corrected(flip_pattern(bits, start, table23()[s as usize]))
}

/// 24-bit extended Golay at `start`. Detects (and leaves untouched) 4-bit errors.
pub fn golay24_check_and_correct(bits: &mut [bool], start: usize) -> Outcome {
    assert!(start + 24 <= bits.len(), "a complete 24-bit Golay codeword must start within the message");
    let odd = bits[start..start + 24].iter().filter(|&&b| b).count() & 1;
    let pattern = table24()[(syndrome(bits, start) as usize) << 1 | odd];
    if pattern == u32::MAX {
        return Outcome::Uncorrectable;
    }
    match flip_pattern(bits, start, pattern) {
        0 => Outcome::Clean,
        n => Outcome::Corrected(n),
    }
}

/// 18-bit shortened Golay (24-bit code with the first 6 data bits zero). Java reports only the corrected bit
/// count and leaves the message unchanged when uncorrectable.
pub fn golay18_check_and_correct(bits: &mut [bool], start: usize) -> Outcome {
    let mut temp = [false; 24];
    temp[6..].copy_from_slice(&bits[start..start + 18]);
    let outcome = golay24_check_and_correct(&mut temp, 0);
    if outcome == Outcome::Uncorrectable {
        return outcome;
    }
    let value = get_uint(&temp, 6, 23);
    set_uint(bits, start, 18, value);
    outcome
}

#[cfg(test)]
mod tests {
    use super::*;

    fn encode24(data: u16) -> [bool; 24] {
        let mut w = [false; 24];
        set_uint(&mut w, 0, 12, data as u64);
        let check = (0..12).filter(|&i| w[i]).fold(0u16, |s, i| s ^ CHECKSUMS[i]);
        set_uint(&mut w, 12, 11, check as u64);
        w[23] = w[..23].iter().filter(|&&b| b).count() & 1 == 1;
        w
    }

    #[test]
    fn golay24_corrects_every_pattern_up_to_3() {
        for data in [0x000, 0xFFF, 0xA5C, 0x123] {
            let cw = encode24(data);
            for p in patterns(24) {
                let mut w = cw;
                for i in 0..24 {
                    if p >> i & 1 == 1 {
                        w[i] = !w[i];
                    }
                }
                let o = golay24_check_and_correct(&mut w, 0);
                assert_eq!(w, cw, "pattern {p:#x}");
                assert_eq!(o, if p == 0 { Outcome::Clean } else { Outcome::Corrected(p.count_ones() as usize) });
            }
        }
    }

    #[test]
    fn golay24_flags_four_errors() {
        let cw = encode24(0x5A5);
        let mut w = cw;
        for i in [0, 5, 11, 20] {
            w[i] = !w[i];
        }
        assert_eq!(golay24_check_and_correct(&mut w, 0), Outcome::Uncorrectable);
    }

    #[test]
    fn golay23_corrects_up_to_3() {
        let cw23 = &encode24(0xC3A)[..23];
        for p in patterns(23) {
            let mut w = cw23.to_vec();
            for i in 0..23 {
                if p >> i & 1 == 1 {
                    w[i] = !w[i];
                }
            }
            golay23_check_and_correct(&mut w, 0);
            assert_eq!(w, cw23, "pattern {p:#x}");
        }
    }

    #[test]
    fn golay18_roundtrip() {
        let full = encode24(0x03F); // top 6 data bits zero
        assert!(full[..6].iter().all(|&b| !b));
        for flip in 0..18 {
            let mut w = full[6..].to_vec();
            w[flip] = !w[flip];
            assert_eq!(golay18_check_and_correct(&mut w, 0), Outcome::Corrected(1));
            assert_eq!(w, &full[6..]);
        }
    }
}
