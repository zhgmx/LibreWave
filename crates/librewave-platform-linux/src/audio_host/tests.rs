//! Fake-backed regressions for the production host orchestration path.

use super::alsa::{select_revalidated_card, validate_applied_pcm, validate_card_id};
use super::pipewire::{
    PhysicalObjectKind, PhysicalObjectProperties, RealPipeWireFacade, is_candidate_physical_object,
};
use super::*;
use crate::audio_lifecycle::{AudioLifecycle, DegradedState, LifecycleError, LifecycleState};
use crate::{AlsaCardInfo, DeviceIdentity, UsbTopology};
use std::collections::VecDeque;
use std::io;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

fn card(number: u32, id: &str) -> AlsaCardInfo {
    AlsaCardInfo { number, id: Some(id.to_owned()), name: Some("Wave:3".to_owned()) }
}

fn candidate() -> UsbDeviceCandidate {
    UsbDeviceCandidate {
        identity: DeviceIdentity::wave3(),
        topology: UsbTopology::new("1-8.3"),
        alsa_cards: vec![card(7, "Wave3")],
    }
}

fn selected() -> SelectedPcmCard {
    SelectedPcmCard {
        identity: DeviceIdentity::wave3(),
        topology: UsbTopology::new("1-8.3"),
        card_number: 7,
        card_id: "Wave3".to_owned(),
        capture_device: 0,
        playback_device: 0,
    }
}

fn pcm_config() -> Wave3PhysicalIoConfig {
    Wave3PhysicalIoConfig::try_new(256, 1_024, 192, 768).expect("valid test PCM geometry")
}

#[test]
fn applied_pcm_validation_has_no_format_or_geometry_fallback() {
    assert_eq!(validate_applied_pcm(PcmDirection::Capture, true, true), Ok(()));
    assert_eq!(
        validate_applied_pcm(PcmDirection::Capture, false, true),
        Err(LinuxAudioError::UnsupportedPcmFormat { direction: PcmDirection::Capture })
    );
    assert_eq!(
        validate_applied_pcm(PcmDirection::Playback, true, false),
        Err(LinuxAudioError::PcmConfigurationAdjusted { direction: PcmDirection::Playback })
    );
}

#[test]
fn exact_card_selection_rejects_renumber_id_topology_and_ambiguity() {
    let expected = candidate();
    assert_eq!(
        select_revalidated_card(&expected, std::slice::from_ref(&expected)),
        Ok((card(7, "Wave3"), UsbTopology::new("1-8.3")))
    );
    for changed in [
        UsbDeviceCandidate { alsa_cards: vec![card(8, "Wave3")], ..expected.clone() },
        UsbDeviceCandidate { alsa_cards: vec![card(7, "Other")], ..expected.clone() },
        UsbDeviceCandidate { topology: UsbTopology::new("1-9"), ..expected.clone() },
    ] {
        assert_eq!(
            select_revalidated_card(&expected, &[changed]),
            Err(LinuxAudioError::AlsaCardChanged)
        );
    }
    let ambiguous = UsbDeviceCandidate {
        alsa_cards: vec![card(7, "Wave3"), card(8, "Wave3_1")],
        ..expected.clone()
    };
    assert_eq!(
        select_revalidated_card(&ambiguous, std::slice::from_ref(&ambiguous)),
        Err(LinuxAudioError::AmbiguousAlsaCard)
    );
}

#[test]
fn alsa_card_id_uses_only_the_kernel_identifier_grammar() {
    for valid in ["Wave3", "Wave_3", "A1"] {
        assert_eq!(validate_card_id(valid), Ok(()));
    }
    for invalid in
        ["", "123", "1Wave", "_Wave", "Wave-3", "Wave3,DEV=9", "Wave3\0Other", "MoreThanFifteen1"]
    {
        assert_eq!(validate_card_id(invalid), Err(LinuxAudioError::InvalidAlsaCardId));
    }

    let expected = UsbDeviceCandidate { alsa_cards: vec![card(7, "Wave3,DEV=9")], ..candidate() };
    assert_eq!(
        select_revalidated_card(&expected, std::slice::from_ref(&expected)),
        Err(LinuxAudioError::InvalidAlsaCardId)
    );
}

#[test]
fn endpoint_allowlist_direction_names_and_properties_are_exact() {
    let plans = endpoint_plans(DELIBERATE_ENDPOINTS).expect("deliberate plans are valid");
    assert_eq!(
        plans,
        vec![
            EndpointPlan {
                endpoint: EndpointId::System,
                direction: PipeWireEndpointDirection::Input,
                node_name: "librewave.system",
                node_description: "LibreWave System",
                media_class: "Audio/Sink",
                node_virtual: true,
            },
            EndpointPlan {
                endpoint: EndpointId::Microphone,
                direction: PipeWireEndpointDirection::Output,
                node_name: "librewave.microphone",
                node_description: "LibreWave Microphone",
                media_class: "Audio/Source",
                node_virtual: true,
            },
            EndpointPlan {
                endpoint: EndpointId::MonitorMix,
                direction: PipeWireEndpointDirection::Output,
                node_name: "librewave.monitor-mix",
                node_description: "LibreWave Monitor Mix",
                media_class: "Audio/Source",
                node_virtual: true,
            },
            EndpointPlan {
                endpoint: EndpointId::StreamMix,
                direction: PipeWireEndpointDirection::Output,
                node_name: "librewave.stream-mix",
                node_description: "LibreWave Stream Mix",
                media_class: "Audio/Source",
                node_virtual: true,
            },
        ]
    );
    assert_eq!(
        endpoint_plans(&[EndpointId::Microphone, EndpointId::Microphone]),
        Err(LinuxAudioError::DuplicateEndpoint(EndpointId::Microphone))
    );
}

#[test]
fn pipewire_physical_objects_match_only_the_admitted_card() {
    let exact_device = PhysicalObjectProperties {
        vendor_id: Some("0x0fd9"),
        product_id: Some("0x0070"),
        alsa_card: Some("2"),
        alsa_path: Some("hw:2"),
    };
    assert!(is_candidate_physical_object(PhysicalObjectKind::Device, exact_device, 2));
    assert!(is_candidate_physical_object(
        PhysicalObjectKind::Node,
        PhysicalObjectProperties { alsa_path: Some("hw:2"), ..PhysicalObjectProperties::default() },
        2
    ));
    assert!(is_candidate_physical_object(
        PhysicalObjectKind::Node,
        PhysicalObjectProperties {
            alsa_path: Some("hw:2,0,0"),
            ..PhysicalObjectProperties::default()
        },
        2
    ));

    for adjacent in [
        PhysicalObjectProperties { alsa_card: Some("20"), ..exact_device },
        PhysicalObjectProperties { product_id: Some("0x0071"), ..exact_device },
    ] {
        assert!(!is_candidate_physical_object(PhysicalObjectKind::Device, adjacent, 2));
    }
    assert!(!is_candidate_physical_object(
        PhysicalObjectKind::Node,
        PhysicalObjectProperties {
            alsa_path: Some("hw:20"),
            ..PhysicalObjectProperties::default()
        },
        2
    ));
    for malformed in ["", "hw:", "hw:02", "hw:2x", "hw:2,", "plughw:2", "hw:2,0,0,0"] {
        assert!(!is_candidate_physical_object(
            PhysicalObjectKind::Node,
            PhysicalObjectProperties {
                alsa_path: Some(malformed),
                ..PhysicalObjectProperties::default()
            },
            2
        ));
    }
    assert!(!is_candidate_physical_object(PhysicalObjectKind::Other, exact_device, 2));
}

#[test]
fn pipewire_scan_fails_closed_without_one_candidate_card() {
    let mut pipewire = RealPipeWireFacade::default();
    for candidate in [
        UsbDeviceCandidate { alsa_cards: Vec::new(), ..candidate() },
        UsbDeviceCandidate {
            alsa_cards: vec![card(2, "Wave3"), card(3, "Wave3_1")],
            ..candidate()
        },
    ] {
        assert_eq!(
            pipewire.verify_physical_nodes_hidden(&candidate),
            Err(LinuxAudioError::AmbiguousAlsaCard)
        );
        assert!(!pipewire.owns_connection());
    }
}

#[test]
fn pcm_config_sizes_independent_checked_period_buffers() {
    let config = pcm_config();
    assert_eq!(config.capture_period_bytes(), Ok(768));
    assert_eq!(config.capture_buffer_bytes(), Ok(3_072));
    assert_eq!(config.playback_period_bytes(), Ok(1_152));
    assert_eq!(config.playback_buffer_bytes(), Ok(4_608));
    assert!(Wave3PhysicalIoConfig::try_new(1, 1, 1, 1).is_ok());
    assert_eq!(
        Wave3PhysicalIoConfig::try_new(2, 1, 1, 1),
        Err(PcmConfigError::PeriodExceedsBuffer { period_frames: 2, buffer_frames: 1 })
    );
}

#[test]
fn capture_timeout_must_form_a_nonzero_deadline() {
    for timeout in [Duration::ZERO, Duration::MAX] {
        assert!(matches!(
            LinuxAudioHost::new(pcm_config(), timeout),
            Err(LinuxAudioError::InvalidCaptureTimeout)
        ));
    }
}

#[test]
fn capture_worker_propagates_thread_spawn_failure() {
    let capture = ScriptedCapture { reads: Arc::new(Mutex::new(VecDeque::new())) };
    let result = CaptureWorker::start_with(Box::new(capture), 2_048, |_| {
        Err(io::Error::other("thread limit"))
    });
    assert!(matches!(
        result,
        Err(LinuxAudioError::CaptureWorkerSpawn(message)) if message == "thread limit"
    ));
}

#[test]
fn host_rejects_an_unknown_hidden_node_contract() {
    let mut harness =
        harness(std::iter::empty::<Vec<CaptureRead>>(), true, Duration::from_millis(5));
    assert_eq!(
        harness.host.verify_physical_nodes_hidden(&candidate(), &[HiddenPhysicalNode::Capture]),
        Err(LinuxAudioError::InvalidHiddenPhysicalNodeContract)
    );
    assert!(harness.pipewire_calls.lock().expect("calls lock").is_empty());
}

#[derive(Debug)]
struct ScriptedCapture {
    reads: Arc<Mutex<VecDeque<CaptureRead>>>,
}

impl CapturePcm for ScriptedCapture {
    fn read_frames(&mut self, _bytes: &mut [u8]) -> CaptureRead {
        self.reads.lock().expect("script lock").pop_front().unwrap_or(CaptureRead::WouldBlock)
    }
}

#[derive(Debug)]
struct FakePlayback {
    close_count: Arc<AtomicU64>,
    fail_close: bool,
}

impl PlaybackPcm for FakePlayback {
    fn close(&mut self) -> Result<(), LinuxAudioError> {
        self.close_count.fetch_add(1, Ordering::Relaxed);
        if self.fail_close { Err(LinuxAudioError::TeardownFailed) } else { Ok(()) }
    }
}

#[derive(Debug)]
struct FakeAlsa {
    capture_scripts: VecDeque<VecDeque<CaptureRead>>,
    calls: Arc<Mutex<Vec<&'static str>>>,
    close_count: Arc<AtomicU64>,
    fail_playback: bool,
}

impl AlsaFacade for FakeAlsa {
    fn select(
        &mut self,
        _candidate: &UsbDeviceCandidate,
    ) -> Result<SelectedPcmCard, LinuxAudioError> {
        self.calls.lock().expect("calls lock").push("select");
        Ok(selected())
    }

    fn start_capture(
        &mut self,
        _candidate: &UsbDeviceCandidate,
        _selected: &SelectedPcmCard,
    ) -> Result<Box<dyn CapturePcm>, LinuxAudioError> {
        self.calls.lock().expect("calls lock").push("capture-started");
        let reads = self.capture_scripts.pop_front().unwrap_or_default();
        Ok(Box::new(ScriptedCapture { reads: Arc::new(Mutex::new(reads)) }))
    }

    fn open_playback(
        &mut self,
        _candidate: &UsbDeviceCandidate,
        _selected: &SelectedPcmCard,
    ) -> Result<Box<dyn PlaybackPcm>, LinuxAudioError> {
        self.calls.lock().expect("calls lock").push("playback");
        if self.fail_playback {
            return Err(LinuxAudioError::Alsa("playback failed".to_owned()));
        }
        Ok(Box::new(FakePlayback { close_count: Arc::clone(&self.close_count), fail_close: false }))
    }
}

#[derive(Debug)]
struct FakePipeWire {
    calls: Arc<Mutex<Vec<&'static str>>>,
    connected: bool,
    endpoint_streams_available: bool,
    fail_disconnect: bool,
}

impl PipeWireFacade for FakePipeWire {
    fn verify_physical_nodes_hidden(
        &mut self,
        _candidate: &UsbDeviceCandidate,
    ) -> Result<(), LinuxAudioError> {
        self.calls.lock().expect("calls lock").push("pipewire-connect");
        self.connected = true;
        Ok(())
    }

    fn publish_endpoints(&mut self, plans: &[EndpointPlan]) -> Result<(), LinuxAudioError> {
        assert_eq!(
            plans.iter().map(|plan| plan.endpoint).collect::<Vec<_>>(),
            DELIBERATE_ENDPOINTS
        );
        self.calls.lock().expect("calls lock").push("publish");
        if self.endpoint_streams_available {
            Ok(())
        } else {
            Err(LinuxAudioError::EndpointStreamTransportUnavailable)
        }
    }

    fn disconnect(&mut self) -> Result<(), LinuxAudioError> {
        self.calls.lock().expect("calls lock").push("pipewire-disconnect");
        self.connected = false;
        if self.fail_disconnect { Err(LinuxAudioError::TeardownFailed) } else { Ok(()) }
    }

    fn owns_connection(&self) -> bool {
        self.connected
    }
}

struct Harness {
    host: LinuxAudioHost,
    alsa_calls: Arc<Mutex<Vec<&'static str>>>,
    pipewire_calls: Arc<Mutex<Vec<&'static str>>>,
    close_count: Arc<AtomicU64>,
}

fn harness(
    capture_scripts: impl IntoIterator<Item = Vec<CaptureRead>>,
    endpoint_streams_available: bool,
    timeout: Duration,
) -> Harness {
    harness_with_playback_failure(capture_scripts, endpoint_streams_available, timeout, false)
}

fn harness_with_playback_failure(
    capture_scripts: impl IntoIterator<Item = Vec<CaptureRead>>,
    endpoint_streams_available: bool,
    timeout: Duration,
    fail_playback: bool,
) -> Harness {
    let alsa_calls = Arc::new(Mutex::new(Vec::new()));
    let pipewire_calls = Arc::new(Mutex::new(Vec::new()));
    let close_count = Arc::new(AtomicU64::new(0));
    let alsa = FakeAlsa {
        capture_scripts: capture_scripts.into_iter().map(VecDeque::from).collect::<VecDeque<_>>(),
        calls: Arc::clone(&alsa_calls),
        close_count: Arc::clone(&close_count),
        fail_playback,
    };
    let pipewire = FakePipeWire {
        calls: Arc::clone(&pipewire_calls),
        connected: false,
        endpoint_streams_available,
        fail_disconnect: false,
    };
    Harness {
        host: LinuxAudioHost::with_facades(Box::new(alsa), Box::new(pipewire), timeout, 2_048),
        alsa_calls,
        pipewire_calls,
        close_count,
    }
}

#[test]
fn capture_frame_and_xrun_are_observed_before_playback() {
    let mut harness = harness(
        [vec![CaptureRead::RecoveredXrun, CaptureRead::Frames(64)]],
        true,
        Duration::from_millis(100),
    );
    let mut lifecycle = AudioLifecycle::new();
    lifecycle.start(&mut harness.host, Some(&candidate())).expect("startup succeeds");
    assert_eq!(lifecycle.state(), LifecycleState::Ready);
    assert_eq!(
        *harness.alsa_calls.lock().expect("calls lock"),
        ["select", "capture-started", "playback"]
    );
    let state = harness.host.resource_state();
    assert_eq!(state.capture, ResourceActivity::Active);
    assert_eq!(state.playback, ResourceActivity::Active);
    assert!(state.capture_frames >= 64);
    assert_eq!(state.recovered_xruns, 1);
    lifecycle.teardown(&mut harness.host).expect("teardown succeeds");
    assert_eq!(harness.close_count.load(Ordering::Relaxed), 1);
}

#[test]
fn prepared_playback_stays_starting_until_processing_is_active() {
    let mut harness = harness([vec![CaptureRead::Frames(64)]], true, Duration::from_millis(100));
    let device = candidate();
    harness
        .host
        .verify_physical_nodes_hidden(&device, &HIDDEN_PHYSICAL_NODES)
        .expect("physical objects are hidden");
    let capture = harness.host.start_capture(&device).expect("capture starts");
    harness.host.observe_capture(&device, &capture).expect("capture is active");
    harness.host.start_playback(&device).expect("playback opens");
    assert_eq!(harness.host.resource_state().playback, ResourceActivity::Starting);
    harness.host.publish_endpoints(&device, DELIBERATE_ENDPOINTS).expect("processing activates");
    assert_eq!(harness.host.resource_state().playback, ResourceActivity::Active);
    harness.host.teardown().expect("teardown succeeds");
}

#[test]
fn timeout_and_disconnect_never_open_playback() {
    for (reads, expected) in [
        (vec![], LinuxAudioError::CaptureTimedOut),
        (vec![CaptureRead::Disconnected], LinuxAudioError::CaptureDisconnected),
        (vec![CaptureRead::Failed], LinuxAudioError::CaptureWorkerFailed),
    ] {
        let mut harness = harness([reads], true, Duration::from_millis(5));
        let mut lifecycle = AudioLifecycle::new();
        let error = lifecycle.start(&mut harness.host, Some(&candidate())).expect_err("fails");
        assert!(matches!(
            error,
            LifecycleError::Host {
                state: DegradedState::CaptureNotConfirmed,
                source,
            } if source == expected
        ));
        assert!(!harness.alsa_calls.lock().expect("calls lock").contains(&"playback"));
    }
}

#[test]
fn engine_boundary_cleans_partial_startup_and_never_claims_endpoints() {
    let mut harness = harness([vec![CaptureRead::Frames(32)]], false, Duration::from_millis(100));
    let mut lifecycle = AudioLifecycle::new();
    let error = lifecycle.start(&mut harness.host, Some(&candidate())).expect_err("fails");
    assert!(matches!(
        error,
        LifecycleError::Host {
            state: DegradedState::EndpointPublicationFailed,
            source: LinuxAudioError::EndpointStreamTransportUnavailable,
        }
    ));
    assert_eq!(harness.close_count.load(Ordering::Relaxed), 1);
    assert_eq!(
        harness.host.resource_state(),
        AudioResourceState {
            not_ready: Some(GraphNotReady::EndpointStreamTransportUnavailable),
            ..AudioResourceState::default()
        }
    );
    harness.host.teardown().expect("repeated host teardown succeeds");
    assert_eq!(harness.close_count.load(Ordering::Relaxed), 1);
}

#[test]
fn playback_open_failure_closes_the_confirmed_capture_worker() {
    let mut harness = harness_with_playback_failure(
        [vec![CaptureRead::Frames(32)]],
        true,
        Duration::from_millis(100),
        true,
    );
    let mut lifecycle = AudioLifecycle::new();
    let error = lifecycle.start(&mut harness.host, Some(&candidate())).expect_err("fails");
    assert!(matches!(
        error,
        LifecycleError::Host {
            state: DegradedState::PlaybackStartFailed,
            source: LinuxAudioError::Alsa(_),
        }
    ));
    assert_eq!(harness.host.resource_state(), AudioResourceState::default());
    assert_eq!(harness.close_count.load(Ordering::Relaxed), 0);
}

#[test]
fn teardown_is_idempotent_and_pipewire_restart_reuses_one_path() {
    let mut harness = harness(
        [vec![CaptureRead::Frames(32)], vec![CaptureRead::Frames(32)]],
        true,
        Duration::from_millis(100),
    );
    let mut lifecycle = AudioLifecycle::new();
    let device = candidate();
    lifecycle.start(&mut harness.host, Some(&device)).expect("startup succeeds");
    lifecycle.teardown(&mut harness.host).expect("teardown succeeds");
    lifecycle.teardown(&mut harness.host).expect("teardown stays idempotent");
    assert_eq!(harness.close_count.load(Ordering::Relaxed), 1);
    lifecycle
        .recover_after_host_restart(&mut harness.host, Some(&device))
        .expect("restart recovery succeeds");
    assert_eq!(lifecycle.state(), LifecycleState::Ready);
    assert_eq!(
        harness
            .pipewire_calls
            .lock()
            .expect("calls lock")
            .iter()
            .filter(|call| **call == "pipewire-connect")
            .count(),
        2
    );
    lifecycle.teardown(&mut harness.host).expect("final teardown succeeds");
}

#[test]
fn facade_has_no_hardware_mixer_control_path() {
    fn assert_audio_only_facade<T: AlsaFacade>() {}
    assert_audio_only_facade::<RealAlsaFacade>();
}
