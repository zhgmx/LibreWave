use crate::{
    CONTROL_TRANSFER_TIMEOUT, ReadOnlyTransport, SessionSchemaError, TransportError, Wave3Session,
    Wave3WriteState,
};
use librewave_protocol::{
    CodecError, Direction, PatchResult, SetupPacket, Wave3Config, Wave3ControlChange,
    config_fields, config_schema, message_transfer,
};
use std::fmt;
use std::time::Duration;

/// One reviewed host-to-device transfer created by an admitted session.
///
/// The device layer keeps construction private. Platform transports can inspect
/// the exact setup packet and complete payload, but cannot create another write
/// path through this trait.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ControlWriteRequest<'a> {
    setup: SetupPacket,
    payload: &'a [u8],
}

impl ControlWriteRequest<'_> {
    #[must_use]
    pub const fn setup(&self) -> SetupPacket {
        self.setup
    }

    #[must_use]
    pub const fn payload(&self) -> &[u8] {
        self.payload
    }
}

/// A transport that can perform one host-to-device USB control transfer.
///
/// Transaction callers must also provide [`ReadOnlyTransport`] so every write
/// can be checked against the device and restored after a failure.
pub trait ControlWriteTransport {
    /// Performs the exact control write created by an admitted session.
    ///
    /// # Errors
    ///
    /// Returns a classified platform transport failure.
    fn write_control(
        &mut self,
        request: ControlWriteRequest<'_>,
        timeout: Duration,
    ) -> Result<usize, TransportError>;
}

/// The step that produced a transaction transfer failure.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TransactionPhase {
    PreflightRead,
    Write,
    Readback,
    RestorationWrite,
    RestorationReadback,
}

/// A precise failure from one Wave:3 control transaction step.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TransactionError {
    NeedsReprobe,
    StaleBaseline { expected: Wave3Config, actual: Wave3Config },
    InvalidChange(CodecError),
    Schema(SessionSchemaError),
    Transport { phase: TransactionPhase, error: TransportError },
    ShortTransfer { phase: TransactionPhase, expected: usize, actual: usize },
    LongTransfer { phase: TransactionPhase, expected: usize, actual: usize },
    InvalidConfiguration { phase: TransactionPhase, error: CodecError },
    ReadbackMismatch { phase: TransactionPhase, expected: Wave3Config, actual: Wave3Config },
}

impl fmt::Display for TransactionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NeedsReprobe => formatter.write_str("session needs a fresh admission probe"),
            Self::StaleBaseline { .. } => formatter.write_str("Wave:3 baseline is stale"),
            Self::InvalidChange(error) => write!(formatter, "invalid control change: {error}"),
            Self::Schema(error) => error.fmt(formatter),
            Self::Transport { phase, error } => {
                write!(formatter, "{phase:?} transport failed: {error}")
            }
            Self::ShortTransfer { phase, expected, actual }
            | Self::LongTransfer { phase, expected, actual } => {
                write!(formatter, "{phase:?} completed {actual} bytes instead of {expected}")
            }
            Self::InvalidConfiguration { phase, error } => {
                write!(formatter, "{phase:?} returned an invalid configuration: {error}")
            }
            Self::ReadbackMismatch { phase, .. } => {
                write!(formatter, "{phase:?} did not match the complete expected configuration")
            }
        }
    }
}

impl std::error::Error for TransactionError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::InvalidChange(error) | Self::InvalidConfiguration { error, .. } => Some(error),
            Self::Schema(error) => Some(error),
            Self::Transport { error, .. } => Some(error),
            Self::NeedsReprobe
            | Self::StaleBaseline { .. }
            | Self::ShortTransfer { .. }
            | Self::LongTransfer { .. }
            | Self::ReadbackMismatch { .. } => None,
        }
    }
}

/// The write and verification results from one restoration attempt.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RestorationOutcome {
    /// The result of sending the complete original payload.
    pub write: Result<(), TransactionError>,
    /// The result of reading and matching the complete original payload.
    pub verification: Result<Wave3Config, TransactionError>,
}

/// The complete result of one control transaction.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TransactionOutcome {
    Applied { config: Wave3Config },
    Unchanged { config: Wave3Config },
    Rejected { error: TransactionError },
    Failed { primary: TransactionError, restoration: RestorationOutcome },
}

impl Wave3Session {
    /// Applies one reviewed hardware control change and verifies the complete payload.
    ///
    /// `expected_baseline` must be the caller's last copy of [`Self::config`].
    /// Normal use must keep this session on the same transport connection that
    /// admitted it. The stored control interface is used for every transfer.
    /// The transaction reads the device before the write and rejects a stale
    /// baseline. Any failure after a write attempt starts one restoration
    /// attempt with the complete original payload.
    #[must_use]
    pub fn transact_control<T>(
        &mut self,
        transport: &mut T,
        expected_baseline: Wave3Config,
        change: Wave3ControlChange,
    ) -> TransactionOutcome
    where
        T: ReadOnlyTransport + ControlWriteTransport,
    {
        if self.write_state != Wave3WriteState::Ready {
            return TransactionOutcome::Rejected { error: TransactionError::NeedsReprobe };
        }
        if expected_baseline != self.config {
            return TransactionOutcome::Rejected {
                error: TransactionError::StaleBaseline {
                    expected: expected_baseline,
                    actual: self.config,
                },
            };
        }

        let mut requested = expected_baseline;
        let patch = match requested.apply_control(change) {
            Ok(patch) => patch,
            Err(error) => {
                return TransactionOutcome::Rejected {
                    error: TransactionError::InvalidChange(error),
                };
            }
        };
        let (read_setup, write_setup) = match transaction_setup(self) {
            Ok(setup) => setup,
            Err(error) => return TransactionOutcome::Rejected { error },
        };
        let current =
            match read_config(transport, read_setup, self.api, TransactionPhase::PreflightRead) {
                Ok(config) => config,
                Err(error) => return TransactionOutcome::Rejected { error },
            };
        if current != expected_baseline {
            self.config = current;
            return TransactionOutcome::Rejected {
                error: TransactionError::StaleBaseline {
                    expected: expected_baseline,
                    actual: current,
                },
            };
        }
        if patch == PatchResult::Unchanged {
            return TransactionOutcome::Unchanged { config: current };
        }

        let primary = match write_exact(
            transport,
            write_setup,
            requested.as_bytes(),
            TransactionPhase::Write,
        ) {
            Ok(()) => {
                match read_config(transport, read_setup, self.api, TransactionPhase::Readback) {
                    Ok(readback) if readback == requested => {
                        self.config = readback;
                        return TransactionOutcome::Applied { config: readback };
                    }
                    Ok(readback) => TransactionError::ReadbackMismatch {
                        phase: TransactionPhase::Readback,
                        expected: requested,
                        actual: readback,
                    },
                    Err(error) => error,
                }
            }
            Err(error) => error,
        };

        let restoration = restore(transport, read_setup, write_setup, self.api, expected_baseline);
        if let Ok(config) = restoration.verification {
            self.config = config;
        } else {
            self.write_state = Wave3WriteState::NeedsReprobe;
        }
        TransactionOutcome::Failed { primary, restoration }
    }
}

fn transaction_setup(
    session: &Wave3Session,
) -> Result<(SetupPacket, SetupPacket), TransactionError> {
    let schema = config_schema(session.api)
        .map_err(|error| TransactionError::Schema(SessionSchemaError::Protocol(error)))?;
    let read = message_transfer(session.control_interface, Direction::In, schema)
        .map_err(|error| TransactionError::Schema(SessionSchemaError::Setup(error)))?;
    let write = message_transfer(session.control_interface, Direction::Out, schema)
        .map_err(|error| TransactionError::Schema(SessionSchemaError::Setup(error)))?;
    Ok((read, write))
}

fn read_config(
    transport: &mut impl ReadOnlyTransport,
    setup: SetupPacket,
    api: librewave_protocol::ApiVersion,
    phase: TransactionPhase,
) -> Result<Wave3Config, TransactionError> {
    let expected = usize::from(setup.length);
    let mut payload = [0; 16];
    if payload.len() != expected {
        return Err(TransactionError::Schema(SessionSchemaError::TransferShape {
            setup_length: expected,
            buffer_length: payload.len(),
        }));
    }
    let actual = transport
        .read_control(setup, &mut payload, CONTROL_TRANSFER_TIMEOUT)
        .map_err(|error| TransactionError::Transport { phase, error })?;
    check_length(phase, expected, actual)?;
    let schema = config_schema(api)
        .map_err(|error| TransactionError::Schema(SessionSchemaError::Protocol(error)))?;
    let config = Wave3Config::from_schema(schema, &payload)
        .map_err(|error| TransactionError::InvalidConfiguration { phase, error })?;
    for field in config_fields() {
        config
            .get(field.field())
            .map_err(|error| TransactionError::InvalidConfiguration { phase, error })?;
    }
    Ok(config)
}

fn write_exact(
    transport: &mut impl ControlWriteTransport,
    setup: SetupPacket,
    payload: &[u8],
    phase: TransactionPhase,
) -> Result<(), TransactionError> {
    let expected = usize::from(setup.length);
    if payload.len() != expected {
        return Err(TransactionError::Schema(SessionSchemaError::TransferShape {
            setup_length: expected,
            buffer_length: payload.len(),
        }));
    }
    let actual = transport
        .write_control(ControlWriteRequest { setup, payload }, CONTROL_TRANSFER_TIMEOUT)
        .map_err(|error| TransactionError::Transport { phase, error })?;
    check_length(phase, expected, actual)
}

fn check_length(
    phase: TransactionPhase,
    expected: usize,
    actual: usize,
) -> Result<(), TransactionError> {
    match actual.cmp(&expected) {
        std::cmp::Ordering::Less => {
            Err(TransactionError::ShortTransfer { phase, expected, actual })
        }
        std::cmp::Ordering::Greater => {
            Err(TransactionError::LongTransfer { phase, expected, actual })
        }
        std::cmp::Ordering::Equal => Ok(()),
    }
}

fn restore<T>(
    transport: &mut T,
    read_setup: SetupPacket,
    write_setup: SetupPacket,
    api: librewave_protocol::ApiVersion,
    original: Wave3Config,
) -> RestorationOutcome
where
    T: ReadOnlyTransport + ControlWriteTransport,
{
    let write = write_exact(
        transport,
        write_setup,
        original.as_bytes(),
        TransactionPhase::RestorationWrite,
    );
    let verification =
        match read_config(transport, read_setup, api, TransactionPhase::RestorationReadback) {
            Ok(readback) if readback == original => Ok(readback),
            Ok(readback) => Err(TransactionError::ReadbackMismatch {
                phase: TransactionPhase::RestorationReadback,
                expected: original,
                actual: readback,
            }),
            Err(error) => Err(error),
        };
    RestorationOutcome { write, verification }
}

#[cfg(test)]
#[path = "transaction/tests.rs"]
mod tests;
