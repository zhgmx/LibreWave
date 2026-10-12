//! Native registry and metadata listeners on the existing graph connection.
use super::applications::{MetadataValue, Stream, Write};
use super::{
    ActiveGraph, EndpointPlan, GraphProcessor, Instant, ObjectType, Rc, RegistryState, Router,
    applications, pw,
};
use librewave_core::{
    ApplicationGeneration, ApplicationId, ApplicationSnapshot, EndpointId, SourceId,
};

pub(super) struct MetadataBinding {
    id: u32,
    _sync_listener: pw::core::Listener,
    _listener: pw::metadata::MetadataListener,
    metadata: pw::metadata::Metadata,
}

impl<P: GraphProcessor> ActiveGraph<P> {
    pub(super) fn observe_applications(&mut self) {
        self.application_nodes
            .retain(|id, _| self.inner.registry_state.borrow().objects.contains_key(id));
        let objects: Vec<_> = self
            .inner
            .registry_state
            .borrow()
            .objects
            .values()
            .filter(|o| {
                o.type_ == ObjectType::Node
                    && o.props
                        .as_ref()
                        .is_some_and(|p| p.get("media.class") == Some("Stream/Output/Audio"))
            })
            .take(applications::MAX_STREAMS)
            .map(|o| o.id)
            .collect();
        for id in objects {
            if self.application_nodes.contains_key(&id) {
                continue;
            }
            let node = {
                let state = self.inner.registry_state.borrow();
                self.inner.registry.bind::<pw::node::Node, _>(&state.objects[&id])
            };
            let Ok(node) = node else {
                continue;
            };
            {
                let mut state = self.inner.registry_state.borrow_mut();
                let properties = state.objects[&id].props.clone().unwrap_or_default();
                state.node_properties.insert(id, properties);
            }
            let observed = Rc::clone(&self.inner.registry_state);
            let listener = node
                .add_listener_local()
                .info(move |info| {
                    let Some(props) = info.props() else {
                        return;
                    };
                    let mut state = observed.borrow_mut();
                    let properties = state.node_properties.entry(id).or_default();
                    for (key, value) in props.iter() {
                        properties.insert(key, value);
                    }
                    state.router.changed();
                })
                .register();
            self.application_nodes.insert(id, (listener, node));
        }
        self.refresh_metadata(false);
        self.refresh_metadata(true);
    }

    #[allow(
        clippy::too_many_lines,
        reason = "keeps listener and readiness lifetimes in one binding transaction"
    )]
    fn refresh_metadata(&mut self, settings: bool) {
        let name = if settings { "sm-settings" } else { "default" };
        let objects: Vec<_> = self
            .inner
            .registry_state
            .borrow()
            .objects
            .values()
            .filter(|o| {
                o.type_ == ObjectType::Metadata
                    && o.props.as_ref().is_some_and(|p| p.get("metadata.name") == Some(name))
            })
            .map(|o| o.id)
            .collect();
        let slot = if settings { &mut self.settings_metadata } else { &mut self.default_metadata };
        if let [object] = objects.as_slice()
            && slot.as_ref().is_some_and(|b| b.id == *object)
        {
            return;
        }
        slot.take();
        {
            let mut state = self.inner.registry_state.borrow_mut();
            if settings {
                state.router.settings_ready = false;
                state.router.moving = None;
            } else {
                state.router.default_ready = false;
                state.router.targets.clear();
            }
            state.router.changed();
        }
        let [object] = objects.as_slice() else {
            return;
        };
        let metadata = {
            let state = self.inner.registry_state.borrow();
            self.inner.registry.bind::<pw::metadata::Metadata, _>(&state.objects[object])
        };
        let Ok(metadata) = metadata else {
            return;
        };
        let observed = Rc::clone(&self.inner.registry_state);
        let listener = metadata
            .add_listener_local()
            .property(move |subject, key, type_, value| {
                let mut state = observed.borrow_mut();
                if settings {
                    if subject == 0
                        && (key.is_none() || key == Some("linking.allow-moving-streams"))
                    {
                        state.router.moving = match value {
                            Some("true") => Some(true),
                            Some("false") => Some(false),
                            _ => None,
                        };
                        state.router.changed();
                    }
                } else if key.is_none() || key == Some("target.object") {
                    let next = key.and(value).map(|value| MetadataValue {
                        type_: type_.map(str::to_owned),
                        value: value.to_owned(),
                    });
                    state.router.update_target(subject, next);
                }
                0
            })
            .register();
        let Ok(sequence) = self.inner.core.sync(0) else {
            return;
        };
        let observed = Rc::clone(&self.inner.registry_state);
        let sync_listener = self
            .inner
            .core
            .add_listener_local()
            .done(move |id, seq| {
                if id == pw::core::PW_ID_CORE && seq == sequence {
                    let mut state = observed.borrow_mut();
                    if settings {
                        state.router.settings_ready = true;
                    } else {
                        state.router.default_ready = true;
                    }
                    state.router.changed();
                }
            })
            .register();
        *slot = Some(MetadataBinding {
            id: *object,
            _sync_listener: sync_listener,
            _listener: listener,
            metadata,
        });
    }

    pub(super) fn write_targets(&self, writes: Vec<Write>) {
        if let Some(binding) = &self.default_metadata {
            for write in writes {
                binding.metadata.set_property(
                    write.subject,
                    "target.object",
                    write.value.as_ref().and_then(|v| v.type_.as_deref()),
                    write.value.as_ref().map(|v| v.value.as_str()),
                );
            }
        }
    }

    pub(in crate::audio_host) fn applications(&self) -> ApplicationSnapshot {
        let state = self.inner.registry_state.borrow();
        let (streams, unidentified_streams, sinks) = state.application_view(&self.inner.endpoints);
        ApplicationSnapshot::Ready {
            generation: ApplicationGeneration {
                daemon: [0; 16],
                epoch: 0,
                revision: state.router.revision,
            },
            applications: Router::project(&streams, &sinks),
            unidentified_streams,
            movement: state.router.movement(),
            route: state.router.result.clone(),
        }
    }

    pub(in crate::audio_host) fn route_application(
        &mut self,
        revision: u64,
        application: ApplicationId,
        source: SourceId,
    ) -> Result<(), String> {
        let writes = {
            let mut state = self.inner.registry_state.borrow_mut();
            let (streams, _, sinks) = state.application_view(&self.inner.endpoints);
            if state.node_classes.values().filter(|class| *class == "Stream/Output/Audio").count()
                > applications::MAX_STREAMS
            {
                return Err("The application stream observation limit was reached.".into());
            }
            state.router.begin(revision, application, source, &streams, &sinks, Instant::now())?
        };
        self.write_targets(writes);
        Ok(())
    }
}
impl RegistryState {
    pub(super) fn application_view(
        &self,
        endpoints: &[EndpointPlan],
    ) -> (Vec<Stream>, usize, Vec<(SourceId, u32, u64)>) {
        let mut streams = Vec::new();
        let mut unidentified = 0;
        for object in self
            .objects
            .values()
            .filter(|o| self.node_classes.get(&o.id).is_some_and(|v| v == "Stream/Output/Audio"))
        {
            let Some(properties) = self.node_properties.get(&object.id) else {
                unidentified += 1;
                continue;
            };
            let Some((identity, name)) = applications::identity(properties.as_ref()) else {
                unidentified += 1;
                continue;
            };
            let Some(serial) = object
                .props
                .as_ref()
                .and_then(|p| p.get("object.serial"))
                .and_then(|v| v.parse().ok())
            else {
                unidentified += 1;
                continue;
            };
            if streams.len() >= applications::MAX_STREAMS {
                unidentified += 1;
                continue;
            }
            let ports = self
                .ports
                .iter()
                .filter(|p| {
                    p.node_id == object.id
                        && p.direction == "out"
                        && p.format_dsp.as_deref() == Some("32 bit float mono audio")
                })
                .map(|p| p.id)
                .collect();
            let links = self
                .objects
                .values()
                .filter(|o| o.type_ == ObjectType::Link)
                .filter_map(|o| {
                    let p = o.props.as_ref()?;
                    if p.get("link.output.node").and_then(|v| v.parse::<u32>().ok())
                        != Some(object.id)
                    {
                        return None;
                    }
                    Some((
                        p.get("link.output.port")?.parse().ok()?,
                        p.get("link.input.node")?.parse().ok()?,
                    ))
                })
                .collect();
            streams.push(Stream {
                node: object.id,
                serial,
                identity,
                name,
                blocked: !matches!(properties.get("node.dont-move"), None | Some("false")),
                ports,
                links,
            });
        }
        let sinks = endpoints
            .iter()
            .filter_map(|p| {
                let source = match p.endpoint {
                    EndpointId::System => librewave_core::SYSTEM_SOURCE_ID,
                    EndpointId::Input { source } => source,
                    _ => return None,
                };
                let id = *self.node_names.get(&p.node_name)?;
                let serial =
                    self.objects.get(&id)?.props.as_ref()?.get("object.serial")?.parse().ok()?;
                if self
                    .owned_application_sinks
                    .as_ref()
                    .is_some_and(|owned| owned.get(&source) != Some(&(id, serial)))
                {
                    return None;
                }
                Some((source, id, serial))
            })
            .collect();
        (streams, unidentified, sinks)
    }
}
