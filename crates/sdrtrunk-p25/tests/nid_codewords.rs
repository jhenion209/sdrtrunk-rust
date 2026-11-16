//! NID codeword construction and the BCH + parity validation path (`nid.rs`).
//! Run with `--nocapture` to print the codewords used by the Java demodulator vector generator.
use sdrtrunk_edac::bch::Bch63;
use sdrtrunk_edac::bits::set_uint;
use sdrtrunk_p25::nid::{self, DataUnitId, NacTracker, NID_BITS};

/// 64-bit NID: NAC(12) DUID(4) BCH parity(47) and the DUID parity bit.
pub fn build_nid(bch: &Bch63, nac: u64, duid: u64) -> [bool; NID_BITS] {
    let mut data = [false; 16];
    set_uint(&mut data, 0, 12, nac);
    set_uint(&mut data, 12, 4, duid);
    let cw = bch.encode(&data);
    let mut nid = [false; NID_BITS];
    nid[..63].copy_from_slice(&cw);
    nid[63] = ((duid & 2) != 0) ^ ((duid & 1) != 0);
    nid
}

fn as_u64(nid: &[bool; NID_BITS]) -> u64 { nid.iter().fold(0u64, |v, &b| (v << 1) | b as u64) }

#[test]
fn built_nids_decode_and_validate() {
    let bch = Bch63::p25_nid();
    for &(nac, duid) in &[(0x293u64, 5u64), (0x293, 7), (0x293, 3), (0x293, 0), (0x1F1, 10), (0xFFF, 15), (0x001, 12)] {
        let nid = build_nid(&bch, nac, duid);
        println!("NID nac={nac:03X} duid={duid} -> {:016X}", as_u64(&nid));
        let mut copy = nid;
        let corrected = nid::decode_nid(&bch, &mut copy, None);
        assert_eq!(corrected, 0, "clean NID must decode with no corrections");
        assert_eq!(nid::validate(&copy, corrected), 0, "parity bit must match");
        assert_eq!(nid::nid_nac(&copy), nac);
        assert_ne!(DataUnitId::from_value(nid::nid_duid(&copy)), DataUnitId::Unknown, "duid {duid}");
    }
}

#[test]
fn bit_errors_are_corrected_and_counted() {
    let bch = Bch63::p25_nid();
    let clean = build_nid(&bch, 0x293, 5);
    let mut nid = clean;
    for i in [3usize, 20, 41] {
        nid[i] = !nid[i];
    }
    let corrected = nid::decode_nid(&bch, &mut nid, None);
    assert_eq!(corrected, 3);
    assert_eq!(&nid[..63], &clean[..63]);

    // A bad DUID parity bit costs one extra corrected bit; at the correction limit it is rejected.
    let mut bad_parity = clean;
    bad_parity[63] = !bad_parity[63];
    assert_eq!(nid::validate(&bad_parity, 0), 1);
    assert_eq!(nid::validate(&bad_parity, 11), -1);
    assert_eq!(nid::validate(&clean, -1), -1);
}

#[test]
fn observed_nac_rescues_a_garbled_nac_field() {
    let bch = Bch63::p25_nid();
    let clean = build_nid(&bch, 0x293, 5);
    // Flip 13 bits inside the NAC/DUID area: beyond t=11, so plain decoding fails.
    let mut nid = clean;
    for i in 0..13 {
        nid[i] = !nid[i];
    }
    let mut plain = nid;
    assert_eq!(nid::decode_nid(&bch, &mut plain, None), -1);
    // With the observed NAC substituted the remaining errors are correctable.
    let mut rescued = nid;
    let corrected = nid::decode_nid(&bch, &mut rescued, Some(0x293));
    assert!(corrected >= 0, "observed NAC retry should correct the NID");
    assert_eq!(nid::nid_nac(&rescued), 0x293);
}

#[test]
fn nac_tracker_needs_three_observations_and_evicts_oldest() {
    let mut t = NacTracker::default();
    assert_eq!(t.tracked_nac(), nid::NO_TRACKED_NAC);
    t.track(0x293);
    t.track(0x293);
    assert_eq!(t.tracked_nac(), nid::NO_TRACKED_NAC);
    t.track(0x293);
    assert_eq!(t.tracked_nac(), 0x293);
    for nac in [1, 2, 3] {
        t.track(nac); // the fourth distinct NAC evicts the least recently seen
    }
    assert_eq!(t.tracked_nac(), nid::NO_TRACKED_NAC, "0x293 was the oldest and was evicted");
}
