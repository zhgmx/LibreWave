//! Read-only `PipeWire` and `WirePlumber` status probes.

use super::{HostAudioServices, HostServiceInvocations, ServiceAvailability, ServiceFailure};
use std::fmt;
use std::io;
use std::process::Command;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::thread::JoinHandle;
mod command;

/// One complete read-only service observation, including restart identities.
#[derive(Debug)]
pub struct HostServiceObservation {
    pub services: HostAudioServices,
    pub invocations: HostServiceInvocations,
}

/// Runs at most one bounded service observation off the daemon owner thread.
#[derive(Default)]
pub struct ServiceProbe {
    job: Option<JoinHandle<HostServiceObservation>>,
    cancel: Arc<AtomicBool>,
}

impl ServiceProbe {
    /// Starts an observation if the previous job has been collected.
    ///
    /// # Errors
    /// Returns an error if the operating system cannot start the worker.
    pub fn request(&mut self) -> io::Result<()> {
        if self.job.is_some() {
            return Ok(());
        }
        self.start(|cancel| HostServiceObservation {
            services: probe_services_with(&cancel),
            invocations: probe_invocations_with(&cancel),
        })
    }

    fn start(
        &mut self,
        work: impl FnOnce(Arc<AtomicBool>) -> HostServiceObservation + Send + 'static,
    ) -> io::Result<()> {
        let cancel = Arc::clone(&self.cancel);
        self.job = Some(
            std::thread::Builder::new()
                .name("librewave-services".to_owned())
                .spawn(move || work(cancel))?,
        );
        Ok(())
    }

    /// Collects a finished observation without waiting for a running probe.
    pub fn poll(&mut self) -> Option<HostServiceObservation> {
        if !self.job.as_ref()?.is_finished() {
            return None;
        }
        self.job.take().map(|job| job.join().unwrap_or_else(|_| failed_observation()))
    }
}

impl Drop for ServiceProbe {
    fn drop(&mut self) {
        self.cancel.store(true, Ordering::Release);
        if let Some(job) = self.job.take() {
            let _ = job.join();
        }
    }
}

/// Returns an explicit failed observation when a probe worker cannot run.
#[must_use]
pub fn failed_observation() -> HostServiceObservation {
    HostServiceObservation {
        services: HostAudioServices {
            pipewire: ServiceAvailability::Unavailable(ServiceFailure::ProbeFailed),
            wireplumber: ServiceAvailability::Unavailable(ServiceFailure::ProbeFailed),
        },
        invocations: HostServiceInvocations::default(),
    }
}

pub(super) fn probe_services() -> HostAudioServices {
    probe_services_with(&AtomicBool::new(false))
}

pub(super) fn probe_invocations() -> HostServiceInvocations {
    probe_invocations_with(&AtomicBool::new(false))
}

fn probe_services_with(cancel: &AtomicBool) -> HostAudioServices {
    HostAudioServices { pipewire: probe_pipewire(cancel), wireplumber: probe_wireplumber(cancel) }
}

fn probe_invocations_with(cancel: &AtomicBool) -> HostServiceInvocations {
    HostServiceInvocations {
        pipewire: probe_invocation("pipewire.service", cancel),
        wireplumber: probe_invocation("wireplumber.service", cancel),
    }
}

fn probe_invocation(unit: &str, cancel: &AtomicBool) -> Option<String> {
    let output = command::run(
        Command::new("systemctl").args([
            "--user",
            "show",
            "--property=InvocationID",
            "--value",
            unit,
        ]),
        cancel,
    )
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

fn probe_pipewire(cancel: &AtomicBool) -> ServiceAvailability {
    match command::run(Command::new("pw-cli").args(["info", "0"]), cancel) {
        Ok(output) => classify_probe(output.status.success(), &output.stdout, &output.stderr),
        Err(error) => classify_spawn_error(&error),
    }
}

fn probe_wireplumber(cancel: &AtomicBool) -> ServiceAvailability {
    match command::run(
        Command::new("systemctl").args(["--user", "is-active", "wireplumber.service"]),
        cancel,
    ) {
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

    #[test]
    fn pending_service_worker_never_blocks_polling() {
        let (release, wait) = std::sync::mpsc::channel();
        let mut probe = ServiceProbe::default();
        probe
            .start(move |_| {
                wait.recv().expect("release worker");
                failed_observation()
            })
            .expect("start");
        for _ in 0..100 {
            assert!(probe.poll().is_none());
        }
        release.send(()).expect("release");
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            if let Some(observed) = probe.poll() {
                assert_eq!(
                    observed.services.pipewire,
                    ServiceAvailability::Unavailable(ServiceFailure::ProbeFailed)
                );
                break;
            }
            assert!(std::time::Instant::now() < deadline, "worker completion");
            std::thread::yield_now();
        }
    }

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
