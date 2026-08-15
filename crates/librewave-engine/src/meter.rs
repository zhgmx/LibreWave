use crate::OUTPUT_COUNT;
use crate::config::{CHANNELS, MAX_SOURCES, MixerConfig};
use librewave_core::{EndpointId, MIXER_OUTPUT_ENDPOINTS, SourceId};

/// Peak and RMS linear amplitude for one channel in one block.
///
/// RMS uses f64 square accumulation. A result above the finite f32 range
/// saturates at `f32::MAX` when the report is built.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct ChannelMeter {
    peak: f32,
    rms: f32,
}

impl ChannelMeter {
    #[must_use]
    pub const fn peak(self) -> f32 {
        self.peak
    }

    #[must_use]
    pub const fn rms(self) -> f32 {
        self.rms
    }
}

/// Separate left and right meters.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct StereoMeter {
    left: ChannelMeter,
    right: ChannelMeter,
}

impl StereoMeter {
    /// The zero-frame meter value. Peak and RMS are `0.0` for both channels.
    pub const ZERO: Self = Self {
        left: ChannelMeter { peak: 0.0, rms: 0.0 },
        right: ChannelMeter { peak: 0.0, rms: 0.0 },
    };

    #[must_use]
    pub const fn left(self) -> ChannelMeter {
        self.left
    }

    #[must_use]
    pub const fn right(self) -> ChannelMeter {
        self.right
    }
}

/// One identified source meter owned by a process report.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SourceMeter {
    source: SourceId,
    meter: StereoMeter,
}

impl SourceMeter {
    const EMPTY: Self = Self { source: SourceId::new(0), meter: StereoMeter::ZERO };

    #[must_use]
    pub const fn source(self) -> SourceId {
        self.source
    }

    #[must_use]
    pub const fn meter(self) -> StereoMeter {
        self.meter
    }
}

/// Fixed-capacity stereo meters for one accepted block.
///
/// Source meters observe input samples at unity before either mix route.
/// Endpoint meters observe samples after the final `-1.0..=1.0` clip. RMS is
/// calculated independently for each channel across the actual frame count.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct BlockMeters {
    sources: [SourceMeter; MAX_SOURCES],
    source_count: usize,
    endpoints: [StereoMeter; OUTPUT_COUNT],
}

impl BlockMeters {
    pub(crate) fn zero(config: &MixerConfig) -> Self {
        let mut meters = Self {
            sources: [SourceMeter::EMPTY; MAX_SOURCES],
            source_count: config.sources().len(),
            endpoints: [StereoMeter::ZERO; OUTPUT_COUNT],
        };
        for (index, source) in config.sources().iter().copied().enumerate() {
            meters.sources[index].source = source;
        }
        meters
    }

    #[must_use]
    pub fn sources(&self) -> &[SourceMeter] {
        &self.sources[..self.source_count]
    }

    #[must_use]
    pub fn source(&self, source: SourceId) -> Option<StereoMeter> {
        self.sources().iter().find(|meter| meter.source == source).map(|meter| meter.meter)
    }

    #[must_use]
    pub fn endpoint(&self, endpoint: EndpointId) -> Option<StereoMeter> {
        let index = mixer_output_index(endpoint)?;
        Some(self.endpoints[index])
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct StereoAccumulator {
    channels: [ChannelAccumulator; CHANNELS],
}

impl StereoAccumulator {
    pub(crate) fn observe(&mut self, channel: usize, sample: f32) {
        self.channels[channel].observe(sample);
    }

    fn finish(self) -> StereoMeter {
        StereoMeter { left: self.channels[0].finish(), right: self.channels[1].finish() }
    }
}

#[derive(Clone, Copy, Debug, Default)]
struct ChannelAccumulator {
    peak: f32,
    sum_squares: f64,
    observations: f64,
}

impl ChannelAccumulator {
    fn observe(&mut self, sample: f32) {
        self.peak = self.peak.max(sample.abs());
        let sample = f64::from(sample);
        self.sum_squares += sample * sample;
        self.observations += 1.0;
    }

    #[allow(clippy::cast_possible_truncation)]
    fn finish(self) -> ChannelMeter {
        // Every observed f32 sample is finite. Its f64 RMS is finite and no
        // larger than f32::MAX before conversion back to the API format.
        let rms = (self.sum_squares / self.observations).sqrt().min(f64::from(f32::MAX)) as f32;
        ChannelMeter { peak: self.peak, rms }
    }
}

pub(crate) fn finish_block(
    config: &MixerConfig,
    sources: &[StereoAccumulator; MAX_SOURCES],
    endpoints: &[StereoAccumulator; OUTPUT_COUNT],
) -> BlockMeters {
    let mut meters = BlockMeters::zero(config);
    for (index, source) in config.sources().iter().copied().enumerate() {
        meters.sources[index] = SourceMeter { source, meter: sources[index].finish() };
    }
    meters.endpoints = std::array::from_fn(|index| endpoints[index].finish());
    meters
}

pub(crate) fn mixer_output_index(endpoint: EndpointId) -> Option<usize> {
    MIXER_OUTPUT_ENDPOINTS.iter().position(|candidate| *candidate == endpoint)
}
