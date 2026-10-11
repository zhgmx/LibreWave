//! Opt-in control-side owner for one physical audio attempt.

use librewave_core::{MixerProfile, ServiceState};
use librewave_engine::ClockAttemptEpoch;
use librewave_platform_linux::audio_host::{AudioAttemptPolicy, ComposedAudioHost};
use librewave_platform_linux::audio_lifecycle::{
    AudioHost, AudioLifecycle, HIDDEN_PHYSICAL_NODES, LifecycleState,
};
use librewave_platform_linux::{HostServiceInvocations, UsbDeviceCandidate};
use std::time::{Duration, Instant};

const RECONCILE_INTERVAL: Duration = Duration::from_secs(1);
const RETRY_INTERVAL: Duration = Duration::from_secs(2);

struct RunningAttempt {
    candidate: UsbDeviceCandidate,
    lifecycle: AudioLifecycle,
    host: ComposedAudioHost,
}

/// Lives on the daemon thread. No graph or ALSA handle crosses into IPC clients.
pub(super) struct AudioSupervisor {
    attempt: Option<RunningAttempt>,
    next_epoch: u64,
    retry_at: Instant,
    next_reconcile: Instant,
    invocations: Option<HostServiceInvocations>,
}

impl AudioSupervisor {
    pub(super) fn new() -> Self {
        let now = Instant::now();
        Self { attempt: None, next_epoch: 1, retry_at: now, next_reconcile: now, invocations: None }
    }

    pub(super) fn is_active(&self) -> bool {
        self.attempt
            .as_ref()
            .is_some_and(|attempt| attempt.lifecycle.state() == LifecycleState::Ready)
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
    ) {
        let now = Instant::now();
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
                Err(librewave_platform_linux::audio_host::LinuxAudioError::AudioAttempt {
                    phase: "reconciliation",
                    reason: "candidate or host service changed".to_owned(),
                })
            };
            let physical_hidden = if progress.is_ok() && now >= self.next_reconcile {
                self.next_reconcile = now + RECONCILE_INTERVAL;
                attempt
                    .host
                    .verify_physical_nodes_hidden(&attempt.candidate, &HIDDEN_PHYSICAL_NODES)
            } else {
                Ok(())
            };
            if let Err(error) = progress.and(physical_hidden) {
                eprintln!("librewaved: audio attempt lost: {error:?}");
                if let Err(error) = attempt.lifecycle.teardown(&mut attempt.host) {
                    eprintln!("librewaved: audio cleanup pending: {error:?}");
                    self.retry_at = now + RETRY_INTERVAL;
                    return;
                }
                self.attempt = None;
                self.invocations = None;
                self.retry_at = now + RETRY_INTERVAL;
            }
        }
        if self.attempt.is_some() || !services_ready || now < self.retry_at {
            return;
        }
        let Some(candidate) = candidate else {
            return;
        };
        let Ok(epoch) = ClockAttemptEpoch::try_new(self.next_epoch) else {
            eprintln!("librewaved: audio epoch exhausted");
            return;
        };
        let Some(next_epoch) = self.next_epoch.checked_add(1) else {
            eprintln!("librewaved: audio epoch exhausted");
            return;
        };
        self.next_epoch = next_epoch;
        let policy = match AudioAttemptPolicy::conservative(epoch) {
            Ok(policy) => policy,
            Err(error) => {
                eprintln!("librewaved: audio policy invalid: {error}");
                self.retry_at = now + RETRY_INTERVAL;
                return;
            }
        };
        let mut attempt = RunningAttempt {
            candidate: candidate.clone(),
            lifecycle: AudioLifecycle::new(),
            host: ComposedAudioHost::new(policy, profile.clone()),
        };
        match attempt.lifecycle.start(&mut attempt.host, Some(candidate)) {
            Ok(()) => {
                self.next_reconcile = Instant::now() + RECONCILE_INTERVAL;
                self.invocations = Some(invocations.clone());
                self.attempt = Some(attempt);
            }
            Err(error) => {
                eprintln!("librewaved: audio attempt unavailable: {error:?}");
                self.retry_at = Instant::now() + RETRY_INTERVAL;
                if attempt.lifecycle.state()
                    == LifecycleState::Degraded(
                        librewave_platform_linux::audio_lifecycle::DegradedState::TeardownFailed,
                    )
                {
                    self.attempt = Some(attempt);
                }
            }
        }
    }

    pub(super) fn stage_profile(&mut self, profile: &MixerProfile) -> bool {
        let Some(attempt) = self.attempt.as_mut() else {
            return false;
        };
        if attempt.lifecycle.state() != LifecycleState::Ready {
            return false;
        }
        if let Err(error) = attempt.host.stage_profile(profile) {
            eprintln!("librewaved: audio profile handoff failed: {error}");
            if let Err(cleanup) = attempt.lifecycle.teardown(&mut attempt.host) {
                eprintln!("librewaved: audio cleanup pending: {cleanup:?}");
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
