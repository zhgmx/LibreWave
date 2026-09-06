#![allow(clippy::float_cmp)]

use super::*;
use crate::audio_host::worker::{
    ParkedPlaybackWorker, PcmIoError, PcmStatusSnapshot, PcmWait, PlaybackPcm, PlaybackWorker,
    PlaybackWorkerBoundary, PlaybackWorkerTerminal, WorkerIoPolicy,
};
use crate::audio_host::{PACKED_S24_SAMPLE_BYTES, PcmDirection, WAVE3_PCM_RATE_HZ};
use librewave_core::MixerProfile;
use librewave_engine::{
    ClockDeltaBounds, ClockFramePosition, MonotonicNanoseconds, RateMatchRatioBounds,
    RateMatcherConfig,
};
use std::collections::VecDeque;
use std::sync::{Arc, Condvar, Mutex, mpsc};
use std::thread;

const EPOCH_VALUE: u64 = 7;
const NANOSECONDS_PER_SECOND: u64 = 1_000_000_000;
const PPM_SCALE: i128 = 1_000_000;
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
const SIMULATION_MAXIMUM_OBSERVATION_DELTA: u64 = 1_000_000_000;
const SIMULATION_MAXIMUM_TIME_DELTA: u64 = 60 * NANOSECONDS_PER_SECOND;
const SIMULATION_MAXIMUM_SKEW: u64 = 2 * NANOSECONDS_PER_SECOND;

const SIMULATION_DELAY_QUANTUM: usize = 32;
const SIMULATION_DELAY_CAPTURE_PERIOD: usize = 64;
const SIMULATION_DELAY_CAPTURE_BUFFER: u32 = 256;
const SIMULATION_DELAY_PLAYBACK_PERIOD: usize = 32;
const SIMULATION_DELAY_PLAYBACK_BUFFER: u32 = 128;
const SIMULATION_DELAY_FIFO_CAPACITY: usize = 4_096;
const SIMULATION_DELAY_TARGET_FILL: usize = 2_048;
const SIMULATION_DELAY_STARTUP_GUARD: usize = 512;
const WORKER_POLL_LIMIT: usize = 1_000_000;

#[derive(Clone, Copy, Debug)]
struct Positions {
    capture_hardware: u64,
    capture_time: u64,
    graph: u64,
    graph_time: u64,
    playback_application: u64,
    playback_hardware: u64,
    playback_time: u64,
    playback_clock_started: bool,
    capture_ppm: i64,
    playback_ppm: i64,
    graph_boundaries: usize,
    playback_boundaries: usize,
}

impl Positions {
    fn new() -> Self {
        Self {
            capture_hardware: 0,
            capture_time: 0,
            graph: 0,
            graph_time: 0,
            playback_application: 0,
            playback_hardware: 0,
            playback_time: 0,
            playback_clock_started: false,
            capture_ppm: 0,
            playback_ppm: 0,
            graph_boundaries: 0,
            playback_boundaries: 0,
        }
    }

    fn playback_observation(&self) -> PlaybackClockObservation {
        PlaybackClockObservation::new(
            clock(self.playback_hardware, self.playback_time),
            ClockFramePosition::new(self.playback_application),
        )
    }
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
struct ExactSubmitter {
    calls: usize,
    bytes: Vec<u8>,
}

impl ExactSubmitter {
    fn new(period_frames: usize) -> Self {
        Self {
            calls: 0,
            bytes: vec![0; period_frames * WAVE3_PLAYBACK_CHANNELS * PACKED_S24_SAMPLE_BYTES],
        }
    }
}

impl PlaybackPeriodSubmitter for ExactSubmitter {
    fn submit_packed_s24_3le(
        &mut self,
        complete_frame_count: usize,
        bytes: &[u8],
    ) -> Result<PlaybackSubmissionProgress, PlaybackWriteError> {
        self.calls += 1;
        self.bytes.copy_from_slice(bytes);
        Ok(PlaybackSubmissionProgress { frames_submitted: complete_frame_count })
    }
}

struct FailingSubmitter;

impl PlaybackPeriodSubmitter for FailingSubmitter {
    fn submit_packed_s24_3le(
        &mut self,
        _complete_frame_count: usize,
        _bytes: &[u8],
    ) -> Result<PlaybackSubmissionProgress, PlaybackWriteError> {
        Err(PlaybackWriteError::Failed { accepted_frames: 0, errno: libc::EIO })
    }
}

struct ShortSubmitter;

impl PlaybackPeriodSubmitter for ShortSubmitter {
    fn submit_packed_s24_3le(
        &mut self,
        complete_frame_count: usize,
        _bytes: &[u8],
    ) -> Result<PlaybackSubmissionProgress, PlaybackWriteError> {
        Ok(PlaybackSubmissionProgress { frames_submitted: complete_frame_count - 1 })
    }
}

struct PartialFailingSubmitter;

impl PlaybackPeriodSubmitter for PartialFailingSubmitter {
    fn submit_packed_s24_3le(
        &mut self,
        _complete_frame_count: usize,
        _bytes: &[u8],
    ) -> Result<PlaybackSubmissionProgress, PlaybackWriteError> {
        Err(PlaybackWriteError::Xrun { accepted_frames: 17 })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PlaybackWorkerCall {
    Status,
    Wait,
    Write { bytes: usize },
    StartPcm,
    DropStream,
}

struct ScriptedPlaybackPcm {
    waits: VecDeque<Result<PcmWait, PcmIoError>>,
    writes: VecDeque<Result<usize, PcmIoError>>,
    statuses: VecDeque<Result<PcmStatusSnapshot, PcmIoError>>,
    calls: Arc<Mutex<Vec<PlaybackWorkerCall>>>,
}

struct WaitGate {
    released: Mutex<bool>,
    condition: Condvar,
}

struct CancellablePlaybackPcm {
    first_status: Option<PcmStatusSnapshot>,
    gate: Arc<WaitGate>,
    calls: Arc<Mutex<Vec<PlaybackWorkerCall>>>,
}

impl PlaybackPcm for CancellablePlaybackPcm {
    fn wait(&mut self, _timeout_millis: u32) -> Result<PcmWait, PcmIoError> {
        self.calls.lock().expect("playback calls").push(PlaybackWorkerCall::Wait);
        let mut released = self.gate.released.lock().expect("wait gate");
        while !*released {
            released = self.gate.condition.wait(released).expect("wait gate notification");
        }
        Ok(PcmWait::TimedOut)
    }

    fn write_frames(&mut self, bytes: &[u8]) -> Result<usize, PcmIoError> {
        self.calls
            .lock()
            .expect("playback calls")
            .push(PlaybackWorkerCall::Write { bytes: bytes.len() });
        Err(PcmIoError::Failed { errno: libc::EIO })
    }

    fn status(&mut self) -> Result<PcmStatusSnapshot, PcmIoError> {
        self.calls.lock().expect("playback calls").push(PlaybackWorkerCall::Status);
        self.first_status.take().ok_or(PcmIoError::Again)
    }

    fn start_pcm(&mut self) -> Result<(), PcmIoError> {
        self.calls.lock().expect("playback calls").push(PlaybackWorkerCall::StartPcm);
        Ok(())
    }

    fn drop_stream(&mut self) -> Result<(), PcmIoError> {
        self.calls.lock().expect("playback calls").push(PlaybackWorkerCall::DropStream);
        Ok(())
    }
}

impl PlaybackPcm for ScriptedPlaybackPcm {
    fn wait(&mut self, _timeout_millis: u32) -> Result<PcmWait, PcmIoError> {
        self.calls.lock().expect("playback calls").push(PlaybackWorkerCall::Wait);
        self.waits.pop_front().unwrap_or(Ok(PcmWait::TimedOut))
    }

    fn write_frames(&mut self, bytes: &[u8]) -> Result<usize, PcmIoError> {
        self.calls
            .lock()
            .expect("playback calls")
            .push(PlaybackWorkerCall::Write { bytes: bytes.len() });
        self.writes.pop_front().unwrap_or(Err(PcmIoError::Failed { errno: libc::EIO }))
    }

    fn status(&mut self) -> Result<PcmStatusSnapshot, PcmIoError> {
        self.calls.lock().expect("playback calls").push(PlaybackWorkerCall::Status);
        self.statuses.pop_front().unwrap_or(Err(PcmIoError::Disconnected))
    }

    fn start_pcm(&mut self) -> Result<(), PcmIoError> {
        self.calls.lock().expect("playback calls").push(PlaybackWorkerCall::StartPcm);
        Ok(())
    }

    fn drop_stream(&mut self) -> Result<(), PcmIoError> {
        self.calls.lock().expect("playback calls").push(PlaybackWorkerCall::DropStream);
        Ok(())
    }
}

fn epoch() -> ClockAttemptEpoch {
    ClockAttemptEpoch::try_new(EPOCH_VALUE).expect("test epoch")
}

fn clock(frames: u64, nanoseconds: u64) -> ClockObservation {
    ClockObservation::new(
        epoch(),
        ClockFramePosition::new(frames),
        MonotonicNanoseconds::new(nanoseconds),
    )
}

fn graph_observation(frames: u64, nanoseconds: u64, quantum: usize) -> GraphClockObservation {
    GraphClockObservation::try_new(clock(frames, nanoseconds), quantum).expect("positive quantum")
}

fn next_playback_boundary(worker: &mut PlaybackWorker) -> PlaybackWorkerBoundary {
    for _ in 0..WORKER_POLL_LIMIT {
        if let Some(boundary) = worker.try_boundary() {
            return boundary;
        }
        assert!(!worker.is_finished(), "playback worker ended before reporting a boundary");
        thread::yield_now();
    }
    panic!("playback worker did not report a boundary")
}

fn wait_for_playback_finish(worker: &PlaybackWorker) {
    for _ in 0..WORKER_POLL_LIMIT {
        if worker.is_finished() {
            return;
        }
        thread::yield_now();
    }
    panic!("playback worker did not finish")
}

fn scaled_frame_time(frames: usize, ppm: i64) -> u64 {
    let rate = PPM_SCALE + i128::from(ppm);
    assert!(rate > 0);
    let numerator = i128::try_from(frames).expect("frame count")
        * i128::from(NANOSECONDS_PER_SECOND)
        * PPM_SCALE;
    u64::try_from(numerator / (i128::from(WAVE3_PCM_RATE_HZ) * rate)).expect("positive scaled time")
}

fn simulation_bounds() -> RateMatchRatioBounds {
    RateMatchRatioBounds::try_new(
        SIMULATION_DEPENDENCY_RATIO_LIMIT,
        SIMULATION_MINIMUM_RATIO,
        SIMULATION_MAXIMUM_RATIO,
    )
    .expect("simulation ratio bounds")
}

fn estimator_config(bounds: RateMatchRatioBounds) -> ClockRateEstimatorConfig {
    let delta = ClockDeltaBounds::try_new(
        1,
        SIMULATION_MAXIMUM_OBSERVATION_DELTA,
        1,
        SIMULATION_MAXIMUM_TIME_DELTA,
    )
    .expect("simulation observation bounds");
    ClockRateEstimatorConfig::new(delta, delta, SIMULATION_MAXIMUM_SKEW, bounds)
}

#[allow(clippy::too_many_arguments)]
fn direction(
    channels: usize,
    maximum_output_frames: usize,
    fifo_capacity: usize,
    target: usize,
    guard: usize,
    minimum_ratio: f64,
    maximum_ratio: f64,
    proportional_gain: f64,
    integral_gain: f64,
    slew_limit: f64,
) -> DirectionBridgeConfig {
    let bounds = RateMatchRatioBounds::try_new(
        SIMULATION_DEPENDENCY_RATIO_LIMIT,
        minimum_ratio,
        maximum_ratio,
    )
    .expect("ratio bounds");
    let matcher = RateMatcherConfig::try_new(maximum_output_frames, channels, bounds)
        .expect("simulation matcher");
    let controller = RateMatchControllerConfig::try_new(
        target,
        proportional_gain,
        integral_gain,
        SIMULATION_INTEGRAL_LIMIT,
        slew_limit,
        bounds,
    )
    .expect("simulation controller");
    DirectionBridgeConfig::try_new(
        fifo_capacity,
        guard,
        matcher,
        controller,
        estimator_config(bounds),
    )
    .expect("simulation direction")
}

fn simulation_direction(channels: usize, maximum_output_frames: usize) -> DirectionBridgeConfig {
    direction(
        channels,
        maximum_output_frames,
        SIMULATION_FIFO_CAPACITY,
        SIMULATION_TARGET_FILL,
        SIMULATION_STARTUP_GUARD,
        SIMULATION_MINIMUM_RATIO,
        SIMULATION_MAXIMUM_RATIO,
        SIMULATION_PROPORTIONAL_GAIN,
        SIMULATION_INTEGRAL_GAIN,
        SIMULATION_SLEW_LIMIT,
    )
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
        epoch(),
        SIMULATION_MAXIMUM_GRAPH_QUANTUM,
        simulation_direction(WAVE3_CAPTURE_CHANNELS, SIMULATION_MAXIMUM_GRAPH_QUANTUM),
        simulation_direction(WAVE3_PLAYBACK_CHANNELS, SIMULATION_PLAYBACK_PERIOD),
    )
    .expect("simulation bridge config");
    ClockBridgeParts::new(physical, &MixerProfile::default(), config).expect("simulation bridge")
}

fn aggressive_delay_parts() -> ClockBridgeParts {
    let physical = Wave3PhysicalIoConfig::try_new(
        u32::try_from(SIMULATION_DELAY_CAPTURE_PERIOD).expect("capture period"),
        SIMULATION_DELAY_CAPTURE_BUFFER,
        u32::try_from(SIMULATION_DELAY_PLAYBACK_PERIOD).expect("playback period"),
        SIMULATION_DELAY_PLAYBACK_BUFFER,
    )
    .expect("delay physical geometry");
    let capture = direction(
        WAVE3_CAPTURE_CHANNELS,
        SIMULATION_DELAY_QUANTUM,
        SIMULATION_DELAY_FIFO_CAPACITY,
        SIMULATION_DELAY_TARGET_FILL,
        SIMULATION_DELAY_STARTUP_GUARD,
        0.5,
        2.0,
        1.0,
        0.0,
        1.0,
    );
    let playback = direction(
        WAVE3_PLAYBACK_CHANNELS,
        SIMULATION_DELAY_QUANTUM,
        SIMULATION_DELAY_FIFO_CAPACITY,
        SIMULATION_DELAY_TARGET_FILL,
        SIMULATION_DELAY_STARTUP_GUARD,
        0.5,
        2.0,
        1.0,
        0.0,
        1.0,
    );
    let config = ClockBridgeConfig::try_new(epoch(), SIMULATION_DELAY_QUANTUM, capture, playback)
        .expect("delay bridge config");
    ClockBridgeParts::new(physical, &MixerProfile::default(), config).expect("delay bridge")
}

fn packed_mono(frames: usize, sample: f32) -> Vec<u8> {
    let samples = vec![sample; frames];
    let mut bytes = vec![0; frames * PACKED_S24_SAMPLE_BYTES];
    encode_s24_3le(&samples, WAVE3_CAPTURE_CHANNELS, &mut bytes).expect("packed capture");
    bytes
}

fn publish_capture(
    parts: &mut ClockBridgeParts,
    positions: &mut Positions,
    frames: usize,
    sample: f32,
) -> CapturePublishReport {
    positions.capture_hardware = positions
        .capture_hardware
        .checked_add(u64::try_from(frames).expect("frame count"))
        .expect("capture position");
    positions.capture_time = positions
        .capture_time
        .checked_add(scaled_frame_time(frames, positions.capture_ppm))
        .expect("capture time");
    let read = parts.capture_ingress.record_completed_read(frames).expect("completed capture read");
    read.publish(
        CaptureClockObservation::new(clock(positions.capture_hardware, positions.capture_time)),
        &packed_mono(frames, sample),
    )
    .expect("capture publication")
}

fn process_graph(
    parts: &mut ClockBridgeParts,
    positions: &mut Positions,
    quantum: usize,
    system: GraphSystemInput<'_>,
    output: &mut OutputStorage,
) -> GraphProcessReport {
    let report = parts
        .graph
        .process(
            graph_observation(positions.graph, positions.graph_time, quantum),
            system,
            output.buffers(quantum),
        )
        .expect("graph boundary");
    positions.graph = report.end_frame;
    positions.graph_time =
        positions.graph_time.checked_add(scaled_frame_time(quantum, 0)).expect("graph time");
    positions.graph_boundaries += 1;
    report
}

fn process_playback(
    parts: &mut ClockBridgeParts,
    positions: &mut Positions,
    submitter: &mut dyn PlaybackPeriodSubmitter,
    period_frames: usize,
) -> PlaybackProcessReport {
    if !positions.playback_clock_started {
        positions.playback_time = positions.graph_time;
        positions.playback_clock_started = true;
    }
    let report = parts
        .playback
        .process(positions.playback_observation(), submitter)
        .expect("playback boundary");
    positions.playback_time = positions
        .playback_time
        .checked_add(scaled_frame_time(period_frames, positions.playback_ppm))
        .expect("playback time");
    positions.playback_application = report.application_end_frame;
    if report.delivery != PlaybackBoundaryDelivery::DiscardedFilterDelay {
        positions.playback_hardware = report.application_end_frame;
    }
    positions.playback_boundaries += 1;
    report
}

fn prime_with_quantum(
    parts: &mut ClockBridgeParts,
    quantum: usize,
    capture_period: usize,
    playback_period: usize,
) -> Positions {
    parts.control.start_capture().expect("start capture priming");
    let mut positions = Positions::new();
    let mut output = OutputStorage::new();
    let mut submitter = ExactSubmitter::new(playback_period);
    for _ in 0..SIMULATION_FIFO_CAPACITY {
        match parts.control.state() {
            BridgeState::CapturePriming => {
                publish_capture(parts, &mut positions, capture_period, 0.25);
            }
            BridgeState::CaptureFilterDelay
            | BridgeState::CaptureEstimatorPriming
            | BridgeState::PlaybackPriming => {
                publish_capture(parts, &mut positions, quantum, 0.25);
                let report = process_graph(
                    parts,
                    &mut positions,
                    quantum,
                    GraphSystemInput::unconnected(),
                    &mut output,
                );
                assert_ne!(report.delivery, GraphBoundaryDelivery::Delivered);
            }
            BridgeState::PlaybackFilterDelay
            | BridgeState::PlaybackEstimatorPriming
            | BridgeState::PlaybackStabilizing => {
                publish_capture(parts, &mut positions, quantum, 0.25);
                process_graph(
                    parts,
                    &mut positions,
                    quantum,
                    GraphSystemInput::unconnected(),
                    &mut output,
                );
                process_playback(parts, &mut positions, &mut submitter, playback_period);
            }
            BridgeState::Primed => return positions,
            state => panic!("unexpected priming state: {state:?}"),
        }
    }
    panic!("simulation did not reach Primed")
}

fn prime(parts: &mut ClockBridgeParts) -> Positions {
    prime_with_quantum(
        parts,
        SIMULATION_PLAYBACK_PERIOD,
        SIMULATION_CAPTURE_PERIOD,
        SIMULATION_PLAYBACK_PERIOD,
    )
}

pub(in crate::audio_host) struct PrimedWorkerFixture {
    pub parts: ClockBridgeParts,
    pub capture_geometry: super::super::pcm::PhysicalPcmParameters,
    pub playback_geometry: super::super::pcm::PhysicalPcmParameters,
    pub capture_time: u64,
    pub playback_time: u64,
}

pub(in crate::audio_host) fn primed_worker_fixture() -> PrimedWorkerFixture {
    let mut parts = simulation_parts();
    let positions = prime(&mut parts);
    PrimedWorkerFixture {
        parts,
        capture_geometry: simulation_capture_geometry(),
        playback_geometry: simulation_playback_geometry(),
        capture_time: positions.capture_time,
        playback_time: positions.playback_time,
    }
}

fn reach_playback_stabilizing(parts: &mut ClockBridgeParts) -> Positions {
    parts.control.start_capture().expect("start capture priming");
    let mut positions = Positions::new();
    let mut output = OutputStorage::new();
    let mut submitter = ExactSubmitter::new(SIMULATION_PLAYBACK_PERIOD);
    for _ in 0..SIMULATION_FIFO_CAPACITY {
        match parts.control.state() {
            BridgeState::CapturePriming => {
                publish_capture(parts, &mut positions, SIMULATION_CAPTURE_PERIOD, 0.25);
            }
            BridgeState::CaptureFilterDelay
            | BridgeState::CaptureEstimatorPriming
            | BridgeState::PlaybackPriming => {
                publish_capture(parts, &mut positions, SIMULATION_PLAYBACK_PERIOD, 0.25);
                process_graph(
                    parts,
                    &mut positions,
                    SIMULATION_PLAYBACK_PERIOD,
                    GraphSystemInput::unconnected(),
                    &mut output,
                );
            }
            BridgeState::PlaybackFilterDelay | BridgeState::PlaybackEstimatorPriming => {
                publish_capture(parts, &mut positions, SIMULATION_PLAYBACK_PERIOD, 0.25);
                process_graph(
                    parts,
                    &mut positions,
                    SIMULATION_PLAYBACK_PERIOD,
                    GraphSystemInput::unconnected(),
                    &mut output,
                );
                process_playback(parts, &mut positions, &mut submitter, SIMULATION_PLAYBACK_PERIOD);
            }
            BridgeState::PlaybackStabilizing => return positions,
            state => panic!("unexpected state before playback stabilization: {state:?}"),
        }
    }
    panic!("simulation did not reach PlaybackStabilizing")
}

fn reach_playback_filter_delay(parts: &mut ClockBridgeParts) -> Positions {
    parts.control.start_capture().expect("start capture priming");
    let mut positions = Positions::new();
    let mut output = OutputStorage::new();
    for _ in 0..SIMULATION_FIFO_CAPACITY {
        match parts.control.state() {
            BridgeState::CapturePriming => {
                publish_capture(parts, &mut positions, SIMULATION_CAPTURE_PERIOD, 0.25);
            }
            BridgeState::CaptureFilterDelay
            | BridgeState::CaptureEstimatorPriming
            | BridgeState::PlaybackPriming => {
                publish_capture(parts, &mut positions, SIMULATION_PLAYBACK_PERIOD, 0.25);
                process_graph(
                    parts,
                    &mut positions,
                    SIMULATION_PLAYBACK_PERIOD,
                    GraphSystemInput::unconnected(),
                    &mut output,
                );
            }
            BridgeState::PlaybackFilterDelay => return positions,
            state => panic!("unexpected state before playback worker activation: {state:?}"),
        }
    }
    panic!("simulation did not queue Monitor Mix")
}

fn playback_worker_status(
    geometry: super::super::pcm::PhysicalPcmParameters,
    nanoseconds: u64,
    available_frames: i64,
    state: alsa::pcm::State,
) -> PcmStatusSnapshot {
    PcmStatusSnapshot {
        timestamp_seconds: i64::try_from(nanoseconds / NANOSECONDS_PER_SECOND)
            .expect("timestamp seconds"),
        timestamp_nanoseconds: i64::try_from(nanoseconds % NANOSECONDS_PER_SECOND)
            .expect("timestamp nanoseconds"),
        available_frames,
        delay_frames: -3,
        state_raw: state as i32,
        geometry,
    }
}

fn simulation_playback_geometry() -> super::super::pcm::PhysicalPcmParameters {
    Wave3PhysicalIoConfig::try_new(
        u32::try_from(SIMULATION_CAPTURE_PERIOD).expect("capture period"),
        SIMULATION_CAPTURE_BUFFER,
        u32::try_from(SIMULATION_PLAYBACK_PERIOD).expect("playback period"),
        SIMULATION_PLAYBACK_BUFFER,
    )
    .expect("simulation physical geometry")
    .parameters(PcmDirection::Playback)
}

fn simulation_capture_geometry() -> super::super::pcm::PhysicalPcmParameters {
    Wave3PhysicalIoConfig::try_new(
        u32::try_from(SIMULATION_CAPTURE_PERIOD).expect("capture period"),
        SIMULATION_CAPTURE_BUFFER,
        u32::try_from(SIMULATION_PLAYBACK_PERIOD).expect("playback period"),
        SIMULATION_PLAYBACK_BUFFER,
    )
    .expect("simulation physical geometry")
    .parameters(PcmDirection::Capture)
}

fn activate(parts: &mut ClockBridgeParts) -> Positions {
    let positions = prime(parts);
    parts.control.activate().expect("activate delivery");
    positions
}

fn active_cycle(
    parts: &mut ClockBridgeParts,
    positions: &mut Positions,
    sample: f32,
    output: &mut OutputStorage,
    submitter: &mut dyn PlaybackPeriodSubmitter,
) -> (GraphProcessReport, PlaybackProcessReport) {
    publish_capture(parts, positions, SIMULATION_PLAYBACK_PERIOD, sample);
    let graph = process_graph(
        parts,
        positions,
        SIMULATION_PLAYBACK_PERIOD,
        GraphSystemInput::unconnected(),
        output,
    );
    let playback = process_playback(parts, positions, submitter, SIMULATION_PLAYBACK_PERIOD);
    (graph, playback)
}

#[test]
fn bridge_uses_independent_capture_and_playback_estimators_and_matchers() {
    let parts = simulation_parts();
    assert_eq!(parts.graph.capture_matcher.config().channels(), WAVE3_CAPTURE_CHANNELS);
    assert_eq!(parts.playback.matcher.config().channels(), WAVE3_PLAYBACK_CHANNELS);
    assert_ne!(
        std::ptr::addr_of!(parts.graph.capture_estimator),
        std::ptr::addr_of!(parts.playback.estimator)
    );
    assert_ne!(
        std::ptr::addr_of!(parts.graph.capture_controller),
        std::ptr::addr_of!(parts.playback.controller)
    );
}

#[test]
fn construction_requires_capture_and_playback_producer_headroom() {
    let profile = MixerProfile::default();
    let capture_physical = Wave3PhysicalIoConfig::try_new(256, 512, 64, 128)
        .expect("capture-headroom physical geometry");
    let capture = direction(1, 64, 512, 256, 128, 1.0, 1.0, 0.0, 0.0, 0.0);
    let playback = direction(2, 64, 4_096, 2_048, 512, 1.0, 1.0, 0.0, 0.0, 0.0);
    let config = ClockBridgeConfig::try_new(epoch(), 64, capture, playback).expect("bridge config");
    assert!(matches!(
        ClockBridgeParts::new(capture_physical, &profile, config),
        Err(BridgeBuildError::FifoProducerHeadroom {
            domain: BridgeClockDomain::WaveCapture,
            required_free_frames: 256,
            available_free_frames: 128,
        })
    ));

    let playback_physical = Wave3PhysicalIoConfig::try_new(64, 128, 32, 128)
        .expect("playback-headroom physical geometry");
    let capture = direction(1, 128, 4_096, 2_048, 512, 1.0, 1.0, 0.0, 0.0, 0.0);
    let playback = direction(2, 32, 512, 256, 192, 1.0, 1.0, 0.0, 0.0, 0.0);
    let config =
        ClockBridgeConfig::try_new(epoch(), 128, capture, playback).expect("bridge config");
    assert!(matches!(
        ClockBridgeParts::new(playback_physical, &profile, config),
        Err(BridgeBuildError::FifoProducerHeadroom {
            domain: BridgeClockDomain::WavePlayback,
            required_free_frames: 128,
            available_free_frames: 64,
        })
    ));
}

#[test]
fn capture_hardware_cannot_trail_completed_application_progress() {
    let mut parts = simulation_parts();
    parts.control.start_capture().expect("start capture");
    let fifo_fill = parts.graph.capture_consumer.available_frames().expect("capture fill");
    let fifo_write_sequence = parts.capture_ingress.producer.write_sequence();
    let fifo_read_sequence = parts.graph.capture_consumer.read_sequence();
    let application_position = parts.capture_ingress.application_position;
    let last_observation = parts.capture_ingress.last_observation;
    assert!(matches!(
        parts.graph.capture_observation_reader.peek(),
        Err(super::observation::CopySlotError::Empty)
    ));
    let read = parts.capture_ingress.record_completed_read(1).expect("completed capture read");

    assert!(matches!(
        read.publish(CaptureClockObservation::new(clock(0, 1)), &packed_mono(1, 0.0),),
        Err(BridgeError::CaptureHardwareBehindApplication { hardware: 0, application: 1 })
    ));
    assert_eq!(parts.control.state(), BridgeState::Faulted);
    assert_eq!(parts.graph.capture_consumer.available_frames(), Ok(fifo_fill));
    assert_eq!(parts.capture_ingress.producer.write_sequence(), fifo_write_sequence);
    assert_eq!(parts.graph.capture_consumer.read_sequence(), fifo_read_sequence);
    assert_eq!(parts.capture_ingress.application_position, application_position + 1);
    assert_eq!(parts.capture_ingress.last_observation, last_observation);
    assert!(matches!(
        parts.graph.capture_observation_reader.peek(),
        Err(super::observation::CopySlotError::Empty)
    ));

    let mut parts = simulation_parts();
    parts.control.start_capture().expect("start capture");
    let read = parts.capture_ingress.record_completed_read(1).expect("completed capture read");
    let report = read
        .publish(CaptureClockObservation::new(clock(2, 1)), &packed_mono(1, 0.0))
        .expect("capture availability may put hardware ahead of application");
    assert_eq!(report.application_end_frame, 1);
    assert_eq!(report.hardware_frame_position, 2);
}

#[test]
fn startup_is_state_driven_and_ready_requires_both_measured_clocks() {
    let mut parts = simulation_parts();
    let mut positions = prime(&mut parts);
    assert_eq!(parts.control.state(), BridgeState::Primed);
    assert!(parts.graph.capture_estimator.ratio().is_some());
    assert!(parts.playback.estimator.ratio().is_some());
    assert!(positions.graph_boundaries > 0);
    assert!(positions.playback_boundaries > 0);
    assert_eq!(parts.shared_delay(BridgeClockDomain::WaveCapture), 0);
    assert_eq!(parts.shared_delay(BridgeClockDomain::WavePlayback), 0);

    parts.control.shared.playback_in_flight.store(1, Ordering::SeqCst);
    assert_eq!(
        parts.control.activate(),
        Err(BridgeError::ActivationNotQuiescent { capture: 0, graph: 0, playback: 1 })
    );
    parts.control.shared.playback_in_flight.store(0, Ordering::SeqCst);
    parts.control.activate().expect("quiescent activation");

    let mut output = OutputStorage::new();
    let mut submitter = ExactSubmitter::new(SIMULATION_PLAYBACK_PERIOD);
    let (graph, playback) =
        active_cycle(&mut parts, &mut positions, 0.25, &mut output, &mut submitter);
    assert_eq!(graph.delivery, GraphBoundaryDelivery::Delivered);
    assert_eq!(playback.delivery, PlaybackBoundaryDelivery::SubmittedActive);
}

#[test]
#[allow(clippy::too_many_lines)]
fn playback_worker_starts_pcm_only_after_a_complete_queued_period() {
    let mut parts = simulation_parts();
    let positions = reach_playback_filter_delay(&mut parts);
    assert_eq!(parts.control.state(), BridgeState::PlaybackFilterDelay);
    assert!(parts.capture_ingress.worker_application_position() > 0);

    let geometry = simulation_playback_geometry();
    let buffer_frames = i64::from(geometry.buffer_frames);
    let period_time = scaled_frame_time(SIMULATION_PLAYBACK_PERIOD, 0);
    let first_time = positions.graph_time;
    let calls = Arc::new(Mutex::new(Vec::new()));
    let pcm = ScriptedPlaybackPcm {
        waits: VecDeque::from([
            Ok(PcmWait::Ready),
            Ok(PcmWait::Ready),
            Ok(PcmWait::Ready),
            Ok(PcmWait::Ready),
        ]),
        writes: VecDeque::from([
            Ok(100),
            Err(PcmIoError::Interrupted),
            Ok(SIMULATION_PLAYBACK_PERIOD - 100),
            Ok(SIMULATION_PLAYBACK_PERIOD),
        ]),
        statuses: VecDeque::from([
            Ok(playback_worker_status(
                geometry,
                first_time,
                buffer_frames,
                alsa::pcm::State::Prepared,
            )),
            Ok(playback_worker_status(
                geometry,
                first_time + period_time,
                buffer_frames,
                alsa::pcm::State::Prepared,
            )),
            Ok(playback_worker_status(
                geometry,
                first_time + 2 * period_time,
                buffer_frames,
                alsa::pcm::State::Running,
            )),
            Err(PcmIoError::Disconnected),
        ]),
        calls: Arc::clone(&calls),
    };
    let policy = WorkerIoPolicy::try_new(1, 2).expect("worker I/O policy");
    let parked = ParkedPlaybackWorker::new(Box::new(pcm), parts.playback, geometry, policy)
        .expect("parked playback worker");
    assert!(calls.lock().expect("playback calls").is_empty());
    let mut worker =
        parked.activate_thread_after_monitor_mix_queued().expect("playback thread activation");

    wait_for_playback_finish(&worker);
    let first = next_playback_boundary(&mut worker);
    assert!(matches!(
        first,
        PlaybackWorkerBoundary {
            report: PlaybackProcessReport {
                delivery: PlaybackBoundaryDelivery::DiscardedFilterDelay,
                ..
            },
            pcm_started: false,
            ..
        }
    ));
    assert!(worker.try_boundary().is_none(), "full boundary slot must retain its first value");
    let shutdown = worker.park_and_join().expect("playback worker join");
    assert_eq!(
        shutdown.terminal,
        PlaybackWorkerTerminal::Observe(PlaybackWriteError::Disconnected { accepted_frames: 0 })
    );
    assert_eq!(shutdown.drop_stream_result, Ok(()));
    assert_eq!(
        shutdown.egress.worker_application_position(),
        (2 * SIMULATION_PLAYBACK_PERIOD) as u64
    );
    assert_eq!(
        shutdown
            .egress
            .last_hardware_observation
            .expect("later playback hardware observation")
            .frame_position()
            .get(),
        SIMULATION_PLAYBACK_PERIOD as u64
    );
    assert_eq!(parts.control.state(), BridgeState::Faulted);

    let calls = calls.lock().expect("playback calls");
    let start = calls
        .iter()
        .position(|call| *call == PlaybackWorkerCall::StartPcm)
        .expect("explicit PCM start");
    let writes_before_start = calls[..start]
        .iter()
        .filter(|call| matches!(call, PlaybackWorkerCall::Write { .. }))
        .count();
    assert_eq!(writes_before_start, 3);
    assert!(matches!(calls.last(), Some(PlaybackWorkerCall::DropStream)));
}

#[test]
fn playback_worker_shutdown_cancels_a_finite_wait_without_faulting() {
    let mut parts = simulation_parts();
    let positions = reach_playback_filter_delay(&mut parts);
    let geometry = simulation_playback_geometry();
    let gate = Arc::new(WaitGate { released: Mutex::new(false), condition: Condvar::new() });
    let calls = Arc::new(Mutex::new(Vec::new()));
    let pcm = CancellablePlaybackPcm {
        first_status: Some(playback_worker_status(
            geometry,
            positions.graph_time,
            i64::from(geometry.buffer_frames),
            alsa::pcm::State::Prepared,
        )),
        gate: Arc::clone(&gate),
        calls: Arc::clone(&calls),
    };
    let parked = ParkedPlaybackWorker::new(
        Box::new(pcm),
        parts.playback,
        geometry,
        WorkerIoPolicy::try_new(1, 2).expect("worker I/O policy"),
    )
    .expect("parked playback worker");
    let mut worker =
        parked.activate_thread_after_monitor_mix_queued().expect("playback thread activation");
    assert!(!next_playback_boundary(&mut worker).pcm_started);
    worker.request_stop();
    {
        let mut released = gate.released.lock().expect("wait gate");
        *released = true;
        gate.condition.notify_all();
    }
    let shutdown = worker.park_and_join().expect("playback worker join");
    assert_eq!(shutdown.terminal, PlaybackWorkerTerminal::Stopped);
    assert_eq!(shutdown.drop_stream_result, Ok(()));
    assert_ne!(parts.control.state(), BridgeState::Faulted);
    assert_eq!(calls.lock().expect("playback calls").last(), Some(&PlaybackWorkerCall::DropStream));
}

#[test]
fn filter_delay_and_estimator_priming_use_nominal_without_controller_history() {
    let mut parts = aggressive_delay_parts();
    parts.control.start_capture().expect("start capture");
    let mut positions = Positions::new();
    let mut output = OutputStorage::new();
    let mut submitter = ExactSubmitter::new(SIMULATION_DELAY_PLAYBACK_PERIOD);
    while parts.control.state() == BridgeState::CapturePriming {
        publish_capture(&mut parts, &mut positions, SIMULATION_DELAY_CAPTURE_PERIOD, 0.25);
    }

    let capture_delay = parts.shared_delay(BridgeClockDomain::WaveCapture);
    let capture_history =
        (parts.graph.capture_controller.integral(), parts.graph.capture_controller.ratio());
    let mut capture_delay_boundaries = 0;
    while parts.control.state() == BridgeState::CaptureFilterDelay {
        publish_capture(&mut parts, &mut positions, SIMULATION_DELAY_QUANTUM, 0.25);
        process_graph(
            &mut parts,
            &mut positions,
            SIMULATION_DELAY_QUANTUM,
            GraphSystemInput::unconnected(),
            &mut output,
        );
        capture_delay_boundaries += 1;
        assert_eq!(
            (parts.graph.capture_controller.integral(), parts.graph.capture_controller.ratio()),
            capture_history
        );
    }
    assert_eq!(capture_delay_boundaries, capture_delay.div_ceil(SIMULATION_DELAY_QUANTUM));

    while matches!(
        parts.control.state(),
        BridgeState::CaptureEstimatorPriming | BridgeState::PlaybackPriming
    ) {
        publish_capture(&mut parts, &mut positions, SIMULATION_DELAY_QUANTUM, 0.25);
        process_graph(
            &mut parts,
            &mut positions,
            SIMULATION_DELAY_QUANTUM,
            GraphSystemInput::unconnected(),
            &mut output,
        );
    }
    let playback_delay = parts.shared_delay(BridgeClockDomain::WavePlayback);
    let playback_history =
        (parts.playback.controller.integral(), parts.playback.controller.ratio());
    let mut playback_delay_boundaries = 0;
    while parts.control.state() == BridgeState::PlaybackFilterDelay {
        publish_capture(&mut parts, &mut positions, SIMULATION_DELAY_QUANTUM, 0.25);
        process_graph(
            &mut parts,
            &mut positions,
            SIMULATION_DELAY_QUANTUM,
            GraphSystemInput::unconnected(),
            &mut output,
        );
        let report = process_playback(
            &mut parts,
            &mut positions,
            &mut submitter,
            SIMULATION_DELAY_PLAYBACK_PERIOD,
        );
        assert_eq!(report.application_start_frame, report.application_end_frame);
        playback_delay_boundaries += 1;
        assert_eq!(
            (parts.playback.controller.integral(), parts.playback.controller.ratio()),
            playback_history
        );
    }
    assert_eq!(
        playback_delay_boundaries,
        playback_delay.div_ceil(SIMULATION_DELAY_PLAYBACK_PERIOD)
    );
    assert_eq!(submitter.calls, 0, "filter-delay output must not be submitted");
    assert_eq!(parts.control.state(), BridgeState::PlaybackEstimatorPriming);
}

#[test]
fn priming_check_rechecks_published_fifo_sequences() {
    let mut parts = simulation_parts();
    let _positions = prime(&mut parts);
    let current_fill =
        parts.control.playback_fill_observer.stable_fill_frames().expect("playback fill");
    let required_fill = SIMULATION_TARGET_FILL + SIMULATION_STARTUP_GUARD + 1;
    let extra = required_fill.saturating_sub(current_fill);
    let samples = vec![0.0; extra * WAVE3_PLAYBACK_CHANNELS];
    parts
        .graph
        .playback_producer
        .prepare_push(&samples)
        .expect("test-only cursor movement")
        .commit();
    assert_eq!(
        parts.control.activate(),
        Err(BridgeError::State { actual: BridgeState::PlaybackStabilizing })
    );
    assert_eq!(parts.control.state(), BridgeState::PlaybackStabilizing);
}

#[test]
fn completed_capture_boundary_spans_gate_accounting_and_publishes_once() {
    let mut parts = simulation_parts();
    let mut positions = reach_playback_stabilizing(&mut parts);
    let mut output = OutputStorage::new();
    process_graph(
        &mut parts,
        &mut positions,
        SIMULATION_PLAYBACK_PERIOD,
        GraphSystemInput::unconnected(),
        &mut output,
    );

    let application_start = parts.capture_ingress.worker_application_position();
    positions.capture_hardware += SIMULATION_PLAYBACK_PERIOD as u64;
    positions.capture_time += scaled_frame_time(SIMULATION_PLAYBACK_PERIOD, 0);
    let capture_observation =
        CaptureClockObservation::new(clock(positions.capture_hardware, positions.capture_time));
    let packed = packed_mono(SIMULATION_PLAYBACK_PERIOD, 0.25);
    let boundary = parts
        .capture_ingress
        .record_completed_read(SIMULATION_PLAYBACK_PERIOD)
        .expect("completed read enters one capture boundary");
    assert_eq!(
        parts.control.shared.capture_in_flight.load(Ordering::SeqCst),
        1,
        "completed-read accounting must keep the gate token"
    );

    let mut submitter = ExactSubmitter::new(SIMULATION_PLAYBACK_PERIOD);
    let playback_observation = positions.playback_observation();
    let (release_tx, release_rx) = mpsc::sync_channel(0);
    let report = thread::scope(|scope| {
        let capture = scope.spawn(move || {
            release_rx.recv().expect("release capture publication");
            boundary.publish(capture_observation, &packed)
        });
        let playback = parts
            .playback
            .process(playback_observation, &mut submitter)
            .expect("playback gate observes in-flight capture");
        assert_eq!(parts.control.state(), BridgeState::PlaybackStabilizing);
        release_tx.send(()).expect("release retained capture prefix");
        let capture = capture.join().expect("capture publication thread").expect("capture report");
        (capture, playback)
    });

    assert_eq!(report.0.application_start_frame, application_start);
    assert_eq!(
        report.0.application_end_frame,
        application_start + SIMULATION_PLAYBACK_PERIOD as u64
    );
    assert_eq!(report.0.frames, SIMULATION_PLAYBACK_PERIOD);
    assert_eq!(parts.capture_ingress.worker_application_position(), report.0.application_end_frame);
    assert_eq!(parts.control.shared.capture_in_flight.load(Ordering::SeqCst), 0);
    assert_eq!(report.1.delivery, PlaybackBoundaryDelivery::SubmittedStabilizing);
}

#[test]
fn completed_capture_prefix_retries_accounting_after_priming_check() {
    let mut parts = simulation_parts();
    let mut positions = reach_playback_stabilizing(&mut parts);
    let application_start = parts.capture_ingress.worker_application_position();
    positions.capture_hardware += SIMULATION_PLAYBACK_PERIOD as u64;
    positions.capture_time += scaled_frame_time(SIMULATION_PLAYBACK_PERIOD, 0);
    let bytes = packed_mono(SIMULATION_PLAYBACK_PERIOD, 0.25);

    assert!(
        parts
            .control
            .shared
            .transition(BridgeState::PlaybackStabilizing, BridgeState::PrimingCheck)
    );
    assert!(matches!(
        parts.capture_ingress.record_completed_read(SIMULATION_PLAYBACK_PERIOD),
        Err(BridgeError::State { actual: BridgeState::PrimingCheck })
    ));
    assert_eq!(parts.capture_ingress.worker_application_position(), application_start);
    assert!(
        parts
            .control
            .shared
            .transition(BridgeState::PrimingCheck, BridgeState::PlaybackStabilizing)
    );

    let boundary = parts
        .capture_ingress
        .record_completed_read(SIMULATION_PLAYBACK_PERIOD)
        .expect("retry retained completed prefix");
    let report = boundary
        .publish(
            CaptureClockObservation::new(clock(positions.capture_hardware, positions.capture_time)),
            &bytes,
        )
        .expect("publish retained prefix once");
    assert_eq!(report.application_start_frame, application_start);
    assert_eq!(report.application_end_frame, application_start + SIMULATION_PLAYBACK_PERIOD as u64);
    assert_eq!(parts.capture_ingress.worker_application_position(), report.application_end_frame);
}

#[test]
fn mono_capture_duplication_and_packed_playback_are_deterministic() {
    fn one_run() -> (Vec<f32>, Vec<f32>, Vec<u8>) {
        let mut parts = simulation_parts();
        let mut positions = activate(&mut parts);
        let mut output = OutputStorage::new();
        let mut submitter = ExactSubmitter::new(SIMULATION_PLAYBACK_PERIOD);
        active_cycle(&mut parts, &mut positions, 0.375, &mut output, &mut submitter);
        (
            output.microphone_left[..SIMULATION_PLAYBACK_PERIOD].to_vec(),
            output.microphone_right[..SIMULATION_PLAYBACK_PERIOD].to_vec(),
            submitter.bytes,
        )
    }

    let first = one_run();
    let second = one_run();
    assert_eq!(first, second);
    assert_eq!(first.0, first.1);
    assert!(first.0.iter().any(|sample| sample.abs() > 0.01));
    let mut playback = vec![0.0; SIMULATION_PLAYBACK_PERIOD * WAVE3_PLAYBACK_CHANNELS];
    decode_s24_3le(&first.2, WAVE3_PLAYBACK_CHANNELS, &mut playback).expect("decode playback");
    assert!(playback.chunks_exact(2).all(|frame| frame[0] == frame[1]));
}

#[test]
fn connected_system_requires_complete_finite_graph_buffers() {
    let mut parts = simulation_parts();
    let mut positions = activate(&mut parts);
    publish_capture(&mut parts, &mut positions, SIMULATION_PLAYBACK_PERIOD, 0.25);
    let mut left = [0.0; SIMULATION_PLAYBACK_PERIOD];
    let right = [0.0; SIMULATION_PLAYBACK_PERIOD];
    left[17] = f32::NAN;
    let mut output = OutputStorage::new();
    assert!(matches!(
        parts.graph.process(
            graph_observation(positions.graph, positions.graph_time, SIMULATION_PLAYBACK_PERIOD),
            GraphSystemInput::connected(&left, &right),
            output.buffers(SIMULATION_PLAYBACK_PERIOD),
        ),
        Err(BridgeError::NonFinite { domain: BridgeClockDomain::PipeWireGraph, sample_index: 34 })
    ));
    assert_eq!(parts.control.state(), BridgeState::Faulted);
}

#[test]
fn cross_epoch_repeated_regressing_and_fixed_quantum_observations_fault() {
    let mut parts = simulation_parts();
    parts.control.start_capture().expect("start capture");
    let wrong_epoch = ClockAttemptEpoch::try_new(EPOCH_VALUE + 1).expect("other epoch");
    let wrong = ClockObservation::new(
        wrong_epoch,
        ClockFramePosition::new(1),
        MonotonicNanoseconds::new(1),
    );
    let read = parts.capture_ingress.record_completed_read(1).expect("completed capture read");
    assert!(matches!(
        read.publish(CaptureClockObservation::new(wrong), &packed_mono(1, 0.0)),
        Err(BridgeError::ObservationEpoch { domain: BridgeClockDomain::WaveCapture, .. })
    ));

    let mut parts = simulation_parts();
    parts.control.start_capture().expect("start capture");
    let bytes = packed_mono(1, 0.0);
    let read = parts.capture_ingress.record_completed_read(1).expect("completed capture read");
    read.publish(CaptureClockObservation::new(clock(1, 1)), &bytes).expect("first observation");
    let read = parts.capture_ingress.record_completed_read(1).expect("completed capture read");
    assert!(matches!(
        read.publish(CaptureClockObservation::new(clock(1, 2)), &bytes),
        Err(BridgeError::RepeatedClockObservation { domain: BridgeClockDomain::WaveCapture })
    ));

    let mut parts = simulation_parts();
    parts.control.start_capture().expect("start capture");
    let read = parts.capture_ingress.record_completed_read(1).expect("completed capture read");
    read.publish(CaptureClockObservation::new(clock(2, 2)), &bytes).expect("first observation");
    let read = parts.capture_ingress.record_completed_read(1).expect("completed capture read");
    assert!(matches!(
        read.publish(CaptureClockObservation::new(clock(1, 3)), &bytes),
        Err(BridgeError::Discontinuity { domain: BridgeClockDomain::WaveCapture, .. })
    ));

    let mut parts = simulation_parts();
    let mut positions = activate(&mut parts);
    publish_capture(&mut parts, &mut positions, SIMULATION_PLAYBACK_PERIOD, 0.25);
    let mut output = OutputStorage::new();
    assert!(matches!(
        parts.graph.process(
            graph_observation(positions.graph, positions.graph_time, 128),
            GraphSystemInput::unconnected(),
            output.buffers(128),
        ),
        Err(BridgeError::FixedQuantumChanged { expected: SIMULATION_PLAYBACK_PERIOD, actual: 128 })
    ));

    let mut parts = simulation_parts();
    let positions = activate(&mut parts);
    let repeated = PlaybackClockObservation::new(
        parts.playback.last_hardware_observation.expect("playback history"),
        ClockFramePosition::new(positions.playback_application),
    );
    assert!(matches!(
        parts.playback.process(repeated, &mut ExactSubmitter::new(SIMULATION_PLAYBACK_PERIOD)),
        Err(BridgeError::RepeatedClockObservation { domain: BridgeClockDomain::WavePlayback })
    ));

    let mut parts = simulation_parts();
    let positions = activate(&mut parts);
    let last_hardware = parts.playback.last_hardware_observation.expect("playback history");
    let regressing = PlaybackClockObservation::new(
        clock(last_hardware.frame_position().get() - 1, last_hardware.monotonic_time().get() + 1),
        ClockFramePosition::new(positions.playback_application),
    );
    assert!(matches!(
        parts.playback.process(regressing, &mut ExactSubmitter::new(SIMULATION_PLAYBACK_PERIOD)),
        Err(BridgeError::Discontinuity { domain: BridgeClockDomain::WavePlayback, .. })
    ));

    let mut parts = simulation_parts();
    let positions = activate(&mut parts);
    let stale_time = parts
        .graph
        .capture_estimator
        .latest_input()
        .expect("capture estimator history")
        .monotonic_time()
        .get()
        + SIMULATION_MAXIMUM_SKEW
        + 1;
    let mut output = OutputStorage::new();
    assert!(matches!(
        parts.graph.process(
            graph_observation(positions.graph, stale_time, SIMULATION_PLAYBACK_PERIOD),
            GraphSystemInput::unconnected(),
            output.buffers(SIMULATION_PLAYBACK_PERIOD),
        ),
        Err(BridgeError::Estimator {
            domain: BridgeClockDomain::WaveCapture,
            error: ClockRateEstimatorError::ObservationSkew { .. }
        })
    ));
}

#[test]
fn graph_and_playback_boundaries_map_epoch_and_time_faults() {
    let wrong_epoch = ClockAttemptEpoch::try_new(EPOCH_VALUE + 1).expect("other epoch");

    let mut parts = simulation_parts();
    let mut positions = activate(&mut parts);
    publish_capture(&mut parts, &mut positions, SIMULATION_PLAYBACK_PERIOD, 0.25);
    let wrong_graph_clock = ClockObservation::new(
        wrong_epoch,
        ClockFramePosition::new(positions.graph),
        MonotonicNanoseconds::new(positions.graph_time),
    );
    let wrong_graph = GraphClockObservation::try_new(wrong_graph_clock, SIMULATION_PLAYBACK_PERIOD)
        .expect("graph observation");
    let mut output = OutputStorage::new();
    assert!(matches!(
        parts.graph.process(
            wrong_graph,
            GraphSystemInput::unconnected(),
            output.buffers(SIMULATION_PLAYBACK_PERIOD),
        ),
        Err(BridgeError::ObservationEpoch { domain: BridgeClockDomain::PipeWireGraph, .. })
    ));

    let mut parts = simulation_parts();
    let mut positions = activate(&mut parts);
    publish_capture(&mut parts, &mut positions, SIMULATION_PLAYBACK_PERIOD, 0.25);
    let previous_graph = parts.graph.last_observation.expect("graph history");
    let regressed_graph_time =
        previous_graph.monotonic_time().get().checked_sub(1).expect("positive graph time");
    assert!(matches!(
        parts.graph.process(
            graph_observation(positions.graph, regressed_graph_time, SIMULATION_PLAYBACK_PERIOD,),
            GraphSystemInput::unconnected(),
            output.buffers(SIMULATION_PLAYBACK_PERIOD),
        ),
        Err(BridgeError::MonotonicTimeDiscontinuity {
            domain: BridgeClockDomain::PipeWireGraph,
            ..
        })
    ));

    let mut parts = simulation_parts();
    let positions = activate(&mut parts);
    let wrong_playback_clock = ClockObservation::new(
        wrong_epoch,
        ClockFramePosition::new(positions.playback_hardware),
        MonotonicNanoseconds::new(positions.playback_time),
    );
    assert!(matches!(
        parts.playback.process(
            PlaybackClockObservation::new(
                wrong_playback_clock,
                ClockFramePosition::new(positions.playback_application),
            ),
            &mut ExactSubmitter::new(SIMULATION_PLAYBACK_PERIOD),
        ),
        Err(BridgeError::ObservationEpoch { domain: BridgeClockDomain::WavePlayback, .. })
    ));

    let mut parts = simulation_parts();
    let positions = activate(&mut parts);
    let previous_playback =
        parts.playback.last_hardware_observation.expect("playback hardware history");
    assert!(positions.playback_hardware > previous_playback.frame_position().get());
    let regressed_playback = PlaybackClockObservation::new(
        clock(
            positions.playback_hardware,
            previous_playback
                .monotonic_time()
                .get()
                .checked_sub(1)
                .expect("positive playback time"),
        ),
        ClockFramePosition::new(positions.playback_application),
    );
    assert!(matches!(
        parts
            .playback
            .process(regressed_playback, &mut ExactSubmitter::new(SIMULATION_PLAYBACK_PERIOD),),
        Err(BridgeError::MonotonicTimeDiscontinuity {
            domain: BridgeClockDomain::WavePlayback,
            ..
        })
    ));
}

#[test]
fn playback_submission_and_hardware_progress_are_separate_ledgers() {
    let mut parts = simulation_parts();
    let mut positions = activate(&mut parts);
    let observed_hardware = positions.playback_hardware;
    let mut output = OutputStorage::new();
    let mut submitter = ExactSubmitter::new(SIMULATION_PLAYBACK_PERIOD);
    let (_, report) = active_cycle(&mut parts, &mut positions, 0.25, &mut output, &mut submitter);
    assert_eq!(report.hardware_frame_position, observed_hardware);
    assert_eq!(
        report.application_end_frame,
        report.application_start_frame + SIMULATION_PLAYBACK_PERIOD as u64
    );
    assert_eq!(parts.playback.submitted_position, report.application_end_frame);
    assert_eq!(
        parts
            .playback
            .last_hardware_observation
            .expect("committed observation")
            .frame_position()
            .get(),
        observed_hardware
    );
}

#[test]
fn fifo_shortage_and_overflow_are_terminal() {
    let mut parts = simulation_parts();
    parts.control.start_capture().expect("start capture");
    let mut positions = Positions::new();
    while parts.capture_ingress.producer.free_frames().expect("free frames") > 0 {
        let frames = parts
            .capture_ingress
            .producer
            .free_frames()
            .expect("free frames")
            .min(SIMULATION_CAPTURE_PERIOD);
        publish_capture(&mut parts, &mut positions, frames, 0.0);
    }
    positions.capture_hardware += 1;
    positions.capture_time += 1;
    let read = parts.capture_ingress.record_completed_read(1).expect("completed capture read");
    assert!(matches!(
        read.publish(
            CaptureClockObservation::new(clock(positions.capture_hardware, positions.capture_time)),
            &packed_mono(1, 0.0),
        ),
        Err(BridgeError::Overflow { domain: BridgeClockDomain::WaveCapture, .. })
    ));

    let mut parts = simulation_parts();
    let mut positions = activate(&mut parts);
    let mut output = OutputStorage::new();
    let capture_error = loop {
        match parts.graph.process(
            graph_observation(positions.graph, positions.graph_time, SIMULATION_PLAYBACK_PERIOD),
            GraphSystemInput::unconnected(),
            output.buffers(SIMULATION_PLAYBACK_PERIOD),
        ) {
            Ok(report) => {
                positions.graph = report.end_frame;
                positions.graph_time += scaled_frame_time(SIMULATION_PLAYBACK_PERIOD, 0);
            }
            Err(error) => break error,
        }
    };
    assert!(matches!(
        capture_error,
        BridgeError::Shortage { domain: BridgeClockDomain::WaveCapture, .. }
    ));

    let mut parts = simulation_parts();
    let mut positions = activate(&mut parts);
    let mut submitter = ExactSubmitter::new(SIMULATION_PLAYBACK_PERIOD);
    let playback_error = loop {
        match parts.playback.process(positions.playback_observation(), &mut submitter) {
            Ok(report) => {
                positions.playback_time += scaled_frame_time(SIMULATION_PLAYBACK_PERIOD, 0);
                positions.playback_application = report.application_end_frame;
                positions.playback_hardware = report.application_end_frame;
            }
            Err(error) => break error,
        }
    };
    assert!(matches!(
        playback_error,
        BridgeError::Shortage { domain: BridgeClockDomain::WavePlayback, .. }
    ));

    let mut parts = simulation_parts();
    parts.control.start_capture().expect("start capture");
    let mut positions = Positions::new();
    let mut output = OutputStorage::new();
    while parts.control.state() == BridgeState::CapturePriming {
        publish_capture(&mut parts, &mut positions, SIMULATION_CAPTURE_PERIOD, 0.0);
    }
    let playback_overflow = loop {
        publish_capture(&mut parts, &mut positions, SIMULATION_PLAYBACK_PERIOD, 0.0);
        match parts.graph.process(
            graph_observation(positions.graph, positions.graph_time, SIMULATION_PLAYBACK_PERIOD),
            GraphSystemInput::unconnected(),
            output.buffers(SIMULATION_PLAYBACK_PERIOD),
        ) {
            Ok(report) => {
                positions.graph = report.end_frame;
                positions.graph_time += scaled_frame_time(SIMULATION_PLAYBACK_PERIOD, 0);
            }
            Err(error) => break error,
        }
    };
    assert!(matches!(
        playback_overflow,
        BridgeError::Overflow { domain: BridgeClockDomain::WavePlayback, .. }
    ));
}

#[test]
fn application_hardware_and_graph_position_contracts_are_exact() {
    let mut parts = simulation_parts();
    let mut positions = activate(&mut parts);
    publish_capture(&mut parts, &mut positions, SIMULATION_PLAYBACK_PERIOD, 0.25);
    let mut output = OutputStorage::new();
    assert!(matches!(
        parts.graph.process(
            graph_observation(
                positions.graph + 1,
                positions.graph_time,
                SIMULATION_PLAYBACK_PERIOD,
            ),
            GraphSystemInput::unconnected(),
            output.buffers(SIMULATION_PLAYBACK_PERIOD),
        ),
        Err(BridgeError::Discontinuity { domain: BridgeClockDomain::PipeWireGraph, .. })
    ));

    let mut parts = simulation_parts();
    let positions = activate(&mut parts);
    let wrong_application = PlaybackClockObservation::new(
        clock(positions.playback_hardware, positions.playback_time),
        ClockFramePosition::new(positions.playback_application + 1),
    );
    assert!(matches!(
        parts
            .playback
            .process(wrong_application, &mut ExactSubmitter::new(SIMULATION_PLAYBACK_PERIOD)),
        Err(BridgeError::PlaybackApplicationPosition { .. })
    ));

    let mut parts = simulation_parts();
    let positions = activate(&mut parts);
    let hardware_ahead = PlaybackClockObservation::new(
        clock(positions.playback_application + 1, positions.playback_time),
        ClockFramePosition::new(positions.playback_application),
    );
    assert!(matches!(
        parts
            .playback
            .process(hardware_ahead, &mut ExactSubmitter::new(SIMULATION_PLAYBACK_PERIOD)),
        Err(BridgeError::PlaybackHardwareAhead { .. })
    ));

    let mut parts = simulation_parts();
    let mut positions = activate(&mut parts);
    publish_capture(&mut parts, &mut positions, SIMULATION_PLAYBACK_PERIOD, 0.25);
    let left = [0.0; SIMULATION_PLAYBACK_PERIOD];
    assert!(matches!(
        parts.graph.process(
            graph_observation(positions.graph, positions.graph_time, SIMULATION_PLAYBACK_PERIOD,),
            GraphSystemInput::from_raw(true, Some(&left), None),
            output.buffers(SIMULATION_PLAYBACK_PERIOD),
        ),
        Err(BridgeError::MissingSystemBuffer)
    ));
}

#[test]
fn failed_downstream_boundaries_commit_no_controller_fifo_estimator_or_position_state() {
    let mut parts = simulation_parts();
    let mut positions = activate(&mut parts);
    publish_capture(&mut parts, &mut positions, SIMULATION_PLAYBACK_PERIOD, 0.25);
    let controller =
        (parts.graph.capture_controller.integral(), parts.graph.capture_controller.ratio());
    let fifo_fill = parts.graph.capture_consumer.available_frames();
    let estimator_input = parts.graph.capture_estimator.latest_input();
    parts.graph.capture_matcher.reset();
    let mut output = OutputStorage::new();
    assert!(matches!(
        parts.graph.process(
            graph_observation(positions.graph, positions.graph_time, SIMULATION_PLAYBACK_PERIOD),
            GraphSystemInput::unconnected(),
            output.buffers(SIMULATION_PLAYBACK_PERIOD),
        ),
        Err(BridgeError::Asrc {
            domain: BridgeClockDomain::WaveCapture,
            error: RateMatchError::NotWarmed
        })
    ));
    assert_eq!(
        (parts.graph.capture_controller.integral(), parts.graph.capture_controller.ratio()),
        controller
    );
    assert_eq!(parts.graph.capture_consumer.available_frames(), fifo_fill);
    assert_eq!(parts.graph.capture_estimator.latest_input(), estimator_input);

    let mut parts = simulation_parts();
    let positions = activate(&mut parts);
    let controller = (parts.playback.controller.integral(), parts.playback.controller.ratio());
    let fifo_fill = parts.playback.consumer.available_frames();
    let estimator_input = parts.playback.estimator.latest_input();
    let submitted = parts.playback.submitted_position;
    assert!(matches!(
        parts.playback.process(positions.playback_observation(), &mut FailingSubmitter),
        Err(BridgeError::Playback(PlaybackWriteError::Failed {
            accepted_frames: 0,
            errno: libc::EIO,
        }))
    ));
    assert_eq!(
        (parts.playback.controller.integral(), parts.playback.controller.ratio()),
        controller
    );
    assert_eq!(parts.playback.consumer.available_frames(), fifo_fill);
    assert_eq!(parts.playback.estimator.latest_input(), estimator_input);
    assert_eq!(parts.playback.submitted_position, submitted);
    assert_eq!(parts.control.state(), BridgeState::Faulted);

    let mut parts = simulation_parts();
    let positions = activate(&mut parts);
    assert!(matches!(
        parts
            .playback
            .process(positions.playback_observation(), &mut ShortSubmitter),
        Err(BridgeError::PlaybackSubmission {
            expected_frames: SIMULATION_PLAYBACK_PERIOD,
            actual_frames
        }) if actual_frames == SIMULATION_PLAYBACK_PERIOD - 1
    ));
}

#[test]
fn partial_playback_failure_commits_only_accepted_application_progress() {
    let mut parts = simulation_parts();
    let positions = activate(&mut parts);
    let submitted = parts.playback.submitted_position;
    let fifo_fill = parts.playback.consumer.available_frames();
    assert_eq!(
        parts.playback.process(positions.playback_observation(), &mut PartialFailingSubmitter),
        Err(BridgeError::Playback(PlaybackWriteError::Xrun { accepted_frames: 17 }))
    );
    assert_eq!(parts.playback.submitted_position, submitted + 17);
    assert_eq!(parts.playback.consumer.available_frames(), fifo_fill);
    assert_eq!(parts.control.state(), BridgeState::Faulted);
}

#[test]
#[allow(clippy::too_many_lines)]
fn playback_fifo_rejection_after_graph_processing_commits_no_boundary_state() {
    let mut parts = simulation_parts();
    let mut positions = activate(&mut parts);
    let free_frames = parts.graph.playback_producer.free_frames().expect("playback free frames");
    let retained_free_frames = SIMULATION_PLAYBACK_PERIOD - 1;
    let fill_frames = free_frames
        .checked_sub(retained_free_frames)
        .expect("simulation has more than one quantum of producer headroom");
    parts
        .graph
        .playback_producer
        .prepare_push(&vec![0.0; fill_frames * WAVE3_PLAYBACK_CHANNELS])
        .expect("fill playback FIFO to one frame short of a graph quantum")
        .commit();

    let capture_report =
        publish_capture(&mut parts, &mut positions, SIMULATION_PLAYBACK_PERIOD, 0.25);
    assert_eq!(capture_report.observation_publication, ObservationPublication::Published);
    let capture_read_sequence = parts.graph.capture_consumer.read_sequence();
    let playback_write_sequence = parts.graph.playback_producer.write_sequence();
    let estimator = (
        parts.graph.capture_estimator.ratio(),
        parts.graph.capture_estimator.latest_input(),
        parts.graph.capture_estimator.latest_output(),
    );
    let controller =
        (parts.graph.capture_controller.integral(), parts.graph.capture_controller.ratio());
    let graph_position = parts.graph.last_observation;
    let fixed_quantum = parts.graph.fixed_quantum;
    let pending_capture = parts
        .graph
        .capture_observation_reader
        .peek()
        .expect("capture observation is pending")
        .value();
    assert_eq!(
        pending_capture.clock(),
        parts.capture_ingress.last_observation.expect("capture history")
    );
    assert!(matches!(
        parts.playback.graph_observation_reader.peek(),
        Err(super::observation::CopySlotError::Empty)
    ));

    let mut output = OutputStorage::new();
    output.microphone_left.fill(0.75);
    output.microphone_right.fill(0.75);
    output.monitor_left.fill(0.75);
    output.monitor_right.fill(0.75);
    output.stream_left.fill(0.75);
    output.stream_right.fill(0.75);
    assert!(matches!(
        parts.graph.process(
            graph_observation(positions.graph, positions.graph_time, SIMULATION_PLAYBACK_PERIOD,),
            GraphSystemInput::unconnected(),
            output.buffers(SIMULATION_PLAYBACK_PERIOD),
        ),
        Err(BridgeError::Overflow { domain: BridgeClockDomain::WavePlayback, .. })
    ));

    assert_eq!(parts.control.state(), BridgeState::Faulted);
    assert_eq!(parts.graph.capture_consumer.read_sequence(), capture_read_sequence);
    assert_eq!(parts.graph.playback_producer.write_sequence(), playback_write_sequence);
    assert_eq!(
        (
            parts.graph.capture_estimator.ratio(),
            parts.graph.capture_estimator.latest_input(),
            parts.graph.capture_estimator.latest_output(),
        ),
        estimator
    );
    assert_eq!(
        (parts.graph.capture_controller.integral(), parts.graph.capture_controller.ratio()),
        controller
    );
    assert_eq!(parts.graph.last_observation, graph_position);
    assert_eq!(parts.graph.fixed_quantum, fixed_quantum);
    assert_eq!(
        parts
            .graph
            .capture_observation_reader
            .peek()
            .expect("capture observation remains owned by the handoff")
            .value(),
        pending_capture
    );
    assert!(matches!(
        parts.playback.graph_observation_reader.peek(),
        Err(super::observation::CopySlotError::Empty)
    ));
    for buffer in [
        &output.microphone_left,
        &output.microphone_right,
        &output.monitor_left,
        &output.monitor_right,
        &output.stream_left,
        &output.stream_right,
    ] {
        assert!(buffer.iter().all(|sample| *sample == 0.75));
    }
    assert!(matches!(
        parts.graph.process(
            graph_observation(positions.graph, positions.graph_time, SIMULATION_PLAYBACK_PERIOD,),
            GraphSystemInput::unconnected(),
            output.buffers(SIMULATION_PLAYBACK_PERIOD),
        ),
        Err(BridgeError::AttemptFaulted)
    ));
}

#[test]
fn each_supported_quantum_is_valid_per_attempt_but_changes_are_terminal() {
    for quantum in 64..=SIMULATION_MAXIMUM_GRAPH_QUANTUM {
        let mut parts = simulation_parts();
        parts.control.start_capture().expect("start capture");
        let mut positions = Positions::new();
        while parts.control.state() == BridgeState::CapturePriming {
            publish_capture(&mut parts, &mut positions, SIMULATION_CAPTURE_PERIOD, 0.0);
        }
        let mut output = OutputStorage::new();
        parts
            .graph
            .process(
                graph_observation(0, 0, quantum),
                GraphSystemInput::unconnected(),
                output.buffers(quantum),
            )
            .expect("first fixed quantum");
    }

    assert_eq!(
        GraphClockObservation::try_new(clock(0, 0), 0),
        Err(GraphClockObservationError::ZeroQuantum)
    );
}

fn run_real_asrc_drift(capture_ppm: i64, playback_ppm: i64) -> (f64, f64) {
    const BOUNDARIES: usize = 400;
    let mut parts = simulation_parts();
    let mut positions = activate(&mut parts);
    positions.capture_ppm = capture_ppm;
    positions.playback_ppm = playback_ppm;
    let mut output = OutputStorage::new();
    let mut submitter = ExactSubmitter::new(SIMULATION_PLAYBACK_PERIOD);
    for _ in 0..BOUNDARIES {
        active_cycle(&mut parts, &mut positions, 0.25, &mut output, &mut submitter);
    }
    (parts.graph.capture_controller.ratio(), parts.playback.controller.ratio())
}

#[test]
fn independent_positive_and_negative_drift_drive_both_real_asrc_bridges() {
    let (capture_ratio, playback_ratio) = run_real_asrc_drift(10_000, -8_000);
    assert!(capture_ratio < 1.0);
    assert!(playback_ratio < 1.0);

    let (capture_ratio, playback_ratio) = run_real_asrc_drift(-9_000, 10_000);
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
    const HOURS: u64 = 24;
    const QUANTUM: u64 = 1_024;
    let total_frames =
        u64::from(WAVE3_PCM_RATE_HZ).checked_mul(60 * 60 * HOURS).expect("24-hour frame ledger");
    let source_rate = PPM_SCALE + i128::from(drift_ppm);
    let measured_feed_forward = PPM_SCALE as f64 / source_rate as f64;
    let mut controller = ledger_controller();
    let mut elapsed = 0_u64;
    let mut source_total = 0_u64;
    let mut fill = SIMULATION_TARGET_FILL;
    let mut consumer_fraction = 0.0_f64;
    while elapsed < total_frames {
        let block = QUANTUM.min(total_frames - elapsed);
        elapsed = elapsed.checked_add(block).expect("graph ledger");
        let next_source_total = u64::try_from(
            i128::from(elapsed).checked_mul(source_rate).expect("source multiplication")
                / PPM_SCALE,
        )
        .expect("source ledger");
        let produced = next_source_total.checked_sub(source_total).expect("source progress");
        source_total = next_source_total;
        fill = fill
            .checked_add(usize::try_from(produced).expect("produced frames"))
            .expect("fill addition");
        let step = controller.preview(fill, measured_feed_forward).expect("controller preview");
        let exact_consumption = block as f64 / step.ratio() + consumer_fraction;
        let consumed = exact_consumption.floor() as usize;
        consumer_fraction = exact_consumption - consumed as f64;
        fill = fill.checked_sub(consumed).expect("no shortage");
        assert!(fill <= SIMULATION_FIFO_CAPACITY, "no overflow");
        step.commit();
    }
    assert!(fill.abs_diff(SIMULATION_TARGET_FILL) <= SIMULATION_STARTUP_GUARD);
    controller.ratio()
}

#[test]
fn measured_controller_ledgers_cover_24_hours_in_both_directions() {
    assert!(run_checked_ledger(275) < 1.0);
    assert!(run_checked_ledger(-325) > 1.0);
}

struct BlockingSubmitter {
    entered: mpsc::SyncSender<()>,
    release: mpsc::Receiver<()>,
}

impl PlaybackPeriodSubmitter for BlockingSubmitter {
    fn submit_packed_s24_3le(
        &mut self,
        complete_frame_count: usize,
        _bytes: &[u8],
    ) -> Result<PlaybackSubmissionProgress, PlaybackWriteError> {
        self.entered.send(()).expect("report submitter entry");
        self.release.recv().expect("release submitter");
        Ok(PlaybackSubmissionProgress { frames_submitted: complete_frame_count })
    }
}

#[test]
fn admitted_capture_boundary_finishes_during_teardown() {
    let mut parts = simulation_parts();
    parts.control.start_capture().expect("start capture priming");
    let application_start = parts.capture_ingress.worker_application_position();
    let boundary = parts.capture_ingress.record_completed_read(1).expect("admit capture boundary");

    parts.control.begin_teardown();
    assert_eq!(parts.control.state(), BridgeState::Quiescing);
    assert_eq!(
        parts.control.finish_teardown(),
        Err(BridgeError::TeardownNotQuiescent { capture: 1, graph: 0, playback: 0 })
    );

    let report = boundary
        .publish(
            CaptureClockObservation::new(clock(application_start + 1, 1)),
            &packed_mono(1, 0.25),
        )
        .expect("admitted capture publication during teardown");
    assert_eq!(report.application_start_frame, application_start);
    assert_eq!(report.application_end_frame, application_start + 1);
    assert_eq!(report.frames, 1);
    assert_eq!(report.fifo_fill_frames, 1);
    assert_eq!(parts.capture_ingress.worker_application_position(), application_start + 1);
    assert_eq!(parts.control.state(), BridgeState::Quiescing);

    parts.control.finish_teardown().expect("capture boundary quiesced");
    assert_eq!(parts.control.state(), BridgeState::Stopped);
}

#[test]
fn teardown_finishes_only_after_in_flight_worker_quiescence() {
    let mut parts = simulation_parts();
    let positions = activate(&mut parts);
    let observation = positions.playback_observation();
    let (entered_tx, entered_rx) = mpsc::sync_channel(0);
    let (release_tx, release_rx) = mpsc::sync_channel(0);
    let mut playback = parts.playback;
    let worker = thread::spawn(move || {
        let mut submitter = BlockingSubmitter { entered: entered_tx, release: release_rx };
        playback.process(observation, &mut submitter)
    });
    entered_rx.recv().expect("submitter entered");
    parts.control.begin_teardown();
    assert!(matches!(
        parts.control.finish_teardown(),
        Err(BridgeError::TeardownNotQuiescent { playback: 1, .. })
    ));
    release_tx.send(()).expect("release submitter");
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
