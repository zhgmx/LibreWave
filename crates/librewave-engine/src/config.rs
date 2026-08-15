use std::fmt;

/// The maximum configured input count supported by one mixer.
///
/// The bound covers four hardware inputs and eight software inputs.
pub const MAX_SOURCES: usize = 12;

/// Frames per second in the portable mixer format.
pub const SAMPLE_RATE_HZ: u32 = 48_000;

/// Channels per interleaved frame in the portable mixer format.
pub const CHANNELS: usize = 2;

/// The one floating-point DSP format accepted by the mixer.
pub const MIXER_FORMAT: MixerFormat = MixerFormat {
    sample_representation: SampleRepresentation::Float32,
    sample_rate_hz: SAMPLE_RATE_HZ,
    channels: CHANNELS,
    layout: SampleLayout::Interleaved,
};

/// A stable portable identifier for one configured logical input.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct SourceId(u16);

impl SourceId {
    #[must_use]
    pub const fn new(value: u16) -> Self {
        Self(value)
    }

    #[must_use]
    pub const fn get(self) -> u16 {
        self.0
    }
}

impl fmt::Display for SourceId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}", self.0)
    }
}

/// The portable sample representation used by the mixer.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SampleRepresentation {
    /// 32-bit floating-point DSP audio. Finite samples may exceed unity.
    Float32,
}

/// The channel layout used by each block.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SampleLayout {
    /// Consecutive samples belong to consecutive channels in the same frame.
    Interleaved,
}

/// The fixed portable mixer format.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MixerFormat {
    pub sample_representation: SampleRepresentation,
    pub sample_rate_hz: u32,
    pub channels: usize,
    pub layout: SampleLayout,
}

/// A complete mixer layout established outside realtime processing.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MixerConfig {
    max_frames: usize,
    microphone_source: SourceId,
    microphone_index: usize,
    sources: [SourceId; MAX_SOURCES],
    source_count: usize,
}

impl MixerConfig {
    /// Constructs a checked mixer layout.
    ///
    /// The configured source order does not affect processing semantics.
    /// Source identity determines every input and control mapping.
    ///
    /// # Errors
    ///
    /// Returns an error for a zero or overflowing frame capacity, an empty or
    /// oversized source set, duplicate source identifiers, or a microphone
    /// source that is absent from the set.
    pub fn try_new(
        max_frames: usize,
        microphone_source: SourceId,
        sources: &[SourceId],
    ) -> Result<Self, ConfigError> {
        if max_frames == 0 {
            return Err(ConfigError::ZeroMaxFrames);
        }
        if max_frames.checked_mul(CHANNELS).is_none() {
            return Err(ConfigError::FrameSampleCountOverflow);
        }
        if sources.is_empty() || sources.len() > MAX_SOURCES {
            return Err(ConfigError::SourceCount { actual: sources.len() });
        }

        let mut configured = [SourceId::new(0); MAX_SOURCES];
        for (index, source) in sources.iter().copied().enumerate() {
            if sources[..index].contains(&source) {
                return Err(ConfigError::DuplicateSource(source));
            }
            configured[index] = source;
        }
        let Some(microphone_index) = sources.iter().position(|source| *source == microphone_source)
        else {
            return Err(ConfigError::MicrophoneSourceMissing(microphone_source));
        };

        Ok(Self {
            max_frames,
            microphone_source,
            microphone_index,
            sources: configured,
            source_count: sources.len(),
        })
    }

    #[must_use]
    pub const fn format(&self) -> MixerFormat {
        MIXER_FORMAT
    }

    #[must_use]
    pub const fn max_frames(&self) -> usize {
        self.max_frames
    }

    #[must_use]
    pub const fn microphone_source(&self) -> SourceId {
        self.microphone_source
    }

    #[must_use]
    pub fn sources(&self) -> &[SourceId] {
        &self.sources[..self.source_count]
    }

    pub(crate) fn source_index(&self, source: SourceId) -> Option<usize> {
        self.sources().iter().position(|configured| *configured == source)
    }

    pub(crate) const fn microphone_index(&self) -> usize {
        self.microphone_index
    }
}

/// Why a mixer layout is invalid.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ConfigError {
    ZeroMaxFrames,
    FrameSampleCountOverflow,
    SourceCount { actual: usize },
    DuplicateSource(SourceId),
    MicrophoneSourceMissing(SourceId),
}

impl fmt::Display for ConfigError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ZeroMaxFrames => formatter.write_str("maximum frame count must be nonzero"),
            Self::FrameSampleCountOverflow => {
                formatter.write_str("maximum interleaved sample count overflows usize")
            }
            Self::SourceCount { actual } => write!(
                formatter,
                "configured source count must be in 1..={MAX_SOURCES}; received {actual}"
            ),
            Self::DuplicateSource(source) => write!(formatter, "duplicate source {source}"),
            Self::MicrophoneSourceMissing(source) => {
                write!(formatter, "microphone source {source} is not configured")
            }
        }
    }
}

impl std::error::Error for ConfigError {}
