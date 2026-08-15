#![doc = "Portable, bounded realtime mixing for `LibreWave`."]

mod config;
mod engine;
mod meter;
mod transfer;

pub(crate) const ENDPOINT_COUNT: usize = librewave_core::DELIBERATE_ENDPOINTS.len();

pub use config::{
    CHANNELS, ConfigError, MAX_SOURCES, MIXER_FORMAT, MixerConfig, MixerFormat, SAMPLE_RATE_HZ,
    SampleLayout, SampleRepresentation,
};
pub use engine::{InputBuffer, MixerEngine, OutputBuffer, ProcessError, ProcessReport};
pub use meter::{BlockMeters, ChannelMeter, SourceMeter, StereoMeter};
pub use transfer::{ControlMappingError, ControlStager, StageError};

#[cfg(test)]
mod tests;
