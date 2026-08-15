#![doc = "The `LibreWave` user-session daemon and its composition root."]

mod control;
mod persistence;

pub use persistence::PersistenceError;
mod manager;

use librewave_core::{
    AudioCardSnapshot, AudioSnapshot, DeviceAdmissionSnapshot, DeviceConnection, DeviceId,
    DeviceSnapshot, ServiceFailureReason, ServiceState, Snapshot,
};
use librewave_platform_linux::{HostAudioInventory, ServiceAvailability, UsbDeviceCandidate};
pub use manager::Daemon;
use manager::DeviceIdAllocator;
use std::collections::BTreeMap;
use std::path::PathBuf;

fn snapshot_from_inventory(
    allocator: &mut DeviceIdAllocator,
    inventory: HostAudioInventory,
) -> (Snapshot, BTreeMap<DeviceId, UsbDeviceCandidate>) {
    let mut devices: Vec<_> = inventory
        .discovery
        .devices
        .into_iter()
        .map(|device| {
            let id = allocator.id_for(&device.topology);
            (
                device.clone(),
                DeviceSnapshot {
                    id,
                    model: device.identity.model(),
                    connection: DeviceConnection::Connected,
                    audio_cards: device
                        .alsa_cards
                        .into_iter()
                        .map(|card| AudioCardSnapshot {
                            number: card.number,
                            id: card.id,
                            name: card.name,
                        })
                        .collect(),
                    admission: DeviceAdmissionSnapshot::not_inspected(),
                },
            )
        })
        .collect();
    devices.sort_by_key(|(_, device)| device.id);
    let mut candidates = BTreeMap::new();
    for (candidate, device) in &mut devices {
        candidates.insert(device.id, candidate.clone());
    }
    let snapshots = devices.into_iter().map(|(_, device)| device).collect();
    (
        Snapshot {
            generation: 0,
            devices: snapshots,
            audio: AudioSnapshot {
                pipewire: service_state(inventory.services.pipewire),
                wireplumber: service_state(inventory.services.wireplumber),
            },
            mixer: librewave_core::MixerSnapshot::default(),
        },
        candidates,
    )
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
    let mut daemon = Daemon::new().map_err(std::io::Error::other)?;
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
