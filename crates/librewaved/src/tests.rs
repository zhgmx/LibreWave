use super::*;
use crate::PersistenceError;
use librewave_core::{AudioSnapshot, FaderGain, ServiceFailureReason, ServiceState};
use librewave_platform_linux::ServiceAvailability;
use std::cell::RefCell;
use std::collections::VecDeque;
use std::fs;
use std::os::unix::fs::symlink;
use std::rc::Rc;
use std::time::{SystemTime, UNIX_EPOCH};

struct FakeConnection {
    config: Wave3ConfigSnapshot,
    outcomes: VecDeque<ConnectionOutcome>,
}

impl ManagedWave3Connection for FakeConnection {
    fn api(&self) -> librewave_core::ApiVersion {
        librewave_core::ApiVersion::new(5, 4)
    }

    fn observed(&self) -> Result<Wave3ConfigSnapshot, String> {
        Ok(self.config.clone())
    }

    fn refresh(&mut self) -> ConnectionRefreshOutcome {
        ConnectionRefreshOutcome::Unchanged(self.config.clone())
    }

    fn apply(&mut self, _control: Wave3Control) -> ConnectionOutcome {
        let outcome = self
            .outcomes
            .pop_front()
            .unwrap_or_else(|| ConnectionOutcome::Unchanged(self.config.clone()));
        match &outcome {
            ConnectionOutcome::Applied(config)
            | ConnectionOutcome::Unchanged(config)
            | ConnectionOutcome::StaleBaseline(config) => self.config = config.clone(),
            ConnectionOutcome::Failed { observed: Some(config), .. } => {
                self.config = config.clone();
            }
            ConnectionOutcome::Invalid(_)
            | ConnectionOutcome::Disconnected
            | ConnectionOutcome::Failed { observed: None, .. } => {}
        }
        outcome
    }
}

fn wave3_config() -> Wave3ConfigSnapshot {
    Wave3ConfigSnapshot {
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
    }
}

#[derive(Clone, Copy)]
enum SaveAction {
    Ok,
    Before,
    After,
}

struct FakeStore {
    state: Rc<RefCell<DesiredState>>,
    actions: Rc<RefCell<VecDeque<SaveAction>>>,
    saves: Rc<RefCell<usize>>,
}

struct FakeMixerStore {
    profile: Rc<RefCell<MixerProfile>>,
    actions: Rc<RefCell<VecDeque<SaveAction>>>,
    saves: Rc<RefCell<usize>>,
}

impl MixerStateStore for FakeMixerStore {
    fn load(&mut self) -> Result<MixerProfile, PersistenceError> {
        Ok(self.profile.borrow().clone())
    }

    fn save(&mut self, profile: &MixerProfile) -> Result<(), PersistenceError> {
        *self.saves.borrow_mut() += 1;
        match self.actions.borrow_mut().pop_front().unwrap_or(SaveAction::Ok) {
            SaveAction::Ok => {
                *self.profile.borrow_mut() = profile.clone();
                Ok(())
            }
            SaveAction::Before => Err(PersistenceError::SaveBeforeRename(std::io::Error::other(
                "injected mixer save failure",
            ))),
            SaveAction::After => {
                *self.profile.borrow_mut() = profile.clone();
                Err(PersistenceError::SaveAfterRename(std::io::Error::other(
                    "injected mixer directory sync failure",
                )))
            }
        }
    }
}

fn mixer_daemon(
    actions: Vec<SaveAction>,
) -> (Daemon, Rc<RefCell<MixerProfile>>, Rc<RefCell<usize>>) {
    let profile = Rc::new(RefCell::new(MixerProfile::default()));
    let saves = Rc::new(RefCell::new(0));
    let store = FakeMixerStore {
        profile: Rc::clone(&profile),
        actions: Rc::new(RefCell::new(VecDeque::from(actions))),
        saves: Rc::clone(&saves),
    };
    let daemon = Daemon::with_mixer_store(Snapshot::empty(), Box::new(store));
    (daemon, profile, saves)
}

impl DesiredStateStore for FakeStore {
    fn load(&mut self) -> Result<DesiredState, PersistenceError> {
        Ok(self.state.borrow().clone())
    }

    fn save(&mut self, state: &DesiredState) -> Result<(), PersistenceError> {
        *self.saves.borrow_mut() += 1;
        match self.actions.borrow_mut().pop_front().unwrap_or(SaveAction::Ok) {
            SaveAction::Ok => {
                *self.state.borrow_mut() = state.clone();
                Ok(())
            }
            SaveAction::Before => Err(PersistenceError::SaveBeforeRename(std::io::Error::other(
                "injected save failure",
            ))),
            SaveAction::After => Err(PersistenceError::SaveAfterRename({
                *self.state.borrow_mut() = state.clone();
                std::io::Error::other("injected directory sync failure")
            })),
        }
    }
}

struct SharedConnection {
    config: Wave3ConfigSnapshot,
    refreshes: Rc<RefCell<VecDeque<ConnectionRefreshOutcome>>>,
    outcomes: Rc<RefCell<VecDeque<ConnectionOutcome>>>,
    calls: Rc<RefCell<Vec<Wave3Control>>>,
}

impl ManagedWave3Connection for SharedConnection {
    fn api(&self) -> librewave_core::ApiVersion {
        librewave_core::ApiVersion::new(5, 4)
    }

    fn observed(&self) -> Result<Wave3ConfigSnapshot, String> {
        Ok(self.config.clone())
    }

    fn refresh(&mut self) -> ConnectionRefreshOutcome {
        let outcome = self
            .refreshes
            .borrow_mut()
            .pop_front()
            .unwrap_or_else(|| ConnectionRefreshOutcome::Unchanged(self.config.clone()));
        match &outcome {
            ConnectionRefreshOutcome::Changed(config)
            | ConnectionRefreshOutcome::Unchanged(config) => self.config = config.clone(),
            ConnectionRefreshOutcome::Failed(_) => {}
        }
        outcome
    }

    fn apply(&mut self, control: Wave3Control) -> ConnectionOutcome {
        self.calls.borrow_mut().push(control);
        let outcome = self
            .outcomes
            .borrow_mut()
            .pop_front()
            .unwrap_or_else(|| ConnectionOutcome::Unchanged(self.config.clone()));
        if let Some(config) = outcome_observed(&outcome) {
            self.config = config;
        }
        outcome
    }
}

struct Harness {
    daemon: Daemon,
    calls: Rc<RefCell<Vec<Wave3Control>>>,
    connections: Rc<RefCell<usize>>,
    refreshes: Rc<RefCell<VecDeque<ConnectionRefreshOutcome>>>,
    store_state: Rc<RefCell<DesiredState>>,
    saves: Rc<RefCell<usize>>,
    _fixture: DiscoveryFixture,
}

fn harness(
    desired: DesiredState,
    observed: Wave3ConfigSnapshot,
    outcomes: Vec<ConnectionOutcome>,
    actions: Vec<SaveAction>,
) -> Harness {
    let fixture = discovery_fixture();
    let candidate = LinuxInventory::new()
        .discover(&fixture.paths)
        .devices
        .into_iter()
        .next()
        .expect("fixture candidate");
    let topology = candidate.topology.clone();
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
        mixer: librewave_core::MixerSnapshot::default(),
    };
    let calls = Rc::new(RefCell::new(Vec::new()));
    let connections = Rc::new(RefCell::new(0));
    let refreshes = Rc::new(RefCell::new(VecDeque::new()));
    let queued = Rc::new(RefCell::new(VecDeque::from(outcomes)));
    let store_state = Rc::new(RefCell::new(desired.clone()));
    let saves = Rc::new(RefCell::new(0));
    let store = FakeStore {
        state: Rc::clone(&store_state),
        actions: Rc::new(RefCell::new(VecDeque::from(actions))),
        saves: Rc::clone(&saves),
    };
    let connection_calls = Rc::clone(&calls);
    let connection_count = Rc::clone(&connections);
    let connection_refreshes = Rc::clone(&refreshes);
    let mut daemon = Daemon::with_connector(
        snapshot,
        BTreeMap::from([(DeviceId(1), candidate)]),
        desired,
        Box::new(store),
        move |_| {
            *connection_count.borrow_mut() += 1;
            Ok(Box::new(SharedConnection {
                config: observed.clone(),
                refreshes: Rc::clone(&connection_refreshes),
                outcomes: Rc::clone(&queued),
                calls: Rc::clone(&connection_calls),
            }))
        },
    );
    daemon.paths = fixture.paths.clone();
    daemon.allocator.ids_by_topology.insert(topology, DeviceId(1));
    Harness { daemon, calls, connections, refreshes, store_state, saves, _fixture: fixture }
}

fn inspect(daemon: &mut Daemon) -> DeviceSnapshot {
    let response = daemon
        .handle(Command::InspectDevice { id: DeviceId(1) }, librewave_core::Origin::Client)
        .expect("inspect fake device");
    let Response::DeviceInspection { device } = response else {
        panic!("unexpected inspection response")
    };
    device
}

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
        mixer: librewave_core::MixerSnapshot::default(),
    };
    let daemon = Daemon::from_snapshot(snapshot);
    assert_eq!(daemon.snapshot().devices.len(), 1);
}

#[test]
fn mixer_get_and_route_change_persist_before_advancing_both_generations() {
    let (mut daemon, persisted, saves) = mixer_daemon(vec![SaveAction::Ok]);
    let response =
        daemon.handle(Command::GetMixer, librewave_core::Origin::Client).expect("get mixer");
    assert_eq!(response, Response::Mixer { mixer: MixerSnapshot::default() });

    let route = MixRoute::new(false, FaderGain::from_half_decibel_steps(-1).expect("valid fader"));
    let response = daemon
        .handle(
            Command::SetMixerRoute {
                expected_generation: MixerGeneration(0),
                source: librewave_core::SYSTEM_SOURCE_ID,
                target: MixTarget::Stream,
                route,
            },
            librewave_core::Origin::Client,
        )
        .expect("change mixer route");
    let Response::MixerRouteChanged { mixer } = response else {
        panic!("unexpected mixer response")
    };
    assert_eq!(mixer.generation(), MixerGeneration(1));
    assert_eq!(daemon.snapshot().generation, 1);
    assert_eq!(
        mixer
            .profile
            .source(librewave_core::SYSTEM_SOURCE_ID)
            .expect("system source")
            .controls
            .stream(),
        route
    );
    assert_eq!(*saves.borrow(), 1);
    assert_eq!(*persisted.borrow(), mixer.profile);
}

#[test]
fn unchanged_mixer_route_is_success_without_persistence_or_generation_change() {
    let (mut daemon, persisted, saves) = mixer_daemon(Vec::new());
    let response = daemon
        .handle(
            Command::SetMixerRoute {
                expected_generation: MixerGeneration(0),
                source: librewave_core::MICROPHONE_SOURCE_ID,
                target: MixTarget::Monitor,
                route: MixRoute::new(true, FaderGain::UNITY),
            },
            librewave_core::Origin::Client,
        )
        .expect("unchanged mixer route");
    assert_eq!(response, Response::MixerRouteChanged { mixer: MixerSnapshot::default() });
    assert_eq!(daemon.snapshot().generation, 0);
    assert_eq!(*saves.borrow(), 0);
    assert_eq!(*persisted.borrow(), MixerProfile::default());
}

#[test]
fn mixer_route_rejects_stale_generation_and_unknown_source_without_saving() {
    let (mut daemon, _persisted, saves) = mixer_daemon(Vec::new());
    let route = MixRoute::new(false, FaderGain::UNITY);
    let stale = daemon
        .handle(
            Command::SetMixerRoute {
                expected_generation: MixerGeneration(4),
                source: librewave_core::MICROPHONE_SOURCE_ID,
                target: MixTarget::Monitor,
                route,
            },
            librewave_core::Origin::Client,
        )
        .expect_err("stale generation");
    assert_eq!(
        stale.kind,
        IpcErrorKind::StaleMixerGeneration {
            expected: MixerGeneration(4),
            actual: MixerGeneration(0),
        }
    );
    let unknown = daemon
        .handle(
            Command::SetMixerRoute {
                expected_generation: MixerGeneration(0),
                source: SourceId::new(99),
                target: MixTarget::Monitor,
                route,
            },
            librewave_core::Origin::Client,
        )
        .expect_err("unknown source");
    assert_eq!(unknown.kind, IpcErrorKind::MixerSourceNotFound { id: SourceId::new(99) });
    assert_eq!(*saves.borrow(), 0);
    assert_eq!(daemon.snapshot().generation, 0);
}

#[test]
fn exhausted_mixer_generation_rejects_without_saving_or_changing_state() {
    let mut snapshot = Snapshot::empty();
    snapshot.mixer.profile.generation = MixerGeneration(u64::MAX);
    let profile = Rc::new(RefCell::new(snapshot.mixer.profile.clone()));
    let saves = Rc::new(RefCell::new(0));
    let store = FakeMixerStore {
        profile: Rc::clone(&profile),
        actions: Rc::new(RefCell::new(VecDeque::new())),
        saves: Rc::clone(&saves),
    };
    let mut daemon = Daemon::with_mixer_store(snapshot, Box::new(store));
    let before = daemon.snapshot().clone();
    let error = daemon
        .handle(
            Command::SetMixerRoute {
                expected_generation: MixerGeneration(u64::MAX),
                source: librewave_core::MICROPHONE_SOURCE_ID,
                target: MixTarget::Monitor,
                route: MixRoute::new(false, FaderGain::UNITY),
            },
            librewave_core::Origin::Client,
        )
        .expect_err("generation exhaustion");
    assert_eq!(error.kind, IpcErrorKind::MixerGenerationExhausted);
    assert_eq!(daemon.snapshot(), &before);
    assert_eq!(*saves.borrow(), 0);
    assert_eq!(profile.borrow().generation, MixerGeneration(u64::MAX));
}

#[test]
fn mixer_pre_rename_failure_leaves_persisted_and_retained_state_unchanged() {
    let (mut daemon, persisted, saves) = mixer_daemon(vec![SaveAction::Before]);
    let error = daemon
        .handle(
            Command::SetMixerRoute {
                expected_generation: MixerGeneration(0),
                source: librewave_core::MICROPHONE_SOURCE_ID,
                target: MixTarget::Stream,
                route: MixRoute::new(false, FaderGain::UNITY),
            },
            librewave_core::Origin::Client,
        )
        .expect_err("pre-rename failure");
    assert_eq!(error.kind, IpcErrorKind::MixerPersistence);
    assert_eq!(daemon.snapshot(), &Snapshot::empty());
    assert_eq!(*persisted.borrow(), MixerProfile::default());
    assert_eq!(*saves.borrow(), 1);
}

#[test]
fn mixer_post_rename_ambiguity_keeps_retained_state_and_retry_converges() {
    let (mut daemon, persisted, saves) = mixer_daemon(vec![SaveAction::After, SaveAction::Ok]);
    let command = Command::SetMixerRoute {
        expected_generation: MixerGeneration(0),
        source: librewave_core::MICROPHONE_SOURCE_ID,
        target: MixTarget::Stream,
        route: MixRoute::new(false, FaderGain::UNITY),
    };
    let error =
        daemon.handle(command, librewave_core::Origin::Client).expect_err("post-rename ambiguity");
    assert_eq!(error.kind, IpcErrorKind::MixerPersistenceAmbiguous);
    assert_eq!(daemon.snapshot(), &Snapshot::empty());
    assert_eq!(persisted.borrow().generation, MixerGeneration(1));

    let response = daemon.handle(command, librewave_core::Origin::Client).expect("retry converges");
    let Response::MixerRouteChanged { mixer } = response else {
        panic!("unexpected mixer response")
    };
    assert_eq!(mixer.profile, *persisted.borrow());
    assert_eq!(mixer.generation(), MixerGeneration(1));
    assert_eq!(daemon.snapshot().generation, 1);
    assert_eq!(*saves.borrow(), 2);
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
        mixer: librewave_core::MixerSnapshot::default(),
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
        mixer: librewave_core::MixerSnapshot::default(),
    };
    let admitted = DeviceAdmissionSnapshot::Admitted {
        api: librewave_core::ApiVersion::new(5, 4),
        generation: DeviceGeneration(1),
        write_access: DeviceWriteAccess::Ready,
        config: Some(wave3_config()),
    };
    let mut results: VecDeque<Result<Box<dyn ManagedWave3Connection>, AdmissionError>> =
        VecDeque::from([
            Err(AdmissionError::Disconnected),
            Ok(Box::new(FakeConnection { config: wave3_config(), outcomes: VecDeque::new() })
                as Box<dyn ManagedWave3Connection>),
        ]);
    let daemon_snapshot = snapshot.clone();
    let mut daemon = Daemon::with_connector(
        daemon_snapshot,
        BTreeMap::from([(DeviceId(1), candidate)]),
        DesiredState::default(),
        Box::new(MemoryStore::default()),
        move |candidate| {
            assert_eq!(candidate.topology.as_str(), "1-8.3");
            results.pop_front().expect("fake connection result")
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

    let mut inconsistent = Daemon::with_connector(
        snapshot,
        BTreeMap::new(),
        DesiredState::default(),
        Box::new(MemoryStore::default()),
        |_| panic!("the inspector must not run for an inconsistent map"),
    );
    let error = inconsistent
        .handle(Command::InspectDevice { id: DeviceId(1) }, librewave_core::Origin::Client)
        .expect_err("missing candidate");
    assert!(matches!(error.kind, librewave_ipc::IpcErrorKind::DeviceNotFound { id: DeviceId(1) }));
}

#[test]
fn command_success_and_unchanged_promote_only_the_explicit_control() {
    let mut changed = wave3_config();
    changed.clipguard_enable = true;
    let mut changed_harness = harness(
        DesiredState::default(),
        wave3_config(),
        vec![ConnectionOutcome::Applied(changed.clone())],
        vec![],
    );
    let device = inspect(&mut changed_harness.daemon);
    assert_eq!(device.admission.generation(), Some(DeviceGeneration(1)));
    let response = changed_harness
        .daemon
        .handle(
            Command::SetWave3Control {
                id: DeviceId(1),
                expected_generation: DeviceGeneration(1),
                control: Wave3Control::Clipguard(true),
            },
            librewave_core::Origin::Client,
        )
        .expect("apply control");
    let Response::ControlChanged { device } = response else {
        panic!("unexpected control response")
    };
    assert_eq!(device.admission.generation(), Some(DeviceGeneration(2)));
    assert_eq!(device.admission.config(), Some(&changed));
    let state = changed_harness.store_state.borrow();
    let desired = state.get("1-8.3").expect("managed topology");
    assert_eq!(desired.managed.clipguard, Some(true));
    assert_eq!(desired.managed.gain_lock, None);
    assert_eq!(desired.pending, None);
    assert_eq!(*changed_harness.saves.borrow(), 2);

    let mut unchanged = harness(
        DesiredState::default(),
        wave3_config(),
        vec![ConnectionOutcome::Unchanged(wave3_config())],
        vec![],
    );
    inspect(&mut unchanged.daemon);
    let response = unchanged
        .daemon
        .handle(
            Command::SetWave3Control {
                id: DeviceId(1),
                expected_generation: DeviceGeneration(1),
                control: Wave3Control::MicrophoneMute(false),
            },
            librewave_core::Origin::Client,
        )
        .expect("accept unchanged control");
    let Response::ControlChanged { device } = response else {
        panic!("unexpected unchanged response")
    };
    assert_eq!(device.admission.generation(), Some(DeviceGeneration(1)));
}

#[test]
fn invalid_range_step_and_stale_generation_stop_before_persistence_and_usb() {
    for control in [
        Wave3Control::InputGain(librewave_core::FixedPointValue {
            raw: 10_241,
            fractional_bits: 8,
        }),
        Wave3Control::InputGain(librewave_core::FixedPointValue { raw: 1, fractional_bits: 8 }),
    ] {
        let mut harness = harness(DesiredState::default(), wave3_config(), vec![], vec![]);
        inspect(&mut harness.daemon);
        let error = harness
            .daemon
            .handle(
                Command::SetWave3Control {
                    id: DeviceId(1),
                    expected_generation: DeviceGeneration(1),
                    control,
                },
                librewave_core::Origin::Client,
            )
            .expect_err("invalid semantic value");
        assert_eq!(error.kind, IpcErrorKind::InvalidControl);
        assert!(harness.calls.borrow().is_empty());
        assert_eq!(*harness.saves.borrow(), 0);
    }

    let mut stale = harness(DesiredState::default(), wave3_config(), vec![], vec![]);
    inspect(&mut stale.daemon);
    let error = stale
        .daemon
        .handle(
            Command::SetWave3Control {
                id: DeviceId(1),
                expected_generation: DeviceGeneration(0),
                control: Wave3Control::Clipguard(true),
            },
            librewave_core::Origin::Client,
        )
        .expect_err("stale generation");
    assert!(matches!(error.kind, IpcErrorKind::StaleDeviceGeneration { .. }));
    assert!(stale.calls.borrow().is_empty());
    assert_eq!(*stale.saves.borrow(), 0);
}

#[test]
fn retained_inspection_keeps_unchanged_generations_and_accepts_device_state_without_writes() {
    let mut desired = DesiredState::default();
    desired
        .stage("1-8.3".to_owned(), Wave3Control::Clipguard(true))
        .expect("stage managed control");
    desired.promote("1-8.3").expect("promote managed control");
    let mut original = wave3_config();
    original.clipguard_enable = true;
    let mut harness = harness(desired, original.clone(), vec![], vec![]);
    let admitted = inspect(&mut harness.daemon);
    assert_eq!(admitted.admission.generation(), Some(DeviceGeneration(1)));
    let state_generation = harness.daemon.snapshot().generation;

    let unchanged = inspect(&mut harness.daemon);
    assert_eq!(unchanged.admission.generation(), Some(DeviceGeneration(1)));
    assert_eq!(harness.daemon.snapshot().generation, state_generation);

    let mut physical = original;
    physical.input_mute = true;
    physical.clipguard_enable = false;
    harness.refreshes.borrow_mut().push_back(ConnectionRefreshOutcome::Changed(physical.clone()));
    let refreshed = inspect(&mut harness.daemon);
    assert_eq!(refreshed.admission.generation(), Some(DeviceGeneration(2)));
    assert_eq!(refreshed.admission.config(), Some(&physical));
    assert!(harness.calls.borrow().is_empty());
    assert_eq!(*harness.saves.borrow(), 0);
    assert_eq!(
        harness.store_state.borrow().get("1-8.3").expect("managed topology").managed.clipguard,
        Some(true)
    );
}

#[test]
fn refresh_command_polls_retained_connections_but_status_and_list_do_not() {
    let original = wave3_config();
    let mut harness = harness(DesiredState::default(), original.clone(), vec![], vec![]);
    inspect(&mut harness.daemon);
    let mut physical = original.clone();
    physical.input_mute = true;
    harness.refreshes.borrow_mut().push_back(ConnectionRefreshOutcome::Changed(physical.clone()));

    let status = harness
        .daemon
        .handle(Command::GetStatus, librewave_core::Origin::Client)
        .expect("get status");
    let Response::Status { snapshot } = status else { panic!("unexpected status response") };
    assert_eq!(snapshot.devices[0].admission.config(), Some(&original));
    let listed = harness
        .daemon
        .handle(Command::ListDevices, librewave_core::Origin::Client)
        .expect("list devices");
    let Response::Devices { devices } = listed else { panic!("unexpected devices response") };
    assert_eq!(devices[0].admission.config(), Some(&original));
    assert_eq!(harness.refreshes.borrow().len(), 1);

    let refreshed = harness
        .daemon
        .handle(Command::Refresh, librewave_core::Origin::Client)
        .expect("refresh retained connections");
    let Response::Refreshed { snapshot } = refreshed else { panic!("unexpected refresh response") };
    assert_eq!(snapshot.devices[0].admission.generation(), Some(DeviceGeneration(2)));
    assert_eq!(snapshot.devices[0].admission.config(), Some(&physical));
    assert!(harness.refreshes.borrow().is_empty());
    assert!(harness.calls.borrow().is_empty());
    assert_eq!(*harness.saves.borrow(), 0);

    let state_generation = snapshot.generation;
    let unchanged = harness
        .daemon
        .handle(Command::Refresh, librewave_core::Origin::Client)
        .expect("unchanged retained refresh");
    let Response::Refreshed { snapshot } = unchanged else {
        panic!("unexpected unchanged refresh response")
    };
    assert_eq!(snapshot.generation, state_generation);
    assert_eq!(snapshot.devices[0].admission.generation(), Some(DeviceGeneration(2)));
}

#[test]
fn retained_refresh_preserves_pending_lock_and_rejects_a_stale_client() {
    let mut pending = DesiredState::default();
    pending
        .stage("1-8.3".to_owned(), Wave3Control::Clipguard(true))
        .expect("stage pending control");
    let mut pending_harness = harness(pending, wave3_config(), vec![], vec![]);
    let admitted = inspect(&mut pending_harness.daemon);
    assert!(matches!(
        admitted.admission,
        DeviceAdmissionSnapshot::Admitted {
            generation: DeviceGeneration(1),
            write_access: DeviceWriteAccess::Locked {
                reason: DeviceWriteLockReason::PersistenceAmbiguous
            },
            ..
        }
    ));

    let mut physical = wave3_config();
    physical.input_mute = true;
    pending_harness
        .refreshes
        .borrow_mut()
        .push_back(ConnectionRefreshOutcome::Changed(physical.clone()));
    let refreshed = pending_harness
        .daemon
        .handle(Command::Refresh, librewave_core::Origin::Client)
        .expect("refresh pending retained connection");
    let Response::Refreshed { snapshot } = refreshed else {
        panic!("unexpected pending refresh response")
    };
    let refreshed = snapshot.devices[0].clone();
    assert!(matches!(
        refreshed.admission,
        DeviceAdmissionSnapshot::Admitted {
            generation: DeviceGeneration(2),
            write_access: DeviceWriteAccess::Locked {
                reason: DeviceWriteLockReason::PersistenceAmbiguous
            },
            ref config,
            ..
        } if config.as_ref() == Some(&physical)
    ));
    assert!(pending_harness.calls.borrow().is_empty());
    assert_eq!(*pending_harness.saves.borrow(), 0);
    assert_eq!(*pending_harness.connections.borrow(), 1);

    let error = pending_harness
        .daemon
        .handle(
            Command::SetWave3Control {
                id: DeviceId(1),
                expected_generation: DeviceGeneration(1),
                control: Wave3Control::MicrophoneMute(false),
            },
            librewave_core::Origin::Client,
        )
        .expect_err("pending lock precedes stale generation");
    assert!(matches!(
        error.kind,
        IpcErrorKind::DeviceWriteLocked { reason: DeviceWriteLockReason::PersistenceAmbiguous }
    ));

    let mut ready = harness(DesiredState::default(), wave3_config(), vec![], vec![]);
    inspect(&mut ready.daemon);
    let mut changed = wave3_config();
    changed.input_mute = true;
    ready.refreshes.borrow_mut().push_back(ConnectionRefreshOutcome::Changed(changed));
    inspect(&mut ready.daemon);
    let error = ready
        .daemon
        .handle(
            Command::SetWave3Control {
                id: DeviceId(1),
                expected_generation: DeviceGeneration(1),
                control: Wave3Control::MicrophoneMute(false),
            },
            librewave_core::Origin::Client,
        )
        .expect_err("stale client generation");
    assert!(matches!(
        error.kind,
        IpcErrorKind::StaleDeviceGeneration {
            expected: DeviceGeneration(1),
            actual: DeviceGeneration(2)
        }
    ));
    assert!(ready.calls.borrow().is_empty());
    assert_eq!(*ready.saves.borrow(), 0);
}

#[test]
fn recovery_locks_reopen_and_readmit_instead_of_using_configuration_refresh() {
    for (restoration_unverified, reason) in [
        (false, DeviceWriteLockReason::RestoreFailed),
        (true, DeviceWriteLockReason::RestorationUnverified),
    ] {
        let mut desired = DesiredState::default();
        desired
            .stage("1-8.3".to_owned(), Wave3Control::Clipguard(true))
            .expect("stage desired control");
        desired.promote("1-8.3").expect("promote desired control");
        let failure = ConnectionOutcome::Failed {
            observed: if restoration_unverified { None } else { Some(wave3_config()) },
            restoration_unverified,
            message: "injected restore failure".to_owned(),
        };
        let mut harness = harness(desired, wave3_config(), vec![failure.clone(), failure], vec![]);

        let first = inspect(&mut harness.daemon);
        assert!(matches!(
            first.admission,
            DeviceAdmissionSnapshot::Admitted {
                write_access: DeviceWriteAccess::Locked { reason: actual },
                ..
            } if actual == reason
        ));
        assert_eq!(*harness.connections.borrow(), 1);

        let second = inspect(&mut harness.daemon);
        assert!(matches!(
            second.admission,
            DeviceAdmissionSnapshot::Admitted {
                write_access: DeviceWriteAccess::Locked { reason: actual },
                ..
            } if actual == reason
        ));
        assert_eq!(*harness.connections.borrow(), 2);
        assert_eq!(harness.calls.borrow().len(), 2);
        assert!(harness.refreshes.borrow().is_empty());
    }
}

#[test]
fn retained_refresh_drops_failed_connections_and_reports_safe_admission_state() {
    for (error, connection) in [
        (AdmissionError::MalformedResponse, DeviceConnection::Connected),
        (AdmissionError::Disconnected, DeviceConnection::Disconnected),
    ] {
        let mut harness = harness(DesiredState::default(), wave3_config(), vec![], vec![]);
        inspect(&mut harness.daemon);
        harness.refreshes.borrow_mut().push_back(ConnectionRefreshOutcome::Failed(error.clone()));

        let refreshed = inspect(&mut harness.daemon);
        assert_eq!(refreshed.connection, connection);
        assert_eq!(refreshed.admission, DeviceAdmissionSnapshot::Failed { error });
        assert!(!harness.daemon.connections.contains_key(&DeviceId(1)));
        assert!(harness.calls.borrow().is_empty());
        assert_eq!(*harness.saves.borrow(), 0);
    }
}

#[test]
fn persistence_failure_precedes_usb_and_disconnect_clears_pending() {
    let mut failed =
        harness(DesiredState::default(), wave3_config(), vec![], vec![SaveAction::Before]);
    inspect(&mut failed.daemon);
    let error = failed
        .daemon
        .handle(
            Command::SetWave3Control {
                id: DeviceId(1),
                expected_generation: DeviceGeneration(1),
                control: Wave3Control::Clipguard(true),
            },
            librewave_core::Origin::Client,
        )
        .expect_err("persistence failure");
    assert_eq!(error.kind, IpcErrorKind::Persistence);
    assert!(failed.calls.borrow().is_empty());

    let mut disconnected = harness(
        DesiredState::default(),
        wave3_config(),
        vec![ConnectionOutcome::Disconnected],
        vec![],
    );
    inspect(&mut disconnected.daemon);
    let error = disconnected
        .daemon
        .handle(
            Command::SetWave3Control {
                id: DeviceId(1),
                expected_generation: DeviceGeneration(1),
                control: Wave3Control::Clipguard(true),
            },
            librewave_core::Origin::Client,
        )
        .expect_err("disconnect");
    assert!(matches!(error.kind, IpcErrorKind::DeviceDisconnected { .. }));
    assert_eq!(
        disconnected.daemon.snapshot().devices[0].connection,
        DeviceConnection::Disconnected
    );
    assert!(
        disconnected.store_state.borrow().get("1-8.3").is_some_and(|state| state.pending.is_none())
    );
}

#[test]
fn reconnect_restores_sparse_controls_and_locks_after_partial_failure() {
    let mut desired = DesiredState::default();
    desired.stage("1-8.3".to_owned(), Wave3Control::Clipguard(true)).expect("stage clipguard");
    desired.promote("1-8.3").expect("promote clipguard");
    desired.stage("1-8.3".to_owned(), Wave3Control::LowCut(true)).expect("stage low cut");
    desired.promote("1-8.3").expect("promote low cut");
    let mut after_first = wave3_config();
    after_first.clipguard_enable = true;
    let mut restored = after_first.clone();
    restored.lowcut_enable = true;
    let mut success = harness(
        desired.clone(),
        wave3_config(),
        vec![
            ConnectionOutcome::Applied(after_first.clone()),
            ConnectionOutcome::Applied(restored.clone()),
        ],
        vec![],
    );
    let device = inspect(&mut success.daemon);
    assert_eq!(device.admission.config(), Some(&restored));
    assert_eq!(device.admission.control_access(), librewave_core::ControlAccessState::Writable);
    assert_eq!(success.calls.borrow().len(), 2);

    let mut partial = harness(
        desired,
        wave3_config(),
        vec![
            ConnectionOutcome::Applied(after_first.clone()),
            ConnectionOutcome::Failed {
                observed: Some(after_first.clone()),
                restoration_unverified: false,
                message: "injected restore failure".to_owned(),
            },
        ],
        vec![],
    );
    let device = inspect(&mut partial.daemon);
    assert_eq!(device.admission.config(), Some(&after_first));
    assert!(matches!(
        device.admission,
        DeviceAdmissionSnapshot::Admitted {
            write_access: DeviceWriteAccess::Locked {
                reason: DeviceWriteLockReason::RestoreFailed
            },
            ..
        }
    ));
    let error = partial
        .daemon
        .handle(
            Command::SetWave3Control {
                id: DeviceId(1),
                expected_generation: DeviceGeneration(1),
                control: Wave3Control::MicrophoneMute(true),
            },
            librewave_core::Origin::Client,
        )
        .expect_err("write lockout");
    assert!(matches!(error.kind, IpcErrorKind::DeviceWriteLocked { .. }));
    assert_eq!(partial.calls.borrow().len(), 2);
}

#[test]
fn pending_state_and_unverified_restore_never_publish_a_default_config() {
    let mut pending = DesiredState::default();
    pending.stage("1-8.3".to_owned(), Wave3Control::GainLock(true)).expect("stage pending control");
    let mut pending_restart = harness(pending, wave3_config(), vec![], vec![]);
    let device = inspect(&mut pending_restart.daemon);
    assert!(matches!(
        device.admission,
        DeviceAdmissionSnapshot::Admitted {
            write_access: DeviceWriteAccess::Locked {
                reason: DeviceWriteLockReason::PersistenceAmbiguous
            },
            ..
        }
    ));
    assert!(pending_restart.calls.borrow().is_empty());

    let mut desired = DesiredState::default();
    desired
        .stage("1-8.3".to_owned(), Wave3Control::Clipguard(true))
        .expect("stage desired control");
    desired.promote("1-8.3").expect("promote desired control");
    let mut unverified = harness(
        desired,
        wave3_config(),
        vec![ConnectionOutcome::Failed {
            observed: None,
            restoration_unverified: true,
            message: "unverified restoration".to_owned(),
        }],
        vec![],
    );
    let device = inspect(&mut unverified.daemon);
    assert_eq!(device.admission.config(), None);
    assert!(matches!(
        device.admission,
        DeviceAdmissionSnapshot::Admitted {
            write_access: DeviceWriteAccess::Locked {
                reason: DeviceWriteLockReason::RestorationUnverified
            },
            config: None,
            ..
        }
    ));
}

#[test]
fn final_save_failure_leaves_pending_state_for_restart_lockout() {
    let mut changed = wave3_config();
    changed.clipguard_enable = true;
    let mut failed_promotion = harness(
        DesiredState::default(),
        wave3_config(),
        vec![ConnectionOutcome::Applied(changed)],
        vec![SaveAction::Ok, SaveAction::Before],
    );
    inspect(&mut failed_promotion.daemon);
    let error = failed_promotion
        .daemon
        .handle(
            Command::SetWave3Control {
                id: DeviceId(1),
                expected_generation: DeviceGeneration(1),
                control: Wave3Control::Clipguard(true),
            },
            librewave_core::Origin::Client,
        )
        .expect_err("promotion failure");
    assert_eq!(error.kind, IpcErrorKind::PersistenceAmbiguous);
    let persisted = failed_promotion.store_state.borrow().clone();
    assert!(persisted.get("1-8.3").is_some_and(|state| state.pending.is_some()));
    let mut restarted = harness(persisted, wave3_config(), vec![], vec![]);
    let device = inspect(&mut restarted.daemon);
    assert_eq!(device.admission.control_access(), librewave_core::ControlAccessState::WriteLocked);
    assert!(restarted.calls.borrow().is_empty());

    let mut failed_cleanup = harness(
        DesiredState::default(),
        wave3_config(),
        vec![ConnectionOutcome::StaleBaseline(wave3_config())],
        vec![SaveAction::Ok, SaveAction::Before],
    );
    inspect(&mut failed_cleanup.daemon);
    let error = failed_cleanup
        .daemon
        .handle(
            Command::SetWave3Control {
                id: DeviceId(1),
                expected_generation: DeviceGeneration(1),
                control: Wave3Control::Clipguard(true),
            },
            librewave_core::Origin::Client,
        )
        .expect_err("pending cleanup failure");
    assert_eq!(error.kind, IpcErrorKind::PersistenceAmbiguous);
    assert!(
        failed_cleanup
            .store_state
            .borrow()
            .get("1-8.3")
            .is_some_and(|state| state.pending.is_some())
    );
}

#[test]
fn post_rename_save_failures_lock_without_false_acknowledgement() {
    let mut uncertain_stage =
        harness(DesiredState::default(), wave3_config(), vec![], vec![SaveAction::After]);
    inspect(&mut uncertain_stage.daemon);
    let error = uncertain_stage
        .daemon
        .handle(
            Command::SetWave3Control {
                id: DeviceId(1),
                expected_generation: DeviceGeneration(1),
                control: Wave3Control::Clipguard(true),
            },
            librewave_core::Origin::Client,
        )
        .expect_err("uncertain pending stage");
    assert_eq!(error.kind, IpcErrorKind::PersistenceAmbiguous);
    assert!(uncertain_stage.calls.borrow().is_empty());
    assert!(
        uncertain_stage
            .store_state
            .borrow()
            .get("1-8.3")
            .is_some_and(|state| state.pending.is_some())
    );

    let mut uncertain_promotion = harness(
        DesiredState::default(),
        wave3_config(),
        vec![ConnectionOutcome::Applied(wave3_config())],
        vec![SaveAction::Ok, SaveAction::After],
    );
    inspect(&mut uncertain_promotion.daemon);
    let error = uncertain_promotion
        .daemon
        .handle(
            Command::SetWave3Control {
                id: DeviceId(1),
                expected_generation: DeviceGeneration(1),
                control: Wave3Control::Clipguard(true),
            },
            librewave_core::Origin::Client,
        )
        .expect_err("uncertain pending promotion");
    assert_eq!(error.kind, IpcErrorKind::PersistenceAmbiguous);
    assert_eq!(
        uncertain_promotion.daemon.snapshot().devices[0].admission.control_access(),
        librewave_core::ControlAccessState::WriteLocked
    );
    assert!(
        uncertain_promotion.store_state.borrow().get("1-8.3").is_some_and(|state| state
            .managed
            .clipguard
            == Some(true)
            && state.pending.is_none())
    );

    let mut uncertain_cleanup = harness(
        DesiredState::default(),
        wave3_config(),
        vec![ConnectionOutcome::StaleBaseline(wave3_config())],
        vec![SaveAction::Ok, SaveAction::After],
    );
    inspect(&mut uncertain_cleanup.daemon);
    let error = uncertain_cleanup
        .daemon
        .handle(
            Command::SetWave3Control {
                id: DeviceId(1),
                expected_generation: DeviceGeneration(1),
                control: Wave3Control::Clipguard(true),
            },
            librewave_core::Origin::Client,
        )
        .expect_err("uncertain pending cleanup");
    assert_eq!(error.kind, IpcErrorKind::PersistenceAmbiguous);
    assert_eq!(
        uncertain_cleanup.daemon.snapshot().devices[0].admission.control_access(),
        librewave_core::ControlAccessState::WriteLocked
    );
    assert!(
        uncertain_cleanup
            .store_state
            .borrow()
            .get("1-8.3")
            .is_some_and(|state| state.pending.is_none())
    );
}

#[test]
fn invalid_loaded_topology_or_control_fails_before_connection() {
    let invalid_control = librewave_core::FixedPointValue { raw: 1, fractional_bits: 8 };
    let mut invalid_semantic = DesiredState::default();
    invalid_semantic
        .stage("1-8.3".to_owned(), Wave3Control::InputGain(invalid_control))
        .expect("stage malformed fixture");
    let mut invalid_topology = DesiredState::default();
    invalid_topology
        .stage("1-8.3.invalid".to_owned(), Wave3Control::Clipguard(true))
        .expect("stage malformed topology fixture");

    for state in [invalid_semantic, invalid_topology] {
        let calls = Rc::new(RefCell::new(0_u32));
        let connector_calls = Rc::clone(&calls);
        let store = FakeStore {
            state: Rc::new(RefCell::new(state)),
            actions: Rc::new(RefCell::new(VecDeque::new())),
            saves: Rc::new(RefCell::new(0)),
        };
        let result = Daemon::with_components(
            LinuxInventory::new(),
            librewave_platform_linux::DiscoveryPaths::default(),
            Box::new(store),
            Box::new(MemoryMixerStore::default()),
            move |_| {
                *connector_calls.borrow_mut() += 1;
                Err(AdmissionError::Transport)
            },
        );
        assert!(matches!(result, Err(PersistenceError::InvalidState(_))));
        assert_eq!(*calls.borrow(), 0);
    }
}

#[test]
fn reconnect_disconnect_drops_connection_and_reports_disconnected() {
    let mut desired = DesiredState::default();
    desired
        .stage("1-8.3".to_owned(), Wave3Control::Clipguard(true))
        .expect("stage desired control");
    desired.promote("1-8.3").expect("promote desired control");
    let mut disconnected =
        harness(desired, wave3_config(), vec![ConnectionOutcome::Disconnected], vec![]);
    let device = inspect(&mut disconnected.daemon);
    assert_eq!(device.connection, DeviceConnection::Disconnected);
    assert!(matches!(
        device.admission,
        DeviceAdmissionSnapshot::Failed { error: AdmissionError::Disconnected }
    ));
    assert!(!disconnected.daemon.connections.contains_key(&DeviceId(1)));
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
    daemon.refresh_inventory(librewave_core::Origin::Recovery);
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
    daemon.refresh_inventory(librewave_core::Origin::Recovery);

    assert_eq!(daemon.candidates.get(&id_a).expect("topology A candidate").alsa_cards[0].number, 8);
    assert_eq!(daemon.candidates.get(&id_b).expect("topology B candidate").alsa_cards[0].number, 7);
    assert!(matches!(
        daemon
            .snapshot()
            .devices
            .iter()
            .find(|device| device.id == id_a)
            .expect("topology A snapshot")
            .admission,
        DeviceAdmissionSnapshot::Failed { error: librewave_core::AdmissionError::PermissionDenied }
    ));
    let ids: Vec<_> = daemon.snapshot().devices.iter().map(|device| device.id).collect();
    let mut expected_ids = vec![id_a, id_b];
    expected_ids.sort_unstable();
    assert_eq!(ids, expected_ids);
    assert!(ids.windows(2).all(|pair| pair[0] < pair[1]));

    fs::remove_file(usb_bus.join("1-8.2")).expect("disconnect topology A");
    daemon.refresh_inventory(librewave_core::Origin::Recovery);
    assert_eq!(daemon.snapshot().devices.len(), 1);
    assert!(!daemon.candidates.contains_key(&id_a));

    symlink(&usb_a, usb_bus.join("1-8.2")).expect("reconnect topology A");
    daemon.refresh_inventory(librewave_core::Origin::Recovery);
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
    daemon.refresh_inventory(librewave_core::Origin::Recovery);
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
