//! Trunking signaling block (TSBK) decoding as far as framing needs it: deinterleave, 1/2-rate Viterbi, CRC-CCITT
//! (`TSBKMessageFactory.deinterleaveViterbiAndCrc`). Opcode parsing lives in a later module.
use crate::symbols::SymbolMessage;
use crate::trellis::{deinterleave_data_symbols, viterbi_half_rate_decode};
use sdrtrunk_edac::bits::{get_uint, Outcome};
use sdrtrunk_edac::crc::correct_ccitt80;

pub const CRC_FAIL: i32 = -1;

/// One decoded 96-bit TSBK (80 data bits + 16 CRC bits).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TsbkBlock {
    pub bits: Vec<bool>,
    /// 0 clean, 1 single-bit CRC correction, -1 failed CRC.
    pub corrected_bit_count: i32,
}

impl TsbkBlock {
    pub fn is_valid(&self) -> bool { self.corrected_bit_count >= 0 }
    /// Last-block flag, bit 0 of the (corrected) block.
    pub fn is_last_block(&self) -> bool { self.bits[0] }
}

/// Decodes a 196-bit (98 dibit) interleaved block.
pub fn deinterleave_viterbi_and_crc(symbols: &SymbolMessage) -> TsbkBlock {
    let mut bits = viterbi_half_rate_decode(&deinterleave_data_symbols(symbols));
    let corrected_bit_count = match correct_ccitt80(&mut bits, 0, 80) {
        Outcome::Clean => 0,
        Outcome::Corrected(n) => n as i32,
        Outcome::Uncorrectable => CRC_FAIL,
    };
    TsbkBlock { bits, corrected_bit_count }
}

/// PDU header format field (bits 3..=7); 21 and 23 are the multi-block trunking control formats.
pub fn is_multi_block_trunking_format(bits: &[bool]) -> bool {
    matches!(get_uint(bits, 3, 7), 21 | 23)
}
