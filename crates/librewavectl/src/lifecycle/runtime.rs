//! User-initiated service and policy ownership. The daemon alone changes audio and defaults.

use super::engine::{Check, InstallPaths, UNIT_FILE_NAME, fail, pass};
use super::fs_ops::{
    atomic_write, atomic_write_absent, checked_exists, matches_entry, matches_snapshot_destination,
    prepare_snapshot, read_json, saved_snapshot_is_valid,
};
use super::hash;
use super::model::{Manifest, OwnedKind, Snapshot};
use super::system::{
    ProcessInspector, ProductionProcessInspector, ProductionUnitInspector, UnitInspector, UnitState,
};
use super::validation::{validate_manifest, validate_parent_chain};
use librewave_core::{Command, MixerInactiveReason, MixerRuntimeState, ServiceState};
use librewave_ipc::Response;
use librewave_platform_linux::audio_policy::Wave3AudioPolicy;
use librewave_platform_linux::ipc::{Client, ClientError, configured_socket_path};
use librewave_platform_linux::operation_lock::OperationLock;
use librewave_platform_linux::{SessionOperation, session_operation};
use serde::{Deserialize, Serialize};
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
enum Phase {
    Preparing,
    Running,
    Restoring,
    Restored,
}

/// Durable ownership metadata, not a history of validation runs.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct RuntimeManifest {
    schema_version: u32,
    installation: String,
    phase: Phase,
    service_was_active: bool,
    suspended: Option<Snapshot>,
    defaults_captured: bool,
    policy_pending: bool,
}

pub(super) trait RuntimeHost {
    fn unit(&mut self) -> io::Result<UnitState>;
    fn verify_process(&mut self, manifest: &Manifest, running: bool) -> io::Result<()>;
    fn service(&mut self, operation: SessionOperation) -> io::Result<()>;
    fn request(&mut self, command: Command) -> io::Result<Response>;
    fn now(&self) -> Instant {
        Instant::now()
    }
    fn pause(&mut self) {
        std::thread::sleep(Duration::from_millis(50));
    }
}

struct ProductionHost;
impl RuntimeHost for ProductionHost {
    fn unit(&mut self) -> io::Result<UnitState> {
        ProductionUnitInspector.inspect(UNIT_FILE_NAME)
    }
    fn verify_process(&mut self, manifest: &Manifest, running: bool) -> io::Result<()> {
        let processes = ProductionProcessInspector.daemons()?;
        if !running && processes.is_empty() {
            return Ok(());
        }
        let [process] = processes.as_slice() else {
            return Err(io::Error::other(
                "expected exactly one current installed daemon; stop extra or stale processes first",
            ));
        };
        if !running
            || process.deleted
            || process.executable != manifest.active_build_directory.join("bin/librewaved")
            || process.sha256.as_deref() != Some(&manifest.identity.daemon_sha256)
        {
            return Err(io::Error::other(
                "daemon process does not match the installed build; no ownership change was made",
            ));
        }
        Ok(())
    }
    fn service(&mut self, operation: SessionOperation) -> io::Result<()> {
        session_operation(operation)
    }
    fn request(&mut self, command: Command) -> io::Result<Response> {
        Client::new(configured_socket_path()).request(command).map_err(|error| {
            let kind = if matches!(error, ClientError::Connection { .. }) {
                io::ErrorKind::ConnectionRefused
            } else {
                io::ErrorKind::InvalidData
            };
            io::Error::new(kind, error)
        })
    }
}

fn record_path(paths: &InstallPaths) -> PathBuf {
    paths.state_root.join("runtime-manifest.json")
}
fn backup_path(paths: &InstallPaths) -> PathBuf {
    paths.state_root.join("runtime-original.conf")
}

pub(super) fn has_ownership(paths: &InstallPaths) -> io::Result<bool> {
    checked_exists(&record_path(paths))
}

pub(super) fn run(arguments: &[String], output: &mut impl Write) -> io::Result<i32> {
    if arguments.get(2).is_some_and(|arg| matches!(arg.as_str(), "--help" | "-h"))
        && arguments.len() == 3
    {
        writeln!(
            output,
            "Usage: librewavectl start --experimental [--suspend-config <PATH>]\n       librewavectl stop\n\nStart requires a current installation, exact USB access, PipeWire, and WirePlumber.\nThe selected user fragment is backed up and restored on stop or failure.\nService enablement and hardware controls are not changed."
        )?;
        return Ok(0);
    }
    if configured_socket_path() != librewave_platform_linux::ipc::default_socket_path() {
        return Err(invalid(
            "installed service lifecycle requires the normal session socket; unset LIBREWAVE_SOCKET",
        ));
    }
    let paths = InstallPaths::from_environment()?;
    let mut host = ProductionHost;
    match arguments.get(1).map(String::as_str) {
        Some("start") => {
            let suspended = parse_start(&arguments[2..])?;
            start(&paths, suspended.as_deref(), &mut host)?;
            writeln!(
                output,
                "Experimental audio is ready. LibreWave owns the Wave:3 audio graph."
            )?;
        }
        Some("stop") if arguments.len() == 2 => {
            stop(&paths, &mut host)?;
            writeln!(
                output,
                "Audio ownership stopped. Configuration and owned default selections are restored."
            )?;
        }
        _ => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "use `start --experimental [--suspend-config <PATH>]` or `stop`",
            ));
        }
    }
    Ok(0)
}

fn parse_start(arguments: &[String]) -> io::Result<Option<PathBuf>> {
    let mut experimental = false;
    let mut suspended = None;
    let mut index = 0;
    while index < arguments.len() {
        match arguments[index].as_str() {
            "--experimental" if !experimental => experimental = true,
            "--suspend-config" if suspended.is_none() => {
                index += 1;
                suspended =
                    Some(PathBuf::from(arguments.get(index).ok_or_else(|| {
                        invalid("--suspend-config requires an absolute file path")
                    })?));
            }
            _ => return Err(invalid("unknown or repeated start option")),
        }
        index += 1;
    }
    if !experimental {
        return Err(invalid("start requires --experimental"));
    }
    Ok(suspended)
}

pub(super) fn start(
    paths: &InstallPaths,
    suspended: Option<&Path>,
    host: &mut impl RuntimeHost,
) -> io::Result<()> {
    let _lock = lock(paths)?;
    let manifest = installation(paths)?;
    let previous = read_record(paths, &manifest)?;
    let selected = suspended.map(Path::to_path_buf).or_else(|| {
        previous
            .as_ref()
            .and_then(|record| record.suspended.as_ref().map(|saved| saved.destination.clone()))
    });
    let suspended = selected.as_deref();
    if let Some(record) = previous {
        if record.suspended.as_ref().map(|saved| saved.destination.as_path()) != suspended {
            return Err(invalid(
                "an existing ownership operation uses a different suspended file; run `stop` first",
            ));
        }
        if record.phase == Phase::Running {
            verify_taken_files(paths, &record)?;
            let result = (|| {
                ensure_service(host, &manifest)?;
                let Response::Mixer { mixer } = host.request(Command::GetMixer)? else {
                    return Err(unexpected());
                };
                if mixer.runtime == MixerRuntimeState::Active {
                    return Ok(());
                }
                request_kind(host, Command::CheckAudioAccess, |reply| {
                    matches!(reply, Response::AudioAccessChecked)
                })?;
                request_kind(host, Command::StartExperimentalAudio, |reply| {
                    matches!(reply, Response::AudioStarting { .. })
                })?;
                wait_ready(host)
            })();
            return rollback_start(paths, &manifest, record, host, result);
        }
        stop_inner(paths, &manifest, record, host)?;
    }
    if checked_exists(&paths.wireplumber())? || checked_exists(&backup_path(paths))? {
        return Err(io::Error::other(
            "an unowned policy or runtime backup exists; no file was replaced",
        ));
    }
    let saved = match suspended {
        None => None,
        Some(path) => {
            validate_suspended(paths, path)?;
            let saved = prepare_snapshot(path, backup_path(paths))?;
            if saved.kind != Some(OwnedKind::File) {
                return Err(invalid("the selected fragment must be an existing regular file"));
            }
            Some(saved)
        }
    };
    let unit = host.unit()?;
    if unit.enabled {
        return Err(io::Error::other(
            "automatic daemon startup is enabled; disable it before experimental takeover",
        ));
    }
    host.verify_process(&manifest, unit.active)?;
    if unit.active {
        let Response::Mixer { mixer } = host.request(Command::GetMixer)? else {
            return Err(unexpected());
        };
        if !matches!(
            mixer.runtime,
            MixerRuntimeState::Inactive {
                reason: MixerInactiveReason::AudioHostEngineNotConnected
            }
        ) {
            return Err(io::Error::other(
                "daemon already has audio intent outside this lifecycle; stop that audio attempt before takeover",
            ));
        }
    }
    let mut record = RuntimeManifest {
        schema_version: 1,
        installation: manifest.identity.stable_key(),
        phase: Phase::Preparing,
        service_was_active: unit.active,
        suspended: saved,
        defaults_captured: false,
        policy_pending: false,
    };
    save_record(paths, &record)?;
    let result = begin(paths, &manifest, &mut record, host);
    rollback_start(paths, &manifest, record, host, result)
}

fn rollback_start(
    paths: &InstallPaths,
    manifest: &Manifest,
    record: RuntimeManifest,
    host: &mut impl RuntimeHost,
    result: io::Result<()>,
) -> io::Result<()> {
    if let Err(error) = result {
        return match stop_inner(paths, manifest, record, host) {
            Ok(()) => Err(io::Error::new(
                error.kind(),
                format!("start failed: {error}; prior desktop ownership was restored"),
            )),
            Err(cleanup) => Err(io::Error::other(format!(
                "start failed: {error}; restoration is pending: {cleanup}; retain the ownership files and retry `librewavectl stop`"
            ))),
        };
    }
    Ok(())
}

fn begin(
    paths: &InstallPaths,
    manifest: &Manifest,
    record: &mut RuntimeManifest,
    host: &mut impl RuntimeHost,
) -> io::Result<()> {
    ensure_service(host, manifest)?;
    wait_services(host)?;
    request_kind(host, Command::CheckAudioAccess, |reply| {
        matches!(reply, Response::AudioAccessChecked)
    })?;
    // Record the capture intent first. If the daemon commits and the client
    // disappears, stop still asks the daemon to restore its own baseline.
    record.defaults_captured = true;
    save_record(paths, record)?;
    request_kind(host, Command::CaptureDesktopDefaults, |reply| {
        matches!(reply, Response::DesktopDefaults { .. })
    })?;
    record.policy_pending = true;
    save_record(paths, record)?;
    if let Some(saved) = &record.suspended {
        if !matches_snapshot_destination(saved)? {
            return Err(io::Error::other("the selected fragment changed before backup"));
        }
        let bytes = fs::read(&saved.destination)?;
        if hash::bytes(&bytes).as_str() != saved.sha256.as_deref().unwrap_or_default() {
            return Err(io::Error::other("the selected fragment changed before backup"));
        }
        atomic_write_absent(&backup_path(paths), &bytes, 0o600)?;
        if !saved_snapshot_is_valid(saved)? || !matches_snapshot_destination(saved)? {
            return Err(io::Error::other("the selected fragment changed while it was backed up"));
        }
        if !matches_snapshot_destination(saved)? {
            return Err(io::Error::other("selected fragment changed before suspension; preserved"));
        }
        remove_durable(&saved.destination)?;
    }
    atomic_write_absent(
        &paths.wireplumber(),
        Wave3AudioPolicy::new().render_wireplumber().as_bytes(),
        0o644,
    )?;
    host.service(SessionOperation::RestartWirePlumber)?;
    wait_services(host)?;
    request_kind(host, Command::StartExperimentalAudio, |reply| {
        matches!(reply, Response::AudioStarting { .. })
    })?;
    wait_ready(host)?;
    request_kind(host, Command::SelectDesktopDefaults, |reply| {
        matches!(reply, Response::DesktopDefaults { .. })
    })?;
    wait_selected(host)?;
    record.phase = Phase::Running;
    save_record(paths, record)
}

pub(super) fn stop(paths: &InstallPaths, host: &mut impl RuntimeHost) -> io::Result<()> {
    let _lock = lock(paths)?;
    let manifest = installation(paths)?;
    let Some(record) = read_record(paths, &manifest)? else {
        if checked_exists(&paths.wireplumber())? || checked_exists(&backup_path(paths))? {
            return Err(io::Error::other(
                "unowned audio policy or backup exists; refused to remove it",
            ));
        }
        return Ok(());
    };
    stop_inner(paths, &manifest, record, host)
}

fn stop_inner(
    paths: &InstallPaths,
    manifest: &Manifest,
    mut record: RuntimeManifest,
    host: &mut impl RuntimeHost,
) -> io::Result<()> {
    if record.phase == Phase::Restored {
        return finish_stop(paths, manifest, &record, host);
    }
    record.phase = Phase::Restoring;
    save_record(paths, &record)?;
    let unit = host.unit()?;
    host.verify_process(manifest, unit.active)?;
    if record.defaults_captured || unit.active {
        ensure_service(host, manifest)?;
        request_kind(host, Command::StopAudio, |reply| {
            matches!(reply, Response::AudioStopped { .. })
        })?;
        wait(
            host,
            |host| {
                let Response::AudioEndpoints { names } =
                    host.request(Command::GetAudioEndpoints)?
                else {
                    return Err(unexpected());
                };
                Ok(names.is_empty())
            },
            "LibreWave audio nodes remain after cleanup; retain ownership and retry stop",
        )?;
    }
    verify_restorable_files(paths, &record)?;
    if record.defaults_captured {
        request_kind(host, Command::RestoreDesktopDefaults, |reply| {
            matches!(reply, Response::DesktopDefaults { .. })
        })?;
    }
    if checked_exists(&paths.wireplumber())? {
        if !policy_matches(paths)? {
            return Err(io::Error::other("policy changed before removal; preserved"));
        }
        remove_durable(&paths.wireplumber())?;
    }
    if let Some(saved) = &record.suspended
        && !matches_snapshot_destination(saved)?
    {
        if checked_exists(&saved.destination)? {
            return Err(io::Error::other(
                "a new user fragment appeared before restoration; it was preserved",
            ));
        }
        let bytes = fs::read(backup_path(paths))?;
        if hash::bytes(&bytes).as_str() != saved.sha256.as_deref().unwrap_or_default() {
            return Err(io::Error::other("runtime backup changed before restoration"));
        }
        atomic_write_absent(&saved.destination, &bytes, saved.mode.unwrap_or(0o644))?;
    }
    if record.policy_pending {
        host.service(SessionOperation::RestartWirePlumber)?;
        wait_services(host)?;
        if !record.defaults_captured {
            return Err(invalid("policy ownership has no captured defaults"));
        }
        // The restarted policy can replay its earlier configured selections.
        // Restore only still-owned selections against the daemon's confirmed
        // restoration baseline, preserving any later user choice.
        let Response::DesktopDefaults { defaults: expected } =
            host.request(Command::RestoreDesktopDefaults)?
        else {
            return Err(unexpected());
        };
        wait(
            host,
            |host| {
                let Response::DesktopDefaults { defaults } =
                    host.request(Command::GetDesktopDefaults)?
                else {
                    return Err(unexpected());
                };
                Ok(defaults.configured_sink == expected.configured_sink
                    && defaults.configured_source == expected.configured_source
                    && defaults.effective_sink.as_deref() != Some("librewave.system")
                    && defaults.effective_source.as_deref() != Some("librewave.microphone"))
            },
            "desktop defaults were not restored after WirePlumber restart; retry stop",
        )?;
    }
    if record.defaults_captured {
        request_kind(host, Command::CompleteDesktopRestore, |reply| {
            matches!(reply, Response::DesktopDefaults { .. })
        })?;
    }
    record.phase = Phase::Restored;
    save_record(paths, &record)?;
    finish_stop(paths, manifest, &record, host)
}

fn finish_stop(
    paths: &InstallPaths,
    manifest: &Manifest,
    record: &RuntimeManifest,
    host: &mut impl RuntimeHost,
) -> io::Result<()> {
    verify_restorable_files(paths, record)?;
    if checked_exists(&paths.wireplumber())? {
        return Err(io::Error::other("ownership policy reappeared after restoration; preserved"));
    }
    if !record.service_was_active {
        let unit = host.unit()?;
        host.verify_process(manifest, unit.active)?;
        if unit.active {
            host.service(SessionOperation::StopDaemon)?;
        }
        if host.unit()?.active {
            return Err(io::Error::other("daemon service is still active after stop"));
        }
        host.verify_process(manifest, false)?;
    }
    verify_restorable_files(paths, record)?;
    if let Some(saved) = &record.suspended {
        if !matches_snapshot_destination(saved)? {
            return Err(io::Error::other("the original fragment was not restored exactly"));
        }
        if checked_exists(&backup_path(paths))? {
            remove_durable(&backup_path(paths))?;
        }
    }
    remove_durable(&record_path(paths))
}

fn ensure_service(host: &mut impl RuntimeHost, manifest: &Manifest) -> io::Result<()> {
    let unit = host.unit()?;
    host.verify_process(manifest, unit.active)?;
    if !unit.active {
        host.service(SessionOperation::ReloadUnits)?;
        host.service(SessionOperation::StartDaemon)?;
    }
    wait(
        host,
        |host| match host.request(Command::GetMixer) {
            Ok(Response::Mixer { .. }) => {
                host.verify_process(manifest, true)?;
                Ok(true)
            }
            Ok(_) => Err(unexpected()),
            Err(error) if error.kind() == io::ErrorKind::ConnectionRefused => Ok(false),
            Err(error) => Err(error),
        },
        "the installed daemon did not become available; check its user service",
    )
}

fn wait_services(host: &mut impl RuntimeHost) -> io::Result<()> {
    wait(
        host,
        |host| {
            let Response::Status { snapshot } = host.request(Command::GetStatus)? else {
                return Err(unexpected());
            };
            Ok(matches!(snapshot.audio.pipewire, ServiceState::Available)
                && matches!(snapshot.audio.wireplumber, ServiceState::Available))
        },
        "PipeWire and WirePlumber are not available; desktop ownership cannot proceed",
    )
}

fn wait_ready(host: &mut impl RuntimeHost) -> io::Result<()> {
    wait(
        host,
        |host| {
            let Response::Mixer { mixer } = host.request(Command::GetMixer)? else {
                return Err(unexpected());
            };
            match mixer.runtime {
                MixerRuntimeState::Active => Ok(true),
                MixerRuntimeState::Inactive {
                    reason:
                        MixerInactiveReason::Starting
                        | MixerInactiveReason::HostServicesUnavailable
                        | MixerInactiveReason::AudioAttemptFailed,
                } => Ok(false),
                MixerRuntimeState::Inactive { reason } => Err(io::Error::other(format!(
                    "audio is not ready: {reason}; suspend the exact conflicting fragment if desktop ownership remains active"
                ))),
            }
        },
        "audio did not become ready within its startup deadline",
    )
}

fn wait_selected(host: &mut impl RuntimeHost) -> io::Result<()> {
    wait(
        host,
        |host| {
            let Response::DesktopDefaults { defaults } =
                host.request(Command::GetDesktopDefaults)?
            else {
                return Err(unexpected());
            };
            Ok(defaults.configured_sink.as_deref() == Some("librewave.system")
                && defaults.configured_source.as_deref() == Some("librewave.microphone")
                && defaults.effective_sink.as_deref() == Some("librewave.system")
                && defaults.effective_source.as_deref() == Some("librewave.microphone"))
        },
        "LibreWave defaults were configured but not selected by the desktop",
    )
}

fn wait(
    host: &mut impl RuntimeHost,
    mut check: impl FnMut(&mut dyn RuntimeHost) -> io::Result<bool>,
    message: &str,
) -> io::Result<()> {
    let deadline = host.now() + Duration::from_secs(20);
    loop {
        if check(host)? {
            return Ok(());
        }
        if host.now() >= deadline {
            return Err(io::Error::new(io::ErrorKind::TimedOut, message));
        }
        host.pause();
    }
}
fn request_kind(
    host: &mut impl RuntimeHost,
    command: Command,
    check: impl FnOnce(&Response) -> bool,
) -> io::Result<()> {
    if check(&host.request(command)?) { Ok(()) } else { Err(unexpected()) }
}
fn unexpected() -> io::Error {
    io::Error::other(
        "daemon returned an unexpected lifecycle response; rebuild clients and daemon together",
    )
}
fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message)
}

pub(super) fn lock(paths: &InstallPaths) -> io::Result<OperationLock> {
    validate_parent_chain(&paths.state_root.join("runtime-manifest.json"), paths)?;
    fs::create_dir_all(&paths.state_root)?;
    OperationLock::acquire(&paths.state_root)
}
fn installation(paths: &InstallPaths) -> io::Result<Manifest> {
    let manifest = read_json::<Manifest>(&paths.manifest())?.ok_or_else(|| {
        io::Error::other("LibreWave is not installed; build and run `setup --plan` first")
    })?;
    validate_manifest(&manifest, paths)?;
    for owned in &manifest.owned_paths {
        if !matches_entry(owned)? {
            return Err(io::Error::other(format!(
                "installed artifact does not match the manifest: {}; run doctor before start",
                owned.path.display()
            )));
        }
    }
    Ok(manifest)
}
fn save_record(paths: &InstallPaths, record: &RuntimeManifest) -> io::Result<()> {
    atomic_write(
        &record_path(paths),
        &serde_json::to_vec_pretty(record).map_err(io::Error::other)?,
        0o600,
    )
}
fn read_record(paths: &InstallPaths, manifest: &Manifest) -> io::Result<Option<RuntimeManifest>> {
    let record = read_json::<RuntimeManifest>(&record_path(paths))?;
    if let Some(record) = &record {
        if record.schema_version != 1 || record.installation != manifest.identity.stable_key() {
            return Err(io::Error::other(
                "runtime ownership does not match this installation; no recorded path was changed",
            ));
        }
        if record.policy_pending && !record.defaults_captured
            || !record.policy_pending && record.phase == Phase::Running
        {
            return Err(invalid("runtime phase does not match its ownership intent"));
        }
        if let Some(saved) = &record.suspended {
            validate_suspended(paths, &saved.destination)?;
            if saved.kind != Some(OwnedKind::File)
                || saved.saved_content.as_ref() != Some(&backup_path(paths))
                || saved.link_target.is_some()
                || !saved.sha256.as_deref().is_some_and(|hash| {
                    hash.len() == 64
                        && hash
                            .bytes()
                            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
                })
                || saved.mode.is_none_or(|mode| mode > 0o777)
            {
                return Err(io::Error::other("runtime backup metadata is not exact"));
            }
        }
    }
    Ok(record)
}
fn validate_suspended(paths: &InstallPaths, path: &Path) -> io::Result<()> {
    validate_parent_chain(path, paths)?;
    if path.parent() != paths.wireplumber().parent()
        || path == paths.wireplumber()
        || path.extension().and_then(|value| value.to_str()) != Some("conf")
    {
        return Err(invalid(
            "select one absolute .conf file in the user WirePlumber fragment directory, excluding the LibreWave policy",
        ));
    }
    Ok(())
}
fn policy_matches(paths: &InstallPaths) -> io::Result<bool> {
    let snapshot = prepare_snapshot(&paths.wireplumber(), backup_path(paths))?;
    Ok(snapshot.kind == Some(OwnedKind::File)
        && snapshot.mode == Some(0o644)
        && snapshot.sha256.as_deref()
            == Some(hash::bytes(Wave3AudioPolicy::new().render_wireplumber().as_bytes()).as_str()))
}
fn verify_restorable_files(paths: &InstallPaths, record: &RuntimeManifest) -> io::Result<()> {
    validate_parent_chain(&paths.wireplumber(), paths)?;
    validate_parent_chain(&backup_path(paths), paths)?;
    if checked_exists(&paths.wireplumber())? && !policy_matches(paths)? {
        return Err(io::Error::other(
            "LibreWave policy was changed; it was preserved and restoration remains pending",
        ));
    }
    if let Some(saved) = &record.suspended {
        if checked_exists(&saved.destination)? && !matches_snapshot_destination(saved)? {
            return Err(io::Error::other(
                "the suspended fragment was replaced by user content; it was preserved",
            ));
        }
        if checked_exists(&backup_path(paths))? && !saved_snapshot_is_valid(saved)? {
            return Err(io::Error::other("runtime backup was modified; it was preserved"));
        }
        if !matches_snapshot_destination(saved)? && !saved_snapshot_is_valid(saved)? {
            return Err(io::Error::other(
                "the original fragment and verified backup are unavailable",
            ));
        }
    } else if checked_exists(&backup_path(paths))? {
        return Err(io::Error::other("an unowned runtime backup exists; it was preserved"));
    }
    Ok(())
}
fn verify_taken_files(paths: &InstallPaths, record: &RuntimeManifest) -> io::Result<()> {
    verify_restorable_files(paths, record)?;
    if !policy_matches(paths)?
        || record
            .suspended
            .as_ref()
            .is_some_and(|saved| checked_exists(&saved.destination).unwrap_or(true))
    {
        return Err(io::Error::other(
            "runtime ownership files changed; run stop before starting again",
        ));
    }
    Ok(())
}
fn remove_durable(path: &Path) -> io::Result<()> {
    fs::remove_file(path)?;
    if let Some(parent) = path.parent() {
        fs::File::open(parent)?.sync_all()?;
    }
    Ok(())
}

#[cfg(test)]
mod tests;

pub(super) fn file_checks(paths: &InstallPaths, manifest: &Manifest) -> Vec<Check> {
    let result = (|| match read_record(paths, manifest)? {
        Some(record) if record.phase == Phase::Running => {
            verify_taken_files(paths, &record)?;
            Ok(
                "Experimental ownership is recorded. Policy and backups match the ownership journal.",
            )
        }
        Some(_) => Err(io::Error::other(
            "audio restoration or an interrupted start is pending; run `librewavectl stop`",
        )),
        None => {
            if checked_exists(&paths.wireplumber())? || checked_exists(&backup_path(paths))? {
                return Err(io::Error::other(
                    "unowned policy or runtime backup remains; preserve it and inspect the exact path",
                ));
            }
            Ok("No audio ownership is recorded. The takeover policy is absent.")
        }
    })();
    vec![match result {
        Ok(detail) => pass("audio ownership journal", detail),
        Err(error) => fail("audio ownership journal", error.to_string()),
    }]
}

pub(super) fn live_checks(paths: &InstallPaths) -> Vec<Check> {
    let result: io::Result<Option<Check>> = (|| {
        let manifest = installation(paths)?;
        let Some(record) = read_record(paths, &manifest)? else {
            return Ok(None);
        };
        if record.phase != Phase::Running {
            return Ok(None);
        }
        let mut host = ProductionHost;
        diagnose_running(&manifest, &mut host)?;
        Ok(Some(pass(
            "PipeWire graph objects",
            "The installed daemon reports ready audio and all product nodes. Default selections are observed through IPC.",
        )))
    })();
    match result {
        Ok(Some(check)) => vec![check],
        Ok(None) => Vec::new(),
        Err(error) => vec![fail("PipeWire graph objects", error.to_string())],
    }
}
fn diagnose_running(manifest: &Manifest, host: &mut impl RuntimeHost) -> io::Result<()> {
    if !host.unit()?.active {
        return Err(io::Error::other(
            "recorded audio ownership has no active daemon service; run start or stop",
        ));
    }
    host.verify_process(manifest, true)?;
    let Response::Mixer { mixer } = host.request(Command::GetMixer)? else {
        return Err(unexpected());
    };
    if mixer.runtime != MixerRuntimeState::Active {
        return Err(io::Error::other(format!("recorded audio is not ready: {:?}", mixer.runtime)));
    }
    let Response::AudioEndpoints { names } = host.request(Command::GetAudioEndpoints)? else {
        return Err(unexpected());
    };
    for name in [
        "librewave.system",
        "librewave.microphone",
        "librewave.monitor-mix",
        "librewave.stream-mix",
    ] {
        if names.iter().filter(|observed| observed.as_str() == name).count() != 1 {
            return Err(io::Error::other(format!("expected one product node: {name}")));
        }
    }
    request_kind(host, Command::GetDesktopDefaults, |response| {
        matches!(response, Response::DesktopDefaults { .. })
    })
}
