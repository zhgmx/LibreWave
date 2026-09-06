use librewave_core::MixerProfile;
use librewave_engine::{RateMatchControllerConfig, RateMatchRatioBounds, RateMatcherConfig};
use librewave_platform_linux::audio_host::{
    BridgeState, ClockBridgeConfig, ClockBridgeParts, DirectionBridgeConfig, GraphOutputBuffers,
    GraphSystemInput, PACKED_S24_SAMPLE_BYTES, PlaybackPeriodWriter, PlaybackWriteError,
    PlaybackWriteProgress, WAVE3_CAPTURE_CHANNELS, WAVE3_PLAYBACK_CHANNELS, Wave3PhysicalIoConfig,
};
use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;

const SIMULATION_GRAPH_QUANTUM: usize = 256;
const SIMULATION_MAXIMUM_GRAPH_QUANTUM: usize = 1_024;
const SIMULATION_CAPTURE_PERIOD: usize = 1_024;
const SIMULATION_CAPTURE_BUFFER: u32 = 8_192;
const SIMULATION_PLAYBACK_PERIOD: usize = 256;
const SIMULATION_PLAYBACK_BUFFER: u32 = 2_048;
const SIMULATION_FIFO_CAPACITY: usize = 16_384;
const SIMULATION_TARGET_FILL: usize = 8_192;
const SIMULATION_STARTUP_GUARD: usize = 4_096;
const SIMULATION_DEPENDENCY_RATIO_LIMIT: f64 = 2.0;
const SIMULATION_MINIMUM_RATIO: f64 = 0.98;
const SIMULATION_MAXIMUM_RATIO: f64 = 1.02;
const SIMULATION_PROPORTIONAL_GAIN: f64 = 0.002;
const SIMULATION_INTEGRAL_GAIN: f64 = 0.000_002;
const SIMULATION_INTEGRAL_LIMIT: f64 = 10_000.0;
const SIMULATION_SLEW_LIMIT: f64 = 0.000_05;

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

fn direction(channels: usize, maximum_output_frames: usize) -> DirectionBridgeConfig {
    let bounds = RateMatchRatioBounds::try_new(
        SIMULATION_DEPENDENCY_RATIO_LIMIT,
        SIMULATION_MINIMUM_RATIO,
        SIMULATION_MAXIMUM_RATIO,
    )
    .expect("simulation bounds");
    let matcher = RateMatcherConfig::try_new(maximum_output_frames, channels, bounds)
        .expect("simulation matcher");
    let controller = RateMatchControllerConfig::try_new(
        SIMULATION_TARGET_FILL,
        SIMULATION_PROPORTIONAL_GAIN,
        SIMULATION_INTEGRAL_GAIN,
        SIMULATION_INTEGRAL_LIMIT,
        SIMULATION_SLEW_LIMIT,
        bounds,
    )
    .expect("simulation controller");
    DirectionBridgeConfig::try_new(
        SIMULATION_FIFO_CAPACITY,
        SIMULATION_STARTUP_GUARD,
        matcher,
        controller,
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
        SIMULATION_MAXIMUM_GRAPH_QUANTUM,
        direction(WAVE3_CAPTURE_CHANNELS, SIMULATION_MAXIMUM_GRAPH_QUANTUM),
        direction(WAVE3_PLAYBACK_CHANNELS, SIMULATION_PLAYBACK_PERIOD),
    )
    .expect("simulation bridge config");
    ClockBridgeParts::new(physical, &MixerProfile::default(), config).expect("simulation bridge")
}

struct ExactWriter {
    bytes: [u8; SIMULATION_PLAYBACK_PERIOD * WAVE3_PLAYBACK_CHANNELS * PACKED_S24_SAMPLE_BYTES],
}

impl PlaybackPeriodWriter for ExactWriter {
    fn write_packed_s24_3le(
        &mut self,
        start_frame: u64,
        complete_frame_count: usize,
        bytes: &[u8],
    ) -> Result<PlaybackWriteProgress, PlaybackWriteError> {
        self.bytes.copy_from_slice(bytes);
        Ok(PlaybackWriteProgress { start_frame, frames_written: complete_frame_count })
    }
}

#[test]
fn active_capture_graph_and_playback_boundaries_do_not_allocate_or_deallocate() {
    let mut parts = bridge();
    parts.control.start_capture().expect("start capture");
    let capture_period =
        [0_u8; SIMULATION_CAPTURE_PERIOD * WAVE3_CAPTURE_CHANNELS * PACKED_S24_SAMPLE_BYTES];
    let capture_quantum =
        [0_u8; SIMULATION_GRAPH_QUANTUM * WAVE3_CAPTURE_CHANNELS * PACKED_S24_SAMPLE_BYTES];
    let mut capture_position = 0_u64;
    let mut graph_position = 0_u64;
    let mut playback_position = 0_u64;
    while parts.control.state() == BridgeState::CapturePriming {
        let report = parts
            .capture_ingress
            .publish(capture_position, SIMULATION_CAPTURE_PERIOD, &capture_period)
            .expect("capture priming");
        capture_position = report.end_frame;
    }
    loop {
        match parts.control.state() {
            BridgeState::CaptureFilterDelay | BridgeState::PlaybackPriming => {
                let report = parts
                    .capture_ingress
                    .publish(capture_position, SIMULATION_GRAPH_QUANTUM, &capture_quantum)
                    .expect("capture preflight");
                capture_position = report.end_frame;
                let report = parts
                    .graph
                    .process_preflight(
                        graph_position,
                        SIMULATION_GRAPH_QUANTUM,
                        GraphSystemInput::unconnected(),
                    )
                    .expect("graph preflight");
                graph_position = report.end_frame;
            }
            BridgeState::PlaybackFilterDelay | BridgeState::PlaybackStabilizing => {
                let report = parts
                    .playback
                    .process_preflight(playback_position)
                    .expect("playback preflight");
                playback_position = report.end_frame;
            }
            BridgeState::Primed => break,
            state => panic!("unexpected preflight state: {state:?}"),
        }
    }
    parts.control.activate().expect("active fake bridge");

    let mut microphone_left = [0.0; SIMULATION_GRAPH_QUANTUM];
    let mut microphone_right = [0.0; SIMULATION_GRAPH_QUANTUM];
    let mut monitor_left = [0.0; SIMULATION_GRAPH_QUANTUM];
    let mut monitor_right = [0.0; SIMULATION_GRAPH_QUANTUM];
    let mut stream_left = [0.0; SIMULATION_GRAPH_QUANTUM];
    let mut stream_right = [0.0; SIMULATION_GRAPH_QUANTUM];
    let mut writer = ExactWriter {
        bytes: [0; SIMULATION_PLAYBACK_PERIOD * WAVE3_PLAYBACK_CHANNELS * PACKED_S24_SAMPLE_BYTES],
    };
    let (result, operations) = count_operations(|| {
        parts.capture_ingress.publish(
            capture_position,
            SIMULATION_GRAPH_QUANTUM,
            &capture_quantum,
        )?;
        parts.graph.process_active(
            graph_position,
            SIMULATION_GRAPH_QUANTUM,
            GraphSystemInput::unconnected(),
            GraphOutputBuffers {
                microphone_left: &mut microphone_left,
                microphone_right: &mut microphone_right,
                monitor_left: &mut monitor_left,
                monitor_right: &mut monitor_right,
                stream_left: &mut stream_left,
                stream_right: &mut stream_right,
            },
        )?;
        parts.playback.process_active(playback_position, &mut writer)?;
        Ok::<(), librewave_platform_linux::audio_host::BridgeError>(())
    });
    assert_eq!(result, Ok(()));
    assert_eq!(operations, 0);
}
