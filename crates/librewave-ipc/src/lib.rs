#![doc = "Portable versioned messages and codecs for `LibreWave` IPC."]

use librewave_core::{
    Command, DeviceId, DeviceSnapshot, MixerGeneration, MixerSnapshot, Snapshot, SourceId,
};
use serde::{Deserialize, Serialize};
use std::fmt;

/// The current local request/response protocol version.
pub const PROTOCOL_VERSION: u16 = 4;
/// The maximum encoded JSON payload accepted by the local protocol.
pub const MAX_FRAME_SIZE: usize = 1024 * 1024;

/// One framed request sent by a client.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RequestEnvelope {
    /// The protocol version selected by the client.
    pub version: u16,
    /// A client-local request identifier.
    pub request_id: u64,
    /// The command for the daemon.
    pub command: Command,
}

/// The successful payload returned by the daemon.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub enum Response {
    /// The complete current daemon snapshot.
    Status { snapshot: Snapshot },
    /// The devices in the complete current daemon snapshot.
    Devices { devices: Vec<DeviceSnapshot> },
    /// The refresh completed and a new snapshot is available.
    Refreshed { snapshot: Snapshot },
    /// One device snapshot from the daemon's retained connection and admission state.
    DeviceInspection { device: DeviceSnapshot },
    /// The retained device snapshot after a verified semantic hardware control request.
    ControlChanged { device: DeviceSnapshot },
    /// The complete current portable mixer snapshot.
    Mixer { mixer: MixerSnapshot },
    /// The mixer snapshot after an accepted route request.
    MixerRouteChanged { mixer: MixerSnapshot },
}

/// A framed response sent by the daemon.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ResponseEnvelope {
    /// The protocol version used by the daemon.
    pub version: u16,
    /// The request identifier being answered.
    pub request_id: u64,
    /// Exactly one successful result or error.
    pub result: Result<Response, IpcError>,
}

impl ResponseEnvelope {
    /// Creates a successful response envelope.
    #[must_use]
    pub fn success(request_id: u64, response: Response) -> Self {
        Self { version: PROTOCOL_VERSION, request_id, result: Ok(response) }
    }

    /// Creates a failed response envelope.
    #[must_use]
    pub fn failure(request_id: u64, error: IpcError) -> Self {
        Self { version: PROTOCOL_VERSION, request_id, result: Err(error) }
    }
}

/// Stable categories for daemon-side IPC failures.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub enum IpcErrorKind {
    /// The peer requested a protocol version this daemon does not implement.
    VersionMismatch { expected: u16, actual: u16 },
    /// The peer credentials do not satisfy the local-user policy.
    PermissionDenied,
    /// The request envelope was structurally invalid.
    InvalidRequest,
    /// The daemon could not complete the requested operation.
    Internal,
    /// The requested logical device is not in the current inventory.
    DeviceNotFound { id: DeviceId },
    /// The client's device baseline is older than the daemon's observation.
    StaleDeviceGeneration {
        expected: librewave_core::DeviceGeneration,
        actual: librewave_core::DeviceGeneration,
    },
    /// The physical baseline changed outside the daemon before a write.
    StaleDeviceBaseline,
    /// The semantic control value is outside its reviewed schema.
    InvalidControl,
    /// The daemon does not currently permit hardware writes.
    DeviceWriteLocked { reason: librewave_core::DeviceWriteLockReason },
    /// The device disconnected during the request.
    DeviceDisconnected { id: DeviceId },
    /// The daemon could not persist desired state before the write.
    Persistence,
    /// Desired-state intent or crash durability is unresolved.
    PersistenceAmbiguous,
    /// The reviewed transaction failed.
    TransactionFailed,
    /// The requested logical mixer source is not in the current product profile.
    MixerSourceNotFound { id: SourceId },
    /// The client's mixer baseline is older than the daemon's desired state.
    StaleMixerGeneration { expected: MixerGeneration, actual: MixerGeneration },
    /// The mixer generation cannot advance beyond its integer range.
    MixerGenerationExhausted,
    /// The mixer profile could not be replaced before rename.
    MixerPersistence,
    /// The mixer profile was renamed, but directory-sync durability is ambiguous.
    MixerPersistenceAmbiguous,
}

/// An explicit error returned over the local interface.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct IpcError {
    /// The stable failure category.
    pub kind: IpcErrorKind,
    /// A short diagnostic intended for humans.
    pub message: String,
}

impl IpcError {
    /// Creates a protocol-version failure.
    #[must_use]
    pub fn version_mismatch(actual: u16) -> Self {
        Self {
            kind: IpcErrorKind::VersionMismatch { expected: PROTOCOL_VERSION, actual },
            message: format!(
                "protocol version {actual} is not supported; expected {PROTOCOL_VERSION}",
            ),
        }
    }

    /// Creates a permission failure.
    #[must_use]
    pub fn permission_denied() -> Self {
        Self {
            kind: IpcErrorKind::PermissionDenied,
            message: "the connecting user is not permitted to use this daemon".to_owned(),
        }
    }

    /// Creates a missing logical-device failure.
    #[must_use]
    pub fn device_not_found(id: DeviceId) -> Self {
        Self {
            kind: IpcErrorKind::DeviceNotFound { id },
            message: format!("device {id} is not in the current inventory"),
        }
    }
}

impl fmt::Display for IpcError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for IpcError {}

/// Errors produced while encoding or decoding a frame.
#[derive(Debug)]
pub enum CodecError {
    Json(serde_json::Error),
    FrameTooLarge(usize),
    Truncated,
}

impl fmt::Display for CodecError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Json(error) => write!(formatter, "invalid JSON envelope: {error}"),
            Self::FrameTooLarge(size) => write!(formatter, "IPC frame is too large ({size} bytes)"),
            Self::Truncated => formatter.write_str("IPC frame ended before its payload completed"),
        }
    }
}

impl std::error::Error for CodecError {}

/// Encodes a request into one length-prefixed frame.
///
/// # Errors
///
/// Returns an error when JSON serialization fails or the encoded frame exceeds the size limit.
pub fn encode_request(request: &RequestEnvelope) -> Result<Vec<u8>, CodecError> {
    encode_json(request)
}

/// Decodes one request payload.
///
/// # Errors
///
/// Returns an error when the payload is not a valid, strictly shaped request JSON object.
pub fn decode_request(payload: &[u8]) -> Result<RequestEnvelope, CodecError> {
    serde_json::from_slice(payload).map_err(CodecError::Json)
}

/// Encodes a response into one length-prefixed frame.
///
/// # Errors
///
/// Returns an error when JSON serialization fails or the encoded frame exceeds the size limit.
pub fn encode_response(response: &ResponseEnvelope) -> Result<Vec<u8>, CodecError> {
    encode_json(response)
}

/// Decodes one response payload.
///
/// # Errors
///
/// Returns an error when the payload is not a valid, strictly shaped response JSON object.
pub fn decode_response(payload: &[u8]) -> Result<ResponseEnvelope, CodecError> {
    serde_json::from_slice(payload).map_err(CodecError::Json)
}

fn encode_json<T: Serialize>(value: &T) -> Result<Vec<u8>, CodecError> {
    let payload = serde_json::to_vec(value).map_err(CodecError::Json)?;
    if payload.len() > MAX_FRAME_SIZE {
        return Err(CodecError::FrameTooLarge(payload.len()));
    }
    let length =
        u32::try_from(payload.len()).map_err(|_| CodecError::FrameTooLarge(payload.len()))?;
    let mut frame = Vec::with_capacity(payload.len() + 4);
    frame.extend_from_slice(&length.to_be_bytes());
    frame.extend_from_slice(&payload);
    Ok(frame)
}

/// Processes one decoded request without performing I/O.
#[must_use]
pub fn process_request<F>(request: RequestEnvelope, handler: F) -> ResponseEnvelope
where
    F: FnOnce(Command) -> Result<Response, IpcError>,
{
    if request.version != PROTOCOL_VERSION {
        return ResponseEnvelope::failure(
            request.request_id,
            IpcError::version_mismatch(request.version),
        );
    }
    match handler(request.command) {
        Ok(response) => ResponseEnvelope::success(request.request_id, response),
        Err(error) => ResponseEnvelope::failure(request.request_id, error),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use librewave_core::{
        DeviceAdmissionSnapshot, DeviceConnection, DeviceGeneration, DeviceId, DeviceModel,
        FaderGain, FixedPointValue, MixRoute, MixTarget, MixerGeneration, MixerSnapshot, SourceId,
        Wave3Control,
    };
    use serde_json::json;

    fn device() -> DeviceSnapshot {
        DeviceSnapshot {
            id: DeviceId(1),
            model: DeviceModel::Wave3,
            connection: DeviceConnection::Connected,
            audio_cards: Vec::new(),
            admission: DeviceAdmissionSnapshot::not_inspected(),
        }
    }

    #[test]
    fn round_trip_preserves_versioned_envelope_without_origin_on_command() {
        let request = RequestEnvelope {
            version: PROTOCOL_VERSION,
            request_id: 9,
            command: Command::GetStatus,
        };
        let frame = encode_request(&request).expect("encode request");
        assert_eq!(
            u32::from_be_bytes(frame[..4].try_into().expect("length")) as usize,
            frame.len() - 4
        );
        assert_eq!(decode_request(&frame[4..]).expect("decode request"), request);
    }

    #[test]
    fn response_result_cannot_hold_success_and_error_together() {
        let success =
            ResponseEnvelope::success(1, Response::Status { snapshot: Snapshot::empty() });
        let failure = ResponseEnvelope::failure(1, IpcError::permission_denied());
        assert!(success.result.is_ok());
        assert!(failure.result.is_err());
    }

    #[test]
    fn strict_request_envelope_rejects_unknown_fields() {
        let payload = br#"{"version":1,"request_id":1,"command":"GetStatus","unexpected":true}"#;
        assert!(matches!(decode_request(payload), Err(CodecError::Json(_))));
    }

    #[test]
    fn fake_server_exercises_request_processing_and_response_validation() {
        let request = RequestEnvelope {
            version: PROTOCOL_VERSION,
            request_id: 1,
            command: Command::GetStatus,
        };
        let response = process_request(request, |command| {
            assert_eq!(command, Command::GetStatus);
            Ok(Response::Status { snapshot: Snapshot::empty() })
        });
        assert_eq!(response.result, Ok(Response::Status { snapshot: Snapshot::empty() }));
        assert_eq!(response.request_id, 1);
    }

    #[test]
    fn protocol_version_failure_is_explicit_and_does_not_call_handler() {
        let request = RequestEnvelope {
            version: PROTOCOL_VERSION + 1,
            request_id: 12,
            command: Command::GetStatus,
        };
        let response = process_request(request, |_| {
            panic!("version-invalid request reached the command handler")
        });
        assert_eq!(response.result, Err(IpcError::version_mismatch(PROTOCOL_VERSION + 1)));
    }

    #[test]
    fn semantic_control_command_and_stale_error_round_trip_at_version_four() {
        assert_eq!(PROTOCOL_VERSION, 4);
        let request = RequestEnvelope {
            version: PROTOCOL_VERSION,
            request_id: 4,
            command: Command::SetWave3Control {
                id: DeviceId(2),
                expected_generation: DeviceGeneration(7),
                control: Wave3Control::InputGain(FixedPointValue { raw: 512, fractional_bits: 8 }),
            },
        };
        assert_eq!(
            serde_json::to_value(request).expect("serialize semantic request"),
            json!({
                "version": 4,
                "request_id": 4,
                "command": {
                    "SetWave3Control": {
                        "id": 2,
                        "expected_generation": 7,
                        "control": {"InputGain": {"raw": 512, "fractional_bits": 8}}
                    }
                }
            })
        );
        let frame = encode_request(&request).expect("encode semantic request");
        assert_eq!(decode_request(&frame[4..]).expect("decode semantic request"), request);

        let error = IpcError {
            kind: IpcErrorKind::StaleDeviceGeneration {
                expected: DeviceGeneration(7),
                actual: DeviceGeneration(8),
            },
            message: "stale device generation".to_owned(),
        };
        let response = ResponseEnvelope::failure(4, error.clone());
        assert_eq!(
            serde_json::to_value(response.clone()).expect("serialize stale response"),
            json!({
                "version": 4,
                "request_id": 4,
                "result": {
                    "Err": {
                        "kind": {"StaleDeviceGeneration": {"expected": 7, "actual": 8}},
                        "message": "stale device generation"
                    }
                }
            })
        );
        let frame = encode_response(&response).expect("encode stale response");
        assert_eq!(decode_response(&frame[4..]).expect("decode stale response").result, Err(error));

        let response = ResponseEnvelope::success(4, Response::ControlChanged { device: device() });
        assert_eq!(
            serde_json::to_value(response).expect("serialize control response"),
            json!({
                "version": 4,
                "request_id": 4,
                "result": {
                    "Ok": {
                        "ControlChanged": {
                            "device": {
                                "id": 1,
                                "model": "Wave3",
                                "connection": "Connected",
                                "audio_cards": [],
                                "admission": "NotInspected"
                            }
                        }
                    }
                }
            })
        );
    }

    #[test]
    fn nested_command_response_and_error_shapes_reject_unknown_fields() {
        let request = RequestEnvelope {
            version: PROTOCOL_VERSION,
            request_id: 1,
            command: Command::InspectDevice { id: DeviceId(1) },
        };
        let mut request_json = serde_json::to_value(request).expect("serialize request");
        request_json["command"]["InspectDevice"]["unexpected"] = json!(true);
        assert!(
            decode_request(&serde_json::to_vec(&request_json).expect("encode request")).is_err()
        );

        let response =
            ResponseEnvelope::success(1, Response::DeviceInspection { device: device() });
        let mut response_json = serde_json::to_value(response).expect("serialize response");
        response_json["result"]["Ok"]["DeviceInspection"]["unexpected"] = json!(true);
        assert!(
            decode_response(&serde_json::to_vec(&response_json).expect("encode response")).is_err()
        );

        let error = ResponseEnvelope::failure(1, IpcError::device_not_found(DeviceId(1)));
        let mut error_json = serde_json::to_value(error).expect("serialize error");
        error_json["result"]["Err"]["kind"]["DeviceNotFound"]["unexpected"] = json!(true);
        assert!(decode_response(&serde_json::to_vec(&error_json).expect("encode error")).is_err());
    }

    #[test]
    fn mixer_route_round_trip_uses_named_exact_fader_steps_at_version_four() {
        let request = RequestEnvelope {
            version: PROTOCOL_VERSION,
            request_id: 18,
            command: Command::SetMixerRoute {
                expected_generation: MixerGeneration(7),
                source: SourceId::new(2),
                target: MixTarget::Stream,
                route: MixRoute::new(
                    false,
                    FaderGain::from_half_decibel_steps(-1).expect("valid fader"),
                ),
            },
        };
        let value = serde_json::to_value(request).expect("serialize mixer request");
        assert_eq!(
            value,
            json!({
                "version": 4,
                "request_id": 18,
                "command": {
                    "SetMixerRoute": {
                        "expected_generation": 7,
                        "source": 2,
                        "target": "Stream",
                        "route": {
                            "enabled": false,
                            "fader": {"half_decibel_steps": -1}
                        }
                    }
                }
            })
        );
        let bytes = serde_json::to_vec(&value).expect("encode mixer request");
        assert_eq!(decode_request(&bytes).expect("decode mixer request"), request);

        for old_fader in [json!(-1), json!(-0.5)] {
            let mut old = value.clone();
            old["command"]["SetMixerRoute"]["route"]["fader"] = old_fader;
            assert!(
                decode_request(&serde_json::to_vec(&old).expect("encode old request")).is_err()
            );
        }

        let response = ResponseEnvelope::success(
            18,
            Response::MixerRouteChanged { mixer: MixerSnapshot::default() },
        );
        let frame = encode_response(&response).expect("encode mixer response");
        assert_eq!(decode_response(&frame[4..]).expect("decode mixer response"), response);

        let mut wrong_identity = serde_json::to_value(response).expect("serialize mixer response");
        wrong_identity["result"]["Ok"]["MixerRouteChanged"]["mixer"]["profile"]["sources"][0]["name"] =
            json!("Wrong microphone");
        assert!(
            decode_response(
                &serde_json::to_vec(&wrong_identity).expect("encode invalid mixer response")
            )
            .is_err()
        );
    }

    #[test]
    fn device_refresh_contract_exposes_only_the_daemon_logical_id() {
        let request = RequestEnvelope {
            version: PROTOCOL_VERSION,
            request_id: 11,
            command: Command::InspectDevice { id: DeviceId(7) },
        };
        assert_eq!(
            serde_json::to_value(request).expect("serialize inspection"),
            json!({
                "version": PROTOCOL_VERSION,
                "request_id": 11,
                "command": {"InspectDevice": {"id": 7}}
            })
        );
        for forbidden in ["setup", "payload", "message", "field", "offset"] {
            let mut payload = serde_json::to_value(request).expect("serialize inspection");
            payload["command"]["InspectDevice"][forbidden] = json!(1);
            assert!(
                decode_request(&serde_json::to_vec(&payload).expect("encode invalid request"))
                    .is_err()
            );
        }
    }
}
