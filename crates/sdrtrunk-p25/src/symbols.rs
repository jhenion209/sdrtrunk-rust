//! Soft symbol buffer (`message.SymbolMessage`): demodulated phase values for the dibits of a message under assembly.

#[derive(Clone, Debug)]
pub struct SymbolMessage {
    symbols: Vec<f32>,
    pointer: usize,
}

impl SymbolMessage {
    /// Empty message with room for `dibit_count` symbols.
    pub fn new(dibit_count: usize) -> Self {
        Self { symbols: vec![0.0; dibit_count], pointer: 0 }
    }

    /// Full message holding a copy of `symbols`.
    pub fn from_slice(symbols: &[f32]) -> Self {
        Self { symbols: symbols.to_vec(), pointer: symbols.len() }
    }

    pub fn add(&mut self, symbol_phase: f32) {
        self.symbols[self.pointer] = symbol_phase;
        self.pointer += 1;
    }

    pub fn is_full(&self) -> bool { self.pointer >= self.symbols.len() }
    pub fn size(&self) -> usize { self.symbols.len() }
    pub fn current_size(&self) -> usize { self.pointer }
    pub fn get(&self, index: usize) -> f32 { self.symbols[index] }
    pub fn set(&mut self, index: usize, value: f32) { self.symbols[index] = value; }

    pub fn set_size(&mut self, dibit_count: usize) {
        if dibit_count != self.symbols.len() {
            self.symbols.resize(dibit_count, 0.0);
            self.pointer = self.pointer.min(dibit_count);
        }
    }

    /// Symbols covering message bits `start_bit..=end_bit` (two bits per symbol), clipped to what has arrived.
    pub fn sub_message(&self, start_bit: usize, end_bit: usize) -> SymbolMessage {
        let end = self.current_size().min((end_bit + 1) / 2);
        let start = (start_bit / 2).min(end);
        SymbolMessage::from_slice(&self.symbols[start..end])
    }
}
