use core::fmt;
use serde::{Deserialize, Serialize};

/// A device API version reported by the device's version request.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
pub struct ApiVersion {
    pub major: u8,
    pub minor: u8,
}

impl ApiVersion {
    #[must_use]
    pub const fn new(major: u8, minor: u8) -> Self {
        Self { major, minor }
    }
}

impl fmt::Display for ApiVersion {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}.{}", self.major, self.minor)
    }
}

/// A device family admitted by this crate.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
pub enum DeviceModel {
    Wave3,
    Unknown,
}

impl fmt::Display for DeviceModel {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Wave3 => formatter.write_str("Wave:3"),
            Self::Unknown => formatter.write_str("unknown"),
        }
    }
}

/// The message path family used by the Wave protocol.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum MessageKind {
    Config,
    Status,
    Version,
    Unknown,
}

impl MessageKind {
    #[must_use]
    pub const fn path(self) -> &'static str {
        match self {
            Self::Config => "/config",
            Self::Status => "/status",
            Self::Version => "/version",
            Self::Unknown => "/unknown",
        }
    }
}

impl fmt::Display for MessageKind {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.path())
    }
}

/// The protocol identity of one message path.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct MessageIdentity {
    pub kind: MessageKind,
    pub id: u8,
}

impl MessageIdentity {
    #[must_use]
    pub const fn new(kind: MessageKind, id: u8) -> Self {
        Self { kind, id }
    }
}

/// All values needed to admit a message schema exactly.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct SchemaIdentity {
    pub model: DeviceModel,
    pub api: ApiVersion,
    pub message: MessageIdentity,
    pub payload_size: usize,
}

impl SchemaIdentity {
    #[must_use]
    pub const fn new(
        model: DeviceModel,
        api: ApiVersion,
        message: MessageIdentity,
        payload_size: usize,
    ) -> Self {
        Self { model, api, message, payload_size }
    }
}

/// The direction of a protocol data transfer.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Direction {
    In,
    Out,
}
