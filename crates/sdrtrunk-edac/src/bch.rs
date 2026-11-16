//! Binary BCH decoder over GF(2^6), N=63. Port of sdrtrunk `edac/bch/BCH*.java`.
//!
//! Bit index 0 is the most significant codeword bit (x^(N-1)), matching the Java bit-message layout.
//! Decoding is bounded-distance: a codeword with more than `t` errors is reported as uncorrectable.
//! The Java closed-form polynomial root finder is replaced by a Chien search; results are identical
//! because both enumerate all roots of the error locator and require count == degree.

use crate::gf64::{Gf64, N};

/// P25 NID: BCH(63,16,23), corrects up to 11 bit errors.
pub const P25_NID_K: usize = 16;
pub const P25_NID_T: usize = 11;

pub struct Bch63 {
    k: usize,
    t: usize,
    gf: Gf64,
}

#[derive(Debug, PartialEq, Eq)]
pub enum Decoded {
    /// Number of bits corrected (0 = clean).
    Corrected(usize),
    Uncorrectable,
}

impl Bch63 {
    pub fn new(k: usize, t: usize) -> Self {
        Self { k, t, gf: Gf64::new() }
    }

    pub fn p25_nid() -> Self {
        Self::new(P25_NID_K, P25_NID_T)
    }

    pub fn k(&self) -> usize {
        self.k
    }

    fn mul(&self, a: u8, b: u8) -> u8 {
        self.gf.mul(a, b)
    }

    fn inv(&self, a: u8) -> u8 {
        self.gf.inv(a)
    }

    /// `bits[i]` is codeword bit i (0 = MSB). Corrects in place.
    pub fn decode(&self, bits: &mut [bool; N]) -> Decoded {
        let two_t = 2 * self.t;
        let mut syn = vec![0u8; two_t];
        for (i, _) in bits.iter().enumerate().filter(|(_, b)| **b) {
            let pos = N - 1 - i;
            for (j, s) in syn.iter_mut().enumerate() {
                *s ^= self.gf.exp[((j + 1) * pos) % N];
            }
        }
        if syn.iter().all(|&s| s == 0) {
            return Decoded::Corrected(0);
        }

        // Berlekamp-Massey over GF(64).
        let mut c = vec![0u8; two_t + 1];
        let mut b = vec![0u8; two_t + 1];
        c[0] = 1;
        b[0] = 1;
        let (mut l, mut m, mut bd) = (0usize, 1usize, 1u8);
        for n in 0..two_t {
            let mut d = syn[n];
            for i in 1..=l {
                d ^= self.mul(c[i], syn[n - i]);
            }
            if d == 0 {
                m += 1;
            } else if 2 * l <= n {
                let prev = c.clone();
                let coef = self.mul(d, self.inv(bd));
                for i in 0..=two_t - m {
                    c[i + m] ^= self.mul(coef, b[i]);
                }
                l = n + 1 - l;
                b = prev;
                bd = d;
                m = 1;
            } else {
                let coef = self.mul(d, self.inv(bd));
                for i in 0..=two_t - m {
                    c[i + m] ^= self.mul(coef, b[i]);
                }
                m += 1;
            }
        }
        if l > self.t {
            return Decoded::Uncorrectable;
        }

        // Chien search: error at x-power p iff locator(alpha^-p) == 0.
        let mut errors = Vec::with_capacity(l);
        for p in 0..N {
            let mut v = 0u8;
            for (i, &ci) in c.iter().enumerate().take(l + 1) {
                if ci != 0 {
                    v ^= self.gf.exp[(self.gf.log[ci as usize] as usize + (N - p) * i) % N];
                }
            }
            if v == 0 {
                errors.push(N - 1 - p);
            }
        }
        if errors.len() != l {
            return Decoded::Uncorrectable;
        }
        for &i in &errors {
            bits[i] = !bits[i];
        }
        Decoded::Corrected(l)
    }

    /// Systematic encode: `data` (k bits, MSB first) followed by N-k parity bits.
    /// Generator = LCM of minimal polynomials of alpha^1..alpha^2t.
    pub fn encode(&self, data: &[bool]) -> [bool; N] {
        assert_eq!(data.len(), self.k);
        let g = self.generator();
        let r = N - self.k;
        let mut reg = vec![false; r];
        for &d in data {
            let fb = d ^ reg[0];
            reg.rotate_left(1);
            reg[r - 1] = false;
            if fb {
                for (i, rb) in reg.iter_mut().enumerate() {
                    *rb ^= g[i + 1];
                }
            }
        }
        let mut out = [false; N];
        out[..self.k].copy_from_slice(data);
        out[self.k..].copy_from_slice(&reg);
        out
    }

    /// Generator polynomial coefficients, highest degree first, length N-k+1.
    fn generator(&self) -> Vec<bool> {
        // Collect cyclotomic cosets of 1..=2t, multiply (x - alpha^e) over GF(64).
        let mut seen = [false; N];
        let mut poly: Vec<u8> = vec![1]; // highest degree first
        for s in 1..=2 * self.t {
            let mut e = s % N;
            if seen[e] {
                continue;
            }
            while !seen[e] {
                seen[e] = true;
                let root = self.gf.exp[e];
                let mut next = vec![0u8; poly.len() + 1];
                for (i, &p) in poly.iter().enumerate() {
                    next[i] ^= p;
                    next[i + 1] ^= self.mul(p, root);
                }
                poly = next;
                e = (e * 2) % N;
            }
        }
        poly.iter().map(|&p| p != 0).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn data(seed: u32) -> Vec<bool> {
        (0..16).map(|i| (seed >> i) & 1 == 1).collect()
    }

    #[test]
    fn generator_degree_is_n_minus_k() {
        let b = Bch63::p25_nid();
        assert_eq!(b.generator().len(), N - P25_NID_K + 1);
    }

    #[test]
    fn clean_codeword_has_zero_errors() {
        let b = Bch63::p25_nid();
        let mut cw = b.encode(&data(0x1234));
        assert_eq!(b.decode(&mut cw), Decoded::Corrected(0));
    }

    #[test]
    fn corrects_up_to_t_errors() {
        let b = Bch63::p25_nid();
        let mut rng = 0x9E3779B9u32;
        let mut next = || {
            rng ^= rng << 13;
            rng ^= rng >> 17;
            rng ^= rng << 5;
            rng
        };
        for trial in 0..2000u32 {
            let cw = b.encode(&data(next()));
            let nerr = (trial as usize % P25_NID_T) + 1;
            let mut bad = cw;
            let mut flipped = 0;
            while flipped < nerr {
                let i = next() as usize % N;
                if bad[i] == cw[i] {
                    bad[i] = !bad[i];
                    flipped += 1;
                }
            }
            assert_eq!(b.decode(&mut bad), Decoded::Corrected(nerr));
            assert_eq!(bad, cw);
        }
    }

    #[test]
    fn rejects_beyond_t_errors() {
        let b = Bch63::p25_nid();
        let cw = b.encode(&data(0xBEEF));
        let mut bad = cw;
        // Burst of t+8 errors: never decodes back to the original codeword.
        for bit in bad.iter_mut().take(P25_NID_T + 8) {
            *bit = !*bit;
        }
        match b.decode(&mut bad) {
            Decoded::Uncorrectable => {}
            Decoded::Corrected(_) => assert_ne!(bad, cw),
        }
    }
}
