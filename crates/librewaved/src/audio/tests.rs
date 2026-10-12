use super::*;
use librewave_core::EndpointId;
use librewave_platform_linux::audio_lifecycle::{
    CaptureConsumption, CaptureObservation, HiddenPhysicalNode,
};
use librewave_platform_linux::{AlsaCardInfo, LinuxInventory};
use std::cell::{Cell, RefCell};
use std::rc::Rc;

#[derive(Default)]
struct Control {
    events: RefCell<Vec<&'static str>>,
    meters: RefCell<std::collections::VecDeque<librewave_engine::BlockMeters>>,
    creates: Cell<usize>,
    cleanup_failures: Cell<usize>,
    admission_error: RefCell<Option<LinuxAudioError>>,
    physical_error: Cell<bool>,
    progress_error: Cell<bool>,
}

struct FakeHost(Rc<Control>);

impl AudioHost for FakeHost {
    type Error = LinuxAudioError;
    fn verify_physical_nodes_hidden(
        &mut self,
        _: &UsbDeviceCandidate,
        _: &[HiddenPhysicalNode],
    ) -> Result<(), Self::Error> {
        self.0.events.borrow_mut().push("physical check");
        if self.0.physical_error.get() { Err(LinuxAudioError::PhysicalNodeExposed) } else { Ok(()) }
    }
    fn start_capture(&mut self, _: &UsbDeviceCandidate) -> Result<CaptureConsumption, Self::Error> {
        if let Some(error) = self.0.admission_error.borrow_mut().take() {
            return Err(error);
        }
        self.0.events.borrow_mut().push("capture");
        Ok(CaptureConsumption::new())
    }
    fn observe_capture(
        &mut self,
        _: &UsbDeviceCandidate,
        _: &CaptureConsumption,
    ) -> Result<CaptureObservation, Self::Error> {
        self.0.events.borrow_mut().push("capture progress");
        Ok(CaptureObservation { active: true, frames_consumed: 256 })
    }
    fn prepare_playback(&mut self, _: &UsbDeviceCandidate) -> Result<(), Self::Error> {
        self.0.events.borrow_mut().push("playback");
        Ok(())
    }
    fn publish_endpoints(
        &mut self,
        _: &UsbDeviceCandidate,
        _: &[EndpointId],
    ) -> Result<(), Self::Error> {
        self.0.events.borrow_mut().push("endpoints");
        Ok(())
    }
    fn restore_software_routing(&mut self, _: &UsbDeviceCandidate) -> Result<(), Self::Error> {
        self.0.events.borrow_mut().push("routes");
        Ok(())
    }
    fn teardown(&mut self) -> Result<(), Self::Error> {
        self.0.events.borrow_mut().push("cleanup");
        let remaining = self.0.cleanup_failures.get();
        if remaining > 0 {
            self.0.cleanup_failures.set(remaining - 1);
            Err(LinuxAudioError::TeardownFailed)
        } else {
            Ok(())
        }
    }
}
impl SupervisedAudioHost for FakeHost {
    fn take_meters(&mut self) -> Option<librewave_engine::BlockMeters> {
        self.0.meters.borrow_mut().pop_front()
    }
    fn poll(&mut self) -> Result<(), LinuxAudioError> {
        if self.0.progress_error.get() { Err(LinuxAudioError::TeardownFailed) } else { Ok(()) }
    }
    fn stage_profile(&mut self, _: &MixerProfile) -> Result<(), LinuxAudioError> {
        Ok(())
    }
}
fn supervisor() -> (AudioSupervisor<FakeHost>, Rc<Control>) {
    let control = Rc::new(Control::default());
    let shared = Rc::clone(&control);
    (
        AudioSupervisor::with_factory(move |_, _| {
            shared.creates.set(shared.creates.get() + 1);
            Ok(FakeHost(Rc::clone(&shared)))
        }),
        control,
    )
}
fn candidate() -> UsbDeviceCandidate {
    let fixture = crate::manager::tests::discovery_fixture();
    let mut candidate = LinuxInventory::new().discover(&fixture.paths).devices.remove(0);
    candidate.alsa_cards =
        vec![AlsaCardInfo { number: 7, id: Some("Wave3".to_owned()), name: None }];
    candidate
}
fn tick(
    audio: &mut AudioSupervisor<FakeHost>,
    candidate: Option<&UsbDeviceCandidate>,
    allow_start: bool,
) {
    audio.tick(
        candidate,
        &ServiceState::Available,
        &ServiceState::Available,
        &HostServiceInvocations::default(),
        &MixerProfile::default(),
        allow_start,
    );
}
fn reason(audio: &AudioSupervisor<FakeHost>) -> MixerInactiveReason {
    let MixerRuntimeState::Inactive { reason } = audio.runtime() else {
        panic!("expected inactive")
    };
    reason
}

#[test]
fn start_stop_preserves_order_and_repeated_start_keeps_the_owner() {
    let (mut audio, control) = supervisor();
    tick(&mut audio, Some(&candidate()), false);
    assert_eq!(control.creates.get(), 0, "frame maintenance cannot start audio");
    tick(&mut audio, Some(&candidate()), true);
    assert_eq!(audio.runtime(), MixerRuntimeState::Active);
    assert_eq!(
        &*control.events.borrow(),
        &["physical check", "capture", "capture progress", "playback", "endpoints", "routes"]
    );
    audio.enable().expect("valid fake lifecycle transition");
    tick(&mut audio, Some(&candidate()), true);
    assert_eq!(control.creates.get(), 1);
    assert_eq!(audio.next_epoch, 2);
    audio.stop().expect("valid fake lifecycle transition");
    audio.stop().expect("valid fake lifecycle transition");
    tick(&mut audio, Some(&candidate()), true);
    assert_eq!(control.creates.get(), 1, "stop disables automatic recovery");
    assert_eq!(reason(&audio), MixerInactiveReason::AudioHostEngineNotConnected);
    assert_eq!(control.events.borrow().iter().filter(|event| **event == "cleanup").count(), 1);
    audio.enable().expect("restart after clean stop");
    tick(&mut audio, Some(&candidate()), true);
    assert_eq!(control.creates.get(), 2);
    assert_eq!(audio.next_epoch, 3, "stop/start preserves the attempt counter");
}

#[test]
fn failed_stop_retains_owner_blocks_start_and_retries_cleanup_without_restarting() {
    let (mut audio, control) = supervisor();
    tick(&mut audio, Some(&candidate()), true);
    control.cleanup_failures.set(1);
    assert!(audio.stop().is_err());
    assert_eq!(reason(&audio), MixerInactiveReason::CleanupPending);
    assert!(audio.attempt.is_some());
    assert!(audio.enable().is_err());
    audio.retry_at = Instant::now();
    tick(&mut audio, Some(&candidate()), true);
    assert!(audio.attempt.is_none());
    assert_eq!(control.creates.get(), 1);
    assert_eq!(reason(&audio), MixerInactiveReason::AudioHostEngineNotConnected);
    audio.enable().expect("valid fake lifecycle transition");
    tick(&mut audio, Some(&candidate()), true);
    assert_eq!(control.creates.get(), 2);
}

#[test]
fn access_and_ownership_errors_are_distinct_and_retry_requires_clean_teardown() {
    for (error, expected) in [
        (
            LinuxAudioError::DeviceAdmission(AdmissionError::PermissionDenied),
            MixerInactiveReason::DeviceAccessDenied,
        ),
        (
            LinuxAudioError::DeviceAdmission(AdmissionError::MalformedResponse),
            MixerInactiveReason::DeviceAdmissionFailed,
        ),
    ] {
        let (mut audio, control) = supervisor();
        *control.admission_error.borrow_mut() = Some(error);
        tick(&mut audio, Some(&candidate()), true);
        assert_eq!(reason(&audio), expected);
        assert!(!control.events.borrow().contains(&"playback"));
        assert!(audio.attempt.is_none());
        audio.retry_at = Instant::now();
        tick(&mut audio, Some(&candidate()), true);
        assert_eq!(audio.runtime(), MixerRuntimeState::Active);
    }
    let (mut audio, control) = supervisor();
    control.physical_error.set(true);
    control.cleanup_failures.set(2);
    tick(&mut audio, Some(&candidate()), true);
    assert_eq!(reason(&audio), MixerInactiveReason::CleanupPending);
    audio.retry_at = Instant::now();
    tick(&mut audio, Some(&candidate()), true);
    assert_eq!(control.creates.get(), 1, "failed cleanup blocks a replacement");
    assert_eq!(reason(&audio), MixerInactiveReason::CleanupPending);
    control.physical_error.set(false);
    audio.retry_at = Instant::now();
    tick(&mut audio, Some(&candidate()), true);
    audio.retry_at = Instant::now();
    tick(&mut audio, Some(&candidate()), true);
    assert_eq!(control.creates.get(), 2);
}

#[test]
fn missing_device_or_service_does_not_construct_an_audio_host() {
    let (mut audio, control) = supervisor();
    tick(&mut audio, None, true);
    assert_eq!(reason(&audio), MixerInactiveReason::NoSingleDevice);
    audio.tick(
        Some(&candidate()),
        &ServiceState::Unavailable { reason: librewave_core::ServiceFailureReason::NotRunning },
        &ServiceState::Available,
        &HostServiceInvocations::default(),
        &MixerProfile::default(),
        true,
    );
    assert_eq!(reason(&audio), MixerInactiveReason::HostServicesUnavailable);
    assert_eq!(control.creates.get(), 0);
}

#[test]
fn software_meter_values_expire_and_cannot_survive_stop_or_restart() {
    use librewave_core::{MICROPHONE_SOURCE_ID, MeterSnapshot, SYSTEM_SOURCE_ID};
    use librewave_engine::{InputBuffer, MixerConfig, MixerEngine, OutputBuffer};
    let profile = MixerProfile::default();
    let controls: Vec<_> = profile.sources.iter().map(|s| s.controls).collect();
    let config =
        MixerConfig::try_new(1, MICROPHONE_SOURCE_ID, &[MICROPHONE_SOURCE_ID, SYSTEM_SOURCE_ID])
            .expect("meter engine config");
    let (mut engine, _stager) = MixerEngine::new(config, &controls).expect("meter engine");
    let mut microphone = [0.; 2];
    let mut monitor = [0.; 2];
    let mut stream = [0.; 2];
    let report = engine
        .process(
            1,
            &[
                InputBuffer::new(MICROPHONE_SOURCE_ID, &[0.5, 0.25]),
                InputBuffer::new(SYSTEM_SOURCE_ID, &[0.25, 0.5]),
            ],
            &mut [
                OutputBuffer::new(EndpointId::Microphone, &mut microphone),
                OutputBuffer::new(EndpointId::MonitorMix, &mut monitor),
                OutputBuffer::new(EndpointId::StreamMix, &mut stream),
            ],
        )
        .expect("real processed block");
    let (mut audio, control) = supervisor();
    let candidate = candidate();
    tick(&mut audio, Some(&candidate), true);
    let now = Instant::now();
    assert_eq!(audio.meter_snapshot_at(now), MeterSnapshot::Waiting);
    control.meters.borrow_mut().push_back(report.meters());
    audio.poll_meters_at(now);
    let MeterSnapshot::Live { frame } = audio.meter_snapshot_at(now) else {
        panic!("live block");
    };
    assert_eq!(frame.sources[0].levels.peak_left, 500_000);
    assert_eq!(frame.sources[0].levels.rms_right, 250_000);
    assert_eq!(frame.outputs[1].peak_left, 750_000);
    assert_eq!(frame.outputs[2].rms_right, 750_000);
    assert!(control.meters.borrow().is_empty());
    control.meters.borrow_mut().push_back(report.meters());
    audio.poll_meters_at(now + Duration::from_millis(99));
    assert_eq!(control.meters.borrow().len(), 1, "drain cadence is bounded");
    audio.poll_meters_at(now + Duration::from_millis(100));
    assert!(control.meters.borrow().is_empty());
    assert_eq!(audio.meter_snapshot_at(now + Duration::from_millis(451)), MeterSnapshot::Waiting);
    audio.stop().expect("stop");
    assert!(matches!(audio.meter_snapshot(), MeterSnapshot::Unavailable { .. }));
    audio.enable().expect("enable");
    tick(&mut audio, Some(&candidate), true);
    assert_eq!(audio.meter_snapshot(), MeterSnapshot::Waiting);
}
