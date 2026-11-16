//! GF(2^6) arithmetic, primitive polynomial x^6 + x + 1 (0x43), shared by P25 BCH and Reed-Solomon codes.

pub const M: usize = 6;
pub const N: usize = (1 << M) - 1;
const PRIMITIVE_POLY: u32 = 0x43;

pub struct Gf64 {
    pub exp: [u8; 2 * N],
    pub log: [u8; N + 1],
}

impl Gf64 {
    pub fn new() -> Self {
        let mut exp = [0u8; 2 * N];
        let mut log = [0u8; N + 1];
        let mut x: u32 = 1;
        for i in 0..N {
            exp[i] = x as u8;
            log[x as usize] = i as u8;
            x <<= 1;
            if x & (1 << M) != 0 {
                x ^= PRIMITIVE_POLY;
            }
        }
        for i in N..2 * N {
            exp[i] = exp[i - N];
        }
        Self { exp, log }
    }

    pub fn mul(&self, a: u8, b: u8) -> u8 {
        if a == 0 || b == 0 {
            0
        } else {
            self.exp[self.log[a as usize] as usize + self.log[b as usize] as usize]
        }
    }

    pub fn inv(&self, a: u8) -> u8 {
        debug_assert!(a != 0);
        self.exp[N - self.log[a as usize] as usize]
    }
}

impl Default for Gf64 {
    fn default() -> Self {
        Self::new()
    }
}
