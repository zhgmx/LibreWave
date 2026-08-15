use std::fs;
use std::io::{self, Write};
use std::os::unix::fs::MetadataExt;
#[cfg(test)]
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use librewave_platform_linux::audio_policy::Wave3AudioPolicy;

pub trait UdevSystem {
    fn read(&self, path: &Path) -> io::Result<Option<Vec<u8>>>;
    fn install_and_refresh(&mut self, path: &Path, content: &[u8]) -> io::Result<()>;
    fn remove_and_refresh(&mut self, path: &Path) -> io::Result<()>;
    fn refresh_wave3_access(&mut self) -> io::Result<()>;
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct UnitState {
    pub enabled: bool,
    pub active: bool,
}

pub trait UnitInspector {
    fn inspect(&self, unit_name: &str) -> io::Result<UnitState>;
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DaemonProcess {
    pub pid: String,
    pub executable: PathBuf,
    pub deleted: bool,
    pub sha256: Option<String>,
}

pub trait ProcessInspector {
    fn daemons(&self) -> io::Result<Vec<DaemonProcess>>;
}

pub struct ProductionProcessInspector;

impl ProcessInspector for ProductionProcessInspector {
    fn daemons(&self) -> io::Result<Vec<DaemonProcess>> {
        let mut processes = Vec::new();
        for entry in fs::read_dir("/proc")? {
            let entry = entry?;
            if !entry.file_name().as_encoded_bytes().iter().all(u8::is_ascii_digit) {
                continue;
            }
            let observed = match fs::read_link(entry.path().join("exe")) {
                Ok(path) => path,
                Err(error)
                    if matches!(
                        error.kind(),
                        io::ErrorKind::NotFound | io::ErrorKind::PermissionDenied
                    ) =>
                {
                    continue;
                }
                Err(error) => return Err(error),
            };
            let Some((executable, deleted)) = parse_daemon_executable(&observed) else {
                continue;
            };
            let sha256 =
                (!deleted).then(|| crate::lifecycle::hash::file(&executable).ok()).flatten();
            processes.push(DaemonProcess {
                pid: entry.file_name().to_string_lossy().into_owned(),
                executable,
                deleted,
                sha256,
            });
        }
        Ok(processes)
    }
}

fn parse_daemon_executable(observed: &Path) -> Option<(PathBuf, bool)> {
    let value = observed.to_string_lossy();
    let (executable, deleted) = value
        .strip_suffix(" (deleted)")
        .map_or_else(|| (observed.to_path_buf(), false), |path| (PathBuf::from(path), true));
    (executable.file_name().and_then(|name| name.to_str()) == Some("librewaved"))
        .then_some((executable, deleted))
}

pub struct ProductionUnitInspector;

impl UnitInspector for ProductionUnitInspector {
    fn inspect(&self, unit_name: &str) -> io::Result<UnitState> {
        let enabled = systemctl_state("is-enabled", unit_name)?;
        let active = systemctl_state("is-active", unit_name)?;
        Ok(UnitState {
            enabled: matches!(enabled.as_str(), "enabled" | "enabled-runtime"),
            active: matches!(active.as_str(), "active" | "activating" | "reloading"),
        })
    }
}

fn systemctl_state(operation: &str, unit_name: &str) -> io::Result<String> {
    let output = Command::new("systemctl").args(["--user", operation, unit_name]).output()?;
    if output.status.code().is_none() {
        Err(io::Error::other(format!("systemctl --user {operation} was terminated")))
    } else {
        parse_systemctl_state(operation, &output.stdout, &output.stderr)
    }
}

fn parse_systemctl_state(operation: &str, stdout: &[u8], stderr: &[u8]) -> io::Result<String> {
    let state = String::from_utf8_lossy(stdout).trim().to_owned();
    let known = match operation {
        "is-enabled" => matches!(
            state.as_str(),
            "enabled"
                | "enabled-runtime"
                | "disabled"
                | "static"
                | "indirect"
                | "masked"
                | "not-found"
        ),
        "is-active" => matches!(
            state.as_str(),
            "active" | "activating" | "reloading" | "inactive" | "failed" | "not-found"
        ),
        _ => false,
    };
    if known {
        Ok(state)
    } else {
        Err(io::Error::other(format!(
            "systemctl --user {operation} failed: {}",
            String::from_utf8_lossy(stderr).trim()
        )))
    }
}

pub struct ProductionUdev {
    expected: PathBuf,
}

impl ProductionUdev {
    pub fn new(expected: PathBuf) -> Self {
        Self { expected }
    }

    fn validate(&self, path: &Path) -> io::Result<()> {
        if path == self.expected {
            Ok(())
        } else {
            Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "refused a udev operation outside the exact LibreWave rule path",
            ))
        }
    }
}

impl UdevSystem for ProductionUdev {
    fn read(&self, path: &Path) -> io::Result<Option<Vec<u8>>> {
        self.validate(path)?;
        read_exact_regular_file(path)
    }

    fn install_and_refresh(&mut self, path: &Path, content: &[u8]) -> io::Result<()> {
        self.validate(path)?;
        let exact = Wave3AudioPolicy::new().render_udev();
        if content != exact.as_bytes() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "refused udev content that differs from the embedded Wave:3 access rule",
            ));
        }
        let expected_hash = crate::lifecycle::hash::bytes(exact.as_bytes());
        let mut child = Command::new("/usr/bin/pkexec")
            .args([
                "/bin/sh",
                "-c",
                "set -eu; check_destination() { test ! -L \"$1\"; if test -e \"$1\"; then test -f \"$1\"; test \"$(/usr/bin/stat -c %a -- \"$1\")\" = 644; actual=$(/usr/bin/sha256sum -- \"$1\"); test \"${actual%% *}\" = \"$2\"; fi; }; check_destination \"$1\" \"$2\"; temporary=\"$1.librewave-root-$$\"; test ! -e \"$temporary\"; test ! -L \"$temporary\"; trap '/usr/bin/rm -f -- \"$temporary\"' EXIT; /usr/bin/install --mode 0644 /dev/stdin \"$temporary\"; check_destination \"$temporary\" \"$2\"; check_destination \"$1\" \"$2\"; /usr/bin/mv -fT -- \"$temporary\" \"$1\"; /usr/bin/udevadm control --reload-rules; /usr/bin/udevadm trigger --action=change --type=devices --subsystem-match=usb --attr-match=idVendor=0fd9 --attr-match=idProduct=0070 --settle; trap - EXIT",
                "librewave-udev-install",
            ])
            .arg(&self.expected)
            .arg(expected_hash)
            .stdin(Stdio::piped())
            .spawn()?;
        child
            .stdin
            .take()
            .ok_or_else(|| io::Error::other("cannot open the privileged installer input"))?
            .write_all(exact.as_bytes())?;
        if child.wait()?.success() {
            Ok(())
        } else {
            Err(io::Error::other("the privileged udev install did not complete"))
        }
    }

    fn remove_and_refresh(&mut self, path: &Path) -> io::Result<()> {
        self.validate(path)?;
        let expected_hash =
            crate::lifecycle::hash::bytes(Wave3AudioPolicy::new().render_udev().as_bytes());
        // The check and removal run in one fixed privileged process. User input
        // cannot select the destination, expected content, or command.
        let status = Command::new("/usr/bin/pkexec")
            .args([
                "/bin/sh",
                "-c",
                "set -eu; if test -e \"$1\" || test -L \"$1\"; then test ! -L \"$1\"; test -f \"$1\"; test \"$(/usr/bin/stat -c %a -- \"$1\")\" = 644; actual=$(/usr/bin/sha256sum -- \"$1\"); test \"${actual%% *}\" = \"$2\"; test ! -L \"$1\"; test \"$(/usr/bin/stat -c %a -- \"$1\")\" = 644; actual=$(/usr/bin/sha256sum -- \"$1\"); test \"${actual%% *}\" = \"$2\"; /usr/bin/rm -- \"$1\"; fi; /usr/bin/udevadm control --reload-rules; /usr/bin/udevadm trigger --action=change --type=devices --subsystem-match=usb --attr-match=idVendor=0fd9 --attr-match=idProduct=0070 --settle",
                "librewave-udev-remove",
            ])
            .arg(&self.expected)
            .arg(expected_hash)
            .status()?;
        if status.success() {
            Ok(())
        } else {
            Err(io::Error::other("the privileged udev removal did not complete"))
        }
    }

    fn refresh_wave3_access(&mut self) -> io::Result<()> {
        let status = Command::new("/usr/bin/pkexec")
            .args([
                "/bin/sh",
                "-c",
                "set -eu; /usr/bin/udevadm control --reload-rules; /usr/bin/udevadm trigger --action=change --type=devices --subsystem-match=usb --attr-match=idVendor=0fd9 --attr-match=idProduct=0070 --settle",
                "librewave-udev-refresh",
            ])
            .status()?;
        if status.success() {
            Ok(())
        } else {
            Err(io::Error::other(
                "the Wave:3 access refresh did not complete; reconnect the microphone, then run `librewavectl doctor`",
            ))
        }
    }
}

#[cfg(test)]
pub struct TestUdev {
    pub expected: PathBuf,
    pub events: Vec<&'static str>,
}

#[cfg(test)]
pub struct TestUnitInspector {
    pub enabled: bool,
    pub active: bool,
}

#[cfg(test)]
#[derive(Default)]
pub struct TestProcessInspector {
    pub daemons: Vec<DaemonProcess>,
}

#[cfg(test)]
impl ProcessInspector for TestProcessInspector {
    fn daemons(&self) -> io::Result<Vec<DaemonProcess>> {
        Ok(self.daemons.clone())
    }
}

#[cfg(test)]
impl UnitInspector for TestUnitInspector {
    fn inspect(&self, _unit_name: &str) -> io::Result<UnitState> {
        Ok(UnitState { enabled: self.enabled, active: self.active })
    }
}

#[cfg(test)]
impl UdevSystem for TestUdev {
    fn read(&self, path: &Path) -> io::Result<Option<Vec<u8>>> {
        assert_eq!(path, self.expected);
        read_exact_regular_file(path)
    }

    fn install_and_refresh(&mut self, path: &Path, content: &[u8]) -> io::Result<()> {
        assert_eq!(path, self.expected);
        self.events.push("install");
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(path, content)?;
        fs::set_permissions(path, fs::Permissions::from_mode(0o644))?;
        self.events.push("refresh");
        Ok(())
    }

    fn remove_and_refresh(&mut self, path: &Path) -> io::Result<()> {
        assert_eq!(path, self.expected);
        self.events.push("remove");
        let result = match fs::remove_file(path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error),
        };
        result?;
        self.events.push("refresh");
        Ok(())
    }

    fn refresh_wave3_access(&mut self) -> io::Result<()> {
        self.events.push("refresh");
        Ok(())
    }
}

fn read_exact_regular_file(path: &Path) -> io::Result<Option<Vec<u8>>> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    if !metadata.is_file() || metadata.file_type().is_symlink() || metadata.mode() & 0o777 != 0o644
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "the udev rule path is not an exact regular mode-0644 file: {}",
                path.display()
            ),
        ));
    }
    fs::read(path).map(Some)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn systemctl_parser_rejects_empty_bus_failures() {
        let error =
            parse_systemctl_state("is-active", b"", b"Failed to connect to bus: No medium found")
                .unwrap_err();
        assert!(error.to_string().contains("Failed to connect to bus"));
    }

    #[test]
    fn systemctl_parser_accepts_explicit_inactive_states() {
        assert_eq!(parse_systemctl_state("is-enabled", b"disabled\n", b"").unwrap(), "disabled");
        assert_eq!(parse_systemctl_state("is-active", b"failed\n", b"").unwrap(), "failed");
    }

    #[test]
    fn daemon_executable_parser_keeps_deleted_processes_visible() {
        assert_eq!(
            parse_daemon_executable(Path::new("/tmp/build/librewaved (deleted)")),
            Some((PathBuf::from("/tmp/build/librewaved"), true))
        );
        assert_eq!(parse_daemon_executable(Path::new("/usr/bin/other")), None);
    }
}
