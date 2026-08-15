use super::engine::{
    Lifecycle, find_owned, is_uninstalled_state, remove_manifest_backups, remove_superseded_build,
    remove_uncommitted_originals,
};
use super::fs_ops::{
    atomic_copy_new, atomic_json, checked_exists, matches_snapshot_destination, prepare_snapshot,
    read_json, remove_empty, remove_if_exists, remove_transaction_files, restore_snapshot,
    restore_udev_snapshot, verify_saved_snapshots,
};
use super::hash;
use super::model::{Journal, Manifest, Operation, OwnedKind, OwnedPath, Phase, Snapshot};
use super::system::{ProcessInspector, UdevSystem, UnitInspector};
use super::validation::{
    invalid_data, validate_journal, validate_lifecycle_paths, validate_manifest,
    validate_purge_path,
};
use std::fs;
use std::io;
use std::path::Path;

impl<U: UdevSystem, S: UnitInspector, P: ProcessInspector> Lifecycle<U, S, P> {
    pub(super) fn read_journal(&self) -> io::Result<Option<Journal>> {
        validate_lifecycle_paths(&self.paths)?;
        let journal = read_json::<Journal>(&self.paths.journal())?;
        if let Some(journal) = journal.as_ref() {
            validate_journal(journal, &self.paths)?;
        }
        Ok(journal)
    }

    pub(super) fn recover_for_setup(&mut self) -> io::Result<()> {
        let Some(journal) = self.read_journal()? else {
            return Ok(());
        };
        self.validate_recovery_linkage(&journal)?;
        match (&journal.operation, &journal.phase) {
            (Operation::Setup, Phase::Prepared) => {
                remove_uncommitted_originals(&journal)?;
                remove_transaction_files(
                    &self.paths.state_root.join("transactions/setup"),
                    &journal.rollback_paths,
                )?;
                remove_if_exists(&self.paths.journal())
            }
            (Operation::Setup, Phase::Applied)
                if self.read_manifest()?.as_ref() == journal.next.as_ref() =>
            {
                self.finish_setup_recovery(&journal)
            }
            (Operation::Setup, Phase::BackedUp | Phase::Applied) => self.rollback_setup(&journal),
            (Operation::Setup, Phase::ManifestSwitched | Phase::Committed) => {
                self.finish_setup_recovery(&journal)
            }
            (Operation::Uninstall, _) => Err(io::Error::other(
                "an uninstall transaction is incomplete; run `librewavectl uninstall` again",
            )),
        }
    }

    pub(super) fn recover_for_uninstall(&mut self) -> io::Result<()> {
        let Some(journal) = self.read_journal()? else {
            return Ok(());
        };
        self.validate_recovery_linkage(&journal)?;
        if journal.operation == Operation::Setup {
            self.recover_for_setup()?;
            return Ok(());
        }
        if self.read_manifest()?.is_none() {
            let previous = journal
                .previous
                .as_ref()
                .ok_or_else(|| invalid_data("uninstall journal has no previous manifest"))?;
            let complete = previous
                .owned_paths
                .iter()
                .map(is_uninstalled_state)
                .collect::<io::Result<Vec<_>>>()?
                .into_iter()
                .all(|state| state);
            if complete {
                if journal.udev_refresh_pending {
                    self.udev.refresh_wave3_access()?;
                }
                if journal.purge_profiles && checked_exists(&previous.profiles_path)? {
                    validate_purge_path(&previous.profiles_path, &self.paths.config_root)?;
                    fs::remove_dir_all(&previous.profiles_path)?;
                }
                remove_manifest_backups(previous)?;
                self.verify_uninstalled(previous)?;
                remove_if_exists(&self.paths.journal())?;
                remove_empty(&self.paths.backup_root())?;
                remove_empty(&self.paths.state_root)?;
                return Ok(());
            }
            self.write_manifest(previous)?;
        }
        Ok(())
    }

    fn finish_setup_recovery(&mut self, journal: &Journal) -> io::Result<()> {
        if let Some(next) = journal.next.as_ref() {
            self.write_manifest(next)?;
            if let Some(old) = journal.previous.as_ref() {
                remove_superseded_build(old, next)?;
            }
            self.remove_stale_installations(next)?;
        }
        remove_transaction_files(
            &self.paths.state_root.join("transactions/setup"),
            &journal.rollback_paths,
        )?;
        if let Some(next) = journal.next.as_ref() {
            self.verify_installed(next)?;
        }
        remove_if_exists(&self.paths.journal())
    }

    fn validate_recovery_linkage(&self, journal: &Journal) -> io::Result<()> {
        let current = self.read_manifest()?;
        let linked = match (&journal.operation, &journal.phase) {
            (Operation::Setup, Phase::Prepared | Phase::BackedUp)
            | (Operation::Uninstall, Phase::Prepared) => {
                current.as_ref() == journal.previous.as_ref()
            }
            (Operation::Setup, Phase::Applied) => {
                current.as_ref() == journal.previous.as_ref()
                    || current.as_ref() == journal.next.as_ref()
            }
            (Operation::Setup, Phase::ManifestSwitched | Phase::Committed) => {
                current.as_ref() == journal.next.as_ref()
            }
            (Operation::Uninstall, Phase::Applied) => {
                current.is_none() || current.as_ref() == journal.previous.as_ref()
            }
            (Operation::Uninstall, Phase::Committed) => current.is_none(),
            _ => false,
        };
        if linked {
            Ok(())
        } else {
            Err(invalid_data(
                "the installation manifest does not match the interrupted transaction",
            ))
        }
    }

    pub(super) fn rollback_setup(&mut self, journal: &Journal) -> io::Result<()> {
        let next = journal.next.as_ref().ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidData, "setup journal has no next manifest")
        })?;
        verify_saved_snapshots(&journal.rollback_paths)?;
        for (owned, snapshot) in next.owned_paths.iter().zip(&journal.rollback_paths) {
            if !self.matches_owned(owned)? && !matches_snapshot_destination(snapshot)? {
                return Err(io::Error::other(format!(
                    "rollback stopped because {} changed during setup",
                    owned.path.display()
                )));
            }
        }
        for (owned, snapshot) in next.owned_paths.iter().zip(&journal.rollback_paths).rev() {
            if self.matches_owned(owned)? {
                if owned.path == self.paths.udev_rule {
                    restore_udev_snapshot(&mut self.udev, &owned.path, snapshot)?;
                } else {
                    restore_snapshot(&owned.path, snapshot)?;
                }
            } else if !matches_snapshot_destination(snapshot)? {
                return Err(io::Error::other(format!(
                    "rollback stopped because {} changed during setup",
                    owned.path.display()
                )));
            }
        }
        match journal.previous.as_ref() {
            Some(previous) => self.write_manifest(previous)?,
            None => remove_if_exists(&self.paths.manifest())?,
        }
        remove_uncommitted_originals(journal)?;
        remove_transaction_files(
            &self.paths.state_root.join("transactions/setup"),
            &journal.rollback_paths,
        )?;
        remove_if_exists(&self.paths.journal())
    }

    pub(super) fn read_manifest(&self) -> io::Result<Option<Manifest>> {
        validate_lifecycle_paths(&self.paths)?;
        let manifest = read_json::<Manifest>(&self.paths.manifest())?;
        if let Some(manifest) = manifest.as_ref() {
            validate_manifest(manifest, &self.paths)?;
        }
        Ok(manifest)
    }

    pub(super) fn write_manifest(&self, manifest: &Manifest) -> io::Result<()> {
        validate_manifest(manifest, &self.paths)?;
        atomic_json(&self.paths.manifest(), manifest)
    }

    pub(super) fn write_journal(&self, journal: &Journal) -> io::Result<()> {
        validate_journal(journal, &self.paths)?;
        atomic_json(&self.paths.journal(), journal)
    }

    pub(super) fn prepare_snapshots(owned: &[OwnedPath], root: &Path) -> io::Result<Vec<Snapshot>> {
        owned
            .iter()
            .enumerate()
            .map(|(index, entry)| {
                prepare_snapshot(&entry.path, root.join(format!("{index}.backup")))
            })
            .collect()
    }

    pub(super) fn materialize_snapshots(snapshots: &[Snapshot]) -> io::Result<()> {
        for snapshot in snapshots {
            if let Some(saved) = snapshot.saved_content.as_ref() {
                atomic_copy_new(&snapshot.destination, saved, 0o600)?;
                if snapshot.sha256.as_deref() != Some(hash::file(saved)?.as_str()) {
                    return Err(io::Error::other("a transaction snapshot did not verify"));
                }
            }
        }
        Ok(())
    }

    pub(super) fn prepare_original_snapshots(
        &self,
        manifest: &mut Manifest,
        previous: Option<&Manifest>,
    ) -> io::Result<()> {
        for (index, entry) in manifest.owned_paths.iter_mut().enumerate() {
            if entry.path == self.paths.udev_rule {
                continue;
            }
            if entry.original.is_some()
                || previous.is_some_and(|old| find_owned(old, &entry.path).is_some())
            {
                continue;
            }
            if matches!(entry.kind, OwnedKind::Directory) {
                match fs::symlink_metadata(&entry.path) {
                    Ok(metadata) if metadata.is_dir() => entry.created_by_librewave = false,
                    Ok(_) => {
                        return Err(io::Error::new(
                            io::ErrorKind::AlreadyExists,
                            format!(
                                "refused to replace the non-directory at {}",
                                entry.path.display()
                            ),
                        ));
                    }
                    Err(error) if error.kind() == io::ErrorKind::NotFound => {
                        entry.created_by_librewave = true;
                    }
                    Err(error) => return Err(error),
                }
                continue;
            }
            let backup_path = self.paths.backup_root().join(format!("original-{index}"));
            if checked_exists(&backup_path)? {
                return Err(io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    format!("refused to overwrite the stale backup at {}", backup_path.display()),
                ));
            }
            let original = prepare_snapshot(&entry.path, backup_path)?;
            if original.kind.is_none() {
                continue;
            }
            if matches!(original.kind, Some(OwnedKind::Directory)) {
                return Err(io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    format!(
                        "refused to replace the existing directory at {}",
                        entry.path.display()
                    ),
                ));
            }
            entry.original = Some(original);
        }
        Ok(())
    }

    pub(super) fn materialize_original_snapshots(
        manifest: &Manifest,
        previous: Option<&Manifest>,
    ) -> io::Result<()> {
        for entry in &manifest.owned_paths {
            let Some(original) = entry.original.as_ref() else {
                continue;
            };
            if previous
                .and_then(|old| find_owned(old, &entry.path))
                .and_then(|owned| owned.original.as_ref())
                == Some(original)
            {
                continue;
            }
            if let Some(saved) = original.saved_content.as_ref() {
                atomic_copy_new(&original.destination, saved, 0o600)?;
                if original.sha256.as_deref() != Some(hash::file(saved)?.as_str()) {
                    return Err(io::Error::other("an original-file backup did not verify"));
                }
            }
        }
        Ok(())
    }
}
