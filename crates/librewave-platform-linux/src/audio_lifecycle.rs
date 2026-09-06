//! Deterministic, host-independent capture-first audio lifecycle.
//!
//! The coordinator is host independent. The production Linux implementation
//! lives in [`crate::audio_host`].

use crate::UsbDeviceCandidate;
use librewave_core::{DELIBERATE_ENDPOINTS, EndpointId};

/// A physical node owned by the daemon and hidden from ordinary desktop
/// listings.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum HiddenPhysicalNode {
    /// The raw physical capture node.
    Capture,
    /// The raw physical playback node.
    Playback,
}

/// The exact physical nodes that must be hidden before graph ownership starts.
pub const HIDDEN_PHYSICAL_NODES: [HiddenPhysicalNode; 2] =
    [HiddenPhysicalNode::Capture, HiddenPhysicalNode::Playback];

/// An internal daemon-owned capture consumer.
///
/// This is intentionally not a desktop endpoint. The host implementation may
/// associate it with an internal stream or link, but it cannot publish it as a
/// keepalive sink through this contract.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CaptureConsumption {
    private: (),
}

impl CaptureConsumption {
    /// Creates a host-owned internal capture-consumption handle.
    #[must_use]
    pub const fn new() -> Self {
        Self { private: () }
    }
}

impl Default for CaptureConsumption {
    fn default() -> Self {
        Self::new()
    }
}

/// A confirmed capture observation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CaptureObservation {
    /// Whether the capture stream is active at the observation boundary.
    pub active: bool,
    /// The number of frames consumed since capture started.
    pub frames_consumed: u64,
}

/// A host operation used by the lifecycle state machine.
///
/// Implementations own all host mutation. The Linux implementation opens only
/// resources that belong to the admitted candidate.
pub trait AudioHost {
    /// The host-specific error type.
    type Error;

    /// Verifies that raw physical nodes are hidden and not exposed as ordinary
    /// desktop endpoints for this device.
    ///
    /// # Errors
    ///
    /// Returns the host-specific error when the verification cannot complete.
    fn verify_physical_nodes_hidden(
        &mut self,
        candidate: &UsbDeviceCandidate,
        hidden_nodes: &[HiddenPhysicalNode],
    ) -> Result<(), Self::Error>;

    /// Starts consuming physical capture before playback exists.
    ///
    /// # Errors
    ///
    /// Returns the host-specific error when capture cannot start.
    fn start_capture(
        &mut self,
        candidate: &UsbDeviceCandidate,
    ) -> Result<CaptureConsumption, Self::Error>;

    /// Observes whether physical capture is active and making progress.
    ///
    /// # Errors
    ///
    /// Returns the host-specific error when the observation cannot complete.
    fn observe_capture(
        &mut self,
        candidate: &UsbDeviceCandidate,
        capture: &CaptureConsumption,
    ) -> Result<CaptureObservation, Self::Error>;

    /// Opens and prepares physical playback after confirmed capture.
    ///
    /// # Errors
    ///
    /// Returns the host-specific error when playback cannot be prepared.
    fn prepare_playback(&mut self, candidate: &UsbDeviceCandidate) -> Result<(), Self::Error>;

    /// Publishes only the deliberate user-facing endpoints.
    ///
    /// # Errors
    ///
    /// Returns the host-specific error when endpoint publication cannot complete.
    fn publish_endpoints(
        &mut self,
        candidate: &UsbDeviceCandidate,
        endpoints: &[EndpointId],
    ) -> Result<(), Self::Error>;

    /// Restores saved software routes and endpoint levels.
    ///
    /// # Errors
    ///
    /// Returns the host-specific error when route restoration cannot complete.
    fn restore_software_routing(
        &mut self,
        candidate: &UsbDeviceCandidate,
    ) -> Result<(), Self::Error>;

    /// Removes every object created by the current attempt.
    ///
    /// Implementations must make this operation idempotent. The lifecycle
    /// calls it again when a previous cleanup attempt failed.
    ///
    /// # Errors
    ///
    /// Returns the host-specific error when cleanup cannot complete.
    fn teardown(&mut self) -> Result<(), Self::Error>;
}

/// A named state in which the graph cannot report ready.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DegradedState {
    /// No admitted device was detected.
    NoAdmittedDevice,
    /// The admitted device has no usable physical ALSA card.
    PhysicalDeviceUnavailable,
    /// Raw physical nodes are exposed as ordinary desktop endpoints.
    PhysicalNodesExposed,
    /// Physical capture could not start.
    CaptureStartFailed,
    /// Capture did not produce a confirmed active observation.
    CaptureNotConfirmed,
    /// Physical playback could not be opened and prepared.
    PlaybackPrepareFailed,
    /// Deliberate endpoints could not be published.
    EndpointPublicationFailed,
    /// Saved software routing could not be restored.
    RoutingRestoreFailed,
    /// The host audio services restarted and recovery is pending.
    HostRestarted,
    /// The admitted device disconnected.
    DeviceDisconnected,
    /// Cleanup itself failed and must be retried before recovery.
    TeardownFailed,
}

/// The externally observable lifecycle state.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LifecycleState {
    /// No graph resources are owned.
    Idle,
    /// The graph is fully started and all deliberate endpoints are restored.
    Ready,
    /// The graph is unavailable for the named reason.
    Degraded(DegradedState),
}

/// Failure returned by a lifecycle operation.
#[derive(Debug, Eq, PartialEq)]
pub enum LifecycleError<E> {
    /// No admitted device or confirmed physical graph is available.
    Unavailable(DegradedState),
    /// A graph is already active.
    AlreadyActive,
    /// A host step failed and cleanup completed.
    Host { state: DegradedState, source: E },
    /// Cleanup failed after a host step or lifecycle event.
    Cleanup { after: DegradedState, primary: CleanupPrimary<E>, teardown: E },
}

/// The primary condition that preceded a teardown failure.
#[derive(Debug, Eq, PartialEq)]
pub enum CleanupPrimary<E> {
    /// A host operation failed before teardown was attempted.
    HostStep { state: DegradedState, source: E },
    /// The lifecycle had a state-only failure and no host error to preserve.
    StateOnly { state: DegradedState },
}

/// The deterministic capture-first lifecycle coordinator.
#[derive(Debug)]
pub struct AudioLifecycle {
    state: LifecycleState,
    cleanup_complete: bool,
}

impl Default for AudioLifecycle {
    fn default() -> Self {
        Self { state: LifecycleState::Idle, cleanup_complete: true }
    }
}

impl AudioLifecycle {
    /// Creates an idle lifecycle coordinator.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Returns the current lifecycle state.
    #[must_use]
    pub const fn state(&self) -> LifecycleState {
        self.state
    }

    /// Runs the one normal capture-first startup path.
    ///
    /// # Errors
    ///
    /// Returns [`LifecycleError::Unavailable`] when no usable admitted device
    /// or confirmed capture is available, or a host/cleanup error otherwise.
    pub fn start<H: AudioHost>(
        &mut self,
        host: &mut H,
        candidate: Option<&UsbDeviceCandidate>,
    ) -> Result<(), LifecycleError<H::Error>> {
        if !self.cleanup_complete {
            return Err(LifecycleError::AlreadyActive);
        }
        if matches!(self.state, LifecycleState::Ready) {
            return Err(LifecycleError::AlreadyActive);
        }
        let Some(candidate) = candidate else {
            self.state = LifecycleState::Degraded(DegradedState::NoAdmittedDevice);
            return Err(LifecycleError::Unavailable(DegradedState::NoAdmittedDevice));
        };
        if candidate.alsa_cards.is_empty() {
            self.state = LifecycleState::Degraded(DegradedState::PhysicalDeviceUnavailable);
            return Err(LifecycleError::Unavailable(DegradedState::PhysicalDeviceUnavailable));
        }

        self.cleanup_complete = false;
        self.run_host_step(host, DegradedState::PhysicalNodesExposed, |host| {
            host.verify_physical_nodes_hidden(candidate, &HIDDEN_PHYSICAL_NODES)
        })?;
        let capture = match host.start_capture(candidate) {
            Ok(capture) => capture,
            Err(source) => {
                return self.host_failure(host, DegradedState::CaptureStartFailed, source);
            }
        };
        let observation = match host.observe_capture(candidate, &capture) {
            Ok(observation) => observation,
            Err(source) => {
                return self.host_failure(host, DegradedState::CaptureNotConfirmed, source);
            }
        };
        if !observation.active || observation.frames_consumed == 0 {
            return self.fail(host, DegradedState::CaptureNotConfirmed);
        }
        self.run_host_step(host, DegradedState::PlaybackPrepareFailed, |host| {
            host.prepare_playback(candidate)
        })?;
        self.run_host_step(host, DegradedState::EndpointPublicationFailed, |host| {
            host.publish_endpoints(candidate, DELIBERATE_ENDPOINTS)
        })?;
        self.run_host_step(host, DegradedState::RoutingRestoreFailed, |host| {
            host.restore_software_routing(candidate)
        })?;
        self.state = LifecycleState::Ready;
        Ok(())
    }

    /// Tears down the current graph. Repeated successful calls are no-ops.
    ///
    /// # Errors
    ///
    /// Returns [`LifecycleError::Cleanup`] when the host cannot remove the
    /// current graph objects.
    pub fn teardown<H: AudioHost>(&mut self, host: &mut H) -> Result<(), LifecycleError<H::Error>> {
        if self.cleanup_complete {
            self.state = LifecycleState::Idle;
            return Ok(());
        }
        match host.teardown() {
            Ok(()) => {
                self.cleanup_complete = true;
                self.state = LifecycleState::Idle;
                Ok(())
            }
            Err(source) => {
                self.state = LifecycleState::Degraded(DegradedState::TeardownFailed);
                Err(LifecycleError::Cleanup {
                    after: DegradedState::TeardownFailed,
                    primary: CleanupPrimary::StateOnly { state: DegradedState::TeardownFailed },
                    teardown: source,
                })
            }
        }
    }

    /// Handles a `PipeWire` or `WirePlumber` restart by reusing startup exactly.
    ///
    /// # Errors
    ///
    /// Returns the same host, cleanup, or unavailable errors as [`Self::start`].
    pub fn recover_after_host_restart<H: AudioHost>(
        &mut self,
        host: &mut H,
        candidate: Option<&UsbDeviceCandidate>,
    ) -> Result<(), LifecycleError<H::Error>> {
        if !self.cleanup_complete {
            self.teardown(host)?;
        }
        self.state = LifecycleState::Degraded(DegradedState::HostRestarted);
        self.start(host, candidate)
    }

    /// Handles device removal and leaves the lifecycle waiting for a new
    /// admitted candidate.
    ///
    /// # Errors
    ///
    /// Returns [`LifecycleError::Cleanup`] when the host cannot remove the
    /// current graph objects.
    pub fn handle_device_disconnect<H: AudioHost>(
        &mut self,
        host: &mut H,
    ) -> Result<(), LifecycleError<H::Error>> {
        if !self.cleanup_complete {
            self.teardown(host)?;
        }
        self.state = LifecycleState::Degraded(DegradedState::DeviceDisconnected);
        Ok(())
    }

    fn run_host_step<H, F>(
        &mut self,
        host: &mut H,
        state: DegradedState,
        operation: F,
    ) -> Result<(), LifecycleError<H::Error>>
    where
        H: AudioHost,
        F: FnOnce(&mut H) -> Result<(), H::Error>,
    {
        match operation(host) {
            Ok(()) => Ok(()),
            Err(source) => self.host_failure(host, state, source),
        }
    }

    fn host_failure<H: AudioHost>(
        &mut self,
        host: &mut H,
        state: DegradedState,
        source: H::Error,
    ) -> Result<(), LifecycleError<H::Error>> {
        self.state = LifecycleState::Degraded(state);
        match self.teardown(host) {
            Ok(()) => {
                self.state = LifecycleState::Degraded(state);
                Err(LifecycleError::Host { state, source })
            }
            Err(LifecycleError::Cleanup { teardown, .. }) => Err(LifecycleError::Cleanup {
                after: state,
                primary: CleanupPrimary::HostStep { state, source },
                teardown,
            }),
            Err(
                LifecycleError::AlreadyActive
                | LifecycleError::Host { .. }
                | LifecycleError::Unavailable(_),
            ) => {
                unreachable!("teardown only returns cleanup failures")
            }
        }
    }

    fn fail<H: AudioHost>(
        &mut self,
        host: &mut H,
        state: DegradedState,
    ) -> Result<(), LifecycleError<H::Error>> {
        self.state = LifecycleState::Degraded(state);
        match self.teardown(host) {
            Ok(()) => {
                self.state = LifecycleState::Degraded(state);
                Err(LifecycleError::Unavailable(state))
            }
            Err(LifecycleError::Cleanup { teardown, .. }) => Err(LifecycleError::Cleanup {
                after: state,
                primary: CleanupPrimary::StateOnly { state },
                teardown,
            }),
            Err(
                LifecycleError::AlreadyActive
                | LifecycleError::Host { .. }
                | LifecycleError::Unavailable(_),
            ) => {
                unreachable!("teardown only returns cleanup failures")
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{AlsaCardInfo, DeviceIdentity, UsbTopology};

    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    enum Operation {
        VerifyPhysicalNodesHidden,
        StartCapture,
        ObserveCapture,
        PreparePlayback,
        PublishEndpoints,
        RestoreRouting,
        Teardown,
    }

    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    struct FakeError(Operation);

    #[derive(Debug)]
    struct FakeHost {
        calls: Vec<Operation>,
        fail_at: Option<Operation>,
        fail_cleanup: bool,
        capture_observation: CaptureObservation,
    }

    impl Default for FakeHost {
        fn default() -> Self {
            Self {
                calls: Vec::new(),
                fail_at: None,
                fail_cleanup: false,
                capture_observation: CaptureObservation { active: true, frames_consumed: 1 },
            }
        }
    }

    impl FakeHost {
        fn call(&mut self, operation: Operation) -> Result<(), FakeError> {
            self.calls.push(operation);
            if self.fail_at == Some(operation) { Err(FakeError(operation)) } else { Ok(()) }
        }
    }

    impl AudioHost for FakeHost {
        type Error = FakeError;

        fn verify_physical_nodes_hidden(
            &mut self,
            _candidate: &UsbDeviceCandidate,
            hidden_nodes: &[HiddenPhysicalNode],
        ) -> Result<(), Self::Error> {
            assert_eq!(hidden_nodes, &HIDDEN_PHYSICAL_NODES);
            self.call(Operation::VerifyPhysicalNodesHidden)
        }

        fn start_capture(
            &mut self,
            _candidate: &UsbDeviceCandidate,
        ) -> Result<CaptureConsumption, Self::Error> {
            self.call(Operation::StartCapture)?;
            Ok(CaptureConsumption::new())
        }

        fn observe_capture(
            &mut self,
            _candidate: &UsbDeviceCandidate,
            _capture: &CaptureConsumption,
        ) -> Result<CaptureObservation, Self::Error> {
            self.calls.push(Operation::ObserveCapture);
            if self.fail_at == Some(Operation::ObserveCapture) {
                Err(FakeError(Operation::ObserveCapture))
            } else {
                Ok(self.capture_observation)
            }
        }

        fn prepare_playback(&mut self, _candidate: &UsbDeviceCandidate) -> Result<(), Self::Error> {
            self.call(Operation::PreparePlayback)
        }

        fn publish_endpoints(
            &mut self,
            _candidate: &UsbDeviceCandidate,
            endpoints: &[EndpointId],
        ) -> Result<(), Self::Error> {
            assert_eq!(endpoints, DELIBERATE_ENDPOINTS);
            self.call(Operation::PublishEndpoints)
        }

        fn restore_software_routing(
            &mut self,
            _candidate: &UsbDeviceCandidate,
        ) -> Result<(), Self::Error> {
            self.call(Operation::RestoreRouting)
        }

        fn teardown(&mut self) -> Result<(), Self::Error> {
            if self.fail_cleanup {
                self.calls.push(Operation::Teardown);
                Err(FakeError(Operation::Teardown))
            } else {
                self.call(Operation::Teardown)
            }
        }
    }

    fn candidate() -> UsbDeviceCandidate {
        UsbDeviceCandidate {
            identity: DeviceIdentity::wave3(),
            topology: UsbTopology::new("1-8.3"),
            alsa_cards: vec![AlsaCardInfo { number: 7, id: None, name: None }],
        }
    }

    const STARTUP_OPERATIONS: [Operation; 6] = [
        Operation::VerifyPhysicalNodesHidden,
        Operation::StartCapture,
        Operation::ObserveCapture,
        Operation::PreparePlayback,
        Operation::PublishEndpoints,
        Operation::RestoreRouting,
    ];

    #[test]
    fn happy_path_is_capture_first_and_publishes_only_deliberate_endpoints() {
        let mut lifecycle = AudioLifecycle::new();
        let mut host = FakeHost::default();
        lifecycle.start(&mut host, Some(&candidate())).expect("startup succeeds");
        assert_eq!(lifecycle.state(), LifecycleState::Ready);
        assert_eq!(host.calls, STARTUP_OPERATIONS);
    }

    #[test]
    fn every_host_step_failure_cleans_up_and_names_the_degraded_state() {
        let expected = [
            (Operation::VerifyPhysicalNodesHidden, DegradedState::PhysicalNodesExposed),
            (Operation::StartCapture, DegradedState::CaptureStartFailed),
            (Operation::ObserveCapture, DegradedState::CaptureNotConfirmed),
            (Operation::PreparePlayback, DegradedState::PlaybackPrepareFailed),
            (Operation::PublishEndpoints, DegradedState::EndpointPublicationFailed),
            (Operation::RestoreRouting, DegradedState::RoutingRestoreFailed),
        ];
        for (operation, state) in expected {
            let mut lifecycle = AudioLifecycle::new();
            let mut host = FakeHost { fail_at: Some(operation), ..FakeHost::default() };
            let result = lifecycle.start(&mut host, Some(&candidate()));
            assert!(
                matches!(result, Err(LifecycleError::Host { state: actual, .. }) if actual == state)
            );
            assert_eq!(lifecycle.state(), LifecycleState::Degraded(state));
            assert_eq!(host.calls.last(), Some(&Operation::Teardown));
        }
    }

    #[test]
    fn inactive_or_stalled_capture_never_starts_playback() {
        for observation in [
            CaptureObservation { active: false, frames_consumed: 1 },
            CaptureObservation { active: true, frames_consumed: 0 },
        ] {
            let mut lifecycle = AudioLifecycle::new();
            let mut host = FakeHost { capture_observation: observation, ..FakeHost::default() };
            let result = lifecycle.start(&mut host, Some(&candidate()));
            assert!(matches!(
                result,
                Err(LifecycleError::Unavailable(DegradedState::CaptureNotConfirmed))
            ));
            assert!(!host.calls.contains(&Operation::PreparePlayback));
            assert_eq!(host.calls.last(), Some(&Operation::Teardown));
        }
    }

    #[test]
    fn absent_or_unusable_device_does_not_touch_the_host() {
        let mut lifecycle = AudioLifecycle::new();
        let mut host = FakeHost::default();
        let result = lifecycle.start(&mut host, None);
        assert!(matches!(
            result,
            Err(LifecycleError::Unavailable(DegradedState::NoAdmittedDevice))
        ));
        assert!(host.calls.is_empty());

        let no_alsa = UsbDeviceCandidate { alsa_cards: Vec::new(), ..candidate() };
        let result = lifecycle.start(&mut host, Some(&no_alsa));
        assert!(matches!(
            result,
            Err(LifecycleError::Unavailable(DegradedState::PhysicalDeviceUnavailable))
        ));
        assert!(host.calls.is_empty());
    }

    #[test]
    fn teardown_is_idempotent_and_recovery_reuses_the_same_order() {
        let mut lifecycle = AudioLifecycle::new();
        let mut host = FakeHost::default();
        let device = candidate();
        lifecycle.start(&mut host, Some(&device)).expect("startup succeeds");
        lifecycle.teardown(&mut host).expect("teardown succeeds");
        lifecycle.teardown(&mut host).expect("repeated teardown succeeds");
        assert_eq!(
            host.calls,
            [
                Operation::VerifyPhysicalNodesHidden,
                Operation::StartCapture,
                Operation::ObserveCapture,
                Operation::PreparePlayback,
                Operation::PublishEndpoints,
                Operation::RestoreRouting,
                Operation::Teardown,
            ]
        );
        assert_eq!(
            host.calls.iter().filter(|&&operation| operation == Operation::Teardown).count(),
            1
        );

        host.calls.clear();
        lifecycle.recover_after_host_restart(&mut host, Some(&device)).expect("recovery succeeds");
        assert_eq!(lifecycle.state(), LifecycleState::Ready);
        assert_eq!(host.calls, STARTUP_OPERATIONS);
    }

    #[test]
    fn disconnect_tears_down_once_and_waits_for_reconnect() {
        let mut lifecycle = AudioLifecycle::new();
        let mut host = FakeHost::default();
        let device = candidate();
        lifecycle.start(&mut host, Some(&device)).expect("startup succeeds");
        lifecycle.handle_device_disconnect(&mut host).expect("disconnect succeeds");
        assert_eq!(lifecycle.state(), LifecycleState::Degraded(DegradedState::DeviceDisconnected));
        let calls = host.calls.len();
        lifecycle.handle_device_disconnect(&mut host).expect("repeated disconnect succeeds");
        assert_eq!(host.calls.len(), calls);
        lifecycle.start(&mut host, Some(&device)).expect("reconnect succeeds");
        assert_eq!(lifecycle.state(), LifecycleState::Ready);
    }

    #[test]
    fn cleanup_failure_is_named_and_retryable() {
        let mut lifecycle = AudioLifecycle::new();
        let mut host = FakeHost {
            fail_at: Some(Operation::StartCapture),
            fail_cleanup: true,
            ..FakeHost::default()
        };
        let error = lifecycle.start(&mut host, Some(&candidate())).expect_err("startup fails");
        assert_eq!(
            error,
            LifecycleError::Cleanup {
                after: DegradedState::CaptureStartFailed,
                primary: CleanupPrimary::HostStep {
                    state: DegradedState::CaptureStartFailed,
                    source: FakeError(Operation::StartCapture),
                },
                teardown: FakeError(Operation::Teardown),
            }
        );
        assert_eq!(lifecycle.state(), LifecycleState::Degraded(DegradedState::TeardownFailed));
        host.fail_at = None;
        host.fail_cleanup = false;
        lifecycle.teardown(&mut host).expect("cleanup retry succeeds");
        assert_eq!(lifecycle.state(), LifecycleState::Idle);
    }

    #[test]
    fn capture_not_confirmed_cleanup_failure_has_no_primary_host_error() {
        let mut lifecycle = AudioLifecycle::new();
        let mut host = FakeHost {
            fail_cleanup: true,
            capture_observation: CaptureObservation { active: false, frames_consumed: 1 },
            ..FakeHost::default()
        };
        let error = lifecycle.start(&mut host, Some(&candidate())).expect_err("startup fails");
        assert_eq!(
            error,
            LifecycleError::Cleanup {
                after: DegradedState::CaptureNotConfirmed,
                primary: CleanupPrimary::StateOnly { state: DegradedState::CaptureNotConfirmed },
                teardown: FakeError(Operation::Teardown),
            }
        );
        assert_eq!(lifecycle.state(), LifecycleState::Degraded(DegradedState::TeardownFailed));
    }
}
