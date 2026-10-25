//! Control-thread preparation and cleanup of software input endpoints.
use super::{
    ActiveGraph, EndpointPlan, FILTER_NODE_NAME, GraphError, GraphProcessor, InputLayout,
    LOOP_ITERATION, MAX_SOFTWARE_INPUTS, ProcessFault, ROUNDTRIP_TIMEOUT, create_adapter,
    create_link, expected_adapter_ports, roundtrip, validate_endpoint_count, validate_endpoints,
};
use librewave_core::{EndpointId, SourceId};
use pipewire as pw;
use pw::loop_::Timeout;
use std::collections::HashMap;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

pub(super) struct PreparedInputs {
    endpoints: Vec<EndpointPlan>,
    layout: InputLayout,
    links: HashMap<SourceId, Vec<pw::link::Link>>,
    nodes: HashMap<SourceId, pw::node::Node>,
    playback_ports: Vec<[u32; 2]>,
}

impl<P: GraphProcessor> ActiveGraph<P> {
    pub(in crate::audio_host) fn prepare_inputs(
        &mut self,
        endpoints: &[EndpointPlan],
        layout: &InputLayout,
    ) -> Result<(), GraphError> {
        if self.prepared_inputs.is_some() || self.inner.input_layout.is_none() {
            return Err(GraphError::InvalidEndpointContract);
        }
        validate_endpoints(endpoints)?;
        let old = self.inner.input_layout.as_ref().ok_or(GraphError::InvalidEndpointContract)?;
        for (source, slot) in layout.pairs() {
            if old.slot(source).is_none()
                && old.pairs().iter().any(|(_, occupied)| *occupied == slot)
            {
                return Err(GraphError::InvalidEndpointContract);
            }
        }
        let mut prepared = PreparedInputs {
            endpoints: endpoints.to_vec(),
            layout: layout.clone(),
            nodes: HashMap::new(),
            links: HashMap::new(),
            playback_ports: vec![[u32::MAX; 2]; MAX_SOFTWARE_INPUTS],
        };
        let result = self.prepare_input_resources(&mut prepared);
        match result {
            Ok(()) => {
                self.prepared_inputs = Some(prepared);
                Ok(())
            }
            Err(error) => {
                drop(prepared);
                roundtrip(&self.inner.main_loop, &self.inner.core)?;
                Err(error)
            }
        }
    }

    fn prepare_input_resources(&mut self, prepared: &mut PreparedInputs) -> Result<(), GraphError> {
        for plan in prepared.endpoints.iter().skip(4) {
            let EndpointId::Input { source } = plan.endpoint else {
                return Err(GraphError::InvalidEndpointContract);
            };
            if !self.inner.additional_nodes.contains_key(&source) {
                prepared.nodes.insert(
                    source,
                    create_adapter(&self.inner.core, plan, false, self.inner.quantum)?,
                );
            }
        }
        let deadline = Instant::now() + ROUNDTRIP_TIMEOUT;
        let mut settled = (u64::MAX, Instant::now());
        loop {
            self.inner.main_loop.loop_().iterate(Timeout::Finite(LOOP_ITERATION));
            let state = self.inner.registry_state.borrow();
            if state.port_generation != settled.0 {
                settled = (state.port_generation, Instant::now());
            }
            if expected_adapter_ports(&state, &prepared.endpoints).is_ok()
                && settled.1.elapsed() >= Duration::from_millis(100)
            {
                break;
            }
            if Instant::now() >= deadline {
                return Err(GraphError::PortConfigurationTimeout);
            }
        }
        let state = self.inner.registry_state.borrow();
        let filter = state.node_id(FILTER_NODE_NAME)?;
        for plan in &prepared.endpoints {
            let source = match plan.endpoint {
                EndpointId::System => librewave_core::SYSTEM_SOURCE_ID,
                EndpointId::Input { source } => source,
                _ => continue,
            };
            let slot = prepared.layout.slot(source).ok_or(GraphError::InvalidEndpointContract)?;
            let index = usize::from(slot.get() - 2);
            let node = state.node_id(&plan.node_name)?;
            prepared.playback_ports[index] = [
                state.port_id(node, "in", "playback_FL")?,
                state.port_id(node, "in", "playback_FR")?,
            ];
            if prepared.nodes.contains_key(&source) {
                let mut links = Vec::new();
                for channel in ["FL", "FR"] {
                    links.push(create_link(
                        &self.inner.core,
                        state.port_id(node, "out", &format!("monitor_{channel}"))?,
                        state.port_id(filter, "in", &format!("input_{slot}_{channel}"))?,
                    )?);
                }
                prepared.links.insert(source, links);
            }
        }
        drop(state);
        roundtrip(&self.inner.main_loop, &self.inner.core)?;
        roundtrip(&self.inner.main_loop, &self.inner.core)?;
        #[cfg(test)]
        if std::mem::take(&mut self.fail_input_preparation) {
            return Err(GraphError::PortConfigurationTimeout);
        }
        if self.diagnostics().failed {
            return Err(GraphError::AttemptFault(ProcessFault::Processor));
        }
        Ok(())
    }

    pub(in crate::audio_host) fn cancel_inputs(&mut self) -> Result<(), GraphError> {
        self.prepared_inputs.take();
        roundtrip(&self.inner.main_loop, &self.inner.core)?;
        validate_endpoint_count(&self.inner.registry_state.borrow(), self.inner.endpoints.len())
    }

    fn activate_inputs(&mut self, mask: u32) -> Result<(), GraphError> {
        let previous = self.inner.activation.requested.load(Ordering::Acquire);
        let revision = (previous >> 32)
            .checked_add(1)
            .filter(|v| u32::try_from(*v).is_ok())
            .ok_or(GraphError::InvalidEndpointContract)?;
        let requested = (revision << 32) | u64::from(mask);
        self.inner.activation.requested.store(requested, Ordering::Release);
        let deadline = Instant::now() + ROUNDTRIP_TIMEOUT;
        while self.inner.activation.applied.load(Ordering::Acquire) != requested {
            self.inner.main_loop.loop_().iterate(Timeout::Finite(LOOP_ITERATION));
            if self.diagnostics().failed {
                return Err(GraphError::AttemptFault(ProcessFault::Processor));
            }
            if Instant::now() >= deadline {
                return Err(GraphError::RoundtripTimeout);
            }
        }
        Ok(())
    }

    pub(in crate::audio_host) fn commit_inputs(&mut self) -> Result<(), GraphError> {
        let Some(prepared) = self.prepared_inputs.take() else {
            return Err(GraphError::InvalidEndpointContract);
        };
        let next_mask = prepared.layout.active_mask();
        let old_mask = self
            .inner
            .input_layout
            .as_ref()
            .ok_or(GraphError::InvalidEndpointContract)?
            .active_mask();
        // Removed slots become silent at a confirmed block boundary before
        // their links are dropped. New slots remain silent until ports exist.
        self.activate_inputs(old_mask & next_mask)?;
        self.inner.registry_state.borrow_mut().set_input_playback_ports(prepared.playback_ports);
        self.inner.additional_nodes.extend(prepared.nodes);
        self.inner.additional_links.extend(prepared.links);
        self.activate_inputs(next_mask)?;
        self.inner.additional_links.retain(|source, _| prepared.layout.slot(*source).is_some());
        self.inner.additional_nodes.retain(|source, _| prepared.layout.slot(*source).is_some());
        self.inner.endpoints = prepared.endpoints;
        self.inner.input_layout = Some(prepared.layout);
        roundtrip(&self.inner.main_loop, &self.inner.core)?;
        let mut state = self.inner.registry_state.borrow_mut();
        state.owned_application_sinks = None;
        let (_, _, sinks) = state.application_view(&self.inner.endpoints);
        state.owned_application_sinks = Some(
            sinks.into_iter().map(|(source, node, serial)| (source, (node, serial))).collect(),
        );
        validate_endpoint_count(&state, self.inner.endpoints.len())
    }
}
