//! Data block deinterleaving and the soft-decision QPSK Viterbi decoder for P25 1/2-rate trellis coded data
//! (`P25P1Interleave.deinterleaveDataSymbols`, `QPSKViterbiDecoder`, `QPSKViterbiDecoder_1_2_P25`).
use crate::dibit::Dibit;
use crate::symbols::SymbolMessage;
use std::f64::consts::PI;

#[rustfmt::skip]
const DATA_DEINTERLEAVE: [usize; 196] = [
    0, 1, 2, 3, 16, 17, 18, 19, 32, 33, 34, 35, 48, 49, 50, 51, 64, 65, 66, 67, 80, 81, 82, 83, 96, 97, 98, 99,
    112, 113, 114, 115, 128, 129, 130, 131, 144, 145, 146, 147, 160, 161, 162, 163, 176, 177, 178, 179, 192, 193,
    194, 195, 4, 5, 6, 7, 20, 21, 22, 23, 36, 37, 38, 39, 52, 53, 54, 55, 68, 69, 70, 71, 84, 85, 86, 87, 100, 101,
    102, 103, 116, 117, 118, 119, 132, 133, 134, 135, 148, 149, 150, 151, 164, 165, 166, 167, 180, 181, 182, 183,
    8, 9, 10, 11, 24, 25, 26, 27, 40, 41, 42, 43, 56, 57, 58, 59, 72, 73, 74, 75, 88, 89, 90, 91, 104, 105, 106,
    107, 120, 121, 122, 123, 136, 137, 138, 139, 152, 153, 154, 155, 168, 169, 170, 171, 184, 185, 186, 187, 12,
    13, 14, 15, 28, 29, 30, 31, 44, 45, 46, 47, 60, 61, 62, 63, 76, 77, 78, 79, 92, 93, 94, 95, 108, 109, 110,
    111, 124, 125, 126, 127, 140, 141, 142, 143, 156, 157, 158, 159, 172, 173, 174, 175, 188, 189, 190, 191,
];

/// Moves each received data dibit to its place in the 98-dibit trellis coded block.
pub fn deinterleave_data_symbols(interleaved: &SymbolMessage) -> SymbolMessage {
    let mut out = SymbolMessage::new(DATA_DEINTERLEAVE.len() / 2);
    for dibit in 0..DATA_DEINTERLEAVE.len() / 2 {
        out.set(DATA_DEINTERLEAVE[2 * dibit] / 2, interleaved.get(dibit));
    }
    out
}

/// Rate 1/2 trellis encoder, the inverse of [`viterbi_half_rate_decode`]: 96 data bits become 98 dibit values (0-3)
/// in deinterleaved order, ending with a flush symbol. Used to build test signals.
pub fn encode_half_rate(bits: &[bool]) -> Vec<u8> {
    let mut out = Vec::with_capacity(bits.len() + 2);
    let mut state = 0usize;
    for pair in bits.chunks(INPUT_BITS) {
        let input = ((pair[0] as usize) << 1) | pair[1] as usize;
        let output = TRANSITION_MATRIX[state][input];
        out.push((output >> 2) & 0x3);
        out.push(output & 0x3);
        state = input;
    }
    let output = TRANSITION_MATRIX[state][0]; // flush
    out.push((output >> 2) & 0x3);
    out.push(output & 0x3);
    out
}

/// Interleaves 98 deinterleaved data dibits into transmission order (inverse of [`deinterleave_data_symbols`]).
pub fn interleave_data_dibits(deinterleaved: &[u8]) -> Vec<u8> {
    (0..DATA_DEINTERLEAVE.len() / 2).map(|k| deinterleaved[DATA_DEINTERLEAVE[2 * k] / 2]).collect()
}

const TRANSITION_MATRIX: [[u8; 4]; 4] = [[2, 12, 1, 15], [14, 0, 13, 3], [9, 7, 10, 4], [5, 11, 6, 8]];
const INPUT_BITS: usize = 2;
const STATES: usize = 4;

fn angular_distance(observed: f32, expected: f32) -> f64 {
    let mut difference = (observed - expected) as f64; // float subtraction, then widened, as in Java
    while difference > PI {
        difference -= PI * 2.0;
    }
    while difference < -PI {
        difference += PI * 2.0;
    }
    difference.abs()
}

fn branch_metric(previous_state: usize, candidate_input: usize, symbol_index: usize, symbols: &SymbolMessage) -> f64 {
    let expected = TRANSITION_MATRIX[previous_state][candidate_input] as usize;
    let dibit_index = symbol_index * 2;
    let first = Dibit::from_value(((expected >> 2) & 0x3) as u8);
    let second = Dibit::from_value((expected & 0x3) as u8);
    angular_distance(symbols.get(dibit_index), first.ideal_phase())
        + angular_distance(symbols.get(dibit_index + 1), second.ideal_phase())
}

/// Decodes a 98-dibit (49 symbol) block into 96 data bits, MSB first.
pub fn viterbi_half_rate_decode(symbols: &SymbolMessage) -> Vec<bool> {
    let symbol_count = symbols.size() / 2;
    let mut previous_metrics = [f64::INFINITY; STATES];
    previous_metrics[0] = 0.0;
    let mut next_metrics;
    let mut previous_state = vec![[0usize; STATES]; symbol_count];
    let mut input_value = vec![[0usize; STATES]; symbol_count];

    for symbol_index in 0..symbol_count - 1 {
        next_metrics = [f64::INFINITY; STATES];
        for prev in 0..STATES {
            let base = previous_metrics[prev];
            if base.is_infinite() {
                continue;
            }
            for candidate in 0..STATES {
                let metric = base + branch_metric(prev, candidate, symbol_index, symbols);
                if metric < next_metrics[candidate] {
                    next_metrics[candidate] = metric;
                    previous_state[symbol_index][candidate] = prev;
                    input_value[symbol_index][candidate] = candidate;
                }
            }
        }
        previous_metrics = next_metrics;
    }

    // Flush symbol: always ends in state 0.
    let final_index = symbol_count - 1;
    let mut best_metric = f64::INFINITY;
    for prev in 0..STATES {
        let base = previous_metrics[prev];
        if base.is_infinite() {
            continue;
        }
        let metric = base + branch_metric(prev, 0, final_index, symbols);
        if metric < best_metric {
            best_metric = metric;
            previous_state[final_index][0] = prev;
            input_value[final_index][0] = 0;
        }
    }

    let mut decoded = vec![0usize; symbol_count];
    let mut state = 0usize;
    for symbol_index in (0..symbol_count).rev() {
        decoded[symbol_index] = input_value[symbol_index][state];
        state = previous_state[symbol_index][state];
    }

    let mut bits = vec![false; (symbol_count - 1) * INPUT_BITS];
    for symbol_index in 0..symbol_count - 1 {
        for bit in 0..INPUT_BITS {
            let mask = 1 << (INPUT_BITS - bit - 1);
            bits[symbol_index * INPUT_BITS + bit] = decoded[symbol_index] & mask == mask;
        }
    }
    bits
}
