use crate::schema::{FieldCodec, FieldSpec, MessageSchema};
use crate::wave3::{VolumeSelect, Wave3ConfigField};

/// Exact semantic values accepted by the portable codecs.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SemanticValue {
    Boolean(bool),
    FixedPoint { raw: i32, fractional_bits: u8 },
    Unsigned(u16),
    Enum(u8),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ValueError {
    WrongType,
    InvalidBoolean(u8),
    UnknownEnum(u8),
    FractionalBits { expected: u8, actual: u8 },
    OutsideRange { minimum: i32, maximum: i32, actual: i32 },
    WrongStep { step: i32, actual: i32 },
    WireRange,
}

impl core::fmt::Display for ValueError {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::WrongType => formatter.write_str("semantic value has the wrong type"),
            Self::InvalidBoolean(value) => write!(formatter, "invalid boolean byte {value}"),
            Self::UnknownEnum(value) => write!(formatter, "unknown enum value {value}"),
            Self::FractionalBits { expected, actual } => {
                write!(formatter, "fixed-point fractional bits {actual} do not match {expected}")
            }
            Self::OutsideRange { minimum, maximum, actual } => {
                write!(formatter, "fixed-point value {actual} is outside {minimum}..={maximum}")
            }
            Self::WrongStep { step, actual } => {
                write!(formatter, "fixed-point value {actual} is not aligned to step {step}")
            }
            Self::WireRange => formatter.write_str("value does not fit the wire type"),
        }
    }
}

impl std::error::Error for ValueError {}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CodecError {
    PayloadLength { expected: usize, actual: usize },
    FieldOutOfBounds { offset: usize, length: usize },
    SchemaMismatch,
    FieldNotWritable { field: Wave3ConfigField },
    FieldNotInSchema { field: Wave3ConfigField },
    Value { field: Wave3ConfigField, reason: ValueError },
}

impl core::fmt::Display for CodecError {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::PayloadLength { expected, actual } => {
                write!(formatter, "payload length {actual} does not match {expected}")
            }
            Self::FieldOutOfBounds { offset, length } => {
                write!(
                    formatter,
                    "field at offset {offset} with length {length} is outside the payload"
                )
            }
            Self::SchemaMismatch => {
                formatter.write_str("schema is not an admitted Wave:3 config schema")
            }
            Self::FieldNotWritable { field } => {
                write!(formatter, "field {field:?} is not writable")
            }
            Self::FieldNotInSchema { field } => {
                write!(formatter, "field {field:?} is not in the schema")
            }
            Self::Value { field, reason } => {
                write!(formatter, "invalid value for {field:?}: {reason}")
            }
        }
    }
}

impl std::error::Error for CodecError {}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PatchResult {
    Changed,
    Unchanged,
}

fn check_payload(payload: &[u8], expected: usize) -> Result<(), CodecError> {
    if payload.len() == expected {
        Ok(())
    } else {
        Err(CodecError::PayloadLength { expected, actual: payload.len() })
    }
}

fn validate(field: &FieldCodec, value: SemanticValue) -> Result<(), ValueError> {
    match *field {
        FieldCodec::Boolean => match value {
            SemanticValue::Boolean(_) => Ok(()),
            _ => Err(ValueError::WrongType),
        },
        FieldCodec::SignedFixed { fractional_bits, minimum_raw, maximum_raw, step_raw } => {
            match value {
                SemanticValue::FixedPoint { raw, fractional_bits: actual } => {
                    if actual != fractional_bits {
                        return Err(ValueError::FractionalBits {
                            expected: fractional_bits,
                            actual,
                        });
                    }
                    if raw < minimum_raw || raw > maximum_raw {
                        return Err(ValueError::OutsideRange {
                            minimum: minimum_raw,
                            maximum: maximum_raw,
                            actual: raw,
                        });
                    }
                    let delta = raw.checked_sub(minimum_raw).ok_or(ValueError::WireRange)?;
                    if delta % step_raw != 0 {
                        return Err(ValueError::WrongStep { step: step_raw, actual: raw });
                    }
                    if i16::try_from(raw).is_err() {
                        return Err(ValueError::WireRange);
                    }
                    Ok(())
                }
                _ => Err(ValueError::WrongType),
            }
        }
        FieldCodec::VolumeSelect => match value {
            SemanticValue::Enum(value) if VolumeSelect::from_wire(value).is_some() => Ok(()),
            SemanticValue::Enum(value) => Err(ValueError::UnknownEnum(value)),
            _ => Err(ValueError::WrongType),
        },
    }
}

fn field_slice(field: FieldSpec, payload: &[u8]) -> Result<&[u8], CodecError> {
    let offset = field.offset();
    let length = match field.codec() {
        FieldCodec::Boolean | FieldCodec::VolumeSelect => 1,
        FieldCodec::SignedFixed { .. } => 2,
    };
    let end = offset.checked_add(length).ok_or(CodecError::FieldOutOfBounds { offset, length })?;
    payload.get(offset..end).ok_or(CodecError::FieldOutOfBounds { offset, length })
}

fn field_slice_mut(field: FieldSpec, payload: &mut [u8]) -> Result<&mut [u8], CodecError> {
    let offset = field.offset();
    let length = match field.codec() {
        FieldCodec::Boolean | FieldCodec::VolumeSelect => 1,
        FieldCodec::SignedFixed { .. } => 2,
    };
    let end = offset.checked_add(length).ok_or(CodecError::FieldOutOfBounds { offset, length })?;
    payload.get_mut(offset..end).ok_or(CodecError::FieldOutOfBounds { offset, length })
}

/// Decodes one field from a complete payload.
///
/// # Errors
///
/// Returns an error when the payload is not 16 bytes or the field contains an invalid wire value.
pub(crate) fn decode_field(
    schema: &MessageSchema,
    spec: FieldSpec,
    payload: &[u8],
) -> Result<SemanticValue, CodecError> {
    check_payload(payload, schema.identity().payload_size)?;
    if !schema.fields().contains(&spec) {
        return Err(CodecError::FieldNotInSchema { field: spec.field() });
    }
    let bytes = field_slice(spec, payload)?;
    match spec.codec() {
        FieldCodec::Boolean => match bytes[0] {
            0 => Ok(SemanticValue::Boolean(false)),
            1 => Ok(SemanticValue::Boolean(true)),
            value => Err(CodecError::Value {
                field: spec.field(),
                reason: ValueError::InvalidBoolean(value),
            }),
        },
        FieldCodec::SignedFixed { fractional_bits, .. } => {
            let value = SemanticValue::FixedPoint {
                raw: i32::from(i16::from_le_bytes([bytes[0], bytes[1]])),
                fractional_bits,
            };
            validate(&spec.codec(), value)
                .map_err(|reason| CodecError::Value { field: spec.field(), reason })?;
            Ok(value)
        }
        FieldCodec::VolumeSelect => {
            let value = bytes[0];
            if VolumeSelect::from_wire(value).is_none() {
                return Err(CodecError::Value {
                    field: spec.field(),
                    reason: ValueError::UnknownEnum(value),
                });
            }
            Ok(SemanticValue::Enum(value))
        }
    }
}

/// Patches one writable field in a complete baseline payload.
///
/// # Errors
///
/// Returns an error when the payload size, field permission, schema membership, or value is
/// invalid.
pub(crate) fn patch_field(
    schema: &MessageSchema,
    field: FieldSpec,
    payload: &mut [u8],
    value: SemanticValue,
) -> Result<PatchResult, CodecError> {
    check_payload(payload, schema.identity().payload_size)?;
    if !schema.access().writable() || !field.access().writable() {
        return Err(CodecError::FieldNotWritable { field: field.field() });
    }
    if !schema.fields().contains(&field) {
        return Err(CodecError::FieldNotInSchema { field: field.field() });
    }
    validate(&field.codec(), value)
        .map_err(|reason| CodecError::Value { field: field.field(), reason })?;
    let before = payload.to_owned();
    let bytes = field_slice_mut(field, payload)?;
    match (field.codec(), value) {
        (FieldCodec::Boolean, SemanticValue::Boolean(value)) => {
            bytes[0] = u8::from(value);
        }
        (FieldCodec::SignedFixed { .. }, SemanticValue::FixedPoint { raw, .. }) => {
            bytes.copy_from_slice(
                &i16::try_from(raw)
                    .map_err(|_| CodecError::Value {
                        field: field.field(),
                        reason: ValueError::WireRange,
                    })?
                    .to_le_bytes(),
            );
        }
        (FieldCodec::VolumeSelect, SemanticValue::Enum(value)) => {
            bytes[0] = value;
        }
        _ => unreachable!("validated values match their field codec"),
    }
    Ok(if before == payload { PatchResult::Unchanged } else { PatchResult::Changed })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::Access;

    #[test]
    fn out_of_bounds_metadata_is_rejected_without_slicing_panic() {
        let field = FieldSpec::boolean(Wave3ConfigField::InputMute, 16, Access::READ_WRITE);
        assert_eq!(
            field_slice(field, &[0; 16]),
            Err(CodecError::FieldOutOfBounds { offset: 16, length: 1 })
        );
        let field = FieldSpec::boolean(Wave3ConfigField::InputMute, usize::MAX, Access::READ_WRITE);
        assert_eq!(
            field_slice(field, &[0; 16]),
            Err(CodecError::FieldOutOfBounds { offset: usize::MAX, length: 1 })
        );
    }
}
