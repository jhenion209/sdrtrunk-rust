//! Wideband P25 receiver: one channelizer feeds the control channel and any number of voice channels.
//! Per channel: channel extractor (one or two bins) -> C4FM front end -> demodulator -> framer.
//! Control channel: TSBK parse -> `TrunkTracker` -> site and grant events.
//! Voice channels: LDU1/LDU2 -> 9 IMBE voice frames each; TDU/TDULC -> end of call.
use crate::demod::{Demodulator, SymbolSink};
use crate::dibit::Dibit;
use crate::framer::{FrameHandler, Framer, FramerEvent};
use crate::nid::DataUnitId;
use crate::trunk::{CallGrant, TrunkTracker};
use crate::tsbk_msg::{parse, Tsbk};
use std::cell::RefCell;
use std::rc::Rc;
use sdrtrunk_dsp::channel_calculator::ChannelCalculator;
use sdrtrunk_dsp::channelizer::{sinc_m2_synthesizer_taps, ChannelExtractor, Channelizer, TwoChannelExtractor};
use crate::demod_lsm::LsmDemodulator;
use crate::ess::{self, EncryptionSync, LinkControl};
use sdrtrunk_dsp::front_end::{C4fmFrontEnd, LsmFrontEnd};

/// One raw IMBE voice frame as carried in an LDU (144 bits, still interleaved and error-coded).
pub type ImbeFrame = [bool; 144];

/// Bit offsets of the 9 IMBE frames inside LDU1/LDU2 message bits (sdrtrunk `LDUMessage.IMBE_FRAME_1..9`).
pub const IMBE_OFFSETS: [usize; 9] = [0, 144, 328, 512, 696, 880, 1064, 1248, 1424];

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ControlEvent {
    Site { nac: i32, wacn: u32, system: u16 },
    Call(CallGrant),
    /// Nine voice frames (one LDU, 180 ms) from voice channel `id`. `ess` is the encryption sync word of an LDU2
    /// when it decoded (None for LDU1 or a failed decode); it tells clear from encrypted voice. `lc` is the link
    /// control word of an LDU1 when it decoded (talkgroup, source radio, service options).
    Voice { id: u32, frames: Vec<ImbeFrame>, ess: Option<EncryptionSync>, lc: Option<LinkControl> },
    /// Terminator seen on voice channel `id`.
    VoiceEnd { id: u32 },
}

/// The 9 IMBE frames of an LDU, or None when the unit is too short.
pub fn ldu_frames(bits: &[bool]) -> Option<Vec<ImbeFrame>> {
    if bits.len() < IMBE_OFFSETS[8] + 144 { return None; }
    Some(IMBE_OFFSETS.iter().map(|&o| bits[o..o + 144].try_into().unwrap()).collect())
}

/// Control channel frame counts since the channel started: valid and failed TSBK blocks, and bits lost to sync loss
/// or dropped samples (the Java reference's decode quality: valid / (valid + invalid + lost_bits / 196)).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct FrameCounts { pub valid: u64, pub invalid: u64, pub lost_bits: u64 }

/// Bits in a P25 Phase 1 frame, for counting lost bits as frames (Java reference `P25_PHASE_1_FRAME_BITS`).
pub const FRAME_BITS: f64 = 196.0;

/// What the control channel's displays need, collected only while watched: soft symbols (radians) for the symbol
/// plot, and the channel's I/Q (interleaved, full scale 1, at the channel rate) for its FFT.
#[derive(Default)]
struct Capture { symbols_on: bool, soft: Vec<f32>, iq_on: bool, iq: Vec<f32> }

const SYMBOL_RATE_HZ: f64 = 4800.0;

/// At most one second of symbols waits to be taken.
const SYMBOL_CAPTURE_MAX: usize = SYMBOL_RATE_HZ as usize;
/// At most 16384 I/Q samples (about 0.3 s) wait to be taken.
const IQ_CAPTURE_MAX: usize = 2 * 16_384;

/// State shared between the framer callbacks and the decoder.
#[derive(Default)]
struct Shared {
    valid: u64,
    invalid: u64,
    lost_bits: u64,
    block_start: u64,
    last_valid_sample: Option<u64>,
    site: Option<(i32, u32, u16)>,
    tracker: TrunkTracker,
    /// Shared with the voice bank in a `ControlChannel`, so control and voice events keep arrival order.
    events: VoiceEvents,
}

struct ControlHandler(Rc<RefCell<Shared>>);

impl FrameHandler for ControlHandler {
    fn handle(&mut self, e: FramerEvent) {
        let mut s = self.0.borrow_mut();
        let (bits, timestamp, nac) = match e {
            FramerEvent::Tsbk { valid: true, bits, timestamp, nac, .. } => (bits, timestamp, nac),
            FramerEvent::Tsbk { valid: false, .. } => return s.invalid += 1,
            FramerEvent::SyncLoss { bits, .. } | FramerEvent::DroppedSamples { bits, .. } => return s.lost_bits += bits.max(0) as u64,
            FramerEvent::Unit { .. } => return,
        };
        s.valid += 1;
        s.last_valid_sample = Some(s.block_start);
        let msg = parse(&bits);
        if let Tsbk::NetworkStatus { wacn, system, .. } = msg {
            if s.site != Some((nac, wacn, system)) {
                s.site = Some((nac, wacn, system));
                s.events.borrow_mut().push(ControlEvent::Site { nac, wacn, system });
            }
        }
        let grants = s.tracker.handle(timestamp, &msg);
        s.events.borrow_mut().extend(grants.into_iter().map(ControlEvent::Call));
    }
}

struct VoiceHandler { id: u32, events: VoiceEvents }

impl FrameHandler for VoiceHandler {
    fn handle(&mut self, e: FramerEvent) {
        let FramerEvent::Unit { duid, bits, .. } = e else { return };
        let event = match duid {
            DataUnitId::LogicalLinkDataUnit1 | DataUnitId::LogicalLinkDataUnit2 => match ldu_frames(&bits) {
                Some(frames) => {
                    let ldu2 = duid == DataUnitId::LogicalLinkDataUnit2;
                    let ess = ldu2.then(|| ess::decode(&bits)).flatten();
                    let lc = (!ldu2).then(|| ess::link_control(&bits)).flatten();
                    ControlEvent::Voice { id: self.id, frames, ess, lc }
                }
                None => return,
            },
            DataUnitId::TerminatorDataUnit | DataUnitId::TerminatorDataUnitLinkControl => ControlEvent::VoiceEnd { id: self.id },
            _ => return,
        };
        self.events.borrow_mut().push(event);
    }
}

/// P25 Phase 1 modulation. Simulcast sites transmit LSM (sdrtrunk "CQPSK"); others C4FM.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Modulation {
    #[default]
    C4fm,
    Lsm,
}

impl Modulation {
    pub fn as_str(self) -> &'static str { match self { Modulation::C4fm => "c4fm", Modulation::Lsm => "lsm" } }

    /// Accepts "c4fm", "lsm" and sdrtrunk's "cqpsk" (any case).
    pub fn parse(s: &str) -> Option<Modulation> {
        match s.to_ascii_lowercase().as_str() {
            "c4fm" => Some(Modulation::C4fm),
            "lsm" | "cqpsk" => Some(Modulation::Lsm),
            _ => None,
        }
    }
}

enum Extractor { One(ChannelExtractor), Two(TwoChannelExtractor) }

/// The framer, plus a copy of each soft symbol for the symbol display while it is watched.
struct Tee<H: FrameHandler> { framer: Framer<H>, capture: Option<Rc<RefCell<Capture>>> }

impl<H: FrameHandler> SymbolSink for Tee<H> {
    fn process(&mut self, soft_symbol: f32, symbol: Dibit) {
        if let Some(c) = &self.capture {
            let mut c = c.borrow_mut();
            if c.symbols_on && c.soft.len() < SYMBOL_CAPTURE_MAX { c.soft.push(soft_symbol); }
        }
        SymbolSink::process(&mut self.framer, soft_symbol, symbol);
    }
    fn sync_detected(&mut self) { SymbolSink::sync_detected(&mut self.framer) }
    fn is_complete(&self) -> bool { SymbolSink::is_complete(&self.framer) }
    fn is_assembling(&self) -> bool { SymbolSink::is_assembling(&self.framer) }
}

enum Demod<H: FrameHandler> {
    C4fm(C4fmFrontEnd, Demodulator<Tee<H>>),
    Lsm(LsmFrontEnd, LsmDemodulator<Tee<H>>),
}

/// One P25 channel taken from the shared channelizer output. `power` sums |sample|^2 of the channel (full scale = 1)
/// over `power_n` samples since last taken. `pll_seconds`: the time the carrier PLL's phase (radians) is measured over.
struct Tap<H: FrameHandler> {
    ex: Extractor, demod: Demod<H>, freq_hz: i64, indexes: Vec<usize>, power: f64, power_n: u64, pll_seconds: f64,
    capture: Option<Rc<RefCell<Capture>>>,
}

impl<H: FrameHandler> Tap<H> {
    /// `decode_hz`: where the channel at `freq_hz` is decoded (after tuner and channel correction).
    #[allow(clippy::too_many_arguments)]
    fn new(calc: &ChannelCalculator, gain: f32, freq_hz: i64, decode_hz: i64, start_ms: i64, modulation: Modulation, handler: H,
           capture: Option<Rc<RefCell<Capture>>>) -> Result<Self, String> {
        let channel_rate = calc.channel_sample_rate();
        let indexes = calc.channel_indexes(decode_hz, 12_500)
            .map_err(|e| format!("{freq_hz} Hz is not inside the tuned band: {e}"))?;
        let offset = calc.frequency_offset(&indexes, decode_hz, 0);
        let ex = if indexes.len() == 1 {
            let mut e = ChannelExtractor::new(channel_rate, indexes[0], gain);
            e.set_frequency_offset(offset);
            Extractor::One(e)
        } else {
            let syn = sinc_m2_synthesizer_taps(channel_rate, calc.channel_bandwidth(), 2, 9)
                .map_err(|e| format!("synthesis filter: {e}"))?;
            let mut e = TwoChannelExtractor::new(channel_rate, [indexes[0], indexes[1]], &syn, gain);
            e.set_frequency_offset(offset);
            Extractor::Two(e)
        };
        let mut framer = Framer::new(handler);
        framer.set_timestamp(start_ms);
        framer.start();
        let framer = Tee { framer, capture: capture.clone() };
        let (demod, pll_seconds) = match modulation {
            Modulation::C4fm => {
                let front = C4fmFrontEnd::new(channel_rate).map_err(|e| format!("front end: {e}"))?;
                let (sps, seconds) = (front.samples_per_symbol(), front.differential_seconds());
                (Demod::C4fm(front, Demodulator::new(framer, sps)), seconds)
            }
            Modulation::Lsm => {
                let front = LsmFrontEnd::new(channel_rate).map_err(|e| format!("front end: {e}"))?;
                let sps = front.samples_per_symbol();
                (Demod::Lsm(front, LsmDemodulator::new(framer, sps)), 1.0 / SYMBOL_RATE_HZ)
            }
        };
        Ok(Tap { ex, demod, freq_hz, indexes, power: 0.0, power_n: 0, pll_seconds, capture })
    }

    /// Moves where the channel is decoded (same channelizer bins; corrections are a few kHz at most).
    fn retune(&mut self, calc: &ChannelCalculator, decode_hz: i64) {
        let offset = calc.frequency_offset(&self.indexes, decode_hz, 0);
        match &mut self.ex { Extractor::One(e) => e.set_frequency_offset(offset), Extractor::Two(e) => e.set_frequency_offset(offset) }
    }

    /// Hz the signal sits above where it is decoded, from the carrier PLL (opposite sign: it cancels the offset).
    fn pll_error_hz(&self) -> f64 {
        let pll = match &self.demod { Demod::C4fm(_, d) => d.equalizer().0, Demod::Lsm(_, d) => d.pll() };
        -(pll as f64) / (std::f64::consts::TAU * self.pll_seconds)
    }

    fn push(&mut self, r: &[f32]) {
        let (demod, power, power_n, capture) = (&mut self.demod, &mut self.power, &mut self.power_n, &self.capture);
        let mut feed = |i: &[f32], q: &[f32]| {
            *power += i.iter().zip(q).map(|(a, b)| (a * a + b * b) as f64).sum::<f64>();
            *power_n += i.len() as u64;
            if let Some(c) = capture {
                let mut c = c.borrow_mut();
                if c.iq_on {
                    let room = (IQ_CAPTURE_MAX - c.iq.len()) / 2;
                    c.iq.extend(i.iter().zip(q).take(room).flat_map(|(&a, &b)| [a, b]));
                }
            }
            match demod {
                Demod::C4fm(front, d) => { let x = front.process(i, q); d.process(&x); }
                Demod::Lsm(front, d) => { let (fi, fq) = front.process(i, q); d.process(&fi, &fq); }
            }
        };
        match &mut self.ex { Extractor::One(e) => e.push(r, &mut feed), Extractor::Two(e) => e.push(r, &mut feed) }
    }
}

/// Voice events of one tuner's voice channels, taken after each block.
type VoiceEvents = Rc<RefCell<Vec<ControlEvent>>>;

/// The voice channels decoded from one tuner: a channelizer, a channel calculator and one tap per voice channel.
/// Ids run `id_base, id_base + 1, ...`, so banks on different tuners can share one id space.
pub struct VoiceBank {
    chan: Channelizer,
    calc: ChannelCalculator,
    rate_hz: f64,
    ppm: f64,
    /// Channel correction (Hz added where every channel is decoded), see `decode_hz`.
    channel_hz: f64,
    start_ms: i64,
    modulation: Modulation,
    voices: Vec<(u32, Tap<VoiceHandler>)>,
    next_id: u32,
    events: VoiceEvents,
    samples: u64,
}

impl VoiceBank {
    /// center/rate: tuner; ppm: tuner error. Voice channels use `modulation`.
    pub fn new(center_hz: f64, rate_hz: f64, ppm: f64, modulation: Modulation, start_ms: i64, id_base: u32) -> Result<Self, String> {
        Self::with_events(center_hz, rate_hz, ppm, modulation, start_ms, id_base, Rc::default())
    }

    /// As `new`, queueing voice events on `events` (a `ControlChannel` shares its queue with the control handler).
    fn with_events(center_hz: f64, rate_hz: f64, ppm: f64, modulation: Modulation, start_ms: i64, id_base: u32, events: VoiceEvents)
        -> Result<Self, String> {
        let chan = Channelizer::for_sample_rate(rate_hz).map_err(|e| format!("unsupported sample rate: {e}"))?;
        let mut calc = ChannelCalculator::new(rate_hz, chan.channels(), center_hz, 2.0);
        calc.set_center_frequency(center_hz);
        Ok(VoiceBank {
            chan, calc, rate_hz, ppm, channel_hz: 0.0, start_ms, modulation, voices: Vec::new(), next_id: id_base,
            events, samples: 0,
        })
    }

    /// Starts decoding a voice channel from the same tuner input. Events for it carry the returned id.
    pub fn add_voice(&mut self, freq_hz: i64) -> Result<u32, String> {
        let id = self.next_id;
        let handler = VoiceHandler { id, events: self.events.clone() };
        let at = decode_hz(freq_hz, self.ppm, self.channel_hz);
        let tap = Tap::new(&self.calc, self.chan.channels() as f32, freq_hz, at, self.start_ms, self.modulation, handler, None)?;
        self.next_id += 1;
        self.voices.push((id, tap));
        Ok(id)
    }

    pub fn remove_voice(&mut self, id: u32) { self.voices.retain(|(v, _)| *v != id); }

    /// Frequencies of the voice channels being decoded.
    pub fn voice_freqs(&self) -> Vec<i64> { self.voices.iter().map(|(_, v)| v.freq_hz).collect() }

    pub fn voice_count(&self) -> usize { self.voices.len() }

    /// Ids of the voice channels being decoded.
    pub fn voice_ids(&self) -> Vec<u32> { self.voices.iter().map(|(id, _)| *id).collect() }

    /// The tuner moved to `center_hz`: every voice channel is set up again at its new place in the band (frames in
    /// progress are lost). On error (a channel would fall outside) nothing changes.
    pub fn set_center(&mut self, center_hz: f64) -> Result<(), String> {
        let at_ms = self.start_ms + (self.samples as f64 * 1000.0 / self.rate_hz) as i64;
        let (calc, voices) = self.moved(center_hz, at_ms)?;
        (self.calc, self.voices) = (calc, voices);
        Ok(())
    }

    /// The calculator and voice taps for a tuner at `center_hz`, built from `at_ms`; the bank is not changed.
    fn moved(&self, center_hz: f64, at_ms: i64) -> Result<(ChannelCalculator, Vec<(u32, Tap<VoiceHandler>)>), String> {
        let mut calc = self.calc.clone();
        calc.set_center_frequency(center_hz);
        let gain = self.chan.channels() as f32;
        let voices = self.voices.iter().map(|(id, v)| {
            let handler = VoiceHandler { id: *id, events: self.events.clone() };
            let at = decode_hz(v.freq_hz, self.ppm, self.channel_hz);
            Tap::new(&calc, gain, v.freq_hz, at, at_ms, self.modulation, handler, None).map(|t| (*id, t))
        }).collect::<Result<Vec<_>, String>>()?;
        Ok((calc, voices))
    }

    /// New tuner PPM and channel correction (Hz) for every channel, from automatic correction.
    pub fn set_tuning(&mut self, ppm: f64, channel_hz: f64) {
        (self.ppm, self.channel_hz) = (ppm, channel_hz);
        let calc = &self.calc;
        for (_, v) in &mut self.voices { v.retune(calc, decode_hz(v.freq_hz, ppm, channel_hz)); }
    }

    /// Feeds one block of channelizer output to every voice tap.
    fn push_channelized(voices: &mut [(u32, Tap<VoiceHandler>)], r: &[f32]) {
        for (_, v) in voices.iter_mut() { v.push(r); }
    }

    /// Voice events collected since last taken.
    fn take_events(&mut self) -> Vec<ControlEvent> { std::mem::take(&mut self.events.borrow_mut()) }

    /// Interleaved int16 IQ, scaled /2048 inside. Voice and VoiceEnd events go to `sink`.
    pub fn process(&mut self, iq: &[i16], sink: &mut dyn FnMut(ControlEvent)) {
        let samples: Vec<f32> = iq.iter().map(|&s| s as f32 / 2048.0).collect();
        let (chan, voices) = (&mut self.chan, &mut self.voices);
        chan.receive(&samples, |r| Self::push_channelized(voices, r));
        self.samples += (iq.len() / 2) as u64;
        for e in self.take_events() { sink(e); }
    }
}

/// One tuner's control channel plus its voice channels (the bank); one channelizer feeds both.
pub struct ControlChannel {
    voices: VoiceBank,
    control: Tap<ControlHandler>,
    shared: Rc<RefCell<Shared>>,
    capture: Rc<RefCell<Capture>>,
    measured_hz: Option<f64>,
    samples: u64,
}

/// Where the channel at `freq_hz` is decoded, given the tuner error (PPM) and channel correction (Hz).
pub fn decode_hz(freq_hz: i64, ppm: f64, channel_hz: f64) -> i64 {
    freq_hz - (ppm * freq_hz as f64 / 1e6).round() as i64 + channel_hz.round() as i64
}

impl ControlChannel {
    /// center/rate: tuner; freq: control channel; ppm: tuner error (decode at freq - ppm*freq/1e6). Voice channels
    /// use the same modulation as the control channel.
    pub fn new(center_hz: f64, rate_hz: f64, freq_hz: i64, ppm: f64, start_ms: i64, modulation: Modulation)
        -> Result<Self, String> {
        let events = VoiceEvents::default();
        let voices = VoiceBank::with_events(center_hz, rate_hz, ppm, modulation, start_ms, 1, events.clone())?;
        let shared = Rc::new(RefCell::new(Shared { events, ..Shared::default() }));
        let capture = Rc::new(RefCell::new(Capture::default()));
        let handler = ControlHandler(shared.clone());
        let control = Tap::new(&voices.calc, voices.chan.channels() as f32, freq_hz, decode_hz(freq_hz, ppm, 0.0), start_ms, modulation,
            handler, Some(capture.clone()))?;
        Ok(ControlChannel { voices, control, shared, capture, measured_hz: None, samples: 0 })
    }

    /// Starts decoding a voice channel from the same tuner input. Events for it carry the returned id.
    pub fn add_voice(&mut self, freq_hz: i64) -> Result<u32, String> { self.voices.add_voice(freq_hz) }

    /// The tuner moved to `center_hz`: every channel is set up again at its new place in the band (frames in progress
    /// are lost; counts, site and grant tracking stay). On error (a channel would fall outside) nothing changes.
    pub fn set_center(&mut self, center_hz: f64) -> Result<(), String> {
        let b = &self.voices;
        let at_ms = b.start_ms + (self.samples as f64 * 1000.0 / b.rate_hz) as i64;
        let (calc, voices) = b.moved(center_hz, at_ms)?;
        let f = self.control.freq_hz;
        let control = Tap::new(&calc, b.chan.channels() as f32, f, decode_hz(f, b.ppm, b.channel_hz), at_ms, b.modulation,
            ControlHandler(self.shared.clone()), Some(self.capture.clone()))?;
        (self.voices.calc, self.voices.voices, self.control) = (calc, voices, control);
        Ok(())
    }

    /// Frequencies of the voice channels being decoded.
    pub fn voice_freqs(&self) -> Vec<i64> { self.voices.voice_freqs() }

    /// New tuner PPM and channel correction (Hz) for every channel, from automatic correction.
    pub fn set_tuning(&mut self, ppm: f64, channel_hz: f64) {
        self.voices.set_tuning(ppm, channel_hz);
        self.control.retune(&self.voices.calc, decode_hz(self.control.freq_hz, ppm, channel_hz));
    }

    /// The carrier PLL's frequency error (Hz the signal sits above where it is decoded), read after a block in which
    /// control frames decoded; None otherwise. Taken once.
    pub fn take_frequency_error(&mut self) -> Option<f64> { self.measured_hz.take() }

    /// Average control channel power (dBFS) since last taken; None before any samples.
    pub fn take_signal_dbfs(&mut self) -> Option<f64> {
        let c = &mut self.control;
        let db = (c.power_n > 0).then(|| 10.0 * (c.power / c.power_n as f64).max(1e-20).log10());
        (c.power, c.power_n) = (0.0, 0);
        db
    }

    pub fn frames(&self) -> FrameCounts {
        let s = self.shared.borrow();
        FrameCounts { valid: s.valid, invalid: s.invalid, lost_bits: s.lost_bits }
    }

    /// Collects control channel soft symbols (radians) while on; off clears them.
    pub fn capture_symbols(&mut self, on: bool) {
        let mut c = self.capture.borrow_mut();
        c.symbols_on = on;
        if !on { c.soft.clear(); }
    }

    pub fn take_symbols(&mut self) -> Vec<f32> { std::mem::take(&mut self.capture.borrow_mut().soft) }

    /// Collects the control channel's I/Q (interleaved, full scale 1) while on; off clears it.
    pub fn capture_iq(&mut self, on: bool) {
        let mut c = self.capture.borrow_mut();
        c.iq_on = on;
        if !on { c.iq.clear(); }
    }

    pub fn take_iq(&mut self) -> Vec<f32> { std::mem::take(&mut self.capture.borrow_mut().iq) }

    /// Sample rate of the channel I/Q, and the frequency at its centre (where the control channel is decoded).
    pub fn channel_rate_hz(&self) -> f64 { self.voices.calc.channel_sample_rate() }
    pub fn control_decode_hz(&self) -> i64 { decode_hz(self.control.freq_hz, self.voices.ppm, self.voices.channel_hz) }

    pub fn remove_voice(&mut self, id: u32) { self.voices.remove_voice(id); }

    pub fn voice_count(&self) -> usize { self.voices.voice_count() }
    pub fn voice_ids(&self) -> Vec<u32> { self.voices.voice_ids() }

    /// Interleaved int16 IQ, scaled /2048 inside. Events go to `sink`.
    pub fn process(&mut self, iq: &[i16], sink: &mut dyn FnMut(ControlEvent)) {
        self.shared.borrow_mut().block_start = self.samples;
        let valid_before = self.shared.borrow().valid;
        let samples: Vec<f32> = iq.iter().map(|&s| s as f32 / 2048.0).collect();
        let (control, bank) = (&mut self.control, &mut self.voices);
        bank.chan.receive(&samples, |r| {
            control.push(r);
            VoiceBank::push_channelized(&mut bank.voices, r);
        });
        self.samples += (iq.len() / 2) as u64;
        self.voices.samples += (iq.len() / 2) as u64;
        if self.shared.borrow().valid > valid_before { self.measured_hz = Some(self.control.pll_error_hz()); }
        for e in self.voices.take_events() { sink(e); }
    }

    pub fn valid_tsbk(&self) -> u64 { self.shared.borrow().valid }
    pub fn samples(&self) -> u64 { self.samples }
    pub fn last_valid_sample(&self) -> Option<u64> { self.shared.borrow().last_valid_sample }
}

/// Tuner centre used for a control channel: two bins below it, clear of DC.
pub fn center_for(freq_hz: i64) -> f64 { (freq_hz - 50_000) as f64 }

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zeros_decode_nothing() {
        let mut c = ControlChannel::new(858_912_500.0, 6e6, 858_962_500, 0.0, 0, Modulation::C4fm).unwrap();
        let zeros = vec![0i16; 2 * 150_000];
        let mut events = Vec::new();
        for _ in 0..4 { c.process(&zeros, &mut |e| events.push(e)); }
        assert_eq!(c.valid_tsbk(), 0);
        assert_eq!(c.samples(), 600_000);
        assert_eq!(c.last_valid_sample(), None);
        assert!(events.is_empty());
        assert_eq!(center_for(858_962_500), 858_912_500.0);
    }

    #[test]
    fn new_rejects_out_of_band() {
        assert!(ControlChannel::new(858_912_500.0, 6e6, 900_000_000, 0.0, 0, Modulation::Lsm).is_err());
    }

    #[test]
    fn voice_channels_added_and_removed() {
        let mut c = ControlChannel::new(858_912_500.0, 6e6, 858_962_500, 0.0, 0, Modulation::C4fm).unwrap();
        let a = c.add_voice(859_737_500).unwrap();
        let b = c.add_voice(859_962_500).unwrap();
        assert_ne!(a, b);
        assert!(c.add_voice(870_000_000).is_err(), "outside the tuned 6 MHz");
        c.process(&vec![0i16; 2 * 100_000], &mut |_| panic!("no events from silence"));
        c.remove_voice(a);
        assert_eq!(c.voice_count(), 1);
    }

    #[test]
    fn voice_bank_alone() {
        let base = 1u32 << 24;
        let mut b = VoiceBank::new(853_000_000.0, 2.4e6, 0.0, Modulation::C4fm, 0, base).unwrap();
        assert_eq!(b.add_voice(853_400_000).unwrap(), base);
        assert_eq!(b.add_voice(853_900_000).unwrap(), base + 1);
        assert!(b.add_voice(856_000_000).is_err());
        b.set_center(853_500_000.0).unwrap();
        assert_eq!(b.voice_freqs(), vec![853_400_000, 853_900_000]);
        b.process(&vec![0i16; 2 * 100_000], &mut |_| panic!("no events from silence"));
        b.remove_voice(base);
        b.remove_voice(base + 1);
        assert_eq!(b.voice_count(), 0);
    }

    #[test]
    fn center_moves_with_channels() {
        // RTL-SDR at 2.4 MS/s, 50 kHz below the control channel: 853.9 MHz is out of reach until the tuner moves up.
        let control = 852_112_500;
        let mut c = ControlChannel::new(center_for(control) as f64, 2.4e6, control, 0.0, 0, Modulation::C4fm).unwrap();
        assert!(c.add_voice(853_900_000).is_err());
        let a = c.add_voice(852_600_000).unwrap();
        c.set_center(853_000_000.0).unwrap();
        let b = c.add_voice(853_900_000).unwrap();
        assert_eq!(c.voice_freqs(), vec![852_600_000, 853_900_000], "the voice channel kept its id and frequency");
        assert!(c.set_center(854_500_000.0).is_err(), "the control channel would fall outside");
        assert_eq!(c.voice_freqs().len(), 2, "a failed move changes nothing");
        c.process(&vec![0i16; 2 * 100_000], &mut |_| panic!("no events from silence"));
        c.remove_voice(a);
        c.remove_voice(b);
        assert_eq!(c.voice_count(), 0);
    }

    /// A C4FM signal carrying the test control stream (7 TSBK frames, 2 with a failed block) `repeats` times: each
    /// dibit is a frequency step of its ideal phase per symbol (+/-600, +/-1800 Hz), at `at_hz` from the tuner
    /// centre, `amp` of full scale, as interleaved int16 at 2.4 MS/s.
    fn c4fm(at_hz: f64, amp: f64, repeats: usize) -> Vec<i16> {
        let stream = include_str!("../tests/fixtures/p25_streams.txt").lines()
            .find_map(|l| l.strip_prefix("STREAM|control|")).unwrap();
        let (rate, sps) = (2.4e6, 500);
        let mut phase = 0f64;
        let mut iq = Vec::new();
        for _ in 0..repeats {
            for c in stream.bytes() {
                let dev = Dibit::from_value(c - b'0').ideal_phase() as f64 / std::f64::consts::TAU * SYMBOL_RATE_HZ;
                for _ in 0..sps {
                    phase = (phase + std::f64::consts::TAU * (at_hz + dev) / rate) % std::f64::consts::TAU;
                    iq.extend([(amp * 2048.0 * phase.cos()) as i16, (amp * 2048.0 * phase.sin()) as i16]);
                }
            }
        }
        iq
    }

    fn run(c: &mut ControlChannel, iq: &[i16]) -> Option<f64> {
        let mut error = None;
        for block in iq.chunks(2 * 65_536) {
            c.process(block, &mut |_| {});
            error = c.take_frequency_error().or(error);
        }
        error
    }

    #[test]
    fn signal_quality_measured() {
        // Control channel at 852.1125 MHz; the signal is 250 Hz high, at a quarter of full scale (-12 dBFS).
        let (freq, center) = (852_112_500, center_for(852_112_500));
        let iq = c4fm(freq as f64 + 250.0 - center, 0.25, 3);
        let mut c = ControlChannel::new(center, 2.4e6, freq, 0.0, 0, Modulation::C4fm).unwrap();
        c.capture_symbols(true);
        c.capture_iq(true);
        let error = run(&mut c, &iq).expect("frames decoded");
        let ch = c.take_iq();
        assert_eq!(ch.len(), IQ_CAPTURE_MAX, "channel I/Q capped");
        let p = ch.chunks(2).map(|s| (s[0] * s[0] + s[1] * s[1]) as f64).sum::<f64>() / (ch.len() / 2) as f64;
        assert!((10.0 * p.log10() + 12.04).abs() < 1.5, "channel I/Q at the signal's level");
        let spectrum = sdrtrunk_dsp::spectrum::Spectrum::new(1024, 1024).frame_f32(&ch, 8).unwrap();
        let peak = spectrum.iter().enumerate().max_by(|a, b| a.1.total_cmp(b.1)).unwrap().0 as f64;
        let peak_hz = (peak - 512.0) / 1024.0 * c.channel_rate_hz();
        assert!(peak_hz.abs() < 2500.0, "the control channel sits in the middle of its FFT: {peak_hz} Hz");
        assert_eq!(c.control_decode_hz(), freq);
        c.capture_iq(false);
        let f = c.frames();
        // Unshaped frequency steps decode about half the blocks; enough to measure with.
        assert!(f.valid >= 10 && f.invalid >= 2 && f.lost_bits > 0, "{f:?}");
        assert!((error - 250.0).abs() < 40.0, "PLL reads the signal 250 Hz high: {error}");
        let db = c.take_signal_dbfs().unwrap();
        assert!((db + 12.04).abs() < 1.5, "quarter of full scale: {db} dBFS");
        assert!(c.take_signal_dbfs().is_none(), "taken");
        let symbols = c.take_symbols();
        assert_eq!(symbols.len(), SYMBOL_CAPTURE_MAX, "capped at one second");
        c.capture_symbols(false);
        run(&mut c, &iq[..2 * 100_000]);
        assert!(c.take_symbols().is_empty(), "nothing collected while off");

        // Decoding 250 Hz higher leaves nothing for the PLL to correct.
        let mut c = ControlChannel::new(center, 2.4e6, freq, 0.0, 0, Modulation::C4fm).unwrap();
        c.set_tuning(0.0, 250.0);
        let error = run(&mut c, &iq).expect("frames decoded");
        assert!(error.abs() < 40.0, "corrected: {error}");
    }

    #[test]
    fn ldu_frames_offsets() {
        let mut bits = vec![false; 1568];
        for (n, &o) in IMBE_OFFSETS.iter().enumerate() { bits[o] = true; bits[o + 143] = n % 2 == 0; }
        let f = ldu_frames(&bits).unwrap();
        assert_eq!(f.len(), 9);
        assert!(f.iter().all(|fr| fr[0]));
        assert!(f[0][143] && !f[1][143]);
        assert!(ldu_frames(&bits[..1500]).is_none());
    }
}
