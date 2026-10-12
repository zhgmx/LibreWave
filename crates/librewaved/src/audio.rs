//! Opt-in control-side owner for one physical audio attempt.
mod meters;

use librewave_core::{
    AdmissionError, MixerInactiveReason, MixerProfile, MixerRuntimeState, ServiceState,
};
use librewave_engine::ClockAttemptEpoch;
use librewave_platform_linux::audio_host::{
    AudioAttemptPolicy, ComposedAudioHost, LinuxAudioError,
};
use librewave_platform_linux::audio_lifecycle::{
    AudioHost, AudioLifecycle, HIDDEN_PHYSICAL_NODES, LifecycleError, LifecycleState,
};
use librewave_platform_linux::{HostServiceInvocations, UsbDeviceCandidate};
use std::time::{Duration, Instant};

const RECONCILE_INTERVAL: Duration = Duration::from_secs(1);
const RETRY_INTERVAL: Duration = Duration::from_secs(2);

pub(super) trait SupervisedAudioHost: AudioHost<Error = LinuxAudioError> {
    fn applications(&self) -> librewave_core::ApplicationSnapshot {
        librewave_core::ApplicationSnapshot::Unavailable {
            reason: MixerInactiveReason::AudioHostEngineNotConnected,
        }
    }
    fn route_application(
        &mut self,
        _revision: u64,
        _application: librewave_core::ApplicationId,
        _source: librewave_core::SourceId,
    ) -> Result<(), String> {
        Err("Application routing is unavailable on this audio host.".into())
    }
    fn poll(&mut self) -> Result<(), LinuxAudioError>;
    fn stage_profile(&mut self, profile: &MixerProfile) -> Result<(), LinuxAudioError>;
    fn take_meters(&mut self) -> Option<librewave_engine::BlockMeters>;
}

impl SupervisedAudioHost for ComposedAudioHost {
    fn applications(&self) -> librewave_core::ApplicationSnapshot {
        Self::applications(self)
    }
    fn route_application(
        &mut self,
        revision: u64,
        application: librewave_core::ApplicationId,
        source: librewave_core::SourceId,
    ) -> Result<(), String> {
        Self::route_application(self, revision, application, source)
    }
    fn take_meters(&mut self) -> Option<librewave_engine::BlockMeters> {
        Self::take_meters(self)
    }
    fn poll(&mut self) -> Result<(), LinuxAudioError> {
        Self::poll(self)
    }

    fn stage_profile(&mut self, profile: &MixerProfile) -> Result<(), LinuxAudioError> {
        Self::stage_profile(self, profile)
    }
}

struct RunningAttempt<H> {
    candidate: UsbDeviceCandidate,
    lifecycle: AudioLifecycle,
    host: H,
}

type HostFactory<H> = dyn FnMut(ClockAttemptEpoch, &MixerProfile) -> Result<H, LinuxAudioError>;

/// Lives on the daemon thread. No graph or ALSA handle crosses into IPC clients.
pub(super) struct AudioSupervisor<H = ComposedAudioHost> {
    attempt: Option<RunningAttempt<H>>,
    factory: Box<HostFactory<H>>,
    enabled: bool,
    reason: MixerInactiveReason,
    next_epoch: u64,
    daemon_epoch: [u8; 16],
    retry_at: Instant,
    next_reconcile: Instant,
    invocations: Option<HostServiceInvocations>,
    meter_sequence: u64,
    meter_frame: Option<(librewave_core::MeterFrame, Instant)>,
    next_meter_read: Instant,
}

impl AudioSupervisor {
    pub(super) fn new(daemon_epoch: [u8; 16]) -> Self {
        let mut supervisor = Self::with_factory(|epoch, profile| {
            AudioAttemptPolicy::conservative(epoch)
                .map(|policy| ComposedAudioHost::new(policy, profile.clone()))
        });
        supervisor.daemon_epoch = daemon_epoch;
        supervisor
    }
}

impl<H: SupervisedAudioHost> AudioSupervisor<H> {
    pub(super) fn applications(&self) -> librewave_core::ApplicationSnapshot {
        if self.runtime() != MixerRuntimeState::Active {
            return librewave_core::ApplicationSnapshot::Unavailable { reason: self.reason };
        }
        let mut snapshot =
            self.attempt.as_ref().expect("active runtime has one attempt").host.applications();
        if let librewave_core::ApplicationSnapshot::Ready { generation, .. } = &mut snapshot {
            generation.epoch = self.next_epoch - 1;
            generation.daemon = self.daemon_epoch;
        }
        snapshot
    }
    pub(super) fn route_application(
        &mut self,
        expected: librewave_core::ApplicationGeneration,
        application: librewave_core::ApplicationId,
        source: librewave_core::SourceId,
    ) -> Result<librewave_core::ApplicationSnapshot, String> {
        if self.runtime() != MixerRuntimeState::Active {
            return Err("Audio must be ready before moving an application.".into());
        }
        if expected.daemon != self.daemon_epoch || expected.epoch != self.next_epoch - 1 {
            return Err("The audio attempt changed; refresh the application list.".into());
        }
        self.attempt.as_mut().expect("active runtime has one attempt").host.route_application(
            expected.revision,
            application,
            source,
        )?;
        Ok(self.applications())
    }
    fn with_factory(
        factory: impl FnMut(ClockAttemptEpoch, &MixerProfile) -> Result<H, LinuxAudioError> + 'static,
    ) -> Self {
        let now = Instant::now();
        Self {
            attempt: None,
            factory: Box::new(factory),
            enabled: true,
            reason: MixerInactiveReason::Starting,
            next_epoch: 1,
            daemon_epoch: [0; 16],
            retry_at: now,
            next_reconcile: now,
            invocations: None,
            meter_sequence: 0,
            meter_frame: None,
            next_meter_read: now,
        }
    }

    pub(super) fn runtime(&self) -> MixerRuntimeState {
        if self
            .attempt
            .as_ref()
            .is_some_and(|attempt| attempt.lifecycle.state() == LifecycleState::Ready)
        {
            MixerRuntimeState::Active
        } else {
            MixerRuntimeState::Inactive { reason: self.reason }
        }
    }

    /// Stopped owners need no inventory or hardware work after cleanup.
    pub(super) fn needs_tick(&self) -> bool {
        self.enabled || self.attempt.is_some()
    }

    /// Repeated start keeps the current owner, clock epoch, and retry deadline.
    pub(super) fn enable(&mut self) -> Result<(), ()> {
        if !self.enabled {
            if self.attempt.is_some() {
                return Err(());
            }
            self.enabled = true;
            self.reason = MixerInactiveReason::Starting;
            self.retry_at = Instant::now();
        }
        Ok(())
    }

    /// Cancel recovery before cleanup. A failed cleanup retains the sole owner.
    pub(super) fn stop(&mut self) -> Result<(), ()> {
        self.meter_frame = None;
        self.enabled = false;
        if let Some(attempt) = self.attempt.as_mut()
            && let Err(error) = attempt.lifecycle.teardown(&mut attempt.host)
        {
            eprintln!("librewaved: audio cleanup pending: {error:?}");
            self.reason = MixerInactiveReason::CleanupPending;
            self.retry_at = Instant::now() + RETRY_INTERVAL;
            return Err(());
        }
        self.attempt = None;
        self.invocations = None;
        self.reason = MixerInactiveReason::AudioHostEngineNotConnected;
        Ok(())
    }

    /// Reconciles an exact candidate and service state. A failed cleanup keeps
    /// the old owner and blocks a replacement attempt.
    pub(super) fn tick(
        &mut self,
        candidate: Option<&UsbDeviceCandidate>,
        pipewire: &ServiceState,
        wireplumber: &ServiceState,
        invocations: &HostServiceInvocations,
        profile: &MixerProfile,
        allow_start: bool,
    ) {
        let now = Instant::now();
        if !self.enabled {
            if self.attempt.is_some() && now >= self.retry_at {
                let _ = self.stop();
            }
            return;
        }
        let services_ready = matches!(pipewire, ServiceState::Available)
            && matches!(wireplumber, ServiceState::Available);
        if self
            .attempt
            .as_ref()
            .is_some_and(|attempt| attempt.lifecycle.state() != LifecycleState::Ready)
            && now < self.retry_at
        {
            return;
        }
        if let Some(attempt) = self.attempt.as_mut() {
            let same_candidate = candidate == Some(&attempt.candidate);
            let ready = attempt.lifecycle.state() == LifecycleState::Ready;
            let same_invocation =
                self.invocations.as_ref().is_none_or(|previous| previous == invocations);
            let progress = if ready && same_candidate && services_ready && same_invocation {
                attempt.host.poll()
            } else {
                Err(LinuxAudioError::AudioAttempt {
                    phase: "reconciliation",
                    reason: "candidate or host service changed".to_owned(),
                })
            };
            let physical_hidden = if allow_start && progress.is_ok() && now >= self.next_reconcile {
                self.next_reconcile = now + RECONCILE_INTERVAL;
                attempt
                    .host
                    .verify_physical_nodes_hidden(&attempt.candidate, &HIDDEN_PHYSICAL_NODES)
            } else {
                Ok(())
            };
            if let Err(error) = progress.and(physical_hidden) {
                eprintln!("librewaved: audio attempt lost: {error:?}");
                self.reason = host_failure_reason(&error);
                if let Err(error) = attempt.lifecycle.teardown(&mut attempt.host) {
                    eprintln!("librewaved: audio cleanup pending: {error:?}");
                    self.reason = MixerInactiveReason::CleanupPending;
                    self.retry_at = now + RETRY_INTERVAL;
                    return;
                }
                self.attempt = None;
                self.invocations = None;
                self.retry_at = now + RETRY_INTERVAL;
            }
        }
        if !services_ready {
            self.reason = MixerInactiveReason::HostServicesUnavailable;
            return;
        }
        let Some(candidate) = candidate else {
            self.reason = MixerInactiveReason::NoSingleDevice;
            return;
        };
        if !allow_start || self.attempt.is_some() || now < self.retry_at {
            return;
        }
        self.start_attempt(candidate, profile, invocations);
    }

    fn start_attempt(
        &mut self,
        candidate: &UsbDeviceCandidate,
        profile: &MixerProfile,
        invocations: &HostServiceInvocations,
    ) {
        let now = Instant::now();
        let Ok(epoch) = ClockAttemptEpoch::try_new(self.next_epoch) else {
            self.reason = MixerInactiveReason::AudioAttemptFailed;
            return;
        };
        let Some(next_epoch) = self.next_epoch.checked_add(1) else {
            self.reason = MixerInactiveReason::AudioAttemptFailed;
            return;
        };
        self.next_epoch = next_epoch;
        let host = match (self.factory)(epoch, profile) {
            Ok(host) => host,
            Err(error) => {
                eprintln!("librewaved: audio policy invalid: {error}");
                self.reason = host_failure_reason(&error);
                self.retry_at = now + RETRY_INTERVAL;
                return;
            }
        };
        let mut attempt =
            RunningAttempt { candidate: candidate.clone(), lifecycle: AudioLifecycle::new(), host };
        self.meter_frame = None;
        match attempt.lifecycle.start(&mut attempt.host, Some(candidate)) {
            Ok(()) => {
                self.next_reconcile = Instant::now() + RECONCILE_INTERVAL;
                self.invocations = Some(invocations.clone());
                self.attempt = Some(attempt);
            }
            Err(error) => {
                eprintln!("librewaved: audio attempt unavailable: {error:?}");
                self.reason = match &error {
                    LifecycleError::Host { source, .. } => host_failure_reason(source),
                    LifecycleError::Cleanup { .. } => MixerInactiveReason::CleanupPending,
                    _ => MixerInactiveReason::AudioAttemptFailed,
                };
                self.retry_at = Instant::now() + RETRY_INTERVAL;
                if self.reason == MixerInactiveReason::CleanupPending {
                    self.attempt = Some(attempt);
                }
            }
        }
    }

    pub(super) fn stage_profile(&mut self, profile: &MixerProfile) -> bool {
        let Some(attempt) = self.attempt.as_mut() else {
            return false;
        };
        if !self.enabled || attempt.lifecycle.state() != LifecycleState::Ready {
            return false;
        }
        if let Err(error) = attempt.host.stage_profile(profile) {
            eprintln!("librewaved: audio profile handoff failed: {error}");
            self.reason = host_failure_reason(&error);
            if let Err(cleanup) = attempt.lifecycle.teardown(&mut attempt.host) {
                eprintln!("librewaved: audio cleanup pending: {cleanup:?}");
                self.reason = MixerInactiveReason::CleanupPending;
            } else {
                self.attempt = None;
                self.invocations = None;
            }
            self.retry_at = Instant::now() + RETRY_INTERVAL;
            return false;
        }
        true
    }
}

fn host_failure_reason(error: &LinuxAudioError) -> MixerInactiveReason {
    match error {
        LinuxAudioError::DeviceAdmission(AdmissionError::PermissionDenied) => {
            MixerInactiveReason::DeviceAccessDenied
        }
        LinuxAudioError::DeviceAdmission(_) => MixerInactiveReason::DeviceAdmissionFailed,
        LinuxAudioError::PhysicalNodeExposed => MixerInactiveReason::PhysicalNodesExposed,
        _ => MixerInactiveReason::AudioAttemptFailed,
    }
}

#[cfg(test)]
mod tests;
