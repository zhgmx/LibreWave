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

/// Software levels applied after each complete mix sum, before final clipping.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct OutputControls {
    monitor: MixRoute,
    stream: MixRoute,
}
impl Default for OutputControls {
    fn default() -> Self {
        let unity = MixRoute::new(true, FaderGain::UNITY);
        Self { monitor: unity, stream: unity }
    }
}
impl OutputControls {
    /// Reads one software output level and mute state.
    #[must_use]
    pub const fn route(self, target: MixTarget) -> MixRoute {
        match target {
            MixTarget::Monitor => self.monitor,
            MixTarget::Stream => self.stream,
        }
    }
    /// Replaces one software output without changing the other.
    #[must_use]
    pub const fn with_route(self, target: MixTarget, route: MixRoute) -> Self {
        match target {
            MixTarget::Monitor => Self { monitor: route, ..self },
            MixTarget::Stream => Self { stream: route, ..self },
        }
    }
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
    Software,
}

impl fmt::Display for SourceRole {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Microphone => "microphone",
            Self::System => "system",
            Self::Software => "software",
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
    pub next_source_id: u16,
    pub assignments: Vec<crate::ApplicationAssignment>,
    pub microphone_source: SourceId,
    pub sources: Vec<MixerSourceSnapshot>,
    pub outputs: OutputControls,
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
            next_source_id: u16,
            assignments: Vec<crate::ApplicationAssignment>,
            microphone_source: SourceId,
            sources: Vec<MixerSourceSnapshot>,
            outputs: OutputControls,
        }

        let wire = MixerProfileWire::deserialize(deserializer)?;
        let profile = Self {
            generation: wire.generation,
            next_source_id: wire.next_source_id,
            assignments: wire.assignments,
            microphone_source: wire.microphone_source,
            sources: wire.sources,
            outputs: wire.outputs,
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
            next_source_id: 3,
            assignments: Vec::new(),
            microphone_source: MICROPHONE_SOURCE_ID,
            outputs: OutputControls::default(),
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
    /// Sets both channel faders in one profile revision, preserving their mute states.
    /// # Errors
    /// Returns an error for a missing source or exhausted generation.
    pub fn with_faders(
        &self,
        source: SourceId,
        monitor: FaderGain,
        stream: FaderGain,
    ) -> Result<Self, MixerProfileError> {
        let mut next = self.clone();
        let controls = &mut next
            .sources
            .iter_mut()
            .find(|s| s.controls.source() == source)
            .ok_or(MixerProfileError::UnknownSource(source))?
            .controls;
        *controls = SourceControls::new(
            source,
            MixRoute::new(controls.monitor().enabled(), monitor),
            MixRoute::new(controls.stream().enabled(), stream),
        );
        next.advance_generation()?;
        Ok(next)
    }
    /// Sets one output in one profile revision.
    /// # Errors
    /// Returns an error for an exhausted generation.
    pub fn with_output(
        &self,
        target: MixTarget,
        route: MixRoute,
    ) -> Result<Self, MixerProfileError> {
        let mut next = self.clone();
        next.outputs = next.outputs.with_route(target, route);
        next.advance_generation()?;
        Ok(next)
    }
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
        if !(PRODUCT_SOURCES.len()..=crate::MAX_MIXER_SOURCES).contains(&self.sources.len()) {
            return Err(MixerProfileError::SourceCount { actual: self.sources.len() });
        }
        for expected in PRODUCT_SOURCES {
            let source =
                self.source(expected.id).ok_or(MixerProfileError::UnknownSource(expected.id))?;
            validate_source(source, expected)?;
        }
        if self.next_source_id < 3 {
            return Err(MixerProfileError::SourceIdExhausted);
        }
        for (index, source) in self.sources.iter().enumerate() {
            let id = source.controls.source();
            if id.get() == 0
                || id.get() >= self.next_source_id
                || self.sources[..index].iter().any(|prior| prior.controls.source() == id)
                || (id.get() >= 3
                    && (source.role != SourceRole::Software
                        || crate::InputName::try_new(&source.name).is_err()))
            {
                return Err(MixerProfileError::SourceIdentity { id });
            }
        }
        if self.assignments.len() > 128
            || self.assignments.iter().enumerate().any(|(index, rule)| {
                self.source(rule.source).is_none_or(|source| source.role == SourceRole::Microphone)
                    || self.assignments[..index]
                        .iter()
                        .any(|prior| prior.application == rule.application)
            })
        {
            return Err(MixerProfileError::InvalidAssignment);
        }
        Ok(())
    }

    /// Adds a named software input with a new identity.
    /// # Errors
    /// Returns an error when capacity or the profile generation is exhausted.
    pub fn with_input(&self, name: crate::InputName) -> Result<Self, MixerProfileError> {
        if self.sources.len() == crate::MAX_MIXER_SOURCES {
            return Err(MixerProfileError::SourceCount { actual: self.sources.len() + 1 });
        }
        let id = SourceId::new(self.next_source_id);
        let mut next = self.clone();
        next.next_source_id =
            self.next_source_id.checked_add(1).ok_or(MixerProfileError::SourceIdExhausted)?;
        let unity = MixRoute::new(true, FaderGain::UNITY);
        next.sources.push(MixerSourceSnapshot {
            role: SourceRole::Software,
            name: name.as_str().to_owned(),
            controls: SourceControls::new(id, unity, unity),
        });
        next.advance_generation()?;
        Ok(next)
    }

    /// Removes one software input. Built-in inputs cannot be removed.
    /// # Errors
    /// Returns an error for a built-in or absent input or exhausted generation.
    pub fn without_input(&self, id: SourceId) -> Result<Self, MixerProfileError> {
        if self.source(id).is_none_or(|source| source.role != SourceRole::Software) {
            return Err(MixerProfileError::UnknownSource(id));
        }
        let mut next = self.clone();
        next.sources.retain(|source| source.controls.source() != id);
        for rule in &mut next.assignments {
            if rule.source == id {
                rule.source = SYSTEM_SOURCE_ID;
            }
        }
        next.advance_generation()?;
        Ok(next)
    }

    /// Creates a first-run profile. Existing saved profiles are never replaced.
    /// # Panics
    /// Panics if a built-in preset name or input count is invalid.
    #[must_use]
    pub fn first_run() -> Self {
        let mut profile = Self::default();
        for name in ["Game", "Chat", "Music"] {
            profile = profile
                .with_input(crate::InputName::try_new(name).expect("preset name"))
                .expect("preset capacity");
        }
        profile.sources[0].controls = profile.sources[0]
            .controls
            .with_route(MixTarget::Monitor, MixRoute::new(false, FaderGain::UNITY));
        profile.generation = MixerGeneration(0);
        profile
    }

    /// Renames one software input without changing its identity.
    /// # Errors
    /// Returns an error for a missing software input or exhausted generation.
    pub fn renamed(&self, id: SourceId, name: crate::InputName) -> Result<Self, MixerProfileError> {
        let mut next = self.clone();
        let source = next
            .sources
            .iter_mut()
            .find(|s| s.controls.source() == id && s.role == SourceRole::Software)
            .ok_or(MixerProfileError::UnknownSource(id))?;
        name.as_str().clone_into(&mut source.name);
        next.advance_generation()?;
        Ok(next)
    }

    /// Changes display order without changing input identities or routes.
    /// # Errors
    /// Returns an error for an invalid position, absent input, or exhausted generation.
    pub fn moved(&self, id: SourceId, position: usize) -> Result<Self, MixerProfileError> {
        if position >= self.sources.len() {
            return Err(MixerProfileError::InvalidOrder);
        }
        let mut next = self.clone();
        let index = next
            .sources
            .iter()
            .position(|s| s.controls.source() == id)
            .ok_or(MixerProfileError::UnknownSource(id))?;
        let source = next.sources.remove(index);
        next.sources.insert(position, source);
        next.advance_generation()?;
        Ok(next)
    }

    /// Saves or removes one application assignment.
    /// # Errors
    /// Returns an error for an invalid destination, full rule set, or exhausted generation.
    pub fn assigned(
        &self,
        application: crate::ApplicationId,
        source: Option<SourceId>,
    ) -> Result<Self, MixerProfileError> {
        if source.is_some_and(|id| self.source(id).is_none_or(|s| s.role == SourceRole::Microphone))
        {
            return Err(MixerProfileError::InvalidAssignment);
        }
        let mut next = self.clone();
        next.assignments.retain(|rule| rule.application != application);
        if let Some(source) = source {
            next.assignments.push(crate::ApplicationAssignment { application, source });
        }
        next.validate()?;
        next.advance_generation()?;
        Ok(next)
    }

    fn advance_generation(&mut self) -> Result<(), MixerProfileError> {
        self.generation = MixerGeneration(
            self.generation.0.checked_add(1).ok_or(MixerProfileError::GenerationExhausted)?,
        );
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
    SourceIdExhausted,
    InvalidOrder,
    InvalidAssignment,
    SourceCount { actual: usize },
    SourceIdentity { id: SourceId },
    UnknownSource(SourceId),
    GenerationExhausted,
}

impl fmt::Display for MixerProfileError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::SourceIdExhausted => formatter.write_str("input identity space is exhausted"),
            Self::InvalidOrder => formatter.write_str("input position is outside the layout"),
            Self::InvalidAssignment => {
                formatter.write_str("application assignment is invalid or exceeds the limit")
            }
            Self::MicrophoneSource => formatter.write_str("the microphone source must be source 1"),
            Self::SourceCount { actual } => {
                write!(
                    formatter,
                    "the current mixer profile requires {} to {} sources; received {actual}",
                    PRODUCT_SOURCES.len(),
                    crate::MAX_MIXER_SOURCES
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
    Starting,
    NoSingleDevice,
    HostServicesUnavailable,
    DeviceAccessDenied,
    DeviceAdmissionFailed,
    PhysicalNodesExposed,
    AudioAttemptFailed,
    CleanupPending,
}

impl fmt::Display for MixerInactiveReason {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::AudioHostEngineNotConnected => {
                formatter.write_str("audio host and mixer engine are not connected")
            }
            Self::Starting => formatter.write_str("experimental audio startup is pending"),
            Self::NoSingleDevice => formatter.write_str("connect exactly one supported device"),
            Self::HostServicesUnavailable => {
                formatter.write_str("PipeWire and WirePlumber must both be available")
            }
            Self::DeviceAccessDenied => formatter.write_str(
                "USB access denied; check the exact Wave:3 access rule with librewavectl doctor",
            ),
            Self::DeviceAdmissionFailed => formatter.write_str("read-only device admission failed"),
            Self::PhysicalNodesExposed => formatter
                .write_str("the physical Wave card is still owned by the desktop audio service"),
            Self::AudioAttemptFailed => {
                formatter.write_str("audio startup or progress failed; recovery is pending")
            }
            Self::CleanupPending => {
                formatter.write_str("audio cleanup failed; ownership remains retained")
            }
        }
    }
}

/// Whether the desired mixer profile is connected to a live audio runtime.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub enum MixerRuntimeState {
    Inactive { reason: MixerInactiveReason },
    Active,
}

/// Whether live meter observations are present in the snapshot.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub enum MeterAvailability {
    Unavailable {
        reason: MixerInactiveReason,
    },
    /// The graph owns a meter stream. `GetMeters` reports block freshness.
    Available,
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
        Self::inactive_for(profile, MixerInactiveReason::AudioHostEngineNotConnected)
    }

    #[must_use]
    pub fn inactive_for(profile: MixerProfile, reason: MixerInactiveReason) -> Self {
        Self {
            profile,
            runtime: MixerRuntimeState::Inactive { reason },
            meters: MeterAvailability::Unavailable { reason },
        }
    }

    #[must_use]
    pub fn active(profile: MixerProfile) -> Self {
        Self { profile, runtime: MixerRuntimeState::Active, meters: MeterAvailability::Available }
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
    fn active_runtime_exposes_the_meter_stream_without_inventing_values() {
        let snapshot = MixerSnapshot::active(MixerProfile::default());
        assert_eq!(snapshot.runtime, MixerRuntimeState::Active);
        assert_eq!(snapshot.meters, MeterAvailability::Available);
        let encoded = serde_json::to_value(&snapshot).expect("serialize active mixer");
        assert_eq!(encoded["runtime"], json!("Active"));
        assert_eq!(encoded["meters"], json!("Available"));
    }

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
        assert!(serde_json::from_value::<MixerProfile>(swapped).is_ok());

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

#[cfg(test)]
mod input_tests {
    use super::*;
    use crate::InputName;

    #[test]
    fn assignments_follow_identity_and_deleted_inputs_fall_back_to_system() {
        let profile = MixerProfile::first_run();
        let app = crate::ApplicationId::Desktop(
            InputName::try_new("example.player").expect("valid fixture"),
        );
        let source = SourceId::new(3);
        let assigned = profile.assigned(app, Some(source)).expect("valid fixture");
        let edited = assigned
            .renamed(source, InputName::try_new("Games").expect("valid fixture"))
            .expect("valid fixture")
            .moved(source, 0)
            .expect("valid fixture");
        assert_eq!(edited.assignments, assigned.assignments);
        assert_eq!(edited.sources[0].controls.source(), source);
        edited.validate().expect("valid fixture");
        assert!(edited.assigned(app, Some(MICROPHONE_SOURCE_ID)).is_err());
        let removed = edited.without_input(source).expect("valid fixture");
        assert_eq!(removed.assignments[0].source, SYSTEM_SOURCE_ID);
        assert!(removed.assigned(app, None).expect("valid fixture").assignments.is_empty());
        assert!(
            !profile
                .source(MICROPHONE_SOURCE_ID)
                .expect("valid fixture")
                .controls
                .monitor()
                .enabled()
        );
        assert_eq!(
            profile.sources.iter().map(|s| s.name.as_str()).collect::<Vec<_>>(),
            ["Microphone", "System", "Game", "Chat", "Music"]
        );
    }

    #[test]
    fn software_inputs_are_bounded_and_never_reuse_deleted_identities() {
        let mut profile = MixerProfile::default();
        for number in 3..=12 {
            profile = profile
                .with_input(InputName::try_new(&format!("Input {number}")).expect("name"))
                .expect("add");
            assert_eq!(
                profile.sources.last().expect("source").controls.source(),
                SourceId::new(number)
            );
            profile.validate().expect("layout");
        }
        assert!(profile.with_input(InputName::try_new("Overflow").expect("name")).is_err());
        assert_eq!(
            serde_json::from_str::<MixerProfile>(&serde_json::to_string(&profile).expect("encode"))
                .expect("decode"),
            profile
        );
        assert!(profile.without_input(MICROPHONE_SOURCE_ID).is_err());
        assert!(profile.without_input(SYSTEM_SOURCE_ID).is_err());
        let removed = profile.without_input(SourceId::new(5)).expect("remove");
        assert!(removed.source(SourceId::new(5)).is_none());
        let restored = removed
            .with_input(InputName::try_new("New input").expect("name"))
            .expect("new identity");
        assert_eq!(restored.source(SourceId::new(13)).expect("new source").name, "New input");
        restored.validate().expect("valid new identity");
    }
}

#[cfg(test)]
mod output_tests {
    use super::*;
    #[test]
    fn complete_output_format_is_required_and_paired_edits_preserve_mutes() {
        let profile = MixerProfile::default()
            .with_route(SYSTEM_SOURCE_ID, MixTarget::Stream, MixRoute::new(false, FaderGain::MAX))
            .expect("mute");
        let next =
            profile.with_faders(SYSTEM_SOURCE_ID, FaderGain::MIN, FaderGain::MIN).expect("pair");
        assert_eq!(next.generation, MixerGeneration(2));
        let controls = next.source(SYSTEM_SOURCE_ID).expect("source").controls;
        assert!(controls.monitor().enabled());
        assert!(!controls.stream().enabled());
        assert_eq!(controls.monitor().fader(), controls.stream().fader());
        assert_eq!(next.outputs, profile.outputs);
        let value = serde_json::to_value(&next).expect("encode");
        assert_eq!(
            serde_json::from_value::<MixerProfile>(value.clone()).expect("current format"),
            next
        );
        let mut old = value;
        old.as_object_mut().expect("profile object").remove("outputs");
        assert!(serde_json::from_value::<MixerProfile>(old).is_err());
    }
}
