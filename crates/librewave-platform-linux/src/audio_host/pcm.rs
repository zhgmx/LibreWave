//! Exact Wave:3 PCM geometry and packed signed 24-bit conversion.

use std::fmt;

pub const WAVE3_PCM_RATE_HZ: u32 = 48_000;
pub const WAVE3_CAPTURE_CHANNELS: usize = 1;
pub const WAVE3_PLAYBACK_CHANNELS: usize = 2;
pub const PACKED_S24_SAMPLE_BYTES: usize = 3;

const S24_MIN: i32 = -8_388_608;
const S24_MAX: i32 = 8_388_607;
const S24_SCALE: f64 = 8_388_608.0;

/// One physical Wave:3 PCM direction.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PcmDirection {
    Capture,
    Playback,
}

impl PcmDirection {
    pub(super) const fn channels(self) -> u32 {
        match self {
            Self::Capture => 1,
            Self::Playback => 2,
        }
    }
}

/// Independent period and buffer geometry for one PCM direction.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PcmBufferConfig {
    period_frames: u32,
    buffer_frames: u32,
}

impl PcmBufferConfig {
    /// Constructs nonzero geometry whose period fits in its buffer.
    ///
    /// # Errors
    ///
    /// Returns an error for a zero value or a period larger than its buffer.
    pub fn try_new(period_frames: u32, buffer_frames: u32) -> Result<Self, PcmConfigError> {
        if period_frames == 0 {
            return Err(PcmConfigError::ZeroPeriod);
        }
        if buffer_frames == 0 {
            return Err(PcmConfigError::ZeroBuffer);
        }
        if period_frames > buffer_frames {
            return Err(PcmConfigError::PeriodExceedsBuffer { period_frames, buffer_frames });
        }
        Ok(Self { period_frames, buffer_frames })
    }

    #[must_use]
    pub const fn period_frames(self) -> u32 {
        self.period_frames
    }

    #[must_use]
    pub const fn buffer_frames(self) -> u32 {
        self.buffer_frames
    }
}

/// The only admitted Wave:3 physical PCM mode.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Wave3PhysicalIoConfig {
    capture: PcmBufferConfig,
    playback: PcmBufferConfig,
}

impl Wave3PhysicalIoConfig {
    /// Constructs independent capture and playback geometry.
    ///
    /// Both directions use packed signed `S24_3LE` at exactly 48 kHz. Capture
    /// is mono and playback is stereo. Those properties are not configurable.
    ///
    /// # Errors
    ///
    /// Returns an error for invalid direction geometry or arithmetic overflow.
    pub fn try_new(
        capture_period_frames: u32,
        capture_buffer_frames: u32,
        playback_period_frames: u32,
        playback_buffer_frames: u32,
    ) -> Result<Self, PcmConfigError> {
        let capture = PcmBufferConfig::try_new(capture_period_frames, capture_buffer_frames)?;
        let playback = PcmBufferConfig::try_new(playback_period_frames, playback_buffer_frames)?;
        let config = Self { capture, playback };
        config.capture_period_bytes()?;
        config.capture_buffer_bytes()?;
        config.playback_period_bytes()?;
        config.playback_buffer_bytes()?;
        Ok(config)
    }

    #[must_use]
    pub const fn capture(self) -> PcmBufferConfig {
        self.capture
    }

    #[must_use]
    pub const fn playback(self) -> PcmBufferConfig {
        self.playback
    }

    /// Returns the checked byte count for one capture period.
    ///
    /// # Errors
    ///
    /// Returns an error if frame-to-byte geometry overflows `usize`.
    pub fn capture_period_bytes(self) -> Result<usize, PcmConfigError> {
        checked_audio_bytes(
            usize::try_from(self.capture.period_frames)
                .map_err(|_| PcmConfigError::GeometryOverflow)?,
            WAVE3_CAPTURE_CHANNELS,
        )
    }

    /// Returns the checked byte count for the capture buffer.
    ///
    /// # Errors
    ///
    /// Returns an error if frame-to-byte geometry overflows `usize`.
    pub fn capture_buffer_bytes(self) -> Result<usize, PcmConfigError> {
        checked_audio_bytes(
            usize::try_from(self.capture.buffer_frames)
                .map_err(|_| PcmConfigError::GeometryOverflow)?,
            WAVE3_CAPTURE_CHANNELS,
        )
    }

    /// Returns the checked byte count for one playback period.
    ///
    /// # Errors
    ///
    /// Returns an error if frame-to-byte geometry overflows `usize`.
    pub fn playback_period_bytes(self) -> Result<usize, PcmConfigError> {
        checked_audio_bytes(
            usize::try_from(self.playback.period_frames)
                .map_err(|_| PcmConfigError::GeometryOverflow)?,
            WAVE3_PLAYBACK_CHANNELS,
        )
    }

    /// Returns the checked byte count for the playback buffer.
    ///
    /// # Errors
    ///
    /// Returns an error if frame-to-byte geometry overflows `usize`.
    pub fn playback_buffer_bytes(self) -> Result<usize, PcmConfigError> {
        checked_audio_bytes(
            usize::try_from(self.playback.buffer_frames)
                .map_err(|_| PcmConfigError::GeometryOverflow)?,
            WAVE3_PLAYBACK_CHANNELS,
        )
    }

    pub(super) const fn parameters(self, direction: PcmDirection) -> PhysicalPcmParameters {
        let geometry = match direction {
            PcmDirection::Capture => self.capture,
            PcmDirection::Playback => self.playback,
        };
        PhysicalPcmParameters {
            direction,
            rate: WAVE3_PCM_RATE_HZ,
            channels: direction.channels(),
            period_frames: geometry.period_frames,
            buffer_frames: geometry.buffer_frames,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct PhysicalPcmParameters {
    pub(super) direction: PcmDirection,
    pub(super) rate: u32,
    pub(super) channels: u32,
    pub(super) period_frames: u32,
    pub(super) buffer_frames: u32,
}

/// Why physical PCM geometry is invalid.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PcmConfigError {
    ZeroPeriod,
    ZeroBuffer,
    PeriodExceedsBuffer { period_frames: u32, buffer_frames: u32 },
    GeometryOverflow,
}

impl fmt::Display for PcmConfigError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ZeroPeriod => formatter.write_str("PCM period frames must be nonzero"),
            Self::ZeroBuffer => formatter.write_str("PCM buffer frames must be nonzero"),
            Self::PeriodExceedsBuffer { period_frames, buffer_frames } => {
                write!(formatter, "PCM period {period_frames} exceeds buffer {buffer_frames}")
            }
            Self::GeometryOverflow => {
                formatter.write_str("PCM frame, sample, or byte geometry overflows")
            }
        }
    }
}

impl std::error::Error for PcmConfigError {}

pub(super) fn checked_audio_bytes(frames: usize, channels: usize) -> Result<usize, PcmConfigError> {
    frames
        .checked_mul(channels)
        .and_then(|samples| samples.checked_mul(PACKED_S24_SAMPLE_BYTES))
        .ok_or(PcmConfigError::GeometryOverflow)
}

/// Why one packed audio block is invalid.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PackedS24Error {
    InvalidChannels { actual: usize },
    PartialSample { bytes: usize },
    PartialFrame { samples: usize, channels: usize },
    OutputLength { expected: usize, actual: usize },
    NonFinite { sample_index: usize },
    GeometryOverflow,
}

impl fmt::Display for PackedS24Error {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidChannels { actual } => {
                write!(formatter, "packed audio channel count must be nonzero: {actual}")
            }
            Self::PartialSample { bytes } => {
                write!(formatter, "packed audio has {bytes} bytes, not complete 3-byte samples")
            }
            Self::PartialFrame { samples, channels } => write!(
                formatter,
                "packed audio has {samples} samples, not complete {channels}-channel frames"
            ),
            Self::OutputLength { expected, actual } => {
                write!(
                    formatter,
                    "packed conversion output has length {actual}; expected {expected}"
                )
            }
            Self::NonFinite { sample_index } => {
                write!(formatter, "audio sample {sample_index} is not finite")
            }
            Self::GeometryOverflow => formatter.write_str("packed audio geometry overflows"),
        }
    }
}

impl std::error::Error for PackedS24Error {}

pub(super) fn packed_frame_count(
    byte_length: usize,
    channels: usize,
) -> Result<usize, PackedS24Error> {
    if channels == 0 {
        return Err(PackedS24Error::InvalidChannels { actual: channels });
    }
    if !byte_length.is_multiple_of(PACKED_S24_SAMPLE_BYTES) {
        return Err(PackedS24Error::PartialSample { bytes: byte_length });
    }
    let samples = byte_length / PACKED_S24_SAMPLE_BYTES;
    if !samples.is_multiple_of(channels) {
        return Err(PackedS24Error::PartialFrame { samples, channels });
    }
    Ok(samples / channels)
}

pub(super) fn decode_s24_3le(
    bytes: &[u8],
    channels: usize,
    output: &mut [f32],
) -> Result<usize, PackedS24Error> {
    let frames = packed_frame_count(bytes.len(), channels)?;
    let samples = frames.checked_mul(channels).ok_or(PackedS24Error::GeometryOverflow)?;
    if output.len() != samples {
        return Err(PackedS24Error::OutputLength { expected: samples, actual: output.len() });
    }
    for (sample, encoded) in output.iter_mut().zip(bytes.chunks_exact(PACKED_S24_SAMPLE_BYTES)) {
        let sign = if encoded[2] & 0x80 == 0 { 0 } else { 0xff };
        let raw = i32::from_le_bytes([encoded[0], encoded[1], encoded[2], sign]);
        #[allow(clippy::cast_possible_truncation)]
        {
            *sample = (f64::from(raw) / S24_SCALE) as f32;
        }
    }
    Ok(frames)
}

pub(super) fn encode_s24_3le(
    samples: &[f32],
    channels: usize,
    output: &mut [u8],
) -> Result<usize, PackedS24Error> {
    if channels == 0 {
        return Err(PackedS24Error::InvalidChannels { actual: channels });
    }
    if !samples.len().is_multiple_of(channels) {
        return Err(PackedS24Error::PartialFrame { samples: samples.len(), channels });
    }
    let expected = samples
        .len()
        .checked_mul(PACKED_S24_SAMPLE_BYTES)
        .ok_or(PackedS24Error::GeometryOverflow)?;
    if output.len() != expected {
        return Err(PackedS24Error::OutputLength { expected, actual: output.len() });
    }
    if let Some(sample_index) = samples.iter().position(|sample| !sample.is_finite()) {
        return Err(PackedS24Error::NonFinite { sample_index });
    }
    for (sample, encoded) in
        samples.iter().copied().zip(output.chunks_exact_mut(PACKED_S24_SAMPLE_BYTES))
    {
        let raw = if sample <= -1.0 {
            S24_MIN
        } else if sample >= 1.0 {
            S24_MAX
        } else {
            #[allow(clippy::cast_possible_truncation)]
            {
                (f64::from(sample) * S24_SCALE)
                    .round()
                    .clamp(f64::from(S24_MIN), f64::from(S24_MAX)) as i32
            }
        };
        let bytes = raw.to_le_bytes();
        encoded.copy_from_slice(&bytes[..PACKED_S24_SAMPLE_BYTES]);
    }
    Ok(samples.len() / channels)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::cast_possible_truncation, clippy::float_cmp)]

    use super::*;

    fn bytes(raw: &[i32]) -> Vec<u8> {
        raw.iter().flat_map(|sample| sample.to_le_bytes()[..3].to_vec()).collect()
    }

    #[test]
    fn physical_geometry_is_exact_independent_and_checked() {
        let config = Wave3PhysicalIoConfig::try_new(1, 1, 3, 5).expect("valid one-period buffers");
        assert_eq!(
            config.parameters(PcmDirection::Capture),
            PhysicalPcmParameters {
                direction: PcmDirection::Capture,
                rate: 48_000,
                channels: 1,
                period_frames: 1,
                buffer_frames: 1,
            }
        );
        assert_eq!(
            config.parameters(PcmDirection::Playback),
            PhysicalPcmParameters {
                direction: PcmDirection::Playback,
                rate: 48_000,
                channels: 2,
                period_frames: 3,
                buffer_frames: 5,
            }
        );
        assert_eq!(config.capture_period_bytes(), Ok(3));
        assert_eq!(config.capture_buffer_bytes(), Ok(3));
        assert_eq!(config.playback_period_bytes(), Ok(18));
        assert_eq!(config.playback_buffer_bytes(), Ok(30));
        assert!(matches!(
            Wave3PhysicalIoConfig::try_new(2, 1, 3, 3),
            Err(PcmConfigError::PeriodExceedsBuffer { .. })
        ));
        assert_eq!(Wave3PhysicalIoConfig::try_new(0, 1, 1, 1), Err(PcmConfigError::ZeroPeriod));
        assert_eq!(Wave3PhysicalIoConfig::try_new(1, 1, 1, 0), Err(PcmConfigError::ZeroBuffer));
        assert_eq!(checked_audio_bytes(usize::MAX, 2), Err(PcmConfigError::GeometryOverflow));
    }

    #[test]
    fn s24_3le_decodes_minimum_maximum_zero_and_sign_extension() {
        let encoded = bytes(&[S24_MIN, -2, -1, 0, 1, S24_MAX]);
        let mut decoded = [0.0; 6];
        assert_eq!(decode_s24_3le(&encoded, 1, &mut decoded), Ok(6));
        assert_eq!(decoded[0], -1.0);
        assert_eq!(decoded[1], -2.0 / 8_388_608.0);
        assert_eq!(decoded[2], -1.0 / 8_388_608.0);
        assert_eq!(decoded[3], 0.0);
        assert_eq!(decoded[4], 1.0 / 8_388_608.0);
        assert_eq!(decoded[5], 8_388_607.0 / 8_388_608.0);
    }

    #[test]
    fn s24_3le_encoding_rounds_and_saturates_deterministically() {
        let half_step = (0.5 / S24_SCALE) as f32;
        let samples = [-2.0, -1.0, -half_step, 0.0, half_step, 1.0, 2.0];
        let mut encoded = [0; 21];
        assert_eq!(encode_s24_3le(&samples, 1, &mut encoded), Ok(samples.len()));
        assert_eq!(encoded.as_slice(), bytes(&[S24_MIN, S24_MIN, -1, 0, 1, S24_MAX, S24_MAX]));
    }

    #[test]
    fn packed_blocks_reject_partial_samples_frames_and_nonfinite_values() {
        assert_eq!(packed_frame_count(1, 1), Err(PackedS24Error::PartialSample { bytes: 1 }));
        assert_eq!(
            packed_frame_count(3, 2),
            Err(PackedS24Error::PartialFrame { samples: 1, channels: 2 })
        );
        assert_eq!(
            encode_s24_3le(&[f32::NAN], 1, &mut [0; 3]),
            Err(PackedS24Error::NonFinite { sample_index: 0 })
        );
    }
}
