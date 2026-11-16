//! Replays vectors produced by the original Java classes (see fixtures/GenVectors.java.txt) against the Rust port.
//! Line format: KIND|input|java_return|java_output. Bits are '0'/'1' strings; RS uses comma-separated symbols.
use sdrtrunk_edac::bch::{Bch63, Decoded};
use sdrtrunk_edac::bits::Outcome;
use sdrtrunk_edac::crc::*;
use sdrtrunk_edac::golay::*;
use sdrtrunk_edac::hamming::*;
use sdrtrunk_edac::rs::ReedSolomon;

fn bits(s: &str) -> Vec<bool> { s.chars().map(|c| c == '1').collect() }
fn bstr(b: &[bool]) -> String { b.iter().map(|&x| if x { '1' } else { '0' }).collect() }

#[test]
fn matches_java_oracle() {
    let text = include_str!("fixtures/java_vectors.txt");
    let bch = Bch63::p25_nid();
    let rs47 = ReedSolomon::rs_63_47_17();
    let rs35 = ReedSolomon::rs_63_35_29();
    let mut counts = std::collections::BTreeMap::<String, usize>::new();
    for (ln, line) in text.lines().enumerate() {
        let f: Vec<&str> = line.split('|').collect();
        let (kind, input, rc, out) = (f[0], f[1], f[2], f[3]);
        let ctx = format!("line {} {kind} in={input}", ln + 1);
        *counts.entry(kind.to_string()).or_default() += 1;
        match kind {
            "GOLAY24" => {
                let mut w = bits(input);
                let o = golay24_check_and_correct(&mut w, 0);
                let want = match rc { "0" => Outcome::Clean, "1" => Outcome::Corrected(0), _ => Outcome::Uncorrectable };
                assert_eq!(std::mem::discriminant(&o), std::mem::discriminant(&want), "{ctx}");
                assert_eq!(bstr(&w), out, "{ctx}");
            }
            "GOLAY23" => {
                let mut w = bits(input);
                let o = golay23_check_and_correct(&mut w, 0);
                let n = match o { Outcome::Clean => 0, Outcome::Corrected(n) => n, Outcome::Uncorrectable => 99 };
                assert_eq!(n.to_string(), rc, "{ctx}");
                assert_eq!(bstr(&w), out, "{ctx}");
            }
            "GOLAY18" => {
                let mut w = bits(input);
                golay18_check_and_correct(&mut w, 0);
                let diff = w.iter().zip(bits(input)).filter(|(a, b)| **a != *b).count();
                assert_eq!(diff.to_string(), rc, "{ctx}");
                assert_eq!(bstr(&w), out, "{ctx}");
            }
            "HAM10" | "HAM16C" => {
                let mut w = bits(input);
                let o = if kind == "HAM10" { hamming10_check_and_correct(&mut w, 0) } else { hamming16_check_and_correct(&mut w, 0) };
                let want = match rc { "0" => Outcome::Clean, "1" => Outcome::Corrected(1), _ => Outcome::Uncorrectable };
                assert_eq!(o, want, "{ctx}");
                assert_eq!(bstr(&w), out, "{ctx}");
            }
            "HAM16I" | "HAM17I" | "HAM13I" | "HAM15I" => {
                let w = bits(input);
                let got = match kind {
                    "HAM16I" => hamming16_error_index(&w, 0),
                    "HAM17I" => hamming17_error_index(&w, 0),
                    "HAM13I" => hamming13_error_index(&w, 0),
                    _ => hamming15_error_index(&w, 0),
                };
                let java: i32 = rc.parse().unwrap();
                match got {
                    HammingError::NoErrors => assert_eq!(java, -1, "{ctx}"),
                    HammingError::Bit(i) => assert_eq!(java, i as i32, "{ctx}"),
                    // Java returns 1000 for multiple errors, and (bug) -1 for unmapped 13/15 syndromes.
                    HammingError::Multiple => assert!(java == 1000 || java == -1, "{ctx} java={java}"),
                }
            }
            "BCH" => {
                let mut w = [false; 63];
                w.copy_from_slice(&bits(input));
                let java: i32 = rc.parse().unwrap();
                match bch.decode(&mut w) {
                    Decoded::Corrected(n) => { assert_eq!(java, n as i32, "{ctx}"); assert_eq!(bstr(&w), out, "{ctx}"); }
                    Decoded::Uncorrectable => assert_eq!(java, -1, "{ctx}"),
                }
            }
            "RS63_47" | "RS63_35" => {
                let mut s: Vec<u8> = input.split(',').map(|x| x.parse().unwrap()).collect();
                let o = if kind == "RS63_47" { rs47.decode(&mut s) } else { rs35.decode(&mut s) };
                if rc == "1" {
                    assert_eq!(o, Outcome::Uncorrectable, "{ctx}");
                } else {
                    assert_ne!(o, Outcome::Uncorrectable, "{ctx}");
                    let got = s.iter().map(|x| x.to_string()).collect::<Vec<_>>().join(",");
                    assert_eq!(got, out, "{ctx}");
                }
            }
            "CCITT80" => {
                let mut w = bits(input);
                let o = correct_ccitt80(&mut w, 0, 80);
                let want = match rc { "0" => Outcome::Clean, "1" => Outcome::Corrected(1), _ => Outcome::Uncorrectable };
                assert_eq!(o, want, "{ctx}");
                assert_eq!(bstr(&w), out, "{ctx}");
            }
            "CRC9" => assert_eq!(check_crc9(&bits(input), 0).passes(), rc == "PASSED", "{ctx}"),
            "CRC32" => assert_eq!(check_crc32(&bits(input), 0, 96).passes(), rc == "PASSED", "{ctx}"),
            "CRC_12_FACCH" => assert_eq!(crc12_facch(&bits(input)), rc == "1", "{ctx}"),
            "CRC_12_SACCH" => assert_eq!(crc12_sacch(&bits(input)), rc == "1", "{ctx}"),
            "CRC_16_LCCH" => assert_eq!(crc16_lcch(&bits(input)), rc == "1", "{ctx}"),
            other => panic!("unknown kind {other}"),
        }
    }
    eprintln!("replayed: {counts:?}");
}
