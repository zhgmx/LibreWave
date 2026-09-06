use super::super::clock_bridge::observation::{CopySlotPublisher, CopySlotReader, copy_slot};
use super::super::clock_bridge::{
    BridgeError, BridgeState, PlaybackBoundaryDelivery, PlaybackEgress, PlaybackPeriodSubmitter,
    PlaybackProcessReport, PlaybackSubmissionProgress, PlaybackWriteError,
};
use super::super::pcm::{
    PACKED_S24_SAMPLE_BYTES, PcmDirection, WAVE3_PCM_RATE_HZ, WAVE3_PLAYBACK_CHANNELS,
};
use super::{
    PcmIoError, PcmStatusError, PcmWait, PlaybackPcm, TransientBudget, TransientKind,
    WorkerIoPolicy, WorkerPhase, WorkerResumeError, WorkerSignals, WorkerStartError,
    validate_playback_status, verify_worker_resume,
};
use std::sync::Arc;
use std::thread::{self, JoinHandle};

#[derive(Clone, Copy, Debug, PartialEq)]
pub(in crate::audio_host) struct PlaybackWorkerBoundary {
    pub report: PlaybackProcessReport,
    pub available_frames: u64,
    pub queued_frames: u64,
    pub delay_frames: i64,
    pub pcm_started: bool,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(in crate::audio_host) enum PlaybackWorkerTerminal {
    Stopped,
    Observe(PlaybackWriteError),
    PcmStart(PlaybackWriteError),
    Status(PcmStatusError),
    Bridge(BridgeError),
}

pub(in crate::audio_host) struct ParkedPlaybackWorker {
    egress: PlaybackEgress,
    submitter: PlaybackSubmitter,
    geometry: super::super::pcm::PhysicalPcmParameters,
}

impl ParkedPlaybackWorker {
    pub(in crate::audio_host) fn new(
        pcm: Box<dyn PlaybackPcm>,
        egress: PlaybackEgress,
        geometry: super::super::pcm::PhysicalPcmParameters,
        policy: WorkerIoPolicy,
    ) -> Result<Self, WorkerStartError> {
        let expected_channels = u32::try_from(WAVE3_PLAYBACK_CHANNELS)
            .map_err(|_| WorkerStartError::InvalidGeometry)?;
        if geometry.direction != PcmDirection::Playback
            || geometry.rate != WAVE3_PCM_RATE_HZ
            || geometry.channels != expected_channels
        {
            return Err(WorkerStartError::InvalidGeometry);
        }
        let attempt_epoch = egress.worker_attempt_epoch();
        Ok(Self { egress, submitter: PlaybackSubmitter::new(pcm, policy, attempt_epoch), geometry })
    }

    pub(in crate::audio_host) fn activate_thread_after_monitor_mix_queued(
        mut self,
    ) -> Result<PlaybackWorker, WorkerStartError> {
        if !matches!(
            self.egress.worker_state(),
            BridgeState::PlaybackFilterDelay
                | BridgeState::PlaybackEstimatorPriming
                | BridgeState::PlaybackStabilizing
                | BridgeState::Primed
                | BridgeState::Active
        ) {
            let drop_stream_result = self.submitter.drop_stream();
            return Err(WorkerStartError::MonitorMixNotQueued { drop_stream_result });
        }
        let signals = Arc::clone(&self.submitter.signals);
        let (boundary_publisher, boundaries) = copy_slot();
        let join = thread::Builder::new()
            .name("librewave-alsa-playback".to_owned())
            .spawn(move || {
                let terminal = self.run(boundary_publisher);
                self.submitter.signals.set_phase(WorkerPhase::Terminal);
                if terminal != PlaybackWorkerTerminal::Stopped {
                    self.egress.worker_fault();
                }
                PlaybackThreadExit { egress: self.egress, submitter: self.submitter, terminal }
            })
            .map_err(|_| WorkerStartError::ThreadSpawn)?;
        Ok(PlaybackWorker { signals, boundaries, join })
    }

    pub(in crate::audio_host) fn park_and_drop_stream(mut self) -> ParkedPlaybackWorkerShutdown {
        let drop_stream_result = self.submitter.drop_stream();
        drop(self.submitter);
        ParkedPlaybackWorkerShutdown { drop_stream_result, egress: self.egress }
    }

    fn run(
        &mut self,
        mut boundary_publisher: CopySlotPublisher<PlaybackWorkerBoundary>,
    ) -> PlaybackWorkerTerminal {
        self.submitter.signals.set_phase(WorkerPhase::Running);
        loop {
            if self.submitter.stopped() {
                return PlaybackWorkerTerminal::Stopped;
            }
            match self.egress.worker_state() {
                BridgeState::Primed if !self.park_until_active() => {
                    return PlaybackWorkerTerminal::Stopped;
                }
                BridgeState::Quiescing | BridgeState::Stopped => {
                    return PlaybackWorkerTerminal::Stopped;
                }
                _ => {}
            }
            let application_frames = self.egress.worker_application_position();
            let snapshot = match self.submitter.status() {
                Ok(snapshot) => snapshot,
                Err(PlaybackWriteError::Stopped { accepted_frames: 0 }) => {
                    return PlaybackWorkerTerminal::Stopped;
                }
                Err(error) => return PlaybackWorkerTerminal::Observe(error),
            };
            let status = match validate_playback_status(
                self.egress.worker_attempt_epoch(),
                application_frames,
                self.geometry,
                self.submitter.pcm_started,
                snapshot,
            ) {
                Ok(status) => status,
                Err(error) => return PlaybackWorkerTerminal::Status(error),
            };
            let report = match self.egress.process(status.observation, &mut self.submitter) {
                Ok(report) => report,
                Err(BridgeError::Playback(PlaybackWriteError::Stopped { accepted_frames: 0 })) => {
                    return PlaybackWorkerTerminal::Stopped;
                }
                Err(BridgeError::Teardown) => return PlaybackWorkerTerminal::Stopped,
                Err(error) => return PlaybackWorkerTerminal::Bridge(error),
            };
            let submitted = report.delivery != PlaybackBoundaryDelivery::DiscardedFilterDelay;
            if submitted && !self.submitter.pcm_started {
                match self.submitter.start_pcm() {
                    Ok(()) => {}
                    Err(PlaybackWriteError::Stopped { accepted_frames: 0 }) => {
                        return PlaybackWorkerTerminal::Stopped;
                    }
                    Err(error) => return PlaybackWorkerTerminal::PcmStart(error),
                }
            }
            let _ = boundary_publisher.try_publish(PlaybackWorkerBoundary {
                report,
                available_frames: status.available_frames,
                queued_frames: status.queued_frames,
                delay_frames: status.delay_frames,
                pcm_started: self.submitter.pcm_started,
            });
        }
    }

    fn park_until_active(&self) -> bool {
        self.submitter.signals.begin_primed_park();
        loop {
            if self.submitter.stopped() {
                return false;
            }
            if self.submitter.signals.take_resume_request()
                && self.egress.worker_state() == BridgeState::Active
            {
                self.submitter.signals.set_phase(WorkerPhase::Running);
                return true;
            }
            thread::park();
        }
    }
}

pub(in crate::audio_host) struct ParkedPlaybackWorkerShutdown {
    pub drop_stream_result: Result<(), PcmIoError>,
    pub egress: PlaybackEgress,
}

pub(in crate::audio_host) struct PlaybackWorker {
    signals: Arc<WorkerSignals>,
    boundaries: CopySlotReader<PlaybackWorkerBoundary>,
    join: JoinHandle<PlaybackThreadExit>,
}

impl PlaybackWorker {
    pub(in crate::audio_host) fn try_boundary(&mut self) -> Option<PlaybackWorkerBoundary> {
        self.boundaries.try_take().ok()
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
    ) -> Result<PlaybackWorkerShutdown, PlaybackWorkerJoinError> {
        self.request_stop();
        let PlaybackThreadExit { egress, mut submitter, terminal } =
            self.join.join().map_err(|_| PlaybackWorkerJoinError::Panicked)?;
        let drop_stream_result = submitter.drop_stream();
        // Safe alsa 0.12.1 performs the final handle release through RAII and
        // does not return the underlying snd_pcm_close result.
        drop(submitter);
        Ok(PlaybackWorkerShutdown { terminal, drop_stream_result, egress })
    }
}

pub(in crate::audio_host) struct PlaybackWorkerShutdown {
    pub terminal: PlaybackWorkerTerminal,
    pub drop_stream_result: Result<(), PcmIoError>,
    pub egress: PlaybackEgress,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(in crate::audio_host) enum PlaybackWorkerJoinError {
    Panicked,
}

struct PlaybackThreadExit {
    egress: PlaybackEgress,
    submitter: PlaybackSubmitter,
    terminal: PlaybackWorkerTerminal,
}

pub(super) struct PlaybackSubmitter {
    pcm: Box<dyn PlaybackPcm>,
    policy: WorkerIoPolicy,
    signals: Arc<WorkerSignals>,
    pcm_started: bool,
}

impl PlaybackSubmitter {
    pub(super) fn new(
        pcm: Box<dyn PlaybackPcm>,
        policy: WorkerIoPolicy,
        attempt_epoch: librewave_engine::ClockAttemptEpoch,
    ) -> Self {
        Self {
            pcm,
            policy,
            signals: Arc::new(WorkerSignals::new(attempt_epoch)),
            pcm_started: false,
        }
    }

    fn stopped(&self) -> bool {
        self.signals.stopped()
    }

    fn status(&mut self) -> Result<super::PcmStatusSnapshot, PlaybackWriteError> {
        let mut retry = TransientBudget::new(self.policy.transient_retry_limit());
        loop {
            if self.stopped() {
                return Err(PlaybackWriteError::Stopped { accepted_frames: 0 });
            }
            match self.pcm.status() {
                Ok(snapshot) => return Ok(snapshot),
                Err(error @ (PcmIoError::Again | PcmIoError::Interrupted)) => {
                    record_playback_retry(&mut retry, error, 0)?;
                    match self.pcm.wait(self.policy.wait_timeout_millis()) {
                        Ok(PcmWait::Ready | PcmWait::TimedOut) => {}
                        Err(wait_error @ (PcmIoError::Again | PcmIoError::Interrupted)) => {
                            record_playback_retry(&mut retry, wait_error, 0)?;
                        }
                        Err(wait_error) => return Err(playback_error(wait_error, 0)),
                    }
                }
                Err(error) => return Err(playback_error(error, 0)),
            }
        }
    }

    fn start_pcm(&mut self) -> Result<(), PlaybackWriteError> {
        let mut retry = TransientBudget::new(self.policy.transient_retry_limit());
        loop {
            if self.stopped() {
                return Err(PlaybackWriteError::Stopped { accepted_frames: 0 });
            }
            match self.pcm.start_pcm() {
                Ok(()) => {
                    self.pcm_started = true;
                    return Ok(());
                }
                Err(error @ (PcmIoError::Again | PcmIoError::Interrupted)) => {
                    record_playback_retry(&mut retry, error, 0)?;
                    match self.pcm.wait(self.policy.wait_timeout_millis()) {
                        Ok(PcmWait::Ready | PcmWait::TimedOut) => {}
                        Err(wait_error @ (PcmIoError::Again | PcmIoError::Interrupted)) => {
                            record_playback_retry(&mut retry, wait_error, 0)?;
                        }
                        Err(wait_error) => return Err(playback_error(wait_error, 0)),
                    }
                }
                Err(error) => return Err(playback_error(error, 0)),
            }
        }
    }

    fn drop_stream(&mut self) -> Result<(), PcmIoError> {
        self.pcm.drop_stream()
    }
}

impl PlaybackPeriodSubmitter for PlaybackSubmitter {
    fn submit_packed_s24_3le(
        &mut self,
        complete_frame_count: usize,
        bytes: &[u8],
    ) -> Result<PlaybackSubmissionProgress, PlaybackWriteError> {
        let frame_bytes = WAVE3_PLAYBACK_CHANNELS
            .checked_mul(PACKED_S24_SAMPLE_BYTES)
            .ok_or(PlaybackWriteError::InvalidProgress { accepted_frames: 0 })?;
        let expected_bytes = complete_frame_count
            .checked_mul(frame_bytes)
            .ok_or(PlaybackWriteError::InvalidProgress { accepted_frames: 0 })?;
        if bytes.len() != expected_bytes {
            return Err(PlaybackWriteError::InvalidProgress { accepted_frames: 0 });
        }
        let mut accepted_frames = 0;
        let mut retry = TransientBudget::new(self.policy.transient_retry_limit());
        while accepted_frames < complete_frame_count {
            if self.stopped() {
                return Err(PlaybackWriteError::Stopped { accepted_frames });
            }
            match self.pcm.wait(self.policy.wait_timeout_millis()) {
                Ok(PcmWait::TimedOut) => continue,
                Ok(PcmWait::Ready) => {}
                Err(error @ (PcmIoError::Again | PcmIoError::Interrupted)) => {
                    record_playback_retry(&mut retry, error, accepted_frames)?;
                    continue;
                }
                Err(error) => return Err(playback_error(error, accepted_frames)),
            }
            if self.stopped() {
                return Err(PlaybackWriteError::Stopped { accepted_frames });
            }
            let byte_offset = accepted_frames
                .checked_mul(frame_bytes)
                .ok_or(PlaybackWriteError::InvalidProgress { accepted_frames })?;
            match self.pcm.write_frames(&bytes[byte_offset..]) {
                Ok(0) => return Err(PlaybackWriteError::InvalidProgress { accepted_frames }),
                Ok(frames) if frames <= complete_frame_count - accepted_frames => {
                    accepted_frames += frames;
                    retry.reset();
                }
                Ok(_) => return Err(PlaybackWriteError::InvalidProgress { accepted_frames }),
                Err(error @ (PcmIoError::Again | PcmIoError::Interrupted)) => {
                    record_playback_retry(&mut retry, error, accepted_frames)?;
                }
                Err(error) => return Err(playback_error(error, accepted_frames)),
            }
        }
        Ok(PlaybackSubmissionProgress { frames_submitted: accepted_frames })
    }
}

fn record_playback_retry(
    retry: &mut TransientBudget,
    error: PcmIoError,
    accepted_frames: usize,
) -> Result<(), PlaybackWriteError> {
    match retry.record(error) {
        Ok(()) => Ok(()),
        Err(TransientKind::Again) => Err(PlaybackWriteError::AgainRetryLimit { accepted_frames }),
        Err(TransientKind::Interrupted) => {
            Err(PlaybackWriteError::InterruptedRetryLimit { accepted_frames })
        }
    }
}

fn playback_error(error: PcmIoError, accepted_frames: usize) -> PlaybackWriteError {
    match error {
        PcmIoError::Again => PlaybackWriteError::AgainRetryLimit { accepted_frames },
        PcmIoError::Interrupted => PlaybackWriteError::InterruptedRetryLimit { accepted_frames },
        PcmIoError::Xrun => PlaybackWriteError::Xrun { accepted_frames },
        PcmIoError::Suspended => PlaybackWriteError::Suspended { accepted_frames },
        PcmIoError::Disconnected => PlaybackWriteError::Disconnected { accepted_frames },
        PcmIoError::Failed { errno } => PlaybackWriteError::Failed { accepted_frames, errno },
    }
}
