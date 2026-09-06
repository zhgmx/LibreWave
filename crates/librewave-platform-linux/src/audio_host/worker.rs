//! Direction-specific ALSA workers and checked runtime observations.

use super::clock_bridge::{BridgeState, ClockBridgeControl};
use super::pcm::PhysicalPcmParameters;
use librewave_engine::ClockAttemptEpoch;
use std::fmt;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};

mod capture;
mod playback;
mod status;

#[cfg(test)]
mod tests;

#[cfg(test)]
pub(super) use capture::{CaptureWorker, CaptureWorkerProgress, CaptureWorkerTerminal};
#[cfg(test)]
pub(super) use playback::{
    ParkedPlaybackWorker, PlaybackWorker, PlaybackWorkerBoundary, PlaybackWorkerTerminal,
};
#[cfg(test)]
pub(super) use status::PcmExpectedState;
pub(super) use status::{
    PcmStatusError, PcmStatusSnapshot, validate_capture_status, validate_playback_status,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum PcmWait {
    Ready,
    TimedOut,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum PcmIoError {
    Again,
    Interrupted,
    Xrun,
    Suspended,
    Disconnected,
    Failed { errno: i32 },
}

pub(super) trait CapturePcm: Send {
    fn start_pcm(&mut self) -> Result<(), PcmIoError>;
    fn wait(&mut self, timeout_millis: u32) -> Result<PcmWait, PcmIoError>;
    fn read_frames(&mut self, bytes: &mut [u8]) -> Result<usize, PcmIoError>;
    fn status(&mut self) -> Result<PcmStatusSnapshot, PcmIoError>;
    fn drop_stream(&mut self) -> Result<(), PcmIoError>;
}

pub(super) trait PlaybackPcm: Send {
    fn wait(&mut self, timeout_millis: u32) -> Result<PcmWait, PcmIoError>;
    fn write_frames(&mut self, bytes: &[u8]) -> Result<usize, PcmIoError>;
    fn status(&mut self) -> Result<PcmStatusSnapshot, PcmIoError>;
    fn start_pcm(&mut self) -> Result<(), PcmIoError>;
    fn drop_stream(&mut self) -> Result<(), PcmIoError>;
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct WorkerIoPolicy {
    wait_timeout_millis: u32,
    transient_retry_limit: u32,
}

impl WorkerIoPolicy {
    pub(super) fn try_new(
        wait_timeout_millis: u32,
        transient_retry_limit: u32,
    ) -> Result<Self, WorkerIoPolicyError> {
        if wait_timeout_millis == 0 {
            return Err(WorkerIoPolicyError::ZeroWaitTimeout);
        }
        if wait_timeout_millis > i32::MAX as u32 {
            return Err(WorkerIoPolicyError::WaitTimeoutOutsideCInt {
                actual: wait_timeout_millis,
                maximum: i32::MAX as u32,
            });
        }
        if transient_retry_limit == 0 {
            return Err(WorkerIoPolicyError::ZeroTransientRetryLimit);
        }
        Ok(Self { wait_timeout_millis, transient_retry_limit })
    }

    pub(super) const fn wait_timeout_millis(self) -> u32 {
        self.wait_timeout_millis
    }

    pub(super) const fn transient_retry_limit(self) -> u32 {
        self.transient_retry_limit
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum WorkerIoPolicyError {
    ZeroWaitTimeout,
    WaitTimeoutOutsideCInt { actual: u32, maximum: u32 },
    ZeroTransientRetryLimit,
}

impl fmt::Display for WorkerIoPolicyError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{self:?}")
    }
}

impl std::error::Error for WorkerIoPolicyError {}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum WorkerStartError {
    InvalidGeometry,
    MonitorMixNotQueued { drop_stream_result: Result<(), PcmIoError> },
    BufferAllocation,
    ThreadSpawn,
}

impl fmt::Display for WorkerStartError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{self:?}")
    }
}

impl std::error::Error for WorkerStartError {}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub(in crate::audio_host) enum WorkerPhase {
    Starting = 0,
    Running = 1,
    PrimedParked = 2,
    Terminal = 3,
}

impl WorkerPhase {
    fn from_raw(raw: u8) -> Self {
        match raw {
            0 => Self::Starting,
            1 => Self::Running,
            2 => Self::PrimedParked,
            _ => Self::Terminal,
        }
    }
}

pub(super) struct WorkerSignals {
    stop: AtomicBool,
    resume_requested: AtomicBool,
    phase: AtomicU8,
    attempt_epoch: ClockAttemptEpoch,
}

impl WorkerSignals {
    pub(super) const fn new(attempt_epoch: ClockAttemptEpoch) -> Self {
        Self {
            stop: AtomicBool::new(false),
            resume_requested: AtomicBool::new(false),
            phase: AtomicU8::new(WorkerPhase::Starting as u8),
            attempt_epoch,
        }
    }

    pub(super) fn request_stop(&self) {
        self.stop.store(true, Ordering::Release);
    }

    pub(super) fn stopped(&self) -> bool {
        self.stop.load(Ordering::Acquire)
    }

    pub(super) fn phase(&self) -> WorkerPhase {
        WorkerPhase::from_raw(self.phase.load(Ordering::Acquire))
    }

    pub(super) fn set_phase(&self, phase: WorkerPhase) {
        self.phase.store(phase as u8, Ordering::Release);
    }

    pub(super) fn begin_primed_park(&self) {
        self.resume_requested.store(false, Ordering::Release);
        self.set_phase(WorkerPhase::PrimedParked);
    }

    pub(super) fn request_resume(&self) {
        self.resume_requested.store(true, Ordering::Release);
    }

    pub(super) fn take_resume_request(&self) -> bool {
        self.resume_requested.swap(false, Ordering::AcqRel)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(in crate::audio_host) enum WorkerResumeError {
    AttemptMismatch { worker: ClockAttemptEpoch, control: ClockAttemptEpoch },
    BridgeNotActive { actual: BridgeState },
    NotPrimedParked { actual: WorkerPhase },
}

impl fmt::Display for WorkerResumeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{self:?}")
    }
}

impl std::error::Error for WorkerResumeError {}

fn verify_worker_resume(
    signals: &WorkerSignals,
    control: &ClockBridgeControl,
) -> Result<(), WorkerResumeError> {
    if signals.attempt_epoch != control.worker_attempt_epoch() {
        return Err(WorkerResumeError::AttemptMismatch {
            worker: signals.attempt_epoch,
            control: control.worker_attempt_epoch(),
        });
    }
    let actual = control.state();
    if actual != BridgeState::Active {
        return Err(WorkerResumeError::BridgeNotActive { actual });
    }
    let actual = signals.phase();
    if actual != WorkerPhase::PrimedParked {
        return Err(WorkerResumeError::NotPrimedParked { actual });
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum TransientKind {
    Again,
    Interrupted,
}

#[derive(Clone, Copy, Debug)]
struct TransientBudget {
    attempts: u32,
    limit: u32,
}

impl TransientBudget {
    const fn new(limit: u32) -> Self {
        Self { attempts: 0, limit }
    }

    fn record(&mut self, error: PcmIoError) -> Result<(), TransientKind> {
        let kind = match error {
            PcmIoError::Again => TransientKind::Again,
            PcmIoError::Interrupted => TransientKind::Interrupted,
            PcmIoError::Xrun
            | PcmIoError::Suspended
            | PcmIoError::Disconnected
            | PcmIoError::Failed { .. } => return Ok(()),
        };
        if self.attempts >= self.limit {
            return Err(kind);
        }
        self.attempts += 1;
        Ok(())
    }

    fn reset(&mut self) {
        self.attempts = 0;
    }
}

fn allocate_zeroed(length: usize) -> Result<Vec<u8>, WorkerStartError> {
    let mut values = Vec::new();
    values.try_reserve_exact(length).map_err(|_| WorkerStartError::BufferAllocation)?;
    values.resize(length, 0);
    Ok(values)
}

fn geometry_period_frames(geometry: PhysicalPcmParameters) -> Result<usize, WorkerStartError> {
    usize::try_from(geometry.period_frames).map_err(|_| WorkerStartError::BufferAllocation)
}
