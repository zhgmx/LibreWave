use crate::SourceId;
use std::fmt;

const MIN_HALF_DECIBEL_STEPS: i16 = -120;
const MAX_HALF_DECIBEL_STEPS: i16 = 24;

/// An exact software fader in half-decibel steps.
///
/// The inclusive range is -60.0 dB through +12.0 dB. This is a software mix
/// value. It does not represent microphone preamp gain or headphone level.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FaderGain {
    half_decibel_steps: i16,
}

impl FaderGain {
    pub const MIN: Self = Self { half_decibel_steps: MIN_HALF_DECIBEL_STEPS };
    pub const UNITY: Self = Self { half_decibel_steps: 0 };
    pub const MAX: Self = Self { half_decibel_steps: MAX_HALF_DECIBEL_STEPS };

    /// Constructs a fader from an exact number of half-decibel steps.
    ///
    /// # Errors
    ///
    /// Returns [`FaderGainError::OutOfRange`] outside -120 through +24 steps.
    pub fn from_half_decibel_steps(steps: i16) -> Result<Self, FaderGainError> {
        if !(MIN_HALF_DECIBEL_STEPS..=MAX_HALF_DECIBEL_STEPS).contains(&steps) {
            return Err(FaderGainError::OutOfRange);
        }
        Ok(Self { half_decibel_steps: steps })
    }

    /// Constructs a fader from a decibel value.
    ///
    /// # Errors
    ///
    /// Returns an error for `NaN`, infinity, a value outside -60.0 dB through
    /// +12.0 dB, or a value that is not an exact 0.5 dB step.
    #[allow(clippy::cast_possible_truncation)]
    pub fn from_decibels(decibels: f32) -> Result<Self, FaderGainError> {
        if !decibels.is_finite() {
            return Err(FaderGainError::NotFinite);
        }
        if decibels < Self::MIN.decibels() || decibels > Self::MAX.decibels() {
            return Err(FaderGainError::OutOfRange);
        }
        let steps = decibels * 2.0;
        if steps.fract() != 0.0 {
            return Err(FaderGainError::NotHalfDecibelStep);
        }
        // The finite, integral value is inside the checked i16 subrange.
        Self::from_half_decibel_steps(steps as i16)
    }

    #[must_use]
    pub const fn half_decibel_steps(self) -> i16 {
        self.half_decibel_steps
    }

    #[must_use]
    pub fn decibels(self) -> f32 {
        f32::from(self.half_decibel_steps) * 0.5
    }

    pub(crate) fn linear_coefficient(self) -> f32 {
        10.0_f32.powf(self.decibels() / 20.0)
    }
}

/// Why a software fader value is invalid.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FaderGainError {
    NotFinite,
    OutOfRange,
    NotHalfDecibelStep,
}

impl fmt::Display for FaderGainError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::NotFinite => "fader gain must be finite",
            Self::OutOfRange => "fader gain must be between -60.0 dB and +12.0 dB",
            Self::NotHalfDecibelStep => "fader gain must use exact 0.5 dB steps",
        })
    }
}

impl std::error::Error for FaderGainError {}

/// One source's route into one output mix.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MixRoute {
    enabled: bool,
    fader: FaderGain,
}

impl MixRoute {
    #[must_use]
    pub const fn new(enabled: bool, fader: FaderGain) -> Self {
        Self { enabled, fader }
    }

    #[must_use]
    pub const fn enabled(self) -> bool {
        self.enabled
    }

    #[must_use]
    pub const fn fader(self) -> FaderGain {
        self.fader
    }
}

/// One source's complete monitor and stream routing state.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SourceControls {
    source: SourceId,
    monitor: MixRoute,
    stream: MixRoute,
}

impl SourceControls {
    #[must_use]
    pub const fn new(source: SourceId, monitor: MixRoute, stream: MixRoute) -> Self {
        Self { source, monitor, stream }
    }

    #[must_use]
    pub const fn source(self) -> SourceId {
        self.source
    }

    #[must_use]
    pub const fn monitor(self) -> MixRoute {
        self.monitor
    }

    #[must_use]
    pub const fn stream(self) -> MixRoute {
        self.stream
    }
}
