use crate::{ApiVersion, DeviceModel, MessageIdentity, MessageKind, SchemaIdentity};

/// Access permissions recovered for a message or field.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Access {
    readable: bool,
    writable: bool,
}

impl Access {
    pub const READ_ONLY: Self = Self { readable: true, writable: false };
    pub const READ_WRITE: Self = Self { readable: true, writable: true };

    #[must_use]
    pub const fn readable(self) -> bool {
        self.readable
    }

    #[must_use]
    pub const fn writable(self) -> bool {
        self.writable
    }
}

/// The wire encoding and constraints for one field.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FieldSpec {
    field: crate::Wave3ConfigField,
    offset: usize,
    codec: FieldCodec,
    access: Access,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum FieldCodec {
    Boolean,
    SignedFixed { fractional_bits: u8, minimum_raw: i32, maximum_raw: i32, step_raw: i32 },
    VolumeSelect,
}

impl FieldSpec {
    pub(crate) const fn boolean(
        field: crate::Wave3ConfigField,
        offset: usize,
        access: Access,
    ) -> Self {
        Self { field, offset, codec: FieldCodec::Boolean, access }
    }

    pub(crate) const fn signed_fixed(
        field: crate::Wave3ConfigField,
        offset: usize,
        fractional_bits: u8,
        minimum_raw: i32,
        maximum_raw: i32,
        step_raw: i32,
        access: Access,
    ) -> Self {
        Self {
            field,
            offset,
            codec: FieldCodec::SignedFixed { fractional_bits, minimum_raw, maximum_raw, step_raw },
            access,
        }
    }

    pub(crate) const fn volume_select(
        field: crate::Wave3ConfigField,
        offset: usize,
        access: Access,
    ) -> Self {
        Self { field, offset, codec: FieldCodec::VolumeSelect, access }
    }

    #[must_use]
    pub const fn field(self) -> crate::Wave3ConfigField {
        self.field
    }

    #[must_use]
    pub const fn offset(self) -> usize {
        self.offset
    }

    #[must_use]
    pub const fn access(self) -> Access {
        self.access
    }

    pub(crate) const fn codec(self) -> FieldCodec {
        self.codec
    }
}

/// A message schema admitted by this crate.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MessageSchema {
    identity: SchemaIdentity,
    access: Access,
    fields: &'static [FieldSpec],
}

impl MessageSchema {
    pub(crate) const fn new(
        identity: SchemaIdentity,
        access: Access,
        fields: &'static [FieldSpec],
    ) -> Self {
        Self { identity, access, fields }
    }

    #[must_use]
    pub const fn identity(self) -> SchemaIdentity {
        self.identity
    }

    #[must_use]
    pub const fn access(self) -> Access {
        self.access
    }

    pub(crate) fn fields(self) -> &'static [FieldSpec] {
        self.fields
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SchemaError {
    UnsupportedModel { model: DeviceModel },
    UnsupportedApi { api: ApiVersion },
    UnsupportedMessage { message: MessageIdentity },
    MessageIdMismatch { expected: u8, actual: u8 },
    PayloadSizeMismatch { expected: usize, actual: usize },
}

impl core::fmt::Display for SchemaError {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::UnsupportedModel { model } => {
                write!(formatter, "unsupported device model {model}")
            }
            Self::UnsupportedApi { api } => write!(formatter, "unsupported API version {api}"),
            Self::UnsupportedMessage { message } => {
                write!(formatter, "unsupported message {}", message.kind)
            }
            Self::MessageIdMismatch { expected, actual } => {
                write!(formatter, "message ID 0x{actual:02x} does not match 0x{expected:02x}")
            }
            Self::PayloadSizeMismatch { expected, actual } => {
                write!(formatter, "payload size {actual} does not match {expected}")
            }
        }
    }
}

impl std::error::Error for SchemaError {}

const WAVE3_API_5_3: ApiVersion = ApiVersion::new(5, 3);
const WAVE3_API_5_4: ApiVersion = ApiVersion::new(5, 4);

const STATUS: MessageIdentity = MessageIdentity::new(MessageKind::Status, 1);
const VERSION: MessageIdentity = MessageIdentity::new(MessageKind::Version, 10);

static NO_FIELDS: &[FieldSpec] = &[];

static STATUS_SCHEMA_5_3: MessageSchema = MessageSchema {
    identity: SchemaIdentity::new(DeviceModel::Wave3, WAVE3_API_5_3, STATUS, 8),
    access: Access::READ_ONLY,
    fields: NO_FIELDS,
};
static STATUS_SCHEMA_5_4: MessageSchema = MessageSchema {
    identity: SchemaIdentity::new(DeviceModel::Wave3, WAVE3_API_5_4, STATUS, 8),
    access: Access::READ_ONLY,
    fields: NO_FIELDS,
};
static VERSION_SCHEMA_5_3: MessageSchema = MessageSchema {
    identity: SchemaIdentity::new(DeviceModel::Wave3, WAVE3_API_5_3, VERSION, 52),
    access: Access::READ_ONLY,
    fields: NO_FIELDS,
};
static VERSION_SCHEMA_5_4: MessageSchema = MessageSchema {
    identity: SchemaIdentity::new(DeviceModel::Wave3, WAVE3_API_5_4, VERSION, 54),
    access: Access::READ_ONLY,
    fields: NO_FIELDS,
};

pub(crate) fn supported_api(api: ApiVersion) -> bool {
    api == WAVE3_API_5_3 || api == WAVE3_API_5_4
}

pub(crate) const fn schema_for(
    api: ApiVersion,
    message: MessageIdentity,
) -> Option<&'static MessageSchema> {
    match (api, message.kind) {
        (WAVE3_API_5_3, MessageKind::Config) => Some(crate::wave3::CONFIG_SCHEMA_5_3),
        (WAVE3_API_5_4, MessageKind::Config) => Some(crate::wave3::CONFIG_SCHEMA_5_4),
        (WAVE3_API_5_3, MessageKind::Status) => Some(&STATUS_SCHEMA_5_3),
        (WAVE3_API_5_4, MessageKind::Status) => Some(&STATUS_SCHEMA_5_4),
        (WAVE3_API_5_3, MessageKind::Version) => Some(&VERSION_SCHEMA_5_3),
        (WAVE3_API_5_4, MessageKind::Version) => Some(&VERSION_SCHEMA_5_4),
        _ => None,
    }
}

/// Admits a message only when model, API, message identity, and payload size match.
///
/// # Errors
///
/// Returns an error when any part of the identity is unsupported or does not match the reviewed
/// schema.
pub fn admit(identity: SchemaIdentity) -> Result<&'static MessageSchema, SchemaError> {
    if identity.model != DeviceModel::Wave3 {
        return Err(SchemaError::UnsupportedModel { model: identity.model });
    }
    if !supported_api(identity.api) {
        return Err(SchemaError::UnsupportedApi { api: identity.api });
    }
    let expected = schema_for(identity.api, identity.message)
        .ok_or(SchemaError::UnsupportedMessage { message: identity.message })?;
    if expected.identity().message.id != identity.message.id {
        return Err(SchemaError::MessageIdMismatch {
            expected: expected.identity().message.id,
            actual: identity.message.id,
        });
    }
    if expected.identity().payload_size != identity.payload_size {
        return Err(SchemaError::PayloadSizeMismatch {
            expected: expected.identity().payload_size,
            actual: identity.payload_size,
        });
    }
    Ok(expected)
}

/// Returns the exact writable Wave:3 configuration schema for a supported API version.
///
/// # Errors
///
/// Returns an error when the API version is not one of the reviewed 16-byte API 5 schemas.
pub fn config_schema(api: ApiVersion) -> Result<&'static MessageSchema, SchemaError> {
    admit(SchemaIdentity::new(
        DeviceModel::Wave3,
        api,
        MessageIdentity::new(MessageKind::Config, 0),
        16,
    ))
}
