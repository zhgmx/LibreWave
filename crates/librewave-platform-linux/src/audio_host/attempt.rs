//! Explicit, daemon-owned composition of the physical workers and graph.
//!
//! Constructing this host does not open hardware. The daemon must opt in to
//! starting it; the ordinary [`super::LinuxAudioHost`] remains fail closed.

use super::alsa::{RealAlsaFacade, SelectedPcmCard};
use super::clock_bridge::observation::{CopySlotReader, copy_slot};
use super::clock_bridge::{
    BridgeError, BridgeState, ClockBridgeControl, ClockBridgeParts, GraphClockProcessor,
    PlaybackEgress,
};
use super::graph_bridge::ClockBridgeGraphProcessor;
use super::pipewire::RealPipeWireFacade;
use super::pipewire_graph::{ActiveGraph, AdapterPortMode, InactiveGraph};
use super::worker::{
    CaptureWorker, ParkedPlaybackWorker, PlaybackWorker, WorkerIoPolicy, WorkerPhase,
};
use super::{
    AlsaFacade, AudioResourceState, EndpointPlan, LinuxAudioError, PcmDirection, PipeWireFacade,
    ResourceActivity, Wave3PhysicalIoConfig, endpoint_plans,
};
use crate::UsbDeviceCandidate;
use crate::audio_lifecycle::{
    AudioHost, CaptureConsumption, CaptureObservation, HIDDEN_PHYSICAL_NODES, HiddenPhysicalNode,
};
use librewave_core::{DELIBERATE_ENDPOINTS, EndpointId, MixerProfile};
use librewave_engine::{
    ClockAttemptEpoch, ClockDeltaBounds, ClockRateEstimatorConfig, MeterReader,
    RateMatchControllerConfig, RateMatchRatioBounds, RateMatcherConfig,
};
use std::num::NonZeroU32;
use std::time::{Duration, Instant};

const POLL_INTERVAL: Duration = Duration::from_millis(5);

#[allow(clippy::needless_pass_by_value, reason = "keeps error conversion at the attempt boundary")]
fn attempt_error(phase: &'static str, reason: impl ToString) -> LinuxAudioError {
    LinuxAudioError::AudioAttempt { phase, reason: reason.to_string() }
}

fn wave3_capture_rate_estimator(
    bounds: RateMatchRatioBounds,
) -> Result<ClockRateEstimatorConfig, LinuxAudioError> {
    // ALSA status timestamps and ring availability are sampled together, but
    // USB packet delivery can move the ring cursor within one short period.
    // Require enough running-clock evidence before testing the rate ratio.
    // Playback must begin before its 16,384-frame startup FIFO fills.
    let delta = ClockDeltaBounds::try_new(8_192, 1_000_000_000, 160_000_000, 60_000_000_000)
        .map_err(|error| attempt_error("policy", error))?;
    Ok(ClockRateEstimatorConfig::new(delta, delta, 2_000_000_000, bounds))
}

trait GraphRuntime {
    fn iterate(&mut self, timeout: Duration);
    fn cycles(&self) -> u64;
    fn fault_reason(&self) -> Option<String>;
    fn startup_note(&self) -> Option<String> {
        None
    }
    fn teardown(&mut self) -> Result<(), LinuxAudioError>;
}

trait AdmissionGate {
    fn admit(&mut self, candidate: &UsbDeviceCandidate) -> Result<(), LinuxAudioError>;
}

struct ReadOnlyAdmission;

impl AdmissionGate for ReadOnlyAdmission {
    fn admit(&mut self, candidate: &UsbDeviceCandidate) -> Result<(), LinuxAudioError> {
        crate::probe_wave3_usb(candidate)
            .map(|_| ())
            .map_err(|error| LinuxAudioError::DeviceAdmission(crate::admission_error(error)))
    }
}

trait GraphFactory {
    fn activate(
        &mut self,
        remote: &str,
        plans: &[EndpointPlan],
        quantum: NonZeroU32,
        processor: ClockBridgeGraphProcessor,
    ) -> Result<Box<dyn GraphRuntime>, LinuxAudioError>;
}

struct RealGraphFactory;
struct RealGraphRuntime(ActiveGraph<ClockBridgeGraphProcessor>);

impl GraphFactory for RealGraphFactory {
    fn activate(
        &mut self,
        remote: &str,
        plans: &[EndpointPlan],
        quantum: NonZeroU32,
        processor: ClockBridgeGraphProcessor,
    ) -> Result<Box<dyn GraphRuntime>, LinuxAudioError> {
        let graph = InactiveGraph::construct(
            remote,
            plans,
            quantum,
            AdapterPortMode::WirePlumber,
            processor,
        )
        .map_err(|error| attempt_error("graph construction", error))?
        .activate()
        .map_err(|error| attempt_error("graph activation", error))?;
        Ok(Box::new(RealGraphRuntime(graph)))
    }
}

impl GraphRuntime for RealGraphRuntime {
    fn iterate(&mut self, timeout: Duration) {
        self.0.iterate(timeout);
    }
    fn cycles(&self) -> u64 {
        self.0.diagnostics().process_cycles
    }
    fn fault_reason(&self) -> Option<String> {
        let diagnostics = self.0.diagnostics();
        diagnostics.failed.then(|| {
            diagnostics.faults.into_iter().flatten().next().map_or_else(
                || "PipeWire callback faulted without a record".to_owned(),
                |record| format!("PipeWire callback fault: {record:?}; complete cycles {}, observed System link generation {}", diagnostics.process_cycles, diagnostics.system_link_word),
            )
        })
    }
    fn startup_note(&self) -> Option<String> {
        let diagnostics = self.0.diagnostics();
        (diagnostics.process_cycles == 0 && diagnostics.startup_incomplete_cycles != 0).then(|| {
            format!(
                "{} incomplete graph cycles; last missing mask {:#x}, quantum {}, driver {}",
                diagnostics.startup_incomplete_cycles,
                diagnostics.startup_missing_mask,
                diagnostics.startup_quantum,
                diagnostics.startup_driver,
            )
        })
    }
    fn teardown(&mut self) -> Result<(), LinuxAudioError> {
        self.0.teardown().map_err(|error| attempt_error("graph teardown", error))
    }
}

/// Explicit, provisional Wave:3 timing policy for an opt-in audio attempt.
///
/// The large FIFO targets favor startup headroom over latency. They are not
/// evidence of a validated live latency or a supported hardware setup.
#[derive(Clone, Debug)]
pub struct AudioAttemptPolicy {
    physical: Wave3PhysicalIoConfig,
    bridge: super::ClockBridgeConfig,
    worker: WorkerIoPolicy,
    quantum: NonZeroU32,
    startup_timeout: Duration,
    progress_timeout: Duration,
    remote_name: String,
}

impl AudioAttemptPolicy {
    /// Builds the named provisional policy. This does not start audio.
    ///
    /// # Errors
    ///
    /// Returns a construction error if any checked resource contract changes.
    pub fn conservative(epoch: ClockAttemptEpoch) -> Result<Self, LinuxAudioError> {
        const FIFO: usize = 16_384;
        const TARGET: usize = 8_192;
        const GUARD: usize = 4_096;
        const MAX_QUANTUM: usize = 1_024;
        let physical = Wave3PhysicalIoConfig::try_new(1_024, 8_192, 256, 2_048)
            .map_err(|error| attempt_error("policy", error))?;
        let bounds = RateMatchRatioBounds::try_new(2.0, 0.98, 1.02)
            .map_err(|error| attempt_error("policy", error))?;
        let capture_estimator = wave3_capture_rate_estimator(bounds)?;
        // Playback status includes PCM start and USB scheduling delay. Its
        // estimator can wait longer because its worker is already consuming
        // the playback FIFO.
        let playback_delta =
            ClockDeltaBounds::try_new(32_768, 1_000_000_000, 640_000_000, 60_000_000_000)
                .map_err(|error| attempt_error("policy", error))?;
        let playback_estimator =
            ClockRateEstimatorConfig::new(playback_delta, playback_delta, 2_000_000_000, bounds);
        let direction = |channels, output, estimator| {
            let matcher = RateMatcherConfig::try_new(output, channels, bounds)
                .map_err(|error| attempt_error("policy", error))?;
            let controller = RateMatchControllerConfig::try_new(
                TARGET, 0.002, 0.000_002, 10_000.0, 0.000_05, bounds,
            )
            .map_err(|error| attempt_error("policy", error))?;
            super::DirectionBridgeConfig::try_new(FIFO, GUARD, matcher, controller, estimator)
                .map_err(|error| attempt_error("policy", error))
        };
        let bridge = super::ClockBridgeConfig::try_new(
            epoch,
            MAX_QUANTUM,
            direction(1, MAX_QUANTUM, capture_estimator)?,
            direction(2, 256, playback_estimator)?,
        )
        .map_err(|error| attempt_error("policy", error))?;
        let worker =
            WorkerIoPolicy::try_new(10, 4).map_err(|error| attempt_error("policy", error))?;
        Ok(Self {
            physical,
            bridge,
            worker,
            quantum: NonZeroU32::new(256).ok_or_else(|| attempt_error("policy", "zero quantum"))?,
            startup_timeout: Duration::from_secs(8),
            progress_timeout: Duration::from_secs(1),
            remote_name: "pipewire-0".to_owned(),
        })
    }
}

/// One normal hardware writer and all resources for its audio attempt.
pub struct ComposedAudioHost {
    pipewire: Box<dyn PipeWireFacade>,
    alsa: Box<dyn AlsaFacade>,
    graph_factory: Box<dyn GraphFactory>,
    admission: Box<dyn AdmissionGate>,
    policy: AudioAttemptPolicy,
    profile: MixerProfile,
    selected: Option<SelectedPcmCard>,
    capture: Option<CaptureWorker>,
    parked_playback: Option<ParkedPlaybackWorker>,
    playback: Option<PlaybackWorker>,
    pending_graph: Option<GraphClockProcessor>,
    pending_playback: Option<PlaybackEgress>,
    graph: Option<Box<dyn GraphRuntime>>,
    graph_fault_reader: Option<CopySlotReader<super::clock_bridge::BridgeError>>,
    control: Option<ClockBridgeControl>,
    meters: Option<MeterReader>,
    capture_frames: u64,
    last_capture: Option<Instant>,
    last_playback: Option<Instant>,
    last_graph: Option<Instant>,
    graph_cycles: u64,
    cleanup_failed: bool,
}

impl ComposedAudioHost {
    /// Takes one actual processed-block observation without waiting.
    pub fn take_meters(&mut self) -> Option<librewave_engine::BlockMeters> {
        self.meters.as_mut().and_then(MeterReader::try_take)
    }

    /// Constructs an inert host. Only [`crate::audio_lifecycle::AudioLifecycle::start`]
    /// can start its physical attempt.
    #[must_use]
    #[allow(
        clippy::large_types_passed_by_value,
        reason = "the host takes ownership of one checked attempt policy"
    )]
    pub fn new(policy: AudioAttemptPolicy, profile: MixerProfile) -> Self {
        let physical = policy.physical;
        Self::with_adapters(
            policy,
            profile,
            Box::new(RealPipeWireFacade::default()),
            Box::new(RealAlsaFacade::new(physical)),
            Box::new(RealGraphFactory),
            Box::new(ReadOnlyAdmission),
        )
    }

    #[allow(
        clippy::large_types_passed_by_value,
        reason = "the host takes ownership of one checked attempt policy"
    )]
    fn with_adapters(
        policy: AudioAttemptPolicy,
        profile: MixerProfile,
        pipewire: Box<dyn PipeWireFacade>,
        alsa: Box<dyn AlsaFacade>,
        graph_factory: Box<dyn GraphFactory>,
        admission: Box<dyn AdmissionGate>,
    ) -> Self {
        Self {
            pipewire,
            alsa,
            graph_factory,
            admission,
            policy,
            profile,
            selected: None,
            capture: None,
            parked_playback: None,
            playback: None,
            pending_graph: None,
            pending_playback: None,
            graph: None,
            graph_fault_reader: None,
            control: None,
            meters: None,
            capture_frames: 0,
            last_capture: None,
            last_playback: None,
            last_graph: None,
            graph_cycles: 0,
            cleanup_failed: false,
        }
    }

    /// Reads current owned resources without exposing device identifiers.
    #[must_use]
    pub fn resource_state(&self) -> AudioResourceState {
        AudioResourceState {
            pipewire_connection_owned: self.pipewire.owns_connection(),
            capture: if self.capture.is_some() {
                ResourceActivity::Active
            } else {
                ResourceActivity::Closed
            },
            playback: if self.playback.is_some() {
                ResourceActivity::Active
            } else if self.parked_playback.is_some() {
                ResourceActivity::Starting
            } else {
                ResourceActivity::Closed
            },
            published_endpoints: if self.graph.is_some() {
                DELIBERATE_ENDPOINTS.to_vec()
            } else {
                Vec::new()
            },
            not_ready: None,
        }
    }

    fn graph_step(&mut self, phase: &'static str) -> Result<BridgeState, LinuxAudioError> {
        let graph = self.graph.as_mut().ok_or_else(|| attempt_error(phase, "graph is absent"))?;
        graph.iterate(POLL_INTERVAL);
        if let Some(reason) = graph.fault_reason() {
            let reason = self
                .graph_fault_reader
                .as_mut()
                .and_then(|reader| reader.try_take().ok())
                .map_or(reason.clone(), |error| format!("{reason}; bridge: {error:?}"));
            return Err(attempt_error(phase, reason));
        }
        let state = self
            .control
            .as_mut()
            .ok_or_else(|| attempt_error(phase, "bridge control is absent"))?
            .progress_priming_check()
            .map_err(|error| attempt_error(phase, error))?;
        if state == BridgeState::Faulted {
            let startup =
                graph.startup_note().unwrap_or_else(|| "startup mapping unavailable".to_owned());
            // A worker publishes its terminal cause before it marks the
            // bridge faulted. A bridge boundary can fault just before the
            // worker exits, so allow one bounded control-side observation.
            let deadline = Instant::now() + POLL_INTERVAL;
            loop {
                if let Some(terminal) = self.capture.as_mut().and_then(CaptureWorker::try_terminal)
                {
                    return Err(attempt_error(
                        phase,
                        format!("capture worker: {terminal:?}; {startup}"),
                    ));
                }
                if let Some(terminal) =
                    self.playback.as_mut().and_then(PlaybackWorker::try_terminal)
                {
                    return Err(attempt_error(
                        phase,
                        format!("playback worker: {terminal:?}; {startup}"),
                    ));
                }
                if Instant::now() >= deadline {
                    return Err(attempt_error(
                        phase,
                        format!("clock bridge faulted without worker cause; {startup}"),
                    ));
                }
                std::thread::yield_now();
            }
        }
        if self.capture.as_ref().is_some_and(CaptureWorker::is_finished) {
            return Err(attempt_error(phase, "capture worker stopped"));
        }
        if self.playback.as_ref().is_some_and(PlaybackWorker::is_finished) {
            return Err(attempt_error(phase, "playback worker stopped"));
        }
        Ok(state)
    }

    fn wait_for_graph_state(
        &mut self,
        phase: &'static str,
        ready: impl Fn(BridgeState) -> bool,
    ) -> Result<(), LinuxAudioError> {
        let deadline = Instant::now() + self.policy.startup_timeout;
        loop {
            let state = self.graph_step(phase)?;
            if ready(state) {
                return Ok(());
            }
            if Instant::now() >= deadline {
                return Err(attempt_error(phase, format!("startup timed out in {state:?}")));
            }
        }
    }

    /// Advances the graph and checks bounded progress. On error the daemon
    /// must tear down the whole lifecycle before it starts another attempt.
    ///
    /// # Errors
    ///
    /// Returns the first stopped worker, callback fault, bridge fault, or
    /// progress timeout.
    pub fn poll(&mut self) -> Result<(), LinuxAudioError> {
        let state = self.graph_step("progress")?;
        if state != BridgeState::Active {
            return Err(attempt_error("progress", format!("attempt is in {state:?}")));
        }
        let now = Instant::now();
        if let Some(capture) = self.capture.as_mut()
            && let Some(progress) = capture.try_progress()
        {
            self.capture_frames = progress.report.application_end_frame;
            self.last_capture = Some(now);
        }
        if let Some(playback) = self.playback.as_mut()
            && playback.try_boundary().is_some()
        {
            self.last_playback = Some(now);
        }
        let cycles = self
            .graph
            .as_ref()
            .ok_or_else(|| attempt_error("progress", "graph is absent"))?
            .cycles();
        if cycles != self.graph_cycles {
            self.graph_cycles = cycles;
            self.last_graph = Some(now);
        }
        for (name, last) in [
            ("capture", self.last_capture),
            ("playback", self.last_playback),
            ("graph", self.last_graph),
        ] {
            if last.is_none_or(|sample| now.duration_since(sample) > self.policy.progress_timeout) {
                return Err(attempt_error("progress", format!("{name} made no progress")));
            }
        }
        Ok(())
    }

    /// Stages a persisted software mixer profile for the next graph boundary.
    ///
    /// # Errors
    ///
    /// Returns a bounded-handoff or profile error; the caller must retain the
    /// old live profile when this fails.
    pub fn stage_profile(&mut self, profile: &MixerProfile) -> Result<(), LinuxAudioError> {
        self.control
            .as_mut()
            .ok_or_else(|| attempt_error("profile", "bridge control is absent"))?
            .try_stage_profile(profile)
            .map_err(|error| attempt_error("profile", error))?;
        self.profile = profile.clone();
        Ok(())
    }
}

impl AudioHost for ComposedAudioHost {
    type Error = LinuxAudioError;

    fn verify_physical_nodes_hidden(
        &mut self,
        candidate: &UsbDeviceCandidate,
        hidden_nodes: &[HiddenPhysicalNode],
    ) -> Result<(), Self::Error> {
        if hidden_nodes != HIDDEN_PHYSICAL_NODES.as_slice() {
            return Err(LinuxAudioError::InvalidHiddenPhysicalNodeContract);
        }
        self.pipewire.verify_physical_nodes_hidden(candidate)
    }

    fn start_capture(
        &mut self,
        candidate: &UsbDeviceCandidate,
    ) -> Result<CaptureConsumption, Self::Error> {
        self.admission.admit(candidate)?;
        let parts = ClockBridgeParts::new(self.policy.physical, &self.profile, self.policy.bridge)
            .map_err(|error| attempt_error("bridge construction", error))?;
        let selected = self.alsa.select(candidate)?;
        let pcm = self.alsa.open_capture(candidate, &selected)?;
        let ClockBridgeParts { capture_ingress, graph, playback, mut control, meters } = parts;
        control.start_capture().map_err(|error| attempt_error("capture priming", error))?;
        let worker = CaptureWorker::start(
            pcm,
            capture_ingress,
            self.policy.physical.parameters(PcmDirection::Capture),
            self.policy.worker,
        )
        .map_err(|error| attempt_error("capture start", error))?;
        self.selected = Some(selected);
        self.capture = Some(worker);
        self.pending_graph = Some(graph);
        self.pending_playback = Some(playback);
        self.control = Some(control);
        self.meters = Some(meters);
        Ok(CaptureConsumption::new())
    }

    fn observe_capture(
        &mut self,
        _candidate: &UsbDeviceCandidate,
        _capture: &CaptureConsumption,
    ) -> Result<CaptureObservation, Self::Error> {
        let deadline = Instant::now() + self.policy.startup_timeout;
        loop {
            let capture = self
                .capture
                .as_mut()
                .ok_or_else(|| attempt_error("capture observation", "worker is absent"))?;
            if let Some(progress) = capture.try_progress() {
                self.capture_frames = progress.report.application_end_frame;
                self.last_capture = Some(Instant::now());
            }
            if capture.is_finished() {
                return Err(attempt_error("capture observation", "worker stopped"));
            }
            let state = self
                .control
                .as_ref()
                .ok_or_else(|| attempt_error("capture observation", "bridge is absent"))?
                .state();
            if state == BridgeState::CaptureFilterDelay && self.capture_frames > 0 {
                return Ok(CaptureObservation {
                    active: true,
                    frames_consumed: self.capture_frames,
                });
            }
            if state == BridgeState::Faulted || Instant::now() >= deadline {
                return Err(attempt_error(
                    "capture observation",
                    format!("capture did not prime: {state:?}"),
                ));
            }
            std::thread::sleep(POLL_INTERVAL);
        }
    }

    fn prepare_playback(&mut self, candidate: &UsbDeviceCandidate) -> Result<(), Self::Error> {
        if self.capture_frames == 0 {
            return Err(attempt_error("playback preparation", "capture was not confirmed"));
        }
        let selected = self
            .selected
            .as_ref()
            .ok_or_else(|| attempt_error("playback preparation", "card is absent"))?;
        let pcm = self.alsa.open_playback(candidate, selected)?;
        let egress = self
            .pending_playback
            .take()
            .ok_or_else(|| attempt_error("playback preparation", "bridge egress is absent"))?;
        self.parked_playback = Some(
            ParkedPlaybackWorker::new(
                pcm,
                egress,
                self.policy.physical.parameters(PcmDirection::Playback),
                self.policy.worker,
            )
            .map_err(|error| attempt_error("playback preparation", error))?,
        );
        Ok(())
    }

    fn publish_endpoints(
        &mut self,
        _candidate: &UsbDeviceCandidate,
        endpoints: &[EndpointId],
    ) -> Result<(), Self::Error> {
        let plans: Vec<EndpointPlan> = endpoint_plans(endpoints)?;
        if endpoints != DELIBERATE_ENDPOINTS {
            return Err(attempt_error("graph publication", "endpoint order or count is invalid"));
        }
        let graph = self
            .pending_graph
            .take()
            .ok_or_else(|| attempt_error("graph publication", "bridge processor is absent"))?;
        let (fault_reporter, fault_reader) = copy_slot();
        self.graph_fault_reader = Some(fault_reader);
        let processor = ClockBridgeGraphProcessor::new(self.policy.bridge.attempt_epoch(), graph)
            .report_faults(fault_reporter);
        self.graph = Some(self.graph_factory.activate(
            &self.policy.remote_name,
            &plans,
            self.policy.quantum,
            processor,
        )?);
        self.wait_for_graph_state("monitor mix queue", |state| {
            state == BridgeState::PlaybackFilterDelay
        })?;
        let parked = self
            .parked_playback
            .take()
            .ok_or_else(|| attempt_error("playback activation", "parked worker is absent"))?;
        self.playback = Some(
            parked
                .activate_thread_after_monitor_mix_queued()
                .map_err(|error| attempt_error("playback activation", error))?,
        );
        self.wait_for_graph_state("bridge priming", |state| state == BridgeState::Primed)?;
        let deadline = Instant::now() + self.policy.startup_timeout;
        while self.capture.as_ref().is_none_or(|worker| worker.phase() != WorkerPhase::PrimedParked)
            || self
                .playback
                .as_ref()
                .is_none_or(|worker| worker.phase() != WorkerPhase::PrimedParked)
        {
            self.graph_step("worker parking")?;
            if Instant::now() >= deadline {
                return Err(attempt_error("worker parking", "workers did not park"));
            }
        }
        let deadline = Instant::now() + self.policy.startup_timeout;
        loop {
            let result = self
                .control
                .as_mut()
                .ok_or_else(|| attempt_error("bridge activation", "control is absent"))?
                .activate();
            match result {
                Ok(()) => break,
                Err(BridgeError::ActivationNotQuiescent { .. }) if Instant::now() < deadline => {
                    self.graph_step("bridge activation")?;
                }
                Err(error) => return Err(attempt_error("bridge activation", error)),
            }
        }
        let control = self
            .control
            .as_ref()
            .ok_or_else(|| attempt_error("bridge activation", "control is absent"))?;
        self.capture
            .as_ref()
            .ok_or_else(|| attempt_error("capture resume", "worker is absent"))?
            .resume_after_activation(control)
            .map_err(|error| attempt_error("capture resume", error))?;
        self.playback
            .as_ref()
            .ok_or_else(|| attempt_error("playback resume", "worker is absent"))?
            .resume_after_activation(control)
            .map_err(|error| attempt_error("playback resume", error))?;
        let now = Instant::now();
        self.last_capture = Some(now);
        self.last_playback = Some(now);
        self.last_graph = Some(now);
        self.graph_cycles = self
            .graph
            .as_ref()
            .ok_or_else(|| attempt_error("graph activation", "graph is absent"))?
            .cycles();
        Ok(())
    }

    fn restore_software_routing(
        &mut self,
        _candidate: &UsbDeviceCandidate,
    ) -> Result<(), Self::Error> {
        // ClockBridgeParts constructed the mixer from the persisted profile.
        // All internal endpoint links were validated before graph activation.
        if self.control.as_ref().is_some_and(|control| control.state() == BridgeState::Active) {
            Ok(())
        } else {
            Err(attempt_error("routing restore", "bridge is not active"))
        }
    }

    fn teardown(&mut self) -> Result<(), Self::Error> {
        if self.cleanup_failed {
            return Err(LinuxAudioError::TeardownFailed);
        }
        if let Some(control) = self.control.as_mut() {
            control.begin_teardown();
        }
        if let Some(worker) = self.capture.as_ref() {
            worker.request_stop();
        }
        if let Some(worker) = self.playback.as_ref() {
            worker.request_stop();
        }
        let mut first = None;
        if let Some(graph) = self.graph.as_mut()
            && let Err(error) = graph.teardown()
        {
            first.get_or_insert(attempt_error("graph teardown", error));
        }
        if first.is_none() {
            self.graph = None;
        }
        if let Some(worker) = self.playback.take() {
            match worker.park_and_join() {
                Ok(result) => {
                    if let Err(error) = result.drop_stream_result {
                        first.get_or_insert(attempt_error("playback drop", format!("{error:?}")));
                    }
                }
                Err(error) => {
                    first.get_or_insert(attempt_error("playback join", format!("{error:?}")));
                }
            }
        }
        if let Some(worker) = self.parked_playback.take()
            && let Err(error) = worker.park_and_drop_stream().drop_stream_result
        {
            first.get_or_insert(attempt_error("parked playback drop", format!("{error:?}")));
        }
        if let Some(worker) = self.capture.take() {
            match worker.park_and_join() {
                Ok(result) => {
                    if let Err(error) = result.drop_stream_result {
                        first.get_or_insert(attempt_error("capture drop", format!("{error:?}")));
                    }
                }
                Err(error) => {
                    first.get_or_insert(attempt_error("capture join", format!("{error:?}")));
                }
            }
        }
        if let Some(mut control) = self.control.take()
            && let Err(error) = control.finish_teardown()
        {
            first.get_or_insert(attempt_error("bridge teardown", error));
        }
        self.pending_graph = None;
        self.graph_fault_reader = None;
        self.pending_playback = None;
        self.selected = None;
        self.meters = None;
        self.capture_frames = 0;
        self.last_capture = None;
        self.last_playback = None;
        self.last_graph = None;
        self.graph_cycles = 0;
        if let Err(error) = self.pipewire.disconnect() {
            first.get_or_insert(error);
        }
        self.cleanup_failed = first.is_some();
        first.map_or(Ok(()), Err)
    }
}

#[cfg(test)]
pub(in crate::audio_host) mod tests {
    use super::super::worker::{CapturePcm, PcmIoError, PcmStatusSnapshot, PcmWait, PlaybackPcm};
    use super::*;
    use crate::{AlsaCardInfo, DeviceIdentity, UsbTopology};
    use ::alsa::pcm::State;
    use librewave_engine::{
        ClockFramePosition, ClockObservation, ClockRateEstimate, ClockRateEstimator,
        ClockRateEstimatorError, MonotonicNanoseconds,
    };
    use std::sync::{Arc, Mutex};

    type Calls = Arc<Mutex<Vec<&'static str>>>;

    fn record(calls: &Calls, event: &'static str) {
        calls.lock().expect("call log").push(event);
    }

    fn candidate() -> UsbDeviceCandidate {
        UsbDeviceCandidate {
            identity: DeviceIdentity::wave3(),
            topology: UsbTopology::new("1-8.3"),
            alsa_cards: vec![AlsaCardInfo {
                number: 7,
                id: Some("Wave3".to_owned()),
                name: Some("Wave:3".to_owned()),
            }],
        }
    }

    struct FakePipeWire(Calls);

    struct FakeAdmission(Calls);

    impl AdmissionGate for FakeAdmission {
        fn admit(&mut self, _candidate: &UsbDeviceCandidate) -> Result<(), LinuxAudioError> {
            record(&self.0, "read-only admission");
            Ok(())
        }
    }

    struct DenyingAdmission(Calls, librewave_core::AdmissionError);

    impl AdmissionGate for DenyingAdmission {
        fn admit(&mut self, _candidate: &UsbDeviceCandidate) -> Result<(), LinuxAudioError> {
            record(&self.0, "read-only admission rejected");
            Err(LinuxAudioError::DeviceAdmission(self.1.clone()))
        }
    }

    impl PipeWireFacade for FakePipeWire {
        fn verify_physical_nodes_hidden(
            &mut self,
            _candidate: &UsbDeviceCandidate,
        ) -> Result<(), LinuxAudioError> {
            record(&self.0, "verify hidden");
            Ok(())
        }
        fn disconnect(&mut self) -> Result<(), LinuxAudioError> {
            record(&self.0, "disconnect PipeWire");
            Ok(())
        }
        fn owns_connection(&self) -> bool {
            true
        }
    }

    #[derive(Clone, Copy)]
    enum CaptureMode {
        ImmediateFault,
        PrimeThenWait,
        Nominal,
    }

    fn monotonic_nanoseconds() -> u64 {
        let mut time = libc::timespec { tv_sec: 0, tv_nsec: 0 };
        // SAFETY: `time` is a valid writable timespec for CLOCK_MONOTONIC.
        assert_eq!(unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &raw mut time) }, 0);
        u64::try_from(time.tv_sec).expect("positive seconds") * 1_000_000_000
            + u64::try_from(time.tv_nsec).expect("positive nanoseconds")
    }

    struct FakeAlsa {
        calls: Calls,
        physical: Wave3PhysicalIoConfig,
        mode: CaptureMode,
    }

    impl AlsaFacade for FakeAlsa {
        fn select(
            &mut self,
            candidate: &UsbDeviceCandidate,
        ) -> Result<SelectedPcmCard, LinuxAudioError> {
            record(&self.calls, "select card");
            Ok(SelectedPcmCard {
                identity: candidate.identity,
                topology: candidate.topology.clone(),
                card_number: 7,
                card_id: "Wave3".to_owned(),
                capture_device: 0,
                playback_device: 0,
            })
        }
        fn open_capture(
            &mut self,
            _candidate: &UsbDeviceCandidate,
            _selected: &SelectedPcmCard,
        ) -> Result<Box<dyn CapturePcm>, LinuxAudioError> {
            record(&self.calls, "open capture");
            Ok(Box::new(FakeCapture {
                calls: Arc::clone(&self.calls),
                geometry: self.physical.parameters(PcmDirection::Capture),
                mode: self.mode,
                reads: 0,
                base_time: monotonic_nanoseconds(),
            }))
        }
        fn open_playback(
            &mut self,
            _candidate: &UsbDeviceCandidate,
            _selected: &SelectedPcmCard,
        ) -> Result<Box<dyn PlaybackPcm>, LinuxAudioError> {
            record(&self.calls, "open playback");
            Ok(Box::new(FakePlayback {
                calls: Arc::clone(&self.calls),
                geometry: self.physical.parameters(PcmDirection::Playback),
                mode: self.mode,
                status_count: 0,
                started: false,
                base_time: monotonic_nanoseconds(),
            }))
        }
    }

    struct FakeCapture {
        calls: Calls,
        geometry: super::super::pcm::PhysicalPcmParameters,
        mode: CaptureMode,
        reads: u32,
        base_time: u64,
    }

    impl CapturePcm for FakeCapture {
        fn start_pcm(&mut self) -> Result<(), PcmIoError> {
            record(&self.calls, "start capture");
            Ok(())
        }
        fn wait(&mut self, _timeout_millis: u32) -> Result<PcmWait, PcmIoError> {
            if matches!(self.mode, CaptureMode::ImmediateFault) {
                return Err(PcmIoError::Disconnected);
            }
            std::thread::sleep(if matches!(self.mode, CaptureMode::Nominal) {
                Duration::from_micros(21_333)
            } else {
                Duration::from_millis(1)
            });
            if self.reads < 8 || matches!(self.mode, CaptureMode::Nominal) {
                Ok(PcmWait::Ready)
            } else {
                Ok(PcmWait::TimedOut)
            }
        }
        fn read_frames(&mut self, bytes: &mut [u8]) -> Result<usize, PcmIoError> {
            bytes.fill(0);
            self.reads += 1;
            Ok(1_024)
        }
        fn status(&mut self) -> Result<PcmStatusSnapshot, PcmIoError> {
            let elapsed = self.base_time + u64::from(self.reads) * 1_024 * 1_000_000_000 / 48_000;
            Ok(PcmStatusSnapshot {
                timestamp_seconds: i64::try_from(elapsed / 1_000_000_000).expect("test seconds"),
                timestamp_nanoseconds: i64::try_from(elapsed % 1_000_000_000).expect("test nanos"),
                available_frames: 0,
                delay_frames: 0,
                state_raw: State::Running as i32,
                geometry: self.geometry,
            })
        }
        fn drop_stream(&mut self) -> Result<(), PcmIoError> {
            record(&self.calls, "drop capture");
            Ok(())
        }
    }

    struct FakePlayback {
        calls: Calls,
        geometry: super::super::pcm::PhysicalPcmParameters,
        mode: CaptureMode,
        status_count: u64,
        started: bool,
        base_time: u64,
    }

    impl PlaybackPcm for FakePlayback {
        fn wait(&mut self, _timeout_millis: u32) -> Result<PcmWait, PcmIoError> {
            assert!(matches!(self.mode, CaptureMode::Nominal), "playback must not run");
            Ok(PcmWait::Ready)
        }
        fn write_frames(&mut self, bytes: &[u8]) -> Result<usize, PcmIoError> {
            assert!(matches!(self.mode, CaptureMode::Nominal), "playback must not write");
            Ok(bytes.len() / 6)
        }
        fn status(&mut self) -> Result<PcmStatusSnapshot, PcmIoError> {
            assert!(matches!(self.mode, CaptureMode::Nominal), "playback must not run");
            // The private PipeWire driver can run ahead of wall time. Keep
            // this fake consumer ahead of its FIFO while its status samples
            // retain the exact 256-frame, 48 kHz synthetic clock.
            std::thread::sleep(Duration::from_millis(2));
            self.status_count += 1;
            let time = self.base_time + self.status_count * 256 * 1_000_000_000 / 48_000;
            Ok(PcmStatusSnapshot {
                timestamp_seconds: i64::try_from(time / 1_000_000_000).expect("test seconds"),
                timestamp_nanoseconds: i64::try_from(time % 1_000_000_000).expect("test nanos"),
                available_frames: i64::from(self.geometry.buffer_frames),
                delay_frames: 0,
                state_raw: if self.started {
                    State::Running as i32
                } else {
                    State::Prepared as i32
                },
                geometry: self.geometry,
            })
        }
        fn start_pcm(&mut self) -> Result<(), PcmIoError> {
            assert!(matches!(self.mode, CaptureMode::Nominal), "playback must not start");
            self.started = true;
            record(&self.calls, "start playback");
            Ok(())
        }
        fn drop_stream(&mut self) -> Result<(), PcmIoError> {
            record(&self.calls, "drop playback");
            Ok(())
        }
    }

    struct FailingGraph(Calls);

    impl GraphFactory for FailingGraph {
        fn activate(
            &mut self,
            _remote: &str,
            _plans: &[EndpointPlan],
            _quantum: NonZeroU32,
            _processor: ClockBridgeGraphProcessor,
        ) -> Result<Box<dyn GraphRuntime>, LinuxAudioError> {
            record(&self.0, "activate graph");
            Err(attempt_error("graph activation", "injected failure"))
        }
    }

    fn host(mode: CaptureMode, calls: &Calls) -> ComposedAudioHost {
        let policy =
            AudioAttemptPolicy::conservative(ClockAttemptEpoch::try_new(5).expect("epoch"))
                .expect("checked policy");
        let physical = policy.physical;
        ComposedAudioHost::with_adapters(
            policy,
            MixerProfile::default(),
            Box::new(FakePipeWire(Arc::clone(calls))),
            Box::new(FakeAlsa { calls: Arc::clone(calls), physical, mode }),
            Box::new(FailingGraph(Arc::clone(calls))),
            Box::new(FakeAdmission(Arc::clone(calls))),
        )
    }

    pub(in crate::audio_host) fn private_composed_attempt(remote: &str) {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let mut policy =
            AudioAttemptPolicy::conservative(ClockAttemptEpoch::try_new(23).expect("epoch"))
                .expect("checked policy");
        policy.remote_name = remote.to_owned();
        let physical = policy.physical;
        let mut host = ComposedAudioHost::with_adapters(
            policy,
            MixerProfile::default(),
            Box::new(FakePipeWire(Arc::clone(&calls))),
            Box::new(FakeAlsa { calls: Arc::clone(&calls), physical, mode: CaptureMode::Nominal }),
            Box::new(RealGraphFactory),
            Box::new(FakeAdmission(Arc::clone(&calls))),
        );
        let mut lifecycle = crate::audio_lifecycle::AudioLifecycle::new();
        let result = lifecycle.start(&mut host, Some(&candidate()));
        assert!(result.is_ok(), "private composition startup failed: {result:?}");
        assert_eq!(lifecycle.state(), crate::audio_lifecycle::LifecycleState::Ready);
        for _ in 0..8 {
            host.poll().expect("composed attempt progress");
        }
        assert_eq!(host.resource_state().published_endpoints, DELIBERATE_ENDPOINTS);
        assert!(calls.lock().expect("calls").contains(&"start playback"));
        lifecycle
            .recover_after_host_restart(&mut host, Some(&candidate()))
            .expect("rebuild composed attempt after host restart");
        for _ in 0..8 {
            host.poll().expect("recovered composition progress");
        }
        let calls = calls.lock().expect("calls");
        assert_eq!(calls.iter().filter(|call| **call == "start capture").count(), 2);
        assert_eq!(calls.iter().filter(|call| **call == "start playback").count(), 2);
        drop(calls);
        lifecycle.handle_device_disconnect(&mut host).expect("device removal teardown");
        assert!(host.resource_state().published_endpoints.is_empty());
    }

    #[test]
    fn checked_policy_builds_a_complete_bridge() {
        let policy =
            AudioAttemptPolicy::conservative(ClockAttemptEpoch::try_new(5).expect("epoch"))
                .expect("policy");
        ClockBridgeParts::new(policy.physical, &MixerProfile::default(), policy.bridge)
            .expect("bridge policy fits physical geometry");
    }

    #[test]
    fn wave3_rate_policy_defers_short_capture_status_jitter_but_rejects_sustained_drift() {
        let epoch = ClockAttemptEpoch::try_new(7).expect("epoch");
        let bounds = RateMatchRatioBounds::try_new(2.0, 0.98, 1.02).expect("ratio bounds");
        let observation = |frames: u64, nanoseconds: u64| {
            ClockObservation::new(
                epoch,
                ClockFramePosition::new(frames),
                MonotonicNanoseconds::new(1_000_000_000 + nanoseconds),
            )
        };
        let origin = observation(0, 0);
        let capture_jitter = observation(1_056, 22_967_766);
        let graph_period = observation(1_024, 21_333_332);
        let short_delta = ClockDeltaBounds::try_new(1, 1_000_000_000, 1, 60_000_000_000)
            .expect("old short evidence span");
        let mut short = ClockRateEstimator::new(ClockRateEstimatorConfig::new(
            short_delta,
            short_delta,
            2_000_000_000,
            bounds,
        ));
        short.preview(origin, origin).expect("anchor").commit();
        assert!(matches!(
            short.preview(capture_jitter, graph_period),
            Err(ClockRateEstimatorError::RatioOutsideBounds { .. })
        ));

        let mut measured = ClockRateEstimator::new(
            wave3_capture_rate_estimator(bounds).expect("Wave:3 capture estimator policy"),
        );
        measured.preview(origin, origin).expect("anchor").commit();
        let step = measured.preview(capture_jitter, graph_period).expect("short status interval");
        assert_eq!(step.estimate(), ClockRateEstimate::Priming);
        step.commit();
        let step = measured
            .preview(observation(8_192, 170_666_667), observation(8_192, 170_666_667))
            .expect("running-clock evidence");
        assert_eq!(step.estimate(), ClockRateEstimate::Measured { ratio: 1.0, updated: true });

        let mut sustained = ClockRateEstimator::new(
            wave3_capture_rate_estimator(bounds).expect("Wave:3 capture estimator policy"),
        );
        sustained.preview(origin, origin).expect("anchor").commit();
        assert!(matches!(
            sustained.preview(observation(8_192, 178_000_000), observation(8_192, 170_666_667)),
            Err(ClockRateEstimatorError::RatioOutsideBounds { .. })
        ));
    }

    #[test]
    fn capture_failure_rolls_back_before_playback_open() {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let mut host = host(CaptureMode::ImmediateFault, &calls);
        let mut lifecycle = crate::audio_lifecycle::AudioLifecycle::new();
        assert!(lifecycle.start(&mut host, Some(&candidate())).is_err());
        assert_eq!(
            *calls.lock().expect("calls"),
            [
                "verify hidden",
                "read-only admission",
                "select card",
                "open capture",
                "start capture",
                "drop capture",
                "disconnect PipeWire",
            ]
        );
        assert!(host.resource_state().published_endpoints.is_empty());
    }

    #[test]
    fn admission_failure_keeps_its_category_and_stops_before_any_alsa_open() {
        for error in [
            librewave_core::AdmissionError::UnsupportedApi {
                api: librewave_core::ApiVersion::new(5, 2),
            },
            librewave_core::AdmissionError::PermissionDenied,
        ] {
            let calls = Arc::new(Mutex::new(Vec::new()));
            let policy =
                AudioAttemptPolicy::conservative(ClockAttemptEpoch::try_new(5).expect("epoch"))
                    .expect("policy");
            let physical = policy.physical;
            let mut host = ComposedAudioHost::with_adapters(
                policy,
                MixerProfile::default(),
                Box::new(FakePipeWire(Arc::clone(&calls))),
                Box::new(FakeAlsa {
                    calls: Arc::clone(&calls),
                    physical,
                    mode: CaptureMode::Nominal,
                }),
                Box::new(FailingGraph(Arc::clone(&calls))),
                Box::new(DenyingAdmission(Arc::clone(&calls), error.clone())),
            );
            let mut lifecycle = crate::audio_lifecycle::AudioLifecycle::new();
            assert!(matches!(lifecycle.start(&mut host, Some(&candidate())),
            Err(crate::audio_lifecycle::LifecycleError::Host { source: LinuxAudioError::DeviceAdmission(observed), .. }) if observed == error));
            assert_eq!(
                *calls.lock().expect("calls"),
                ["verify hidden", "read-only admission rejected", "disconnect PipeWire",]
            );
        }
    }

    #[test]
    fn graph_failure_drops_prepared_playback_and_capture() {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let mut host = host(CaptureMode::PrimeThenWait, &calls);
        let mut lifecycle = crate::audio_lifecycle::AudioLifecycle::new();
        assert!(lifecycle.start(&mut host, Some(&candidate())).is_err());
        assert_eq!(
            *calls.lock().expect("calls"),
            [
                "verify hidden",
                "read-only admission",
                "select card",
                "open capture",
                "start capture",
                "open playback",
                "activate graph",
                "drop playback",
                "drop capture",
                "disconnect PipeWire",
            ]
        );
        assert!(host.resource_state().published_endpoints.is_empty());
    }
}
