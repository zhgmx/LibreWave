//! Linux endpoint-zero transport for Wave:3 admission and control.

mod connection;

pub use connection::Wave3UsbConnection;

use librewave_core::{
    AdmissionError, DeviceAdmissionSnapshot, FixedPointValue, VolumeSelection, Wave3ConfigSnapshot,
};
use librewave_device::{
    ReadOnlyTransport, SessionError, SetupPacket, TransportError, Wave3Session, probe_wave3,
};
use librewave_protocol::{SemanticValue, Wave3ConfigField};
use rusb::{Context, Device, DeviceHandle, UsbContext};
use std::fmt;
use std::time::Duration;

const WAVE3_CONTROL_CLASS: u8 = 0xff;
const WAVE3_CONTROL_SUBCLASS: u8 = 0xf0;
const WAVE3_CONTROL_PROTOCOL: u8 = 0x00;
const DFU_CLASS: u8 = 0xfe;
const DFU_SUBCLASS: u8 = 0x01;

/// A USB descriptor shape that cannot select one safe control interface.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DescriptorError {
    Malformed,
    IdentityMismatch { vendor_id: u16, product_id: u16 },
    ControlInterfaceNotFound,
    MultipleControlInterfaces { first: u8, second: u8 },
}

impl fmt::Display for DescriptorError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Malformed => formatter.write_str("malformed USB configuration descriptor"),
            Self::IdentityMismatch { vendor_id, product_id } => write!(
                formatter,
                "USB device at the selected topology has identity {vendor_id:04x}:{product_id:04x}"
            ),
            Self::ControlInterfaceNotFound => {
                formatter.write_str("USB configuration has no reviewed Wave:3 control interface")
            }
            Self::MultipleControlInterfaces { first, second } => write!(
                formatter,
                "USB configuration has multiple reviewed Wave:3 control interfaces ({first} and {second})"
            ),
        }
    }
}

impl std::error::Error for DescriptorError {}

/// A sysfs USB topology name that cannot identify a libusb location.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TopologyError {
    InvalidFormat,
    InvalidBus,
    InvalidPortChain,
}

impl fmt::Display for TopologyError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::InvalidFormat => "USB topology must have the form BUS-PORT[.PORT]",
            Self::InvalidBus => "USB topology has an invalid bus number",
            Self::InvalidPortChain => "USB topology has an invalid port-number chain",
        })
    }
}

impl std::error::Error for TopologyError {}

/// A classified Linux USB discovery, descriptor, or session failure.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum UsbProbeError {
    NotFound,
    Topology(TopologyError),
    Descriptor(DescriptorError),
    Session(SessionError),
    AdmissionCleanup { admission: SessionError, release: TransportError },
}

impl fmt::Display for UsbProbeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotFound => formatter.write_str("selected Wave:3 USB device not found"),
            Self::Topology(error) => error.fmt(formatter),
            Self::Descriptor(error) => error.fmt(formatter),
            Self::Session(error) => error.fmt(formatter),
            Self::AdmissionCleanup { admission, release } => write!(
                formatter,
                "Wave:3 admission failed ({admission}) and releasing the control interface failed ({release})"
            ),
        }
    }
}

impl std::error::Error for UsbProbeError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::NotFound => None,
            Self::Topology(error) => Some(error),
            Self::Descriptor(error) => Some(error),
            Self::Session(error) => Some(error),
            Self::AdmissionCleanup { admission, .. } => Some(admission),
        }
    }
}

/// Opens the admitted Wave:3 candidate at its exact Linux USB topology and
/// performs a read-only admission probe.
///
/// The topology is the libusb bus number and port-number chain. The current USB
/// bus address is not part of the match. After the location matches, the probe
/// checks the normal-mode USB identity and the `0xff/0xf0/0x00` interface.
/// The selected `0xff/0xf0/0x00` interface must have no active kernel driver.
/// This function never claims an interface and never enables automatic driver
/// detachment. The USB handle and context close when the function returns.
///
/// # Errors
///
/// Returns a classified discovery, descriptor, transport, API, or schema
/// failure.
pub fn probe_wave3_usb(
    candidate: &super::UsbDeviceCandidate,
) -> Result<Wave3Session, UsbProbeError> {
    if candidate.identity != super::DeviceIdentity::wave3() {
        return Err(UsbProbeError::NotFound);
    }
    let location =
        UsbLocation::parse(candidate.topology.as_str()).map_err(UsbProbeError::Topology)?;
    let context = Context::new().map_err(transport_probe_error)?;
    let devices = context.devices().map_err(transport_probe_error)?;
    let device = select_device_at_location(devices.iter(), &location)?;

    let descriptor = device.device_descriptor().map_err(|error| match error {
        rusb::Error::BadDescriptor => UsbProbeError::Descriptor(DescriptorError::Malformed),
        _ => transport_probe_error(error),
    })?;
    if !is_reviewed_wave3(descriptor.vendor_id(), descriptor.product_id()) {
        return Err(UsbProbeError::Descriptor(DescriptorError::IdentityMismatch {
            vendor_id: descriptor.vendor_id(),
            product_id: descriptor.product_id(),
        }));
    }

    let configuration = device.active_config_descriptor().map_err(|error| match error {
        rusb::Error::BadDescriptor => UsbProbeError::Descriptor(DescriptorError::Malformed),
        _ => transport_probe_error(error),
    })?;
    let interfaces: Vec<_> = configuration
        .interfaces()
        .flat_map(|interface| {
            interface.descriptors().map(|descriptor| InterfaceObservation {
                number: descriptor.interface_number(),
                class: descriptor.class_code(),
                subclass: descriptor.sub_class_code(),
                protocol: descriptor.protocol_code(),
            })
        })
        .collect();
    let interface = select_control_interface(interfaces).map_err(UsbProbeError::Descriptor)?;

    let handle = device.open().map_err(transport_probe_error)?;
    if handle.kernel_driver_active(interface).map_err(transport_probe_error)? {
        return Err(UsbProbeError::Session(SessionError::Transport(TransportError::Busy)));
    }

    let mut transport = RusbReadOnlyTransport { handle };
    probe_wave3(&mut transport, interface).map_err(UsbProbeError::Session)
}

/// Runs the read-only probe for one current inventory candidate and converts
/// the result into the portable admission snapshot.
#[must_use]
pub fn inspect_wave3_usb(candidate: &super::UsbDeviceCandidate) -> DeviceAdmissionSnapshot {
    admission_snapshot(probe_wave3_usb(candidate))
}

/// Converts a probe result without exposing Linux topology or USB serial data.
#[must_use]
pub fn admission_snapshot(result: Result<Wave3Session, UsbProbeError>) -> DeviceAdmissionSnapshot {
    match result {
        Ok(session) => match config_snapshot(session.config()) {
            Ok(config) => DeviceAdmissionSnapshot::Admitted { api: session.api(), config },
            Err(()) => DeviceAdmissionSnapshot::Failed { error: AdmissionError::MalformedResponse },
        },
        Err(error) => DeviceAdmissionSnapshot::Failed { error: admission_error(error) },
    }
}

fn admission_error(error: UsbProbeError) -> AdmissionError {
    match error {
        UsbProbeError::NotFound
        | UsbProbeError::Session(SessionError::Transport(
            TransportError::NotFound | TransportError::Disconnected,
        )) => AdmissionError::Disconnected,
        UsbProbeError::Topology(_) | UsbProbeError::Descriptor(_) => {
            AdmissionError::DescriptorMismatch
        }
        UsbProbeError::Session(SessionError::Transport(TransportError::PermissionDenied)) => {
            AdmissionError::PermissionDenied
        }
        UsbProbeError::Session(SessionError::Transport(TransportError::Busy)) => {
            AdmissionError::InterfaceBusy
        }
        UsbProbeError::Session(SessionError::Transport(TransportError::TimedOut)) => {
            AdmissionError::TimedOut
        }
        UsbProbeError::Session(SessionError::Transport(TransportError::Io)) => {
            AdmissionError::Transport
        }
        UsbProbeError::Session(SessionError::UnsupportedApi { api }) => {
            AdmissionError::UnsupportedApi { api }
        }
        UsbProbeError::Session(
            SessionError::ShortTransfer { .. }
            | SessionError::LongTransfer { .. }
            | SessionError::Schema(_),
        ) => AdmissionError::MalformedResponse,
        UsbProbeError::AdmissionCleanup { .. } => AdmissionError::Transport,
    }
}

fn config_snapshot(config: &librewave_protocol::Wave3Config) -> Result<Wave3ConfigSnapshot, ()> {
    let controls = config.controls().map_err(|_| ())?;
    Ok(Wave3ConfigSnapshot {
        input_gain: FixedPointValue {
            raw: controls.microphone_gain.raw_q8_8(),
            fractional_bits: controls.microphone_gain.fractional_bits(),
        },
        input_mute: controls.microphone_mute,
        clipguard_enable: controls.clipguard_enabled,
        lowcut_enable: controls.lowcut_enabled,
        headphone_volume: FixedPointValue {
            raw: controls.headphone_volume.raw_q8_8(),
            fractional_bits: controls.headphone_volume.fractional_bits(),
        },
        headphone_mute: controls.headphone_mute,
        direct_monitor: FixedPointValue {
            raw: controls.direct_monitor.raw_q8_8(),
            fractional_bits: controls.direct_monitor.fractional_bits(),
        },
        volume_select: match controls.volume_select {
            librewave_protocol::VolumeSelect::Mic => VolumeSelection::Microphone,
            librewave_protocol::VolumeSelect::Headphone => VolumeSelection::Headphone,
            librewave_protocol::VolumeSelect::Mix => VolumeSelection::Mix,
        },
        all_leds_off: boolean(config, Wave3ConfigField::AllLedsOff)?,
        leds_flip: boolean(config, Wave3ConfigField::LedsFlip)?,
        gain_lock: controls.gain_lock,
    })
}

fn boolean(config: &librewave_protocol::Wave3Config, field: Wave3ConfigField) -> Result<bool, ()> {
    match config.get(field).map_err(|_| ())? {
        SemanticValue::Boolean(value) => Ok(value),
        _ => Err(()),
    }
}

struct RusbReadOnlyTransport {
    handle: DeviceHandle<Context>,
}

impl ReadOnlyTransport for RusbReadOnlyTransport {
    fn read_control(
        &mut self,
        setup: SetupPacket,
        response: &mut [u8],
        timeout: Duration,
    ) -> Result<usize, TransportError> {
        self.handle
            .read_control(
                setup.request_type,
                setup.request,
                setup.value,
                setup.index,
                response,
                timeout,
            )
            .map_err(map_rusb_error)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct InterfaceObservation {
    number: u8,
    class: u8,
    subclass: u8,
    protocol: u8,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct UsbLocation {
    bus: u8,
    ports: Vec<u8>,
}

impl UsbLocation {
    fn parse(topology: &str) -> Result<Self, TopologyError> {
        let (bus, ports) = topology.split_once('-').ok_or(TopologyError::InvalidFormat)?;
        let bus = parse_decimal_u8(bus).ok_or(TopologyError::InvalidBus)?;
        if bus == 0 {
            return Err(TopologyError::InvalidBus);
        }
        let ports: Vec<_> = ports
            .split('.')
            .map(|port| parse_decimal_u8(port).filter(|port| *port != 0))
            .collect::<Option<_>>()
            .ok_or(TopologyError::InvalidPortChain)?;
        if ports.is_empty() || ports.len() > 7 {
            return Err(TopologyError::InvalidPortChain);
        }
        Ok(Self { bus, ports })
    }
}

fn parse_decimal_u8(value: &str) -> Option<u8> {
    if value.is_empty() || !value.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    value.parse().ok()
}

trait LocatedUsbDevice {
    fn bus_number(&self) -> u8;
    fn port_numbers(&self) -> Option<Vec<u8>>;
}

impl<T: UsbContext> LocatedUsbDevice for Device<T> {
    fn bus_number(&self) -> u8 {
        Device::bus_number(self)
    }

    fn port_numbers(&self) -> Option<Vec<u8>> {
        Device::port_numbers(self).ok()
    }
}

fn select_device_at_location<D>(
    devices: impl IntoIterator<Item = D>,
    location: &UsbLocation,
) -> Result<D, UsbProbeError>
where
    D: LocatedUsbDevice,
{
    devices
        .into_iter()
        .find(|device| {
            device.bus_number() == location.bus
                && device.port_numbers().as_deref() == Some(location.ports.as_slice())
        })
        .ok_or(UsbProbeError::NotFound)
}

fn select_control_interface(
    interfaces: impl IntoIterator<Item = InterfaceObservation>,
) -> Result<u8, DescriptorError> {
    let mut selected = None;
    for interface in interfaces {
        if interface.class == DFU_CLASS && interface.subclass == DFU_SUBCLASS {
            continue;
        }
        if interface.class != WAVE3_CONTROL_CLASS
            || interface.subclass != WAVE3_CONTROL_SUBCLASS
            || interface.protocol != WAVE3_CONTROL_PROTOCOL
        {
            continue;
        }
        match selected {
            None => selected = Some(interface.number),
            Some(number) if number == interface.number => {}
            Some(first) => {
                return Err(DescriptorError::MultipleControlInterfaces {
                    first,
                    second: interface.number,
                });
            }
        }
    }
    selected.ok_or(DescriptorError::ControlInterfaceNotFound)
}

fn transport_probe_error(error: rusb::Error) -> UsbProbeError {
    UsbProbeError::Session(SessionError::Transport(map_rusb_error(error)))
}

const fn is_reviewed_wave3(vendor_id: u16, product_id: u16) -> bool {
    vendor_id == super::WAVE3_USB.vendor_id && product_id == super::WAVE3_USB.product_id
}

fn map_rusb_error(error: rusb::Error) -> TransportError {
    match error {
        rusb::Error::Access => TransportError::PermissionDenied,
        rusb::Error::Busy => TransportError::Busy,
        rusb::Error::NotFound => TransportError::NotFound,
        rusb::Error::NoDevice => TransportError::Disconnected,
        rusb::Error::Timeout => TransportError::TimedOut,
        rusb::Error::Io
        | rusb::Error::InvalidParam
        | rusb::Error::Overflow
        | rusb::Error::Pipe
        | rusb::Error::Interrupted
        | rusb::Error::NoMem
        | rusb::Error::NotSupported
        | rusb::Error::BadDescriptor
        | rusb::Error::Other => TransportError::Io,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use librewave_core::{AdmissionError, ControlAccessState, VolumeSelection};
    use librewave_protocol::ApiVersion;
    use std::collections::VecDeque;

    struct FakeTransport {
        reads: VecDeque<Vec<u8>>,
    }

    impl ReadOnlyTransport for FakeTransport {
        fn read_control(
            &mut self,
            _setup: SetupPacket,
            response: &mut [u8],
            _timeout: Duration,
        ) -> Result<usize, TransportError> {
            let data = self.reads.pop_front().expect("fake request");
            response.copy_from_slice(&data);
            Ok(data.len())
        }
    }

    #[derive(Clone, Debug, Eq, PartialEq)]
    struct FakeLocatedDevice {
        name: &'static str,
        usb: super::super::UsbIdentity,
        bus: u8,
        ports: Option<Vec<u8>>,
    }

    impl LocatedUsbDevice for FakeLocatedDevice {
        fn bus_number(&self) -> u8 {
            self.bus
        }

        fn port_numbers(&self) -> Option<Vec<u8>> {
            self.ports.clone()
        }
    }

    fn interface(number: u8, class: u8, subclass: u8, protocol: u8) -> InterfaceObservation {
        InterfaceObservation { number, class, subclass, protocol }
    }

    fn fake_wave3(name: &'static str, bus: u8, ports: &[u8]) -> FakeLocatedDevice {
        FakeLocatedDevice { name, usb: super::super::WAVE3_USB, bus, ports: Some(ports.to_vec()) }
    }

    #[test]
    fn selects_exact_reviewed_control_interface_without_assuming_its_number() {
        let interfaces = [
            interface(0, 0x01, 0x01, 0x00),
            interface(3, 0x01, 0x02, 0x00),
            interface(7, WAVE3_CONTROL_CLASS, WAVE3_CONTROL_SUBCLASS, WAVE3_CONTROL_PROTOCOL),
        ];

        assert_eq!(select_control_interface(interfaces), Ok(7));
    }

    #[test]
    fn matches_only_reviewed_normal_mode_usb_identity() {
        assert!(is_reviewed_wave3(0x0fd9, 0x0070));
        assert!(!is_reviewed_wave3(0x0fd9, 0x0071));
        assert!(!is_reviewed_wave3(0x152a, 0x0070));
    }

    #[test]
    fn parses_bus_and_port_chain_from_sysfs_topology() {
        assert_eq!(UsbLocation::parse("1-8.3"), Ok(UsbLocation { bus: 1, ports: vec![8, 3] }));
        assert_eq!(UsbLocation::parse("12-4"), Ok(UsbLocation { bus: 12, ports: vec![4] }));
    }

    #[test]
    fn classifies_malformed_topologies_before_usb_access() {
        assert_eq!(UsbLocation::parse("1"), Err(TopologyError::InvalidFormat));
        assert_eq!(UsbLocation::parse("usb1-8"), Err(TopologyError::InvalidBus));
        assert_eq!(UsbLocation::parse("1-8:1.0"), Err(TopologyError::InvalidPortChain));
        assert_eq!(UsbLocation::parse("1-0"), Err(TopologyError::InvalidPortChain));
    }

    #[test]
    fn selects_exact_location_when_two_identical_wave3_devices_exist() {
        let first = fake_wave3("first", 1, &[8, 2]);
        let selected = fake_wave3("selected", 1, &[8, 3]);
        let devices = [first, selected.clone()];

        assert_eq!(
            select_device_at_location(devices, &UsbLocation::parse("1-8.3").expect("topology")),
            Ok(selected)
        );
    }

    #[test]
    fn reports_not_found_when_exact_location_is_absent() {
        let devices = [
            fake_wave3("different port", 1, &[8, 2]),
            FakeLocatedDevice {
                name: "unreadable topology",
                usb: super::super::WAVE3_USB,
                bus: 1,
                ports: None,
            },
        ];

        assert_eq!(
            select_device_at_location(devices, &UsbLocation::parse("1-8.3").expect("topology")),
            Err(UsbProbeError::NotFound)
        );
    }

    #[test]
    fn rejects_dfu_class_interface_even_when_it_is_unclaimed() {
        assert_eq!(
            select_control_interface([interface(4, DFU_CLASS, DFU_SUBCLASS, 0x01)]),
            Err(DescriptorError::ControlInterfaceNotFound)
        );
        assert_eq!(
            select_control_interface([
                interface(3, WAVE3_CONTROL_CLASS, WAVE3_CONTROL_SUBCLASS, WAVE3_CONTROL_PROTOCOL),
                interface(4, DFU_CLASS, DFU_SUBCLASS, 0x01),
            ]),
            Ok(3)
        );
    }

    #[test]
    fn refuses_nearby_and_ambiguous_control_interface_shapes() {
        assert_eq!(
            select_control_interface([
                interface(2, WAVE3_CONTROL_CLASS, 0xef, WAVE3_CONTROL_PROTOCOL),
                interface(3, WAVE3_CONTROL_CLASS, WAVE3_CONTROL_SUBCLASS, 0x01),
            ]),
            Err(DescriptorError::ControlInterfaceNotFound)
        );
        assert_eq!(
            select_control_interface([
                interface(4, WAVE3_CONTROL_CLASS, WAVE3_CONTROL_SUBCLASS, WAVE3_CONTROL_PROTOCOL),
                interface(9, WAVE3_CONTROL_CLASS, WAVE3_CONTROL_SUBCLASS, WAVE3_CONTROL_PROTOCOL),
            ]),
            Err(DescriptorError::MultipleControlInterfaces { first: 4, second: 9 })
        );
    }

    #[test]
    fn maps_libusb_failures_to_stable_transport_classes() {
        assert_eq!(map_rusb_error(rusb::Error::Access), TransportError::PermissionDenied);
        assert_eq!(map_rusb_error(rusb::Error::Busy), TransportError::Busy);
        assert_eq!(map_rusb_error(rusb::Error::NotFound), TransportError::NotFound);
        assert_eq!(map_rusb_error(rusb::Error::NoDevice), TransportError::Disconnected);
        assert_eq!(map_rusb_error(rusb::Error::Timeout), TransportError::TimedOut);
        assert_eq!(map_rusb_error(rusb::Error::Pipe), TransportError::Io);
    }

    #[test]
    fn fake_transport_produces_a_serial_free_admitted_snapshot() {
        let mut payload = vec![0; 16];
        payload[12] = 1;
        let mut transport = FakeTransport { reads: VecDeque::from([vec![5, 4], payload]) };
        let session = probe_wave3(&mut transport, 3).expect("fake Wave:3 session");
        let admission = admission_snapshot(Ok(session));
        assert_eq!(admission.control_access(), ControlAccessState::ReadOnly);
        assert_eq!(admission.admitted_api(), Some(ApiVersion::new(5, 4)));
        assert_eq!(
            admission.config().expect("decoded config").volume_select,
            VolumeSelection::Microphone
        );
    }

    #[test]
    fn probe_failures_remain_explicit_in_the_portable_snapshot() {
        let cases = [
            (
                UsbProbeError::Session(SessionError::Transport(TransportError::PermissionDenied)),
                ControlAccessState::PermissionDenied,
                AdmissionError::PermissionDenied,
            ),
            (
                UsbProbeError::Session(SessionError::UnsupportedApi { api: ApiVersion::new(5, 2) }),
                ControlAccessState::ReadOnly,
                AdmissionError::UnsupportedApi { api: ApiVersion::new(5, 2) },
            ),
            (
                UsbProbeError::Session(SessionError::ShortTransfer { expected: 16, actual: 15 }),
                ControlAccessState::ReadOnly,
                AdmissionError::MalformedResponse,
            ),
            (
                UsbProbeError::Descriptor(DescriptorError::Malformed),
                ControlAccessState::Unavailable,
                AdmissionError::DescriptorMismatch,
            ),
            (
                UsbProbeError::Session(SessionError::Transport(TransportError::Disconnected)),
                ControlAccessState::Unavailable,
                AdmissionError::Disconnected,
            ),
        ];
        for (error, access, expected) in cases {
            let admission = admission_snapshot(Err(error));
            assert_eq!(admission.control_access(), access);
            assert_eq!(admission.error(), Some(&expected));
        }
    }
}
