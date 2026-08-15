use crate::schema::{Access, FieldSpec, MessageSchema};
use crate::{ApiVersion, DeviceModel, MessageIdentity, MessageKind, SchemaIdentity};

/// The three valid Wave:3 knob targets in the API 5 configuration message.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum VolumeSelect {
    Mic = 1,
    Headphone = 2,
    Mix = 3,
}

impl VolumeSelect {
    pub(crate) const fn from_wire(value: u8) -> Option<Self> {
        match value {
            1 => Some(Self::Mic),
            2 => Some(Self::Headphone),
            3 => Some(Self::Mix),
            _ => None,
        }
    }
}

/// Every field in the reviewed 16-byte Wave:3 API 5 configuration message.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Wave3ConfigField {
    InputGain,
    InputMute,
    ClipguardEnable,
    LowcutEnable,
    HeadphoneVolume,
    HeadphoneMute,
    DirectMonitor,
    VolumeSelect,
    AllLedsOff,
    LedsFlip,
    GainLock,
}

const RW: Access = Access::READ_WRITE;

// Reviewed normalized protocol evidence for the Wave:3 API 5.3/5.4 schemas.
static CONFIG_FIELDS: &[FieldSpec] = &[
    FieldSpec::signed_fixed(Wave3ConfigField::InputGain, 0, 8, 0, 10_240, 128, RW),
    FieldSpec::boolean(Wave3ConfigField::InputMute, 4, RW),
    FieldSpec::boolean(Wave3ConfigField::ClipguardEnable, 5, RW),
    FieldSpec::boolean(Wave3ConfigField::LowcutEnable, 6, RW),
    FieldSpec::signed_fixed(Wave3ConfigField::HeadphoneVolume, 7, 8, -15_360, 0, 128, RW),
    FieldSpec::boolean(Wave3ConfigField::HeadphoneMute, 9, RW),
    FieldSpec::signed_fixed(Wave3ConfigField::DirectMonitor, 10, 8, 0, 25_600, 1_280, RW),
    FieldSpec::volume_select(Wave3ConfigField::VolumeSelect, 12, RW),
    FieldSpec::boolean(Wave3ConfigField::AllLedsOff, 13, RW),
    FieldSpec::boolean(Wave3ConfigField::LedsFlip, 14, RW),
    FieldSpec::boolean(Wave3ConfigField::GainLock, 15, RW),
];

pub(crate) static CONFIG_SCHEMA_5_3: &MessageSchema = &MessageSchema::new(
    SchemaIdentity::new(
        DeviceModel::Wave3,
        ApiVersion::new(5, 3),
        MessageIdentity::new(MessageKind::Config, 0),
        16,
    ),
    RW,
    CONFIG_FIELDS,
);

pub(crate) static CONFIG_SCHEMA_5_4: &MessageSchema = &MessageSchema::new(
    SchemaIdentity::new(
        DeviceModel::Wave3,
        ApiVersion::new(5, 4),
        MessageIdentity::new(MessageKind::Config, 0),
        16,
    ),
    RW,
    CONFIG_FIELDS,
);

#[must_use]
pub fn config_fields() -> &'static [FieldSpec] {
    CONFIG_FIELDS
}

pub(crate) fn field_spec(field: Wave3ConfigField) -> &'static FieldSpec {
    CONFIG_FIELDS
        .iter()
        .find(|spec| spec.field() == field)
        .expect("every Wave3ConfigField has a schema entry")
}

pub(crate) fn is_config_schema(schema: &MessageSchema) -> bool {
    core::ptr::eq(schema, CONFIG_SCHEMA_5_3) || core::ptr::eq(schema, CONFIG_SCHEMA_5_4)
}

/// An exact Wave:3 configuration payload. The raw bytes retain reserved bytes and unknown state.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Wave3Config {
    bytes: [u8; 16],
    schema: &'static MessageSchema,
}

impl Wave3Config {
    /// Decodes a complete configuration baseline for an admitted schema.
    ///
    /// # Errors
    ///
    /// Returns an error when the schema is not an admitted writable config schema or the payload
    /// is not exactly 16 bytes.
    pub fn from_schema(
        schema: &'static MessageSchema,
        payload: &[u8],
    ) -> Result<Self, crate::CodecError> {
        if !is_config_schema(schema) {
            return Err(crate::CodecError::SchemaMismatch);
        }
        if payload.len() != schema.identity().payload_size {
            return Err(crate::CodecError::PayloadLength {
                expected: schema.identity().payload_size,
                actual: payload.len(),
            });
        }
        let mut bytes = [0; 16];
        bytes.copy_from_slice(payload);
        Ok(Self { bytes, schema })
    }

    #[must_use]
    pub const fn as_bytes(&self) -> &[u8; 16] {
        &self.bytes
    }

    /// Reads one semantic value using the exact field codec.
    ///
    /// # Errors
    ///
    /// Returns an error when the stored field contains an invalid wire value.
    pub fn get(&self, field: Wave3ConfigField) -> Result<crate::SemanticValue, crate::CodecError> {
        crate::codec::decode_field(self.schema, *field_spec(field), &self.bytes)
    }

    /// Changes one field in a complete baseline and preserves every other byte.
    ///
    /// # Errors
    ///
    /// Returns an error when the semantic value is not valid for the selected field.
    pub fn set(
        &mut self,
        field: Wave3ConfigField,
        value: crate::SemanticValue,
    ) -> Result<crate::PatchResult, crate::CodecError> {
        crate::codec::patch_field(self.schema, *field_spec(field), &mut self.bytes, value)
    }
}
