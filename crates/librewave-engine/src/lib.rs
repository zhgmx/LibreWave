#![doc = "Portable, bounded realtime mixing for `LibreWave`."]

mod config;
mod engine;
mod meter;
mod meter_transfer;
mod transfer;

pub(crate) const OUTPUT_COUNT: usize = librewave_core::MIXER_OUTPUT_ENDPOINTS.len();

pub use config::{
    CHANNELS, ConfigError, MAX_SOURCES, MIXER_FORMAT, MixerConfig, MixerFormat, SAMPLE_RATE_HZ,
    SampleLayout, SampleRepresentation,
};
pub use engine::{InputBuffer, MixerEngine, OutputBuffer, ProcessError, ProcessReport};
pub use meter::{BlockMeters, ChannelMeter, SourceMeter, StereoMeter};
pub use meter_transfer::{MeterPublisher, MeterReader};
pub use transfer::{ControlMappingError, ControlStager, StageError};

#[cfg(test)]
mod tests;
