//! Maps a wanted frequency and bandwidth to polyphase channelizer bin index(es) and the mixer offset.
//! Ported from sdrtrunk `dsp.filter.channelizer.ChannelCalculator`; behaviour at bin boundaries and at the
//! wrap-around bin (N/2) is preserved exactly.

/// Which bin to pick when a frequency sits exactly on the boundary of two bins (or on the wrap-around bin).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Boundary {
    AdjustPositive,
    AdjustNegative,
}

#[derive(Clone, Debug)]
pub struct ChannelCalculator {
    sample_rate: f64,
    channel_count: i32,
    center_frequency: f64,
    channel_bandwidth: f64,
    oversampling: f64,
}

impl ChannelCalculator {
    /// `oversampling` is 2.0 for the M2 channelizer.
    pub fn new(sample_rate: f64, channel_count: usize, center_frequency: f64, oversampling: f64) -> Self {
        Self {
            sample_rate,
            channel_count: channel_count as i32,
            center_frequency,
            channel_bandwidth: sample_rate / channel_count as f64,
            oversampling,
        }
    }

    pub fn set_center_frequency(&mut self, frequency: f64) { self.center_frequency = frequency; }
    pub fn center_frequency(&self) -> f64 { self.center_frequency }
    pub fn channel_count(&self) -> usize { self.channel_count as usize }
    pub fn channel_bandwidth(&self) -> f64 { self.channel_bandwidth }
    fn half_bandwidth(&self) -> f64 { self.channel_bandwidth / 2.0 }
    /// Output sample rate of each channel (bandwidth times oversampling).
    pub fn channel_sample_rate(&self) -> f64 { self.channel_bandwidth * self.oversampling }
    fn wrap_around_index(&self) -> i32 { self.channel_count / 2 }
    pub fn minimum_frequency(&self) -> i64 { (self.center_frequency - self.sample_rate / 2.0) as i64 }
    pub fn maximum_frequency(&self) -> i64 { (self.center_frequency + self.sample_rate / 2.0) as i64 }

    fn normalize(&self, mut index: i32) -> i32 {
        while index < 0 { index += self.channel_count; }
        while index >= self.channel_count { index -= self.channel_count; }
        index
    }

    /// Bin index(es) that must be combined to produce a channel at `frequency` with `bandwidth` (Hz).
    /// Returns one index normally, or two or more when the signal spans bin boundaries.
    pub fn channel_indexes(&self, frequency: i64, bandwidth: i64) -> Result<Vec<usize>, String> {
        let min_frequency = frequency - bandwidth / 2;
        let max_frequency = frequency + bandwidth / 2;
        if min_frequency < self.minimum_frequency() || max_frequency > self.maximum_frequency() {
            return Err(format!(
                "Requested channel cannot be provided by this channelizer. Requested channel [{} - {}] exceeds current channelizer frequency range [{} - {}]",
                min_frequency, max_frequency, self.minimum_frequency(), self.maximum_frequency()
            ));
        }
        let min_index = self.index_for_frequency(min_frequency, Boundary::AdjustPositive);
        let max_index = self.index_for_frequency(max_frequency, Boundary::AdjustNegative);
        let wrap = self.wrap_around_index();
        if min_index == wrap && max_index == wrap {
            return Err("Requested tuner channel cannot be provided. Requested bandwidth is within two channel bandwidths of the sample rate.".into());
        }
        let mut indexes = vec![min_index as usize];
        if min_index != max_index {
            if min_index < 0 || min_index >= self.channel_count || max_index < 0 || max_index >= self.channel_count {
                return Err("Something went wrong while calculating the min and max polyphase channel indexes".into());
            }
            let mut pointer = min_index + 1;
            if pointer >= self.channel_count { pointer -= self.channel_count; }
            while pointer != max_index {
                indexes.push(pointer as usize);
                pointer += 1;
                if pointer >= self.channel_count { pointer -= self.channel_count; }
            }
            indexes.push(max_index as usize);
        }
        Ok(indexes)
    }

    pub fn index_for_frequency(&self, frequency: i64, policy: Boundary) -> i32 {
        let mut offset = frequency as f64 - self.center_frequency;
        if offset.abs() < self.half_bandwidth() {
            return 0;
        }
        if offset > 0.0 { offset += self.half_bandwidth(); } else { offset -= self.half_bandwidth(); }
        let mut index = (offset / self.channel_bandwidth) as i32;
        if index < 0 {
            index += self.channel_count;
        }
        if policy == Boundary::AdjustPositive && self.is_overlap_frequency(frequency, index, index + 1) {
            index = self.normalize(index + 1);
        } else if policy == Boundary::AdjustNegative && self.is_overlap_frequency(frequency, index - 1, index) {
            index = self.normalize(index - 1);
        }
        index
    }

    /// True when `frequency` is the maximum of bin `index1` and the minimum of the adjacent bin `index2`.
    pub fn is_overlap_frequency(&self, frequency: i64, index1: i32, index2: i32) -> bool {
        let index1 = self.normalize(index1);
        let index2 = self.normalize(index2);
        if self.normalize(index2 - index1) != 1 {
            return false;
        }
        let wrap = self.wrap_around_index();
        let mut index1_max = self.index_maximum_frequency(index1, Boundary::AdjustPositive) as i64;
        let mut index2_min = self.index_minimum_frequency(index2, Boundary::AdjustNegative) as i64;
        if index1 == wrap {
            index1_max = self.index_maximum_frequency(index1, Boundary::AdjustNegative) as i64;
        }
        if index2 == wrap {
            index2_min = self.index_minimum_frequency(index2, Boundary::AdjustPositive) as i64;
        }
        frequency == index1_max && frequency == index2_min
    }

    pub fn index_center_frequency(&self, index: i32, policy: Boundary) -> f64 {
        assert!(index >= 0 && index < self.channel_count, "Illegal channel index [{index}]");
        let bw = self.channel_bandwidth;
        let wrap = self.wrap_around_index();
        if index == wrap {
            if policy == Boundary::AdjustPositive {
                self.center_frequency + index as f64 * bw
            } else {
                self.center_frequency - index as f64 * bw
            }
        } else if index < wrap {
            self.center_frequency + index as f64 * bw
        } else {
            self.center_frequency - (self.channel_count - index) as f64 * bw
        }
    }

    pub fn index_minimum_frequency(&self, index: i32, policy: Boundary) -> f64 {
        assert!(index >= 0 && index < self.channel_count, "illegal channel index [{index}]");
        let (bw, half) = (self.channel_bandwidth, self.half_bandwidth());
        let wrap = self.wrap_around_index();
        if index == wrap {
            if policy == Boundary::AdjustPositive {
                self.center_frequency + index as f64 * bw - half
            } else {
                self.index_center_frequency(index, policy)
            }
        } else if index <= wrap {
            self.center_frequency + index as f64 * bw - half
        } else {
            self.center_frequency - (self.channel_count - index) as f64 * bw - half
        }
    }

    pub fn index_maximum_frequency(&self, index: i32, policy: Boundary) -> f64 {
        assert!(index >= 0 && index < self.channel_count, "illegal channel index [{index}]");
        let (bw, half) = (self.channel_bandwidth, self.half_bandwidth());
        let wrap = self.wrap_around_index();
        if index == wrap {
            if policy == Boundary::AdjustPositive {
                self.index_center_frequency(index, policy)
            } else {
                self.center_frequency - (index as f64 * bw - half)
            }
        } else if index <= wrap {
            self.center_frequency + index as f64 * bw + half
        } else {
            self.center_frequency - (self.channel_count - index) as f64 * bw + half
        }
    }

    /// Center frequency of the channel synthesized from contiguous `indexes`.
    pub fn center_frequency_for_indexes(&self, indexes: &[usize]) -> i64 {
        assert!(!indexes.is_empty(), "Indexes cannot be empty");
        let center = (indexes.len() - 1) / 2;
        let index = indexes[center] as i32;
        if indexes.len() % 2 == 0 {
            self.index_maximum_frequency(index, Boundary::AdjustNegative) as i64
        } else if index == self.wrap_around_index() {
            self.index_center_frequency(index, Boundary::AdjustNegative) as i64
        } else {
            self.index_center_frequency(index, Boundary::AdjustPositive) as i64
        }
    }

    /// Mixer frequency (Hz) that moves `channel_frequency` to baseband: bin center - wanted + tuner correction.
    pub fn frequency_offset(&self, indexes: &[usize], channel_frequency: i64, frequency_correction: i64) -> i64 {
        self.center_frequency_for_indexes(indexes) - channel_frequency + frequency_correction
    }
}
