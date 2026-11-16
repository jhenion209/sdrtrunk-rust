//! Network ID (NID) handling for the C4FM timing path: data unit IDs, NAC tracking, BCH decoding with the
//! observed-NAC retry, and parity validation (`P25P1DataUnitID`, `NACTracker`, `P25P1NIDValidator`,
//! `BCH_63_16_23_P25`).
use sdrtrunk_edac::bch::{Bch63, Decoded};
use sdrtrunk_edac::bits::{get_uint, set_uint};

/// Data unit IDs (`P25P1DataUnitID`). Only the values the NID can carry are produced by `from_value`; the block
/// variants are used internally while a multi-block unit is assembled.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum DataUnitId {
    HeaderDataUnit,
    TerminatorDataUnit,
    LogicalLinkDataUnit1,
    TrunkingSignalingBlock1,
    LogicalLinkDataUnit2,
    PacketDataUnit,
    TerminatorDataUnitLinkControl,
    /// Marker for "no NID decoded yet".
    PlaceHolder,
    PacketDataUnitBlock1,
    PacketDataUnitBlock2,
    PacketDataUnitBlock3,
    PacketDataUnitBlock4,
    PacketDataUnitBlock5,
    TrunkingSignalingBlock2,
    TrunkingSignalingBlock3,
    Unknown,
}

impl DataUnitId {
    /// (message length bits, status dibits, null bits) from the Java enum constructor arguments.
    fn layout(self) -> (i32, i32, i32) {
        match self {
            DataUnitId::HeaderDataUnit => (648, 11, 10),
            DataUnitId::TerminatorDataUnit => (0, 2, 28),
            DataUnitId::LogicalLinkDataUnit1 | DataUnitId::LogicalLinkDataUnit2 => (1568, 24, 0),
            DataUnitId::TrunkingSignalingBlock1 | DataUnitId::PacketDataUnit => (196, 5, 42),
            DataUnitId::TerminatorDataUnitLinkControl => (288, 6, 20),
            DataUnitId::PlaceHolder => (1572, 24, 0),
            DataUnitId::PacketDataUnitBlock1 | DataUnitId::TrunkingSignalingBlock2 => (196 * 2, 8, 56),
            DataUnitId::PacketDataUnitBlock2 | DataUnitId::TrunkingSignalingBlock3 => (196 * 3, 10, 0),
            DataUnitId::PacketDataUnitBlock3 => (196 * 4, 13, 14),
            DataUnitId::PacketDataUnitBlock4 => (196 * 5, 16, 28),
            DataUnitId::PacketDataUnitBlock5 => (196 * 6, 19, 42),
            DataUnitId::Unknown => (-1, 0, 0),
        }
    }

    /// Bits to collect for this unit after the sync and NID (message plus null bits).
    pub fn message_length(self) -> i32 {
        let (len, _, null) = self.layout();
        len + null
    }

    /// Dibits elapsed from the start of the sync to the end of this unit: sync, NID, message, status, nulls.
    pub fn elapsed_dibit_length(self) -> i32 {
        let (len, status, null) = self.layout();
        56 + (len / 2) + status + (null / 2)
    }

    pub fn from_value(value: u64) -> DataUnitId {
        match value {
            0 => DataUnitId::HeaderDataUnit,
            3 => DataUnitId::TerminatorDataUnit,
            5 => DataUnitId::LogicalLinkDataUnit1,
            7 => DataUnitId::TrunkingSignalingBlock1,
            10 => DataUnitId::LogicalLinkDataUnit2,
            12 => DataUnitId::PacketDataUnit,
            15 => DataUnitId::TerminatorDataUnitLinkControl,
            _ => DataUnitId::Unknown,
        }
    }
}

pub const NO_TRACKED_NAC: i32 = -1;
pub const NO_EXPECTED_NAC: i32 = -1;
const MAX_TRACKER_COUNT: usize = 3;
const MIN_OBSERVATION_THRESHOLD: i32 = 3;

/// Tracks the most frequently observed NACs. Java orders by wall-clock time and `HashMap` iteration; here recency is
/// a counter and ties between equal counts follow `HashMap<Integer>` bucket order (`nac & 15`, then insertion).
#[derive(Default)]
pub struct NacTracker {
    entries: Vec<TrackerEntry>,
    clock: u64,
}

struct TrackerEntry {
    nac: i32,
    count: i32,
    seen: u64,
    inserted: u64,
}

impl NacTracker {
    pub fn reset(&mut self) { self.entries.clear(); }

    pub fn track(&mut self, nac: i32) {
        self.clock += 1;
        let clock = self.clock;
        if let Some(e) = self.entries.iter_mut().find(|e| e.nac == nac) {
            e.count = e.count.saturating_add(1);
            e.seen = clock;
            return;
        }
        self.entries.push(TrackerEntry { nac, count: 1, seen: clock, inserted: clock });
        if self.entries.len() > MAX_TRACKER_COUNT {
            let oldest = self.entries.iter().enumerate().min_by_key(|(_, e)| e.seen).map(|(i, _)| i).unwrap();
            self.entries.remove(oldest);
        }
    }

    pub fn tracked_nac(&self) -> i32 {
        let mut ordered: Vec<&TrackerEntry> = self.entries.iter().collect();
        ordered.sort_by_key(|e| (e.nac & 15, e.inserted)); // HashMap iteration order
        ordered.sort_by(|a, b| b.count.cmp(&a.count)); // stable, count descending
        match ordered.first() {
            Some(e) if e.count >= MIN_OBSERVATION_THRESHOLD => e.nac,
            _ => NO_TRACKED_NAC,
        }
    }
}

pub const NID_BITS: usize = 64;
pub const NID_NAC_END: usize = 11; // bits 0..=11
pub const NID_DUID_START: usize = 12;
pub const NID_DUID_END: usize = 15;
const PARITY_BIT_INDEX: usize = 63;
const MAX_CORRECTABLE_BIT_COUNT: i32 = 11;
const NOT_CORRECTED: i32 = -1;

pub fn nid_nac(nid: &[bool; NID_BITS]) -> u64 { get_uint(nid, 0, NID_NAC_END) }
pub fn nid_duid(nid: &[bool; NID_BITS]) -> u64 { get_uint(nid, NID_DUID_START, NID_DUID_END) }

fn bch_decode(bch: &Bch63, nid: &mut [bool; NID_BITS]) -> i32 {
    let mut cw = [false; 63];
    cw.copy_from_slice(&nid[..63]);
    match bch.decode(&mut cw) {
        Decoded::Corrected(n) => {
            nid[..63].copy_from_slice(&cw);
            n as i32
        }
        Decoded::Uncorrectable => NOT_CORRECTED,
    }
}

/// Corrects the 64-bit NID in place. Returns the corrected bit count or -1 when uncorrectable. With an observed NAC
/// an uncorrectable NID is retried once with its NAC field overwritten (`decode(message, observedNAC)`).
pub fn decode_nid(bch: &Bch63, nid: &mut [bool; NID_BITS], observed_nac: Option<i32>) -> i32 {
    let mut corrected = bch_decode(bch, nid);
    if let Some(observed) = observed_nac {
        if corrected == NOT_CORRECTED && (0..=0xFFF).contains(&observed) && nid_nac(nid) != observed as u64 {
            set_uint(nid, 0, 12, observed as u64);
            corrected = bch_decode(bch, nid);
        }
    }
    corrected
}

/// Adds the DUID parity bit check to the BCH corrected bit count; -1 when invalid.
pub fn validate(nid: &[bool; NID_BITS], corrected: i32) -> i32 {
    if corrected < 0 {
        return -1;
    }
    let duid = nid_duid(nid);
    let expected_parity = ((duid & 0x2) != 0) ^ ((duid & 0x1) != 0);
    if nid[PARITY_BIT_INDEX] != expected_parity {
        if corrected >= MAX_CORRECTABLE_BIT_COUNT {
            return -1;
        }
        return corrected + 1;
    }
    corrected
}

pub fn validate_expected_nac(nid: &[bool; NID_BITS], corrected: i32, expected_nac: i32) -> i32 {
    let count = validate(nid, corrected);
    if count >= 0 && nid_nac(nid) == expected_nac as u64 { count } else { -1 }
}
