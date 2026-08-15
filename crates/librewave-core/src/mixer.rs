use serde::{Deserialize, Serialize};
use std::fmt;

const MIN_HALF_DECIBEL_STEPS: i16 = -120;
const MAX_HALF_DECIBEL_STEPS: i16 = 24;

/// The stable logical identifier for the initial microphone source.
pub const MICROPHONE_SOURCE_ID: SourceId = SourceId::new(1);
/// The stable logical identifier for the initial system source.
pub const SYSTEM_SOURCE_ID: SourceId = SourceId::new(2);

#[derive(Clone, Copy)]
struct ProductSource {
    id: SourceId,
    role: SourceRole,
    name: &'static str,
}

const PRODUCT_SOURCES: [ProductSource; 2] = [
    ProductSource { id: MICROPHONE_SOURCE_ID, role: SourceRole::Microphone, name: "Microphone" },
    ProductSource { id: SYSTEM_SOURCE_ID, role: SourceRole::System, name: "System" },
];

/// A stable portable identifier for one logical mixer source.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(transparent)]
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

/// An exact software fader in half-decibel steps.
///
/// The inclusive range is -60.0 dB through +12.0 dB. This software mix value
/// does not represent microphone preamp gain or headphone level.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
pub struct FaderGain {
    half_decibel_steps: i16,
}

impl<'de> Deserialize<'de> for FaderGain {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct FaderGainWire {
            half_decibel_steps: i16,
        }

        let wire = FaderGainWire::deserialize(deserializer)?;
        Self::from_half_decibel_steps(wire.half_decibel_steps).map_err(serde::de::Error::custom)
    }
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
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
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
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
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

    #[must_use]
    pub const fn route(self, target: MixTarget) -> MixRoute {
        match target {
            MixTarget::Monitor => self.monitor,
            MixTarget::Stream => self.stream,
        }
    }

    #[must_use]
    pub const fn with_route(self, target: MixTarget, route: MixRoute) -> Self {
        match target {
            MixTarget::Monitor => Self { monitor: route, ..self },
            MixTarget::Stream => Self { stream: route, ..self },
        }
    }
}

/// A daemon-issued revision of the complete desired mixer profile.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(transparent)]
pub struct MixerGeneration(pub u64);

impl fmt::Display for MixerGeneration {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}", self.0)
    }
}

/// One independently controlled output mix.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum MixTarget {
    Monitor,
    Stream,
}

impl fmt::Display for MixTarget {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Monitor => "monitor",
            Self::Stream => "stream",
        })
    }
}

/// The product role of one stable logical source.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum SourceRole {
    Microphone,
    System,
}

impl fmt::Display for SourceRole {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Microphone => "microphone",
            Self::System => "system",
        })
    }
}

/// One logical source in the current product profile.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct MixerSourceSnapshot {
    pub role: SourceRole,
    pub name: String,
    pub controls: SourceControls,
}

/// The exact desired state of the current portable mixer profile.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct MixerProfile {
    pub generation: MixerGeneration,
    pub microphone_source: SourceId,
    pub sources: Vec<MixerSourceSnapshot>,
}

impl<'de> Deserialize<'de> for MixerProfile {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct MixerProfileWire {
            generation: MixerGeneration,
            microphone_source: SourceId,
            sources: Vec<MixerSourceSnapshot>,
        }

        let wire = MixerProfileWire::deserialize(deserializer)?;
        let profile = Self {
            generation: wire.generation,
            microphone_source: wire.microphone_source,
            sources: wire.sources,
        };
        profile.validate().map_err(serde::de::Error::custom)?;
        Ok(profile)
    }
}

impl Default for MixerProfile {
    fn default() -> Self {
        let unity = MixRoute::new(true, FaderGain::UNITY);
        Self {
            generation: MixerGeneration(0),
            microphone_source: MICROPHONE_SOURCE_ID,
            sources: PRODUCT_SOURCES
                .iter()
                .map(|source| MixerSourceSnapshot {
                    role: source.role,
                    name: source.name.to_owned(),
                    controls: SourceControls::new(source.id, unity, unity),
                })
                .collect(),
        }
    }
}

impl MixerProfile {
    /// Validates the one current pre-release product profile.
    ///
    /// # Errors
    ///
    /// Returns an error if any source identity, role, name, or microphone
    /// designation differs from the current schema.
    pub fn validate(&self) -> Result<(), MixerProfileError> {
        if self.microphone_source != MICROPHONE_SOURCE_ID {
            return Err(MixerProfileError::MicrophoneSource);
        }
        if self.sources.len() != PRODUCT_SOURCES.len() {
            return Err(MixerProfileError::SourceCount { actual: self.sources.len() });
        }
        for (source, expected) in self.sources.iter().zip(PRODUCT_SOURCES) {
            validate_source(source, expected)?;
        }
        Ok(())
    }

    #[must_use]
    pub fn source(&self, id: SourceId) -> Option<&MixerSourceSnapshot> {
        self.sources.iter().find(|source| source.controls.source() == id)
    }

    /// Returns a complete profile with one route replaced and its generation advanced.
    ///
    /// # Errors
    ///
    /// Returns [`MixerProfileError::UnknownSource`] when the source is absent,
    /// or [`MixerProfileError::GenerationExhausted`] at the generation limit.
    pub fn with_route(
        &self,
        source: SourceId,
        target: MixTarget,
        route: MixRoute,
    ) -> Result<Self, MixerProfileError> {
        let mut next = self.clone();
        let source_snapshot = next
            .sources
            .iter_mut()
            .find(|candidate| candidate.controls.source() == source)
            .ok_or(MixerProfileError::UnknownSource(source))?;
        source_snapshot.controls = source_snapshot.controls.with_route(target, route);
        next.generation = MixerGeneration(
            self.generation.0.checked_add(1).ok_or(MixerProfileError::GenerationExhausted)?,
        );
        Ok(next)
    }
}

fn validate_source(
    source: &MixerSourceSnapshot,
    expected: ProductSource,
) -> Result<(), MixerProfileError> {
    if source.controls.source() != expected.id
        || source.role != expected.role
        || source.name != expected.name
    {
        return Err(MixerProfileError::SourceIdentity { id: expected.id });
    }
    Ok(())
}

/// Why a persisted or constructed mixer profile is invalid.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MixerProfileError {
    MicrophoneSource,
    SourceCount { actual: usize },
    SourceIdentity { id: SourceId },
    UnknownSource(SourceId),
    GenerationExhausted,
}

impl fmt::Display for MixerProfileError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MicrophoneSource => formatter.write_str("the microphone source must be source 1"),
            Self::SourceCount { actual } => {
                write!(
                    formatter,
                    "the current mixer profile requires exactly {} sources; received {actual}",
                    PRODUCT_SOURCES.len()
                )
            }
            Self::SourceIdentity { id } => {
                write!(formatter, "mixer source {id} does not match the current stable identity")
            }
            Self::UnknownSource(id) => write!(formatter, "mixer source {id} is not configured"),
            Self::GenerationExhausted => formatter.write_str("mixer generation is exhausted"),
        }
    }
}

impl std::error::Error for MixerProfileError {}

/// Why the daemon is not applying desired mixer controls to live audio.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum MixerInactiveReason {
    AudioHostEngineNotConnected,
}

impl fmt::Display for MixerInactiveReason {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::AudioHostEngineNotConnected => {
                formatter.write_str("audio host and mixer engine are not connected")
            }
        }
    }
}

/// Whether the desired mixer profile is connected to a live audio runtime.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub enum MixerRuntimeState {
    Inactive { reason: MixerInactiveReason },
}

/// Whether live meter observations are present in the snapshot.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub enum MeterAvailability {
    Unavailable { reason: MixerInactiveReason },
}

/// The portable mixer portion of a daemon snapshot.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct MixerSnapshot {
    pub profile: MixerProfile,
    pub runtime: MixerRuntimeState,
    pub meters: MeterAvailability,
}

impl MixerSnapshot {
    #[must_use]
    pub const fn generation(&self) -> MixerGeneration {
        self.profile.generation
    }

    #[must_use]
    pub fn inactive(profile: MixerProfile) -> Self {
        let reason = MixerInactiveReason::AudioHostEngineNotConnected;
        Self {
            profile,
            runtime: MixerRuntimeState::Inactive { reason },
            meters: MeterAvailability::Unavailable { reason },
        }
    }
}

impl Default for MixerSnapshot {
    fn default() -> Self {
        Self::inactive(MixerProfile::default())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn fader_serde_is_exact_checked_integer_state() {
        let fader = FaderGain::from_decibels(-59.5).expect("valid fader");
        assert_eq!(
            serde_json::to_value(fader).expect("serialize fader"),
            json!({"half_decibel_steps": -119})
        );
        assert_eq!(
            serde_json::from_value::<FaderGain>(json!({"half_decibel_steps": -119}))
                .expect("deserialize"),
            fader
        );
        assert!(serde_json::from_value::<FaderGain>(json!({"half_decibel_steps": -121})).is_err());
        assert!(serde_json::from_value::<FaderGain>(json!(-59.5)).is_err());
    }

    #[test]
    fn mixer_structures_reject_unknown_and_old_float_shapes() {
        let route = json!({
            "enabled": true,
            "fader": {"half_decibel_steps": 0},
            "old": true
        });
        assert!(serde_json::from_value::<MixRoute>(route).is_err());
        let old_route = json!({"enabled": true, "level_db": 0.0});
        assert!(serde_json::from_value::<MixRoute>(old_route).is_err());

        let mut profile = serde_json::to_value(MixerProfile::default()).expect("serialize profile");
        profile["unexpected"] = json!(true);
        assert!(serde_json::from_value::<MixerProfile>(profile).is_err());

        let mut swapped =
            serde_json::to_value(MixerProfile::default()).expect("serialize current profile");
        swapped["sources"].as_array_mut().expect("sources").swap(0, 1);
        assert!(serde_json::from_value::<MixerProfile>(swapped).is_err());

        let mut wrong_name =
            serde_json::to_value(MixerProfile::default()).expect("serialize current profile");
        wrong_name["sources"][0]["name"] = json!("Wrong microphone");
        assert!(serde_json::from_value::<MixerProfile>(wrong_name).is_err());
    }

    #[test]
    fn default_profile_has_only_the_two_stable_unity_sources() {
        let profile = MixerProfile::default();
        profile.validate().expect("default profile");
        assert_eq!(profile.microphone_source, MICROPHONE_SOURCE_ID);
        assert_eq!(profile.sources.len(), 2);
        assert_eq!(profile.sources[0].controls.source(), MICROPHONE_SOURCE_ID);
        assert_eq!(profile.sources[1].controls.source(), SYSTEM_SOURCE_ID);
        for source in &profile.sources {
            assert_eq!(source.controls.monitor(), MixRoute::new(true, FaderGain::UNITY));
            assert_eq!(source.controls.stream(), MixRoute::new(true, FaderGain::UNITY));
        }
    }
}
