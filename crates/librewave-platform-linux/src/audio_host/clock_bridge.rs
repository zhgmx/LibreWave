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
    ControlStager, InputBuffer, MeterPublisher, MeterReader, MixerConfig, MixerEngine,
    OutputBuffer, ProcessError, RateMatchController, RateMatchControllerConfig,
    RateMatchControllerError, RateMatchError, RateMatcher, RateMatcherConfig, StageError,
};
use std::fmt;
use std::sync::Arc;
use std::sync::atomic::{AtomicU8, AtomicUsize, Ordering};

mod spsc;

use spsc::{
    Consumer, FillObserver, Producer, RingBuildError, RingPeekError, RingPushError,
    RingSequenceError,
};

#[cfg(test)]
#[path = "clock_bridge/tests.rs"]
mod tests;

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
    PlaybackPriming = 3,
    PlaybackFilterDelay = 4,
    PlaybackStabilizing = 5,
    PrimingCheck = 6,
    Primed = 7,
    Active = 8,
    Faulted = 9,
    Quiescing = 10,
    Stopped = 11,
}

impl BridgeState {
    fn from_raw(raw: u8) -> Self {
        match raw {
            0 => Self::Allocated,
            1 => Self::CapturePriming,
            2 => Self::CaptureFilterDelay,
            3 => Self::PlaybackPriming,
            4 => Self::PlaybackFilterDelay,
            5 => Self::PlaybackStabilizing,
            6 => Self::PrimingCheck,
            7 => Self::Primed,
            8 => Self::Active,
            9 => Self::Faulted,
            10 => Self::Quiescing,
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
}

impl DirectionBridgeConfig {
    /// Checks one direction's caller-selected resource and control policy.
    ///
    /// # Errors
    ///
    /// Returns an error for zero capacity, mismatched ratio bounds, or a
    /// startup range that does not fit in the FIFO.
    pub fn try_new(
        fifo_capacity_frames: usize,
        startup_guard_frames: usize,
        matcher: RateMatcherConfig,
        controller: RateMatchControllerConfig,
    ) -> Result<Self, BridgeBuildError> {
        if fifo_capacity_frames == 0 {
            return Err(BridgeBuildError::ZeroFifoCapacity);
        }
        if matcher.ratio_bounds() != controller.ratio_bounds() {
            return Err(BridgeBuildError::RatioBoundsDiffer);
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
        Ok(Self { fifo_capacity_frames, startup_guard_frames, matcher, controller })
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
        Ok(Self { maximum_graph_quantum_frames, capture, playback })
    }
}

/// Why an offline bridge could not be built.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum BridgeBuildError {
    ZeroFifoCapacity,
    ZeroMaximumGraphQuantum,
    RatioBoundsDiffer,
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

/// Stable fake-writer failure classes.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PlaybackWriteError {
    XrunRecoveryFailed,
    Disconnected,
    Failed,
}

/// Playback-domain progress reported after one fake worker write.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PlaybackWriteProgress {
    pub start_frame: u64,
    pub frames_written: usize,
}

/// The delivery boundary used by the offline playback worker.
pub trait PlaybackPeriodWriter: Send {
    /// Delivers one complete packed playback period and reports its progress.
    ///
    /// # Errors
    ///
    /// Returns a classified worker failure if the period was not delivered.
    fn write_packed_s24_3le(
        &mut self,
        start_frame: u64,
        complete_frame_count: usize,
        bytes: &[u8],
    ) -> Result<PlaybackWriteProgress, PlaybackWriteError>;
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
    InvalidQuantum {
        actual: usize,
        maximum: usize,
    },
    InvalidPeriod {
        actual: usize,
        expected: usize,
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
    Asrc {
        domain: BridgeClockDomain,
        error: RateMatchError,
    },
    Engine(ProcessError),
    Playback(PlaybackWriteError),
    PlaybackProgress {
        expected_start: u64,
        actual_start: u64,
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
    shared: Arc<SharedAttempt>,
}

impl ClockBridgeControl {
    #[must_use]
    pub fn state(&self) -> BridgeState {
        self.shared.state()
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

    /// Enables fake offline delivery after all preflight work completes.
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
        let (playback_producer, playback_consumer, _playback_fill_observer) =
            spsc::channel::<WAVE3_PLAYBACK_CHANNELS>(config.playback.fifo_capacity_frames)
                .map_err(map_ring_build)?;
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
                expected_position: None,
                shared: Arc::clone(&shared),
            },
            graph: GraphClockProcessor {
                capture_consumer,
                playback_producer,
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
                expected_position: None,
                shared: Arc::clone(&shared),
            },
            playback: PlaybackEgress {
                consumer: playback_consumer,
                capture_fill_observer,
                matcher: playback_matcher,
                controller: RateMatchController::new(config.playback.controller),
                input: allocate_zeroed(playback_input_samples)?,
                output: allocate_zeroed(playback_output_samples)?,
                encoded: allocate_zeroed(playback_bytes)?,
                period_frames: playback_period,
                expected_position: None,
                shared: Arc::clone(&shared),
            },
            control: ClockBridgeControl { stager, shared },
            meters,
        })
    }
}

fn map_ring_build(_error: RingBuildError) -> BridgeBuildError {
    BridgeBuildError::Allocation
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
    expected_position: Option<u64>,
    shared: Arc<SharedAttempt>,
}

/// One accepted capture-domain publication.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CapturePublishReport {
    pub start_frame: u64,
    pub end_frame: u64,
    pub frames: usize,
    pub fifo_fill_frames: usize,
}

impl CaptureIngress {
    /// Publishes one complete capture-domain block without waiting.
    ///
    /// # Errors
    ///
    /// Returns a state error before work starts, or a terminal error for
    /// geometry, position, packed data, non-finite samples, or insufficient
    /// FIFO capacity.
    pub fn publish(
        &mut self,
        start_frame: u64,
        complete_frame_count: usize,
        packed_s24_3le: &[u8],
    ) -> Result<CapturePublishReport, BridgeError> {
        let _activity = ActivityGuard::enter(&self.shared.capture_in_flight);
        let state = attempt_state(&self.shared)?;
        if state == BridgeState::Allocated {
            return Err(BridgeError::State { actual: state });
        }
        if complete_frame_count == 0 || complete_frame_count > self.maximum_frames {
            return fault(
                &self.shared,
                BridgeError::FrameCount {
                    domain: BridgeClockDomain::WaveCapture,
                    actual: complete_frame_count,
                    maximum: self.maximum_frames,
                },
            );
        }
        validate_position(BridgeClockDomain::WaveCapture, self.expected_position, start_frame)
            .map_err(|error| mark_fault(&self.shared, error))?;
        let end_frame =
            checked_end(BridgeClockDomain::WaveCapture, start_frame, complete_frame_count)
                .map_err(|error| mark_fault(&self.shared, error))?;
        let encoded_frames = packed_frame_count(packed_s24_3le.len(), WAVE3_CAPTURE_CHANNELS)
            .map_err(|error| mark_fault(&self.shared, BridgeError::Packed(error)))?;
        if encoded_frames != complete_frame_count {
            return fault(
                &self.shared,
                BridgeError::FrameCountMismatch {
                    domain: BridgeClockDomain::WaveCapture,
                    declared: complete_frame_count,
                    encoded: encoded_frames,
                },
            );
        }
        let remaining = self.producer.free_frames().map_err(|error| {
            mark_fault(&self.shared, map_sequence(error, BridgeClockDomain::WaveCapture))
        })?;
        if complete_frame_count > remaining {
            return fault(
                &self.shared,
                BridgeError::Overflow {
                    domain: BridgeClockDomain::WaveCapture,
                    requested: complete_frame_count,
                    remaining,
                    capacity: self.producer.capacity_frames(),
                },
            );
        }
        decode_s24_3le(
            packed_s24_3le,
            WAVE3_CAPTURE_CHANNELS,
            &mut self.decode[..complete_frame_count],
        )
        .map_err(|error| mark_fault(&self.shared, BridgeError::Packed(error)))?;
        self.producer.try_push(&self.decode[..complete_frame_count]).map_err(|error| {
            mark_fault(&self.shared, map_push(error, BridgeClockDomain::WaveCapture))
        })?;
        self.expected_position = Some(end_frame);
        let fill = self.producer.fill_frames().map_err(|error| {
            mark_fault(&self.shared, map_sequence(error, BridgeClockDomain::WaveCapture))
        })?;
        if self.shared.state() == BridgeState::CapturePriming && fill >= self.shared.capture_target
        {
            let _ = self
                .shared
                .transition(BridgeState::CapturePriming, BridgeState::CaptureFilterDelay);
        }
        Ok(CapturePublishReport {
            start_frame,
            end_frame,
            frames: complete_frame_count,
            fifo_fill_frames: fill,
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
    pub preflight_discarded: bool,
}

/// The PipeWire-data-loop owner of the mixer and capture bridge consumer.
pub struct GraphClockProcessor {
    capture_consumer: Consumer<WAVE3_CAPTURE_CHANNELS>,
    playback_producer: Producer<WAVE3_PLAYBACK_CHANNELS>,
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
    expected_position: Option<u64>,
    shared: Arc<SharedAttempt>,
}

impl fmt::Debug for GraphClockProcessor {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("GraphClockProcessor")
            .field("maximum_quantum", &self.maximum_quantum)
            .field("expected_position", &self.expected_position)
            .finish_non_exhaustive()
    }
}

impl GraphClockProcessor {
    /// Processes one complete graph boundary during explicit preflight.
    ///
    /// # Errors
    ///
    /// Returns a state error before work starts, or a terminal error for
    /// position, quantum, System input, FIFO state, controller, ASRC, mixer,
    /// or output data.
    pub fn process_preflight(
        &mut self,
        graph_start_frame: u64,
        quantum_frames: usize,
        system: GraphSystemInput<'_>,
    ) -> Result<GraphProcessReport, BridgeError> {
        self.process(graph_start_frame, quantum_frames, system, None)
    }

    /// Processes and exposes one complete active fake graph boundary.
    ///
    /// # Errors
    ///
    /// Returns a state error before work starts, or a terminal error for
    /// position, quantum, System input, output buffers, FIFO state,
    /// controller, ASRC, mixer, or samples.
    pub fn process_active(
        &mut self,
        graph_start_frame: u64,
        quantum_frames: usize,
        system: GraphSystemInput<'_>,
        outputs: GraphOutputBuffers<'_>,
    ) -> Result<GraphProcessReport, BridgeError> {
        self.process(graph_start_frame, quantum_frames, system, Some(outputs))
    }

    #[allow(clippy::too_many_lines)]
    fn process(
        &mut self,
        graph_start_frame: u64,
        quantum_frames: usize,
        system: GraphSystemInput<'_>,
        mut outputs: Option<GraphOutputBuffers<'_>>,
    ) -> Result<GraphProcessReport, BridgeError> {
        let _activity = ActivityGuard::enter(&self.shared.graph_in_flight);
        let state = attempt_state(&self.shared)?;
        if outputs.is_some() && state != BridgeState::Active {
            return Err(BridgeError::State { actual: state });
        }
        if outputs.is_none()
            && !matches!(
                state,
                BridgeState::CaptureFilterDelay
                    | BridgeState::PlaybackPriming
                    | BridgeState::PlaybackFilterDelay
                    | BridgeState::PlaybackStabilizing
            )
        {
            return Err(BridgeError::State { actual: state });
        }
        if quantum_frames == 0 || quantum_frames > self.maximum_quantum {
            return fault(
                &self.shared,
                BridgeError::InvalidQuantum {
                    actual: quantum_frames,
                    maximum: self.maximum_quantum,
                },
            );
        }
        validate_position(
            BridgeClockDomain::PipeWireGraph,
            self.expected_position,
            graph_start_frame,
        )
        .map_err(|error| mark_fault(&self.shared, error))?;
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
        if let Some(output) = outputs.as_ref() {
            validate_output_lengths(output, quantum_frames)
                .map_err(|error| mark_fault(&self.shared, error))?;
        }
        let playback_remaining = self.playback_producer.free_frames().map_err(|error| {
            mark_fault(&self.shared, map_sequence(error, BridgeClockDomain::WavePlayback))
        })?;
        if quantum_frames > playback_remaining {
            return fault(
                &self.shared,
                BridgeError::Overflow {
                    domain: BridgeClockDomain::WavePlayback,
                    requested: quantum_frames,
                    remaining: playback_remaining,
                    capacity: self.playback_producer.capacity_frames(),
                },
            );
        }

        let fill = self.capture_consumer.available_frames().map_err(|error| {
            mark_fault(&self.shared, map_sequence(error, BridgeClockDomain::WaveCapture))
        })?;
        let controller_step = if state == BridgeState::CaptureFilterDelay {
            None
        } else {
            Some(self.capture_controller.preview(fill).map_err(|error| {
                mark_fault(
                    &self.shared,
                    BridgeError::Controller { domain: BridgeClockDomain::WaveCapture, error },
                )
            })?)
        };
        let ratio = controller_step
            .as_ref()
            .map_or(1.0, librewave_engine::RateMatchControllerStep::relative_ratio);
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
        self.capture_consumer.peek_exact(&mut self.capture_input[..capture_input_frames]).map_err(
            |error| mark_fault(&self.shared, map_peek(error, BridgeClockDomain::WaveCapture)),
        )?;
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

        self.playback_producer.try_push(&self.monitor_output[..sample_count]).map_err(|error| {
            mark_fault(&self.shared, map_push(error, BridgeClockDomain::WavePlayback))
        })?;
        if let Some(output) = outputs.as_mut() {
            deinterleave(
                &self.microphone_output[..sample_count],
                output.microphone_left,
                output.microphone_right,
            );
            deinterleave(
                &self.monitor_output[..sample_count],
                output.monitor_left,
                output.monitor_right,
            );
            deinterleave(
                &self.stream_output[..sample_count],
                output.stream_left,
                output.stream_right,
            );
        }
        let _ = self.meter_publisher.try_publish(engine_report.meters());

        self.capture_consumer.commit_peek().map_err(|error| {
            mark_fault(&self.shared, map_sequence(error, BridgeClockDomain::WaveCapture))
        })?;
        if let Some(controller_step) = controller_step {
            controller_step.commit();
        }
        self.expected_position = Some(graph_end_frame);
        let playback_fill = self.playback_producer.fill_frames().map_err(|error| {
            mark_fault(&self.shared, map_sequence(error, BridgeClockDomain::WavePlayback))
        })?;
        if outputs.is_none() {
            self.finish_preflight_boundary(state, quantum_frames, playback_fill);
        }
        Ok(GraphProcessReport {
            start_frame: graph_start_frame,
            end_frame: graph_end_frame,
            frames: quantum_frames,
            capture_input_frames,
            control_update_applied: engine_report.control_update_applied(),
            preflight_discarded: outputs.is_none(),
        })
    }

    fn finish_preflight_boundary(
        &self,
        starting_state: BridgeState,
        quantum_frames: usize,
        playback_fill: usize,
    ) {
        if starting_state == BridgeState::CaptureFilterDelay {
            let previous = self.shared.capture_delay_remaining.load(Ordering::Acquire);
            let remaining = previous.saturating_sub(quantum_frames);
            self.shared.capture_delay_remaining.store(remaining, Ordering::Release);
            if remaining == 0 {
                let _ = self
                    .shared
                    .transition(BridgeState::CaptureFilterDelay, BridgeState::PlaybackPriming);
            }
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
    pub start_frame: u64,
    pub end_frame: u64,
    pub output_frames: usize,
    pub input_frames: usize,
    pub preflight_discarded: bool,
}

/// The playback-worker owner of the stereo matcher and FIFO consumer.
pub struct PlaybackEgress {
    consumer: Consumer<WAVE3_PLAYBACK_CHANNELS>,
    capture_fill_observer: FillObserver<WAVE3_CAPTURE_CHANNELS>,
    matcher: RateMatcher,
    controller: RateMatchController,
    input: Vec<f32>,
    output: Vec<f32>,
    encoded: Vec<u8>,
    period_frames: usize,
    expected_position: Option<u64>,
    shared: Arc<SharedAttempt>,
}

impl fmt::Debug for PlaybackEgress {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("PlaybackEgress")
            .field("period_frames", &self.period_frames)
            .field("expected_position", &self.expected_position)
            .finish_non_exhaustive()
    }
}

impl PlaybackEgress {
    /// Processes and discards one complete playback preflight period.
    ///
    /// # Errors
    ///
    /// Returns a state error before work starts, or a terminal error for
    /// position, FIFO data, controller, ASRC, or packed conversion.
    pub fn process_preflight(
        &mut self,
        playback_start_frame: u64,
    ) -> Result<PlaybackProcessReport, BridgeError> {
        self.process(playback_start_frame, None)
    }

    /// Processes and delivers one complete active fake playback period.
    ///
    /// # Errors
    ///
    /// Returns a state error before work starts, or a terminal error for
    /// position, FIFO data, controller, ASRC, packed conversion, writer
    /// failure, or wrong progress.
    pub fn process_active(
        &mut self,
        playback_start_frame: u64,
        writer: &mut dyn PlaybackPeriodWriter,
    ) -> Result<PlaybackProcessReport, BridgeError> {
        self.process(playback_start_frame, Some(writer))
    }

    #[allow(clippy::too_many_lines)]
    fn process(
        &mut self,
        playback_start_frame: u64,
        mut writer: Option<&mut dyn PlaybackPeriodWriter>,
    ) -> Result<PlaybackProcessReport, BridgeError> {
        let _activity = ActivityGuard::enter(&self.shared.playback_in_flight);
        let state = attempt_state(&self.shared)?;
        if writer.is_some() && state != BridgeState::Active {
            return Err(BridgeError::State { actual: state });
        }
        if writer.is_none()
            && !matches!(state, BridgeState::PlaybackFilterDelay | BridgeState::PlaybackStabilizing)
        {
            return Err(BridgeError::State { actual: state });
        }
        validate_position(
            BridgeClockDomain::WavePlayback,
            self.expected_position,
            playback_start_frame,
        )
        .map_err(|error| mark_fault(&self.shared, error))?;
        let playback_end_frame =
            checked_end(BridgeClockDomain::WavePlayback, playback_start_frame, self.period_frames)
                .map_err(|error| mark_fault(&self.shared, error))?;
        let fill = self.consumer.available_frames().map_err(|error| {
            mark_fault(&self.shared, map_sequence(error, BridgeClockDomain::WavePlayback))
        })?;
        let controller_step = if state == BridgeState::PlaybackFilterDelay {
            None
        } else {
            Some(self.controller.preview(fill).map_err(|error| {
                mark_fault(
                    &self.shared,
                    BridgeError::Controller { domain: BridgeClockDomain::WavePlayback, error },
                )
            })?)
        };
        let ratio = controller_step
            .as_ref()
            .map_or(1.0, librewave_engine::RateMatchControllerStep::relative_ratio);
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
        if let Some(writer) = writer.as_mut() {
            let progress = writer
                .write_packed_s24_3le(playback_start_frame, self.period_frames, &self.encoded)
                .map_err(|error| mark_fault(&self.shared, BridgeError::Playback(error)))?;
            if progress.start_frame != playback_start_frame
                || progress.frames_written != self.period_frames
            {
                return fault(
                    &self.shared,
                    BridgeError::PlaybackProgress {
                        expected_start: playback_start_frame,
                        actual_start: progress.start_frame,
                        expected_frames: self.period_frames,
                        actual_frames: progress.frames_written,
                    },
                );
            }
        }

        self.consumer.commit_peek().map_err(|error| {
            mark_fault(&self.shared, map_sequence(error, BridgeClockDomain::WavePlayback))
        })?;
        if let Some(controller_step) = controller_step {
            controller_step.commit();
        }
        self.expected_position = Some(playback_end_frame);
        if writer.is_none() {
            let delay = self.shared.playback_delay_remaining.load(Ordering::Acquire);
            if state == BridgeState::PlaybackFilterDelay {
                self.shared
                    .playback_delay_remaining
                    .store(delay.saturating_sub(self.period_frames), Ordering::Release);
                if delay <= self.period_frames {
                    let _ = self.shared.transition(
                        BridgeState::PlaybackFilterDelay,
                        BridgeState::PlaybackStabilizing,
                    );
                }
            } else if state == BridgeState::PlaybackStabilizing {
                self.try_enter_primed()?;
            }
        }
        Ok(PlaybackProcessReport {
            start_frame: playback_start_frame,
            end_frame: playback_end_frame,
            output_frames: self.period_frames,
            input_frames,
            preflight_discarded: writer.is_none(),
        })
    }

    fn try_enter_primed(&self) -> Result<(), BridgeError> {
        if !self.shared.transition(BridgeState::PlaybackStabilizing, BridgeState::PrimingCheck) {
            return Ok(());
        }

        // Activity counters increment before their owner reads the state. The
        // sequentially consistent gate therefore either observes an earlier
        // owner in flight or makes that owner observe PrimingCheck and stop
        // before it can change a FIFO.
        let capture_in_flight = self.shared.capture_in_flight.load(Ordering::SeqCst);
        let graph_in_flight = self.shared.graph_in_flight.load(Ordering::SeqCst);
        if capture_in_flight != 0 || graph_in_flight != 0 {
            let _ =
                self.shared.transition(BridgeState::PrimingCheck, BridgeState::PlaybackStabilizing);
            return Ok(());
        }

        let capture_fill = self.capture_fill_observer.stable_fill_frames().map_err(|error| {
            mark_fault(&self.shared, map_sequence(error, BridgeClockDomain::WaveCapture))
        })?;
        let playback_fill = self.consumer.available_frames().map_err(|error| {
            mark_fault(&self.shared, map_sequence(error, BridgeClockDomain::WavePlayback))
        })?;
        let next = if self.shared.within_startup_guards(capture_fill, playback_fill) {
            BridgeState::Primed
        } else {
            BridgeState::PlaybackStabilizing
        };
        let _ = self.shared.transition(BridgeState::PrimingCheck, next);
        Ok(())
    }
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
        | BridgeState::PlaybackPriming
        | BridgeState::PlaybackFilterDelay
        | BridgeState::PlaybackStabilizing => Ok(state),
    }
}

fn validate_position(
    domain: BridgeClockDomain,
    expected: Option<u64>,
    actual: u64,
) -> Result<(), BridgeError> {
    if let Some(expected) = expected
        && expected != actual
    {
        return Err(BridgeError::Discontinuity { domain, expected, actual });
    }
    Ok(())
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
        RingPeekError::PeekPending => BridgeError::AttemptFaulted,
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
