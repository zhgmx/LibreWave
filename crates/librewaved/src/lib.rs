#![doc = "The `LibreWave` user-session daemon and its composition root."]

use librewave_core::{
    AudioCardSnapshot, AudioSnapshot, Command, DeviceConnection, DeviceId, DeviceSnapshot, Event,
    ServiceFailureReason, ServiceState, Snapshot, State,
};
use librewave_ipc::{IpcError, Response};
use librewave_platform_linux::{HostAudioInventory, LinuxInventory, ServiceAvailability};
use std::path::PathBuf;

/// Names an intentionally discarded state event at the current no-subscriber boundary.
fn ignore_reconciliation_event(_event: Option<Event>) {}

/// The daemon's read-only composition root.
pub struct Daemon {
    inventory: LinuxInventory,
    paths: librewave_platform_linux::DiscoveryPaths,
    state: State,
}

impl Daemon {
    /// Creates a daemon using the default Linux inventory and paths.
    #[must_use]
    pub fn new() -> Self {
        Self::with_inventory(
            LinuxInventory::new(),
            librewave_platform_linux::DiscoveryPaths::default(),
        )
    }

    /// Creates a daemon with an explicit read-only inventory source.
    #[must_use]
    pub fn with_inventory(
        inventory: LinuxInventory,
        paths: librewave_platform_linux::DiscoveryPaths,
    ) -> Self {
        Self { inventory, paths, state: State::default() }
    }

    /// Creates a daemon with deterministic state for protocol and CLI tests.
    #[must_use]
    pub fn from_snapshot(snapshot: Snapshot) -> Self {
        let mut state = State::default();
        ignore_reconciliation_event(state.replace(snapshot, librewave_core::Origin::Recovery));
        Self {
            inventory: LinuxInventory::new(),
            paths: librewave_platform_linux::DiscoveryPaths::default(),
            state,
        }
    }

    /// Returns the current immutable snapshot.
    #[must_use]
    pub const fn snapshot(&self) -> &Snapshot {
        self.state.snapshot()
    }

    /// Executes one client command through the daemon's single state path.
    ///
    /// # Errors
    ///
    /// Returns an explicit IPC error when the command cannot be represented or completed.
    pub fn handle(
        &mut self,
        command: Command,
        origin: librewave_core::Origin,
    ) -> Result<Response, IpcError> {
        match command {
            Command::Refresh => {
                ignore_reconciliation_event(self.refresh(origin));
                Ok(Response::Refreshed { snapshot: self.snapshot().clone() })
            }
            Command::GetStatus => {
                ignore_reconciliation_event(self.refresh(origin));
                Ok(Response::Status { snapshot: self.snapshot().clone() })
            }
            Command::ListDevices => {
                ignore_reconciliation_event(self.refresh(origin));
                Ok(Response::Devices { devices: self.snapshot().devices.clone() })
            }
        }
    }

    fn refresh(&mut self, origin: librewave_core::Origin) -> Option<Event> {
        let snapshot = snapshot_from_inventory(self.inventory.inspect(&self.paths));
        self.state.replace(snapshot, origin)
    }
}

impl Default for Daemon {
    fn default() -> Self {
        Self::new()
    }
}

fn snapshot_from_inventory(inventory: HostAudioInventory) -> Snapshot {
    let mut devices: Vec<_> = inventory
        .discovery
        .devices
        .into_iter()
        .map(|device| DeviceSnapshot {
            id: DeviceId(0),
            model: device.identity.model(),
            connection: DeviceConnection::Connected,
            audio_cards: device
                .alsa_cards
                .into_iter()
                .map(|card| AudioCardSnapshot { number: card.number, id: card.id, name: card.name })
                .collect(),
        })
        .collect();
    devices.sort_by_key(|device| {
        (device.model, device.audio_cards.first().map_or(u32::MAX, |card| card.number))
    });
    for (index, device) in devices.iter_mut().enumerate() {
        device.id = DeviceId(u32::try_from(index + 1).unwrap_or(u32::MAX));
    }
    Snapshot {
        generation: 0,
        devices,
        audio: AudioSnapshot {
            pipewire: service_state(inventory.services.pipewire),
            wireplumber: service_state(inventory.services.wireplumber),
        },
    }
}

fn service_state(availability: ServiceAvailability) -> ServiceState {
    match availability {
        ServiceAvailability::Available => ServiceState::Available,
        ServiceAvailability::Unavailable(failure) => ServiceState::Unavailable {
            reason: match failure {
                librewave_platform_linux::ServiceFailure::NotInstalled => {
                    ServiceFailureReason::NotInstalled
                }
                librewave_platform_linux::ServiceFailure::PermissionDenied => {
                    ServiceFailureReason::PermissionDenied
                }
                librewave_platform_linux::ServiceFailure::NotRunning => {
                    ServiceFailureReason::NotRunning
                }
                librewave_platform_linux::ServiceFailure::ProbeFailed => {
                    ServiceFailureReason::ProbeFailed
                }
            },
        },
    }
}

/// Runs the daemon loop on its private user-session socket.
///
/// # Errors
///
/// Returns the operating-system error when the daemon socket cannot be created or accepted.
pub fn run(socket_path: impl Into<PathBuf>) -> std::io::Result<()> {
    let socket_path = socket_path.into();
    let mut daemon = Daemon::new();
    librewave_platform_linux::ipc::serve(
        &socket_path,
        |command| daemon.handle(command, librewave_core::Origin::Client),
        |error| eprintln!("librewaved: connection error: {error}"),
    )
}

/// Returns the daemon socket path selected by the environment.
#[must_use]
pub fn socket_path() -> PathBuf {
    std::env::var_os("LIBREWAVE_SOCKET")
        .map_or_else(librewave_platform_linux::ipc::default_socket_path, PathBuf::from)
}

#[cfg(test)]
mod tests {
    use super::*;
    use librewave_core::ServiceFailureReason;

    #[test]
    fn deterministic_snapshot_is_returned_by_status_and_devices() {
        let snapshot = Snapshot {
            generation: 0,
            devices: vec![DeviceSnapshot {
                id: DeviceId(1),
                model: librewave_core::DeviceModel::Wave3,
                connection: DeviceConnection::Connected,
                audio_cards: Vec::new(),
            }],
            audio: AudioSnapshot {
                pipewire: ServiceState::Available,
                wireplumber: ServiceState::Unavailable { reason: ServiceFailureReason::NotRunning },
            },
        };
        let daemon = Daemon::from_snapshot(snapshot);
        assert_eq!(daemon.snapshot().devices.len(), 1);
    }
}
