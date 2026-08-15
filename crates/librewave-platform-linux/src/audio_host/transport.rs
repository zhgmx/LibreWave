//! Bounded capture-clock transport between Linux audio and the portable engine.

use super::{PhysicalPcmConfig, PhysicalSampleFormat};
use librewave_core::{
    EndpointId, MICROPHONE_SOURCE_ID, MixerProfile, MixerProfileError, SYSTEM_SOURCE_ID,
    SourceControls,
};
use librewave_engine::{
    CHANNELS, ConfigError, ControlStager, InputBuffer, MeterPublisher, MeterReader, MixerConfig,
    MixerEngine, OutputBuffer, ProcessError, StageError,
};
use std::cell::UnsafeCell;
use std::fmt;
use std::sync::Arc;
use std::sync::atomic::{AtomicU8, AtomicU64, AtomicUsize, Ordering};

const SYSTEM_IDLE: u8 = 0;
const SYSTEM_ACTIVE: u8 = 1;
const SYSTEM_FAULTED: u8 = 2;
const S32_SCALE: f64 = 2_147_483_648.0;

/// Control-side handles for one capture-clock transport.
#[derive(Debug)]
pub struct TransportHandles {
    pub control: TransportControl,
    pub system_ingress: SystemIngress,
    pub meters: MeterReader,
}

/// The control-side owner of the engine's existing SPSC staging handle.
#[derive(Debug)]
pub struct TransportControl {
    stager: ControlStager,
}

impl TransportControl {
    /// Stages the exact source controls from the current product profile.
    ///
    /// # Errors
    ///
    /// Returns an error when the profile is not the current strict profile or
    /// while an older complete control snapshot is pending.
    pub fn try_stage_profile(
        &mut self,
        profile: &MixerProfile,
    ) -> Result<(), TransportControlError> {
        let controls = profile_controls(profile).map_err(TransportControlError::Profile)?;
        self.stager.try_stage(&controls).map_err(TransportControlError::Stage)
    }
}

/// Why the current mixer profile could not be staged.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TransportControlError {
    Profile(MixerProfileError),
    Stage(StageError),
}

impl fmt::Display for TransportControlError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Profile(error) => write!(formatter, "invalid mixer profile: {error}"),
            Self::Stage(error) => write!(formatter, "mixer controls were not staged: {error}"),
        }
    }
}

impl std::error::Error for TransportControlError {}

/// The sole producer for the System source's bounded stereo frame FIFO.
#[derive(Debug)]
pub struct SystemIngress {
    shared: Arc<SystemRing>,
    write_index: usize,
}

impl SystemIngress {
    /// Publishes the first block and atomically marks the endpoint active.
    ///
    /// Before activation, the System source is intentionally silent. A
    /// transport underrun is possible only after the first write index is
    /// visible and the active state is published with Release ordering.
    ///
    /// # Errors
    ///
    /// Returns an error for an invalid state, an empty or invalid block, or a
    /// block that exceeds the remaining FIFO capacity.
    pub fn activate_with_first_block(&mut self, samples: &[f32]) -> Result<(), SystemIngressError> {
        match self.shared.state.load(Ordering::Acquire) {
            SYSTEM_IDLE => {}
            SYSTEM_FAULTED => return Err(SystemIngressError::Faulted),
            _ => return Err(SystemIngressError::AlreadyActive),
        }
        let frames = validate_system_samples(samples)?;
        if frames == 0 {
            return Err(SystemIngressError::EmptyActivation);
        }
        self.push_validated(samples, frames)?;
        self.shared.state.store(SYSTEM_ACTIVE, Ordering::Release);
        Ok(())
    }

    /// Adds one finite stereo interleaved block without waiting.
    ///
    /// Blocks may use a different frame count from the ALSA capture period.
    /// The FIFO bridges only the block-size difference; it does not correct
    /// independent-clock drift.
    ///
    /// # Errors
    ///
    /// Returns an error unless the stream is active and the finite, complete
    /// stereo block fits in the remaining FIFO capacity.
    pub fn try_push_interleaved(&mut self, samples: &[f32]) -> Result<(), SystemIngressError> {
        match self.shared.state.load(Ordering::Acquire) {
            SYSTEM_IDLE => return Err(SystemIngressError::Idle),
            SYSTEM_FAULTED => return Err(SystemIngressError::Faulted),
            _ => {}
        }
        let frames = validate_system_samples(samples)?;
        self.push_validated(samples, frames)
    }

    fn push_validated(&mut self, samples: &[f32], frames: usize) -> Result<(), SystemIngressError> {
        let read_index = self.shared.read_index.load(Ordering::Acquire);
        let remaining_frames = self.shared.free_frames(self.write_index, read_index);
        if frames > remaining_frames {
            self.shared.overflows.fetch_add(1, Ordering::Relaxed);
            self.shared.state.store(SYSTEM_FAULTED, Ordering::Release);
            return Err(SystemIngressError::Overflow {
                requested_frames: frames,
                remaining_frames,
                capacity_frames: self.shared.capacity_frames(),
            });
        }

        for frame in samples.chunks_exact(CHANNELS) {
            // SAFETY: free-space admission prevents the producer from
            // overwriting unread data. This sole producer owns write_index.
            unsafe {
                *self.shared.frames[self.write_index].get() = [frame[0], frame[1]];
            }
            self.write_index = self.shared.advance(self.write_index);
        }
        self.shared.write_index.store(self.write_index, Ordering::Release);
        Ok(())
    }
}

/// Why a System ingress operation was rejected.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SystemIngressError {
    Idle,
    EmptyActivation,
    AlreadyActive,
    Faulted,
    PartialFrame { samples: usize },
    NonFinite { sample_index: usize },
    Overflow { requested_frames: usize, remaining_frames: usize, capacity_frames: usize },
}

impl fmt::Display for SystemIngressError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Idle => formatter.write_str("the System endpoint stream is idle"),
            Self::EmptyActivation => {
                formatter.write_str("System activation requires at least one complete frame")
            }
            Self::AlreadyActive => formatter.write_str("the System endpoint stream is active"),
            Self::Faulted => formatter.write_str("the System endpoint stream has failed"),
            Self::PartialFrame { samples } => {
                write!(
                    formatter,
                    "System ingress has {samples} samples, not complete stereo frames"
                )
            }
            Self::NonFinite { sample_index } => {
                write!(formatter, "System ingress sample {sample_index} is not finite")
            }
            Self::Overflow { requested_frames, remaining_frames, capacity_frames } => write!(
                formatter,
                "System ingress requested {requested_frames} frames; {remaining_frames} of {capacity_frames} FIFO frames remained"
            ),
        }
    }
}

impl std::error::Error for SystemIngressError {}

/// A physical playback writer owned by the capture-clock worker.
pub trait TransportPlayback: Send {
    /// Writes exactly one complete stereo S32LE block.
    ///
    /// # Errors
    ///
    /// Returns a classified error unless the complete block reaches playback.
    fn write_interleaved_s32le(
        &mut self,
        frames: usize,
        bytes: &[u8],
    ) -> Result<(), TransportPlaybackError>;
}

/// Stable playback failure classes for the transport seam.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TransportPlaybackError {
    ShortWrite,
    XrunRecoveryFailed,
    Disconnected,
    Failed,
}

/// Whether one processed capture block reached physical playback.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProcessedDelivery {
    /// The block proved capture, conversion, engine, and meter operation while
    /// graph readiness was false. It was not sent to physical playback or a
    /// public endpoint stream.
    Preflight,
    /// The complete Monitor Mix block reached the worker-owned playback PCM.
    PhysicalPlayback,
}

/// The accepted result for one variable-size capture block.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ProcessedCapture {
    pub frames: usize,
    pub delivery: ProcessedDelivery,
    pub control_update_applied: bool,
}

/// Aggregate bounded-transport observations without platform identifiers.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct TransportCounters {
    pub capture_blocks: u64,
    pub processed_blocks: u64,
    pub preflight_blocks: u64,
    pub playback_blocks: u64,
    pub idle_system_silence_blocks: u64,
    pub system_underflows: u64,
    pub system_overflows: u64,
    pub meter_overflows: u64,
    pub playback_failures: u64,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
struct LocalTransportCounters {
    capture_blocks: u64,
    processed_blocks: u64,
    preflight_blocks: u64,
    playback_blocks: u64,
    idle_system_silence_blocks: u64,
    system_underflows: u64,
    meter_overflows: u64,
    playback_failures: u64,
}

/// One capture-clock engine instance and its preallocated block buffers.
#[derive(Debug)]
pub struct CaptureClockTransport {
    engine: MixerEngine,
    system: SystemConsumer,
    meter_publisher: MeterPublisher,
    microphone_input: Vec<f32>,
    system_input: Vec<f32>,
    microphone_output: Vec<f32>,
    monitor_output: Vec<f32>,
    stream_output: Vec<f32>,
    playback_bytes: Vec<u8>,
    max_frames: usize,
    last_frames: usize,
    failed: bool,
    counters: LocalTransportCounters,
}

impl CaptureClockTransport {
    /// Creates a transport with every buffer and handoff allocated up front.
    ///
    /// # Errors
    ///
    /// Returns an error unless the physical format is stereo, interleaved
    /// S32LE at the engine's 48 kHz rate and the System FIFO can hold a maximum
    /// capture block.
    pub fn new(
        pcm: PhysicalPcmConfig,
        profile: &MixerProfile,
        system_capacity_frames: usize,
    ) -> Result<(Self, TransportHandles), TransportBuildError> {
        let pcm = pcm.validate().map_err(|_| TransportBuildError::PhysicalFormat)?;
        if pcm.sample_format != PhysicalSampleFormat::Signed32LittleEndian
            || pcm.rate != librewave_engine::SAMPLE_RATE_HZ
            || usize::try_from(pcm.channels).ok() != Some(CHANNELS)
        {
            return Err(TransportBuildError::PhysicalFormat);
        }
        let max_frames =
            usize::try_from(pcm.period_frames).map_err(|_| TransportBuildError::PhysicalFormat)?;
        if system_capacity_frames < max_frames {
            return Err(TransportBuildError::SystemCapacity {
                minimum_frames: max_frames,
                actual_frames: system_capacity_frames,
            });
        }
        let controls = profile_controls(profile).map_err(TransportBuildError::Profile)?;
        let source_ids =
            [profile.sources[0].controls.source(), profile.sources[1].controls.source()];
        let config = MixerConfig::try_new(max_frames, profile.microphone_source, &source_ids)
            .map_err(TransportBuildError::EngineConfig)?;
        let (engine, stager) =
            MixerEngine::new(config, &controls).map_err(|_| TransportBuildError::ControlMapping)?;
        let (system_ingress, system) = SystemConsumer::channel(system_capacity_frames)?;
        let (meter_publisher, meters) = MeterPublisher::channel();
        let sample_capacity =
            max_frames.checked_mul(CHANNELS).ok_or(TransportBuildError::PhysicalFormat)?;
        let byte_capacity = sample_capacity
            .checked_mul(size_of::<i32>())
            .ok_or(TransportBuildError::PhysicalFormat)?;
        let transport = Self {
            engine,
            system,
            meter_publisher,
            microphone_input: allocate_zeroed(sample_capacity)?,
            system_input: allocate_zeroed(sample_capacity)?,
            microphone_output: allocate_zeroed(sample_capacity)?,
            monitor_output: allocate_zeroed(sample_capacity)?,
            stream_output: allocate_zeroed(sample_capacity)?,
            playback_bytes: allocate_zeroed(byte_capacity)?,
            max_frames,
            last_frames: 0,
            failed: false,
            counters: LocalTransportCounters::default(),
        };
        Ok((
            transport,
            TransportHandles { control: TransportControl { stager }, system_ingress, meters },
        ))
    }

    /// Processes one complete variable-size capture block.
    ///
    /// An idle System stream supplies semantic silence. An active System
    /// stream must supply the exact frame count; a shortage is a transport
    /// underrun and fails this attempt.
    ///
    /// # Errors
    ///
    /// Returns [`TransportError::NoCaptureProgress`] for an empty read without
    /// failing the attempt. Invalid capture blocks, active System transport
    /// faults, engine failures, encoding failures, and playback failures set
    /// the sticky failed state and return their classified error.
    pub fn process_capture(
        &mut self,
        capture_bytes: &[u8],
        playback: Option<&mut dyn TransportPlayback>,
    ) -> Result<ProcessedCapture, TransportError> {
        if self.failed {
            return Err(TransportError::Failed);
        }
        let frame_bytes = CHANNELS * size_of::<i32>();
        if capture_bytes.is_empty() {
            return Err(TransportError::NoCaptureProgress);
        }
        if !capture_bytes.len().is_multiple_of(frame_bytes) {
            return self.fail(TransportError::PartialCaptureFrame { bytes: capture_bytes.len() });
        }
        let frames = capture_bytes.len() / frame_bytes;
        if frames > self.max_frames {
            return self.fail(TransportError::CaptureBlockTooLarge {
                maximum_frames: self.max_frames,
                actual_frames: frames,
            });
        }
        self.counters.capture_blocks = self.counters.capture_blocks.saturating_add(1);
        let samples = frames * CHANNELS;
        decode_s32le(
            &capture_bytes[..samples * size_of::<i32>()],
            &mut self.microphone_input[..samples],
        );

        match self.system.state() {
            SystemState::Idle => {
                self.system_input[..samples].fill(0.0);
                self.counters.idle_system_silence_blocks =
                    self.counters.idle_system_silence_blocks.saturating_add(1);
            }
            SystemState::Active => {
                if let Err(available_frames) =
                    self.system.try_pop_exact(&mut self.system_input[..samples])
                {
                    self.counters.system_underflows =
                        self.counters.system_underflows.saturating_add(1);
                    self.system.fail();
                    return self.fail(TransportError::SystemUnderflow {
                        required_frames: frames,
                        available_frames,
                    });
                }
            }
            SystemState::Faulted => return self.fail(TransportError::SystemIngressFaulted),
        }

        let inputs = [
            InputBuffer::new(MICROPHONE_SOURCE_ID, &self.microphone_input[..samples]),
            InputBuffer::new(SYSTEM_SOURCE_ID, &self.system_input[..samples]),
        ];
        let mut outputs = [
            OutputBuffer::new(EndpointId::Microphone, &mut self.microphone_output[..samples]),
            OutputBuffer::new(EndpointId::MonitorMix, &mut self.monitor_output[..samples]),
            OutputBuffer::new(EndpointId::StreamMix, &mut self.stream_output[..samples]),
        ];
        let report = match self.engine.process(frames, &inputs, &mut outputs) {
            Ok(report) => report,
            Err(error) => return self.fail(TransportError::Engine(error)),
        };
        self.last_frames = frames;
        self.counters.processed_blocks = self.counters.processed_blocks.saturating_add(1);
        // A fault observed here suppresses meter and playback delivery for
        // this block. A producer fault published after this Acquire may let
        // the already in-flight block complete; the next block observes it
        // before processing.
        if self.system.state() == SystemState::Faulted {
            return self.fail(TransportError::SystemIngressFaulted);
        }
        if !self.meter_publisher.try_publish(report.meters()) {
            self.counters.meter_overflows = self.counters.meter_overflows.saturating_add(1);
        }

        let capture_was_confirmed = self.counters.processed_blocks > 1;
        let delivery = if let Some(playback) = playback.filter(|_| capture_was_confirmed) {
            if let Err(error) = encode_s32le(
                &self.monitor_output[..samples],
                &mut self.playback_bytes[..capture_bytes.len()],
            ) {
                return self.fail(error);
            }
            if let Err(error) = playback
                .write_interleaved_s32le(frames, &self.playback_bytes[..capture_bytes.len()])
            {
                self.counters.playback_failures = self.counters.playback_failures.saturating_add(1);
                return self.fail(TransportError::Playback(error));
            }
            self.counters.playback_blocks = self.counters.playback_blocks.saturating_add(1);
            ProcessedDelivery::PhysicalPlayback
        } else {
            self.counters.preflight_blocks = self.counters.preflight_blocks.saturating_add(1);
            ProcessedDelivery::Preflight
        };
        Ok(ProcessedCapture {
            frames,
            delivery,
            control_update_applied: report.control_update_applied(),
        })
    }

    #[must_use]
    pub fn last_output(&self, endpoint: EndpointId) -> Option<&[f32]> {
        let samples = self.last_frames * CHANNELS;
        match endpoint {
            EndpointId::System => None,
            EndpointId::Microphone => Some(&self.microphone_output[..samples]),
            EndpointId::MonitorMix => Some(&self.monitor_output[..samples]),
            EndpointId::StreamMix => Some(&self.stream_output[..samples]),
        }
    }

    #[must_use]
    pub fn counters(&self) -> TransportCounters {
        TransportCounters {
            capture_blocks: self.counters.capture_blocks,
            processed_blocks: self.counters.processed_blocks,
            preflight_blocks: self.counters.preflight_blocks,
            playback_blocks: self.counters.playback_blocks,
            idle_system_silence_blocks: self.counters.idle_system_silence_blocks,
            system_underflows: self.counters.system_underflows,
            system_overflows: self.system.overflows(),
            meter_overflows: self.counters.meter_overflows,
            playback_failures: self.counters.playback_failures,
        }
    }

    fn fail<T>(&mut self, error: TransportError) -> Result<T, TransportError> {
        self.failed = true;
        Err(error)
    }
}

/// Why a capture-clock transport could not be constructed.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TransportBuildError {
    PhysicalFormat,
    SystemCapacity { minimum_frames: usize, actual_frames: usize },
    SystemCapacityOverflow,
    BufferAllocation,
    Profile(MixerProfileError),
    EngineConfig(ConfigError),
    ControlMapping,
}

impl fmt::Display for TransportBuildError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::PhysicalFormat => formatter.write_str("the physical engine format is invalid"),
            Self::SystemCapacity { minimum_frames, actual_frames } => write!(
                formatter,
                "System FIFO capacity is {actual_frames} frames; at least {minimum_frames} are required"
            ),
            Self::SystemCapacityOverflow => {
                formatter.write_str("System FIFO capacity cannot reserve its boundary slot")
            }
            Self::BufferAllocation => {
                formatter.write_str("audio transport buffers could not be allocated")
            }
            Self::Profile(error) => write!(formatter, "invalid mixer profile: {error}"),
            Self::EngineConfig(error) => write!(formatter, "invalid engine layout: {error}"),
            Self::ControlMapping => formatter.write_str("mixer controls do not match the engine"),
        }
    }
}

impl std::error::Error for TransportBuildError {}

/// Why one capture-clock block was not accepted or delivered.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TransportError {
    Failed,
    NoCaptureProgress,
    PartialCaptureFrame { bytes: usize },
    CaptureBlockTooLarge { maximum_frames: usize, actual_frames: usize },
    SystemUnderflow { required_frames: usize, available_frames: usize },
    SystemIngressFaulted,
    Engine(ProcessError),
    NonFinitePlayback { sample_index: usize },
    Playback(TransportPlaybackError),
}

impl fmt::Display for TransportError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Failed => formatter.write_str("the audio transport attempt has failed"),
            Self::NoCaptureProgress => formatter.write_str("capture made no progress"),
            Self::PartialCaptureFrame { bytes } => {
                write!(formatter, "capture returned {bytes} bytes, not complete stereo frames")
            }
            Self::CaptureBlockTooLarge { maximum_frames, actual_frames } => write!(
                formatter,
                "capture returned {actual_frames} frames; maximum is {maximum_frames}"
            ),
            Self::SystemUnderflow { required_frames, available_frames } => write!(
                formatter,
                "active System ingress has {available_frames} frames; {required_frames} are required"
            ),
            Self::SystemIngressFaulted => formatter.write_str("System ingress has failed"),
            Self::Engine(error) => write!(formatter, "engine rejected the block: {error}"),
            Self::NonFinitePlayback { sample_index } => {
                write!(formatter, "playback sample {sample_index} is not finite")
            }
            Self::Playback(error) => write!(formatter, "physical playback failed: {error:?}"),
        }
    }
}

impl std::error::Error for TransportError {}

fn profile_controls(profile: &MixerProfile) -> Result<[SourceControls; 2], MixerProfileError> {
    profile.validate()?;
    Ok([profile.sources[0].controls, profile.sources[1].controls])
}

fn validate_system_samples(samples: &[f32]) -> Result<usize, SystemIngressError> {
    if !samples.len().is_multiple_of(CHANNELS) {
        return Err(SystemIngressError::PartialFrame { samples: samples.len() });
    }
    if let Some(sample_index) = samples.iter().position(|sample| !sample.is_finite()) {
        return Err(SystemIngressError::NonFinite { sample_index });
    }
    Ok(samples.len() / CHANNELS)
}

fn allocate_zeroed<T: Clone + Default>(length: usize) -> Result<Vec<T>, TransportBuildError> {
    let mut values = Vec::new();
    values.try_reserve_exact(length).map_err(|_| TransportBuildError::BufferAllocation)?;
    values.resize(length, T::default());
    Ok(values)
}

#[allow(clippy::cast_possible_truncation)]
fn decode_s32le(bytes: &[u8], output: &mut [f32]) {
    for (sample, encoded) in output.iter_mut().zip(bytes.chunks_exact(size_of::<i32>())) {
        let raw = i32::from_le_bytes([encoded[0], encoded[1], encoded[2], encoded[3]]);
        *sample = (f64::from(raw) / S32_SCALE) as f32;
    }
}

#[allow(clippy::cast_possible_truncation)]
fn encode_s32le(samples: &[f32], output: &mut [u8]) -> Result<(), TransportError> {
    if let Some(sample_index) = samples.iter().position(|sample| !sample.is_finite()) {
        return Err(TransportError::NonFinitePlayback { sample_index });
    }
    for (sample, encoded) in samples.iter().copied().zip(output.chunks_exact_mut(size_of::<i32>()))
    {
        let raw = if sample <= -1.0 {
            i32::MIN
        } else if sample >= 1.0 {
            i32::MAX
        } else {
            (f64::from(sample) * S32_SCALE).round() as i32
        };
        encoded.copy_from_slice(&raw.to_le_bytes());
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SystemState {
    Idle,
    Active,
    Faulted,
}

#[derive(Debug)]
struct SystemConsumer {
    shared: Arc<SystemRing>,
    read_index: usize,
}

impl SystemConsumer {
    fn channel(capacity_frames: usize) -> Result<(SystemIngress, Self), TransportBuildError> {
        let slots =
            capacity_frames.checked_add(1).ok_or(TransportBuildError::SystemCapacityOverflow)?;
        let mut frames = Vec::new();
        frames.try_reserve_exact(slots).map_err(|_| TransportBuildError::BufferAllocation)?;
        for _ in 0..slots {
            frames.push(UnsafeCell::new([0.0; CHANNELS]));
        }
        let shared = Arc::new(SystemRing {
            frames,
            write_index: AtomicUsize::new(0),
            read_index: AtomicUsize::new(0),
            state: AtomicU8::new(SYSTEM_IDLE),
            overflows: AtomicU64::new(0),
        });
        Ok((
            SystemIngress { shared: Arc::clone(&shared), write_index: 0 },
            Self { shared, read_index: 0 },
        ))
    }

    fn state(&self) -> SystemState {
        match self.shared.state.load(Ordering::Acquire) {
            SYSTEM_IDLE => SystemState::Idle,
            SYSTEM_ACTIVE => SystemState::Active,
            _ => SystemState::Faulted,
        }
    }

    fn available_frames(&self) -> usize {
        let write_index = self.shared.write_index.load(Ordering::Acquire);
        self.shared.distance(self.read_index, write_index)
    }

    fn try_pop_exact(&mut self, output: &mut [f32]) -> Result<(), usize> {
        let frames = output.len() / CHANNELS;
        let available_frames = self.available_frames();
        if available_frames < frames {
            return Err(available_frames);
        }
        for frame in output.chunks_exact_mut(CHANNELS) {
            // SAFETY: availability admission proves that this slot was
            // published by the producer and has not been released for reuse.
            let source = unsafe { *self.shared.frames[self.read_index].get() };
            frame.copy_from_slice(&source);
            self.read_index = self.shared.advance(self.read_index);
        }
        self.shared.read_index.store(self.read_index, Ordering::Release);
        Ok(())
    }

    fn fail(&self) {
        self.shared.state.store(SYSTEM_FAULTED, Ordering::Release);
    }

    fn overflows(&self) -> u64 {
        self.shared.overflows.load(Ordering::Relaxed)
    }
}

#[derive(Debug)]
struct SystemRing {
    frames: Vec<UnsafeCell<[f32; CHANNELS]>>,
    write_index: AtomicUsize,
    read_index: AtomicUsize,
    state: AtomicU8,
    overflows: AtomicU64,
}

impl SystemRing {
    fn slots(&self) -> usize {
        self.frames.len()
    }

    fn capacity_frames(&self) -> usize {
        self.slots() - 1
    }

    fn advance(&self, index: usize) -> usize {
        if index + 1 == self.slots() { 0 } else { index + 1 }
    }

    fn distance(&self, start: usize, end: usize) -> usize {
        if end >= start { end - start } else { self.slots() - start + end }
    }

    fn free_frames(&self, write_index: usize, read_index: usize) -> usize {
        self.capacity_frames() - self.distance(read_index, write_index)
    }
}

// SAFETY: one SystemIngress writes only unpublished slots, and one
// SystemConsumer reads only published slots. Release/acquire index updates
// transfer ownership of every frame.
unsafe impl Sync for SystemRing {}

#[cfg(test)]
mod tests {
    use super::*;
    use librewave_core::{FaderGain, MixRoute, MixTarget};
    use std::alloc::{GlobalAlloc, Layout, System};
    use std::cell::Cell;

    struct CountingAllocator;

    thread_local! {
        static COUNT_ALLOCATOR_OPERATIONS: Cell<bool> = const { Cell::new(false) };
        static ALLOCATOR_OPERATION_COUNT: Cell<usize> = const { Cell::new(0) };
    }

    #[global_allocator]
    static ALLOCATOR: CountingAllocator = CountingAllocator;

    unsafe impl GlobalAlloc for CountingAllocator {
        unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
            record_allocator_operation();
            // SAFETY: the system allocator receives the unchanged valid layout.
            unsafe { System.alloc(layout) }
        }

        unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
            record_allocator_operation();
            // SAFETY: the system allocator receives the unchanged valid layout.
            unsafe { System.alloc_zeroed(layout) }
        }

        unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
            record_allocator_operation();
            // SAFETY: the pointer and layout came from the system allocator.
            unsafe { System.dealloc(pointer, layout) };
        }

        unsafe fn realloc(&self, pointer: *mut u8, layout: Layout, size: usize) -> *mut u8 {
            record_allocator_operation();
            // SAFETY: the pointer and layout came from the system allocator,
            // and the requested size is forwarded unchanged.
            unsafe { System.realloc(pointer, layout, size) }
        }
    }

    fn record_allocator_operation() {
        if COUNT_ALLOCATOR_OPERATIONS.try_with(Cell::get).unwrap_or(false) {
            let _ = ALLOCATOR_OPERATION_COUNT.try_with(|count| count.set(count.get() + 1));
        }
    }

    fn count_allocator_operations<T>(operation: impl FnOnce() -> T) -> (T, usize) {
        ALLOCATOR_OPERATION_COUNT.with(|count| count.set(0));
        COUNT_ALLOCATOR_OPERATIONS.with(|enabled| enabled.set(true));
        let result = operation();
        COUNT_ALLOCATOR_OPERATIONS.with(|enabled| enabled.set(false));
        let count = ALLOCATOR_OPERATION_COUNT.with(Cell::get);
        (result, count)
    }

    fn pcm(period_frames: u32) -> PhysicalPcmConfig {
        PhysicalPcmConfig {
            sample_format: PhysicalSampleFormat::Signed32LittleEndian,
            rate: 48_000,
            channels: 2,
            period_frames,
            buffer_frames: period_frames * 2,
        }
    }

    fn bytes(samples: &[i32]) -> Vec<u8> {
        samples.iter().flat_map(|sample| sample.to_le_bytes()).collect()
    }

    fn new_transport(
        maximum_frames: u32,
        system_capacity_frames: usize,
    ) -> (CaptureClockTransport, TransportHandles) {
        CaptureClockTransport::new(
            pcm(maximum_frames),
            &MixerProfile::default(),
            system_capacity_frames,
        )
        .expect("valid transport")
    }

    #[test]
    fn construction_rejects_format_capacity_and_profile_boundaries() {
        for invalid_pcm in [
            PhysicalPcmConfig { rate: 44_100, ..pcm(4) },
            PhysicalPcmConfig { channels: 1, ..pcm(4) },
        ] {
            assert!(matches!(
                CaptureClockTransport::new(invalid_pcm, &MixerProfile::default(), 4),
                Err(TransportBuildError::PhysicalFormat)
            ));
        }

        assert!(matches!(
            CaptureClockTransport::new(pcm(4), &MixerProfile::default(), 3),
            Err(TransportBuildError::SystemCapacity { minimum_frames: 4, actual_frames: 3 })
        ));

        let invalid_profile =
            MixerProfile { microphone_source: SYSTEM_SOURCE_ID, ..MixerProfile::default() };
        assert!(matches!(
            CaptureClockTransport::new(pcm(4), &invalid_profile, 4),
            Err(TransportBuildError::Profile(MixerProfileError::MicrophoneSource))
        ));
    }

    fn assert_close(actual: &[f32], expected: &[f32]) {
        assert_eq!(actual.len(), expected.len());
        for (actual, expected) in actual.iter().zip(expected) {
            assert!((actual - expected).abs() <= 1.0e-6, "expected {expected}, got {actual}");
        }
    }

    #[test]
    fn s32le_conversion_is_deterministic_and_saturating() {
        let raw = [i32::MIN, -1_073_741_824, 0, 1_073_741_824, i32::MAX];
        let mut decoded = [0.0; 5];
        decode_s32le(&bytes(&raw), &mut decoded);
        assert_close(&decoded, &[-1.0, -0.5, 0.0, 0.5, 1.0]);

        let samples = [-2.0, -1.0, -0.5, 0.0, 0.5, 1.0, 2.0];
        let mut encoded = [0_u8; 28];
        encode_s32le(&samples, &mut encoded).expect("finite samples");
        assert_eq!(
            encoded.as_slice(),
            bytes(&[i32::MIN, i32::MIN, -1_073_741_824, 0, 1_073_741_824, i32::MAX, i32::MAX,])
        );
        assert_eq!(
            encode_s32le(&[f32::NAN], &mut [0; 4]),
            Err(TransportError::NonFinitePlayback { sample_index: 0 })
        );
    }

    #[test]
    fn idle_system_is_semantic_silence_and_capture_reaches_all_outputs() {
        let (mut transport, mut handles) = new_transport(4, 8);
        assert_eq!(handles.meters.try_take(), None);
        let capture = bytes(&[536_870_912, -536_870_912, 1_073_741_824, -1_073_741_824]);
        let result = transport.process_capture(&capture, None).expect("preflight block");
        assert_eq!(
            result,
            ProcessedCapture {
                frames: 2,
                delivery: ProcessedDelivery::Preflight,
                control_update_applied: false,
            }
        );
        let expected = [0.25, -0.25, 0.5, -0.5];
        assert_eq!(transport.last_output(EndpointId::Microphone), Some(expected.as_slice()));
        assert_eq!(transport.last_output(EndpointId::MonitorMix), Some(expected.as_slice()));
        assert_eq!(transport.last_output(EndpointId::StreamMix), Some(expected.as_slice()));
        assert_eq!(transport.last_output(EndpointId::System), None);
        assert!(handles.meters.try_take().is_some());
        assert_eq!(
            transport.counters(),
            TransportCounters {
                capture_blocks: 1,
                processed_blocks: 1,
                preflight_blocks: 1,
                idle_system_silence_blocks: 1,
                ..TransportCounters::default()
            }
        );
    }

    #[test]
    fn system_fifo_bridges_quantum_sizes_and_active_underflow_is_sticky() {
        let (mut transport, TransportHandles { mut system_ingress, .. }) = new_transport(4, 8);
        system_ingress.activate_with_first_block(&[0.125, -0.125]).expect("one frame quantum");
        system_ingress
            .try_push_interleaved(&[0.25, -0.25, 0.375, -0.375])
            .expect("two frame quantum");
        let capture = bytes(&[0, 0, 0, 0]);
        transport.process_capture(&capture, None).expect("two frames are available");
        assert_close(
            transport.last_output(EndpointId::MonitorMix).expect("monitor output"),
            &[0.125, -0.125, 0.25, -0.25],
        );
        assert_eq!(
            transport.last_output(EndpointId::Microphone),
            Some([0.0, 0.0, 0.0, 0.0].as_slice())
        );
        assert_close(
            transport.last_output(EndpointId::StreamMix).expect("stream output"),
            &[0.125, -0.125, 0.25, -0.25],
        );

        assert_eq!(
            transport.process_capture(&capture, None),
            Err(TransportError::SystemUnderflow { required_frames: 2, available_frames: 1 })
        );
        assert_eq!(transport.process_capture(&capture, None), Err(TransportError::Failed));
        assert_eq!(transport.counters().system_underflows, 1);
    }

    #[test]
    fn system_fifo_overflow_is_bounded_and_faults_the_attempt() {
        let (mut transport, TransportHandles { mut system_ingress, .. }) = new_transport(2, 8);
        system_ingress
            .activate_with_first_block(&[0.0; 14])
            .expect("activate with seven queued frames");
        let error = system_ingress
            .try_push_interleaved(&[0.0; 4])
            .expect_err("two frames do not fit in one remaining frame");
        assert_eq!(
            error,
            SystemIngressError::Overflow {
                requested_frames: 2,
                remaining_frames: 1,
                capacity_frames: 8,
            }
        );
        assert_eq!(
            error.to_string(),
            "System ingress requested 2 frames; 1 of 8 FIFO frames remained"
        );
        assert_eq!(
            transport.process_capture(&bytes(&[0, 0]), None),
            Err(TransportError::SystemIngressFaulted)
        );
        assert_eq!(transport.counters().system_overflows, 1);
    }

    #[test]
    fn full_meter_slot_drops_newer_observation_and_counts_overflow() {
        let (mut transport, TransportHandles { mut meters, .. }) = new_transport(1, 1);
        transport
            .process_capture(&bytes(&[536_870_912, -536_870_912]), None)
            .expect("publish the older actual meter block");
        transport
            .process_capture(&bytes(&[1_073_741_824, -1_073_741_824]), None)
            .expect("drop the newer meter block without failing processing");

        assert_eq!(transport.counters().meter_overflows, 1);
        let older = meters.try_take().expect("older observation remains pending");
        let microphone = older.endpoint(EndpointId::Microphone).expect("microphone meter");
        assert_close(&[microphone.left().peak(), microphone.right().peak()], &[0.25, 0.25]);
        assert!(microphone.left().peak() > 0.0);
        assert_eq!(meters.try_take(), None);
    }

    #[test]
    fn observed_system_fault_suppresses_a_new_playback_block() {
        let (mut transport, TransportHandles { mut system_ingress, .. }) = new_transport(1, 1);
        transport
            .process_capture(&bytes(&[0, 0]), None)
            .expect("confirm capture with semantic System silence");
        system_ingress.activate_with_first_block(&[0.0, 0.0]).expect("activate System");
        assert!(matches!(
            system_ingress.try_push_interleaved(&[0.0; 4]),
            Err(SystemIngressError::Overflow { .. })
        ));

        let mut playback = FakePlayback::default();
        assert_eq!(
            transport.process_capture(&bytes(&[0, 0]), Some(&mut playback)),
            Err(TransportError::SystemIngressFaulted)
        );
        assert_eq!(playback.writes, 0);
        assert_eq!(transport.counters().playback_blocks, 0);
    }

    #[test]
    fn system_activation_publishes_initial_frames_before_active_state() {
        let (mut transport, TransportHandles { mut system_ingress, .. }) = new_transport(1, 2);
        assert_eq!(
            system_ingress.activate_with_first_block(&[]),
            Err(SystemIngressError::EmptyActivation)
        );
        transport
            .process_capture(&bytes(&[0, 0]), None)
            .expect("rejected activation leaves semantic idle silence");

        system_ingress
            .activate_with_first_block(&[0.25, -0.25])
            .expect("initial frames precede active publication");
        transport
            .process_capture(&bytes(&[0, 0]), None)
            .expect("active state has its initial frame");
        assert_eq!(transport.last_output(EndpointId::MonitorMix), Some([0.25, -0.25].as_slice()));
        assert_eq!(transport.counters().system_underflows, 0);
    }

    #[test]
    fn system_fifo_capacity_overflow_is_a_build_error() {
        assert!(matches!(
            CaptureClockTransport::new(pcm(1), &MixerProfile::default(), usize::MAX,),
            Err(TransportBuildError::SystemCapacityOverflow)
        ));
    }

    #[derive(Debug, Default)]
    struct FakePlayback {
        bytes: [u8; 32],
        length: usize,
        writes: usize,
    }

    impl TransportPlayback for FakePlayback {
        fn write_interleaved_s32le(
            &mut self,
            _frames: usize,
            bytes: &[u8],
        ) -> Result<(), TransportPlaybackError> {
            self.bytes[..bytes.len()].copy_from_slice(bytes);
            self.length = bytes.len();
            self.writes += 1;
            Ok(())
        }
    }

    #[derive(Debug)]
    struct FaultingPlayback {
        system_ingress: SystemIngress,
        writes: usize,
    }

    #[derive(Debug)]
    struct FailingPlayback {
        error: TransportPlaybackError,
        writes: usize,
    }

    impl TransportPlayback for FailingPlayback {
        fn write_interleaved_s32le(
            &mut self,
            _frames: usize,
            _bytes: &[u8],
        ) -> Result<(), TransportPlaybackError> {
            self.writes += 1;
            Err(self.error)
        }
    }

    impl TransportPlayback for FaultingPlayback {
        fn write_interleaved_s32le(
            &mut self,
            _frames: usize,
            _bytes: &[u8],
        ) -> Result<(), TransportPlaybackError> {
            self.writes += 1;
            assert!(matches!(
                self.system_ingress.try_push_interleaved(&[0.0; 4]),
                Err(SystemIngressError::Overflow { .. })
            ));
            Ok(())
        }
    }

    #[test]
    fn playback_stays_preflight_until_an_earlier_capture_block_is_confirmed() {
        let (mut transport, _) = new_transport(2, 2);
        let capture = bytes(&[536_870_912, -536_870_912]);
        let mut playback = FakePlayback::default();
        assert_eq!(
            transport.process_capture(&capture, Some(&mut playback)),
            Ok(ProcessedCapture {
                frames: 1,
                delivery: ProcessedDelivery::Preflight,
                control_update_applied: false,
            })
        );
        assert_eq!(playback.writes, 0);
        assert_eq!(
            transport.process_capture(&capture, Some(&mut playback)),
            Ok(ProcessedCapture {
                frames: 1,
                delivery: ProcessedDelivery::PhysicalPlayback,
                control_update_applied: false,
            })
        );
        assert_eq!(playback.writes, 1);
        assert_eq!(&playback.bytes[..playback.length], capture.as_slice());
        assert_eq!(transport.counters().preflight_blocks, 1);
        assert_eq!(transport.counters().playback_blocks, 1);
    }

    #[test]
    fn block_already_in_playback_may_finish_when_system_faults() {
        let (mut transport, TransportHandles { mut system_ingress, .. }) = new_transport(1, 1);
        system_ingress.activate_with_first_block(&[0.0, 0.0]).expect("activate System");
        transport.process_capture(&bytes(&[0, 0]), None).expect("preflight block");
        system_ingress.try_push_interleaved(&[0.0, 0.0]).expect("next System frame");
        let mut playback = FaultingPlayback { system_ingress, writes: 0 };

        assert_eq!(
            transport.process_capture(&bytes(&[0, 0]), Some(&mut playback)),
            Ok(ProcessedCapture {
                frames: 1,
                delivery: ProcessedDelivery::PhysicalPlayback,
                control_update_applied: false,
            })
        );
        assert_eq!(playback.writes, 1);
        assert_eq!(transport.counters().playback_blocks, 1);
        assert_eq!(
            transport.process_capture(&bytes(&[0, 0]), None),
            Err(TransportError::SystemIngressFaulted)
        );
        assert_eq!(transport.process_capture(&bytes(&[0, 0]), None), Err(TransportError::Failed));
    }

    #[test]
    fn playback_failures_are_classified_and_sticky() {
        for error in [
            TransportPlaybackError::ShortWrite,
            TransportPlaybackError::XrunRecoveryFailed,
            TransportPlaybackError::Disconnected,
            TransportPlaybackError::Failed,
        ] {
            let (mut transport, _) = new_transport(1, 1);
            let capture = bytes(&[0, 0]);
            transport.process_capture(&capture, None).expect("preflight block");
            let mut playback = FailingPlayback { error, writes: 0 };

            assert_eq!(
                transport.process_capture(&capture, Some(&mut playback)),
                Err(TransportError::Playback(error))
            );
            assert_eq!(playback.writes, 1);
            assert_eq!(transport.counters().playback_failures, 1);
            assert_eq!(transport.process_capture(&capture, None), Err(TransportError::Failed));
        }
    }

    #[test]
    fn profile_control_update_applies_at_one_block_boundary() {
        let (mut transport, TransportHandles { mut control, .. }) = new_transport(2, 2);
        let profile = MixerProfile::default()
            .with_route(
                MICROPHONE_SOURCE_ID,
                MixTarget::Monitor,
                MixRoute::new(false, FaderGain::UNITY),
            )
            .expect("current source");
        control.try_stage_profile(&profile).expect("stage complete current profile");
        let result = transport
            .process_capture(&bytes(&[536_870_912, -536_870_912]), None)
            .expect("process block");
        assert!(result.control_update_applied);
        assert_eq!(transport.last_output(EndpointId::Microphone), Some([0.25, -0.25].as_slice()));
        assert_eq!(transport.last_output(EndpointId::MonitorMix), Some([0.0, 0.0].as_slice()));
    }

    #[test]
    fn capture_lengths_are_exact_and_failed_validation_is_sticky() {
        let (mut transport, _) = new_transport(2, 2);
        assert_eq!(transport.process_capture(&[], None), Err(TransportError::NoCaptureProgress));
        assert_eq!(
            transport.process_capture(&[0; 7], None),
            Err(TransportError::PartialCaptureFrame { bytes: 7 })
        );
        assert_eq!(transport.process_capture(&bytes(&[0, 0]), None), Err(TransportError::Failed));

        let (mut transport, _) = new_transport(2, 2);
        assert_eq!(
            transport.process_capture(&bytes(&[0; 6]), None),
            Err(TransportError::CaptureBlockTooLarge { maximum_frames: 2, actual_frames: 3 })
        );
    }

    #[test]
    fn capture_processing_does_not_allocate_or_free() {
        let (mut transport, _) = new_transport(2, 2);
        let capture = bytes(&[536_870_912, -536_870_912]);

        let (result, operations) =
            count_allocator_operations(|| transport.process_capture(&capture, None));

        assert!(result.is_ok());
        assert_eq!(operations, 0);
    }
}
