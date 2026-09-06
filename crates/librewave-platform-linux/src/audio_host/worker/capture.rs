use super::super::clock_bridge::observation::{CopySlotPublisher, CopySlotReader, copy_slot};
use super::super::clock_bridge::{BridgeError, BridgeState, CaptureIngress, CapturePublishReport};
use super::super::pcm::{
    PACKED_S24_SAMPLE_BYTES, PcmDirection, WAVE3_CAPTURE_CHANNELS, WAVE3_PCM_RATE_HZ,
};
use super::{
    CapturePcm, PcmIoError, PcmStatusError, PcmWait, TransientBudget, TransientKind,
    WorkerIoPolicy, WorkerPhase, WorkerResumeError, WorkerSignals, WorkerStartError,
    allocate_zeroed, geometry_period_frames, validate_capture_status, verify_worker_resume,
};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::Duration;

const PRIMING_GATE_RECHECK: Duration = Duration::from_millis(1);

#[derive(Clone, Copy, Debug, PartialEq)]
pub(in crate::audio_host) struct CaptureWorkerProgress {
    pub report: CapturePublishReport,
    pub delay_frames: i64,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(in crate::audio_host) enum CaptureWorkerTerminal {
    Stopped,
    AgainRetryLimit,
    InterruptedRetryLimit,
    InvalidProgress { actual_frames: usize, maximum_frames: usize },
    PositionOverflow,
    Xrun,
    Suspended,
    Disconnected,
    PcmFailed { errno: i32 },
    Status(PcmStatusError),
    Bridge(BridgeError),
}

pub(in crate::audio_host) struct CaptureWorker {
    signals: Arc<WorkerSignals>,
    progress: CopySlotReader<CaptureWorkerProgress>,
    join: JoinHandle<CaptureThreadExit>,
}

impl CaptureWorker {
    pub(in crate::audio_host) fn start(
        pcm: Box<dyn CapturePcm>,
        ingress: CaptureIngress,
        geometry: super::super::pcm::PhysicalPcmParameters,
        policy: WorkerIoPolicy,
    ) -> Result<Self, WorkerStartError> {
        let expected_channels =
            u32::try_from(WAVE3_CAPTURE_CHANNELS).map_err(|_| WorkerStartError::InvalidGeometry)?;
        if geometry.direction != PcmDirection::Capture
            || geometry.rate != WAVE3_PCM_RATE_HZ
            || geometry.channels != expected_channels
        {
            return Err(WorkerStartError::InvalidGeometry);
        }
        let period_frames = geometry_period_frames(geometry)?;
        let buffer_bytes = period_frames
            .checked_mul(WAVE3_CAPTURE_CHANNELS)
            .and_then(|samples| samples.checked_mul(PACKED_S24_SAMPLE_BYTES))
            .ok_or(WorkerStartError::BufferAllocation)?;
        let bytes = allocate_zeroed(buffer_bytes)?;
        let signals = Arc::new(WorkerSignals::new(ingress.worker_attempt_epoch()));
        let worker_signals = Arc::clone(&signals);
        let (progress_publisher, progress) = copy_slot();
        let capture_loop = CaptureLoop {
            pcm,
            ingress,
            geometry,
            period_frames,
            bytes,
            policy,
            signals: worker_signals,
            progress_publisher,
        };
        let join = thread::Builder::new()
            .name("librewave-alsa-capture".to_owned())
            .spawn(move || capture_loop.run())
            .map_err(|_| WorkerStartError::ThreadSpawn)?;
        Ok(Self { signals, progress, join })
    }

    pub(in crate::audio_host) fn try_progress(&mut self) -> Option<CaptureWorkerProgress> {
        self.progress.try_take().ok()
    }

    pub(in crate::audio_host) fn phase(&self) -> WorkerPhase {
        self.signals.phase()
    }

    pub(in crate::audio_host) fn is_finished(&self) -> bool {
        self.join.is_finished()
    }

    pub(in crate::audio_host) fn resume_after_activation(
        &self,
        control: &super::super::clock_bridge::ClockBridgeControl,
    ) -> Result<(), WorkerResumeError> {
        verify_worker_resume(&self.signals, control)?;
        self.signals.request_resume();
        self.join.thread().unpark();
        Ok(())
    }

    pub(in crate::audio_host) fn request_stop(&self) {
        self.signals.request_stop();
        self.join.thread().unpark();
    }

    pub(in crate::audio_host) fn park_and_join(
        self,
    ) -> Result<CaptureWorkerShutdown, CaptureWorkerJoinError> {
        self.request_stop();
        let CaptureThreadExit { mut pcm, ingress, terminal } =
            self.join.join().map_err(|_| CaptureWorkerJoinError::Panicked)?;
        let drop_stream_result = pcm.drop_stream();
        // The safe ALSA wrapper releases the handle through RAII here, on the
        // control thread. It does not expose the underlying close result.
        drop(pcm);
        Ok(CaptureWorkerShutdown { terminal, drop_stream_result, ingress })
    }
}

#[derive(Debug)]
pub(in crate::audio_host) struct CaptureWorkerShutdown {
    pub terminal: CaptureWorkerTerminal,
    pub drop_stream_result: Result<(), PcmIoError>,
    pub ingress: CaptureIngress,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(in crate::audio_host) enum CaptureWorkerJoinError {
    Panicked,
}

struct CaptureThreadExit {
    pcm: Box<dyn CapturePcm>,
    ingress: CaptureIngress,
    terminal: CaptureWorkerTerminal,
}

struct CaptureLoop {
    pcm: Box<dyn CapturePcm>,
    ingress: CaptureIngress,
    geometry: super::super::pcm::PhysicalPcmParameters,
    period_frames: usize,
    bytes: Vec<u8>,
    policy: WorkerIoPolicy,
    signals: Arc<WorkerSignals>,
    progress_publisher: CopySlotPublisher<CaptureWorkerProgress>,
}

impl CaptureLoop {
    fn run(mut self) -> CaptureThreadExit {
        let terminal = match self.start_pcm() {
            Ok(()) => {
                self.signals.set_phase(WorkerPhase::Running);
                self.run_started()
            }
            Err(terminal) => terminal,
        };
        self.signals.set_phase(WorkerPhase::Terminal);
        if terminal != CaptureWorkerTerminal::Stopped {
            self.ingress.worker_fault();
        }
        CaptureThreadExit { pcm: self.pcm, ingress: self.ingress, terminal }
    }

    fn start_pcm(&mut self) -> Result<(), CaptureWorkerTerminal> {
        let mut retry = TransientBudget::new(self.policy.transient_retry_limit());
        loop {
            if self.signals.stopped() {
                return Err(CaptureWorkerTerminal::Stopped);
            }
            match self.pcm.start_pcm() {
                Ok(()) => return Ok(()),
                Err(error @ (PcmIoError::Again | PcmIoError::Interrupted)) => {
                    if let Some(terminal) = capture_retry_terminal(&mut retry, error) {
                        return Err(terminal);
                    }
                    match self.pcm.wait(self.policy.wait_timeout_millis()) {
                        Ok(PcmWait::Ready | PcmWait::TimedOut) => {}
                        Err(wait_error @ (PcmIoError::Again | PcmIoError::Interrupted)) => {
                            if let Some(terminal) = capture_retry_terminal(&mut retry, wait_error) {
                                return Err(terminal);
                            }
                        }
                        Err(wait_error) => return Err(terminal_from_pcm(wait_error)),
                    }
                }
                Err(error) => return Err(terminal_from_pcm(error)),
            }
        }
    }

    #[allow(clippy::too_many_lines)]
    fn run_started(&mut self) -> CaptureWorkerTerminal {
        let mut retry = TransientBudget::new(self.policy.transient_retry_limit());
        'worker: loop {
            if self.signals.stopped() {
                break CaptureWorkerTerminal::Stopped;
            }
            match self.ingress.worker_state() {
                BridgeState::Primed if !self.park_until_active() => {
                    break CaptureWorkerTerminal::Stopped;
                }
                BridgeState::Quiescing | BridgeState::Stopped => {
                    break CaptureWorkerTerminal::Stopped;
                }
                _ => {}
            }
            match self.pcm.wait(self.policy.wait_timeout_millis()) {
                Ok(PcmWait::TimedOut) => continue,
                Ok(PcmWait::Ready) => {}
                Err(error @ (PcmIoError::Again | PcmIoError::Interrupted)) => {
                    if let Some(terminal) = capture_retry_terminal(&mut retry, error) {
                        break terminal;
                    }
                    continue;
                }
                Err(error) => break terminal_from_pcm(error),
            }
            if self.signals.stopped() {
                break CaptureWorkerTerminal::Stopped;
            }
            let frames_read = match self.pcm.read_frames(&mut self.bytes) {
                Ok(0) => {
                    break CaptureWorkerTerminal::InvalidProgress {
                        actual_frames: 0,
                        maximum_frames: self.period_frames,
                    };
                }
                Ok(frames) if frames <= self.period_frames => frames,
                Ok(frames) => {
                    break CaptureWorkerTerminal::InvalidProgress {
                        actual_frames: frames,
                        maximum_frames: self.period_frames,
                    };
                }
                Err(error @ (PcmIoError::Again | PcmIoError::Interrupted)) => {
                    if let Some(terminal) = capture_retry_terminal(&mut retry, error) {
                        break terminal;
                    }
                    continue;
                }
                Err(error) => break terminal_from_pcm(error),
            };
            retry.reset();
            let read = loop {
                match self.ingress.worker_state() {
                    BridgeState::Primed => {
                        if !self.park_until_active() {
                            break 'worker CaptureWorkerTerminal::Stopped;
                        }
                    }
                    BridgeState::PrimingCheck => {
                        if self.signals.stopped() {
                            break 'worker CaptureWorkerTerminal::Stopped;
                        }
                        thread::park_timeout(PRIMING_GATE_RECHECK);
                        continue;
                    }
                    _ => {}
                }
                match self.ingress.record_completed_read(frames_read) {
                    Ok(read) => break read,
                    Err(BridgeError::State { actual: BridgeState::PrimingCheck }) => {
                        if self.signals.stopped() {
                            break 'worker CaptureWorkerTerminal::Stopped;
                        }
                        thread::park_timeout(PRIMING_GATE_RECHECK);
                    }
                    Err(BridgeError::State { actual: BridgeState::Primed }) => {}
                    Err(BridgeError::Teardown) => {
                        break 'worker CaptureWorkerTerminal::Stopped;
                    }
                    Err(BridgeError::PositionOverflow { .. }) => {
                        break 'worker CaptureWorkerTerminal::PositionOverflow;
                    }
                    Err(error) => break 'worker CaptureWorkerTerminal::Bridge(error),
                }
            };
            let snapshot = loop {
                if self.signals.stopped() {
                    break 'worker CaptureWorkerTerminal::Stopped;
                }
                match self.pcm.status() {
                    Ok(snapshot) => break snapshot,
                    Err(error @ (PcmIoError::Again | PcmIoError::Interrupted)) => {
                        if let Some(terminal) = capture_retry_terminal(&mut retry, error) {
                            break 'worker terminal;
                        }
                        match self.pcm.wait(self.policy.wait_timeout_millis()) {
                            Ok(PcmWait::Ready | PcmWait::TimedOut) => {}
                            Err(wait_error @ (PcmIoError::Again | PcmIoError::Interrupted)) => {
                                if let Some(terminal) =
                                    capture_retry_terminal(&mut retry, wait_error)
                                {
                                    break 'worker terminal;
                                }
                            }
                            Err(wait_error) => break 'worker terminal_from_pcm(wait_error),
                        }
                    }
                    Err(error) => break 'worker terminal_from_pcm(error),
                }
            };
            let status = match validate_capture_status(
                read.attempt_epoch(),
                read.application_end_frame(),
                self.geometry,
                snapshot,
            ) {
                Ok(status) => status,
                Err(error) => break CaptureWorkerTerminal::Status(error),
            };
            let Some(byte_count) = frames_read.checked_mul(PACKED_S24_SAMPLE_BYTES) else {
                break CaptureWorkerTerminal::PositionOverflow;
            };
            let report = match read.publish(status.observation, &self.bytes[..byte_count]) {
                Ok(report) => report,
                Err(error) => break CaptureWorkerTerminal::Bridge(error),
            };
            retry.reset();
            let _ = self
                .progress_publisher
                .try_publish(CaptureWorkerProgress { report, delay_frames: status.delay_frames });
        }
    }

    fn park_until_active(&self) -> bool {
        self.signals.begin_primed_park();
        loop {
            if self.signals.stopped() {
                return false;
            }
            if self.signals.take_resume_request()
                && self.ingress.worker_state() == BridgeState::Active
            {
                self.signals.set_phase(WorkerPhase::Running);
                return true;
            }
            thread::park();
        }
    }
}

fn capture_retry_terminal(
    retry: &mut TransientBudget,
    error: PcmIoError,
) -> Option<CaptureWorkerTerminal> {
    match retry.record(error) {
        Ok(()) => None,
        Err(TransientKind::Again) => Some(CaptureWorkerTerminal::AgainRetryLimit),
        Err(TransientKind::Interrupted) => Some(CaptureWorkerTerminal::InterruptedRetryLimit),
    }
}

fn terminal_from_pcm(error: PcmIoError) -> CaptureWorkerTerminal {
    match error {
        PcmIoError::Again => CaptureWorkerTerminal::AgainRetryLimit,
        PcmIoError::Interrupted => CaptureWorkerTerminal::InterruptedRetryLimit,
        PcmIoError::Xrun => CaptureWorkerTerminal::Xrun,
        PcmIoError::Suspended => CaptureWorkerTerminal::Suspended,
        PcmIoError::Disconnected => CaptureWorkerTerminal::Disconnected,
        PcmIoError::Failed { errno } => CaptureWorkerTerminal::PcmFailed { errno },
    }
}
