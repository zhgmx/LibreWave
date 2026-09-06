//! Direct ALSA ownership and `PipeWire` graph inspection for the Linux host.
//!
//! The host opens only a revalidated PCM on the card correlated to the admitted
//! USB candidate. It consumes capture on a bounded worker before opening
//! playback. An offline graph-clock bridge tests the independent capture and
//! playback clock boundaries. The production `PipeWire` adapter cannot create
//! endpoint streams yet, so it refuses publication and never substitutes silence.

use self::alsa::{RealAlsaFacade, SelectedPcmCard};
use self::pipewire::RealPipeWireFacade;
use self::worker::{CaptureAtomicState, CaptureFailure, CaptureWorker};
use crate::UsbDeviceCandidate;
use crate::audio_lifecycle::{
    AudioHost, CaptureConsumption, CaptureObservation, HIDDEN_PHYSICAL_NODES, HiddenPhysicalNode,
};
use librewave_core::{DELIBERATE_ENDPOINTS, EndpointFlow, EndpointId};
use std::fmt;
use std::thread;
use std::time::{Duration, Instant};

mod alsa;
mod clock_bridge;
mod pcm;
mod pipewire;
#[cfg_attr(not(test), allow(dead_code))]
mod pipewire_filter_ffi;
#[cfg_attr(not(test), allow(dead_code))]
mod pipewire_graph;
mod worker;

pub use clock_bridge::{
    BridgeBuildError, BridgeClockDomain, BridgeControlError, BridgeError, BridgeState,
    CaptureClockObservation, CaptureIngress, CapturePublishReport, ClockBridgeConfig,
    ClockBridgeControl, ClockBridgeParts, DirectionBridgeConfig, GraphBoundaryDelivery,
    GraphClockObservation, GraphClockObservationError, GraphClockProcessor, GraphOutputBuffers,
    GraphProcessReport, GraphSystemInput, ObservationPublication, PlaybackBoundaryDelivery,
    PlaybackClockObservation, PlaybackEgress, PlaybackPeriodSubmitter, PlaybackProcessReport,
    PlaybackSubmissionProgress, PlaybackWriteError,
};
pub use pcm::{
    PACKED_S24_SAMPLE_BYTES, PackedS24Error, PcmBufferConfig, PcmConfigError, PcmDirection,
    WAVE3_CAPTURE_CHANNELS, WAVE3_PCM_RATE_HZ, WAVE3_PLAYBACK_CHANNELS, Wave3PhysicalIoConfig,
};

#[cfg(test)]
mod tests;

const CAPTURE_OBSERVATION_POLL: Duration = Duration::from_millis(1);

/// `PipeWire`'s direction for one deliberate endpoint.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PipeWireEndpointDirection {
    /// The node receives frames rendered by applications.
    Input,
    /// The node produces frames for applications to capture.
    Output,
}

/// A complete, inspectable plan for one public `PipeWire` endpoint.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct EndpointPlan {
    /// Portable endpoint identity.
    pub endpoint: EndpointId,
    /// Direction derived from [`EndpointFlow`].
    pub direction: PipeWireEndpointDirection,
    /// Stable `PipeWire` node name.
    pub node_name: &'static str,
    /// Human-readable node description.
    pub node_description: &'static str,
    /// Explicit `PipeWire` media class.
    pub media_class: &'static str,
    /// Whether `node.virtual` is true.
    pub node_virtual: bool,
}

/// Builds endpoint plans from the portable allowlist and flow contract.
///
/// # Errors
///
/// Returns an error for a duplicate endpoint or an endpoint outside
/// [`DELIBERATE_ENDPOINTS`].
pub fn endpoint_plans(endpoints: &[EndpointId]) -> Result<Vec<EndpointPlan>, LinuxAudioError> {
    let mut plans = Vec::with_capacity(endpoints.len());
    for endpoint in endpoints {
        if !DELIBERATE_ENDPOINTS.contains(endpoint) {
            return Err(LinuxAudioError::EndpointNotAllowed(*endpoint));
        }
        if plans.iter().any(|plan: &EndpointPlan| plan.endpoint == *endpoint) {
            return Err(LinuxAudioError::DuplicateEndpoint(*endpoint));
        }
        let direction = match endpoint.flow() {
            EndpointFlow::PublicSink => PipeWireEndpointDirection::Input,
            EndpointFlow::PublicSource => PipeWireEndpointDirection::Output,
        };
        let (node_name, node_description) = match endpoint {
            EndpointId::System => ("librewave.system", "LibreWave System"),
            EndpointId::Microphone => ("librewave.microphone", "LibreWave Microphone"),
            EndpointId::MonitorMix => ("librewave.monitor-mix", "LibreWave Monitor Mix"),
            EndpointId::StreamMix => ("librewave.stream-mix", "LibreWave Stream Mix"),
        };
        plans.push(EndpointPlan {
            endpoint: *endpoint,
            direction,
            node_name,
            node_description,
            media_class: match endpoint.flow() {
                EndpointFlow::PublicSink => "Audio/Sink",
                EndpointFlow::PublicSource => "Audio/Source",
            },
            node_virtual: true,
        });
    }
    Ok(plans)
}

/// Coarse resource activity without ALSA card numbers or `PipeWire` object IDs.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ResourceActivity {
    /// No resource is owned.
    Closed,
    /// The resource exists but is not ready to carry frames.
    Starting,
    /// The processing path is active.
    Active,
    /// The resource failed and requires teardown.
    Failed,
}

/// Why a graph cannot be reported as ready.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GraphNotReady {
    /// The production adapter cannot create frame-carrying endpoint streams.
    EndpointStreamTransportUnavailable,
    /// Independent ALSA and `PipeWire` clocks do not have a synchronization policy.
    IndependentClockSynchronizationUnresolved,
}

/// Inspectable production resource state for reconciliation and diagnostics.
///
/// This state contains portable endpoint identities and aggregate counters. It
/// does not expose card numbers, PCM names, USB topology, or `PipeWire` object IDs.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AudioResourceState {
    /// Whether the host owns a `PipeWire` core connection resource.
    pub pipewire_connection_owned: bool,
    /// Physical capture state.
    pub capture: ResourceActivity,
    /// Physical playback state.
    pub playback: ResourceActivity,
    /// Total frames consumed by the current capture worker.
    pub capture_frames: u64,
    /// Total xruns recovered by the current capture worker.
    pub recovered_xruns: u64,
    /// Endpoints created by the current host attempt.
    pub published_endpoints: Vec<EndpointId>,
    /// Explicit readiness boundary, when present.
    pub not_ready: Option<GraphNotReady>,
}

impl Default for AudioResourceState {
    fn default() -> Self {
        Self {
            pipewire_connection_owned: false,
            capture: ResourceActivity::Closed,
            playback: ResourceActivity::Closed,
            capture_frames: 0,
            recovered_xruns: 0,
            published_endpoints: Vec::new(),
            not_ready: None,
        }
    }
}

/// A classified Linux audio host failure.
#[derive(Debug, Eq, PartialEq)]
pub enum LinuxAudioError {
    /// The caller supplied an incomplete or internally inconsistent PCM format.
    InvalidPcmConfig,
    /// The capture confirmation timeout is zero or cannot form a deadline.
    InvalidCaptureTimeout,
    /// The admitted candidate did not identify exactly one ALSA card.
    AmbiguousAlsaCard,
    /// The candidate had no stable ALSA card identifier.
    MissingAlsaCardId,
    /// The ALSA card identifier cannot be interpolated into a safe PCM name.
    InvalidAlsaCardId,
    /// The candidate no longer resolves to the same USB topology and ALSA card.
    AlsaCardChanged,
    /// The selected ALSA card has zero or multiple PCMs for a required direction.
    AmbiguousPcmDirection { direction: &'static str },
    /// A safe ALSA wrapper operation failed.
    Alsa(String),
    /// ALSA did not apply packed signed `S24_3LE` for the requested direction.
    UnsupportedPcmFormat { direction: PcmDirection },
    /// ALSA adjusted the exact rate, channels, period, buffer, or access mode.
    PcmConfigurationAdjusted { direction: PcmDirection },
    /// Capture did not produce a frame before the configured deadline.
    CaptureTimedOut,
    /// The worker buffer could not be allocated before capture started.
    CaptureBufferAllocation,
    /// The operating system could not create the capture worker thread.
    CaptureWorkerSpawn(String),
    /// The capture device disconnected.
    CaptureDisconnected,
    /// The capture worker stopped after an unrecoverable PCM failure.
    CaptureWorkerFailed,
    /// The capture worker panicked while it owned the PCM.
    CaptureWorkerPanicked,
    /// Playback opened before a confirmed capture frame.
    CaptureNotConfirmed,
    /// A `PipeWire` wrapper operation failed.
    PipeWire(String),
    /// `PipeWire` still exposes the admitted physical card or one of its nodes.
    PhysicalNodeExposed,
    /// The lifecycle requested an unknown set of physical nodes to hide.
    InvalidHiddenPhysicalNodeContract,
    /// A requested endpoint is not part of the portable product contract.
    EndpointNotAllowed(EndpointId),
    /// An endpoint appeared more than once in one publication request.
    DuplicateEndpoint(EndpointId),
    /// The production adapter cannot create frame-carrying endpoint streams.
    EndpointStreamTransportUnavailable,
    /// One or more owned resources could not be closed.
    TeardownFailed,
}

impl fmt::Display for LinuxAudioError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidPcmConfig => formatter.write_str("the physical PCM format is invalid"),
            Self::InvalidCaptureTimeout => {
                formatter.write_str("the capture confirmation timeout is invalid")
            }
            Self::AmbiguousAlsaCard => {
                formatter.write_str("the admitted USB device does not identify one ALSA card")
            }
            Self::MissingAlsaCardId => {
                formatter.write_str("the admitted ALSA card has no stable identifier")
            }
            Self::InvalidAlsaCardId => {
                formatter.write_str("the admitted ALSA card identifier is invalid")
            }
            Self::AlsaCardChanged => {
                formatter.write_str("the admitted USB-to-ALSA correlation changed before open")
            }
            Self::AmbiguousPcmDirection { direction } => {
                write!(formatter, "the ALSA card does not have one {direction} PCM")
            }
            Self::Alsa(message) => write!(formatter, "ALSA failed: {message}"),
            Self::UnsupportedPcmFormat { direction } => {
                write!(formatter, "ALSA did not apply S24_3LE for {direction:?}")
            }
            Self::PcmConfigurationAdjusted { direction } => {
                write!(formatter, "ALSA adjusted the required {direction:?} PCM configuration")
            }
            Self::CaptureTimedOut => {
                formatter.write_str("capture produced no frame before timeout")
            }
            Self::CaptureBufferAllocation => {
                formatter.write_str("the capture worker buffer could not be allocated")
            }
            Self::CaptureWorkerSpawn(message) => {
                write!(formatter, "the capture worker could not start: {message}")
            }
            Self::CaptureDisconnected => formatter.write_str("capture disconnected"),
            Self::CaptureWorkerFailed => formatter.write_str("capture stopped after a PCM failure"),
            Self::CaptureWorkerPanicked => formatter.write_str("the capture worker panicked"),
            Self::CaptureNotConfirmed => {
                formatter.write_str("playback requires one confirmed capture frame")
            }
            Self::PipeWire(message) => write!(formatter, "PipeWire failed: {message}"),
            Self::PhysicalNodeExposed => {
                formatter.write_str("PipeWire exposes the admitted physical Wave card")
            }
            Self::InvalidHiddenPhysicalNodeContract => {
                formatter.write_str("the physical-node ownership contract is invalid")
            }
            Self::EndpointNotAllowed(endpoint) => {
                write!(formatter, "endpoint {endpoint:?} is not in the product allowlist")
            }
            Self::DuplicateEndpoint(endpoint) => {
                write!(formatter, "endpoint {endpoint:?} appears more than once")
            }
            Self::EndpointStreamTransportUnavailable => {
                formatter.write_str("the endpoint stream transport is unavailable")
            }
            Self::TeardownFailed => formatter.write_str("one or more audio resources stayed open"),
        }
    }
}

impl std::error::Error for LinuxAudioError {}

trait CapturePcm: Send {
    fn read_frames(&mut self, bytes: &mut [u8]) -> CaptureRead;
}

trait PlaybackPcm {
    fn close(&mut self) -> Result<(), LinuxAudioError>;
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum CaptureRead {
    Frames(u64),
    WouldBlock,
    RecoveredXrun,
    Disconnected,
    Failed,
}

trait AlsaFacade {
    fn select(
        &mut self,
        candidate: &UsbDeviceCandidate,
    ) -> Result<SelectedPcmCard, LinuxAudioError>;
    fn start_capture(
        &mut self,
        candidate: &UsbDeviceCandidate,
        selected: &SelectedPcmCard,
    ) -> Result<Box<dyn CapturePcm>, LinuxAudioError>;
    fn open_playback(
        &mut self,
        candidate: &UsbDeviceCandidate,
        selected: &SelectedPcmCard,
    ) -> Result<Box<dyn PlaybackPcm>, LinuxAudioError>;
}

trait PipeWireFacade {
    fn verify_physical_nodes_hidden(
        &mut self,
        candidate: &UsbDeviceCandidate,
    ) -> Result<(), LinuxAudioError>;
    /// Creates endpoints after format negotiation and activates their processing path.
    fn publish_endpoints(&mut self, plans: &[EndpointPlan]) -> Result<(), LinuxAudioError>;
    fn disconnect(&mut self) -> Result<(), LinuxAudioError>;
    fn owns_connection(&self) -> bool;
}

/// The production Linux implementation of [`AudioHost`].
///
/// One value owns the `PipeWire` connection, capture worker, and playback PCM.
/// [`AudioHost::teardown`] is the only normal cleanup path.
pub struct LinuxAudioHost {
    alsa: Box<dyn AlsaFacade>,
    pipewire: Box<dyn PipeWireFacade>,
    capture_timeout: Duration,
    capture_buffer_bytes: usize,
    selection: Option<SelectedPcmCard>,
    capture: Option<CaptureWorker>,
    playback: Option<Box<dyn PlaybackPcm>>,
    playback_active: bool,
    published_endpoints: Vec<EndpointId>,
    not_ready: Option<GraphNotReady>,
}

impl fmt::Debug for LinuxAudioHost {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("LinuxAudioHost")
            .field("capture_timeout", &self.capture_timeout)
            .field("capture_buffer_bytes", &self.capture_buffer_bytes)
            .field("has_selection", &self.selection.is_some())
            .field("has_capture", &self.capture.is_some())
            .field("has_playback", &self.playback.is_some())
            .field("playback_active", &self.playback_active)
            .field("published_endpoints", &self.published_endpoints)
            .field("not_ready", &self.not_ready)
            .finish_non_exhaustive()
    }
}

impl LinuxAudioHost {
    /// Creates a production host with safe ALSA and `PipeWire` wrappers.
    ///
    /// # Errors
    ///
    /// Returns an error when the physical PCM format or capture timeout is invalid.
    pub fn new(
        pcm: Wave3PhysicalIoConfig,
        capture_timeout: Duration,
    ) -> Result<Self, LinuxAudioError> {
        if capture_timeout.is_zero() || Instant::now().checked_add(capture_timeout).is_none() {
            return Err(LinuxAudioError::InvalidCaptureTimeout);
        }
        let capture_buffer_bytes =
            pcm.capture_period_bytes().map_err(|_| LinuxAudioError::InvalidPcmConfig)?;
        Ok(Self::with_facades(
            Box::new(RealAlsaFacade::new(pcm)),
            Box::new(RealPipeWireFacade::default()),
            capture_timeout,
            capture_buffer_bytes,
        ))
    }

    fn with_facades(
        alsa: Box<dyn AlsaFacade>,
        pipewire: Box<dyn PipeWireFacade>,
        capture_timeout: Duration,
        capture_buffer_bytes: usize,
    ) -> Self {
        Self {
            alsa,
            pipewire,
            capture_timeout,
            capture_buffer_bytes,
            selection: None,
            capture: None,
            playback: None,
            playback_active: false,
            published_endpoints: Vec::new(),
            not_ready: None,
        }
    }

    /// Returns a current snapshot of resources owned by this host.
    #[must_use]
    pub fn resource_state(&self) -> AudioResourceState {
        let observation =
            self.capture.as_ref().map_or_else(CaptureAtomicState::default, CaptureWorker::snapshot);
        AudioResourceState {
            pipewire_connection_owned: self.pipewire.owns_connection(),
            capture: observation.activity,
            playback: match (self.playback.is_some(), self.playback_active) {
                (true, true) => ResourceActivity::Active,
                (true, false) => ResourceActivity::Starting,
                (false, _) => ResourceActivity::Closed,
            },
            capture_frames: observation.frames,
            recovered_xruns: observation.recovered_xruns,
            published_endpoints: self.published_endpoints.clone(),
            not_ready: self.not_ready,
        }
    }

    fn capture_observation(&self) -> Result<CaptureObservation, LinuxAudioError> {
        let capture = self.capture.as_ref().ok_or(LinuxAudioError::CaptureWorkerFailed)?;
        let deadline = Instant::now()
            .checked_add(self.capture_timeout)
            .ok_or(LinuxAudioError::InvalidCaptureTimeout)?;
        loop {
            let snapshot = capture.snapshot();
            if snapshot.frames > 0 && snapshot.activity == ResourceActivity::Active {
                return Ok(CaptureObservation { active: true, frames_consumed: snapshot.frames });
            }
            match snapshot.failure {
                Some(CaptureFailure::Disconnected) => {
                    return Err(LinuxAudioError::CaptureDisconnected);
                }
                Some(CaptureFailure::Failed) => {
                    return Err(LinuxAudioError::CaptureWorkerFailed);
                }
                None if Instant::now() < deadline => thread::sleep(CAPTURE_OBSERVATION_POLL),
                None => return Err(LinuxAudioError::CaptureTimedOut),
            }
        }
    }
}

impl AudioHost for LinuxAudioHost {
    type Error = LinuxAudioError;

    fn verify_physical_nodes_hidden(
        &mut self,
        candidate: &UsbDeviceCandidate,
        hidden_nodes: &[HiddenPhysicalNode],
    ) -> Result<(), Self::Error> {
        if hidden_nodes != HIDDEN_PHYSICAL_NODES.as_slice() {
            return Err(LinuxAudioError::InvalidHiddenPhysicalNodeContract);
        }
        self.not_ready = None;
        self.pipewire.verify_physical_nodes_hidden(candidate)
    }

    fn start_capture(
        &mut self,
        candidate: &UsbDeviceCandidate,
    ) -> Result<CaptureConsumption, Self::Error> {
        let selected = self.alsa.select(candidate)?;
        let pcm = self.alsa.start_capture(candidate, &selected)?;
        self.capture = Some(CaptureWorker::start(pcm, self.capture_buffer_bytes)?);
        self.selection = Some(selected);
        Ok(CaptureConsumption::new())
    }

    fn observe_capture(
        &mut self,
        _candidate: &UsbDeviceCandidate,
        _capture: &CaptureConsumption,
    ) -> Result<CaptureObservation, Self::Error> {
        self.capture_observation()
    }

    fn start_playback(&mut self, candidate: &UsbDeviceCandidate) -> Result<(), Self::Error> {
        let observation = self.capture_observation()?;
        if !observation.active || observation.frames_consumed == 0 {
            return Err(LinuxAudioError::CaptureNotConfirmed);
        }
        let selection = self.selection.as_ref().ok_or(LinuxAudioError::AlsaCardChanged)?;
        self.playback = Some(self.alsa.open_playback(candidate, selection)?);
        self.playback_active = false;
        Ok(())
    }

    fn publish_endpoints(
        &mut self,
        _candidate: &UsbDeviceCandidate,
        endpoints: &[EndpointId],
    ) -> Result<(), Self::Error> {
        let plans = endpoint_plans(endpoints)?;
        match self.pipewire.publish_endpoints(&plans) {
            Ok(()) => {
                self.published_endpoints = plans.iter().map(|plan| plan.endpoint).collect();
                self.playback_active = true;
                Ok(())
            }
            Err(LinuxAudioError::EndpointStreamTransportUnavailable) => {
                self.not_ready = Some(GraphNotReady::EndpointStreamTransportUnavailable);
                Err(LinuxAudioError::EndpointStreamTransportUnavailable)
            }
            Err(error) => Err(error),
        }
    }

    fn restore_software_routing(
        &mut self,
        _candidate: &UsbDeviceCandidate,
    ) -> Result<(), Self::Error> {
        if self.published_endpoints.as_slice() != DELIBERATE_ENDPOINTS {
            return Err(LinuxAudioError::EndpointStreamTransportUnavailable);
        }
        Ok(())
    }

    fn teardown(&mut self) -> Result<(), Self::Error> {
        let mut failed = false;
        failed |= self.pipewire.disconnect().is_err();
        if let Some(mut playback) = self.playback.take() {
            failed |= playback.close().is_err();
        }
        self.playback_active = false;
        if let Some(capture) = self.capture.take() {
            failed |= capture.stop().is_err();
        }
        self.selection = None;
        self.published_endpoints.clear();
        if failed { Err(LinuxAudioError::TeardownFailed) } else { Ok(()) }
    }
}

impl Drop for LinuxAudioHost {
    fn drop(&mut self) {
        let _ = self.teardown();
    }
}
