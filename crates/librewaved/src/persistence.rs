use librewave_core::{FixedPointValue, VolumeSelection, Wave3Control};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

const DESIRED_STATE_SCHEMA: u16 = 1;

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct DesiredState {
    schema: u16,
    devices: BTreeMap<String, DeviceDesiredState>,
}

impl Default for DesiredState {
    fn default() -> Self {
        Self { schema: DESIRED_STATE_SCHEMA, devices: BTreeMap::new() }
    }
}

impl DesiredState {
    pub(crate) fn get(&self, topology: &str) -> Option<&DeviceDesiredState> {
        self.devices.get(topology)
    }

    pub(crate) fn devices(&self) -> &BTreeMap<String, DeviceDesiredState> {
        &self.devices
    }

    pub(crate) fn stage(&mut self, topology: String, control: Wave3Control) -> Result<(), ()> {
        let device = self.devices.entry(topology).or_default();
        if device.pending.is_some() {
            return Err(());
        }
        device.pending = Some(control);
        Ok(())
    }

    pub(crate) fn promote(&mut self, topology: &str) -> Result<(), ()> {
        let device = self.devices.get_mut(topology).ok_or(())?;
        let control = device.pending.take().ok_or(())?;
        device.managed.set(control);
        Ok(())
    }

    pub(crate) fn clear_pending(&mut self, topology: &str) -> Result<(), ()> {
        let device = self.devices.get_mut(topology).ok_or(())?;
        if device.pending.take().is_none() {
            return Err(());
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct DeviceDesiredState {
    pub(crate) managed: DesiredWave3Controls,
    pub(crate) pending: Option<Wave3Control>,
}

/// Only controls explicitly changed through `LibreWave` become desired state.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
#[allow(clippy::struct_excessive_bools)]
pub(crate) struct DesiredWave3Controls {
    pub(crate) input_gain: Option<FixedPointValue>,
    pub(crate) microphone_mute: Option<bool>,
    pub(crate) clipguard: Option<bool>,
    pub(crate) low_cut: Option<bool>,
    pub(crate) headphone_level: Option<FixedPointValue>,
    pub(crate) headphone_mute: Option<bool>,
    pub(crate) monitor_mix: Option<FixedPointValue>,
    pub(crate) knob_target: Option<VolumeSelection>,
    pub(crate) all_leds_off: Option<bool>,
    pub(crate) leds_flip: Option<bool>,
    pub(crate) gain_lock: Option<bool>,
}

impl DesiredWave3Controls {
    fn set(&mut self, control: Wave3Control) {
        match control {
            Wave3Control::InputGain(value) => self.input_gain = Some(value),
            Wave3Control::MicrophoneMute(value) => self.microphone_mute = Some(value),
            Wave3Control::Clipguard(value) => self.clipguard = Some(value),
            Wave3Control::LowCut(value) => self.low_cut = Some(value),
            Wave3Control::HeadphoneLevel(value) => self.headphone_level = Some(value),
            Wave3Control::HeadphoneMute(value) => self.headphone_mute = Some(value),
            Wave3Control::MonitorMix(value) => self.monitor_mix = Some(value),
            Wave3Control::KnobTarget(value) => self.knob_target = Some(value),
            Wave3Control::AllLedsOff(value) => self.all_leds_off = Some(value),
            Wave3Control::LedsFlip(value) => self.leds_flip = Some(value),
            Wave3Control::GainLock(value) => self.gain_lock = Some(value),
        }
    }

    pub(crate) fn controls(&self) -> Vec<Wave3Control> {
        let mut controls = Vec::new();
        if let Some(value) = self.input_gain {
            controls.push(Wave3Control::InputGain(value));
        }
        if let Some(value) = self.microphone_mute {
            controls.push(Wave3Control::MicrophoneMute(value));
        }
        if let Some(value) = self.clipguard {
            controls.push(Wave3Control::Clipguard(value));
        }
        if let Some(value) = self.low_cut {
            controls.push(Wave3Control::LowCut(value));
        }
        if let Some(value) = self.headphone_level {
            controls.push(Wave3Control::HeadphoneLevel(value));
        }
        if let Some(value) = self.headphone_mute {
            controls.push(Wave3Control::HeadphoneMute(value));
        }
        if let Some(value) = self.monitor_mix {
            controls.push(Wave3Control::MonitorMix(value));
        }
        if let Some(value) = self.knob_target {
            controls.push(Wave3Control::KnobTarget(value));
        }
        if let Some(value) = self.all_leds_off {
            controls.push(Wave3Control::AllLedsOff(value));
        }
        if let Some(value) = self.leds_flip {
            controls.push(Wave3Control::LedsFlip(value));
        }
        if let Some(value) = self.gain_lock {
            controls.push(Wave3Control::GainLock(value));
        }
        controls
    }
}

#[derive(Debug)]
pub enum PersistenceError {
    Io(io::Error),
    SaveBeforeRename(io::Error),
    SaveAfterRename(io::Error),
    Invalid(serde_json::Error),
    UnsupportedSchema(u16),
    InvalidState(String),
}

impl fmt::Display for PersistenceError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => write!(formatter, "desired-state I/O failed: {error}"),
            Self::SaveBeforeRename(error) => {
                write!(formatter, "desired-state save did not commit: {error}")
            }
            Self::SaveAfterRename(error) => {
                write!(formatter, "desired-state directory sync failed after rename: {error}")
            }
            Self::Invalid(error) => write!(formatter, "desired-state file is invalid: {error}"),
            Self::UnsupportedSchema(schema) => {
                write!(formatter, "desired-state schema {schema} is not supported")
            }
            Self::InvalidState(message) => formatter.write_str(message),
        }
    }
}

impl std::error::Error for PersistenceError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(error) | Self::SaveBeforeRename(error) | Self::SaveAfterRename(error) => {
                Some(error)
            }
            Self::Invalid(error) => Some(error),
            Self::UnsupportedSchema(_) | Self::InvalidState(_) => None,
        }
    }
}

impl From<io::Error> for PersistenceError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

pub(crate) trait DesiredStateStore {
    fn load(&mut self) -> Result<DesiredState, PersistenceError>;
    fn save(&mut self, state: &DesiredState) -> Result<(), PersistenceError>;
}

pub(crate) struct FileDesiredStateStore {
    path: PathBuf,
}

impl FileDesiredStateStore {
    pub(crate) fn new(path: PathBuf) -> Self {
        Self { path }
    }
}

impl DesiredStateStore for FileDesiredStateStore {
    fn load(&mut self) -> Result<DesiredState, PersistenceError> {
        recover_pending(&self.path)?;
        let metadata = match fs::symlink_metadata(&self.path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                return Ok(DesiredState::default());
            }
            Err(error) => return Err(error.into()),
        };
        validate_metadata(&self.path, &metadata)?;
        let mut file = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW)
            .open(&self.path)?;
        validate_metadata(&self.path, &file.metadata()?)?;
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes)?;
        let state: DesiredState =
            serde_json::from_slice(&bytes).map_err(PersistenceError::Invalid)?;
        if state.schema != DESIRED_STATE_SCHEMA {
            return Err(PersistenceError::UnsupportedSchema(state.schema));
        }
        Ok(state)
    }

    fn save(&mut self, state: &DesiredState) -> Result<(), PersistenceError> {
        let parent = self.path.parent().ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, "desired-state path has no parent")
        })?;
        ensure_private_directory(parent).map_err(PersistenceError::SaveBeforeRename)?;
        recover_pending(&self.path).map_err(PersistenceError::SaveBeforeRename)?;
        match fs::symlink_metadata(&self.path) {
            Ok(metadata) => validate_metadata(&self.path, &metadata)
                .map_err(PersistenceError::SaveBeforeRename)?,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(PersistenceError::SaveBeforeRename(error)),
        }
        let bytes = serde_json::to_vec(state).map_err(PersistenceError::Invalid)?;
        let temporary = pending_path(&self.path);
        write_atomic(parent, &temporary, &self.path, &bytes)
    }
}

fn pending_path(path: &Path) -> PathBuf {
    let name = path.file_name().and_then(|name| name.to_str()).unwrap_or("device-state.json");
    path.with_file_name(format!(".{name}.pending"))
}

fn write_atomic(
    parent: &Path,
    temporary: &Path,
    destination: &Path,
    bytes: &[u8],
) -> Result<(), PersistenceError> {
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(temporary)
        .map_err(PersistenceError::SaveBeforeRename)?;
    if let Err(error) = file.write_all(bytes).and_then(|()| file.sync_all()) {
        drop(file);
        return match fs::remove_file(temporary) {
            Ok(()) => Err(PersistenceError::SaveBeforeRename(error)),
            Err(cleanup) => Err(PersistenceError::SaveBeforeRename(io::Error::other(format!(
                "temporary-file write failed ({error}); cleanup also failed ({cleanup})"
            )))),
        };
    }
    drop(file);
    if let Err(rename) = fs::rename(temporary, destination) {
        return match fs::remove_file(temporary) {
            Ok(()) => Err(PersistenceError::SaveBeforeRename(rename)),
            Err(cleanup) => Err(PersistenceError::SaveBeforeRename(io::Error::other(format!(
                "rename failed ({rename}); temporary-file cleanup also failed ({cleanup})"
            )))),
        };
    }
    File::open(parent)
        .and_then(|directory| directory.sync_all())
        .map_err(PersistenceError::SaveAfterRename)
}

fn recover_pending(path: &Path) -> io::Result<()> {
    let pending = pending_path(path);
    let pending_metadata = match fs::symlink_metadata(&pending) {
        Ok(metadata) => Some(metadata),
        Err(error) if error.kind() == io::ErrorKind::NotFound => None,
        Err(error) => return Err(error),
    };
    let destination_metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => Some(metadata),
        Err(error) if error.kind() == io::ErrorKind::NotFound => None,
        Err(error) => return Err(error),
    };
    if let Some(metadata) = destination_metadata.as_ref() {
        validate_metadata(path, metadata)?;
    }
    let Some(pending_metadata) = pending_metadata else {
        return Ok(());
    };
    validate_metadata(&pending, &pending_metadata)?;
    fs::remove_file(&pending)?;
    File::open(path.parent().ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidInput, "desired-state path has no parent")
    })?)?
    .sync_all()
}

fn ensure_private_directory(path: &Path) -> io::Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => {
            if !metadata.file_type().is_dir() || metadata.file_type().is_symlink() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("{} is not a private state directory", path.display()),
                ));
            }
            Ok(())
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            fs::DirBuilder::new().recursive(true).mode(0o700).create(path)
        }
        Err(error) => Err(error),
    }
}

fn validate_metadata(path: &Path, metadata: &fs::Metadata) -> io::Result<()> {
    if !metadata.file_type().is_file() || metadata.file_type().is_symlink() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("{} is not a regular state file", path.display()),
        ));
    }
    if metadata.mode() & 0o777 != 0o600 {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("{} must have mode 0600", path.display()),
        ));
    }
    if metadata.nlink() != 1 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("{} must have exactly one link", path.display()),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt, symlink};
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn file_store_round_trips_only_the_current_strict_schema() {
        let nonce =
            SystemTime::now().duration_since(UNIX_EPOCH).expect("clock before epoch").as_nanos();
        let root = std::env::temp_dir().join(format!("librewave-desired-state-{nonce}"));
        let path = root.join("device-state.json");
        let mut store = FileDesiredStateStore::new(path.clone());
        let mut state = DesiredState::default();
        state.stage("1-2.3".to_owned(), Wave3Control::Clipguard(true)).expect("stage control");
        state.promote("1-2.3").expect("promote control");
        store.save(&state).expect("save desired state");
        assert_eq!(store.load().expect("load desired state"), state);
        assert!(!root.join(".device-state.json.pending").exists());

        fs::write(&path, br#"{"schema":2,"devices":{}}"#).expect("write new schema");
        assert!(matches!(store.load(), Err(PersistenceError::UnsupportedSchema(2))));
        fs::write(&path, br#"{"schema":1,"devices":{},"old":true}"#).expect("write old field");
        assert!(matches!(store.load(), Err(PersistenceError::Invalid(_))));
        fs::remove_dir_all(root).expect("remove desired-state fixture");
    }

    #[test]
    fn file_store_rejects_wrong_type_mode_and_symlink_without_following_it() {
        let nonce =
            SystemTime::now().duration_since(UNIX_EPOCH).expect("clock before epoch").as_nanos();
        let root = std::env::temp_dir().join(format!("librewave-state-shape-{nonce}"));
        let path = root.join("device-state.json");
        let mut store = FileDesiredStateStore::new(path.clone());
        store.save(&DesiredState::default()).expect("create private state");
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).expect("change mode");
        assert!(matches!(store.load(), Err(PersistenceError::Io(_))));

        fs::remove_file(&path).expect("remove wrong-mode file");
        fs::create_dir(&path).expect("create wrong-type path");
        assert!(matches!(store.load(), Err(PersistenceError::Io(_))));
        fs::remove_dir(&path).expect("remove wrong-type path");

        let target = root.join("target.json");
        fs::write(&target, br#"{"schema":1,"devices":{}}"#).expect("write symlink target");
        fs::set_permissions(&target, fs::Permissions::from_mode(0o600)).expect("set target mode");
        symlink(&target, &path).expect("create state symlink");
        assert!(matches!(store.load(), Err(PersistenceError::Io(_))));
        fs::remove_dir_all(root).expect("remove state-shape fixture");
    }

    #[test]
    fn stale_uncommitted_temp_is_removed_before_load() {
        let nonce =
            SystemTime::now().duration_since(UNIX_EPOCH).expect("clock before epoch").as_nanos();
        let root = std::env::temp_dir().join(format!("librewave-state-crash-{nonce}"));
        fs::DirBuilder::new().mode(0o700).create(&root).expect("create private root");
        let path = root.join("device-state.json");
        let pending = pending_path(&path);
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&pending)
            .expect("create stale pending file");
        file.write_all(b"partial").expect("write stale pending file");
        drop(file);
        let mut store = FileDesiredStateStore::new(path);
        assert_eq!(store.load().expect("recover stale pending file"), DesiredState::default());
        assert!(!pending.exists());
        fs::remove_dir_all(root).expect("remove crash fixture");
    }
}
