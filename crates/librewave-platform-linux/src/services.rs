//! Read-only `PipeWire` and `WirePlumber` status probes.

use super::{HostAudioServices, HostServiceInvocations, ServiceAvailability, ServiceFailure};
use std::fmt;
use std::io;
use std::process::Command;

pub(super) fn probe_services() -> HostAudioServices {
    HostAudioServices { pipewire: probe_pipewire(), wireplumber: probe_wireplumber() }
}

pub(super) fn probe_invocations() -> HostServiceInvocations {
    HostServiceInvocations {
        pipewire: probe_invocation("pipewire.service"),
        wireplumber: probe_invocation("wireplumber.service"),
    }
}

fn probe_invocation(unit: &str) -> Option<String> {
    let output = Command::new("systemctl")
        .args(["--user", "show", "--property=InvocationID", "--value", unit])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    parse_invocation(&output.stdout)
}

fn parse_invocation(bytes: &[u8]) -> Option<String> {
    let value = std::str::from_utf8(bytes).ok()?.trim();
    (value.len() == 32 && value.bytes().all(|byte| byte.is_ascii_hexdigit()))
        .then(|| value.to_ascii_lowercase())
}

fn probe_pipewire() -> ServiceAvailability {
    match Command::new("pw-cli").args(["info", "0"]).output() {
        Ok(output) => classify_probe(output.status.success(), &output.stdout, &output.stderr),
        Err(error) => classify_spawn_error(&error),
    }
}

fn probe_wireplumber() -> ServiceAvailability {
    match Command::new("systemctl").args(["--user", "is-active", "wireplumber.service"]).output() {
        Ok(output) => {
            classify_wireplumber_probe(output.status.success(), &output.stdout, &output.stderr)
        }
        Err(error) => classify_spawn_error(&error),
    }
}

fn classify_wireplumber_probe(success: bool, stdout: &[u8], stderr: &[u8]) -> ServiceAvailability {
    if success && String::from_utf8_lossy(stdout).trim() == "active" {
        ServiceAvailability::Available
    } else {
        classify_probe(false, stdout, stderr)
    }
}

fn classify_probe(success: bool, stdout: &[u8], stderr: &[u8]) -> ServiceAvailability {
    if success {
        return ServiceAvailability::Available;
    }
    let output = format!("{} {}", String::from_utf8_lossy(stdout), String::from_utf8_lossy(stderr))
        .to_ascii_lowercase();
    let failure = if output.contains("permission denied")
        || output.contains("operation not permitted")
        || output.contains("access denied")
    {
        ServiceFailure::PermissionDenied
    } else if output.contains("not found") || output.contains("no such file") {
        ServiceFailure::NotInstalled
    } else if output.contains("could not connect")
        || output.contains("failed to connect")
        || output.contains("inactive")
        || output.contains("failed")
    {
        ServiceFailure::NotRunning
    } else {
        ServiceFailure::ProbeFailed
    };
    ServiceAvailability::Unavailable(failure)
}

fn classify_spawn_error(error: &io::Error) -> ServiceAvailability {
    let failure = match error.kind() {
        io::ErrorKind::NotFound => ServiceFailure::NotInstalled,
        io::ErrorKind::PermissionDenied => ServiceFailure::PermissionDenied,
        _ => ServiceFailure::ProbeFailed,
    };
    ServiceAvailability::Unavailable(failure)
}

impl fmt::Display for ServiceFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let text = match self {
            ServiceFailure::NotInstalled => "not installed",
            ServiceFailure::PermissionDenied => "permission denied",
            ServiceFailure::NotRunning => "not running",
            ServiceFailure::ProbeFailed => "probe failed",
        };
        formatter.write_str(text)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIXTURE_PIPEWIRE_FAILURE: &str =
        include_str!("../tests/fixtures/services/pipewire-not-running.txt");
    const FIXTURE_WIREPLUMBER_ACTIVE: &str =
        include_str!("../tests/fixtures/services/wireplumber-active.txt");

    #[test]
    fn classifies_status_probe_failures() {
        assert_eq!(
            classify_probe(false, b"", b"Operation not permitted"),
            ServiceAvailability::Unavailable(ServiceFailure::PermissionDenied)
        );
        assert_eq!(
            classify_probe(false, b"", b"Could not connect to PipeWire"),
            ServiceAvailability::Unavailable(ServiceFailure::NotRunning)
        );
        assert_eq!(
            classify_probe(false, b"", b"No such file or directory"),
            ServiceAvailability::Unavailable(ServiceFailure::NotInstalled)
        );
        assert_eq!(classify_probe(true, b"object", b""), ServiceAvailability::Available);
    }

    #[test]
    fn classifies_service_fixtures() {
        assert_eq!(
            classify_probe(false, b"", FIXTURE_PIPEWIRE_FAILURE.as_bytes()),
            ServiceAvailability::Unavailable(ServiceFailure::NotRunning)
        );
        assert_eq!(
            classify_wireplumber_probe(true, FIXTURE_WIREPLUMBER_ACTIVE.as_bytes(), b""),
            ServiceAvailability::Available
        );
    }

    #[test]
    fn invocation_probe_accepts_only_a_complete_hex_token() {
        assert_eq!(
            parse_invocation(b"0123456789abcdef0123456789ABCDEF\n"),
            Some("0123456789abcdef0123456789abcdef".to_owned())
        );
        for invalid in [b"".as_slice(), b"0\n", b"0123456789abcdef0123456789abcdeg\n"] {
            assert_eq!(parse_invocation(invalid), None);
        }
    }
}
