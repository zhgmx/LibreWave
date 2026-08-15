//! Allocation-bounded capture worker and atomic observation state.

use super::{CapturePcm, CaptureRead, LinuxAudioError, ResourceActivity};
use std::io;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering};
use std::thread::{self, JoinHandle};

#[derive(Debug)]
pub(super) struct CaptureWorker {
    stop: Arc<AtomicBool>,
    state: Arc<CaptureState>,
    join: JoinHandle<()>,
}

impl CaptureWorker {
    pub(super) fn start(
        pcm: Box<dyn CapturePcm>,
        buffer_bytes: usize,
    ) -> Result<Self, LinuxAudioError> {
        Self::start_with(pcm, buffer_bytes, |capture_loop| {
            thread::Builder::new()
                .name("librewave-alsa-capture".to_owned())
                .spawn(move || capture_loop.run())
        })
    }

    pub(super) fn start_with<F>(
        pcm: Box<dyn CapturePcm>,
        buffer_bytes: usize,
        spawn: F,
    ) -> Result<Self, LinuxAudioError>
    where
        F: FnOnce(CaptureLoop) -> io::Result<JoinHandle<()>>,
    {
        let mut bytes = Vec::new();
        bytes
            .try_reserve_exact(buffer_bytes)
            .map_err(|_| LinuxAudioError::CaptureBufferAllocation)?;
        bytes.resize(buffer_bytes, 0);
        let stop = Arc::new(AtomicBool::new(false));
        let state = Arc::new(CaptureState::default());
        let worker_stop = Arc::clone(&stop);
        let worker_state = Arc::clone(&state);
        let capture_loop = CaptureLoop { pcm, bytes, stop: worker_stop, state: worker_state };
        let join = spawn(capture_loop)
            .map_err(|error| LinuxAudioError::CaptureWorkerSpawn(error.to_string()))?;
        Ok(Self { stop, state, join })
    }

    pub(super) fn snapshot(&self) -> CaptureAtomicState {
        self.state.snapshot()
    }

    pub(super) fn stop(self) -> Result<(), LinuxAudioError> {
        self.stop.store(true, Ordering::Release);
        self.join.join().map_err(|_| LinuxAudioError::CaptureWorkerPanicked)
    }
}

pub(super) struct CaptureLoop {
    pcm: Box<dyn CapturePcm>,
    bytes: Vec<u8>,
    stop: Arc<AtomicBool>,
    state: Arc<CaptureState>,
}

impl CaptureLoop {
    fn run(mut self) {
        while !self.stop.load(Ordering::Acquire) {
            match self.pcm.read_frames(&mut self.bytes) {
                CaptureRead::Frames(frames) if frames > 0 => {
                    self.state.frames.fetch_add(frames, Ordering::Release);
                    self.state.status.store(CaptureStatus::Active as u8, Ordering::Release);
                }
                CaptureRead::Frames(_) | CaptureRead::WouldBlock => {}
                CaptureRead::RecoveredXrun => {
                    self.state.recovered_xruns.fetch_add(1, Ordering::Release);
                }
                CaptureRead::Disconnected => {
                    self.state.status.store(CaptureStatus::Disconnected as u8, Ordering::Release);
                    return;
                }
                CaptureRead::Failed => {
                    self.state.status.store(CaptureStatus::Failed as u8, Ordering::Release);
                    return;
                }
            }
        }
        self.state.status.store(CaptureStatus::Stopped as u8, Ordering::Release);
    }
}

#[repr(u8)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum CaptureStatus {
    Starting = 0,
    Active = 1,
    Disconnected = 2,
    Failed = 3,
    Stopped = 4,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum CaptureFailure {
    Disconnected,
    Failed,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct CaptureAtomicState {
    pub(super) activity: ResourceActivity,
    pub(super) frames: u64,
    pub(super) recovered_xruns: u64,
    pub(super) failure: Option<CaptureFailure>,
}

impl Default for CaptureAtomicState {
    fn default() -> Self {
        Self { activity: ResourceActivity::Closed, frames: 0, recovered_xruns: 0, failure: None }
    }
}

#[derive(Debug)]
struct CaptureState {
    status: AtomicU8,
    pub(super) frames: AtomicU64,
    pub(super) recovered_xruns: AtomicU64,
}

impl Default for CaptureState {
    fn default() -> Self {
        Self {
            status: AtomicU8::new(CaptureStatus::Starting as u8),
            frames: AtomicU64::new(0),
            recovered_xruns: AtomicU64::new(0),
        }
    }
}

impl CaptureState {
    fn snapshot(&self) -> CaptureAtomicState {
        let status = match self.status.load(Ordering::Acquire) {
            value if value == CaptureStatus::Starting as u8 => CaptureStatus::Starting,
            value if value == CaptureStatus::Active as u8 => CaptureStatus::Active,
            value if value == CaptureStatus::Disconnected as u8 => CaptureStatus::Disconnected,
            value if value == CaptureStatus::Failed as u8 => CaptureStatus::Failed,
            _ => CaptureStatus::Stopped,
        };
        CaptureAtomicState {
            activity: match status {
                CaptureStatus::Starting => ResourceActivity::Starting,
                CaptureStatus::Active => ResourceActivity::Active,
                CaptureStatus::Disconnected | CaptureStatus::Failed => ResourceActivity::Failed,
                CaptureStatus::Stopped => ResourceActivity::Closed,
            },
            frames: self.frames.load(Ordering::Acquire),
            recovered_xruns: self.recovered_xruns.load(Ordering::Acquire),
            failure: match status {
                CaptureStatus::Disconnected => Some(CaptureFailure::Disconnected),
                CaptureStatus::Failed => Some(CaptureFailure::Failed),
                CaptureStatus::Starting | CaptureStatus::Active | CaptureStatus::Stopped => None,
            },
        }
    }
}
