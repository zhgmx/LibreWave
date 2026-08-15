use super::engine::{Check, InstallPaths, Lifecycle, UNIT_FILE_NAME, fail, pass};
use super::fs_ops::{checked_exists, saved_snapshot_is_valid};
use super::model::Manifest;
use super::system::{ProcessInspector, UdevSystem, UnitInspector};
use std::fs;
use std::io;
use std::path::Path;

impl<U: UdevSystem, S: UnitInspector, P: ProcessInspector> Lifecycle<U, S, P> {
    pub(super) fn process_checks(&self, manifest: &Manifest) -> Vec<Check> {
        let expected = manifest.active_build_directory.join("bin/librewaved");
        let processes = match self.process_inspector.daemons() {
            Ok(processes) => processes,
            Err(error) => {
                return vec![fail(
                    "running daemon",
                    format!("Cannot inspect running daemon processes: {error}"),
                )];
            }
        };
        if processes.is_empty() {
            return vec![pass("running daemon", "No LibreWave daemon process is running.")];
        }
        processes
            .into_iter()
            .map(|process| {
                let status = if !process.deleted
                    && process.executable == expected
                    && process.sha256.as_deref() == Some(manifest.identity.daemon_sha256.as_str())
                {
                    "current installed"
                } else {
                    "stale, modified, or deleted"
                };
                fail(
                    "running daemon",
                    format!(
                        "Process {} uses the {status} daemon at {}. Stop it before changing this safety-gated installation.",
                        process.pid,
                        process.executable.display()
                    ),
                )
            })
            .collect()
    }

    pub(super) fn backup_artifact_checks(&self, manifest: &Manifest) -> Vec<Check> {
        let expected = manifest
            .owned_paths
            .iter()
            .filter_map(|owned| owned.original.as_ref())
            .filter_map(|snapshot| snapshot.saved_content.as_ref())
            .collect::<std::collections::BTreeSet<_>>();
        let mut checks = Vec::new();
        for owned in &manifest.owned_paths {
            let Some(snapshot) = owned.original.as_ref() else {
                continue;
            };
            match saved_snapshot_is_valid(snapshot) {
                Ok(true) => checks.push(pass(
                    format!("backup for {}", owned.path.display()),
                    "The original backup matches its recorded hash and mode.",
                )),
                Ok(false) => checks.push(fail(
                    format!("backup for {}", owned.path.display()),
                    "The original backup is missing, modified, or has the wrong mode.",
                )),
                Err(error) => checks.push(fail(
                    format!("backup for {}", owned.path.display()),
                    format!("Cannot inspect the original backup: {error}"),
                )),
            }
        }
        match fs::read_dir(self.paths.backup_root()) {
            Ok(entries) => {
                for entry in entries {
                    match entry {
                        Ok(entry) if !expected.contains(&entry.path()) => checks.push(fail(
                            "unexpected installation backup",
                            format!("Unowned backup remains at {}.", entry.path().display()),
                        )),
                        Ok(_) => {}
                        Err(error) => checks.push(scan_failure(
                            "installation backup scan",
                            &self.paths.backup_root(),
                            &error,
                        )),
                    }
                }
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => checks.push(scan_failure(
                "installation backup scan",
                &self.paths.backup_root(),
                &error,
            )),
        }
        let transactions = self.paths.state_root.join("transactions");
        match checked_exists(&transactions) {
            Ok(true) => checks.push(fail(
                "stale transaction directory",
                format!("Transaction artifacts remain at {}.", transactions.display()),
            )),
            Ok(false) => {}
            Err(error) => {
                checks.push(scan_failure(
                    "transaction directory inspection",
                    &transactions,
                    &error,
                ));
            }
        }
        if checks.is_empty() {
            checks.push(pass("installation backups", "No original backup is required."));
        }
        checks
    }

    pub(super) fn stale_artifact_checks(&self, manifest: &Manifest) -> Vec<Check> {
        let mut checks = Vec::new();
        match fs::read_dir(self.paths.installations()) {
            Ok(entries) => {
                for entry in entries {
                    match entry {
                        Ok(entry) if entry.path() != manifest.active_build_directory => {
                            checks.push(fail(
                                "stale build directory",
                                format!("Unexpected build directory: {}", entry.path().display()),
                            ));
                        }
                        Ok(_) => {}
                        Err(error) => checks.push(scan_failure(
                            "installation directory scan",
                            &self.paths.installations(),
                            &error,
                        )),
                    }
                }
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => checks.push(scan_failure(
                "installation directory scan",
                &self.paths.installations(),
                &error,
            )),
        }
        for root in temporary_scan_roots(&self.paths, Some(manifest)) {
            inspect_temporary_directory(&root, &mut checks);
        }
        if checks.is_empty() {
            checks.push(pass(
                "stale installation artifacts",
                "No stale build or temporary file was found.",
            ));
        }
        checks
    }

    pub(super) fn orphan_checks(&self) -> Vec<Check> {
        let mut checks = Vec::new();
        for (name, path) in [
            ("active installation link", self.paths.active_link()),
            ("CLI link", self.paths.bin_root.join("librewavectl")),
            ("daemon link", self.paths.bin_root.join("librewaved")),
            ("systemd user unit", self.paths.unit()),
            ("udev access rule", self.paths.udev_rule.clone()),
            ("WirePlumber ownership rule", self.paths.wireplumber()),
        ] {
            match checked_exists(&path) {
                Ok(true) => checks.push(fail(
                    name,
                    format!("Unowned LibreWave path remains at {}.", path.display()),
                )),
                Ok(false) => {}
                Err(error) => checks.push(scan_failure(name, &path, &error)),
            }
        }
        match fs::read_dir(self.paths.installations()) {
            Ok(entries) => {
                for entry in entries {
                    match entry {
                        Ok(entry) => checks.push(fail(
                            "stale build directory",
                            format!("Unowned build remains at {}.", entry.path().display()),
                        )),
                        Err(error) => checks.push(scan_failure(
                            "installation directory scan",
                            &self.paths.installations(),
                            &error,
                        )),
                    }
                }
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => checks.push(scan_failure(
                "installation directory scan",
                &self.paths.installations(),
                &error,
            )),
        }
        for root in temporary_scan_roots(&self.paths, None) {
            inspect_temporary_directory(&root, &mut checks);
        }
        match checked_exists(&self.paths.backup_root()) {
            Ok(true) => checks.push(fail(
                "orphaned installation backups",
                format!("Unowned backups remain at {}.", self.paths.backup_root().display()),
            )),
            Ok(false) => {}
            Err(error) => checks.push(scan_failure(
                "orphaned installation backup inspection",
                &self.paths.backup_root(),
                &error,
            )),
        }
        let transactions = self.paths.state_root.join("transactions");
        match checked_exists(&transactions) {
            Ok(true) => checks.push(fail(
                "orphaned transaction files",
                format!("Unowned transaction state remains at {}.", transactions.display()),
            )),
            Ok(false) => {}
            Err(error) => {
                checks.push(scan_failure("orphaned transaction inspection", &transactions, &error));
            }
        }
        checks.extend(process_checks_without_manifest(&self.process_inspector));
        checks.push(self.unit_state_check());
        checks
    }

    pub(super) fn unit_state_check(&self) -> Check {
        match self.unit_inspector.inspect(UNIT_FILE_NAME) {
            Ok(state) if state.enabled || state.active => fail(
                "systemd user unit state",
                format!(
                    "The safety-gated unit is{}{}. Disable and stop it before audio ownership is implemented.",
                    if state.enabled { " enabled" } else { "" },
                    if state.active { " active" } else { "" }
                ),
            ),
            Ok(_) => {
                pass("systemd user unit state", "The safety-gated unit is disabled and inactive.")
            }
            Err(error) => {
                fail("systemd user unit state", format!("Cannot inspect the user unit: {error}"))
            }
        }
    }
}

fn temporary_scan_roots(
    paths: &InstallPaths,
    manifest: Option<&Manifest>,
) -> Vec<std::path::PathBuf> {
    let mut roots = vec![
        paths.data_root.clone(),
        paths.state_root.clone(),
        paths.state_root.join("transactions"),
        paths.state_root.join("transactions/setup"),
        paths.backup_root(),
        paths.config_root.join("systemd/user"),
        paths.bin_root.clone(),
    ];
    if let Some(manifest) = manifest {
        roots.push(manifest.active_build_directory.join("bin"));
    }
    roots
}

fn inspect_temporary_directory(root: &Path, checks: &mut Vec<Check>) {
    let entries = match fs::read_dir(root) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return,
        Err(error) => {
            checks.push(scan_failure("temporary-file scan", root, &error));
            return;
        }
    };
    for entry in entries {
        let entry = match entry {
            Ok(entry) => entry,
            Err(error) => {
                checks.push(scan_failure("temporary-file scan", root, &error));
                continue;
            }
        };
        let path = entry.path();
        if let Err(error) = entry.file_type() {
            checks.push(scan_failure("temporary-file inspection", &path, &error));
            continue;
        }
        if path.file_name().and_then(|name| name.to_str()).is_some_and(|name| {
            name.contains(".librewave-")
                && Path::new(name)
                    .extension()
                    .is_some_and(|extension| extension.eq_ignore_ascii_case("tmp"))
        }) {
            checks.push(fail(
                "stale transaction file",
                format!("Remove the stale manifest-scoped temporary file: {}", path.display()),
            ));
        }
    }
}

fn scan_failure(name: &str, path: &Path, error: &io::Error) -> Check {
    fail(name, format!("Cannot inspect {}: {error}", path.display()))
}

fn process_checks_without_manifest<P: ProcessInspector>(inspector: &P) -> Vec<Check> {
    let mut checks: Vec<Check> = match inspector.daemons() {
        Ok(processes) => processes
            .into_iter()
            .map(|process| {
                fail(
                    "running daemon",
                    format!(
                        "Unowned daemon process {} uses {}{}.",
                        process.pid,
                        process.executable.display(),
                        if process.deleted { " (deleted)" } else { "" }
                    ),
                )
            })
            .collect(),
        Err(error) => {
            return vec![fail(
                "running daemon",
                format!("Cannot inspect running daemon processes: {error}"),
            )];
        }
    };
    if checks.is_empty() {
        checks.push(pass("running daemon", "No LibreWave daemon process is running."));
    }
    checks
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inspection_errors_are_failed_checks() {
        let check = scan_failure(
            "temporary-file scan",
            Path::new("/unreadable"),
            &io::Error::from(io::ErrorKind::PermissionDenied),
        );
        assert_eq!(check.state, super::super::engine::CheckState::Fail);
        assert!(check.detail.contains("/unreadable"));
        assert!(check.detail.to_lowercase().contains("permission denied"));
    }
}
