//! Application identities and transient routing observations.
use crate::{InputName, MixerInactiveReason, SourceId};
use serde::{Deserialize, Serialize};

/// Stable properties supplied by a playback application, never a process ID.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(deny_unknown_fields)]
pub enum ApplicationId {
    /// The application declares a desktop application identifier.
    Desktop(InputName),
    /// No desktop identifier exists; use the declared executable name.
    Executable(InputName),
}

/// Valid only for one daemon audio attempt and registry revision.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ApplicationGeneration {
    /// Random daemon process identity.
    pub daemon: [u8; 16],
    /// Monotonic daemon audio attempt identity.
    pub epoch: u64,
    /// Registry and metadata observation revision.
    pub revision: u64,
}

/// One group of currently observed playback streams.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Application {
    /// Stable group key.
    pub id: ApplicationId,
    /// Application-provided display name, with the identity as fallback.
    pub name: InputName,
    /// Number of currently observed streams in this group.
    pub streams: usize,
    /// A stream explicitly forbids movement, or has an invalid movement property.
    pub move_blocked: bool,
    /// All observed audio output ports link exclusively to this input.
    pub source: Option<SourceId>,
}

/// Session-policy availability. Unknown or missing policy blocks movement.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum ApplicationMovement {
    Allowed,
    Disabled,
    Unavailable,
}

/// A move request remains pending until actual links match every selected stream.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub enum ApplicationRoute {
    /// The session policy has not yet linked every output port.
    Pending { application: ApplicationId, source: SourceId },
    /// Actual links match the requested input.
    Verified { application: ApplicationId, source: SourceId },
    /// A failed move is waiting for conditional restoration acknowledgement.
    Restoring { application: ApplicationId, reason: String },
    /// The move failed. The reason states any unconfirmed restoration.
    Failed { application: ApplicationId, reason: String },
}

/// Transient registry data. It does not change device or mixer generations.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub enum ApplicationSnapshot {
    /// The daemon has no ready audio graph.
    Unavailable { reason: MixerInactiveReason },
    /// Playback streams observed on the daemon-owned graph connection.
    Ready {
        generation: ApplicationGeneration,
        applications: Vec<Application>,
        unidentified_streams: usize,
        movement: ApplicationMovement,
        route: Option<ApplicationRoute>,
    },
}

/// A saved application destination. System receives unassigned applications.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ApplicationAssignment {
    pub application: ApplicationId,
    pub source: SourceId,
}
