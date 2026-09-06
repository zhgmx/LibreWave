use super::playback::PlaybackSubmitter;
use super::*;
use crate::audio_host::clock_bridge::observation::{CopySlotError, copy_slot};
use crate::audio_host::clock_bridge::tests::primed_worker_fixture;
use crate::audio_host::clock_bridge::{
    BridgeError, BridgeState, CapturePublishReport, ClockBridgeConfig, ClockBridgeParts,
    DirectionBridgeConfig, ObservationPublication, PlaybackPeriodSubmitter, PlaybackWriteError,
};
use crate::audio_host::pcm::{
    PACKED_S24_SAMPLE_BYTES, PcmDirection, PhysicalPcmParameters, Wave3PhysicalIoConfig,
};
use alsa::pcm::State;
use librewave_core::MixerProfile;
use librewave_engine::{
    ClockAttemptEpoch, ClockDeltaBounds, ClockRateEstimatorConfig, RateMatchControllerConfig,
    RateMatchRatioBounds, RateMatcherConfig,
};
use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::collections::VecDeque;
use std::sync::{Arc, Condvar, Mutex, mpsc};

const TEST_EPOCH: u64 = 19;
const POLL_LIMIT: usize = 1_000_000;

struct CountingAllocator;

thread_local! {
    static COUNT_ALLOCATIONS: Cell<bool> = const { Cell::new(false) };
    static ALLOCATION_COUNT: Cell<usize> = const { Cell::new(0) };
}

#[global_allocator]
static ALLOCATOR: CountingAllocator = CountingAllocator;

unsafe impl GlobalAlloc for CountingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        record_allocation();
        // SAFETY: the valid layout is forwarded unchanged.
        unsafe { System.alloc(layout) }
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        record_allocation();
        // SAFETY: the valid layout is forwarded unchanged.
        unsafe { System.alloc_zeroed(layout) }
    }

    unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
        record_allocation();
        // SAFETY: the pointer and layout came from this system allocator.
        unsafe { System.dealloc(pointer, layout) };
    }

    unsafe fn realloc(&self, pointer: *mut u8, layout: Layout, size: usize) -> *mut u8 {
        record_allocation();
        // SAFETY: the allocator receives its original pointer and layout.
        unsafe { System.realloc(pointer, layout, size) }
    }
}

fn record_allocation() {
    if COUNT_ALLOCATIONS.try_with(Cell::get).unwrap_or(false) {
        let _ = ALLOCATION_COUNT.try_with(|count| count.set(count.get() + 1));
    }
}

fn count_allocations<T>(operation: impl FnOnce() -> T) -> (T, usize) {
    ALLOCATION_COUNT.with(|count| count.set(0));
    COUNT_ALLOCATIONS.with(|enabled| enabled.set(true));
    let result = operation();
    COUNT_ALLOCATIONS.with(|enabled| enabled.set(false));
    (result, ALLOCATION_COUNT.with(Cell::get))
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum FakeCall {
    CaptureStart,
    CaptureWait,
    CaptureRead { buffer_bytes: usize },
    CaptureStatus,
    PlaybackWait,
    PlaybackWrite { bytes: usize },
    PlaybackStatus,
    PlaybackStart,
    DropStream,
}

type SharedCalls = Arc<Mutex<Vec<FakeCall>>>;

struct FakeCapturePcm {
    waits: VecDeque<Result<PcmWait, PcmIoError>>,
    reads: VecDeque<Result<usize, PcmIoError>>,
    statuses: VecDeque<Result<PcmStatusSnapshot, PcmIoError>>,
    calls: SharedCalls,
}

impl CapturePcm for FakeCapturePcm {
    fn start_pcm(&mut self) -> Result<(), PcmIoError> {
        record(&self.calls, FakeCall::CaptureStart);
        Ok(())
    }

    fn wait(&mut self, _timeout_millis: u32) -> Result<PcmWait, PcmIoError> {
        record(&self.calls, FakeCall::CaptureWait);
        self.waits.pop_front().unwrap_or(Ok(PcmWait::TimedOut))
    }

    fn read_frames(&mut self, bytes: &mut [u8]) -> Result<usize, PcmIoError> {
        record(&self.calls, FakeCall::CaptureRead { buffer_bytes: bytes.len() });
        let result = self.reads.pop_front().unwrap_or(Err(PcmIoError::Failed { errno: libc::EIO }));
        if let Ok(frames) = result {
            let byte_count = frames.saturating_mul(crate::audio_host::PACKED_S24_SAMPLE_BYTES);
            for (index, byte) in bytes.iter_mut().take(byte_count).enumerate() {
                *byte = u8::try_from(index % 251).expect("bounded byte");
            }
        }
        result
    }

    fn status(&mut self) -> Result<PcmStatusSnapshot, PcmIoError> {
        record(&self.calls, FakeCall::CaptureStatus);
        self.statuses.pop_front().unwrap_or(Err(PcmIoError::Again))
    }

    fn drop_stream(&mut self) -> Result<(), PcmIoError> {
        record(&self.calls, FakeCall::DropStream);
        Ok(())
    }
}

struct FakePlaybackPcm {
    waits: VecDeque<Result<PcmWait, PcmIoError>>,
    writes: VecDeque<Result<usize, PcmIoError>>,
    statuses: VecDeque<Result<PcmStatusSnapshot, PcmIoError>>,
    calls: SharedCalls,
}

struct ResumeGate {
    released: Mutex<bool>,
    condition: Condvar,
}

struct ActivePlaybackPcm {
    first_status: Option<PcmStatusSnapshot>,
    first_wait_ready: bool,
    gate: Arc<ResumeGate>,
    calls: SharedCalls,
}

struct TeardownCapturePcm {
    entered: mpsc::SyncSender<()>,
    release: mpsc::Receiver<()>,
    calls: SharedCalls,
}

struct TeardownPlaybackPcm {
    entered: mpsc::SyncSender<()>,
    release: mpsc::Receiver<()>,
    snapshot: PcmStatusSnapshot,
    calls: SharedCalls,
}

impl PlaybackPcm for FakePlaybackPcm {
    fn wait(&mut self, _timeout_millis: u32) -> Result<PcmWait, PcmIoError> {
        record(&self.calls, FakeCall::PlaybackWait);
        self.waits.pop_front().unwrap_or(Ok(PcmWait::TimedOut))
    }

    fn write_frames(&mut self, bytes: &[u8]) -> Result<usize, PcmIoError> {
        record(&self.calls, FakeCall::PlaybackWrite { bytes: bytes.len() });
        self.writes.pop_front().unwrap_or(Err(PcmIoError::Failed { errno: libc::EIO }))
    }

    fn status(&mut self) -> Result<PcmStatusSnapshot, PcmIoError> {
        record(&self.calls, FakeCall::PlaybackStatus);
        self.statuses.pop_front().unwrap_or(Err(PcmIoError::Disconnected))
    }

    fn start_pcm(&mut self) -> Result<(), PcmIoError> {
        record(&self.calls, FakeCall::PlaybackStart);
        Ok(())
    }

    fn drop_stream(&mut self) -> Result<(), PcmIoError> {
        record(&self.calls, FakeCall::DropStream);
        Ok(())
    }
}

impl PlaybackPcm for ActivePlaybackPcm {
    fn wait(&mut self, _timeout_millis: u32) -> Result<PcmWait, PcmIoError> {
        record(&self.calls, FakeCall::PlaybackWait);
        if self.first_wait_ready {
            self.first_wait_ready = false;
            return Ok(PcmWait::Ready);
        }
        let mut released = self.gate.released.lock().expect("active playback gate");
        while !*released {
            released = self.gate.condition.wait(released).expect("active playback release");
        }
        Ok(PcmWait::TimedOut)
    }

    fn write_frames(&mut self, bytes: &[u8]) -> Result<usize, PcmIoError> {
        record(&self.calls, FakeCall::PlaybackWrite { bytes: bytes.len() });
        Ok(bytes.len() / (2 * crate::audio_host::PACKED_S24_SAMPLE_BYTES))
    }

    fn status(&mut self) -> Result<PcmStatusSnapshot, PcmIoError> {
        record(&self.calls, FakeCall::PlaybackStatus);
        self.first_status.take().ok_or(PcmIoError::Again)
    }

    fn start_pcm(&mut self) -> Result<(), PcmIoError> {
        record(&self.calls, FakeCall::PlaybackStart);
        Ok(())
    }

    fn drop_stream(&mut self) -> Result<(), PcmIoError> {
        record(&self.calls, FakeCall::DropStream);
        Ok(())
    }
}

impl CapturePcm for TeardownCapturePcm {
    fn start_pcm(&mut self) -> Result<(), PcmIoError> {
        record(&self.calls, FakeCall::CaptureStart);
        Ok(())
    }

    fn wait(&mut self, _timeout_millis: u32) -> Result<PcmWait, PcmIoError> {
        record(&self.calls, FakeCall::CaptureWait);
        Ok(PcmWait::Ready)
    }

    fn read_frames(&mut self, bytes: &mut [u8]) -> Result<usize, PcmIoError> {
        record(&self.calls, FakeCall::CaptureRead { buffer_bytes: bytes.len() });
        self.entered.send(()).expect("report capture read entry");
        self.release.recv().expect("release capture read");
        bytes[..PACKED_S24_SAMPLE_BYTES].copy_from_slice(&[1, 2, 3]);
        Ok(1)
    }

    fn status(&mut self) -> Result<PcmStatusSnapshot, PcmIoError> {
        record(&self.calls, FakeCall::CaptureStatus);
        Err(PcmIoError::Failed { errno: libc::EIO })
    }

    fn drop_stream(&mut self) -> Result<(), PcmIoError> {
        record(&self.calls, FakeCall::DropStream);
        Ok(())
    }
}

impl PlaybackPcm for TeardownPlaybackPcm {
    fn wait(&mut self, _timeout_millis: u32) -> Result<PcmWait, PcmIoError> {
        record(&self.calls, FakeCall::PlaybackWait);
        Ok(PcmWait::Ready)
    }

    fn write_frames(&mut self, bytes: &[u8]) -> Result<usize, PcmIoError> {
        record(&self.calls, FakeCall::PlaybackWrite { bytes: bytes.len() });
        Err(PcmIoError::Failed { errno: libc::EIO })
    }

    fn status(&mut self) -> Result<PcmStatusSnapshot, PcmIoError> {
        record(&self.calls, FakeCall::PlaybackStatus);
        self.entered.send(()).expect("report playback status entry");
        self.release.recv().expect("release playback status");
        Ok(self.snapshot)
    }

    fn start_pcm(&mut self) -> Result<(), PcmIoError> {
        record(&self.calls, FakeCall::PlaybackStart);
        Err(PcmIoError::Failed { errno: libc::EIO })
    }

    fn drop_stream(&mut self) -> Result<(), PcmIoError> {
        record(&self.calls, FakeCall::DropStream);
        Ok(())
    }
}

fn record(calls: &SharedCalls, call: FakeCall) {
    calls.lock().expect("fake call log").push(call);
}

fn calls() -> SharedCalls {
    Arc::new(Mutex::new(Vec::new()))
}

fn policy(retry_limit: u32) -> WorkerIoPolicy {
    WorkerIoPolicy::try_new(1, retry_limit).expect("worker policy")
}

fn epoch() -> ClockAttemptEpoch {
    ClockAttemptEpoch::try_new(TEST_EPOCH).expect("test epoch")
}

fn physical() -> Wave3PhysicalIoConfig {
    Wave3PhysicalIoConfig::try_new(5, 32, 3, 24).expect("test physical geometry")
}

fn capture_geometry() -> PhysicalPcmParameters {
    physical().parameters(PcmDirection::Capture)
}

fn playback_geometry() -> PhysicalPcmParameters {
    physical().parameters(PcmDirection::Playback)
}

fn capture_status(
    seconds: i64,
    nanoseconds: i64,
    available_frames: i64,
    delay_frames: i64,
) -> PcmStatusSnapshot {
    PcmStatusSnapshot {
        timestamp_seconds: seconds,
        timestamp_nanoseconds: nanoseconds,
        available_frames,
        delay_frames,
        state_raw: State::Running as i32,
        geometry: capture_geometry(),
    }
}

fn playback_status(
    seconds: i64,
    nanoseconds: i64,
    available_frames: i64,
    delay_frames: i64,
    state: State,
) -> PcmStatusSnapshot {
    PcmStatusSnapshot {
        timestamp_seconds: seconds,
        timestamp_nanoseconds: nanoseconds,
        available_frames,
        delay_frames,
        state_raw: state as i32,
        geometry: playback_geometry(),
    }
}

fn direction(channels: usize, maximum_output_frames: usize) -> DirectionBridgeConfig {
    let bounds = RateMatchRatioBounds::try_new(2.0, 0.98, 1.02).expect("ratio bounds");
    let delta =
        ClockDeltaBounds::try_new(1, 1_000_000, 1, 60_000_000_000).expect("clock delta bounds");
    let estimator = ClockRateEstimatorConfig::new(delta, delta, 2_000_000_000, bounds);
    let matcher = RateMatcherConfig::try_new(maximum_output_frames, channels, bounds)
        .expect("rate matcher config");
    let controller =
        RateMatchControllerConfig::try_new(256, 0.002, 0.000_002, 10_000.0, 0.000_05, bounds)
            .expect("rate controller config");
    DirectionBridgeConfig::try_new(512, 32, matcher, controller, estimator)
        .expect("direction bridge config")
}

fn bridge_parts() -> ClockBridgeParts {
    let config = ClockBridgeConfig::try_new(epoch(), 4, direction(1, 4), direction(2, 3))
        .expect("bridge config");
    ClockBridgeParts::new(physical(), &MixerProfile::default(), config).expect("bridge parts")
}

fn next_capture_progress(worker: &mut CaptureWorker) -> CaptureWorkerProgress {
    for _ in 0..POLL_LIMIT {
        if let Some(progress) = worker.try_progress() {
            return progress;
        }
        assert!(!worker.is_finished(), "capture worker ended before reporting progress");
        std::thread::yield_now();
    }
    panic!("capture worker did not report progress")
}

fn wait_for_capture_finish(worker: &CaptureWorker) {
    for _ in 0..POLL_LIMIT {
        if worker.is_finished() {
            return;
        }
        std::thread::yield_now();
    }
    panic!("capture worker did not finish")
}

fn wait_for_capture_phase(worker: &CaptureWorker, expected: WorkerPhase) {
    for _ in 0..POLL_LIMIT {
        if worker.phase() == expected {
            return;
        }
        assert!(!worker.is_finished(), "capture worker ended before {expected:?}");
        std::thread::yield_now();
    }
    panic!("capture worker did not reach {expected:?}")
}

fn next_playback_boundary(worker: &mut super::playback::PlaybackWorker) -> PlaybackWorkerBoundary {
    for _ in 0..POLL_LIMIT {
        if let Some(boundary) = worker.try_boundary() {
            return boundary;
        }
        assert!(!worker.is_finished(), "playback worker ended before reporting a boundary");
        std::thread::yield_now();
    }
    panic!("playback worker did not report a boundary")
}

fn wait_for_playback_phase(worker: &super::playback::PlaybackWorker, expected: WorkerPhase) {
    for _ in 0..POLL_LIMIT {
        if worker.phase() == expected {
            return;
        }
        assert!(!worker.is_finished(), "playback worker ended before {expected:?}");
        std::thread::yield_now();
    }
    panic!("playback worker did not reach {expected:?}")
}

fn wait_for_playback_finish(worker: &super::playback::PlaybackWorker) {
    for _ in 0..POLL_LIMIT {
        if worker.is_finished() {
            return;
        }
        std::thread::yield_now();
    }
    panic!("playback worker did not finish")
}

#[test]
fn worker_progress_slot_is_nonallocating_and_drops_newest_while_full() {
    let first = CaptureWorkerProgress {
        report: CapturePublishReport {
            application_start_frame: 0,
            application_end_frame: 5,
            hardware_frame_position: 7,
            frames: 5,
            fifo_fill_frames: 5,
            observation_publication: ObservationPublication::Published,
        },
        delay_frames: -3,
    };
    let second = CaptureWorkerProgress { delay_frames: 4, ..first };
    let (mut publisher, mut reader) = copy_slot();

    let ((), allocations) = count_allocations(|| {
        assert_eq!(publisher.try_publish(first), Ok(()));
        assert_eq!(publisher.try_publish(second), Err(CopySlotError::Full));
        assert_eq!(reader.try_take(), Ok(first));
        assert_eq!(reader.try_take(), Err(CopySlotError::Empty));
    });
    assert_eq!(allocations, 0);
}

#[test]
fn status_validation_uses_exact_formulas_and_retains_signed_delay() {
    let capture =
        validate_capture_status(epoch(), 7, capture_geometry(), capture_status(4, 25, 3, -9))
            .expect("valid capture status");
    assert_eq!(capture.observation.clock().frame_position().get(), 10);
    assert_eq!(capture.observation.clock().monotonic_time().get(), 4_000_000_025);
    assert_eq!(capture.available_frames, 3);
    assert_eq!(capture.delay_frames, -9);

    let playback = validate_playback_status(
        epoch(),
        20,
        playback_geometry(),
        true,
        playback_status(5, 30, 9, -11, State::Running),
    )
    .expect("valid playback status");
    assert_eq!(playback.queued_frames, 15);
    assert_eq!(playback.observation.hardware().frame_position().get(), 5);
    assert_eq!(playback.observation.application_frame_position().get(), 20);
    assert_eq!(playback.delay_frames, -11);
}

#[test]
fn status_validation_rejects_invalid_timestamps_and_overflow() {
    assert!(matches!(
        validate_capture_status(epoch(), 0, capture_geometry(), capture_status(-1, 0, 0, 0)),
        Err(PcmStatusError::NegativeTimestampSeconds { actual: -1 })
    ));
    for nanoseconds in [-1, 1_000_000_000] {
        assert!(matches!(
            validate_capture_status(
                epoch(),
                0,
                capture_geometry(),
                capture_status(0, nanoseconds, 0, 0),
            ),
            Err(PcmStatusError::InvalidTimestampNanoseconds { actual }) if actual == nanoseconds
        ));
    }
    assert_eq!(
        validate_capture_status(
            epoch(),
            0,
            capture_geometry(),
            capture_status(i64::MAX, 999_999_999, 0, 0),
        ),
        Err(PcmStatusError::TimestampOverflow)
    );
}

#[test]
fn status_validation_rejects_bad_geometry_availability_position_and_state() {
    let wrong_geometry = playback_status(0, 0, 24, 0, State::Prepared);
    assert!(matches!(
        validate_capture_status(epoch(), 0, capture_geometry(), wrong_geometry),
        Err(PcmStatusError::Geometry { .. })
    ));
    for availability in [-1, 33] {
        assert!(
            validate_capture_status(
                epoch(),
                0,
                capture_geometry(),
                capture_status(0, 0, availability, 0),
            )
            .is_err()
        );
    }
    assert_eq!(
        validate_capture_status(epoch(), u64::MAX, capture_geometry(), capture_status(0, 0, 1, 0),),
        Err(PcmStatusError::PositionOverflow)
    );
    assert!(matches!(
        validate_playback_status(
            epoch(),
            2,
            playback_geometry(),
            false,
            playback_status(0, 0, 0, 0, State::Prepared),
        ),
        Err(PcmStatusError::PlaybackPositionBeforeQueue { application: 2, queued: 24 })
    ));

    for (state, expected) in [
        (State::XRun, PcmStatusError::Xrun),
        (State::Suspended, PcmStatusError::Suspended),
        (State::Disconnected, PcmStatusError::Disconnected),
    ] {
        assert_eq!(
            validate_capture_status(
                epoch(),
                0,
                capture_geometry(),
                PcmStatusSnapshot { state_raw: state as i32, ..capture_status(0, 0, 0, 0) },
            ),
            Err(expected)
        );
    }
    assert!(matches!(
        validate_playback_status(
            epoch(),
            0,
            playback_geometry(),
            false,
            playback_status(0, 0, 24, 0, State::Running),
        ),
        Err(PcmStatusError::InvalidState { expected: PcmExpectedState::PlaybackPrepared, .. })
    ));
}

#[test]
fn capture_worker_publishes_only_the_partial_read_and_stops_after_timeouts() {
    let mut parts = bridge_parts();
    parts.control.start_capture().expect("capture-first bridge start");
    let calls = calls();
    let fake = FakeCapturePcm {
        waits: VecDeque::from([
            Ok(PcmWait::TimedOut),
            Err(PcmIoError::Again),
            Err(PcmIoError::Interrupted),
            Ok(PcmWait::Ready),
        ]),
        reads: VecDeque::from([Ok(3)]),
        statuses: VecDeque::from([Ok(capture_status(1, 7, 2, -4))]),
        calls: Arc::clone(&calls),
    };
    let mut worker =
        CaptureWorker::start(Box::new(fake), parts.capture_ingress, capture_geometry(), policy(3))
            .expect("capture worker");
    let CaptureWorkerProgress { report, delay_frames } = next_capture_progress(&mut worker);
    assert_eq!(report.application_start_frame, 0);
    assert_eq!(report.application_end_frame, 3);
    assert_eq!(report.hardware_frame_position, 5);
    assert_eq!(report.frames, 3);
    assert_eq!(delay_frames, -4);
    worker.request_stop();
    let shutdown = worker.park_and_join().expect("capture worker join");
    assert_eq!(shutdown.terminal, CaptureWorkerTerminal::Stopped);
    assert_eq!(shutdown.drop_stream_result, Ok(()));
    assert_eq!(shutdown.ingress.worker_application_position(), 3);

    let calls = calls.lock().expect("fake call log");
    assert_eq!(calls.first(), Some(&FakeCall::CaptureStart));
    assert!(calls.contains(&FakeCall::CaptureRead { buffer_bytes: 15 }));
    assert_eq!(calls.last(), Some(&FakeCall::DropStream));
}

#[test]
fn capture_worker_transient_retry_limits_are_terminal() {
    for (error, expected) in [
        (PcmIoError::Again, CaptureWorkerTerminal::AgainRetryLimit),
        (PcmIoError::Interrupted, CaptureWorkerTerminal::InterruptedRetryLimit),
    ] {
        let mut parts = bridge_parts();
        parts.control.start_capture().expect("capture-first bridge start");
        let fake = FakeCapturePcm {
            waits: VecDeque::from([
                Err(error),
                Ok(PcmWait::TimedOut),
                Err(error),
                Ok(PcmWait::TimedOut),
                Err(error),
            ]),
            reads: VecDeque::new(),
            statuses: VecDeque::new(),
            calls: calls(),
        };
        let worker = CaptureWorker::start(
            Box::new(fake),
            parts.capture_ingress,
            capture_geometry(),
            policy(2),
        )
        .expect("capture worker");
        wait_for_capture_finish(&worker);
        let shutdown = worker.park_and_join().expect("capture worker join");
        assert_eq!(shutdown.terminal, expected);
        assert_eq!(shutdown.drop_stream_result, Ok(()));
        assert_eq!(parts.control.state(), BridgeState::Faulted);
    }
}

#[test]
fn completed_capture_cycle_resets_status_transients_before_the_next_wait() {
    let mut parts = bridge_parts();
    parts.control.start_capture().expect("capture-first bridge start");
    let fake = FakeCapturePcm {
        waits: VecDeque::from([
            Ok(PcmWait::Ready),
            Ok(PcmWait::Ready),
            Err(PcmIoError::Again),
            Ok(PcmWait::Ready),
            Ok(PcmWait::Ready),
        ]),
        reads: VecDeque::from([Ok(1), Ok(1), Err(PcmIoError::Xrun)]),
        statuses: VecDeque::from([
            Err(PcmIoError::Again),
            Ok(capture_status(1, 0, 0, 0)),
            Ok(capture_status(2, 0, 0, 0)),
        ]),
        calls: calls(),
    };
    let worker =
        CaptureWorker::start(Box::new(fake), parts.capture_ingress, capture_geometry(), policy(1))
            .expect("capture worker");
    wait_for_capture_finish(&worker);
    let shutdown = worker.park_and_join().expect("capture worker join");
    assert_eq!(shutdown.terminal, CaptureWorkerTerminal::Xrun);
    assert_eq!(shutdown.ingress.worker_application_position(), 2);
}

#[test]
fn capture_worker_classifies_pcm_terminal_failures() {
    for (error, expected) in [
        (PcmIoError::Xrun, CaptureWorkerTerminal::Xrun),
        (PcmIoError::Suspended, CaptureWorkerTerminal::Suspended),
        (PcmIoError::Disconnected, CaptureWorkerTerminal::Disconnected),
        (
            PcmIoError::Failed { errno: libc::EIO },
            CaptureWorkerTerminal::PcmFailed { errno: libc::EIO },
        ),
    ] {
        let mut parts = bridge_parts();
        parts.control.start_capture().expect("capture-first bridge start");
        let fake = FakeCapturePcm {
            waits: VecDeque::from([Ok(PcmWait::Ready)]),
            reads: VecDeque::from([Err(error)]),
            statuses: VecDeque::new(),
            calls: calls(),
        };
        let worker = CaptureWorker::start(
            Box::new(fake),
            parts.capture_ingress,
            capture_geometry(),
            policy(2),
        )
        .expect("capture worker");
        wait_for_capture_finish(&worker);
        assert_eq!(worker.park_and_join().expect("capture worker join").terminal, expected);
        assert_eq!(parts.control.state(), BridgeState::Faulted);
    }
}

#[test]
fn capture_worker_detects_timestamp_regression_after_a_completed_read() {
    let mut parts = bridge_parts();
    parts.control.start_capture().expect("capture-first bridge start");
    let fake = FakeCapturePcm {
        waits: VecDeque::from([Ok(PcmWait::Ready), Ok(PcmWait::Ready)]),
        reads: VecDeque::from([Ok(1), Ok(1)]),
        statuses: VecDeque::from([
            Ok(capture_status(2, 0, 0, 0)),
            Ok(capture_status(1, 999_999_999, 0, 0)),
        ]),
        calls: calls(),
    };
    let mut worker =
        CaptureWorker::start(Box::new(fake), parts.capture_ingress, capture_geometry(), policy(2))
            .expect("capture worker");
    let _ = next_capture_progress(&mut worker);
    wait_for_capture_finish(&worker);
    let shutdown = worker.park_and_join().expect("capture worker join");
    assert!(matches!(
        shutdown.terminal,
        CaptureWorkerTerminal::Bridge(BridgeError::MonotonicTimeDiscontinuity { .. })
    ));
    assert_eq!(shutdown.ingress.worker_application_position(), 2);
    assert_eq!(parts.control.state(), BridgeState::Faulted);
}

#[test]
fn capture_worker_shutdown_is_cancellable_without_a_sleep() {
    let mut parts = bridge_parts();
    parts.control.start_capture().expect("capture-first bridge start");
    let fake = FakeCapturePcm {
        waits: VecDeque::new(),
        reads: VecDeque::new(),
        statuses: VecDeque::new(),
        calls: calls(),
    };
    let worker =
        CaptureWorker::start(Box::new(fake), parts.capture_ingress, capture_geometry(), policy(2))
            .expect("capture worker");
    worker.request_stop();
    let shutdown = worker.park_and_join().expect("capture worker join");
    assert_eq!(shutdown.terminal, CaptureWorkerTerminal::Stopped);
    assert_eq!(shutdown.drop_stream_result, Ok(()));
    assert_ne!(parts.control.state(), BridgeState::Faulted);
}

#[test]
fn capture_worker_stops_normally_when_teardown_refuses_a_new_boundary() {
    let mut parts = bridge_parts();
    parts.control.start_capture().expect("capture-first bridge start");
    let calls = calls();
    let (entered_tx, entered_rx) = mpsc::sync_channel(0);
    let (release_tx, release_rx) = mpsc::sync_channel(0);
    let fake =
        TeardownCapturePcm { entered: entered_tx, release: release_rx, calls: Arc::clone(&calls) };
    let worker =
        CaptureWorker::start(Box::new(fake), parts.capture_ingress, capture_geometry(), policy(2))
            .expect("capture worker");

    entered_rx.recv().expect("capture read entered");
    parts.control.begin_teardown();
    assert_eq!(parts.control.state(), BridgeState::Quiescing);
    release_tx.send(()).expect("release completed capture read");
    wait_for_capture_finish(&worker);

    let shutdown = worker.park_and_join().expect("capture worker join");
    assert_eq!(shutdown.terminal, CaptureWorkerTerminal::Stopped);
    assert_eq!(shutdown.ingress.worker_application_position(), 0);
    assert_eq!(shutdown.drop_stream_result, Ok(()));
    assert_eq!(
        *calls.lock().expect("capture calls"),
        [
            FakeCall::CaptureStart,
            FakeCall::CaptureWait,
            FakeCall::CaptureRead { buffer_bytes: 15 },
            FakeCall::DropStream,
        ]
    );
    assert_eq!(parts.control.state(), BridgeState::Quiescing);
    parts.control.finish_teardown().expect("capture worker quiesced");
    assert_eq!(parts.control.state(), BridgeState::Stopped);
}

#[test]
fn playback_worker_stops_normally_when_teardown_refuses_a_new_boundary() {
    let fixture = primed_worker_fixture();
    let application_position = fixture.parts.playback.worker_application_position();
    let calls = calls();
    let (entered_tx, entered_rx) = mpsc::sync_channel(0);
    let (release_tx, release_rx) = mpsc::sync_channel(0);
    let snapshot = PcmStatusSnapshot {
        timestamp_seconds: i64::try_from(fixture.playback_time / 1_000_000_000)
            .expect("playback seconds"),
        timestamp_nanoseconds: i64::try_from(fixture.playback_time % 1_000_000_000)
            .expect("playback nanoseconds"),
        available_frames: i64::from(fixture.playback_geometry.buffer_frames),
        delay_frames: 0,
        state_raw: State::Prepared as i32,
        geometry: fixture.playback_geometry,
    };
    let fake = TeardownPlaybackPcm {
        entered: entered_tx,
        release: release_rx,
        snapshot,
        calls: Arc::clone(&calls),
    };
    let ClockBridgeParts { capture_ingress: _, graph: _, playback, mut control, meters: _ } =
        fixture.parts;
    let parked =
        ParkedPlaybackWorker::new(Box::new(fake), playback, fixture.playback_geometry, policy(2))
            .expect("parked playback worker");
    let worker =
        parked.activate_thread_after_monitor_mix_queued().expect("playback thread activation");
    wait_for_playback_phase(&worker, WorkerPhase::PrimedParked);
    control.activate().expect("activate quiescent bridge");
    worker.resume_after_activation(&control).expect("resume playback worker");

    entered_rx.recv().expect("playback status entered");
    control.begin_teardown();
    assert_eq!(control.state(), BridgeState::Quiescing);
    release_tx.send(()).expect("release playback status");
    wait_for_playback_finish(&worker);

    let shutdown = worker.park_and_join().expect("playback worker join");
    assert_eq!(shutdown.terminal, PlaybackWorkerTerminal::Stopped);
    assert_eq!(shutdown.egress.worker_application_position(), application_position);
    assert_eq!(shutdown.drop_stream_result, Ok(()));
    assert_eq!(
        *calls.lock().expect("playback calls"),
        [FakeCall::PlaybackStatus, FakeCall::DropStream]
    );
    assert_eq!(control.state(), BridgeState::Quiescing);
    control.finish_teardown().expect("playback worker quiesced");
    assert_eq!(control.state(), BridgeState::Stopped);
}

#[test]
#[allow(clippy::too_many_lines)]
fn primed_workers_park_activate_resume_and_continue_active_io() {
    let fixture = primed_worker_fixture();
    let capture_application = fixture.parts.capture_ingress.worker_application_position();
    let playback_application = fixture.parts.playback.worker_application_position();
    let capture_nanoseconds = fixture.capture_time + 1_000_000;
    let playback_nanoseconds = fixture.playback_time;
    let capture_calls = calls();
    let playback_calls = calls();
    let capture_pcm = FakeCapturePcm {
        waits: VecDeque::from([Ok(PcmWait::Ready)]),
        reads: VecDeque::from([Ok(5)]),
        statuses: VecDeque::from([Ok(PcmStatusSnapshot {
            timestamp_seconds: i64::try_from(capture_nanoseconds / 1_000_000_000)
                .expect("capture seconds"),
            timestamp_nanoseconds: i64::try_from(capture_nanoseconds % 1_000_000_000)
                .expect("capture nanoseconds"),
            available_frames: 0,
            delay_frames: -7,
            state_raw: State::Running as i32,
            geometry: fixture.capture_geometry,
        })]),
        calls: Arc::clone(&capture_calls),
    };
    let playback_gate =
        Arc::new(ResumeGate { released: Mutex::new(false), condition: Condvar::new() });
    let playback_pcm = ActivePlaybackPcm {
        first_status: Some(PcmStatusSnapshot {
            timestamp_seconds: i64::try_from(playback_nanoseconds / 1_000_000_000)
                .expect("playback seconds"),
            timestamp_nanoseconds: i64::try_from(playback_nanoseconds % 1_000_000_000)
                .expect("playback nanoseconds"),
            available_frames: i64::from(fixture.playback_geometry.buffer_frames),
            delay_frames: -9,
            state_raw: State::Prepared as i32,
            geometry: fixture.playback_geometry,
        }),
        first_wait_ready: true,
        gate: Arc::clone(&playback_gate),
        calls: Arc::clone(&playback_calls),
    };
    let ClockBridgeParts { capture_ingress, graph: _graph, playback, mut control, meters: _meters } =
        fixture.parts;
    let mut capture = CaptureWorker::start(
        Box::new(capture_pcm),
        capture_ingress,
        fixture.capture_geometry,
        policy(2),
    )
    .expect("capture worker");
    let parked_playback = ParkedPlaybackWorker::new(
        Box::new(playback_pcm),
        playback,
        fixture.playback_geometry,
        policy(2),
    )
    .expect("parked playback worker");
    let mut playback = parked_playback
        .activate_thread_after_monitor_mix_queued()
        .expect("playback thread activation");

    wait_for_capture_phase(&capture, WorkerPhase::PrimedParked);
    wait_for_playback_phase(&playback, WorkerPhase::PrimedParked);
    assert_eq!(*capture_calls.lock().expect("capture calls"), [FakeCall::CaptureStart]);
    assert!(playback_calls.lock().expect("playback calls").is_empty());
    assert_eq!(
        capture.resume_after_activation(&control),
        Err(WorkerResumeError::BridgeNotActive { actual: BridgeState::Primed })
    );
    assert_eq!(
        playback.resume_after_activation(&control),
        Err(WorkerResumeError::BridgeNotActive { actual: BridgeState::Primed })
    );

    control.activate().expect("quiescent bridge activation");
    assert_eq!(capture.phase(), WorkerPhase::PrimedParked);
    assert_eq!(playback.phase(), WorkerPhase::PrimedParked);
    assert_eq!(*capture_calls.lock().expect("capture calls"), [FakeCall::CaptureStart]);
    assert!(playback_calls.lock().expect("playback calls").is_empty());
    capture.resume_after_activation(&control).expect("resume capture after activation");
    playback.resume_after_activation(&control).expect("resume playback after activation");

    let capture_progress = next_capture_progress(&mut capture);
    assert_eq!(capture_progress.report.application_start_frame, capture_application);
    assert_eq!(capture_progress.report.application_end_frame, capture_application + 5);
    assert_eq!(capture_progress.delay_frames, -7);
    let playback_boundary = next_playback_boundary(&mut playback);
    assert_eq!(
        playback_boundary.report.delivery,
        crate::audio_host::clock_bridge::PlaybackBoundaryDelivery::SubmittedActive
    );
    assert_eq!(playback_boundary.report.application_start_frame, playback_application);
    assert!(playback_boundary.pcm_started);
    assert_eq!(playback_boundary.delay_frames, -9);

    capture.request_stop();
    playback.request_stop();
    {
        let mut released = playback_gate.released.lock().expect("playback release");
        *released = true;
        playback_gate.condition.notify_all();
    }
    let capture_shutdown = capture.park_and_join().expect("capture worker join");
    let playback_shutdown = playback.park_and_join().expect("playback worker join");
    assert_eq!(capture_shutdown.terminal, CaptureWorkerTerminal::Stopped);
    assert_eq!(playback_shutdown.terminal, PlaybackWorkerTerminal::Stopped);
    assert_eq!(capture_shutdown.drop_stream_result, Ok(()));
    assert_eq!(playback_shutdown.drop_stream_result, Ok(()));
    control.begin_teardown();
    control.finish_teardown().expect("workers quiesced before teardown");
    assert_eq!(control.state(), BridgeState::Stopped);
}

fn submitter_with(
    waits: impl IntoIterator<Item = Result<PcmWait, PcmIoError>>,
    writes: impl IntoIterator<Item = Result<usize, PcmIoError>>,
    retry_limit: u32,
) -> (PlaybackSubmitter, SharedCalls) {
    let calls = calls();
    let fake = FakePlaybackPcm {
        waits: waits.into_iter().collect(),
        writes: writes.into_iter().collect(),
        statuses: VecDeque::new(),
        calls: Arc::clone(&calls),
    };
    (PlaybackSubmitter::new(Box::new(fake), policy(retry_limit), epoch()), calls)
}

#[test]
fn playback_submitter_completes_partial_writes_with_exact_offsets() {
    let (mut submitter, calls) = submitter_with(
        [Ok(PcmWait::TimedOut), Ok(PcmWait::Ready), Ok(PcmWait::Ready), Ok(PcmWait::Ready)],
        [Ok(2), Err(PcmIoError::Again), Ok(3)],
        2,
    );
    let bytes = [0_u8; 5 * 2 * 3];
    let progress =
        submitter.submit_packed_s24_3le(5, &bytes).expect("complete partial playback writes");
    assert_eq!(progress.frames_submitted, 5);
    let write_lengths = calls
        .lock()
        .expect("fake call log")
        .iter()
        .filter_map(|call| match call {
            FakeCall::PlaybackWrite { bytes } => Some(*bytes),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(write_lengths, [30, 18, 18]);
}

#[test]
fn playback_submitter_bounds_transient_retries() {
    for (error, expected) in [
        (PcmIoError::Again, PlaybackWriteError::AgainRetryLimit { accepted_frames: 0 }),
        (PcmIoError::Interrupted, PlaybackWriteError::InterruptedRetryLimit { accepted_frames: 0 }),
    ] {
        let (mut submitter, _) = submitter_with(
            [Err(error), Ok(PcmWait::TimedOut), Err(error), Ok(PcmWait::TimedOut), Err(error)],
            [],
            2,
        );
        assert_eq!(submitter.submit_packed_s24_3le(1, &[0; 6]), Err(expected));
    }
}

#[test]
fn playback_submitter_reports_partial_progress_on_terminal_failure() {
    for (error, expected) in [
        (PcmIoError::Xrun, PlaybackWriteError::Xrun { accepted_frames: 2 }),
        (PcmIoError::Suspended, PlaybackWriteError::Suspended { accepted_frames: 2 }),
        (PcmIoError::Disconnected, PlaybackWriteError::Disconnected { accepted_frames: 2 }),
        (
            PcmIoError::Failed { errno: libc::EIO },
            PlaybackWriteError::Failed { accepted_frames: 2, errno: libc::EIO },
        ),
    ] {
        let (mut submitter, _) =
            submitter_with([Ok(PcmWait::Ready), Ok(PcmWait::Ready)], [Ok(2), Err(error)], 2);
        assert_eq!(submitter.submit_packed_s24_3le(3, &[0; 18]), Err(expected));
    }
}

#[test]
fn parked_playback_refuses_early_activation_and_drops_its_only_stream() {
    let parts = bridge_parts();
    let calls = calls();
    let fake = FakePlaybackPcm {
        waits: VecDeque::new(),
        writes: VecDeque::new(),
        statuses: VecDeque::new(),
        calls: Arc::clone(&calls),
    };
    let parked =
        ParkedPlaybackWorker::new(Box::new(fake), parts.playback, playback_geometry(), policy(2))
            .expect("parked playback worker");
    assert!(matches!(
        parked.activate_thread_after_monitor_mix_queued(),
        Err(WorkerStartError::MonitorMixNotQueued { drop_stream_result: Ok(()) })
    ));
    assert_eq!(*calls.lock().expect("fake call log"), [FakeCall::DropStream]);
}

#[test]
fn parked_playback_teardown_drops_stream_without_starting_the_pcm() {
    let parts = bridge_parts();
    let calls = calls();
    let fake = FakePlaybackPcm {
        waits: VecDeque::new(),
        writes: VecDeque::new(),
        statuses: VecDeque::new(),
        calls: Arc::clone(&calls),
    };
    let parked =
        ParkedPlaybackWorker::new(Box::new(fake), parts.playback, playback_geometry(), policy(2))
            .expect("parked playback worker");
    let shutdown = parked.park_and_drop_stream();
    assert_eq!(shutdown.drop_stream_result, Ok(()));
    assert_eq!(shutdown.egress.worker_application_position(), 0);
    assert_eq!(*calls.lock().expect("fake call log"), [FakeCall::DropStream]);
}

#[test]
fn worker_policy_rejects_invalid_wait_and_retry_limits() {
    assert_eq!(WorkerIoPolicy::try_new(0, 1), Err(WorkerIoPolicyError::ZeroWaitTimeout));
    assert!(WorkerIoPolicy::try_new(i32::MAX as u32, 1).is_ok());
    assert_eq!(
        WorkerIoPolicy::try_new(i32::MAX as u32 + 1, 1),
        Err(WorkerIoPolicyError::WaitTimeoutOutsideCInt {
            actual: i32::MAX as u32 + 1,
            maximum: i32::MAX as u32,
        })
    );
    assert_eq!(WorkerIoPolicy::try_new(1, 0), Err(WorkerIoPolicyError::ZeroTransientRetryLimit));
}

#[test]
fn transient_budget_exhausts_at_u32_max_without_saturating_forever() {
    let mut budget = TransientBudget { attempts: u32::MAX - 1, limit: u32::MAX };
    assert_eq!(budget.record(PcmIoError::Again), Ok(()));
    assert_eq!(budget.attempts, u32::MAX);
    assert_eq!(budget.record(PcmIoError::Again), Err(TransientKind::Again));
}
