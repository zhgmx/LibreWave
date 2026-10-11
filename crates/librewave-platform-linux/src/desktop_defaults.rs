//! Daemon-owned desktop default metadata. This adapter never changes ALSA controls.

use librewave_core::DesktopDefaults;
use pipewire as pw;
use pw::types::ObjectType;
use std::cell::{Cell, RefCell};
use std::io;
use std::rc::Rc;
use std::time::{Duration, Instant};

const SYSTEM: &str = "librewave.system";
const MICROPHONE: &str = "librewave.microphone";
const KEYS: [&str; 4] = [
    "default.configured.audio.sink",
    "default.configured.audio.source",
    "default.audio.sink",
    "default.audio.source",
];

/// One authority for desktop default selection and restoration.
pub trait DesktopDefaultStore {
    /// Read configured and effective defaults without changing the graph.
    /// # Errors
    /// Returns a connection, metadata, or decoding failure.
    fn read(&mut self) -> io::Result<DesktopDefaults>;
    /// Select the deliberate `LibreWave` endpoints after audio is ready.
    /// # Errors
    /// Returns a metadata failure; the caller must retain its restoration baseline.
    fn select(&mut self) -> io::Result<DesktopDefaults>;
    /// Reapply owned or automatic selections after graph recovery; preserve user choices.
    /// # Errors
    /// Returns a metadata or verification failure.
    fn recover(&mut self) -> io::Result<DesktopDefaults>;
    /// Restore each configured default only if it still selects `LibreWave`.
    /// # Errors
    /// Returns a metadata failure. A caller can retry safely after partial progress.
    fn restore(&mut self, saved: &DesktopDefaults) -> io::Result<DesktopDefaults>;
}

#[derive(Default)]
pub struct PipeWireDesktopDefaults;

impl DesktopDefaultStore for PipeWireDesktopDefaults {
    fn read(&mut self) -> io::Result<DesktopDefaults> {
        MetadataClient::connect()?.read()
    }
    fn select(&mut self) -> io::Result<DesktopDefaults> {
        let client = MetadataClient::connect()?;
        client.set(0, Some(SYSTEM));
        client.set(1, Some(MICROPHONE));
        client.wait_configured(Some(SYSTEM), Some(MICROPHONE), "selection")
    }
    fn recover(&mut self) -> io::Result<DesktopDefaults> {
        let client = MetadataClient::connect()?;
        let current = client.read()?;
        let expected = recovery(&current);
        if current.configured_sink != expected.configured_sink {
            client.set(0, expected.configured_sink.as_deref());
        }
        if current.configured_source != expected.configured_source {
            client.set(1, expected.configured_source.as_deref());
        }
        client.wait_configured(
            expected.configured_sink.as_deref(),
            expected.configured_source.as_deref(),
            "recovery",
        )
    }
    fn restore(&mut self, saved: &DesktopDefaults) -> io::Result<DesktopDefaults> {
        validate_defaults(saved)?;
        let client = MetadataClient::connect()?;
        let current = client.read()?;
        let expected = restoration(&current, saved);
        if current.configured_sink != expected.configured_sink {
            client.set(0, expected.configured_sink.as_deref());
        }
        if current.configured_source != expected.configured_source {
            client.set(1, expected.configured_source.as_deref());
        }
        client.wait_configured(
            expected.configured_sink.as_deref(),
            expected.configured_source.as_deref(),
            "restoration",
        )
    }
}

/// Validate only software endpoint names, before any metadata write.
/// # Errors
/// Returns an error for an invalid or oversized endpoint name.
pub fn validate_defaults(defaults: &DesktopDefaults) -> io::Result<()> {
    for name in [
        &defaults.configured_sink,
        &defaults.configured_source,
        &defaults.effective_sink,
        &defaults.effective_source,
    ]
    .into_iter()
    .flatten()
    {
        if name.is_empty() || name.len() > 1024 || name.chars().any(char::is_control) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "invalid desktop endpoint name",
            ));
        }
    }
    Ok(())
}

fn recovery(current: &DesktopDefaults) -> DesktopDefaults {
    let mut result = current.clone();
    if current.configured_sink.is_none() {
        result.configured_sink = Some(SYSTEM.into());
    }
    if current.configured_source.is_none() {
        result.configured_source = Some(MICROPHONE.into());
    }
    result
}

fn restoration(current: &DesktopDefaults, saved: &DesktopDefaults) -> DesktopDefaults {
    let mut result = current.clone();
    if current.configured_sink.as_deref() == Some(SYSTEM) {
        result.configured_sink.clone_from(&saved.configured_sink);
    }
    if current.configured_source.as_deref() == Some(MICROPHONE) {
        result.configured_source.clone_from(&saved.configured_source);
    }
    result
}

struct MetadataClient {
    _metadata_listener: pw::metadata::MetadataListener,
    metadata: pw::metadata::Metadata,
    defaults: Rc<RefCell<io::Result<DesktopDefaults>>>,
    core: pw::core::CoreRc,
    _context: pw::context::ContextRc,
    loop_: pw::main_loop::MainLoopRc,
}

impl MetadataClient {
    fn connect() -> io::Result<Self> {
        pw::init();
        let loop_ = pw::main_loop::MainLoopRc::new(None).map_err(io::Error::other)?;
        let context = pw::context::ContextRc::new(&loop_, None).map_err(io::Error::other)?;
        let core = context.connect_rc(None).map_err(io::Error::other)?;
        let registry = core.get_registry_rc().map_err(io::Error::other)?;
        let found = Rc::new(RefCell::new(Vec::new()));
        let globals = Rc::clone(&found);
        let listener = registry
            .add_listener_local()
            .global(move |global| {
                if global.type_ == ObjectType::Metadata
                    && global
                        .props
                        .as_ref()
                        .is_some_and(|props| props.get("metadata.name") == Some("default"))
                {
                    globals.borrow_mut().push(global.to_owned());
                }
            })
            .register();
        roundtrip(&core, &loop_)?;
        // The service can be running before WirePlumber publishes its
        // metadata after a restart. Keep the registry listener alive while
        // waiting for that exact object; do not substitute empty defaults.
        let deadline = Instant::now() + Duration::from_secs(2);
        while found.borrow().is_empty() && Instant::now() < deadline {
            loop_.loop_().iterate(Duration::from_millis(5));
        }
        roundtrip(&core, &loop_)?;
        let objects = found.borrow();
        if objects.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "desktop default metadata did not appear within its restart deadline",
            ));
        }
        let [object] = objects.as_slice() else {
            return Err(io::Error::other("one desktop defaults metadata object is required"));
        };
        let metadata =
            registry.bind::<pw::metadata::Metadata, _>(object).map_err(io::Error::other)?;
        let defaults = Rc::new(RefCell::new(Ok(DesktopDefaults::default())));
        let observed = Rc::clone(&defaults);
        let metadata_listener = metadata
            .add_listener_local()
            .property(move |subject, key, _, value| {
                if subject != 0 {
                    return 0;
                }
                let mut state = observed.borrow_mut();
                if let Ok(defaults) = state.as_mut()
                    && let Err(error) = update(defaults, key, value)
                {
                    *state = Err(error);
                }
                0
            })
            .register();
        roundtrip(&core, &loop_)?;
        drop(objects);
        drop(listener);
        Ok(Self {
            _metadata_listener: metadata_listener,
            metadata,
            defaults,
            core,
            _context: context,
            loop_,
        })
    }
    fn roundtrip(&self) -> io::Result<()> {
        roundtrip(&self.core, &self.loop_)
    }
    fn read(&self) -> io::Result<DesktopDefaults> {
        self.roundtrip()?;
        match &*self.defaults.borrow() {
            Ok(defaults) => Ok(defaults.clone()),
            Err(error) => Err(io::Error::new(error.kind(), error.to_string())),
        }
    }
    fn wait_configured(
        &self,
        sink: Option<&str>,
        source: Option<&str>,
        action: &str,
    ) -> io::Result<DesktopDefaults> {
        self.roundtrip()?;
        // A core sync flushes the request to the metadata owner. WirePlumber
        // publishes the resulting property event asynchronously, after that
        // sync can complete. Verify the owner acknowledgement, not the flush.
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            let observed = match &*self.defaults.borrow() {
                Ok(defaults) => defaults.clone(),
                Err(error) => return Err(io::Error::new(error.kind(), error.to_string())),
            };
            if observed.configured_sink.as_deref() == sink
                && observed.configured_source.as_deref() == source
            {
                return Ok(observed);
            }
            if Instant::now() >= deadline {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    format!("desktop default {action} was not acknowledged within its deadline"),
                ));
            }
            self.loop_.loop_().iterate(Duration::from_millis(5));
        }
    }
    fn set(&self, index: usize, name: Option<&str>) {
        let value = name.map(|name| serde_json::json!({"name":name}).to_string());
        self.metadata.set_property(
            0,
            KEYS[index],
            value.as_ref().map(|_| "Spa:String:JSON"),
            value.as_deref(),
        );
    }
}

fn update(
    defaults: &mut DesktopDefaults,
    key: Option<&str>,
    value: Option<&str>,
) -> io::Result<()> {
    let Some(key) = key else {
        *defaults = DesktopDefaults::default();
        return Ok(());
    };
    let slot = match key {
        "default.configured.audio.sink" => &mut defaults.configured_sink,
        "default.configured.audio.source" => &mut defaults.configured_source,
        "default.audio.sink" => &mut defaults.effective_sink,
        "default.audio.source" => &mut defaults.effective_source,
        _ => return Ok(()),
    };
    *slot = match value {
        None => None,
        Some(value) => Some(
            serde_json::from_str::<serde_json::Value>(value)
                .map_err(io::Error::other)?
                .get("name")
                .and_then(serde_json::Value::as_str)
                .ok_or_else(|| io::Error::other("default metadata has no endpoint name"))?
                .to_owned(),
        ),
    };
    validate_defaults(defaults)
}

fn roundtrip(core: &pw::core::CoreRc, loop_: &pw::main_loop::MainLoopRc) -> io::Result<()> {
    let done = Rc::new(Cell::new(false));
    let completed = Rc::clone(&done);
    let fatal = Rc::new(RefCell::new(None));
    let failure = Rc::clone(&fatal);
    let sequence = core.sync(0).map_err(io::Error::other)?;
    let listener = core
        .add_listener_local()
        .done(move |id, observed| {
            if id == pw::core::PW_ID_CORE && observed == sequence {
                completed.set(true);
            }
        })
        .error(move |_, _, result, _| {
            *failure.borrow_mut() = Some(result);
        })
        .register();
    let deadline = Instant::now() + Duration::from_secs(2);
    while !done.get() && fatal.borrow().is_none() && Instant::now() < deadline {
        loop_.loop_().iterate(Duration::from_millis(5));
    }
    drop(listener);
    if let Some(result) = *fatal.borrow() {
        return Err(io::Error::other(format!("desktop metadata connection failed ({result})")));
    }
    if !done.get() {
        return Err(io::Error::new(io::ErrorKind::TimedOut, "desktop metadata deadline expired"));
    }
    Ok(())
}

/// Read product node names without opening ALSA or changing metadata.
/// # Errors
/// Returns a connection or registry round-trip error.
pub fn audio_endpoints() -> io::Result<Vec<String>> {
    pw::init();
    let loop_ = pw::main_loop::MainLoopRc::new(None).map_err(io::Error::other)?;
    let context = pw::context::ContextRc::new(&loop_, None).map_err(io::Error::other)?;
    let core = context.connect_rc(None).map_err(io::Error::other)?;
    let registry = core.get_registry_rc().map_err(io::Error::other)?;
    let nodes = Rc::new(RefCell::new(std::collections::BTreeMap::new()));
    let added = Rc::clone(&nodes);
    let removed = Rc::clone(&nodes);
    let listener = registry
        .add_listener_local()
        .global(move |object| {
            if object.type_ == ObjectType::Node
                && let Some(name) = object.props.as_ref().and_then(|props| props.get("node.name"))
                && name.starts_with("librewave.")
            {
                added.borrow_mut().insert(object.id, name.to_owned());
            }
        })
        .global_remove(move |id| {
            removed.borrow_mut().remove(&id);
        })
        .register();
    roundtrip(&core, &loop_)?;
    let result = nodes.borrow().values().cloned().collect();
    drop(listener);
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn recovery_reselects_missing_defaults_and_preserves_explicit_user_choices() {
        let current = DesktopDefaults {
            configured_sink: Some("user-output".into()),
            ..DesktopDefaults::default()
        };
        let recovered = recovery(&current);
        assert_eq!(recovered.configured_sink, current.configured_sink);
        assert_eq!(recovered.configured_source.as_deref(), Some(MICROPHONE));
        assert_eq!(recovery(&recovered), recovered);
    }
    #[test]
    fn restoration_is_conditional_per_endpoint_and_preserves_automatic_selection() {
        let current = DesktopDefaults {
            configured_sink: Some(SYSTEM.to_owned()),
            configured_source: Some("user-choice".to_owned()),
            ..DesktopDefaults::default()
        };
        let saved = DesktopDefaults {
            configured_source: Some("wave3-source".to_owned()),
            ..DesktopDefaults::default()
        };
        let restored = restoration(&current, &saved);
        assert_eq!(restored.configured_sink, None);
        assert_eq!(restored.configured_source.as_deref(), Some("user-choice"));
        assert_eq!(restoration(&restored, &saved), restored);
    }
    #[test]
    fn metadata_removal_and_malformed_values_are_explicit() {
        let mut defaults = DesktopDefaults::default();
        update(&mut defaults, Some(KEYS[0]), Some(r#"{"name":"wave3-sink"}"#))
            .expect("valid metadata");
        assert_eq!(defaults.configured_sink.as_deref(), Some("wave3-sink"));
        update(&mut defaults, Some(KEYS[0]), None).expect("removed metadata");
        assert_eq!(defaults.configured_sink, None);
        assert!(update(&mut defaults, Some(KEYS[0]), Some(r#"{"name":4}"#)).is_err());
        assert!(
            validate_defaults(&DesktopDefaults {
                configured_sink: Some("unsafe\0name".to_owned()),
                ..DesktopDefaults::default()
            })
            .is_err()
        );
    }
}
