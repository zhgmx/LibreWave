#![doc = "The `LibreWave` user-session daemon and its composition root."]

use librewave_core::{
    AudioCardSnapshot, AudioSnapshot, Command, DeviceAdmissionSnapshot, DeviceConnection, DeviceId,
    DeviceSnapshot, Event, ServiceFailureReason, ServiceState, Snapshot, State,
};
use librewave_ipc::{IpcError, Response};
use librewave_platform_linux::{
    HostAudioInventory, LinuxInventory, ServiceAvailability, UsbDeviceCandidate, UsbTopology,
    inspect_wave3_usb,
};
use std::collections::BTreeMap;
use std::path::PathBuf;

/// Names an intentionally discarded state event at the current no-subscriber boundary.
fn ignore_reconciliation_event(_event: Option<Event>) {}

/// The daemon's read-only composition root.
pub struct Daemon {
    inventory: LinuxInventory,
    paths: librewave_platform_linux::DiscoveryPaths,
    state: State,
    candidates: BTreeMap<DeviceId, UsbDeviceCandidate>,
    inspector: Box<dyn FnMut(&UsbDeviceCandidate) -> DeviceAdmissionSnapshot>,
    allocator: DeviceIdAllocator,
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
        Self {
            inventory,
            paths,
            state: State::default(),
            candidates: BTreeMap::new(),
            inspector: Box::new(inspect_wave3_usb),
            allocator: DeviceIdAllocator::default(),
        }
    }

    /// Creates a daemon with deterministic state for protocol and CLI tests.
    #[must_use]
    pub fn from_snapshot(snapshot: Snapshot) -> Self {
        let mut state = State::default();
        ignore_reconciliation_event(state.replace(snapshot, librewave_core::Origin::Recovery));
        let allocator = DeviceIdAllocator::from_snapshot(state.snapshot());
        Self {
            inventory: LinuxInventory::new(),
            paths: librewave_platform_linux::DiscoveryPaths::default(),
            state,
            candidates: BTreeMap::new(),
            inspector: Box::new(inspect_wave3_usb),
            allocator,
        }
    }

    #[cfg(test)]
    #[must_use]
    fn with_inspector(
        snapshot: Snapshot,
        candidates: BTreeMap<DeviceId, UsbDeviceCandidate>,
        inspector: impl FnMut(&UsbDeviceCandidate) -> DeviceAdmissionSnapshot + 'static,
    ) -> Self {
        let mut daemon = Self::from_snapshot(snapshot);
        daemon.candidates = candidates;
        daemon.inspector = Box::new(inspector);
        daemon
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
            Command::InspectDevice { id } => {
                let current_connection = self
                    .snapshot()
                    .devices
                    .iter()
                    .find(|device| device.id == id)
                    .map(|device| device.connection)
                    .ok_or_else(|| IpcError::device_not_found(id))?;
                let candidate = self
                    .candidates
                    .get(&id)
                    .cloned()
                    .ok_or_else(|| IpcError::device_not_found(id))?;
                let admission = (self.inspector)(&candidate);
                let connection = if matches!(
                    &admission,
                    DeviceAdmissionSnapshot::Failed {
                        error: librewave_core::AdmissionError::Disconnected
                    }
                ) {
                    DeviceConnection::Disconnected
                } else if matches!(&admission, DeviceAdmissionSnapshot::Admitted { .. }) {
                    DeviceConnection::Connected
                } else {
                    current_connection
                };
                ignore_reconciliation_event(
                    self.state
                        .update_device_inspection(id, connection, admission, origin)
                        .map_err(|_| IpcError::device_not_found(id))?,
                );
                let device = self
                    .snapshot()
                    .devices
                    .iter()
                    .find(|device| device.id == id)
                    .cloned()
                    .ok_or_else(|| IpcError::device_not_found(id))?;
                Ok(Response::DeviceInspection { device })
            }
        }
    }

    fn refresh(&mut self, origin: librewave_core::Origin) -> Option<Event> {
        let (mut snapshot, candidates) =
            self.snapshot_from_inventory(self.inventory.inspect(&self.paths));
        for device in &mut snapshot.devices {
            if let Some(previous) =
                self.snapshot().devices.iter().find(|previous| previous.id == device.id)
            {
                device.admission = previous.admission.clone();
            }
        }
        self.candidates = candidates;
        self.state.replace(snapshot, origin)
    }

    fn snapshot_from_inventory(
        &mut self,
        inventory: HostAudioInventory,
    ) -> (Snapshot, BTreeMap<DeviceId, UsbDeviceCandidate>) {
        snapshot_from_inventory(&mut self.allocator, inventory)
    }
}

impl Default for Daemon {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Default)]
struct DeviceIdAllocator {
    ids_by_topology: BTreeMap<UsbTopology, DeviceId>,
    next_id: u64,
}

impl DeviceIdAllocator {
    fn from_snapshot(snapshot: &Snapshot) -> Self {
        let next_id =
            snapshot.devices.iter().map(|device| u64::from(device.id.0)).max().unwrap_or(0);
        Self { ids_by_topology: BTreeMap::new(), next_id }
    }

    fn id_for(&mut self, topology: &UsbTopology) -> DeviceId {
        if let Some(id) = self.ids_by_topology.get(topology) {
            return *id;
        }
        let next_id = self.next_id.checked_add(1).expect("LibreWave device ID space exhausted");
        let id = DeviceId(
            u32::try_from(next_id).expect("LibreWave device ID space exhausted before allocation"),
        );
        self.next_id = next_id;
        self.ids_by_topology.insert(topology.clone(), id);
        id
    }
}

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
    use std::collections::VecDeque;
    use std::fs;
    use std::os::unix::fs::symlink;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn deterministic_snapshot_is_returned_by_status_and_devices() {
        let snapshot = Snapshot {
            generation: 0,
            devices: vec![DeviceSnapshot {
                id: DeviceId(1),
                model: librewave_core::DeviceModel::Wave3,
                connection: DeviceConnection::Connected,
                audio_cards: Vec::new(),
                admission: DeviceAdmissionSnapshot::not_inspected(),
            }],
            audio: AudioSnapshot {
                pipewire: ServiceState::Available,
                wireplumber: ServiceState::Unavailable { reason: ServiceFailureReason::NotRunning },
            },
        };
        let daemon = Daemon::from_snapshot(snapshot);
        assert_eq!(daemon.snapshot().devices.len(), 1);
    }

    #[test]
    fn from_snapshot_seeds_allocator_above_existing_ids() {
        let fixture = discovery_fixture();
        let candidate = LinuxInventory::new()
            .discover(&fixture.paths)
            .devices
            .into_iter()
            .next()
            .expect("fixture candidate");
        let snapshot = Snapshot {
            generation: 0,
            devices: vec![DeviceSnapshot {
                id: DeviceId(9),
                model: librewave_core::DeviceModel::Wave3,
                connection: DeviceConnection::Connected,
                audio_cards: Vec::new(),
                admission: DeviceAdmissionSnapshot::not_inspected(),
            }],
            audio: AudioSnapshot {
                pipewire: ServiceState::Available,
                wireplumber: ServiceState::Available,
            },
        };
        let mut daemon = Daemon::from_snapshot(snapshot);
        let (refreshed, _) = daemon.snapshot_from_inventory(HostAudioInventory {
            discovery: librewave_platform_linux::DiscoveryReport {
                devices: vec![candidate],
                failures: Vec::new(),
            },
            services: librewave_platform_linux::HostAudioServices {
                pipewire: ServiceAvailability::Available,
                wireplumber: ServiceAvailability::Available,
            },
        });
        assert_eq!(refreshed.devices[0].id, DeviceId(10));
    }

    #[test]
    fn listed_candidate_survives_disconnect_and_can_reconnect_without_refresh() {
        let fixture = discovery_fixture();
        let candidate = LinuxInventory::new()
            .discover(&fixture.paths)
            .devices
            .into_iter()
            .next()
            .expect("fixture candidate");
        let snapshot = Snapshot {
            generation: 0,
            devices: vec![DeviceSnapshot {
                id: DeviceId(1),
                model: librewave_core::DeviceModel::Wave3,
                connection: DeviceConnection::Connected,
                audio_cards: Vec::new(),
                admission: DeviceAdmissionSnapshot::not_inspected(),
            }],
            audio: AudioSnapshot {
                pipewire: ServiceState::Available,
                wireplumber: ServiceState::Available,
            },
        };
        let admitted = DeviceAdmissionSnapshot::Admitted {
            api: librewave_core::ApiVersion::new(5, 4),
            config: librewave_core::Wave3ConfigSnapshot {
                input_gain: librewave_core::FixedPointValue { raw: 0, fractional_bits: 8 },
                input_mute: false,
                clipguard_enable: false,
                lowcut_enable: false,
                headphone_volume: librewave_core::FixedPointValue { raw: 0, fractional_bits: 8 },
                headphone_mute: false,
                direct_monitor: librewave_core::FixedPointValue { raw: 0, fractional_bits: 8 },
                volume_select: librewave_core::VolumeSelection::Microphone,
                all_leds_off: false,
                leds_flip: false,
                gain_lock: false,
            },
        };
        let mut results = VecDeque::from([
            DeviceAdmissionSnapshot::Failed { error: librewave_core::AdmissionError::Disconnected },
            admitted.clone(),
        ]);
        let daemon_snapshot = snapshot.clone();
        let mut daemon = Daemon::with_inspector(
            daemon_snapshot,
            BTreeMap::from([(DeviceId(1), candidate)]),
            move |candidate| {
                assert_eq!(candidate.topology.as_str(), "1-8.3");
                results.pop_front().expect("fake inspection result")
            },
        );

        let first = daemon
            .handle(Command::InspectDevice { id: DeviceId(1) }, librewave_core::Origin::Client)
            .expect("disconnected inspection");
        let Response::DeviceInspection { device } = first else { panic!("unexpected response") };
        assert_eq!(device.connection, DeviceConnection::Disconnected);
        assert!(matches!(
            device.admission,
            DeviceAdmissionSnapshot::Failed { error: librewave_core::AdmissionError::Disconnected }
        ));
        assert_eq!(daemon.snapshot().devices.len(), 1);
        assert_eq!(daemon.snapshot().generation, 2);

        let second = daemon
            .handle(Command::InspectDevice { id: DeviceId(1) }, librewave_core::Origin::Client)
            .expect("reconnected inspection");
        let Response::DeviceInspection { device } = second else { panic!("unexpected response") };
        assert_eq!(device.connection, DeviceConnection::Connected);
        assert_eq!(device.admission, admitted);
        assert_eq!(daemon.snapshot().generation, 3);

        let mut inconsistent = Daemon::with_inspector(snapshot, BTreeMap::new(), |_| {
            panic!("the inspector must not run for an inconsistent map")
        });
        let error = inconsistent
            .handle(Command::InspectDevice { id: DeviceId(1) }, librewave_core::Origin::Client)
            .expect_err("missing candidate");
        assert!(matches!(
            error.kind,
            librewave_ipc::IpcErrorKind::DeviceNotFound { id: DeviceId(1) }
        ));
    }

    struct DiscoveryFixture {
        root: std::path::PathBuf,
        paths: librewave_platform_linux::DiscoveryPaths,
    }

    impl Drop for DiscoveryFixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    fn discovery_fixture() -> DiscoveryFixture {
        let nonce =
            SystemTime::now().duration_since(UNIX_EPOCH).expect("clock before epoch").as_nanos();
        let root = std::env::temp_dir().join(format!("librewave-daemon-fixture-{nonce}"));
        let usb_target = root.join("sys/devices/pci/usb1/1-8.3");
        let usb_bus = root.join("sys/bus/usb/devices");
        let sound_cards = root.join("sys/class/sound");
        let asound_root = root.join("proc/asound");
        fs::create_dir_all(&usb_target).expect("create USB fixture");
        fs::create_dir_all(&usb_bus).expect("create USB bus fixture");
        fs::create_dir_all(&sound_cards).expect("create sound fixture");
        fs::create_dir_all(&asound_root).expect("create ALSA fixture");
        fs::write(usb_target.join("idVendor"), "0fd9\n").expect("write vendor");
        fs::write(usb_target.join("idProduct"), "0070\n").expect("write product");
        symlink(&usb_target, usb_bus.join("1-8.3")).expect("link USB fixture");
        let asound_cards = root.join("proc/asound/cards");
        fs::write(&asound_cards, "").expect("write ALSA cards");
        DiscoveryFixture {
            root: root.clone(),
            paths: librewave_platform_linux::DiscoveryPaths {
                usb_devices: usb_bus,
                sound_cards,
                asound_cards,
                asound_root,
            },
        }
    }

    #[test]
    fn stable_ids_survive_card_order_swaps_and_topology_reconnects() {
        let (fixture, usb_bus, usb_a, usb_c, card7_link, card8_link) = two_device_fixture();
        let mut daemon = Daemon::with_inventory(LinuxInventory::new(), fixture.paths.clone());
        daemon.refresh(librewave_core::Origin::Recovery);
        let id_a = daemon
            .candidates
            .iter()
            .find(|(_, candidate)| candidate.topology.as_str() == "1-8.2")
            .map(|(id, _)| *id)
            .expect("topology A ID");
        let id_b = daemon
            .candidates
            .iter()
            .find(|(_, candidate)| candidate.topology.as_str() == "1-8.3")
            .map(|(id, _)| *id)
            .expect("topology B ID");
        assert_ne!(id_a, id_b);
        daemon
            .state
            .update_device_inspection(
                id_a,
                DeviceConnection::Connected,
                DeviceAdmissionSnapshot::Failed {
                    error: librewave_core::AdmissionError::PermissionDenied,
                },
                librewave_core::Origin::Client,
            )
            .expect("topology A exists");

        fs::remove_file(&card7_link).expect("remove card 7 link");
        fs::remove_file(&card8_link).expect("remove card 8 link");
        symlink(fixture.root.join("sys/devices/pci/usb1/1-8.3/1-8.3:1.0/sound/card7"), &card7_link)
            .expect("swap card 7 link");
        symlink(fixture.root.join("sys/devices/pci/usb1/1-8.2/1-8.2:1.0/sound/card8"), &card8_link)
            .expect("swap card 8 link");
        daemon.refresh(librewave_core::Origin::Recovery);

        assert_eq!(
            daemon.candidates.get(&id_a).expect("topology A candidate").alsa_cards[0].number,
            8
        );
        assert_eq!(
            daemon.candidates.get(&id_b).expect("topology B candidate").alsa_cards[0].number,
            7
        );
        assert!(matches!(
            daemon
                .snapshot()
                .devices
                .iter()
                .find(|device| device.id == id_a)
                .expect("topology A snapshot")
                .admission,
            DeviceAdmissionSnapshot::Failed {
                error: librewave_core::AdmissionError::PermissionDenied
            }
        ));
        let ids: Vec<_> = daemon.snapshot().devices.iter().map(|device| device.id).collect();
        let mut expected_ids = vec![id_a, id_b];
        expected_ids.sort_unstable();
        assert_eq!(ids, expected_ids);
        assert!(ids.windows(2).all(|pair| pair[0] < pair[1]));

        fs::remove_file(usb_bus.join("1-8.2")).expect("disconnect topology A");
        daemon.refresh(librewave_core::Origin::Recovery);
        assert_eq!(daemon.snapshot().devices.len(), 1);
        assert!(!daemon.candidates.contains_key(&id_a));

        symlink(&usb_a, usb_bus.join("1-8.2")).expect("reconnect topology A");
        daemon.refresh(librewave_core::Origin::Recovery);
        assert_eq!(
            daemon
                .candidates
                .iter()
                .find(|(_, candidate)| candidate.topology.as_str() == "1-8.2")
                .map(|(id, _)| *id),
            Some(id_a)
        );

        let new_topology = usb_bus.join("1-8.4");
        symlink(&usb_c, &new_topology).expect("observe new topology");
        daemon.refresh(librewave_core::Origin::Recovery);
        let id_c = daemon
            .candidates
            .iter()
            .find(|(_, candidate)| candidate.topology.as_str() == "1-8.4")
            .map(|(id, _)| *id)
            .expect("topology C ID");
        assert_ne!(id_c, id_a);
        assert_ne!(id_c, id_b);
        assert!(id_c > id_a && id_c > id_b);
    }

    fn two_device_fixture() -> (
        DiscoveryFixture,
        std::path::PathBuf,
        std::path::PathBuf,
        std::path::PathBuf,
        std::path::PathBuf,
        std::path::PathBuf,
    ) {
        let nonce =
            SystemTime::now().duration_since(UNIX_EPOCH).expect("clock before epoch").as_nanos();
        let root = std::env::temp_dir().join(format!("librewave-two-device-{nonce}"));
        let usb_bus = root.join("sys/bus/usb/devices");
        let usb_a = root.join("sys/devices/pci/usb1/1-8.2");
        let usb_b = root.join("sys/devices/pci/usb1/1-8.3");
        let usb_c = root.join("sys/devices/pci/usb1/1-8.4");
        let sound_cards = root.join("sys/class/sound");
        let asound_root = root.join("proc/asound");
        for path in [&usb_a, &usb_b, &usb_c, &sound_cards, &asound_root, &usb_bus] {
            fs::create_dir_all(path).expect("create two-device fixture");
        }
        for (topology, usb) in [("1-8.2", &usb_a), ("1-8.3", &usb_b), ("1-8.4", &usb_c)] {
            fs::write(usb.join("idVendor"), "0fd9\n").expect("write vendor");
            fs::write(usb.join("idProduct"), "0070\n").expect("write product");
            fs::create_dir_all(usb.join(format!("{topology}:1.0/sound/card7")))
                .expect("create card 7 target");
            fs::create_dir_all(usb.join(format!("{topology}:1.0/sound/card8")))
                .expect("create card 8 target");
            if topology != "1-8.4" {
                symlink(usb, usb_bus.join(topology)).expect("link USB device");
            }
        }
        let card7 = root.join("sys/class/sound/card7");
        let card8 = root.join("sys/class/sound/card8");
        fs::create_dir_all(&card7).expect("create card 7");
        fs::create_dir_all(&card8).expect("create card 8");
        let card7_link = card7.join("device");
        let card8_link = card8.join("device");
        symlink(usb_a.join("1-8.2:1.0/sound/card7"), &card7_link).expect("link card 7");
        symlink(usb_b.join("1-8.3:1.0/sound/card8"), &card8_link).expect("link card 8");
        fs::create_dir_all(asound_root.join("card7")).expect("create ALSA card 7");
        fs::create_dir_all(asound_root.join("card8")).expect("create ALSA card 8");
        fs::write(asound_root.join("card7/id"), "Wave3A\n").expect("write card 7 ID");
        fs::write(asound_root.join("card8/id"), "Wave3B\n").expect("write card 8 ID");
        let asound_cards = root.join("proc/asound/cards");
        fs::write(
            &asound_cards,
            " 7 [Wave3A] - USB-Audio - Elgato Wave:3\n 8 [Wave3B] - USB-Audio - Elgato Wave:3\n",
        )
        .expect("write ALSA summary");
        let fixture = DiscoveryFixture {
            root: root.clone(),
            paths: librewave_platform_linux::DiscoveryPaths {
                usb_devices: usb_bus.clone(),
                sound_cards,
                asound_cards,
                asound_root,
            },
        };
        (fixture, usb_bus, usb_a, usb_c, card7_link, card8_link)
    }
}
