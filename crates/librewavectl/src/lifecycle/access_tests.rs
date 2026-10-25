#![allow(clippy::unwrap_used)]
use super::engine::{InstallRequest, Lifecycle, UninstallOutcome, test_paths};
use super::system::{TestProcessInspector, TestUdev, TestUnitInspector, UdevSystem};
use super::tests::Fixture;
use librewave_platform_linux::usb_access::{AccessSnapshot, AccessState};
use std::cell::{Cell, RefCell};
use std::fs;
use std::io;
use std::path::Path;
use std::rc::Rc;

const ORIGINAL: &str = "user::rw-\ngroup::r--\nother::---\n";
const GRANTED: &str = "user::rw-\nuser:1000:rw-\ngroup::r--\nmask::rw-\nother::---\n";

struct AccessUdev {
    rule: TestUdev,
    acl: Rc<RefCell<String>>,
    captures: Cell<u32>,
    restore_fails: bool,
}
impl UdevSystem for AccessUdev {
    fn read(&self, path: &Path) -> io::Result<Option<Vec<u8>>> {
        self.rule.read(path)
    }
    fn install_and_refresh(&mut self, path: &Path, bytes: &[u8]) -> io::Result<()> {
        self.rule.install_and_refresh(path, bytes)?;
        *self.acl.borrow_mut() = GRANTED.into();
        Ok(())
    }
    fn remove_and_refresh(&mut self, path: &Path) -> io::Result<()> {
        // A udev refresh does not remove the already granted ACL.
        self.rule.remove_and_refresh(path)
    }
    fn refresh_wave3_access(&mut self) -> io::Result<()> {
        self.rule.refresh_wave3_access()
    }
    fn snapshot_access(&self) -> io::Result<Vec<AccessSnapshot>> {
        self.captures.set(self.captures.get() + 1);
        Ok(vec![AccessSnapshot {
            node: "/dev/bus/usb/001/015".into(),
            topology: "1-8.3".into(),
            boot_id: "00000000-0000-0000-0000-000000000001".into(),
            filesystem: 1,
            inode: 2,
            device: 3,
            owner: 0,
            group: 0,
            grant_uid: 1000,
            original_acl: self.acl.borrow().clone(),
        }])
    }
    fn verify_access(&self, _: &[AccessSnapshot], state: AccessState) -> io::Result<()> {
        let acl = self.acl.borrow();
        let matches = match state {
            AccessState::Granted => acl.as_str() == GRANTED,
            AccessState::Restored => acl.as_str() == ORIGINAL,
            AccessState::Restorable => acl.as_str() == GRANTED || acl.as_str() == ORIGINAL,
        };
        if matches { Ok(()) } else { Err(io::Error::other("independent ACL change")) }
    }
    fn restore_access(&mut self, saved: &[AccessSnapshot]) -> io::Result<()> {
        if self.restore_fails {
            return Err(io::Error::other("injected ACL restoration failure"));
        }
        self.verify_access(saved, AccessState::Restorable)?;
        assert!(!self.rule.expected.exists());
        *self.acl.borrow_mut() = saved[0].original_acl.clone();
        Ok(())
    }
}

fn lifecycle(fixture: &Fixture) -> Lifecycle<AccessUdev, TestUnitInspector, TestProcessInspector> {
    let paths = test_paths(&fixture.root);
    Lifecycle::new(
        paths.clone(),
        AccessUdev {
            rule: TestUdev { expected: paths.udev_rule, events: Vec::new() },
            acl: Rc::new(RefCell::new(ORIGINAL.into())),
            captures: Cell::new(0),
            restore_fails: false,
        },
        TestUnitInspector { enabled: false, active: false },
        TestProcessInspector::default(),
    )
}
fn install(
    fixture: &Fixture,
    lifecycle: &mut Lifecycle<AccessUdev, TestUnitInspector, TestProcessInspector>,
    revision: &str,
) {
    lifecycle
        .setup(&InstallRequest {
            device_access: crate::lifecycle::model::DeviceAccess::Persistent,
            identity: fixture.identity(revision),
            cli_source: &fixture.cli,
            daemon_source: &fixture.daemon,
            desktop: None,
        })
        .unwrap();
}

#[test]
fn access_baseline_is_durable_preserved_across_upgrade_and_restored_on_removal() {
    let fixture = Fixture::new();
    let mut lifecycle = lifecycle(&fixture);
    install(&fixture, &mut lifecycle, "first");
    assert_eq!(lifecycle.read_manifest().unwrap().unwrap().usb_access[0].original_acl, ORIGINAL);
    install(&fixture, &mut lifecycle, "second");
    assert_eq!(lifecycle.udev.captures.get(), 1);
    assert_eq!(lifecycle.udev.rule.events, vec!["install", "refresh"]);
    assert_eq!(lifecycle.read_manifest().unwrap().unwrap().usb_access[0].original_acl, ORIGINAL);
    assert_eq!(lifecycle.uninstall(false).unwrap(), UninstallOutcome::Removed { kept: Vec::new() });
    assert_eq!(lifecycle.udev.acl.borrow().as_str(), ORIGINAL);
}

#[test]
fn failed_setup_restores_its_access_grant_with_the_rule() {
    let fixture = Fixture::new();
    let mut lifecycle = lifecycle(&fixture);
    lifecycle.fail_after_apply = true;
    assert!(
        lifecycle
            .setup(&InstallRequest {
                device_access: crate::lifecycle::model::DeviceAccess::Persistent,
                identity: fixture.identity("failure"),
                cli_source: &fixture.cli,
                daemon_source: &fixture.daemon,
                desktop: None,
            })
            .is_err()
    );
    assert_eq!(lifecycle.udev.acl.borrow().as_str(), ORIGINAL);
    assert!(!lifecycle.paths.udev_rule.exists());
    assert!(!lifecycle.paths.journal().exists());
}

#[test]
fn independently_changed_permissions_stop_removal_before_the_first_mutation() {
    let fixture = Fixture::new();
    let mut lifecycle = lifecycle(&fixture);
    install(&fixture, &mut lifecycle, "changed");
    let before = fs::read(lifecycle.paths.manifest()).unwrap();
    *lifecycle.udev.acl.borrow_mut() = "foreign permissions".into();
    assert!(lifecycle.doctor(None).iter().any(|check| check.name == "USB access permissions"
        && check.state == super::engine::CheckState::Fail));
    assert!(lifecycle.uninstall(false).is_err());
    assert_eq!(fs::read(lifecycle.paths.manifest()).unwrap(), before);
    assert!(lifecycle.paths.udev_rule.exists());
    assert!(!lifecycle.paths.journal().exists());
    assert_eq!(lifecycle.udev.acl.borrow().as_str(), "foreign permissions");
}

#[test]
fn failed_acl_restoration_retains_records_and_binaries_and_resumes_idempotently() {
    let fixture = Fixture::new();
    let mut lifecycle = lifecycle(&fixture);
    install(&fixture, &mut lifecycle, "interrupted");
    lifecycle.udev.restore_fails = true;
    assert!(lifecycle.uninstall(false).is_err());
    let journal = lifecycle.read_journal().unwrap().unwrap();
    assert_eq!(journal.previous.unwrap().usb_access[0].original_acl, ORIGINAL);
    assert!(lifecycle.paths.active_link().exists());
    assert!(!lifecycle.paths.udev_rule.exists());
    assert_eq!(lifecycle.udev.acl.borrow().as_str(), GRANTED);
    lifecycle.udev.restore_fails = false;
    assert_eq!(lifecycle.uninstall(false).unwrap(), UninstallOutcome::Removed { kept: Vec::new() });
    assert_eq!(lifecycle.udev.acl.borrow().as_str(), ORIGINAL);
    assert!(!lifecycle.paths.journal().exists());
    assert_eq!(lifecycle.uninstall(false).unwrap(), UninstallOutcome::NotInstalled);
}

/// Existing access is outside installation ownership. No permission operation,
/// including read/snapshot/verification, belongs to this mode.
struct ExternalAccess;
impl UdevSystem for ExternalAccess {
    fn read(&self, _: &Path) -> io::Result<Option<Vec<u8>>> {
        panic!("unowned rule read")
    }
    fn install_and_refresh(&mut self, _: &Path, _: &[u8]) -> io::Result<()> {
        panic!("access install")
    }
    fn remove_and_refresh(&mut self, _: &Path) -> io::Result<()> {
        panic!("access removal")
    }
    fn refresh_wave3_access(&mut self) -> io::Result<()> {
        panic!("access refresh")
    }
    fn snapshot_access(&self) -> io::Result<Vec<AccessSnapshot>> {
        panic!("access snapshot")
    }
    fn verify_access(&self, _: &[AccessSnapshot], _: AccessState) -> io::Result<()> {
        panic!("owned access verification")
    }
    fn restore_access(&mut self, _: &[AccessSnapshot]) -> io::Result<()> {
        panic!("access restoration")
    }
}
fn external(
    fixture: &Fixture,
) -> Lifecycle<ExternalAccess, TestUnitInspector, TestProcessInspector> {
    Lifecycle::new(
        test_paths(&fixture.root),
        ExternalAccess,
        TestUnitInspector { enabled: false, active: false },
        TestProcessInspector::default(),
    )
}

#[test]
fn existing_access_install_upgrade_and_removal_preserve_external_permissions_and_profiles() {
    let fixture = Fixture::new();
    let mut lifecycle = external(&fixture);
    let paths = lifecycle.paths.clone();
    fs::create_dir_all(paths.udev_rule.parent().unwrap()).unwrap();
    fs::write(&paths.udev_rule, "externally managed access").unwrap();
    fs::create_dir_all(paths.profiles()).unwrap();
    let profile = paths.profiles().join("mixer-state.json");
    fs::write(&profile, "original profile").unwrap();
    for revision in ["first", "second"] {
        let manifest = lifecycle
            .setup(&InstallRequest {
                device_access: super::model::DeviceAccess::Existing,
                identity: fixture.identity(revision),
                cli_source: &fixture.cli,
                daemon_source: &fixture.daemon,
                desktop: None,
            })
            .unwrap();
        assert!(manifest.usb_access.is_empty());
        assert!(!manifest.owned_paths.iter().any(|p| p.path == paths.udev_rule));
        assert!(
            lifecycle.doctor(None).iter().any(|c| c.name == "USB access permissions"
                && c.state == super::engine::CheckState::Blocked)
        );
    }
    assert_eq!(lifecycle.uninstall(false).unwrap(), UninstallOutcome::Removed { kept: Vec::new() });
    assert_eq!(fs::read(&paths.udev_rule).unwrap(), b"externally managed access");
    assert_eq!(fs::read(profile).unwrap(), b"original profile");
}

#[test]
fn existing_access_failed_setup_rolls_back_user_files_without_permission_operations() {
    let fixture = Fixture::new();
    let mut lifecycle = external(&fixture);
    let unit = lifecycle.paths.unit();
    fs::create_dir_all(unit.parent().unwrap()).unwrap();
    fs::write(&unit, "user unit").unwrap();
    lifecycle.fail_after_apply = true;
    assert!(
        lifecycle
            .setup(&InstallRequest {
                device_access: super::model::DeviceAccess::Existing,
                identity: fixture.identity("failed"),
                cli_source: &fixture.cli,
                daemon_source: &fixture.daemon,
                desktop: None,
            })
            .is_err()
    );
    assert_eq!(fs::read(unit).unwrap(), b"user unit");
    assert!(!lifecycle.paths.manifest().exists());
    assert!(!lifecycle.paths.journal().exists());
}

#[test]
fn access_mode_changes_require_uninstall_and_cannot_drop_owned_grants() {
    let fixture = Fixture::new();
    let mut lifecycle = lifecycle(&fixture);
    install(&fixture, &mut lifecycle, "persistent");
    let before = fs::read(lifecycle.paths.manifest()).unwrap();
    assert!(
        lifecycle
            .setup(&InstallRequest {
                device_access: super::model::DeviceAccess::Existing,
                identity: fixture.identity("existing"),
                cli_source: &fixture.cli,
                daemon_source: &fixture.daemon,
                desktop: None,
            })
            .unwrap_err()
            .to_string()
            .contains("uninstall")
    );
    assert_eq!(fs::read(lifecycle.paths.manifest()).unwrap(), before);
    assert_eq!(lifecycle.udev.acl.borrow().as_str(), GRANTED);
    assert!(lifecycle.paths.udev_rule.exists());
}

#[test]
fn existing_access_manifest_cannot_claim_a_rule_or_acl() {
    let fixture = Fixture::new();
    let mut lifecycle = lifecycle(&fixture);
    install(&fixture, &mut lifecycle, "persistent");
    let mut manifest = lifecycle.read_manifest().unwrap().unwrap();
    manifest.device_access = super::model::DeviceAccess::Existing;
    assert!(super::validation::validate_manifest(&manifest, &lifecycle.paths).is_err());
    manifest.owned_paths.retain(|p| p.path != lifecycle.paths.udev_rule);
    assert!(super::validation::validate_manifest(&manifest, &lifecycle.paths).is_err());
    manifest.usb_access.clear();
    super::validation::validate_manifest(&manifest, &lifecycle.paths).unwrap();
}
