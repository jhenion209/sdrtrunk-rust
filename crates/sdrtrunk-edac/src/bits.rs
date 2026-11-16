//! Bit-message helpers. A message is `&[bool]` with index 0 as the first (most significant) bit,
//! matching sdrtrunk's `BinaryMessage` layout.

/// Unsigned value of bits `start..=end` (inclusive), MSB first. At most 64 bits.
pub fn get_uint(bits: &[bool], start: usize, end_inclusive: usize) -> u64 {
    debug_assert!(end_inclusive >= start && end_inclusive - start < 64);
    bits[start..=end_inclusive].iter().fold(0u64, |v, &b| (v << 1) | b as u64)
}

/// Write the low `len` bits of `value` into `bits[start..start+len]`, MSB first.
pub fn set_uint(bits: &mut [bool], start: usize, len: usize, value: u64) {
    for i in 0..len {
        bits[start + i] = (value >> (len - 1 - i)) & 1 == 1;
    }
}

/// Result of an error check/correct pass over one codeword.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// Codeword was already valid.
    Clean,
    /// Codeword was repaired; payload is the number of bits flipped.
    Corrected(usize),
    /// Too many errors to repair; message left unchanged.
    Uncorrectable,
}
