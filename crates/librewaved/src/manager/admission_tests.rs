//! Integrated daemon control ownership, startup admission, and audio lifecycle.
use super::*;
use crate::audio::tests::{Control, FakeHost};
use librewave_core::MixerInactiveReason;
use std::cell::Cell;

struct Bus {
    claims: Rc<Cell<usize>>,
    reads: Rc<Cell<usize>>,
    incarnation: Cell<u64>,
    deny: Cell<bool>,
    config: RefCell<Wave3ConfigSnapshot>,
}

struct ClaimedConnection {
    bus: Rc<Bus>,
    incarnation: u64,
    config: Wave3ConfigSnapshot,
    writes: Rc<RefCell<Vec<Wave3Control>>>,
}
impl Drop for ClaimedConnection {
    fn drop(&mut self) {
        assert_eq!(self.bus.claims.get(), 1);
        self.bus.claims.set(0);
    }
}
impl ManagedWave3Connection for ClaimedConnection {
    fn api(&self) -> librewave_core::ApiVersion {
        librewave_core::ApiVersion::new(5, 4)
    }
    fn observed(&self) -> Result<Wave3ConfigSnapshot, String> {
        Ok(self.config.clone())
    }
    fn refresh(&mut self) -> ConnectionRefreshOutcome {
        assert_eq!(self.bus.claims.get(), 1);
        self.bus.reads.set(self.bus.reads.get() + 1);
        if self.incarnation != self.bus.incarnation.get() {
            return ConnectionRefreshOutcome::Failed(AdmissionError::Disconnected);
        }
        if self.bus.deny.get() {
            return ConnectionRefreshOutcome::Failed(AdmissionError::PermissionDenied);
        }
        let next = self.bus.config.borrow().clone();
        if next == self.config {
            ConnectionRefreshOutcome::Unchanged(next)
        } else {
            self.config = next.clone();
            ConnectionRefreshOutcome::Changed(next)
        }
    }
    fn apply(&mut self, control: Wave3Control) -> ConnectionOutcome {
        self.writes.borrow_mut().push(control);
        panic!("startup or recovery issued a hardware write")
    }
}

fn integrated() -> (Harness, Rc<Bus>, AudioSupervisor<FakeHost>, Rc<Control>) {
    let mut desired = DesiredState::default();
    desired.stage("1-8.3".into(), Wave3Control::MicrophoneMute(true)).expect("desired fixture");
    desired.promote("1-8.3").expect("desired fixture");
    let mut h = harness(desired, wave3_config(), vec![], vec![]);
    let card = fs::canonicalize(h.daemon.paths.usb_devices.join("1-8.3"))
        .expect("USB fixture")
        .join("1-8.3:1.0/sound/card7");
    fs::create_dir_all(&card).expect("ALSA sysfs fixture");
    let sound = h.daemon.paths.sound_cards.join("card7");
    fs::create_dir_all(&sound).expect("ALSA class fixture");
    symlink(&card, sound.join("device")).expect("ALSA correlation");
    fs::create_dir_all(h.daemon.paths.asound_root.join("card7")).expect("ALSA proc fixture");
    fs::write(
        &h.daemon.paths.asound_cards,
        include_str!("../../../librewave-platform-linux/tests/fixtures/proc/asound/cards"),
    )
    .expect("ALSA cards fixture");
    fs::write(h.daemon.paths.asound_root.join("card7/id"), "Wave3\n").expect("card identity");
    let bus = Rc::new(Bus {
        incarnation: Cell::new(1),
        config: RefCell::new(wave3_config()),
        claims: Rc::new(Cell::new(0)),
        reads: Rc::new(Cell::new(0)),
        deny: Cell::new(false),
    });
    let owned = Rc::clone(&bus);
    let opens = Rc::clone(&h.connections);
    let writes = Rc::clone(&h.calls);
    h.daemon.connector = Box::new(move |_| {
        assert_eq!(owned.claims.get(), 0, "a second USB open competed with the daemon claim");
        if owned.deny.get() {
            return Err(AdmissionError::PermissionDenied);
        }
        *opens.borrow_mut() += 1;
        owned.claims.set(1);
        Ok(Box::new(ClaimedConnection {
            bus: Rc::clone(&owned),
            incarnation: owned.incarnation.get(),
            config: owned.config.borrow().clone(),
            writes: Rc::clone(&writes),
        }))
    });
    let mut host_control = Control::default();
    host_control.claim = Some(Rc::clone(&bus.claims));
    host_control.admission_reads = Some(Rc::clone(&bus.reads));
    let control = Rc::new(host_control);
    let host = Rc::clone(&control);
    let audio = AudioSupervisor::with_factory(move |_, _| {
        host.creates.set(host.creates.get() + 1);
        Ok(FakeHost(Rc::clone(&host)))
    });
    (h, bus, audio, control)
}

#[test]
fn observed_claim_is_retained_through_preflight_start_and_automatic_audio_retry() {
    let (mut h, bus, mut audio, control) = integrated();
    h.daemon.observe_controls(Instant::now());
    let old_generation = h.daemon.snapshot().devices[0].admission.generation().expect("generation");
    assert_eq!(*h.connections.borrow(), 1);
    h.daemon.handle(Command::CheckAudioAccess, Origin::Client).expect("preflight through owner");
    assert_eq!(bus.reads.get(), 1);
    h.daemon.handle(Command::StartAudio, Origin::Client).expect("durable start intent");
    bus.config.borrow_mut().input_gain.raw = 4726;
    h.daemon.supervise_audio(&mut audio, false);
    assert_eq!(control.creates.get(), 0, "maintenance cannot start an attempt");
    h.daemon.supervise_audio(&mut audio, true);
    assert_eq!(audio.runtime(), MixerRuntimeState::Active);
    assert_eq!(bus.reads.get(), 2, "the attempt needs a new read after preflight");
    assert_eq!(
        h.daemon.snapshot().devices[0].admission.config().expect("config").input_gain.raw,
        4726
    );
    assert_eq!(
        &*control.events.borrow(),
        &["physical check", "capture", "capture progress", "playback", "endpoints", "routes"]
    );
    h.daemon.supervise_audio(&mut audio, true);
    assert_eq!(bus.reads.get(), 2, "an active graph must not restart its admission");
    control.progress_error.set(true);
    h.daemon.supervise_audio(&mut audio, true);
    assert_eq!(bus.claims.get(), 1, "audio teardown must not release the control claim");
    control.progress_error.set(false);
    std::thread::sleep(Duration::from_millis(2050));
    bus.config.borrow_mut().input_gain.raw = 3938;
    h.daemon.supervise_audio(&mut audio, true);
    assert_eq!(audio.runtime(), MixerRuntimeState::Active);
    assert_eq!(bus.reads.get(), 3);
    assert_eq!(*h.connections.borrow(), 1);
    assert_eq!(control.creates.get(), 2);
    let error = h
        .daemon
        .handle(
            Command::SetWave3Control {
                id: DeviceId(1),
                expected_generation: old_generation,
                control: Wave3Control::Clipguard(true),
            },
            Origin::Client,
        )
        .expect_err("stale client");
    assert!(matches!(error.kind, IpcErrorKind::StaleDeviceGeneration { .. }));
    audio.stop().expect("stop audio");
    h.daemon.handle(Command::StopAudio, Origin::Client).expect("cancel durable intent");
    assert_eq!(bus.claims.get(), 1);
    assert!(h.calls.borrow().is_empty());
    assert_eq!(*h.saves.borrow(), 0, "no desired hardware settings were restored");
}

#[test]
fn disconnect_permission_failure_and_reconnect_require_fresh_owned_admission() {
    let (mut h, bus, mut audio, control) = integrated();
    h.daemon.observe_controls(Instant::now());
    let old_generation = h.daemon.snapshot().devices[0].admission.generation().expect("generation");
    h.daemon.supervise_audio(&mut audio, true);
    assert_eq!(audio.runtime(), MixerRuntimeState::Active);
    bus.incarnation.set(2);
    let due = h.daemon.control_reads[&DeviceId(1)];
    h.daemon.observe_controls(due);
    assert_eq!(bus.claims.get(), 0, "the disconnected handle must be dropped");
    control.progress_error.set(true);
    h.daemon.supervise_audio(&mut audio, true);
    control.progress_error.set(false);
    bus.deny.set(true);
    std::thread::sleep(Duration::from_millis(2050));
    let captures = control.events.borrow().iter().filter(|s| **s == "capture").count();
    h.daemon.supervise_audio(&mut audio, true);
    assert_eq!(
        audio.runtime(),
        MixerRuntimeState::Inactive { reason: MixerInactiveReason::DeviceAccessDenied }
    );
    assert_eq!(
        control.events.borrow().iter().filter(|s| **s == "capture").count(),
        captures,
        "failed admission must precede capture"
    );
    assert_eq!(bus.claims.get(), 0);
    assert_eq!(*h.connections.borrow(), 1);
    bus.deny.set(false);
    bus.config.borrow_mut().volume_select = librewave_core::VolumeSelection::Headphone;
    std::thread::sleep(Duration::from_millis(2050));
    h.daemon.supervise_audio(&mut audio, true);
    assert_eq!(audio.runtime(), MixerRuntimeState::Active);
    assert_eq!(*h.connections.borrow(), 2);
    assert_eq!(bus.claims.get(), 1);
    assert_ne!(h.daemon.snapshot().devices[0].admission.generation(), Some(old_generation));
    let mut stale = h.daemon.candidates[&DeviceId(1)].clone();
    stale.alsa_cards[0].number += 1;
    let reads = bus.reads.get();
    assert!(h.daemon.admit_audio_candidate(&stale, Origin::Recovery).is_err());
    assert_eq!(bus.reads.get(), reads, "stale correlation must fail before USB reads");
    assert_eq!(*h.connections.borrow(), 2);
    assert!(h.calls.borrow().is_empty());
    assert_eq!(*h.saves.borrow(), 0);
    audio.stop().expect("stop rebuilt audio");
}

#[test]
fn a_failed_fresh_read_blocks_capture_even_with_an_admitted_cached_baseline() {
    let (mut h, bus, mut audio, control) = integrated();
    h.daemon.observe_controls(Instant::now());
    assert!(matches!(
        h.daemon.snapshot().devices[0].admission,
        DeviceAdmissionSnapshot::Admitted { .. }
    ));
    bus.deny.set(true);
    h.daemon.supervise_audio(&mut audio, true);
    assert_eq!(
        audio.runtime(),
        MixerRuntimeState::Inactive { reason: MixerInactiveReason::DeviceAccessDenied }
    );
    assert_eq!(&*control.events.borrow(), &["physical check", "cleanup"]);
    assert_eq!(bus.reads.get(), 1);
    assert_eq!(bus.claims.get(), 0);
    assert_eq!(*h.connections.borrow(), 1);
    assert!(h.calls.borrow().is_empty());
    assert_eq!(*h.saves.borrow(), 0);
}
