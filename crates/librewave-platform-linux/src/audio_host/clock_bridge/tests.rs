#![allow(clippy::float_cmp)]

use super::*;
use crate::audio_host::{PACKED_S24_SAMPLE_BYTES, WAVE3_PCM_RATE_HZ};
use librewave_core::MixerProfile;
use librewave_engine::{RateMatchRatioBounds, RateMatcherConfig};
use std::sync::mpsc;
use std::thread;

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

const SIMULATION_DELAY_QUANTUM: usize = 32;
const SIMULATION_DELAY_CAPTURE_PERIOD: usize = 64;
const SIMULATION_DELAY_CAPTURE_BUFFER: u32 = 256;
const SIMULATION_DELAY_PLAYBACK_PERIOD: usize = 32;
const SIMULATION_DELAY_PLAYBACK_BUFFER: u32 = 128;
const SIMULATION_DELAY_FIFO_CAPACITY: usize = 4_096;
const SIMULATION_DELAY_TARGET_FILL: usize = 2_048;
const SIMULATION_DELAY_STARTUP_GUARD: usize = 512;
const SIMULATION_DELAY_MINIMUM_RATIO: f64 = 0.5;
const SIMULATION_DELAY_MAXIMUM_RATIO: f64 = 2.0;
const SIMULATION_DELAY_PROPORTIONAL_GAIN: f64 = 1.0;
const SIMULATION_DELAY_INTEGRAL_GAIN: f64 = 0.0;
const SIMULATION_DELAY_INTEGRAL_LIMIT: f64 = 10_000.0;
const SIMULATION_DELAY_SLEW_LIMIT: f64 = 1.0;

#[derive(Clone, Copy, Debug)]
struct Positions {
    capture: u64,
    graph: u64,
    playback: u64,
    graph_preflight_boundaries: usize,
    playback_preflight_boundaries: usize,
}

struct OutputStorage {
    microphone_left: [f32; SIMULATION_MAXIMUM_GRAPH_QUANTUM],
    microphone_right: [f32; SIMULATION_MAXIMUM_GRAPH_QUANTUM],
    monitor_left: [f32; SIMULATION_MAXIMUM_GRAPH_QUANTUM],
    monitor_right: [f32; SIMULATION_MAXIMUM_GRAPH_QUANTUM],
    stream_left: [f32; SIMULATION_MAXIMUM_GRAPH_QUANTUM],
    stream_right: [f32; SIMULATION_MAXIMUM_GRAPH_QUANTUM],
}

impl OutputStorage {
    fn new() -> Self {
        Self {
            microphone_left: [0.0; SIMULATION_MAXIMUM_GRAPH_QUANTUM],
            microphone_right: [0.0; SIMULATION_MAXIMUM_GRAPH_QUANTUM],
            monitor_left: [0.0; SIMULATION_MAXIMUM_GRAPH_QUANTUM],
            monitor_right: [0.0; SIMULATION_MAXIMUM_GRAPH_QUANTUM],
            stream_left: [0.0; SIMULATION_MAXIMUM_GRAPH_QUANTUM],
            stream_right: [0.0; SIMULATION_MAXIMUM_GRAPH_QUANTUM],
        }
    }

    fn buffers(&mut self, frames: usize) -> GraphOutputBuffers<'_> {
        GraphOutputBuffers {
            microphone_left: &mut self.microphone_left[..frames],
            microphone_right: &mut self.microphone_right[..frames],
            monitor_left: &mut self.monitor_left[..frames],
            monitor_right: &mut self.monitor_right[..frames],
            stream_left: &mut self.stream_left[..frames],
            stream_right: &mut self.stream_right[..frames],
        }
    }
}

#[derive(Debug)]
struct ExactWriter {
    calls: usize,
    bytes: Vec<u8>,
}

impl ExactWriter {
    fn new() -> Self {
        Self { calls: 0, bytes: vec![0; SIMULATION_PLAYBACK_PERIOD * WAVE3_PLAYBACK_CHANNELS * 3] }
    }
}

impl PlaybackPeriodWriter for ExactWriter {
    fn write_packed_s24_3le(
        &mut self,
        start_frame: u64,
        complete_frame_count: usize,
        bytes: &[u8],
    ) -> Result<PlaybackWriteProgress, PlaybackWriteError> {
        self.calls += 1;
        self.bytes.copy_from_slice(bytes);
        Ok(PlaybackWriteProgress { start_frame, frames_written: complete_frame_count })
    }
}

struct FailingWriter;

impl PlaybackPeriodWriter for FailingWriter {
    fn write_packed_s24_3le(
        &mut self,
        _start_frame: u64,
        _complete_frame_count: usize,
        _bytes: &[u8],
    ) -> Result<PlaybackWriteProgress, PlaybackWriteError> {
        Err(PlaybackWriteError::Failed)
    }
}

struct ShortWriter;

impl PlaybackPeriodWriter for ShortWriter {
    fn write_packed_s24_3le(
        &mut self,
        start_frame: u64,
        complete_frame_count: usize,
        _bytes: &[u8],
    ) -> Result<PlaybackWriteProgress, PlaybackWriteError> {
        Ok(PlaybackWriteProgress { start_frame, frames_written: complete_frame_count - 1 })
    }
}

fn simulation_bounds() -> RateMatchRatioBounds {
    RateMatchRatioBounds::try_new(
        SIMULATION_DEPENDENCY_RATIO_LIMIT,
        SIMULATION_MINIMUM_RATIO,
        SIMULATION_MAXIMUM_RATIO,
    )
    .expect("simulation ratio bounds")
}

fn simulation_direction(channels: usize, maximum_output_frames: usize) -> DirectionBridgeConfig {
    let bounds = simulation_bounds();
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

fn simulation_parts() -> ClockBridgeParts {
    let physical = Wave3PhysicalIoConfig::try_new(
        u32::try_from(SIMULATION_CAPTURE_PERIOD).expect("capture period"),
        SIMULATION_CAPTURE_BUFFER,
        u32::try_from(SIMULATION_PLAYBACK_PERIOD).expect("playback period"),
        SIMULATION_PLAYBACK_BUFFER,
    )
    .expect("simulation physical geometry");
    let config = ClockBridgeConfig::try_new(
        SIMULATION_MAXIMUM_GRAPH_QUANTUM,
        simulation_direction(WAVE3_CAPTURE_CHANNELS, SIMULATION_MAXIMUM_GRAPH_QUANTUM),
        simulation_direction(WAVE3_PLAYBACK_CHANNELS, SIMULATION_PLAYBACK_PERIOD),
    )
    .expect("simulation bridge config");
    ClockBridgeParts::new(physical, &MixerProfile::default(), config).expect("simulation bridge")
}

fn aggressive_delay_direction(channels: usize) -> DirectionBridgeConfig {
    let bounds = RateMatchRatioBounds::try_new(
        SIMULATION_DEPENDENCY_RATIO_LIMIT,
        SIMULATION_DELAY_MINIMUM_RATIO,
        SIMULATION_DELAY_MAXIMUM_RATIO,
    )
    .expect("delay-test ratio bounds");
    let matcher = RateMatcherConfig::try_new(SIMULATION_DELAY_QUANTUM, channels, bounds)
        .expect("delay-test matcher");
    let controller = RateMatchControllerConfig::try_new(
        SIMULATION_DELAY_TARGET_FILL,
        SIMULATION_DELAY_PROPORTIONAL_GAIN,
        SIMULATION_DELAY_INTEGRAL_GAIN,
        SIMULATION_DELAY_INTEGRAL_LIMIT,
        SIMULATION_DELAY_SLEW_LIMIT,
        bounds,
    )
    .expect("delay-test controller");
    DirectionBridgeConfig::try_new(
        SIMULATION_DELAY_FIFO_CAPACITY,
        SIMULATION_DELAY_STARTUP_GUARD,
        matcher,
        controller,
    )
    .expect("delay-test direction")
}

fn aggressive_delay_parts() -> ClockBridgeParts {
    let physical = Wave3PhysicalIoConfig::try_new(
        u32::try_from(SIMULATION_DELAY_CAPTURE_PERIOD).expect("delay capture period"),
        SIMULATION_DELAY_CAPTURE_BUFFER,
        u32::try_from(SIMULATION_DELAY_PLAYBACK_PERIOD).expect("delay playback period"),
        SIMULATION_DELAY_PLAYBACK_BUFFER,
    )
    .expect("delay-test physical geometry");
    let config = ClockBridgeConfig::try_new(
        SIMULATION_DELAY_QUANTUM,
        aggressive_delay_direction(WAVE3_CAPTURE_CHANNELS),
        aggressive_delay_direction(WAVE3_PLAYBACK_CHANNELS),
    )
    .expect("delay-test bridge config");
    ClockBridgeParts::new(physical, &MixerProfile::default(), config).expect("delay-test bridge")
}

fn packed_mono(frames: usize, sample: f32) -> Vec<u8> {
    let samples = vec![sample; frames];
    let mut bytes = vec![0; frames * PACKED_S24_SAMPLE_BYTES];
    encode_s24_3le(&samples, WAVE3_CAPTURE_CHANNELS, &mut bytes).expect("test packed capture");
    bytes
}

fn publish_capture(parts: &mut ClockBridgeParts, position: &mut u64, frames: usize, sample: f32) {
    let bytes = packed_mono(frames, sample);
    let report =
        parts.capture_ingress.publish(*position, frames, &bytes).expect("capture publication");
    *position = report.end_frame;
}

fn prime(parts: &mut ClockBridgeParts) -> Positions {
    parts.control.start_capture().expect("start capture priming");
    let mut positions = Positions {
        capture: 0,
        graph: 0,
        playback: 0,
        graph_preflight_boundaries: 0,
        playback_preflight_boundaries: 0,
    };
    while parts.control.state() == BridgeState::CapturePriming {
        publish_capture(parts, &mut positions.capture, SIMULATION_CAPTURE_PERIOD, 0.25);
    }
    for _ in 0..SIMULATION_FIFO_CAPACITY {
        match parts.control.state() {
            BridgeState::CaptureFilterDelay | BridgeState::PlaybackPriming => {
                publish_capture(parts, &mut positions.capture, SIMULATION_PLAYBACK_PERIOD, 0.25);
                let report = parts
                    .graph
                    .process_preflight(
                        positions.graph,
                        SIMULATION_PLAYBACK_PERIOD,
                        GraphSystemInput::unconnected(),
                    )
                    .expect("graph preflight");
                assert!(report.preflight_discarded);
                positions.graph = report.end_frame;
                positions.graph_preflight_boundaries += 1;
            }
            BridgeState::PlaybackFilterDelay | BridgeState::PlaybackStabilizing => {
                let report = parts
                    .playback
                    .process_preflight(positions.playback)
                    .expect("playback preflight");
                assert!(report.preflight_discarded);
                positions.playback = report.end_frame;
                positions.playback_preflight_boundaries += 1;
            }
            BridgeState::Primed => return positions,
            state => panic!("unexpected preflight state: {state:?}"),
        }
    }
    panic!("simulation did not reach Primed")
}

fn activate(parts: &mut ClockBridgeParts) -> Positions {
    let positions = prime(parts);
    parts.control.activate().expect("activate fake delivery");
    positions
}

#[test]
fn bridge_uses_separate_mono_capture_and_stereo_playback_matchers() {
    let parts = simulation_parts();
    assert_eq!(parts.graph.capture_matcher.config().channels(), WAVE3_CAPTURE_CHANNELS);
    assert_eq!(parts.playback.matcher.config().channels(), WAVE3_PLAYBACK_CHANNELS);
    assert_ne!(
        std::ptr::addr_of!(parts.graph.capture_controller),
        std::ptr::addr_of!(parts.playback.controller)
    );
}

#[test]
fn startup_discards_only_complete_preflight_boundaries() {
    let mut parts = simulation_parts();
    let mut positions = prime(&mut parts);
    assert_eq!(parts.control.state(), BridgeState::Primed);
    assert!(positions.graph_preflight_boundaries > 0);
    assert!(positions.playback_preflight_boundaries > 0);
    assert_eq!(parts.shared_delay(BridgeClockDomain::WaveCapture), 0);
    assert_eq!(parts.shared_delay(BridgeClockDomain::WavePlayback), 0);

    parts.control.shared.playback_in_flight.store(1, Ordering::Release);
    assert_eq!(
        parts.control.activate(),
        Err(BridgeError::ActivationNotQuiescent { capture: 0, graph: 0, playback: 1 })
    );
    assert_eq!(parts.control.state(), BridgeState::Primed);
    parts.control.shared.playback_in_flight.store(0, Ordering::Release);
    parts.control.activate().expect("active fake delivery");
    publish_capture(&mut parts, &mut positions.capture, SIMULATION_PLAYBACK_PERIOD, 0.25);
    let mut output = OutputStorage::new();
    let graph_report = parts
        .graph
        .process_active(
            positions.graph,
            SIMULATION_PLAYBACK_PERIOD,
            GraphSystemInput::unconnected(),
            output.buffers(SIMULATION_PLAYBACK_PERIOD),
        )
        .expect("active graph boundary");
    assert!(!graph_report.preflight_discarded);
    let mut writer = ExactWriter::new();
    let playback_report = parts
        .playback
        .process_active(positions.playback, &mut writer)
        .expect("active playback boundary");
    assert!(!playback_report.preflight_discarded);
    assert_eq!(writer.calls, 1);
}

#[test]
fn filter_delay_uses_exact_nominal_boundaries_without_controller_history() {
    let mut parts = aggressive_delay_parts();
    parts.control.start_capture().expect("start capture");
    let mut capture_position = 0;
    while parts.control.state() == BridgeState::CapturePriming {
        publish_capture(&mut parts, &mut capture_position, SIMULATION_DELAY_CAPTURE_PERIOD, 0.25);
    }

    let capture_delay = parts.shared_delay(BridgeClockDomain::WaveCapture);
    let capture_integral = parts.graph.capture_controller.integral();
    let capture_ratio = parts.graph.capture_controller.ratio();
    let mut graph_position = 0;
    let mut capture_boundaries = 0;
    while parts.control.state() == BridgeState::CaptureFilterDelay {
        publish_capture(&mut parts, &mut capture_position, SIMULATION_DELAY_QUANTUM, 0.25);
        let report = parts
            .graph
            .process_preflight(
                graph_position,
                SIMULATION_DELAY_QUANTUM,
                GraphSystemInput::unconnected(),
            )
            .expect("capture delay boundary");
        graph_position = report.end_frame;
        capture_boundaries += 1;
        assert_eq!(parts.graph.capture_controller.integral(), capture_integral);
        assert_eq!(parts.graph.capture_controller.ratio(), capture_ratio);
    }
    assert_eq!(capture_boundaries, capture_delay.div_ceil(SIMULATION_DELAY_QUANTUM));
    assert_eq!(parts.control.state(), BridgeState::PlaybackPriming);

    while parts.control.state() == BridgeState::PlaybackPriming {
        publish_capture(&mut parts, &mut capture_position, SIMULATION_DELAY_QUANTUM, 0.25);
        let report = parts
            .graph
            .process_preflight(
                graph_position,
                SIMULATION_DELAY_QUANTUM,
                GraphSystemInput::unconnected(),
            )
            .expect("playback priming graph boundary");
        graph_position = report.end_frame;
    }

    let playback_delay = parts.shared_delay(BridgeClockDomain::WavePlayback);
    let playback_integral = parts.playback.controller.integral();
    let playback_ratio = parts.playback.controller.ratio();
    let mut playback_position = 0;
    let mut playback_boundaries = 0;
    while parts.control.state() == BridgeState::PlaybackFilterDelay {
        let report =
            parts.playback.process_preflight(playback_position).expect("playback delay boundary");
        playback_position = report.end_frame;
        playback_boundaries += 1;
        assert_eq!(parts.playback.controller.integral(), playback_integral);
        assert_eq!(parts.playback.controller.ratio(), playback_ratio);
    }
    assert_eq!(playback_boundaries, playback_delay.div_ceil(SIMULATION_DELAY_PLAYBACK_PERIOD));
    assert_eq!(parts.control.state(), BridgeState::PlaybackStabilizing);
}

#[test]
fn primed_gate_uses_quiescent_published_cursor_sequences() {
    let mut parts = simulation_parts();
    parts.control.start_capture().expect("start capture");
    let mut positions = Positions {
        capture: 0,
        graph: 0,
        playback: 0,
        graph_preflight_boundaries: 0,
        playback_preflight_boundaries: 0,
    };
    while parts.control.state() == BridgeState::CapturePriming {
        publish_capture(&mut parts, &mut positions.capture, SIMULATION_CAPTURE_PERIOD, 0.25);
    }
    while matches!(
        parts.control.state(),
        BridgeState::CaptureFilterDelay | BridgeState::PlaybackPriming
    ) {
        publish_capture(&mut parts, &mut positions.capture, SIMULATION_PLAYBACK_PERIOD, 0.25);
        let report = parts
            .graph
            .process_preflight(
                positions.graph,
                SIMULATION_PLAYBACK_PERIOD,
                GraphSystemInput::unconnected(),
            )
            .expect("graph preflight");
        positions.graph = report.end_frame;
    }
    while parts.control.state() == BridgeState::PlaybackFilterDelay {
        let report = parts.playback.process_preflight(positions.playback).expect("playback delay");
        positions.playback = report.end_frame;
    }
    assert_eq!(parts.control.state(), BridgeState::PlaybackStabilizing);

    assert!(
        parts
            .control
            .shared
            .transition(BridgeState::PlaybackStabilizing, BridgeState::PrimingCheck)
    );
    let capture_fill =
        parts.capture_ingress.producer.fill_frames().expect("capture fill before gate");
    let playback_fill =
        parts.playback.consumer.available_frames().expect("playback fill before gate");
    assert_eq!(
        parts.capture_ingress.publish(
            positions.capture,
            SIMULATION_PLAYBACK_PERIOD,
            &packed_mono(SIMULATION_PLAYBACK_PERIOD, 0.25),
        ),
        Err(BridgeError::State { actual: BridgeState::PrimingCheck })
    );
    assert_eq!(
        parts.graph.process_preflight(
            positions.graph,
            SIMULATION_PLAYBACK_PERIOD,
            GraphSystemInput::unconnected(),
        ),
        Err(BridgeError::State { actual: BridgeState::PrimingCheck })
    );
    assert_eq!(parts.capture_ingress.producer.fill_frames(), Ok(capture_fill));
    assert_eq!(parts.playback.consumer.available_frames(), Ok(playback_fill));
    assert!(
        parts
            .control
            .shared
            .transition(BridgeState::PrimingCheck, BridgeState::PlaybackStabilizing)
    );

    parts.control.shared.graph_in_flight.store(1, Ordering::SeqCst);
    let report = parts
        .playback
        .process_preflight(positions.playback)
        .expect("stabilizing boundary with graph owner present");
    positions.playback = report.end_frame;
    assert_eq!(parts.control.state(), BridgeState::PlaybackStabilizing);
    parts.control.shared.graph_in_flight.store(0, Ordering::SeqCst);

    let capture_upper_guard = SIMULATION_TARGET_FILL + SIMULATION_STARTUP_GUARD;
    while parts.capture_ingress.producer.fill_frames().expect("capture fill") <= capture_upper_guard
    {
        publish_capture(&mut parts, &mut positions.capture, SIMULATION_CAPTURE_PERIOD, 0.25);
    }
    parts
        .playback
        .process_preflight(positions.playback)
        .expect("stabilizing boundary with high capture fill");
    assert_eq!(parts.control.state(), BridgeState::PlaybackStabilizing);
}

#[test]
fn mono_capture_is_duplicated_and_playback_order_is_deterministic() {
    fn one_run() -> (Vec<f32>, Vec<f32>, Vec<u8>) {
        let mut parts = simulation_parts();
        let mut positions = activate(&mut parts);
        publish_capture(&mut parts, &mut positions.capture, SIMULATION_PLAYBACK_PERIOD, 0.375);
        let mut output = OutputStorage::new();
        parts
            .graph
            .process_active(
                positions.graph,
                SIMULATION_PLAYBACK_PERIOD,
                GraphSystemInput::unconnected(),
                output.buffers(SIMULATION_PLAYBACK_PERIOD),
            )
            .expect("active graph");
        let mut writer = ExactWriter::new();
        parts.playback.process_active(positions.playback, &mut writer).expect("active playback");
        (
            output.microphone_left[..SIMULATION_PLAYBACK_PERIOD].to_vec(),
            output.microphone_right[..SIMULATION_PLAYBACK_PERIOD].to_vec(),
            writer.bytes,
        )
    }

    let first = one_run();
    let second = one_run();
    assert_eq!(first, second);
    assert_eq!(first.0, first.1, "mono capture must duplicate to both graph channels");
    assert!(first.0.iter().any(|sample| sample.abs() > 0.01));
    let mut playback = vec![0.0; SIMULATION_PLAYBACK_PERIOD * WAVE3_PLAYBACK_CHANNELS];
    decode_s24_3le(&first.2, WAVE3_PLAYBACK_CHANNELS, &mut playback).expect("decode playback");
    assert!(playback.chunks_exact(2).all(|frame| frame[0] == frame[1]));
}

#[test]
fn connected_system_requires_complete_finite_graph_time_buffers() {
    let mut parts = simulation_parts();
    let mut positions = activate(&mut parts);
    publish_capture(&mut parts, &mut positions.capture, SIMULATION_PLAYBACK_PERIOD, 0.25);
    let mut left = [0.0; SIMULATION_PLAYBACK_PERIOD];
    let right = [0.0; SIMULATION_PLAYBACK_PERIOD];
    left[17] = f32::NAN;
    let mut output = OutputStorage::new();
    assert!(matches!(
        parts.graph.process_active(
            positions.graph,
            SIMULATION_PLAYBACK_PERIOD,
            GraphSystemInput::connected(&left, &right),
            output.buffers(SIMULATION_PLAYBACK_PERIOD),
        ),
        Err(BridgeError::NonFinite { domain: BridgeClockDomain::PipeWireGraph, sample_index: 34 })
    ));
    assert_eq!(parts.control.state(), BridgeState::Faulted);

    let mut parts = simulation_parts();
    let mut positions = activate(&mut parts);
    publish_capture(&mut parts, &mut positions.capture, SIMULATION_PLAYBACK_PERIOD, 0.25);
    assert!(matches!(
        parts.graph.process_active(
            positions.graph,
            SIMULATION_PLAYBACK_PERIOD,
            GraphSystemInput::from_raw(true, Some(&left), None),
            output.buffers(SIMULATION_PLAYBACK_PERIOD),
        ),
        Err(BridgeError::MissingSystemBuffer)
    ));
}

#[test]
fn each_clock_domain_faults_its_own_discontinuity_and_overflow() {
    let mut parts = simulation_parts();
    parts.control.start_capture().expect("start capture");
    let one = packed_mono(1, 0.0);
    parts.capture_ingress.publish(9, 1, &one).expect("first capture position");
    assert!(matches!(
        parts.capture_ingress.publish(11, 1, &one),
        Err(BridgeError::Discontinuity {
            domain: BridgeClockDomain::WaveCapture,
            expected: 10,
            actual: 11
        })
    ));

    let mut parts = simulation_parts();
    parts.control.start_capture().expect("start capture");
    assert!(matches!(
        parts.capture_ingress.publish(u64::MAX, 1, &one),
        Err(BridgeError::PositionOverflow { domain: BridgeClockDomain::WaveCapture, .. })
    ));

    let mut parts = simulation_parts();
    let mut positions = activate(&mut parts);
    publish_capture(&mut parts, &mut positions.capture, SIMULATION_PLAYBACK_PERIOD, 0.25);
    let mut output = OutputStorage::new();
    assert!(matches!(
        parts.graph.process_active(
            positions.graph + 1,
            SIMULATION_PLAYBACK_PERIOD,
            GraphSystemInput::unconnected(),
            output.buffers(SIMULATION_PLAYBACK_PERIOD),
        ),
        Err(BridgeError::Discontinuity { domain: BridgeClockDomain::PipeWireGraph, .. })
    ));

    let mut parts = simulation_parts();
    let positions = activate(&mut parts);
    let mut writer = ExactWriter::new();
    assert!(matches!(
        parts.playback.process_active(positions.playback + 1, &mut writer),
        Err(BridgeError::Discontinuity { domain: BridgeClockDomain::WavePlayback, .. })
    ));
}

#[test]
fn capture_and_playback_shortage_and_fifo_overflow_are_terminal() {
    let mut parts = simulation_parts();
    parts.control.start_capture().expect("start capture");
    let mut capture_position = 0;
    while parts.capture_ingress.producer.free_frames().expect("capture free frames") > 0 {
        let frames = parts
            .capture_ingress
            .producer
            .free_frames()
            .expect("capture free frames")
            .min(SIMULATION_CAPTURE_PERIOD);
        publish_capture(&mut parts, &mut capture_position, frames, 0.0);
    }
    assert!(matches!(
        parts.capture_ingress.publish(capture_position, 1, &packed_mono(1, 0.0)),
        Err(BridgeError::Overflow { domain: BridgeClockDomain::WaveCapture, .. })
    ));

    let mut parts = simulation_parts();
    let mut positions = activate(&mut parts);
    let mut output = OutputStorage::new();
    let mut writer = ExactWriter::new();
    let capture_shortage = loop {
        match parts.graph.process_active(
            positions.graph,
            SIMULATION_PLAYBACK_PERIOD,
            GraphSystemInput::unconnected(),
            output.buffers(SIMULATION_PLAYBACK_PERIOD),
        ) {
            Ok(report) => {
                positions.graph = report.end_frame;
                let playback = parts
                    .playback
                    .process_active(positions.playback, &mut writer)
                    .expect("drain playback while testing capture shortage");
                positions.playback = playback.end_frame;
            }
            Err(error) => break error,
        }
    };
    assert!(matches!(
        capture_shortage,
        BridgeError::Shortage { domain: BridgeClockDomain::WaveCapture, .. }
    ));

    let mut parts = simulation_parts();
    let positions = activate(&mut parts);
    let mut playback_position = positions.playback;
    let playback_shortage = loop {
        match parts.playback.process_active(playback_position, &mut writer) {
            Ok(report) => playback_position = report.end_frame,
            Err(error) => break error,
        }
    };
    assert!(matches!(
        playback_shortage,
        BridgeError::Shortage { domain: BridgeClockDomain::WavePlayback, .. }
    ));

    let mut parts = simulation_parts();
    parts.control.start_capture().expect("start capture");
    let mut capture_position = 0;
    while parts.control.state() == BridgeState::CapturePriming {
        publish_capture(&mut parts, &mut capture_position, SIMULATION_CAPTURE_PERIOD, 0.0);
    }
    let mut graph_position = 0;
    let playback_overflow = loop {
        publish_capture(&mut parts, &mut capture_position, SIMULATION_PLAYBACK_PERIOD, 0.0);
        match parts.graph.process_preflight(
            graph_position,
            SIMULATION_PLAYBACK_PERIOD,
            GraphSystemInput::unconnected(),
        ) {
            Ok(report) => graph_position = report.end_frame,
            Err(error) => break error,
        }
    };
    assert!(matches!(
        playback_overflow,
        BridgeError::Overflow { domain: BridgeClockDomain::WavePlayback, .. }
    ));
}

#[test]
fn failed_asrc_and_writer_boundaries_do_not_commit_controllers_or_fifos() {
    let mut parts = simulation_parts();
    let mut positions = activate(&mut parts);
    publish_capture(&mut parts, &mut positions.capture, SIMULATION_PLAYBACK_PERIOD, 0.25);
    let capture_integral = parts.graph.capture_controller.integral();
    let capture_ratio = parts.graph.capture_controller.ratio();
    let capture_fill = parts.graph.capture_consumer.available_frames();
    parts.graph.capture_matcher.reset();
    let mut output = OutputStorage::new();
    assert!(matches!(
        parts.graph.process_active(
            positions.graph,
            SIMULATION_PLAYBACK_PERIOD,
            GraphSystemInput::unconnected(),
            output.buffers(SIMULATION_PLAYBACK_PERIOD),
        ),
        Err(BridgeError::Asrc {
            domain: BridgeClockDomain::WaveCapture,
            error: RateMatchError::NotWarmed
        })
    ));
    assert_eq!(parts.graph.capture_controller.integral(), capture_integral);
    assert_eq!(parts.graph.capture_controller.ratio(), capture_ratio);
    assert_eq!(parts.graph.capture_consumer.available_frames(), capture_fill);

    let mut parts = simulation_parts();
    let positions = activate(&mut parts);
    let playback_integral = parts.playback.controller.integral();
    let playback_ratio = parts.playback.controller.ratio();
    let playback_fill = parts.playback.consumer.available_frames();
    parts.playback.matcher.reset();
    assert!(matches!(
        parts.playback.process_active(positions.playback, &mut ExactWriter::new()),
        Err(BridgeError::Asrc {
            domain: BridgeClockDomain::WavePlayback,
            error: RateMatchError::NotWarmed
        })
    ));
    assert_eq!(parts.playback.controller.integral(), playback_integral);
    assert_eq!(parts.playback.controller.ratio(), playback_ratio);
    assert_eq!(parts.playback.consumer.available_frames(), playback_fill);

    let mut parts = simulation_parts();
    let positions = activate(&mut parts);
    let playback_integral = parts.playback.controller.integral();
    let playback_ratio = parts.playback.controller.ratio();
    let playback_fill = parts.playback.consumer.available_frames();
    let playback_position = parts.playback.expected_position;
    assert!(matches!(
        parts.playback.process_active(positions.playback, &mut FailingWriter),
        Err(BridgeError::Playback(PlaybackWriteError::Failed))
    ));
    assert_eq!(parts.playback.controller.integral(), playback_integral);
    assert_eq!(parts.playback.controller.ratio(), playback_ratio);
    assert_eq!(parts.playback.consumer.available_frames(), playback_fill);
    assert_eq!(parts.playback.expected_position, playback_position);

    let mut parts = simulation_parts();
    let positions = activate(&mut parts);
    assert!(matches!(
        parts.playback.process_active(positions.playback, &mut ShortWriter),
        Err(BridgeError::PlaybackProgress {
            expected_frames,
            actual_frames,
            ..
        }) if expected_frames == SIMULATION_PLAYBACK_PERIOD
            && actual_frames == SIMULATION_PLAYBACK_PERIOD - 1
    ));
    assert_eq!(parts.control.state(), BridgeState::Faulted);
}

#[test]
fn invalid_graph_quantum_is_terminal_before_processing() {
    let mut parts = simulation_parts();
    parts.control.start_capture().expect("start capture");
    let mut capture_position = 0;
    while parts.control.state() == BridgeState::CapturePriming {
        publish_capture(&mut parts, &mut capture_position, SIMULATION_CAPTURE_PERIOD, 0.0);
    }
    assert_eq!(
        parts.graph.process_preflight(0, 0, GraphSystemInput::unconnected()),
        Err(BridgeError::InvalidQuantum { actual: 0, maximum: SIMULATION_MAXIMUM_GRAPH_QUANTUM })
    );
    assert_eq!(parts.control.state(), BridgeState::Faulted);
}

#[test]
fn variable_graph_quanta_cover_every_size_from_64_through_1024() {
    let mut parts = simulation_parts();
    let mut positions = activate(&mut parts);
    let mut output = OutputStorage::new();
    let mut writer = ExactWriter::new();
    let mut playback_due = 0usize;
    for quantum in 64..=SIMULATION_MAXIMUM_GRAPH_QUANTUM {
        publish_capture(&mut parts, &mut positions.capture, quantum, 0.25);
        let report = parts
            .graph
            .process_active(
                positions.graph,
                quantum,
                GraphSystemInput::unconnected(),
                output.buffers(quantum),
            )
            .expect("variable graph quantum");
        positions.graph = report.end_frame;
        playback_due = playback_due.checked_add(quantum).expect("playback ledger");
        while playback_due >= SIMULATION_PLAYBACK_PERIOD {
            let report = parts
                .playback
                .process_active(positions.playback, &mut writer)
                .expect("variable-quantum playback");
            positions.playback = report.end_frame;
            playback_due -= SIMULATION_PLAYBACK_PERIOD;
        }
    }
    assert_eq!(parts.control.state(), BridgeState::Active);
    assert_eq!(playback_due, (64..=SIMULATION_MAXIMUM_GRAPH_QUANTUM).sum::<usize>() % 256);
}

fn run_real_asrc_drift(capture_ppm: i64, playback_ppm: i64) -> (f64, f64) {
    const SIMULATION_DRIFT_SCALE: u64 = 1_000_000;
    const SIMULATION_REAL_ASRC_BOUNDARIES: u64 = 400;
    let mut parts = simulation_parts();
    let mut positions = activate(&mut parts);
    let mut output = OutputStorage::new();
    let mut writer = ExactWriter::new();
    let mut graph_elapsed = 0_u64;
    let mut capture_generated = 0_u64;
    let mut playback_delivered = 0_u64;
    for _ in 0..SIMULATION_REAL_ASRC_BOUNDARIES {
        graph_elapsed =
            graph_elapsed.checked_add(SIMULATION_PLAYBACK_PERIOD as u64).expect("graph elapsed");
        let capture_rate = i128::from(SIMULATION_DRIFT_SCALE) + i128::from(capture_ppm);
        let capture_total = u64::try_from(
            i128::from(graph_elapsed) * capture_rate / i128::from(SIMULATION_DRIFT_SCALE),
        )
        .expect("nonnegative capture frame total");
        let capture_frames =
            usize::try_from(capture_total - capture_generated).expect("capture frame delta");
        capture_generated = capture_total;
        publish_capture(&mut parts, &mut positions.capture, capture_frames, 0.25);
        let graph_report = parts
            .graph
            .process_active(
                positions.graph,
                SIMULATION_PLAYBACK_PERIOD,
                GraphSystemInput::unconnected(),
                output.buffers(SIMULATION_PLAYBACK_PERIOD),
            )
            .expect("drift graph boundary");
        positions.graph = graph_report.end_frame;

        let playback_rate = i128::from(SIMULATION_DRIFT_SCALE) + i128::from(playback_ppm);
        let playback_due = u64::try_from(
            i128::from(graph_elapsed) * playback_rate / i128::from(SIMULATION_DRIFT_SCALE),
        )
        .expect("nonnegative playback frame total");
        while playback_delivered + SIMULATION_PLAYBACK_PERIOD as u64 <= playback_due {
            let playback_report = parts
                .playback
                .process_active(positions.playback, &mut writer)
                .expect("drift playback boundary");
            positions.playback = playback_report.end_frame;
            playback_delivered += SIMULATION_PLAYBACK_PERIOD as u64;
        }
    }
    assert_eq!(parts.control.state(), BridgeState::Active);
    (parts.graph.capture_controller.ratio(), parts.playback.controller.ratio())
}

#[test]
fn independent_positive_and_negative_drift_use_both_real_asrc_bridges() {
    const SIMULATION_POSITIVE_CAPTURE_PPM: i64 = 10_000;
    const SIMULATION_NEGATIVE_PLAYBACK_PPM: i64 = -8_000;
    const SIMULATION_NEGATIVE_CAPTURE_PPM: i64 = -9_000;
    const SIMULATION_POSITIVE_PLAYBACK_PPM: i64 = 10_000;
    let (capture_ratio, playback_ratio) =
        run_real_asrc_drift(SIMULATION_POSITIVE_CAPTURE_PPM, SIMULATION_NEGATIVE_PLAYBACK_PPM);
    assert!(capture_ratio < 1.0);
    assert!(playback_ratio < 1.0);

    let (capture_ratio, playback_ratio) =
        run_real_asrc_drift(SIMULATION_NEGATIVE_CAPTURE_PPM, SIMULATION_POSITIVE_PLAYBACK_PPM);
    assert!(capture_ratio > 1.0);
    assert!(playback_ratio > 1.0);
}

fn ledger_controller() -> RateMatchController {
    let config = RateMatchControllerConfig::try_new(
        SIMULATION_TARGET_FILL,
        SIMULATION_PROPORTIONAL_GAIN,
        SIMULATION_INTEGRAL_GAIN,
        SIMULATION_INTEGRAL_LIMIT,
        SIMULATION_SLEW_LIMIT,
        simulation_bounds(),
    )
    .expect("ledger controller");
    RateMatchController::new(config)
}

#[allow(clippy::cast_possible_truncation, clippy::cast_precision_loss, clippy::cast_sign_loss)]
fn run_checked_ledger(drift_ppm: i64) -> f64 {
    const SIMULATION_LEDGER_SCALE: u64 = 1_000_000;
    const SIMULATION_LEDGER_HOURS: u64 = 24;
    const SIMULATION_LEDGER_QUANTUM: u64 = 1_024;
    let total_frames = u64::from(WAVE3_PCM_RATE_HZ)
        .checked_mul(60 * 60 * SIMULATION_LEDGER_HOURS)
        .expect("24-hour frame ledger");
    let source_rate = i128::from(SIMULATION_LEDGER_SCALE) + i128::from(drift_ppm);
    let mut controller = ledger_controller();
    let mut elapsed = 0_u64;
    let mut source_total = 0_u64;
    let mut fill = SIMULATION_TARGET_FILL;
    let mut consumer_fraction = 0.0_f64;
    while elapsed < total_frames {
        let block = SIMULATION_LEDGER_QUANTUM.min(total_frames - elapsed);
        elapsed = elapsed.checked_add(block).expect("ledger graph advancement");
        let next_source_total = u64::try_from(
            i128::from(elapsed).checked_mul(source_rate).expect("ledger source multiplication")
                / i128::from(SIMULATION_LEDGER_SCALE),
        )
        .expect("nonnegative source ledger");
        let produced =
            next_source_total.checked_sub(source_total).expect("monotonic source ledger");
        source_total = next_source_total;
        fill = fill
            .checked_add(usize::try_from(produced).expect("produced frame count"))
            .expect("bounded fill addition");
        let step = controller.preview(fill).expect("ledger controller preview");
        let exact_consumption = block as f64 / step.relative_ratio() + consumer_fraction;
        let consumed = exact_consumption.floor() as usize;
        consumer_fraction = exact_consumption - consumed as f64;
        fill = fill.checked_sub(consumed).expect("ledger avoids shortage");
        assert!(fill <= SIMULATION_FIFO_CAPACITY, "ledger avoids overflow");
        step.commit();
    }
    assert!(fill.abs_diff(SIMULATION_TARGET_FILL) <= SIMULATION_STARTUP_GUARD);
    controller.ratio()
}

#[test]
fn checked_controller_ledgers_cover_24_simulated_hours_in_both_directions() {
    const SIMULATION_LEDGER_POSITIVE_PPM: i64 = 275;
    const SIMULATION_LEDGER_NEGATIVE_PPM: i64 = -325;
    assert!(run_checked_ledger(SIMULATION_LEDGER_POSITIVE_PPM) < 1.0);
    assert!(run_checked_ledger(SIMULATION_LEDGER_NEGATIVE_PPM) > 1.0);
}

#[test]
fn controller_arithmetic_failure_is_terminal() {
    const SIMULATION_FAULT_FIFO_CAPACITY: usize = 32_768;
    const SIMULATION_FAULT_CAPTURE_PERIOD: usize = 4_096;
    const SIMULATION_FAULT_CAPTURE_BLOCKS: usize = 8;
    let bounds = simulation_bounds();
    let capture_controller = RateMatchControllerConfig::try_new(
        SIMULATION_TARGET_FILL,
        f64::MAX,
        SIMULATION_INTEGRAL_GAIN,
        SIMULATION_INTEGRAL_LIMIT,
        SIMULATION_SLEW_LIMIT,
        bounds,
    )
    .expect("fault-injection controller");
    let capture = DirectionBridgeConfig::try_new(
        SIMULATION_FAULT_FIFO_CAPACITY,
        SIMULATION_STARTUP_GUARD,
        RateMatcherConfig::try_new(
            SIMULATION_MAXIMUM_GRAPH_QUANTUM,
            WAVE3_CAPTURE_CHANNELS,
            bounds,
        )
        .expect("capture matcher"),
        capture_controller,
    )
    .expect("capture direction");
    let config = ClockBridgeConfig::try_new(
        SIMULATION_MAXIMUM_GRAPH_QUANTUM,
        capture,
        simulation_direction(WAVE3_PLAYBACK_CHANNELS, SIMULATION_PLAYBACK_PERIOD),
    )
    .expect("fault-injection config");
    let physical = Wave3PhysicalIoConfig::try_new(
        u32::try_from(SIMULATION_FAULT_CAPTURE_PERIOD).expect("fault capture period"),
        u32::try_from(SIMULATION_FAULT_CAPTURE_PERIOD).expect("fault capture buffer"),
        u32::try_from(SIMULATION_PLAYBACK_PERIOD).expect("playback period"),
        SIMULATION_PLAYBACK_BUFFER,
    )
    .expect("fault-injection physical geometry");
    let mut parts = ClockBridgeParts::new(physical, &MixerProfile::default(), config)
        .expect("fault-injection bridge");
    parts.control.start_capture().expect("start capture");
    let mut capture_position = 0;
    for _ in 0..SIMULATION_FAULT_CAPTURE_BLOCKS {
        publish_capture(&mut parts, &mut capture_position, SIMULATION_FAULT_CAPTURE_PERIOD, 0.0);
    }
    let capture_integral = parts.graph.capture_controller.integral();
    let capture_ratio = parts.graph.capture_controller.ratio();
    let delay_report = parts
        .graph
        .process_preflight(0, 256, GraphSystemInput::unconnected())
        .expect("nominal capture delay boundary");
    assert_eq!(parts.graph.capture_controller.integral(), capture_integral);
    assert_eq!(parts.graph.capture_controller.ratio(), capture_ratio);
    assert_eq!(parts.control.state(), BridgeState::PlaybackPriming);
    assert!(matches!(
        parts.graph.process_preflight(delay_report.end_frame, 256, GraphSystemInput::unconnected()),
        Err(BridgeError::Controller { domain: BridgeClockDomain::WaveCapture, .. })
    ));
    assert_eq!(parts.control.state(), BridgeState::Faulted);
}

struct BlockingWriter {
    entered: mpsc::SyncSender<()>,
    release: mpsc::Receiver<()>,
}

impl PlaybackPeriodWriter for BlockingWriter {
    fn write_packed_s24_3le(
        &mut self,
        start_frame: u64,
        complete_frame_count: usize,
        _bytes: &[u8],
    ) -> Result<PlaybackWriteProgress, PlaybackWriteError> {
        self.entered.send(()).expect("report writer entry");
        self.release.recv().expect("release writer");
        Ok(PlaybackWriteProgress { start_frame, frames_written: complete_frame_count })
    }
}

#[test]
fn teardown_finishes_only_after_in_flight_worker_quiescence() {
    let mut parts = simulation_parts();
    let positions = activate(&mut parts);
    let (entered_tx, entered_rx) = mpsc::sync_channel(0);
    let (release_tx, release_rx) = mpsc::sync_channel(0);
    let mut playback = parts.playback;
    let worker = thread::spawn(move || {
        let mut writer = BlockingWriter { entered: entered_tx, release: release_rx };
        playback.process_active(positions.playback, &mut writer)
    });
    entered_rx.recv().expect("writer entered");
    parts.control.begin_teardown();
    assert!(matches!(
        parts.control.finish_teardown(),
        Err(BridgeError::TeardownNotQuiescent { playback: 1, .. })
    ));
    release_tx.send(()).expect("release writer");
    assert!(worker.join().expect("join worker").is_ok());
    parts.control.finish_teardown().expect("quiescent teardown");
    assert_eq!(parts.control.state(), BridgeState::Stopped);
}

impl ClockBridgeParts {
    fn shared_delay(&self, domain: BridgeClockDomain) -> usize {
        match domain {
            BridgeClockDomain::WaveCapture => {
                self.control.shared.capture_delay_remaining.load(Ordering::Acquire)
            }
            BridgeClockDomain::WavePlayback => {
                self.control.shared.playback_delay_remaining.load(Ordering::Acquire)
            }
            BridgeClockDomain::PipeWireGraph => 0,
        }
    }
}
