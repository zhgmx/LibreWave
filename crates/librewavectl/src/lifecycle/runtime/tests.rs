#![allow(clippy::unwrap_used)]
use super::*;
use crate::lifecycle::engine::test_paths;
use crate::lifecycle::tests::Fixture;
use librewave_core::{AudioSnapshot, DesktopDefaults, MixerSnapshot, Snapshot as DaemonSnapshot};
use std::os::unix::fs::{PermissionsExt, symlink};

#[allow(clippy::struct_excessive_bools)]
struct Host {
    active: bool,
    audio: bool,
    defaults: DesktopDefaults,
    baseline: Option<DesktopDefaults>,
    failure: Option<String>,
    interrupt: Option<String>,
    cleanup_fails: bool,
    access_fails: bool,
    partial_selection: bool,
    reason: Option<MixerInactiveReason>,
    events: Vec<String>,
    clock: Instant,
    stale_nodes: bool,
}
impl Host {
    fn new() -> Self {
        Self {
            active: false,
            clock: Instant::now(),
            stale_nodes: false,
            baseline: None,
            audio: false,
            defaults: DesktopDefaults {
                configured_sink: Some("wave3-sink".into()),
                configured_source: Some("wave3-source".into()),
                effective_sink: Some("wave3-sink".into()),
                effective_source: Some("wave3-source".into()),
            },
            failure: None,
            interrupt: None,
            cleanup_fails: false,
            access_fails: false,
            partial_selection: false,
            reason: None,
            events: Vec::new(),
        }
    }
    fn event(&mut self, event: &str) -> io::Result<()> {
        self.events.push(event.into());
        if self.interrupt.as_deref() == Some(event) {
            self.interrupt = None;
            panic!("simulated interrupted process");
        }
        if self.failure.as_deref() == Some(event) {
            self.failure = None;
            return Err(io::Error::other("injected operation failure"));
        }
        Ok(())
    }
}
impl RuntimeHost for Host {
    fn unit(&mut self) -> io::Result<UnitState> {
        Ok(UnitState { enabled: false, active: self.active })
    }
    fn verify_process(&mut self, _: &Manifest, running: bool) -> io::Result<()> {
        if running != self.active {
            return Err(io::Error::other("fake process does not match service"));
        }
        Ok(())
    }
    fn service(&mut self, operation: SessionOperation) -> io::Result<()> {
        self.event(&format!("service {operation:?}"))?;
        match operation {
            SessionOperation::StartDaemon => self.active = true,
            SessionOperation::StopDaemon => {
                self.active = false;
                self.audio = false;
            }
            _ => {}
        }
        Ok(())
    }
    #[allow(clippy::too_many_lines)]
    fn request(&mut self, command: Command) -> io::Result<Response> {
        if !self.active {
            return Err(io::Error::other("fake service is not running"));
        }
        self.event(&format!(
            "request {}",
            match &command {
                Command::GetMixer => "GetMixer",
                Command::GetStatus => "GetStatus",
                Command::CheckAudioAccess => "CheckAudioAccess",
                Command::GetDesktopDefaults => "GetDesktopDefaults",
                Command::CaptureDesktopDefaults => "CaptureDefaults",
                Command::StartExperimentalAudio => "StartAudio",
                Command::StopAudio => "StopAudio",
                Command::SelectDesktopDefaults => "SelectDefaults",
                Command::RestoreDesktopDefaults => "RestoreDefaults",
                Command::CompleteDesktopRestore => "CompleteRestore",
                _ => "Other",
            }
        ))?;
        let mixer = if self.audio {
            MixerSnapshot::active(librewave_core::MixerProfile::default())
        } else if let Some(reason) = self.reason {
            MixerSnapshot::inactive_for(librewave_core::MixerProfile::default(), reason)
        } else {
            MixerSnapshot::default()
        };
        match command {
            Command::GetMixer => Ok(Response::Mixer { mixer }),
            Command::GetAudioEndpoints => Ok(Response::AudioEndpoints {
                names: if self.audio || self.stale_nodes {
                    vec![
                        "librewave.system".into(),
                        "librewave.microphone".into(),
                        "librewave.monitor-mix".into(),
                        "librewave.stream-mix".into(),
                    ]
                } else {
                    Vec::new()
                },
            }),
            Command::GetStatus => Ok(Response::Status {
                snapshot: DaemonSnapshot {
                    audio: AudioSnapshot {
                        pipewire: ServiceState::Available,
                        wireplumber: ServiceState::Available,
                    },
                    ..DaemonSnapshot::empty()
                },
            }),
            Command::CheckAudioAccess if self.access_fails => {
                Err(io::Error::other("USB permission denied"))
            }
            Command::CheckAudioAccess => Ok(Response::AudioAccessChecked),
            Command::GetDesktopDefaults => {
                Ok(Response::DesktopDefaults { defaults: self.defaults.clone() })
            }
            Command::CaptureDesktopDefaults => {
                let mut baseline = self.defaults.clone();
                baseline.effective_sink = None;
                baseline.effective_source = None;
                if [&baseline.configured_sink, &baseline.configured_source]
                    .into_iter()
                    .flatten()
                    .any(|name| {
                        name.starts_with("alsa_output.usb-") || name.starts_with("alsa_input.usb-")
                    })
                {
                    return Err(io::Error::other("serial-derived baseline"));
                }
                self.baseline.get_or_insert(baseline);
                Ok(Response::DesktopDefaults { defaults: self.defaults.clone() })
            }
            Command::StartExperimentalAudio => {
                self.audio = self.reason.is_none();
                Ok(Response::AudioStarting { mixer })
            }
            Command::StopAudio if self.cleanup_fails => {
                Err(io::Error::other("cleanup pending; owner retained"))
            }
            Command::StopAudio => {
                self.audio = false;
                Ok(Response::AudioStopped { mixer: MixerSnapshot::default() })
            }
            Command::SelectDesktopDefaults => {
                self.defaults.configured_sink = Some("librewave.system".into());
                self.defaults.effective_sink = self.defaults.configured_sink.clone();
                if self.partial_selection {
                    self.partial_selection = false;
                    return Err(io::Error::other("partial default selection failed"));
                }
                self.defaults.configured_source = Some("librewave.microphone".into());
                self.defaults.effective_source = self.defaults.configured_source.clone();
                Ok(Response::DesktopDefaults { defaults: self.defaults.clone() })
            }
            Command::RestoreDesktopDefaults => {
                let defaults = self.baseline.clone().unwrap_or_else(|| self.defaults.clone());
                if self.defaults.configured_sink.as_deref() == Some("librewave.system") {
                    self.defaults.configured_sink = defaults.configured_sink;
                }
                if self.defaults.configured_source.as_deref() == Some("librewave.microphone") {
                    self.defaults.configured_source = defaults.configured_source;
                }
                self.defaults.effective_sink = self.defaults.configured_sink.clone();
                self.defaults.effective_source = self.defaults.configured_source.clone();
                Ok(Response::DesktopDefaults { defaults: self.defaults.clone() })
            }
            Command::CompleteDesktopRestore => {
                self.baseline = None;
                Ok(Response::DesktopDefaults { defaults: self.defaults.clone() })
            }
            _ => Err(unexpected()),
        }
    }
    fn now(&self) -> Instant {
        self.clock
    }
    fn pause(&mut self) {
        self.clock += Duration::from_secs(1);
    }
}

fn fixture() -> (Fixture, InstallPaths, PathBuf) {
    let fixture = Fixture::new();
    let mut installer = fixture.lifecycle();
    fixture.install(&mut installer, "runtime-build");
    let paths = test_paths(&fixture.root);
    let fragment = paths.wireplumber().with_file_name("51-elgato.conf");
    fs::create_dir_all(fragment.parent().unwrap()).unwrap();
    fs::write(&fragment, "original user configuration\n").unwrap();
    fs::set_permissions(&fragment, fs::Permissions::from_mode(0o640)).unwrap();
    (fixture, paths, fragment)
}
fn restored(paths: &InstallPaths, fragment: &Path, host: &Host) {
    assert_eq!(fs::read(fragment).unwrap(), b"original user configuration\n");
    assert_eq!(fs::metadata(fragment).unwrap().permissions().mode() & 0o777, 0o640);
    assert!(!paths.wireplumber().exists());
    assert!(!record_path(paths).exists());
    assert!(!backup_path(paths).exists());
    assert!(!host.audio);
}

#[test]
fn start_stop_owns_exact_files_and_preserves_unrelated_configuration() {
    let (_fixture, paths, fragment) = fixture();
    let unrelated = fragment.with_file_name("70-user.conf");
    fs::write(&unrelated, "keep me").unwrap();
    let mut host = Host::new();
    let prior = host.defaults.clone();
    start(&paths, Some(&fragment), &mut host).unwrap();
    assert!(host.audio);
    assert!(!fragment.exists());
    assert!(policy_matches(&paths).unwrap());
    assert_eq!(host.defaults.configured_sink.as_deref(), Some("librewave.system"));
    let count = host.events.iter().filter(|event| *event == "service RestartWirePlumber").count();
    start(&paths, None, &mut host).unwrap();
    assert_eq!(
        host.events.iter().filter(|event| *event == "service RestartWirePlumber").count(),
        count
    );
    stop(&paths, &mut host).unwrap();
    restored(&paths, &fragment, &host);
    assert!(!host.active);
    assert_eq!(host.defaults, prior);
    assert_eq!(fs::read_to_string(unrelated).unwrap(), "keep me");
    let count = host.events.len();
    stop(&paths, &mut host).unwrap();
    assert_eq!(host.events.len(), count);
}

#[test]
fn access_failure_precedes_policy_changes_and_rolls_back_launched_service() {
    let (_fixture, paths, fragment) = fixture();
    let mut host = Host::new();
    host.access_fails = true;
    assert!(start(&paths, Some(&fragment), &mut host).is_err());
    restored(&paths, &fragment, &host);
    assert!(!host.active);
    assert!(
        !host
            .events
            .iter()
            .any(|event| event == "service RestartWirePlumber" || event == "request StartAudio")
    );
}

#[test]
fn startup_and_partial_default_failures_restore_every_owned_change() {
    for phase in [
        "service StartDaemon",
        "service RestartWirePlumber",
        "request StartAudio",
        "request SelectDefaults",
        "partial",
    ] {
        let (_fixture, paths, fragment) = fixture();
        let mut host = Host::new();
        if phase == "partial" {
            host.partial_selection = true;
        } else {
            host.failure = Some(phase.into());
        }
        assert!(start(&paths, Some(&fragment), &mut host).is_err(), "{phase}");
        restored(&paths, &fragment, &host);
        assert!(!host.active);
        assert_eq!(host.defaults.configured_sink.as_deref(), Some("wave3-sink"));
        assert_eq!(host.defaults.configured_source.as_deref(), Some("wave3-source"));
    }
}

#[test]
fn failed_audio_cleanup_blocks_policy_and_default_restoration_until_retry() {
    let (_fixture, paths, fragment) = fixture();
    let mut host = Host::new();
    start(&paths, Some(&fragment), &mut host).unwrap();
    host.cleanup_fails = true;
    let before = host.events.len();
    assert!(stop(&paths, &mut host).is_err());
    assert!(host.audio);
    assert!(policy_matches(&paths).unwrap());
    assert!(!fragment.exists());
    assert!(
        !host.events[before..].iter().any(
            |event| event == "request RestoreDefaults" || event == "service RestartWirePlumber"
        )
    );
    host.cleanup_fails = false;
    stop(&paths, &mut host).unwrap();
    restored(&paths, &fragment, &host);
}

#[test]
fn interrupted_start_and_stop_are_recoverable_from_owned_metadata() {
    for phase in [
        "service StartDaemon",
        "request GetDesktopDefaults",
        "service RestartWirePlumber",
        "request StartAudio",
        "request SelectDefaults",
    ] {
        let (_fixture, paths, fragment) = fixture();
        let mut host = Host::new();
        host.interrupt = Some(phase.into());
        assert!(
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| start(
                &paths,
                Some(&fragment),
                &mut host
            )))
            .is_err()
        );
        assert!(record_path(&paths).exists());
        stop(&paths, &mut host).unwrap();
        restored(&paths, &fragment, &host);
    }
    let (_fixture, paths, fragment) = fixture();
    let mut host = Host::new();
    start(&paths, Some(&fragment), &mut host).unwrap();
    host.interrupt = Some("service RestartWirePlumber".into());
    assert!(
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| stop(&paths, &mut host))).is_err()
    );
    stop(&paths, &mut host).unwrap();
    restored(&paths, &fragment, &host);
}

#[test]
fn changed_user_files_and_backups_are_preserved_and_keep_restoration_pending() {
    for changed in ["user", "policy", "backup"] {
        let (_fixture, paths, fragment) = fixture();
        let mut host = Host::new();
        start(&paths, Some(&fragment), &mut host).unwrap();
        let target = match changed {
            "user" => fragment.clone(),
            "policy" => paths.wireplumber(),
            _ => backup_path(&paths),
        };
        fs::write(&target, "new user content").unwrap();
        assert!(stop(&paths, &mut host).is_err());
        assert!(!host.audio);
        assert!(record_path(&paths).exists());
        assert_eq!(fs::read_to_string(&target).unwrap(), "new user content");
    }
}

#[test]
fn new_default_choices_and_preexisting_service_are_preserved() {
    let (_fixture, paths, fragment) = fixture();
    let mut host = Host::new();
    host.active = true;
    start(&paths, Some(&fragment), &mut host).unwrap();
    host.defaults.configured_sink = Some("new-user-output".into());
    stop(&paths, &mut host).unwrap();
    restored(&paths, &fragment, &host);
    assert!(host.active);
    assert_eq!(host.defaults.configured_sink.as_deref(), Some("new-user-output"));
    assert_eq!(host.defaults.configured_source.as_deref(), Some("wave3-source"));
}

#[test]
fn malicious_paths_symlinks_and_concurrent_operations_stop_before_mutation() {
    let (_fixture, paths, fragment) = fixture();
    let mut host = Host::new();
    let outside = paths.state_root.join("outside.conf");
    fs::write(&outside, "foreign").unwrap();
    assert!(start(&paths, Some(&outside), &mut host).is_err());
    assert!(host.events.is_empty());
    let link = fragment.with_file_name("linked.conf");
    symlink(&fragment, &link).unwrap();
    assert!(start(&paths, Some(&link), &mut host).is_err());
    assert!(host.events.is_empty());
    let held = lock(&paths).unwrap();
    assert!(start(&paths, Some(&fragment), &mut host).is_err());
    drop(held);
    start(&paths, Some(&fragment), &mut host).unwrap();
    let manifest = installation(&paths).unwrap();
    let mut record = read_record(&paths, &manifest).unwrap().unwrap();
    record.suspended.as_mut().unwrap().destination = outside.clone();
    save_record(&paths, &record).unwrap();
    let count = host.events.len();
    assert!(stop(&paths, &mut host).is_err());
    assert_eq!(host.events.len(), count);
    assert_eq!(fs::read_to_string(outside).unwrap(), "foreign");
}

#[test]
fn serial_derived_default_names_fail_before_configuration_takeover() {
    let (_fixture, paths, fragment) = fixture();
    let mut host = Host::new();
    host.defaults.configured_sink = Some("alsa_output.usb-sanitized-example".into());
    assert!(start(&paths, Some(&fragment), &mut host).is_err());
    restored(&paths, &fragment, &host);
    assert!(!host.events.iter().any(|event| event == "service RestartWirePlumber"));
}

#[test]
fn deadlines_restore_failed_start_and_preserve_policy_when_nodes_remain() {
    let (_fixture, paths, fragment) = fixture();
    let mut host = Host::new();
    host.reason = Some(MixerInactiveReason::Starting);
    let error = start(&paths, Some(&fragment), &mut host).unwrap_err();
    assert!(error.to_string().contains("deadline"));
    restored(&paths, &fragment, &host);
    host.reason = None;
    start(&paths, Some(&fragment), &mut host).unwrap();
    host.stale_nodes = true;
    assert!(stop(&paths, &mut host).unwrap_err().to_string().contains("nodes remain"));
    assert!(policy_matches(&paths).unwrap());
    assert!(!fragment.exists());
    assert!(host.baseline.is_some());
    host.stale_nodes = false;
    stop(&paths, &mut host).unwrap();
    restored(&paths, &fragment, &host);
}
#[test]
fn a_failed_repeated_start_restores_the_original_takeover() {
    let (_fixture, paths, fragment) = fixture();
    let mut host = Host::new();
    start(&paths, Some(&fragment), &mut host).unwrap();
    host.audio = false;
    host.access_fails = true;
    assert!(start(&paths, Some(&fragment), &mut host).is_err());
    restored(&paths, &fragment, &host);
    assert!(!host.active);
}
#[test]
fn interrupted_final_service_stop_resumes_without_relaunching_or_restarting_desktop() {
    let (_fixture, paths, fragment) = fixture();
    let mut host = Host::new();
    start(&paths, Some(&fragment), &mut host).unwrap();
    host.interrupt = Some("service StopDaemon".into());
    assert!(
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| stop(&paths, &mut host))).is_err()
    );
    let count = host.events.len();
    stop(&paths, &mut host).unwrap();
    assert_eq!(&host.events[count..], &["service StopDaemon"]);
    restored(&paths, &fragment, &host);
}
#[test]
fn setup_and_uninstall_refuse_recorded_audio_and_concurrent_ownership_operations() {
    let (fixture, paths, fragment) = fixture();
    let mut host = Host::new();
    let mut installer = fixture.lifecycle();
    let held = lock(&paths).unwrap();
    assert!(installer.uninstall(false).is_err());
    drop(held);
    start(&paths, Some(&fragment), &mut host).unwrap();
    assert!(installer.uninstall(false).is_err());
    assert!(
        installer
            .setup(&super::super::engine::InstallRequest {
                identity: fixture.identity("replacement"),
                cli_source: &fixture.cli,
                daemon_source: &fixture.daemon
            })
            .is_err()
    );
    assert!(paths.manifest().exists());
    stop(&paths, &mut host).unwrap();
    installer.uninstall(false).unwrap();
}
#[test]
fn doctor_checks_the_recorded_runtime_and_reports_modified_ownership_files() {
    let (_fixture, paths, fragment) = fixture();
    let mut host = Host::new();
    start(&paths, Some(&fragment), &mut host).unwrap();
    let manifest = installation(&paths).unwrap();
    assert!(
        file_checks(&paths, &manifest)
            .iter()
            .all(|check| check.state == super::super::engine::CheckState::Pass)
    );
    diagnose_running(&manifest, &mut host).unwrap();
    fs::write(paths.wireplumber(), "new user policy").unwrap();
    assert!(
        file_checks(&paths, &manifest)
            .iter()
            .any(|check| check.state == super::super::engine::CheckState::Fail)
    );
}
