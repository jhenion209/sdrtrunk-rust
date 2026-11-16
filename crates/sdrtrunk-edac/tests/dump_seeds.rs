//! Writes valid codewords (from our encoders) to `$SEED_OUT`, so the Java oracle generator can corrupt and
//! decode them. Run: `SEED_OUT=/path cargo test --test dump_seeds -- --ignored`.
use sdrtrunk_edac::{bch::Bch63, rs::ReedSolomon};

#[test]
#[ignore]
fn dump_seeds() {
    let path = std::env::var("SEED_OUT").expect("SEED_OUT");
    let mut out = String::new();
    let mut x: u32 = 12345;
    let mut next = move || { x ^= x << 13; x ^= x >> 17; x ^= x << 5; x };
    let bch = Bch63::p25_nid();
    for _ in 0..40 {
        let d: Vec<bool> = (0..16).map(|_| next() & 1 == 1).collect();
        let cw = bch.encode(&d);
        out += &format!("BCH {}\n", cw.iter().map(|&b| if b { '1' } else { '0' }).collect::<String>());
    }
    for (name, rs) in [("RS63_47", ReedSolomon::rs_63_47_17()), ("RS63_35", ReedSolomon::rs_63_35_29())] {
        for _ in 0..40 {
            let d: Vec<u8> = (0..rs.k()).map(|_| (next() & 63) as u8).collect();
            let cw = rs.encode(&d);
            out += &format!("{name} {}\n", cw.iter().map(|s| s.to_string()).collect::<Vec<_>>().join(","));
        }
    }
    std::fs::write(path, out).unwrap();
}
