use super::engine::{InstallRequest, Lifecycle, find_owned};
use super::fs_ops::{atomic_copy, atomic_symlink, atomic_write, matches_snapshot_destination};
use super::hash;
use super::model::{AudioOwnership, MANIFEST_SCHEMA, Manifest, OwnedKind, OwnedPath, Snapshot};
use super::system::{ProcessInspector, UdevSystem, UnitInspector};
use super::validation::validate_systemd_path;
use librewave_platform_linux::audio_policy::Wave3AudioPolicy;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

impl<U: UdevSystem, S: UnitInspector, P: ProcessInspector> Lifecycle<U, S, P> {
    pub(super) fn build_manifest(
        &self,
        request: &InstallRequest<'_>,
        previous: Option<&Manifest>,
    ) -> io::Result<Manifest> {
        let build = self.paths.installations().join(request.identity.stable_key());
        let cli = build.join("bin/librewavectl");
        let daemon = build.join("bin/librewaved");
        let active = self.paths.active_link();
        let unit = self.paths.unit();
        validate_systemd_path(&active.join("bin/librewaved"))?;
        let udev_content = Wave3AudioPolicy::new().render_udev();
        let unit_content = render_unit(&active.join("bin/librewaved"));
        let values = [
            directory(self.paths.data_root.clone()),
            directory(self.paths.installations()),
            directory(build.clone()),
            directory(build.join("bin")),
            file_entry(cli, request.identity.cli_sha256.clone(), 0o755),
            file_entry(daemon, request.identity.daemon_sha256.clone(), 0o755),
            file_entry(unit.clone(), hash::bytes(unit_content.as_bytes()), 0o644),
            file_entry(self.paths.udev_rule.clone(), hash::bytes(udev_content.as_bytes()), 0o644),
            symlink_entry(active.clone(), build.clone()),
            symlink_entry(
                self.paths.bin_root.join("librewavectl"),
                active.join("bin/librewavectl"),
            ),
            symlink_entry(self.paths.bin_root.join("librewaved"), active.join("bin/librewaved")),
        ];
        let mut owned_paths = Vec::from(values);
        for entry in &mut owned_paths {
            if let Some(old) = previous.and_then(|manifest| find_owned(manifest, &entry.path)) {
                entry.original.clone_from(&old.original);
                entry.created_by_librewave = old.created_by_librewave;
            }
        }
        Ok(Manifest {
            schema_version: MANIFEST_SCHEMA,
            identity: request.identity.clone(),
            active_build_directory: build,
            active_link: active,
            unit_path: unit,
            udev_policy_path: self.paths.udev_rule.clone(),
            wireplumber_policy_path: self.paths.wireplumber(),
            profiles_path: self.paths.profiles(),
            audio_ownership: AudioOwnership::BlockedMissingProductionHost,
            owned_paths,
        })
    }

    pub(super) fn apply_manifest(
        &mut self,
        manifest: &Manifest,
        request: &InstallRequest<'_>,
        rollback: &[Snapshot],
    ) -> io::Result<()> {
        let cli_target = manifest.active_build_directory.join("bin/librewavectl");
        let daemon_target = manifest.active_build_directory.join("bin/librewaved");
        let unit_content = render_unit(&manifest.active_link.join("bin/librewaved"));
        let udev_content = Wave3AudioPolicy::new().render_udev();
        for (owned, snapshot) in manifest.owned_paths.iter().zip(rollback) {
            let unchanged = if owned.path == manifest.udev_policy_path {
                matches_udev_snapshot(&self.udev, snapshot)?
            } else {
                matches_snapshot_destination(snapshot)?
            };
            if !unchanged {
                return Err(io::Error::other(format!(
                    "the destination changed after setup preflight: {}",
                    owned.path.display()
                )));
            }
            match owned.kind {
                OwnedKind::Directory => fs::create_dir_all(&owned.path)?,
                OwnedKind::File if owned.path == cli_target => {
                    if owned.sha256.as_deref() != Some(hash::file(request.cli_source)?.as_str()) {
                        return Err(io::Error::new(
                            io::ErrorKind::InvalidData,
                            "the CLI source changed after setup validation",
                        ));
                    }
                    atomic_copy(request.cli_source, &owned.path, 0o755)?;
                }
                OwnedKind::File if owned.path == daemon_target => {
                    if owned.sha256.as_deref() != Some(hash::file(request.daemon_source)?.as_str())
                    {
                        return Err(io::Error::new(
                            io::ErrorKind::InvalidData,
                            "the daemon source changed after setup validation",
                        ));
                    }
                    atomic_copy(request.daemon_source, &owned.path, 0o755)?;
                }
                OwnedKind::File if owned.path == manifest.unit_path => {
                    atomic_write(&owned.path, unit_content.as_bytes(), 0o644)?;
                }
                OwnedKind::File if owned.path == manifest.udev_policy_path => {
                    self.udev.install_and_refresh(&owned.path, udev_content.as_bytes())?;
                }
                OwnedKind::File => {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "unknown file artifact",
                    ));
                }
                OwnedKind::Symlink => atomic_symlink(
                    owned.link_target.as_deref().ok_or_else(|| {
                        io::Error::new(io::ErrorKind::InvalidData, "symlink target is missing")
                    })?,
                    &owned.path,
                )?,
            }
        }
        #[cfg(test)]
        if self.fail_after_apply {
            return Err(io::Error::other("injected failure after artifact application"));
        }
        Ok(())
    }
}

fn matches_udev_snapshot<U: UdevSystem>(udev: &U, snapshot: &Snapshot) -> io::Result<bool> {
    let current = udev.read(&snapshot.destination)?;
    match snapshot.kind {
        None => Ok(current.is_none()),
        Some(OwnedKind::File) => Ok(current.as_deref().is_some_and(|content| {
            snapshot.sha256.as_deref() == Some(hash::bytes(content).as_str())
                && snapshot.mode == Some(0o644)
        })),
        _ => Ok(false),
    }
}

pub(super) fn render_unit(daemon: &Path) -> String {
    format!(
        "[Unit]\nDescription=LibreWave read-only device service\n\n[Service]\nExecStart={}\nRestart=on-failure\n\n[Install]\nWantedBy=default.target\n",
        daemon.display()
    )
}

fn directory(path: PathBuf) -> OwnedPath {
    OwnedPath {
        path,
        kind: OwnedKind::Directory,
        sha256: None,
        link_target: None,
        mode: None,
        created_by_librewave: false,
        original: None,
    }
}

fn file_entry(path: PathBuf, sha256: String, mode: u32) -> OwnedPath {
    OwnedPath {
        path,
        kind: OwnedKind::File,
        sha256: Some(sha256),
        link_target: None,
        mode: Some(mode),
        created_by_librewave: false,
        original: None,
    }
}

fn symlink_entry(path: PathBuf, target: PathBuf) -> OwnedPath {
    OwnedPath {
        path,
        kind: OwnedKind::Symlink,
        sha256: None,
        link_target: Some(target),
        mode: None,
        created_by_librewave: false,
        original: None,
    }
}
