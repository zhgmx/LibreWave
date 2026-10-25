//! `PipeWire` core ownership and physical-object inspection.

use super::{LinuxAudioError, PipeWireFacade};
use crate::{UsbDeviceCandidate, WAVE3_USB};
use pipewire as pw;
use pw::loop_::Timeout;
use pw::types::ObjectType;
use std::cell::{Cell, RefCell};
use std::collections::HashMap;
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

#[derive(Clone, Debug)]
struct PhysicalObjectSnapshot {
    kind: PhysicalObjectKind,
    vendor_id: Option<String>,
    product_id: Option<String>,
    alsa_card: Option<String>,
    alsa_path: Option<String>,
}

impl PhysicalObjectSnapshot {
    fn from_properties(
        kind: PhysicalObjectKind,
        properties: &pw::spa::utils::dict::DictRef,
    ) -> Self {
        Self {
            kind,
            vendor_id: properties.get(*pw::keys::DEVICE_VENDOR_ID).map(str::to_owned),
            product_id: properties.get(*pw::keys::DEVICE_PRODUCT_ID).map(str::to_owned),
            alsa_card: properties.get(API_ALSA_CARD).map(str::to_owned),
            alsa_path: properties.get(API_ALSA_PATH).map(str::to_owned),
        }
    }

    fn matches(&self, expected_card: u32) -> bool {
        is_candidate_physical_object(
            self.kind,
            PhysicalObjectProperties {
                vendor_id: self.vendor_id.as_deref(),
                product_id: self.product_id.as_deref(),
                alsa_card: self.alsa_card.as_deref(),
                alsa_path: self.alsa_path.as_deref(),
            },
            expected_card,
        )
    }
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
        // Each inspection binds and drops transient Node and Device proxies.
        // Their destroy replies can arrive after the inspection roundtrip.
        // Disconnect before the next inspection so an old proxy error cannot
        // be mistaken for a failure of the current physical ownership check.
        self.client = None;
        self.client = Some(PipeWireClient::connect()?);
        let exposed = self
            .client
            .as_ref()
            .ok_or_else(|| LinuxAudioError::PipeWire("connection disappeared".to_owned()))?
            .physical_object_visible(card.number)?;
        if exposed { Err(LinuxAudioError::PhysicalNodeExposed) } else { Ok(()) }
    }

    fn disconnect(&mut self) -> Result<(), LinuxAudioError> {
        self.client = None;
        Ok(())
    }

    fn owns_connection(&self) -> bool {
        self.client.is_some()
    }
}

struct PipeWireClient {
    _registry_listener: pw::registry::Listener,
    registry: pw::registry::RegistryRc,
    objects: Rc<RefCell<HashMap<u32, pw::registry::GlobalObject<pw::properties::PropertiesBox>>>>,
    core: pw::core::CoreRc,
    _context: pw::context::ContextRc,
    main_loop: pw::main_loop::MainLoopRc,
}

impl std::fmt::Debug for PipeWireClient {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.debug_struct("PipeWireClient").finish_non_exhaustive()
    }
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
        // Registry globals are delivered once when the registry is created.
        // Keep this listener for the entire connection so later checks also
        // see objects that existed before the check began.
        let objects = Rc::new(RefCell::new(HashMap::new()));
        let observed = Rc::clone(&objects);
        let removed = Rc::clone(&objects);
        let registry_listener = registry
            .add_listener_local()
            .global(move |global| {
                if matches!(global.type_, ObjectType::Device | ObjectType::Node) {
                    observed.borrow_mut().insert(global.id, global.to_owned());
                }
            })
            .global_remove(move |id| {
                removed.borrow_mut().remove(&id);
            })
            .register();
        Ok(Self {
            _registry_listener: registry_listener,
            registry,
            objects,
            core,
            _context: context,
            main_loop,
        })
    }

    fn physical_object_visible(&self, expected_card: u32) -> Result<bool, LinuxAudioError> {
        self.roundtrip()?;
        let observed = Rc::new(RefCell::new(HashMap::<u32, PhysicalObjectSnapshot>::new()));
        let mut devices = Vec::new();
        let mut device_listeners = Vec::new();
        let mut nodes = Vec::new();
        let mut node_listeners = Vec::new();
        for object in self.objects.borrow().values() {
            match object.type_ {
                ObjectType::Device => {
                    let device = self
                        .registry
                        .bind::<pw::device::Device, _>(object)
                        .map_err(|error| LinuxAudioError::PipeWire(error.to_string()))?;
                    let snapshots = Rc::clone(&observed);
                    let listener = device
                        .add_listener_local()
                        .info(move |info| {
                            if let Some(properties) = info.props() {
                                snapshots.borrow_mut().insert(
                                    info.id(),
                                    PhysicalObjectSnapshot::from_properties(
                                        PhysicalObjectKind::Device,
                                        properties,
                                    ),
                                );
                            }
                        })
                        .register();
                    devices.push(device);
                    device_listeners.push(listener);
                }
                ObjectType::Node => {
                    let node = self
                        .registry
                        .bind::<pw::node::Node, _>(object)
                        .map_err(|error| LinuxAudioError::PipeWire(error.to_string()))?;
                    let snapshots = Rc::clone(&observed);
                    let listener = node
                        .add_listener_local()
                        .info(move |info| {
                            if let Some(properties) = info.props() {
                                snapshots.borrow_mut().insert(
                                    info.id(),
                                    PhysicalObjectSnapshot::from_properties(
                                        PhysicalObjectKind::Node,
                                        properties,
                                    ),
                                );
                            }
                        })
                        .register();
                    nodes.push(node);
                    node_listeners.push(listener);
                }
                _ => {}
            }
        }
        self.roundtrip()?;
        let exposed = observed.borrow().values().any(|object| object.matches(expected_card));
        drop(node_listeners);
        drop(device_listeners);
        drop(nodes);
        drop(devices);
        Ok(exposed)
    }

    fn roundtrip(&self) -> Result<(), LinuxAudioError> {
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
            self.main_loop.loop_().iterate(Timeout::Finite(PIPEWIRE_ITERATION));
        }
        drop(core_listener);
        if let Some(error) = fatal_error.take() {
            return Err(LinuxAudioError::PipeWire(error));
        }
        if !done.get() {
            return Err(LinuxAudioError::PipeWire("registry roundtrip timed out".to_owned()));
        }
        Ok(())
    }
}
