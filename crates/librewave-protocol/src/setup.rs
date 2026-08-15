use crate::{ApiVersion, Direction, MessageSchema};

/// An eight-byte USB control setup packet, without its data stage.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SetupPacket {
    pub request_type: u8,
    pub request: u8,
    pub value: u16,
    pub index: u16,
    pub length: u16,
}

impl SetupPacket {
    #[must_use]
    pub const fn to_bytes(self) -> [u8; 8] {
        let value = self.value.to_le_bytes();
        let index = self.index.to_le_bytes();
        let length = self.length.to_le_bytes();
        [
            self.request_type,
            self.request,
            value[0],
            value[1],
            index[0],
            index[1],
            length[0],
            length[1],
        ]
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SetupError {
    MessageNotReadable,
    MessageNotWritable,
    PayloadTooLarge,
}

impl core::fmt::Display for SetupError {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::MessageNotReadable => formatter.write_str("message is not readable"),
            Self::MessageNotWritable => formatter.write_str("message is not writable"),
            Self::PayloadTooLarge => formatter.write_str("message payload exceeds USB wLength"),
        }
    }
}

impl std::error::Error for SetupError {}

/// Builds the Wave:3 legacy UAC1 version probe.
#[must_use]
pub const fn version_probe(interface: u8) -> SetupPacket {
    SetupPacket {
        request_type: 0xa1,
        request: 0x85,
        value: 0x000a,
        index: 0x3300 | interface as u16,
        length: 2,
    }
}

#[must_use]
pub const fn decode_version_response(response: [u8; 2]) -> ApiVersion {
    ApiVersion::new(response[0], response[1])
}

/// Builds a Wave:3 legacy UAC1 transfer for an already admitted message.
///
/// # Errors
///
/// Returns an error when the direction violates the message access metadata or the payload size
/// cannot fit USB's 16-bit length field.
pub fn message_transfer(
    interface: u8,
    direction: Direction,
    message: &MessageSchema,
) -> Result<SetupPacket, SetupError> {
    match direction {
        Direction::In if !message.access().readable() => {
            return Err(SetupError::MessageNotReadable);
        }
        Direction::Out if !message.access().writable() => {
            return Err(SetupError::MessageNotWritable);
        }
        _ => {}
    }
    let length =
        u16::try_from(message.identity().payload_size).map_err(|_| SetupError::PayloadTooLarge)?;
    Ok(SetupPacket {
        request_type: match direction {
            Direction::In => 0xa1,
            Direction::Out => 0x21,
        },
        request: match direction {
            Direction::In => 0x85,
            Direction::Out => 0x05,
        },
        value: u16::from(message.identity().message.id),
        index: 0x3300 | u16::from(interface),
        length,
    })
}
