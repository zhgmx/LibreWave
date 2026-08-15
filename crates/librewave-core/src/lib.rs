#![doc = "Portable `LibreWave` state, commands, and events."]

use serde::{Deserialize, Serialize};
use std::fmt;

pub use librewave_protocol::ApiVersion;
pub use librewave_protocol::DeviceModel;

/// A deliberate user-facing audio endpoint.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
pub enum EndpointId {
    /// The managed microphone endpoint.
    Microphone,
    /// The software monitor-mix endpoint.
    MonitorMix,
    /// The software stream-mix endpoint.
    StreamMix,
}

impl EndpointId {
    /// Returns the stable display name for this endpoint.
    #[must_use]
    pub const fn display_name(self) -> &'static str {
        match self {
            Self::Microphone => "Microphone",
            Self::MonitorMix => "Monitor mix",
            Self::StreamMix => "Stream mix",
        }
    }

    /// Returns the explicit desktop-facing flow for this endpoint.
    #[must_use]
    pub const fn flow(self) -> EndpointFlow {
        match self {
            Self::Microphone | Self::MonitorMix | Self::StreamMix => EndpointFlow::PublicSource,
        }
    }
}

/// The direction and flow contract for a user-facing endpoint.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum EndpointFlow {
    /// A public source published to desktop applications.
    PublicSource,
}

/// The current deliberate endpoint set exposed to the desktop.
///
/// Future endpoint flows, such as application-input sinks, must be added as
/// explicit contract entries instead of inferred from endpoint names.
pub const DELIBERATE_ENDPOINTS: &[EndpointId] =
    &[EndpointId::Microphone, EndpointId::MonitorMix, EndpointId::StreamMix];

/// The source of a command or observation.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum Origin {
    /// A request from a `LibreWave` client.
    Client,
    /// A physical control or event from the device.
    Device,
    /// A desktop or operating-system audio event.
    OperatingSystem,
    /// A daemon-owned recovery or reconciliation action.
    Recovery,
}

/// A logical device handle allocated by the daemon.
///
/// The handle is intentionally not a USB serial number or a kernel topology
/// identifier. It is valid only for the lifetime of the current daemon
/// process.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
pub struct DeviceId(pub u32);

impl fmt::Display for DeviceId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}", self.0)
    }
}

/// A daemon-issued revision of one device's observed hardware baseline.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
pub struct DeviceGeneration(pub u64);

impl fmt::Display for DeviceGeneration {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}", self.0)
    }
}

/// The connection state of a managed device.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum DeviceConnection {
    Connected,
    Disconnected,
}

impl fmt::Display for DeviceConnection {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Connected => formatter.write_str("connected"),
            Self::Disconnected => formatter.write_str("disconnected"),
        }
    }
}

/// The result of the read-only control admission probe.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub enum DeviceAdmissionSnapshot {
    /// The device has not been probed in this daemon state.
    NotInspected,
    /// The exact API schema was admitted; the observed configuration can be unavailable.
    Admitted {
        api: ApiVersion,
        generation: DeviceGeneration,
        write_access: DeviceWriteAccess,
        config: Option<Wave3ConfigSnapshot>,
    },
    /// Admission failed without removing the device from inventory.
    Failed { error: AdmissionError },
}

impl DeviceAdmissionSnapshot {
    /// Returns the state before a device has been inspected.
    #[must_use]
    pub const fn not_inspected() -> Self {
        Self::NotInspected
    }

    /// Derives the read-only control access state without storing duplicate state.
    #[must_use]
    pub const fn control_access(&self) -> ControlAccessState {
        match self {
            Self::NotInspected => ControlAccessState::NotInspected,
            Self::Admitted { write_access: DeviceWriteAccess::Ready, .. } => {
                ControlAccessState::Writable
            }
            Self::Admitted {
                write_access:
                    DeviceWriteAccess::Locked { reason: DeviceWriteLockReason::NoOwnedConnection },
                ..
            } => ControlAccessState::ReadOnly,
            Self::Admitted { write_access: DeviceWriteAccess::Locked { .. }, .. } => {
                ControlAccessState::WriteLocked
            }
            Self::Failed { error } => match error {
                AdmissionError::PermissionDenied => ControlAccessState::PermissionDenied,
                AdmissionError::UnsupportedApi { .. } | AdmissionError::MalformedResponse => {
                    ControlAccessState::ReadOnly
                }
                AdmissionError::Disconnected
                | AdmissionError::DescriptorMismatch
                | AdmissionError::InterfaceBusy
                | AdmissionError::TimedOut
                | AdmissionError::Transport => ControlAccessState::Unavailable,
            },
        }
    }

    /// Returns the observed device generation, when admission succeeded.
    #[must_use]
    pub const fn generation(&self) -> Option<DeviceGeneration> {
        match self {
            Self::Admitted { generation, .. } => Some(*generation),
            Self::NotInspected | Self::Failed { .. } => None,
        }
    }

    /// Returns the admitted API version, when admission succeeded.
    #[must_use]
    pub const fn admitted_api(&self) -> Option<ApiVersion> {
        match self {
            Self::Admitted { api, .. } => Some(*api),
            Self::NotInspected | Self::Failed { .. } => None,
        }
    }

    /// Returns the decoded configuration, when admission succeeded.
    #[must_use]
    pub const fn config(&self) -> Option<&Wave3ConfigSnapshot> {
        match self {
            Self::Admitted { config, .. } => config.as_ref(),
            Self::NotInspected | Self::Failed { .. } => None,
        }
    }

    /// Returns the admission error, when admission failed.
    #[must_use]
    pub const fn error(&self) -> Option<&AdmissionError> {
        match self {
            Self::Failed { error } => Some(error),
            Self::NotInspected | Self::Admitted { .. } => None,
        }
    }
}

/// The read-only control access state shown to clients.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum ControlAccessState {
    NotInspected,
    ReadOnly,
    Writable,
    WriteLocked,
    PermissionDenied,
    Unavailable,
}

impl fmt::Display for ControlAccessState {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::NotInspected => "not inspected",
            Self::ReadOnly => "read-only",
            Self::Writable => "writable",
            Self::WriteLocked => "write locked",
            Self::PermissionDenied => "permission denied",
            Self::Unavailable => "unavailable",
        })
    }
}

/// Whether the daemon can start a hardware control transaction.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub enum DeviceWriteAccess {
    Ready,
    Locked { reason: DeviceWriteLockReason },
}

/// Why an admitted device rejects hardware writes.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum DeviceWriteLockReason {
    NoOwnedConnection,
    PersistenceAmbiguous,
    RestoreFailed,
    RestorationUnverified,
}

impl fmt::Display for DeviceWriteLockReason {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::NoOwnedConnection => "the daemon does not own an active connection",
            Self::PersistenceAmbiguous => "desired-state intent or durability is unresolved",
            Self::RestoreFailed => "desired state could not be restored",
            Self::RestorationUnverified => "transaction restoration could not be verified",
        })
    }
}

/// A safe, decoded view of the reviewed Wave:3 configuration message.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
#[allow(clippy::struct_excessive_bools)]
pub struct Wave3ConfigSnapshot {
    pub input_gain: FixedPointValue,
    pub input_mute: bool,
    pub clipguard_enable: bool,
    pub lowcut_enable: bool,
    pub headphone_volume: FixedPointValue,
    pub headphone_mute: bool,
    pub direct_monitor: FixedPointValue,
    pub volume_select: VolumeSelection,
    pub all_leds_off: bool,
    pub leds_flip: bool,
    pub gain_lock: bool,
}

/// One decoded signed fixed-point configuration value.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct FixedPointValue {
    pub raw: i32,
    pub fractional_bits: u8,
}

/// One typed Wave:3 hardware control change.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub enum Wave3Control {
    InputGain(FixedPointValue),
    MicrophoneMute(bool),
    Clipguard(bool),
    LowCut(bool),
    HeadphoneLevel(FixedPointValue),
    HeadphoneMute(bool),
    MonitorMix(FixedPointValue),
    KnobTarget(VolumeSelection),
    AllLedsOff(bool),
    LedsFlip(bool),
    GainLock(bool),
}

/// A reviewed Wave:3 hardware knob target.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum VolumeSelection {
    Microphone,
    Headphone,
    Mix,
}

impl fmt::Display for VolumeSelection {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Microphone => "microphone",
            Self::Headphone => "headphone",
            Self::Mix => "mix",
        })
    }
}

/// A stable, serial-free reason why read-only admission did not complete.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub enum AdmissionError {
    PermissionDenied,
    UnsupportedApi { api: ApiVersion },
    Disconnected,
    DescriptorMismatch,
    MalformedResponse,
    InterfaceBusy,
    TimedOut,
    Transport,
}

impl fmt::Display for AdmissionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::PermissionDenied => formatter.write_str("permission denied"),
            Self::UnsupportedApi { api } => write!(formatter, "unsupported API version {api}"),
            Self::Disconnected => formatter.write_str("device disconnected"),
            Self::DescriptorMismatch => formatter.write_str("USB descriptor mismatch"),
            Self::MalformedResponse => formatter.write_str("malformed device response"),
            Self::InterfaceBusy => formatter.write_str("USB control interface is busy"),
            Self::TimedOut => formatter.write_str("device response timed out"),
            Self::Transport => formatter.write_str("USB transport error"),
        }
    }
}

/// One ALSA card associated with a supported device.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct AudioCardSnapshot {
    /// The kernel-assigned card number.
    pub number: u32,
    /// The short ALSA identifier, when readable.
    pub id: Option<String>,
    /// The human-readable ALSA name, when readable.
    pub name: Option<String>,
}

/// A supported device observed by the daemon.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct DeviceSnapshot {
    /// The daemon-local logical handle.
    pub id: DeviceId,
    /// The admitted product family.
    pub model: DeviceModel,
    /// The current connection state.
    pub connection: DeviceConnection,
    /// Host audio cards correlated with this device.
    pub audio_cards: Vec<AudioCardSnapshot>,
    /// The latest read-only admission result for this device.
    pub admission: DeviceAdmissionSnapshot,
}

/// The result of one host-service status probe.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum ServiceState {
    /// The service responded successfully.
    Available,
    /// The service could not be used for the named reason.
    Unavailable { reason: ServiceFailureReason },
}

impl ServiceState {
    /// Returns the stable human-readable status word.
    #[must_use]
    pub const fn status_word(&self) -> &'static str {
        match self {
            Self::Available => "available",
            Self::Unavailable { .. } => "unavailable",
        }
    }
}

/// A host-service failure that is safe to expose to clients.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum ServiceFailureReason {
    NotInstalled,
    PermissionDenied,
    NotRunning,
    ProbeFailed,
}

impl fmt::Display for ServiceFailureReason {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let text = match self {
            Self::NotInstalled => "not installed",
            Self::PermissionDenied => "permission denied",
            Self::NotRunning => "not running",
            Self::ProbeFailed => "probe failed",
        };
        formatter.write_str(text)
    }
}

/// The host audio portion of a daemon snapshot.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct AudioSnapshot {
    /// `PipeWire` registry status.
    pub pipewire: ServiceState,
    /// `WirePlumber` user-service status.
    pub wireplumber: ServiceState,
}

/// An immutable view of the daemon's observed state.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Snapshot {
    /// Monotonically increasing state revision.
    pub generation: u64,
    /// Supported devices in deterministic logical-ID order.
    pub devices: Vec<DeviceSnapshot>,
    /// Host audio service status.
    pub audio: AudioSnapshot,
}

impl Snapshot {
    /// Creates the empty initial state.
    #[must_use]
    pub fn empty() -> Self {
        Self {
            generation: 0,
            devices: Vec::new(),
            audio: AudioSnapshot {
                pipewire: ServiceState::Unavailable { reason: ServiceFailureReason::NotRunning },
                wireplumber: ServiceState::Unavailable { reason: ServiceFailureReason::NotRunning },
            },
        }
    }
}

/// Commands understood by the daemon.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub enum Command {
    /// Refresh the host inventory and retained hardware configurations.
    Refresh,
    /// Refresh the host inventory and return the snapshot without polling retained hardware.
    GetStatus,
    /// Refresh the host inventory and return its devices without polling retained hardware.
    ListDevices,
    /// Refresh one retained configuration, or open and admit a connection when needed.
    InspectDevice { id: DeviceId },
    /// Change one reviewed Wave:3 hardware control through the daemon-owned connection.
    SetWave3Control { id: DeviceId, expected_generation: DeviceGeneration, control: Wave3Control },
}

/// Events emitted when daemon state changes.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum Event {
    /// A complete replacement snapshot was observed.
    SnapshotUpdated { origin: Origin, snapshot: Snapshot },
}

/// The daemon's single in-memory state owner.
#[derive(Clone, Debug)]
pub struct State {
    snapshot: Snapshot,
}

impl Default for State {
    fn default() -> Self {
        Self { snapshot: Snapshot::empty() }
    }
}

impl State {
    /// Returns the current immutable snapshot.
    #[must_use]
    pub const fn snapshot(&self) -> &Snapshot {
        &self.snapshot
    }

    /// Reconciles the observed state and records an event only when it changes.
    pub fn replace(&mut self, mut snapshot: Snapshot, origin: Origin) -> Option<Event> {
        if self.snapshot.devices == snapshot.devices && self.snapshot.audio == snapshot.audio {
            return None;
        }
        snapshot.generation = self.snapshot.generation.saturating_add(1);
        self.snapshot = snapshot.clone();
        Some(Event::SnapshotUpdated { origin, snapshot })
    }

    /// Replaces one device's connection and admission result atomically.
    ///
    /// # Errors
    ///
    /// Returns [`DeviceUpdateError::DeviceNotFound`] when the logical ID is absent.
    pub fn update_device_inspection(
        &mut self,
        id: DeviceId,
        connection: DeviceConnection,
        admission: DeviceAdmissionSnapshot,
        origin: Origin,
    ) -> Result<Option<Event>, DeviceUpdateError> {
        let mut snapshot = self.snapshot.clone();
        let device = snapshot
            .devices
            .iter_mut()
            .find(|device| device.id == id)
            .ok_or(DeviceUpdateError::DeviceNotFound { id })?;
        device.connection = connection;
        device.admission = admission;
        Ok(self.replace(snapshot, origin))
    }
}

/// A state update could not find the requested logical device.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DeviceUpdateError {
    DeviceNotFound { id: DeviceId },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn state_replaces_complete_snapshots_and_advances_generation() {
        let mut state = State::default();
        let mut snapshot = Snapshot::empty();
        snapshot.devices.push(DeviceSnapshot {
            id: DeviceId(1),
            model: DeviceModel::Wave3,
            connection: DeviceConnection::Connected,
            audio_cards: vec![AudioCardSnapshot {
                number: 7,
                id: Some("Wave3".to_owned()),
                name: Some("Elgato Wave:3".to_owned()),
            }],
            admission: DeviceAdmissionSnapshot::not_inspected(),
        });

        let event = state.replace(snapshot, Origin::Recovery);
        assert_eq!(state.snapshot().generation, 1);
        assert_eq!(state.snapshot().devices.len(), 1);
        assert_eq!(
            event,
            Some(Event::SnapshotUpdated {
                origin: Origin::Recovery,
                snapshot: state.snapshot().clone(),
            })
        );
    }

    #[test]
    fn unchanged_state_does_not_advance_generation_or_emit_event() {
        let mut state = State::default();
        let snapshot = Snapshot::empty();
        assert_eq!(state.replace(snapshot.clone(), Origin::Recovery), None);
        assert_eq!(state.snapshot().generation, 0);
        assert_eq!(state.replace(snapshot, Origin::Client), None);
        assert_eq!(state.snapshot().generation, 0);
    }

    #[test]
    fn deliberate_endpoint_set_is_stable() {
        assert_eq!(
            DELIBERATE_ENDPOINTS,
            &[EndpointId::Microphone, EndpointId::MonitorMix, EndpointId::StreamMix]
        );
        assert_eq!(EndpointId::MonitorMix.display_name(), "Monitor mix");
        assert_eq!(EndpointId::Microphone.flow(), EndpointFlow::PublicSource);
        assert_eq!(EndpointId::MonitorMix.flow(), EndpointFlow::PublicSource);
        assert_eq!(EndpointId::StreamMix.flow(), EndpointFlow::PublicSource);
    }

    #[test]
    fn admission_changes_advance_generation_without_removing_the_device() {
        let mut state = State::default();
        let mut snapshot = Snapshot::empty();
        snapshot.devices.push(DeviceSnapshot {
            id: DeviceId(1),
            model: DeviceModel::Wave3,
            connection: DeviceConnection::Connected,
            audio_cards: Vec::new(),
            admission: DeviceAdmissionSnapshot::not_inspected(),
        });
        state.replace(snapshot, Origin::Recovery);

        let admission = DeviceAdmissionSnapshot::Failed { error: AdmissionError::Disconnected };
        assert!(
            state
                .update_device_inspection(
                    DeviceId(1),
                    DeviceConnection::Disconnected,
                    admission.clone(),
                    Origin::Client,
                )
                .expect("device exists")
                .is_some()
        );
        assert_eq!(state.snapshot().generation, 2);
        assert_eq!(state.snapshot().devices.len(), 1);
        assert_eq!(state.snapshot().devices[0].admission, admission);
        assert_eq!(
            state.snapshot().devices[0].admission.control_access(),
            ControlAccessState::Unavailable
        );
        assert!(
            state
                .update_device_inspection(
                    DeviceId(1),
                    DeviceConnection::Disconnected,
                    admission,
                    Origin::Client,
                )
                .expect("device exists")
                .is_none()
        );
        assert_eq!(state.snapshot().generation, 2);

        let denied = DeviceAdmissionSnapshot::Failed { error: AdmissionError::PermissionDenied };
        state
            .update_device_inspection(
                DeviceId(1),
                DeviceConnection::Connected,
                denied.clone(),
                Origin::Client,
            )
            .expect("device exists");
        assert_eq!(state.snapshot().devices[0].connection, DeviceConnection::Connected);
        assert_eq!(state.snapshot().devices[0].admission, denied);
        assert_eq!(
            state.snapshot().devices[0].admission.control_access(),
            ControlAccessState::PermissionDenied
        );
    }

    #[test]
    fn admission_serde_accepts_only_the_three_state_shape() {
        let old_shape = r#"{
            "control_access":"ReadOnly",
            "admitted_api":{"major":5,"minor":4},
            "config":null,
            "error":null
        }"#;
        assert!(serde_json::from_str::<DeviceAdmissionSnapshot>(old_shape).is_err());

        let unknown_state = r#"{"Paused":{}}"#;
        assert!(serde_json::from_str::<DeviceAdmissionSnapshot>(unknown_state).is_err());

        let unknown_api_field = r#"{
            "Admitted": {
                "api":{"major":5,"minor":4,"extra":true},
                "config":{
                    "input_gain":{"raw":0,"fractional_bits":8},
                    "input_mute":false,
                    "clipguard_enable":false,
                    "lowcut_enable":false,
                    "headphone_volume":{"raw":0,"fractional_bits":8},
                    "headphone_mute":false,
                    "direct_monitor":{"raw":0,"fractional_bits":8},
                    "volume_select":"Microphone",
                    "all_leds_off":false,
                    "leds_flip":false,
                    "gain_lock":false
                }
            }
        }"#;
        assert!(serde_json::from_str::<DeviceAdmissionSnapshot>(unknown_api_field).is_err());
    }
}
