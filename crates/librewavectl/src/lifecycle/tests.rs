#![allow(clippy::unwrap_used)]

use super::engine::{CheckState, InstallRequest, Lifecycle, UninstallOutcome, test_paths};
use super::hash;
use super::model::{
    AudioOwnership, BuildIdentity, Journal, MANIFEST_SCHEMA, Operation, OwnedKind, OwnedPath,
    Phase, Snapshot,
};
use super::system::{DaemonProcess, TestProcessInspector, TestUdev, TestUnitInspector, UdevSystem};
use super::{CommandKind, Options, run};
use std::fs;
use std::io;
use std::os::unix::ffi::OsStringExt;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

struct Fixture {
    root: PathBuf,
    cli: PathBuf,
    daemon: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let root = std::env::temp_dir().join(format!(
            "librewave-lifecycle-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&root).unwrap();
        let cli = root.join("source/librewavectl");
        let daemon = root.join("source/librewaved");
        write_executable(&cli, b"cli-v1");
        write_executable(&daemon, b"daemon-v1");
        Self { root, cli, daemon }
    }

    fn lifecycle(&self) -> Lifecycle<TestUdev, TestUnitInspector, TestProcessInspector> {
        let paths = test_paths(&self.root);
        Lifecycle::new(
            paths.clone(),
            TestUdev { expected: paths.udev_rule, events: Vec::new() },
            TestUnitInspector { enabled: false, active: false },
            TestProcessInspector::default(),
        )
    }

    fn identity(&self, revision: &str) -> BuildIdentity {
        BuildIdentity {
            source_revision: revision.to_owned(),
            source_dirty: false,
            dirty_fingerprint: None,
            profile: "debug".to_owned(),
            target: "test-target".to_owned(),
            cli_sha256: hash::file(&self.cli).unwrap(),
            daemon_sha256: hash::file(&self.daemon).unwrap(),
            installed_at_unix_seconds: 123,
        }
    }

    fn install(
        &self,
        lifecycle: &mut Lifecycle<TestUdev, TestUnitInspector, TestProcessInspector>,
        revision: &str,
    ) -> super::model::Manifest {
        lifecycle
            .setup(&InstallRequest {
                identity: self.identity(revision),
                cli_source: &self.cli,
                daemon_source: &self.daemon,
            })
            .unwrap()
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.root).unwrap();
    }
}

#[test]
fn first_install_records_exact_build_and_preserves_profiles() {
    let fixture = Fixture::new();
    let paths = test_paths(&fixture.root);
    let profiles = paths.config_root.join("librewave/profiles/default.json");
    write_file(&profiles, b"profile");
    let mut lifecycle = fixture.lifecycle();
    let manifest = fixture.install(&mut lifecycle, "revision-1");

    assert_eq!(
        fs::read(manifest.active_build_directory.join("bin/librewavectl")).unwrap(),
        b"cli-v1"
    );
    assert_eq!(fs::read_link(&manifest.active_link).unwrap(), manifest.active_build_directory);
    assert_eq!(manifest.audio_ownership, AudioOwnership::BlockedMissingProductionHost);
    assert!(!manifest.wireplumber_policy_path.exists());
    assert_eq!(lifecycle.uninstall(false).unwrap(), UninstallOutcome::Removed);
    assert_eq!(fs::read(profiles).unwrap(), b"profile");
    assert!(!paths.udev_rule.exists());
}

#[test]
fn explicit_purge_removes_profiles() {
    let fixture = Fixture::new();
    let profiles = test_paths(&fixture.root).config_root.join("librewave/profiles/default.json");
    write_file(&profiles, b"profile");
    let mut lifecycle = fixture.lifecycle();
    fixture.install(&mut lifecycle, "revision-1");
    assert_eq!(lifecycle.uninstall(true).unwrap(), UninstallOutcome::Removed);
    assert!(!profiles.exists());
}

#[test]
fn upgrade_removes_the_old_exact_build_and_reports_identity_mismatch() {
    let fixture = Fixture::new();
    let unit = test_paths(&fixture.root).config_root.join("systemd/user/librewaved.service");
    write_file(&unit, b"original-unit");
    let mut lifecycle = fixture.lifecycle();
    let old = fixture.install(&mut lifecycle, "revision-1");
    write_executable(&fixture.cli, b"cli-v2");
    write_executable(&fixture.daemon, b"daemon-v2");
    let current = fixture.identity("revision-2");
    let new = lifecycle
        .setup(&InstallRequest {
            identity: current.clone(),
            cli_source: &fixture.cli,
            daemon_source: &fixture.daemon,
        })
        .unwrap();

    assert_ne!(old.active_build_directory, new.active_build_directory);
    assert!(!old.active_build_directory.exists());
    let mut wrong = current;
    wrong.target = "wrong-target".to_owned();
    assert!(
        lifecycle
            .doctor(Some(&wrong))
            .iter()
            .any(|check| check.name == "current build identity" && check.state == CheckState::Fail)
    );
    assert_eq!(lifecycle.uninstall(false).unwrap(), UninstallOutcome::Removed);
    assert_eq!(fs::read(unit).unwrap(), b"original-unit");
}

#[test]
fn modified_file_stops_uninstall_before_any_removal() {
    let fixture = Fixture::new();
    let mut lifecycle = fixture.lifecycle();
    let manifest = fixture.install(&mut lifecycle, "revision-1");
    write_file(&manifest.unit_path, b"changed");
    let active_before = fs::read_link(&manifest.active_link).unwrap();

    let modified = lifecycle.uninstall(false).unwrap();

    assert_eq!(modified, UninstallOutcome::Modified(vec![manifest.unit_path]));
    assert_eq!(fs::read_link(&manifest.active_link).unwrap(), active_before);
    assert!(manifest.udev_policy_path.exists());
    assert!(test_paths(&fixture.root).manifest().exists());
}

#[test]
fn corrupt_original_backup_stops_uninstall_before_any_removal() {
    let fixture = Fixture::new();
    let paths = test_paths(&fixture.root);
    let unit = paths.config_root.join("systemd/user/librewaved.service");
    write_file(&unit, b"user-unit");
    let mut lifecycle = fixture.lifecycle();
    let manifest = fixture.install(&mut lifecycle, "revision-1");
    let installed_unit = fs::read(&unit).unwrap();
    let saved = manifest
        .owned_paths
        .iter()
        .find(|entry| entry.path == unit)
        .and_then(|entry| entry.original.as_ref())
        .and_then(|snapshot| snapshot.saved_content.as_ref())
        .unwrap()
        .clone();
    write_file(&saved, b"corrupt");

    let modified = lifecycle.uninstall(false).unwrap();

    assert_eq!(modified, UninstallOutcome::Modified(vec![saved]));
    assert_eq!(fs::read(&unit).unwrap(), installed_unit);
    assert!(manifest.active_link.exists());
    assert!(manifest.udev_policy_path.exists());
}

#[test]
fn upgrade_refuses_modified_owned_paths_without_mutation() {
    let fixture = Fixture::new();
    let mut lifecycle = fixture.lifecycle();
    let manifest = fixture.install(&mut lifecycle, "revision-1");
    write_file(&manifest.unit_path, b"locally-changed");
    write_executable(&fixture.cli, b"cli-v2");
    let active = fs::read_link(&manifest.active_link).unwrap();

    let result = lifecycle.setup(&InstallRequest {
        identity: fixture.identity("revision-2"),
        cli_source: &fixture.cli,
        daemon_source: &fixture.daemon,
    });

    assert!(result.is_err());
    assert_eq!(fs::read(&manifest.unit_path).unwrap(), b"locally-changed");
    assert_eq!(fs::read_link(&manifest.active_link).unwrap(), active);
}

#[test]
fn upgrade_refuses_a_corrupt_carried_backup_without_mutation() {
    let fixture = Fixture::new();
    let unit = test_paths(&fixture.root).config_root.join("systemd/user/librewaved.service");
    write_file(&unit, b"original-unit");
    let mut lifecycle = fixture.lifecycle();
    let manifest = fixture.install(&mut lifecycle, "revision-1");
    let saved = manifest
        .owned_paths
        .iter()
        .find(|entry| entry.path == unit)
        .and_then(|entry| entry.original.as_ref())
        .and_then(|snapshot| snapshot.saved_content.as_ref())
        .unwrap();
    fs::remove_file(saved).unwrap();
    write_executable(&fixture.cli, b"cli-v2");
    let installed_unit = fs::read(&unit).unwrap();

    let result = lifecycle.setup(&InstallRequest {
        identity: fixture.identity("revision-2"),
        cli_source: &fixture.cli,
        daemon_source: &fixture.daemon,
    });

    assert_eq!(result.unwrap_err().kind(), io::ErrorKind::InvalidData);
    assert_eq!(fs::read(unit).unwrap(), installed_unit);
    assert_eq!(fs::read_link(&manifest.active_link).unwrap(), manifest.active_build_directory);
}

#[test]
fn setup_failure_rolls_back_files_links_directories_and_udev() {
    let fixture = Fixture::new();
    let paths = test_paths(&fixture.root);
    let original_unit = paths.config_root.join("systemd/user/librewaved.service");
    let original_target = fixture.root.join("original-active");
    write_file(&original_unit, b"original-unit");
    fs::create_dir_all(&original_target).unwrap();
    fs::create_dir_all(&paths.data_root).unwrap();
    symlink(&original_target, paths.data_root.join("active")).unwrap();
    let mut lifecycle = fixture.lifecycle();
    lifecycle.inject_failure_after_apply();

    let result = lifecycle.setup(&InstallRequest {
        identity: fixture.identity("revision-1"),
        cli_source: &fixture.cli,
        daemon_source: &fixture.daemon,
    });

    assert!(result.is_err());
    assert_eq!(fs::read(original_unit).unwrap(), b"original-unit");
    assert_eq!(fs::read_link(paths.data_root.join("active")).unwrap(), original_target);
    assert!(!paths.udev_rule.exists());
    assert!(!paths.manifest().exists());
    assert!(!paths.journal().exists());
    assert!(paths.data_root.exists());
}

#[test]
fn foreign_udev_collision_fails_without_mutation() {
    let fixture = Fixture::new();
    let paths = test_paths(&fixture.root);
    write_file(&paths.udev_rule, b"foreign-rule");
    let mut lifecycle = fixture.lifecycle();
    let result = lifecycle.setup(&InstallRequest {
        identity: fixture.identity("revision-1"),
        cli_source: &fixture.cli,
        daemon_source: &fixture.daemon,
    });
    assert_eq!(result.unwrap_err().kind(), io::ErrorKind::AlreadyExists);
    assert_eq!(fs::read(&paths.udev_rule).unwrap(), b"foreign-rule");
    assert!(!paths.manifest().exists());
}

#[test]
fn first_install_refuses_an_unmanifested_planned_build_directory() {
    let fixture = Fixture::new();
    let paths = test_paths(&fixture.root);
    let planned = paths.installations().join(fixture.identity("revision-1").stable_key());
    fs::create_dir_all(&planned).unwrap();
    let mut lifecycle = fixture.lifecycle();

    assert!(
        lifecycle
            .setup(&InstallRequest {
                identity: fixture.identity("revision-1"),
                cli_source: &fixture.cli,
                daemon_source: &fixture.daemon,
            })
            .is_err()
    );
    assert!(planned.is_dir());
    assert!(!paths.manifest().exists());
    assert!(!paths.udev_rule.exists());
}

#[test]
fn uninstall_rejects_a_symlink_at_the_fixed_udev_path_before_mutation() {
    let fixture = Fixture::new();
    let mut lifecycle = fixture.lifecycle();
    let manifest = fixture.install(&mut lifecycle, "revision-1");
    let target = fixture.root.join("foreign-udev-target");
    let exact = librewave_platform_linux::audio_policy::Wave3AudioPolicy::new().render_udev();
    write_file(&target, exact.as_bytes());
    fs::remove_file(&manifest.udev_policy_path).unwrap();
    symlink(&target, &manifest.udev_policy_path).unwrap();
    let active = fs::read_link(&manifest.active_link).unwrap();

    assert_eq!(lifecycle.uninstall(false).unwrap_err().kind(), io::ErrorKind::InvalidData);
    assert_eq!(fs::read_link(&manifest.active_link).unwrap(), active);
    assert!(fs::symlink_metadata(&manifest.udev_policy_path).unwrap().file_type().is_symlink());
}

#[test]
fn doctor_reports_an_unsafe_udev_path_as_a_failed_check() {
    let fixture = Fixture::new();
    let mut lifecycle = fixture.lifecycle();
    let manifest = fixture.install(&mut lifecycle, "revision-1");
    let target = fixture.root.join("udev-target");
    let exact = librewave_platform_linux::audio_policy::Wave3AudioPolicy::new().render_udev();
    write_file(&target, exact.as_bytes());
    fs::remove_file(&manifest.udev_policy_path).unwrap();
    symlink(&target, &manifest.udev_policy_path).unwrap();

    let checks = lifecycle.doctor(None);
    assert!(checks.iter().any(|check| {
        check.name == format!("owned path {}", manifest.udev_policy_path.display())
            && check.state == CheckState::Fail
            && check.detail.contains("cannot be inspected safely")
    }));
}

#[test]
fn setup_rejects_a_symlink_at_the_fixed_udev_path_before_mutation() {
    let fixture = Fixture::new();
    let paths = test_paths(&fixture.root);
    let target = fixture.root.join("foreign-udev-target");
    let exact = librewave_platform_linux::audio_policy::Wave3AudioPolicy::new().render_udev();
    write_file(&target, exact.as_bytes());
    fs::create_dir_all(paths.udev_rule.parent().unwrap()).unwrap();
    symlink(&target, &paths.udev_rule).unwrap();
    let mut lifecycle = fixture.lifecycle();

    assert_eq!(
        lifecycle
            .setup(&InstallRequest {
                identity: fixture.identity("revision-1"),
                cli_source: &fixture.cli,
                daemon_source: &fixture.daemon,
            })
            .unwrap_err()
            .kind(),
        io::ErrorKind::InvalidData
    );
    assert!(!paths.manifest().exists());
    assert!(fs::symlink_metadata(&paths.udev_rule).unwrap().file_type().is_symlink());
}

#[test]
fn uninstall_rejects_a_wrong_mode_udev_rule_before_mutation() {
    let fixture = Fixture::new();
    let mut lifecycle = fixture.lifecycle();
    let manifest = fixture.install(&mut lifecycle, "revision-1");
    fs::set_permissions(&manifest.udev_policy_path, fs::Permissions::from_mode(0o600)).unwrap();
    let active = fs::read_link(&manifest.active_link).unwrap();

    assert_eq!(lifecycle.uninstall(false).unwrap_err().kind(), io::ErrorKind::InvalidData);
    assert_eq!(fs::read_link(&manifest.active_link).unwrap(), active);
}

#[test]
fn udev_install_and_remove_are_each_followed_by_an_exact_refresh() {
    let fixture = Fixture::new();
    let mut lifecycle = fixture.lifecycle();
    fixture.install(&mut lifecycle, "revision-1");
    assert_eq!(lifecycle.udev.events, vec!["install", "refresh"]);
    assert_eq!(lifecycle.uninstall(false).unwrap(), UninstallOutcome::Removed);
    assert_eq!(lifecycle.udev.events, vec!["install", "refresh", "remove", "refresh"]);
}

#[test]
fn preexisting_nonempty_data_directory_is_preserved() {
    let fixture = Fixture::new();
    let paths = test_paths(&fixture.root);
    let user_file = paths.data_root.join("user-file");
    write_file(&user_file, b"keep");
    let mut lifecycle = fixture.lifecycle();
    fixture.install(&mut lifecycle, "revision-1");
    assert_eq!(lifecycle.uninstall(false).unwrap(), UninstallOutcome::Removed);
    assert_eq!(fs::read(user_file).unwrap(), b"keep");
}

#[test]
fn interrupted_uninstall_resumes_after_backup_cleanup() {
    let fixture = Fixture::new();
    let paths = test_paths(&fixture.root);
    let unit = paths.unit();
    write_file(&unit, b"user-unit");
    let mut lifecycle = fixture.lifecycle();
    fixture.install(&mut lifecycle, "revision-1");
    lifecycle.inject_failure_after_backup_cleanup();

    assert!(lifecycle.uninstall(false).is_err());
    assert_eq!(fs::read(&unit).unwrap(), b"user-unit");
    assert!(!paths.manifest().exists());
    assert!(paths.journal().exists());

    let mut lifecycle = fixture.lifecycle();
    assert_eq!(lifecycle.uninstall(false).unwrap(), UninstallOutcome::Removed);
    assert!(!paths.manifest().exists());
    assert!(!paths.journal().exists());
}

#[test]
fn setup_recovers_when_the_manifest_switch_precedes_its_journal_phase() {
    let fixture = Fixture::new();
    let paths = test_paths(&fixture.root);
    let mut lifecycle = fixture.lifecycle();
    lifecycle.inject_failure_after_manifest_switch();
    let identity = fixture.identity("revision-1");
    let result = lifecycle.setup(&InstallRequest {
        identity,
        cli_source: &fixture.cli,
        daemon_source: &fixture.daemon,
    });
    assert!(result.is_err());
    assert_eq!(lifecycle.read_journal().unwrap().unwrap().phase, Phase::Applied);
    assert!(paths.manifest().exists());

    let mut lifecycle = fixture.lifecycle();
    let manifest = fixture.install(&mut lifecycle, "revision-2");
    assert_eq!(manifest.identity.source_revision, "revision-2");
    assert!(!paths.journal().exists());
}

#[test]
fn corrupted_manifest_cannot_add_a_deletion_target() {
    let fixture = Fixture::new();
    let paths = test_paths(&fixture.root);
    let mut lifecycle = fixture.lifecycle();
    let mut manifest = fixture.install(&mut lifecycle, "revision-1");
    let foreign = paths.config_root.join("important.conf");
    write_file(&foreign, b"keep");
    manifest.owned_paths.push(OwnedPath {
        path: foreign.clone(),
        kind: OwnedKind::File,
        sha256: Some(hash::file(&foreign).unwrap()),
        link_target: None,
        mode: Some(0o644),
        created_by_librewave: false,
        original: None,
    });
    super::fs_ops::atomic_json(&paths.manifest(), &manifest).unwrap();

    assert_eq!(lifecycle.uninstall(false).unwrap_err().kind(), io::ErrorKind::InvalidData);
    assert_eq!(fs::read(foreign).unwrap(), b"keep");
}

#[test]
fn corrupted_journal_cannot_escape_transaction_roots() {
    let fixture = Fixture::new();
    let paths = test_paths(&fixture.root);
    let mut lifecycle = fixture.lifecycle();
    let manifest = fixture.install(&mut lifecycle, "revision-1");
    let outside = fixture.root.join("outside");
    write_file(&outside, b"keep");
    let mut rollback = manifest
        .owned_paths
        .iter()
        .map(|entry| Snapshot {
            destination: entry.path.clone(),
            kind: None,
            saved_content: None,
            sha256: None,
            link_target: None,
            mode: None,
        })
        .collect::<Vec<_>>();
    rollback[0].destination = outside.clone();
    super::fs_ops::atomic_json(
        &paths.journal(),
        &Journal {
            schema_version: MANIFEST_SCHEMA,
            operation: Operation::Setup,
            phase: Phase::BackedUp,
            previous: Some(manifest.clone()),
            next: Some(manifest),
            rollback_paths: rollback,
            udev_refresh_pending: false,
            purge_profiles: false,
        },
    )
    .unwrap();

    let result = lifecycle.setup(&InstallRequest {
        identity: fixture.identity("revision-1"),
        cli_source: &fixture.cli,
        daemon_source: &fixture.daemon,
    });
    assert_eq!(result.unwrap_err().kind(), io::ErrorKind::InvalidData);
    assert_eq!(fs::read(outside).unwrap(), b"keep");
}

#[test]
fn uninstall_journal_requires_complete_operation_state() {
    let fixture = Fixture::new();
    let paths = test_paths(&fixture.root);
    let mut lifecycle = fixture.lifecycle();
    let manifest = fixture.install(&mut lifecycle, "revision-1");
    super::fs_ops::atomic_json(
        &paths.journal(),
        &Journal {
            schema_version: MANIFEST_SCHEMA,
            operation: Operation::Uninstall,
            phase: Phase::Applied,
            previous: None,
            next: None,
            rollback_paths: Vec::new(),
            udev_refresh_pending: true,
            purge_profiles: false,
        },
    )
    .unwrap();

    assert_eq!(lifecycle.uninstall(false).unwrap_err().kind(), io::ErrorKind::InvalidData);
    assert!(manifest.active_link.exists());
    assert!(manifest.udev_policy_path.exists());
}

#[test]
fn recovery_rejects_a_manifest_that_is_not_linked_to_its_journal() {
    let fixture = Fixture::new();
    let paths = test_paths(&fixture.root);
    let mut lifecycle = fixture.lifecycle();
    let manifest = fixture.install(&mut lifecycle, "revision-1");
    let mut other = manifest.clone();
    other.identity.installed_at_unix_seconds += 1;
    super::fs_ops::atomic_json(
        &paths.journal(),
        &Journal {
            schema_version: MANIFEST_SCHEMA,
            operation: Operation::Uninstall,
            phase: Phase::Prepared,
            previous: Some(other),
            next: None,
            rollback_paths: Vec::new(),
            udev_refresh_pending: false,
            purge_profiles: false,
        },
    )
    .unwrap();

    assert_eq!(lifecycle.uninstall(false).unwrap_err().kind(), io::ErrorKind::InvalidData);
    assert!(manifest.active_link.exists());
}

#[test]
fn file_symlink_directory_and_udev_snapshots_restore_exactly() {
    let fixture = Fixture::new();
    let root = fixture.root.join("snapshot");
    let file = root.join("file");
    let link = root.join("link");
    let directory = root.join("directory");
    write_file(&file, b"before");
    fs::create_dir_all(&directory).unwrap();
    write_file(&directory.join("user"), b"keep");
    symlink("before-target", &link).unwrap();
    let file_snapshot = super::fs_ops::prepare_snapshot(&file, root.join("saved")).unwrap();
    super::fs_ops::atomic_copy_new(&file, file_snapshot.saved_content.as_ref().unwrap(), 0o600)
        .unwrap();
    assert_eq!(
        fs::metadata(file_snapshot.saved_content.as_ref().unwrap()).unwrap().permissions().mode()
            & 0o777,
        0o600
    );
    let link_snapshot = super::fs_ops::prepare_snapshot(&link, root.join("unused")).unwrap();
    let directory_snapshot =
        super::fs_ops::prepare_snapshot(&directory, root.join("unused-dir")).unwrap();
    write_file(&file, b"after");
    fs::remove_file(&link).unwrap();
    symlink("after-target", &link).unwrap();
    super::fs_ops::restore_snapshot(&file, &file_snapshot).unwrap();
    super::fs_ops::restore_snapshot(&link, &link_snapshot).unwrap();
    super::fs_ops::restore_snapshot(&directory, &directory_snapshot).unwrap();
    assert_eq!(fs::read(file).unwrap(), b"before");
    assert_eq!(fs::read_link(link).unwrap(), PathBuf::from("before-target"));
    assert_eq!(fs::read(directory.join("user")).unwrap(), b"keep");

    let udev_path = root.join("udev");
    let exact = librewave_platform_linux::audio_policy::Wave3AudioPolicy::new().render_udev();
    write_file(&udev_path, exact.as_bytes());
    let udev_snapshot =
        super::fs_ops::prepare_snapshot(&udev_path, root.join("udev-saved")).unwrap();
    super::fs_ops::atomic_copy_new(
        &udev_path,
        udev_snapshot.saved_content.as_ref().unwrap(),
        0o600,
    )
    .unwrap();
    let mut udev = TestUdev { expected: udev_path.clone(), events: Vec::new() };
    udev.remove_and_refresh(&udev_path).unwrap();
    super::fs_ops::restore_udev_snapshot(&mut udev, &udev_path, &udev_snapshot).unwrap();
    assert_eq!(fs::read(udev_path).unwrap(), exact.as_bytes());
}

#[test]
fn temporary_files_start_with_private_mode() {
    let fixture = Fixture::new();
    let destination = fixture.root.join("private/state");
    let (file, temporary) = super::fs_ops::create_temporary(&destination).unwrap();
    assert_eq!(fs::metadata(&temporary).unwrap().permissions().mode() & 0o777, 0o600);
    drop(file);
    fs::remove_file(temporary).unwrap();
}

#[test]
fn doctor_reports_stale_artifacts_without_a_manifest() {
    let fixture = Fixture::new();
    let paths = test_paths(&fixture.root);
    write_file(&paths.config_root.join("systemd/user/librewaved.service"), b"stale");
    write_file(&paths.state_root.join("backups/original-1"), b"stale");
    write_file(&paths.data_root.join("installations/stale/bin/librewaved"), b"stale");
    let lifecycle = fixture.lifecycle();
    let checks = lifecycle.doctor(None);
    assert!(
        checks
            .iter()
            .any(|check| check.name == "systemd user unit" && check.state == CheckState::Fail)
    );
    assert!(
        checks
            .iter()
            .any(|check| check.name == "stale build directory" && check.state == CheckState::Fail)
    );
    assert!(
        checks.iter().any(|check| check.name == "orphaned installation backups"
            && check.state == CheckState::Fail)
    );
}

#[test]
fn doctor_temporary_scan_stays_within_lifecycle_directories() {
    let fixture = Fixture::new();
    let paths = test_paths(&fixture.root);
    let unrelated_config = paths.config_root.join("unrelated/.librewave-foreign.tmp");
    let unrelated_bin = paths.bin_root.join("unrelated/.librewave-foreign.tmp");
    let stale_build = paths.installations().join("foreign/bin/librewaved.librewave-foreign.tmp");
    write_file(&unrelated_config, b"keep");
    write_file(&unrelated_bin, b"keep");
    write_file(&stale_build, b"keep");

    let lifecycle = fixture.lifecycle();
    let checks = lifecycle.doctor(None);
    for path in [&unrelated_config, &unrelated_bin, &stale_build] {
        assert!(!checks.iter().any(|check| check.detail.contains(&path.display().to_string())));
    }

    let owned_scope = paths.bin_root.join("librewavectl.librewave-stale.tmp");
    write_file(&owned_scope, b"stale");
    assert!(lifecycle.doctor(None).iter().any(|check| {
        check.name == "stale transaction file"
            && check.detail.contains(&owned_scope.display().to_string())
    }));
}

#[test]
fn doctor_reports_invalid_journal_and_manifest_state_as_failed_checks() {
    let fixture = Fixture::new();
    let paths = test_paths(&fixture.root);
    let mut lifecycle = fixture.lifecycle();
    fixture.install(&mut lifecycle, "revision-1");
    super::fs_ops::atomic_write(&paths.journal(), b"{invalid", 0o600).unwrap();

    let checks = lifecycle.doctor(None);
    assert!(checks.iter().any(|check| {
        check.name == "transaction journal"
            && check.state == CheckState::Fail
            && check.detail.contains("invalid")
    }));
    assert!(
        checks.iter().any(|check| {
            check.name == "installation manifest" && check.detail.contains("Schema")
        })
    );

    fs::remove_file(paths.journal()).unwrap();
    super::fs_ops::atomic_write(&paths.manifest(), b"{invalid", 0o600).unwrap();
    let checks = lifecycle.doctor(None);
    assert!(checks.iter().any(|check| {
        check.name == "installation manifest"
            && check.state == CheckState::Fail
            && check.detail.contains("invalid")
    }));
}

#[test]
fn doctor_fails_when_the_safety_gated_unit_is_enabled_or_active() {
    let fixture = Fixture::new();
    let paths = test_paths(&fixture.root);
    let lifecycle = Lifecycle::new(
        paths.clone(),
        TestUdev { expected: paths.udev_rule, events: Vec::new() },
        TestUnitInspector { enabled: true, active: true },
        TestProcessInspector::default(),
    );
    assert!(lifecycle.doctor(None).iter().any(|check| {
        check.name == "systemd user unit state" && check.state == CheckState::Fail
    }));
}

#[test]
fn setup_and_uninstall_stop_before_mutation_for_runtime_owners() {
    let fixture = Fixture::new();
    let paths = test_paths(&fixture.root);
    let mut blocked_setup = Lifecycle::new(
        paths.clone(),
        TestUdev { expected: paths.udev_rule.clone(), events: Vec::new() },
        TestUnitInspector { enabled: true, active: false },
        TestProcessInspector::default(),
    );
    assert!(
        blocked_setup
            .setup(&InstallRequest {
                identity: fixture.identity("revision-1"),
                cli_source: &fixture.cli,
                daemon_source: &fixture.daemon,
            })
            .is_err()
    );
    assert!(!paths.manifest().exists());

    let mut lifecycle = fixture.lifecycle();
    let manifest = fixture.install(&mut lifecycle, "revision-1");
    lifecycle.process_inspector.daemons.push(DaemonProcess {
        pid: "4242".to_owned(),
        executable: manifest.active_build_directory.join("bin/librewaved"),
        deleted: false,
        sha256: Some(manifest.identity.daemon_sha256.clone()),
    });
    let active = fs::read_link(&manifest.active_link).unwrap();
    assert!(lifecycle.uninstall(false).is_err());
    assert_eq!(fs::read_link(&manifest.active_link).unwrap(), active);
    assert!(manifest.udev_policy_path.exists());
}

#[test]
fn stale_wireplumber_policy_blocks_setup_before_mutation() {
    let fixture = Fixture::new();
    let paths = test_paths(&fixture.root);
    write_file(&paths.wireplumber(), b"stale");
    let mut lifecycle = fixture.lifecycle();
    assert!(
        lifecycle
            .setup(&InstallRequest {
                identity: fixture.identity("revision-1"),
                cli_source: &fixture.cli,
                daemon_source: &fixture.daemon,
            })
            .is_err()
    );
    assert!(!paths.manifest().exists());
}

#[test]
fn backup_parent_symlinks_cannot_escape_the_state_root() {
    let fixture = Fixture::new();
    let paths = test_paths(&fixture.root);
    let outside = fixture.root.join("outside-backups");
    fs::create_dir_all(&outside).unwrap();
    fs::create_dir_all(&paths.state_root).unwrap();
    symlink(&outside, paths.backup_root()).unwrap();
    write_file(&paths.unit(), b"user-unit");
    let mut lifecycle = fixture.lifecycle();
    assert!(
        lifecycle
            .setup(&InstallRequest {
                identity: fixture.identity("revision-1"),
                cli_source: &fixture.cli,
                daemon_source: &fixture.daemon,
            })
            .is_err()
    );
    assert!(fs::read_dir(outside).unwrap().next().is_none());
    assert!(!paths.udev_rule.exists());
}

#[test]
fn transaction_parent_symlink_stops_upgrade_before_mutation() {
    let fixture = Fixture::new();
    let paths = test_paths(&fixture.root);
    let mut lifecycle = fixture.lifecycle();
    let manifest = fixture.install(&mut lifecycle, "revision-1");
    let outside = fixture.root.join("outside-transactions");
    fs::create_dir_all(&outside).unwrap();
    fs::create_dir_all(paths.state_root.join("transactions")).unwrap();
    symlink(&outside, paths.state_root.join("transactions/setup")).unwrap();
    write_executable(&fixture.cli, b"cli-v2");
    let active = fs::read_link(&manifest.active_link).unwrap();

    assert!(
        lifecycle
            .setup(&InstallRequest {
                identity: fixture.identity("revision-2"),
                cli_source: &fixture.cli,
                daemon_source: &fixture.daemon,
            })
            .is_err()
    );
    assert_eq!(fs::read_link(&manifest.active_link).unwrap(), active);
    assert!(fs::read_dir(outside).unwrap().next().is_none());
}

#[test]
fn foreign_transaction_content_is_never_removed_by_recovery() {
    let fixture = Fixture::new();
    let paths = test_paths(&fixture.root);
    let mut lifecycle = fixture.lifecycle();
    let manifest = fixture.install(&mut lifecycle, "revision-1");
    let rollback_paths = manifest
        .owned_paths
        .iter()
        .map(|owned| Snapshot {
            destination: owned.path.clone(),
            kind: None,
            saved_content: None,
            sha256: None,
            link_target: None,
            mode: None,
        })
        .collect();
    super::fs_ops::atomic_json(
        &paths.journal(),
        &Journal {
            schema_version: MANIFEST_SCHEMA,
            operation: Operation::Setup,
            phase: Phase::Prepared,
            previous: Some(manifest.clone()),
            next: Some(manifest),
            rollback_paths,
            udev_refresh_pending: false,
            purge_profiles: false,
        },
    )
    .unwrap();
    let foreign = paths.state_root.join("transactions/setup/foreign");
    write_file(&foreign, b"keep");

    assert!(
        lifecycle
            .setup(&InstallRequest {
                identity: fixture.identity("revision-1"),
                cli_source: &fixture.cli,
                daemon_source: &fixture.daemon,
            })
            .is_err()
    );
    assert_eq!(fs::read(foreign).unwrap(), b"keep");
}

#[test]
fn unmanifested_empty_build_directory_is_reported_and_preserved() {
    let fixture = Fixture::new();
    let paths = test_paths(&fixture.root);
    let mut lifecycle = fixture.lifecycle();
    fixture.install(&mut lifecycle, "revision-1");
    write_executable(&fixture.cli, b"cli-v2");
    let next = fixture.identity("revision-2");
    let stale = paths.installations().join(next.stable_key());
    fs::create_dir_all(&stale).unwrap();
    let active_before = fs::read_link(paths.active_link()).unwrap();

    assert!(
        lifecycle
            .setup(&InstallRequest {
                identity: next,
                cli_source: &fixture.cli,
                daemon_source: &fixture.daemon,
            })
            .is_err()
    );
    assert!(stale.is_dir());
    assert_eq!(fs::read_link(paths.active_link()).unwrap(), active_before);
    assert!(!paths.journal().exists());
}

#[test]
fn setup_refuses_a_destination_changed_after_preflight() {
    let fixture = Fixture::new();
    let paths = test_paths(&fixture.root);
    let mut lifecycle = fixture.lifecycle();
    lifecycle.inject_mutation_before_apply(paths.udev_rule.clone(), b"foreign-race".to_vec());

    assert!(
        lifecycle
            .setup(&InstallRequest {
                identity: fixture.identity("revision-1"),
                cli_source: &fixture.cli,
                daemon_source: &fixture.daemon,
            })
            .is_err()
    );
    assert_eq!(fs::read(&paths.udev_rule).unwrap(), b"foreign-race");
    assert!(!paths.manifest().exists());
}

#[test]
fn setup_refuses_a_source_binary_changed_after_initial_validation() {
    let fixture = Fixture::new();
    let paths = test_paths(&fixture.root);
    let mut lifecycle = fixture.lifecycle();
    lifecycle.inject_mutation_before_apply(fixture.cli.clone(), b"changed-cli".to_vec());

    assert!(
        lifecycle
            .setup(&InstallRequest {
                identity: fixture.identity("revision-1"),
                cli_source: &fixture.cli,
                daemon_source: &fixture.daemon,
            })
            .is_err()
    );
    assert!(!paths.manifest().exists());
    assert!(!paths.active_link().exists());
}

#[test]
fn setup_never_overwrites_a_stale_original_backup() {
    let fixture = Fixture::new();
    let paths = test_paths(&fixture.root);
    write_file(&paths.unit(), b"user-unit");
    let stale = paths.backup_root().join("original-6");
    write_file(&stale, b"foreign-backup");
    let mut lifecycle = fixture.lifecycle();

    assert!(
        lifecycle
            .setup(&InstallRequest {
                identity: fixture.identity("revision-1"),
                cli_source: &fixture.cli,
                daemon_source: &fixture.daemon,
            })
            .is_err()
    );
    assert_eq!(fs::read(stale).unwrap(), b"foreign-backup");
    assert_eq!(fs::read(paths.unit()).unwrap(), b"user-unit");
    assert!(!paths.udev_rule.exists());
}

#[test]
fn uninstall_reports_not_installed_without_claiming_removal() {
    let fixture = Fixture::new();
    assert_eq!(fixture.lifecycle().uninstall(false).unwrap(), UninstallOutcome::NotInstalled);
}

#[test]
fn lifecycle_options_are_command_specific() {
    let no_arguments: Vec<String> = Vec::new();
    let doctor = Options::parse(&no_arguments, CommandKind::Doctor).unwrap();
    assert!(doctor.source_root.is_none());
    assert!(doctor.profile.is_none());
    assert!(doctor.target.is_none());
    assert!(doctor.daemon_path.is_none());

    assert!(Options::parse(&["--purge".to_owned()], CommandKind::Setup).is_err());
    assert!(
        Options::parse(&["--source-root".to_owned(), "/tmp".to_owned()], CommandKind::Uninstall)
            .is_err()
    );
    assert!(
        Options::parse(&["--target".to_owned(), "test".to_owned()], CommandKind::Doctor).is_err()
    );
    assert!(
        Options::parse(
            &["--daemon-path".to_owned(), "/tmp/librewaved".to_owned()],
            CommandKind::Uninstall
        )
        .is_err()
    );
}

#[test]
fn lifecycle_help_is_available_and_command_specific() {
    let cases = [
        ("setup", "--source-root", "--purge"),
        ("doctor", "--expect-current", "--yes"),
        ("uninstall", "--purge", "--source-root"),
    ];
    for (command, expected, absent) in cases {
        let mut output = Vec::new();
        let mut errors = Vec::new();
        let exit = run(
            &["librewavectl".to_owned(), command.to_owned(), "--help".to_owned()],
            &mut output,
            &mut errors,
        )
        .unwrap();
        let output = String::from_utf8(output).unwrap();
        assert_eq!(exit, 0);
        assert!(output.contains(&format!("Usage: librewavectl {command}")));
        assert!(output.contains(expected));
        assert!(!output.contains(absent));
        assert!(errors.is_empty());
    }

    let mut output = Vec::new();
    assert_eq!(
        run(
            &["librewavectl".to_owned(), "setup".to_owned(), "-h".to_owned()],
            &mut output,
            &mut Vec::new(),
        )
        .unwrap(),
        0
    );
    assert!(String::from_utf8(output).unwrap().contains("Usage: librewavectl setup"));
}

#[test]
fn systemd_paths_reject_specifier_and_environment_expansion() {
    assert!(super::validation::validate_systemd_path(Path::new("/tmp/%n/librewaved")).is_err());
    assert!(super::validation::validate_systemd_path(Path::new("/tmp/$HOME/librewaved")).is_err());
    let non_utf8 =
        PathBuf::from(std::ffi::OsString::from_vec(vec![b'/', b't', b'm', b'p', b'/', 0xff]));
    assert!(super::validation::validate_systemd_path(&non_utf8).is_err());
}

#[test]
fn interrupted_uninstall_cannot_change_profile_intent() {
    let fixture = Fixture::new();
    let paths = test_paths(&fixture.root);
    let mut lifecycle = fixture.lifecycle();
    let manifest = fixture.install(&mut lifecycle, "revision-1");
    super::fs_ops::atomic_json(
        &paths.journal(),
        &Journal {
            schema_version: MANIFEST_SCHEMA,
            operation: Operation::Uninstall,
            phase: Phase::Prepared,
            previous: Some(manifest.clone()),
            next: None,
            rollback_paths: Vec::new(),
            udev_refresh_pending: true,
            purge_profiles: true,
        },
    )
    .unwrap();

    assert_eq!(lifecycle.uninstall(false).unwrap_err().kind(), io::ErrorKind::InvalidInput);
    assert!(manifest.active_link.exists());
    assert!(paths.manifest().exists());
}

#[test]
fn terminal_uninstall_recovery_checks_profile_intent_before_purge() {
    let fixture = Fixture::new();
    let paths = test_paths(&fixture.root);
    let profiles = paths.profiles();
    write_file(&profiles.join("default.json"), b"original");
    let mut lifecycle = fixture.lifecycle();
    fixture.install(&mut lifecycle, "revision-1");
    lifecycle.inject_failure_after_backup_cleanup();
    assert!(lifecycle.uninstall(true).is_err());
    assert!(!paths.manifest().exists());
    assert!(paths.journal().exists());

    write_file(&profiles.join("recreated.json"), b"keep");
    let mut lifecycle = fixture.lifecycle();
    assert_eq!(lifecycle.uninstall(false).unwrap_err().kind(), io::ErrorKind::InvalidInput);
    assert_eq!(fs::read(profiles.join("recreated.json")).unwrap(), b"keep");
    assert!(paths.journal().exists());
}

fn write_file(path: &Path, content: &[u8]) {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    fs::write(path, content).unwrap();
}

fn write_executable(path: &Path, content: &[u8]) {
    write_file(path, content);
    fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
}
