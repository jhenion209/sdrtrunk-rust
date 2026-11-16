//! Reed-Solomon over GF(64) for P25 (shortened RS(63,k) family). Replaces sdrtrunk `BerlekempMassey` +
//! `ReedSolomon_*_P25`. Generator roots are alpha^1..alpha^(n-k); `sym[j]` is the coefficient of x^j.
//! Decoding is Berlekamp-Massey + Chien search + Forney, so results equal the Java (Lin/Costello) decoder
//! for every correctable pattern, and both reject patterns whose located-root count differs from the
//! locator degree.

use crate::bits::Outcome;
use crate::gf64::{Gf64, N};

pub struct ReedSolomon {
    gf: Gf64,
    n: usize,
    k: usize,
    t: usize,
}

impl ReedSolomon {
    pub fn new(n: usize, k: usize) -> Self {
        assert!(n <= N && k < n);
        Self { gf: Gf64::new(), n, k, t: (n - k) / 2 }
    }

    // The parameters below are the ones sdrtrunk passes for each P25 class (note the Java 44_16_29 class
    // really constructs RS(63,35), and 24_12_13 / 24_16_9 construct RS(63,51) / RS(63,55)).
    pub fn rs_63_47_17() -> Self { Self::new(63, 47) }
    pub fn rs_63_35_29() -> Self { Self::new(63, 35) }
    pub fn rs_44_16_29() -> Self { Self::new(63, 35) }
    pub fn rs_24_12_13() -> Self { Self::new(63, 51) }
    pub fn rs_24_16_9() -> Self { Self::new(63, 55) }

    pub fn n(&self) -> usize { self.n }
    pub fn k(&self) -> usize { self.k }

    fn alpha(&self, e: usize) -> u8 {
        self.gf.exp[e % N]
    }

    /// Evaluate polynomial (low-degree first) at alpha^e.
    fn eval(&self, poly: &[u8], e: usize) -> u8 {
        poly.iter().enumerate().fold(0u8, |acc, (i, &c)| {
            if c == 0 { acc } else { acc ^ self.alpha(self.gf.log[c as usize] as usize + e * i) }
        })
    }

    /// Correct `sym` (length n, each symbol < 64) in place.
    pub fn decode(&self, sym: &mut [u8]) -> Outcome {
        assert_eq!(sym.len(), self.n);
        let two_t = 2 * self.t;
        let s: Vec<u8> = (1..=two_t)
            .map(|i| sym.iter().enumerate().fold(0u8, |a, (j, &r)| if r == 0 { a } else { a ^ self.alpha(self.gf.log[r as usize] as usize + i * j) }))
            .collect();
        if s.iter().all(|&x| x == 0) {
            return Outcome::Clean;
        }

        // Berlekamp-Massey -> error locator lambda (low-degree first), degree l.
        let mut lam = vec![0u8; two_t + 1];
        let mut prev = vec![0u8; two_t + 1];
        lam[0] = 1;
        prev[0] = 1;
        let (mut l, mut m, mut bd) = (0usize, 1usize, 1u8);
        for n in 0..two_t {
            let mut d = s[n];
            for i in 1..=l {
                d ^= self.gf.mul(lam[i], s[n - i]);
            }
            if d == 0 {
                m += 1;
                continue;
            }
            let coef = self.gf.mul(d, self.gf.inv(bd));
            let snapshot = lam.clone();
            for i in 0..=two_t - m {
                lam[i + m] ^= self.gf.mul(coef, prev[i]);
            }
            if 2 * l <= n {
                l = n + 1 - l;
                prev = snapshot;
                bd = d;
                m = 1;
            } else {
                m += 1;
            }
        }
        if l > self.t {
            return Outcome::Uncorrectable;
        }

        // Chien search: error at position j iff lambda(alpha^-j) == 0.
        let positions: Vec<usize> = (0..self.n).filter(|&j| self.eval(&lam[..=l], N - j % N) == 0).collect();
        if positions.len() != l {
            return Outcome::Uncorrectable;
        }

        // Forney: omega = S(x) * lambda(x) mod x^2t, e_j = omega(X^-1) / lambda'(X^-1), X = alpha^j (b = 1).
        let mut omega = vec![0u8; two_t];
        for (i, o) in omega.iter_mut().enumerate() {
            for j in 0..=i.min(l) {
                *o ^= self.gf.mul(lam[j], s[i - j]);
            }
        }
        let dlam: Vec<u8> = (1..=l).map(|i| if i % 2 == 1 { lam[i] } else { 0 }).collect();
        for &j in &positions {
            let xinv = N - j % N;
            let denom = self.eval(&dlam, xinv);
            if denom == 0 {
                return Outcome::Uncorrectable;
            }
            let e = self.gf.mul(self.eval(&omega, xinv), self.gf.inv(denom));
            sym[j] ^= e;
        }
        Outcome::Corrected(positions.len())
    }

    /// Systematic encode for tests/tools: returns n symbols, parity in positions 0..n-k, data above.
    pub fn encode(&self, data: &[u8]) -> Vec<u8> {
        assert_eq!(data.len(), self.k);
        let p = self.n - self.k;
        // g(x) = prod (x - alpha^i), i=1..=p, low-degree first.
        let mut g = vec![1u8];
        for i in 1..=p {
            let root = self.alpha(i);
            let mut next = vec![0u8; g.len() + 1];
            for (d, &c) in g.iter().enumerate() {
                next[d + 1] ^= c;
                next[d] ^= self.gf.mul(c, root);
            }
            g = next;
        }
        let mut rem = vec![0u8; self.n];
        rem[p..].copy_from_slice(data);
        for d in (p..self.n).rev() {
            let c = rem[d];
            if c != 0 {
                for (i, &gc) in g.iter().enumerate() {
                    rem[d - p + i] ^= self.gf.mul(c, gc);
                }
            }
        }
        let mut out = rem[..p].to_vec();
        out.extend_from_slice(data);
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn data(k: usize, seed: u32) -> Vec<u8> {
        let mut x = seed;
        (0..k).map(|_| { x = x.wrapping_mul(1664525).wrapping_add(1013904223); (x >> 24) as u8 & 63 }).collect()
    }

    #[test]
    fn valid_codeword_has_zero_syndrome() {
        for rs in [ReedSolomon::rs_63_47_17(), ReedSolomon::rs_63_35_29(), ReedSolomon::rs_24_12_13(), ReedSolomon::rs_24_16_9()] {
            let mut cw = rs.encode(&data(rs.k(), 3));
            assert_eq!(rs.decode(&mut cw), Outcome::Clean);
        }
    }

    #[test]
    fn corrects_up_to_t_symbol_errors() {
        for rs in [ReedSolomon::rs_63_47_17(), ReedSolomon::rs_63_35_29(), ReedSolomon::rs_24_12_13(), ReedSolomon::rs_24_16_9()] {
            let t = (rs.n() - rs.k()) / 2;
            for trial in 1..=200u32 {
                let cw = rs.encode(&data(rs.k(), trial));
                let mut bad = cw.clone();
                let nerr = (trial as usize % t) + 1;
                let mut r = trial.wrapping_mul(2654435761);
                let mut done = 0;
                while done < nerr {
                    r = r.wrapping_mul(1664525).wrapping_add(1013904223);
                    let pos = (r >> 8) as usize % rs.n();
                    if bad[pos] == cw[pos] {
                        bad[pos] ^= ((r >> 3) as u8 % 63) + 1;
                        done += 1;
                    }
                }
                assert_eq!(rs.decode(&mut bad), Outcome::Corrected(nerr), "n={} k={} trial {trial}", rs.n(), rs.k());
                assert_eq!(bad, cw);
            }
        }
    }

    #[test]
    fn never_miscorrects_to_original_beyond_t() {
        let rs = ReedSolomon::rs_24_16_9(); // t = 4
        let cw = rs.encode(&data(rs.k(), 5));
        let mut bad = cw.clone();
        for p in 0..9 {
            bad[p * 5] ^= 17;
        }
        match rs.decode(&mut bad) {
            Outcome::Uncorrectable => {}
            _ => assert_ne!(bad, cw),
        }
    }
}
