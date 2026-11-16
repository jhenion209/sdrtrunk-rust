//! P25 Phase 1 message framer, ported from sdrtrunk `P25P1MessageFramer` and `P25P1MessageAssembler`.
//!
//! Receives every symbol decision from the demodulator, re-decodes the NID after each sync notification, assembles
//! data units (HDU, LDU1/2, TDU, TDULC, TSBK 1-3) and reports them as [`FramerEvent`]s, together with the sync loss
//! and dropped-symbol accounting. Parsing the unit contents (TSBK opcodes, LDU voice frames, ...) is left to later
//! stages; TSBK blocks are decoded as far as the framer itself needs (CRC, last-block flag).
//!
//! Not ported yet: packet data units (PDU). The framer follows the Java path for a PDU whose header cannot be used:
//! the unit is dropped and the dibit counter adjusted. Multi-block trunking messages sent as PDUs are therefore lost.
use crate::demod::SymbolSink;
use crate::dibit::Dibit;
use crate::nid::{self, DataUnitId, NacTracker, NID_BITS, NO_EXPECTED_NAC, NO_TRACKED_NAC};
use crate::symbols::SymbolMessage;
use crate::tsbk::{self, TsbkBlock};
use sdrtrunk_edac::bch::Bch63;

const DIBIT_LENGTH_NID: usize = 33; // 32 dibits (64 bits) + 1 status
const TSBK_BLOCK_BITS: usize = 196;
const TSBK_BLOCK_DIBITS: usize = TSBK_BLOCK_BITS / 2;
const MIN_VOICE_NID_RECOVERY_CORRECTED_BITS: i32 = 8;
const MINIMUM_LDU_BIT_LENGTH: usize = 1500; // actual length is 1568
const TSBK_BIT_OFFSETS: [usize; 3] = [0, TSBK_BLOCK_BITS, TSBK_BLOCK_BITS * 2];
const TSBK_DUIDS: [DataUnitId; 3] =
    [DataUnitId::TrunkingSignalingBlock1, DataUnitId::TrunkingSignalingBlock2, DataUnitId::TrunkingSignalingBlock3];

/// What the framer reports.
#[derive(Clone, Debug, PartialEq)]
pub enum FramerEvent {
    /// A decoded TSBK block. `valid` is false when the CRC failed.
    Tsbk {
        duid: DataUnitId,
        nac: i32,
        timestamp: i64,
        valid: bool,
        last_block: bool,
        /// Corrected bit count of the block, including the sync bit errors on the first block.
        corrected_bit_count: i32,
        /// Decoded 96 bits (80 data + 16 CRC).
        bits: Vec<bool>,
    },
    /// A raw HDU, LDU1, LDU2, TDU or TDULC (message bits as received after the NID, status symbols removed).
    Unit { duid: DataUnitId, nac: i32, timestamp: i64, sync_bit_errors: i32, bits: Vec<bool> },
    /// Bits skipped over without a sync (signal present but not decoded).
    SyncLoss { timestamp: i64, bits: i32 },
    /// Bits that never arrived (a unit ended early).
    DroppedSamples { timestamp: i64, bits: i32 },
}

pub trait FrameHandler {
    fn handle(&mut self, event: FramerEvent);
}

impl<F: FnMut(FramerEvent)> FrameHandler for F {
    fn handle(&mut self, event: FramerEvent) { self(event) }
}

/// Collects the bits and soft symbols of one data unit (`P25P1MessageAssembler`).
struct Assembler {
    nac: i32,
    duid: DataUnitId,
    bits: Vec<bool>,
    size: usize,
    symbols: SymbolMessage,
}

impl Assembler {
    fn new(nac: i32, duid: DataUnitId) -> Self {
        let length = duid.message_length().max(0) as usize;
        Self { nac, duid, bits: Vec::new(), size: length, symbols: SymbolMessage::new(length / 2) }
    }

    fn set_data_unit_id(&mut self, duid: DataUnitId) {
        let length = duid.message_length().max(0) as usize;
        self.size = length;
        self.symbols.set_size(length / 2);
        self.duid = duid;
    }

    fn is_complete(&self) -> bool { self.bits.len() >= self.size }

    fn receive(&mut self, symbol_phase: f32, dibit: Dibit) {
        if self.is_complete() {
            return; // Java logs a suppressed error
        }
        self.bits.push(dibit.bit1());
        self.bits.push(dibit.bit2());
        self.symbols.add(symbol_phase);
    }

    /// Completes a unit that is cut short by the next sync; returns the number of bits that did not arrive.
    fn force_completion(&mut self, previous: DataUnitId, next: DataUnitId) -> i32 {
        if self.duid == DataUnitId::PlaceHolder {
            let length = self.bits.len();
            if length <= 28 {
                self.duid = DataUnitId::TerminatorDataUnit;
            } else {
                match next {
                    DataUnitId::LogicalLinkDataUnit1 => {
                        if length <= 770 {
                            self.duid = DataUnitId::HeaderDataUnit;
                        } else if length >= MINIMUM_LDU_BIT_LENGTH {
                            self.duid = DataUnitId::LogicalLinkDataUnit2;
                        }
                    }
                    DataUnitId::LogicalLinkDataUnit2 => {
                        if length >= MINIMUM_LDU_BIT_LENGTH {
                            self.duid = DataUnitId::LogicalLinkDataUnit1;
                        }
                    }
                    DataUnitId::TrunkingSignalingBlock1 => {
                        if length >= 195 {
                            let candidate = tsbk::deinterleave_viterbi_and_crc(&self.symbols.sub_message(0, 195));
                            self.duid = if candidate.corrected_bit_count >= 0 && tsbk::is_multi_block_trunking_format(&candidate.bits) {
                                DataUnitId::PacketDataUnit
                            } else {
                                DataUnitId::TrunkingSignalingBlock1
                            };
                        }
                    }
                    _ => {}
                }
            }

            if self.duid == DataUnitId::PlaceHolder {
                match previous {
                    DataUnitId::LogicalLinkDataUnit1 => {
                        if length >= MINIMUM_LDU_BIT_LENGTH {
                            self.duid = DataUnitId::LogicalLinkDataUnit2;
                        }
                    }
                    DataUnitId::LogicalLinkDataUnit2 => {
                        if length >= MINIMUM_LDU_BIT_LENGTH {
                            self.duid = DataUnitId::LogicalLinkDataUnit1;
                        }
                    }
                    _ => {}
                }
            }
        }

        if self.duid == DataUnitId::PlaceHolder {
            self.duid = DataUnitId::TerminatorDataUnit;
        }

        let length = self.duid.message_length().max(0) as usize;
        self.size = length;
        self.symbols.set_size(length / 2);
        self.size as i32 - self.bits.len() as i32
    }
}

pub struct Framer<H: FrameHandler> {
    handler: H,
    bch: Bch63,
    nac_tracker: NacTracker,
    nid_buffer: [Dibit; DIBIT_LENGTH_NID],
    nid_pointer: usize,
    sync_detected: bool,
    running: bool,
    message_assembly_required: bool,
    dibit_counter: i32,
    dibit_since_timestamp_counter: i64,
    status_symbol_dibit_counter: i32,
    reference_timestamp: i64,
    assembler: Option<Assembler>,
    previous_duid: DataUnitId,
    detected_duid: DataUnitId,
    detected_nac: i32,
    expected_nac: i32,
    require_valid_nid: bool,
    detected_sync_bit_errors: i32,
}

impl<H: FrameHandler> Framer<H> {
    pub fn new(handler: H) -> Self {
        Self {
            handler,
            bch: Bch63::p25_nid(),
            nac_tracker: NacTracker::default(),
            nid_buffer: [Dibit::D00Plus1; DIBIT_LENGTH_NID],
            nid_pointer: 0,
            sync_detected: false,
            running: false,
            message_assembly_required: false,
            dibit_counter: 58, // one greater than SYNC+NID so message assembly isn't triggered on startup
            dibit_since_timestamp_counter: 0,
            status_symbol_dibit_counter: 36, // one greater than the suppression trigger at 35 dibits
            reference_timestamp: 0,
            assembler: None,
            previous_duid: DataUnitId::PlaceHolder,
            detected_duid: DataUnitId::PlaceHolder,
            detected_nac: NO_EXPECTED_NAC,
            expected_nac: NO_EXPECTED_NAC,
            require_valid_nid: false,
            detected_sync_bit_errors: 0,
        }
    }

    pub fn handler(&self) -> &H { &self.handler }
    pub fn handler_mut(&mut self) -> &mut H { &mut self.handler }
    pub fn into_handler(self) -> H { self.handler }

    /// Starts dispatching events (the Java framer ignores everything until started).
    pub fn start(&mut self) { self.running = true; }
    pub fn stop(&mut self) { self.running = false; }

    /// Sets the reference time (ms) of the sample buffer about to be processed.
    pub fn set_timestamp(&mut self, time: i64) {
        self.reference_timestamp = time;
        self.dibit_since_timestamp_counter = 0;
    }

    fn has_expected_nac(&self) -> bool { self.expected_nac != NO_EXPECTED_NAC }
    pub fn expected_nac(&self) -> i32 { self.expected_nac }

    pub fn set_expected_nac(&mut self, nac: i32) {
        assert!((0..=0xFFF).contains(&nac), "Expected P25 Phase 1 traffic NAC must be a concrete 12-bit value");
        self.expected_nac = nac;
        self.reset_for_source_frequency_change();
    }

    pub fn set_require_valid_nid(&mut self, require: bool) {
        self.require_valid_nid = require;
        self.reset_for_source_frequency_change();
    }

    pub fn reset_for_source_frequency_change(&mut self) {
        self.nac_tracker.reset();
        self.sync_detected = false;
        self.nid_pointer = 0;
        self.assembler = None;
        self.message_assembly_required = false;
        self.previous_duid = DataUnitId::PlaceHolder;
        self.detected_duid = DataUnitId::PlaceHolder;
        self.detected_nac = NO_EXPECTED_NAC;
        self.detected_sync_bit_errors = 0;
        self.dibit_counter = 58;
        self.status_symbol_dibit_counter = 36;
    }

    /// Externally triggered sync detection (by the demodulator).
    pub fn sync_detected(&mut self) {
        // Only allow sync detection processing if we're not currently assembling a message
        if self.assembler.is_none() {
            self.sync_detected = true;
            self.nid_pointer = 0;
        }
    }

    /// Processes a symbol decision. Returns true when this symbol completed a valid NID.
    pub fn process(&mut self, symbol_phase: f32, symbol: Dibit) -> bool {
        let mut valid_nid_detected = false;
        self.dibit_since_timestamp_counter += 1;

        // Strip a status symbol after every 35 dibits. Reset on sync detect; runs continuously otherwise.
        self.status_symbol_dibit_counter += 1;

        if self.sync_detected {
            if self.nid_pointer < DIBIT_LENGTH_NID {
                self.nid_buffer[self.nid_pointer] = symbol;
                self.nid_pointer += 1;
                if self.nid_pointer >= DIBIT_LENGTH_NID {
                    valid_nid_detected = self.check_nid();
                    self.sync_detected = false;
                }
            } else {
                self.sync_detected = false;
                self.nid_pointer = 0;
            }
        }

        if self.status_symbol_dibit_counter == 36 {
            // The status dibit goes to the channel status processor in Java, which ignores it.
            self.status_symbol_dibit_counter = 0;
            self.dibit_counter += 1;
            return false;
        }

        if self.assembler.is_some() {
            // Delay the completeness check until the next non-status dibit so a trailing status dibit is consumed
            // before the unit is dispatched.
            if self.assembler.as_ref().unwrap().is_complete() {
                self.dispatch_message();
                if let Some(a) = self.assembler.as_mut() {
                    a.receive(symbol_phase, symbol);
                }
            } else {
                self.assembler.as_mut().unwrap().receive(symbol_phase, symbol);
            }
        } else if self.dibit_counter == 57 {
            // Skip 24 sync, 32 NID and 1 status dibit, then start assembling and feed the current dibit.
            if self.message_assembly_required {
                self.assembler = Some(Assembler::new(self.detected_nac, self.detected_duid));
                self.message_assembly_required = false;
            } else if self.detected_nac != NO_EXPECTED_NAC && !self.has_expected_nac() && !self.require_valid_nid {
                // Placeholder assembly; thrown away if it completes before another sync detect.
                self.detected_duid = DataUnitId::PlaceHolder;
                self.assembler = Some(Assembler::new(self.detected_nac, self.detected_duid));
            }
        } else if self.dibit_counter >= 4800 {
            self.dibit_counter -= 4800;
            let timestamp = self.timestamp();
            self.broadcast(FramerEvent::SyncLoss { timestamp, bits: 9600 });
        }

        self.dibit_counter += 1;
        valid_nid_detected
    }

    /// A unit is complete, not yet dispatched, and its trailing status dibit has been consumed.
    fn assembler_complete(&self) -> bool {
        self.assembler.as_ref().map_or(false, |a| a.is_complete()) && self.status_symbol_dibit_counter == 35
    }

    fn assembling(&self) -> bool {
        self.assembler.as_ref().map_or(false, |a| !a.is_complete()) || self.message_assembly_required || self.dibit_counter < 2
    }

    fn broadcast(&mut self, event: FramerEvent) {
        if self.running {
            self.handler.handle(event);
        }
    }

    /// Timestamp (ms) accurate to the current dibit.
    fn timestamp(&mut self) -> i64 {
        if self.reference_timestamp > 0 {
            self.reference_timestamp + (1000.0 * self.dibit_since_timestamp_counter as f64 / 4800.0) as i64
        } else {
            self.dibit_since_timestamp_counter = 0;
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_millis() as i64)
        }
    }

    fn adjust_dibit_counter_from_assembler(&mut self) {
        if let Some(a) = &self.assembler {
            self.dibit_counter -= a.duid.elapsed_dibit_length(); // SYNC + NID + message
        }
    }

    fn dispatch_message(&mut self) {
        let duid = self.assembler.as_ref().unwrap().duid;
        // The assembler has a valid DUID by now (forced completion); remember it as the previous DUID.
        self.previous_duid = duid;
        match duid {
            DataUnitId::TrunkingSignalingBlock1 | DataUnitId::TrunkingSignalingBlock2 | DataUnitId::TrunkingSignalingBlock3 => {
                self.dispatch_tsbk()
            }
            DataUnitId::PacketDataUnit
            | DataUnitId::PacketDataUnitBlock1
            | DataUnitId::PacketDataUnitBlock2
            | DataUnitId::PacketDataUnitBlock3
            | DataUnitId::PacketDataUnitBlock4
            | DataUnitId::PacketDataUnitBlock5 => {
                // PDU support is not ported: drop the unit as the Java does when the header can't be used.
                self.adjust_dibit_counter_from_assembler();
                self.assembler = None;
            }
            DataUnitId::PlaceHolder => self.assembler = None,
            // HDU, LDU1, LDU2, TDU, TDULC (and anything else): report the raw unit. Java adjusts the dibit counter
            // before or after building the message depending on the type; the order has no effect.
            _ => {
                self.adjust_dibit_counter_from_assembler();
                self.dispatch_unit();
                self.assembler = None;
            }
        }
    }

    fn dispatch_unit(&mut self) {
        let (duid, nac, bits) = {
            let a = self.assembler.as_ref().unwrap();
            // The unit is as long as its data unit declares; bits received beyond that (a forced completion that
            // shortened the unit) are not part of the message.
            (a.duid, a.nac, a.bits.iter().copied().take(a.size).collect::<Vec<bool>>())
        };
        let timestamp = self.timestamp();
        let sync_bit_errors = self.detected_sync_bit_errors;
        self.broadcast(FramerEvent::Unit { duid, nac, timestamp, sync_bit_errors, bits });
    }

    fn tsbk_slot(duid: DataUnitId) -> Option<usize> {
        match duid {
            DataUnitId::TrunkingSignalingBlock1 => Some(0),
            DataUnitId::TrunkingSignalingBlock2 => Some(1),
            DataUnitId::TrunkingSignalingBlock3 => Some(2),
            _ => None,
        }
    }

    fn can_dispatch_tsbk_slot(&self, slot: usize) -> bool {
        self.assembler.as_ref().unwrap().symbols.current_size() >= TSBK_BIT_OFFSETS[slot] / 2 + TSBK_BLOCK_DIBITS
    }

    /// Dispatches TSBK blocks 1-3, iterating over the continuation blocks that have already arrived.
    fn dispatch_tsbk(&mut self) {
        let Some(mut slot) = Self::tsbk_slot(self.assembler.as_ref().unwrap().duid) else { return };

        while slot <= 2 && self.assembler.is_some() {
            if !self.can_dispatch_tsbk_slot(slot) {
                // Symbol buffer hasn't caught up: can't decode this slot
                self.adjust_dibit_counter_from_assembler();
                self.assembler = None;
                break;
            }

            let (block, valid, last_block) = self.decode_tsbk_slot(slot);
            self.broadcast_tsbk(slot, block, valid, last_block);

            if slot == 2 || (valid && last_block) {
                // TSBK3 is always last; also terminate when the block signals the end of the sequence
                self.adjust_dibit_counter_from_assembler();
                self.assembler = None;
            } else {
                // Advance to the next slot: either enough symbols already arrived (forced completion) or the
                // assembler is reconfigured to wait for the continuation block.
                slot += 1;
                let next = TSBK_DUIDS[slot];
                let ready = self.can_dispatch_tsbk_slot(slot);
                self.assembler.as_mut().unwrap().set_data_unit_id(next);
                if !ready {
                    break;
                }
            }
        }
    }

    /// Decodes one TSBK block; validity and the last-block flag are captured before the sync errors are added.
    fn decode_tsbk_slot(&mut self, slot: usize) -> (TsbkBlock, bool, bool) {
        let start = TSBK_BIT_OFFSETS[slot];
        let symbols = self.assembler.as_ref().unwrap().symbols.sub_message(start, start + TSBK_BLOCK_BITS);
        let mut block = tsbk::deinterleave_viterbi_and_crc(&symbols);
        let (valid, last_block) = (block.is_valid(), block.is_last_block());
        if slot == 0 {
            block.corrected_bit_count += self.detected_sync_bit_errors;
        }
        (block, valid, last_block)
    }

    fn broadcast_tsbk(&mut self, slot: usize, block: TsbkBlock, valid: bool, last_block: bool) {
        let nac = self.assembler.as_ref().unwrap().nac;
        let timestamp = self.timestamp();
        self.broadcast(FramerEvent::Tsbk {
            duid: TSBK_DUIDS[slot],
            nac,
            timestamp,
            valid,
            last_block,
            corrected_bit_count: block.corrected_bit_count,
            bits: block.bits,
        });
    }

    // ------------------------------------------------------------------ NID

    fn check_nid(&mut self) -> bool {
        let mut nid_bits = [false; NID_BITS];
        let mut bit = 0;
        for i in 0..DIBIT_LENGTH_NID {
            if i != 11 {
                nid_bits[bit] = self.nid_buffer[i].bit1();
                nid_bits[bit + 1] = self.nid_buffer[i].bit2();
                bit += 2;
            }
        }
        self.decode_nid(&mut nid_bits)
    }

    /// Decodes and validates a captured NID; on success reports it through [`Framer::nid_detected`].
    pub fn decode_nid(&mut self, nid_bits: &mut [bool; NID_BITS]) -> bool {
        let strict = self.has_expected_nac() || self.require_valid_nid;
        let tracked_nac = if strict { NO_TRACKED_NAC } else { self.nac_tracker.tracked_nac() };
        let observed = if strict || tracked_nac < 0 { None } else { Some(tracked_nac) };
        let corrected = nid::decode_nid(&self.bch, nid_bits, observed);

        let mut nac = nid::nid_nac(nid_bits) as i32;
        let duid = DataUnitId::from_value(nid::nid_duid(nid_bits));

        // If error correction fails, give up on this NID
        if corrected < 0 {
            return false;
        }

        let mut corrected_bit_count;
        if self.has_expected_nac() {
            corrected_bit_count = nid::validate(nid_bits, corrected);
            if corrected_bit_count >= 0 && nac != self.expected_nac {
                if self.is_recoverable_voice_nid(duid, corrected_bit_count) {
                    // The traffic channel's preloaded NAC stays authoritative: recover only the unit type.
                    nac = self.expected_nac;
                } else {
                    corrected_bit_count = -1;
                }
            }
        } else {
            corrected_bit_count = nid::validate(nid_bits, corrected);
        }

        if corrected_bit_count < 0 {
            return false;
        }
        if self.require_valid_nid && duid == DataUnitId::Unknown {
            return false;
        }
        if !self.has_expected_nac() {
            self.nac_tracker.track(nac);
        }

        self.nid_detected(nac, duid, corrected_bit_count);
        true
    }

    /// A BCH-valid but NAC-mismatched NID that is badly damaged is treated as an over-correction for voice continuity
    /// (LDU1/LDU2 alternate); control, packet, header and terminator units stay fail-closed.
    fn is_recoverable_voice_nid(&self, duid: DataUnitId, corrected_bit_count: i32) -> bool {
        let alternating = (self.previous_duid == DataUnitId::LogicalLinkDataUnit1 && duid == DataUnitId::LogicalLinkDataUnit2)
            || (self.previous_duid == DataUnitId::LogicalLinkDataUnit2 && duid == DataUnitId::LogicalLinkDataUnit1);
        corrected_bit_count >= MIN_VOICE_NID_RECOVERY_CORRECTED_BITS && alternating
    }

    /// A sync with a valid NID was found; the next arriving dibit is the first symbol after the NID.
    pub fn nid_detected(&mut self, nac: i32, duid: DataUnitId, detected_bit_errors: i32) {
        self.detected_duid = duid;

        // An UNKNOWN DUID becomes the placeholder.
        if self.detected_duid == DataUnitId::Unknown {
            self.detected_duid = DataUnitId::PlaceHolder;
        }

        if self.detected_duid != DataUnitId::PlaceHolder || (self.has_expected_nac() && nac == self.expected_nac) {
            self.detected_nac = nac;
        }

        self.detected_sync_bit_errors = detected_bit_errors;

        // If a message assembler is still active, force it to complete
        if self.assembler.is_some() {
            if self.assembler.as_ref().unwrap().is_complete() {
                // A message that completed as a placeholder means no subsequent sync was found: throw it away
                if self.assembler.as_ref().unwrap().duid != DataUnitId::PlaceHolder {
                    self.dispatch_message();
                }
            } else {
                let (previous, detected) = (self.previous_duid, self.detected_duid);
                let dropped = self.assembler.as_mut().unwrap().force_completion(previous, detected);
                self.dispatch_dropped_samples(dropped);
                self.dispatch_message();
            }
        }

        self.dibit_counter -= 57;

        if self.dibit_counter > 0 {
            let bits = self.dibit_counter * 2;
            self.dispatch_sync_loss(bits);
        } else if self.dibit_counter < 0 {
            let bits = -self.dibit_counter * 2;
            self.dispatch_dropped_samples(bits);
        }

        // Start a message assembler once the SYNC and NID dibits are skipped at dibit count 57
        self.message_assembly_required = true;
        self.dibit_counter = 57;
        self.status_symbol_dibit_counter = 21;
    }

    fn dispatch_sync_loss(&mut self, bits: i32) {
        if bits > 0 {
            let timestamp = self.timestamp();
            self.broadcast(FramerEvent::SyncLoss { timestamp, bits });
        }
    }

    fn dispatch_dropped_samples(&mut self, bits: i32) {
        if bits > 0 {
            let timestamp = self.timestamp();
            self.broadcast(FramerEvent::DroppedSamples { timestamp, bits });
        }
    }
}

impl<H: FrameHandler> SymbolSink for Framer<H> {
    fn process(&mut self, soft_symbol: f32, symbol: Dibit) { Framer::process(self, soft_symbol, symbol); }
    fn sync_detected(&mut self) { Framer::sync_detected(self) }
    fn is_complete(&self) -> bool { self.assembler_complete() }
    fn is_assembling(&self) -> bool { self.assembling() }
}
