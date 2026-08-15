//! Portable identity types and read-only sessions for supported Wave hardware.

use std::fmt;

pub use librewave_protocol::{
    ApiVersion, CodecError, DeviceModel, SchemaError, SetupError, SetupPacket,
};

mod session;

pub use session::{
    CONTROL_TRANSFER_TIMEOUT, ReadOnlyTransport, SessionError, SessionSchemaError, TransportError,
    Wave3Session, probe_wave3,
};

/// A USB vendor and product pair.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct UsbIdentity {
    /// The USB vendor identifier.
    pub vendor_id: u16,
    /// The USB product identifier.
    pub product_id: u16,
}

impl UsbIdentity {
    /// Creates a USB identity from the numeric identifiers in a descriptor.
    #[must_use]
    pub const fn new(vendor_id: u16, product_id: u16) -> Self {
        Self { vendor_id, product_id }
    }
}

impl fmt::Display for UsbIdentity {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{:04x}:{:04x}", self.vendor_id, self.product_id)
    }
}

/// A sanitized, portable device identity.
///
/// This type deliberately has no USB serial field. Platform adapters must not
/// add a serial number to this identity or expose one through formatting,
/// diagnostics, or persistence.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct DeviceIdentity {
    /// The reviewed USB identity.
    usb: UsbIdentity,
    /// The admitted product family.
    model: DeviceModel,
}

impl DeviceIdentity {
    /// Returns the reviewed Wave:3 identity.
    ///
    /// Product identities are intentionally created through reviewed
    /// constructors instead of a generic public constructor.
    #[must_use]
    pub const fn wave3() -> Self {
        Self { usb: UsbIdentity::new(0x0fd9, 0x0070), model: DeviceModel::Wave3 }
    }

    /// Returns the reviewed USB identity.
    #[must_use]
    pub const fn usb(&self) -> UsbIdentity {
        self.usb
    }

    /// Returns the admitted product family.
    #[must_use]
    pub const fn model(&self) -> DeviceModel {
        self.model
    }
}

impl fmt::Display for DeviceIdentity {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{} ({})", self.model, self.usb)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reviewed_wave3_constructor_exposes_only_reviewed_identity() {
        let identity = DeviceIdentity::wave3();
        assert_eq!(identity.usb(), UsbIdentity::new(0x0fd9, 0x0070));
        assert_eq!(identity.model(), DeviceModel::Wave3);
    }
}
