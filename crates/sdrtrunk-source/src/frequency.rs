//! Tuner frequency correction. Port of sdrtrunk `FrequencyController.getTunedFrequency`.
//!
//! The hardware LO is `tuned * (1 + e)` where `e` is the reference-clock error. Programming
//! `tuned = f / (1 + ppm / 1e6)` cancels the error exactly when `ppm == e * 1e6`. A negative ppm therefore tunes the
//! hardware higher, which is what an Airspy whose clock reads low needs (one tested here needed about -4 ppm).

/// Frequency to program into the tuner for a requested center `frequency_hz`, given a correction in ppm.
/// Truncates toward zero to whole hertz, exactly as the Java `(long)` cast does.
pub fn tuned_frequency(frequency_hz: u64, correction_ppm: f64) -> u64 {
    (frequency_hz as f64 / (1.0 + correction_ppm / 1_000_000.0)) as u64
}

/// New correction (ppm) that removes an observed LO error.
///
/// With `current_ppm` programmed, the LO landed `lo_error_hz` away from `frequency_hz` (negative = too low).
/// Returns the ppm that puts the LO on frequency: `(1 + current) * (1 + lo_error / f) - 1`.
pub fn corrected_ppm(frequency_hz: u64, current_ppm: f64, lo_error_hz: f64) -> f64 {
    let current = current_ppm / 1_000_000.0;
    ((1.0 + current) * (1.0 + lo_error_hz / frequency_hz as f64) - 1.0) * 1_000_000.0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zero_ppm_is_identity() {
        assert_eq!(tuned_frequency(852_112_500, 0.0), 852_112_500);
    }

    #[test]
    fn negative_ppm_tunes_higher_positive_lower() {
        assert!(tuned_frequency(852_112_500, -3.8) > 852_112_500);
        assert!(tuned_frequency(852_112_500, 3.8) < 852_112_500);
    }

    #[test]
    fn matches_java_truncating_cast() {
        // Hand-computed: 852_112_500 / (1 - 3.84e-6) = 852_115_772.3..., Java (long) truncates.
        assert_eq!(tuned_frequency(852_112_500, -3.84), 852_115_772);
    }

    #[test]
    fn pi_airspy_case_estimates_about_minus_3_8_ppm() {
        // Installed correction -0.9117 ppm, signal needed 2.5 kHz of upward retune: LO 2.5 kHz low at 852.1125 MHz.
        let p = corrected_ppm(852_112_500, -0.9117, -2500.0);
        assert!((p - -3.845).abs() < 0.01, "{p}");
        // Both observed systems imply the same ppm (clock error, not channel error).
        let q = corrected_ppm(858_962_500, -0.9117, -2500.0);
        assert!((p - q).abs() < 0.05, "{p} vs {q}");
    }

    #[test]
    fn applying_the_estimate_puts_lo_on_frequency() {
        // Simulate the hardware: crystal error e makes the LO = tuned * (1 + e). Pick e from the Pi numbers.
        let f = 852_112_500u64;
        let e = corrected_ppm(f, -0.9117, -2500.0) / 1e6;
        let lo = tuned_frequency(f, corrected_ppm(f, -0.9117, -2500.0)) as f64 * (1.0 + e);
        assert!((lo - f as f64).abs() < 1.5, "LO off by {} Hz", lo - f as f64);
    }
}
