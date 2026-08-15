#![doc = "Portable Wave protocol identities, schemas, codecs, and safe payload mutation."]

mod codec;
mod identity;
mod schema;
mod setup;
mod wave3;

pub use codec::{CodecError, PatchResult, SemanticValue, ValueError};
pub use identity::{
    ApiVersion, DeviceModel, Direction, MessageIdentity, MessageKind, SchemaIdentity,
};
pub use schema::{Access, FieldSpec, MessageSchema, SchemaError, admit, config_schema};
pub use setup::{
    SetupError, SetupPacket, decode_version_response, message_transfer, version_probe,
};
pub use wave3::{
    VolumeSelect, Wave3Config, Wave3ConfigField, Wave3ControlChange, Wave3ControlState,
    Wave3GainDb, Wave3HeadphoneDb, Wave3MonitorPercent, config_fields,
};

#[cfg(test)]
mod tests {
    use super::*;

    fn config_field(field: Wave3ConfigField) -> FieldSpec {
        match config_fields().iter().find(|spec| spec.field() == field) {
            Some(spec) => *spec,
            None => panic!("missing configuration field"),
        }
    }

    fn config() -> Wave3Config {
        let schema = admitted_config(ApiVersion::new(5, 4));
        match Wave3Config::from_schema(schema, &[0; 16]) {
            Ok(config) => config,
            Err(error) => panic!("valid configuration rejected: {error}"),
        }
    }

    fn admitted_config(api: ApiVersion) -> &'static MessageSchema {
        match config_schema(api) {
            Ok(schema) => schema,
            Err(error) => panic!("config schema rejected: {error}"),
        }
    }

    #[test]
    fn admits_only_exact_wave3_api5_message_shapes() {
        let identity = SchemaIdentity::new(
            DeviceModel::Wave3,
            ApiVersion::new(5, 4),
            MessageIdentity::new(MessageKind::Config, 0),
            16,
        );
        assert_eq!(admit(identity).map(|schema| schema.identity()), Ok(identity));
        assert!(matches!(
            config_schema(ApiVersion::new(5, 2)),
            Err(SchemaError::UnsupportedApi { .. })
        ));
        assert!(matches!(
            admit(SchemaIdentity::new(
                DeviceModel::Wave3,
                ApiVersion::new(5, 4),
                MessageIdentity::new(MessageKind::Config, 0),
                14,
            )),
            Err(SchemaError::PayloadSizeMismatch { expected: 16, actual: 14 })
        ));
        assert!(matches!(
            admit(SchemaIdentity::new(
                DeviceModel::Wave3,
                ApiVersion::new(5, 4),
                MessageIdentity::new(MessageKind::Config, 1),
                16,
            )),
            Err(SchemaError::MessageIdMismatch { expected: 0, actual: 1 })
        ));
        assert!(matches!(
            admit(SchemaIdentity::new(
                DeviceModel::Wave3,
                ApiVersion::new(5, 4),
                MessageIdentity::new(MessageKind::Unknown, 99),
                16,
            )),
            Err(SchemaError::UnsupportedMessage { .. })
        ));
        assert!(matches!(
            admit(SchemaIdentity::new(
                DeviceModel::Unknown,
                ApiVersion::new(5, 4),
                MessageIdentity::new(MessageKind::Config, 0),
                16,
            )),
            Err(SchemaError::UnsupportedModel { .. })
        ));
    }

    #[test]
    fn only_admitted_api5_config_schemas_can_construct_mutable_configs() {
        assert!(matches!(
            config_schema(ApiVersion::new(5, 2)),
            Err(SchemaError::UnsupportedApi { .. })
        ));
        assert_eq!(
            Wave3Config::from_schema(admitted_config(ApiVersion::new(5, 4)), &[0; 16])
                .map(|config| config.get(Wave3ConfigField::GainLock)),
            Ok(Ok(SemanticValue::Boolean(false)))
        );
        let mut api_5_3 =
            match Wave3Config::from_schema(admitted_config(ApiVersion::new(5, 3)), &[0; 16]) {
                Ok(config) => config,
                Err(error) => panic!("API 5.3 config rejected: {error}"),
            };
        assert_eq!(
            api_5_3.set(Wave3ConfigField::GainLock, SemanticValue::Boolean(true)),
            Ok(PatchResult::Changed)
        );
        assert!(matches!(
            Wave3Config::from_schema(
                match admit(SchemaIdentity::new(
                    DeviceModel::Wave3,
                    ApiVersion::new(5, 4),
                    MessageIdentity::new(MessageKind::Status, 1),
                    8,
                )) {
                    Ok(schema) => schema,
                    Err(error) => panic!("status schema rejected: {error}"),
                },
                &[0; 16],
            ),
            Err(CodecError::SchemaMismatch)
        ));
    }

    #[test]
    fn config_layout_contains_all_eleven_fields_and_reserved_gap() {
        assert_eq!(config_fields().len(), 11);
        let offsets: Vec<_> = config_fields().iter().map(|field| field.offset()).collect();
        assert_eq!(offsets, [0, 4, 5, 6, 7, 9, 10, 12, 13, 14, 15]);
        assert_eq!(config_field(Wave3ConfigField::AllLedsOff).offset(), 13);
        assert_eq!(config_field(Wave3ConfigField::LedsFlip).offset(), 14);
        assert_eq!(config_field(Wave3ConfigField::GainLock).offset(), 15);
    }

    #[test]
    fn fixed_point_boundaries_and_steps_are_checked() {
        let mut config = config();
        assert_eq!(
            config.set(
                Wave3ConfigField::InputGain,
                SemanticValue::FixedPoint { raw: 0, fractional_bits: 8 }
            ),
            Ok(PatchResult::Unchanged)
        );
        assert_eq!(
            config.set(
                Wave3ConfigField::InputGain,
                SemanticValue::FixedPoint { raw: 10_240, fractional_bits: 8 }
            ),
            Ok(PatchResult::Changed)
        );
        assert!(matches!(
            config.set(
                Wave3ConfigField::InputGain,
                SemanticValue::FixedPoint { raw: 10_241, fractional_bits: 8 }
            ),
            Err(CodecError::Value { reason: ValueError::OutsideRange { .. }, .. })
        ));
        assert!(matches!(
            config.set(
                Wave3ConfigField::InputGain,
                SemanticValue::FixedPoint { raw: 1, fractional_bits: 8 }
            ),
            Err(CodecError::Value { reason: ValueError::WrongStep { .. }, .. })
        ));
        assert!(matches!(
            config.set(
                Wave3ConfigField::InputGain,
                SemanticValue::FixedPoint { raw: 0, fractional_bits: 7 }
            ),
            Err(CodecError::Value { reason: ValueError::FractionalBits { .. }, .. })
        ));
    }

    #[test]
    fn typed_controls_decode_units_and_capability_boundaries() {
        let schema = admitted_config(ApiVersion::new(5, 4));
        let mut payload = [0; 16];
        payload[0..2].copy_from_slice(&5_120i16.to_le_bytes());
        payload[7..9].copy_from_slice(&(-7_680i16).to_le_bytes());
        payload[10..12].copy_from_slice(&12_800i16.to_le_bytes());
        payload[12] = VolumeSelect::Mix as u8;
        payload[4] = 1;
        payload[9] = 1;
        payload[15] = 1;
        let config = Wave3Config::from_schema(schema, &payload).expect("valid controls");
        let controls = config.controls().expect("typed controls");

        assert_eq!(controls.microphone_gain.raw_q8_8(), 5_120);
        assert_eq!(controls.microphone_gain.fractional_bits(), 8);
        assert_eq!(controls.headphone_volume.raw_q8_8(), -7_680);
        assert_eq!(controls.headphone_volume.fractional_bits(), 8);
        assert_eq!(controls.direct_monitor.raw_q8_8(), 12_800);
        assert_eq!(controls.direct_monitor.fractional_bits(), 8);
        assert!(controls.microphone_mute);
        assert!(controls.headphone_mute);
        assert!(controls.gain_lock);
        assert_eq!(controls.volume_select, VolumeSelect::Mix);
    }

    #[test]
    fn typed_controls_reject_invalid_wire_values_through_the_schema() {
        let schema = admitted_config(ApiVersion::new(5, 4));
        let mut payload = [0; 16];
        payload[0..2].copy_from_slice(&1i16.to_le_bytes());
        payload[12] = VolumeSelect::Mic as u8;
        let config = Wave3Config::from_schema(schema, &payload).expect("payload shape");

        assert!(matches!(
            config.controls(),
            Err(CodecError::Value {
                field: Wave3ConfigField::InputGain,
                reason: ValueError::WrongStep { .. }
            })
        ));
    }

    #[test]
    fn typed_control_changes_use_the_schema_ranges_and_steps() {
        assert_eq!(Wave3GainDb::from_raw_q8_8(10_240).map(Wave3GainDb::raw_q8_8), Ok(10_240));
        assert!(matches!(Wave3GainDb::from_raw_q8_8(10_241), Err(ValueError::OutsideRange { .. })));
        assert!(matches!(Wave3GainDb::from_raw_q8_8(1), Err(ValueError::WrongStep { .. })));
        assert_eq!(
            Wave3HeadphoneDb::from_raw_q8_8(-15_360).map(Wave3HeadphoneDb::raw_q8_8),
            Ok(-15_360)
        );
        assert!(matches!(
            Wave3HeadphoneDb::from_raw_q8_8(128),
            Err(ValueError::OutsideRange { .. })
        ));
        assert_eq!(
            Wave3MonitorPercent::from_raw_q8_8(25_600).map(Wave3MonitorPercent::raw_q8_8),
            Ok(25_600)
        );
        assert!(matches!(Wave3MonitorPercent::from_raw_q8_8(1), Err(ValueError::WrongStep { .. })));
    }

    #[test]
    fn every_reviewed_control_change_patches_only_its_schema_field() {
        let changes = [
            Wave3ControlChange::MicrophoneGain(
                Wave3GainDb::from_raw_q8_8(512).expect("schema value"),
            ),
            Wave3ControlChange::MicrophoneMute(true),
            Wave3ControlChange::Clipguard(true),
            Wave3ControlChange::Lowcut(true),
            Wave3ControlChange::HeadphoneVolume(
                Wave3HeadphoneDb::from_raw_q8_8(-512).expect("schema value"),
            ),
            Wave3ControlChange::HeadphoneMute(true),
            Wave3ControlChange::DirectMonitor(
                Wave3MonitorPercent::from_raw_q8_8(1_280).expect("schema value"),
            ),
            Wave3ControlChange::VolumeSelect(VolumeSelect::Headphone),
            Wave3ControlChange::AllLedsOff(true),
            Wave3ControlChange::LedsFlip(true),
            Wave3ControlChange::GainLock(true),
        ];

        for change in changes {
            let mut config = config();
            let original = *config.as_bytes();
            assert_eq!(config.apply_control(change), Ok(PatchResult::Changed));
            let changed_indexes: Vec<_> = original
                .iter()
                .zip(config.as_bytes())
                .enumerate()
                .filter_map(|(index, (before, after))| (before != after).then_some(index))
                .collect();
            let spec = config_field(change.field());
            assert!(changed_indexes.iter().all(|index| *index >= spec.offset()));
            assert!(changed_indexes.iter().all(|index| *index <= spec.offset() + 1));
            assert_eq!(config.as_bytes()[2..4], original[2..4]);
        }
    }

    #[test]
    fn booleans_and_enum_values_reject_invalid_wire_values() {
        let mut payload = [0; 16];
        payload[4] = 2;
        assert!(matches!(
            crate::codec::decode_field(
                admitted_config(ApiVersion::new(5, 4)),
                config_field(Wave3ConfigField::InputMute),
                &payload,
            ),
            Err(CodecError::Value { reason: ValueError::InvalidBoolean(2), .. })
        ));
        payload[4] = 0;
        payload[12] = 4;
        assert!(matches!(
            crate::codec::decode_field(
                admitted_config(ApiVersion::new(5, 4)),
                config_field(Wave3ConfigField::VolumeSelect),
                &payload,
            ),
            Err(CodecError::Value { reason: ValueError::UnknownEnum(4), .. })
        ));
        assert_eq!(VolumeSelect::Mic as u8, 1);
        assert_eq!(VolumeSelect::Headphone as u8, 2);
        assert_eq!(VolumeSelect::Mix as u8, 3);
    }

    #[test]
    fn short_and_long_payloads_are_rejected() {
        assert!(matches!(
            Wave3Config::from_schema(admitted_config(ApiVersion::new(5, 4)), &[0; 15],),
            Err(CodecError::PayloadLength { expected: 16, actual: 15 })
        ));
        assert!(matches!(
            Wave3Config::from_schema(admitted_config(ApiVersion::new(5, 4)), &[0; 17],),
            Err(CodecError::PayloadLength { expected: 16, actual: 17 })
        ));
    }

    #[test]
    fn mutation_preserves_reserved_bytes_and_gain_lock() {
        let original: Vec<u8> = (0..16).collect();
        let mut config =
            match Wave3Config::from_schema(admitted_config(ApiVersion::new(5, 4)), &original) {
                Ok(config) => config,
                Err(error) => panic!("valid configuration rejected: {error}"),
            };
        assert_eq!(
            config.set(Wave3ConfigField::GainLock, SemanticValue::Boolean(true)),
            Ok(PatchResult::Changed)
        );
        let mut expected = original;
        expected[15] = 1;
        assert_eq!(config.as_bytes(), expected.as_slice());
        assert_eq!(config.as_bytes()[2..4], [2, 3]);
        assert_eq!(
            config.set(Wave3ConfigField::GainLock, SemanticValue::Boolean(true)),
            Ok(PatchResult::Unchanged)
        );
        assert_eq!(config.get(Wave3ConfigField::GainLock), Ok(SemanticValue::Boolean(true)));
    }

    #[test]
    fn read_only_messages_cannot_be_written() {
        let status = match admit(SchemaIdentity::new(
            DeviceModel::Wave3,
            ApiVersion::new(5, 4),
            MessageIdentity::new(MessageKind::Status, 1),
            8,
        )) {
            Ok(schema) => schema,
            Err(error) => panic!("status schema rejected: {error}"),
        };
        assert_eq!(status.access(), Access::READ_ONLY);
        assert_eq!(
            message_transfer(3, Direction::Out, status),
            Err(SetupError::MessageNotWritable)
        );
        let mut payload = [0; 8];
        assert_eq!(
            crate::codec::patch_field(
                status,
                config_field(Wave3ConfigField::GainLock),
                &mut payload,
                SemanticValue::Boolean(true)
            ),
            Err(CodecError::FieldNotWritable { field: Wave3ConfigField::GainLock })
        );
    }

    #[test]
    fn setup_packets_match_wave3_legacy_uac1_identity() {
        assert_eq!(version_probe(3).to_bytes(), [0xa1, 0x85, 0x0a, 0x00, 0x03, 0x33, 0x02, 0x00]);
        let schema = match config_schema(ApiVersion::new(5, 4)) {
            Ok(schema) => schema,
            Err(error) => panic!("config schema rejected: {error}"),
        };
        assert_eq!(
            message_transfer(3, Direction::In, schema).map(SetupPacket::to_bytes),
            Ok([0xa1, 0x85, 0x00, 0x00, 0x03, 0x33, 0x10, 0x00])
        );
        assert_eq!(
            message_transfer(3, Direction::Out, schema).map(SetupPacket::to_bytes),
            Ok([0x21, 0x05, 0x00, 0x00, 0x03, 0x33, 0x10, 0x00])
        );
        assert_eq!(decode_version_response([5, 4]), ApiVersion::new(5, 4));
    }
}
