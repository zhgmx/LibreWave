#![doc = "Portable, bounded realtime mixing for `LibreWave`."]

mod config;
mod engine;
mod meter;
mod meter_transfer;
mod rate_matching;
mod transfer;

pub(crate) const OUTPUT_COUNT: usize = librewave_core::MIXER_OUTPUT_ENDPOINTS.len();

pub use config::{
    CHANNELS, ConfigError, MAX_SOURCES, MIXER_FORMAT, MixerConfig, MixerFormat, SAMPLE_RATE_HZ,
    SampleLayout, SampleRepresentation,
};
pub use engine::{InputBuffer, MixerEngine, OutputBuffer, ProcessError, ProcessReport};
pub use meter::{BlockMeters, ChannelMeter, SourceMeter, StereoMeter};
pub use meter_transfer::{MeterPublisher, MeterReader};
pub use rate_matching::{
    MAX_RATE_MATCH_DEPENDENCY_RELATIVE_RATIO, MAX_RATE_MATCH_RESOURCE_OUTPUT_FRAMES,
    MIN_RATE_MATCH_DEPENDENCY_RELATIVE_RATIO, MIN_RATE_MATCH_RESOURCE_OUTPUT_FRAMES,
    PreparedRateMatch, RateMatchController, RateMatchControllerArithmetic,
    RateMatchControllerConfig, RateMatchControllerError, RateMatchControllerStep, RateMatchError,
    RateMatchFault, RateMatchRatioBounds, RateMatchRatioBoundsError, RateMatchReport, RateMatcher,
    RateMatcherConfig, RateMatcherConfigError,
};
pub use transfer::{ControlMappingError, ControlStager, StageError};

#[cfg(test)]
mod tests;
