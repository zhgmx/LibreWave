use crate::control::{ConnectionOutcome, ConnectionRefreshOutcome, ManagedWave3Connection};
use crate::persistence::{
    DesiredState, DesiredStateStore, DesiredWave3Controls, FileDesiredStateStore,
    FileMixerStateStore, MixerStateStore,
};
use crate::{PersistenceError, control, snapshot_from_inventory};
use librewave_core::{
    AdmissionError, Command, DeviceAdmissionSnapshot, DeviceConnection, DeviceGeneration, DeviceId,
    DeviceSnapshot, DeviceWriteAccess, DeviceWriteLockReason, Event, MixRoute, MixTarget,
    MixerGeneration, MixerProfile, MixerSnapshot, Snapshot, SourceId, State, Wave3ConfigSnapshot,
    Wave3Control,
};
use librewave_ipc::{IpcError, IpcErrorKind, Response};
use librewave_platform_linux::{
    HostAudioInventory, LinuxInventory, UsbDeviceCandidate, UsbTopology, Wave3UsbConnection,
    admission_error,
};
use std::collections::BTreeMap;
use std::path::PathBuf;

/// Names an intentionally discarded state event at the current no-subscriber boundary.
fn ignore_reconciliation_event(_event: Option<Event>) {}

pub struct Daemon {
    inventory: LinuxInventory,
    paths: librewave_platform_linux::DiscoveryPaths,
    state: State,
    candidates: BTreeMap<DeviceId, UsbDeviceCandidate>,
    connector: Box<ConnectionFactory>,
    connections: BTreeMap<DeviceId, OwnedConnection>,
    device_generations: BTreeMap<String, u64>,
    desired: DesiredState,
    store: Box<dyn DesiredStateStore>,
    mixer_store: Box<dyn MixerStateStore>,
    allocator: DeviceIdAllocator,
}

type ConnectionFactory =
    dyn FnMut(&UsbDeviceCandidate) -> Result<Box<dyn ManagedWave3Connection>, AdmissionError>;

struct OwnedConnection {
    topology: String,
    connection: Box<dyn ManagedWave3Connection>,
    locked: Option<DeviceWriteLockReason>,
}

enum RestoreResult {
    Ready(Wave3ConfigSnapshot),
    Locked { config: Option<Wave3ConfigSnapshot>, reason: DeviceWriteLockReason },
    Disconnected,
}

impl Daemon {
    /// Creates a daemon using the default Linux inventory and paths.
    ///
    /// # Errors
    ///
    /// Returns an error when the desired-state path or its strict persisted state is invalid.
    pub fn new() -> Result<Self, PersistenceError> {
        let store = FileDesiredStateStore::new(profile_state_path("device-state.json")?);
        let mixer_store = FileMixerStateStore::new(profile_state_path("mixer-state.json")?);
        Self::with_components(
            LinuxInventory::new(),
            librewave_platform_linux::DiscoveryPaths::default(),
            Box::new(store),
            Box::new(mixer_store),
            |candidate| {
                Wave3UsbConnection::open(candidate)
                    .map(|connection| Box::new(connection) as Box<dyn ManagedWave3Connection>)
                    .map_err(admission_error)
            },
        )
    }

    fn with_components(
        inventory: LinuxInventory,
        paths: librewave_platform_linux::DiscoveryPaths,
        mut store: Box<dyn DesiredStateStore>,
        mut mixer_store: Box<dyn MixerStateStore>,
        connector: impl FnMut(
            &UsbDeviceCandidate,
        ) -> Result<Box<dyn ManagedWave3Connection>, AdmissionError>
        + 'static,
    ) -> Result<Self, PersistenceError> {
        let desired = store.load()?;
        validate_desired_state(&desired)?;
        let mixer = mixer_store.load()?;
        let mut state = State::default();
        let mut snapshot = state.snapshot().clone();
        snapshot.mixer = MixerSnapshot::inactive(mixer);
        ignore_reconciliation_event(state.replace(snapshot, librewave_core::Origin::Recovery));
        Ok(Self {
            inventory,
            paths,
            state,
            candidates: BTreeMap::new(),
            connector: Box::new(connector),
            connections: BTreeMap::new(),
            device_generations: BTreeMap::new(),
            desired,
            store,
            mixer_store,
            allocator: DeviceIdAllocator::default(),
        })
    }

    /// Creates a daemon with deterministic state for protocol and CLI tests.
    #[must_use]
    pub fn from_snapshot(snapshot: Snapshot) -> Self {
        let mixer_profile = snapshot.mixer.profile.clone();
        let mut state = State::default();
        ignore_reconciliation_event(state.replace(snapshot, librewave_core::Origin::Recovery));
        let allocator = DeviceIdAllocator::from_snapshot(state.snapshot());
        Self {
            inventory: LinuxInventory::new(),
            paths: librewave_platform_linux::DiscoveryPaths::default(),
            state,
            candidates: BTreeMap::new(),
            connector: Box::new(|_| Err(AdmissionError::Disconnected)),
            connections: BTreeMap::new(),
            device_generations: BTreeMap::new(),
            desired: DesiredState::default(),
            store: Box::new(MemoryStore::default()),
            mixer_store: Box::new(MemoryMixerStore { profile: mixer_profile }),
            allocator,
        }
    }

    #[cfg(test)]
    #[must_use]
    fn with_connector(
        snapshot: Snapshot,
        candidates: BTreeMap<DeviceId, UsbDeviceCandidate>,
        desired: DesiredState,
        store: Box<dyn DesiredStateStore>,
        connector: impl FnMut(
            &UsbDeviceCandidate,
        ) -> Result<Box<dyn ManagedWave3Connection>, AdmissionError>
        + 'static,
    ) -> Self {
        let mut daemon = Self::from_snapshot(snapshot);
        daemon.candidates = candidates;
        daemon.desired = desired;
        daemon.store = store;
        daemon.connector = Box::new(connector);
        daemon
    }

    #[cfg(test)]
    #[must_use]
    fn with_mixer_store(snapshot: Snapshot, mixer_store: Box<dyn MixerStateStore>) -> Self {
        let mut daemon = Self::from_snapshot(snapshot);
        daemon.mixer_store = mixer_store;
        daemon
    }

    #[cfg(test)]
    fn with_inventory(
        inventory: LinuxInventory,
        paths: librewave_platform_linux::DiscoveryPaths,
    ) -> Self {
        Self::with_components(
            inventory,
            paths,
            Box::new(MemoryStore::default()),
            Box::new(MemoryMixerStore::default()),
            |_| Err(AdmissionError::Disconnected),
        )
        .expect("memory store loads")
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
                ignore_reconciliation_event(self.refresh_inventory(origin));
                self.refresh_retained_connections()?;
                Ok(Response::Refreshed { snapshot: self.snapshot().clone() })
            }
            Command::GetStatus => {
                ignore_reconciliation_event(self.refresh_inventory(origin));
                Ok(Response::Status { snapshot: self.snapshot().clone() })
            }
            Command::ListDevices => {
                ignore_reconciliation_event(self.refresh_inventory(origin));
                Ok(Response::Devices { devices: self.snapshot().devices.clone() })
            }
            Command::InspectDevice { id } => {
                self.inspect(id, origin)?;
                let device = self.device(id)?;
                Ok(Response::DeviceInspection { device })
            }
            Command::SetWave3Control { id, expected_generation, control } => {
                let device = self.set_control(id, expected_generation, control, origin)?;
                Ok(Response::ControlChanged { device })
            }
            Command::GetMixer => Ok(Response::Mixer { mixer: self.snapshot().mixer.clone() }),
            Command::SetMixerRoute { expected_generation, source, target, route } => {
                let mixer =
                    self.set_mixer_route(expected_generation, source, target, route, origin)?;
                Ok(Response::MixerRouteChanged { mixer })
            }
        }
    }

    fn set_mixer_route(
        &mut self,
        expected_generation: MixerGeneration,
        source: SourceId,
        target: MixTarget,
        route: MixRoute,
        origin: librewave_core::Origin,
    ) -> Result<MixerSnapshot, IpcError> {
        let current = &self.snapshot().mixer.profile;
        if expected_generation != current.generation {
            return Err(IpcError {
                kind: IpcErrorKind::StaleMixerGeneration {
                    expected: expected_generation,
                    actual: current.generation,
                },
                message: format!(
                    "mixer generation {expected_generation} is stale; current generation is {}",
                    current.generation
                ),
            });
        }
        let source_snapshot = current.source(source).ok_or_else(|| IpcError {
            kind: IpcErrorKind::MixerSourceNotFound { id: source },
            message: format!("mixer source {source} is not configured"),
        })?;
        if source_snapshot.controls.route(target) == route {
            return Ok(self.snapshot().mixer.clone());
        }
        let next = current.with_route(source, target, route).map_err(|error| match error {
            librewave_core::MixerProfileError::GenerationExhausted => IpcError {
                kind: IpcErrorKind::MixerGenerationExhausted,
                message: "mixer generation is exhausted".to_owned(),
            },
            other => internal_error(format!("validated mixer profile update failed: {other}")),
        })?;
        match self.mixer_store.save(&next) {
            Ok(()) => {}
            Err(PersistenceError::SaveAfterRename(_)) => {
                return Err(IpcError {
                    kind: IpcErrorKind::MixerPersistenceAmbiguous,
                    message: "mixer state was replaced, but its crash durability is ambiguous; retained state was not advanced and retrying the command will converge it"
                        .to_owned(),
                });
            }
            Err(_) => {
                return Err(IpcError {
                    kind: IpcErrorKind::MixerPersistence,
                    message: "mixer state could not be persisted; retained state was not changed"
                        .to_owned(),
                });
            }
        }
        let mut snapshot = self.snapshot().clone();
        snapshot.mixer = MixerSnapshot::inactive(next);
        let event = self.state.replace(snapshot, origin);
        if event.is_none() {
            return Err(internal_error(
                "persisted mixer state did not change the retained snapshot".to_owned(),
            ));
        }
        Ok(self.snapshot().mixer.clone())
    }

    fn inspect(&mut self, id: DeviceId, origin: librewave_core::Origin) -> Result<(), IpcError> {
        self.device(id)?;
        if self.connections.get(&id).is_some_and(|owned| {
            matches!(owned.locked, None | Some(DeviceWriteLockReason::PersistenceAmbiguous))
        }) {
            return self.refresh_connection(id);
        }
        self.connections.remove(&id);
        let candidate =
            self.candidates.get(&id).cloned().ok_or_else(|| IpcError::device_not_found(id))?;
        let topology = candidate.topology.as_str().to_owned();
        let mut connection = match (self.connector)(&candidate) {
            Ok(connection) => connection,
            Err(error) => {
                let connection_state = if error == AdmissionError::Disconnected {
                    DeviceConnection::Disconnected
                } else {
                    DeviceConnection::Connected
                };
                self.update_admission(
                    id,
                    connection_state,
                    DeviceAdmissionSnapshot::Failed { error },
                    origin,
                )?;
                return Ok(());
            }
        };
        let api = connection.api();
        let observed = connection.observed().map_err(internal_error)?;
        let restore = match self.desired.get(&topology).cloned() {
            Some(device) if device.pending.is_some() => RestoreResult::Locked {
                config: Some(observed),
                reason: DeviceWriteLockReason::PersistenceAmbiguous,
            },
            Some(device) => Self::restore_desired(connection.as_mut(), observed, &device.managed),
            None => RestoreResult::Ready(observed),
        };
        if matches!(restore, RestoreResult::Disconnected) {
            self.update_admission(
                id,
                DeviceConnection::Disconnected,
                DeviceAdmissionSnapshot::Failed { error: AdmissionError::Disconnected },
                origin,
            )?;
            return Ok(());
        }
        let (config, locked) = match restore {
            RestoreResult::Ready(config) => (Some(config), None),
            RestoreResult::Locked { config, reason } => (config, Some(reason)),
            RestoreResult::Disconnected => unreachable!("handled above"),
        };
        let generation = self.next_device_generation(&topology);
        let write_access =
            locked.map_or(DeviceWriteAccess::Ready, |reason| DeviceWriteAccess::Locked { reason });
        self.update_admission(
            id,
            DeviceConnection::Connected,
            DeviceAdmissionSnapshot::Admitted { api, generation, write_access, config },
            origin,
        )?;
        self.connections.insert(id, OwnedConnection { topology, connection, locked });
        Ok(())
    }

    fn refresh_connection(&mut self, id: DeviceId) -> Result<(), IpcError> {
        let (api, topology, locked, outcome) = {
            let owned = self
                .connections
                .get_mut(&id)
                .ok_or_else(|| write_locked(DeviceWriteLockReason::NoOwnedConnection))?;
            (
                owned.connection.api(),
                owned.topology.clone(),
                owned.locked,
                owned.connection.refresh(),
            )
        };
        match outcome {
            ConnectionRefreshOutcome::Changed(config) => {
                let generation = self.next_device_generation(&topology);
                let write_access = locked.map_or(DeviceWriteAccess::Ready, |reason| {
                    DeviceWriteAccess::Locked { reason }
                });
                self.update_admission(
                    id,
                    DeviceConnection::Connected,
                    DeviceAdmissionSnapshot::Admitted {
                        api,
                        generation,
                        write_access,
                        config: Some(config),
                    },
                    librewave_core::Origin::Device,
                )
            }
            ConnectionRefreshOutcome::Unchanged(config) => {
                let generation = self
                    .device(id)?
                    .admission
                    .generation()
                    .ok_or_else(|| write_locked(DeviceWriteLockReason::NoOwnedConnection))?;
                let write_access = locked.map_or(DeviceWriteAccess::Ready, |reason| {
                    DeviceWriteAccess::Locked { reason }
                });
                self.update_admission(
                    id,
                    DeviceConnection::Connected,
                    DeviceAdmissionSnapshot::Admitted {
                        api,
                        generation,
                        write_access,
                        config: Some(config),
                    },
                    librewave_core::Origin::Device,
                )
            }
            ConnectionRefreshOutcome::Failed(error) => {
                self.connections.remove(&id);
                let connection = if error == AdmissionError::Disconnected {
                    DeviceConnection::Disconnected
                } else {
                    DeviceConnection::Connected
                };
                self.update_admission(
                    id,
                    connection,
                    DeviceAdmissionSnapshot::Failed { error },
                    librewave_core::Origin::Device,
                )
            }
        }
    }

    fn refresh_retained_connections(&mut self) -> Result<(), IpcError> {
        let ids: Vec<_> = self
            .connections
            .iter()
            .filter_map(|(id, owned)| {
                matches!(owned.locked, None | Some(DeviceWriteLockReason::PersistenceAmbiguous))
                    .then_some(*id)
            })
            .collect();
        for id in ids {
            self.refresh_connection(id)?;
        }
        Ok(())
    }

    fn set_control(
        &mut self,
        id: DeviceId,
        expected_generation: DeviceGeneration,
        control: Wave3Control,
        origin: librewave_core::Origin,
    ) -> Result<DeviceSnapshot, IpcError> {
        let (topology, current) = self.prepare_control(id, expected_generation, control)?;
        self.stage_control(id, &topology, control, current.clone(), origin)?;
        let outcome = self
            .connections
            .get_mut(&id)
            .expect("connection checked during control preparation")
            .connection
            .apply(control);
        self.settle_pending(id, &topology, &outcome, current, origin)?;
        self.finish_control(id, &topology, outcome, origin)?;
        self.device(id)
    }

    fn prepare_control(
        &self,
        id: DeviceId,
        expected_generation: DeviceGeneration,
        control: Wave3Control,
    ) -> Result<(String, Wave3ConfigSnapshot), IpcError> {
        let device = self.device(id)?;
        let (actual_generation, current) = match &device.admission {
            DeviceAdmissionSnapshot::Admitted { generation, config, write_access, .. } => {
                if let DeviceWriteAccess::Locked { reason } = write_access {
                    return Err(write_locked(*reason));
                }
                let config = config.clone().ok_or_else(|| IpcError {
                    kind: IpcErrorKind::DeviceWriteLocked {
                        reason: DeviceWriteLockReason::RestorationUnverified,
                    },
                    message: "the observed device configuration is unavailable".to_owned(),
                })?;
                (*generation, config)
            }
            DeviceAdmissionSnapshot::NotInspected | DeviceAdmissionSnapshot::Failed { .. } => {
                return Err(write_locked(DeviceWriteLockReason::NoOwnedConnection));
            }
        };
        if expected_generation != actual_generation {
            return Err(IpcError {
                kind: IpcErrorKind::StaleDeviceGeneration {
                    expected: expected_generation,
                    actual: actual_generation,
                },
                message: format!(
                    "device generation {expected_generation} is stale; current generation is {actual_generation}"
                ),
            });
        }
        let mut requested = current.clone();
        apply_control_snapshot(&mut requested, control).map_err(invalid_control)?;
        let topology = self
            .connections
            .get(&id)
            .map(|owned| owned.topology.clone())
            .ok_or_else(|| write_locked(DeviceWriteLockReason::NoOwnedConnection))?;
        Ok((topology, current))
    }

    fn stage_control(
        &mut self,
        id: DeviceId,
        topology: &str,
        control: Wave3Control,
        current: Wave3ConfigSnapshot,
        origin: librewave_core::Origin,
    ) -> Result<(), IpcError> {
        let mut staged = self.desired.clone();
        staged
            .stage(topology.to_owned(), control)
            .map_err(|()| write_locked(DeviceWriteLockReason::PersistenceAmbiguous))?;
        match self.store.save(&staged) {
            Ok(()) => {
                self.desired = staged;
                Ok(())
            }
            Err(PersistenceError::SaveAfterRename(error)) => {
                self.desired = staged;
                self.lock_pending(id, topology, Some(current), origin)?;
                Err(IpcError {
                    kind: IpcErrorKind::PersistenceAmbiguous,
                    message: format!(
                        "the pending control was not durably staged; no USB write was attempted: {error}"
                    ),
                })
            }
            Err(error) => Err(IpcError {
                kind: IpcErrorKind::Persistence,
                message: format!("{error}; no USB write was attempted"),
            }),
        }
    }

    fn settle_pending(
        &mut self,
        id: DeviceId,
        topology: &str,
        outcome: &ConnectionOutcome,
        current: Wave3ConfigSnapshot,
        origin: librewave_core::Origin,
    ) -> Result<(), IpcError> {
        let (settled, failure_message) =
            if matches!(outcome, ConnectionOutcome::Applied(_) | ConnectionOutcome::Unchanged(_)) {
                let mut promoted = self.desired.clone();
                promoted.promote(topology).map_err(|()| {
                    internal_error("pending control disappeared before promotion".to_owned())
                })?;
                (
                    promoted,
                    "the hardware change was verified but its desired state was not promoted",
                )
            } else {
                let mut cleared = self.desired.clone();
                cleared.clear_pending(topology).map_err(|()| {
                    internal_error("pending control disappeared before failure cleanup".to_owned())
                })?;
                (
                    cleared,
                    "the control transaction failed and its pending intent could not be cleared",
                )
            };
        match self.store.save(&settled) {
            Ok(()) => {
                self.desired = settled;
                Ok(())
            }
            Err(error) => {
                let observed = outcome_observed(outcome).or(Some(current));
                if matches!(outcome, ConnectionOutcome::Disconnected) {
                    self.connections.remove(&id);
                    self.update_admission(
                        id,
                        DeviceConnection::Disconnected,
                        DeviceAdmissionSnapshot::Failed { error: AdmissionError::Disconnected },
                        origin,
                    )?;
                } else {
                    self.lock_pending(id, topology, observed, origin)?;
                }
                Err(IpcError {
                    kind: IpcErrorKind::PersistenceAmbiguous,
                    message: format!("{failure_message}: {error}"),
                })
            }
        }
    }

    fn finish_control(
        &mut self,
        id: DeviceId,
        topology: &str,
        outcome: ConnectionOutcome,
        origin: librewave_core::Origin,
    ) -> Result<(), IpcError> {
        let api = self
            .connections
            .get(&id)
            .map(|owned| owned.connection.api())
            .ok_or_else(|| write_locked(DeviceWriteLockReason::NoOwnedConnection))?;
        match outcome {
            ConnectionOutcome::Applied(config) => {
                let generation = self.next_device_generation(topology);
                self.update_admission(
                    id,
                    DeviceConnection::Connected,
                    DeviceAdmissionSnapshot::Admitted {
                        api,
                        generation,
                        write_access: DeviceWriteAccess::Ready,
                        config: Some(config),
                    },
                    origin,
                )
            }
            ConnectionOutcome::Unchanged(config) => {
                let generation = self
                    .device(id)?
                    .admission
                    .generation()
                    .ok_or_else(|| write_locked(DeviceWriteLockReason::NoOwnedConnection))?;
                self.update_admission(
                    id,
                    DeviceConnection::Connected,
                    DeviceAdmissionSnapshot::Admitted {
                        api,
                        generation,
                        write_access: DeviceWriteAccess::Ready,
                        config: Some(config),
                    },
                    origin,
                )
            }
            ConnectionOutcome::Invalid(message) => Err(invalid_control(message)),
            ConnectionOutcome::StaleBaseline(config) => {
                let generation = self.next_device_generation(topology);
                self.update_admission(
                    id,
                    DeviceConnection::Connected,
                    DeviceAdmissionSnapshot::Admitted {
                        api,
                        generation,
                        write_access: DeviceWriteAccess::Ready,
                        config: Some(config),
                    },
                    librewave_core::Origin::Device,
                )?;
                Err(IpcError {
                    kind: IpcErrorKind::StaleDeviceBaseline,
                    message: format!(
                        "the device baseline changed; inspect device {id} and retry with its new generation"
                    ),
                })
            }
            ConnectionOutcome::Disconnected => {
                self.connections.remove(&id);
                self.update_admission(
                    id,
                    DeviceConnection::Disconnected,
                    DeviceAdmissionSnapshot::Failed { error: AdmissionError::Disconnected },
                    librewave_core::Origin::Device,
                )?;
                Err(IpcError {
                    kind: IpcErrorKind::DeviceDisconnected { id },
                    message: format!("device {id} disconnected during the control transaction"),
                })
            }
            ConnectionOutcome::Failed { observed, restoration_unverified, message } => {
                let reason = if restoration_unverified {
                    DeviceWriteLockReason::RestorationUnverified
                } else {
                    DeviceWriteLockReason::RestoreFailed
                };
                if let Some(owned) = self.connections.get_mut(&id) {
                    owned.locked = Some(reason);
                }
                let generation = self.next_device_generation(topology);
                self.update_admission(
                    id,
                    DeviceConnection::Connected,
                    DeviceAdmissionSnapshot::Admitted {
                        api,
                        generation,
                        write_access: DeviceWriteAccess::Locked { reason },
                        config: observed,
                    },
                    librewave_core::Origin::Recovery,
                )?;
                Err(IpcError { kind: IpcErrorKind::TransactionFailed, message })
            }
        }
    }

    fn lock_pending(
        &mut self,
        id: DeviceId,
        topology: &str,
        config: Option<Wave3ConfigSnapshot>,
        origin: librewave_core::Origin,
    ) -> Result<(), IpcError> {
        let reason = DeviceWriteLockReason::PersistenceAmbiguous;
        let api = self.connections.get(&id).map(|owned| owned.connection.api());
        if let Some(owned) = self.connections.get_mut(&id) {
            owned.locked = Some(reason);
        }
        let Some(api) = api else {
            return Err(write_locked(DeviceWriteLockReason::NoOwnedConnection));
        };
        let generation = self.next_device_generation(topology);
        self.update_admission(
            id,
            DeviceConnection::Connected,
            DeviceAdmissionSnapshot::Admitted {
                api,
                generation,
                write_access: DeviceWriteAccess::Locked { reason },
                config,
            },
            origin,
        )
    }

    fn restore_desired(
        connection: &mut dyn ManagedWave3Connection,
        mut observed: Wave3ConfigSnapshot,
        desired: &DesiredWave3Controls,
    ) -> RestoreResult {
        for control in controls_to_restore(&observed, desired) {
            match connection.apply(control) {
                ConnectionOutcome::Applied(config) | ConnectionOutcome::Unchanged(config) => {
                    observed = config;
                }
                ConnectionOutcome::Failed { observed: current, restoration_unverified, .. } => {
                    return RestoreResult::Locked {
                        config: current,
                        reason: if restoration_unverified {
                            DeviceWriteLockReason::RestorationUnverified
                        } else {
                            DeviceWriteLockReason::RestoreFailed
                        },
                    };
                }
                ConnectionOutcome::StaleBaseline(current) => {
                    return RestoreResult::Locked {
                        config: Some(current),
                        reason: DeviceWriteLockReason::RestoreFailed,
                    };
                }
                ConnectionOutcome::Disconnected => return RestoreResult::Disconnected,
                ConnectionOutcome::Invalid(_) => {
                    return RestoreResult::Locked {
                        config: Some(observed),
                        reason: DeviceWriteLockReason::RestoreFailed,
                    };
                }
            }
        }
        RestoreResult::Ready(observed)
    }

    fn update_admission(
        &mut self,
        id: DeviceId,
        connection: DeviceConnection,
        admission: DeviceAdmissionSnapshot,
        origin: librewave_core::Origin,
    ) -> Result<(), IpcError> {
        ignore_reconciliation_event(
            self.state
                .update_device_inspection(id, connection, admission, origin)
                .map_err(|_| IpcError::device_not_found(id))?,
        );
        Ok(())
    }

    fn device(&self, id: DeviceId) -> Result<DeviceSnapshot, IpcError> {
        self.snapshot()
            .devices
            .iter()
            .find(|device| device.id == id)
            .cloned()
            .ok_or_else(|| IpcError::device_not_found(id))
    }

    fn next_device_generation(&mut self, topology: &str) -> DeviceGeneration {
        let generation = self.device_generations.entry(topology.to_owned()).or_default();
        *generation = generation.saturating_add(1);
        DeviceGeneration(*generation)
    }

    fn refresh_inventory(&mut self, origin: librewave_core::Origin) -> Option<Event> {
        let (mut snapshot, candidates) =
            self.snapshot_from_inventory(self.inventory.inspect(&self.paths));
        snapshot.mixer = self.snapshot().mixer.clone();
        for device in &mut snapshot.devices {
            if let Some(previous) =
                self.snapshot().devices.iter().find(|previous| previous.id == device.id)
            {
                device.admission = previous.admission.clone();
            }
        }
        self.candidates = candidates;
        self.connections.retain(|id, _| self.candidates.contains_key(id));
        self.state.replace(snapshot, origin)
    }

    fn snapshot_from_inventory(
        &mut self,
        inventory: HostAudioInventory,
    ) -> (Snapshot, BTreeMap<DeviceId, UsbDeviceCandidate>) {
        snapshot_from_inventory(&mut self.allocator, inventory)
    }
}

#[derive(Default)]
struct MemoryStore {
    state: DesiredState,
}

impl DesiredStateStore for MemoryStore {
    fn load(&mut self) -> Result<DesiredState, PersistenceError> {
        Ok(self.state.clone())
    }

    fn save(&mut self, state: &DesiredState) -> Result<(), PersistenceError> {
        self.state = state.clone();
        Ok(())
    }
}

#[derive(Default)]
struct MemoryMixerStore {
    profile: MixerProfile,
}

impl MixerStateStore for MemoryMixerStore {
    fn load(&mut self) -> Result<MixerProfile, PersistenceError> {
        Ok(self.profile.clone())
    }

    fn save(&mut self, profile: &MixerProfile) -> Result<(), PersistenceError> {
        self.profile = profile.clone();
        Ok(())
    }
}

fn apply_control_snapshot(
    config: &mut Wave3ConfigSnapshot,
    control: Wave3Control,
) -> Result<(), String> {
    control::validate_control(control)?;
    match control {
        Wave3Control::InputGain(value) => config.input_gain = value,
        Wave3Control::MicrophoneMute(value) => config.input_mute = value,
        Wave3Control::Clipguard(value) => config.clipguard_enable = value,
        Wave3Control::LowCut(value) => config.lowcut_enable = value,
        Wave3Control::HeadphoneLevel(value) => config.headphone_volume = value,
        Wave3Control::HeadphoneMute(value) => config.headphone_mute = value,
        Wave3Control::MonitorMix(value) => config.direct_monitor = value,
        Wave3Control::KnobTarget(value) => config.volume_select = value,
        Wave3Control::AllLedsOff(value) => config.all_leds_off = value,
        Wave3Control::LedsFlip(value) => config.leds_flip = value,
        Wave3Control::GainLock(value) => config.gain_lock = value,
    }
    Ok(())
}

fn controls_to_restore(
    observed: &Wave3ConfigSnapshot,
    desired: &DesiredWave3Controls,
) -> Vec<Wave3Control> {
    let mut controls = Vec::new();
    let mut add = |control, changed| {
        if changed {
            controls.push(control);
        }
    };
    if let Some(value) = desired.input_gain {
        add(Wave3Control::InputGain(value), observed.input_gain != value);
    }
    if let Some(value) = desired.microphone_mute {
        add(Wave3Control::MicrophoneMute(value), observed.input_mute != value);
    }
    if let Some(value) = desired.clipguard {
        add(Wave3Control::Clipguard(value), observed.clipguard_enable != value);
    }
    if let Some(value) = desired.low_cut {
        add(Wave3Control::LowCut(value), observed.lowcut_enable != value);
    }
    if let Some(value) = desired.headphone_level {
        add(Wave3Control::HeadphoneLevel(value), observed.headphone_volume != value);
    }
    if let Some(value) = desired.headphone_mute {
        add(Wave3Control::HeadphoneMute(value), observed.headphone_mute != value);
    }
    if let Some(value) = desired.monitor_mix {
        add(Wave3Control::MonitorMix(value), observed.direct_monitor != value);
    }
    if let Some(value) = desired.knob_target {
        add(Wave3Control::KnobTarget(value), observed.volume_select != value);
    }
    if let Some(value) = desired.all_leds_off {
        add(Wave3Control::AllLedsOff(value), observed.all_leds_off != value);
    }
    if let Some(value) = desired.leds_flip {
        add(Wave3Control::LedsFlip(value), observed.leds_flip != value);
    }
    if let Some(value) = desired.gain_lock {
        add(Wave3Control::GainLock(value), observed.gain_lock != value);
    }
    controls
}

fn profile_state_path(file_name: &str) -> Result<PathBuf, PersistenceError> {
    if let Some(config) = std::env::var_os("XDG_CONFIG_HOME") {
        return Ok(PathBuf::from(config).join("librewave/profiles").join(file_name));
    }
    let home = std::env::var_os("HOME").ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "HOME is not set and XDG_CONFIG_HOME is unavailable",
        )
    })?;
    Ok(PathBuf::from(home).join(".config/librewave/profiles").join(file_name))
}

fn validate_desired_state(state: &DesiredState) -> Result<(), PersistenceError> {
    for (topology, device) in state.devices() {
        librewave_platform_linux::validate_usb_topology(topology).map_err(|error| {
            PersistenceError::InvalidState(format!(
                "desired-state topology `{topology}` is invalid: {error}"
            ))
        })?;
        for control in device.managed.controls().into_iter().chain(device.pending) {
            control::validate_control(control).map_err(|error| {
                PersistenceError::InvalidState(format!(
                    "desired state for topology `{topology}` is invalid: {error}"
                ))
            })?;
        }
    }
    Ok(())
}

fn invalid_control(message: String) -> IpcError {
    IpcError { kind: IpcErrorKind::InvalidControl, message }
}

fn internal_error(message: String) -> IpcError {
    IpcError { kind: IpcErrorKind::Internal, message }
}

fn write_locked(reason: DeviceWriteLockReason) -> IpcError {
    IpcError {
        kind: IpcErrorKind::DeviceWriteLocked { reason },
        message: format!("hardware writes are locked: {reason}"),
    }
}

fn outcome_observed(outcome: &ConnectionOutcome) -> Option<Wave3ConfigSnapshot> {
    match outcome {
        ConnectionOutcome::Applied(config)
        | ConnectionOutcome::Unchanged(config)
        | ConnectionOutcome::StaleBaseline(config) => Some(config.clone()),
        ConnectionOutcome::Failed { observed, .. } => observed.clone(),
        ConnectionOutcome::Invalid(_) | ConnectionOutcome::Disconnected => None,
    }
}

#[derive(Default)]
pub(crate) struct DeviceIdAllocator {
    ids_by_topology: BTreeMap<UsbTopology, DeviceId>,
    next_id: u64,
}

impl DeviceIdAllocator {
    fn from_snapshot(snapshot: &Snapshot) -> Self {
        let next_id =
            snapshot.devices.iter().map(|device| u64::from(device.id.0)).max().unwrap_or(0);
        Self { ids_by_topology: BTreeMap::new(), next_id }
    }

    pub(crate) fn id_for(&mut self, topology: &UsbTopology) -> DeviceId {
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

#[cfg(test)]
#[path = "tests.rs"]
mod tests;
