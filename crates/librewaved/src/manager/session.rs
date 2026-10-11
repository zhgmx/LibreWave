use super::{
    AudioSupervisor, Daemon, IpcError, IpcErrorKind, MixerRuntimeState, PersistenceError, Response,
    ignore_reconciliation_event, internal_error,
};

impl Daemon {
    pub(super) fn check_audio_access(
        &mut self,
        origin: librewave_core::Origin,
    ) -> Result<Response, IpcError> {
        ignore_reconciliation_event(self.refresh_inventory(origin));
        if self.candidates.len() != 1 {
            return Err(IpcError {
                kind: IpcErrorKind::AudioNotReady,
                message: "connect exactly one supported Wave:3 before takeover".to_owned(),
            });
        }
        let candidate = self
            .candidates
            .values()
            .next()
            .ok_or_else(|| internal_error("no candidate after admission count check".into()))?;
        (self.audio_access)(candidate).map_err(|error| IpcError {
            message: format!(
                "read-only Wave:3 admission failed: {error}; no desktop ownership was changed"
            ),
            kind: IpcErrorKind::AudioAccess { error },
        })?;
        Ok(Response::AudioAccessChecked)
    }
    pub(super) fn read_desktop_defaults(&mut self) -> Result<Response, IpcError> {
        let defaults = self
            .desktop_defaults
            .read()
            .map_err(|error| internal_error(format!("cannot read desktop defaults: {error}")))?;
        Ok(Response::DesktopDefaults { defaults })
    }
    pub(super) fn read_audio_endpoints(&mut self) -> Result<Response, IpcError> {
        let names = (self.audio_endpoints)()
            .map_err(|error| internal_error(format!("cannot inspect audio endpoints: {error}")))?;
        Ok(Response::AudioEndpoints { names })
    }
    pub(super) fn capture_desktop_defaults(&mut self) -> Result<Response, IpcError> {
        if self.audio_session.defaults.is_none() {
            let mut defaults = self.desktop_defaults.read().map_err(|error| {
                internal_error(format!("cannot read desktop defaults: {error}"))
            })?;
            defaults.effective_sink = None;
            defaults.effective_source = None;
            crate::audio_session::validate_baseline(&defaults)
                .map_err(|error| internal_error(error.to_string()))?;
            let mut session = self.audio_session.clone();
            session.defaults = Some(defaults);
            self.save_audio_session(session)?;
        }
        Ok(Response::DesktopDefaults {
            defaults: self
                .audio_session
                .defaults
                .clone()
                .ok_or_else(|| internal_error("default baseline was not captured".into()))?,
        })
    }
    pub(super) fn select_desktop_defaults(&mut self) -> Result<Response, IpcError> {
        if self.snapshot().mixer.runtime != MixerRuntimeState::Active {
            return Err(IpcError {
                kind: IpcErrorKind::AudioNotReady,
                message: "audio must be ready before selecting LibreWave defaults".to_owned(),
            });
        }
        if self.audio_session.defaults.is_none() {
            return Err(internal_error(
                "capture desktop defaults before selecting audio endpoints".to_owned(),
            ));
        }
        let mut session = self.audio_session.clone();
        session.selected = true;
        self.save_audio_session(session)?;
        let defaults = self
            .desktop_defaults
            .select()
            .map_err(|error| internal_error(format!("cannot select desktop defaults: {error}")))?;
        self.defaults_recovery_pending = false;
        Ok(Response::DesktopDefaults { defaults })
    }
    pub(super) fn restore_desktop_defaults(&mut self) -> Result<Response, IpcError> {
        if self.audio_session.enabled
            || self.audio.as_ref().is_some_and(AudioSupervisor::needs_tick)
        {
            return Err(internal_error(
                "stop audio and finish cleanup before restoring desktop defaults".to_owned(),
            ));
        }
        let mut session = self.audio_session.clone();
        session.selected = false;
        self.save_audio_session(session)?;
        let defaults = if let Some(saved) = &self.audio_session.defaults {
            self.desktop_defaults.restore(saved).map_err(|error| {
                internal_error(format!("cannot restore desktop defaults: {error}"))
            })?
        } else {
            self.desktop_defaults
                .read()
                .map_err(|error| internal_error(format!("cannot read desktop defaults: {error}")))?
        };
        if self.audio_session.defaults.is_some() {
            let mut baseline = defaults.clone();
            baseline.effective_sink = None;
            baseline.effective_source = None;
            let mut session = self.audio_session.clone();
            session.defaults = Some(baseline);
            // Retain the confirmed user selections if WirePlumber replays an
            // earlier owned default during the policy restart.
            self.save_audio_session(session)?;
        }
        Ok(Response::DesktopDefaults { defaults })
    }
    pub(super) fn complete_desktop_restore(&mut self) -> Result<Response, IpcError> {
        if self.audio_session.enabled
            || self.audio.as_ref().is_some_and(AudioSupervisor::needs_tick)
        {
            return Err(internal_error("stop audio before completing default restoration".into()));
        }
        if self.audio_session.defaults.is_none() {
            return self.read_desktop_defaults();
        }
        let Response::DesktopDefaults { defaults } = self.read_desktop_defaults()? else {
            unreachable!()
        };
        if defaults.configured_sink.as_deref() == Some("librewave.system")
            || defaults.configured_source.as_deref() == Some("librewave.microphone")
        {
            return Err(internal_error("default restoration still selects LibreWave".into()));
        }
        let mut session = self.audio_session.clone();
        session.defaults = None;
        session.selected = false;
        self.save_audio_session(session)?;
        Ok(Response::DesktopDefaults { defaults })
    }
    pub(super) fn load_audio_session(&mut self) -> Result<(), PersistenceError> {
        self.audio_session = self.audio_session_store.load()?;
        self.audio_session.validate()?;
        self.defaults_recovery_pending = self.audio_session.selected;
        if self.audio_session.enabled {
            self.activate_audio(librewave_core::Origin::Recovery)
                .map_err(|error| PersistenceError::InvalidState(error.to_string()))?;
        }
        Ok(())
    }
}

#[cfg(test)]
#[path = "session_tests.rs"]
mod tests;
