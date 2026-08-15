use super::*;
use crate::probe_wave3;
use librewave_protocol::{ApiVersion, VolumeSelect, Wave3GainDb};
use std::collections::VecDeque;

#[derive(Clone, Debug, Eq, PartialEq)]
enum Event {
    Read { setup: SetupPacket, length: usize, timeout: Duration },
    Write { setup: SetupPacket, payload: Vec<u8>, timeout: Duration },
}

#[derive(Debug)]
struct ReadResult {
    payload: Vec<u8>,
    completed: usize,
}

#[derive(Debug, Default)]
struct FakeTransport {
    reads: VecDeque<Result<ReadResult, TransportError>>,
    writes: VecDeque<Result<usize, TransportError>>,
    events: Vec<Event>,
}

impl FakeTransport {
    fn with_probe(api: ApiVersion, baseline: [u8; 16]) -> Self {
        Self {
            reads: VecDeque::from([
                Ok(ReadResult { payload: vec![api.major, api.minor], completed: 2 }),
                Ok(ReadResult { payload: baseline.to_vec(), completed: 16 }),
            ]),
            writes: VecDeque::new(),
            events: Vec::new(),
        }
    }

    fn read(&mut self, payload: [u8; 16]) {
        self.reads.push_back(Ok(ReadResult { payload: payload.to_vec(), completed: 16 }));
    }

    fn read_with_length(&mut self, payload: [u8; 16], completed: usize) {
        self.reads.push_back(Ok(ReadResult { payload: payload.to_vec(), completed }));
    }

    fn read_error(&mut self, error: TransportError) {
        self.reads.push_back(Err(error));
    }

    fn admit(&mut self, interface: u8) -> Wave3Session {
        let session = probe_wave3(self, interface).expect("admitted fake Wave:3");
        self.events.clear();
        session
    }
}

impl ReadOnlyTransport for FakeTransport {
    fn read_control(
        &mut self,
        setup: SetupPacket,
        response: &mut [u8],
        timeout: Duration,
    ) -> Result<usize, TransportError> {
        self.events.push(Event::Read { setup, length: response.len(), timeout });
        match self.reads.pop_front().expect("unexpected fake read") {
            Ok(result) => {
                let copied = response.len().min(result.payload.len());
                response[..copied].copy_from_slice(&result.payload[..copied]);
                Ok(result.completed)
            }
            Err(error) => Err(error),
        }
    }
}

impl ControlWriteTransport for FakeTransport {
    fn write_control(
        &mut self,
        request: ControlWriteRequest<'_>,
        timeout: Duration,
    ) -> Result<usize, TransportError> {
        self.events.push(Event::Write {
            setup: request.setup(),
            payload: request.payload().to_vec(),
            timeout,
        });
        self.writes.pop_front().expect("unexpected fake write")
    }
}

fn baseline() -> [u8; 16] {
    let mut payload = [0; 16];
    payload[0..2].copy_from_slice(&512i16.to_le_bytes());
    payload[2] = 0xa5;
    payload[3] = 0x5a;
    payload[7..9].copy_from_slice(&(-7_680i16).to_le_bytes());
    payload[10..12].copy_from_slice(&12_800i16.to_le_bytes());
    payload[12] = VolumeSelect::Mic as u8;
    payload
}

fn gain_change() -> Wave3ControlChange {
    Wave3ControlChange::MicrophoneGain(Wave3GainDb::from_raw_q8_8(1_024).expect("schema gain"))
}

fn changed_gain(mut payload: [u8; 16]) -> [u8; 16] {
    payload[0..2].copy_from_slice(&1_024i16.to_le_bytes());
    payload
}

fn assert_write_locked(session: &mut Wave3Session, transport: &mut FakeTransport) {
    assert_eq!(session.write_state(), Wave3WriteState::NeedsReprobe);
    let event_count = transport.events.len();
    let expected = *session.config();
    assert_eq!(
        session.transact_control(transport, expected, gain_change()),
        TransactionOutcome::Rejected { error: TransactionError::NeedsReprobe }
    );
    assert_eq!(transport.events.len(), event_count);
}

fn run_with_restoration_read(
    restoration_read: Result<ReadResult, TransportError>,
) -> (Wave3Session, FakeTransport, TransactionOutcome) {
    let original = baseline();
    let requested = changed_gain(original);
    let mut primary_mismatch = requested;
    primary_mismatch[15] = 1;
    let mut transport = FakeTransport::with_probe(ApiVersion::new(5, 4), original);
    let mut session = transport.admit(4);
    let expected = *session.config();
    transport.read(original);
    transport.writes.extend([Ok(16), Ok(16)]);
    transport.read(primary_mismatch);
    transport.reads.push_back(restoration_read);
    let outcome = session.transact_control(&mut transport, expected, gain_change());
    (session, transport, outcome)
}

#[test]
fn writes_one_complete_payload_and_verifies_exact_readback() {
    let original = baseline();
    let requested = changed_gain(original);
    let mut transport = FakeTransport::with_probe(ApiVersion::new(5, 4), original);
    let mut session = transport.admit(7);
    let expected = *session.config();
    transport.read(original);
    transport.writes.push_back(Ok(16));
    transport.read(requested);

    assert_eq!(
        session.transact_control(&mut transport, expected, gain_change()),
        TransactionOutcome::Applied { config: *session.config() }
    );
    assert_eq!(session.config().as_bytes(), &requested);
    assert_eq!(&requested[2..4], &original[2..4]);
    assert_eq!(transport.events.len(), 3);
    let read_setup = match &transport.events[0] {
        Event::Read { setup, length, timeout } => {
            assert_eq!(*length, 16);
            assert_eq!(*timeout, CONTROL_TRANSFER_TIMEOUT);
            *setup
        }
        Event::Write { .. } => panic!("preflight was not a read"),
    };
    assert_eq!(read_setup.to_bytes(), [0xa1, 0x85, 0, 0, 7, 0x33, 16, 0]);
    match &transport.events[1] {
        Event::Write { setup, payload, timeout } => {
            assert_eq!(setup.to_bytes(), [0x21, 0x05, 0, 0, 7, 0x33, 16, 0]);
            assert_eq!(payload, &requested);
            assert_eq!(*timeout, CONTROL_TRANSFER_TIMEOUT);
        }
        Event::Read { .. } => panic!("second transfer was not a write"),
    }
    assert_eq!(
        transport.events[2],
        Event::Read { setup: read_setup, length: 16, timeout: CONTROL_TRANSFER_TIMEOUT }
    );
}

#[test]
fn uses_the_admitted_interface_for_api_5_3_and_5_4() {
    for api in [ApiVersion::new(5, 3), ApiVersion::new(5, 4)] {
        let original = baseline();
        let requested = changed_gain(original);
        let mut transport = FakeTransport::with_probe(api, original);
        let mut session = transport.admit(11);
        let expected = *session.config();
        transport.read(original);
        transport.writes.push_back(Ok(16));
        transport.read(requested);

        assert!(matches!(
            session.transact_control(&mut transport, expected, gain_change()),
            TransactionOutcome::Applied { .. }
        ));
        let setup_bytes: Vec<_> = transport
            .events
            .iter()
            .map(|event| match event {
                Event::Read { setup, .. } | Event::Write { setup, .. } => setup.to_bytes(),
            })
            .collect();
        assert_eq!(
            setup_bytes,
            [
                [0xa1, 0x85, 0, 0, 11, 0x33, 16, 0],
                [0x21, 0x05, 0, 0, 11, 0x33, 16, 0],
                [0xa1, 0x85, 0, 0, 11, 0x33, 16, 0],
            ]
        );
        assert!(matches!(
            &transport.events[1],
            Event::Write { payload, timeout, .. }
                if payload == &requested && *timeout == CONTROL_TRANSFER_TIMEOUT
        ));
    }
}

#[test]
fn rejects_a_device_side_stale_baseline_before_any_write() {
    let original = baseline();
    let mut newer = original;
    newer[4] = 1;
    let mut transport = FakeTransport::with_probe(ApiVersion::new(5, 3), original);
    let mut session = transport.admit(3);
    let expected = *session.config();
    transport.read(newer);

    assert_eq!(
        session.transact_control(&mut transport, expected, gain_change()),
        TransactionOutcome::Rejected {
            error: TransactionError::StaleBaseline { expected, actual: *session.config() }
        }
    );
    assert_eq!(session.config().as_bytes(), &newer);
    assert_eq!(transport.events.len(), 1);
    assert!(matches!(transport.events[0], Event::Read { .. }));
}

#[test]
fn unchanged_and_caller_stale_requests_never_write() {
    let original = baseline();
    let mut unchanged_transport = FakeTransport::with_probe(ApiVersion::new(5, 4), original);
    let mut unchanged_session = unchanged_transport.admit(6);
    let expected = *unchanged_session.config();
    unchanged_transport.read(original);
    let current_gain =
        Wave3ControlChange::MicrophoneGain(Wave3GainDb::from_raw_q8_8(512).expect("schema gain"));
    assert_eq!(
        unchanged_session.transact_control(&mut unchanged_transport, expected, current_gain),
        TransactionOutcome::Unchanged { config: expected }
    );
    assert_eq!(unchanged_transport.events.len(), 1);
    assert!(matches!(unchanged_transport.events[0], Event::Read { .. }));

    let mut stale_transport = FakeTransport::with_probe(ApiVersion::new(5, 4), original);
    let mut stale_session = stale_transport.admit(6);
    let actual = *stale_session.config();
    let mut stale = actual;
    stale.apply_control(Wave3ControlChange::GainLock(true)).expect("stale test baseline");
    assert_eq!(
        stale_session.transact_control(&mut stale_transport, stale, gain_change()),
        TransactionOutcome::Rejected {
            error: TransactionError::StaleBaseline { expected: stale, actual }
        }
    );
    assert!(stale_transport.events.is_empty());
}

#[test]
fn preflight_failures_are_classified_before_any_write() {
    for actual in [15, 17] {
        let original = baseline();
        let mut transport = FakeTransport::with_probe(ApiVersion::new(5, 4), original);
        let mut session = transport.admit(8);
        let expected = *session.config();
        transport.read_with_length(original, actual);
        let error = if actual < 16 {
            TransactionError::ShortTransfer {
                phase: TransactionPhase::PreflightRead,
                expected: 16,
                actual,
            }
        } else {
            TransactionError::LongTransfer {
                phase: TransactionPhase::PreflightRead,
                expected: 16,
                actual,
            }
        };
        assert_eq!(
            session.transact_control(&mut transport, expected, gain_change()),
            TransactionOutcome::Rejected { error }
        );
        assert_eq!(transport.events.len(), 1);
        assert!(matches!(transport.events[0], Event::Read { .. }));
    }

    for error in [TransportError::TimedOut, TransportError::Disconnected] {
        let original = baseline();
        let mut transport = FakeTransport::with_probe(ApiVersion::new(5, 4), original);
        let mut session = transport.admit(8);
        let expected = *session.config();
        transport.read_error(error);
        assert_eq!(
            session.transact_control(&mut transport, expected, gain_change()),
            TransactionOutcome::Rejected {
                error: TransactionError::Transport {
                    phase: TransactionPhase::PreflightRead,
                    error,
                }
            }
        );
        assert_eq!(transport.events.len(), 1);
        assert!(matches!(transport.events[0], Event::Read { .. }));
    }
}

#[test]
fn short_and_long_writes_restore_and_verify_the_original_payload() {
    for actual in [15, 17] {
        let original = baseline();
        let requested = changed_gain(original);
        let mut transport = FakeTransport::with_probe(ApiVersion::new(5, 4), original);
        let mut session = transport.admit(5);
        let expected = *session.config();
        transport.read(original);
        transport.writes.extend([Ok(actual), Ok(16)]);
        transport.read(original);

        let outcome = session.transact_control(&mut transport, expected, gain_change());
        let primary = if actual < 16 {
            TransactionError::ShortTransfer { phase: TransactionPhase::Write, expected: 16, actual }
        } else {
            TransactionError::LongTransfer { phase: TransactionPhase::Write, expected: 16, actual }
        };
        assert_eq!(
            outcome,
            TransactionOutcome::Failed {
                primary,
                restoration: RestorationOutcome { write: Ok(()), verification: Ok(expected) },
            }
        );
        let writes: Vec<_> = transport
            .events
            .iter()
            .filter_map(|event| match event {
                Event::Write { payload, .. } => Some(payload.as_slice()),
                Event::Read { .. } => None,
            })
            .collect();
        assert_eq!(writes, [requested.as_slice(), original.as_slice()]);
        assert_eq!(session.write_state(), Wave3WriteState::Ready);
    }
}

#[test]
fn readback_transfer_and_transport_failures_restore_the_original() {
    for actual in [15, 17] {
        let original = baseline();
        let requested = changed_gain(original);
        let mut transport = FakeTransport::with_probe(ApiVersion::new(5, 4), original);
        let mut session = transport.admit(5);
        let expected = *session.config();
        transport.read(original);
        transport.writes.extend([Ok(16), Ok(16)]);
        transport.read_with_length(requested, actual);
        transport.read(original);

        let primary = if actual < 16 {
            TransactionError::ShortTransfer {
                phase: TransactionPhase::Readback,
                expected: 16,
                actual,
            }
        } else {
            TransactionError::LongTransfer {
                phase: TransactionPhase::Readback,
                expected: 16,
                actual,
            }
        };
        assert_eq!(
            session.transact_control(&mut transport, expected, gain_change()),
            TransactionOutcome::Failed {
                primary,
                restoration: RestorationOutcome { write: Ok(()), verification: Ok(expected) },
            }
        );
        assert_eq!(session.write_state(), Wave3WriteState::Ready);
    }

    for error in [TransportError::TimedOut, TransportError::Disconnected] {
        let original = baseline();
        let mut transport = FakeTransport::with_probe(ApiVersion::new(5, 4), original);
        let mut session = transport.admit(5);
        let expected = *session.config();
        transport.read(original);
        transport.writes.extend([Ok(16), Ok(16)]);
        transport.read_error(error);
        transport.read(original);
        assert_eq!(
            session.transact_control(&mut transport, expected, gain_change()),
            TransactionOutcome::Failed {
                primary: TransactionError::Transport { phase: TransactionPhase::Readback, error },
                restoration: RestorationOutcome { write: Ok(()), verification: Ok(expected) },
            }
        );
        assert_eq!(session.write_state(), Wave3WriteState::Ready);
    }
}

#[test]
fn invalid_readback_is_classified_and_restored() {
    let original = baseline();
    let mut invalid = changed_gain(original);
    invalid[4] = 2;
    let mut transport = FakeTransport::with_probe(ApiVersion::new(5, 4), original);
    let mut session = transport.admit(5);
    let expected = *session.config();
    transport.read(original);
    transport.writes.extend([Ok(16), Ok(16)]);
    transport.read(invalid);
    transport.read(original);

    let outcome = session.transact_control(&mut transport, expected, gain_change());
    assert!(matches!(
        outcome,
        TransactionOutcome::Failed {
            primary: TransactionError::InvalidConfiguration {
                phase: TransactionPhase::Readback,
                ..
            },
            restoration: RestorationOutcome { write: Ok(()), verification: Ok(_) },
        }
    ));
    assert_eq!(session.config(), &expected);
}

#[test]
fn readback_mismatch_returns_primary_and_verified_restoration() {
    let original = baseline();
    let requested = changed_gain(original);
    let mut mismatched = requested;
    mismatched[15] = 1;
    let mut transport = FakeTransport::with_probe(ApiVersion::new(5, 4), original);
    let mut session = transport.admit(2);
    let expected = *session.config();
    transport.read(original);
    transport.writes.extend([Ok(16), Ok(16)]);
    transport.read(mismatched);
    transport.read(original);

    assert_eq!(
        session.transact_control(&mut transport, expected, gain_change()),
        TransactionOutcome::Failed {
            primary: TransactionError::ReadbackMismatch {
                phase: TransactionPhase::Readback,
                expected: Wave3Config::from_schema(
                    config_schema(ApiVersion::new(5, 4)).expect("schema"),
                    &requested,
                )
                .expect("requested config"),
                actual: Wave3Config::from_schema(
                    config_schema(ApiVersion::new(5, 4)).expect("schema"),
                    &mismatched,
                )
                .expect("mismatched config"),
            },
            restoration: RestorationOutcome { write: Ok(()), verification: Ok(expected) },
        }
    );
    assert_eq!(session.config(), &expected);
}

#[test]
fn restoration_short_and_long_writes_keep_their_error_after_verified_readback() {
    for actual in [15, 17] {
        let original = baseline();
        let requested = changed_gain(original);
        let mut primary_mismatch = requested;
        primary_mismatch[15] = 1;
        let mut transport = FakeTransport::with_probe(ApiVersion::new(5, 4), original);
        let mut session = transport.admit(4);
        let expected = *session.config();
        transport.read(original);
        transport.writes.extend([Ok(16), Ok(actual)]);
        transport.read(primary_mismatch);
        transport.read(original);

        let outcome = session.transact_control(&mut transport, expected, gain_change());
        let write = if actual < 16 {
            TransactionError::ShortTransfer {
                phase: TransactionPhase::RestorationWrite,
                expected: 16,
                actual,
            }
        } else {
            TransactionError::LongTransfer {
                phase: TransactionPhase::RestorationWrite,
                expected: 16,
                actual,
            }
        };
        assert!(matches!(
            outcome,
            TransactionOutcome::Failed {
                primary: TransactionError::ReadbackMismatch {
                    phase: TransactionPhase::Readback,
                    ..
                },
                restoration: RestorationOutcome {
                    write: Err(error),
                    verification: Ok(config),
                },
            } if error == write && config == expected
        ));
        assert_eq!(session.write_state(), Wave3WriteState::Ready);
        assert_eq!(session.config(), &expected);
    }
}

#[test]
fn every_unverified_restoration_class_locks_future_writes() {
    let original = baseline();
    let requested = changed_gain(original);
    let (mut session, mut transport, outcome) =
        run_with_restoration_read(Ok(ReadResult { payload: requested.to_vec(), completed: 16 }));
    assert!(matches!(
        outcome,
        TransactionOutcome::Failed {
            restoration: RestorationOutcome {
                verification: Err(TransactionError::ReadbackMismatch {
                    phase: TransactionPhase::RestorationReadback,
                    ..
                }),
                ..
            },
            ..
        }
    ));
    assert_write_locked(&mut session, &mut transport);

    for actual in [15, 17] {
        let (mut session, mut transport, outcome) = run_with_restoration_read(Ok(ReadResult {
            payload: original.to_vec(),
            completed: actual,
        }));
        let verification = if actual < 16 {
            TransactionError::ShortTransfer {
                phase: TransactionPhase::RestorationReadback,
                expected: 16,
                actual,
            }
        } else {
            TransactionError::LongTransfer {
                phase: TransactionPhase::RestorationReadback,
                expected: 16,
                actual,
            }
        };
        assert!(matches!(
            outcome,
            TransactionOutcome::Failed {
                restoration: RestorationOutcome {
                    verification: Err(error),
                    ..
                },
                ..
            } if error == verification
        ));
        assert_write_locked(&mut session, &mut transport);
    }

    for error in [TransportError::TimedOut, TransportError::Disconnected] {
        let (mut session, mut transport, outcome) = run_with_restoration_read(Err(error));
        assert!(matches!(
            outcome,
            TransactionOutcome::Failed {
                restoration: RestorationOutcome {
                    verification: Err(TransactionError::Transport {
                        phase: TransactionPhase::RestorationReadback,
                        error: actual,
                    }),
                    ..
                },
                ..
            } if actual == error
        ));
        assert_write_locked(&mut session, &mut transport);
    }

    let mut invalid = original;
    invalid[4] = 2;
    let (mut session, mut transport, outcome) =
        run_with_restoration_read(Ok(ReadResult { payload: invalid.to_vec(), completed: 16 }));
    assert!(matches!(
        outcome,
        TransactionOutcome::Failed {
            restoration: RestorationOutcome {
                verification: Err(TransactionError::InvalidConfiguration {
                    phase: TransactionPhase::RestorationReadback,
                    ..
                }),
                ..
            },
            ..
        }
    ));
    assert_write_locked(&mut session, &mut transport);
}

#[test]
fn timeout_and_disconnect_failures_preserve_both_errors_and_require_reprobe() {
    for primary_error in [TransportError::TimedOut, TransportError::Disconnected] {
        let original = baseline();
        let mut transport = FakeTransport::with_probe(ApiVersion::new(5, 4), original);
        let mut session = transport.admit(1);
        let expected = *session.config();
        transport.read(original);
        transport.writes.extend([Err(primary_error), Err(TransportError::Disconnected)]);
        transport.reads.push_back(Err(TransportError::Disconnected));

        assert_eq!(
            session.transact_control(&mut transport, expected, gain_change()),
            TransactionOutcome::Failed {
                primary: TransactionError::Transport {
                    phase: TransactionPhase::Write,
                    error: primary_error,
                },
                restoration: RestorationOutcome {
                    write: Err(TransactionError::Transport {
                        phase: TransactionPhase::RestorationWrite,
                        error: TransportError::Disconnected,
                    }),
                    verification: Err(TransactionError::Transport {
                        phase: TransactionPhase::RestorationReadback,
                        error: TransportError::Disconnected,
                    }),
                },
            }
        );
        assert_eq!(session.write_state(), Wave3WriteState::NeedsReprobe);
        let event_count = transport.events.len();
        assert_eq!(
            session.transact_control(&mut transport, expected, gain_change()),
            TransactionOutcome::Rejected { error: TransactionError::NeedsReprobe }
        );
        assert_eq!(transport.events.len(), event_count);
    }
}
