#![doc = "Portable `LibreWave` state, commands, and events."]

use serde::{Deserialize, Serialize};
use std::fmt;

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
/// identifier. It is valid only in the current daemon snapshot.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
pub struct DeviceId(pub u32);

impl fmt::Display for DeviceId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}", self.0)
    }
}

/// The connection state of a managed device.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum DeviceConnection {
    Connected,
}

impl fmt::Display for DeviceConnection {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Connected => formatter.write_str("connected"),
        }
    }
}

/// One ALSA card associated with a supported device.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
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
pub struct DeviceSnapshot {
    /// The daemon-local logical handle.
    pub id: DeviceId,
    /// The admitted product family.
    pub model: DeviceModel,
    /// The current connection state.
    pub connection: DeviceConnection,
    /// Host audio cards correlated with this device.
    pub audio_cards: Vec<AudioCardSnapshot>,
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
pub struct AudioSnapshot {
    /// `PipeWire` registry status.
    pub pipewire: ServiceState,
    /// `WirePlumber` user-service status.
    pub wireplumber: ServiceState,
}

/// An immutable view of the daemon's observed state.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
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
pub enum Command {
    /// Refresh the read-only host inventory.
    Refresh,
    /// Return the current immutable snapshot.
    GetStatus,
    /// Return the devices in the current immutable snapshot.
    ListDevices,
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
}
