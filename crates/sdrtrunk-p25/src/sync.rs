//! P25 Phase 1 frame sync pattern and soft sync detector
//! (`sync.P25P1SyncDetector`, `sync.P25P1SoftSyncDetectorScalar`).
use crate::dibit::Dibit;

pub const SYNC_PATTERN: u64 = 0x5575F5FF77FF;
pub const SYNC_SYMBOLS: usize = 24;

/// The 24 sync dibits, first transmitted first. Only the +3 and -3 symbols occur in the pattern.
pub fn sync_pattern_dibits() -> [Dibit; SYNC_SYMBOLS] {
    let mut dibits = [Dibit::D00Plus1; SYNC_SYMBOLS];
    for x in 0..SYNC_SYMBOLS {
        let value = (SYNC_PATTERN >> (2 * x)) & 3;
        dibits[SYNC_SYMBOLS - 1 - x] = if value == 1 { Dibit::D01Plus3 } else { Dibit::D11Minus3 };
    }
    dibits
}

/// Ideal phase for each sync dibit.
pub fn sync_pattern_symbols() -> [f32; SYNC_SYMBOLS] {
    let dibits = sync_pattern_dibits();
    let mut symbols = [0.0f32; SYNC_SYMBOLS];
    for x in 0..SYNC_SYMBOLS {
        symbols[x] = dibits[x].ideal_phase();
    }
    symbols
}

/// Correlates the most recent 24 soft symbols against the sync pattern.
#[derive(Clone)]
pub struct SoftSyncDetector {
    symbols: [f32; 48],
    pointer: usize,
    pattern: [f32; SYNC_SYMBOLS],
}

impl Default for SoftSyncDetector {
    fn default() -> Self { Self::new() }
}

impl SoftSyncDetector {
    pub fn new() -> Self {
        Self { symbols: [0.0; 48], pointer: 0, pattern: sync_pattern_symbols() }
    }

    pub fn reset(&mut self) {
        self.symbols = [0.0; 48];
        self.pointer = 0;
    }

    /// Adds a soft symbol and returns the correlation score.
    pub fn process(&mut self, soft_symbol: f32) -> f32 {
        self.symbols[self.pointer] = soft_symbol;
        self.symbols[self.pointer + 24] = soft_symbol;
        self.pointer = (self.pointer + 1) % 24;
        let mut score = 0.0f32;
        for x in 0..SYNC_SYMBOLS {
            score += self.pattern[x] * self.symbols[self.pointer + x];
        }
        score
    }
}
