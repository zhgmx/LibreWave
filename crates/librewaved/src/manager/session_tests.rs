use super::*;
use crate::audio_session::{AudioSession, AudioSessionStore, FileAudioSessionStore};
use librewave_core::{DesktopDefaults, MixerProfile, MixerSnapshot, Snapshot};
use librewave_platform_linux::desktop_defaults::DesktopDefaultStore;
use std::cell::RefCell;
use std::rc::Rc;
use std::sync::atomic::{AtomicU64, Ordering};

struct Defaults {
    value: Rc<RefCell<DesktopDefaults>>,
    fail_selection: bool,
}
impl DesktopDefaultStore for Defaults {
    fn read(&mut self) -> std::io::Result<DesktopDefaults> {
        Ok(self.value.borrow().clone())
    }
    fn select(&mut self) -> std::io::Result<DesktopDefaults> {
        let mut value = self.value.borrow_mut();
        value.configured_sink = Some("librewave.system".into());
        if self.fail_selection {
            return Err(std::io::Error::other("partial selection failed"));
        }
        value.configured_source = Some("librewave.microphone".into());
        Ok(value.clone())
    }
    fn recover(&mut self) -> std::io::Result<DesktopDefaults> {
        self.read()
    }
    fn restore(&mut self, saved: &DesktopDefaults) -> std::io::Result<DesktopDefaults> {
        let mut value = self.value.borrow_mut();
        if value.configured_sink.as_deref() == Some("librewave.system") {
            value.configured_sink.clone_from(&saved.configured_sink);
        }
        if value.configured_source.as_deref() == Some("librewave.microphone") {
            value.configured_source.clone_from(&saved.configured_source);
        }
        Ok(value.clone())
    }
}
struct Fixture {
    root: std::path::PathBuf,
    value: Rc<RefCell<DesktopDefaults>>,
}
impl Fixture {
    fn new() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        Self {
            root: std::env::temp_dir().join(format!(
                "librewave-session-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            )),
            value: Rc::new(RefCell::new(DesktopDefaults {
                configured_sink: Some("original-output".into()),
                ..DesktopDefaults::default()
            })),
        }
    }
    fn store(&self) -> FileAudioSessionStore {
        FileAudioSessionStore(self.root.join("audio-state.json"))
    }
    fn daemon(&self) -> Daemon {
        let mut daemon = Daemon::from_snapshot(Snapshot::empty());
        daemon.desktop_defaults =
            Box::new(Defaults { value: Rc::clone(&self.value), fail_selection: false });
        daemon.audio_session_store = Box::new(self.store());
        daemon.load_audio_session().expect("load session");
        daemon
    }
    fn request(
        daemon: &mut Daemon,
        command: librewave_core::Command,
    ) -> Result<Response, IpcError> {
        daemon.handle(command, librewave_core::Origin::Client)
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        if self.root.exists() {
            std::fs::remove_dir_all(&self.root).expect("remove fixture");
        }
    }
}
use librewave_core::Command;

#[test]
fn start_intent_survives_daemon_restart_and_stop_cancels_it_before_cleanup() {
    let fixture = Fixture::new();
    let mut first = fixture.daemon();
    Fixture::request(&mut first, Command::StartExperimentalAudio).expect("start intent");
    assert!(fixture.store().load().expect("durable intent").enabled);
    let mut restarted = fixture.daemon();
    assert!(restarted.audio.as_ref().is_some_and(AudioSupervisor::needs_tick));
    assert_eq!(
        restarted.snapshot().mixer.runtime,
        MixerRuntimeState::Inactive { reason: librewave_core::MixerInactiveReason::Starting }
    );
    Fixture::request(&mut restarted, Command::StopAudio).expect("stop intent");
    assert!(!fixture.store().load().expect("durable stop").enabled);
    let stopped = fixture.daemon();
    assert!(stopped.audio.is_none());
}

#[test]
fn process_shutdown_releases_owner_without_cancelling_durable_restart_intent() {
    let fixture = Fixture::new();
    let mut daemon = fixture.daemon();
    Fixture::request(&mut daemon, Command::StartExperimentalAudio).expect("start intent");
    daemon.quiesce_for_shutdown().expect("release process owner");
    assert!(!daemon.audio.as_ref().is_some_and(AudioSupervisor::needs_tick));
    assert!(fixture.store().load().expect("retained intent").enabled);
    let restarted = fixture.daemon();
    assert!(restarted.audio.as_ref().is_some_and(AudioSupervisor::needs_tick));
}

#[test]
fn baseline_survives_partial_selection_restart_and_interrupted_restoration() {
    let fixture = Fixture::new();
    let original = fixture.value.borrow().clone();
    let mut daemon = fixture.daemon();
    Fixture::request(&mut daemon, Command::CaptureDesktopDefaults).expect("baseline");
    fixture.value.borrow_mut().configured_sink = Some("later-output".into());
    Fixture::request(&mut daemon, Command::CaptureDesktopDefaults).expect("idempotent baseline");
    assert_eq!(fixture.store().load().expect("saved baseline").defaults, Some(original.clone()));
    let mut active = Snapshot::empty();
    active.mixer = MixerSnapshot::active(MixerProfile::default());
    daemon.state.replace(active, librewave_core::Origin::Recovery);
    daemon.desktop_defaults =
        Box::new(Defaults { value: Rc::clone(&fixture.value), fail_selection: true });
    assert!(Fixture::request(&mut daemon, Command::SelectDesktopDefaults).is_err());
    assert!(fixture.store().load().expect("selection intent").selected);
    let mut restarted = fixture.daemon();
    Fixture::request(&mut restarted, Command::RestoreDesktopDefaults)
        .expect("restore partial selection");
    assert_eq!(*fixture.value.borrow(), original);
    assert!(fixture.store().load().expect("retained baseline").defaults.is_some());
    let mut after_restore = fixture.daemon();
    Fixture::request(&mut after_restore, Command::RestoreDesktopDefaults)
        .expect("retry restoration");
    Fixture::request(&mut after_restore, Command::CompleteDesktopRestore)
        .expect("verified restoration");
    assert_eq!(fixture.store().load().expect("completed session"), AudioSession::default());
}

#[test]
fn new_default_choices_survive_restoration_and_selection_requires_readiness() {
    let fixture = Fixture::new();
    let mut daemon = fixture.daemon();
    assert_eq!(
        Fixture::request(&mut daemon, Command::SelectDesktopDefaults).expect_err("inactive").kind,
        IpcErrorKind::AudioNotReady
    );
    Fixture::request(&mut daemon, Command::CaptureDesktopDefaults).expect("capture");
    let mut active = Snapshot::empty();
    active.mixer = MixerSnapshot::active(MixerProfile::default());
    daemon.state.replace(active, librewave_core::Origin::Recovery);
    Fixture::request(&mut daemon, Command::SelectDesktopDefaults).expect("select");
    fixture.value.borrow_mut().configured_sink = Some("new-user-output".into());
    fixture.value.borrow_mut().configured_source = Some("librewave.microphone".into());
    Fixture::request(&mut daemon, Command::RestoreDesktopDefaults).expect("restore");
    assert_eq!(fixture.value.borrow().configured_sink.as_deref(), Some("new-user-output"));
    assert_eq!(fixture.value.borrow().configured_source, None);
    assert_eq!(
        fixture.store().load().expect("confirmed restoration baseline").defaults,
        Some(fixture.value.borrow().clone())
    );
    let mut restarted = fixture.daemon();
    *fixture.value.borrow_mut() = DesktopDefaults::default();
    Fixture::request(&mut restarted, Command::RestoreDesktopDefaults)
        .expect("transient empty policy metadata");
    assert_eq!(
        fixture
            .store()
            .load()
            .expect("retained confirmed baseline")
            .defaults
            .expect("baseline remains owned")
            .configured_sink
            .as_deref(),
        Some("new-user-output")
    );
    fixture.value.borrow_mut().configured_sink = Some("librewave.system".into());
    fixture.value.borrow_mut().configured_source = Some("librewave.microphone".into());
    Fixture::request(&mut restarted, Command::RestoreDesktopDefaults)
        .expect("restore replayed policy state");
    assert_eq!(fixture.value.borrow().configured_sink.as_deref(), Some("new-user-output"));
    assert_eq!(fixture.value.borrow().configured_source, None);
    Fixture::request(&mut restarted, Command::CompleteDesktopRestore).expect("complete");
}

#[test]
fn an_active_or_incompletely_restored_session_cannot_discard_the_baseline() {
    let fixture = Fixture::new();
    let mut daemon = fixture.daemon();
    Fixture::request(&mut daemon, Command::CaptureDesktopDefaults).expect("capture");
    Fixture::request(&mut daemon, Command::StartExperimentalAudio).expect("start");
    assert!(Fixture::request(&mut daemon, Command::RestoreDesktopDefaults).is_err());
    assert!(Fixture::request(&mut daemon, Command::CompleteDesktopRestore).is_err());
    Fixture::request(&mut daemon, Command::StopAudio).expect("stop");
    fixture.value.borrow_mut().configured_sink = Some("librewave.system".into());
    assert!(Fixture::request(&mut daemon, Command::CompleteDesktopRestore).is_err());
    assert!(fixture.store().load().expect("retained baseline").defaults.is_some());
}

struct FailStore {
    after: bool,
}
impl AudioSessionStore for FailStore {
    fn load(&mut self) -> Result<AudioSession, PersistenceError> {
        Ok(AudioSession::default())
    }
    fn save(&mut self, _: &AudioSession) -> Result<(), PersistenceError> {
        let error = std::io::Error::other("injected persistence failure");
        Err(if self.after {
            PersistenceError::SaveAfterRename(error)
        } else {
            PersistenceError::SaveBeforeRename(error)
        })
    }
}
#[test]
fn failed_start_persistence_never_opens_an_audio_owner_or_forgets_ambiguity() {
    for after in [false, true] {
        let mut daemon = Daemon::from_snapshot(Snapshot::empty());
        daemon.audio_session_store = Box::new(FailStore { after });
        assert!(Fixture::request(&mut daemon, Command::StartExperimentalAudio).is_err());
        assert!(daemon.audio.is_none());
        assert_eq!(daemon.audio_session.enabled, after);
    }
}

#[test]
fn strict_session_schema_and_serial_derived_names_fail_before_persistence() {
    let fixture = Fixture::new();
    let mut daemon = fixture.daemon();
    fixture.value.borrow_mut().configured_sink = Some("alsa_output.usb-sanitized-fixture".into());
    assert!(Fixture::request(&mut daemon, Command::CaptureDesktopDefaults).is_err());
    assert!(!fixture.store().0.exists());
    fixture.store().save(&AudioSession::default()).expect("create safe file");
    std::fs::write(
        &fixture.store().0,
        br#"{"schema":99,"enabled":false,"selected":false,"defaults":null}"#,
    )
    .expect("unknown schema");
    assert!(matches!(fixture.store().load(), Err(PersistenceError::UnsupportedSchema(99))));
    std::fs::write(
        &fixture.store().0,
        br#"{"schema":1,"enabled":false,"selected":false,"defaults":null,"obsolete":true}"#,
    )
    .expect("unknown field");
    assert!(matches!(fixture.store().load(), Err(PersistenceError::Invalid(_))));
}

#[test]
fn unowned_product_default_names_are_preserved_without_becoming_a_baseline() {
    let fixture = Fixture::new();
    let mut daemon = fixture.daemon();
    fixture.value.borrow_mut().configured_sink = Some("librewave.system".into());
    assert!(Fixture::request(&mut daemon, Command::CaptureDesktopDefaults).is_err());
    assert!(!fixture.store().0.exists());
    Fixture::request(&mut daemon, Command::StopAudio).expect("cancel audio intent");
    Fixture::request(&mut daemon, Command::RestoreDesktopDefaults).expect("no owned defaults");
    Fixture::request(&mut daemon, Command::CompleteDesktopRestore).expect("no baseline to discard");
    assert_eq!(fixture.value.borrow().configured_sink.as_deref(), Some("librewave.system"));
}

#[test]
fn loading_an_accepted_audio_intent_does_not_require_another_file_write() {
    struct ReadOnlyStore;
    impl AudioSessionStore for ReadOnlyStore {
        fn load(&mut self) -> Result<AudioSession, PersistenceError> {
            let mut session = AudioSession::default();
            session.enabled = true;
            Ok(session)
        }
        fn save(&mut self, _: &AudioSession) -> Result<(), PersistenceError> {
            panic!("restart must use the committed intent without rewriting it")
        }
    }
    let mut daemon = Daemon::from_snapshot(Snapshot::empty());
    daemon.audio_session_store = Box::new(ReadOnlyStore);
    daemon.load_audio_session().expect("recover accepted intent");
    assert!(daemon.audio.as_ref().is_some_and(AudioSupervisor::needs_tick));
}
