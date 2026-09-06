//! Offline bridges between the Wave capture, `PipeWire` graph, and Wave playback clocks.

use super::pcm::{
    PackedS24Error, WAVE3_CAPTURE_CHANNELS, WAVE3_PLAYBACK_CHANNELS, Wave3PhysicalIoConfig,
    decode_s24_3le, encode_s24_3le, packed_frame_count,
};
use librewave_core::{
    EndpointId, MICROPHONE_SOURCE_ID, MixerProfile, MixerProfileError, SYSTEM_SOURCE_ID,
    SourceControls,
};
use librewave_engine::{
    ClockAttemptEpoch, ClockObservation, ClockRateEstimator, ClockRateEstimatorConfig,
    ClockRateEstimatorError, ControlStager, InputBuffer, MeterPublisher, MeterReader, MixerConfig,
    MixerEngine, OutputBuffer, ProcessError, RateMatchController, RateMatchControllerConfig,
    RateMatchControllerError, RateMatchError, RateMatcher, RateMatcherConfig, StageError,
};
use std::fmt;
use std::sync::Arc;
use std::sync::atomic::{AtomicU8, AtomicUsize, Ordering};

pub(super) mod observation;
mod spsc;

pub use observation::{
    CaptureClockObservation, GraphClockObservation, GraphClockObservationError,
    PlaybackClockObservation,
};

use observation::{CopySlotPublisher, CopySlotReader};
use spsc::{
    Consumer, FillObserver, Producer, RingBuildError, RingPeekError, RingPushError,
    RingSequenceError,
};

#[cfg(test)]
#[path = "clock_bridge/tests.rs"]
pub(super) mod tests;

/// One of the three independent frame-position domains.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BridgeClockDomain {
    WaveCapture,
    PipeWireGraph,
    WavePlayback,
}

/// The explicit lifecycle of one offline bridge attempt.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum BridgeState {
    Allocated = 0,
    CapturePriming = 1,
    CaptureFilterDelay = 2,
    CaptureEstimatorPriming = 3,
    PlaybackPriming = 4,
    PlaybackFilterDelay = 5,
    PlaybackEstimatorPriming = 6,
    PlaybackStabilizing = 7,
    PrimingCheck = 8,
    Primed = 9,
    Active = 10,
    Faulted = 11,
    Quiescing = 12,
    Stopped = 13,
}

impl BridgeState {
    fn from_raw(raw: u8) -> Self {
        match raw {
            0 => Self::Allocated,
            1 => Self::CapturePriming,
            2 => Self::CaptureFilterDelay,
            3 => Self::CaptureEstimatorPriming,
            4 => Self::PlaybackPriming,
            5 => Self::PlaybackFilterDelay,
            6 => Self::PlaybackEstimatorPriming,
            7 => Self::PlaybackStabilizing,
            8 => Self::PrimingCheck,
            9 => Self::Primed,
            10 => Self::Active,
            11 => Self::Faulted,
            12 => Self::Quiescing,
            _ => Self::Stopped,
        }
    }
}

/// Caller-selected resources and controller policy for one direction.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct DirectionBridgeConfig {
    fifo_capacity_frames: usize,
    startup_guard_frames: usize,
    matcher: RateMatcherConfig,
    controller: RateMatchControllerConfig,
    estimator: ClockRateEstimatorConfig,
}

impl DirectionBridgeConfig {
    /// Checks one direction's caller-selected resource and control policy.
    ///
    /// # Errors
    ///
    /// Returns an error for zero capacity, matcher/controller/estimator ratio
    /// bounds that differ, or a startup range that does not fit in the FIFO.
    pub fn try_new(
        fifo_capacity_frames: usize,
        startup_guard_frames: usize,
        matcher: RateMatcherConfig,
        controller: RateMatchControllerConfig,
        estimator: ClockRateEstimatorConfig,
    ) -> Result<Self, BridgeBuildError> {
        if fifo_capacity_frames == 0 {
            return Err(BridgeBuildError::ZeroFifoCapacity);
        }
        if matcher.ratio_bounds() != controller.ratio_bounds() {
            return Err(BridgeBuildError::RatioBoundsDiffer);
        }
        if matcher.ratio_bounds() != estimator.ratio_bounds() {
            return Err(BridgeBuildError::EstimatorRatioBoundsDiffer);
        }
        let target = controller.target_fill_frames();
        if target <= startup_guard_frames
            || target
                .checked_add(startup_guard_frames)
                .is_none_or(|maximum| maximum > fifo_capacity_frames)
        {
            return Err(BridgeBuildError::InvalidStartupGuard {
                target_fill_frames: target,
                startup_guard_frames,
                fifo_capacity_frames,
            });
        }
        Ok(Self { fifo_capacity_frames, startup_guard_frames, matcher, controller, estimator })
    }

    #[must_use]
    pub const fn fifo_capacity_frames(self) -> usize {
        self.fifo_capacity_frames
    }

    #[must_use]
    pub const fn startup_guard_frames(self) -> usize {
        self.startup_guard_frames
    }
}

/// Complete caller-selected policy for the offline bridge.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ClockBridgeConfig {
    attempt_epoch: ClockAttemptEpoch,
    maximum_graph_quantum_frames: usize,
    capture: DirectionBridgeConfig,
    playback: DirectionBridgeConfig,
}

impl ClockBridgeConfig {
    /// Checks the two direction policies against the graph resource ceiling.
    ///
    /// # Errors
    ///
    /// Returns an error for a zero graph limit, wrong matcher channel count,
    /// or insufficient capture matcher output capacity.
    pub fn try_new(
        attempt_epoch: ClockAttemptEpoch,
        maximum_graph_quantum_frames: usize,
        capture: DirectionBridgeConfig,
        playback: DirectionBridgeConfig,
    ) -> Result<Self, BridgeBuildError> {
        if maximum_graph_quantum_frames == 0 {
            return Err(BridgeBuildError::ZeroMaximumGraphQuantum);
        }
        if capture.matcher.channels() != WAVE3_CAPTURE_CHANNELS {
            return Err(BridgeBuildError::MatcherChannels {
                domain: BridgeClockDomain::WaveCapture,
                expected: WAVE3_CAPTURE_CHANNELS,
                actual: capture.matcher.channels(),
            });
        }
        if playback.matcher.channels() != WAVE3_PLAYBACK_CHANNELS {
            return Err(BridgeBuildError::MatcherChannels {
                domain: BridgeClockDomain::WavePlayback,
                expected: WAVE3_PLAYBACK_CHANNELS,
                actual: playback.matcher.channels(),
            });
        }
        if maximum_graph_quantum_frames > capture.matcher.max_output_frames() {
            return Err(BridgeBuildError::MatcherOutputCapacity {
                domain: BridgeClockDomain::WaveCapture,
                required: maximum_graph_quantum_frames,
                actual: capture.matcher.max_output_frames(),
            });
        }
        Ok(Self { attempt_epoch, maximum_graph_quantum_frames, capture, playback })
    }
}

/// Why an offline bridge could not be built.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum BridgeBuildError {
    ZeroFifoCapacity,
    ZeroMaximumGraphQuantum,
    RatioBoundsDiffer,
    EstimatorRatioBoundsDiffer,
    InvalidStartupGuard {
        target_fill_frames: usize,
        startup_guard_frames: usize,
        fifo_capacity_frames: usize,
    },
    MatcherChannels {
        domain: BridgeClockDomain,
        expected: usize,
        actual: usize,
    },
    MatcherOutputCapacity {
        domain: BridgeClockDomain,
        required: usize,
        actual: usize,
    },
    FifoCapacity {
        domain: BridgeClockDomain,
        required: usize,
        actual: usize,
    },
    FifoOperatingRange {
        domain: BridgeClockDomain,
        minimum_fill: usize,
        maximum_input_frames: usize,
    },
    FifoProducerHeadroom {
        domain: BridgeClockDomain,
        required_free_frames: usize,
        available_free_frames: usize,
    },
    PhysicalGeometry,
    Profile(MixerProfileError),
    EngineConfig,
    ControlMapping,
    RateMatcher {
        domain: BridgeClockDomain,
        error: RateMatchError,
    },
    Allocation,
}

impl fmt::Display for BridgeBuildError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{self:?}")
    }
}

impl std::error::Error for BridgeBuildError {}

/// Why a complete mixer profile could not be staged.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BridgeControlError {
    Profile(MixerProfileError),
    Stage(StageError),
}

impl fmt::Display for BridgeControlError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{self:?}")
    }
}

impl std::error::Error for BridgeControlError {}

/// Stable playback-submission failure classes.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PlaybackWriteError {
    AgainRetryLimit { accepted_frames: usize },
    InterruptedRetryLimit { accepted_frames: usize },
    Xrun { accepted_frames: usize },
    Suspended { accepted_frames: usize },
    Disconnected { accepted_frames: usize },
    Stopped { accepted_frames: usize },
    InvalidProgress { accepted_frames: usize },
    Failed { accepted_frames: usize, errno: i32 },
}

impl PlaybackWriteError {
    #[must_use]
    pub const fn accepted_frames(self) -> usize {
        match self {
            Self::AgainRetryLimit { accepted_frames }
            | Self::InterruptedRetryLimit { accepted_frames }
            | Self::Xrun { accepted_frames }
            | Self::Suspended { accepted_frames }
            | Self::Disconnected { accepted_frames }
            | Self::Stopped { accepted_frames }
            | Self::InvalidProgress { accepted_frames }
            | Self::Failed { accepted_frames, .. } => accepted_frames,
        }
    }
}

/// Playback application progress reported after one worker submission.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PlaybackSubmissionProgress {
    pub frames_submitted: usize,
}

/// The delivery boundary used by the offline playback worker.
pub trait PlaybackPeriodSubmitter: Send {
    /// Submits one complete packed period to ALSA's application side.
    ///
    /// # Errors
    ///
    /// Returns a classified worker failure if the period was not delivered.
    fn submit_packed_s24_3le(
        &mut self,
        complete_frame_count: usize,
        bytes: &[u8],
    ) -> Result<PlaybackSubmissionProgress, PlaybackWriteError>;
}

/// Why an offline bridge boundary failed.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum BridgeError {
    AttemptFaulted,
    State {
        actual: BridgeState,
    },
    ActivationNotQuiescent {
        capture: usize,
        graph: usize,
        playback: usize,
    },
    Teardown,
    TeardownNotQuiescent {
        capture: usize,
        graph: usize,
        playback: usize,
    },
    FrameCount {
        domain: BridgeClockDomain,
        actual: usize,
        maximum: usize,
    },
    FrameCountMismatch {
        domain: BridgeClockDomain,
        declared: usize,
        encoded: usize,
    },
    PositionOverflow {
        domain: BridgeClockDomain,
        start: u64,
        frames: usize,
    },
    Discontinuity {
        domain: BridgeClockDomain,
        expected: u64,
        actual: u64,
    },
    ObservationEpoch {
        domain: BridgeClockDomain,
        expected: ClockAttemptEpoch,
        actual: ClockAttemptEpoch,
    },
    RepeatedClockObservation {
        domain: BridgeClockDomain,
    },
    MonotonicTimeDiscontinuity {
        domain: BridgeClockDomain,
        previous: u64,
        actual: u64,
    },
    InvalidQuantum {
        actual: usize,
        maximum: usize,
    },
    InvalidPeriod {
        actual: usize,
        expected: usize,
    },
    FixedQuantumChanged {
        expected: usize,
        actual: usize,
    },
    Packed(PackedS24Error),
    MissingSystemBuffer,
    SystemLength {
        expected: usize,
        left: usize,
        right: usize,
    },
    NonFinite {
        domain: BridgeClockDomain,
        sample_index: usize,
    },
    OutputLength {
        expected: usize,
        actual: usize,
    },
    Overflow {
        domain: BridgeClockDomain,
        requested: usize,
        remaining: usize,
        capacity: usize,
    },
    Shortage {
        domain: BridgeClockDomain,
        required: usize,
        available: usize,
    },
    FifoSequence {
        domain: BridgeClockDomain,
    },
    Controller {
        domain: BridgeClockDomain,
        error: RateMatchControllerError,
    },
    Estimator {
        domain: BridgeClockDomain,
        error: ClockRateEstimatorError,
    },
    ObservationUnavailable {
        domain: BridgeClockDomain,
    },
    PlaybackApplicationPosition {
        expected: u64,
        actual: u64,
    },
    CaptureApplicationPosition {
        expected: u64,
        actual: u64,
    },
    PlaybackHardwareAhead {
        hardware: u64,
        application: u64,
    },
    CaptureHardwareBehindApplication {
        hardware: u64,
        application: u64,
    },
    Asrc {
        domain: BridgeClockDomain,
        error: RateMatchError,
    },
    Engine(ProcessError),
    Playback(PlaybackWriteError),
    PlaybackSubmission {
        expected_frames: usize,
        actual_frames: usize,
    },
}

impl fmt::Display for BridgeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{self:?}")
    }
}

impl std::error::Error for BridgeError {}

#[derive(Debug)]
struct SharedAttempt {
    attempt_epoch: ClockAttemptEpoch,
    state: AtomicU8,
    capture_delay_remaining: AtomicUsize,
    playback_delay_remaining: AtomicUsize,
    capture_in_flight: AtomicUsize,
    graph_in_flight: AtomicUsize,
    playback_in_flight: AtomicUsize,
    capture_target: usize,
    capture_guard: usize,
    playback_target: usize,
    playback_guard: usize,
}

impl SharedAttempt {
    fn state(&self) -> BridgeState {
        BridgeState::from_raw(self.state.load(Ordering::SeqCst))
    }

    fn transition(&self, from: BridgeState, to: BridgeState) -> bool {
        self.state
            .compare_exchange(from as u8, to as u8, Ordering::SeqCst, Ordering::SeqCst)
            .is_ok()
    }

    fn fault(&self) {
        let mut current = self.state.load(Ordering::SeqCst);
        loop {
            let state = BridgeState::from_raw(current);
            if matches!(state, BridgeState::Quiescing | BridgeState::Stopped) {
                return;
            }
            match self.state.compare_exchange_weak(
                current,
                BridgeState::Faulted as u8,
                Ordering::SeqCst,
                Ordering::SeqCst,
            ) {
                Ok(_) => return,
                Err(actual) => current = actual,
            }
        }
    }

    fn within_startup_guards(&self, capture_fill: usize, playback_fill: usize) -> bool {
        within_guard(capture_fill, self.capture_target, self.capture_guard)
            && within_guard(playback_fill, self.playback_target, self.playback_guard)
    }
}

fn within_guard(fill: usize, target: usize, guard: usize) -> bool {
    fill.abs_diff(target) <= guard
}

struct ActivityGuard<'a> {
    counter: &'a AtomicUsize,
}

impl<'a> ActivityGuard<'a> {
    fn enter(counter: &'a AtomicUsize) -> Self {
        counter.fetch_add(1, Ordering::SeqCst);
        Self { counter }
    }
}

impl Drop for ActivityGuard<'_> {
    fn drop(&mut self) {
        self.counter.fetch_sub(1, Ordering::SeqCst);
    }
}

/// Control-side ownership for lifecycle and mixer updates.
#[derive(Debug)]
pub struct ClockBridgeControl {
    stager: ControlStager,
    capture_fill_observer: FillObserver<WAVE3_CAPTURE_CHANNELS>,
    playback_fill_observer: FillObserver<WAVE3_PLAYBACK_CHANNELS>,
    shared: Arc<SharedAttempt>,
}

impl ClockBridgeControl {
    #[must_use]
    pub fn state(&self) -> BridgeState {
        self.shared.state()
    }

    pub(super) fn worker_attempt_epoch(&self) -> ClockAttemptEpoch {
        self.shared.attempt_epoch
    }

    /// Starts the capture-first priming sequence.
    ///
    /// # Errors
    ///
    /// Returns an error unless the attempt is in `Allocated`.
    pub fn start_capture(&mut self) -> Result<(), BridgeError> {
        match self.state() {
            BridgeState::Allocated => {
                if self.shared.transition(BridgeState::Allocated, BridgeState::CapturePriming) {
                    Ok(())
                } else {
                    Err(BridgeError::State { actual: self.state() })
                }
            }
            actual => Err(BridgeError::State { actual }),
        }
    }

    /// Enables offline delivery after all priming work completes.
    ///
    /// # Errors
    ///
    /// Returns an error unless the attempt is in `Primed` with no boundary in flight.
    pub fn activate(&mut self) -> Result<(), BridgeError> {
        match self.state() {
            BridgeState::Primed => {
                if !self.shared.transition(BridgeState::Primed, BridgeState::PrimingCheck) {
                    return Err(BridgeError::State { actual: self.state() });
                }
                let capture = self.shared.capture_in_flight.load(Ordering::SeqCst);
                let graph = self.shared.graph_in_flight.load(Ordering::SeqCst);
                let playback = self.shared.playback_in_flight.load(Ordering::SeqCst);
                if capture != 0 || graph != 0 || playback != 0 {
                    let _ = self.shared.transition(BridgeState::PrimingCheck, BridgeState::Primed);
                    return Err(BridgeError::ActivationNotQuiescent { capture, graph, playback });
                }
                let capture_fill =
                    self.capture_fill_observer.stable_fill_frames().map_err(|error| {
                        mark_fault(
                            &self.shared,
                            map_sequence(error, BridgeClockDomain::WaveCapture),
                        )
                    })?;
                let playback_fill =
                    self.playback_fill_observer.stable_fill_frames().map_err(|error| {
                        mark_fault(
                            &self.shared,
                            map_sequence(error, BridgeClockDomain::WavePlayback),
                        )
                    })?;
                if !self.shared.within_startup_guards(capture_fill, playback_fill) {
                    let _ = self
                        .shared
                        .transition(BridgeState::PrimingCheck, BridgeState::PlaybackStabilizing);
                    return Err(BridgeError::State { actual: BridgeState::PlaybackStabilizing });
                }
                if self.shared.transition(BridgeState::PrimingCheck, BridgeState::Active) {
                    Ok(())
                } else {
                    Err(BridgeError::State { actual: self.state() })
                }
            }
            actual => Err(BridgeError::State { actual }),
        }
    }

    /// Stages one complete mixer profile for the next graph boundary.
    ///
    /// # Errors
    ///
    /// Returns an error for an invalid profile or a full control handoff.
    pub fn try_stage_profile(&mut self, profile: &MixerProfile) -> Result<(), BridgeControlError> {
        let controls = profile_controls(profile).map_err(BridgeControlError::Profile)?;
        self.stager.try_stage(&controls).map_err(BridgeControlError::Stage)
    }

    pub fn begin_teardown(&mut self) {
        if self.state() != BridgeState::Stopped {
            self.shared.state.store(BridgeState::Quiescing as u8, Ordering::SeqCst);
        }
    }

    /// Marks teardown complete after every processing owner has quiesced.
    ///
    /// # Errors
    ///
    /// Returns an error before `Quiescing` or while any owner is in flight.
    pub fn finish_teardown(&mut self) -> Result<(), BridgeError> {
        let state = self.state();
        if state == BridgeState::Stopped {
            return Ok(());
        }
        if state != BridgeState::Quiescing {
            return Err(BridgeError::State { actual: state });
        }
        let capture = self.shared.capture_in_flight.load(Ordering::SeqCst);
        let graph = self.shared.graph_in_flight.load(Ordering::SeqCst);
        let playback = self.shared.playback_in_flight.load(Ordering::SeqCst);
        if capture != 0 || graph != 0 || playback != 0 {
            return Err(BridgeError::TeardownNotQuiescent { capture, graph, playback });
        }
        self.shared.state.store(BridgeState::Stopped as u8, Ordering::SeqCst);
        Ok(())
    }
}

/// All single-owner roles created for one offline attempt.
pub struct ClockBridgeParts {
    pub capture_ingress: CaptureIngress,
    pub graph: GraphClockProcessor,
    pub playback: PlaybackEgress,
    pub control: ClockBridgeControl,
    pub meters: MeterReader,
}

impl ClockBridgeParts {
    /// Allocates, warms, and splits one offline bridge before any worker starts.
    ///
    /// # Errors
    ///
    /// Returns an error for invalid physical geometry, profile or bridge
    /// policy, a matcher failure, or a failed bounded allocation.
    #[allow(clippy::too_many_lines)]
    pub fn new(
        physical: Wave3PhysicalIoConfig,
        profile: &MixerProfile,
        config: ClockBridgeConfig,
    ) -> Result<Self, BridgeBuildError> {
        profile.validate().map_err(BridgeBuildError::Profile)?;
        let capture_period = usize::try_from(physical.capture().period_frames())
            .map_err(|_| BridgeBuildError::PhysicalGeometry)?;
        let playback_period = usize::try_from(physical.playback().period_frames())
            .map_err(|_| BridgeBuildError::PhysicalGeometry)?;
        physical.capture_period_bytes().map_err(|_| BridgeBuildError::PhysicalGeometry)?;
        let playback_bytes =
            physical.playback_period_bytes().map_err(|_| BridgeBuildError::PhysicalGeometry)?;
        if playback_period > config.playback.matcher.max_output_frames() {
            return Err(BridgeBuildError::MatcherOutputCapacity {
                domain: BridgeClockDomain::WavePlayback,
                required: playback_period,
                actual: config.playback.matcher.max_output_frames(),
            });
        }
        if capture_period > config.capture.fifo_capacity_frames {
            return Err(BridgeBuildError::FifoCapacity {
                domain: BridgeClockDomain::WaveCapture,
                required: capture_period,
                actual: config.capture.fifo_capacity_frames,
            });
        }
        if config.maximum_graph_quantum_frames > config.playback.fifo_capacity_frames {
            return Err(BridgeBuildError::FifoCapacity {
                domain: BridgeClockDomain::WavePlayback,
                required: config.maximum_graph_quantum_frames,
                actual: config.playback.fifo_capacity_frames,
            });
        }
        require_producer_headroom(
            BridgeClockDomain::WaveCapture,
            config.capture.fifo_capacity_frames,
            config.capture.controller.target_fill_frames(),
            config.capture.startup_guard_frames,
            capture_period,
        )?;
        require_producer_headroom(
            BridgeClockDomain::WavePlayback,
            config.playback.fifo_capacity_frames,
            config.playback.controller.target_fill_frames(),
            config.playback.startup_guard_frames,
            config.maximum_graph_quantum_frames,
        )?;

        let mut capture_matcher = RateMatcher::new(config.capture.matcher).map_err(|error| {
            BridgeBuildError::RateMatcher { domain: BridgeClockDomain::WaveCapture, error }
        })?;
        let mut playback_matcher = RateMatcher::new(config.playback.matcher).map_err(|error| {
            BridgeBuildError::RateMatcher { domain: BridgeClockDomain::WavePlayback, error }
        })?;
        if capture_matcher.maximum_input_frames() > config.capture.fifo_capacity_frames {
            return Err(BridgeBuildError::FifoCapacity {
                domain: BridgeClockDomain::WaveCapture,
                required: capture_matcher.maximum_input_frames(),
                actual: config.capture.fifo_capacity_frames,
            });
        }
        if playback_matcher.maximum_input_frames() > config.playback.fifo_capacity_frames {
            return Err(BridgeBuildError::FifoCapacity {
                domain: BridgeClockDomain::WavePlayback,
                required: playback_matcher.maximum_input_frames(),
                actual: config.playback.fifo_capacity_frames,
            });
        }
        let capture_minimum_fill = config
            .capture
            .controller
            .target_fill_frames()
            .saturating_sub(config.capture.startup_guard_frames);
        if capture_minimum_fill < capture_matcher.maximum_input_frames() {
            return Err(BridgeBuildError::FifoOperatingRange {
                domain: BridgeClockDomain::WaveCapture,
                minimum_fill: capture_minimum_fill,
                maximum_input_frames: capture_matcher.maximum_input_frames(),
            });
        }
        let playback_minimum_fill = config
            .playback
            .controller
            .target_fill_frames()
            .saturating_sub(config.playback.startup_guard_frames);
        if playback_minimum_fill < playback_matcher.maximum_input_frames() {
            return Err(BridgeBuildError::FifoOperatingRange {
                domain: BridgeClockDomain::WavePlayback,
                minimum_fill: playback_minimum_fill,
                maximum_input_frames: playback_matcher.maximum_input_frames(),
            });
        }
        capture_matcher.warm().map_err(|error| BridgeBuildError::RateMatcher {
            domain: BridgeClockDomain::WaveCapture,
            error,
        })?;
        playback_matcher.warm().map_err(|error| BridgeBuildError::RateMatcher {
            domain: BridgeClockDomain::WavePlayback,
            error,
        })?;
        let capture_delay = capture_matcher.output_delay_frames();
        let playback_delay = playback_matcher.output_delay_frames();

        let (capture_producer, capture_consumer, capture_fill_observer) =
            spsc::channel::<WAVE3_CAPTURE_CHANNELS>(config.capture.fifo_capacity_frames)
                .map_err(map_ring_build)?;
        let (playback_producer, playback_consumer, playback_fill_observer) =
            spsc::channel::<WAVE3_PLAYBACK_CHANNELS>(config.playback.fifo_capacity_frames)
                .map_err(map_ring_build)?;
        let (capture_observation_publisher, capture_observation_reader) =
            observation::copy_slot::<CaptureClockObservation>();
        let (graph_observation_publisher, graph_observation_reader) =
            observation::copy_slot::<GraphClockObservation>();
        let source_ids =
            [profile.sources[0].controls.source(), profile.sources[1].controls.source()];
        let mixer_config = MixerConfig::try_new(
            config.maximum_graph_quantum_frames,
            profile.microphone_source,
            &source_ids,
        )
        .map_err(|_| BridgeBuildError::EngineConfig)?;
        let controls = profile_controls(profile).map_err(BridgeBuildError::Profile)?;
        let (engine, stager) = MixerEngine::new(mixer_config, &controls)
            .map_err(|_| BridgeBuildError::ControlMapping)?;
        let (meter_publisher, meters) = MeterPublisher::channel();
        let shared = Arc::new(SharedAttempt {
            attempt_epoch: config.attempt_epoch,
            state: AtomicU8::new(BridgeState::Allocated as u8),
            capture_delay_remaining: AtomicUsize::new(capture_delay),
            playback_delay_remaining: AtomicUsize::new(playback_delay),
            capture_in_flight: AtomicUsize::new(0),
            graph_in_flight: AtomicUsize::new(0),
            playback_in_flight: AtomicUsize::new(0),
            capture_target: config.capture.controller.target_fill_frames(),
            capture_guard: config.capture.startup_guard_frames,
            playback_target: config.playback.controller.target_fill_frames(),
            playback_guard: config.playback.startup_guard_frames,
        });
        let maximum_graph_samples = config
            .maximum_graph_quantum_frames
            .checked_mul(WAVE3_PLAYBACK_CHANNELS)
            .ok_or(BridgeBuildError::Allocation)?;
        let capture_input_samples = capture_matcher.maximum_input_frames();
        let playback_input_samples = playback_matcher
            .maximum_input_frames()
            .checked_mul(WAVE3_PLAYBACK_CHANNELS)
            .ok_or(BridgeBuildError::Allocation)?;
        let playback_output_samples = playback_period
            .checked_mul(WAVE3_PLAYBACK_CHANNELS)
            .ok_or(BridgeBuildError::Allocation)?;

        Ok(Self {
            capture_ingress: CaptureIngress {
                producer: capture_producer,
                decode: allocate_zeroed(capture_period)?,
                maximum_frames: capture_period,
                application_position: 0,
                last_observation: None,
                observation_publisher: capture_observation_publisher,
                shared: Arc::clone(&shared),
            },
            graph: GraphClockProcessor {
                capture_consumer,
                playback_producer,
                capture_observation_reader,
                graph_observation_publisher,
                capture_estimator: ClockRateEstimator::new(config.capture.estimator),
                capture_matcher,
                capture_controller: RateMatchController::new(config.capture.controller),
                engine,
                meter_publisher,
                capture_input: allocate_zeroed(capture_input_samples)?,
                capture_matched: allocate_zeroed(config.maximum_graph_quantum_frames)?,
                microphone_input: allocate_zeroed(maximum_graph_samples)?,
                system_input: allocate_zeroed(maximum_graph_samples)?,
                microphone_output: allocate_zeroed(maximum_graph_samples)?,
                monitor_output: allocate_zeroed(maximum_graph_samples)?,
                stream_output: allocate_zeroed(maximum_graph_samples)?,
                maximum_quantum: config.maximum_graph_quantum_frames,
                last_observation: None,
                fixed_quantum: None,
                shared: Arc::clone(&shared),
            },
            playback: PlaybackEgress {
                consumer: playback_consumer,
                capture_fill_observer: capture_fill_observer.clone(),
                graph_observation_reader,
                estimator: ClockRateEstimator::new(config.playback.estimator),
                matcher: playback_matcher,
                controller: RateMatchController::new(config.playback.controller),
                input: allocate_zeroed(playback_input_samples)?,
                output: allocate_zeroed(playback_output_samples)?,
                encoded: allocate_zeroed(playback_bytes)?,
                period_frames: playback_period,
                submitted_position: 0,
                last_hardware_observation: None,
                shared: Arc::clone(&shared),
            },
            control: ClockBridgeControl {
                stager,
                capture_fill_observer: capture_fill_observer.clone(),
                playback_fill_observer: playback_fill_observer.clone(),
                shared,
            },
            meters,
        })
    }
}

fn map_ring_build(_error: RingBuildError) -> BridgeBuildError {
    BridgeBuildError::Allocation
}

fn require_producer_headroom(
    domain: BridgeClockDomain,
    capacity_frames: usize,
    target_fill_frames: usize,
    startup_guard_frames: usize,
    required_free_frames: usize,
) -> Result<(), BridgeBuildError> {
    let available_free_frames = capacity_frames
        .checked_sub(target_fill_frames)
        .and_then(|remaining| remaining.checked_sub(startup_guard_frames))
        .ok_or(BridgeBuildError::InvalidStartupGuard {
            target_fill_frames,
            startup_guard_frames,
            fifo_capacity_frames: capacity_frames,
        })?;
    if available_free_frames < required_free_frames {
        return Err(BridgeBuildError::FifoProducerHeadroom {
            domain,
            required_free_frames,
            available_free_frames,
        });
    }
    Ok(())
}

fn allocate_zeroed<T: Clone + Default>(length: usize) -> Result<Vec<T>, BridgeBuildError> {
    let mut values = Vec::new();
    values.try_reserve_exact(length).map_err(|_| BridgeBuildError::Allocation)?;
    values.resize(length, T::default());
    Ok(values)
}

fn profile_controls(profile: &MixerProfile) -> Result<[SourceControls; 2], MixerProfileError> {
    profile.validate()?;
    Ok([profile.sources[0].controls, profile.sources[1].controls])
}

/// The capture-worker owner of the mono capture FIFO producer.
#[derive(Debug)]
pub struct CaptureIngress {
    producer: Producer<WAVE3_CAPTURE_CHANNELS>,
    decode: Vec<f32>,
    maximum_frames: usize,
    application_position: u64,
    last_observation: Option<ClockObservation>,
    observation_publisher: CopySlotPublisher<CaptureClockObservation>,
    shared: Arc<SharedAttempt>,
}

/// Whether a checked clock sample entered its no-overwrite handoff.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ObservationPublication {
    Published,
    NotPublishedHandoffFull,
}

/// One accepted capture-domain publication.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CapturePublishReport {
    pub application_start_frame: u64,
    pub application_end_frame: u64,
    pub hardware_frame_position: u64,
    pub frames: usize,
    pub fifo_fill_frames: usize,
    pub observation_publication: ObservationPublication,
}

/// One in-flight capture boundary from completed-read accounting through publication.
pub struct CaptureReadBoundary<'a> {
    ingress: &'a mut CaptureIngress,
    application_start_frame: u64,
    application_end_frame: u64,
    frames: usize,
}

impl fmt::Debug for CaptureReadBoundary<'_> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("CaptureReadBoundary")
            .field("application_start_frame", &self.application_start_frame)
            .field("application_end_frame", &self.application_end_frame)
            .field("frames", &self.frames)
            .finish_non_exhaustive()
    }
}

impl CaptureReadBoundary<'_> {
    /// Returns the attempt epoch used by the status validator.
    #[must_use]
    pub fn attempt_epoch(&self) -> ClockAttemptEpoch {
        self.ingress.shared.attempt_epoch
    }

    /// Returns the application position immediately after the completed read.
    #[must_use]
    pub const fn application_end_frame(&self) -> u64 {
        self.application_end_frame
    }

    /// Publishes the complete captured prefix and ends this capture boundary.
    ///
    /// # Errors
    ///
    /// Returns a terminal error for geometry, position, packed data,
    /// non-finite samples, or insufficient FIFO capacity.
    #[allow(
        clippy::needless_pass_by_value,
        reason = "consuming the boundary prevents duplicate capture publication"
    )]
    pub fn publish(
        self,
        observation: CaptureClockObservation,
        packed_s24_3le: &[u8],
    ) -> Result<CapturePublishReport, BridgeError> {
        let application_start_frame = self.application_start_frame;
        let application_end_frame = self.application_end_frame;
        let frames = self.frames;
        let ingress = &mut *self.ingress;
        if application_end_frame != ingress.application_position {
            return fault(
                &ingress.shared,
                BridgeError::CaptureApplicationPosition {
                    expected: ingress.application_position,
                    actual: application_end_frame,
                },
            );
        }
        let clock = observation.clock();
        validate_observation_epoch(&ingress.shared, BridgeClockDomain::WaveCapture, clock)
            .map_err(|error| mark_fault(&ingress.shared, error))?;
        validate_clock_progress(
            BridgeClockDomain::WaveCapture,
            ingress.last_observation,
            clock,
            true,
        )
        .map_err(|error| mark_fault(&ingress.shared, error))?;
        let hardware_frame_position = clock.frame_position().get();
        if hardware_frame_position < application_end_frame {
            return fault(
                &ingress.shared,
                BridgeError::CaptureHardwareBehindApplication {
                    hardware: hardware_frame_position,
                    application: application_end_frame,
                },
            );
        }
        let encoded_frames = packed_frame_count(packed_s24_3le.len(), WAVE3_CAPTURE_CHANNELS)
            .map_err(|error| mark_fault(&ingress.shared, BridgeError::Packed(error)))?;
        if encoded_frames != frames {
            return fault(
                &ingress.shared,
                BridgeError::FrameCountMismatch {
                    domain: BridgeClockDomain::WaveCapture,
                    declared: frames,
                    encoded: encoded_frames,
                },
            );
        }
        decode_s24_3le(packed_s24_3le, WAVE3_CAPTURE_CHANNELS, &mut ingress.decode[..frames])
            .map_err(|error| mark_fault(&ingress.shared, BridgeError::Packed(error)))?;
        let fifo_step =
            ingress.producer.prepare_push(&ingress.decode[..frames]).map_err(|error| {
                mark_fault(&ingress.shared, map_push(error, BridgeClockDomain::WaveCapture))
            })?;
        let fill = fifo_step.fill_after_commit();
        let observation_step = ingress.observation_publisher.prepare_publish(observation).ok();

        fifo_step.commit();
        let observation_publication = if let Some(step) = observation_step {
            step.commit();
            ObservationPublication::Published
        } else {
            ObservationPublication::NotPublishedHandoffFull
        };
        ingress.last_observation = Some(clock);
        if ingress.shared.state() == BridgeState::CapturePriming
            && fill >= ingress.shared.capture_target
        {
            let _ = ingress
                .shared
                .transition(BridgeState::CapturePriming, BridgeState::CaptureFilterDelay);
        }
        Ok(CapturePublishReport {
            application_start_frame,
            application_end_frame,
            hardware_frame_position,
            frames,
            fifo_fill_frames: fill,
            observation_publication,
        })
    }
}

impl Drop for CaptureReadBoundary<'_> {
    fn drop(&mut self) {
        self.ingress.shared.capture_in_flight.fetch_sub(1, Ordering::SeqCst);
    }
}

impl CaptureIngress {
    pub(super) fn worker_attempt_epoch(&self) -> ClockAttemptEpoch {
        self.shared.attempt_epoch
    }

    #[cfg(test)]
    pub(super) const fn worker_application_position(&self) -> u64 {
        self.application_position
    }

    pub(super) fn worker_fault(&self) {
        self.shared.fault();
    }

    pub(super) fn worker_state(&self) -> BridgeState {
        self.shared.state()
    }

    /// Records one completed ALSA read before its status sample is obtained.
    ///
    /// # Errors
    ///
    /// Returns a state error, an invalid frame count, or checked position
    /// overflow. A terminal validation error faults the bridge attempt.
    pub fn record_completed_read(
        &mut self,
        complete_frame_count: usize,
    ) -> Result<CaptureReadBoundary<'_>, BridgeError> {
        self.shared.capture_in_flight.fetch_add(1, Ordering::SeqCst);
        let state = match attempt_state(&self.shared) {
            Ok(state) => state,
            Err(error) => {
                self.shared.capture_in_flight.fetch_sub(1, Ordering::SeqCst);
                return Err(error);
            }
        };
        if matches!(state, BridgeState::Allocated | BridgeState::Primed) {
            self.shared.capture_in_flight.fetch_sub(1, Ordering::SeqCst);
            return Err(BridgeError::State { actual: state });
        }
        if complete_frame_count == 0 || complete_frame_count > self.maximum_frames {
            self.shared.capture_in_flight.fetch_sub(1, Ordering::SeqCst);
            return fault(
                &self.shared,
                BridgeError::FrameCount {
                    domain: BridgeClockDomain::WaveCapture,
                    actual: complete_frame_count,
                    maximum: self.maximum_frames,
                },
            );
        }
        let application_start_frame = self.application_position;
        let application_end_frame = match checked_end(
            BridgeClockDomain::WaveCapture,
            application_start_frame,
            complete_frame_count,
        ) {
            Ok(position) => position,
            Err(error) => {
                self.shared.capture_in_flight.fetch_sub(1, Ordering::SeqCst);
                return Err(mark_fault(&self.shared, error));
            }
        };
        self.application_position = application_end_frame;
        Ok(CaptureReadBoundary {
            ingress: self,
            application_start_frame,
            application_end_frame,
            frames: complete_frame_count,
        })
    }
}

/// Raw graph-side System buffers and their explicit connection state.
#[derive(Clone, Copy, Debug)]
pub struct GraphSystemInput<'a> {
    connected: bool,
    left: Option<&'a [f32]>,
    right: Option<&'a [f32]>,
}

impl<'a> GraphSystemInput<'a> {
    #[must_use]
    pub const fn unconnected() -> Self {
        Self { connected: false, left: None, right: None }
    }

    #[must_use]
    pub const fn connected(left: &'a [f32], right: &'a [f32]) -> Self {
        Self { connected: true, left: Some(left), right: Some(right) }
    }

    #[must_use]
    pub const fn from_raw(
        connected: bool,
        left: Option<&'a [f32]>,
        right: Option<&'a [f32]>,
    ) -> Self {
        Self { connected, left, right }
    }
}

/// Direct graph-time output buffers for the three mixer endpoints.
pub struct GraphOutputBuffers<'a> {
    pub microphone_left: &'a mut [f32],
    pub microphone_right: &'a mut [f32],
    pub monitor_left: &'a mut [f32],
    pub monitor_right: &'a mut [f32],
    pub stream_left: &'a mut [f32],
    pub stream_right: &'a mut [f32],
}

/// One successful graph-domain boundary.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct GraphProcessReport {
    pub start_frame: u64,
    pub end_frame: u64,
    pub frames: usize,
    pub capture_input_frames: usize,
    pub control_update_applied: bool,
    pub delivery: GraphBoundaryDelivery,
    pub observation_publication: ObservationPublication,
}

/// How one state-driven graph boundary treated its public outputs.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GraphBoundaryDelivery {
    DiscardedFilterDelay,
    DiscardedEstimatorPriming,
    DiscardedStartup,
    Delivered,
}

/// The PipeWire-data-loop owner of the mixer and capture bridge consumer.
pub struct GraphClockProcessor {
    capture_consumer: Consumer<WAVE3_CAPTURE_CHANNELS>,
    playback_producer: Producer<WAVE3_PLAYBACK_CHANNELS>,
    capture_observation_reader: CopySlotReader<CaptureClockObservation>,
    graph_observation_publisher: CopySlotPublisher<GraphClockObservation>,
    capture_estimator: ClockRateEstimator,
    capture_matcher: RateMatcher,
    capture_controller: RateMatchController,
    engine: MixerEngine,
    meter_publisher: MeterPublisher,
    capture_input: Vec<f32>,
    capture_matched: Vec<f32>,
    microphone_input: Vec<f32>,
    system_input: Vec<f32>,
    microphone_output: Vec<f32>,
    monitor_output: Vec<f32>,
    stream_output: Vec<f32>,
    maximum_quantum: usize,
    last_observation: Option<ClockObservation>,
    fixed_quantum: Option<usize>,
    shared: Arc<SharedAttempt>,
}

impl fmt::Debug for GraphClockProcessor {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("GraphClockProcessor")
            .field("maximum_quantum", &self.maximum_quantum)
            .field("last_observation", &self.last_observation)
            .field("fixed_quantum", &self.fixed_quantum)
            .finish_non_exhaustive()
    }
}

impl GraphClockProcessor {
    /// Processes one graph observation according to the attempt state.
    ///
    /// # Errors
    ///
    /// Returns a state error before work starts, or a terminal error for a
    /// clock observation, fixed quantum, buffer, FIFO, estimator, controller,
    /// ASRC, or mixer failure.
    #[allow(clippy::too_many_lines)]
    pub fn process(
        &mut self,
        observation: GraphClockObservation,
        system: GraphSystemInput<'_>,
        mut outputs: GraphOutputBuffers<'_>,
    ) -> Result<GraphProcessReport, BridgeError> {
        let _activity = ActivityGuard::enter(&self.shared.graph_in_flight);
        let state = attempt_state(&self.shared)?;
        if !matches!(
            state,
            BridgeState::CaptureFilterDelay
                | BridgeState::CaptureEstimatorPriming
                | BridgeState::PlaybackPriming
                | BridgeState::PlaybackFilterDelay
                | BridgeState::PlaybackEstimatorPriming
                | BridgeState::PlaybackStabilizing
                | BridgeState::Active
        ) {
            return Err(BridgeError::State { actual: state });
        }
        let clock = observation.clock();
        let quantum_frames = observation.quantum_frames();
        let graph_start_frame = clock.frame_position().get();
        if quantum_frames == 0 || quantum_frames > self.maximum_quantum {
            return fault(
                &self.shared,
                BridgeError::InvalidQuantum {
                    actual: quantum_frames,
                    maximum: self.maximum_quantum,
                },
            );
        }
        if let Some(expected) = self.fixed_quantum
            && expected != quantum_frames
        {
            return fault(
                &self.shared,
                BridgeError::FixedQuantumChanged { expected, actual: quantum_frames },
            );
        }
        validate_observation_epoch(&self.shared, BridgeClockDomain::PipeWireGraph, clock)
            .map_err(|error| mark_fault(&self.shared, error))?;
        validate_clock_progress(
            BridgeClockDomain::PipeWireGraph,
            self.last_observation,
            clock,
            true,
        )
        .map_err(|error| mark_fault(&self.shared, error))?;
        if let Some(previous) = self.last_observation {
            let expected = checked_end(
                BridgeClockDomain::PipeWireGraph,
                previous.frame_position().get(),
                quantum_frames,
            )
            .map_err(|error| mark_fault(&self.shared, error))?;
            if expected != graph_start_frame {
                return fault(
                    &self.shared,
                    BridgeError::Discontinuity {
                        domain: BridgeClockDomain::PipeWireGraph,
                        expected,
                        actual: graph_start_frame,
                    },
                );
            }
        }
        let graph_end_frame =
            checked_end(BridgeClockDomain::PipeWireGraph, graph_start_frame, quantum_frames)
                .map_err(|error| mark_fault(&self.shared, error))?;
        let sample_count =
            quantum_frames.checked_mul(WAVE3_PLAYBACK_CHANNELS).ok_or_else(|| {
                mark_fault(
                    &self.shared,
                    BridgeError::InvalidQuantum {
                        actual: quantum_frames,
                        maximum: self.maximum_quantum,
                    },
                )
            })?;
        prepare_system_input(
            &self.shared,
            system,
            quantum_frames,
            &mut self.system_input[..sample_count],
        )?;
        validate_output_lengths(&outputs, quantum_frames)
            .map_err(|error| mark_fault(&self.shared, error))?;

        let fill = self.capture_consumer.available_frames().map_err(|error| {
            mark_fault(&self.shared, map_sequence(error, BridgeClockDomain::WaveCapture))
        })?;
        let committed_ratio = self.capture_estimator.ratio();
        let capture_observation_step = self.capture_observation_reader.peek().ok();
        let mut candidate_ratio = committed_ratio;
        let estimator_step = if let Some(observation_step) = capture_observation_step.as_ref() {
            let step = self
                .capture_estimator
                .preview(observation_step.value().clock(), clock)
                .map_err(|error| {
                    mark_fault(
                        &self.shared,
                        BridgeError::Estimator { domain: BridgeClockDomain::WaveCapture, error },
                    )
                })?;
            candidate_ratio = step.ratio();
            Some(step)
        } else {
            if state == BridgeState::Active {
                validate_estimator_freshness(&self.capture_estimator, clock).map_err(|error| {
                    mark_fault(
                        &self.shared,
                        BridgeError::Estimator { domain: BridgeClockDomain::WaveCapture, error },
                    )
                })?;
            }
            None
        };
        let nominal_state =
            matches!(state, BridgeState::CaptureFilterDelay | BridgeState::CaptureEstimatorPriming);
        let controller_step = if nominal_state {
            None
        } else {
            let Some(measured_feed_forward) = candidate_ratio else {
                return fault(
                    &self.shared,
                    BridgeError::ObservationUnavailable { domain: BridgeClockDomain::WaveCapture },
                );
            };
            Some(self.capture_controller.preview(fill, measured_feed_forward).map_err(|error| {
                mark_fault(
                    &self.shared,
                    BridgeError::Controller { domain: BridgeClockDomain::WaveCapture, error },
                )
            })?)
        };
        let ratio =
            controller_step.as_ref().map_or(1.0, librewave_engine::RateMatchControllerStep::ratio);
        let cycle = self
            .capture_matcher
            .prepare(quantum_frames, ratio, &mut self.capture_matched[..quantum_frames])
            .map_err(|error| {
                mark_fault(
                    &self.shared,
                    BridgeError::Asrc { domain: BridgeClockDomain::WaveCapture, error },
                )
            })?;
        let capture_input_frames = cycle.input_frames();
        let capture_fifo_step = self
            .capture_consumer
            .peek_exact(&mut self.capture_input[..capture_input_frames])
            .map_err(|error| {
                mark_fault(&self.shared, map_peek(error, BridgeClockDomain::WaveCapture))
            })?;
        cycle.process(&self.capture_input[..capture_input_frames]).map_err(|error| {
            mark_fault(
                &self.shared,
                BridgeError::Asrc { domain: BridgeClockDomain::WaveCapture, error },
            )
        })?;

        for (frame, sample) in self.capture_matched[..quantum_frames].iter().copied().enumerate() {
            self.microphone_input[frame * 2] = sample;
            self.microphone_input[frame * 2 + 1] = sample;
        }
        let inputs = [
            InputBuffer::new(MICROPHONE_SOURCE_ID, &self.microphone_input[..sample_count]),
            InputBuffer::new(SYSTEM_SOURCE_ID, &self.system_input[..sample_count]),
        ];
        let mut engine_outputs = [
            OutputBuffer::new(EndpointId::Microphone, &mut self.microphone_output[..sample_count]),
            OutputBuffer::new(EndpointId::MonitorMix, &mut self.monitor_output[..sample_count]),
            OutputBuffer::new(EndpointId::StreamMix, &mut self.stream_output[..sample_count]),
        ];
        let engine_report = self
            .engine
            .process(quantum_frames, &inputs, &mut engine_outputs)
            .map_err(|error| mark_fault(&self.shared, BridgeError::Engine(error)))?;
        validate_finite_output(&self.shared, &self.microphone_output[..sample_count])?;
        validate_finite_output(&self.shared, &self.monitor_output[..sample_count])?;
        validate_finite_output(&self.shared, &self.stream_output[..sample_count])?;

        let playback_fifo_step =
            self.playback_producer.prepare_push(&self.monitor_output[..sample_count]).map_err(
                |error| mark_fault(&self.shared, map_push(error, BridgeClockDomain::WavePlayback)),
            )?;
        let playback_fill = playback_fifo_step.fill_after_commit();
        let (graph_observation_step, observation_publication) =
            match self.graph_observation_publisher.prepare_publish(observation) {
                Ok(step) => (Some(step), ObservationPublication::Published),
                Err(_) => (None, ObservationPublication::NotPublishedHandoffFull),
            };
        let delivery = graph_delivery(state);
        if delivery == GraphBoundaryDelivery::Delivered {
            deinterleave(
                &self.microphone_output[..sample_count],
                outputs.microphone_left,
                outputs.microphone_right,
            );
            deinterleave(
                &self.monitor_output[..sample_count],
                outputs.monitor_left,
                outputs.monitor_right,
            );
            deinterleave(
                &self.stream_output[..sample_count],
                outputs.stream_left,
                outputs.stream_right,
            );
        } else {
            clear_graph_outputs(&mut outputs);
        }

        capture_fifo_step.commit();
        playback_fifo_step.commit();
        if let Some(step) = capture_observation_step {
            step.commit();
        }
        if let Some(step) = graph_observation_step {
            step.commit();
        }
        if let Some(step) = estimator_step {
            step.commit();
        }
        if let Some(controller_step) = controller_step {
            controller_step.commit();
        }
        self.last_observation = Some(clock);
        self.fixed_quantum = Some(quantum_frames);
        self.finish_boundary(state, quantum_frames, playback_fill, candidate_ratio.is_some());
        let _ = self.meter_publisher.try_publish(engine_report.meters());
        Ok(GraphProcessReport {
            start_frame: graph_start_frame,
            end_frame: graph_end_frame,
            frames: quantum_frames,
            capture_input_frames,
            control_update_applied: engine_report.control_update_applied(),
            delivery,
            observation_publication,
        })
    }

    fn finish_boundary(
        &self,
        starting_state: BridgeState,
        quantum_frames: usize,
        playback_fill: usize,
        estimator_measured: bool,
    ) {
        if starting_state == BridgeState::CaptureFilterDelay {
            let previous = self.shared.capture_delay_remaining.load(Ordering::Acquire);
            let remaining = previous.saturating_sub(quantum_frames);
            self.shared.capture_delay_remaining.store(remaining, Ordering::Release);
            if remaining == 0 {
                let next = if estimator_measured {
                    BridgeState::PlaybackPriming
                } else {
                    BridgeState::CaptureEstimatorPriming
                };
                let _ = self.shared.transition(BridgeState::CaptureFilterDelay, next);
            }
        } else if starting_state == BridgeState::CaptureEstimatorPriming && estimator_measured {
            let _ = self
                .shared
                .transition(BridgeState::CaptureEstimatorPriming, BridgeState::PlaybackPriming);
        } else if starting_state == BridgeState::PlaybackPriming
            && playback_fill >= self.shared.playback_target
        {
            let _ = self
                .shared
                .transition(BridgeState::PlaybackPriming, BridgeState::PlaybackFilterDelay);
        }
    }
}

fn prepare_system_input(
    shared: &SharedAttempt,
    system: GraphSystemInput<'_>,
    frames: usize,
    interleaved: &mut [f32],
) -> Result<(), BridgeError> {
    if !system.connected {
        if system.left.is_some() || system.right.is_some() {
            return fault(shared, BridgeError::MissingSystemBuffer);
        }
        interleaved.fill(0.0);
        return Ok(());
    }
    let (Some(left), Some(right)) = (system.left, system.right) else {
        return fault(shared, BridgeError::MissingSystemBuffer);
    };
    if left.len() != frames || right.len() != frames {
        return fault(
            shared,
            BridgeError::SystemLength { expected: frames, left: left.len(), right: right.len() },
        );
    }
    for index in 0..frames {
        if !left[index].is_finite() {
            return fault(
                shared,
                BridgeError::NonFinite {
                    domain: BridgeClockDomain::PipeWireGraph,
                    sample_index: index * 2,
                },
            );
        }
        if !right[index].is_finite() {
            return fault(
                shared,
                BridgeError::NonFinite {
                    domain: BridgeClockDomain::PipeWireGraph,
                    sample_index: index * 2 + 1,
                },
            );
        }
        interleaved[index * 2] = left[index];
        interleaved[index * 2 + 1] = right[index];
    }
    Ok(())
}

fn validate_output_lengths(
    output: &GraphOutputBuffers<'_>,
    frames: usize,
) -> Result<(), BridgeError> {
    for buffer in [
        &*output.microphone_left,
        &*output.microphone_right,
        &*output.monitor_left,
        &*output.monitor_right,
        &*output.stream_left,
        &*output.stream_right,
    ] {
        if buffer.len() != frames {
            return Err(BridgeError::OutputLength { expected: frames, actual: buffer.len() });
        }
    }
    Ok(())
}

fn validate_finite_output(shared: &SharedAttempt, samples: &[f32]) -> Result<(), BridgeError> {
    if let Some(sample_index) = samples.iter().position(|sample| !sample.is_finite()) {
        return fault(
            shared,
            BridgeError::NonFinite { domain: BridgeClockDomain::PipeWireGraph, sample_index },
        );
    }
    Ok(())
}

fn deinterleave(interleaved: &[f32], left: &mut [f32], right: &mut [f32]) {
    for (index, frame) in interleaved.chunks_exact(2).enumerate() {
        left[index] = frame[0];
        right[index] = frame[1];
    }
}

/// One successful playback-domain period.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PlaybackProcessReport {
    pub application_start_frame: u64,
    pub application_end_frame: u64,
    pub hardware_frame_position: u64,
    pub output_frames: usize,
    pub input_frames: usize,
    pub delivery: PlaybackBoundaryDelivery,
}

/// How one state-driven playback boundary handled its output period.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PlaybackBoundaryDelivery {
    DiscardedFilterDelay,
    SubmittedEstimatorPriming,
    SubmittedStabilizing,
    SubmittedActive,
}

/// The playback-worker owner of the stereo matcher and FIFO consumer.
pub struct PlaybackEgress {
    consumer: Consumer<WAVE3_PLAYBACK_CHANNELS>,
    capture_fill_observer: FillObserver<WAVE3_CAPTURE_CHANNELS>,
    graph_observation_reader: CopySlotReader<GraphClockObservation>,
    estimator: ClockRateEstimator,
    matcher: RateMatcher,
    controller: RateMatchController,
    input: Vec<f32>,
    output: Vec<f32>,
    encoded: Vec<u8>,
    period_frames: usize,
    submitted_position: u64,
    last_hardware_observation: Option<ClockObservation>,
    shared: Arc<SharedAttempt>,
}

impl fmt::Debug for PlaybackEgress {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("PlaybackEgress")
            .field("period_frames", &self.period_frames)
            .field("submitted_position", &self.submitted_position)
            .field("last_hardware_observation", &self.last_hardware_observation)
            .finish_non_exhaustive()
    }
}

impl PlaybackEgress {
    pub(super) fn worker_attempt_epoch(&self) -> ClockAttemptEpoch {
        self.shared.attempt_epoch
    }

    pub(super) const fn worker_application_position(&self) -> u64 {
        self.submitted_position
    }

    pub(super) fn worker_fault(&self) {
        self.shared.fault();
    }

    pub(super) fn worker_state(&self) -> BridgeState {
        self.shared.state()
    }

    /// Processes one observed playback boundary according to attempt state.
    ///
    /// # Errors
    ///
    /// Returns a state error before work starts, or a terminal error for
    /// observation, FIFO, estimator, controller, ASRC, packed conversion,
    /// submitter failure, or wrong application progress.
    #[allow(clippy::too_many_lines)]
    pub fn process(
        &mut self,
        observation: PlaybackClockObservation,
        submitter: &mut dyn PlaybackPeriodSubmitter,
    ) -> Result<PlaybackProcessReport, BridgeError> {
        let _activity = ActivityGuard::enter(&self.shared.playback_in_flight);
        let state = attempt_state(&self.shared)?;
        if !matches!(
            state,
            BridgeState::PlaybackFilterDelay
                | BridgeState::PlaybackEstimatorPriming
                | BridgeState::PlaybackStabilizing
                | BridgeState::Active
        ) {
            return Err(BridgeError::State { actual: state });
        }
        let hardware = observation.hardware();
        validate_observation_epoch(&self.shared, BridgeClockDomain::WavePlayback, hardware)
            .map_err(|error| mark_fault(&self.shared, error))?;
        let observed_application_position = observation.application_frame_position().get();
        if observed_application_position != self.submitted_position {
            return fault(
                &self.shared,
                BridgeError::PlaybackApplicationPosition {
                    expected: self.submitted_position,
                    actual: observed_application_position,
                },
            );
        }
        let hardware_position = hardware.frame_position().get();
        if hardware_position > self.submitted_position {
            return fault(
                &self.shared,
                BridgeError::PlaybackHardwareAhead {
                    hardware: hardware_position,
                    application: self.submitted_position,
                },
            );
        }
        let hardware_progress = playback_hardware_progress(
            BridgeClockDomain::WavePlayback,
            self.last_hardware_observation,
            hardware,
            state == BridgeState::Active,
        )
        .map_err(|error| mark_fault(&self.shared, error))?;
        let submits_period = state != BridgeState::PlaybackFilterDelay;
        let application_start_frame = self.submitted_position;
        let application_end_frame = if submits_period {
            checked_end(
                BridgeClockDomain::WavePlayback,
                application_start_frame,
                self.period_frames,
            )
            .map_err(|error| mark_fault(&self.shared, error))?
        } else {
            application_start_frame
        };
        let fill = self.consumer.available_frames().map_err(|error| {
            mark_fault(&self.shared, map_sequence(error, BridgeClockDomain::WavePlayback))
        })?;
        let committed_ratio = self.estimator.ratio();
        let mut candidate_ratio = committed_ratio;
        let graph_observation_step = if state != BridgeState::PlaybackFilterDelay
            && hardware_progress != HardwareProgress::Repeated
        {
            self.graph_observation_reader.peek().ok()
        } else {
            None
        };
        let estimator_step = if let Some(graph_step) = graph_observation_step.as_ref() {
            let step =
                self.estimator.preview(graph_step.value().clock(), hardware).map_err(|error| {
                    mark_fault(
                        &self.shared,
                        BridgeError::Estimator { domain: BridgeClockDomain::WavePlayback, error },
                    )
                })?;
            candidate_ratio = step.ratio();
            Some(step)
        } else {
            if state == BridgeState::Active {
                validate_estimator_freshness(&self.estimator, hardware).map_err(|error| {
                    mark_fault(
                        &self.shared,
                        BridgeError::Estimator { domain: BridgeClockDomain::WavePlayback, error },
                    )
                })?;
            }
            None
        };
        let nominal_state = matches!(
            state,
            BridgeState::PlaybackFilterDelay | BridgeState::PlaybackEstimatorPriming
        );
        let controller_step = if nominal_state {
            None
        } else {
            let Some(measured_feed_forward) = candidate_ratio else {
                return fault(
                    &self.shared,
                    BridgeError::ObservationUnavailable { domain: BridgeClockDomain::WavePlayback },
                );
            };
            Some(self.controller.preview(fill, measured_feed_forward).map_err(|error| {
                mark_fault(
                    &self.shared,
                    BridgeError::Controller { domain: BridgeClockDomain::WavePlayback, error },
                )
            })?)
        };
        let ratio =
            controller_step.as_ref().map_or(1.0, librewave_engine::RateMatchControllerStep::ratio);
        let cycle =
            self.matcher.prepare(self.period_frames, ratio, &mut self.output).map_err(|error| {
                mark_fault(
                    &self.shared,
                    BridgeError::Asrc { domain: BridgeClockDomain::WavePlayback, error },
                )
            })?;
        let input_frames = cycle.input_frames();
        let input_samples = input_frames.checked_mul(WAVE3_PLAYBACK_CHANNELS).ok_or_else(|| {
            mark_fault(
                &self.shared,
                BridgeError::InvalidPeriod { actual: input_frames, expected: self.period_frames },
            )
        })?;
        let playback_fill_after_commit = fill.checked_sub(input_frames).ok_or_else(|| {
            mark_fault(
                &self.shared,
                BridgeError::Shortage {
                    domain: BridgeClockDomain::WavePlayback,
                    required: input_frames,
                    available: fill,
                },
            )
        })?;
        let fifo_step =
            self.consumer.peek_exact(&mut self.input[..input_samples]).map_err(|error| {
                mark_fault(&self.shared, map_peek(error, BridgeClockDomain::WavePlayback))
            })?;
        cycle.process(&self.input[..input_samples]).map_err(|error| {
            mark_fault(
                &self.shared,
                BridgeError::Asrc { domain: BridgeClockDomain::WavePlayback, error },
            )
        })?;
        encode_s24_3le(&self.output, WAVE3_PLAYBACK_CHANNELS, &mut self.encoded)
            .map_err(|error| mark_fault(&self.shared, BridgeError::Packed(error)))?;
        if submits_period {
            let progress = match submitter.submit_packed_s24_3le(self.period_frames, &self.encoded)
            {
                Ok(progress) => progress,
                Err(error) => {
                    let accepted_position = accepted_application_position(
                        application_start_frame,
                        error.accepted_frames(),
                        self.period_frames,
                    )
                    .map_err(|error| mark_fault(&self.shared, error))?;
                    self.submitted_position = accepted_position;
                    if matches!(error, PlaybackWriteError::Stopped { accepted_frames: 0 }) {
                        return Err(BridgeError::Playback(error));
                    }
                    return Err(mark_fault(&self.shared, BridgeError::Playback(error)));
                }
            };
            if progress.frames_submitted != self.period_frames {
                let accepted_position = accepted_application_position(
                    application_start_frame,
                    progress.frames_submitted,
                    self.period_frames,
                )
                .map_err(|error| mark_fault(&self.shared, error))?;
                self.submitted_position = accepted_position;
                return fault(
                    &self.shared,
                    BridgeError::PlaybackSubmission {
                        expected_frames: self.period_frames,
                        actual_frames: progress.frames_submitted,
                    },
                );
            }
        }
        let priming_gate = if state == BridgeState::PlaybackStabilizing
            && hardware_progress == HardwareProgress::Advanced
        {
            Some(prepare_priming_gate(
                &self.shared,
                &self.capture_fill_observer,
                playback_fill_after_commit,
            )?)
        } else {
            None
        };

        fifo_step.commit();
        if let Some(step) = graph_observation_step {
            step.commit();
        }
        if let Some(step) = estimator_step {
            step.commit();
        }
        if let Some(controller_step) = controller_step {
            controller_step.commit();
        }
        if hardware_progress != HardwareProgress::Repeated {
            self.last_hardware_observation = Some(hardware);
        }
        self.submitted_position = application_end_frame;
        if state == BridgeState::PlaybackFilterDelay {
            let delay = self.shared.playback_delay_remaining.load(Ordering::Acquire);
            self.shared
                .playback_delay_remaining
                .store(delay.saturating_sub(self.period_frames), Ordering::Release);
            if delay <= self.period_frames {
                let _ = self.shared.transition(
                    BridgeState::PlaybackFilterDelay,
                    BridgeState::PlaybackEstimatorPriming,
                );
            }
        } else if state == BridgeState::PlaybackEstimatorPriming && candidate_ratio.is_some() {
            let _ = self.shared.transition(
                BridgeState::PlaybackEstimatorPriming,
                BridgeState::PlaybackStabilizing,
            );
        } else if let Some(next) = priming_gate {
            let _ = self.shared.transition(BridgeState::PrimingCheck, next);
        }
        Ok(PlaybackProcessReport {
            application_start_frame,
            application_end_frame,
            hardware_frame_position: hardware_position,
            output_frames: self.period_frames,
            input_frames,
            delivery: playback_delivery(state),
        })
    }
}

fn accepted_application_position(
    application_start_frame: u64,
    accepted_frames: usize,
    period_frames: usize,
) -> Result<u64, BridgeError> {
    if accepted_frames > period_frames {
        return Err(BridgeError::PlaybackSubmission {
            expected_frames: period_frames,
            actual_frames: accepted_frames,
        });
    }
    checked_end(BridgeClockDomain::WavePlayback, application_start_frame, accepted_frames)
}

fn prepare_priming_gate(
    shared: &SharedAttempt,
    capture_fill_observer: &FillObserver<WAVE3_CAPTURE_CHANNELS>,
    playback_fill: usize,
) -> Result<BridgeState, BridgeError> {
    if !shared.transition(BridgeState::PlaybackStabilizing, BridgeState::PrimingCheck) {
        return Err(BridgeError::State { actual: shared.state() });
    }

    // Activity counters increment before their owner reads the state. The
    // sequentially consistent gate therefore either observes an earlier
    // owner in flight or makes that owner observe PrimingCheck and stop before
    // it can publish a FIFO sequence.
    let capture_in_flight = shared.capture_in_flight.load(Ordering::SeqCst);
    let graph_in_flight = shared.graph_in_flight.load(Ordering::SeqCst);
    if capture_in_flight != 0 || graph_in_flight != 0 {
        return Ok(BridgeState::PlaybackStabilizing);
    }
    let capture_fill = capture_fill_observer
        .stable_fill_frames()
        .map_err(|error| mark_fault(shared, map_sequence(error, BridgeClockDomain::WaveCapture)))?;
    Ok(if shared.within_startup_guards(capture_fill, playback_fill) {
        BridgeState::Primed
    } else {
        BridgeState::PlaybackStabilizing
    })
}

fn attempt_state(shared: &SharedAttempt) -> Result<BridgeState, BridgeError> {
    let state = shared.state();
    match state {
        BridgeState::Faulted => Err(BridgeError::AttemptFaulted),
        BridgeState::Quiescing | BridgeState::Stopped => Err(BridgeError::Teardown),
        BridgeState::PrimingCheck => Err(BridgeError::State { actual: state }),
        BridgeState::Active
        | BridgeState::Primed
        | BridgeState::Allocated
        | BridgeState::CapturePriming
        | BridgeState::CaptureFilterDelay
        | BridgeState::CaptureEstimatorPriming
        | BridgeState::PlaybackPriming
        | BridgeState::PlaybackFilterDelay
        | BridgeState::PlaybackEstimatorPriming
        | BridgeState::PlaybackStabilizing => Ok(state),
    }
}

fn validate_observation_epoch(
    shared: &SharedAttempt,
    domain: BridgeClockDomain,
    observation: ClockObservation,
) -> Result<(), BridgeError> {
    if observation.epoch() != shared.attempt_epoch {
        return Err(BridgeError::ObservationEpoch {
            domain,
            expected: shared.attempt_epoch,
            actual: observation.epoch(),
        });
    }
    Ok(())
}

fn validate_clock_progress(
    domain: BridgeClockDomain,
    previous: Option<ClockObservation>,
    actual: ClockObservation,
    reject_repeated: bool,
) -> Result<(), BridgeError> {
    let Some(previous) = previous else {
        return Ok(());
    };
    let previous_frame = previous.frame_position().get();
    let actual_frame = actual.frame_position().get();
    if actual_frame < previous_frame {
        return Err(BridgeError::Discontinuity {
            domain,
            expected: previous_frame,
            actual: actual_frame,
        });
    }
    if actual_frame == previous_frame && reject_repeated {
        return Err(BridgeError::RepeatedClockObservation { domain });
    }
    let previous_time = previous.monotonic_time().get();
    let actual_time = actual.monotonic_time().get();
    if actual_time <= previous_time && (actual_frame != previous_frame || reject_repeated) {
        return Err(BridgeError::MonotonicTimeDiscontinuity {
            domain,
            previous: previous_time,
            actual: actual_time,
        });
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum HardwareProgress {
    First,
    Repeated,
    Advanced,
}

fn playback_hardware_progress(
    domain: BridgeClockDomain,
    previous: Option<ClockObservation>,
    actual: ClockObservation,
    active: bool,
) -> Result<HardwareProgress, BridgeError> {
    let Some(previous) = previous else {
        return Ok(HardwareProgress::First);
    };
    validate_clock_progress(domain, Some(previous), actual, active)?;
    if actual.frame_position() == previous.frame_position() {
        Ok(HardwareProgress::Repeated)
    } else {
        Ok(HardwareProgress::Advanced)
    }
}

fn validate_estimator_freshness(
    estimator: &ClockRateEstimator,
    current_output: ClockObservation,
) -> Result<(), ClockRateEstimatorError> {
    let Some(latest_input) = estimator.latest_input() else {
        return Ok(());
    };
    let actual =
        latest_input.monotonic_time().get().abs_diff(current_output.monotonic_time().get());
    let maximum = estimator.config().maximum_observation_skew_nanoseconds();
    if actual > maximum {
        return Err(ClockRateEstimatorError::ObservationSkew {
            actual_nanoseconds: actual,
            maximum_nanoseconds: maximum,
        });
    }
    Ok(())
}

fn graph_delivery(state: BridgeState) -> GraphBoundaryDelivery {
    match state {
        BridgeState::CaptureFilterDelay => GraphBoundaryDelivery::DiscardedFilterDelay,
        BridgeState::CaptureEstimatorPriming => GraphBoundaryDelivery::DiscardedEstimatorPriming,
        BridgeState::Active => GraphBoundaryDelivery::Delivered,
        BridgeState::Allocated
        | BridgeState::CapturePriming
        | BridgeState::PlaybackPriming
        | BridgeState::PlaybackFilterDelay
        | BridgeState::PlaybackEstimatorPriming
        | BridgeState::PlaybackStabilizing
        | BridgeState::PrimingCheck
        | BridgeState::Primed
        | BridgeState::Faulted
        | BridgeState::Quiescing
        | BridgeState::Stopped => GraphBoundaryDelivery::DiscardedStartup,
    }
}

fn playback_delivery(state: BridgeState) -> PlaybackBoundaryDelivery {
    match state {
        BridgeState::PlaybackEstimatorPriming => {
            PlaybackBoundaryDelivery::SubmittedEstimatorPriming
        }
        BridgeState::PlaybackStabilizing => PlaybackBoundaryDelivery::SubmittedStabilizing,
        BridgeState::Active => PlaybackBoundaryDelivery::SubmittedActive,
        BridgeState::Allocated
        | BridgeState::CapturePriming
        | BridgeState::CaptureFilterDelay
        | BridgeState::CaptureEstimatorPriming
        | BridgeState::PlaybackPriming
        | BridgeState::PlaybackFilterDelay
        | BridgeState::PrimingCheck
        | BridgeState::Primed
        | BridgeState::Faulted
        | BridgeState::Quiescing
        | BridgeState::Stopped => PlaybackBoundaryDelivery::DiscardedFilterDelay,
    }
}

fn clear_graph_outputs(outputs: &mut GraphOutputBuffers<'_>) {
    outputs.microphone_left.fill(0.0);
    outputs.microphone_right.fill(0.0);
    outputs.monitor_left.fill(0.0);
    outputs.monitor_right.fill(0.0);
    outputs.stream_left.fill(0.0);
    outputs.stream_right.fill(0.0);
}

fn checked_end(domain: BridgeClockDomain, start: u64, frames: usize) -> Result<u64, BridgeError> {
    let frames = u64::try_from(frames).map_err(|_| BridgeError::PositionOverflow {
        domain,
        start,
        frames,
    })?;
    start.checked_add(frames).ok_or(BridgeError::PositionOverflow {
        domain,
        start,
        frames: usize::try_from(frames).unwrap_or(usize::MAX),
    })
}

fn map_push(error: RingPushError, domain: BridgeClockDomain) -> BridgeError {
    match error {
        RingPushError::PartialFrame { samples, .. } => {
            BridgeError::Packed(PackedS24Error::PartialFrame {
                samples,
                channels: match domain {
                    BridgeClockDomain::WaveCapture => WAVE3_CAPTURE_CHANNELS,
                    BridgeClockDomain::PipeWireGraph | BridgeClockDomain::WavePlayback => {
                        WAVE3_PLAYBACK_CHANNELS
                    }
                },
            })
        }
        RingPushError::NonFinite { sample_index } => {
            BridgeError::NonFinite { domain, sample_index }
        }
        RingPushError::Overflow { requested, remaining, capacity } => {
            BridgeError::Overflow { domain, requested, remaining, capacity }
        }
        RingPushError::Sequence(error) => map_sequence(error, domain),
    }
}

fn map_peek(error: RingPeekError, domain: BridgeClockDomain) -> BridgeError {
    match error {
        RingPeekError::Shortage { required, available } => {
            BridgeError::Shortage { domain, required, available }
        }
        RingPeekError::PartialFrame { samples, channels } => {
            BridgeError::Packed(PackedS24Error::PartialFrame { samples, channels })
        }
        RingPeekError::Sequence(error) => map_sequence(error, domain),
    }
}

fn map_sequence(_error: RingSequenceError, domain: BridgeClockDomain) -> BridgeError {
    BridgeError::FifoSequence { domain }
}

fn mark_fault(shared: &SharedAttempt, error: BridgeError) -> BridgeError {
    shared.fault();
    error
}

fn fault<T>(shared: &SharedAttempt, error: BridgeError) -> Result<T, BridgeError> {
    Err(mark_fault(shared, error))
}
