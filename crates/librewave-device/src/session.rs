use crate::DeviceIdentity;
use librewave_protocol::{
    ApiVersion, CodecError, Direction, SchemaError, SetupError, SetupPacket, Wave3Config,
    config_fields, config_schema, decode_version_response, message_transfer, version_probe,
};
use std::fmt;
use std::time::Duration;

/// The maximum duration of every Wave USB control read.
pub const CONTROL_TRANSFER_TIMEOUT: Duration = Duration::from_millis(500);

/// A transport that can perform only device-to-host USB control transfers.
///
/// The portable session deliberately has no write operation. Platform adapters
/// must return the number of bytes completed by the underlying transfer.
pub trait ReadOnlyTransport {
    /// Performs one control read into `response`.
    ///
    /// # Errors
    ///
    /// Returns a classified platform transport failure.
    fn read_control(
        &mut self,
        setup: SetupPacket,
        response: &mut [u8],
        timeout: Duration,
    ) -> Result<usize, TransportError>;
}

/// A platform transport failure that is safe to expose to the device layer.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TransportError {
    NotFound,
    PermissionDenied,
    Busy,
    TimedOut,
    Disconnected,
    Io,
}

impl fmt::Display for TransportError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::NotFound => "USB device not found",
            Self::PermissionDenied => "USB device access denied",
            Self::Busy => "USB device interface is busy",
            Self::TimedOut => "USB control read timed out",
            Self::Disconnected => "USB device disconnected",
            Self::Io => "USB transport error",
        })
    }
}

impl std::error::Error for TransportError {}

/// A schema or setup invariant that prevented session admission.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SessionSchemaError {
    Protocol(SchemaError),
    Setup(SetupError),
    Configuration(CodecError),
    TransferShape { setup_length: usize, buffer_length: usize },
}

impl fmt::Display for SessionSchemaError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Protocol(error) => write!(formatter, "protocol schema error: {error}"),
            Self::Setup(error) => write!(formatter, "USB setup schema error: {error}"),
            Self::Configuration(error) => write!(formatter, "configuration schema error: {error}"),
            Self::TransferShape { setup_length, buffer_length } => write!(
                formatter,
                "USB setup length {setup_length} does not match buffer length {buffer_length}"
            ),
        }
    }
}

impl std::error::Error for SessionSchemaError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Protocol(error) => Some(error),
            Self::Setup(error) => Some(error),
            Self::Configuration(error) => Some(error),
            Self::TransferShape { .. } => None,
        }
    }
}

/// A classified failure from a read-only Wave:3 admission probe.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SessionError {
    Transport(TransportError),
    ShortTransfer { expected: usize, actual: usize },
    LongTransfer { expected: usize, actual: usize },
    UnsupportedApi { api: ApiVersion },
    Schema(SessionSchemaError),
}

impl fmt::Display for SessionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Transport(error) => error.fmt(formatter),
            Self::ShortTransfer { expected, actual } => {
                write!(
                    formatter,
                    "short USB transfer: expected {expected} bytes, received {actual}"
                )
            }
            Self::LongTransfer { expected, actual } => {
                write!(formatter, "long USB transfer: expected {expected} bytes, received {actual}")
            }
            Self::UnsupportedApi { api } => write!(formatter, "unsupported API version {api}"),
            Self::Schema(error) => error.fmt(formatter),
        }
    }
}

impl std::error::Error for SessionError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Transport(error) => Some(error),
            Self::Schema(error) => Some(error),
            Self::ShortTransfer { .. }
            | Self::LongTransfer { .. }
            | Self::UnsupportedApi { .. } => None,
        }
    }
}

/// The admitted API version and complete initial configuration for a Wave:3.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Wave3Session {
    identity: DeviceIdentity,
    api: ApiVersion,
    config: Wave3Config,
}

impl Wave3Session {
    #[must_use]
    pub const fn identity(&self) -> DeviceIdentity {
        self.identity
    }

    #[must_use]
    pub const fn api(&self) -> ApiVersion {
        self.api
    }

    #[must_use]
    pub const fn config(&self) -> &Wave3Config {
        &self.config
    }
}

/// Reads the API version first, admits its exact schema, and then reads the
/// complete 16-byte Wave:3 configuration.
///
/// # Errors
///
/// Returns a classified transport, transfer-length, API, or schema failure.
pub fn probe_wave3(
    transport: &mut impl ReadOnlyTransport,
    interface: u8,
) -> Result<Wave3Session, SessionError> {
    let mut version = [0; 2];
    read_exact(transport, version_probe(interface), &mut version)?;
    let api = decode_version_response(version);
    let schema = match config_schema(api) {
        Ok(schema) => schema,
        Err(SchemaError::UnsupportedApi { .. }) => {
            return Err(SessionError::UnsupportedApi { api });
        }
        Err(error) => return Err(SessionError::Schema(SessionSchemaError::Protocol(error))),
    };

    let setup = message_transfer(interface, Direction::In, schema)
        .map_err(|error| SessionError::Schema(SessionSchemaError::Setup(error)))?;
    let mut payload = [0; 16];
    read_exact(transport, setup, &mut payload)?;
    let config = Wave3Config::from_schema(schema, &payload)
        .map_err(|error| SessionError::Schema(SessionSchemaError::Configuration(error)))?;
    for field in config_fields() {
        config
            .get(field.field())
            .map_err(|error| SessionError::Schema(SessionSchemaError::Configuration(error)))?;
    }

    Ok(Wave3Session { identity: DeviceIdentity::wave3(), api, config })
}

fn read_exact(
    transport: &mut impl ReadOnlyTransport,
    setup: SetupPacket,
    response: &mut [u8],
) -> Result<(), SessionError> {
    let expected = usize::from(setup.length);
    if response.len() != expected {
        return Err(SessionError::Schema(SessionSchemaError::TransferShape {
            setup_length: expected,
            buffer_length: response.len(),
        }));
    }
    let actual = transport
        .read_control(setup, response, CONTROL_TRANSFER_TIMEOUT)
        .map_err(SessionError::Transport)?;
    if actual < expected {
        return Err(SessionError::ShortTransfer { expected, actual });
    }
    if actual > expected {
        return Err(SessionError::LongTransfer { expected, actual });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;

    #[derive(Debug)]
    struct ReadResult {
        response: Vec<u8>,
        completed: usize,
    }

    #[derive(Debug, Default)]
    struct FakeTransport {
        reads: VecDeque<Result<ReadResult, TransportError>>,
        requests: Vec<(SetupPacket, usize, Duration)>,
    }

    impl FakeTransport {
        fn admitted(api: ApiVersion) -> Self {
            let mut config = vec![0; 16];
            config[12] = 1;
            Self {
                reads: VecDeque::from([
                    Ok(ReadResult { response: vec![api.major, api.minor], completed: 2 }),
                    Ok(ReadResult { response: config, completed: 16 }),
                ]),
                requests: Vec::new(),
            }
        }
    }

    impl ReadOnlyTransport for FakeTransport {
        fn read_control(
            &mut self,
            setup: SetupPacket,
            response: &mut [u8],
            timeout: Duration,
        ) -> Result<usize, TransportError> {
            self.requests.push((setup, response.len(), timeout));
            match self.reads.pop_front().expect("unexpected control read") {
                Ok(result) => {
                    let copied = response.len().min(result.response.len());
                    response[..copied].copy_from_slice(&result.response[..copied]);
                    Ok(result.completed)
                }
                Err(error) => Err(error),
            }
        }
    }

    #[test]
    fn probe_uses_protocol_setup_packets_and_fixed_timeout() {
        let mut transport = FakeTransport::admitted(ApiVersion::new(5, 4));
        let session = probe_wave3(&mut transport, 7).expect("admit Wave:3");

        assert_eq!(session.identity(), DeviceIdentity::wave3());
        assert_eq!(session.api(), ApiVersion::new(5, 4));
        assert_eq!(session.config().as_bytes()[12], 1);
        assert_eq!(transport.requests.len(), 2);
        assert_eq!(transport.requests[0], (version_probe(7), 2, CONTROL_TRANSFER_TIMEOUT,));
        assert_eq!(
            transport.requests[0].0.to_bytes(),
            [0xa1, 0x85, 0x0a, 0x00, 0x07, 0x33, 0x02, 0x00]
        );
        assert_eq!(
            transport.requests[1].0.to_bytes(),
            [0xa1, 0x85, 0x00, 0x00, 0x07, 0x33, 0x10, 0x00]
        );
        assert_eq!(transport.requests[1].1, 16);
        assert_eq!(transport.requests[1].2, CONTROL_TRANSFER_TIMEOUT);
    }

    #[test]
    fn admits_each_reviewed_api_version() {
        for api in [ApiVersion::new(5, 3), ApiVersion::new(5, 4)] {
            let mut transport = FakeTransport::admitted(api);
            assert_eq!(probe_wave3(&mut transport, 9).map(|session| session.api()), Ok(api));
        }
    }

    #[test]
    fn unsupported_api_stops_before_configuration_read() {
        let mut transport = FakeTransport {
            reads: VecDeque::from([Ok(ReadResult { response: vec![5, 2], completed: 2 })]),
            requests: Vec::new(),
        };

        assert_eq!(
            probe_wave3(&mut transport, 4),
            Err(SessionError::UnsupportedApi { api: ApiVersion::new(5, 2) })
        );
        assert_eq!(transport.requests.len(), 1);
    }

    #[test]
    fn rejects_short_version_and_configuration_reads() {
        let mut short_version = FakeTransport {
            reads: VecDeque::from([Ok(ReadResult { response: vec![5], completed: 1 })]),
            requests: Vec::new(),
        };
        assert_eq!(
            probe_wave3(&mut short_version, 2),
            Err(SessionError::ShortTransfer { expected: 2, actual: 1 })
        );

        let mut short_config = FakeTransport::admitted(ApiVersion::new(5, 4));
        short_config.reads[1] = Ok(ReadResult { response: vec![0; 15], completed: 15 });
        assert_eq!(
            probe_wave3(&mut short_config, 2),
            Err(SessionError::ShortTransfer { expected: 16, actual: 15 })
        );
    }

    #[test]
    fn preserves_classified_transport_errors() {
        for error in
            [TransportError::NotFound, TransportError::PermissionDenied, TransportError::Busy]
        {
            let mut transport =
                FakeTransport { reads: VecDeque::from([Err(error)]), requests: Vec::new() };
            assert_eq!(probe_wave3(&mut transport, 6), Err(SessionError::Transport(error)));
        }
    }

    #[test]
    fn rejects_configuration_values_outside_the_admitted_schema() {
        let mut transport = FakeTransport::admitted(ApiVersion::new(5, 4));
        let mut malformed = vec![0; 16];
        malformed[12] = 4;
        transport.reads[1] = Ok(ReadResult { response: malformed, completed: 16 });

        assert!(matches!(
            probe_wave3(&mut transport, 8),
            Err(SessionError::Schema(SessionSchemaError::Configuration(CodecError::Value { .. })))
        ));
    }
}
