//! Inactive construction and deterministic ownership for the `PipeWire` mixer graph.

use super::pipewire_filter_ffi::{
    FILTER_STATE_STREAMING, FilterDiagnostics, FilterHandle, GraphProcessor, ProcessFault,
};
use super::{EndpointPlan, PipeWireEndpointDirection};
use librewave_core::DELIBERATE_ENDPOINTS;
use pipewire as pw;
use pw::types::ObjectType;
use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::num::NonZeroU32;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

const ROUNDTRIP_TIMEOUT: Duration = Duration::from_secs(2);
const LOOP_ITERATION: Duration = Duration::from_millis(10);
const FILTER_NODE_NAME: &str = "librewave.internal.mixer";

#[derive(Clone, Debug)]
struct ObservedPort {
    id: u32,
    node_id: u32,
    direction: String,
    name: String,
    channel: Option<String>,
    format_dsp: Option<String>,
}

struct RegistryState {
    objects: HashMap<u32, pw::registry::GlobalObject<pw::properties::PropertiesBox>>,
    node_names: HashMap<String, u32>,
    node_classes: HashMap<u32, String>,
    ports: Vec<ObservedPort>,
    port_generation: u64,
    link_inputs: HashMap<u32, u32>,
    system_playback_ports: Option<[u32; 2]>,
    system_link_generation: u64,
    system_links_present: bool,
    system_link_word: Arc<AtomicU64>,
}

impl RegistryState {
    fn new(system_link_word: Arc<AtomicU64>) -> Self {
        Self {
            objects: HashMap::new(),
            node_names: HashMap::new(),
            node_classes: HashMap::new(),
            ports: Vec::new(),
            port_generation: 0,
            link_inputs: HashMap::new(),
            system_playback_ports: None,
            system_link_generation: 0,
            system_links_present: false,
            system_link_word,
        }
    }

    fn set_system_playback_ports(&mut self, ports: [u32; 2]) {
        self.system_playback_ports = Some(ports);
        self.publish_system_links();
    }

    fn publish_system_links(&mut self) {
        let present = self.system_playback_ports.is_some_and(|ports| {
            ports.iter().all(|port| self.link_inputs.values().any(|input| input == port))
        });
        if present != self.system_links_present {
            self.system_links_present = present;
            self.system_link_generation = self.system_link_generation.saturating_add(1);
            let word = self.system_link_generation.saturating_mul(2) | u64::from(present);
            self.system_link_word.store(word, Ordering::Release);
        }
    }
}

impl RegistryState {
    fn observe(&mut self, global: &pw::registry::GlobalObject<&pw::spa::utils::dict::DictRef>) {
        if global.type_ == ObjectType::Node {
            if let Some(properties) = global.props {
                if let Some(name) = properties.get("node.name") {
                    self.node_names.insert(name.to_owned(), global.id);
                }
                if let Some(class) = properties.get("media.class") {
                    self.node_classes.insert(global.id, class.to_owned());
                }
            }
        } else if global.type_ == ObjectType::Port {
            if let Some(properties) = global.props {
                let node_id = properties.get("node.id").and_then(|id| id.parse().ok());
                let direction = properties.get("port.direction");
                let name = properties.get("port.name");
                if let (Some(node_id), Some(direction), Some(name)) = (node_id, direction, name) {
                    self.ports.push(ObservedPort {
                        id: global.id,
                        node_id,
                        direction: direction.to_owned(),
                        name: name.to_owned(),
                        channel: properties.get(*pw::keys::AUDIO_CHANNEL).map(str::to_owned),
                        format_dsp: properties.get(*pw::keys::FORMAT_DSP).map(str::to_owned),
                    });
                    self.port_generation = self.port_generation.saturating_add(1);
                }
            }
        } else if global.type_ == ObjectType::Link
            && let Some(input) = global
                .props
                .and_then(|properties| properties.get("link.input.port"))
                .and_then(|id| id.parse().ok())
        {
            self.link_inputs.insert(global.id, input);
            self.publish_system_links();
        }
        self.objects.insert(global.id, global.to_owned());
    }

    fn remove(&mut self, id: u32) {
        self.objects.remove(&id);
        self.link_inputs.remove(&id);
        self.publish_system_links();
        self.node_names.retain(|_, node_id| *node_id != id);
        self.node_classes.remove(&id);
        let prior_ports = self.ports.len();
        self.ports.retain(|port| port.id != id && port.node_id != id);
        if self.ports.len() != prior_ports {
            self.port_generation = self.port_generation.saturating_add(1);
        }
    }

    fn node_id(&self, name: &str) -> Result<u32, GraphError> {
        self.node_names.get(name).copied().ok_or_else(|| GraphError::MissingNode(name.to_owned()))
    }

    fn port_id(&self, node_id: u32, direction: &str, name: &str) -> Result<u32, GraphError> {
        self.ports
            .iter()
            .find(|port| {
                port.node_id == node_id && port.direction == direction && port.name == name
            })
            .map(|port| port.id)
            .ok_or_else(|| GraphError::MissingPort {
                node_id,
                direction: direction.to_owned(),
                name: name.to_owned(),
            })
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) enum GraphError {
    InvalidEndpointContract,
    PipeWire(String),
    RoundtripTimeout,
    PortConfigurationTimeout,
    MissingNode(String),
    MissingPort { node_id: u32, direction: String, name: String },
    DriverMismatch { node: String, expected: u32, actual: Option<u32> },
    EndpointCount(usize),
    InvalidFilterPort(String),
    CallbackDidNotQuiesce,
    AttemptFault(ProcessFault),
}

impl std::fmt::Display for GraphError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidEndpointContract => {
                formatter.write_str("invalid PipeWire endpoint contract")
            }
            Self::PipeWire(error) => formatter.write_str(error),
            Self::RoundtripTimeout => formatter.write_str("PipeWire roundtrip timed out"),
            Self::PortConfigurationTimeout => {
                formatter.write_str("WirePlumber adapter ports did not become stable")
            }
            Self::MissingNode(name) => write!(formatter, "PipeWire node {name} did not appear"),
            Self::MissingPort { node_id, direction, name } => {
                write!(formatter, "PipeWire node {node_id} has no {direction} port named {name}")
            }
            Self::DriverMismatch { node, expected, actual } => {
                write!(formatter, "PipeWire node {node} has driver {actual:?}, expected {expected}")
            }
            Self::EndpointCount(count) => {
                write!(formatter, "PipeWire exposed {count} LibreWave audio endpoints, expected 4")
            }
            Self::InvalidFilterPort(name) => {
                write!(formatter, "PipeWire filter port {name} has an invalid DSP contract")
            }
            Self::CallbackDidNotQuiesce => {
                formatter.write_str("PipeWire filter callback did not quiesce")
            }
            Self::AttemptFault(fault) => {
                write!(formatter, "PipeWire graph attempt faulted: {fault:?}")
            }
        }
    }
}

impl std::error::Error for GraphError {}

#[derive(Default)]
struct TeardownState {
    terminal: Option<Result<(), GraphError>>,
}

impl TeardownState {
    fn terminal(&self) -> Option<Result<(), GraphError>> {
        self.terminal.clone()
    }

    fn finish(&mut self, result: Result<(), GraphError>) -> Result<(), GraphError> {
        if let Some(terminal) = self.terminal() {
            return terminal;
        }
        self.terminal = Some(result.clone());
        result
    }
}

pub(super) struct InactiveGraph<P: GraphProcessor> {
    _core_listener: pw::core::Listener,
    main_loop: pw::main_loop::MainLoopRc,
    core: pw::core::CoreRc,
    peer_disconnected: Rc<Cell<bool>>,
    registry: pw::registry::RegistryRc,
    registry_listener: Option<pw::registry::Listener>,
    registry_state: Rc<RefCell<RegistryState>>,
    filter: FilterHandle<P>,
    system: Option<pw::node::Node>,
    microphone: Option<pw::node::Node>,
    monitor: Option<pw::node::Node>,
    stream: Option<pw::node::Node>,
    links: Vec<pw::link::Link>,
    teardown: TeardownState,
    endpoints: [EndpointPlan; 4],
    _context: pw::context::ContextRc,
}

pub(super) struct ActiveGraph<P: GraphProcessor> {
    inner: InactiveGraph<P>,
}

#[derive(Clone, Copy)]
pub(super) enum AdapterPortMode {
    SelfConfigured,
    WirePlumber,
}

impl<P: GraphProcessor> InactiveGraph<P> {
    #[allow(clippy::too_many_lines)]
    pub(super) fn construct(
        remote_name: &str,
        endpoints: &[EndpointPlan],
        quantum: NonZeroU32,
        port_mode: AdapterPortMode,
        processor: P,
    ) -> Result<Self, GraphError> {
        validate_endpoints(endpoints)?;
        pw::init();
        let main_loop = pw::main_loop::MainLoopRc::new(None).map_err(pipewire_error)?;
        let context = pw::context::ContextRc::new(&main_loop, None).map_err(pipewire_error)?;
        let core = context
            .connect_rc(Some(pw::properties::properties! {
                *pw::keys::REMOTE_NAME => remote_name
            }))
            .map_err(pipewire_error)?;
        let peer_disconnected = Rc::new(Cell::new(false));
        let disconnected = Rc::clone(&peer_disconnected);
        let core_listener = core
            .add_listener_local()
            .error(move |id, _, result, _| {
                if id == pw::core::PW_ID_CORE && result == -libc::EPIPE {
                    disconnected.set(true);
                }
            })
            .register();
        let registry = core.get_registry_rc().map_err(pipewire_error)?;
        let system_link_word = Arc::new(AtomicU64::new(0));
        let registry_state =
            Rc::new(RefCell::new(RegistryState::new(Arc::clone(&system_link_word))));
        let observed = Rc::clone(&registry_state);
        let removed = Rc::clone(&registry_state);
        let registry_listener = registry
            .add_listener_local()
            .global(move |global| observed.borrow_mut().observe(global))
            .global_remove(move |id| removed.borrow_mut().remove(id))
            .register();
        roundtrip(&main_loop, &core)?;
        let system = create_adapter(&core, &endpoints[0], true, quantum)?;
        let microphone = create_adapter(&core, &endpoints[1], false, quantum)?;
        let monitor = create_adapter(&core, &endpoints[2], false, quantum)?;
        let stream = create_adapter(&core, &endpoints[3], false, quantum)?;
        roundtrip(&main_loop, &core)?;
        roundtrip(&main_loop, &core)?;
        // WirePlumber can replace adapter ports after the PipeWire round trips.
        // Bind the permanent graph only after the expected port shape is quiet.
        let port_deadline = Instant::now() + Duration::from_secs(2);
        let mut settled = None;
        loop {
            let (ports, generation) = {
                let state = registry_state.borrow();
                (expected_adapter_ports(&state, endpoints), state.port_generation)
            };
            if ports.is_ok()
                && (matches!(port_mode, AdapterPortMode::SelfConfigured)
                    || settled.is_some_and(|(prior, since): (u64, Instant)| {
                        prior == generation && since.elapsed() >= Duration::from_millis(100)
                    }))
            {
                break;
            }
            if Instant::now() >= port_deadline {
                return Err(ports.err().unwrap_or(GraphError::PortConfigurationTimeout));
            }
            if ports.is_ok() && settled.is_none_or(|(prior, _): (u64, Instant)| prior != generation)
            {
                settled = Some((generation, Instant::now()));
            }
            main_loop.loop_().iterate(Duration::from_millis(10));
        }

        let system_id = registry_state.borrow().node_id(endpoints[0].node_name)?;
        let mut filter = FilterHandle::new(
            &core,
            "LibreWave mixer",
            pw::properties::properties! {
                *pw::keys::NODE_NAME => FILTER_NODE_NAME,
                *pw::keys::MEDIA_TYPE => "Audio",
                *pw::keys::MEDIA_CATEGORY => "Filter",
                *pw::keys::MEDIA_ROLE => "DSP",
                *pw::keys::NODE_GROUP => "librewave.mixer",
                "node.sync-group" => "librewave.graph",
                *pw::keys::NODE_DRIVER => "false",
                "node.want-driver" => "true",
                *pw::keys::NODE_ALWAYS_PROCESS => "true",
                *pw::keys::NODE_PAUSE_ON_IDLE => "false",
                "node.suspend-on-idle" => "false"
            },
            system_id,
            quantum,
            processor,
            system_link_word,
        )
        .map_err(GraphError::PipeWire)?;
        for (index, output, name, channel) in filter_ports() {
            filter
                .add_port(
                    index,
                    output,
                    pw::properties::properties! {
                        *pw::keys::FORMAT_DSP => "32 bit float mono audio",
                        *pw::keys::PORT_NAME => name,
                        *pw::keys::AUDIO_CHANNEL => channel
                    },
                )
                .map_err(GraphError::PipeWire)?;
        }
        filter.connect_inactive().map_err(GraphError::PipeWire)?;
        roundtrip(&main_loop, &core)?;
        roundtrip(&main_loop, &core)?;

        let filter_id = registry_state.borrow().node_id(FILTER_NODE_NAME)?;
        if filter.node_id() != Some(filter_id) {
            return Err(GraphError::MissingNode(FILTER_NODE_NAME.to_owned()));
        }
        validate_filter_ports(&registry_state.borrow(), filter_id)?;
        let ids = [
            system_id,
            registry_state.borrow().node_id(endpoints[1].node_name)?,
            registry_state.borrow().node_id(endpoints[2].node_name)?,
            registry_state.borrow().node_id(endpoints[3].node_name)?,
            filter_id,
        ];
        let ports = internal_ports(&registry_state.borrow(), ids)?;
        let system_playback_ports = [
            registry_state.borrow().port_id(system_id, "in", "playback_FL")?,
            registry_state.borrow().port_id(system_id, "in", "playback_FR")?,
        ];
        registry_state.borrow_mut().set_system_playback_ports(system_playback_ports);
        let mut links = Vec::with_capacity(8);
        for (output, input) in ports {
            links.push(create_link(&core, output, input)?);
        }
        roundtrip(&main_loop, &core)?;
        roundtrip(&main_loop, &core)?;
        validate_endpoint_count(&registry_state.borrow())?;

        Ok(Self {
            main_loop,
            core,
            _core_listener: core_listener,
            peer_disconnected,
            registry,
            registry_listener: Some(registry_listener),
            registry_state,
            filter,
            system: Some(system),
            microphone: Some(microphone),
            monitor: Some(monitor),
            stream: Some(stream),
            links,
            teardown: TeardownState::default(),
            endpoints: endpoints.try_into().map_err(|_| GraphError::InvalidEndpointContract)?,
            _context: context,
        })
    }

    pub(super) fn activate(mut self) -> Result<ActiveGraph<P>, GraphError> {
        self.filter.set_active(true).map_err(GraphError::PipeWire)?;
        let deadline = Instant::now() + ROUNDTRIP_TIMEOUT;
        while self.filter.diagnostics().filter_state != FILTER_STATE_STREAMING {
            let diagnostics = self.filter.diagnostics();
            if diagnostics.failed {
                let fault = diagnostics
                    .faults
                    .into_iter()
                    .flatten()
                    .next()
                    .map_or(ProcessFault::Processor, |record| record.fault);
                return Err(GraphError::AttemptFault(fault));
            }
            if Instant::now() >= deadline {
                return Err(GraphError::RoundtripTimeout);
            }
            self.main_loop.loop_().iterate(LOOP_ITERATION);
        }
        let ids = {
            let state = self.registry_state.borrow();
            [
                state.node_id(self.endpoints[0].node_name)?,
                state.node_id(self.endpoints[1].node_name)?,
                state.node_id(self.endpoints[2].node_name)?,
                state.node_id(self.endpoints[3].node_name)?,
                state.node_id(FILTER_NODE_NAME)?,
            ]
        };
        let driver_deadline = Instant::now() + ROUNDTRIP_TIMEOUT;
        loop {
            match validate_drivers(
                &self.main_loop,
                &self.core,
                &self.registry,
                &self.registry_state,
                &ids,
                &self.endpoints,
            ) {
                Ok(()) => break,
                Err(error @ GraphError::DriverMismatch { .. }) => {
                    if Instant::now() >= driver_deadline {
                        return Err(error);
                    }
                    self.main_loop.loop_().iterate(LOOP_ITERATION);
                }
                Err(error) => return Err(error),
            }
        }
        Ok(ActiveGraph { inner: self })
    }

    fn teardown(&mut self) -> Result<(), GraphError> {
        if let Some(terminal) = self.teardown.terminal() {
            return terminal;
        }
        let mut first_error = self.filter.set_active(false).err().map(GraphError::PipeWire);
        if !self.peer_disconnected.get()
            && let Err(error) = roundtrip(&self.main_loop, &self.core)
            && !self.peer_disconnected.get()
        {
            first_error.get_or_insert(error);
        }
        let deadline = Instant::now() + ROUNDTRIP_TIMEOUT;
        while self.filter.callbacks_in_flight() != 0 {
            if Instant::now() >= deadline {
                first_error.get_or_insert(GraphError::CallbackDidNotQuiesce);
                break;
            }
            self.main_loop.loop_().iterate(LOOP_ITERATION);
        }
        self.links.clear();
        if let Err(error) = self.filter.destroy() {
            first_error.get_or_insert(GraphError::PipeWire(error));
        }
        for node in
            [self.stream.take(), self.monitor.take(), self.microphone.take(), self.system.take()]
                .into_iter()
                .flatten()
        {
            drop(node);
        }
        // A confirmed core EPIPE makes server acknowledgement impossible.
        // Local filter destruction and callback quiescence remain mandatory.
        // A timeout on a connected peer still blocks replacement.
        if !self.peer_disconnected.get()
            && let Err(error) = roundtrip(&self.main_loop, &self.core)
            && !self.peer_disconnected.get()
        {
            first_error.get_or_insert(error);
        }
        self.registry_listener.take();
        self.teardown.finish(first_error.map_or(Ok(()), Err))
    }
}

impl<P: GraphProcessor> ActiveGraph<P> {
    pub(super) fn iterate(&mut self, timeout: Duration) {
        self.inner.main_loop.loop_().iterate(timeout);
    }

    pub(super) fn diagnostics(&self) -> FilterDiagnostics {
        self.inner.filter.diagnostics()
    }

    pub(super) fn node_ids(&self) -> Result<[u32; 5], GraphError> {
        let state = self.inner.registry_state.borrow();
        Ok([
            state.node_id("librewave.system")?,
            state.node_id("librewave.microphone")?,
            state.node_id("librewave.monitor-mix")?,
            state.node_id("librewave.stream-mix")?,
            state.node_id(FILTER_NODE_NAME)?,
        ])
    }

    pub(super) fn teardown(&mut self) -> Result<(), GraphError> {
        self.inner.teardown()
    }
}

impl<P: GraphProcessor> Drop for InactiveGraph<P> {
    fn drop(&mut self) {
        let _ = self.teardown();
    }
}

fn validate_endpoints(endpoints: &[EndpointPlan]) -> Result<(), GraphError> {
    if endpoints.len() != 4
        || endpoints.iter().map(|plan| plan.endpoint).ne(DELIBERATE_ENDPOINTS.iter().copied())
        || endpoints[0].direction != PipeWireEndpointDirection::Input
        || endpoints[1..].iter().any(|plan| plan.direction != PipeWireEndpointDirection::Output)
    {
        return Err(GraphError::InvalidEndpointContract);
    }
    Ok(())
}

fn create_adapter(
    core: &pw::core::Core,
    plan: &EndpointPlan,
    driver: bool,
    quantum: NonZeroU32,
) -> Result<pw::node::Node, GraphError> {
    let mut properties = pw::properties::properties! {
        *pw::keys::FACTORY_NAME => "support.null-audio-sink",
        *pw::keys::NODE_NAME => plan.node_name,
        *pw::keys::NODE_DESCRIPTION => plan.node_description,
        *pw::keys::MEDIA_CLASS => plan.media_class,
        *pw::keys::MEDIA_TYPE => "Audio",
        *pw::keys::MEDIA_CATEGORY => "Duplex",
        *pw::keys::MEDIA_ROLE => "Production",
        *pw::keys::NODE_VIRTUAL => "true",
        *pw::keys::NODE_GROUP => "librewave.adapters",
        "node.sync-group" => "librewave.graph",
        *pw::keys::NODE_DRIVER => if driver { "true" } else { "false" },
        "node.want-driver" => "true",
        *pw::keys::NODE_ALWAYS_PROCESS => "true",
        *pw::keys::NODE_PAUSE_ON_IDLE => "false",
        "node.suspend-on-idle" => "false",
        "audio.rate" => "48000",
        *pw::keys::AUDIO_CHANNELS => "2",
        "audio.position" => "[ FL FR ]",
        "adapter.auto-port-config" => "{ mode = dsp monitor = true }",
        "monitor.passthrough" => "true"
    };
    if driver {
        // System feeds the mixer through monitor ports, which are unscaled by default.
        properties.insert("monitor.channel-volumes", "true");
        properties.insert(*pw::keys::PRIORITY_DRIVER, "200001");
        properties.insert("node.force-quantum", quantum.get().to_string());
    }
    core.create_object::<pw::node::Node>("adapter", &properties).map_err(pipewire_error)
}

fn filter_ports() -> [(usize, bool, &'static str, &'static str); 8] {
    [
        (0, false, "system_FL", "FL"),
        (1, false, "system_FR", "FR"),
        (2, true, "microphone_FL", "FL"),
        (3, true, "microphone_FR", "FR"),
        (4, true, "monitor_FL", "FL"),
        (5, true, "monitor_FR", "FR"),
        (6, true, "stream_FL", "FL"),
        (7, true, "stream_FR", "FR"),
    ]
}

fn validate_filter_ports(state: &RegistryState, filter_id: u32) -> Result<(), GraphError> {
    for (_, output, name, channel) in filter_ports() {
        let direction = if output { "out" } else { "in" };
        let port = state
            .ports
            .iter()
            .find(|port| {
                port.node_id == filter_id && port.direction == direction && port.name == name
            })
            .ok_or_else(|| GraphError::InvalidFilterPort(name.to_owned()))?;
        if port.channel.as_deref() != Some(channel)
            || port.format_dsp.as_deref() != Some("32 bit float mono audio")
        {
            return Err(GraphError::InvalidFilterPort(name.to_owned()));
        }
    }
    if state.ports.iter().filter(|port| port.node_id == filter_id).count() != 8 {
        return Err(GraphError::InvalidFilterPort("port-count".to_owned()));
    }
    Ok(())
}

const SOURCE_PORT_NAMES: [&str; 4] = ["input_FL", "input_FR", "capture_FL", "capture_FR"];

fn expected_adapter_ports(
    state: &RegistryState,
    endpoints: &[EndpointPlan],
) -> Result<(), GraphError> {
    let system = state.node_id(endpoints[0].node_name)?;
    for name in ["playback_FL", "playback_FR"] {
        state.port_id(system, "in", name)?;
    }
    for name in ["monitor_FL", "monitor_FR"] {
        state.port_id(system, "out", name)?;
    }
    for endpoint in endpoints.iter().skip(1) {
        let node = state.node_id(endpoint.node_name)?;
        for name in &SOURCE_PORT_NAMES[..2] {
            state.port_id(node, "in", name)?;
        }
        for name in &SOURCE_PORT_NAMES[2..] {
            state.port_id(node, "out", name)?;
        }
    }
    Ok(())
}

fn internal_ports(state: &RegistryState, ids: [u32; 5]) -> Result<[(u32, u32); 8], GraphError> {
    let [system, microphone, monitor, stream, filter] = ids;
    let [left, right, _, _] = SOURCE_PORT_NAMES;
    Ok([
        (state.port_id(system, "out", "monitor_FL")?, state.port_id(filter, "in", "system_FL")?),
        (state.port_id(system, "out", "monitor_FR")?, state.port_id(filter, "in", "system_FR")?),
        (state.port_id(filter, "out", "microphone_FL")?, state.port_id(microphone, "in", left)?),
        (state.port_id(filter, "out", "microphone_FR")?, state.port_id(microphone, "in", right)?),
        (state.port_id(filter, "out", "monitor_FL")?, state.port_id(monitor, "in", left)?),
        (state.port_id(filter, "out", "monitor_FR")?, state.port_id(monitor, "in", right)?),
        (state.port_id(filter, "out", "stream_FL")?, state.port_id(stream, "in", left)?),
        (state.port_id(filter, "out", "stream_FR")?, state.port_id(stream, "in", right)?),
    ])
}

fn create_link(
    core: &pw::core::Core,
    output: u32,
    input: u32,
) -> Result<pw::link::Link, GraphError> {
    core.create_object::<pw::link::Link>(
        "link-factory",
        &pw::properties::properties! {
            *pw::keys::LINK_OUTPUT_PORT => output.to_string(),
            *pw::keys::LINK_INPUT_PORT => input.to_string(),
            *pw::keys::OBJECT_LINGER => "false",
            *pw::keys::LINK_PASSIVE => "false"
        },
    )
    .map_err(pipewire_error)
}

fn validate_endpoint_count(state: &RegistryState) -> Result<(), GraphError> {
    let count = state
        .node_names
        .iter()
        .filter(|(name, id)| {
            name.starts_with("librewave.")
                && matches!(
                    state.node_classes.get(id).map(String::as_str),
                    Some("Audio/Sink" | "Audio/Source/Virtual")
                )
        })
        .count();
    if count == 4 { Ok(()) } else { Err(GraphError::EndpointCount(count)) }
}

fn validate_drivers(
    main_loop: &pw::main_loop::MainLoop,
    core: &pw::core::Core,
    registry: &pw::registry::Registry,
    state: &Rc<RefCell<RegistryState>>,
    ids: &[u32; 5],
    endpoints: &[EndpointPlan],
) -> Result<(), GraphError> {
    let observed = Rc::new(RefCell::new(HashMap::<String, Option<u32>>::new()));
    let mut nodes = Vec::new();
    let mut listeners = Vec::new();
    for (index, id) in ids.iter().copied().enumerate() {
        let name = if index < 4 { endpoints[index].node_name } else { FILTER_NODE_NAME };
        let node = {
            let state = state.borrow();
            let object =
                state.objects.get(&id).ok_or_else(|| GraphError::MissingNode(name.to_owned()))?;
            registry.bind::<pw::node::Node, _>(object).map_err(pipewire_error)?
        };
        let target = Rc::clone(&observed);
        let name = name.to_owned();
        let listener = node
            .add_listener_local()
            .info(move |info| {
                let driver = info
                    .props()
                    .and_then(|props| props.get("node.driver-id"))
                    .and_then(|value| value.parse().ok());
                target.borrow_mut().insert(name.clone(), driver);
            })
            .register();
        nodes.push(node);
        listeners.push(listener);
    }
    roundtrip(main_loop, core)?;
    let expected = ids[0];
    for name in endpoints.iter().skip(1).map(|plan| plan.node_name).chain([FILTER_NODE_NAME]) {
        let actual = observed.borrow().get(name).copied().flatten();
        if actual != Some(expected) {
            return Err(GraphError::DriverMismatch { node: name.to_owned(), expected, actual });
        }
    }
    drop(listeners);
    drop(nodes);
    Ok(())
}

fn roundtrip(main_loop: &pw::main_loop::MainLoop, core: &pw::core::Core) -> Result<(), GraphError> {
    let done = Rc::new(Cell::new(false));
    let completed = Rc::clone(&done);
    let error = Rc::new(RefCell::new(None));
    let failed = Rc::clone(&error);
    let pending = core.sync(0).map_err(pipewire_error)?;
    let listener = core
        .add_listener_local()
        .done(move |id, sequence| {
            if id == pw::core::PW_ID_CORE && sequence == pending {
                completed.set(true);
            }
        })
        .error(move |_id, _sequence, result, message| {
            *failed.borrow_mut() = Some(format!("{message} ({result})"));
        })
        .register();
    let deadline = Instant::now() + ROUNDTRIP_TIMEOUT;
    while !done.get() && error.borrow().is_none() {
        if Instant::now() >= deadline {
            drop(listener);
            return Err(GraphError::RoundtripTimeout);
        }
        main_loop.loop_().iterate(LOOP_ITERATION);
    }
    drop(listener);
    if let Some(error) = error.take() { Err(GraphError::PipeWire(error)) } else { Ok(()) }
}

#[allow(clippy::needless_pass_by_value)]
fn pipewire_error(error: pw::Error) -> GraphError {
    GraphError::PipeWire(error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio_host::endpoint_plans;
    use librewave_core::EndpointId;

    #[test]
    fn exact_endpoint_contract_is_required() {
        let plans = endpoint_plans(DELIBERATE_ENDPOINTS).expect("deliberate endpoints");
        assert!(validate_endpoints(&plans).is_ok());
        assert!(matches!(
            validate_endpoints(&plans[..3]),
            Err(GraphError::InvalidEndpointContract)
        ));
    }

    #[test]
    fn permanent_filter_port_contract_is_exact() {
        assert_eq!(filter_ports().len(), 8);
        assert_eq!(filter_ports()[0], (0, false, "system_FL", "FL"));
        assert_eq!(filter_ports()[7], (7, true, "stream_FR", "FR"));
    }

    #[test]
    fn endpoint_identity_is_not_reordered() {
        let plans = endpoint_plans(DELIBERATE_ENDPOINTS).expect("deliberate endpoints");
        assert_eq!(plans[0].endpoint, EndpointId::System);
        assert_eq!(plans[3].endpoint, EndpointId::StreamMix);
    }

    #[test]
    fn failed_teardown_result_is_terminal() {
        let mut state = TeardownState::default();
        let failure = Err(GraphError::CallbackDidNotQuiesce);

        assert_eq!(state.finish(failure.clone()), failure);
        assert_eq!(state.terminal(), Some(failure.clone()));
        assert_eq!(state.finish(Ok(())), failure);
    }

    mod integration {
        use super::*;
        use crate::audio_host::pipewire_filter_ffi::{
            ProcessBlock, ProcessFault, SystemCycleState, TestPeer, TestPeerMode,
        };
        use librewave_core::{
            MICROPHONE_SOURCE_ID, MIXER_OUTPUT_ENDPOINTS, MixerProfile, SYSTEM_SOURCE_ID,
        };
        use librewave_engine::{InputBuffer, MixerConfig, MixerEngine, OutputBuffer};
        use std::fs;
        use std::path::PathBuf;
        use std::process::{Child, Command, Stdio};
        use std::sync::Arc;
        use std::sync::atomic::{AtomicU64, Ordering};

        const REMOTE: &str = "librewave-pipewire-integration";

        #[derive(Default)]
        struct ProcessorProof {
            idle: AtomicU64,
            priming: AtomicU64,
            active: AtomicU64,
            aligned_system: AtomicU64,
            draining: AtomicU64,
            vector_errors: AtomicU64,
        }

        struct EngineProcessor {
            engine: MixerEngine,
            proof: Arc<ProcessorProof>,
            microphone: Vec<f32>,
            system: Vec<f32>,
            microphone_output: Vec<f32>,
            monitor_output: Vec<f32>,
            stream_output: Vec<f32>,
            system_aligned: bool,
            system_warmup_cycles: u32,
        }

        impl EngineProcessor {
            fn new(maximum_frames: usize, proof: Arc<ProcessorProof>) -> Self {
                let profile = MixerProfile::default();
                let config = MixerConfig::try_new(
                    maximum_frames,
                    MICROPHONE_SOURCE_ID,
                    &[MICROPHONE_SOURCE_ID, SYSTEM_SOURCE_ID],
                )
                .expect("integration mixer config");
                let controls = [profile.sources[0].controls, profile.sources[1].controls];
                let (engine, _control) =
                    MixerEngine::new(config, &controls).expect("integration mixer controls");
                let samples = maximum_frames * 2;
                Self {
                    engine,
                    proof,
                    microphone: vec![0.0; samples],
                    system: vec![0.0; samples],
                    microphone_output: vec![0.0; samples],
                    monitor_output: vec![0.0; samples],
                    stream_output: vec![0.0; samples],
                    system_aligned: false,
                    system_warmup_cycles: 0,
                }
            }
        }

        impl GraphProcessor for EngineProcessor {
            fn process(&mut self, block: ProcessBlock<'_>) -> Result<(), ProcessFault> {
                let frames = block.timing.quantum as usize;
                let samples = frames * 2;
                for index in 0..frames {
                    let position = block.timing.position + index as u64;
                    let left_code =
                        u16::try_from(position % 997).map_err(|_| ProcessFault::Processor)?;
                    let right_code =
                        u16::try_from(position % 991).map_err(|_| ProcessFault::Processor)?;
                    self.microphone[index * 2] = 0.125 + f32::from(left_code) / 8192.0;
                    self.microphone[index * 2 + 1] = 0.1875 + f32::from(right_code) / 8192.0;
                    self.system[index * 2] = block.system.left[index];
                    self.system[index * 2 + 1] = block.system.right[index];
                }
                let system_active = self.system[..samples].iter().any(|sample| *sample != 0.0);
                match block.system_state {
                    SystemCycleState::Idle => {
                        self.proof.idle.fetch_add(1, Ordering::Relaxed);
                        if system_active {
                            return Err(ProcessFault::Processor);
                        }
                    }
                    SystemCycleState::Priming => {
                        self.proof.priming.fetch_add(1, Ordering::Relaxed);
                        self.system_aligned = false;
                        self.system_warmup_cycles = 0;
                        if system_active {
                            return Err(ProcessFault::SystemPriming);
                        }
                    }
                    SystemCycleState::Active => {
                        self.proof.active.fetch_add(1, Ordering::Relaxed);
                        if system_active {
                            let bounded_latency = system_has_bounded_latency(
                                block.timing.position,
                                block.timing.quantum,
                                block.system.left,
                                block.system.right,
                            );
                            if bounded_latency {
                                self.system_aligned = true;
                                self.proof.aligned_system.fetch_add(1, Ordering::Relaxed);
                            } else if self.system_aligned || self.system_warmup_cycles >= 16 {
                                self.proof.vector_errors.fetch_add(1, Ordering::Relaxed);
                                return Err(ProcessFault::SystemLatency);
                            } else {
                                self.system_warmup_cycles += 1;
                            }
                        }
                    }
                    SystemCycleState::Draining => {
                        self.proof.draining.fetch_add(1, Ordering::Relaxed);
                        self.system[..samples].fill(0.0);
                    }
                }
                let inputs = [
                    InputBuffer::new(MICROPHONE_SOURCE_ID, &self.microphone[..samples]),
                    InputBuffer::new(SYSTEM_SOURCE_ID, &self.system[..samples]),
                ];
                let mut outputs = [
                    OutputBuffer::new(
                        MIXER_OUTPUT_ENDPOINTS[0],
                        &mut self.microphone_output[..samples],
                    ),
                    OutputBuffer::new(
                        MIXER_OUTPUT_ENDPOINTS[1],
                        &mut self.monitor_output[..samples],
                    ),
                    OutputBuffer::new(
                        MIXER_OUTPUT_ENDPOINTS[2],
                        &mut self.stream_output[..samples],
                    ),
                ];
                self.engine
                    .process(frames, &inputs, &mut outputs)
                    .map_err(|_| ProcessFault::Processor)?;
                for index in 0..frames {
                    block.microphone.left[index] = self.microphone_output[index * 2];
                    block.microphone.right[index] = self.microphone_output[index * 2 + 1];
                    block.monitor.left[index] = self.monitor_output[index * 2];
                    block.monitor.right[index] = self.monitor_output[index * 2 + 1];
                    block.stream.left[index] = self.stream_output[index * 2];
                    block.stream.right[index] = self.stream_output[index * 2 + 1];
                }
                Ok(())
            }
        }

        #[allow(clippy::cast_possible_truncation)]
        fn system_has_bounded_latency(
            position: u64,
            quantum: u32,
            left: &[f32],
            right: &[f32],
        ) -> bool {
            // The two adapter ports can acquire different buffer offsets.
            // Each channel must still carry one continuous recent producer vector.
            let mut first_left = None;
            let mut first_right = None;
            for (index, (left, right)) in left.iter().zip(right).enumerate() {
                let left_code = ((*left - 0.25) * 16_384.0).round() as i32;
                let right_code = ((*right - 0.375) * 16_384.0).round() as i32;
                let filter = position + index as u64;
                let Some(left_code) = u32::try_from(left_code).ok().filter(|code| *code < 4093)
                else {
                    return false;
                };
                let Some(right_code) = u32::try_from(right_code).ok().filter(|code| *code < 4091)
                else {
                    return false;
                };
                let left_latency = ((filter % 4093) as u32 + 4093 - left_code) % 4093;
                let right_latency = ((filter % 4091) as u32 + 4091 - right_code) % 4091;
                if left_latency > 2 * quantum
                    || right_latency > 2 * quantum
                    || first_left.is_some_and(|prior| prior != left_latency)
                    || first_right.is_some_and(|prior| prior != right_latency)
                {
                    return false;
                }
                first_left = Some(left_latency);
                first_right = Some(right_latency);
            }
            first_left.is_some() && first_right.is_some()
        }

        struct PrivatePipeWire {
            child: Child,
            root: PathBuf,
            prior_environment: Vec<(&'static str, Option<std::ffi::OsString>)>,
        }

        struct TestChild(Child);

        impl Drop for TestChild {
            fn drop(&mut self) {
                let _ = self.0.kill();
                let _ = self.0.wait();
            }
        }

        impl PrivatePipeWire {
            fn start(quantum: u32, cycle: u32) -> Self {
                Self::start_with_metadata(quantum, cycle, false)
            }
            fn start_with_metadata(quantum: u32, cycle: u32, metadata: bool) -> Self {
                require_program("pipewire");
                require_program("pw-cli");
                require_program("pw-metadata");
                let root = std::env::temp_dir().join(format!(
                    "librewave-pipewire-integration-{}-{quantum}-{cycle}",
                    std::process::id()
                ));
                if root.exists() {
                    fs::remove_dir_all(&root)
                        .expect("remove stale private PipeWire test directory");
                }
                let config = root.join("config/pipewire");
                let runtime = root.join("runtime");
                fs::create_dir_all(&config).expect("create private PipeWire config directory");
                fs::create_dir_all(&runtime).expect("create private PipeWire runtime directory");
                let mut config_text = server_config(quantum);
                if metadata {
                    config_text.push_str("\ncontext.objects = [ { factory = metadata args = { metadata.name = default } } ]\n");
                }
                fs::write(config.join("proof-server.conf"), config_text)
                    .expect("write private PipeWire server config");
                fs::write(config.join("proof-client.conf"), client_config())
                    .expect("write private PipeWire client config");
                let log = fs::File::create(root.join("pipewire.log"))
                    .expect("create private PipeWire log");
                let mut child = Command::new("pipewire")
                    .arg("-c")
                    .arg("proof-server.conf")
                    .env("XDG_CONFIG_HOME", root.join("config"))
                    .env("XDG_RUNTIME_DIR", &runtime)
                    .env("PIPEWIRE_RUNTIME_DIR", &runtime)
                    .stdout(Stdio::null())
                    .stderr(Stdio::from(log))
                    .spawn()
                    .expect("start private PipeWire daemon");
                let socket = runtime.join(REMOTE);
                let deadline = Instant::now() + Duration::from_secs(3);
                while !socket.exists() {
                    if let Some(status) = child.try_wait().expect("inspect private PipeWire daemon")
                    {
                        panic!("private PipeWire daemon exited before startup: {status}");
                    }
                    if Instant::now() >= deadline {
                        let _ = child.kill();
                        let _ = child.wait();
                        panic!("private PipeWire socket did not appear");
                    }
                    std::thread::sleep(Duration::from_millis(10));
                }
                let variables = [
                    ("XDG_CONFIG_HOME", root.join("config").into_os_string()),
                    ("XDG_RUNTIME_DIR", runtime.clone().into_os_string()),
                    ("PIPEWIRE_RUNTIME_DIR", runtime.into_os_string()),
                    ("PIPEWIRE_CONFIG_NAME", "proof-client.conf".into()),
                    ("PIPEWIRE_REMOTE", REMOTE.into()),
                ];
                let prior_environment =
                    variables.iter().map(|(name, _)| (*name, std::env::var_os(name))).collect();
                for (name, value) in variables {
                    // SAFETY: this ignored integration test is required to run
                    // alone. It changes the environment before it creates any
                    // PipeWire client or helper thread.
                    unsafe { std::env::set_var(name, value) };
                }
                Self { child, root, prior_environment }
            }

            fn command(&self, program: &str) -> Command {
                let mut command = Command::new(program);
                command
                    .env("XDG_CONFIG_HOME", self.root.join("config"))
                    .env("XDG_RUNTIME_DIR", self.root.join("runtime"))
                    .env("PIPEWIRE_RUNTIME_DIR", self.root.join("runtime"))
                    .env("PIPEWIRE_CONFIG_NAME", "proof-client.conf")
                    .env("PIPEWIRE_REMOTE", REMOTE);
                command
            }

            fn start_wireplumber(&self) -> TestChild {
                require_program("wireplumber");
                let log = fs::File::create(self.root.join("wireplumber.log"))
                    .expect("create private WirePlumber log");
                TestChild(
                    self.command("wireplumber")
                        .args(["-p", "policy"])
                        .env("XDG_DATA_HOME", self.root.join("data"))
                        .env("XDG_STATE_HOME", self.root.join("state"))
                        .env("XDG_CACHE_HOME", self.root.join("cache"))
                        .stdout(Stdio::null())
                        .stderr(Stdio::from(log))
                        .spawn()
                        .expect("start private WirePlumber"),
                )
            }
        }

        impl Drop for PrivatePipeWire {
            fn drop(&mut self) {
                let _ = self.child.kill();
                let _ = self.child.wait();
                for (name, value) in self.prior_environment.drain(..) {
                    // SAFETY: the integration test still runs alone and all
                    // PipeWire clients have already been destroyed.
                    unsafe {
                        if let Some(value) = value {
                            std::env::set_var(name, value);
                        } else {
                            std::env::remove_var(name);
                        }
                    }
                }
                let _ = fs::remove_dir_all(&self.root);
            }
        }

        fn require_program(program: &str) {
            assert!(
                Command::new(program)
                    .arg("--version")
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .status()
                    .is_ok(),
                "{program} is required for the explicit PipeWire integration test"
            );
        }

        fn server_config(quantum: u32) -> String {
            format!(
                r"context.properties = {{
    core.daemon = true
    core.name = {REMOTE}
    default.clock.rate = 48000
    default.clock.allowed-rates = [ 48000 ]
    default.clock.quantum = {quantum}
    default.clock.min-quantum = 32
    default.clock.max-quantum = 1024
    default.clock.quantum-limit = 1024
    settings.check-quantum = true
    settings.check-rate = true
}}
context.spa-libs = {{
    audio.convert.* = audioconvert/libspa-audioconvert
    audio.adapt = audioconvert/libspa-audioconvert
    support.* = support/libspa-support
}}
context.modules = [
    {{ name = libpipewire-module-protocol-native }}
    {{ name = libpipewire-module-metadata }}
    {{ name = libpipewire-module-spa-node-factory }}
    {{ name = libpipewire-module-client-node }}
    {{ name = libpipewire-module-client-device }}
    {{ name = libpipewire-module-access args = {{ access.socket = {{ {REMOTE} = unrestricted }} }} }}
    {{ name = libpipewire-module-adapter }}
    {{ name = libpipewire-module-link-factory }}
    {{ name = libpipewire-module-session-manager }}
]
"
            )
        }

        fn client_config() -> &'static str {
            r"context.properties = {}
context.spa-libs = {
    audio.convert.* = audioconvert/libspa-audioconvert
    audio.adapt = audioconvert/libspa-audioconvert
    support.* = support/libspa-support
}
context.modules = [
    { name = libpipewire-module-protocol-native }
    { name = libpipewire-module-client-node }
    { name = libpipewire-module-adapter }
    { name = libpipewire-module-metadata }
]
"
        }

        fn peer_properties(name: &'static str) -> pw::properties::PropertiesBox {
            pw::properties::properties! {
                *pw::keys::NODE_NAME => name,
                *pw::keys::MEDIA_TYPE => "Audio",
                *pw::keys::MEDIA_CATEGORY => "Test",
                *pw::keys::MEDIA_ROLE => "Test",
                *pw::keys::NODE_GROUP => "librewave.integration.peers",
                "node.sync-group" => "librewave.graph",
                *pw::keys::NODE_DRIVER => "false",
                "node.want-driver" => "true",
                *pw::keys::NODE_ALWAYS_PROCESS => "true",
                *pw::keys::NODE_PAUSE_ON_IDLE => "false",
                "node.suspend-on-idle" => "false"
            }
        }

        fn run_cycles<P: GraphProcessor>(graph: &mut ActiveGraph<P>, count: u64) {
            let target = graph.diagnostics().process_cycles + count;
            let deadline = Instant::now() + Duration::from_secs(3);
            while graph.diagnostics().process_cycles < target && !graph.diagnostics().failed {
                assert!(Instant::now() < deadline, "PipeWire graph did not process {count} cycles");
                graph.iterate(Duration::from_millis(10));
            }
        }

        struct SystemLevels(Arc<[std::sync::atomic::AtomicU32; 2]>);

        impl GraphProcessor for SystemLevels {
            fn process(&mut self, block: ProcessBlock<'_>) -> Result<(), ProcessFault> {
                for (peak, samples) in self.0.iter().zip([block.system.left, block.system.right]) {
                    let value =
                        samples.iter().fold(0.0_f32, |value, sample| value.max(sample.abs()));
                    peak.store(value.to_bits(), Ordering::Relaxed);
                }
                block.microphone.left.fill(0.0);
                block.microphone.right.fill(0.0);
                block.monitor.left.fill(0.0);
                block.monitor.right.fill(0.0);
                block.stream.left.fill(0.0);
                block.stream.right.fill(0.0);
                Ok(())
            }
        }

        #[test]
        #[ignore = "requires LIBREWAVE_PIPEWIRE_INTEGRATION=1; uses a private policy session without USB, ALSA, or desktop services"]
        fn private_desktop_defaults_wait_for_the_policy_session() {
            use crate::desktop_defaults::{DesktopDefaultStore, PipeWireDesktopDefaults};
            assert_eq!(std::env::var("LIBREWAVE_PIPEWIRE_INTEGRATION").as_deref(), Ok("1"));
            let server = PrivatePipeWire::start(256, 8);
            std::thread::scope(|scope| {
                let policy = scope.spawn(|| {
                    std::thread::sleep(Duration::from_millis(150));
                    server.start_wireplumber()
                });
                let mut store = PipeWireDesktopDefaults;
                let defaults = store.read().expect("wait for policy metadata publication");
                assert_eq!(defaults, librewave_core::DesktopDefaults::default());
                let _policy = policy.join().expect("policy helper completed");
                assert!(store.read().is_ok());
            });
        }

        #[test]
        #[ignore = "requires LIBREWAVE_PIPEWIRE_INTEGRATION=1; delays only a private WirePlumber process without USB or ALSA"]
        fn private_desktop_defaults_wait_for_policy_acknowledgement() {
            use crate::desktop_defaults::{DesktopDefaultStore, PipeWireDesktopDefaults};
            assert_eq!(std::env::var("LIBREWAVE_PIPEWIRE_INTEGRATION").as_deref(), Ok("1"));
            let server = PrivatePipeWire::start(256, 11);
            let policy = server.start_wireplumber();
            let mut store = PipeWireDesktopDefaults;
            let original = store.read().expect("private policy defaults");
            for selecting in [true, false] {
                let pid = policy.0.id().to_string();
                assert!(
                    Command::new("kill")
                        .args(["-STOP", &pid])
                        .status()
                        .expect("pause private policy")
                        .success()
                );
                std::thread::scope(|scope| {
                    scope.spawn(|| {
                        std::thread::sleep(Duration::from_millis(150));
                        assert!(
                            Command::new("kill")
                                .args(["-CONT", &pid])
                                .status()
                                .expect("resume private policy")
                                .success()
                        );
                    });
                    if selecting {
                        let selected = store.select().expect("wait for selection acknowledgement");
                        assert_eq!(selected.configured_sink.as_deref(), Some("librewave.system"));
                        assert_eq!(
                            selected.configured_source.as_deref(),
                            Some("librewave.microphone")
                        );
                    } else {
                        let restored =
                            store.restore(&original).expect("wait for restoration acknowledgement");
                        assert_eq!(restored.configured_sink, original.configured_sink);
                        assert_eq!(restored.configured_source, original.configured_source);
                    }
                });
            }
        }

        #[test]
        #[ignore = "requires LIBREWAVE_PIPEWIRE_INTEGRATION=1; uses a private metadata server without USB, ALSA, or desktop services"]
        fn private_desktop_defaults_are_selected_and_restored() {
            use crate::desktop_defaults::{DesktopDefaultStore, PipeWireDesktopDefaults};
            assert_eq!(std::env::var("LIBREWAVE_PIPEWIRE_INTEGRATION").as_deref(), Ok("1"));
            let server = PrivatePipeWire::start_with_metadata(256, 7, true);
            let mut store = PipeWireDesktopDefaults;
            let original = store.read().expect("read automatic defaults");
            assert_eq!(original.configured_sink, None);
            assert_eq!(original.configured_source, None);
            let selected = store.select().expect("select software endpoints");
            assert_eq!(selected.configured_sink.as_deref(), Some("librewave.system"));
            assert_eq!(selected.configured_source.as_deref(), Some("librewave.microphone"));
            let status = server
                .command("pw-metadata")
                .args([
                    "-n",
                    "default",
                    "0",
                    "default.configured.audio.source",
                    r#"{"name":"new-user-choice"}"#,
                    "Spa:String:JSON",
                ])
                .stdout(Stdio::null())
                .status()
                .expect("private user choice");
            assert!(status.success());
            let restored = store.restore(&original).expect("restore automatic selection");
            assert_eq!(restored.configured_sink, None);
            assert_eq!(restored.configured_source.as_deref(), Some("new-user-choice"));
            assert_eq!(store.restore(&original).expect("repeat restoration"), restored);
            let recovered = store.recover().expect("recover missing defaults");
            assert_eq!(recovered.configured_sink.as_deref(), Some("librewave.system"));
            assert_eq!(recovered.configured_source.as_deref(), Some("new-user-choice"));
            assert_eq!(store.restore(&original).expect("restore recovered defaults"), restored);
            assert!(
                crate::desktop_defaults::audio_endpoints()
                    .expect("inspect private nodes")
                    .is_empty()
            );
        }

        #[test]
        #[ignore = "requires LIBREWAVE_PIPEWIRE_INTEGRATION=1 and PipeWire runtime tools"]
        fn private_system_volume_and_mute_reach_mixer() {
            assert_eq!(std::env::var("LIBREWAVE_PIPEWIRE_INTEGRATION").as_deref(), Ok("1"));
            let server = PrivatePipeWire::start(256, 10);
            let plans = endpoint_plans(DELIBERATE_ENDPOINTS).expect("deliberate endpoints");
            let peaks = Arc::new(std::array::from_fn(|_| std::sync::atomic::AtomicU32::new(0)));
            let quantum = NonZeroU32::new(256).expect("fixed quantum");
            let mut graph = InactiveGraph::construct(
                REMOTE,
                &plans,
                quantum,
                AdapterPortMode::SelfConfigured,
                SystemLevels(Arc::clone(&peaks)),
            )
            .expect("construct volume graph")
            .activate()
            .expect("activate volume graph");
            let system = graph.node_ids().expect("graph ids")[0];
            let mut producer = TestPeer::new(
                &graph.inner.core,
                "Volume producer",
                peer_properties("librewave.integration.producer"),
                TestPeerMode::Producer,
                system,
                quantum,
            )
            .expect("create volume producer");
            producer.add_port(0, true, "test_system_FL", "FL").expect("producer FL");
            producer.add_port(1, true, "test_system_FR", "FR").expect("producer FR");
            producer.connect_inactive().expect("connect producer");
            roundtrip(&graph.inner.main_loop, &graph.inner.core).expect("observe producer");
            roundtrip(&graph.inner.main_loop, &graph.inner.core).expect("observe producer ports");
            let links: Vec<_> = ["FL", "FR"]
                .into_iter()
                .map(|channel| {
                    let state = graph.inner.registry_state.borrow();
                    create_link(
                        &graph.inner.core,
                        state
                            .port_id(producer.node_id(), "out", &format!("test_system_{channel}"))
                            .expect("producer port"),
                        state
                            .port_id(system, "in", &format!("playback_{channel}"))
                            .expect("System port"),
                    )
                    .expect("link volume producer")
                })
                .collect();
            producer.set_active(true).expect("activate producer");
            for (props, bounds) in [
                ("{ channelVolumes = [ 1.0 1.0 ] mute = false }", [(0.25, 0.5), (0.375, 0.625)]),
                (
                    "{ channelVolumes = [ 0.25 0.5 ] mute = false }",
                    [(0.0625, 0.125), (0.1875, 0.3125)],
                ),
                ("{ mute = true }", [(0.0, 0.0), (0.0, 0.0)]),
                ("{ mute = false }", [(0.0625, 0.125), (0.1875, 0.3125)]),
            ] {
                let status = server
                    .command("pw-cli")
                    .args(["set-param", &system.to_string(), "Props", props])
                    .output()
                    .expect("set System software controls");
                assert!(status.status.success(), "software control rejected: {status:?}");
                run_cycles(&mut graph, 32);
                assert!(!graph.diagnostics().failed, "volume change faulted graph");
                for (peak, (minimum, maximum)) in peaks.iter().zip(bounds) {
                    let observed = f32::from_bits(peak.load(Ordering::Relaxed));
                    assert!(
                        (minimum..=maximum).contains(&observed),
                        "{props}: expected {minimum}..={maximum}, observed {observed}"
                    );
                }
            }
            drop(links);
            producer.destroy();
            graph.teardown().expect("tear down volume graph");
        }

        fn assert_driver_stable<P: GraphProcessor>(
            graph: &ActiveGraph<P>,
            plans: &[EndpointPlan; 4],
        ) {
            let ids = graph.node_ids().expect("stable graph node ids");
            validate_drivers(
                &graph.inner.main_loop,
                &graph.inner.core,
                &graph.inner.registry,
                &graph.inner.registry_state,
                &ids,
                plans,
            )
            .expect("System remains the driver for all five nodes");
        }

        fn create_peer_links<P: GraphProcessor>(
            graph: &mut ActiveGraph<P>,
            producer: &TestPeer,
            consumer: &TestPeer,
        ) -> Vec<pw::link::Link> {
            roundtrip(&graph.inner.main_loop, &graph.inner.core).expect("observe test peer ports");
            roundtrip(&graph.inner.main_loop, &graph.inner.core).expect("observe test peer ports");
            let [system, microphone, monitor, stream, _filter] =
                graph.node_ids().expect("integration graph node ids");
            let state = graph.inner.registry_state.borrow();
            let links = [
                (
                    state.port_id(producer.node_id(), "out", "test_system_FL"),
                    state.port_id(system, "in", "playback_FL"),
                ),
                (
                    state.port_id(producer.node_id(), "out", "test_system_FR"),
                    state.port_id(system, "in", "playback_FR"),
                ),
                (
                    state.port_id(microphone, "out", "capture_FL"),
                    state.port_id(consumer.node_id(), "in", "test_microphone_FL"),
                ),
                (
                    state.port_id(microphone, "out", "capture_FR"),
                    state.port_id(consumer.node_id(), "in", "test_microphone_FR"),
                ),
                (
                    state.port_id(monitor, "out", "capture_FL"),
                    state.port_id(consumer.node_id(), "in", "test_monitor_FL"),
                ),
                (
                    state.port_id(monitor, "out", "capture_FR"),
                    state.port_id(consumer.node_id(), "in", "test_monitor_FR"),
                ),
                (
                    state.port_id(stream, "out", "capture_FL"),
                    state.port_id(consumer.node_id(), "in", "test_stream_FL"),
                ),
                (
                    state.port_id(stream, "out", "capture_FR"),
                    state.port_id(consumer.node_id(), "in", "test_stream_FR"),
                ),
            ];
            drop(state);
            links
                .into_iter()
                .map(|(output, input)| {
                    create_link(
                        &graph.inner.core,
                        output.expect("peer output port"),
                        input.expect("peer input port"),
                    )
                    .expect("create peer link")
                })
                .collect()
        }

        #[allow(clippy::too_many_lines)]
        fn run_fixed_quantum(quantum: u32, cycle: u32) {
            let server = PrivatePipeWire::start(quantum, cycle);
            let plans = endpoint_plans(DELIBERATE_ENDPOINTS).expect("deliberate endpoints");
            let proof = Arc::new(ProcessorProof::default());
            let processor = EngineProcessor::new(1024, Arc::clone(&proof));
            let quantum = NonZeroU32::new(quantum).expect("fixed quantum");
            let mut graph = InactiveGraph::construct(
                REMOTE,
                &plans,
                quantum,
                AdapterPortMode::SelfConfigured,
                processor,
            )
            .expect("construct inactive PipeWire graph")
            .activate()
            .expect("activate PipeWire graph once");
            run_cycles(&mut graph, 8);
            assert!(!graph.diagnostics().failed, "idle graph faulted");
            assert_eq!(graph.diagnostics().dropped_faults, 0);
            assert!(proof.idle.load(Ordering::Relaxed) >= 8);
            let plans: [EndpointPlan; 4] = plans.try_into().expect("four endpoint plans");
            assert_driver_stable(&graph, &plans);
            {
                let ids = graph.node_ids().expect("endpoint ids");
                let state = graph.inner.registry_state.borrow();
                assert_eq!(state.node_classes.get(&ids[0]).map(String::as_str), Some("Audio/Sink"));
                for id in &ids[1..4] {
                    assert_eq!(
                        state.node_classes.get(id).map(String::as_str),
                        Some("Audio/Source/Virtual")
                    );
                }
                assert!(!state.node_classes.contains_key(&ids[4]));
            }

            let driver = graph.node_ids().expect("graph ids")[0];
            let mut producer = TestPeer::new(
                &graph.inner.core,
                "Integration producer",
                peer_properties("librewave.integration.producer"),
                TestPeerMode::Producer,
                driver,
                quantum,
            )
            .expect("create producer");
            producer.add_port(0, true, "test_system_FL", "FL").expect("producer FL");
            producer.add_port(1, true, "test_system_FR", "FR").expect("producer FR");
            producer.connect_inactive().expect("connect producer");
            let mut consumer = TestPeer::new(
                &graph.inner.core,
                "Integration consumer",
                peer_properties("librewave.integration.consumer"),
                TestPeerMode::Consumer,
                driver,
                quantum,
            )
            .expect("create consumer");
            for (index, name, channel) in [
                (0, "test_microphone_FL", "FL"),
                (1, "test_microphone_FR", "FR"),
                (2, "test_monitor_FL", "FL"),
                (3, "test_monitor_FR", "FR"),
                (4, "test_stream_FL", "FL"),
                (5, "test_stream_FR", "FR"),
            ] {
                consumer.add_port(index, false, name, channel).expect("consumer port");
            }
            consumer.connect_inactive().expect("connect consumer");
            roundtrip(&graph.inner.main_loop, &graph.inner.core).expect("bind test peers");
            let mut peer_links = create_peer_links(&mut graph, &producer, &consumer);
            roundtrip(&graph.inner.main_loop, &graph.inner.core).expect("create peer links");
            producer.set_active(true).expect("activate producer");
            let producer_deadline = graph.diagnostics().process_cycles + 64;
            let startup_deadline = Instant::now() + Duration::from_secs(3);
            while proof.aligned_system.load(Ordering::Relaxed) == 0
                && graph.diagnostics().process_cycles < producer_deadline
                && !graph.diagnostics().failed
                && Instant::now() < startup_deadline
            {
                graph.iterate(Duration::from_millis(10));
            }
            assert!(
                !graph.diagnostics().failed && proof.aligned_system.load(Ordering::Relaxed) > 0,
                "producer did not reach steady graph input: {:?}; producer {:?}",
                graph.diagnostics().faults,
                producer.diagnostics(),
            );
            consumer.set_active(true).expect("activate consumer");
            roundtrip(&graph.inner.main_loop, &graph.inner.core).expect("activate consumer links");
            run_cycles(&mut graph, 24);
            assert!(
                !graph.diagnostics().failed,
                "attached graph faulted: {:?}",
                graph.diagnostics().faults
            );
            let producer_diagnostics = producer.diagnostics();
            let consumer_diagnostics = consumer.diagnostics();
            assert!(producer_diagnostics.cycles > 0);
            assert!(consumer_diagnostics.cycles > 0);
            assert!(
                producer_diagnostics.startup_missing < 64,
                "producer ports did not map promptly: {producer_diagnostics:?}"
            );
            assert!(
                consumer_diagnostics.startup_missing < 64,
                "consumer ports did not map promptly: {consumer_diagnostics:?}"
            );
            assert_eq!(producer_diagnostics.missing, 0, "{producer_diagnostics:?}");
            assert_eq!(consumer_diagnostics.missing, 0, "{consumer_diagnostics:?}");
            assert_eq!(consumer_diagnostics.timing_errors, 0);
            assert_eq!(consumer_diagnostics.vector_errors, 0);
            assert_eq!(consumer_diagnostics.route_errors, 0);
            assert!(!consumer_diagnostics.panic);
            assert_eq!(consumer_diagnostics.output_latency_min, 0);
            assert_eq!(consumer_diagnostics.output_latency_max, 0);
            assert!(
                consumer_diagnostics.system_latency_min <= consumer_diagnostics.system_latency_max
            );
            assert!(consumer_diagnostics.system_latency_max <= 2 * quantum.get());
            assert!(consumer_diagnostics.active_system_cycles > 0);
            assert!(proof.aligned_system.load(Ordering::Relaxed) > 0);
            assert_eq!(
                consumer_diagnostics.silent_system_cycles
                    + consumer_diagnostics.active_system_cycles,
                consumer_diagnostics.cycles
            );
            assert_driver_stable(&graph, &plans);

            for link in peer_links.drain(2..) {
                drop(link);
            }
            consumer.set_active(false).expect("deactivate consumer");
            roundtrip(&graph.inner.main_loop, &graph.inner.core).expect("detach consumer");
            run_cycles(&mut graph, 6);
            assert!(!graph.diagnostics().failed, "consumer detach faulted graph");

            for link in peer_links.drain(..) {
                drop(link);
            }
            roundtrip(&graph.inner.main_loop, &graph.inner.core).expect("detach producer");
            run_cycles(&mut graph, 6);
            producer.set_active(false).expect("deactivate producer");
            assert!(
                !graph.diagnostics().failed,
                "producer detach faulted graph: {:?}",
                graph.diagnostics().faults
            );
            assert_eq!(proof.priming.load(Ordering::Relaxed), 1);
            assert_eq!(proof.draining.load(Ordering::Relaxed), 1);
            assert_driver_stable(&graph, &plans);

            consumer.destroy();
            producer.destroy();
            roundtrip(&graph.inner.main_loop, &graph.inner.core).expect("remove test peers");
            graph.teardown().expect("tear down integration graph");
            graph.teardown().expect("repeat successful graph teardown");
            let nodes = server
                .command("pw-cli")
                .args(["-r", REMOTE, "ls", "Node"])
                .output()
                .expect("list residual nodes");
            assert!(nodes.status.success(), "pw-cli node listing failed");
            assert!(
                !String::from_utf8_lossy(&nodes.stdout).contains("librewave."),
                "LibreWave node survived teardown"
            );
            let links = server
                .command("pw-cli")
                .args(["-r", REMOTE, "ls", "Link"])
                .output()
                .expect("list residual links");
            assert!(links.status.success(), "pw-cli link listing failed");
            assert!(
                !String::from_utf8_lossy(&links.stdout).contains("PipeWire:Interface:Link"),
                "link survived teardown"
            );
        }

        fn run_forced_quantum_fault(cycle: u32) {
            let server = PrivatePipeWire::start(128, cycle);
            let plans = endpoint_plans(DELIBERATE_ENDPOINTS).expect("deliberate endpoints");
            let proof = Arc::new(ProcessorProof::default());
            let processor = EngineProcessor::new(1024, proof);
            let quantum = NonZeroU32::new(128).expect("fixed quantum");
            let mut graph = InactiveGraph::construct(
                REMOTE,
                &plans,
                quantum,
                AdapterPortMode::SelfConfigured,
                processor,
            )
            .expect("construct forced-change graph")
            .activate()
            .expect("activate forced-change graph");
            run_cycles(&mut graph, 8);
            let status = server
                .command("pw-metadata")
                .args(["--remote", REMOTE, "--name", "settings", "0", "clock.force-quantum", "64"])
                .status()
                .expect("request forced quantum change");
            assert!(status.success(), "pw-metadata rejected forced quantum change");
            let deadline = Instant::now() + Duration::from_secs(3);
            while !graph.diagnostics().failed {
                assert!(
                    Instant::now() < deadline,
                    "forced quantum change did not fault the attempt"
                );
                graph.iterate(Duration::from_millis(10));
            }
            let diagnostics = graph.diagnostics();
            assert_eq!(
                diagnostics.faults[0].map(|record| record.fault),
                Some(ProcessFault::Pause),
                "the observed forced-change pause must fault before invalid delivery"
            );
            graph.teardown().expect("tear down forced-change graph");
            graph.teardown().expect("repeat successful forced-change teardown");
            let nodes = server
                .command("pw-cli")
                .args(["-r", REMOTE, "ls", "Node"])
                .output()
                .expect("list nodes after forced fault");
            assert!(!String::from_utf8_lossy(&nodes.stdout).contains("librewave."));
        }

        #[test]
        #[ignore = "requires LIBREWAVE_PIPEWIRE_INTEGRATION=1 and PipeWire runtime tools"]
        fn private_pipewire_graph() {
            assert_eq!(
                std::env::var("LIBREWAVE_PIPEWIRE_INTEGRATION").as_deref(),
                Ok("1"),
                "set LIBREWAVE_PIPEWIRE_INTEGRATION=1 to run the explicit private-daemon proof"
            );
            for (cycle, quantum) in [(0, 64), (1, 128), (2, 256), (3, 512)] {
                run_fixed_quantum(quantum, cycle);
            }
            run_forced_quantum_fault(4);
        }

        #[test]
        #[ignore = "requires LIBREWAVE_PIPEWIRE_INTEGRATION=1; stops only the private PipeWire server without USB or ALSA"]
        fn private_graph_teardown_after_server_disconnect() {
            assert_eq!(std::env::var("LIBREWAVE_PIPEWIRE_INTEGRATION").as_deref(), Ok("1"));
            for dispatch_first in [true, false] {
                let mut server = PrivatePipeWire::start(256, if dispatch_first { 12 } else { 13 });
                let plans = endpoint_plans(DELIBERATE_ENDPOINTS).expect("endpoints");
                let processor = EngineProcessor::new(1024, Arc::new(ProcessorProof::default()));
                let mut graph = InactiveGraph::construct(
                    REMOTE,
                    &plans,
                    NonZeroU32::new(256).expect("quantum"),
                    AdapterPortMode::SelfConfigured,
                    processor,
                )
                .expect("construct private graph")
                .activate()
                .expect("activate private graph");
                run_cycles(&mut graph, 64);
                server.child.kill().expect("stop only private server");
                server.child.wait().expect("private server exited");
                if dispatch_first {
                    graph.iterate(Duration::from_millis(100));
                }
                graph.teardown().expect("release local graph after peer disconnected");
                assert_eq!(graph.inner.filter.callbacks_in_flight(), 0);
                assert_eq!(graph.inner.filter.node_id(), None);
                graph.teardown().expect("repeat disconnected teardown");
            }
        }

        #[test]
        #[ignore = "requires LIBREWAVE_PIPEWIRE_INTEGRATION=1; pauses only the private PipeWire server without USB or ALSA"]
        fn private_graph_teardown_keeps_unconfirmed_timeout_terminal() {
            assert_eq!(std::env::var("LIBREWAVE_PIPEWIRE_INTEGRATION").as_deref(), Ok("1"));
            let server = PrivatePipeWire::start(256, 14);
            let plans = endpoint_plans(DELIBERATE_ENDPOINTS).expect("endpoints");
            let processor = EngineProcessor::new(1024, Arc::new(ProcessorProof::default()));
            let mut graph = InactiveGraph::construct(
                REMOTE,
                &plans,
                NonZeroU32::new(256).expect("quantum"),
                AdapterPortMode::SelfConfigured,
                processor,
            )
            .expect("construct private graph")
            .activate()
            .expect("activate private graph");
            run_cycles(&mut graph, 64);
            let pid = server.child.id().to_string();
            assert!(
                Command::new("kill")
                    .args(["-STOP", &pid])
                    .status()
                    .expect("pause private server")
                    .success()
            );
            let result = graph.teardown();
            assert!(
                Command::new("kill")
                    .args(["-CONT", &pid])
                    .status()
                    .expect("resume private server")
                    .success()
            );
            assert_eq!(result, Err(GraphError::RoundtripTimeout));
            assert!(!graph.inner.peer_disconnected.get());
            assert_eq!(graph.teardown(), result);
        }

        #[test]
        #[ignore = "requires LIBREWAVE_PIPEWIRE_INTEGRATION=1; uses private policy and software clients without USB or ALSA"]
        fn private_wireplumber_starts_with_waiting_clients() {
            assert_eq!(std::env::var("LIBREWAVE_PIPEWIRE_INTEGRATION").as_deref(), Ok("1"));
            require_program("pw-play");
            require_program("pw-record");
            for (cycle, rate) in [(9, "48000"), (10, "44100")] {
                let server = PrivatePipeWire::start(256, cycle);
                let policy = server.start_wireplumber();
                // These clients exist before any LibreWave endpoint. Policy
                // must link them as the graph is published and activated.
                let player = TestChild(
                    server
                        .command("pw-play")
                        .args([
                            "--target",
                            "librewave.system",
                            "--raw",
                            "--rate",
                            rate,
                            "--channels",
                            "2",
                            "--format",
                            "s16",
                            "/dev/zero",
                        ])
                        .stdout(Stdio::null())
                        .stderr(Stdio::null())
                        .spawn()
                        .expect("waiting private player"),
                );
                let recorder = TestChild(
                    server
                        .command("pw-record")
                        .args([
                            "--target",
                            "librewave.microphone",
                            "--raw",
                            "--rate",
                            rate,
                            "--channels",
                            "2",
                            "--format",
                            "s16",
                            "/dev/null",
                        ])
                        .stdout(Stdio::null())
                        .stderr(Stdio::null())
                        .spawn()
                        .expect("waiting private recorder"),
                );
                let plans = endpoint_plans(DELIBERATE_ENDPOINTS).expect("endpoints");
                let processor = EngineProcessor::new(1024, Arc::new(ProcessorProof::default()));
                let mut graph = InactiveGraph::construct(
                    REMOTE,
                    &plans,
                    NonZeroU32::new(256).expect("quantum"),
                    AdapterPortMode::WirePlumber,
                    processor,
                )
                .expect("construct graph with waiting clients")
                .activate()
                .expect("activate graph with waiting clients");
                run_cycles(&mut graph, 128);
                let links =
                    server.command("pw-link").arg("-l").output().expect("private startup links");
                assert!(links.status.success());
                let links = String::from_utf8(links.stdout).expect("UTF-8 links");
                for expected in [
                    "pw-play:output_FL\n  |-> librewave.system:playback_FL",
                    "pw-play:output_FR\n  |-> librewave.system:playback_FR",
                    "librewave.microphone:capture_FL\n  |-> pw-record:input_FL",
                    "librewave.microphone:capture_FR\n  |-> pw-record:input_FR",
                ] {
                    assert!(
                        links.contains(expected),
                        "waiting client did not connect at {rate} Hz: {links}"
                    );
                }
                assert!(!graph.diagnostics().failed);
                assert!(graph.diagnostics().process_cycles >= 128);
                assert_eq!(graph.diagnostics().system_link_word & 1, 1);
                drop(player);
                drop(recorder);
                graph.teardown().expect("startup cleanup");
                assert!(
                    crate::desktop_defaults::audio_endpoints()
                        .expect("private cleanup nodes")
                        .is_empty()
                );
                drop(graph);
                drop(policy);
            }
        }

        #[test]
        #[ignore = "requires LIBREWAVE_PIPEWIRE_INTEGRATION=1 and private PipeWire/WirePlumber runtime tools"]
        #[allow(
            clippy::too_many_lines,
            reason = "the isolated runtime and both client links are one regression"
        )]
        fn private_wireplumber_routes_a_named_player_to_system() {
            assert_eq!(
                std::env::var("LIBREWAVE_PIPEWIRE_INTEGRATION").as_deref(),
                Ok("1"),
                "set LIBREWAVE_PIPEWIRE_INTEGRATION=1 for the private routing proof"
            );
            require_program("pw-play");
            require_program("pw-record");
            require_program("pw-link");
            let server = PrivatePipeWire::start(256, 5);
            let mut wireplumber = server.start_wireplumber();
            let plans = endpoint_plans(DELIBERATE_ENDPOINTS).expect("deliberate endpoints");
            let processor = EngineProcessor::new(1024, Arc::new(ProcessorProof::default()));
            let quantum = NonZeroU32::new(256).expect("fixed quantum");
            let mut graph = InactiveGraph::construct(
                REMOTE,
                &plans,
                quantum,
                AdapterPortMode::WirePlumber,
                processor,
            )
            .expect("construct private graph")
            .activate()
            .expect("activate private graph");
            let mut player = TestChild(
                server
                    .command("pw-play")
                    .args([
                        "--target",
                        "librewave.system",
                        "--raw",
                        "--rate",
                        "48000",
                        "--channels",
                        "2",
                        "--format",
                        "s16",
                        "/dev/zero",
                    ])
                    .stdout(Stdio::null())
                    .stderr(Stdio::from(
                        fs::File::create(server.root.join("pw-play.log"))
                            .expect("create private player log"),
                    ))
                    .spawn()
                    .expect("start silent targeted player"),
            );
            let mut recorder = TestChild(
                server
                    .command("pw-record")
                    .args([
                        "--target",
                        "librewave.monitor-mix",
                        "--raw",
                        "--rate",
                        "48000",
                        "--channels",
                        "2",
                        "--format",
                        "s16",
                        "/dev/null",
                    ])
                    .stdout(Stdio::null())
                    .stderr(Stdio::from(
                        fs::File::create(server.root.join("pw-record.log"))
                            .expect("create private recorder log"),
                    ))
                    .spawn()
                    .expect("start targeted recorder"),
            );
            let deadline = Instant::now() + Duration::from_secs(3);
            let mut links = String::new();
            while Instant::now() < deadline && !graph.diagnostics().failed {
                graph.iterate(Duration::from_millis(10));
                let output =
                    server.command("pw-link").arg("-l").output().expect("inspect private links");
                assert!(output.status.success(), "pw-link failed");
                links = String::from_utf8_lossy(&output.stdout).into_owned();
                if links.contains("pw-play:output_FL\n  |-> librewave.system:playback_FL")
                    && links.contains("pw-play:output_FR\n  |-> librewave.system:playback_FR")
                    && links.contains("librewave.monitor-mix:capture_FL\n  |-> pw-record:input_FL")
                    && links.contains("librewave.monitor-mix:capture_FR\n  |-> pw-record:input_FR")
                {
                    break;
                }
            }
            let player_status = player.0.try_wait().expect("inspect private player");
            let recorder_status = recorder.0.try_wait().expect("inspect private recorder");
            let wireplumber_status = wireplumber.0.try_wait().expect("inspect private WirePlumber");
            let player_log = fs::read_to_string(server.root.join("pw-play.log"))
                .expect("read private player log");
            let wireplumber_log = fs::read_to_string(server.root.join("wireplumber.log"))
                .expect("read private WirePlumber log");
            let recorder_log = fs::read_to_string(server.root.join("pw-record.log"))
                .expect("read private recorder log");
            drop(player);
            drop(recorder);
            assert!(
                links.contains("pw-play:output_FL\n  |-> librewave.system:playback_FL")
                    && links.contains("pw-play:output_FR\n  |-> librewave.system:playback_FR")
                    && links.contains("librewave.monitor-mix:capture_FL\n  |-> pw-record:input_FL")
                    && links.contains("librewave.monitor-mix:capture_FR\n  |-> pw-record:input_FR"),
                "named streams did not link to LibreWave endpoints: graph_failed={} graph_faults={:?} player={player_status:?} player_log={player_log} recorder={recorder_status:?} recorder_log={recorder_log} wireplumber={wireplumber_status:?} wireplumber_log={wireplumber_log} links={links}",
                graph.diagnostics().failed,
                graph.diagnostics().faults,
            );
            assert!(!graph.diagnostics().failed, "private graph faulted");
            graph.teardown().expect("tear down private graph");
            drop(wireplumber);
        }

        #[test]
        #[ignore = "requires LIBREWAVE_DESKTOP_PIPEWIRE_INTEGRATION=1 and the desktop PipeWire session"]
        fn desktop_pipewire_keeps_all_filter_buffers_mapped() {
            assert_eq!(
                std::env::var("LIBREWAVE_DESKTOP_PIPEWIRE_INTEGRATION").as_deref(),
                Ok("1"),
                "set LIBREWAVE_DESKTOP_PIPEWIRE_INTEGRATION=1 for the desktop graph proof",
            );
            let plans = endpoint_plans(DELIBERATE_ENDPOINTS).expect("deliberate endpoints");
            let proof = Arc::new(ProcessorProof::default());
            let processor = EngineProcessor::new(1024, proof);
            let quantum = NonZeroU32::new(256).expect("fixed quantum");
            let mut graph = InactiveGraph::construct(
                "pipewire-0",
                &plans,
                quantum,
                AdapterPortMode::WirePlumber,
                processor,
            )
            .expect("construct desktop graph")
            .activate()
            .expect("activate desktop graph");
            let deadline = Instant::now() + Duration::from_millis(750);
            while Instant::now() < deadline && !graph.diagnostics().failed {
                graph.iterate(Duration::from_millis(10));
            }
            let diagnostics = graph.diagnostics();
            assert!(
                !diagnostics.failed && diagnostics.process_cycles >= 64,
                "desktop graph: cycles={} incomplete={} mask={:#x} quantum={} driver={} faults={:?}",
                diagnostics.process_cycles,
                diagnostics.startup_incomplete_cycles,
                diagnostics.startup_missing_mask,
                diagnostics.startup_quantum,
                diagnostics.startup_driver,
                diagnostics.faults,
            );
            let status = Command::new("wpctl")
                .args(["status", "--name"])
                .output()
                .expect("inspect desktop endpoints");
            assert!(status.status.success(), "wpctl status failed");
            let visible = String::from_utf8_lossy(&status.stdout);
            for plan in &plans {
                assert!(
                    visible.contains(plan.node_name),
                    "{} is not in wpctl status",
                    plan.node_name
                );
            }
            assert!(!visible.contains(FILTER_NODE_NAME), "internal mixer appears in wpctl status");
            let microphone = graph.node_ids().expect("graph node ids")[1].to_string();
            let original = Command::new("wpctl")
                .args(["get-volume", &microphone])
                .output()
                .expect("read original source volume");
            assert!(original.status.success());
            let original = String::from_utf8_lossy(&original.stdout);
            let original = original.split_whitespace().nth(1).expect("source volume value");
            let volume = Command::new("wpctl")
                .args(["set-volume", &microphone, "0.5"])
                .status()
                .expect("set temporary source volume");
            assert!(volume.success(), "wpctl could not set source volume");
            let volume = Command::new("wpctl")
                .args(["get-volume", &microphone])
                .output()
                .expect("read temporary source volume");
            assert!(volume.status.success());
            assert!(
                String::from_utf8_lossy(&volume.stdout).contains("0.50"),
                "temporary source volume did not read back",
            );
            let restored = Command::new("wpctl")
                .args(["set-volume", &microphone, original])
                .status()
                .expect("restore source volume");
            assert!(restored.success(), "wpctl could not restore source volume");
            graph.teardown().expect("tear down desktop graph");
        }

        #[test]
        #[ignore = "requires LIBREWAVE_DESKTOP_PIPEWIRE_INTEGRATION=1 and the desktop PipeWire session"]
        fn desktop_pipewire_routes_named_streams_without_manual_links() {
            assert_eq!(
                std::env::var("LIBREWAVE_DESKTOP_PIPEWIRE_INTEGRATION").as_deref(),
                Ok("1"),
                "set LIBREWAVE_DESKTOP_PIPEWIRE_INTEGRATION=1 for the desktop routing proof",
            );
            require_program("pw-play");
            require_program("pw-record");
            require_program("pw-link");
            let plans = endpoint_plans(DELIBERATE_ENDPOINTS).expect("deliberate endpoints");
            let processor = EngineProcessor::new(1024, Arc::new(ProcessorProof::default()));
            let quantum = NonZeroU32::new(256).expect("fixed quantum");
            let mut graph = InactiveGraph::construct(
                "pipewire-0",
                &plans,
                quantum,
                AdapterPortMode::WirePlumber,
                processor,
            )
            .expect("construct desktop graph")
            .activate()
            .expect("activate desktop graph");
            let player = TestChild(
                Command::new("pw-play")
                    .args([
                        "--target",
                        "librewave.system",
                        "--raw",
                        "--rate",
                        "48000",
                        "--channels",
                        "2",
                        "--format",
                        "s16",
                        "/dev/zero",
                    ])
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .spawn()
                    .expect("start silent desktop player"),
            );
            let recorder = TestChild(
                Command::new("pw-record")
                    .args([
                        "--target",
                        "librewave.monitor-mix",
                        "--raw",
                        "--rate",
                        "48000",
                        "--channels",
                        "2",
                        "--format",
                        "s16",
                        "/dev/null",
                    ])
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .spawn()
                    .expect("start desktop recorder"),
            );
            let deadline = Instant::now() + Duration::from_secs(3);
            let mut links = String::new();
            while Instant::now() < deadline && !graph.diagnostics().failed {
                graph.iterate(Duration::from_millis(10));
                let output = Command::new("pw-link").arg("-l").output().expect("inspect links");
                assert!(output.status.success(), "pw-link failed");
                links = String::from_utf8_lossy(&output.stdout).into_owned();
                if links.contains("pw-play:output_FL\n  |-> librewave.system:playback_FL")
                    && links.contains("pw-play:output_FR\n  |-> librewave.system:playback_FR")
                    && links.contains("librewave.monitor-mix:capture_FL\n  |-> pw-record:input_FL")
                    && links.contains("librewave.monitor-mix:capture_FR\n  |-> pw-record:input_FR")
                {
                    break;
                }
            }
            drop(recorder);
            drop(player);
            assert!(
                !graph.diagnostics().failed
                    && links.contains("pw-play:output_FL\n  |-> librewave.system:playback_FL")
                    && links.contains("pw-play:output_FR\n  |-> librewave.system:playback_FR")
                    && links.contains("librewave.monitor-mix:capture_FL\n  |-> pw-record:input_FL")
                    && links.contains("librewave.monitor-mix:capture_FR\n  |-> pw-record:input_FR"),
                "desktop streams did not link to LibreWave: graph_failed={} faults={:?} links={links}",
                graph.diagnostics().failed,
                graph.diagnostics().faults,
            );
            graph.teardown().expect("tear down desktop graph");
        }

        #[test]
        #[ignore = "requires LIBREWAVE_PIPEWIRE_INTEGRATION=1 and PipeWire runtime tools"]
        fn private_composed_audio_attempt() {
            assert_eq!(
                std::env::var("LIBREWAVE_PIPEWIRE_INTEGRATION").as_deref(),
                Ok("1"),
                "set LIBREWAVE_PIPEWIRE_INTEGRATION=1 for the private audio attempt",
            );
            let server = PrivatePipeWire::start(256, 5);
            pw::init();
            let main_loop = pw::main_loop::MainLoopRc::new(None).expect("test loop");
            let context = pw::context::ContextRc::new(&main_loop, None).expect("test context");
            let core = context.connect_rc(None).expect("test connection");
            let desktop_driver = core
                .create_object::<pw::node::Node>(
                    "adapter",
                    &pw::properties::properties! {
                        *pw::keys::FACTORY_NAME => "support.null-audio-sink",
                        *pw::keys::NODE_NAME => "integration.desktop-driver",
                        *pw::keys::NODE_DRIVER => "true",
                        *pw::keys::PRIORITY_DRIVER => "200000",
                        *pw::keys::NODE_ALWAYS_PROCESS => "true",
                        *pw::keys::NODE_PAUSE_ON_IDLE => "false",
                        "audio.rate" => "48000",
                        *pw::keys::AUDIO_CHANNELS => "2",
                        "audio.position" => "[ FL FR ]",
                    },
                )
                .expect("create competing private driver");
            roundtrip(&main_loop, &core).expect("publish competing private driver");
            crate::audio_host::attempt::tests::private_composed_attempt(REMOTE);
            drop(desktop_driver);
            roundtrip(&main_loop, &core).expect("remove competing private driver");
            let nodes = server
                .command("pw-cli")
                .args(["-r", REMOTE, "ls", "Node"])
                .output()
                .expect("list nodes after composed teardown");
            assert!(nodes.status.success());
            assert!(!String::from_utf8_lossy(&nodes.stdout).contains("librewave."));
        }

        #[test]
        #[ignore = "requires LIBREWAVE_PIPEWIRE_INTEGRATION=1 and PipeWire runtime tools"]
        fn private_existing_physical_node_blocks_ownership() {
            assert_eq!(
                std::env::var("LIBREWAVE_PIPEWIRE_INTEGRATION").as_deref(),
                Ok("1"),
                "set LIBREWAVE_PIPEWIRE_INTEGRATION=1 for the private ownership proof",
            );
            let _server = PrivatePipeWire::start(256, 6);
            pw::init();
            let main_loop = pw::main_loop::MainLoopRc::new(None).expect("test loop");
            let context = pw::context::ContextRc::new(&main_loop, None).expect("test context");
            let core = context.connect_rc(None).expect("test connection");
            let physical = core
                .create_object::<pw::node::Node>(
                    "adapter",
                    &pw::properties::properties! {
                        *pw::keys::FACTORY_NAME => "support.null-audio-sink",
                        *pw::keys::NODE_NAME => "librewave.test-physical",
                        *pw::keys::MEDIA_CLASS => "Audio/Sink",
                        "api.alsa.path" => "hw:7",
                        *pw::keys::AUDIO_CHANNELS => "2",
                        "audio.position" => "FL,FR",
                    },
                )
                .expect("test physical node");
            roundtrip(&main_loop, &core).expect("publish test node");
            let candidate = crate::UsbDeviceCandidate {
                identity: crate::DeviceIdentity::wave3(),
                topology: crate::UsbTopology::new("1-8.3"),
                alsa_cards: vec![crate::AlsaCardInfo {
                    number: 7,
                    id: Some("Wave3".to_owned()),
                    name: Some("Wave:3".to_owned()),
                }],
            };
            let mut facade = crate::audio_host::pipewire::RealPipeWireFacade::default();
            for _ in 0..32 {
                assert_eq!(
                    crate::audio_host::PipeWireFacade::verify_physical_nodes_hidden(
                        &mut facade,
                        &candidate,
                    ),
                    Err(crate::audio_host::LinuxAudioError::PhysicalNodeExposed),
                );
            }
            drop(physical);
            roundtrip(&main_loop, &core).expect("remove test node");
            for _ in 0..32 {
                assert_eq!(
                    crate::audio_host::PipeWireFacade::verify_physical_nodes_hidden(
                        &mut facade,
                        &candidate,
                    ),
                    Ok(()),
                );
            }
        }
    }
}
