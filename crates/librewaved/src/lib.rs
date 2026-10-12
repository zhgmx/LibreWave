#![doc = "The `LibreWave` user-session daemon and its composition root."]

mod audio;
mod audio_session;
mod control;
mod persistence;

pub use persistence::PersistenceError;
mod manager;

use librewave_core::{
    AudioCardSnapshot, AudioSnapshot, DeviceAdmissionSnapshot, DeviceConnection, DeviceId,
    DeviceSnapshot, ServiceFailureReason, ServiceState, Snapshot,
};
use librewave_platform_linux::{DiscoveryReport, ServiceAvailability, UsbDeviceCandidate};
pub use manager::Daemon;
use manager::DeviceIdAllocator;
use std::collections::BTreeMap;
use std::path::PathBuf;

fn snapshot_from_discovery(
    allocator: &mut DeviceIdAllocator,
    discovery: DiscoveryReport,
    audio: AudioSnapshot,
) -> (Snapshot, BTreeMap<DeviceId, UsbDeviceCandidate>) {
    let mut devices: Vec<_> = discovery
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
            audio,
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
    let signals = librewave_platform_linux::shutdown::ShutdownSignals::install()?;
    let daemon = Daemon::new().map_err(std::io::Error::other)?;
    let daemon = std::cell::RefCell::new(daemon);
    let serving = librewave_platform_linux::ipc::serve_with_tick(
        &socket_path,
        |command| daemon.borrow_mut().handle(command, librewave_core::Origin::Client),
        || daemon.borrow_mut().control_tick(),
        || daemon.borrow_mut().audio_maintenance_tick(),
        |error| eprintln!("librewaved: connection error: {error}"),
        || signals.requested(),
    );
    let cleanup = daemon.borrow_mut().quiesce_for_shutdown();
    match (serving, cleanup) {
        (Err(error), Err(cleanup)) => Err(std::io::Error::other(format!(
            "IPC stopped: {error}; audio cleanup also failed: {cleanup}"
        ))),
        (Err(error), _) | (_, Err(error)) => Err(error),
        (Ok(()), Ok(())) => Ok(()),
    }
}

/// Returns the daemon socket path selected by the environment.
#[must_use]
pub fn socket_path() -> PathBuf {
    librewave_platform_linux::ipc::configured_socket_path()
}

/// Checks the selected profile files without starting a service or opening hardware.
///
/// # Errors
///
/// Returns the exact profile path and validation error. Pending writes are preserved.
pub fn check_config() -> Result<(), PersistenceError> {
    let mut root = manager::profile_state_path("mixer-state.json")?;
    root.pop();
    persistence::check_profiles(&root)
}
