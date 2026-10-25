use super::{artifacts::quote_unit_value, engine::InstallPaths, model::DesktopConfig};
use std::{io, path::PathBuf};

pub(super) const UNIT: &str = "librewave-desktop.service";
/// The desktop ID. The client window uses the same application ID, so the
/// desktop shows this entry's icon for it.
const APPLICATION_ID: &str = "com.zhgmx.LibreWave";
const ENTRY: &str = "com.zhgmx.LibreWave.desktop";
const ICON: &str = include_str!("../../../../assets/librewave.svg");

fn data_home(paths: &InstallPaths) -> io::Result<&std::path::Path> {
    paths.data_root.parent().ok_or_else(|| io::Error::other("data root has no parent"))
}

pub(super) fn application_entry(paths: &InstallPaths) -> io::Result<PathBuf> {
    Ok(data_home(paths)?.join("applications").join(ENTRY))
}

/// The application icon in the user `hicolor` theme.
pub(super) fn application_icon(paths: &InstallPaths) -> io::Result<PathBuf> {
    Ok(data_home(paths)?.join(format!("icons/hicolor/scalable/apps/{APPLICATION_ID}.svg")))
}

/// Desktop artifacts that live outside the `LibreWave` roots.
pub(super) fn is_shared_path(path: &std::path::Path, paths: &InstallPaths) -> io::Result<bool> {
    Ok(path == application_entry(paths)? || path == application_icon(paths)?)
}

pub(super) fn files(
    paths: &InstallPaths,
    config: &DesktopConfig,
) -> io::Result<Vec<(PathBuf, String, u32)>> {
    let active = paths.active_link();
    let cli = quote_unit_value(&active.join("bin/librewavectl").to_string_lossy(), true);
    let gui = quote_unit_value(&active.join("bin/librewave").to_string_lossy(), true);
    let environment =
        quote_unit_value(&format!("XDG_CONFIG_HOME={}", paths.config_root.display()), false);
    let suspend = config
        .suspend_config
        .as_ref()
        .map(|path| {
            super::validation::validate_systemd_path(path)?;
            if !path.is_absolute() {
                return Err(io::Error::other("suspended configuration path must be absolute"));
            }
            Ok(format!(" --suspend-config {}", quote_unit_value(&path.to_string_lossy(), true)))
        })
        .transpose()?
        .unwrap_or_default();
    let service = format!(
        "[Unit]\nDescription=LibreWave desktop and mixer startup\nAfter=graphical-session.target pipewire.service wireplumber.service\nWants=pipewire.service wireplumber.service\nPartOf=graphical-session.target\n\n[Service]\nType=exec\nExecStartPre={cli} start{suspend}\nExecStart={gui}\nEnvironment={environment}\nTimeoutStartSec=90\n"
    );
    let entry = format!(
        "[Desktop Entry]\nType=Application\nName=LibreWave\nComment=Control the Wave microphone and software mixer\nExec=/usr/bin/systemctl --user start {UNIT}\nIcon={APPLICATION_ID}\nStartupWMClass={APPLICATION_ID}\nTerminal=false\nCategories=AudioVideo;Audio;\n"
    );
    let login = format!("{entry}Hidden={}\n", !config.autostart);
    Ok(vec![
        (paths.config_root.join("systemd/user").join(UNIT), service, 0o644),
        (
            paths.bin_root.join("librewave"),
            format!("#!/bin/sh\nexec /usr/bin/systemctl --user start {UNIT}\n"),
            0o755,
        ),
        (application_entry(paths)?, entry, 0o644),
        (paths.config_root.join("autostart").join(ENTRY), login, 0o644),
        (application_icon(paths)?, ICON.to_owned(), 0o644),
    ])
}

pub(super) fn resolve_config(
    mut config: DesktopConfig,
    previous: Option<DesktopConfig>,
    autostart: Option<bool>,
) -> DesktopConfig {
    if let Some(previous) = previous {
        if config.suspend_config.is_none() {
            config.suspend_config = previous.suspend_config;
        }
        config.autostart = previous.autostart;
    }
    if let Some(autostart) = autostart {
        config.autostart = autostart;
    }
    config
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lifecycle::{
        engine::{DesktopInstall, InstallRequest, UninstallOutcome},
        hash,
        model::DeviceAccess,
        tests::Fixture,
    };
    use std::fs;

    #[test]
    fn failed_service_reload_rolls_back_desktop_artifacts() {
        struct FailFirstReload(std::cell::Cell<bool>);
        impl crate::lifecycle::system::UnitInspector for FailFirstReload {
            fn inspect(&self, _: &str) -> io::Result<crate::lifecycle::system::UnitState> {
                Ok(crate::lifecycle::system::UnitState { enabled: false, active: false })
            }
            fn reload(&self) -> io::Result<()> {
                if self.0.replace(false) {
                    Err(io::Error::other("injected reload failure"))
                } else {
                    Ok(())
                }
            }
        }
        let fixture = Fixture::new();
        let base = fixture.lifecycle();
        let mut lifecycle = crate::lifecycle::engine::Lifecycle::new(
            base.paths,
            base.udev,
            FailFirstReload(std::cell::Cell::new(true)),
            base.process_inspector,
        );
        let mut identity = fixture.identity("reload-failure");
        identity.gui_sha256 = Some(hash::file(&fixture.cli).expect("hash"));
        assert!(
            lifecycle
                .setup(&InstallRequest {
                    device_access: DeviceAccess::Persistent,
                    identity,
                    cli_source: &fixture.cli,
                    daemon_source: &fixture.daemon,
                    desktop: Some(DesktopInstall {
                        gui_source: &fixture.cli,
                        config: DesktopConfig { autostart: true, suspend_config: None }
                    }),
                })
                .is_err()
        );
        assert!(!lifecycle.paths.active_link().exists());
        assert!(!lifecycle.paths.bin_root.join("librewave").exists());
        assert!(!application_entry(&lifecycle.paths).expect("path").exists());
        assert!(!lifecycle.paths.manifest().exists());
    }

    #[test]
    fn upgrade_preserves_login_and_legacy_loader_unless_explicitly_changed() {
        let previous = DesktopConfig {
            autostart: true,
            suspend_config: Some(PathBuf::from("/tmp/legacy.conf")),
        };
        let requested = DesktopConfig { autostart: false, suspend_config: None };
        assert_eq!(resolve_config(requested.clone(), Some(previous.clone()), None), previous);
        let disabled = resolve_config(requested, Some(previous.clone()), Some(false));
        assert!(!disabled.autostart);
        assert_eq!(disabled.suspend_config, previous.suspend_config);
    }

    #[test]
    fn desktop_install_upgrade_and_uninstall_preserve_original_launchers() {
        let fixture = Fixture::new();
        let mut lifecycle = fixture.lifecycle();
        let entry = application_entry(&lifecycle.paths).expect("application path");
        fs::create_dir_all(entry.parent().expect("parent")).expect("directory");
        fs::write(&entry, b"original desktop entry").expect("original");
        let config = DesktopConfig {
            autostart: true,
            suspend_config: Some(fixture.root.join("legacy.conf")),
        };
        let install = |lifecycle: &mut crate::lifecycle::engine::Lifecycle<_, _, _>,
                       revision: &str| {
            let mut identity = fixture.identity(revision);
            identity.gui_sha256 = Some(hash::file(&fixture.cli).expect("GUI fixture hash"));
            lifecycle
                .setup(&InstallRequest {
                    device_access: DeviceAccess::Persistent,
                    identity,
                    cli_source: &fixture.cli,
                    daemon_source: &fixture.daemon,
                    desktop: Some(DesktopInstall {
                        gui_source: &fixture.cli,
                        config: config.clone(),
                    }),
                })
                .expect("install desktop")
        };
        let first = install(&mut lifecycle, "desktop-1");
        assert!(first.active_build_directory.join("bin/librewave").is_file());
        let second = install(&mut lifecycle, "desktop-2");
        assert!(!first.active_build_directory.exists());
        assert_eq!(second.desktop, Some(config));
        assert!(
            fs::read_to_string(lifecycle.paths.config_root.join("autostart").join(ENTRY))
                .expect("login")
                .contains("Hidden=false")
        );
        assert!(
            lifecycle
                .doctor(None)
                .iter()
                .all(|check| check.state != crate::lifecycle::engine::CheckState::Fail)
        );
        assert_eq!(
            lifecycle.uninstall(false).expect("uninstall"),
            UninstallOutcome::Removed { kept: Vec::new() }
        );
        assert_eq!(fs::read(&entry).expect("restored"), b"original desktop entry");
        assert!(!lifecycle.paths.bin_root.join("librewave").exists());
    }

    #[test]
    fn failed_desktop_setup_restores_original_files_and_keeps_no_active_build() {
        let fixture = Fixture::new();
        let mut lifecycle = fixture.lifecycle();
        let entry = application_entry(&lifecycle.paths).expect("path");
        fs::create_dir_all(entry.parent().expect("parent")).expect("directory");
        fs::write(&entry, b"original").expect("entry");
        lifecycle.fail_after_apply = true;
        let mut identity = fixture.identity("failed-desktop");
        identity.gui_sha256 = Some(hash::file(&fixture.cli).expect("hash"));
        assert!(
            lifecycle
                .setup(&InstallRequest {
                    device_access: DeviceAccess::Persistent,
                    identity,
                    cli_source: &fixture.cli,
                    daemon_source: &fixture.daemon,
                    desktop: Some(DesktopInstall {
                        gui_source: &fixture.cli,
                        config: DesktopConfig { autostart: true, suspend_config: None }
                    }),
                })
                .is_err()
        );
        assert_eq!(fs::read(&entry).expect("restored"), b"original");
        assert!(!lifecycle.paths.active_link().exists());
        assert!(!lifecycle.paths.bin_root.join("librewave").exists());
        assert!(!lifecycle.paths.manifest().exists());
    }

    #[test]
    fn startup_is_single_service_and_disabled_without_explicit_login_choice() {
        let fixture = Fixture::new();
        let lifecycle = fixture.lifecycle();
        let config = DesktopConfig {
            autostart: false,
            suspend_config: Some(fixture.root.join("legacy.conf")),
        };
        let files = files(&lifecycle.paths, &config).expect("render");
        let unit = &files[0].1;
        assert!(unit.contains("start --suspend-config"));
        assert!(unit.contains("legacy.conf"));
        assert!(
            files[1].1.contains("exec /usr/bin/systemctl --user start librewave-desktop.service")
        );
        assert!(
            files[2].1.contains("Exec=/usr/bin/systemctl --user start librewave-desktop.service")
        );
        assert!(files[3].1.contains("Hidden=true"));
        assert!(files[2].1.contains("Icon=com.zhgmx.LibreWave\n"));
        assert_eq!(files[4].0, application_icon(&lifecycle.paths).expect("icon path"));
        assert!(files[4].1.starts_with("<svg"));
        assert!(!unit.contains("ExecStop="));
    }
}
