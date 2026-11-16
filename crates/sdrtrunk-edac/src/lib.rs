//! Error detection/correction codes, ported from sdrtrunk `io.github.dsheirer.edac`.
pub mod bch;
pub mod bits;
pub mod crc;
mod crc_tables;
pub mod golay;
pub mod gf64;
pub mod hamming;
pub mod rs;
