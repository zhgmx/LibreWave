use super::fs_ops::{
    checked_exists, matches_entry, matches_snapshot_destination, remove_empty, remove_entry,
    remove_if_exists, remove_transaction_files, restore_or_remove, saved_snapshot_is_valid,
};
use super::hash;
use super::model::{
    BuildIdentity, Journal, MANIFEST_SCHEMA, Manifest, Operation, OwnedKind, OwnedPath, Phase,
};
use super::system::{ProcessInspector, UdevSystem, UnitInspector};
use super::validation::{validate_manifest, validate_purge_path};
use librewave_platform_linux::audio_policy::{UDEV_POLICY_FILE_NAME, Wave3AudioPolicy};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

pub(super) const UNIT_FILE_NAME: &str = "librewaved.service";
const WIREPLUMBER_FILE_NAME: &str = "80-librewave-wave3.conf";

#[derive(Clone, Debug)]
pub struct InstallPaths {
    pub data_root: PathBuf,
    pub state_root: PathBuf,
    pub config_root: PathBuf,
    pub bin_root: PathBuf,
    pub udev_rule: PathBuf,
}

impl InstallPaths {
    pub fn from_environment() -> io::Result<Self> {
        let home = std::env::var_os("HOME")
            .map(PathBuf::from)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "HOME is not set"))?;
        Ok(Self {
            data_root: env_path("XDG_DATA_HOME", home.join(".local/share")).join("librewave"),
            state_root: env_path("XDG_STATE_HOME", home.join(".local/state")).join("librewave"),
            config_root: env_path("XDG_CONFIG_HOME", home.join(".config")),
            bin_root: env_path("XDG_BIN_HOME", home.join(".local/bin")),
            udev_rule: PathBuf::from("/etc/udev/rules.d").join(UDEV_POLICY_FILE_NAME),
        })
    }

    pub(super) fn manifest(&self) -> PathBuf {
        self.state_root.join("install-manifest.json")
    }

    pub(super) fn journal(&self) -> PathBuf {
        self.state_root.join("install-journal.json")
    }

    pub(super) fn installations(&self) -> PathBuf {
        self.data_root.join("installations")
    }

    pub(super) fn active_link(&self) -> PathBuf {
        self.data_root.join("active")
    }

    pub(super) fn unit(&self) -> PathBuf {
        self.config_root.join("systemd/user").join(UNIT_FILE_NAME)
    }

    pub(super) fn wireplumber(&self) -> PathBuf {
        self.config_root.join("wireplumber/wireplumber.conf.d").join(WIREPLUMBER_FILE_NAME)
    }

    pub(super) fn profiles(&self) -> PathBuf {
        self.config_root.join("librewave/profiles")
    }

    pub(super) fn backup_root(&self) -> PathBuf {
        self.state_root.join("backups")
    }
}

fn env_path(name: &str, default: PathBuf) -> PathBuf {
    std::env::var_os(name).map_or(default, PathBuf::from)
}

pub struct InstallRequest<'a> {
    pub identity: BuildIdentity,
    pub cli_source: &'a Path,
    pub daemon_source: &'a Path,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CheckState {
    Pass,
    Fail,
    Blocked,
    NotImplemented,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Check {
    pub state: CheckState,
    pub name: String,
    pub detail: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum UninstallOutcome {
    Removed,
    NotInstalled,
    Modified(Vec<PathBuf>),
}

pub struct Lifecycle<U, S, P> {
    pub(super) paths: InstallPaths,
    pub(super) udev: U,
    pub(super) unit_inspector: S,
    pub(super) process_inspector: P,
    #[cfg(test)]
    pub(super) fail_after_apply: bool,
    #[cfg(test)]
    fail_after_backup_cleanup: bool,
    #[cfg(test)]
    fail_after_manifest_switch: bool,
    #[cfg(test)]
    mutate_before_apply: Option<(PathBuf, Vec<u8>)>,
}

impl<U: UdevSystem, S: UnitInspector, P: ProcessInspector> Lifecycle<U, S, P> {
    pub fn new(paths: InstallPaths, udev: U, unit_inspector: S, process_inspector: P) -> Self {
        Self {
            paths,
            udev,
            unit_inspector,
            process_inspector,
            #[cfg(test)]
            fail_after_apply: false,
            #[cfg(test)]
            fail_after_backup_cleanup: false,
            #[cfg(test)]
            fail_after_manifest_switch: false,
            #[cfg(test)]
            mutate_before_apply: None,
        }
    }

    pub fn setup(&mut self, request: &InstallRequest<'_>) -> io::Result<Manifest> {
        self.preflight_runtime()?;
        self.recover_for_setup()?;
        validate_source(request.cli_source, &request.identity.cli_sha256)?;
        validate_source(request.daemon_source, &request.identity.daemon_sha256)?;
        let previous = self.read_manifest()?;
        if let Some(previous) = previous.as_ref() {
            self.preflight_upgrade(previous)?;
        }
        self.preflight_installations(previous.as_ref())?;
        let exact_udev = Wave3AudioPolicy::new().render_udev();
        if let Some(existing) = self.udev.read(&self.paths.udev_rule)? {
            let previously_owned = previous
                .as_ref()
                .and_then(|manifest| find_owned(manifest, &self.paths.udev_rule))
                .is_some();
            if existing != exact_udev.as_bytes() || !previously_owned {
                return Err(io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    format!(
                        "refused to replace the unowned or modified udev rule at {}",
                        self.paths.udev_rule.display()
                    ),
                ));
            }
        }
        let mut next = self.build_manifest(request, previous.as_ref())?;
        let transaction_root = self.paths.state_root.join("transactions/setup");
        self.prepare_original_snapshots(&mut next, previous.as_ref())?;
        validate_manifest(&next, &self.paths)?;
        let rollback_paths = Self::prepare_snapshots(&next.owned_paths, &transaction_root)?;
        let mut journal = Journal {
            schema_version: MANIFEST_SCHEMA,
            operation: Operation::Setup,
            phase: Phase::Prepared,
            previous: previous.clone(),
            next: Some(next.clone()),
            rollback_paths,
            udev_refresh_pending: false,
            purge_profiles: false,
        };
        self.write_journal(&journal)?;
        Self::materialize_snapshots(&journal.rollback_paths)?;
        Self::materialize_original_snapshots(&next, previous.as_ref())?;
        journal.phase = Phase::BackedUp;
        self.write_journal(&journal)?;
        #[cfg(test)]
        if let Some((path, content)) = self.mutate_before_apply.take() {
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent)?;
            }
            fs::write(path, content)?;
        }
        if let Err(error) = self.apply_manifest(&next, request, &journal.rollback_paths) {
            let rollback = self.rollback_setup(&journal);
            return match rollback {
                Ok(()) => Err(error),
                Err(rollback_error) => Err(io::Error::other(format!(
                    "setup failed: {error}; rollback also failed: {rollback_error}"
                ))),
            };
        }
        for owned in &next.owned_paths {
            let verification = self.matches_owned(owned);
            if !matches!(verification, Ok(true)) {
                let detail = match verification {
                    Ok(false) => "does not match its manifest".to_owned(),
                    Err(error) => error.to_string(),
                    Ok(true) => unreachable!(),
                };
                let error = io::Error::other(format!(
                    "a staged artifact failed verification at {}: {detail}",
                    owned.path.display(),
                ));
                let rollback = self.rollback_setup(&journal);
                return match rollback {
                    Ok(()) => Err(error),
                    Err(rollback_error) => Err(io::Error::other(format!(
                        "setup verification failed: {error}; rollback also failed: {rollback_error}"
                    ))),
                };
            }
        }
        journal.phase = Phase::Applied;
        self.write_journal(&journal)?;
        self.switch_manifest(&next)?;
        journal.phase = Phase::ManifestSwitched;
        self.write_journal(&journal)?;
        if let Some(old) = previous.as_ref() {
            remove_superseded_build(old, &next)?;
        }
        remove_transaction_files(&transaction_root, &journal.rollback_paths)?;
        self.remove_stale_installations(&next)?;
        self.verify_installed(&next)?;
        journal.phase = Phase::Committed;
        self.write_journal(&journal)?;
        remove_if_exists(&self.paths.journal())?;
        Ok(next)
    }

    fn switch_manifest(&self, next: &Manifest) -> io::Result<()> {
        self.write_manifest(next)?;
        #[cfg(test)]
        if self.fail_after_manifest_switch {
            return Err(io::Error::other("injected failure after manifest switch"));
        }
        Ok(())
    }

    pub fn uninstall(&mut self, purge_profiles: bool) -> io::Result<UninstallOutcome> {
        self.preflight_runtime()?;
        let interrupted = self.read_journal()?;
        let recovering = interrupted.is_some();
        if let Some(journal) = interrupted.as_ref()
            && journal.operation == Operation::Uninstall
            && journal.purge_profiles != purge_profiles
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "the interrupted uninstall must resume with the same profile purge choice",
            ));
        }
        self.recover_for_uninstall()?;
        let Some(manifest) = self.read_manifest()? else {
            return Ok(if recovering {
                UninstallOutcome::Removed
            } else {
                UninstallOutcome::NotInstalled
            });
        };
        let modified = self.preflight_uninstall(&manifest)?;
        if !modified.is_empty() {
            return Ok(UninstallOutcome::Modified(modified));
        }
        let mut journal = Journal {
            schema_version: MANIFEST_SCHEMA,
            operation: Operation::Uninstall,
            phase: Phase::Prepared,
            previous: Some(manifest.clone()),
            next: None,
            rollback_paths: Vec::new(),
            udev_refresh_pending: manifest
                .owned_paths
                .iter()
                .any(|entry| entry.path == self.paths.udev_rule),
            purge_profiles,
        };
        self.write_journal(&journal)?;
        if let Some(udev) =
            manifest.owned_paths.iter().find(|owned| owned.path == self.paths.udev_rule)
            && !is_uninstalled_state(udev)?
        {
            if !self.matches_owned(udev)? {
                return Err(io::Error::other("the udev rule changed after uninstall preflight"));
            }
            self.restore_udev(udev)?;
            journal.udev_refresh_pending = false;
            self.write_journal(&journal)?;
        }
        if journal.udev_refresh_pending {
            self.udev.refresh_wave3_access()?;
            journal.udev_refresh_pending = false;
            self.write_journal(&journal)?;
        }
        for owned in manifest.owned_paths.iter().rev() {
            if owned.path == self.paths.udev_rule {
                continue;
            }
            if is_uninstalled_state(owned)? {
                continue;
            }
            if !self.matches_owned(owned)? {
                return Err(io::Error::other(format!(
                    "the owned path changed after uninstall preflight: {}",
                    owned.path.display()
                )));
            }
            if let Some(original) = owned.original.as_ref()
                && !saved_snapshot_is_valid(original)?
            {
                return Err(io::Error::other(format!(
                    "the backup changed after uninstall preflight: {}",
                    owned.path.display()
                )));
            }
            restore_or_remove(owned)?;
        }
        if purge_profiles && checked_exists(&manifest.profiles_path)? {
            validate_purge_path(&manifest.profiles_path, &self.paths.config_root)?;
            fs::remove_dir_all(&manifest.profiles_path)?;
        }
        self.verify_uninstalled(&manifest)?;
        journal.phase = Phase::Applied;
        self.write_journal(&journal)?;
        remove_if_exists(&self.paths.manifest())?;
        validate_manifest(&manifest, &self.paths)?;
        remove_manifest_backups(&manifest)?;
        #[cfg(test)]
        if self.fail_after_backup_cleanup {
            return Err(io::Error::other("injected failure after backup cleanup"));
        }
        journal.phase = Phase::Committed;
        self.write_journal(&journal)?;
        remove_if_exists(&self.paths.journal())?;
        remove_empty(&self.paths.backup_root())?;
        remove_empty(&self.paths.state_root)?;
        Ok(UninstallOutcome::Removed)
    }

    pub fn doctor(&self, expected: Option<&BuildIdentity>) -> Vec<Check> {
        let mut checks = Vec::new();
        match self.read_journal() {
            Ok(Some(_)) => checks.push(fail(
                "transaction journal",
                format!(
                    "A valid interrupted transaction remains at {}. Run setup or uninstall again.",
                    self.paths.journal().display()
                ),
            )),
            Ok(None) => {
                checks.push(pass("transaction journal", "No interrupted transaction exists."));
            }
            Err(error) => checks.push(fail(
                "transaction journal",
                format!("The journal is invalid and will not be used: {error}"),
            )),
        }
        let manifest = match self.read_manifest() {
            Ok(Some(manifest)) => manifest,
            Ok(None) => {
                checks.push(fail("installation manifest", "LibreWave is not installed."));
                checks.extend(self.orphan_checks());
                checks.push(blocked_audio());
                checks.push(not_implemented_graph());
                return checks;
            }
            Err(error) => {
                checks.push(fail(
                    "installation manifest",
                    format!("The manifest is invalid and will not be used: {error}"),
                ));
                checks.extend(self.orphan_checks());
                checks.push(blocked_audio());
                checks.push(not_implemented_graph());
                return checks;
            }
        };
        checks.push(pass(
            "installation manifest",
            format!("Schema {} is valid.", manifest.schema_version),
        ));
        for owned in &manifest.owned_paths {
            match self.matches_owned(owned) {
                Ok(true) => checks.push(pass(
                    format!("owned path {}", owned.path.display()),
                    "Type, content, mode, or link target matches the manifest.",
                )),
                Ok(false) => checks.push(fail(
                    format!("owned path {}", owned.path.display()),
                    "The path is missing or does not match the manifest.",
                )),
                Err(error) => checks.push(fail(
                    format!("owned path {}", owned.path.display()),
                    format!("The path cannot be inspected safely: {error}"),
                )),
            }
        }
        if let Some(expected) = expected {
            if manifest.identity.same_build(expected) {
                checks.push(pass(
                    "current build identity",
                    "The installed build matches this checkout.",
                ));
            } else {
                checks.push(fail(
                    "current build identity",
                    "The installed revision, source fingerprint, target, profile, or binary hash differs from this checkout.",
                ));
            }
        }
        checks.extend(self.process_checks(&manifest));
        checks.extend(self.stale_artifact_checks(&manifest));
        checks.extend(self.backup_artifact_checks(&manifest));
        checks.push(self.unit_state_check());
        match checked_exists(&manifest.wireplumber_policy_path) {
            Ok(true) => checks.push(fail(
                "WirePlumber ownership policy",
                format!(
                    "A stale LibreWave card-disable rule exists at {}. This build does not own or activate it.",
                    manifest.wireplumber_policy_path.display()
                ),
            )),
            Ok(false) => checks.push(blocked(
                "WirePlumber ownership policy",
                "Not installed because the production ALSA/PipeWire host is not implemented.",
            )),
            Err(error) => checks.push(fail(
                "WirePlumber ownership policy",
                format!("Cannot inspect the blocked policy path: {error}"),
            )),
        }
        checks.push(blocked_audio());
        checks.push(not_implemented_graph());
        checks
    }

    pub(super) fn matches_owned(&self, owned: &OwnedPath) -> io::Result<bool> {
        if owned.path == self.paths.udev_rule {
            let current = self.udev.read(&owned.path)?;
            return Ok(current.as_deref().is_some_and(|value| {
                owned.sha256.as_deref() == Some(hash::bytes(value).as_str())
            }));
        }
        matches_entry(owned)
    }

    fn restore_udev(&mut self, owned: &OwnedPath) -> io::Result<()> {
        match owned.original.as_ref() {
            Some(_) => Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "the exact udev path cannot have foreign original content",
            )),
            None => self.udev.remove_and_refresh(&owned.path),
        }
    }

    fn preflight_uninstall(&self, manifest: &Manifest) -> io::Result<Vec<PathBuf>> {
        let mut modified = Vec::new();
        for owned in &manifest.owned_paths {
            if is_uninstalled_state(owned)? {
                continue;
            }
            if let Some(original) = owned.original.as_ref()
                && !saved_snapshot_is_valid(original)?
            {
                modified.push(
                    original.saved_content.clone().unwrap_or_else(|| original.destination.clone()),
                );
                continue;
            }
            if self.matches_owned(owned)? {
                if matches!(owned.kind, OwnedKind::Directory)
                    && owned.created_by_librewave
                    && directory_has_foreign_children(&owned.path, &manifest.owned_paths)?
                {
                    modified.push(owned.path.clone());
                }
                continue;
            }
            modified.push(owned.path.clone());
        }
        Ok(modified)
    }

    fn preflight_upgrade(&self, manifest: &Manifest) -> io::Result<()> {
        for owned in &manifest.owned_paths {
            if let Some(original) = owned.original.as_ref()
                && !saved_snapshot_is_valid(original)?
            {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!(
                        "cannot upgrade because the original backup for {} is missing or corrupt",
                        owned.path.display()
                    ),
                ));
            }
            if !self.matches_owned(owned)? {
                return Err(io::Error::other(format!(
                    "cannot upgrade because the installed path was changed: {}",
                    owned.path.display()
                )));
            }
        }
        Ok(())
    }

    fn preflight_installations(&self, previous: Option<&Manifest>) -> io::Result<()> {
        let entries = match fs::read_dir(self.paths.installations()) {
            Ok(entries) => entries,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(error),
        };
        for entry in entries {
            let entry = entry?;
            let file_type = entry.file_type()?;
            let allowed = previous.is_some_and(|manifest| {
                entry.path() == manifest.active_build_directory && file_type.is_dir()
            });
            if !allowed {
                return Err(io::Error::other(format!(
                    "an unmanifested installation entry blocks setup: {}",
                    entry.path().display()
                )));
            }
        }
        Ok(())
    }

    fn preflight_runtime(&self) -> io::Result<()> {
        let unit = self.unit_inspector.inspect(UNIT_FILE_NAME)?;
        if unit.enabled || unit.active {
            return Err(io::Error::other(
                "the safety-gated unit is enabled or active; run `systemctl --user disable --now librewaved.service` first",
            ));
        }
        let daemons = self.process_inspector.daemons()?;
        if daemons.is_empty() {
            if checked_exists(&self.paths.wireplumber())? {
                Err(io::Error::other(format!(
                    "a stale WirePlumber ownership rule exists at {}; remove it before setup or uninstall",
                    self.paths.wireplumber().display()
                )))
            } else {
                Ok(())
            }
        } else {
            Err(io::Error::other(format!(
                "a librewaved process is still running (PID {}); stop it before setup or uninstall",
                daemons[0].pid
            )))
        }
    }

    pub(super) fn verify_installed(&self, manifest: &Manifest) -> io::Result<()> {
        self.preflight_runtime()?;
        for owned in &manifest.owned_paths {
            if !self.matches_owned(owned)? {
                return Err(io::Error::other(format!(
                    "installed path failed verification: {}",
                    owned.path.display()
                )));
            }
        }
        if checked_exists(&manifest.wireplumber_policy_path)? {
            return Err(io::Error::other(format!(
                "the blocked WirePlumber rule exists at {}",
                manifest.wireplumber_policy_path.display()
            )));
        }
        let entries = fs::read_dir(self.paths.installations())?.collect::<Result<Vec<_>, _>>()?;
        if entries.len() != 1 || entries[0].path() != manifest.active_build_directory {
            return Err(io::Error::other("stale installation directories remain"));
        }
        for check in self
            .stale_artifact_checks(manifest)
            .into_iter()
            .chain(self.backup_artifact_checks(manifest))
        {
            if check.state == CheckState::Fail {
                return Err(io::Error::other(check.detail));
            }
        }
        Ok(())
    }

    pub(super) fn verify_uninstalled(&self, manifest: &Manifest) -> io::Result<()> {
        self.preflight_runtime()?;
        for owned in &manifest.owned_paths {
            if !is_uninstalled_state(owned)? {
                return Err(io::Error::other(format!(
                    "owned path remained after uninstall: {}",
                    owned.path.display()
                )));
            }
        }
        if checked_exists(&manifest.wireplumber_policy_path)? {
            return Err(io::Error::other(format!(
                "a stale WirePlumber rule remains at {}",
                manifest.wireplumber_policy_path.display()
            )));
        }
        for check in self.stale_artifact_checks(manifest) {
            if check.state == CheckState::Fail {
                return Err(io::Error::other(check.detail));
            }
        }
        Ok(())
    }

    pub(super) fn remove_stale_installations(&self, active: &Manifest) -> io::Result<()> {
        let root = self.paths.installations();
        let entries = match fs::read_dir(&root) {
            Ok(entries) => entries,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(error),
        };
        for entry in entries {
            let path = entry?.path();
            if path != active.active_build_directory {
                return Err(io::Error::other(format!(
                    "an unmanifested installation directory remains: {}",
                    path.display()
                )));
            }
        }
        Ok(())
    }

    #[cfg(test)]
    pub(super) fn inject_failure_after_apply(&mut self) {
        self.fail_after_apply = true;
    }

    #[cfg(test)]
    pub(super) fn inject_failure_after_backup_cleanup(&mut self) {
        self.fail_after_backup_cleanup = true;
    }

    #[cfg(test)]
    pub(super) fn inject_failure_after_manifest_switch(&mut self) {
        self.fail_after_manifest_switch = true;
    }

    #[cfg(test)]
    pub(super) fn inject_mutation_before_apply(&mut self, path: PathBuf, content: Vec<u8>) {
        self.mutate_before_apply = Some((path, content));
    }
}

fn validate_source(path: &Path, expected: &str) -> io::Result<()> {
    if hash::file(path)? == expected {
        Ok(())
    } else {
        Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "a source binary changed after its identity was recorded",
        ))
    }
}

pub(super) fn remove_superseded_build(old: &Manifest, new: &Manifest) -> io::Result<()> {
    if old.active_build_directory == new.active_build_directory {
        return Ok(());
    }
    for entry in old
        .owned_paths
        .iter()
        .rev()
        .filter(|entry| entry.path.starts_with(&old.active_build_directory))
    {
        if matches_entry(entry)? {
            remove_entry(&entry.path, &entry.kind)?;
        }
    }
    Ok(())
}

pub(super) fn is_uninstalled_state(owned: &OwnedPath) -> io::Result<bool> {
    if let Some(original) = owned.original.as_ref() {
        return matches_snapshot_destination(original);
    }
    if matches!(owned.kind, OwnedKind::Directory) && !owned.created_by_librewave {
        return checked_exists(&owned.path);
    }
    Ok(!checked_exists(&owned.path)?)
}

pub(super) fn remove_uncommitted_originals(journal: &Journal) -> io::Result<()> {
    let Some(next) = journal.next.as_ref() else {
        return Ok(());
    };
    for entry in &next.owned_paths {
        let carried = journal
            .previous
            .as_ref()
            .and_then(|manifest| find_owned(manifest, &entry.path))
            .and_then(|owned| owned.original.as_ref());
        if carried == entry.original.as_ref() {
            continue;
        }
        if let Some(original) = entry.original.as_ref()
            && let Some(saved) = original.saved_content.as_ref()
        {
            if checked_exists(saved)? && !saved_snapshot_is_valid(original)? {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("refused to remove the modified backup at {}", saved.display()),
                ));
            }
            remove_if_exists(saved)?;
        }
    }
    Ok(())
}

pub(super) fn find_owned<'a>(manifest: &'a Manifest, path: &Path) -> Option<&'a OwnedPath> {
    manifest.owned_paths.iter().find(|entry| entry.path == path)
}

pub(super) fn remove_manifest_backups(manifest: &Manifest) -> io::Result<()> {
    for snapshot in manifest.owned_paths.iter().filter_map(|entry| entry.original.as_ref()) {
        let Some(saved) = snapshot.saved_content.as_ref() else {
            continue;
        };
        if checked_exists(saved)? && !saved_snapshot_is_valid(snapshot)? {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("refused to remove the modified backup at {}", saved.display()),
            ));
        }
        remove_if_exists(saved)?;
    }
    Ok(())
}

fn directory_has_foreign_children(path: &Path, owned: &[OwnedPath]) -> io::Result<bool> {
    let entries = match fs::read_dir(path) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error),
    };
    for entry in entries {
        let child = entry?.path();
        if !owned
            .iter()
            .any(|candidate| candidate.path == child || candidate.path.starts_with(&child))
        {
            return Ok(true);
        }
    }
    Ok(false)
}

pub(super) fn pass(name: impl Into<String>, detail: impl Into<String>) -> Check {
    Check { state: CheckState::Pass, name: name.into(), detail: detail.into() }
}

pub(super) fn fail(name: impl Into<String>, detail: impl Into<String>) -> Check {
    Check { state: CheckState::Fail, name: name.into(), detail: detail.into() }
}

fn blocked(name: impl Into<String>, detail: impl Into<String>) -> Check {
    Check { state: CheckState::Blocked, name: name.into(), detail: detail.into() }
}

fn blocked_audio() -> Check {
    blocked(
        "production audio ownership",
        "Blocked because the production ALSA/PipeWire host is not implemented. The physical Wave card remains under desktop control.",
    )
}

fn not_implemented_graph() -> Check {
    Check {
        state: CheckState::NotImplemented,
        name: "PipeWire graph objects".to_owned(),
        detail: "Not checked because LibreWave does not yet publish a production graph.".to_owned(),
    }
}

#[cfg(test)]
pub fn test_paths(root: &Path) -> InstallPaths {
    InstallPaths {
        data_root: root.join("data/librewave"),
        state_root: root.join("state/librewave"),
        config_root: root.join("config"),
        bin_root: root.join("bin"),
        udev_rule: root.join("system/70-librewave-wave3.rules"),
    }
}
