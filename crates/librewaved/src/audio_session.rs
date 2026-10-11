//! Durable daemon-owned audio intent and default restoration baseline.
use crate::persistence::{PersistenceError, load_json, save_json};
use librewave_core::DesktopDefaults;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct AudioSession {
    schema: u16,
    pub enabled: bool,
    pub selected: bool,
    pub defaults: Option<DesktopDefaults>,
}
impl Default for AudioSession {
    fn default() -> Self {
        Self { schema: 1, enabled: false, selected: false, defaults: None }
    }
}
impl AudioSession {
    pub fn validate(&self) -> Result<(), PersistenceError> {
        if self.schema != 1 {
            return Err(PersistenceError::UnsupportedSchema(self.schema));
        }
        if self.selected && self.defaults.is_none() {
            return Err(PersistenceError::InvalidState(
                "selected defaults have no restoration baseline".into(),
            ));
        }
        if let Some(defaults) = &self.defaults {
            validate_baseline(defaults)?;
        }
        Ok(())
    }
}
pub(super) fn validate_baseline(defaults: &DesktopDefaults) -> Result<(), PersistenceError> {
    librewave_platform_linux::desktop_defaults::validate_defaults(defaults)?;
    if defaults.configured_sink.as_deref() == Some("librewave.system")
        || defaults.configured_source.as_deref() == Some("librewave.microphone")
    {
        return Err(PersistenceError::InvalidState("existing LibreWave default selections have no restoration baseline; choose automatic or stable virtual defaults before takeover".into()));
    }
    if defaults.effective_sink.is_some()
        || defaults.effective_source.is_some()
        || [&defaults.configured_sink, &defaults.configured_source]
            .into_iter()
            .flatten()
            .any(|name| name.starts_with("alsa_output.usb-") || name.starts_with("alsa_input.usb-"))
    {
        return Err(PersistenceError::InvalidState(
            "USB-derived default names cannot be saved without risking a device serial; use automatic or stable virtual defaults before takeover".into()));
    }
    Ok(())
}
pub(super) trait AudioSessionStore {
    fn load(&mut self) -> Result<AudioSession, PersistenceError>;
    fn save(&mut self, state: &AudioSession) -> Result<(), PersistenceError>;
}
pub(super) struct FileAudioSessionStore(pub PathBuf);
impl AudioSessionStore for FileAudioSessionStore {
    fn load(&mut self) -> Result<AudioSession, PersistenceError> {
        let state: AudioSession = load_json(&self.0)?.unwrap_or_default();
        state.validate()?;
        Ok(state)
    }
    fn save(&mut self, state: &AudioSession) -> Result<(), PersistenceError> {
        state.validate()?;
        save_json(&self.0, state)
    }
}
pub(super) struct MemoryAudioSessionStore(pub AudioSession);
impl AudioSessionStore for MemoryAudioSessionStore {
    fn load(&mut self) -> Result<AudioSession, PersistenceError> {
        Ok(self.0.clone())
    }
    fn save(&mut self, state: &AudioSession) -> Result<(), PersistenceError> {
        state.validate()?;
        self.0 = state.clone();
        Ok(())
    }
}
