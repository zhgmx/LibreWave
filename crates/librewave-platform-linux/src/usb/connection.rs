use super::{
    DescriptorError, InterfaceObservation, LocatedUsbDevice, UsbLocation, UsbProbeError,
    is_reviewed_wave3, select_control_interface, select_device_at_location, transport_probe_error,
};
use librewave_device::{
    CONTROL_TRANSFER_TIMEOUT, ControlWriteRequest, ControlWriteTransport, DeviceIdentity,
    ReadOnlyTransport, SetupPacket, TransportError, Wave3Session, Wave3WriteState, probe_wave3,
};
use librewave_protocol::{
    ApiVersion, CodecError, Wave3Config, Wave3ControlChange, Wave3ControlState,
};
use rusb::{Context, Device, DeviceHandle, UsbContext};
use std::time::Duration;

/// An exclusive Linux connection to one admitted Wave:3 control interface.
///
/// The connection owns one libusb handle and the session admitted through that
/// handle. It claims only the exact `0xff/0xf0/0x00` vendor interface. An active
/// kernel driver causes the connection to fail before the claim. Automatic
/// driver detachment is never enabled.
pub struct Wave3UsbConnection {
    inner: Connection<RusbControlHandle>,
}

impl Wave3UsbConnection {
    /// Opens the exact serial-free topology in `candidate` and admits its
    /// Wave:3 API and complete configuration through the claimed handle.
    ///
    /// # Errors
    ///
    /// Returns a classified topology, descriptor, claim, transport, admission,
    /// or cleanup failure.
    pub fn open(candidate: &super::super::UsbDeviceCandidate) -> Result<Self, UsbProbeError> {
        let backend = RusbBackend::new()?;
        open_with_backend(candidate, backend).map(|inner| Self { inner })
    }

    /// Returns the admitted serial-free device identity.
    #[must_use]
    pub const fn identity(&self) -> DeviceIdentity {
        self.inner.session.identity()
    }

    /// Returns the admitted API version.
    #[must_use]
    pub const fn api(&self) -> ApiVersion {
        self.inner.session.api()
    }

    /// Returns the current complete configuration baseline.
    #[must_use]
    pub const fn config(&self) -> &Wave3Config {
        self.inner.session.config()
    }

    /// Returns the typed hardware controls from the current baseline.
    ///
    /// # Errors
    ///
    /// Returns an error if the stored configuration cannot be decoded.
    pub fn controls(&self) -> Result<Wave3ControlState, CodecError> {
        self.inner.session.controls()
    }

    /// Returns whether the admitted session can start a control transaction.
    #[must_use]
    pub const fn write_state(&self) -> Wave3WriteState {
        self.inner.session.write_state()
    }

    /// Applies one reviewed change using the caller's exact baseline.
    ///
    /// The transaction uses this connection's admitted session, claimed
    /// interface, and libusb handle. It performs the portable preflight,
    /// complete-payload write, readback, and restoration rules.
    #[must_use]
    pub fn transact_control(
        &mut self,
        expected_baseline: Wave3Config,
        change: Wave3ControlChange,
    ) -> librewave_device::TransactionOutcome {
        self.inner.transact_control(expected_baseline, change)
    }

    /// Releases the vendor control interface and closes the libusb handle.
    ///
    /// Dropping the connection also makes one release attempt before libusb
    /// closes the handle. Use this method when the caller needs the release
    /// result.
    ///
    /// # Errors
    ///
    /// Returns the classified libusb release failure. The handle still closes
    /// after a failed release.
    pub fn close(self) -> Result<(), TransportError> {
        self.inner.close()
    }
}

struct Connection<H: ControlHandle> {
    transport: ClaimedTransport<H>,
    session: Wave3Session,
}

impl<H: ControlHandle> Connection<H> {
    fn transact_control(
        &mut self,
        expected_baseline: Wave3Config,
        change: Wave3ControlChange,
    ) -> librewave_device::TransactionOutcome {
        self.session.transact_control(&mut self.transport, expected_baseline, change)
    }

    fn close(self) -> Result<(), TransportError> {
        self.transport.close()
    }
}

trait UsbBackend {
    type Device: ConnectionDevice;

    fn devices(&mut self) -> Result<Vec<Self::Device>, UsbProbeError>;
}

trait ConnectionDevice: LocatedUsbDevice {
    type Handle: ControlHandle;

    fn identity(&self) -> Result<(u16, u16), UsbProbeError>;
    fn interfaces(&self) -> Result<Vec<InterfaceObservation>, UsbProbeError>;
    fn open(self) -> Result<Self::Handle, UsbProbeError>;
}

trait ControlHandle {
    fn kernel_driver_active(&mut self, interface: u8) -> Result<bool, TransportError>;
    fn claim_interface(&mut self, interface: u8) -> Result<(), TransportError>;
    fn release_interface(&mut self, interface: u8) -> Result<(), TransportError>;
    fn read_control(
        &mut self,
        setup: SetupPacket,
        response: &mut [u8],
        timeout: Duration,
    ) -> Result<usize, TransportError>;
    fn write_control(
        &mut self,
        setup: SetupPacket,
        payload: &[u8],
        timeout: Duration,
    ) -> Result<usize, TransportError>;
}

fn open_with_backend<B>(
    candidate: &super::super::UsbDeviceCandidate,
    mut backend: B,
) -> Result<Connection<<B::Device as ConnectionDevice>::Handle>, UsbProbeError>
where
    B: UsbBackend,
{
    if candidate.identity != DeviceIdentity::wave3() {
        return Err(UsbProbeError::NotFound);
    }
    let location =
        UsbLocation::parse(candidate.topology.as_str()).map_err(UsbProbeError::Topology)?;
    let device = select_device_at_location(backend.devices()?, &location)?;

    let (vendor_id, product_id) = device.identity()?;
    if !is_reviewed_wave3(vendor_id, product_id) {
        return Err(UsbProbeError::Descriptor(DescriptorError::IdentityMismatch {
            vendor_id,
            product_id,
        }));
    }
    let interface =
        select_control_interface(device.interfaces()?).map_err(UsbProbeError::Descriptor)?;
    let handle = device.open()?;
    let mut transport = ClaimedTransport::claim(handle, interface)?;
    let session = match probe_wave3(&mut transport, interface) {
        Ok(session) => session,
        Err(admission) => {
            return match transport.release_claim() {
                Ok(()) => Err(UsbProbeError::Session(admission)),
                Err(release) => Err(UsbProbeError::AdmissionCleanup { admission, release }),
            };
        }
    };
    Ok(Connection { transport, session })
}

struct ClaimedTransport<H: ControlHandle> {
    handle: H,
    claimed_interface: Option<u8>,
}

impl<H: ControlHandle> ClaimedTransport<H> {
    fn claim(mut handle: H, interface: u8) -> Result<Self, UsbProbeError> {
        if handle.kernel_driver_active(interface).map_err(session_transport_error)? {
            return Err(session_transport_error(TransportError::Busy));
        }
        handle.claim_interface(interface).map_err(session_transport_error)?;
        Ok(Self { handle, claimed_interface: Some(interface) })
    }

    fn release_claim(&mut self) -> Result<(), TransportError> {
        let Some(interface) = self.claimed_interface.take() else {
            return Ok(());
        };
        self.handle.release_interface(interface)
    }

    fn close(mut self) -> Result<(), TransportError> {
        self.release_claim()
    }
}

impl<H: ControlHandle> Drop for ClaimedTransport<H> {
    fn drop(&mut self) {
        let _ = self.release_claim();
    }
}

impl<H: ControlHandle> ReadOnlyTransport for ClaimedTransport<H> {
    fn read_control(
        &mut self,
        setup: SetupPacket,
        response: &mut [u8],
        timeout: Duration,
    ) -> Result<usize, TransportError> {
        if timeout != CONTROL_TRANSFER_TIMEOUT {
            return Err(TransportError::Io);
        }
        self.handle.read_control(setup, response, CONTROL_TRANSFER_TIMEOUT)
    }
}

impl<H: ControlHandle> ControlWriteTransport for ClaimedTransport<H> {
    fn write_control(
        &mut self,
        request: ControlWriteRequest<'_>,
        timeout: Duration,
    ) -> Result<usize, TransportError> {
        if timeout != CONTROL_TRANSFER_TIMEOUT {
            return Err(TransportError::Io);
        }
        self.handle.write_control(request.setup(), request.payload(), CONTROL_TRANSFER_TIMEOUT)
    }
}

fn session_transport_error(error: TransportError) -> UsbProbeError {
    UsbProbeError::Session(librewave_device::SessionError::Transport(error))
}

struct RusbBackend {
    context: Context,
}

impl RusbBackend {
    fn new() -> Result<Self, UsbProbeError> {
        Context::new().map(|context| Self { context }).map_err(transport_probe_error)
    }
}

impl UsbBackend for RusbBackend {
    type Device = Device<Context>;

    fn devices(&mut self) -> Result<Vec<Self::Device>, UsbProbeError> {
        self.context
            .devices()
            .map(|devices| devices.iter().collect())
            .map_err(transport_probe_error)
    }
}

impl ConnectionDevice for Device<Context> {
    type Handle = RusbControlHandle;

    fn identity(&self) -> Result<(u16, u16), UsbProbeError> {
        self.device_descriptor()
            .map(|descriptor| (descriptor.vendor_id(), descriptor.product_id()))
            .map_err(descriptor_error)
    }

    fn interfaces(&self) -> Result<Vec<InterfaceObservation>, UsbProbeError> {
        self.active_config_descriptor()
            .map(|configuration| {
                configuration
                    .interfaces()
                    .flat_map(|interface| {
                        interface.descriptors().map(|descriptor| InterfaceObservation {
                            number: descriptor.interface_number(),
                            class: descriptor.class_code(),
                            subclass: descriptor.sub_class_code(),
                            protocol: descriptor.protocol_code(),
                        })
                    })
                    .collect()
            })
            .map_err(descriptor_error)
    }

    fn open(self) -> Result<Self::Handle, UsbProbeError> {
        Device::open(&self)
            .map(|handle| RusbControlHandle { handle })
            .map_err(transport_probe_error)
    }
}

fn descriptor_error(error: rusb::Error) -> UsbProbeError {
    match error {
        rusb::Error::BadDescriptor => UsbProbeError::Descriptor(DescriptorError::Malformed),
        _ => transport_probe_error(error),
    }
}

struct RusbControlHandle {
    handle: DeviceHandle<Context>,
}

impl ControlHandle for RusbControlHandle {
    fn kernel_driver_active(&mut self, interface: u8) -> Result<bool, TransportError> {
        self.handle.kernel_driver_active(interface).map_err(super::map_rusb_error)
    }

    fn claim_interface(&mut self, interface: u8) -> Result<(), TransportError> {
        self.handle.claim_interface(interface).map_err(super::map_rusb_error)
    }

    fn release_interface(&mut self, interface: u8) -> Result<(), TransportError> {
        self.handle.release_interface(interface).map_err(super::map_rusb_error)
    }

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
            .map_err(super::map_rusb_error)
    }

    fn write_control(
        &mut self,
        setup: SetupPacket,
        payload: &[u8],
        timeout: Duration,
    ) -> Result<usize, TransportError> {
        self.handle
            .write_control(
                setup.request_type,
                setup.request,
                setup.value,
                setup.index,
                payload,
                timeout,
            )
            .map_err(super::map_rusb_error)
    }
}

#[cfg(test)]
mod tests;
