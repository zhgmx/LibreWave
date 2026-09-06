use librewave_core::MixerProfile;
use librewave_engine::{
    ClockAttemptEpoch, ClockDeltaBounds, ClockFramePosition, ClockObservation,
    ClockRateEstimatorConfig, MonotonicNanoseconds, RateMatchControllerConfig,
    RateMatchRatioBounds, RateMatcherConfig,
};
use librewave_platform_linux::audio_host::{
    BridgeState, CaptureClockObservation, ClockBridgeConfig, ClockBridgeParts,
    DirectionBridgeConfig, GraphClockObservation, GraphOutputBuffers, GraphSystemInput,
    PACKED_S24_SAMPLE_BYTES, PlaybackBoundaryDelivery, PlaybackClockObservation,
    PlaybackPeriodSubmitter, PlaybackSubmissionProgress, PlaybackWriteError,
    WAVE3_CAPTURE_CHANNELS, WAVE3_PCM_RATE_HZ, WAVE3_PLAYBACK_CHANNELS, Wave3PhysicalIoConfig,
};
use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;

const EPOCH_VALUE: u64 = 11;
const NANOSECONDS_PER_SECOND: u64 = 1_000_000_000;
const SIMULATION_GRAPH_QUANTUM: usize = 256;
const SIMULATION_MAXIMUM_GRAPH_QUANTUM: usize = 1_024;
const SIMULATION_CAPTURE_PERIOD: usize = 1_024;
const SIMULATION_CAPTURE_BUFFER: u32 = 8_192;
const SIMULATION_PLAYBACK_PERIOD: usize = 256;
const SIMULATION_PLAYBACK_BUFFER: u32 = 2_048;
const SIMULATION_FIFO_CAPACITY: usize = 16_384;
const SIMULATION_TARGET_FILL: usize = 8_192;
const SIMULATION_STARTUP_GUARD: usize = 4_096;

struct CountingAllocator;

thread_local! {
    static COUNT_OPERATIONS: Cell<bool> = const { Cell::new(false) };
    static OPERATION_COUNT: Cell<usize> = const { Cell::new(0) };
}

#[global_allocator]
static ALLOCATOR: CountingAllocator = CountingAllocator;

unsafe impl GlobalAlloc for CountingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        record_operation();
        // SAFETY: the valid layout is forwarded unchanged.
        unsafe { System.alloc(layout) }
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        record_operation();
        // SAFETY: the valid layout is forwarded unchanged.
        unsafe { System.alloc_zeroed(layout) }
    }

    unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
        record_operation();
        // SAFETY: the pointer and layout came from this system allocator.
        unsafe { System.dealloc(pointer, layout) };
    }

    unsafe fn realloc(&self, pointer: *mut u8, layout: Layout, size: usize) -> *mut u8 {
        record_operation();
        // SAFETY: the allocator receives its original pointer and layout.
        unsafe { System.realloc(pointer, layout, size) }
    }
}

fn record_operation() {
    if COUNT_OPERATIONS.try_with(Cell::get).unwrap_or(false) {
        let _ = OPERATION_COUNT.try_with(|count| count.set(count.get() + 1));
    }
}

fn count_operations<T>(operation: impl FnOnce() -> T) -> (T, usize) {
    OPERATION_COUNT.with(|count| count.set(0));
    COUNT_OPERATIONS.with(|enabled| enabled.set(true));
    let result = operation();
    COUNT_OPERATIONS.with(|enabled| enabled.set(false));
    let count = OPERATION_COUNT.with(Cell::get);
    (result, count)
}

fn epoch() -> ClockAttemptEpoch {
    ClockAttemptEpoch::try_new(EPOCH_VALUE).expect("test epoch")
}

fn observation(frames: u64, nanoseconds: u64) -> ClockObservation {
    ClockObservation::new(
        epoch(),
        ClockFramePosition::new(frames),
        MonotonicNanoseconds::new(nanoseconds),
    )
}

fn frame_time(frames: usize) -> u64 {
    u64::try_from(
        u128::try_from(frames).expect("frames") * u128::from(NANOSECONDS_PER_SECOND)
            / u128::from(WAVE3_PCM_RATE_HZ),
    )
    .expect("frame time")
}

fn direction(channels: usize, maximum_output_frames: usize) -> DirectionBridgeConfig {
    let bounds = RateMatchRatioBounds::try_new(2.0, 0.98, 1.02).expect("simulation bounds");
    let matcher = RateMatcherConfig::try_new(maximum_output_frames, channels, bounds)
        .expect("simulation matcher");
    let controller = RateMatchControllerConfig::try_new(
        SIMULATION_TARGET_FILL,
        0.002,
        0.000_002,
        10_000.0,
        0.000_05,
        bounds,
    )
    .expect("simulation controller");
    let delta = ClockDeltaBounds::try_new(1, 1_000_000_000, 1, 60 * NANOSECONDS_PER_SECOND)
        .expect("clock delta policy");
    let estimator = ClockRateEstimatorConfig::new(delta, delta, 2 * NANOSECONDS_PER_SECOND, bounds);
    DirectionBridgeConfig::try_new(
        SIMULATION_FIFO_CAPACITY,
        SIMULATION_STARTUP_GUARD,
        matcher,
        controller,
        estimator,
    )
    .expect("simulation direction")
}

fn bridge() -> ClockBridgeParts {
    let physical = Wave3PhysicalIoConfig::try_new(
        u32::try_from(SIMULATION_CAPTURE_PERIOD).expect("capture period"),
        SIMULATION_CAPTURE_BUFFER,
        u32::try_from(SIMULATION_PLAYBACK_PERIOD).expect("playback period"),
        SIMULATION_PLAYBACK_BUFFER,
    )
    .expect("simulation physical geometry");
    let config = ClockBridgeConfig::try_new(
        epoch(),
        SIMULATION_MAXIMUM_GRAPH_QUANTUM,
        direction(WAVE3_CAPTURE_CHANNELS, SIMULATION_MAXIMUM_GRAPH_QUANTUM),
        direction(WAVE3_PLAYBACK_CHANNELS, SIMULATION_PLAYBACK_PERIOD),
    )
    .expect("simulation bridge config");
    ClockBridgeParts::new(physical, &MixerProfile::default(), config).expect("simulation bridge")
}

struct ExactSubmitter {
    bytes: [u8; SIMULATION_PLAYBACK_PERIOD * WAVE3_PLAYBACK_CHANNELS * PACKED_S24_SAMPLE_BYTES],
}

impl PlaybackPeriodSubmitter for ExactSubmitter {
    fn submit_packed_s24_3le(
        &mut self,
        complete_frame_count: usize,
        bytes: &[u8],
    ) -> Result<PlaybackSubmissionProgress, PlaybackWriteError> {
        self.bytes.copy_from_slice(bytes);
        Ok(PlaybackSubmissionProgress { frames_submitted: complete_frame_count })
    }
}

#[test]
#[allow(clippy::too_many_lines)]
fn exact_callback_reachable_sequence_does_not_allocate_or_deallocate() {
    let mut parts = bridge();
    parts.control.start_capture().expect("start capture");
    let capture_period =
        [0_u8; SIMULATION_CAPTURE_PERIOD * WAVE3_CAPTURE_CHANNELS * PACKED_S24_SAMPLE_BYTES];
    let capture_quantum =
        [0_u8; SIMULATION_GRAPH_QUANTUM * WAVE3_CAPTURE_CHANNELS * PACKED_S24_SAMPLE_BYTES];
    let mut capture_hardware = 0_u64;
    let mut capture_time = 0_u64;
    let mut graph_position = 0_u64;
    let mut graph_time = 0_u64;
    let mut playback_application = 0_u64;
    let mut playback_hardware = 0_u64;
    let mut playback_time = 0_u64;
    let mut playback_started = false;
    let mut output = (
        [0.0; SIMULATION_GRAPH_QUANTUM],
        [0.0; SIMULATION_GRAPH_QUANTUM],
        [0.0; SIMULATION_GRAPH_QUANTUM],
        [0.0; SIMULATION_GRAPH_QUANTUM],
        [0.0; SIMULATION_GRAPH_QUANTUM],
        [0.0; SIMULATION_GRAPH_QUANTUM],
    );
    let mut submitter = ExactSubmitter {
        bytes: [0; SIMULATION_PLAYBACK_PERIOD * WAVE3_PLAYBACK_CHANNELS * PACKED_S24_SAMPLE_BYTES],
    };

    for _ in 0..SIMULATION_FIFO_CAPACITY {
        match parts.control.state() {
            BridgeState::CapturePriming => {
                capture_hardware += SIMULATION_CAPTURE_PERIOD as u64;
                capture_time += frame_time(SIMULATION_CAPTURE_PERIOD);
                parts
                    .capture_ingress
                    .publish(
                        CaptureClockObservation::new(observation(capture_hardware, capture_time)),
                        SIMULATION_CAPTURE_PERIOD,
                        &capture_period,
                    )
                    .expect("capture priming");
            }
            BridgeState::CaptureFilterDelay
            | BridgeState::CaptureEstimatorPriming
            | BridgeState::PlaybackPriming
            | BridgeState::PlaybackFilterDelay
            | BridgeState::PlaybackEstimatorPriming
            | BridgeState::PlaybackStabilizing => {
                capture_hardware += SIMULATION_GRAPH_QUANTUM as u64;
                capture_time += frame_time(SIMULATION_GRAPH_QUANTUM);
                parts
                    .capture_ingress
                    .publish(
                        CaptureClockObservation::new(observation(capture_hardware, capture_time)),
                        SIMULATION_GRAPH_QUANTUM,
                        &capture_quantum,
                    )
                    .expect("capture preflight");
                parts
                    .graph
                    .process(
                        GraphClockObservation::try_new(
                            observation(graph_position, graph_time),
                            SIMULATION_GRAPH_QUANTUM,
                        )
                        .expect("graph observation"),
                        GraphSystemInput::unconnected(),
                        GraphOutputBuffers {
                            microphone_left: &mut output.0,
                            microphone_right: &mut output.1,
                            monitor_left: &mut output.2,
                            monitor_right: &mut output.3,
                            stream_left: &mut output.4,
                            stream_right: &mut output.5,
                        },
                    )
                    .expect("graph preflight");
                graph_position += SIMULATION_GRAPH_QUANTUM as u64;
                graph_time += frame_time(SIMULATION_GRAPH_QUANTUM);
                if matches!(
                    parts.control.state(),
                    BridgeState::PlaybackFilterDelay
                        | BridgeState::PlaybackEstimatorPriming
                        | BridgeState::PlaybackStabilizing
                ) {
                    if !playback_started {
                        playback_time = graph_time;
                        playback_started = true;
                    }
                    let report = parts
                        .playback
                        .process(
                            PlaybackClockObservation::new(
                                observation(playback_hardware, playback_time),
                                ClockFramePosition::new(playback_application),
                            ),
                            &mut submitter,
                        )
                        .expect("playback preflight");
                    playback_time += frame_time(SIMULATION_PLAYBACK_PERIOD);
                    playback_application = report.application_end_frame;
                    if report.delivery != PlaybackBoundaryDelivery::DiscardedFilterDelay {
                        playback_hardware = playback_application;
                    }
                }
            }
            BridgeState::Primed => break,
            state => panic!("unexpected state: {state:?}"),
        }
    }
    assert_eq!(parts.control.state(), BridgeState::Primed);
    parts.control.activate().expect("active bridge");

    capture_hardware += SIMULATION_GRAPH_QUANTUM as u64;
    capture_time += frame_time(SIMULATION_GRAPH_QUANTUM);
    let capture_observation =
        CaptureClockObservation::new(observation(capture_hardware, capture_time));
    let graph_observation = GraphClockObservation::try_new(
        observation(graph_position, graph_time),
        SIMULATION_GRAPH_QUANTUM,
    )
    .expect("graph observation");
    let playback_observation = PlaybackClockObservation::new(
        observation(playback_hardware, playback_time),
        ClockFramePosition::new(playback_application),
    );
    let (result, operations) = count_operations(|| {
        parts.capture_ingress.publish(
            capture_observation,
            SIMULATION_GRAPH_QUANTUM,
            &capture_quantum,
        )?;
        parts.graph.process(
            graph_observation,
            GraphSystemInput::unconnected(),
            GraphOutputBuffers {
                microphone_left: &mut output.0,
                microphone_right: &mut output.1,
                monitor_left: &mut output.2,
                monitor_right: &mut output.3,
                stream_left: &mut output.4,
                stream_right: &mut output.5,
            },
        )?;
        parts.playback.process(playback_observation, &mut submitter)?;
        Ok::<(), librewave_platform_linux::audio_host::BridgeError>(())
    });
    assert_eq!(result, Ok(()));
    assert_eq!(operations, 0);
}
