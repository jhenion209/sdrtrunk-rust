//! P25 decoder front ends: the sample-processing chain at the top of sdrtrunk's `P25P1DecoderC4FM.receive()` and
//! `P25P1DecoderLSM.receive()`: decimate (to just under 38.4 kHz) -> baseband low-pass -> RRC pulse shaping.
//! C4FM then differentially demodulates to phase values; LSM (CQPSK) hands the filtered I/Q to its demodulator.
use crate::baseband::{baseband_taps, lsm_baseband_taps};
use crate::decimate::HalfBandDecimator;
use crate::diff_demod::DifferentialDemodulator;
use crate::fir::RealFir;
use crate::rrc::root_raised_cosine;

const SYMBOL_RATE: u32 = 4800;

/// Decimation, baseband low-pass and pulse shaping, per I and Q.
struct Shaping {
    decimator: Option<(HalfBandDecimator, HalfBandDecimator)>,
    baseband: (RealFir, RealFir),
    pulse_shaping: (RealFir, RealFir),
    decimated_sample_rate: f32,
}

impl Shaping {
    /// `sample_rate` is the channel rate (50 kHz from the M2 channelizer). Only decimation factors 1 and 2 are
    /// supported, which covers channel rates up to 76.8 kHz. `taps` gives the baseband filter for a decimated rate.
    /// `rrc_in_double`: the LSM decoder computes the RRC samples per symbol in double, C4FM in float.
    fn new(sample_rate: f64, taps: fn(u32) -> Option<Vec<f32>>, rrc_in_double: bool) -> Result<Self, String> {
        if sample_rate <= (SYMBOL_RATE * 2) as f64 {
            return Err(format!("Sample rate [{sample_rate}] must be >9600 (2 * {SYMBOL_RATE} symbol rate)"));
        }
        let mut decimation = 1u32;
        while sample_rate / decimation as f64 >= 38_400.0 {
            decimation *= 2;
        }
        if decimation > 2 {
            return Err(format!("Sample rate [{sample_rate}] needs decimation by {decimation}; only 1 and 2 are ported"));
        }
        // Java computes this in float.
        let decimated = sample_rate as f32 / decimation as f32;
        let taps_rate = decimated as u32;
        let baseband = taps(taps_rate)
            .ok_or_else(|| format!("No baseband filter available for decimated rate {taps_rate} Hz"))?;
        let rrc_sps = if rrc_in_double { decimated as f64 / SYMBOL_RATE as f64 } else { (decimated / SYMBOL_RATE as f32) as f64 };
        let rrc = root_raised_cosine(rrc_sps, 16, 0.2);
        Ok(Self {
            decimator: (decimation == 2).then(|| (HalfBandDecimator::x2(), HalfBandDecimator::x2())),
            baseband: (RealFir::new(&baseband), RealFir::new(&baseband)),
            pulse_shaping: (RealFir::new(&rrc), RealFir::new(&rrc)),
            decimated_sample_rate: decimated,
        })
    }

    fn process(&mut self, i: &[f32], q: &[f32]) -> (Vec<f32>, Vec<f32>) {
        let (mut i, mut q) = match &mut self.decimator {
            Some((di, dq)) => (di.decimate(i), dq.decimate(q)),
            None => (i.to_vec(), q.to_vec()),
        };
        i = self.baseband.0.filter(&i);
        q = self.baseband.1.filter(&q);
        (self.pulse_shaping.0.filter(&i), self.pulse_shaping.1.filter(&q))
    }
}

pub struct C4fmFrontEnd {
    shaping: Shaping,
    demodulator: DifferentialDemodulator,
}

impl C4fmFrontEnd {
    pub fn new(sample_rate: f64) -> Result<Self, String> {
        let shaping = Shaping::new(sample_rate, baseband_taps, false)?;
        let demodulator = DifferentialDemodulator::new(shaping.decimated_sample_rate as f64, SYMBOL_RATE);
        Ok(Self { shaping, demodulator })
    }

    pub fn decimated_sample_rate(&self) -> f32 { self.shaping.decimated_sample_rate }
    pub fn samples_per_symbol(&self) -> f32 { self.demodulator.samples_per_symbol() }

    /// Seconds each demodulated phase difference spans: a carrier offset of f Hz adds 2*pi*f times this.
    pub fn differential_seconds(&self) -> f64 { self.demodulator.delay_samples() as f64 / self.decimated_sample_rate() as f64 }

    /// Processes one channel I/Q buffer (even length when decimating) into demodulated values.
    pub fn process(&mut self, i: &[f32], q: &[f32]) -> Vec<f32> {
        let (i, q) = self.shaping.process(i, q);
        self.demodulator.demodulate(&i, &q)
    }
}

/// LSM (CQPSK, simulcast) front end: filtered, pulse-shaped I/Q for `sdrtrunk_p25::demod_lsm::LsmDemodulator`.
pub struct LsmFrontEnd { shaping: Shaping }

impl LsmFrontEnd {
    pub fn new(sample_rate: f64) -> Result<Self, String> {
        Ok(Self { shaping: Shaping::new(sample_rate, lsm_baseband_taps, true)? })
    }

    /// Java: `decimatedSampleRate / SYMBOL_RATE` in float.
    pub fn samples_per_symbol(&self) -> f32 { self.shaping.decimated_sample_rate / SYMBOL_RATE as f32 }

    /// Processes one channel I/Q buffer (even length when decimating) into filtered I/Q.
    pub fn process(&mut self, i: &[f32], q: &[f32]) -> (Vec<f32>, Vec<f32>) { self.shaping.process(i, q) }
}
