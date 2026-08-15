use crate::schema::{Access, FieldSpec, MessageSchema};
use crate::{ApiVersion, DeviceModel, MessageIdentity, MessageKind, SchemaIdentity, ValueError};

pub(crate) const Q8_8_FRACTIONAL_BITS: u8 = 8;

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

/// A Wave:3 microphone gain in signed Q8.8 dB.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Wave3GainDb(i32);

impl Wave3GainDb {
    /// Returns the signed Q8.8 wire value.
    #[must_use]
    pub const fn raw_q8_8(self) -> i32 {
        self.0
    }

    fn from_validated_raw(raw: i32) -> Self {
        Self(raw)
    }

    /// Returns the Q-format fractional-bit count admitted by the schema.
    #[must_use]
    pub const fn fractional_bits(self) -> u8 {
        Q8_8_FRACTIONAL_BITS
    }
}

/// A Wave:3 headphone output level in signed Q8.8 dB.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Wave3HeadphoneDb(i32);

impl Wave3HeadphoneDb {
    /// Returns the signed Q8.8 wire value.
    #[must_use]
    pub const fn raw_q8_8(self) -> i32 {
        self.0
    }

    fn from_validated_raw(raw: i32) -> Self {
        Self(raw)
    }

    /// Returns the Q-format fractional-bit count admitted by the schema.
    #[must_use]
    pub const fn fractional_bits(self) -> u8 {
        Q8_8_FRACTIONAL_BITS
    }
}

/// A Wave:3 direct-monitor balance in Q8.8 percent.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Wave3MonitorPercent(i32);

impl Wave3MonitorPercent {
    /// Returns the Q8.8 wire value.
    #[must_use]
    pub const fn raw_q8_8(self) -> i32 {
        self.0
    }
    fn from_validated_raw(raw: i32) -> Self {
        Self(raw)
    }

    /// Returns the Q-format fractional-bit count admitted by the schema.
    #[must_use]
    pub const fn fractional_bits(self) -> u8 {
        Q8_8_FRACTIONAL_BITS
    }
}

/// The typed hardware controls decoded from a Wave:3 API 5 configuration.
///
/// This is a derived read view. The complete [`Wave3Config`] remains the
/// baseline for any future reviewed transaction so reserved bytes are kept.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[allow(clippy::struct_excessive_bools)]
pub struct Wave3ControlState {
    /// Microphone preamp gain.
    pub microphone_gain: Wave3GainDb,
    /// Hardware microphone mute state.
    pub microphone_mute: bool,
    /// Clipguard processing state.
    pub clipguard_enabled: bool,
    /// Low-cut filter state.
    pub lowcut_enabled: bool,
    /// Headphone output level.
    pub headphone_volume: Wave3HeadphoneDb,
    /// Hardware headphone mute state.
    pub headphone_mute: bool,
    /// Direct monitor balance.
    pub direct_monitor: Wave3MonitorPercent,
    /// Physical knob target.
    pub volume_select: VolumeSelect,
    /// Device policy that controls operating-system gain requests.
    pub gain_lock: bool,
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
    FieldSpec::signed_fixed(
        Wave3ConfigField::InputGain,
        0,
        Q8_8_FRACTIONAL_BITS,
        0,
        10_240,
        128,
        RW,
    ),
    FieldSpec::boolean(Wave3ConfigField::InputMute, 4, RW),
    FieldSpec::boolean(Wave3ConfigField::ClipguardEnable, 5, RW),
    FieldSpec::boolean(Wave3ConfigField::LowcutEnable, 6, RW),
    FieldSpec::signed_fixed(
        Wave3ConfigField::HeadphoneVolume,
        7,
        Q8_8_FRACTIONAL_BITS,
        -15_360,
        0,
        128,
        RW,
    ),
    FieldSpec::boolean(Wave3ConfigField::HeadphoneMute, 9, RW),
    FieldSpec::signed_fixed(
        Wave3ConfigField::DirectMonitor,
        10,
        Q8_8_FRACTIONAL_BITS,
        0,
        25_600,
        1_280,
        RW,
    ),
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

    /// Decodes the user-facing Wave:3 hardware controls with their units and limits.
    ///
    /// # Errors
    ///
    /// Returns an error when a field contains an invalid wire value.
    pub fn controls(&self) -> Result<Wave3ControlState, crate::CodecError> {
        let fixed = |field| match self.get(field)? {
            crate::SemanticValue::FixedPoint { raw, fractional_bits }
                if fractional_bits == Q8_8_FRACTIONAL_BITS =>
            {
                Ok(raw)
            }
            crate::SemanticValue::FixedPoint { fractional_bits, .. } => {
                Err(crate::CodecError::Value {
                    field,
                    reason: ValueError::FractionalBits {
                        expected: Q8_8_FRACTIONAL_BITS,
                        actual: fractional_bits,
                    },
                })
            }
            _ => Err(crate::CodecError::Value { field, reason: ValueError::WrongType }),
        };
        let boolean = |field| match self.get(field)? {
            crate::SemanticValue::Boolean(value) => Ok(value),
            _ => Err(crate::CodecError::Value { field, reason: ValueError::WrongType }),
        };
        let selection = match self.get(Wave3ConfigField::VolumeSelect)? {
            crate::SemanticValue::Enum(value) => {
                VolumeSelect::from_wire(value).ok_or(crate::CodecError::Value {
                    field: Wave3ConfigField::VolumeSelect,
                    reason: ValueError::UnknownEnum(value),
                })?
            }
            _ => {
                return Err(crate::CodecError::Value {
                    field: Wave3ConfigField::VolumeSelect,
                    reason: ValueError::WrongType,
                });
            }
        };
        let gain_raw = fixed(Wave3ConfigField::InputGain)?;
        let headphone_raw = fixed(Wave3ConfigField::HeadphoneVolume)?;
        let monitor_raw = fixed(Wave3ConfigField::DirectMonitor)?;
        let microphone_gain = Wave3GainDb::from_validated_raw(gain_raw);
        let headphone_volume = Wave3HeadphoneDb::from_validated_raw(headphone_raw);
        let direct_monitor = Wave3MonitorPercent::from_validated_raw(monitor_raw);

        Ok(Wave3ControlState {
            microphone_gain,
            microphone_mute: boolean(Wave3ConfigField::InputMute)?,
            clipguard_enabled: boolean(Wave3ConfigField::ClipguardEnable)?,
            lowcut_enabled: boolean(Wave3ConfigField::LowcutEnable)?,
            headphone_volume,
            headphone_mute: boolean(Wave3ConfigField::HeadphoneMute)?,
            direct_monitor,
            volume_select: selection,
            gain_lock: boolean(Wave3ConfigField::GainLock)?,
        })
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
