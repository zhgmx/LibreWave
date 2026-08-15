//! `PipeWire` core ownership and physical-object inspection.

use super::{EndpointPlan, LinuxAudioError, PipeWireFacade};
use crate::{UsbDeviceCandidate, WAVE3_USB};
use pipewire as pw;
use pw::types::ObjectType;
use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::time::{Duration, Instant};

const PIPEWIRE_ROUNDTRIP_TIMEOUT: Duration = Duration::from_secs(2);
const PIPEWIRE_ITERATION: Duration = Duration::from_millis(50);
const API_ALSA_CARD: &str = "api.alsa.card";
const API_ALSA_PATH: &str = "api.alsa.path";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum PhysicalObjectKind {
    Device,
    Node,
    Other,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(super) struct PhysicalObjectProperties<'a> {
    pub(super) vendor_id: Option<&'a str>,
    pub(super) product_id: Option<&'a str>,
    pub(super) alsa_card: Option<&'a str>,
    pub(super) alsa_path: Option<&'a str>,
}

pub(super) fn is_candidate_physical_object(
    kind: PhysicalObjectKind,
    properties: PhysicalObjectProperties<'_>,
    expected_card: u32,
) -> bool {
    match kind {
        PhysicalObjectKind::Device => {
            properties.vendor_id == Some("0x0fd9")
                && properties.product_id == Some("0x0070")
                && properties.alsa_card.and_then(parse_canonical_number) == Some(expected_card)
        }
        PhysicalObjectKind::Node => {
            properties.alsa_path.and_then(parse_hw_card) == Some(expected_card)
        }
        PhysicalObjectKind::Other => false,
    }
}

fn parse_hw_card(path: &str) -> Option<u32> {
    let mut components = path.strip_prefix("hw:")?.split(',');
    let card = parse_canonical_number(components.next()?)?;
    for _ in 0..2 {
        let Some(component) = components.next() else {
            return Some(card);
        };
        parse_canonical_number(component)?;
    }
    components.next().is_none().then_some(card)
}

fn parse_canonical_number(value: &str) -> Option<u32> {
    if value.is_empty()
        || !value.bytes().all(|byte| byte.is_ascii_digit())
        || (value.len() > 1 && value.starts_with('0'))
    {
        return None;
    }
    value.parse().ok()
}

#[derive(Debug, Default)]
pub(super) struct RealPipeWireFacade {
    client: Option<PipeWireClient>,
}

impl PipeWireFacade for RealPipeWireFacade {
    fn verify_physical_nodes_hidden(
        &mut self,
        candidate: &UsbDeviceCandidate,
    ) -> Result<(), LinuxAudioError> {
        if candidate.identity.usb() != WAVE3_USB {
            return Err(LinuxAudioError::AlsaCardChanged);
        }
        let [card] = candidate.alsa_cards.as_slice() else {
            return Err(LinuxAudioError::AmbiguousAlsaCard);
        };
        if self.client.is_none() {
            self.client = Some(PipeWireClient::connect()?);
        }
        let exposed = self
            .client
            .as_ref()
            .ok_or_else(|| LinuxAudioError::PipeWire("connection disappeared".to_owned()))?
            .physical_object_visible(card.number)?;
        if exposed { Err(LinuxAudioError::PhysicalNodeExposed) } else { Ok(()) }
    }

    fn publish_endpoints(&mut self, plans: &[EndpointPlan]) -> Result<(), LinuxAudioError> {
        if plans.is_empty() {
            return Ok(());
        }
        Err(LinuxAudioError::EndpointStreamTransportUnavailable)
    }

    fn disconnect(&mut self) -> Result<(), LinuxAudioError> {
        self.client = None;
        Ok(())
    }

    fn owns_connection(&self) -> bool {
        self.client.is_some()
    }
}

#[derive(Debug)]
struct PipeWireClient {
    registry: pw::registry::RegistryRc,
    core: pw::core::CoreRc,
    _context: pw::context::ContextRc,
    main_loop: pw::main_loop::MainLoopRc,
}

impl PipeWireClient {
    fn connect() -> Result<Self, LinuxAudioError> {
        pw::init();
        let main_loop = pw::main_loop::MainLoopRc::new(None)
            .map_err(|error| LinuxAudioError::PipeWire(error.to_string()))?;
        let context = pw::context::ContextRc::new(&main_loop, None)
            .map_err(|error| LinuxAudioError::PipeWire(error.to_string()))?;
        let core = context
            .connect_rc(None)
            .map_err(|error| LinuxAudioError::PipeWire(error.to_string()))?;
        let registry =
            core.get_registry_rc().map_err(|error| LinuxAudioError::PipeWire(error.to_string()))?;
        Ok(Self { registry, core, _context: context, main_loop })
    }

    fn physical_object_visible(&self, expected_card: u32) -> Result<bool, LinuxAudioError> {
        let exposed = Rc::new(Cell::new(false));
        let exposed_callback = Rc::clone(&exposed);
        let registry_listener = self
            .registry
            .add_listener_local()
            .global(move |global| {
                let Some(properties) = global.props else {
                    return;
                };
                let kind = match global.type_ {
                    ObjectType::Device => PhysicalObjectKind::Device,
                    ObjectType::Node => PhysicalObjectKind::Node,
                    _ => PhysicalObjectKind::Other,
                };
                let observed = PhysicalObjectProperties {
                    vendor_id: properties.get(*pw::keys::DEVICE_VENDOR_ID),
                    product_id: properties.get(*pw::keys::DEVICE_PRODUCT_ID),
                    alsa_card: properties.get(API_ALSA_CARD),
                    alsa_path: properties.get(API_ALSA_PATH),
                };
                if is_candidate_physical_object(kind, observed, expected_card) {
                    exposed_callback.set(true);
                }
            })
            .register();

        let done = Rc::new(Cell::new(false));
        let done_callback = Rc::clone(&done);
        let fatal_error = Rc::new(RefCell::new(None));
        let error_callback = Rc::clone(&fatal_error);
        let pending =
            self.core.sync(0).map_err(|error| LinuxAudioError::PipeWire(error.to_string()))?;
        let core_listener = self
            .core
            .add_listener_local()
            .done(move |id, sequence| {
                if id == pw::core::PW_ID_CORE && sequence == pending {
                    done_callback.set(true);
                }
            })
            .error(move |_id, _sequence, result, message| {
                *error_callback.borrow_mut() = Some(format!("{message} ({result})"));
            })
            .register();

        let deadline = Instant::now() + PIPEWIRE_ROUNDTRIP_TIMEOUT;
        while !done.get() && fatal_error.borrow().is_none() && Instant::now() < deadline {
            self.main_loop.loop_().iterate(PIPEWIRE_ITERATION);
        }
        drop(core_listener);
        drop(registry_listener);
        if let Some(error) = fatal_error.take() {
            return Err(LinuxAudioError::PipeWire(error));
        }
        if !done.get() {
            return Err(LinuxAudioError::PipeWire("registry roundtrip timed out".to_owned()));
        }
        Ok(exposed.get())
    }
}
