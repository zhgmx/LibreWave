//! The saved microphone and headphone levels of each device.
//!
//! ALSA writes stale card volumes when a Wave:3 card appears after a replug
//! or a boot. The daemon saves the last levels it read and writes them back
//! on the first admission of a new USB connection. A daemon restart keeps the
//! connection, so it writes nothing and keeps any change made meanwhile.
use crate::control;
use crate::persistence::{PersistenceError, load_json, save_json};
use librewave_core::{FixedPointValue, Wave3ConfigSnapshot, Wave3Control};
use librewave_platform_linux::alsa_levels::{AlsaLevelLock, usb_connection_id};
use librewave_platform_linux::{DiscoveryPaths, UsbDeviceCandidate};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::PathBuf;

const LEVEL_STATE_SCHEMA: u16 = 1;

/// The levels of one device and the USB connection they were read on.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct DeviceLevels {
    /// From [`usb_connection_id`]. `None` when it could not be read; such
    /// levels are written back on every admission.
    pub(crate) connection: Option<String>,
    pub(crate) input_gain: FixedPointValue,
    pub(crate) microphone_mute: bool,
    pub(crate) headphone_level: FixedPointValue,
    pub(crate) headphone_mute: bool,
}

impl DeviceLevels {
    pub(crate) fn read(connection: Option<String>, config: &Wave3ConfigSnapshot) -> Self {
        Self {
            connection,
            input_gain: config.input_gain,
            microphone_mute: config.input_mute,
            headphone_level: config.headphone_volume,
            headphone_mute: config.headphone_mute,
        }
    }

    /// Whether these levels belong to an earlier connection, so ALSA may have
    /// overwritten them since.
    pub(crate) fn due_on(&self, connection: Option<&String>) -> bool {
        self.connection.is_none() || connection.is_none() || self.connection.as_ref() != connection
    }

    /// Takes a level that a client wrote while the restore waited.
    pub(crate) fn update(&mut self, control: Wave3Control) {
        match control {
            Wave3Control::InputGain(value) => self.input_gain = value,
            Wave3Control::MicrophoneMute(value) => self.microphone_mute = value,
            Wave3Control::HeadphoneLevel(value) => self.headphone_level = value,
            Wave3Control::HeadphoneMute(value) => self.headphone_mute = value,
            _ => {}
        }
    }

    /// The writes that put these levels back on `observed`. A level that is
    /// already right is left alone. A gain or headphone level is written at
    /// the nearest 0.5 dB step, because the dial can set one between steps.
    pub(crate) fn controls_for(&self, observed: &Wave3ConfigSnapshot) -> Vec<Wave3Control> {
        [
            (observed.input_gain != self.input_gain)
                .then(|| Wave3Control::InputGain(nearest_step(self.input_gain))),
            (observed.input_mute != self.microphone_mute)
                .then_some(Wave3Control::MicrophoneMute(self.microphone_mute)),
            (observed.headphone_volume != self.headphone_level)
                .then(|| Wave3Control::HeadphoneLevel(nearest_step(self.headphone_level))),
            (observed.headphone_mute != self.headphone_mute)
                .then_some(Wave3Control::HeadphoneMute(self.headphone_mute)),
        ]
        .into_iter()
        .flatten()
        .filter(|control| control::validate_control(*control).is_ok())
        .collect()
    }

    fn validate(&self) -> Result<(), String> {
        for control in [
            Wave3Control::InputGain(nearest_step(self.input_gain)),
            Wave3Control::HeadphoneLevel(nearest_step(self.headphone_level)),
        ] {
            control::validate_control(control)?;
        }
        Ok(())
    }
}

/// Rounds a Q8.8 level to the nearest 0.5 dB write step (raw multiple of 128).
fn nearest_step(value: FixedPointValue) -> FixedPointValue {
    if value.fractional_bits != 8 {
        return value;
    }
    FixedPointValue { raw: value.raw.saturating_add(64).div_euclid(128) * 128, ..value }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SavedLevels {
    schema: u16,
    devices: BTreeMap<String, DeviceLevels>,
}

impl Default for SavedLevels {
    fn default() -> Self {
        Self { schema: LEVEL_STATE_SCHEMA, devices: BTreeMap::new() }
    }
}

impl SavedLevels {
    pub(crate) fn get(&self, topology: &str) -> Option<&DeviceLevels> {
        self.devices.get(topology)
    }

    pub(crate) fn set(&mut self, topology: String, levels: DeviceLevels) {
        self.devices.insert(topology, levels);
    }

    pub(crate) fn validate(&self) -> Result<(), PersistenceError> {
        if self.schema != LEVEL_STATE_SCHEMA {
            return Err(PersistenceError::UnsupportedSchema(self.schema));
        }
        for (topology, levels) in &self.devices {
            librewave_platform_linux::validate_usb_topology(topology)
                .map_err(|error| error.to_string())
                .and_then(|()| levels.validate())
                .map_err(|error| {
                    PersistenceError::InvalidState(format!(
                        "saved levels for topology `{topology}` are invalid: {error}"
                    ))
                })?;
        }
        Ok(())
    }
}

pub(crate) trait LevelStore {
    fn load(&mut self) -> Result<SavedLevels, PersistenceError>;
    fn save(&mut self, levels: &SavedLevels) -> Result<(), PersistenceError>;
}

pub(crate) struct FileLevelStore(pub(crate) PathBuf);

impl LevelStore for FileLevelStore {
    fn load(&mut self) -> Result<SavedLevels, PersistenceError> {
        let levels: SavedLevels = load_json(&self.0)?.unwrap_or_default();
        levels.validate()?;
        Ok(levels)
    }

    fn save(&mut self, levels: &SavedLevels) -> Result<(), PersistenceError> {
        save_json(&self.0, levels)
    }
}

#[derive(Default)]
pub(crate) struct MemoryLevelStore(pub(crate) SavedLevels);

impl LevelStore for MemoryLevelStore {
    fn load(&mut self) -> Result<SavedLevels, PersistenceError> {
        Ok(self.0.clone())
    }

    fn save(&mut self, levels: &SavedLevels) -> Result<(), PersistenceError> {
        self.0 = levels.clone();
        Ok(())
    }
}

/// Holds the ALSA level controls locked until it is dropped.
pub(crate) type LevelLock = Box<dyn std::any::Any>;

/// The host side of the level restore.
pub(crate) trait LevelHost {
    /// Names the current USB connection of the device, when it can be read.
    fn connection_id(&mut self, candidate: &UsbDeviceCandidate) -> Option<String>;
    /// Locks the device's ALSA level controls against other ALSA writers.
    fn lock_levels(&mut self, candidate: &UsbDeviceCandidate) -> Result<LevelLock, String>;
}

pub(crate) struct LinuxLevelHost(pub(crate) DiscoveryPaths);

impl LevelHost for LinuxLevelHost {
    fn connection_id(&mut self, candidate: &UsbDeviceCandidate) -> Option<String> {
        usb_connection_id(&self.0, candidate)
            .inspect_err(|error| {
                eprintln!("librewaved: the USB connection of the device is unknown: {error}");
            })
            .ok()
    }

    fn lock_levels(&mut self, candidate: &UsbDeviceCandidate) -> Result<LevelLock, String> {
        AlsaLevelLock::acquire(&self.0, candidate)
            .map(|lock| Box::new(lock) as LevelLock)
            .map_err(|error| error.to_string())
    }
}

/// A daemon without host audio: it names no connection and locks nothing.
pub(crate) struct NoLevelHost;

impl LevelHost for NoLevelHost {
    fn connection_id(&mut self, _: &UsbDeviceCandidate) -> Option<String> {
        None
    }

    fn lock_levels(&mut self, _: &UsbDeviceCandidate) -> Result<LevelLock, String> {
        Err("this daemon has no ALSA access".to_owned())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn level(raw: i32) -> FixedPointValue {
        FixedPointValue { raw, fractional_bits: 8 }
    }

    fn config() -> Wave3ConfigSnapshot {
        Wave3ConfigSnapshot {
            input_gain: level(4096),
            input_mute: false,
            clipguard_enable: false,
            lowcut_enable: false,
            headphone_volume: level(-6016),
            headphone_mute: false,
            direct_monitor: level(0),
            volume_select: librewave_core::VolumeSelection::Headphone,
            all_leds_off: false,
            leds_flip: false,
            gain_lock: false,
        }
    }

    #[test]
    fn every_changed_level_is_put_back_at_the_nearest_step() {
        let alsa = config();
        let mut saved = DeviceLevels::read(Some("a".into()), &alsa);
        saved.input_gain = level(4726); // 18.46 dB, set with the dial.
        saved.microphone_mute = true;
        saved.headphone_level = level(-1536);
        saved.headphone_mute = true;
        assert_eq!(
            saved.controls_for(&alsa),
            vec![
                Wave3Control::InputGain(level(4736)),
                Wave3Control::MicrophoneMute(true),
                Wave3Control::HeadphoneLevel(level(-1536)),
                Wave3Control::HeadphoneMute(true),
            ]
        );
        // An unmute is put back too.
        let mut muted = alsa.clone();
        muted.input_mute = true;
        assert_eq!(
            DeviceLevels::read(None, &alsa).controls_for(&muted),
            vec![Wave3Control::MicrophoneMute(false)]
        );
        assert!(DeviceLevels::read(None, &alsa).controls_for(&alsa).is_empty());
    }

    #[test]
    fn only_another_or_unknown_connection_is_due() {
        let saved = DeviceLevels::read(Some("boot/001/009".into()), &config());
        assert!(!saved.due_on(Some(&"boot/001/009".to_owned())));
        assert!(saved.due_on(Some(&"boot/001/019".to_owned())));
        assert!(saved.due_on(None));
        assert!(DeviceLevels::read(None, &config()).due_on(Some(&"boot/001/009".to_owned())));
    }

    #[test]
    fn saved_levels_reload_and_reject_bad_values() {
        let root =
            std::env::temp_dir().join(format!("librewave-saved-levels-{}", std::process::id()));
        let path = root.join("device-levels.json");
        let mut store = FileLevelStore(path.clone());
        assert_eq!(store.load().expect("empty"), SavedLevels::default());
        let mut levels = SavedLevels::default();
        levels.set("1-8.3".into(), DeviceLevels::read(Some("a".into()), &config()));
        store.save(&levels).expect("save");
        assert_eq!(store.load().expect("reload"), levels);

        let mut bad = levels.clone();
        bad.devices.get_mut("1-8.3").expect("device").input_gain = level(i32::MAX);
        store.save(&bad).expect("save");
        assert!(matches!(store.load(), Err(PersistenceError::InvalidState(_))));
        std::fs::remove_dir_all(root).expect("cleanup");
    }
}
