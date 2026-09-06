//! Checked ALSA workers and `PipeWire` graph inspection for the Linux host.
//!
//! The ALSA adapters open only revalidated PCMs on the card correlated to the
//! admitted USB candidate. Fake composition tests the direction-specific workers
//! with the independent-clock bridge. The production host refuses before opening
//! ALSA until the `PipeWire` graph can own that composition.

use self::alsa::SelectedPcmCard;
use self::pipewire::RealPipeWireFacade;
use self::worker::{CapturePcm, PlaybackPcm};
use crate::UsbDeviceCandidate;
use crate::audio_lifecycle::{
    AudioHost, CaptureConsumption, CaptureObservation, HIDDEN_PHYSICAL_NODES, HiddenPhysicalNode,
};
use librewave_core::{DELIBERATE_ENDPOINTS, EndpointFlow, EndpointId};
use std::fmt;

#[allow(
    dead_code,
    reason = "Stage 2 exercises these adapters under fakes; Stage 3 will compose them"
)]
mod alsa;
mod clock_bridge;
mod pcm;
mod pipewire;
#[cfg_attr(not(test), allow(dead_code))]
mod pipewire_filter_ffi;
#[cfg_attr(not(test), allow(dead_code))]
mod pipewire_graph;
#[cfg_attr(not(test), allow(dead_code))]
mod worker;

pub use clock_bridge::{
    BridgeBuildError, BridgeClockDomain, BridgeControlError, BridgeError, BridgeState,
    CaptureClockObservation, CaptureIngress, CapturePublishReport, CaptureReadBoundary,
    ClockBridgeConfig, ClockBridgeControl, ClockBridgeParts, DirectionBridgeConfig,
    GraphBoundaryDelivery, GraphClockObservation, GraphClockObservationError, GraphClockProcessor,
    GraphOutputBuffers, GraphProcessReport, GraphSystemInput, ObservationPublication,
    PlaybackBoundaryDelivery, PlaybackClockObservation, PlaybackEgress, PlaybackPeriodSubmitter,
    PlaybackProcessReport, PlaybackSubmissionProgress, PlaybackWriteError,
};
pub use pcm::{
    PACKED_S24_SAMPLE_BYTES, PackedS24Error, PcmBufferConfig, PcmConfigError, PcmDirection,
    WAVE3_CAPTURE_CHANNELS, WAVE3_PCM_RATE_HZ, WAVE3_PLAYBACK_CHANNELS, Wave3PhysicalIoConfig,
};

#[cfg(test)]
mod tests;

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
            published_endpoints: Vec::new(),
            not_ready: None,
        }
    }
}

/// A classified Linux audio host failure.
#[derive(Debug, Eq, PartialEq)]
pub enum LinuxAudioError {
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
    /// ALSA adjusted the required monotonic timestamp configuration.
    PcmTimestampConfigurationAdjusted { direction: PcmDirection },
    /// ALSA adjusted the playback threshold that prevents automatic start.
    PlaybackStartThresholdAdjusted,
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
            Self::PcmTimestampConfigurationAdjusted { direction } => {
                write!(formatter, "ALSA adjusted the required {direction:?} timestamp mode")
            }
            Self::PlaybackStartThresholdAdjusted => {
                formatter.write_str("ALSA adjusted the playback start threshold")
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

#[allow(dead_code, reason = "the production host refuses before ALSA until Stage 3 composition")]
trait AlsaFacade {
    fn select(
        &mut self,
        candidate: &UsbDeviceCandidate,
    ) -> Result<SelectedPcmCard, LinuxAudioError>;
    fn open_capture(
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
    fn disconnect(&mut self) -> Result<(), LinuxAudioError>;
    fn owns_connection(&self) -> bool;
}

/// The production Linux implementation of [`AudioHost`].
///
/// Stage 2 can inspect `PipeWire`, but it refuses audio startup before ALSA
/// opens until the production graph can own the clock bridge.
pub struct LinuxAudioHost {
    pipewire: Box<dyn PipeWireFacade>,
    not_ready: Option<GraphNotReady>,
}

impl fmt::Debug for LinuxAudioHost {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("LinuxAudioHost")
            .field("not_ready", &self.not_ready)
            .finish_non_exhaustive()
    }
}

impl LinuxAudioHost {
    /// Creates the production host at its Stage 2 readiness boundary.
    #[must_use]
    pub fn new() -> Self {
        Self::with_facade(Box::new(RealPipeWireFacade::default()))
    }

    fn with_facade(pipewire: Box<dyn PipeWireFacade>) -> Self {
        Self { pipewire, not_ready: None }
    }

    /// Returns a current snapshot of resources owned by this host.
    #[must_use]
    pub fn resource_state(&self) -> AudioResourceState {
        AudioResourceState {
            pipewire_connection_owned: self.pipewire.owns_connection(),
            capture: ResourceActivity::Closed,
            playback: ResourceActivity::Closed,
            published_endpoints: Vec::new(),
            not_ready: self.not_ready,
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
        _candidate: &UsbDeviceCandidate,
    ) -> Result<CaptureConsumption, Self::Error> {
        self.not_ready = Some(GraphNotReady::EndpointStreamTransportUnavailable);
        Err(LinuxAudioError::EndpointStreamTransportUnavailable)
    }

    fn observe_capture(
        &mut self,
        _candidate: &UsbDeviceCandidate,
        _capture: &CaptureConsumption,
    ) -> Result<CaptureObservation, Self::Error> {
        Err(LinuxAudioError::EndpointStreamTransportUnavailable)
    }

    fn prepare_playback(&mut self, _candidate: &UsbDeviceCandidate) -> Result<(), Self::Error> {
        Err(LinuxAudioError::EndpointStreamTransportUnavailable)
    }

    fn publish_endpoints(
        &mut self,
        _candidate: &UsbDeviceCandidate,
        endpoints: &[EndpointId],
    ) -> Result<(), Self::Error> {
        endpoint_plans(endpoints)?;
        self.not_ready = Some(GraphNotReady::EndpointStreamTransportUnavailable);
        Err(LinuxAudioError::EndpointStreamTransportUnavailable)
    }

    fn restore_software_routing(
        &mut self,
        _candidate: &UsbDeviceCandidate,
    ) -> Result<(), Self::Error> {
        Err(LinuxAudioError::EndpointStreamTransportUnavailable)
    }

    fn teardown(&mut self) -> Result<(), Self::Error> {
        self.pipewire.disconnect().map_err(|_| LinuxAudioError::TeardownFailed)
    }
}

impl Default for LinuxAudioHost {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for LinuxAudioHost {
    fn drop(&mut self) {
        let _ = self.teardown();
    }
}
