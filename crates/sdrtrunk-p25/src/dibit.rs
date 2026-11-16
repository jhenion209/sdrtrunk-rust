//! Four-level symbol (`io.github.dsheirer.dsp.symbol.Dibit`).
use std::f64::consts::PI;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Dibit {
    /// +3 symbol, bits 01
    D01Plus3,
    /// +1 symbol, bits 00
    D00Plus1,
    /// -1 symbol, bits 10
    D10Minus1,
    /// -3 symbol, bits 11
    D11Minus3,
}

/// Soft symbol decision boundary between the +/-1 and +/-3 symbols, (float)(PI / 2).
pub const SOFT_SYMBOL_QUADRANT_BOUNDARY: f32 = (PI / 2.0) as f32;

impl Dibit {
    /// Ideal demodulated phase in radians.
    pub fn ideal_phase(self) -> f32 {
        match self {
            Dibit::D01Plus3 => ((PI / 4.0) * 3.0) as f32,
            Dibit::D00Plus1 => (PI / 4.0) as f32,
            Dibit::D10Minus1 => (PI / -4.0) as f32,
            Dibit::D11Minus3 => ((PI / 4.0) * -3.0) as f32,
        }
    }

    pub fn bit1(self) -> bool { matches!(self, Dibit::D10Minus1 | Dibit::D11Minus3) }
    pub fn bit2(self) -> bool { matches!(self, Dibit::D01Plus3 | Dibit::D11Minus3) }

    /// 2-bit value: 00 -> 0, 01 -> 1, 10 -> 2, 11 -> 3.
    pub fn value(self) -> u8 {
        match self {
            Dibit::D00Plus1 => 0,
            Dibit::D01Plus3 => 1,
            Dibit::D10Minus1 => 2,
            Dibit::D11Minus3 => 3,
        }
    }

    pub fn from_value(value: u8) -> Dibit {
        match value {
            1 => Dibit::D01Plus3,
            2 => Dibit::D10Minus1,
            3 => Dibit::D11Minus3,
            _ => Dibit::D00Plus1,
        }
    }

    /// Number of differing bits (0, 1 or 2).
    pub fn bit_error_from(self, other: Dibit) -> i32 {
        match self.value() ^ other.value() {
            0 => 0,
            3 => 2,
            _ => 1,
        }
    }

    /// Symbol decision for a soft symbol in radians (`P25P1DemodulatorC4FM.toSymbol`).
    pub fn from_soft(sample: f32) -> Dibit {
        if sample > 0.0 {
            if sample > SOFT_SYMBOL_QUADRANT_BOUNDARY { Dibit::D01Plus3 } else { Dibit::D00Plus1 }
        } else if sample < -SOFT_SYMBOL_QUADRANT_BOUNDARY {
            Dibit::D11Minus3
        } else {
            Dibit::D10Minus1
        }
    }
}
