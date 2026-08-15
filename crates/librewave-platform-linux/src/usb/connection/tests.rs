use super::*;
use crate::{UsbDeviceCandidate, UsbTopology};
use librewave_device::{
    RestorationOutcome, TransactionError, TransactionOutcome, TransactionPhase,
};
use librewave_protocol::{VolumeSelect, Wave3GainDb};
use std::cell::RefCell;
use std::collections::VecDeque;
use std::rc::Rc;

#[derive(Clone, Debug, Eq, PartialEq)]
enum Event {
    Enumerate,
    Identity { device: &'static str },
    Interfaces { device: &'static str },
    Open { device: &'static str, handle: u8 },
    KernelDriverActive { handle: u8, interface: u8 },
    Claim { handle: u8, interface: u8 },
    Read { handle: u8, setup: SetupPacket, length: usize, timeout: Duration },
    Write { handle: u8, setup: SetupPacket, payload: Vec<u8>, timeout: Duration },
    Release { handle: u8, interface: u8 },
    Close { handle: u8 },
}

#[derive(Debug)]
struct ReadResult {
    payload: Vec<u8>,
    completed: usize,
}

struct HandlePlan {
    id: u8,
    kernel_active: Result<bool, TransportError>,
    claim: Result<(), TransportError>,
    releases: VecDeque<Result<(), TransportError>>,
    reads: VecDeque<Result<ReadResult, TransportError>>,
    writes: VecDeque<Result<usize, TransportError>>,
}

impl HandlePlan {
    fn admitted(id: u8, api: ApiVersion, baseline: [u8; 16]) -> Self {
        Self {
            id,
            kernel_active: Ok(false),
            claim: Ok(()),
            releases: VecDeque::from([Ok(())]),
            reads: VecDeque::from([
                Ok(ReadResult { payload: vec![api.major, api.minor], completed: 2 }),
                Ok(ReadResult { payload: baseline.to_vec(), completed: 16 }),
            ]),
            writes: VecDeque::new(),
        }
    }
}

struct FakeBackend {
    devices: Vec<FakeDevice>,
    events: Rc<RefCell<Vec<Event>>>,
}

impl UsbBackend for FakeBackend {
    type Device = FakeDevice;

    fn devices(&mut self) -> Result<Vec<Self::Device>, UsbProbeError> {
        self.events.borrow_mut().push(Event::Enumerate);
        Ok(std::mem::take(&mut self.devices))
    }
}

struct FakeDevice {
    name: &'static str,
    bus: u8,
    ports: Option<Vec<u8>>,
    identity: (u16, u16),
    interfaces: Vec<InterfaceObservation>,
    plan: Rc<RefCell<HandlePlan>>,
    events: Rc<RefCell<Vec<Event>>>,
}

impl LocatedUsbDevice for FakeDevice {
    fn bus_number(&self) -> u8 {
        self.bus
    }

    fn port_numbers(&self) -> Option<Vec<u8>> {
        self.ports.clone()
    }
}

impl ConnectionDevice for FakeDevice {
    type Handle = FakeHandle;

    fn identity(&self) -> Result<(u16, u16), UsbProbeError> {
        self.events.borrow_mut().push(Event::Identity { device: self.name });
        Ok(self.identity)
    }

    fn interfaces(&self) -> Result<Vec<InterfaceObservation>, UsbProbeError> {
        self.events.borrow_mut().push(Event::Interfaces { device: self.name });
        Ok(self.interfaces.clone())
    }

    fn open(self) -> Result<Self::Handle, UsbProbeError> {
        let handle = self.plan.borrow().id;
        self.events.borrow_mut().push(Event::Open { device: self.name, handle });
        Ok(FakeHandle { plan: self.plan, events: self.events })
    }
}

struct FakeHandle {
    plan: Rc<RefCell<HandlePlan>>,
    events: Rc<RefCell<Vec<Event>>>,
}

impl ControlHandle for FakeHandle {
    fn kernel_driver_active(&mut self, interface: u8) -> Result<bool, TransportError> {
        let plan = self.plan.borrow();
        self.events.borrow_mut().push(Event::KernelDriverActive { handle: plan.id, interface });
        plan.kernel_active
    }

    fn claim_interface(&mut self, interface: u8) -> Result<(), TransportError> {
        let plan = self.plan.borrow();
        self.events.borrow_mut().push(Event::Claim { handle: plan.id, interface });
        plan.claim
    }

    fn release_interface(&mut self, interface: u8) -> Result<(), TransportError> {
        let mut plan = self.plan.borrow_mut();
        self.events.borrow_mut().push(Event::Release { handle: plan.id, interface });
        plan.releases.pop_front().expect("unexpected fake release")
    }

    fn read_control(
        &mut self,
        setup: SetupPacket,
        response: &mut [u8],
        timeout: Duration,
    ) -> Result<usize, TransportError> {
        let mut plan = self.plan.borrow_mut();
        self.events.borrow_mut().push(Event::Read {
            handle: plan.id,
            setup,
            length: response.len(),
            timeout,
        });
        match plan.reads.pop_front().expect("unexpected fake read") {
            Ok(result) => {
                let copied = response.len().min(result.payload.len());
                response[..copied].copy_from_slice(&result.payload[..copied]);
                Ok(result.completed)
            }
            Err(error) => Err(error),
        }
    }

    fn write_control(
        &mut self,
        setup: SetupPacket,
        payload: &[u8],
        timeout: Duration,
    ) -> Result<usize, TransportError> {
        let mut plan = self.plan.borrow_mut();
        self.events.borrow_mut().push(Event::Write {
            handle: plan.id,
            setup,
            payload: payload.to_vec(),
            timeout,
        });
        plan.writes.pop_front().expect("unexpected fake write")
    }
}

impl Drop for FakeHandle {
    fn drop(&mut self) {
        let handle = self.plan.borrow().id;
        self.events.borrow_mut().push(Event::Close { handle });
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

fn changed_gain(mut payload: [u8; 16]) -> [u8; 16] {
    payload[0..2].copy_from_slice(&1_024i16.to_le_bytes());
    payload
}

fn gain_change() -> Wave3ControlChange {
    Wave3ControlChange::MicrophoneGain(Wave3GainDb::from_raw_q8_8(1_024).expect("valid test gain"))
}

fn candidate() -> UsbDeviceCandidate {
    UsbDeviceCandidate {
        identity: DeviceIdentity::wave3(),
        topology: UsbTopology::new("1-8.3"),
        alsa_cards: Vec::new(),
    }
}

fn reviewed_interfaces() -> Vec<InterfaceObservation> {
    vec![
        InterfaceObservation { number: 0, class: 0x01, subclass: 0x01, protocol: 0x00 },
        InterfaceObservation { number: 3, class: 0x01, subclass: 0x02, protocol: 0x00 },
        InterfaceObservation {
            number: 7,
            class: super::super::WAVE3_CONTROL_CLASS,
            subclass: super::super::WAVE3_CONTROL_SUBCLASS,
            protocol: super::super::WAVE3_CONTROL_PROTOCOL,
        },
    ]
}

fn device(
    name: &'static str,
    bus: u8,
    ports: &[u8],
    plan: HandlePlan,
    events: &Rc<RefCell<Vec<Event>>>,
) -> FakeDevice {
    FakeDevice {
        name,
        bus,
        ports: Some(ports.to_vec()),
        identity: (0x0fd9, 0x0070),
        interfaces: reviewed_interfaces(),
        plan: Rc::new(RefCell::new(plan)),
        events: Rc::clone(events),
    }
}

fn backend(devices: Vec<FakeDevice>, events: &Rc<RefCell<Vec<Event>>>) -> FakeBackend {
    FakeBackend { devices, events: Rc::clone(events) }
}

#[test]
fn selects_revalidates_and_claims_only_the_vendor_interface_in_order() {
    let events = Rc::new(RefCell::new(Vec::new()));
    let other = device(
        "other topology",
        1,
        &[8, 2],
        HandlePlan::admitted(11, ApiVersion::new(5, 4), baseline()),
        &events,
    );
    let selected = device(
        "selected",
        1,
        &[8, 3],
        HandlePlan::admitted(22, ApiVersion::new(5, 4), baseline()),
        &events,
    );

    let connection = open_with_backend(&candidate(), backend(vec![other, selected], &events))
        .expect("open selected Wave:3");

    assert_eq!(connection.session.api(), ApiVersion::new(5, 4));
    assert_eq!(connection.session.identity(), DeviceIdentity::wave3());
    let recorded = events.borrow();
    assert_eq!(
        &recorded[..6],
        [
            Event::Enumerate,
            Event::Identity { device: "selected" },
            Event::Interfaces { device: "selected" },
            Event::Open { device: "selected", handle: 22 },
            Event::KernelDriverActive { handle: 22, interface: 7 },
            Event::Claim { handle: 22, interface: 7 },
        ]
    );
    assert_eq!(recorded.iter().filter(|event| matches!(event, Event::Claim { .. })).count(), 1);
    drop(recorded);
    connection.close().expect("release selected interface");
}

#[test]
fn rejects_missing_topology_and_descriptor_mismatches_before_open() {
    let events = Rc::new(RefCell::new(Vec::new()));
    let mut wrong_candidate = candidate();
    wrong_candidate.topology = UsbTopology::new("2-4");
    assert!(matches!(
        open_with_backend(&wrong_candidate, backend(Vec::new(), &events)),
        Err(UsbProbeError::NotFound)
    ));
    assert_eq!(*events.borrow(), [Event::Enumerate]);

    events.borrow_mut().clear();
    let mut wrong_identity = device(
        "wrong identity",
        1,
        &[8, 3],
        HandlePlan::admitted(30, ApiVersion::new(5, 4), baseline()),
        &events,
    );
    wrong_identity.identity = (0x0fd9, 0x0071);
    assert!(matches!(
        open_with_backend(&candidate(), backend(vec![wrong_identity], &events)),
        Err(UsbProbeError::Descriptor(DescriptorError::IdentityMismatch {
            vendor_id: 0x0fd9,
            product_id: 0x0071,
        }))
    ));
    assert_eq!(*events.borrow(), [Event::Enumerate, Event::Identity { device: "wrong identity" }]);

    events.borrow_mut().clear();
    let mut ambiguous = device(
        "ambiguous",
        1,
        &[8, 3],
        HandlePlan::admitted(31, ApiVersion::new(5, 4), baseline()),
        &events,
    );
    ambiguous.interfaces.push(InterfaceObservation {
        number: 9,
        class: super::super::WAVE3_CONTROL_CLASS,
        subclass: super::super::WAVE3_CONTROL_SUBCLASS,
        protocol: super::super::WAVE3_CONTROL_PROTOCOL,
    });
    assert!(matches!(
        open_with_backend(&candidate(), backend(vec![ambiguous], &events)),
        Err(UsbProbeError::Descriptor(DescriptorError::MultipleControlInterfaces {
            first: 7,
            second: 9,
        }))
    ));
    assert_eq!(
        *events.borrow(),
        [
            Event::Enumerate,
            Event::Identity { device: "ambiguous" },
            Event::Interfaces { device: "ambiguous" },
        ]
    );
}

#[test]
fn refuses_active_kernel_driver_and_claim_failures_without_detaching() {
    for (kernel_active, claim, expected) in [
        (Ok(true), Ok(()), TransportError::Busy),
        (Ok(false), Err(TransportError::PermissionDenied), TransportError::PermissionDenied),
    ] {
        let events = Rc::new(RefCell::new(Vec::new()));
        let mut plan = HandlePlan::admitted(32, ApiVersion::new(5, 4), baseline());
        plan.kernel_active = kernel_active;
        plan.claim = claim;
        let selected = device("selected", 1, &[8, 3], plan, &events);

        assert_eq!(
            open_with_backend(&candidate(), backend(vec![selected], &events)).map(|_| ()),
            Err(UsbProbeError::Session(librewave_device::SessionError::Transport(expected)))
        );
        let recorded = events.borrow();
        assert!(!recorded.iter().any(|event| matches!(event, Event::Release { .. })));
        assert_eq!(recorded.last(), Some(&Event::Close { handle: 32 }));
    }
}

#[test]
fn admission_failure_releases_then_closes_and_reports_cleanup_failure() {
    let events = Rc::new(RefCell::new(Vec::new()));
    let unsupported = device(
        "unsupported",
        1,
        &[8, 3],
        HandlePlan::admitted(40, ApiVersion::new(5, 2), baseline()),
        &events,
    );
    assert!(matches!(
        open_with_backend(&candidate(), backend(vec![unsupported], &events)),
        Err(UsbProbeError::Session(librewave_device::SessionError::UnsupportedApi {
            api: ApiVersion { major: 5, minor: 2 },
        }))
    ));
    let recorded = events.borrow();
    assert_eq!(
        &recorded[recorded.len() - 2..],
        [Event::Release { handle: 40, interface: 7 }, Event::Close { handle: 40 }]
    );
    drop(recorded);

    events.borrow_mut().clear();
    let mut short_config = HandlePlan::admitted(41, ApiVersion::new(5, 4), baseline());
    short_config.reads[1] = Ok(ReadResult { payload: baseline().to_vec(), completed: 15 });
    let selected = device("short config", 1, &[8, 3], short_config, &events);
    assert_eq!(
        open_with_backend(&candidate(), backend(vec![selected], &events)).map(|_| ()),
        Err(UsbProbeError::Session(librewave_device::SessionError::ShortTransfer {
            expected: 16,
            actual: 15,
        }))
    );
    let recorded = events.borrow();
    assert_eq!(
        &recorded[recorded.len() - 2..],
        [Event::Release { handle: 41, interface: 7 }, Event::Close { handle: 41 }]
    );
    drop(recorded);

    events.borrow_mut().clear();
    let mut failed_release = HandlePlan::admitted(42, ApiVersion::new(5, 2), baseline());
    failed_release.releases = VecDeque::from([Err(TransportError::Disconnected)]);
    let selected = device("cleanup failure", 1, &[8, 3], failed_release, &events);
    assert!(matches!(
        open_with_backend(&candidate(), backend(vec![selected], &events)),
        Err(UsbProbeError::AdmissionCleanup {
            admission: librewave_device::SessionError::UnsupportedApi { .. },
            release: TransportError::Disconnected,
        })
    ));
    let recorded = events.borrow();
    assert_eq!(
        &recorded[recorded.len() - 2..],
        [Event::Release { handle: 42, interface: 7 }, Event::Close { handle: 42 }]
    );
}

#[test]
fn admission_and_restored_transaction_use_the_same_handle() {
    let original = baseline();
    let requested = changed_gain(original);
    let mut mismatched = requested;
    mismatched[15] = 1;
    let events = Rc::new(RefCell::new(Vec::new()));
    let mut plan = HandlePlan::admitted(50, ApiVersion::new(5, 3), original);
    plan.reads.extend([
        Ok(ReadResult { payload: original.to_vec(), completed: 16 }),
        Ok(ReadResult { payload: mismatched.to_vec(), completed: 16 }),
        Ok(ReadResult { payload: original.to_vec(), completed: 16 }),
    ]);
    plan.writes.extend([Ok(16), Ok(16)]);
    let selected = device("selected", 1, &[8, 3], plan, &events);
    let mut connection = open_with_backend(&candidate(), backend(vec![selected], &events))
        .expect("admit connection");
    let expected = *connection.session.config();

    assert!(matches!(
        connection.transact_control(expected, gain_change()),
        TransactionOutcome::Failed {
            primary: TransactionError::ReadbackMismatch {
                phase: TransactionPhase::Readback,
                ..
            },
            restoration: RestorationOutcome { write: Ok(()), verification: Ok(config) },
        } if config == expected
    ));
    assert_eq!(connection.session.config(), &expected);

    let recorded = events.borrow();
    let transfers: Vec<_> = recorded
        .iter()
        .filter(|event| matches!(event, Event::Read { .. } | Event::Write { .. }))
        .collect();
    assert_eq!(transfers.len(), 7);
    assert!(transfers.iter().all(|event| match event {
        Event::Read { handle, timeout, .. } | Event::Write { handle, timeout, .. } => {
            *handle == 50 && *timeout == CONTROL_TRANSFER_TIMEOUT
        }
        _ => false,
    }));
    let writes: Vec<_> = transfers
        .iter()
        .filter_map(|event| match event {
            Event::Write { payload, .. } => Some(payload.as_slice()),
            _ => None,
        })
        .collect();
    assert_eq!(writes, [requested.as_slice(), original.as_slice()]);
    drop(recorded);
    connection.close().expect("close connection");
}

#[test]
fn platform_transport_preserves_short_counts_and_classified_errors() {
    let original = baseline();
    for primary in [Ok(15), Err(TransportError::TimedOut)] {
        let events = Rc::new(RefCell::new(Vec::new()));
        let mut plan = HandlePlan::admitted(60, ApiVersion::new(5, 4), original);
        plan.reads.extend([
            Ok(ReadResult { payload: original.to_vec(), completed: 16 }),
            Ok(ReadResult { payload: original.to_vec(), completed: 16 }),
        ]);
        plan.writes.extend([primary, Ok(16)]);
        let selected = device("selected", 1, &[8, 3], plan, &events);
        let mut connection = open_with_backend(&candidate(), backend(vec![selected], &events))
            .expect("admit connection");
        let expected = *connection.session.config();

        let outcome = connection.transact_control(expected, gain_change());
        match primary {
            Ok(actual) => assert!(matches!(
                outcome,
                TransactionOutcome::Failed {
                    primary: TransactionError::ShortTransfer {
                        phase: TransactionPhase::Write,
                        expected: 16,
                        actual: count,
                    },
                    restoration: RestorationOutcome {
                        write: Ok(()),
                        verification: Ok(config),
                    },
                } if count == actual && config == expected
            )),
            Err(error) => assert!(matches!(
                outcome,
                TransactionOutcome::Failed {
                    primary: TransactionError::Transport {
                        phase: TransactionPhase::Write,
                        error: actual,
                    },
                    restoration: RestorationOutcome {
                        write: Ok(()),
                        verification: Ok(config),
                    },
                } if actual == error && config == expected
            )),
        }
        connection.close().expect("close connection");
    }
}

#[test]
fn explicit_close_and_drop_release_once_before_handle_close() {
    for explicit in [true, false] {
        let events = Rc::new(RefCell::new(Vec::new()));
        let selected = device(
            "selected",
            1,
            &[8, 3],
            HandlePlan::admitted(70, ApiVersion::new(5, 4), baseline()),
            &events,
        );
        let connection = open_with_backend(&candidate(), backend(vec![selected], &events))
            .expect("admit connection");
        if explicit {
            connection.close().expect("explicit close");
        } else {
            drop(connection);
        }
        let recorded = events.borrow();
        assert_eq!(
            &recorded[recorded.len() - 2..],
            [Event::Release { handle: 70, interface: 7 }, Event::Close { handle: 70 }]
        );
        assert_eq!(
            recorded.iter().filter(|event| matches!(event, Event::Release { .. })).count(),
            1
        );
    }
}

#[test]
fn failed_explicit_release_still_closes_without_retry() {
    let events = Rc::new(RefCell::new(Vec::new()));
    let mut plan = HandlePlan::admitted(71, ApiVersion::new(5, 4), baseline());
    plan.releases = VecDeque::from([Err(TransportError::Disconnected)]);
    let selected = device("selected", 1, &[8, 3], plan, &events);
    let connection = open_with_backend(&candidate(), backend(vec![selected], &events))
        .expect("admit connection");

    assert_eq!(connection.close(), Err(TransportError::Disconnected));
    let recorded = events.borrow();
    assert_eq!(
        &recorded[recorded.len() - 2..],
        [Event::Release { handle: 71, interface: 7 }, Event::Close { handle: 71 }]
    );
    assert_eq!(recorded.iter().filter(|event| matches!(event, Event::Release { .. })).count(), 1);
}

#[test]
fn reviewed_api_versions_are_admitted_through_the_claimed_handle() {
    for api in [ApiVersion::new(5, 3), ApiVersion::new(5, 4)] {
        let events = Rc::new(RefCell::new(Vec::new()));
        let selected =
            device("selected", 1, &[8, 3], HandlePlan::admitted(80, api, baseline()), &events);
        let connection = open_with_backend(&candidate(), backend(vec![selected], &events))
            .expect("admit reviewed API");
        assert_eq!(connection.session.api(), api);
        assert_eq!(connection.session.config().as_bytes(), &baseline());
        connection.close().expect("close connection");
    }
}
